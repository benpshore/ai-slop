//! Error type shared by every client and parser in this crate.

use thiserror::Error;

/// Errors from the bibliographic clients and parsers.
///
/// Messages never contain the request URL, because some URLs carry API keys.
#[derive(Debug, Error)]
pub enum BiblioError {
    /// The client is in offline mode; no request was made.
    #[error("offline mode: network requests are disabled")]
    Offline,
    /// The server answered 404.
    #[error("not found")]
    NotFound,
    /// The server answered 429 (rate limited).
    #[error("rate limited by the server (HTTP 429)")]
    RateLimited,
    /// Any other non-success HTTP status.
    #[error("HTTP status {0}")]
    Status(u16),
    /// A transport-level failure (DNS, TLS, timeout, ...), sanitised.
    #[error("transport error: {0}")]
    Transport(String),
    /// The response body was not valid JSON.
    #[error("invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// A required setting (such as the contact e-mail Unpaywall needs) is missing.
    #[error("missing configuration: {0}")]
    MissingConfig(&'static str),
    /// The JSON was valid but did not have the expected shape.
    #[error("unexpected response shape: {0}")]
    Shape(String),
}

impl BiblioError {
    /// Map a `ureq` error to a sanitised `BiblioError` (no URL in the message).
    pub fn from_ureq(err: &ureq::Error) -> Self {
        match err {
            ureq::Error::StatusCode(404) => Self::NotFound,
            ureq::Error::StatusCode(429) => Self::RateLimited,
            ureq::Error::StatusCode(code) => Self::Status(*code),
            ureq::Error::Timeout(_) => Self::Transport("timeout".to_string()),
            ureq::Error::HostNotFound => Self::Transport("host not found".to_string()),
            ureq::Error::ConnectionFailed => Self::Transport("connection failed".to_string()),
            ureq::Error::Io(io) => Self::Transport(format!("io: {}", io.kind())),
            ureq::Error::BodyExceedsLimit(limit) => {
                Self::Transport(format!("response body exceeds {limit} bytes"))
            }
            ureq::Error::TooManyRedirects => Self::Transport("too many redirects".to_string()),
            ureq::Error::BadUri(_) => Self::Transport("invalid request URI".to_string()),
            _ => Self::Transport("request failed".to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_errors_never_echo_the_uri() {
        let secret_uri = "https://example.org/x?api_key=SECRET123".to_string();
        let err = BiblioError::from_ureq(&ureq::Error::BadUri(secret_uri));
        assert!(!err.to_string().contains("SECRET123"));
        let err = BiblioError::from_ureq(&ureq::Error::StatusCode(503));
        assert_eq!(err.to_string(), "HTTP status 503");
        assert!(matches!(
            BiblioError::from_ureq(&ureq::Error::StatusCode(404)),
            BiblioError::NotFound
        ));
    }
}
