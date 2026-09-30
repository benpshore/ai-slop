//! What the server binds to, where it keeps its state, and its limits.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::PathBuf;
use std::time::Duration;

/// The port used when none is given. Arbitrary: chosen to be easy to type
/// and outside the ephemeral range (49152-65535); `--port 0` asks the
/// operating system for a free one instead.
pub const DEFAULT_PORT: u16 = 47470;

/// Bounds on what one client, or all of them together, can make the server
/// hold. Every field is generous on purpose: a person driving a client
/// through a switch or eye tracker must never be cut off for being slow.
#[derive(Clone, Debug)]
pub struct Limits {
    /// Largest JSON request body, in bytes.
    pub max_body_bytes: usize,
    /// Most PDFs in one `POST /v1/jobs`.
    pub max_paths_per_request: usize,
    /// Most jobs kept at once (queued, running or finished and not yet
    /// deleted). Jobs are never dropped on a timer; past this a new batch is
    /// refused until the client deletes some. It also bounds `GET /v1/jobs`,
    /// which builds its answer under the lock the engine thread reports
    /// progress through (about 2 µs per finished job measured on Linux
    /// x86-64, docs/API.md).
    pub max_jobs: usize,
    /// Most TCP connections served at once; further ones wait in the
    /// kernel's accept queue.
    pub max_connections: usize,
    /// Time a connection has to send a complete request head, including an
    /// idle keep-alive connection waiting for its next request.
    pub header_read_timeout: Duration,
    /// Time to read a request body and produce the response head. Streams
    /// (events, output files) are not cut by it once they have started.
    pub request_timeout: Duration,
    /// Interval of the comment line an event stream sends when nothing has
    /// happened, so intermediaries and clients can tell it is alive.
    pub sse_keepalive: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_body_bytes: 256 * 1024,
            max_paths_per_request: 1000,
            max_jobs: 10_000,
            max_connections: 64,
            header_read_timeout: Duration::from_secs(30),
            request_timeout: Duration::from_secs(60),
            sse_keepalive: Duration::from_secs(15),
        }
    }
}

/// Everything [`crate::Server::bind`] needs.
#[derive(Clone, Debug)]
pub struct Config {
    /// `127.0.0.1` or `::1`; anything else is refused ([`check_bind`]).
    pub bind: IpAddr,
    /// `0` for any free port.
    pub port: u16,
    /// Holds the token file and the ledger that text jobs write.
    pub state_dir: PathBuf,
    /// Browser origins allowed besides the server's own
    /// (`http://127.0.0.1:<port>`, `http://[::1]:<port>`,
    /// `http://localhost:<port>`). Empty by default.
    pub extra_origins: Vec<String>,
    pub limits: Limits,
}

impl Config {
    /// Loopback IPv4 on [`DEFAULT_PORT`], no extra origins, default limits.
    pub fn new(state_dir: PathBuf) -> Self {
        Self {
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: DEFAULT_PORT,
            state_dir,
            extra_origins: Vec::new(),
            limits: Limits::default(),
        }
    }

    /// The directory the app keeps its ledger in (`jobs::default_ledger_path`):
    /// `~/Library/Application Support/PDFTextract` on macOS, else
    /// `$XDG_DATA_HOME/PDFTextract` (or `~/.local/share/PDFTextract`). The
    /// server and the app then share one ledger.
    pub fn default_state_dir() -> PathBuf {
        let ledger = tpe_app::jobs::default_ledger_path();
        ledger
            .parent()
            .map_or_else(std::env::temp_dir, PathBuf::from)
    }

    /// The ledger text jobs write, as the app's does.
    pub fn ledger_path(&self) -> PathBuf {
        self.state_dir.join("ledger.sqlite")
    }
}

/// A configuration the server refuses to start with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// Only `127.0.0.1` and `::1` may be bound.
    NotLoopback(IpAddr),
    /// An extra origin that is not `http(s)://host[:port]`.
    BadOrigin(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotLoopback(ip) => write!(
                f,
                "refusing to bind {ip}: tpe-serve listens only on 127.0.0.1 or ::1"
            ),
            Self::BadOrigin(origin) => write!(
                f,
                "not an origin (expected http://host[:port] or https://host[:port]): {origin:?}"
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

/// Accept exactly the two loopback addresses. Other `127.0.0.0/8` addresses
/// and IPv4-mapped forms are refused too, so the set of names a request may
/// carry in `Host` stays the three the server checks.
pub fn check_bind(ip: IpAddr) -> Result<(), ConfigError> {
    if ip == IpAddr::V4(Ipv4Addr::LOCALHOST) || ip == IpAddr::V6(Ipv6Addr::LOCALHOST) {
        Ok(())
    } else {
        Err(ConfigError::NotLoopback(ip))
    }
}

/// An extra allowed origin, lower-cased: `http://` or `https://`, then a
/// non-empty host with an optional port, and nothing else (no path, no
/// wildcard, never `null`).
pub fn normalize_origin(origin: &str) -> Result<String, ConfigError> {
    let lower = origin.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("http://")
        .or_else(|| lower.strip_prefix("https://"));
    let valid = rest.is_some_and(|authority| {
        !authority.is_empty()
            && authority
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'[' | b']'))
    });
    if valid {
        Ok(lower)
    } else {
        Err(ConfigError::BadOrigin(origin.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::{ConfigError, check_bind, normalize_origin};
    use std::net::IpAddr;

    #[test]
    fn only_the_two_loopback_addresses_bind() {
        for ok in ["127.0.0.1", "::1"] {
            assert_eq!(check_bind(ok.parse().unwrap()), Ok(()), "{ok}");
        }
        for refused in [
            "0.0.0.0",
            "::",
            "192.168.1.10",
            "127.0.0.2",
            "::ffff:127.0.0.1",
            "10.0.0.1",
        ] {
            let ip: IpAddr = refused.parse().unwrap();
            assert_eq!(
                check_bind(ip),
                Err(ConfigError::NotLoopback(ip)),
                "{refused}"
            );
        }
    }

    #[test]
    fn extra_origins_are_plain_scheme_and_authority() {
        assert_eq!(
            normalize_origin("HTTP://Example.org:8080").unwrap(),
            "http://example.org:8080"
        );
        assert!(normalize_origin("https://[::1]:3000").is_ok());
        for bad in [
            "null",
            "*",
            "http://",
            "example.org",
            "http://example.org/path",
            "file://x",
            "http://a b",
        ] {
            assert!(normalize_origin(bad).is_err(), "{bad}");
        }
    }
}
