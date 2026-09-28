//! Full-text location: combine every source that can point at a PDF or landing page.

use tpe_common::PaperRecord;

use crate::client::Client;
use crate::util::encode_path;
use crate::{CandidateKind, FullTextCandidate, crossref, openalex, openurl, push_unique};

/// `arXiv` PDF URL for a (normalised) `arXiv` id.
pub fn arxiv_pdf_url(arxiv_id: &str) -> String {
    format!("https://arxiv.org/pdf/{}", encode_path(arxiv_id))
}

/// PMC PDF URL for a `PMC<digits>` id.
pub fn pmc_pdf_url(pmcid: &str) -> String {
    format!(
        "https://www.ncbi.nlm.nih.gov/pmc/articles/{}/pdf",
        encode_path(pmcid)
    )
}

/// Candidates derivable from the record alone, without any request:
/// `arXiv` PDF, PMC PDF, and (when a resolver is configured) the library
/// link-resolver page, which needs an institutional session.
pub fn static_candidates(rec: &PaperRecord, link_resolver: Option<&str>) -> Vec<FullTextCandidate> {
    let mut out: Vec<FullTextCandidate> = Vec::new();
    if let Some(id) = rec.arxiv_id.as_deref() {
        let url = arxiv_pdf_url(id);
        push_unique(
            &mut out,
            FullTextCandidate::new(&url, "arxiv", CandidateKind::Pdf, false),
        );
    }
    if let Some(id) = rec.pmcid.as_deref() {
        let url = pmc_pdf_url(id);
        push_unique(
            &mut out,
            FullTextCandidate::new(&url, "pmc", CandidateKind::Pdf, false),
        );
    }
    if let Some(base) = link_resolver {
        let url = openurl::build_openurl(base, rec);
        push_unique(
            &mut out,
            FullTextCandidate::new(&url, "link_resolver", CandidateKind::Landing, true),
        );
    }
    out
}

/// Candidates that need a request per service, in order Unpaywall, `OpenAlex`,
/// Crossref. A failing service is skipped (its error is not reported), so an
/// offline client returns an empty list without making any request.
pub fn network_candidates(client: &Client, rec: &PaperRecord) -> Vec<FullTextCandidate> {
    let mut out: Vec<FullTextCandidate> = Vec::new();
    let Some(doi) = rec.doi.as_deref() else {
        return out;
    };
    if client.is_offline() {
        return out;
    }
    // Without a `mailto` this is `MissingConfig` and skipped like any other failure.
    if let Ok(found) = openurl::fetch_unpaywall(client, doi) {
        for c in found {
            push_unique(&mut out, c);
        }
    }
    if let Ok(Some(found)) = openalex::fetch_by_doi(client, doi) {
        for c in found.candidates {
            push_unique(&mut out, c);
        }
    }
    if let Ok(Some(found)) = crossref::fetch_by_doi(client, doi) {
        for c in found.candidates {
            push_unique(&mut out, c);
        }
    }
    out
}

impl Client {
    /// All full-text candidates for `rec`: open-access services first (Unpaywall,
    /// `OpenAlex`, Crossref links; skipped when offline or without a DOI), then
    /// `arXiv`, PMC, and the publisher via the link resolver (`requires_session`).
    pub fn locate_fulltext(&self, rec: &PaperRecord) -> Vec<FullTextCandidate> {
        let mut out = network_candidates(self, rec);
        for c in static_candidates(rec, self.link_resolver()) {
            push_unique(&mut out, c);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> PaperRecord {
        PaperRecord {
            title: "Attention Is All You Need".to_string(),
            year: Some(2017),
            doi: Some("10.48550/arxiv.1706.03762".to_string()),
            arxiv_id: Some("1706.03762".to_string()),
            pmcid: Some("PMC5815332".to_string()),
            source: "openalex".to_string(),
            ..PaperRecord::default()
        }
    }

    #[test]
    fn offline_locate_returns_static_candidates_only() {
        let client = Client::new("t")
            .with_offline(true)
            .with_mailto("a@b.org")
            .with_link_resolver("https://resolver.example.edu/openurl");
        let found = client.locate_fulltext(&record());
        let urls: Vec<&str> = found.iter().map(|c| c.url.as_str()).collect();
        assert_eq!(found.len(), 3);
        assert_eq!(urls[0], "https://arxiv.org/pdf/1706.03762");
        assert_eq!(
            urls[1],
            "https://www.ncbi.nlm.nih.gov/pmc/articles/PMC5815332/pdf"
        );
        assert!(urls[2].starts_with("https://resolver.example.edu/openurl?url_ver="));
        assert!(found[2].requires_session);
        assert_eq!(found[2].kind, CandidateKind::Landing);
        assert!(!found[0].requires_session);
    }

    #[test]
    fn no_identifiers_no_candidates() {
        let rec = PaperRecord {
            title: "x".to_string(),
            ..PaperRecord::default()
        };
        assert!(static_candidates(&rec, None).is_empty());
        let client = Client::new("t").with_offline(true);
        assert!(client.locate_fulltext(&rec).is_empty());
    }
}
