//! `OpenURL` 1.0 (Z39.88-2004 KEV) link-resolver queries and the Unpaywall lookup.
//!
//! Unpaywall response shape (public v2 API; assumed, see crate docs):
//!
//! ```text
//! { "doi", "is_oa",
//!   "best_oa_location": { "url", "url_for_pdf", "url_for_landing_page", "host_type" } | null,
//!   "oa_locations": [ location... ] }
//! ```

use serde_json::Value;
use tpe_common::PaperRecord;

use crate::client::Client;
use crate::error::BiblioError;
use crate::util::{array, encode_path, str_field, with_query};
use crate::{CandidateKind, FullTextCandidate, push_unique};

/// Unpaywall API base URL.
pub const UNPAYWALL: &str = "https://api.unpaywall.org/v2";

/// Build an `OpenURL` 1.0 KEV query against a library link resolver `base`.
///
/// Emits `rft_id=info:doi/...` plus `rft.doi`, `rft_id=info:pmid/...`,
/// `rft.title`, `rft.jtitle`, one `rft.au` per author and `rft.date`, each only
/// when the record has the field.
pub fn build_openurl(base: &str, rec: &PaperRecord) -> String {
    let year = rec.year.map(|y| y.to_string());
    let doi_id = rec.doi.as_ref().map(|d| format!("info:doi/{d}"));
    let pmid_id = rec.pmid.as_ref().map(|p| format!("info:pmid/{p}"));
    let mut pairs: Vec<(&str, &str)> = vec![
        ("url_ver", "Z39.88-2004"),
        ("ctx_ver", "Z39.88-2004"),
        ("rft_val_fmt", "info:ofi/fmt:kev:mtx:journal"),
    ];
    if let (Some(id), Some(doi)) = (doi_id.as_deref(), rec.doi.as_deref()) {
        pairs.push(("rft_id", id));
        pairs.push(("rft.doi", doi));
    }
    if let Some(id) = pmid_id.as_deref() {
        pairs.push(("rft_id", id));
    }
    if !rec.title.trim().is_empty() {
        pairs.push(("rft.title", rec.title.trim()));
    }
    if let Some(venue) = rec.venue.as_deref() {
        pairs.push(("rft.jtitle", venue));
    }
    for author in &rec.authors {
        pairs.push(("rft.au", author.as_str()));
    }
    if let Some(y) = year.as_deref() {
        pairs.push(("rft.date", y));
    }
    with_query(base, &pairs)
}

/// `GET /v2/<doi>?email=<m>`.
pub fn unpaywall_url(doi: &str, email: &str) -> String {
    with_query(
        &format!("{UNPAYWALL}/{}", encode_path(doi)),
        &[("email", email)],
    )
}

fn location_candidate(loc: &Value) -> Option<FullTextCandidate> {
    if let Some(pdf) = str_field(loc, "url_for_pdf") {
        return Some(FullTextCandidate::new(
            &pdf,
            "unpaywall",
            CandidateKind::Pdf,
            false,
        ));
    }
    str_field(loc, "url_for_landing_page")
        .map(|u| FullTextCandidate::new(&u, "unpaywall", CandidateKind::Landing, false))
}

/// Parse an Unpaywall v2 response: `best_oa_location` first (PDF, else landing
/// page), then the other `oa_locations`, without duplicate URLs.
pub fn parse_unpaywall(json: &str) -> Result<Vec<FullTextCandidate>, BiblioError> {
    let root: Value = serde_json::from_str(json)?;
    if root.get("doi").is_none() {
        return Err(BiblioError::Shape("Unpaywall: no `doi`".to_string()));
    }
    let mut out: Vec<FullTextCandidate> = Vec::new();
    if let Some(best) = root.get("best_oa_location").and_then(location_candidate) {
        push_unique(&mut out, best);
    }
    for loc in array(&root, "oa_locations") {
        if let Some(c) = location_candidate(loc) {
            push_unique(&mut out, c);
        }
    }
    Ok(out)
}

/// Look up open-access locations for a DOI (needs the client's `mailto`).
pub fn fetch_unpaywall(client: &Client, doi: &str) -> Result<Vec<FullTextCandidate>, BiblioError> {
    let Some(email) = client.mailto() else {
        return Err(BiblioError::MissingConfig(
            "mailto (Unpaywall requires an e-mail)",
        ));
    };
    match client.get_text(&unpaywall_url(doi, email), &[]) {
        Ok(body) => parse_unpaywall(&body),
        Err(BiblioError::NotFound) => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UNPAYWALL_JSON: &str = r#"{
      "doi": "10.7717/peerj.4375",
      "doi_url": "https://doi.org/10.7717/peerj.4375",
      "is_oa": true,
      "oa_status": "gold",
      "title": "The state of OA",
      "year": 2018,
      "journal_name": "PeerJ",
      "best_oa_location": {
        "host_type": "publisher", "is_best": true, "license": "cc-by", "version": "publishedVersion",
        "url": "https://peerj.com/articles/4375.pdf",
        "url_for_pdf": "https://peerj.com/articles/4375.pdf",
        "url_for_landing_page": "https://doi.org/10.7717/peerj.4375"
      },
      "oa_locations": [
        {"host_type": "publisher", "url_for_pdf": "https://peerj.com/articles/4375.pdf", "url_for_landing_page": "https://doi.org/10.7717/peerj.4375"},
        {"host_type": "repository", "url_for_pdf": null, "url_for_landing_page": "https://europepmc.org/articles/pmc5815332"}
      ],
      "z_authors": [{"given": "Heather", "family": "Piwowar"}]
    }"#;

    #[test]
    fn unpaywall_best_location_first() {
        let c = parse_unpaywall(UNPAYWALL_JSON).unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].url, "https://peerj.com/articles/4375.pdf");
        assert_eq!(c[0].kind, CandidateKind::Pdf);
        assert_eq!(c[1].kind, CandidateKind::Landing);
        assert_eq!(c[1].source, "unpaywall");
    }

    #[test]
    fn unpaywall_closed_article() {
        let closed =
            r#"{"doi": "10.1/x", "is_oa": false, "best_oa_location": null, "oa_locations": []}"#;
        assert!(parse_unpaywall(closed).unwrap().is_empty());
        assert!(matches!(
            parse_unpaywall(r#"{"HTTP_status_code": 404}"#),
            Err(BiblioError::Shape(_))
        ));
    }

    #[test]
    fn openurl_contains_expected_keys() {
        let rec = PaperRecord {
            title: "Deep learning".to_string(),
            authors: vec!["Yann LeCun".to_string(), "Yoshua Bengio".to_string()],
            year: Some(2015),
            venue: Some("Nature".to_string()),
            doi: Some("10.1038/nature14539".to_string()),
            source: "crossref".to_string(),
            ..PaperRecord::default()
        };
        let url = build_openurl("https://resolver.example.edu/openurl?sid=tpe", &rec);
        assert_eq!(
            url,
            "https://resolver.example.edu/openurl?sid=tpe&url_ver=Z39.88-2004&ctx_ver=Z39.88-2004\
             &rft_val_fmt=info%3Aofi%2Ffmt%3Akev%3Amtx%3Ajournal\
             &rft_id=info%3Adoi%2F10.1038%2Fnature14539&rft.doi=10.1038%2Fnature14539\
             &rft.title=Deep%20learning&rft.jtitle=Nature\
             &rft.au=Yann%20LeCun&rft.au=Yoshua%20Bengio&rft.date=2015"
        );
    }

    #[test]
    fn openurl_omits_missing_fields() {
        let rec = PaperRecord {
            title: "Only a title".to_string(),
            ..PaperRecord::default()
        };
        let url = build_openurl("https://r.example/", &rec);
        assert!(url.starts_with("https://r.example/?url_ver="));
        assert!(!url.contains("rft.doi"));
        assert!(!url.contains("rft.date"));
        assert!(url.ends_with("&rft.title=Only%20a%20title"));
    }

    #[test]
    fn unpaywall_needs_mailto_and_network() {
        let no_mail = Client::new("t").with_offline(true);
        assert!(matches!(
            fetch_unpaywall(&no_mail, "10.1/x"),
            Err(BiblioError::MissingConfig(_))
        ));
        let offline = Client::new("t").with_offline(true).with_mailto("a@b.org");
        assert!(matches!(
            fetch_unpaywall(&offline, "10.1/x"),
            Err(BiblioError::Offline)
        ));
        assert_eq!(
            unpaywall_url("10.1/x", "a@b.org"),
            "https://api.unpaywall.org/v2/10.1/x?email=a%40b.org"
        );
    }
}
