//! Small helpers: percent-encoding, URL host extraction and defensive JSON access.

use serde_json::Value;

const HEX: &[u8; 16] = b"0123456789ABCDEF";

fn encode_with(input: &str, keep: &[u8]) -> String {
    let mut out = String::with_capacity(input.len());
    for &byte in input.as_bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) || keep.contains(&byte) {
            out.push(char::from(byte));
        } else {
            out.push('%');
            out.push(char::from(HEX[usize::from(byte >> 4)]));
            out.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    out
}

/// Percent-encode a query component (everything but RFC 3986 unreserved characters).
pub fn encode_component(input: &str) -> String {
    encode_with(input, b"")
}

/// Percent-encode a path fragment such as a DOI, keeping `/` and `:` literal.
pub fn encode_path(input: &str) -> String {
    encode_with(input, b"/:")
}

/// Build `base?k1=v1&k2=v2` with each key and value percent-encoded.
/// Uses `&` when `base` already contains a `?`.
pub fn with_query(base: &str, pairs: &[(&str, &str)]) -> String {
    let mut url = base.to_string();
    let mut sep = if base.contains('?') { '&' } else { '?' };
    for (key, value) in pairs {
        url.push(sep);
        url.push_str(&encode_component(key));
        url.push('=');
        url.push_str(&encode_component(value));
        sep = '&';
    }
    url
}

/// The lower-cased host of an absolute URL (`https://host:port/path` gives `host`).
pub fn host_of(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = host_port.split(':').next().unwrap_or(host_port);
    host.to_ascii_lowercase()
}

/// A non-empty, trimmed string at `v[key]`, or `None` (absent, null, empty, non-string).
pub fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).and_then(non_empty)
}

/// A trimmed copy of `s`, or `None` when it is empty.
pub fn non_empty(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// The first non-empty string of the array at `v[key]`.
pub fn first_str(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_array)
        .and_then(|a| a.iter().filter_map(Value::as_str).find_map(non_empty))
}

/// The array at `v[key]`, or an empty slice when absent or null.
pub fn array<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    match v.get(key).and_then(Value::as_array) {
        Some(items) => items.as_slice(),
        None => &[],
    }
}

/// A plausible publication year from a JSON number or a string starting with four digits.
pub fn year_of(v: &Value) -> Option<u16> {
    let year = match v {
        Value::Number(n) => n.as_i64().and_then(|y| u16::try_from(y).ok()),
        Value::String(s) => year_prefix(s),
        _ => None,
    }?;
    if (1000..=2999).contains(&year) {
        Some(year)
    } else {
        None
    }
}

/// The year from a string whose first four characters are digits (`"2020 Jan 5"`).
pub fn year_prefix(s: &str) -> Option<u16> {
    let t = s.trim();
    let head = t.get(..4)?;
    if head.chars().all(|c| c.is_ascii_digit()) {
        head.parse::<u16>().ok()
    } else {
        None
    }
}

/// Normalise a `PubMed` Central id to the `PMC<digits>` form. Accepts `PMC123`,
/// `pmc-id: PMC123;`, bare digits, or a URL ending in either form.
pub fn normalize_pmcid(raw: &str) -> Option<String> {
    let t = raw.trim().trim_end_matches(['/', ';']);
    let last = t.rsplit(['/', ' ']).next().unwrap_or(t);
    let digits = last
        .strip_prefix("PMC")
        .or_else(|| last.strip_prefix("pmc"))
        .unwrap_or(last);
    if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
        Some(format!("PMC{digits}"))
    } else {
        None
    }
}

/// Normalise a `PubMed` id: bare digits, or a URL whose last segment is digits.
pub fn normalize_pmid(raw: &str) -> Option<String> {
    let t = raw.trim().trim_end_matches('/');
    let last = t.rsplit('/').next().unwrap_or(t);
    if !last.is_empty() && last.chars().all(|c| c.is_ascii_digit()) {
        Some(last.to_string())
    } else {
        None
    }
}

/// An `arXiv` id derived from an `arXiv` `DataCite` DOI (`10.48550/arxiv.2301.00001`).
pub fn arxiv_from_doi(doi: &str) -> Option<String> {
    let lower = doi.to_ascii_lowercase();
    let rest = lower.strip_prefix("10.48550/arxiv.")?;
    tpe_common::normalize_arxiv_id(rest)
}

/// Remove markup tags (`<jats:p>` and friends) and collapse whitespace.
pub fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => {
                in_tag = true;
                out.push(' ');
            }
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<&str>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_encoding() {
        assert_eq!(encode_component("a b&c/é"), "a%20b%26c%2F%C3%A9");
        assert_eq!(encode_path("10.1000/a<b>"), "10.1000/a%3Cb%3E");
        assert_eq!(
            with_query("https://h/p?x=1", &[("q", "a b")]),
            "https://h/p?x=1&q=a%20b"
        );
    }

    #[test]
    fn hosts() {
        assert_eq!(
            host_of("https://API.openalex.org/works?x"),
            "api.openalex.org"
        );
        assert_eq!(host_of("http://user@h.example:8080/"), "h.example");
    }

    #[test]
    fn identifiers() {
        assert_eq!(normalize_pmcid("5815332"), Some("PMC5815332".to_string()));
        assert_eq!(
            normalize_pmcid("https://www.ncbi.nlm.nih.gov/pmc/articles/5815332"),
            Some("PMC5815332".to_string())
        );
        assert_eq!(
            normalize_pmcid("pmc-id: PMC123;"),
            Some("PMC123".to_string())
        );
        assert_eq!(normalize_pmcid("abc"), None);
        assert_eq!(
            normalize_pmid("https://pubmed.ncbi.nlm.nih.gov/29456894"),
            Some("29456894".to_string())
        );
        assert_eq!(
            arxiv_from_doi("10.48550/arXiv.2301.00001"),
            Some("2301.00001".to_string())
        );
        assert_eq!(year_prefix("2020 Jan 5"), Some(2020));
        assert_eq!(year_of(&Value::Null), None);
        assert_eq!(
            strip_tags("<jats:p>Hello  <i>world</i></jats:p>"),
            "Hello world"
        );
    }
}
