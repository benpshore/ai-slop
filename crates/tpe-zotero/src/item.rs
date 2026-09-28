//! Zotero item JSON (Web API v3, `format=json`) and its mapping to and from
//! [`PaperRecord`].
//!
//! Mapping rules (never invented: a field stays `None` without evidence):
//! * `title` <- `title`; `abstract_text` <- `abstractNote`; `url` <- `url`;
//! * `authors` <- creators whose `creatorType` is `author` (`name`, or
//!   `firstName lastName`);
//! * `year` <- the first four-digit year in `date`;
//! * `venue` <- the type's container field (see [`venue_fields`]);
//! * `doi` <- `DOI`, else a `DOI:` line in `extra`;
//! * `arxiv_id` <- an `arXiv:` line in `extra`, else `archiveID` (preprints);
//! * `pmid` / `pmcid` <- `PMID:` / `PMCID:` lines in `extra`.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};
use tpe_common::{PaperRecord, normalize_arxiv_id, normalize_doi};

use crate::error::ZError;

/// Value of [`PaperRecord::source`] for records produced by this crate.
pub const SOURCE: &str = "zotero";

/// Item types whose venue field is known to this mapping.
pub const MAPPED_ITEM_TYPES: [&str; 7] = [
    "journalArticle",
    "preprint",
    "conferencePaper",
    "book",
    "bookSection",
    "report",
    "thesis",
];

/// Item types that carry a dedicated `DOI` field in this mapping; for other
/// types the DOI is written to `extra` as a `DOI:` line.
const DOI_FIELD_TYPES: [&str; 3] = ["journalArticle", "conferencePaper", "preprint"];

/// Candidate container ("venue") fields for an item type, in preference order.
pub fn venue_fields(item_type: &str) -> &'static [&'static str] {
    match item_type {
        "conferencePaper" => &["proceedingsTitle", "conferenceName"],
        "bookSection" => &["bookTitle"],
        "book" => &["publisher"],
        "report" => &["institution"],
        "thesis" => &["university"],
        "preprint" => &["repository"],
        _ => &["publicationTitle"],
    }
}

/// One creator of an item as the API represents it: either a two-field
/// name (`firstName` + `lastName`) or a single-field `name`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ZCreator {
    /// `author`, `editor`, `contributor`, ...
    pub creator_type: String,
    /// Given name (two-field mode).
    pub first_name: Option<String>,
    /// Family name (two-field mode).
    pub last_name: Option<String>,
    /// Full name (single-field mode).
    pub name: Option<String>,
}

impl ZCreator {
    /// Display form: the single-field `name`, else `firstName lastName`.
    pub fn display_name(&self) -> Option<String> {
        if let Some(name) = non_empty(self.name.as_deref()) {
            return Some(name.to_string());
        }
        let parts: Vec<&str> = [self.first_name.as_deref(), self.last_name.as_deref()]
            .into_iter()
            .filter_map(non_empty)
            .collect();
        if parts.is_empty() {
            None
        } else {
            Some(parts.join(" "))
        }
    }

    /// Build an author creator from a display string. `"Family, Given"`
    /// becomes a two-field name; anything else is kept as a single-field
    /// name rather than guessing where the family name starts.
    pub fn author_from_display(display: &str) -> Self {
        let display = display.trim();
        match display.split_once(',') {
            Some((last, first)) if !last.trim().is_empty() && !first.trim().is_empty() => Self {
                creator_type: "author".to_string(),
                first_name: Some(first.trim().to_string()),
                last_name: Some(last.trim().to_string()),
                name: None,
            },
            _ => Self {
                creator_type: "author".to_string(),
                first_name: None,
                last_name: None,
                name: Some(display.to_string()),
            },
        }
    }

    /// The creator as API JSON (`creatorType` plus either `name` or
    /// `firstName`/`lastName`).
    pub fn to_json(&self) -> Value {
        let mut map = Map::new();
        map.insert("creatorType".to_string(), json!(self.creator_type));
        if let Some(name) = &self.name {
            map.insert("name".to_string(), json!(name));
        } else {
            map.insert(
                "firstName".to_string(),
                json!(self.first_name.clone().unwrap_or_default()),
            );
            map.insert(
                "lastName".to_string(),
                json!(self.last_name.clone().unwrap_or_default()),
            );
        }
        Value::Object(map)
    }

    /// True when this creator counts as an author of the work.
    pub fn is_author(&self) -> bool {
        self.creator_type.is_empty() || self.creator_type == "author"
    }
}

/// One Zotero item from a `format=json` response: the envelope's `key` and
/// `version` plus the editable `data` object.
#[derive(Clone, Debug, PartialEq)]
pub struct ZItem {
    /// 8-character object key.
    pub key: String,
    /// Object version (for `If-Unmodified-Since-Version`).
    pub version: u64,
    /// `data.itemType`.
    pub item_type: String,
    /// The editable JSON (`data` property).
    pub data: Map<String, Value>,
}

impl ZItem {
    /// Parse one object envelope (`{"key", "version", "data": {...}}`).
    pub fn from_value(value: &Value) -> Result<Self, ZError> {
        let data = value
            .get("data")
            .and_then(Value::as_object)
            .ok_or_else(|| ZError::Parse("item without a data object".to_string()))?;
        let key = value
            .get("key")
            .and_then(Value::as_str)
            .or_else(|| data.get("key").and_then(Value::as_str))
            .ok_or_else(|| ZError::Parse("item without a key".to_string()))?
            .to_string();
        let version = value
            .get("version")
            .and_then(Value::as_u64)
            .or_else(|| data.get("version").and_then(Value::as_u64))
            .unwrap_or(0);
        let item_type = data
            .get("itemType")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        Ok(Self {
            key,
            version,
            item_type,
            data: data.clone(),
        })
    }

    /// Parse a single-object response body.
    pub fn parse_one(body: &str) -> Result<Self, ZError> {
        let value: Value = serde_json::from_str(body)?;
        Self::from_value(&value)
    }

    /// Parse a multi-object response body (a JSON array of envelopes).
    pub fn parse_many(body: &str) -> Result<Vec<Self>, ZError> {
        let value: Value = serde_json::from_str(body)?;
        let array = value
            .as_array()
            .ok_or_else(|| ZError::Parse("expected a JSON array of items".to_string()))?;
        array.iter().map(Self::from_value).collect()
    }

    /// A non-empty, trimmed string field of `data`.
    pub fn field(&self, name: &str) -> Option<&str> {
        non_empty(self.data.get(name).and_then(Value::as_str))
    }

    /// The parent item key of a child note or attachment.
    pub fn parent_item(&self) -> Option<&str> {
        self.field("parentItem")
    }

    /// All string fields of `data` (non-string values are skipped).
    pub fn string_fields(&self) -> BTreeMap<String, String> {
        self.data
            .iter()
            .filter_map(|(name, value)| value.as_str().map(|s| (name.clone(), s.to_string())))
            .collect()
    }

    /// `data.creators` in order.
    pub fn creators(&self) -> Vec<ZCreator> {
        let Some(list) = self.data.get("creators").and_then(Value::as_array) else {
            return Vec::new();
        };
        list.iter()
            .map(|c| ZCreator {
                creator_type: c
                    .get("creatorType")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                first_name: owned(c.get("firstName").and_then(Value::as_str)),
                last_name: owned(c.get("lastName").and_then(Value::as_str)),
                name: owned(c.get("name").and_then(Value::as_str)),
            })
            .collect()
    }

    /// Map this item to a [`PaperRecord`] (see the module docs for rules).
    pub fn to_record(&self) -> PaperRecord {
        record_from_fields(
            &self.item_type,
            &self.string_fields(),
            &self.creators(),
            Some(&self.key),
        )
    }
}

/// Map an item's fields and creators to a [`PaperRecord`]. Shared by the
/// Web API items and the local `zotero.sqlite` reader.
pub fn record_from_fields(
    item_type: &str,
    fields: &BTreeMap<String, String>,
    creators: &[ZCreator],
    key: Option<&str>,
) -> PaperRecord {
    let extra = parse_extra(field(fields, "extra").unwrap_or_default());
    let doi = field(fields, "DOI")
        .and_then(normalize_doi)
        .or_else(|| field(&extra, "doi").and_then(normalize_doi));
    let arxiv_id = field(&extra, "arxiv")
        .and_then(normalize_arxiv_id)
        .or_else(|| field(fields, "archiveID").and_then(normalize_arxiv_id));
    let pmid = field(&extra, "pmid")
        .filter(|v| v.chars().all(|c| c.is_ascii_digit()))
        .map(str::to_string);
    let pmc_id = field(&extra, "pmcid").and_then(normalize_pmcid);
    let venue = venue_fields(item_type)
        .iter()
        .find_map(|name| field(fields, name))
        .map(str::to_string);
    let authors: Vec<String> = creators
        .iter()
        .filter(|c| c.is_author())
        .filter_map(ZCreator::display_name)
        .collect();
    PaperRecord {
        title: field(fields, "title").unwrap_or_default().to_string(),
        authors,
        year: field(fields, "date").and_then(year_from_date),
        venue,
        doi,
        arxiv_id,
        pmid,
        pmcid: pmc_id,
        url: field(fields, "url").map(str::to_string),
        abstract_text: field(fields, "abstractNote").map(str::to_string),
        source: SOURCE.to_string(),
        source_id: key.map(str::to_string),
    }
}

/// Parse the `extra` field's `Key: value` lines. Keys are lower-cased; the
/// first occurrence of a key wins; lines without a colon are ignored.
pub fn parse_extra(extra: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in extra.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        if name.is_empty() || value.is_empty() {
            continue;
        }
        out.entry(name).or_insert_with(|| value.to_string());
    }
    out
}

/// The first four-digit year (1000–2999) in a Zotero date string, whether
/// free text (`"March 2020"`) or the stored multipart form
/// (`"2019-00-00 2019"`). Digits that are part of a longer number are skipped.
pub fn year_from_date(date: &str) -> Option<u16> {
    let bytes = date.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i - start == 4 {
            let year = date[start..i].parse::<u16>().ok();
            if let Some(year) = year.filter(|y| (1000..=2999).contains(y)) {
                return Some(year);
            }
        }
    }
    None
}

/// `PMC` followed by digits, upper-cased; anything else is rejected.
fn normalize_pmcid(raw: &str) -> Option<String> {
    let upper = raw.trim().to_ascii_uppercase();
    let digits = upper.strip_prefix("PMC")?;
    if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
        Some(upper)
    } else {
        None
    }
}

/// A non-empty, trimmed field value from a field map.
fn field<'a>(fields: &'a BTreeMap<String, String>, name: &str) -> Option<&'a str> {
    non_empty(fields.get(name).map(String::as_str))
}

/// Trim and drop empty strings.
fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|s| !s.is_empty())
}

/// Owned copy of a non-empty string.
fn owned(value: Option<&str>) -> Option<String> {
    non_empty(value).map(str::to_string)
}

/// A JSON object to send in a write request: a new item (for `POST /items`)
/// or the changed properties of an existing one (for `PATCH`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ZItemPatch {
    /// The properties to send, exactly as they will be serialised.
    pub fields: Map<String, Value>,
}

impl ZItemPatch {
    /// An empty patch.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set one property (builder style).
    #[must_use]
    pub fn set(mut self, name: &str, value: Value) -> Self {
        self.fields.insert(name.to_string(), value);
        self
    }

    /// The JSON object that is sent.
    pub fn to_json(&self) -> Value {
        Value::Object(self.fields.clone())
    }

    /// A reasonable item type for a record: `preprint` when it has an
    /// `arXiv` id and no venue, else `journalArticle`.
    pub fn suggest_item_type(record: &PaperRecord) -> &'static str {
        if record.arxiv_id.is_some() && record.venue.is_none() {
            "preprint"
        } else {
            "journalArticle"
        }
    }

    /// A new top-level item from a record. Includes the properties the write
    /// docs list as required for new items (`itemType`, `tags`,
    /// `collections`, `relations`). Identifiers without a dedicated field
    /// go to `extra` as `arXiv:` / `PMID:` / `PMCID:` / `DOI:` lines.
    pub fn from_record(record: &PaperRecord, item_type: &str) -> Self {
        let creators: Vec<Value> = record
            .authors
            .iter()
            .map(|a| ZCreator::author_from_display(a).to_json())
            .collect();
        let mut patch = Self::new()
            .set("itemType", json!(item_type))
            .set("title", json!(record.title))
            .set("creators", Value::Array(creators))
            .set("tags", json!([]))
            .set("collections", json!([]))
            .set("relations", json!({}));
        if let Some(text) = &record.abstract_text {
            patch = patch.set("abstractNote", json!(text));
        }
        if let Some(year) = record.year {
            patch = patch.set("date", json!(year.to_string()));
        }
        if let Some(url) = &record.url {
            patch = patch.set("url", json!(url));
        }
        if let (Some(venue), Some(name)) = (&record.venue, venue_fields(item_type).first()) {
            patch = patch.set(name, json!(venue));
        }
        let mut extra: Vec<String> = Vec::new();
        if let Some(doi) = &record.doi {
            if DOI_FIELD_TYPES.contains(&item_type) {
                patch = patch.set("DOI", json!(doi));
            } else {
                extra.push(format!("DOI: {doi}"));
            }
        }
        if let Some(id) = &record.arxiv_id {
            extra.push(format!("arXiv: {id}"));
        }
        if let Some(id) = &record.pmid {
            extra.push(format!("PMID: {id}"));
        }
        if let Some(id) = &record.pmcid {
            extra.push(format!("PMCID: {id}"));
        }
        if !extra.is_empty() {
            patch = patch.set("extra", json!(extra.join("\n")));
        }
        patch
    }

    /// A child note (HTML) under `parent`.
    pub fn note(parent: &str, html: &str) -> Self {
        Self::new()
            .set("itemType", json!("note"))
            .set("parentItem", json!(parent))
            .set("note", json!(html))
            .set("tags", json!([]))
            .set("relations", json!({}))
    }

    /// A child `linked_url` attachment (a link, no file) under `parent`.
    pub fn linked_url_attachment(parent: &str, url: &str, title: &str) -> Self {
        Self::new()
            .set("itemType", json!("attachment"))
            .set("linkMode", json!("linked_url"))
            .set("parentItem", json!(parent))
            .set("title", json!(title))
            .set("url", json!(url))
            .set("accessDate", json!(""))
            .set("note", json!(""))
            .set("contentType", json!(""))
            .set("charset", json!(""))
            .set("tags", json!([]))
            .set("relations", json!({}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const JOURNAL: &str = r#"{
      "key": "ABCD2345", "version": 1204,
      "library": {"type": "user", "id": 475425, "name": "x"},
      "links": {}, "meta": {"numChildren": 1},
      "data": {
        "key": "ABCD2345", "version": 1204, "itemType": "journalArticle",
        "title": "Attention Is All You Need in Zotero",
        "creators": [
          {"creatorType": "author", "firstName": "Ada", "lastName": "Lovelace"},
          {"creatorType": "author", "name": "Zotero Consortium"},
          {"creatorType": "editor", "firstName": "Ed", "lastName": "Itor"}
        ],
        "abstractNote": "We test mapping.",
        "publicationTitle": "Journal of Tests",
        "volume": "12", "issue": "3", "pages": "1-10",
        "date": "March 2019",
        "DOI": "https://doi.org/10.1000/ABC.123",
        "url": "https://example.org/a",
        "extra": "PMID: 31234567\nPMCID: pmc6543210\nCitation Key: lovelace2019",
        "tags": [], "collections": ["BCDE3456"], "relations": {},
        "dateAdded": "2019-05-01T10:00:00Z", "dateModified": "2019-05-02T10:00:00Z"
      }
    }"#;

    const PREPRINT: &str = r#"{
      "key": "PREP2345", "version": 7,
      "data": {
        "key": "PREP2345", "version": 7, "itemType": "preprint",
        "title": "A Preprint", "creators": [{"creatorType": "author", "firstName": "", "lastName": "Solo"}],
        "repository": "arXiv", "archiveID": "arXiv:2502.00857",
        "date": "2025-02-02", "DOI": "", "url": "http://arxiv.org/abs/2502.00857v2",
        "extra": "arXiv: 2502.00857v2", "abstractNote": "",
        "tags": [], "collections": [], "relations": {}
      }
    }"#;

    const CONFERENCE: &str = r#"{
      "key": "CONF2345", "version": 3,
      "data": {
        "key": "CONF2345", "version": 3, "itemType": "conferencePaper",
        "title": "Conference Things",
        "creators": [{"creatorType": "author", "firstName": "Grace", "lastName": "Hopper"}],
        "proceedingsTitle": "", "conferenceName": "PLDI 2021",
        "date": "2021-06-20", "extra": "DOI: 10.1145/3453483.3454035",
        "tags": [], "collections": [], "relations": {}
      }
    }"#;

    #[test]
    fn journal_article_maps_to_record() {
        let item = ZItem::parse_one(JOURNAL).unwrap();
        assert_eq!(item.key, "ABCD2345");
        assert_eq!(item.version, 1204);
        let rec = item.to_record();
        assert_eq!(rec.title, "Attention Is All You Need in Zotero");
        assert_eq!(rec.authors, vec!["Ada Lovelace", "Zotero Consortium"]);
        assert_eq!(rec.year, Some(2019));
        assert_eq!(rec.venue.as_deref(), Some("Journal of Tests"));
        assert_eq!(rec.doi.as_deref(), Some("10.1000/abc.123"));
        assert_eq!(rec.pmid.as_deref(), Some("31234567"));
        assert_eq!(rec.pmcid.as_deref(), Some("PMC6543210"));
        assert_eq!(rec.arxiv_id, None);
        assert_eq!(rec.abstract_text.as_deref(), Some("We test mapping."));
        assert_eq!(rec.source, "zotero");
        assert_eq!(rec.source_id.as_deref(), Some("ABCD2345"));
    }

    #[test]
    fn preprint_maps_arxiv_and_repository() {
        let rec = ZItem::parse_one(PREPRINT).unwrap().to_record();
        assert_eq!(rec.arxiv_id.as_deref(), Some("2502.00857"));
        assert_eq!(rec.venue.as_deref(), Some("arXiv"));
        assert_eq!(rec.authors, vec!["Solo"]);
        assert_eq!(rec.year, Some(2025));
        assert_eq!(rec.doi, None);
        assert_eq!(rec.abstract_text, None);
    }

    #[test]
    fn conference_paper_uses_conference_name_and_extra_doi() {
        let rec = ZItem::parse_one(CONFERENCE).unwrap().to_record();
        assert_eq!(rec.venue.as_deref(), Some("PLDI 2021"));
        assert_eq!(rec.doi.as_deref(), Some("10.1145/3453483.3454035"));
        assert_eq!(rec.year, Some(2021));
    }

    #[test]
    fn parse_many_reads_an_array() {
        let body = format!("[{JOURNAL},{PREPRINT}]");
        let items = ZItem::parse_many(&body).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[1].item_type, "preprint");
        assert!(ZItem::parse_many("{}").is_err());
    }

    #[test]
    fn years_from_zotero_dates() {
        assert_eq!(year_from_date("2019-00-00 2019"), Some(2019));
        assert_eq!(year_from_date("5/3/2020"), Some(2020));
        assert_eq!(year_from_date("no date"), None);
        assert_eq!(year_from_date("123456"), None);
    }

    #[test]
    fn extra_lines_parse_case_insensitively() {
        let extra = parse_extra("arXiv: 1234.5678\nnot a pair\nPMID: 1\npmid: 2");
        assert_eq!(extra.get("arxiv").map(String::as_str), Some("1234.5678"));
        assert_eq!(extra.get("pmid").map(String::as_str), Some("1"));
    }

    #[test]
    fn patch_from_record_journal() {
        let rec = PaperRecord {
            title: "T".to_string(),
            authors: vec!["Lovelace, Ada".to_string(), "Zotero Consortium".to_string()],
            year: Some(2019),
            venue: Some("J".to_string()),
            doi: Some("10.1/x".to_string()),
            pmid: Some("42".to_string()),
            source: "test".to_string(),
            ..PaperRecord::default()
        };
        let body = ZItemPatch::from_record(&rec, "journalArticle").to_json();
        assert_eq!(
            body,
            json!({
                "itemType": "journalArticle",
                "title": "T",
                "creators": [
                    {"creatorType": "author", "firstName": "Ada", "lastName": "Lovelace"},
                    {"creatorType": "author", "name": "Zotero Consortium"}
                ],
                "tags": [], "collections": [], "relations": {},
                "date": "2019",
                "publicationTitle": "J",
                "DOI": "10.1/x",
                "extra": "PMID: 42"
            })
        );
    }

    #[test]
    fn patch_from_record_book_puts_doi_in_extra() {
        let rec = PaperRecord {
            title: "B".to_string(),
            doi: Some("10.1/b".to_string()),
            arxiv_id: Some("2502.00857".to_string()),
            ..PaperRecord::default()
        };
        let body = ZItemPatch::from_record(&rec, "book").to_json();
        assert_eq!(body["extra"], json!("DOI: 10.1/b\narXiv: 2502.00857"));
        assert!(body.get("DOI").is_none());
        assert_eq!(ZItemPatch::suggest_item_type(&rec), "preprint");
    }

    #[test]
    fn record_round_trips_through_patch() {
        let original = ZItem::parse_one(JOURNAL).unwrap().to_record();
        let patch =
            ZItemPatch::from_record(&original, "journalArticle").set("key", json!("ABCD2345"));
        let envelope = json!({ "data": patch.to_json() });
        let back = ZItem::from_value(&envelope).unwrap().to_record();
        assert_eq!(back.title, original.title);
        assert_eq!(back.doi, original.doi);
        assert_eq!(back.year, original.year);
        assert_eq!(back.venue, original.venue);
        assert_eq!(back.pmid, original.pmid);
        assert_eq!(back.pmcid, original.pmcid);
    }

    #[test]
    fn note_and_link_patch_bodies() {
        let note = ZItemPatch::note("ABCD2345", "<p>Hi</p>").to_json();
        assert_eq!(
            note,
            json!({"itemType": "note", "parentItem": "ABCD2345", "note": "<p>Hi</p>",
                   "tags": [], "relations": {}})
        );
        let link =
            ZItemPatch::linked_url_attachment("ABCD2345", "https://example.org/p.pdf", "PDF")
                .to_json();
        assert_eq!(link["linkMode"], json!("linked_url"));
        assert_eq!(link["parentItem"], json!("ABCD2345"));
        assert_eq!(link["url"], json!("https://example.org/p.pdf"));
        assert!(link.get("collections").is_none());
    }
}
