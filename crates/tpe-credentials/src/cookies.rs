//! Per-host cookie persistence on top of any [`CredentialStore`].
//!
//! All cookies for one host are serialised as a JSON array and stored as a single
//! [`Secret`] under service [`COOKIE_SERVICE`], account = lower-cased host name.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::{CredError, CredentialStore, Secret};

/// Service name under which cookie jars are stored.
pub const COOKIE_SERVICE: &str = "tpe.cookies";

/// One HTTP cookie. `Debug` redacts `value`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cookie {
    /// Cookie name.
    pub name: String,
    /// Cookie value (often a session token: treat as secret).
    pub value: String,
    /// `Domain` attribute (or the host the cookie was set by).
    pub domain: String,
    /// `Path` attribute.
    pub path: String,
    /// Expiry as seconds since the Unix epoch; `None` for a session cookie.
    pub expires_unix: Option<i64>,
    /// `Secure` attribute.
    pub secure: bool,
    /// `HttpOnly` attribute.
    pub http_only: bool,
}

impl Cookie {
    /// Whether the cookie has expired at `now_unix` (expiry at or before now).
    pub fn is_expired_at(&self, now_unix: i64) -> bool {
        self.expires_unix.is_some_and(|expires| expires <= now_unix)
    }
}

impl fmt::Debug for Cookie {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cookie")
            .field("name", &self.name)
            .field("value", &"***")
            .field("domain", &self.domain)
            .field("path", &self.path)
            .field("expires_unix", &self.expires_unix)
            .field("secure", &self.secure)
            .field("http_only", &self.http_only)
            .finish()
    }
}

/// Cookie jar persisted in a [`CredentialStore`], one secret per host.
#[derive(Debug)]
pub struct CookieJarStore<S> {
    store: S,
}

impl<S: CredentialStore> CookieJarStore<S> {
    /// Wrap a credential store (pass `&store` to keep using it elsewhere).
    pub fn new(store: S) -> Self {
        Self { store }
    }

    /// The underlying credential store.
    pub fn store(&self) -> &S {
        &self.store
    }

    /// All stored cookies for `host`, including expired ones.
    pub fn load(&self, host: &str) -> Result<Vec<Cookie>, CredError> {
        let host = normalize_host(host);
        let Some(secret) = self.store.get(COOKIE_SERVICE, &host)? else {
            return Ok(Vec::new());
        };
        serde_json::from_str::<Vec<Cookie>>(secret.expose()).map_err(|e| {
            CredError::Format(format!(
                "stored cookies for {host} are not valid JSON (line {}, column {})",
                e.line(),
                e.column()
            ))
        })
    }

    /// Replace every cookie stored for `host`. An empty slice deletes the entry.
    pub fn save(&self, host: &str, cookies: &[Cookie]) -> Result<(), CredError> {
        let host = normalize_host(host);
        if cookies.is_empty() {
            self.store.delete(COOKIE_SERVICE, &host)?;
            return Ok(());
        }
        let json = serde_json::to_string(cookies)
            .map_err(|e| CredError::Format(format!("cannot serialise cookies: {e}")))?;
        self.store.set(COOKIE_SERVICE, &host, &Secret(json))
    }

    /// Insert `cookie` for `host`, replacing one with the same name, domain and path.
    pub fn upsert(&self, host: &str, cookie: Cookie) -> Result<(), CredError> {
        let mut cookies = self.load(host)?;
        cookies.retain(|c| {
            !(c.name == cookie.name && c.domain == cookie.domain && c.path == cookie.path)
        });
        cookies.push(cookie);
        self.save(host, &cookies)
    }

    /// Remove every cookie for `host`; returns whether any were stored.
    pub fn clear(&self, host: &str) -> Result<bool, CredError> {
        self.store.delete(COOKIE_SERVICE, &normalize_host(host))
    }

    /// Hosts that currently have a stored cookie jar.
    pub fn hosts(&self) -> Result<Vec<String>, CredError> {
        self.store.list(COOKIE_SERVICE)
    }

    /// Drop expired cookies for `host` from storage; returns how many were removed.
    pub fn purge_expired(&self, host: &str, now_unix: i64) -> Result<usize, CredError> {
        let mut cookies = self.load(host)?;
        let before = cookies.len();
        cookies.retain(|c| !c.is_expired_at(now_unix));
        let removed = before - cookies.len();
        if removed > 0 {
            self.save(host, &cookies)?;
        }
        Ok(removed)
    }

    /// `Cookie:` header value (`a=1; b=2`) for `host` at `now_unix`, skipping
    /// expired cookies; `None` when nothing is left.
    pub fn cookie_header_at(&self, host: &str, now_unix: i64) -> Result<Option<String>, CredError> {
        let pairs: Vec<String> = self
            .load(host)?
            .iter()
            .filter(|c| !c.is_expired_at(now_unix))
            .map(|c| format!("{}={}", c.name, c.value))
            .collect();
        if pairs.is_empty() {
            Ok(None)
        } else {
            Ok(Some(pairs.join("; ")))
        }
    }

    /// `Cookie:` header value for `host` using the system clock. Storage errors
    /// are treated as "no cookies" (use [`CookieJarStore::cookie_header_at`] to see them).
    pub fn cookie_header(&self, host: &str) -> Option<String> {
        self.cookie_header_at(host, now_unix()).ok().flatten()
    }
}

/// Lower-case, trim, and drop a trailing dot so `Example.org.` and `example.org` match.
fn normalize_host(host: &str) -> String {
    host.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// Seconds since the Unix epoch (0 if the clock is before 1970).
fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::{COOKIE_SERVICE, Cookie, CookieJarStore};
    use crate::{CredentialStore, MemoryStore};

    fn cookie(name: &str, value: &str, expires_unix: Option<i64>) -> Cookie {
        Cookie {
            name: name.to_owned(),
            value: value.to_owned(),
            domain: "example.org".to_owned(),
            path: "/".to_owned(),
            expires_unix,
            secure: true,
            http_only: true,
        }
    }

    #[test]
    fn cookie_expiry_filtering() {
        let jar = CookieJarStore::new(MemoryStore::new());
        jar.save(
            "example.org",
            &[
                cookie("a", "1", Some(100)),
                cookie("b", "2", Some(200)),
                cookie("c", "3", None),
            ],
        )
        .unwrap();
        assert_eq!(
            jar.cookie_header_at("example.org", 50).unwrap().as_deref(),
            Some("a=1; b=2; c=3")
        );
        assert_eq!(
            jar.cookie_header_at("example.org", 100).unwrap().as_deref(),
            Some("b=2; c=3")
        );
        assert_eq!(
            jar.cookie_header_at("example.org", 250).unwrap().as_deref(),
            Some("c=3")
        );
        // Filtering does not modify storage; purging does.
        assert_eq!(jar.load("example.org").unwrap().len(), 3);
        assert_eq!(jar.purge_expired("example.org", 250).unwrap(), 2);
        assert_eq!(jar.load("example.org").unwrap().len(), 1);
    }

    #[test]
    fn cookie_header_uses_system_clock() {
        let jar = CookieJarStore::new(MemoryStore::new());
        jar.save(
            "example.org",
            &[
                cookie("old", "x", Some(1)),
                cookie("live", "y", Some(i64::MAX)),
            ],
        )
        .unwrap();
        assert_eq!(jar.cookie_header("example.org").as_deref(), Some("live=y"));
        assert_eq!(jar.cookie_header("unknown.example"), None);
    }

    #[test]
    fn all_expired_gives_none() {
        let jar = CookieJarStore::new(MemoryStore::new());
        jar.save("example.org", &[cookie("old", "x", Some(10))])
            .unwrap();
        assert_eq!(jar.cookie_header_at("example.org", 10).unwrap(), None);
    }

    #[test]
    fn cookies_stored_as_secret_per_host() {
        let store = MemoryStore::new();
        let jar = CookieJarStore::new(&store);
        jar.upsert("Example.ORG.", cookie("sid", "abc", None))
            .unwrap();
        jar.upsert("example.org", cookie("sid", "def", None))
            .unwrap();
        jar.upsert("example.org", cookie("pref", "1", None))
            .unwrap();
        assert_eq!(
            store.list(COOKIE_SERVICE).unwrap(),
            vec!["example.org".to_owned()]
        );
        assert_eq!(jar.hosts().unwrap(), vec!["example.org".to_owned()]);
        assert_eq!(
            jar.cookie_header_at("EXAMPLE.org", 0).unwrap().as_deref(),
            Some("sid=def; pref=1")
        );
        let raw = store.get(COOKIE_SERVICE, "example.org").unwrap().unwrap();
        assert!(raw.expose().contains("\"http_only\":true"));
        assert!(jar.clear("example.org").unwrap());
        assert!(!jar.clear("example.org").unwrap());
        assert!(jar.load("example.org").unwrap().is_empty());
    }

    #[test]
    fn saving_empty_list_deletes_entry() {
        let store = MemoryStore::new();
        let jar = CookieJarStore::new(&store);
        jar.save("example.org", &[cookie("a", "1", None)]).unwrap();
        jar.save("example.org", &[]).unwrap();
        assert!(store.list(COOKIE_SERVICE).unwrap().is_empty());
    }

    #[test]
    fn cookie_debug_redacts_value() {
        let text = format!("{:?}", cookie("sid", "super-secret-session", None));
        assert!(text.contains("sid"));
        assert!(text.contains("***"));
        assert!(!text.contains("super-secret-session"));
    }
}
