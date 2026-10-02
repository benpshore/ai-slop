//! Shared, versioned record types. This is the contract every module codes
//! against. Bump [`SCHEMA_VERSION`] when a stored shape changes.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Version of the record shapes and of the `SQLite` schema. Bumped to 4 when
/// `ReferenceEntry` gained `anchor`, `doi_link`, `attempts` and `resolved`
/// (stored in the references table's `extra` column). Bumped to 3 when
/// `StageTimings::hash_ms` was added (older rows read it as 0). Bumped to 2 when
/// `Line::role` was added: older ledgers deserialise every line as `body`.
pub const SCHEMA_VERSION: u32 = 5;

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

/// A link annotation on a page: its rectangle and the URI it opens.
/// Publishers embed reference DOIs as `doi.org` links; the string in the
/// annotation is exact, unlike text that wrapped or hyphenated.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Link {
    pub bbox: Option<BBox>,
    pub uri: String,
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
///
/// `Default` is implemented by hand (not derived) so that `Line::default()`
/// has the role `body`, the same value serde uses for a missing field.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Line {
    pub text: String,
    pub bbox: Option<BBox>,
    /// Zero-based column index assigned by the layout pass.
    pub column: u32,
    /// Indices into `PageText::spans` in reading order.
    pub spans: Vec<u32>,
    /// What the line is, as tagged after reading order. One of `body`
    /// (running text, the default), `heading`, `caption`, `figure` (text
    /// inside a figure), `table` (table cells), `algorithm`, `toc` (table of
    /// contents), `front` (page-1 title, authors and affiliations before the
    /// abstract), `math` (display-equation fragments), `footnote` (page-foot
    /// notes), `code` (monospace listings), `biography` (author biographies) or
    /// `furniture` (running heads, page numbers, stamps; these
    /// lines are not in `PageText::text`). Tags never remove text by
    /// themselves.
    #[serde(default = "default_line_role")]
    pub role: String,
}

/// The role of an untagged [`Line`]: `body`.
pub fn default_line_role() -> String {
    "body".to_string()
}

impl Default for Line {
    fn default() -> Self {
        Self {
            text: String::new(),
            bbox: None,
            column: 0,
            spans: Vec::new(),
            role: default_line_role(),
        }
    }
}

/// An image or drawing region on a page. Pixel data never lives in text
/// output: `file` points at the exported bytes when they were saved.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Figure {
    /// 0-based index of the figure on its page, in backend order.
    pub index: u32,
    pub bbox: Option<BBox>,
    /// `raster` (an image), `vector` (a cluster of painted paths), `rule` (a
    /// thin horizontal or vertical painted line, e.g. a table rule) or `layout`
    /// (a model-detected picture region).
    pub kind: String,
    /// MIME type of the exported bytes when known, e.g. `image/png`.
    pub mime: Option<String>,
    pub width_px: Option<u32>,
    pub height_px: Option<u32>,
    /// SHA-256 of the exported bytes when they were captured.
    pub sha256: Option<String>,
    /// Path of the exported file relative to the output directory.
    pub file: Option<String>,
    pub caption: Option<String>,
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
    /// Images and drawings found on the page; never inlined into `text`.
    #[serde(default)]
    pub figures: Vec<Figure>,
    /// Link annotations on the page (`/Annots` of subtype `Link` with a URI).
    #[serde(default)]
    pub links: Vec<Link>,
    /// Filled by `reading_order`; empty until then.
    pub lines: Vec<Line>,
    /// Final ordered text for the page; lines joined by `\n`, paragraphs by `\n\n`.
    pub text: String,
    pub warnings: Vec<String>,
}

impl PageText {
    /// Whether extraction failed, reached a work limit, or has unresolved Unicode mapping.
    /// Ordinary diagnostic warnings do not make a page partial.
    pub fn extraction_status(&self) -> Status {
        if self.warnings.iter().any(|warning| {
            warning.starts_with("failed:")
                || warning.starts_with("resource_limit:")
                || warning.starts_with("unicode_mapping:")
        }) {
            Status::Partial
        } else {
            Status::Complete
        }
    }

    pub fn new(page: u32, width: f32, height: f32, rotation: i32) -> Self {
        Self {
            page,
            width,
            height,
            rotation,
            spans: Vec::new(),
            figures: Vec::new(),
            links: Vec::new(),
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
    /// Left edge, baseline and font size of the entry's first line
    /// (`x0`, `y0`; `y1 - y0` is the size), used to attach link annotations.
    #[serde(default)]
    pub anchor: Option<BBox>,
    /// The DOI of a `doi.org` link annotation placed on this entry: an exact
    /// string from the PDF, independent of the printed text.
    #[serde(default)]
    pub doi_link: Option<String>,
    /// What resolution tried for this entry and what came back, so a reader
    /// can tell a citation that is wrong in the paper from a lookup that
    /// failed. Empty until `--resolve` runs.
    #[serde(default)]
    pub attempts: Vec<Attempt>,
    /// The record this entry resolved to, when `--resolve` ran and the
    /// record was verified against the printed entry.
    #[serde(default)]
    pub resolved: Option<Resolved>,
}

/// One resolution attempt on an entry. `outcome` is `candidate` (metadata
/// agrees but the query winner is not yet known), `ambiguous`, `verified`,
/// `not_found` (the DOI or query returned nothing), `mismatch` (a record
/// came back but disagreed with the printed entry; `detail` says which
/// field and both values) or `error` (the request failed).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Attempt {
    pub method: String,
    pub doi: Option<String>,
    pub outcome: String,
    pub detail: Option<String>,
}

/// A bibliographic record an entry (or the paper) resolved to, verified
/// against what is printed. `method` says how the DOI was obtained:
/// `link` (a `doi.org` link annotation on the entry), `printed` (the DOI
/// text of the entry), `query` (a bibliographic search on the entry text)
/// or `metadata` (the paper's own DOI from its metadata).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Resolved {
    /// None for `PubMed` records without a DOI; never invent a DOI from a PMID.
    pub doi: Option<String>,
    #[serde(default)]
    pub pmid: Option<String>,
    #[serde(default)]
    pub pmcid: Option<String>,
    pub title: Option<String>,
    pub authors: Vec<String>,
    pub year: Option<u16>,
    pub venue: Option<String>,
    /// `crossref`, `europepmc` or `openalex`.
    pub source: String,
    pub method: String,
    /// Agreement between the record and the printed entry, 0 to 1: the
    /// first-author similarity, or the title similarity for entries whose
    /// first author could not be parsed.
    pub score: f32,
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
    /// Time the document hash took on its own thread. It overlaps `parse_ms`
    /// (any wait for it is inside `parse_ms`), so it is not part of the total.
    #[serde(default)]
    pub hash_ms: f64,
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
    /// Directory that receives exported figure bytes as
    /// `<dir>/<document hash>/p<page>-f<index>.<ext>`; `None` exports nothing
    /// (figure hashes are still recorded when the backend supplies bytes).
    #[serde(default)]
    pub figures_dir: Option<String>,
}

/// Lower-case hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    hex::encode(digest.as_slice())
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

    #[test]
    fn sha256_hex_matches_known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // 1000 bytes: several 64-byte compression blocks plus a partial one.
        let long: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        assert_eq!(
            sha256_hex(&long),
            "4e4c294b331f7a2099a379bec34b9f9fc03dc46ab465d998f4d683da53487e6d"
        );
    }

    #[test]
    fn timings_without_hash_ms_parse() {
        let json = r#"{"acquire_ms":1.0,"parse_ms":2.0,"order_ms":3.0,"metadata_ms":4.0,"citations_ms":5.0,"write_ms":6.0}"#;
        let timings: StageTimings = serde_json::from_str(json).unwrap();
        assert!((timings.write_ms - 6.0).abs() < f64::EPSILON);
        assert!(timings.hash_ms.abs() < f64::EPSILON);
    }

    #[test]
    fn job_without_figures_dir_parses() {
        let json =
            r#"{"path":"a.pdf","backend":"lopdf","pages":null,"password":null,"max_bytes":null}"#;
        let job: Job = serde_json::from_str(json).unwrap();
        assert_eq!(job.figures_dir, None);
        assert_eq!(job.backend, "lopdf");
    }

    #[test]
    fn line_without_role_is_body() {
        let json = r#"{"text":"a","bbox":null,"column":0,"spans":[0]}"#;
        let line: Line = serde_json::from_str(json).unwrap();
        assert_eq!(line.role, "body");
        assert_eq!(Line::default().role, "body");
        let tagged = Line {
            role: "caption".to_string(),
            ..Line::default()
        };
        let back: Line = serde_json::from_str(&serde_json::to_string(&tagged).unwrap()).unwrap();
        assert_eq!(back, tagged);
    }
}
