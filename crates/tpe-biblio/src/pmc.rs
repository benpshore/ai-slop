//! `PubMed` Central via NCBI E-utilities (`esearch` + `esummary`, `db=pmc`) and
//! Europe PMC REST search.
//!
//! Response shapes (public documentation; assumed, see crate docs):
//!
//! ```text
//! esearch:  { "esearchresult": { "count", "retmax", "retstart", "idlist": [..] } }
//! esummary: { "result": { "uids": [..], "<uid>": { "title", "authors": [ { "name" } ],
//!             "source", "fulljournalname", "pubdate", "epubdate",
//!             "articleids": [ { "idtype", "value" } ] } } }
//! Europe PMC (resultType=core):
//!   { "resultList": { "result": [ { "id", "source", "pmid", "pmcid", "doi", "title",
//!     "authorString", "authorList": { "author": [ { "fullName" } ] },
//!     "journalTitle" (lite) | "journalInfo": { "journal": { "title" } } (core),
//!     "pubYear", "abstractText",
//!     "fullTextUrlList": { "fullTextUrl": [ { "availabilityCode", "documentStyle",
//!       "site", "url" } ] } } ] } }
//! ```

use serde_json::Value;
use tpe_common::{PaperRecord, normalize_doi};

use crate::client::{Client, KEY_NCBI};
use crate::error::BiblioError;
use crate::util::{
    array, arxiv_from_doi, normalize_pmcid, normalize_pmid, str_field, strip_tags, with_query,
    year_of, year_prefix,
};
use crate::{CandidateKind, Found, FullTextCandidate, push_unique};

/// E-utilities base URL.
pub const EUTILS: &str = "https://eutils.ncbi.nlm.nih.gov/entrez/eutils";
/// Europe PMC REST base URL.
pub const EUROPE_PMC: &str = "https://www.ebi.ac.uk/europepmc/webservices/rest";

/// `esearch.fcgi?db=pmc&term=<q>&retmode=json&retmax=<n>[&api_key=<k>]`.
pub fn esearch_url(term: &str, retmax: u32, api_key: Option<&str>) -> String {
    let n = retmax.clamp(1, 10_000).to_string();
    let mut pairs: Vec<(&str, &str)> = vec![
        ("db", "pmc"),
        ("term", term),
        ("retmode", "json"),
        ("retmax", n.as_str()),
    ];
    if let Some(k) = api_key {
        pairs.push(("api_key", k));
    }
    with_query(&format!("{EUTILS}/esearch.fcgi"), &pairs)
}

/// `esummary.fcgi?db=pmc&id=<id,id>&retmode=json[&api_key=<k>]`.
pub fn esummary_url(ids: &[String], api_key: Option<&str>) -> String {
    let joined = ids.join(",");
    let mut pairs: Vec<(&str, &str)> =
        vec![("db", "pmc"), ("id", joined.as_str()), ("retmode", "json")];
    if let Some(k) = api_key {
        pairs.push(("api_key", k));
    }
    with_query(&format!("{EUTILS}/esummary.fcgi"), &pairs)
}

/// Europe PMC `search?query=<q>&format=json&resultType=core&pageSize=<n>`.
pub fn europepmc_url(query: &str, page_size: u32) -> String {
    let n = page_size.clamp(1, 1000).to_string();
    with_query(
        &format!("{EUROPE_PMC}/search"),
        &[
            ("query", query),
            ("format", "json"),
            ("resultType", "core"),
            ("pageSize", n.as_str()),
        ],
    )
}

/// The ids from an `esearch` JSON response.
pub fn parse_esearch_ids(json: &str) -> Result<Vec<String>, BiblioError> {
    let root: Value = serde_json::from_str(json)?;
    let Some(result) = root.get("esearchresult") else {
        return Err(BiblioError::Shape(
            "esearch: no `esearchresult`".to_string(),
        ));
    };
    Ok(array(result, "idlist")
        .iter()
        .filter_map(Value::as_str)
        .filter_map(crate::util::non_empty)
        .collect())
}

fn esummary_ids(doc: &Value, rec: &mut PaperRecord) {
    for id in array(doc, "articleids") {
        let kind = str_field(id, "idtype").unwrap_or_default();
        let Some(value) = str_field(id, "value") else {
            continue;
        };
        match kind.as_str() {
            "doi" => rec.doi = rec.doi.take().or_else(|| normalize_doi(&value)),
            "pmid" => rec.pmid = rec.pmid.take().or_else(|| normalize_pmid(&value)),
            "pmc" | "pmcid" => rec.pmcid = rec.pmcid.take().or_else(|| normalize_pmcid(&value)),
            _ => {}
        }
    }
}

fn esummary_doc(uid: &str, doc: &Value) -> PaperRecord {
    let mut rec = PaperRecord {
        title: str_field(doc, "title").unwrap_or_default(),
        authors: array(doc, "authors")
            .iter()
            .filter_map(|a| str_field(a, "name"))
            .collect(),
        year: str_field(doc, "pubdate")
            .and_then(|d| year_prefix(&d))
            .or_else(|| str_field(doc, "epubdate").and_then(|d| year_prefix(&d))),
        venue: str_field(doc, "fulljournalname").or_else(|| str_field(doc, "source")),
        source: "pmc".to_string(),
        source_id: Some(uid.to_string()),
        ..PaperRecord::default()
    };
    esummary_ids(doc, &mut rec);
    // In db=pmc the uid is the numeric PMC id.
    if rec.pmcid.is_none() {
        rec.pmcid = normalize_pmcid(uid);
    }
    rec.arxiv_id = rec.doi.as_deref().and_then(arxiv_from_doi);
    rec
}

/// Parse an `esummary` (`db=pmc`) JSON response into records, in `uids` order.
pub fn parse_pmc_esummary(json: &str) -> Result<Vec<PaperRecord>, BiblioError> {
    let root: Value = serde_json::from_str(json)?;
    let Some(result) = root.get("result") else {
        return Err(BiblioError::Shape("esummary: no `result`".to_string()));
    };
    let mut out: Vec<PaperRecord> = Vec::new();
    for uid in array(result, "uids").iter().filter_map(Value::as_str) {
        let Some(doc) = result.get(uid) else {
            continue;
        };
        if doc.get("error").is_none() {
            out.push(esummary_doc(uid, doc));
        }
    }
    Ok(out)
}

// Prefer structured names: fullName is commonly `Family Initials`, unlike
// Crossref's `Given Family` representation used by the resolver.
fn europepmc_author(author: &Value) -> Option<String> {
    if let Some(family) = str_field(author, "lastName") {
        return Some(
            str_field(author, "firstName")
                .or_else(|| str_field(author, "initials"))
                .map_or_else(|| family.clone(), |given| format!("{given} {family}")),
        );
    }
    str_field(author, "collectiveName")
        .or_else(|| str_field(author, "fullName").map(|name| medline_name(&name)))
}

fn medline_name(name: &str) -> String {
    if let Some((family, initials)) = name.rsplit_once(' ')
        && !initials.is_empty()
        && initials.len() <= 5
        && initials.chars().all(|c| c.is_ascii_uppercase() || c == '.')
    {
        format!("{initials} {family}")
    } else {
        name.to_string()
    }
}

fn europepmc_authors(item: &Value) -> Vec<String> {
    let listed: Vec<String> = item
        .get("authorList")
        .map(|l| array(l, "author"))
        .unwrap_or_default()
        .iter()
        .filter_map(europepmc_author)
        .collect();
    if !listed.is_empty() {
        return listed;
    }
    str_field(item, "authorString")
        .map(|s| {
            s.trim_end_matches('.')
                .split(", ")
                .filter_map(crate::util::non_empty)
                .map(|name| medline_name(&name))
                .collect()
        })
        .unwrap_or_default()
}

fn europepmc_candidates(item: &Value) -> Vec<FullTextCandidate> {
    let mut out: Vec<FullTextCandidate> = Vec::new();
    let urls = item
        .get("fullTextUrlList")
        .map(|l| array(l, "fullTextUrl"))
        .unwrap_or_default();
    for u in urls {
        let Some(url) = str_field(u, "url") else {
            continue;
        };
        let kind = if str_field(u, "documentStyle").as_deref() == Some("pdf") {
            CandidateKind::Pdf
        } else {
            CandidateKind::Landing
        };
        // Availability codes: OA = open access, F = free, S = subscription required.
        let requires_session = str_field(u, "availabilityCode").as_deref() == Some("S");
        push_unique(
            &mut out,
            FullTextCandidate::new(&url, "europepmc", kind, requires_session),
        );
    }
    out
}

fn europepmc_item(item: &Value) -> Found {
    let doi = str_field(item, "doi").and_then(|d| normalize_doi(&d));
    let record = PaperRecord {
        title: str_field(item, "title").unwrap_or_default(),
        authors: europepmc_authors(item),
        year: item.get("pubYear").and_then(year_of),
        venue: str_field(item, "journalTitle").or_else(|| {
            item.get("journalInfo")
                .and_then(|j| j.get("journal"))
                .and_then(|j| str_field(j, "title"))
        }),
        arxiv_id: doi.as_deref().and_then(arxiv_from_doi),
        doi,
        pmid: str_field(item, "pmid")
            .and_then(|p| normalize_pmid(&p))
            .or_else(|| {
                (str_field(item, "source").as_deref() == Some("MED"))
                    .then(|| str_field(item, "id").and_then(|p| normalize_pmid(&p)))
                    .flatten()
            }),
        pmcid: str_field(item, "pmcid").and_then(|p| normalize_pmcid(&p)),
        abstract_text: str_field(item, "abstractText")
            .map(|a| strip_tags(&a))
            .and_then(|a| crate::util::non_empty(&a)),
        source: "europepmc".to_string(),
        source_id: str_field(item, "id"),
        ..PaperRecord::default()
    };
    Found {
        record,
        candidates: europepmc_candidates(item),
    }
}

/// Parse a Europe PMC search response into records.
pub fn parse_europepmc(json: &str) -> Result<Vec<PaperRecord>, BiblioError> {
    Ok(parse_europepmc_found(json)?
        .into_iter()
        .map(|f| f.record)
        .collect())
}

/// Like [`parse_europepmc`] but keeps `fullTextUrlList` entries as candidates.
pub fn parse_europepmc_found(json: &str) -> Result<Vec<Found>, BiblioError> {
    let root: Value = serde_json::from_str(json)?;
    let Some(list) = root.get("resultList") else {
        return Err(BiblioError::Shape(
            "Europe PMC: no `resultList`".to_string(),
        ));
    };
    Ok(array(list, "result").iter().map(europepmc_item).collect())
}

/// Search PMC through E-utilities (`esearch` then `esummary`).
pub fn fetch_pmc_search(
    client: &Client,
    term: &str,
    retmax: u32,
) -> Result<Vec<PaperRecord>, BiblioError> {
    let key = client.key(KEY_NCBI);
    let ids = parse_esearch_ids(&client.get_text(&esearch_url(term, retmax, key), &[])?)?;
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    parse_pmc_esummary(&client.get_text(&esummary_url(&ids, key), &[])?)
}

/// Search Europe PMC.
pub fn fetch_europepmc_search(
    client: &Client,
    query: &str,
    page_size: u32,
) -> Result<Vec<Found>, BiblioError> {
    parse_europepmc_found(&client.get_text(&europepmc_url(query, page_size), &[])?)
}

/// An exact biomedical identifier; bare numeric PMC ids must never be
/// confused with `PubMed` ids. Search responses are filtered by the same identity.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Identifier {
    Pmid(String),
    Pmcid(String),
    Doi(String),
}

impl Identifier {
    /// A typed exact search, including the source restriction for a PMID.
    pub fn query(&self) -> Result<String, BiblioError> {
        let invalid = || BiblioError::Shape("invalid biomedical identifier".to_string());
        match self {
            Self::Pmid(id) => Ok(format!(
                "EXT_ID:{} AND SRC:MED",
                normalize_pmid(id).ok_or_else(invalid)?
            )),
            Self::Pmcid(id) => Ok(format!(
                "PMCID:{}",
                normalize_pmcid(id).ok_or_else(invalid)?
            )),
            Self::Doi(id) => {
                let doi = normalize_doi(id).ok_or_else(invalid)?;
                let escaped = doi.replace('\\', "\\\\").replace('"', "\\\"");
                Ok(format!("DOI:\"{escaped}\""))
            }
        }
    }

    /// Check the returned identifier, never trust search order or a fuzzy hit.
    pub fn matches(&self, record: &PaperRecord) -> bool {
        match self {
            Self::Pmid(id) => {
                normalize_pmid(id).is_some_and(|id| record.pmid.as_deref() == Some(&id))
            }
            Self::Pmcid(id) => {
                normalize_pmcid(id).is_some_and(|id| record.pmcid.as_deref() == Some(&id))
            }
            Self::Doi(id) => normalize_doi(id).is_some_and(|id| record.doi.as_deref() == Some(&id)),
        }
    }
}

/// Fetch core metadata for an exact PMID, PMCID or DOI through Europe PMC.
/// Multiple exact records are retained so callers can report ambiguity.
pub fn fetch_identifier(client: &Client, id: &Identifier) -> Result<Vec<Found>, BiblioError> {
    let query = id.query()?;
    Ok(fetch_europepmc_search(client, &query, 10)?
        .into_iter()
        .filter(|found| id.matches(&found.record))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ESEARCH: &str = r#"{
      "header": {"type": "esearch", "version": "0.3"},
      "esearchresult": {"count": "2", "retmax": "2", "retstart": "0",
        "idlist": ["5815332", "7096066"], "translationset": [], "querytranslation": "open access[All Fields]"}
    }"#;

    const ESUMMARY: &str = r#"{
      "header": {"type": "esummary", "version": "0.3"},
      "result": {
        "uids": ["5815332", "7096066"],
        "5815332": {
          "uid": "5815332",
          "pubdate": "2018",
          "epubdate": "2018 Feb 13",
          "source": "PeerJ",
          "authors": [{"name": "Piwowar H", "authtype": "Author"}, {"name": "Priem J", "authtype": "Author"}],
          "title": "The state of OA: a large-scale analysis",
          "fulljournalname": "PeerJ",
          "articleids": [
            {"idtype": "pmid", "idtypen": 1, "value": "29456894"},
            {"idtype": "doi", "idtypen": 3, "value": "10.7717/peerj.4375"},
            {"idtype": "pmcid", "idtypen": 5, "value": "PMC5815332"}
          ]
        },
        "7096066": {"uid": "7096066", "error": "cannot get document summary"}
      }
    }"#;

    const EUROPE: &str = r#"{
      "version": "6.9", "hitCount": 1, "nextCursorMark": "AoIIQ",
      "request": {"queryString": "DOI:10.7717/peerj.4375", "resultType": "core"},
      "resultList": {"result": [
        {
          "id": "29456894", "source": "MED", "pmid": "29456894", "pmcid": "PMC5815332",
          "doi": "10.7717/peerj.4375",
          "title": "The state of OA: a large-scale analysis of the prevalence and impact of Open Access articles.",
          "authorString": "Piwowar H, Priem J, Larivière V.",
          "authorList": {"author": [
            {"fullName": "Piwowar H", "firstName": "Heather", "lastName": "Piwowar"},
            {"fullName": "Priem J", "firstName": "Jason", "lastName": "Priem"}
          ]},
          "journalInfo": {"volume": "6", "journal": {"title": "PeerJ", "isoabbreviation": "PeerJ"}},
          "pubYear": "2018",
          "abstractText": "Despite growing interest in Open Access (OA) to scholarly literature.",
          "fullTextUrlList": {"fullTextUrl": [
            {"availability": "Open access", "availabilityCode": "OA", "documentStyle": "pdf", "site": "Europe_PMC", "url": "https://europepmc.org/articles/PMC5815332?pdf=render"},
            {"availability": "Subscription required", "availabilityCode": "S", "documentStyle": "doi", "site": "DOI", "url": "https://doi.org/10.7717/peerj.4375"}
          ]}
        },
        {"id": "PPR1", "source": "PPR", "title": "Preprint only", "authorString": "Lee K, Kim S.", "pubYear": "2021"}
      ]}
    }"#;

    #[test]
    fn exact_identifier_queries_and_returned_identity() {
        assert_eq!(
            Identifier::Pmid("29456894".into()).query().unwrap(),
            "EXT_ID:29456894 AND SRC:MED"
        );
        assert_eq!(
            Identifier::Pmcid("PMC5815332".into()).query().unwrap(),
            "PMCID:PMC5815332"
        );
        assert_eq!(
            Identifier::Doi("10.7717/PeerJ.4375".into())
                .query()
                .unwrap(),
            "DOI:\"10.7717/peerj.4375\""
        );
        assert!(Identifier::Pmid("123 OR 456".into()).query().is_err());
        let record = parse_europepmc(EUROPE).unwrap().remove(0);
        assert!(Identifier::Pmid("29456894".into()).matches(&record));
        assert!(!Identifier::Pmid("5815332".into()).matches(&record));
        assert!(Identifier::Pmcid("PMC5815332".into()).matches(&record));
    }

    #[test]
    fn med_source_ids_and_author_names_are_not_confused() {
        let json = r#"{"resultList":{"result":[{"id":"123456","source":"MED","authorList":{"author":[{"fullName":"van der Kogel A"}]}},{"id":"123456","source":"AGR"}]}}"#;
        let records = parse_europepmc(json).unwrap();
        assert_eq!(records[0].pmid.as_deref(), Some("123456"));
        assert_eq!(records[1].pmid, None);
        assert_eq!(records[0].authors, ["A van der Kogel"]);
    }

    #[test]
    fn esearch_ids() {
        assert_eq!(
            parse_esearch_ids(ESEARCH).unwrap(),
            vec!["5815332", "7096066"]
        );
        assert!(matches!(
            parse_esearch_ids("{}"),
            Err(BiblioError::Shape(_))
        ));
    }

    #[test]
    fn esummary_records_skip_errors() {
        let recs = parse_pmc_esummary(ESUMMARY).unwrap();
        assert_eq!(recs.len(), 1);
        let r = &recs[0];
        assert_eq!(r.title, "The state of OA: a large-scale analysis");
        assert_eq!(r.authors, vec!["Piwowar H", "Priem J"]);
        assert_eq!(r.year, Some(2018));
        assert_eq!(r.venue.as_deref(), Some("PeerJ"));
        assert_eq!(r.doi.as_deref(), Some("10.7717/peerj.4375"));
        assert_eq!(r.pmid.as_deref(), Some("29456894"));
        assert_eq!(r.pmcid.as_deref(), Some("PMC5815332"));
        assert_eq!(r.source, "pmc");
    }

    #[test]
    fn europepmc_core_results() {
        let found = parse_europepmc_found(EUROPE).unwrap();
        assert_eq!(found.len(), 2);
        let r = &found[0].record;
        assert_eq!(r.venue.as_deref(), Some("PeerJ"));
        assert_eq!(r.authors, vec!["Heather Piwowar", "Jason Priem"]);
        assert_eq!(r.year, Some(2018));
        assert_eq!(r.pmcid.as_deref(), Some("PMC5815332"));
        assert_eq!(found[0].candidates.len(), 2);
        assert_eq!(found[0].candidates[0].kind, CandidateKind::Pdf);
        assert!(!found[0].candidates[0].requires_session);
        assert!(found[0].candidates[1].requires_session);
        // Second result: no authorList, no journal, no DOI.
        let p = &found[1].record;
        assert_eq!(p.authors, vec!["K Lee", "S Kim"]);
        assert_eq!(p.venue, None);
        assert_eq!(p.doi, None);
        assert!(found[1].candidates.is_empty());
    }

    #[test]
    fn urls_include_key_only_when_present() {
        assert_eq!(
            esearch_url("open access", 2, None),
            "https://eutils.ncbi.nlm.nih.gov/entrez/eutils/esearch.fcgi?db=pmc&term=open%20access&retmode=json&retmax=2"
        );
        let with_key = esummary_url(&["1".to_string(), "2".to_string()], Some("K"));
        assert!(with_key.ends_with("id=1%2C2&retmode=json&api_key=K"));
        assert!(europepmc_url("x", 5).contains("resultType=core"));
    }

    #[test]
    fn offline_fetch_errors() {
        let client = Client::new("t").with_offline(true);
        assert!(matches!(
            fetch_pmc_search(&client, "x", 1),
            Err(BiblioError::Offline)
        ));
        assert!(matches!(
            fetch_europepmc_search(&client, "x", 1),
            Err(BiblioError::Offline)
        ));
    }
}
