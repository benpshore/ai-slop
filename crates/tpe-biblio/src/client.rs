//! The shared HTTP client: one `ureq` agent, per-host rate limiting, offline mode.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::error::BiblioError;
use crate::util::host_of;

/// Key name for the `OpenAlex` API key (sent as the `api_key` query parameter).
pub const KEY_OPENALEX: &str = "openalex";
/// Key name for the Semantic Scholar API key (sent as the `x-api-key` header).
pub const KEY_SEMANTIC_SCHOLAR: &str = "semantic_scholar";
/// Key name for the NCBI E-utilities API key (sent as the `api_key` query parameter).
pub const KEY_NCBI: &str = "ncbi";

/// Default minimum spacing between two requests to the same host.
pub const DEFAULT_MIN_INTERVAL: Duration = Duration::from_millis(100);

/// Per-host request spacing. Pure: the caller supplies the current time, so it
/// can be tested with synthetic instants.
#[derive(Debug, Clone)]
pub struct RateLimiter {
    default_interval: Duration,
    per_host: HashMap<String, Duration>,
    next_free: HashMap<String, Instant>,
}

impl RateLimiter {
    /// A limiter spacing requests to every host by `default_interval`.
    pub fn new(default_interval: Duration) -> Self {
        Self {
            default_interval,
            per_host: HashMap::new(),
            next_free: HashMap::new(),
        }
    }

    /// Override the spacing for one host (lower-case host name).
    pub fn set_host_interval(&mut self, host: &str, interval: Duration) {
        self.per_host.insert(host.to_ascii_lowercase(), interval);
    }

    /// The spacing that applies to `host`.
    pub fn interval_for(&self, host: &str) -> Duration {
        self.per_host
            .get(host)
            .copied()
            .unwrap_or(self.default_interval)
    }

    /// Reserve the next request slot for `host` and return how long the caller
    /// must wait (from `now`) before sending.
    pub fn reserve(&mut self, host: &str, now: Instant) -> Duration {
        let slot = match self.next_free.get(host) {
            Some(&next) if next > now => next,
            _ => now,
        };
        let interval = self.interval_for(host);
        self.next_free.insert(host.to_string(), slot + interval);
        slot.saturating_duration_since(now)
    }
}

/// HTTP client shared by all bibliographic sources.
pub struct Client {
    agent: ureq::Agent,
    user_agent: String,
    mailto: Option<String>,
    keys: HashMap<&'static str, String>,
    offline: bool,
    link_resolver: Option<String>,
    limiter: Mutex<RateLimiter>,
    now: fn() -> Instant,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut key_names: Vec<&str> = self.keys.keys().copied().collect();
        key_names.sort_unstable();
        f.debug_struct("Client")
            .field("user_agent", &self.user_agent)
            .field("mailto", &self.mailto)
            .field("keys", &key_names)
            .field("offline", &self.offline)
            .field("link_resolver", &self.link_resolver)
            .finish_non_exhaustive()
    }
}

impl Client {
    /// A client with the given `User-Agent` (include a contact address, as
    /// Crossref and `OpenAlex` ask), 30 s timeout, and default host spacing.
    pub fn new(user_agent: &str) -> Self {
        let config = ureq::Agent::config_builder()
            .user_agent(user_agent)
            .timeout_global(Some(Duration::from_secs(30)))
            .build();
        let agent: ureq::Agent = config.into();
        let mut limiter = RateLimiter::new(DEFAULT_MIN_INTERVAL);
        // Semantic Scholar allows about 1 request/s; E-utilities 3/s without a key.
        limiter.set_host_interval("api.semanticscholar.org", Duration::from_millis(1100));
        limiter.set_host_interval("eutils.ncbi.nlm.nih.gov", Duration::from_millis(350));
        Self {
            agent,
            user_agent: user_agent.to_string(),
            mailto: None,
            keys: HashMap::new(),
            offline: false,
            link_resolver: None,
            limiter: Mutex::new(limiter),
            now: Instant::now,
        }
    }

    /// Set the contact e-mail sent as `mailto` / `email` (polite pools, Unpaywall).
    #[must_use]
    pub fn with_mailto(mut self, mailto: &str) -> Self {
        self.mailto = crate::util::non_empty(mailto);
        self
    }

    /// Store an API key under one of the `KEY_*` names.
    #[must_use]
    pub fn with_key(mut self, name: &'static str, value: &str) -> Self {
        self.keys.insert(name, value.to_string());
        self
    }

    /// Enable or disable offline mode (every request fails with `BiblioError::Offline`).
    #[must_use]
    pub fn with_offline(mut self, offline: bool) -> Self {
        self.offline = offline;
        self
    }

    /// Set the base URL of the library's `OpenURL` link resolver.
    #[must_use]
    pub fn with_link_resolver(mut self, base_url: &str) -> Self {
        self.link_resolver = crate::util::non_empty(base_url);
        self
    }

    /// Override the minimum spacing for one host.
    #[must_use]
    pub fn with_host_interval(self, host: &str, interval: Duration) -> Self {
        self.limiter
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .set_host_interval(host, interval);
        self
    }

    /// Replace the clock used by the rate limiter (tests use a frozen clock).
    #[must_use]
    pub fn with_clock(mut self, now: fn() -> Instant) -> Self {
        self.now = now;
        self
    }

    /// The configured contact e-mail.
    pub fn mailto(&self) -> Option<&str> {
        self.mailto.as_deref()
    }

    /// The API key stored under `name`, if any.
    pub fn key(&self, name: &str) -> Option<&str> {
        self.keys.get(name).map(String::as_str)
    }

    /// Whether offline mode is on.
    pub fn is_offline(&self) -> bool {
        self.offline
    }

    /// The configured link-resolver base URL.
    pub fn link_resolver(&self) -> Option<&str> {
        self.link_resolver.as_deref()
    }

    /// Reserve a request slot for `host`; returns the delay to wait before sending.
    pub fn reserve_slot(&self, host: &str) -> Duration {
        let now = (self.now)();
        self.limiter
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .reserve(&host.to_ascii_lowercase(), now)
    }

    /// GET `url` with extra `headers` and return the body as text. Respects
    /// offline mode and the per-host rate limit. Errors never contain the URL.
    pub fn get_text(&self, url: &str, headers: &[(&str, &str)]) -> Result<String, BiblioError> {
        if self.offline {
            return Err(BiblioError::Offline);
        }
        let wait = self.reserve_slot(&host_of(url));
        if !wait.is_zero() {
            std::thread::sleep(wait);
        }
        let mut request = self.agent.get(url);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let mut response = request.call().map_err(|e| BiblioError::from_ureq(&e))?;
        response
            .body_mut()
            .read_to_string()
            .map_err(|e| BiblioError::from_ureq(&e))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::OnceLock;

    use super::*;

    #[test]
    fn limiter_spaces_requests_per_host() {
        let t0 = Instant::now();
        let mut limiter = RateLimiter::new(Duration::from_millis(100));
        assert_eq!(limiter.reserve("a", t0), Duration::ZERO);
        assert_eq!(limiter.reserve("a", t0), Duration::from_millis(100));
        assert_eq!(limiter.reserve("a", t0), Duration::from_millis(200));
        assert_eq!(limiter.reserve("b", t0), Duration::ZERO);
        // Well after the reserved slots: no wait.
        let later = t0 + Duration::from_secs(5);
        assert_eq!(limiter.reserve("a", later), Duration::ZERO);
        // Partly elapsed: wait only for the remainder.
        let mid = later + Duration::from_millis(40);
        assert_eq!(limiter.reserve("a", mid), Duration::from_millis(60));
    }

    #[test]
    fn limiter_honours_host_override() {
        let t0 = Instant::now();
        let mut limiter = RateLimiter::new(Duration::from_millis(100));
        limiter.set_host_interval("slow.example", Duration::from_secs(1));
        assert_eq!(limiter.reserve("slow.example", t0), Duration::ZERO);
        assert_eq!(limiter.reserve("slow.example", t0), Duration::from_secs(1));
    }

    #[test]
    fn client_uses_injected_clock() {
        fn frozen() -> Instant {
            static T: OnceLock<Instant> = OnceLock::new();
            *T.get_or_init(Instant::now)
        }
        let client = Client::new("tpe-biblio-test")
            .with_host_interval("api.example.org", Duration::from_millis(250))
            .with_clock(frozen);
        assert_eq!(client.reserve_slot("api.example.org"), Duration::ZERO);
        assert_eq!(
            client.reserve_slot("API.example.org"),
            Duration::from_millis(250)
        );
        assert_eq!(
            client.reserve_slot("api.example.org"),
            Duration::from_millis(500)
        );
    }

    #[test]
    fn offline_client_refuses_requests() {
        let client = Client::new("tpe-biblio-test").with_offline(true);
        let result = client.get_text("https://api.openalex.org/works", &[]);
        assert!(matches!(result, Err(BiblioError::Offline)));
    }

    #[test]
    fn debug_hides_key_values() {
        let client = Client::new("ua").with_key(KEY_OPENALEX, "SECRET-KEY-VALUE");
        let shown = format!("{client:?}");
        assert!(shown.contains("openalex"));
        assert!(!shown.contains("SECRET-KEY-VALUE"));
        assert_eq!(client.key(KEY_OPENALEX), Some("SECRET-KEY-VALUE"));
    }
}
