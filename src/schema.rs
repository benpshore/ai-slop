//! Shared, versioned record types. This is the contract every module codes
//! against. Bump [`SCHEMA_VERSION`] when a stored shape changes.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Version of the record shapes and of the `SQLite` schema.
pub const SCHEMA_VERSION: u32 = 1;

/// Pages per scheduling chunk (the "20-page chunk" of the product target).
pub const CHUNK_PAGES: u32 = 20;

/// Lower-case hex SHA-256 of the complete input bytes. Durable document identity.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ContentHash(pub String);

/// Identity of the code that produced a result. Results are keyed by input
/// hash *and* this identity, so upgrades produce new attributable rows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendIdentity {
    /// e.g. `lopdf`, `pdfium`, `poppler`, `docling`.
    pub name: String,
    /// Library or crate version actually linked.
    pub version: String,
    /// Hex digest of the effective configuration (sorted key=value lines).
    pub config_digest: String,
}

/// Axis-aligned box in PDF user space (points, origin bottom-left, unrotated).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct BBox {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

/// One positioned run of text as the backend produced it (evidence, not output).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Span {
    /// Text after `ToUnicode`/encoding mapping, NFC-normalised, no repair.
    pub text: String,
    pub bbox: Option<BBox>,
    pub font: Option<String>,
    /// Font size in points after the text matrix is applied.
    pub size: Option<f32>,
    /// Backend-native ordering index (content stream order).
    pub seq: u32,
}

/// A line assembled by `reading_order` from spans on one page.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Line {
    pub text: String,
    pub bbox: Option<BBox>,
    /// Zero-based column index assigned by the layout pass.
    pub column: u32,
    /// Indices into `PageText::spans` in reading order.
    pub spans: Vec<u32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PageText {
    /// 1-based page number as printed by the backend (PDF page index + 1).
    pub page: u32,
    pub width: f32,
    pub height: f32,
    /// `/Rotate` in degrees, 0/90/180/270.
    pub rotation: i32,
    pub spans: Vec<Span>,
    /// Filled by `reading_order`; empty until then.
    pub lines: Vec<Line>,
    /// Final ordered text for the page; lines joined by `\n`, paragraphs by `\n\n`.
    pub text: String,
    pub warnings: Vec<String>,
}

impl PageText {
    pub fn new(page: u32, width: f32, height: f32, rotation: i32) -> Self {
        Self {
            page,
            width,
            height,
            rotation,
            spans: Vec::new(),
            lines: Vec::new(),
            text: String::new(),
            warnings: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Author {
    pub name: String,
    pub affiliation: Option<String>,
    pub orcid: Option<String>,
    pub email: Option<String>,
}

/// Paper-level metadata. Every field is `None`/empty unless found; never guessed.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Metadata {
    pub title: Option<String>,
    pub authors: Vec<Author>,
    pub doi: Option<String>,
    pub arxiv_id: Option<String>,
    pub year: Option<u16>,
    pub venue: Option<String>,
    pub abstract_text: Option<String>,
    pub keywords: Vec<String>,
    /// Raw PDF `/Info` dictionary, string-valued entries only, keys without `/`.
    pub info: BTreeMap<String, String>,
    /// Which source each field came from, e.g. `title` -> `info:Title` or `page1:largest-font`.
    pub provenance: BTreeMap<String, String>,
}

/// One entry of the reference list. `raw` is authoritative; parsed fields are best-effort.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ReferenceEntry {
    /// 1-based position in the reference list.
    pub index: u32,
    /// Label as printed, e.g. `[12]`, `12.`, or `Smith2020` for author-year styles.
    pub label: Option<String>,
    pub raw: String,
    pub authors: Vec<String>,
    pub title: Option<String>,
    pub year: Option<u16>,
    pub venue: Option<String>,
    pub volume: Option<String>,
    pub issue: Option<String>,
    pub pages: Option<String>,
    pub doi: Option<String>,
    pub arxiv_id: Option<String>,
    pub url: Option<String>,
    /// Page on which the entry starts.
    pub page: u32,
}

/// An in-text citation marker such as `[3, 7]` or `(Smith et al., 2020)`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CitationMarker {
    pub page: u32,
    /// Char offset of the marker within `PageText::text`.
    pub offset: u32,
    pub text: String,
    /// Resolved `ReferenceEntry::index` values; empty if unresolved.
    pub targets: Vec<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Complete,
    Partial,
    Failed,
    Deferred,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Partial => "partial",
            Self::Failed => "failed",
            Self::Deferred => "deferred",
        }
    }
}

/// Wall-clock milliseconds per stage for one document.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StageTimings {
    pub acquire_ms: f64,
    pub parse_ms: f64,
    pub order_ms: f64,
    pub metadata_ms: f64,
    pub citations_ms: f64,
    pub write_ms: f64,
}

/// A 20-page scheduling chunk summary; the unit of the 30 ms target.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChunkResult {
    pub chunk_index: u32,
    pub first_page: u32,
    pub last_page: u32,
    pub status: Status,
    /// SHA-256 of the concatenated page texts in this chunk.
    pub text_sha256: String,
    pub ms: f64,
}

/// Where the bytes came from. Path/inode are observations; the hash is identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceObservation {
    pub path: String,
    pub inode: Option<u64>,
    pub device: Option<u64>,
    pub mtime_unix: Option<i64>,
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Document {
    pub hash: ContentHash,
    pub size: u64,
    pub pages: u32,
    pub sources: Vec<SourceObservation>,
}

/// Everything produced for one document by one backend identity.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExtractionResult {
    pub schema_version: u32,
    pub document: Document,
    pub backend: BackendIdentity,
    pub status: Status,
    pub pages: Vec<PageText>,
    pub chunks: Vec<ChunkResult>,
    pub metadata: Metadata,
    pub references: Vec<ReferenceEntry>,
    pub citations: Vec<CitationMarker>,
    pub warnings: Vec<String>,
    pub timings: StageTimings,
}

/// Job description handed to a worker.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    pub path: String,
    /// Backend name; see `backend::by_name`.
    pub backend: String,
    /// Inclusive 1-based page range; `None` means all pages.
    pub pages: Option<(u32, u32)>,
    pub password: Option<String>,
    pub max_bytes: Option<u64>,
}

/// Lower-case hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

/// Digest of a configuration map, stable across key order.
pub fn config_digest(config: &BTreeMap<String, String>) -> String {
    let mut buf = String::new();
    for (k, v) in config {
        buf.push_str(k);
        buf.push('=');
        buf.push_str(v);
        buf.push('\n');
    }
    sha256_hex(buf.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_is_order_independent() {
        let mut a = BTreeMap::new();
        a.insert("b".to_string(), "2".to_string());
        a.insert("a".to_string(), "1".to_string());
        let mut b = BTreeMap::new();
        b.insert("a".to_string(), "1".to_string());
        b.insert("b".to_string(), "2".to_string());
        assert_eq!(config_digest(&a), config_digest(&b));
    }

    #[test]
    fn result_round_trips_json() {
        let r = ExtractionResult {
            schema_version: SCHEMA_VERSION,
            document: Document {
                hash: ContentHash(sha256_hex(b"x")),
                size: 1,
                pages: 0,
                sources: vec![],
            },
            backend: BackendIdentity {
                name: "test".into(),
                version: "0".into(),
                config_digest: config_digest(&BTreeMap::new()),
            },
            status: Status::Complete,
            pages: vec![],
            chunks: vec![],
            metadata: Metadata::default(),
            references: vec![],
            citations: vec![],
            warnings: vec![],
            timings: StageTimings::default(),
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: ExtractionResult = serde_json::from_str(&s).unwrap();
        assert_eq!(back, r);
    }
}
