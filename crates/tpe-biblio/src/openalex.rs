//! `OpenAlex` works API: search and DOI lookup.
//!
//! Response shape (public `OpenAlex` Work object; assumed, see crate docs):
//!
//! ```text
//! { "id", "doi", "display_name", "publication_year",
//!   "ids": { "pmid", "pmcid" },
//!   "primary_location": { "source": { "display_name" }, "landing_page_url", "pdf_url" },
//!   "best_oa_location": { "pdf_url" }, "open_access": { "oa_url" },
//!   "authorships": [ { "author": { "display_name" }, "raw_author_name" } ],
//!   "abstract_inverted_index": { "word": [positions] }, "locations": [...] }
//! ```
//!
//! A list response wraps works in `results`.

use std::collections::BTreeMap;

use serde_json::Value;
use tpe_common::{PaperRecord, normalize_arxiv_id, normalize_doi};

use crate::client::{Client, KEY_OPENALEX};
use crate::error::BiblioError;
use crate::util::{
    array, arxiv_from_doi, encode_path, normalize_pmcid, normalize_pmid, str_field, with_query,
    year_of,
};
use crate::{CandidateKind, Found, FullTextCandidate, push_unique};

/// API base URL.
pub const BASE: &str = "https://api.openalex.org";

fn common_params<'a>(mailto: Option<&'a str>, api_key: Option<&'a str>) -> Vec<(&'a str, &'a str)> {
    let mut pairs: Vec<(&str, &str)> = Vec::new();
    if let Some(m) = mailto {
        pairs.push(("mailto", m));
    }
    if let Some(k) = api_key {
        pairs.push(("api_key", k));
    }
    pairs
}

/// `GET /works?search=<q>&per-page=<n>&mailto=<m>[&api_key=<k>]`.
pub fn search_url(
    query: &str,
    per_page: u32,
    mailto: Option<&str>,
    api_key: Option<&str>,
) -> String {
    let n = per_page.clamp(1, 200).to_string();
    let mut pairs: Vec<(&str, &str)> = vec![("search", query), ("per-page", n.as_str())];
    pairs.extend(common_params(mailto, api_key));
    with_query(&format!("{BASE}/works"), &pairs)
}

/// `GET /works/https://doi.org/<doi>`.
pub fn doi_url(doi: &str, mailto: Option<&str>, api_key: Option<&str>) -> String {
    let path = format!("{BASE}/works/https://doi.org/{}", encode_path(doi));
    with_query(&path, &common_params(mailto, api_key))
}

/// Rebuild abstract text from an `abstract_inverted_index` (word → positions).
/// Returns `None` for null, empty or malformed indexes.
pub fn reconstruct_abstract(index: &Value) -> Option<String> {
    let map = index.as_object()?;
    let mut by_pos: BTreeMap<u64, &str> = BTreeMap::new();
    for (word, positions) in map {
        let Some(list) = positions.as_array() else {
            continue;
        };
        for pos in list {
            if let Some(p) = pos.as_u64() {
                by_pos.insert(p, word.as_str());
            }
        }
    }
    if by_pos.is_empty() {
        return None;
    }
    let words: Vec<&str> = by_pos.into_values().collect();
    Some(words.join(" "))
}

/// Parse a works list (`{"results": [...]}`) or a single work into records.
pub fn parse_openalex(json: &str) -> Result<Vec<PaperRecord>, BiblioError> {
    Ok(parse_openalex_found(json)?
        .into_iter()
        .map(|f| f.record)
        .collect())
}

/// Like [`parse_openalex`] but keeps the open-access links as candidates.
pub fn parse_openalex_found(json: &str) -> Result<Vec<Found>, BiblioError> {
    let root: Value = serde_json::from_str(json)?;
    if let Some(results) = root.get("results").and_then(Value::as_array) {
        return Ok(results.iter().map(work_to_found).collect());
    }
    if root.get("id").is_some() {
        return Ok(vec![work_to_found(&root)]);
    }
    Err(BiblioError::Shape(
        "OpenAlex: neither `results` nor a work `id`".to_string(),
    ))
}

fn arxiv_from_locations(work: &Value) -> Option<String> {
    array(work, "locations").iter().find_map(|loc| {
        let url = str_field(loc, "landing_page_url")?;
        let (_, rest) = url.split_once("arxiv.org/abs/")?;
        normalize_arxiv_id(rest)
    })
}

fn authors(work: &Value) -> Vec<String> {
    array(work, "authorships")
        .iter()
        .filter_map(|a| {
            a.get("author")
                .and_then(|au| str_field(au, "display_name"))
                .or_else(|| str_field(a, "raw_author_name"))
        })
        .collect()
}

fn candidates(work: &Value) -> Vec<FullTextCandidate> {
    let mut out: Vec<FullTextCandidate> = Vec::new();
    let mut pdf_urls: Vec<String> = Vec::new();
    for key in ["best_oa_location", "primary_location"] {
        if let Some(url) = work.get(key).and_then(|l| str_field(l, "pdf_url")) {
            pdf_urls.push(url);
        }
    }
    for url in &pdf_urls {
        push_unique(
            &mut out,
            FullTextCandidate::new(url, "openalex", CandidateKind::Pdf, false),
        );
    }
    if let Some(oa) = work.get("open_access").and_then(|o| str_field(o, "oa_url")) {
        let is_pdf = pdf_urls.contains(&oa) || oa.to_ascii_lowercase().ends_with(".pdf");
        let kind = if is_pdf {
            CandidateKind::Pdf
        } else {
            CandidateKind::Landing
        };
        push_unique(
            &mut out,
            FullTextCandidate::new(&oa, "openalex", kind, false),
        );
    }
    out
}

fn work_to_found(work: &Value) -> Found {
    let doi = str_field(work, "doi").and_then(|d| normalize_doi(&d));
    let ids = work.get("ids").unwrap_or(&Value::Null);
    let primary = work.get("primary_location").unwrap_or(&Value::Null);
    let arxiv_id = doi
        .as_deref()
        .and_then(arxiv_from_doi)
        .or_else(|| arxiv_from_locations(work));
    let record = PaperRecord {
        title: str_field(work, "display_name")
            .or_else(|| str_field(work, "title"))
            .unwrap_or_default(),
        authors: authors(work),
        year: work.get("publication_year").and_then(year_of),
        venue: primary
            .get("source")
            .and_then(|s| str_field(s, "display_name")),
        doi,
        arxiv_id,
        pmid: str_field(ids, "pmid").and_then(|p| normalize_pmid(&p)),
        pmcid: str_field(ids, "pmcid").and_then(|p| normalize_pmcid(&p)),
        url: str_field(primary, "landing_page_url"),
        abstract_text: work
            .get("abstract_inverted_index")
            .and_then(reconstruct_abstract),
        source: "openalex".to_string(),
        source_id: str_field(work, "id")
            .map(|id| id.trim_start_matches("https://openalex.org/").to_string()),
    };
    Found {
        record,
        candidates: candidates(work),
    }
}

/// Search works by free text.
pub fn fetch_search(
    client: &Client,
    query: &str,
    per_page: u32,
) -> Result<Vec<Found>, BiblioError> {
    let url = search_url(query, per_page, client.mailto(), client.key(KEY_OPENALEX));
    parse_openalex_found(&client.get_text(&url, &[])?)
}

/// Look up one work by DOI; `Ok(None)` when `OpenAlex` does not know it.
pub fn fetch_by_doi(client: &Client, doi: &str) -> Result<Option<Found>, BiblioError> {
    let url = doi_url(doi, client.mailto(), client.key(KEY_OPENALEX));
    match client.get_text(&url, &[]) {
        Ok(body) => Ok(parse_openalex_found(&body)?.into_iter().next()),
        Err(BiblioError::NotFound) => Ok(None),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEARCH: &str = r#"{
      "meta": {"count": 2, "db_response_time_ms": 31, "page": 1, "per_page": 2},
      "results": [
        {
          "id": "https://openalex.org/W2741809807",
          "doi": "https://doi.org/10.7717/PEERJ.4375",
          "title": "The state of OA: a large-scale analysis of the prevalence and impact of Open Access articles",
          "display_name": "The state of OA: a large-scale analysis of the prevalence and impact of Open Access articles",
          "publication_year": 2018,
          "publication_date": "2018-02-13",
          "ids": {
            "openalex": "https://openalex.org/W2741809807",
            "doi": "https://doi.org/10.7717/peerj.4375",
            "mag": "2741809807",
            "pmid": "https://pubmed.ncbi.nlm.nih.gov/29456894",
            "pmcid": "https://www.ncbi.nlm.nih.gov/pmc/articles/5815332"
          },
          "primary_location": {
            "is_oa": true,
            "landing_page_url": "https://doi.org/10.7717/peerj.4375",
            "pdf_url": "https://peerj.com/articles/4375.pdf",
            "source": {"id": "https://openalex.org/S1983995261", "display_name": "PeerJ", "issn_l": "2167-8359"}
          },
          "open_access": {"is_oa": true, "oa_status": "gold", "oa_url": "https://peerj.com/articles/4375.pdf", "any_repository_has_fulltext": true},
          "best_oa_location": {"is_oa": true, "landing_page_url": "https://doi.org/10.7717/peerj.4375", "pdf_url": "https://peerj.com/articles/4375.pdf"},
          "authorships": [
            {"author_position": "first", "author": {"id": "https://openalex.org/A5048491430", "display_name": "Heather Piwowar", "orcid": null}, "raw_author_name": "Heather Piwowar"},
            {"author_position": "middle", "author": {"id": "https://openalex.org/A5040821463", "display_name": "Jason Priem"}, "raw_author_name": "Jason Priem"}
          ],
          "abstract_inverted_index": {"Despite": [0], "growing": [1], "interest": [2], "in": [3], "Open": [4], "Access": [5]},
          "locations": []
        },
        {
          "id": "https://openalex.org/W4313",
          "doi": "https://doi.org/10.48550/arxiv.1706.03762",
          "display_name": "Attention Is All You Need",
          "publication_year": 2017,
          "ids": {"openalex": "https://openalex.org/W4313"},
          "primary_location": {"landing_page_url": "https://arxiv.org/abs/1706.03762", "pdf_url": null, "source": null},
          "open_access": {"is_oa": true, "oa_status": "green", "oa_url": "https://arxiv.org/abs/1706.03762"},
          "best_oa_location": null,
          "authorships": [{"author": {"display_name": "Ashish Vaswani"}}],
          "abstract_inverted_index": null
        }
      ]
    }"#;

    #[test]
    fn parses_search_results() {
        let found = parse_openalex_found(SEARCH).unwrap();
        assert_eq!(found.len(), 2);
        let r = &found[0].record;
        assert!(r.title.starts_with("The state of OA"));
        assert_eq!(r.authors, vec!["Heather Piwowar", "Jason Priem"]);
        assert_eq!(r.year, Some(2018));
        assert_eq!(r.venue.as_deref(), Some("PeerJ"));
        assert_eq!(r.doi.as_deref(), Some("10.7717/peerj.4375"));
        assert_eq!(r.pmid.as_deref(), Some("29456894"));
        assert_eq!(r.pmcid.as_deref(), Some("PMC5815332"));
        assert_eq!(r.source, "openalex");
        assert_eq!(r.source_id.as_deref(), Some("W2741809807"));
        assert_eq!(
            r.abstract_text.as_deref(),
            Some("Despite growing interest in Open Access")
        );
        // best_oa pdf and oa_url are the same URL: one candidate.
        assert_eq!(found[0].candidates.len(), 1);
        assert_eq!(found[0].candidates[0].kind, CandidateKind::Pdf);
    }

    #[test]
    fn handles_nulls_and_arxiv_doi() {
        let found = parse_openalex_found(SEARCH).unwrap();
        let r = &found[1].record;
        assert_eq!(r.venue, None);
        assert_eq!(r.abstract_text, None);
        assert_eq!(r.arxiv_id.as_deref(), Some("1706.03762"));
        assert_eq!(found[1].candidates.len(), 1);
        assert_eq!(found[1].candidates[0].kind, CandidateKind::Landing);
    }

    #[test]
    fn single_work_and_bad_shape() {
        let single = r#"{"id": "https://openalex.org/W1", "display_name": "T", "doi": null}"#;
        let recs = parse_openalex(single).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].doi, None);
        assert!(matches!(
            parse_openalex(r#"{"error": "x"}"#),
            Err(BiblioError::Shape(_))
        ));
        assert!(matches!(
            parse_openalex("not json"),
            Err(BiblioError::Json(_))
        ));
    }

    #[test]
    fn abstract_reconstruction_orders_by_position() {
        let idx: Value =
            serde_json::from_str(r#"{"world": [1, 3], "hello": [0, 2], "again": [4]}"#).unwrap();
        assert_eq!(
            reconstruct_abstract(&idx).as_deref(),
            Some("hello world hello world again")
        );
        assert_eq!(reconstruct_abstract(&Value::Null), None);
        let empty: Value = serde_json::from_str("{}").unwrap();
        assert_eq!(reconstruct_abstract(&empty), None);
    }

    #[test]
    fn urls() {
        assert_eq!(
            search_url("deep learning", 5, Some("a@b.org"), None),
            "https://api.openalex.org/works?search=deep%20learning&per-page=5&mailto=a%40b.org"
        );
        assert_eq!(
            doi_url("10.7717/peerj.4375", None, None),
            "https://api.openalex.org/works/https://doi.org/10.7717/peerj.4375"
        );
    }

    #[test]
    fn offline_fetch_errors() {
        let client = Client::new("t").with_offline(true);
        assert!(matches!(
            fetch_search(&client, "x", 1),
            Err(BiblioError::Offline)
        ));
        assert!(matches!(
            fetch_by_doi(&client, "10.1/x"),
            Err(BiblioError::Offline)
        ));
    }
}
