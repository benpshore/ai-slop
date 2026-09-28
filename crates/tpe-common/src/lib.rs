//! Shared, serialisable record types used by the workbench tracks (search,
//! bibliographic integrations, speech, app) to talk about papers without
//! depending on the extraction engine crate.

#![allow(clippy::must_use_candidate, clippy::module_name_repetitions)]

use serde::{Deserialize, Serialize};

/// A bibliographic record as the integrations exchange it. Every field is
/// optional except `title`; identifiers are stored normalised (lower-case
/// DOI without a resolver prefix, arXiv id without version).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PaperRecord {
    pub title: String,
    pub authors: Vec<String>,
    pub year: Option<u16>,
    pub venue: Option<String>,
    pub doi: Option<String>,
    pub arxiv_id: Option<String>,
    pub pmid: Option<String>,
    pub pmcid: Option<String>,
    pub url: Option<String>,
    pub abstract_text: Option<String>,
    /// Where this record came from, e.g. `openalex`, `crossref`, `zotero`, `tpe`.
    pub source: String,
    /// Source-specific identifier (`OpenAlex` work id, Zotero item key, ...).
    pub source_id: Option<String>,
}

/// Normalise a DOI: strip resolver prefixes (matched ASCII case-insensitively)
/// and surrounding whitespace, lower-case it. Returns `None` unless the
/// remainder has the DOI shape `10.<4-9 digits>/<suffix>`.
pub fn normalize_doi(raw: &str) -> Option<String> {
    let mut s = raw.trim();
    let prefixes = [
        "https://doi.org/",
        "http://doi.org/",
        "https://dx.doi.org/",
        "http://dx.doi.org/",
        "doi:",
    ];
    loop {
        let lower = s.to_ascii_lowercase();
        let Some(prefix) = prefixes.iter().find(|p| lower.starts_with(**p)) else {
            break;
        };
        s = s[prefix.len()..].trim();
    }
    let s = s.trim_end_matches(['.', ',', ';']);
    // A trailing ')' is punctuation only when it is unbalanced; DOIs such as
    // 10.1002/(SICI)1097-0258(19980815)17:15<1741::AID-SIM868>3.0.CO;2-8 end
    // in a legitimate ')'-bearing suffix and must be kept intact.
    let s = trim_unbalanced_paren(s);
    let rest = s.strip_prefix("10.")?;
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    if !(4..=9).contains(&digits) || rest.as_bytes().get(digits) != Some(&b'/') {
        return None;
    }
    let suffix = &rest[digits + 1..];
    if suffix.is_empty() || suffix.chars().any(char::is_whitespace) {
        return None;
    }
    Some(s.to_ascii_lowercase())
}

/// Drop trailing `)` characters that have no matching `(` in `s`.
fn trim_unbalanced_paren(mut s: &str) -> &str {
    while let Some(stripped) = s.strip_suffix(')') {
        let opens = s.matches('(').count();
        let closes = s.matches(')').count();
        if closes > opens {
            s = stripped;
        } else {
            break;
        }
    }
    s
}

/// Normalise an arXiv identifier: strip `arXiv:` and any `vN` suffix.
/// Accepts the new style `YYMM.NNNNN` (4 digits, dot, 4–5 digits) and the
/// old style `archive[.SC]/YYMMNNN` (a letter/hyphen archive name, optional
/// two-letter subject class, exactly seven digits).
pub fn normalize_arxiv_id(raw: &str) -> Option<String> {
    let s = raw.trim();
    let lower = s.to_ascii_lowercase();
    let s = if lower.starts_with("arxiv:") {
        s[6..].trim()
    } else {
        s
    };
    let base = match s.rfind('v') {
        Some(pos) if pos + 1 < s.len() && s[pos + 1..].bytes().all(|b| b.is_ascii_digit()) => {
            &s[..pos]
        }
        _ => s,
    };
    if is_new_style_arxiv(base) || is_old_style_arxiv(base) {
        Some(base.to_string())
    } else {
        None
    }
}

fn is_new_style_arxiv(s: &str) -> bool {
    let Some((left, right)) = s.split_once('.') else {
        return false;
    };
    left.len() == 4
        && left.bytes().all(|b| b.is_ascii_digit())
        && (4..=5).contains(&right.len())
        && right.bytes().all(|b| b.is_ascii_digit())
}

fn is_old_style_arxiv(s: &str) -> bool {
    let Some((archive, number)) = s.split_once('/') else {
        return false;
    };
    let (name, class) = match archive.split_once('.') {
        Some((n, c)) => (n, Some(c)),
        None => (archive, None),
    };
    let name_ok = !name.is_empty()
        && name.bytes().all(|b| b.is_ascii_lowercase() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-');
    let class_ok = class.is_none_or(|c| c.len() == 2 && c.bytes().all(|b| b.is_ascii_uppercase()));
    name_ok && class_ok && number.len() == 7 && number.bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doi_forms_normalise() {
        assert_eq!(
            normalize_doi("https://doi.org/10.1000/ABC.123."),
            Some("10.1000/abc.123".to_string())
        );
        assert_eq!(
            normalize_doi("doi:10.1000/x"),
            Some("10.1000/x".to_string())
        );
        assert_eq!(normalize_doi("not a doi"), None);
    }

    #[test]
    fn arxiv_forms_normalise() {
        assert_eq!(
            normalize_arxiv_id("arXiv:2502.00857v2"),
            Some("2502.00857".to_string())
        );
        assert_eq!(
            normalize_arxiv_id("hep-th/9901001"),
            Some("hep-th/9901001".to_string())
        );
        assert_eq!(
            normalize_arxiv_id("2502.00857"),
            Some("2502.00857".to_string())
        );
        assert_eq!(normalize_arxiv_id("12.34"), None);
        assert_eq!(
            normalize_arxiv_id("math.GT/0309136v1"),
            Some("math.GT/0309136".to_string())
        );
        assert_eq!(normalize_arxiv_id("/1234567"), None);
        assert_eq!(normalize_arxiv_id("1234567/"), None);
        assert_eq!(normalize_arxiv_id("bad value/1234567"), None);
        assert_eq!(normalize_arxiv_id("hep-th/123456"), None);
    }

    #[test]
    fn partial_record_deserialises_with_defaults() {
        let r: PaperRecord = serde_json::from_str(r#"{"title":"Only a title"}"#).unwrap();
        assert_eq!(r.title, "Only a title");
        assert!(r.authors.is_empty());
        assert_eq!(r.source, "");
    }

    #[test]
    fn record_round_trips() {
        let r = PaperRecord {
            title: "T".into(),
            source: "test".into(),
            ..PaperRecord::default()
        };
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(serde_json::from_str::<PaperRecord>(&s).unwrap(), r);
    }
}
