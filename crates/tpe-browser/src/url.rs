//! Strict normalisation of browsable (`http`/`https`) URLs.
//!
//! The normal form is: lower-case scheme and host, no trailing dot on the
//! host, no default port, dot-segments resolved and empty segments collapsed
//! in the path, upper-case hex in percent escapes, tracking parameters
//! removed, fragment removed, credentials (`user:pass@`) dropped. The path
//! and query are otherwise kept byte-for-byte (no decoding), because
//! publishers do distinguish `%2F` from `/` inside DOIs.

use std::fmt;

use crate::BrowserError;

/// Query parameters dropped during normalisation because they only track the
/// visitor and never select content. An entry ending in `*` is a prefix.
pub const TRACKING_PARAMS: &[&str] = &[
    "utm_*",
    "fbclid",
    "gclid",
    "dclid",
    "gbraid",
    "wbraid",
    "msclkid",
    "mc_cid",
    "mc_eid",
    "igshid",
    "yclid",
    "_hsenc",
    "_hsmi",
    "mkt_tok",
    "oly_anon_id",
    "oly_enc_id",
    "vero_id",
    "_ga",
    "ref_src",
    "ref_url",
    "cmpid",
    "s_cid",
    "ncid",
];

/// A parsed, normalised `http`/`https` URL.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct NormalizedUrl {
    /// `http` or `https`.
    pub scheme: String,
    /// Lower-case host without trailing dot; `IPv6` literals keep their brackets.
    pub host: String,
    /// Explicit port, `None` when it is the scheme default.
    pub port: Option<u16>,
    /// Absolute path, always starting with `/`.
    pub path: String,
    /// Query pairs in original order, tracking parameters removed, values raw.
    pub query: Vec<(String, String)>,
}

impl NormalizedUrl {
    /// Parse and normalise `raw`. A missing scheme is accepted when the text
    /// starts with something that looks like a host (`doi.org/10.1000/x`) and
    /// then defaults to `https`.
    pub fn parse(raw: &str) -> Result<Self, BrowserError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(BrowserError::InvalidUrl("empty".to_string()));
        }
        if trimmed.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(BrowserError::InvalidUrl(
                "contains whitespace or control characters".to_string(),
            ));
        }
        let (scheme, rest) = split_scheme(trimmed)?;
        if scheme != "http" && scheme != "https" {
            return Err(BrowserError::UnsupportedScheme(scheme));
        }
        let without_fragment = rest.split_once('#').map_or(rest, |(before, _)| before);
        let (authority_and_path, query) = without_fragment
            .split_once('?')
            .unwrap_or((without_fragment, ""));
        let (authority, path) = authority_and_path
            .find('/')
            .map_or((authority_and_path, ""), |i| {
                (&authority_and_path[..i], &authority_and_path[i..])
            });
        let (host, port) = parse_authority(authority, &scheme)?;
        Ok(Self {
            scheme,
            host,
            port,
            path: normalize_path(path),
            query: parse_query(query),
        })
    }

    /// `true` for `https`.
    pub fn is_secure(&self) -> bool {
        self.scheme == "https"
    }

    /// `scheme://host[:port]` without a trailing slash.
    pub fn origin(&self) -> String {
        let Self { scheme, host, .. } = self;
        if let Some(port) = self.port {
            format!("{scheme}://{host}:{port}")
        } else {
            format!("{scheme}://{host}")
        }
    }

    /// First value of the query parameter `key` (case-insensitive key match).
    pub fn query_param(&self, key: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    }

    /// Non-empty path segments, still percent-encoded.
    pub fn path_segments(&self) -> Vec<&str> {
        self.path.split('/').filter(|s| !s.is_empty()).collect()
    }

    /// Lower-case extension of the last path segment (`pdf` for `/x/paper.PDF`),
    /// after percent-decoding. `None` when the last segment has no dot or is a
    /// bare directory.
    pub fn extension(&self) -> Option<String> {
        if self.path.ends_with('/') {
            return None;
        }
        let last = self.path_segments().last().copied()?;
        let decoded = percent_decode(last);
        let (_, ext) = decoded.rsplit_once('.')?;
        if ext.is_empty() || ext.len() > 8 || !ext.chars().all(|c| c.is_ascii_alphanumeric()) {
            return None;
        }
        Some(ext.to_ascii_lowercase())
    }

    /// Resolve `href` (absolute, protocol-relative, root-relative or relative)
    /// against this URL, as a browser would for an `<a href>`.
    pub fn resolve(&self, href: &str) -> Result<Self, BrowserError> {
        let href = href.trim();
        if href.is_empty() {
            return Ok(self.clone());
        }
        if href.contains("://") || has_scheme_prefix(href) {
            return Self::parse(href);
        }
        if let Some(rest) = href.strip_prefix("//") {
            return Self::parse(&format!("{}://{rest}", self.scheme));
        }
        let target = if href.starts_with('/') {
            href.to_string()
        } else if href.starts_with(['?', '#']) {
            format!("{}{href}", self.path)
        } else {
            let base_dir = self.path.rfind('/').map_or("/", |i| &self.path[..=i]);
            format!("{base_dir}{href}")
        };
        Self::parse(&format!("{}{target}", self.origin()))
    }
}

impl fmt::Display for NormalizedUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.origin())?;
        f.write_str(&self.path)?;
        for (i, (k, v)) in self.query.iter().enumerate() {
            f.write_str(if i == 0 { "?" } else { "&" })?;
            write!(f, "{k}={v}")?;
        }
        Ok(())
    }
}

/// Parse and return the canonical string form in one step.
pub fn normalize_url(raw: &str) -> Result<String, BrowserError> {
    NormalizedUrl::parse(raw).map(|u| u.to_string())
}

/// `true` when `host` is `domain` itself or a subdomain of it. Both sides are
/// compared case-insensitively; a leading dot on `domain` is ignored.
pub fn host_in_domain(host: &str, domain: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let domain = domain
        .trim_start_matches('.')
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if domain.is_empty() {
        return false;
    }
    host == domain
        || host
            .strip_suffix(domain.as_str())
            .is_some_and(|p| p.ends_with('.'))
}

/// Decode `%XX` escapes; invalid escapes are kept literally and invalid UTF-8
/// becomes U+FFFD.
pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        if b == b'%' {
            let hi = bytes.get(i + 1).copied().and_then(hex_value);
            let lo = bytes.get(i + 2).copied().and_then(hex_value);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(b);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Percent-encode everything except RFC 3986 unreserved characters, as needed
/// for a value inside a query string (`login?url=<encoded>`).
pub fn percent_encode_component(s: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push('%');
            out.push(char::from(HEX[usize::from(b >> 4)]));
            out.push(char::from(HEX[usize::from(b & 0x0f)]));
        }
    }
    out
}

fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Upper-case the hex digits of valid percent escapes; leave everything else.
fn canonical_percent(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while let Some(&c) = chars.get(i) {
        let hi = chars.get(i + 1).copied();
        let lo = chars.get(i + 2).copied();
        if c == '%'
            && let (Some(hi), Some(lo)) = (hi, lo)
            && hi.is_ascii_hexdigit()
            && lo.is_ascii_hexdigit()
        {
            out.push('%');
            out.push(hi.to_ascii_uppercase());
            out.push(lo.to_ascii_uppercase());
            i += 3;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

fn is_scheme_text(s: &str) -> bool {
    !s.is_empty()
        && s.starts_with(|c: char| c.is_ascii_alphabetic())
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// `mailto:x`, `javascript:...`, `data:...`: a scheme without `//`.
fn has_scheme_prefix(s: &str) -> bool {
    s.split_once(':')
        .is_some_and(|(scheme, _)| is_scheme_text(scheme) && scheme.len() > 1)
}

fn split_scheme(s: &str) -> Result<(String, &str), BrowserError> {
    if let Some((scheme, rest)) = s.split_once("://") {
        if !is_scheme_text(scheme) {
            return Err(BrowserError::InvalidUrl(format!("bad scheme `{scheme}`")));
        }
        return Ok((scheme.to_ascii_lowercase(), rest));
    }
    if has_scheme_prefix(s) {
        let scheme = s.split_once(':').map_or("", |(scheme, _)| scheme);
        return Err(BrowserError::UnsupportedScheme(scheme.to_ascii_lowercase()));
    }
    let bare = s.strip_prefix("//").unwrap_or(s);
    let first = bare.find(['/', '?', '#']).map_or(bare, |i| &bare[..i]);
    if looks_like_host(first) {
        Ok(("https".to_string(), bare))
    } else {
        Err(BrowserError::InvalidUrl(format!("no scheme in `{s}`")))
    }
}

fn looks_like_host(s: &str) -> bool {
    if s.contains('@') {
        return false;
    }
    let host = s.rsplit_once(':').map_or(s, |(h, port)| {
        if port.chars().all(|c| c.is_ascii_digit()) && !port.is_empty() {
            h
        } else {
            s
        }
    });
    host.contains('.') && validate_host(host).is_ok()
}

fn validate_host(host: &str) -> Result<(), BrowserError> {
    if host.is_empty() {
        return Err(BrowserError::InvalidUrl("empty host".to_string()));
    }
    if host.starts_with('[') {
        let ok = host.ends_with(']')
            && host.len() > 3
            && host
                .chars()
                .all(|c| c.is_ascii_hexdigit() || matches!(c, ':' | '.' | '[' | ']'));
        return if ok {
            Ok(())
        } else {
            Err(BrowserError::InvalidUrl(format!(
                "bad IPv6 literal `{host}`"
            )))
        };
    }
    for label in host.split('.') {
        let ok = !label.is_empty()
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label.chars().all(|c| c.is_alphanumeric() || c == '-');
        if !ok {
            return Err(BrowserError::InvalidUrl(format!("bad host `{host}`")));
        }
    }
    Ok(())
}

fn parse_authority(authority: &str, scheme: &str) -> Result<(String, Option<u16>), BrowserError> {
    // Credentials never survive normalisation: they belong in the cookie jar
    // or the credential store, not in history or logs.
    let hostport = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let (host, port_text) = if hostport.starts_with('[') {
        let Some((inner, after)) = hostport.split_once(']') else {
            return Err(BrowserError::InvalidUrl(
                "unterminated IPv6 literal".to_string(),
            ));
        };
        let port_text = match after.strip_prefix(':') {
            Some(p) => Some(p),
            None if after.is_empty() => None,
            None => {
                return Err(BrowserError::InvalidUrl(format!(
                    "junk after IPv6 literal `{hostport}`"
                )));
            }
        };
        (format!("{inner}]"), port_text)
    } else {
        hostport
            .rsplit_once(':')
            .map_or((hostport.to_string(), None), |(h, p)| {
                (h.to_string(), Some(p))
            })
    };
    let host = host.trim_end_matches('.').to_lowercase();
    validate_host(&host)?;
    let Some(p) = port_text else {
        return Ok((host, None));
    };
    let n: u16 = p
        .parse()
        .map_err(|_| BrowserError::InvalidUrl(format!("bad port `{p}`")))?;
    if n == 0 {
        return Err(BrowserError::InvalidUrl("port 0".to_string()));
    }
    let default = if scheme == "https" { 443 } else { 80 };
    let port = if n == default { None } else { Some(n) };
    Ok((host, port))
}

fn normalize_path(path: &str) -> String {
    let trailing_slash = path.ends_with('/') || path.ends_with("/.") || path.ends_with("/..");
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    let mut joined = String::from("/");
    joined.push_str(&segments.join("/"));
    if trailing_slash && !segments.is_empty() {
        joined.push('/');
    }
    canonical_percent(&joined)
}

fn is_tracking_param(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    TRACKING_PARAMS.iter().any(|pattern| {
        pattern
            .strip_suffix('*')
            .map_or(lower.as_str() == *pattern, |prefix| {
                lower.starts_with(prefix)
            })
    })
}

fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            if k.is_empty() || is_tracking_param(k) {
                None
            } else {
                Some((canonical_percent(k), canonical_percent(v)))
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowercases_scheme_and_host_and_drops_fragment() {
        let u = NormalizedUrl::parse("HTTPS://WWW.Nature.COM./articles/s41586-020-2649-2#Abs1")
            .unwrap();
        assert_eq!(u.scheme, "https");
        assert_eq!(u.host, "www.nature.com");
        assert_eq!(u.port, None);
        assert_eq!(
            u.to_string(),
            "https://www.nature.com/articles/s41586-020-2649-2"
        );
    }

    #[test]
    fn default_ports_are_dropped_and_others_kept() {
        assert_eq!(
            normalize_url("http://example.org:80/a").unwrap(),
            "http://example.org/a"
        );
        assert_eq!(
            normalize_url("https://example.org:443/a").unwrap(),
            "https://example.org/a"
        );
        assert_eq!(
            normalize_url("https://example.org:8443/a").unwrap(),
            "https://example.org:8443/a"
        );
        assert!(normalize_url("https://example.org:0/").is_err());
        assert!(normalize_url("https://example.org:abc/").is_err());
    }

    #[test]
    fn scheme_less_text_defaults_to_https() {
        assert_eq!(
            normalize_url("doi.org/10.1000/xyz").unwrap(),
            "https://doi.org/10.1000/xyz"
        );
        assert_eq!(
            normalize_url("//arxiv.org/abs/2502.00857").unwrap(),
            "https://arxiv.org/abs/2502.00857"
        );
        assert!(normalize_url("not a url").is_err());
        assert!(normalize_url("localhost/x").is_err());
    }

    #[test]
    fn non_web_schemes_are_refused() {
        for bad in [
            "javascript:alert(1)",
            "file:///etc/passwd",
            "data:text/html,hi",
            "mailto:a@b.org",
            "ftp://ftp.example.org/x",
            "chrome://settings",
        ] {
            assert!(
                matches!(
                    NormalizedUrl::parse(bad),
                    Err(BrowserError::UnsupportedScheme(_))
                ),
                "{bad}"
            );
        }
    }

    #[test]
    fn credentials_are_dropped() {
        let u = NormalizedUrl::parse("https://alice:secret@proxy.lib.edu/login").unwrap();
        assert_eq!(u.host, "proxy.lib.edu");
        assert!(!u.to_string().contains("secret"));
    }

    #[test]
    fn path_dot_segments_and_empty_segments_normalise() {
        assert_eq!(
            normalize_url("https://x.org/a/./b//../c/").unwrap(),
            "https://x.org/a/c/"
        );
        assert_eq!(normalize_url("https://x.org").unwrap(), "https://x.org/");
        assert_eq!(
            normalize_url("https://x.org/../..").unwrap(),
            "https://x.org/"
        );
        assert_eq!(
            normalize_url("https://x.org/doi/10.1000%2fabc").unwrap(),
            "https://x.org/doi/10.1000%2Fabc"
        );
    }

    #[test]
    fn tracking_params_are_removed_but_order_kept() {
        let u = NormalizedUrl::parse(
            "https://x.org/p?b=2&utm_source=tw&a=1&fbclid=zzz&casa_token=keep&=novalue",
        )
        .unwrap();
        assert_eq!(u.to_string(), "https://x.org/p?b=2&a=1&casa_token=keep");
        assert_eq!(u.query_param("A"), Some("1"));
        assert_eq!(u.query_param("utm_source"), None);
    }

    #[test]
    fn ipv6_literals_parse() {
        let u = NormalizedUrl::parse("http://[::1]:8080/x").unwrap();
        assert_eq!(u.host, "[::1]");
        assert_eq!(u.port, Some(8080));
        assert_eq!(u.origin(), "http://[::1]:8080");
        assert!(NormalizedUrl::parse("http://[::1/x").is_err());
    }

    #[test]
    fn whitespace_inside_is_rejected_but_around_is_trimmed() {
        assert!(NormalizedUrl::parse("https://x.org/a b").is_err());
        assert_eq!(
            normalize_url("  https://x.org/a \n").unwrap(),
            "https://x.org/a"
        );
        assert!(NormalizedUrl::parse("").is_err());
    }

    #[test]
    fn extension_and_segments() {
        let u = NormalizedUrl::parse("https://x.org/content/pdf/paper.PDF?x=1").unwrap();
        assert_eq!(u.extension(), Some("pdf".to_string()));
        assert_eq!(u.path_segments(), ["content", "pdf", "paper.PDF"]);
        assert_eq!(
            NormalizedUrl::parse("https://x.org/a/")
                .unwrap()
                .extension(),
            None
        );
        assert_eq!(
            NormalizedUrl::parse("https://x.org/a.b/")
                .unwrap()
                .extension(),
            None
        );
        assert_eq!(
            NormalizedUrl::parse("https://x.org/10.1000/abc.def")
                .unwrap()
                .extension(),
            Some("def".to_string())
        );
    }

    #[test]
    fn resolve_relative_references() {
        let base = NormalizedUrl::parse("https://x.org/dir/page.html?q=1").unwrap();
        assert_eq!(
            base.resolve("paper.pdf").unwrap().to_string(),
            "https://x.org/dir/paper.pdf"
        );
        assert_eq!(
            base.resolve("/pdf/1.pdf").unwrap().to_string(),
            "https://x.org/pdf/1.pdf"
        );
        assert_eq!(
            base.resolve("../up.pdf").unwrap().to_string(),
            "https://x.org/up.pdf"
        );
        assert_eq!(
            base.resolve("?dl=1").unwrap().to_string(),
            "https://x.org/dir/page.html?dl=1"
        );
        assert_eq!(
            base.resolve("//cdn.x.org/a").unwrap().to_string(),
            "https://cdn.x.org/a"
        );
        assert_eq!(
            base.resolve("http://y.org/z").unwrap().to_string(),
            "http://y.org/z"
        );
        assert!(base.resolve("javascript:void(0)").is_err());
        assert!(base.resolve("mailto:a@b.org").is_err());
        assert_eq!(base.resolve("  ").unwrap(), base);
    }

    #[test]
    fn host_in_domain_rules() {
        assert!(host_in_domain("www.nature.com", "nature.com"));
        assert!(host_in_domain("nature.com", ".nature.com"));
        assert!(host_in_domain("NATURE.com", "nature.COM"));
        assert!(!host_in_domain("notnature.com", "nature.com"));
        assert!(!host_in_domain("nature.com", ""));
        assert!(!host_in_domain("nature.com.evil.org", "nature.com"));
    }

    #[test]
    fn percent_coding_round_trips() {
        assert_eq!(percent_decode("10.1000%2Fa%20b%zz%4"), "10.1000/a b%zz%4");
        assert_eq!(percent_decode("caf%C3%A9"), "café");
        assert_eq!(percent_decode("%FF"), "\u{FFFD}");
        let enc = percent_encode_component("https://x.org/a b?c=d&e~_-.");
        assert_eq!(enc, "https%3A%2F%2Fx.org%2Fa%20b%3Fc%3Dd%26e~_-.");
        assert_eq!(percent_decode(&enc), "https://x.org/a b?c=d&e~_-.");
    }
}
