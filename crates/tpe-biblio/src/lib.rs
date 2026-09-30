//! Scholarly metadata and full-text location clients for the workbench.
//!
//! Each source module has a pure `parse_*` function (JSON text in, records out,
//! tested on recorded responses) and a thin `fetch_*` wrapper that performs the
//! HTTP request through a shared [`Client`] (per-host rate limiting, offline mode).
//! Sources: `OpenAlex`, Crossref, Semantic Scholar, NCBI E-utilities (PMC),
//! Europe PMC, Unpaywall, and `OpenURL` link resolvers. [`dedupe`] merges records
//! from several sources and [`resolve`] lists candidate full-text locations.
//!
//! Response shapes follow each service's public API documentation. The offline
//! copies of those docs available when this crate was written were page shells
//! without field listings, so the recorded test fixtures reproduce the
//! well-known public shapes rather than a verified capture.
//!
//! Google Scholar has no public API and its terms forbid scraping, so only
//! [`scholar_search_url`] is provided, for the browser track to open.

#![allow(
    clippy::must_use_candidate,
    clippy::module_name_repetitions,
    clippy::missing_errors_doc
)]

pub mod client;
pub mod crossref;
pub mod dedupe;
pub mod error;
pub mod filename;
pub mod openalex;
pub mod openurl;
pub mod pmc;
pub mod resolve;
pub mod semantic_scholar;
pub mod util;

pub use client::{Client, KEY_NCBI, KEY_OPENALEX, KEY_SEMANTIC_SCHOLAR, RateLimiter};
pub use dedupe::merge_records;
pub use error::BiblioError;
pub use tpe_common::PaperRecord;

/// Whether a full-text candidate points at a PDF or at an HTML landing page.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CandidateKind {
    /// A direct PDF link.
    Pdf,
    /// An HTML landing page (publisher page, link-resolver menu).
    Landing,
}

impl CandidateKind {
    /// `"pdf"` or `"landing"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pdf => "pdf",
            Self::Landing => "landing",
        }
    }
}

/// A place where the full text of a paper may be found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FullTextCandidate {
    /// Absolute URL.
    pub url: String,
    /// Which service produced the candidate (`unpaywall`, `openalex`, `arxiv`, ...).
    pub source: String,
    /// PDF or landing page.
    pub kind: CandidateKind,
    /// True when the link needs an institutional session (link resolver, subscription).
    pub requires_session: bool,
}

impl FullTextCandidate {
    /// Convenience constructor.
    pub fn new(url: &str, source: &str, kind: CandidateKind, requires_session: bool) -> Self {
        Self {
            url: url.to_string(),
            source: source.to_string(),
            kind,
            requires_session,
        }
    }
}

/// One search hit: the normalised record plus any full-text links the source reported.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Found {
    /// The bibliographic record (identifiers normalised).
    pub record: PaperRecord,
    /// Full-text candidates found in the same response.
    pub candidates: Vec<FullTextCandidate>,
}

/// Append `cand` unless a candidate with the same URL is already present.
pub fn push_unique(out: &mut Vec<FullTextCandidate>, cand: FullTextCandidate) {
    if !out.iter().any(|c| c.url == cand.url) {
        out.push(cand);
    }
}

/// A Google Scholar search URL for the browser track to open for a human.
///
/// Google Scholar has no public API and its terms of service forbid automated
/// querying or scraping, so this crate never fetches it.
pub fn scholar_search_url(query: &str) -> String {
    util::with_query("https://scholar.google.com/scholar", &[("q", query)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scholar_url_is_encoded() {
        assert_eq!(
            scholar_search_url("attention is all you need"),
            "https://scholar.google.com/scholar?q=attention%20is%20all%20you%20need"
        );
    }

    #[test]
    fn kind_strings() {
        assert_eq!(CandidateKind::Pdf.as_str(), "pdf");
        assert_eq!(CandidateKind::Landing.as_str(), "landing");
    }
}
