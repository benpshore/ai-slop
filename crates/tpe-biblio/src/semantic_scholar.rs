//! Semantic Scholar Academic Graph API: paper search.
//!
//! Response shape (public Graph API v1; assumed, see crate docs):
//!
//! ```text
//! search: { "total", "offset", "next", "data": [paper...] }
//! paper:  { "paperId", "title", "abstract", "venue", "year", "url",
//!           "authors": [ { "name" } ],
//!           "externalIds": { "DOI", "ArXiv", "PubMed", "PubMedCentral" },
//!           "openAccessPdf": { "url", "status" } }   (url may be "")
//! ```
//!
//! A single-paper response is the paper object itself.

use serde_json::Value;
use tpe_common::{PaperRecord, normalize_arxiv_id, normalize_doi};

use crate::client::{Client, KEY_SEMANTIC_SCHOLAR};
use crate::error::BiblioError;
use crate::util::{array, normalize_pmcid, normalize_pmid, str_field, with_query, year_of};
use crate::{CandidateKind, Found, FullTextCandidate};

/// API base URL.
pub const BASE: &str = "https://api.semanticscholar.org/graph/v1";

/// Fields requested from the API.
pub const FIELDS: &str = "title,authors,year,venue,externalIds,openAccessPdf,abstract,url";

/// `GET /paper/search?query=<q>&fields=...&limit=<n>`.
pub fn search_url(query: &str, limit: u32) -> String {
    let n = limit.clamp(1, 100).to_string();
    with_query(
        &format!("{BASE}/paper/search"),
        &[("query", query), ("fields", FIELDS), ("limit", n.as_str())],
    )
}

/// Parse a search response (`data`) or a single paper into records.
pub fn parse_semantic_scholar(json: &str) -> Result<Vec<PaperRecord>, BiblioError> {
    Ok(parse_semantic_scholar_found(json)?
        .into_iter()
        .map(|f| f.record)
        .collect())
}

/// Like [`parse_semantic_scholar`] but keeps `openAccessPdf.url` as a candidate.
pub fn parse_semantic_scholar_found(json: &str) -> Result<Vec<Found>, BiblioError> {
    let root: Value = serde_json::from_str(json)?;
    if let Some(data) = root.get("data").and_then(Value::as_array) {
        return Ok(data.iter().map(paper_to_found).collect());
    }
    if root.get("paperId").is_some() {
        return Ok(vec![paper_to_found(&root)]);
    }
    Err(BiblioError::Shape(
        "Semantic Scholar: neither `data` nor `paperId`".to_string(),
    ))
}

fn paper_to_found(paper: &Value) -> Found {
    let ext = paper.get("externalIds").unwrap_or(&Value::Null);
    let record = PaperRecord {
        title: str_field(paper, "title").unwrap_or_default(),
        authors: array(paper, "authors")
            .iter()
            .filter_map(|a| str_field(a, "name"))
            .collect(),
        year: paper.get("year").and_then(year_of),
        venue: str_field(paper, "venue"),
        doi: str_field(ext, "DOI").and_then(|d| normalize_doi(&d)),
        arxiv_id: str_field(ext, "ArXiv").and_then(|a| normalize_arxiv_id(&a)),
        pmid: str_field(ext, "PubMed").and_then(|p| normalize_pmid(&p)),
        pmcid: str_field(ext, "PubMedCentral").and_then(|p| normalize_pmcid(&p)),
        url: str_field(paper, "url"),
        abstract_text: str_field(paper, "abstract"),
        source: "semantic_scholar".to_string(),
        source_id: str_field(paper, "paperId"),
    };
    let candidates: Vec<FullTextCandidate> = paper
        .get("openAccessPdf")
        .and_then(|o| str_field(o, "url"))
        .map(|url| FullTextCandidate::new(&url, "semantic_scholar", CandidateKind::Pdf, false))
        .into_iter()
        .collect();
    Found { record, candidates }
}

/// Search papers; sends `x-api-key` when a Semantic Scholar key is configured.
pub fn fetch_search(client: &Client, query: &str, limit: u32) -> Result<Vec<Found>, BiblioError> {
    let url = search_url(query, limit);
    let body = match client.key(KEY_SEMANTIC_SCHOLAR) {
        Some(key) => client.get_text(&url, &[("x-api-key", key)])?,
        None => client.get_text(&url, &[])?,
    };
    parse_semantic_scholar_found(&body)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEARCH: &str = r#"{
      "total": 2, "offset": 0, "next": 2,
      "data": [
        {
          "paperId": "204e3073870fae3d05bcbc2f6a8e263d9b72e776",
          "externalIds": {"DBLP": "conf/nips/VaswaniSPUJGKP17", "ArXiv": "1706.03762", "MAG": "2963403868", "DOI": "10.48550/arXiv.1706.03762", "CorpusId": 13756489},
          "title": "Attention is All you Need",
          "abstract": "The dominant sequence transduction models are based on complex recurrent networks.",
          "venue": "Neural Information Processing Systems",
          "year": 2017,
          "openAccessPdf": {"url": "https://arxiv.org/pdf/1706.03762", "status": "GREEN"},
          "authors": [{"authorId": "40348417", "name": "Ashish Vaswani"}, {"authorId": "1846258", "name": "Noam M. Shazeer"}]
        },
        {
          "paperId": "abc123",
          "externalIds": {"PubMed": "29456894", "PubMedCentral": "5815332"},
          "title": "The state of OA",
          "abstract": null,
          "venue": "",
          "year": null,
          "openAccessPdf": {"url": "", "status": "CLOSED"},
          "authors": []
        }
      ]
    }"#;

    #[test]
    fn parses_search() {
        let found = parse_semantic_scholar_found(SEARCH).unwrap();
        assert_eq!(found.len(), 2);
        let r = &found[0].record;
        assert_eq!(r.title, "Attention is All you Need");
        assert_eq!(r.authors, vec!["Ashish Vaswani", "Noam M. Shazeer"]);
        assert_eq!(r.year, Some(2017));
        assert_eq!(r.arxiv_id.as_deref(), Some("1706.03762"));
        assert_eq!(r.doi.as_deref(), Some("10.48550/arxiv.1706.03762"));
        assert_eq!(r.source, "semantic_scholar");
        assert_eq!(
            found[0].candidates[0].url,
            "https://arxiv.org/pdf/1706.03762"
        );
    }

    #[test]
    fn empty_pdf_url_and_nulls() {
        let found = parse_semantic_scholar_found(SEARCH).unwrap();
        let r = &found[1].record;
        assert_eq!(r.pmid.as_deref(), Some("29456894"));
        assert_eq!(r.pmcid.as_deref(), Some("PMC5815332"));
        assert_eq!(r.venue, None);
        assert_eq!(r.year, None);
        assert_eq!(r.abstract_text, None);
        assert!(found[1].candidates.is_empty());
    }

    #[test]
    fn url_and_offline() {
        assert_eq!(
            search_url("a b", 3),
            "https://api.semanticscholar.org/graph/v1/paper/search?query=a%20b&fields=title%2Cauthors%2Cyear%2Cvenue%2CexternalIds%2CopenAccessPdf%2Cabstract%2Curl&limit=3"
        );
        let client = Client::new("t").with_offline(true);
        assert!(matches!(
            fetch_search(&client, "x", 1),
            Err(BiblioError::Offline)
        ));
        assert!(matches!(
            parse_semantic_scholar("[]"),
            Err(BiblioError::Shape(_))
        ));
    }
}
