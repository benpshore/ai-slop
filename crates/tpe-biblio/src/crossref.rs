//! Crossref REST API: `/works` search and DOI lookup.
//!
//! Response shape (public Crossref REST API; assumed, see crate docs):
//!
//! ```text
//! list:   { "status": "ok", "message-type": "work-list", "message": { "items": [work...] } }
//! single: { "status": "ok", "message-type": "work", "message": work }
//! work:   { "title": [..], "author": [ { "given", "family", "name" } ],
//!           "issued": { "date-parts": [[y, m, d]] }, "published", "published-print",
//!           "published-online", "container-title": [..], "DOI", "URL",
//!           "abstract" (JATS markup), "link": [ { "URL", "content-type" } ],
//!           "license": [ { "URL", "delay-in-days", "content-version" } ] }
//! ```
//!
//! A Crossref `link` of type `application/pdf` is often a publisher URL that
//! needs a subscription, so `requires_session` is derived conservatively: it is
//! `false` only when the work is known to be open access, that is when a
//! `license` entry points at a Creative Commons URL with no embargo
//! (`delay-in-days` absent or zero), or when the link's host is a known
//! open-access host ([`OPEN_ACCESS_HOSTS`] or a subdomain of one). Every other
//! link is marked `true`.

use serde_json::Value;
use tpe_common::{PaperRecord, normalize_doi};

use crate::client::Client;
use crate::error::BiblioError;
use crate::util::{
    array, arxiv_from_doi, encode_path, first_str, host_of, str_field, strip_tags, with_query,
    year_of,
};
use crate::{CandidateKind, Found, FullTextCandidate, push_unique};

/// API base URL.
pub const BASE: &str = "https://api.crossref.org";

/// Hosts (and their subdomains) that serve full text without a subscription.
pub const OPEN_ACCESS_HOSTS: [&str; 3] = ["arxiv.org", "europepmc.org", "ncbi.nlm.nih.gov"];

/// `GET /works?query=<q>&rows=<n>&mailto=<m>`.
pub fn search_url(query: &str, rows: u32, mailto: Option<&str>) -> String {
    let n = rows.clamp(1, 1000).to_string();
    let mut pairs: Vec<(&str, &str)> = vec![("query", query), ("rows", n.as_str())];
    if let Some(m) = mailto {
        pairs.push(("mailto", m));
    }
    with_query(&format!("{BASE}/works"), &pairs)
}

/// `GET /works/<doi>?mailto=<m>`.
pub fn doi_url(doi: &str, mailto: Option<&str>) -> String {
    let path = format!("{BASE}/works/{}", encode_path(doi));
    if let Some(m) = mailto {
        with_query(&path, &[("mailto", m)])
    } else {
        path
    }
}

/// Parse a work list or a single work into records.
pub fn parse_crossref(json: &str) -> Result<Vec<PaperRecord>, BiblioError> {
    Ok(parse_crossref_found(json)?
        .into_iter()
        .map(|f| f.record)
        .collect())
}

/// Like [`parse_crossref`] but keeps `link[]` entries of type `application/pdf`.
pub fn parse_crossref_found(json: &str) -> Result<Vec<Found>, BiblioError> {
    let root: Value = serde_json::from_str(json)?;
    let Some(message) = root.get("message") else {
        return Err(BiblioError::Shape("Crossref: no `message`".to_string()));
    };
    if let Some(items) = message.get("items").and_then(Value::as_array) {
        return Ok(items.iter().map(item_to_found).collect());
    }
    if message.get("DOI").is_some() {
        return Ok(vec![item_to_found(message)]);
    }
    Err(BiblioError::Shape(
        "Crossref: `message` has neither `items` nor `DOI`".to_string(),
    ))
}

fn author_name(a: &Value) -> Option<String> {
    let given = str_field(a, "given");
    let family = str_field(a, "family");
    match (given, family) {
        (Some(g), Some(f)) => Some(format!("{g} {f}")),
        (None, Some(f)) => Some(f),
        (given, None) => str_field(a, "name").or(given),
    }
}

fn year(item: &Value) -> Option<u16> {
    ["issued", "published", "published-print", "published-online"]
        .iter()
        .find_map(|key| {
            let parts = item.get(*key)?.get("date-parts")?.get(0)?.get(0)?;
            year_of(parts)
        })
}

fn abstract_text(item: &Value) -> Option<String> {
    let raw = str_field(item, "abstract")?;
    let text = strip_tags(&raw);
    let body = text.strip_prefix("Abstract ").unwrap_or(&text);
    crate::util::non_empty(body)
}

/// True when `host` is `domain` or a subdomain of it.
fn host_is(host: &str, domain: &str) -> bool {
    host.strip_suffix(domain)
        .is_some_and(|prefix| prefix.is_empty() || prefix.ends_with('.'))
}

/// True when a `license` entry is a Creative Commons licence with no embargo.
fn has_open_license(item: &Value) -> bool {
    array(item, "license").iter().any(|license| {
        let delay = license
            .get("delay-in-days")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        delay <= 0
            && str_field(license, "URL")
                .is_some_and(|url| host_is(&host_of(&url), "creativecommons.org"))
    })
}

/// Conservative `requires_session`: `false` only for known open access (see module docs).
fn requires_session(url: &str, open_license: bool) -> bool {
    if open_license {
        return false;
    }
    let host = host_of(url);
    !OPEN_ACCESS_HOSTS
        .iter()
        .any(|domain| host_is(&host, domain))
}

fn pdf_links(item: &Value) -> Vec<FullTextCandidate> {
    let open_license = has_open_license(item);
    let mut out: Vec<FullTextCandidate> = Vec::new();
    for link in array(item, "link") {
        if str_field(link, "content-type").as_deref() != Some("application/pdf") {
            continue;
        }
        if let Some(url) = str_field(link, "URL") {
            let session = requires_session(&url, open_license);
            push_unique(
                &mut out,
                FullTextCandidate::new(&url, "crossref", CandidateKind::Pdf, session),
            );
        }
    }
    out
}

fn item_to_found(item: &Value) -> Found {
    let doi = str_field(item, "DOI").and_then(|d| normalize_doi(&d));
    let record = PaperRecord {
        title: first_str(item, "title").unwrap_or_default(),
        authors: array(item, "author")
            .iter()
            .filter_map(author_name)
            .collect(),
        year: year(item),
        venue: first_str(item, "container-title"),
        arxiv_id: doi.as_deref().and_then(arxiv_from_doi),
        source_id: doi.clone(),
        doi,
        url: str_field(item, "URL"),
        abstract_text: abstract_text(item),
        source: "crossref".to_string(),
        ..PaperRecord::default()
    };
    Found {
        record,
        candidates: pdf_links(item),
    }
}

/// Search works by free text (`query`).
pub fn fetch_search(client: &Client, query: &str, rows: u32) -> Result<Vec<Found>, BiblioError> {
    let url = search_url(query, rows, client.mailto());
    parse_crossref_found(&client.get_text(&url, &[])?)
}

/// Look up one DOI; `Ok(None)` when Crossref answers 404.
pub fn fetch_by_doi(client: &Client, doi: &str) -> Result<Option<Found>, BiblioError> {
    let url = doi_url(doi, client.mailto());
    match client.get_text(&url, &[]) {
        Ok(body) => Ok(parse_crossref_found(&body)?.into_iter().next()),
        Err(BiblioError::NotFound) => Ok(None),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = r#"{
      "status": "ok",
      "message-type": "work-list",
      "message-version": "1.0.0",
      "message": {
        "facets": {},
        "total-results": 2,
        "items": [
          {
            "DOI": "10.1038/NATURE14539",
            "URL": "https://doi.org/10.1038/nature14539",
            "type": "journal-article",
            "title": ["Deep learning"],
            "container-title": ["Nature"],
            "volume": "521", "issue": "7553", "page": "436-444",
            "author": [
              {"given": "Yann", "family": "LeCun", "sequence": "first", "affiliation": []},
              {"given": "Yoshua", "family": "Bengio", "sequence": "additional", "affiliation": []},
              {"name": "The Deep Learning Consortium", "sequence": "additional", "affiliation": []}
            ],
            "issued": {"date-parts": [[2015, 5, 27]]},
            "abstract": "<jats:title>Abstract</jats:title><jats:p>Deep learning allows computational models to learn.</jats:p>",
            "link": [
              {"URL": "https://www.nature.com/articles/nature14539.pdf", "content-type": "application/pdf", "content-version": "vor", "intended-application": "text-mining"},
              {"URL": "https://www.nature.com/articles/nature14539", "content-type": "text/html", "content-version": "vor", "intended-application": "text-mining"}
            ]
          },
          {
            "DOI": "10.5555/12345678",
            "URL": "https://doi.org/10.5555/12345678",
            "title": [],
            "container-title": [],
            "author": [{"family": "Onlyfamily"}],
            "issued": {"date-parts": [[null]]},
            "published-print": {"date-parts": [[2009]]}
          }
        ],
        "items-per-page": 2
      }
    }"#;

    #[test]
    fn parses_work_list() {
        let found = parse_crossref_found(LIST).unwrap();
        assert_eq!(found.len(), 2);
        let r = &found[0].record;
        assert_eq!(r.title, "Deep learning");
        assert_eq!(
            r.authors,
            vec![
                "Yann LeCun",
                "Yoshua Bengio",
                "The Deep Learning Consortium"
            ]
        );
        assert_eq!(r.year, Some(2015));
        assert_eq!(r.venue.as_deref(), Some("Nature"));
        assert_eq!(r.doi.as_deref(), Some("10.1038/nature14539"));
        assert_eq!(
            r.url.as_deref(),
            Some("https://doi.org/10.1038/nature14539")
        );
        assert_eq!(
            r.abstract_text.as_deref(),
            Some("Deep learning allows computational models to learn.")
        );
        assert_eq!(r.source, "crossref");
        assert_eq!(found[0].candidates.len(), 1);
        assert_eq!(
            found[0].candidates[0].url,
            "https://www.nature.com/articles/nature14539.pdf"
        );
        // Publisher PDF with no open licence: assume a subscription is needed.
        assert!(found[0].candidates[0].requires_session);
    }

    #[test]
    fn handles_empty_and_null_fields() {
        let found = parse_crossref_found(LIST).unwrap();
        let r = &found[1].record;
        assert_eq!(r.title, "");
        assert_eq!(r.venue, None);
        assert_eq!(r.authors, vec!["Onlyfamily"]);
        // issued is [[null]]: falls back to published-print.
        assert_eq!(r.year, Some(2009));
        assert!(found[1].candidates.is_empty());
    }

    #[test]
    fn single_work_and_bad_shape() {
        let single =
            r#"{"status":"ok","message-type":"work","message":{"DOI":"10.1234/A","title":["X"]}}"#;
        let recs = parse_crossref(single).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].doi.as_deref(), Some("10.1234/a"));
        // A DOI without the `10.<4-9 digits>/` shape is not invented into a record DOI.
        let invalid =
            r#"{"status":"ok","message-type":"work","message":{"DOI":"10.1/A","title":["X"]}}"#;
        assert_eq!(parse_crossref(invalid).unwrap()[0].doi, None);
        assert!(matches!(
            parse_crossref(r#"{"status":"failed"}"#),
            Err(BiblioError::Shape(_))
        ));
    }

    #[test]
    fn closed_access_pdf_link_requires_session() {
        let closed = r#"{"status":"ok","message":{"DOI":"10.1016/j.cell.2020.01.001",
            "license":[{"URL":"https://www.elsevier.com/tdm/userlicense/1.0/","delay-in-days":0}],
            "link":[{"URL":"https://api.elsevier.com/content/article/PII:X?httpAccept=text/pdf",
                     "content-type":"application/pdf"}]}}"#;
        let found = parse_crossref_found(closed).unwrap();
        assert_eq!(found[0].candidates.len(), 1);
        assert!(found[0].candidates[0].requires_session);

        // A Creative Commons licence under embargo is not open yet.
        let embargoed = r#"{"status":"ok","message":{"DOI":"10.1016/j.cell.2020.01.002",
            "license":[{"URL":"https://creativecommons.org/licenses/by/4.0/","delay-in-days":365}],
            "link":[{"URL":"https://example.com/a.pdf","content-type":"application/pdf"}]}}"#;
        assert!(parse_crossref_found(embargoed).unwrap()[0].candidates[0].requires_session);
    }

    #[test]
    fn open_access_pdf_links_need_no_session() {
        let licensed = r#"{"status":"ok","message":{"DOI":"10.1371/journal.pone.0000001",
            "license":[{"URL":"http://creativecommons.org/licenses/by/4.0/","delay-in-days":0}],
            "link":[{"URL":"https://journals.plos.org/x.pdf","content-type":"application/pdf"}]}}"#;
        assert!(!parse_crossref_found(licensed).unwrap()[0].candidates[0].requires_session);

        let oa_host = r#"{"status":"ok","message":{"DOI":"10.48550/arXiv.1706.03762",
            "link":[{"URL":"https://arxiv.org/pdf/1706.03762","content-type":"application/pdf"},
                    {"URL":"https://www.ncbi.nlm.nih.gov/pmc/articles/PMC1/pdf","content-type":"application/pdf"},
                    {"URL":"https://notarxiv.org/x.pdf","content-type":"application/pdf"}]}}"#;
        let found = parse_crossref_found(oa_host).unwrap();
        assert_eq!(found[0].candidates.len(), 3);
        assert!(!found[0].candidates[0].requires_session);
        assert!(!found[0].candidates[1].requires_session);
        // Only exact hosts and true subdomains count.
        assert!(found[0].candidates[2].requires_session);
    }

    #[test]
    fn urls() {
        assert_eq!(
            search_url("deep learning", 2, Some("me@x.org")),
            "https://api.crossref.org/works?query=deep%20learning&rows=2&mailto=me%40x.org"
        );
        assert_eq!(
            doi_url("10.1038/nature14539", None),
            "https://api.crossref.org/works/10.1038/nature14539"
        );
    }

    #[test]
    fn offline_fetch_errors() {
        let client = Client::new("t").with_offline(true);
        assert!(matches!(
            fetch_by_doi(&client, "10.1/x"),
            Err(BiblioError::Offline)
        ));
    }
}
