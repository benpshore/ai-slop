//! Shared, serialisable record types used by the workbench tracks (search,
//! bibliographic integrations, speech, app) to talk about papers without
//! depending on the extraction engine crate.

#![allow(clippy::must_use_candidate, clippy::module_name_repetitions)]

use serde::{Deserialize, Serialize};

/// A bibliographic record as the integrations exchange it. Every field is
/// optional except `title`; identifiers are stored normalised (lower-case
/// DOI without a resolver prefix, arXiv id without version).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Source-specific identifier (OpenAlex work id, Zotero item key, ...).
    pub source_id: Option<String>,
}

/// Normalise a DOI: strip resolver prefixes and surrounding whitespace,
/// lower-case it. Returns `None` when the remainder does not look like a DOI.
pub fn normalize_doi(raw: &str) -> Option<String> {
    let mut s = raw.trim();
    for prefix in [
        "https://doi.org/",
        "http://doi.org/",
        "https://dx.doi.org/",
        "http://dx.doi.org/",
        "doi:",
        "DOI:",
    ] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest.trim();
        }
    }
    let s = s.trim_end_matches(['.', ',', ';', ')']);
    if s.starts_with("10.") && s.contains('/') {
        Some(s.to_ascii_lowercase())
    } else {
        None
    }
}

/// Normalise an arXiv identifier: strip `arXiv:` and any `vN` suffix.
pub fn normalize_arxiv_id(raw: &str) -> Option<String> {
    let s = raw.trim();
    let s = s
        .strip_prefix("arXiv:")
        .or_else(|| s.strip_prefix("arxiv:"))
        .unwrap_or(s);
    let s = s.trim();
    let base = match s.rfind('v') {
        Some(pos) if s[pos + 1..].chars().all(|c| c.is_ascii_digit()) && pos + 1 < s.len() => {
            &s[..pos]
        }
        _ => s,
    };
    let new_style = base.len() >= 9
        && base.as_bytes()[4] == b'.'
        && base[..4].chars().all(|c| c.is_ascii_digit())
        && base[5..].chars().all(|c| c.is_ascii_digit());
    let old_style = base.contains('/') && base.chars().filter(|c| c.is_ascii_digit()).count() == 7;
    if new_style || old_style {
        Some(base.to_string())
    } else {
        None
    }
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
