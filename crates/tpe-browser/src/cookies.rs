//! Cookies: the model, `Set-Cookie` parsing, request-header building, the
//! Netscape `cookies.txt` format (what CEF, curl and browser extensions
//! exchange) and the bridge to the credential store.
//!
//! Storage contract (shared with `tpe-credentials`'s `CookieJarStore`): one
//! secret per host under service [`COOKIE_SERVICE`], account = host, secret =
//! JSON array of [`Cookie`] objects. [`CookieSecretStore`] mirrors that
//! crate's `CredentialStore` trait method-for-method with `String` in place
//! of `Secret`, so wiring the real store is a one-impl adapter.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

use crate::BrowserError;
use crate::url::{NormalizedUrl, host_in_domain};

/// Credential-store service name under which cookies live.
pub const COOKIE_SERVICE: &str = "tpe.cookies";

/// Netscape `cookies.txt` domain prefix marking an `HttpOnly` cookie (curl convention).
const HTTP_ONLY_PREFIX: &str = "#HttpOnly_";

/// One cookie. Field names and JSON shape match `tpe_credentials::Cookie`.
/// A domain starting with `.` is a domain cookie (sent to subdomains); a bare
/// domain is host-only.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cookie {
    /// Cookie name.
    pub name: String,
    /// Cookie value; never logged (see the `Debug` impl).
    pub value: String,
    /// `example.org` (host-only) or `.example.org` (domain-wide), lower-case.
    pub domain: String,
    /// Path scope, `/` by default.
    pub path: String,
    /// Expiry as Unix seconds; `None` for a session cookie.
    pub expires_unix: Option<i64>,
    /// Only sent over `https`.
    pub secure: bool,
    /// Not exposed to page scripts.
    pub http_only: bool,
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

impl Cookie {
    /// `true` when the cookie has an expiry at or before `now_unix`.
    pub fn is_expired(&self, now_unix: i64) -> bool {
        self.expires_unix.is_some_and(|t| t <= now_unix)
    }

    /// Host the cookie is stored under: the domain without its leading dot.
    pub fn storage_host(&self) -> &str {
        self.domain.trim_start_matches('.')
    }

    /// RFC 6265 request matching: domain (host-only or suffix), path prefix, `Secure`.
    pub fn matches(&self, host: &str, path: &str, secure: bool) -> bool {
        if self.secure && !secure {
            return false;
        }
        let domain_ok = self.domain.strip_prefix('.').map_or_else(
            || host.eq_ignore_ascii_case(&self.domain),
            |d| host_in_domain(host, d),
        );
        domain_ok && path_matches(&self.path, path)
    }

    /// Parse a `Set-Cookie` header received from `request_host`. Returns `None`
    /// for an empty name, a `Domain` that does not cover the request host, or
    /// a `Domain` without a dot (a public suffix such as `com`). `Max-Age`
    /// wins over `Expires`; a non-positive `Max-Age` yields a cookie that is
    /// already expired at `now_unix`.
    pub fn parse_set_cookie(header: &str, request_host: &str, now_unix: i64) -> Option<Self> {
        let mut parts = header.split(';');
        let (name, value) = parts.next()?.split_once('=')?;
        let name = name.trim();
        if name.is_empty() {
            return None;
        }
        let host = request_host.trim_end_matches('.').to_ascii_lowercase();
        let mut cookie = Self {
            name: name.to_string(),
            value: unquote(value.trim()),
            domain: host.clone(),
            path: "/".to_string(),
            expires_unix: None,
            secure: false,
            http_only: false,
        };
        let mut max_age: Option<i64> = None;
        let mut expires: Option<i64> = None;
        for part in parts {
            let (key, val) = part
                .split_once('=')
                .map_or((part.trim(), ""), |(k, v)| (k.trim(), v.trim()));
            match key.to_ascii_lowercase().as_str() {
                "expires" => expires = parse_http_date(val),
                "max-age" => max_age = val.parse::<i64>().ok(),
                "domain" => {
                    let d = val
                        .trim_start_matches('.')
                        .trim_end_matches('.')
                        .to_ascii_lowercase();
                    if d.is_empty() {
                        continue;
                    }
                    if !d.contains('.') || !host_in_domain(&host, &d) {
                        return None;
                    }
                    cookie.domain = format!(".{d}");
                }
                "path" => {
                    if val.starts_with('/') {
                        cookie.path = val.to_string();
                    }
                }
                "secure" => cookie.secure = true,
                "httponly" => cookie.http_only = true,
                _ => {}
            }
        }
        cookie.expires_unix =
            max_age.map_or(expires, |age| Some(now_unix.saturating_add(age.max(0))));
        Some(cookie)
    }

    /// One Netscape `cookies.txt` line (seven tab-separated fields).
    pub fn to_netscape_line(&self) -> String {
        let prefix = if self.http_only { HTTP_ONLY_PREFIX } else { "" };
        let domain_wide = if self.domain.starts_with('.') {
            "TRUE"
        } else {
            "FALSE"
        };
        let secure = if self.secure { "TRUE" } else { "FALSE" };
        let expires = self.expires_unix.unwrap_or(0);
        format!(
            "{prefix}{}\t{domain_wide}\t{}\t{secure}\t{expires}\t{}\t{}",
            self.domain, self.path, self.name, self.value
        )
    }

    /// Parse one Netscape `cookies.txt` line; `None` for comments, blank lines
    /// and lines without exactly seven fields.
    pub fn parse_netscape_line(line: &str) -> Option<Self> {
        let (http_only, body) = line
            .strip_prefix(HTTP_ONLY_PREFIX)
            .map_or((false, line), |rest| (true, rest));
        if body.trim().is_empty() || body.starts_with('#') {
            return None;
        }
        let fields: Vec<&str> = body.split('\t').collect();
        let &[domain, domain_wide, path, secure, expires, name, value] = fields.as_slice() else {
            return None;
        };
        if name.is_empty() {
            return None;
        }
        let mut domain = domain.trim().to_ascii_lowercase();
        if domain_wide.eq_ignore_ascii_case("TRUE") && !domain.starts_with('.') {
            domain.insert(0, '.');
        }
        let expires_unix = expires.trim().parse::<i64>().ok().filter(|t| *t > 0);
        Some(Self {
            name: name.to_string(),
            value: value.to_string(),
            domain,
            path: if path.is_empty() {
                "/".to_string()
            } else {
                path.to_string()
            },
            expires_unix,
            secure: secure.eq_ignore_ascii_case("TRUE"),
            http_only,
        })
    }
}

/// Build a `Cookie:` header value from `cookies` for a request to `url`,
/// dropping expired and non-matching cookies; longer paths first (RFC 6265
/// 5.4), otherwise original order.
pub fn cookie_header(cookies: &[&Cookie], url: &NormalizedUrl, now_unix: i64) -> Option<String> {
    let mut chosen: Vec<&Cookie> = cookies
        .iter()
        .copied()
        .filter(|c| !c.is_expired(now_unix) && c.matches(&url.host, &url.path, url.is_secure()))
        .collect();
    chosen.sort_by_key(|c| std::cmp::Reverse(c.path.len()));
    if chosen.is_empty() {
        return None;
    }
    let pairs: Vec<String> = chosen
        .iter()
        .map(|c| format!("{}={}", c.name, c.value))
        .collect();
    Some(pairs.join("; "))
}

/// Cookies grouped by storage host (domain without leading dot).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CookieJar {
    by_host: BTreeMap<String, Vec<Cookie>>,
}

impl CookieJar {
    /// An empty jar.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace (same name, domain and path).
    pub fn insert(&mut self, cookie: Cookie) {
        let host = cookie.storage_host().to_string();
        let list = self.by_host.entry(host).or_default();
        if let Some(existing) = list
            .iter_mut()
            .find(|c| c.name == cookie.name && c.domain == cookie.domain && c.path == cookie.path)
        {
            *existing = cookie;
        } else {
            list.push(cookie);
        }
    }

    /// Number of cookies.
    pub fn len(&self) -> usize {
        self.by_host.values().map(Vec::len).sum()
    }

    /// `true` when there are no cookies.
    pub fn is_empty(&self) -> bool {
        self.by_host.values().all(Vec::is_empty)
    }

    /// Storage hosts in sorted order.
    pub fn hosts(&self) -> Vec<&str> {
        self.by_host.keys().map(String::as_str).collect()
    }

    /// Every cookie, grouped by host in sorted host order.
    pub fn all(&self) -> Vec<&Cookie> {
        self.by_host.values().flatten().collect()
    }

    /// Cookies whose domain covers `host` (host-only for that host, or a
    /// domain cookie of it or a parent domain), expired ones included.
    pub fn cookies_for_host(&self, host: &str) -> Vec<&Cookie> {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        self.by_host
            .iter()
            .filter(|(key, _)| host_in_domain(&host, key))
            .flat_map(|(_, list)| list.iter())
            .filter(|c| {
                c.domain
                    .strip_prefix('.')
                    .map_or_else(|| c.domain == host, |d| host_in_domain(&host, d))
            })
            .collect()
    }

    /// `Cookie:` header for a request to `url`, or `None`.
    pub fn header_for(&self, url: &NormalizedUrl, now_unix: i64) -> Option<String> {
        let candidates = self.cookies_for_host(&url.host);
        cookie_header(&candidates, url, now_unix)
    }

    /// Remove expired cookies; returns how many were dropped.
    pub fn remove_expired(&mut self, now_unix: i64) -> usize {
        let before = self.len();
        for list in self.by_host.values_mut() {
            list.retain(|c| !c.is_expired(now_unix));
        }
        self.by_host.retain(|_, list| !list.is_empty());
        before - self.len()
    }

    /// Netscape `cookies.txt` text, one line per cookie plus the standard header.
    pub fn to_netscape(&self) -> String {
        let mut out = String::from("# Netscape HTTP Cookie File\n");
        for cookie in self.all() {
            out.push_str(&cookie.to_netscape_line());
            out.push('\n');
        }
        out
    }

    /// Parse Netscape `cookies.txt` text. Comments and blank lines are
    /// skipped; any other line that is not seven tab-separated fields is an error.
    pub fn parse_netscape(text: &str) -> Result<Self, BrowserError> {
        let mut jar = Self::new();
        for (idx, line) in text.lines().enumerate() {
            let body = line.strip_prefix(HTTP_ONLY_PREFIX).unwrap_or(line);
            if body.trim().is_empty() || body.starts_with('#') {
                continue;
            }
            let Some(cookie) = Cookie::parse_netscape_line(line) else {
                return Err(BrowserError::CookieLine {
                    line: idx + 1,
                    reason: format!(
                        "expected 7 tab-separated fields, found {}",
                        body.split('\t').count()
                    ),
                });
            };
            jar.insert(cookie);
        }
        Ok(jar)
    }

    /// Write every host's cookies to `store` as JSON under service
    /// [`COOKIE_SERVICE`], account = host. Returns the number of hosts written.
    pub fn export_to_store(&self, store: &dyn CookieSecretStore) -> Result<usize, BrowserError> {
        let mut written = 0;
        for (host, cookies) in &self.by_host {
            let json = serde_json::to_string(cookies)?;
            store.set(COOKIE_SERVICE, host, &json)?;
            written += 1;
        }
        Ok(written)
    }

    /// Load every host's cookies from `store`.
    pub fn import_from_store(store: &dyn CookieSecretStore) -> Result<Self, BrowserError> {
        let mut jar = Self::new();
        for host in store.list(COOKIE_SERVICE)? {
            if let Some(json) = store.get(COOKIE_SERVICE, &host)? {
                let cookies: Vec<Cookie> = serde_json::from_str(&json)?;
                for cookie in cookies {
                    jar.insert(cookie);
                }
            }
        }
        Ok(jar)
    }
}

/// Mirror of `tpe_credentials::CredentialStore` (get/set/delete/list keyed by
/// service and account) with `String` secrets. Implement it for the real
/// store by wrapping/unwrapping `Secret`.
pub trait CookieSecretStore {
    /// The secret for `service`/`account`, if any.
    fn get(&self, service: &str, account: &str) -> Result<Option<String>, BrowserError>;
    /// Store or replace the secret for `service`/`account`.
    fn set(&self, service: &str, account: &str, secret: &str) -> Result<(), BrowserError>;
    /// Remove the secret for `service`/`account`; not an error when absent.
    fn delete(&self, service: &str, account: &str) -> Result<(), BrowserError>;
    /// All accounts under `service`, sorted.
    fn list(&self, service: &str) -> Result<Vec<String>, BrowserError>;
}

type Entries = BTreeMap<(String, String), String>;

/// In-memory [`CookieSecretStore`] for tests and for sessions that must not persist.
#[derive(Default)]
pub struct MemoryCookieStore {
    entries: Mutex<Entries>,
}

impl fmt::Debug for MemoryCookieStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemoryCookieStore")
            .field("entries", &"***")
            .finish()
    }
}

impl MemoryCookieStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<MutexGuard<'_, Entries>, BrowserError> {
        self.entries
            .lock()
            .map_err(|_| BrowserError::Store("memory store poisoned".to_string()))
    }
}

impl CookieSecretStore for MemoryCookieStore {
    fn get(&self, service: &str, account: &str) -> Result<Option<String>, BrowserError> {
        let entries = self.lock()?;
        Ok(entries
            .get(&(service.to_string(), account.to_string()))
            .cloned())
    }

    fn set(&self, service: &str, account: &str, secret: &str) -> Result<(), BrowserError> {
        let mut entries = self.lock()?;
        entries.insert(
            (service.to_string(), account.to_string()),
            secret.to_string(),
        );
        Ok(())
    }

    fn delete(&self, service: &str, account: &str) -> Result<(), BrowserError> {
        let mut entries = self.lock()?;
        entries.remove(&(service.to_string(), account.to_string()));
        Ok(())
    }

    fn list(&self, service: &str) -> Result<Vec<String>, BrowserError> {
        let entries = self.lock()?;
        Ok(entries
            .keys()
            .filter(|(s, _)| s == service)
            .map(|(_, account)| account.clone())
            .collect())
    }
}

fn path_matches(cookie_path: &str, request_path: &str) -> bool {
    request_path
        .strip_prefix(cookie_path)
        .is_some_and(|rest| rest.is_empty() || cookie_path.ends_with('/') || rest.starts_with('/'))
}

fn unquote(value: &str) -> String {
    value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or(value)
        .to_string()
}

/// Parse an HTTP date (RFC 1123, RFC 850 or asctime) with the order-independent
/// RFC 6265 5.1.1 algorithm: a time token, a day token, a month token and a
/// year token in any order.
fn parse_http_date(s: &str) -> Option<i64> {
    let mut time: Option<(i64, i64, i64)> = None;
    let mut day: Option<i64> = None;
    let mut month: Option<i64> = None;
    let mut year: Option<i64> = None;
    for token in s.split([' ', ',', '-', '\t']).filter(|t| !t.is_empty()) {
        if time.is_none()
            && let Some(t) = parse_hms(token)
        {
            time = Some(t);
        } else if day.is_none()
            && let Some(d) = parse_digits(token, 1, 2)
        {
            day = Some(d);
        } else if month.is_none()
            && let Some(m) = month_number(token)
        {
            month = Some(m);
        } else if year.is_none()
            && let Some(y) = parse_digits(token, 2, 4)
        {
            year = Some(y);
        }
    }
    let (hour, minute, second) = time?;
    let (day, month, mut year) = (day?, month?, year?);
    if (70..=99).contains(&year) {
        year += 1900;
    } else if year < 70 {
        year += 2000;
    }
    if year < 1601 || !(1..=31).contains(&day) {
        return None;
    }
    let days = days_from_civil(year, month, day);
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second)
}

fn parse_hms(token: &str) -> Option<(i64, i64, i64)> {
    let mut parts = token.split(':');
    let h = parse_digits(parts.next()?, 1, 2)?;
    let m = parse_digits(parts.next()?, 1, 2)?;
    let s = parse_digits(parts.next()?, 1, 2)?;
    if parts.next().is_some() || h > 23 || m > 59 || s > 59 {
        return None;
    }
    Some((h, m, s))
}

fn parse_digits(token: &str, min: usize, max: usize) -> Option<i64> {
    let digits: String = token.chars().take_while(char::is_ascii_digit).collect();
    if digits.len() < min || digits.len() > max {
        return None;
    }
    digits.parse().ok()
}

fn month_number(token: &str) -> Option<i64> {
    let lower = token.to_ascii_lowercase();
    let prefix = lower.get(..3)?;
    let months = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    months
        .iter()
        .position(|m| *m == prefix)
        .and_then(|i| i64::try_from(i + 1).ok())
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let shifted = if y >= 0 { y } else { y - 399 };
    let era = shifted / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_700_000_000;

    fn url(s: &str) -> NormalizedUrl {
        NormalizedUrl::parse(s).unwrap()
    }

    fn cookie(name: &str, domain: &str, path: &str) -> Cookie {
        Cookie {
            name: name.to_string(),
            value: format!("v-{name}"),
            domain: domain.to_string(),
            path: path.to_string(),
            expires_unix: None,
            secure: false,
            http_only: false,
        }
    }

    #[test]
    fn debug_redacts_value() {
        let c = cookie("sid", ".example.org", "/");
        let text = format!("{c:?}");
        assert!(text.contains("name: \"sid\""));
        assert!(text.contains("value: \"***\""));
        assert!(!text.contains("v-sid"));
    }

    #[test]
    fn set_cookie_basic_attributes() {
        let c = Cookie::parse_set_cookie(
            "ezproxy=abc123; Path=/; Domain=.ezproxy.lib.edu; Secure; HttpOnly; Max-Age=3600",
            "login.ezproxy.lib.edu",
            NOW,
        )
        .unwrap();
        assert_eq!(c.name, "ezproxy");
        assert_eq!(c.value, "abc123");
        assert_eq!(c.domain, ".ezproxy.lib.edu");
        assert_eq!(c.path, "/");
        assert!(c.secure && c.http_only);
        assert_eq!(c.expires_unix, Some(NOW + 3600));
        assert!(!c.is_expired(NOW + 3599));
        assert!(c.is_expired(NOW + 3600));
    }

    #[test]
    fn set_cookie_defaults_and_rejections() {
        let c = Cookie::parse_set_cookie("a=\"quoted\"", "WWW.Example.org.", NOW).unwrap();
        assert_eq!(c.domain, "www.example.org");
        assert_eq!(c.value, "quoted");
        assert_eq!(c.path, "/");
        assert_eq!(c.expires_unix, None);
        assert!(Cookie::parse_set_cookie("=novalue", "x.org", NOW).is_none());
        assert!(Cookie::parse_set_cookie("nothing", "x.org", NOW).is_none());
        assert!(Cookie::parse_set_cookie("a=1; Domain=other.org", "x.org", NOW).is_none());
        assert!(Cookie::parse_set_cookie("a=1; Domain=org", "x.org", NOW).is_none());
        let sub =
            Cookie::parse_set_cookie("a=1; Domain=x.org; Path=relative", "a.x.org", NOW).unwrap();
        assert_eq!(sub.domain, ".x.org");
        assert_eq!(sub.path, "/");
    }

    #[test]
    fn set_cookie_expiry_forms() {
        let exp =
            Cookie::parse_set_cookie("a=1; Expires=Wed, 21 Oct 2015 07:28:00 GMT", "x.org", NOW)
                .unwrap();
        assert_eq!(exp.expires_unix, Some(1_445_412_480));
        let rfc850 = Cookie::parse_set_cookie(
            "a=1; expires=Wednesday, 21-Oct-15 07:28:00 GMT",
            "x.org",
            NOW,
        )
        .unwrap();
        assert_eq!(rfc850.expires_unix, Some(1_445_412_480));
        let asctime =
            Cookie::parse_set_cookie("a=1; Expires=Wed Oct 21 07:28:00 2015", "x.org", NOW)
                .unwrap();
        assert_eq!(asctime.expires_unix, Some(1_445_412_480));
        let max_age_wins = Cookie::parse_set_cookie(
            "a=1; Expires=Wed, 21 Oct 2015 07:28:00 GMT; Max-Age=10",
            "x.org",
            NOW,
        )
        .unwrap();
        assert_eq!(max_age_wins.expires_unix, Some(NOW + 10));
        let deleted = Cookie::parse_set_cookie("a=; Max-Age=0", "x.org", NOW).unwrap();
        assert!(deleted.is_expired(NOW));
        let negative = Cookie::parse_set_cookie("a=1; Max-Age=-5", "x.org", NOW).unwrap();
        assert!(negative.is_expired(NOW));
        let garbage = Cookie::parse_set_cookie("a=1; Expires=never", "x.org", NOW).unwrap();
        assert_eq!(garbage.expires_unix, None);
    }

    #[test]
    fn http_dates() {
        assert_eq!(parse_http_date("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        assert_eq!(
            parse_http_date("Fri, 31 Dec 1999 23:59:59 GMT"),
            Some(946_684_799)
        );
        assert_eq!(
            parse_http_date("Sat, 29 Feb 2020 12:00:00 GMT"),
            Some(1_582_977_600)
        );
        assert_eq!(
            parse_http_date("Tue, 19 Jan 2038 03:14:08 GMT"),
            Some(2_147_483_648)
        );
        assert_eq!(parse_http_date("01 Jan 1600 00:00:00"), None);
        assert_eq!(parse_http_date("Jan 2015 07:28:00"), None);
        assert_eq!(parse_http_date(""), None);
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
    }

    #[test]
    fn matching_rules() {
        let domain_cookie = cookie("d", ".example.org", "/");
        assert!(domain_cookie.matches("example.org", "/", false));
        assert!(domain_cookie.matches("a.b.example.org", "/x", false));
        assert!(!domain_cookie.matches("notexample.org", "/", false));

        let host_only = cookie("h", "example.org", "/");
        assert!(host_only.matches("example.org", "/", false));
        assert!(!host_only.matches("www.example.org", "/", false));

        let pathed = cookie("p", "example.org", "/docs");
        assert!(pathed.matches("example.org", "/docs", false));
        assert!(pathed.matches("example.org", "/docs/a", false));
        assert!(!pathed.matches("example.org", "/documents", false));
        assert!(!pathed.matches("example.org", "/", false));

        let mut secure = cookie("s", "example.org", "/");
        secure.secure = true;
        assert!(secure.matches("example.org", "/", true));
        assert!(!secure.matches("example.org", "/", false));
    }

    #[test]
    fn jar_insert_replaces_and_counts() {
        let mut jar = CookieJar::new();
        assert!(jar.is_empty());
        jar.insert(cookie("a", ".x.org", "/"));
        jar.insert(cookie("a", ".x.org", "/"));
        jar.insert(cookie("a", ".x.org", "/other"));
        jar.insert(cookie("a", "x.org", "/"));
        jar.insert(cookie("b", "y.org", "/"));
        assert_eq!(jar.len(), 4);
        assert_eq!(jar.hosts(), ["x.org", "y.org"]);
        assert_eq!(jar.cookies_for_host("x.org").len(), 3);
        assert_eq!(jar.cookies_for_host("sub.x.org").len(), 2);
        assert_eq!(jar.cookies_for_host("z.org").len(), 0);
        assert_eq!(jar.all().len(), 4);
    }

    #[test]
    fn header_orders_longer_paths_first_and_drops_expired() {
        let mut jar = CookieJar::new();
        jar.insert(cookie("root", ".x.org", "/"));
        jar.insert(cookie("deep", ".x.org", "/a/b"));
        let mut gone = cookie("gone", ".x.org", "/");
        gone.expires_unix = Some(NOW - 1);
        jar.insert(gone);
        let mut secure = cookie("sec", ".x.org", "/");
        secure.secure = true;
        jar.insert(secure);
        assert_eq!(
            jar.header_for(&url("https://www.x.org/a/b/c"), NOW),
            Some("deep=v-deep; root=v-root; sec=v-sec".to_string())
        );
        assert_eq!(
            jar.header_for(&url("http://www.x.org/"), NOW),
            Some("root=v-root".to_string())
        );
        assert_eq!(jar.header_for(&url("https://other.org/"), NOW), None);
        assert_eq!(jar.remove_expired(NOW), 1);
        assert_eq!(jar.len(), 3);
    }

    #[test]
    fn netscape_round_trip() {
        let mut jar = CookieJar::new();
        let mut a = cookie("a", ".x.org", "/");
        a.expires_unix = Some(1_800_000_000);
        a.secure = true;
        a.http_only = true;
        jar.insert(a);
        jar.insert(cookie("b", "y.org", "/p"));
        let text = jar.to_netscape();
        assert!(text.starts_with("# Netscape HTTP Cookie File\n"));
        assert!(text.contains("#HttpOnly_.x.org\tTRUE\t/\tTRUE\t1800000000\ta\tv-a\n"));
        assert!(text.contains("y.org\tFALSE\t/p\tFALSE\t0\tb\tv-b\n"));
        let back = CookieJar::parse_netscape(&text).unwrap();
        assert_eq!(back, jar);
    }

    #[test]
    fn netscape_parsing_errors_and_leniency() {
        let ok =
            CookieJar::parse_netscape("# comment\n\n.x.org\tTRUE\t/\tFALSE\t0\tn\tv\n").unwrap();
        assert_eq!(ok.len(), 1);
        let flagged = CookieJar::parse_netscape("x.org\tTRUE\t/\tFALSE\t0\tn\tv\n").unwrap();
        assert_eq!(flagged.all()[0].domain, ".x.org");
        let err = CookieJar::parse_netscape("# ok\nx.org\tTRUE\t/\n").unwrap_err();
        match err {
            BrowserError::CookieLine { line, reason } => {
                assert_eq!(line, 2);
                assert!(reason.contains("found 3"));
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(Cookie::parse_netscape_line("# comment"), None);
        assert_eq!(Cookie::parse_netscape_line(""), None);
    }

    #[test]
    fn store_round_trip_uses_contract_layout() {
        let store = MemoryCookieStore::new();
        let mut jar = CookieJar::new();
        jar.insert(cookie("a", ".x.org", "/"));
        jar.insert(cookie("b", "x.org", "/"));
        jar.insert(cookie("c", "y.org", "/"));
        assert_eq!(jar.export_to_store(&store).unwrap(), 2);
        assert_eq!(store.list(COOKIE_SERVICE).unwrap(), ["x.org", "y.org"]);
        let json = store.get(COOKIE_SERVICE, "x.org").unwrap().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed[0]["name"], "a");
        assert_eq!(parsed[0]["domain"], ".x.org");
        assert_eq!(parsed[0]["expires_unix"], serde_json::Value::Null);
        assert_eq!(parsed[0]["http_only"], false);
        let back = CookieJar::import_from_store(&store).unwrap();
        assert_eq!(back, jar);
        store.delete(COOKIE_SERVICE, "y.org").unwrap();
        assert_eq!(CookieJar::import_from_store(&store).unwrap().len(), 2);
        assert!(store.list("other").unwrap().is_empty());
        assert!(format!("{store:?}").contains("***"));
    }

    #[test]
    fn corrupt_store_entry_is_an_error() {
        let store = MemoryCookieStore::new();
        store.set(COOKIE_SERVICE, "x.org", "not json").unwrap();
        assert!(matches!(
            CookieJar::import_from_store(&store),
            Err(BrowserError::Json(_))
        ));
    }
}
