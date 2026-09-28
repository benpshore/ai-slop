//! Error type shared by the Web API client and the local database reader.

/// Everything that can go wrong talking to Zotero (remote or local).
#[derive(Debug, thiserror::Error)]
pub enum ZError {
    /// The client was built with `offline(true)`; no request was attempted.
    #[error("client is offline; no request was made")]
    Offline,
    /// A write request was attempted without an API key.
    #[error("an API key with write access is required for this request")]
    MissingKey,
    /// Connection, TLS, timeout or body-decoding failure from `ureq`
    /// (kept as text so the error type stays small).
    #[error("transport error: {0}")]
    Transport(String),
    /// A status code without a more specific variant.
    #[error("HTTP {code}: {message}")]
    Status {
        /// HTTP status code.
        code: u16,
        /// Start of the response body (at most 300 characters).
        message: String,
    },
    /// 403: the key is invalid or lacks the needed privileges.
    #[error("forbidden (403): the API key is invalid or lacks the needed permission")]
    Forbidden,
    /// 404 from the API, or a missing local file.
    #[error("not found: {0}")]
    NotFound(String),
    /// 412: the library or object changed since the given version, or the
    /// write token was already used.
    #[error("precondition failed (412): stale version or reused write token")]
    PreconditionFailed,
    /// 428: a versioned request was sent without `If-Unmodified-Since-Version`.
    #[error("precondition required (428): a version header is missing")]
    PreconditionRequired,
    /// 429: rate limited; wait `retry_after_secs` (or back off exponentially).
    #[error("rate limited (429); retry after {retry_after_secs:?} s")]
    RateLimited {
        /// Value of the `Retry-After` header, when present.
        retry_after_secs: Option<u64>,
    },
    /// More than 50 objects in one write request.
    #[error("too many objects in one write: {count} (maximum 50)")]
    TooMany {
        /// Number of objects that were passed.
        count: usize,
    },
    /// An object key that is not 8 alphanumeric ASCII characters.
    #[error("invalid Zotero object key: {0:?}")]
    InvalidKey(String),
    /// A pagination link pointing outside the configured API base.
    #[error("refusing to follow a pagination link outside the API base: {0}")]
    ForeignLink(String),
    /// JSON that parsed but did not have the documented shape.
    #[error("unexpected response shape: {0}")]
    Parse(String),
    /// Invalid JSON.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// Error from the local `zotero.sqlite` reader.
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
}

impl From<ureq::Error> for ZError {
    fn from(error: ureq::Error) -> Self {
        Self::Transport(error.to_string())
    }
}
