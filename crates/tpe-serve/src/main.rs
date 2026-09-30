//! `tpe-serve`: run the local API (docs/API.md).
//!
//! ```text
//! tpe-serve                      # http://127.0.0.1:47470
//! tpe-serve --port 0 --address-file addr   # any free port, written to addr
//! tpe-serve --print-token        # the bearer token (made on first use)
//! ```
//!
//! The URL it listens on is the only line on stdout; logs go to stderr.

#![allow(clippy::must_use_candidate, clippy::missing_errors_doc)]

use std::io::Write as _;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;
use tpe_serve::{Config, DEFAULT_PORT, Server, StartError};

#[derive(Parser)]
#[command(
    name = "tpe-serve",
    version = tpe_serve::version_text(),
    about = "Local HTTP API for PDFTextract: get text or a bibliography from PDFs (loopback only)"
)]
struct Cli {
    /// Address to listen on: `127.0.0.1` or `::1` (nothing else is accepted).
    #[arg(long, default_value = "127.0.0.1")]
    bind: IpAddr,
    /// Port to listen on; 0 picks a free one.
    #[arg(long, default_value_t = DEFAULT_PORT)]
    port: u16,
    /// Directory for the token file and the ledger [default: the app's,
    /// e.g. ~/.local/share/PDFTextract or ~/Library/Application Support/PDFTextract].
    #[arg(long)]
    state_dir: Option<PathBuf>,
    /// Also accept browser requests from this origin (repeatable), e.g.
    /// `http://localhost:5173` for a front end served elsewhere. None by default.
    #[arg(long = "allow-origin", value_name = "ORIGIN")]
    allow_origin: Vec<String>,
    /// Write the URL the server listens on to this file once it is listening.
    #[arg(long, value_name = "FILE")]
    address_file: Option<PathBuf>,
    /// Print the bearer token (making it if there is none) and exit.
    #[arg(long)]
    print_token: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let state_dir = cli.state_dir.unwrap_or_else(Config::default_state_dir);
    if cli.print_token {
        return match tpe_serve::token::load_or_create(&state_dir) {
            Ok(token) => {
                println!("{}", token.expose());
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("tpe-serve: {error}");
                ExitCode::FAILURE
            }
        };
    }
    let mut config = Config::new(state_dir);
    config.bind = cli.bind;
    config.port = cli.port;
    config.extra_origins = cli.allow_origin;
    let runtime = match tpe_serve::runtime() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("tpe-serve: starting the runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(async move {
        let server = match Server::bind(config) {
            Ok(server) => server,
            Err(error) => {
                eprintln!("tpe-serve: {error}");
                return match error {
                    StartError::Config(_) => ExitCode::from(2),
                    _ => ExitCode::FAILURE,
                };
            }
        };
        let url = server.url();
        if let Some(path) = &cli.address_file
            && let Err(error) = write_address(path, &url)
        {
            eprintln!("tpe-serve: writing the address file: {error}");
            return ExitCode::FAILURE;
        }
        println!("{url}");
        let _ = std::io::stdout().flush();
        eprintln!("tpe-serve: listening on {url}");
        match server.run(shutdown_signal()).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("tpe-serve: {error}");
                ExitCode::FAILURE
            }
        }
    })
}

/// Write `url` to `path` whole: a temporary file renamed over it.
fn write_address(path: &Path, url: &str) -> std::io::Result<()> {
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(format!(".{}.partial", std::process::id()));
    let temporary = PathBuf::from(temporary);
    std::fs::write(&temporary, format!("{url}\n"))?;
    std::fs::rename(&temporary, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temporary);
    })
}

/// Ctrl-C or SIGTERM.
async fn shutdown_signal() {
    use futures::future::{Either, select};
    use tokio::signal::unix::{SignalKind, signal};
    let interrupt = std::pin::pin!(tokio::signal::ctrl_c());
    match signal(SignalKind::terminate()) {
        Ok(mut terminate) => {
            let terminated = std::pin::pin!(terminate.recv());
            match select(interrupt, terminated).await {
                Either::Left(_) | Either::Right(_) => {}
            }
        }
        Err(_) => {
            let _ = interrupt.await;
        }
    }
    eprintln!("tpe-serve: stopping");
}
