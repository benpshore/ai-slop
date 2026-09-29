//! Format-aware extraction. Native document structure is retained instead of
//! forcing sheets, HTML, or Word paragraphs through PDF layout heuristics.

#[cfg(feature = "formats")]
mod office;

use std::collections::BTreeMap;
use std::path::Path;
#[cfg(feature = "formats")]
use std::sync::OnceLock;
use std::time::Instant;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::acquire;
use crate::backend::{Extractor, lopdf_backend::LopdfBackend};
use crate::schema::{BackendIdentity, PageText, SourceObservation, config_digest};

/// Deliberately narrow allowlist: recognizing an input is not a promise that
/// every format an upstream dependency accepts has been validated here.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    Pdf,
    Text,
    Html,
    Markdown,
    Csv,
    Docx,
    Xlsx,
    Image,
    Audio,
    #[default]
    Unknown,
}

/// `Extracted` reports successful execution, never certified transcription.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Extracted,
    ReviewRequired,
    NeedsOcr,
    NeedsTranscription,
    Unsupported,
    Failed,
}

/// Bounds apply per input, not per collection. Compressed/parsed data may be
/// larger than its input; these are not an operating-system memory limit.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Options {
    pub max_bytes: u64,
    pub max_expanded_bytes: u64,
    pub max_archive_entries: usize,
    pub max_cells: usize,
    pub max_pages: u32,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            max_bytes: 64 * 1024 * 1024,
            max_expanded_bytes: 256 * 1024 * 1024,
            max_archive_entries: 10_000,
            max_cells: 250_000,
            max_pages: 10_000,
        }
    }
}

/// A versioned, independently addressable record suitable for JSONL streams.
/// `content` carries the named extractor's structure (PDF pages, Docling JSON,
/// sparse XLSX cells, CSV rows with source text, or exact UTF-8 text). Retain
/// the original by its SHA-256.
#[derive(Debug, Deserialize, Serialize)]
pub struct Record {
    pub schema_version: String,
    pub path: String,
    pub source: Option<SourceObservation>,
    pub sha256: Option<String>,
    pub format: Format,
    pub outcome: Outcome,
    pub extractor: Option<BackendIdentity>,
    /// Public, effective policy values; never a dump of the process environment.
    #[serde(default)]
    pub policy: BTreeMap<String, String>,
    pub policy_digest: String,
    pub content: Option<Value>,
    pub warnings: Vec<String>,
    pub elapsed_ms: f64,
}

impl Record {
    /// Used for read failures and by supervisors when a worker cannot return
    /// a result. Missing source identity stays null, never a guessed hash.
    pub fn failure(path: &Path, options: &Options, message: String) -> Self {
        let mut policy = BTreeMap::from([
            ("ingest_revision".to_owned(), "2".to_owned()),
            (
                "formats_enabled".to_owned(),
                cfg!(feature = "formats").to_string(),
            ),
            (
                "limits".to_owned(),
                serde_json::to_string(options).expect("serializable limits"),
            ),
        ]);
        #[cfg(feature = "formats")]
        policy.extend(parser_limits().clone());
        // Keep the map mutable in both feature configurations.
        policy.insert("csv_dialect".into(), "comma-double-quote-no-header".into());
        Self {
            schema_version: "tpe.ingest.v1".into(),
            path: path.to_string_lossy().into_owned(),
            source: None,
            sha256: None,
            format: Format::Unknown,
            outcome: Outcome::Failed,
            extractor: None,
            policy_digest: config_digest(&policy),
            policy,
            content: None,
            warnings: vec![message],
            elapsed_ms: 0.0,
        }
    }
}

#[cfg(feature = "formats")]
fn environment_limits() -> BTreeMap<String, String> {
    fn numeric<T: std::str::FromStr>(key: &str) -> Option<T> {
        std::env::var(key).ok()?.trim().parse().ok()
    }
    // Match the pinned upstream's parsing, defaults and zero handling. Only
    // numeric, allowlisted settings enter records; invalid strings stay private.
    BTreeMap::from([
        (
            "DOCLING_RS_MAX_XML_DEPTH".into(),
            numeric::<usize>("DOCLING_RS_MAX_XML_DEPTH")
                .filter(|n| *n > 0)
                .unwrap_or(512)
                .to_string(),
        ),
        (
            "DOCLING_RS_MAX_HTML_DEPTH".into(),
            numeric::<usize>("DOCLING_RS_MAX_HTML_DEPTH")
                .unwrap_or(2000)
                .to_string(),
        ),
        (
            "DOCLING_RS_MAX_PART_BYTES".into(),
            numeric::<u64>("DOCLING_RS_MAX_PART_BYTES")
                .unwrap_or(512 * 1024 * 1024)
                .to_string(),
        ),
    ])
}

#[cfg(feature = "formats")]
fn parser_limits() -> &'static BTreeMap<String, String> {
    // Upstream caches its XML limit too. Applications must set these variables
    // before using either parser and leave them fixed for the process lifetime.
    static LIMITS: OnceLock<BTreeMap<String, String>> = OnceLock::new();
    LIMITS.get_or_init(environment_limits)
}

/// Read and hash once, then route the immutable bytes. Each failure produces
/// a record. Panics are contained here; process aborts need a supervisor.
pub fn run(path: &Path, options: &Options) -> Record {
    let start = Instant::now();
    let mut record = Record::failure(path, options, String::new());
    record.warnings.clear();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
        let snapshot = acquire::snapshot(path, Some(options.max_bytes))?;
        record.sha256 = Some(snapshot.hash.0);
        record.source = Some(snapshot.source);
        record.format = detect(path, &snapshot.bytes, options)?;
        extract(snapshot.bytes, options, &mut record)
    }));
    match result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            record.outcome = Outcome::Failed;
            record.content = None;
            record.warnings.push(error.to_string());
        }
        Err(_) => {
            record.outcome = Outcome::Failed;
            record.content = None;
            record.warnings.push("extractor panicked".into());
        }
    }
    record.elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
    record
}

fn detect(path: &Path, bytes: &[u8], options: &Options) -> Result<Format> {
    #[cfg(not(feature = "formats"))]
    let _ = options;
    // A complete header line may follow a short whitespace/binary prefix.
    // A mention of "%PDF-1.7" in prose is not a format signature.
    if has_pdf_header(bytes) {
        return Ok(Format::Pdf);
    }
    if bytes.starts_with(b"PK\x03\x04") {
        #[cfg(feature = "formats")]
        return office::inspect_archive(bytes, options);
        #[cfg(not(feature = "formats"))]
        return Ok(Format::Unknown);
    }
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        || bytes.starts_with(b"\xff\xd8\xff")
        || bytes.starts_with(b"II*\0")
        || bytes.starts_with(b"MM\0*")
        || bytes.starts_with(b"GIF87a")
        || bytes.starts_with(b"GIF89a")
        || (bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP"))
    {
        return Ok(Format::Image);
    }
    if bytes.starts_with(b"fLaC")
        || bytes.starts_with(b"ID3")
        || (bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WAVE"))
    {
        return Ok(Format::Audio);
    }
    let extension = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    Ok(match extension.as_str() {
        "txt" => Format::Text,
        "html" | "htm" => Format::Html,
        "md" | "markdown" => Format::Markdown,
        "csv" => Format::Csv,
        "pdf" => bail!("PDF extension without a PDF header"),
        "docx" | "xlsx" => bail!("Office extension without an OOXML ZIP container"),
        // Recognition by extension here is only a routing hint. The OCR/ASR
        // adapter must validate the bytes when it actually decodes them.
        "png" | "jpg" | "jpeg" | "tif" | "tiff" | "bmp" | "webp" => Format::Image,
        "wav" | "mp3" | "m4a" | "aac" | "ogg" | "flac" => Format::Audio,
        _ => Format::Unknown,
    })
}

fn has_pdf_header(bytes: &[u8]) -> bool {
    let prefix = &bytes[..bytes.len().min(1024)];
    prefix.windows(8).enumerate().any(|(offset, candidate)| {
        candidate.starts_with(b"%PDF-")
            && matches!(candidate[5], b'1' | b'2')
            && candidate[6] == b'.'
            && candidate[7].is_ascii_digit()
            && matches!(bytes.get(offset + 8), Some(b'\r' | b'\n'))
            && prefix[..offset]
                .iter()
                .all(|b| b.is_ascii_whitespace() || !b.is_ascii_graphic())
    })
}

fn identity(name: &str, version: &str, policy_digest: &str) -> BackendIdentity {
    BackendIdentity {
        name: name.into(),
        version: version.into(),
        config_digest: policy_digest.into(),
    }
}

fn extract(bytes: Vec<u8>, options: &Options, record: &mut Record) -> Result<()> {
    #[cfg(feature = "formats")]
    if environment_limits() != *parser_limits() {
        bail!(
            "Docling parser limits changed during this process; restart with fixed environment settings"
        );
    }
    record.outcome = Outcome::Extracted;
    match record.format {
        Format::Pdf => extract_pdf(&bytes, options, record)?,
        Format::Text => {
            let text = String::from_utf8(bytes)?;
            if text.contains('\0') {
                bail!("NUL in text input; refusing to treat binary data as text");
            }
            record.extractor = Some(identity("utf8", "1", &record.policy_digest));
            record.content = Some(json!({"text": text}));
        }
        Format::Docx | Format::Xlsx | Format::Html | Format::Markdown | Format::Csv => {
            #[cfg(feature = "formats")]
            office::extract(bytes, options, record)?;
            #[cfg(not(feature = "formats"))]
            {
                record.outcome = Outcome::Unsupported;
                record.warnings.push(
                    "build with --features formats to enable native Office/HTML conversion".into(),
                );
            }
        }
        Format::Image => {
            record.outcome = Outcome::NeedsOcr;
            record
                .warnings
                .push("image requires an OCR adapter; no text has been extracted".into());
        }
        Format::Audio => {
            record.outcome = Outcome::NeedsTranscription;
            record.warnings.push(
                "audio requires a transcription adapter; no transcript has been produced".into(),
            );
        }
        Format::Unknown => {
            record.outcome = Outcome::Unsupported;
            record
                .warnings
                .push("unrecognized format (Office containers require --features formats)".into());
        }
    }
    Ok(())
}

fn extract_pdf(bytes: &[u8], options: &Options, record: &mut Record) -> Result<()> {
    let extractor = LopdfBackend::default();
    record.extractor = Some(extractor.identity());
    let mut session = extractor.open(bytes, None)?;
    if session.page_count() == 0 {
        bail!("PDF contains no pages");
    }
    if session.page_count() > options.max_pages {
        bail!(
            "PDF has {} pages, over the {} page limit",
            session.page_count(),
            options.max_pages
        );
    }
    let mut pages = Vec::new();
    let mut ocr_candidates = Vec::new();
    let mut ocr_evidence = Vec::new();
    for number in 1..=session.page_count() {
        let mut page = session.page_text(number)?;
        crate::reading_order::order_page(&mut page);
        // Keep source spans and ordered lines. Bibliography-specific and
        // scholarly header/dehyphenation rules must not erase general content.
        if page.text.trim().is_empty() {
            ocr_candidates.push(number);
            ocr_evidence.push(json!({"page": number, "reason": "no_text"}));
        } else if let Some(coverage) = sparse_raster_page(&page) {
            ocr_candidates.push(number);
            ocr_evidence.push(json!({"page": number, "reason": "sparse_text_dominant_raster", "dominant_raster_fraction": coverage}));
        }
        if page
            .warnings
            .iter()
            .any(|warning| !warning.starts_with("ligatures expanded: "))
        {
            record.outcome = Outcome::ReviewRequired;
        }
        if page.text.contains('\u{fffd}') {
            record.outcome = Outcome::ReviewRequired;
            record
                .warnings
                .push(format!("page {number}: unresolved replacement character"));
        }
        record.warnings.extend(page.warnings.iter().cloned());
        pages.push(page);
    }
    if !ocr_candidates.is_empty() {
        record.outcome = Outcome::NeedsOcr;
        record.warnings.push(
            "OCR candidates include empty-text pages and sparse text over a dominant raster; blank pages and captioned photographs can be false positives".into(),
        );
    }
    record.content = Some(
        json!({"pages": pages, "info": session.info(), "ocr_candidates": ocr_candidates, "ocr_evidence": ocr_evidence}),
    );
    Ok(())
}

/// A conservative routing heuristic, not an assertion that a raster is text.
/// Tiled scans and incomplete OCR layers with abundant text can still escape it.
fn sparse_raster_page(page: &PageText) -> Option<f64> {
    if page
        .text
        .chars()
        .filter(|c| !c.is_whitespace())
        .take(81)
        .count()
        > 80
    {
        return None;
    }
    let width = f64::from(page.width);
    let height = f64::from(page.height);
    if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
        return None;
    }
    page.figures
        .iter()
        .filter(|figure| figure.kind == "raster")
        .filter_map(|figure| {
            let bbox = figure.bbox?;
            let [x0, y0, x1, y1] = [bbox.x0, bbox.y0, bbox.x1, bbox.y1].map(f64::from);
            if ![x0, y0, x1, y1].iter().all(|n| n.is_finite()) {
                return None;
            }
            let area =
                (x1.min(width) - x0.max(0.0)).max(0.0) * (y1.min(height) - y0.max(0.0)).max(0.0);
            Some(area / (width * height))
        })
        .filter(|coverage| *coverage >= 0.65)
        .max_by(f64::total_cmp)
}
