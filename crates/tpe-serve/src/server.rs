//! Binding, the accept loop and the engine thread's lifetime.

use std::fmt;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::pin;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use futures::future::{Either, select};
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::TcpListener;
use tokio::runtime::Runtime;
use tokio::sync::{Semaphore, oneshot};

use crate::config::{Config, ConfigError, check_bind, normalize_origin};
use crate::guard::Guard;
use crate::http::{self, App};
use crate::store::{Store, log};
use crate::token::{self, Token, TokenError};

/// Largest request head (request line and headers) a connection may send.
const MAX_HEAD_BYTES: usize = 64 * 1024;

/// Why the server did not start.
#[derive(Debug)]
pub enum StartError {
    Config(ConfigError),
    Token(TokenError),
    Io(&'static str, io::Error),
}

impl fmt::Display for StartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(error) => error.fmt(f),
            Self::Token(error) => error.fmt(f),
            Self::Io(what, error) => write!(f, "{what}: {error}"),
        }
    }
}

impl std::error::Error for StartError {}

impl From<ConfigError> for StartError {
    fn from(error: ConfigError) -> Self {
        Self::Config(error)
    }
}

impl From<TokenError> for StartError {
    fn from(error: TokenError) -> Self {
        Self::Token(error)
    }
}

/// The runtime `tpe-serve` uses: two I/O threads are plenty, since handlers
/// do little and the engine has a thread of its own.
pub fn runtime() -> io::Result<Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("tpe-serve-io")
        .enable_io()
        .enable_time()
        .build()
}

/// A bound server with its engine thread started, ready to [`Server::run`].
pub struct Server {
    listener: TcpListener,
    app: Arc<App>,
    worker: Option<JoinHandle<()>>,
    local_addr: SocketAddr,
    token: Token,
}

impl Server {
    /// Check the configuration, load or make the token, bind the listener
    /// and start the engine thread. Must be called inside a Tokio runtime.
    /// Nothing is created on disk when the address is refused.
    pub fn bind(config: Config) -> Result<Self, StartError> {
        check_bind(config.bind)?;
        let extra_origins = config
            .extra_origins
            .iter()
            .map(|origin| normalize_origin(origin))
            .collect::<Result<Vec<_>, _>>()?;
        let token = token::load_or_create(&config.state_dir)?;
        let listener = std::net::TcpListener::bind((config.bind, config.port))
            .map_err(|e| StartError::Io("binding the listener", e))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| StartError::Io("configuring the listener", e))?;
        let listener = TcpListener::from_std(listener)
            .map_err(|e| StartError::Io("registering the listener", e))?;
        let local_addr = listener
            .local_addr()
            .map_err(|e| StartError::Io("reading the bound address", e))?;
        let mut instance = [0u8; 4];
        getrandom::fill(&mut instance).map_err(|e| {
            StartError::Io("drawing an instance id", io::Error::other(e.to_string()))
        })?;
        let store = Arc::new(Store::new(
            hex::encode(instance),
            config.ledger_path(),
            config.limits.max_jobs,
        ));
        let worker = std::thread::Builder::new()
            .name("tpe-serve-engine".into())
            .spawn({
                let store = store.clone();
                move || store.work()
            })
            .map_err(|e| StartError::Io("starting the engine thread", e))?;
        let app = Arc::new(App {
            guard: Guard::new(local_addr.port(), &extra_origins, token.clone()),
            store,
            limits: config.limits,
        });
        Ok(Self {
            listener,
            app,
            worker: Some(worker),
            local_addr,
            token,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// `http://127.0.0.1:<port>` or `http://[::1]:<port>`.
    pub fn url(&self) -> String {
        format!("http://{}", self.local_addr)
    }

    pub fn token(&self) -> &Token {
        &self.token
    }

    /// Serve until `shutdown` completes, then stop taking jobs, ask the
    /// running job to stop and wait for the engine thread.
    pub async fn run(mut self, shutdown: impl Future<Output = ()>) -> io::Result<()> {
        let permits = Arc::new(Semaphore::new(self.app.limits.max_connections));
        let header_read_timeout = self.app.limits.header_read_timeout;
        let mut shutdown = pin!(shutdown);
        loop {
            let accept = pin!(async {
                // Wait for a free slot before accepting, so extra
                // connections queue in the kernel instead of here.
                let permit = permits.clone().acquire_owned().await;
                (permit, self.listener.accept().await)
            });
            let (permit, accepted) = match select(accept, shutdown.as_mut()).await {
                Either::Left((pair, _)) => pair,
                Either::Right(((), _)) => break,
            };
            let Ok(permit) = permit else { break };
            let (stream, peer) = match accepted {
                Ok(pair) => pair,
                Err(error) => {
                    // Out of file descriptors and the like: back off.
                    log(format_args!("accept failed: {error}"));
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            };
            // Unreachable while bound to loopback; kept as a second fence.
            if !peer.ip().is_loopback() {
                continue;
            }
            let _ = stream.set_nodelay(true);
            let app = self.app.clone();
            tokio::spawn(async move {
                let _permit = permit;
                let service = service_fn(move |request| http::handle(app.clone(), request));
                let mut builder = hyper::server::conn::http1::Builder::new();
                builder
                    .timer(TokioTimer::new())
                    .header_read_timeout(header_read_timeout)
                    .max_buf_size(MAX_HEAD_BYTES);
                let _ = builder
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
        self.app.store.stop();
        if let Some(worker) = self.worker.take() {
            let _ = tokio::task::spawn_blocking(move || worker.join()).await;
        }
        Ok(())
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // A server dropped without running must not leave its engine thread
        // waiting for work forever.
        self.app.store.stop();
    }
}

/// A server running on a thread of its own, with its own runtime: how a
/// program without Tokio (the app, a test) embeds it. Stopped on drop.
pub struct RunningServer {
    local_addr: SocketAddr,
    token: Token,
    stop: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<io::Result<()>>>,
}

impl RunningServer {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn token(&self) -> &Token {
        &self.token
    }

    /// Stop serving and wait for the running job to stop.
    pub fn shutdown(mut self) -> io::Result<()> {
        self.stop_and_join()
    }

    fn stop_and_join(&mut self) -> io::Result<()> {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        match self.thread.take().map(JoinHandle::join) {
            Some(Ok(result)) => result,
            Some(Err(_)) => Err(io::Error::other("the server thread panicked")),
            None => Ok(()),
        }
    }
}

impl Drop for RunningServer {
    fn drop(&mut self) {
        let _ = self.stop_and_join();
    }
}

/// Bind and serve on a new thread; returns once the listener is bound.
pub fn spawn(config: Config) -> Result<RunningServer, StartError> {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let thread = std::thread::Builder::new()
        .name("tpe-serve".into())
        .spawn(move || {
            let runtime = match runtime() {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = ready_tx.send(Err(StartError::Io("starting the runtime", error)));
                    return Ok(());
                }
            };
            runtime.block_on(async move {
                let server = match Server::bind(config) {
                    Ok(server) => server,
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                        return Ok(());
                    }
                };
                let _ = ready_tx.send(Ok((server.local_addr(), server.token().clone())));
                server
                    .run(async {
                        let _ = stop_rx.await;
                    })
                    .await
            })
        })
        .map_err(|e| StartError::Io("starting the server thread", e))?;
    match ready_rx.recv() {
        Ok(Ok((local_addr, token))) => Ok(RunningServer {
            local_addr,
            token,
            stop: Some(stop_tx),
            thread: Some(thread),
        }),
        Ok(Err(error)) => {
            let _ = thread.join();
            Err(error)
        }
        Err(_) => {
            let _ = thread.join();
            Err(StartError::Io(
                "starting the server",
                io::Error::other("the server thread ended early"),
            ))
        }
    }
}
