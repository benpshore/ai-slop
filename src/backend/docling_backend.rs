//! Backend built on `docling-pdf` / `docling-core` 1.69.2 (the `docling.rs`
//! port of docling). This is the legacy model-backed/library adapter. The CLI
//! `docling-text` backend now uses `docling_text_backend`, a separate retained
//! page parser without ML dependencies. Two library modes remain here:
//!
//! * **text layer** (`docling-text`): `docling_pdf::convert_text_layer_pages`,
//!   a pure-Rust content-stream parser plus docling's line/paragraph
//!   assembly. Needs no models and no pdfium.
//! * **full** (`docling`): `docling_pdf::Pipeline` — pdfium text cells and
//!   page renders, ONNX layout detection, optional OCR and `TableFormer`.
//!   Needs the pdfium shared library at an absolute
//!   `PDFIUM_DYNAMIC_LIB_PATH` and the ONNX models (`.models/…` relative to the
//!   working directory, `$DOCLING_RS_MODELS_DIR`, or next to the executable;
//!   see docling-core `assets.rs`). Nothing is downloaded here.
//!
//! docling already decides the reading order, so [`Extractor::provides_reading_order`]
//! is `true` and spans are emitted in docling's node order with a running
//! `seq` per page. The adapter also supplies final lines: a narrowly qualified
//! split reference opening at the bottom of two columns is moved beside its
//! continuation, while the upstream spans and sequence remain unchanged.
//!
//! # Limitations (all by construction of the docling API)
//!
//! * docling converts the **whole document at once**. The first
//!   [`DocumentSession::page_text`] call runs the conversion and caches the
//!   result; later pages are served from the cache. A document-level
//!   conversion failure therefore surfaces as
//!   [`BackendError::Unsupported`] from that first call (the trait has no
//!   other channel), not from `open`.
//! * Pixel data never enters a span: a `Picture` becomes a
//!   [`Figure`] (`kind: "layout"`) and its bytes are held for
//!   [`DocumentSession::take_figure_bytes`].
//! * Spans carry no font name or size (docling does not expose them). Grid
//!   boxes are docling's 0–511 `<location>` grid denormalised exactly as
//!   docling-core `json.rs` (`prov_json`) does, so they are 2-decimal
//!   approximations of the region box, not glyph boxes.
//! * docling sanitises text (curly quotes to `'`, dashes to `-`, wrapped-word
//!   hyphens removed, paragraphs reflowed) and merges a paragraph that
//!   continues across a page break into the page where it started. The
//!   Markdown escaping docling applies (`\_`, `&amp;`, `&lt;`, `&gt;`), its
//!   `<!-- image -->` / `<!-- formula-not-decoded -->` placeholders and the
//!   `[text](uri)` wrapper it puts on linked footnotes are removed here, so
//!   no docling markup reaches a span.
//! * The text-layer path cannot read encrypted files even with the right
//!   password (docling opens the bytes without one), so `docling-text`
//!   reports such files as `Unsupported`; the full mode passes the password
//!   to pdfium.
//! * `full: true, ocr: false` makes docling skip layout, OCR and tables
//!   entirely (its `no_ocr` switch), which is the text layer through pdfium.
//! * Pipelines are process-wide and keyed by `(ocr, tables, force_ocr)`;
//!   every distinct configuration used in one process loads its own models.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};

use docling_core::{DoclingDocument, Node, PictureImage, Table, TableCell};
use docling_pdf::{PdfError, Pipeline};
use lopdf::{Dictionary, Document, Error as LopdfError, LoadOptions, Object};
use unicode_normalization::UnicodeNormalization;

use crate::backend::{BackendError, DocumentSession, EncryptionProblem, Extractor};
use crate::schema::{BBox, BackendIdentity, Figure, PageText, Span, config_digest, sha256_hex};

/// The `docling-pdf` release this backend is built against. Part of the
/// [`BackendIdentity`]; a unit test checks it against `Cargo.lock`.
const DOCLING_VERSION: &str = "1.69.2";
/// Resolution of docling's `<location>` grid (`docling-core/src/json.rs`,
/// `prov_json`: `x * width / 512.0`).
const GRID: f64 = 512.0;
/// Paragraph docling emits for a formula region it did not decode.
const FORMULA_PLACEHOLDER: &str = "<!-- formula-not-decoded -->";
/// Placeholder docling writes into a table cell that holds a picture.
const IMAGE_PLACEHOLDER: &str = "<!-- image -->";
/// Warning for a page docling produced nothing for.
const NO_ITEMS_WARNING: &str = "extraction_incomplete: docling produced no items on page";
/// Logical document name handed to docling (it only labels the output).
const DOC_NAME: &str = "doc";
/// Bound on the `/Parent` walk used for inherited page attributes.
const MAX_PARENT_DEPTH: u32 = 64;

/// The docling extractor. `full == false` is the text-layer mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // the four switches are the public configuration surface
pub struct DoclingBackend {
    /// Run the ONNX pipeline (`docling`) instead of the text layer (`docling-text`).
    pub full: bool,
    /// Full mode: run OCR (and layout/tables); `false` skips the ML stack.
    pub ocr: bool,
    /// Full mode: run `TableFormer` for table structure.
    pub tables: bool,
    /// Full mode: OCR every page even when it has a text layer.
    pub force_ocr: bool,
    /// Keep the bytes of the pictures docling crops, for `take_figure_bytes`.
    /// Off by default: a job without a figures directory never holds pixels.
    pub keep_figures: bool,
    /// Full mode: convert only pages `first..=last` (1-based, inclusive);
    /// pages outside come back empty with the no-items warning.
    pub window: Option<(u32, u32)>,
}

impl DoclingBackend {
    /// Text-layer mode: no models, no pdfium.
    pub fn text_layer() -> Self {
        Self {
            full: false,
            ocr: false,
            tables: false,
            force_ocr: false,
            keep_figures: false,
            window: None,
        }
    }

    /// Full pipeline with OCR, without `TableFormer` (its models are large).
    pub fn full() -> Self {
        Self {
            full: true,
            ocr: true,
            tables: false,
            force_ocr: false,
            keep_figures: false,
            window: None,
        }
    }

    /// Convert only pages `first..=last` (full mode).
    #[must_use]
    pub fn with_window(mut self, first: u32, last: u32) -> Self {
        self.window = Some((first.max(1), last.max(first.max(1))));
        self
    }

    /// Keep (or drop) the picture bytes docling produces.
    #[must_use]
    pub fn with_figures(mut self, keep: bool) -> Self {
        self.keep_figures = keep;
        self
    }

    /// CLI name: `docling` in full mode, `docling-text` otherwise.
    pub fn name(self) -> &'static str {
        if self.full { "docling" } else { "docling-text" }
    }

    fn pipeline_key(self) -> PipelineKey {
        PipelineKey {
            ocr: self.ocr,
            tables: self.tables,
            force_ocr: self.force_ocr,
        }
    }
}

impl Default for DoclingBackend {
    fn default() -> Self {
        Self::text_layer()
    }
}

impl Extractor for DoclingBackend {
    /// Name [`DoclingBackend::name`], version [`DOCLING_VERSION`], digest over
    /// `full`, `ocr`, `tables`, `force_ocr` and `provider` (`cpu`).
    fn identity(&self) -> BackendIdentity {
        let mut config = BTreeMap::new();
        config.insert("full".to_string(), self.full.to_string());
        config.insert("ocr".to_string(), self.ocr.to_string());
        config.insert("tables".to_string(), self.tables.to_string());
        config.insert("force_ocr".to_string(), self.force_ocr.to_string());
        config.insert("provider".to_string(), "cpu".to_string());
        config.insert("ocr_engine".to_string(), "ppocr".to_string());
        config.insert("evidence_policy".to_string(), "2".to_string());
        if self.full {
            config.insert(
                "line_layout".to_string(),
                "split-reference-start-v1".to_string(),
            );
        }
        if let Some((first, last)) = self.window {
            config.insert("window".to_string(), format!("{first}-{last}"));
        }
        BackendIdentity {
            name: self.name().to_string(),
            version: DOCLING_VERSION.to_string(),
            config_digest: config_digest(&config),
        }
    }

    /// Count pages and read `/Info` and page geometry; nothing is converted
    /// yet. Text mode parses with `lopdf`; full mode asks pdfium
    /// (`docling_pdf::page_count`) and uses `lopdf` best-effort.
    fn open(
        &self,
        bytes: &[u8],
        password: Option<&str>,
    ) -> Result<Box<dyn DocumentSession>, BackendError> {
        let (page_count, geometry, info) = if self.full {
            require_trusted_pdfium_path()?;
            let count = docling_pdf::page_count(bytes, password)
                .map_err(|err| open_error(&err, password.is_some()))?;
            let count = u32::try_from(count).unwrap_or(u32::MAX);
            match load_lopdf(bytes, password) {
                Ok(doc) => (count, page_geometry(&doc), info_entries(&doc)),
                Err(_) => (count, Vec::new(), BTreeMap::new()),
            }
        } else {
            let doc = load_lopdf(bytes, password)?;
            if password.is_some() && encrypted_without_password(bytes) {
                return Err(BackendError::Unsupported(
                    "docling-text cannot read encrypted PDFs; use the lopdf or docling backend"
                        .to_string(),
                ));
            }
            let count = u32::try_from(doc.get_pages().len()).unwrap_or(u32::MAX);
            (count, page_geometry(&doc), info_entries(&doc))
        };
        if page_count == 0 {
            return Err(BackendError::Malformed(
                "docling document has no pages".to_string(),
            ));
        }
        let native_evidence = super::lopdf_backend::LopdfBackend::default()
            .open(bytes, password)
            .map_err(|error| error.to_string());
        Ok(Box::new(DoclingSession {
            bytes: bytes.to_vec(),
            password: password.map(String::from),
            config: *self,
            page_count,
            geometry,
            info,
            converted: None,
            native_evidence,
            figure_bytes: HashMap::new(),
        }))
    }

    /// Raw spans preserve upstream node order; the line projection can repair
    /// a geometrically and semantically qualified split reference opening.
    fn provides_reading_order(&self) -> bool {
        true
    }

    fn provides_line_layout(&self) -> bool {
        self.full
    }
}

/// `docling-pdf` otherwise falls back to `.pdfium/lib` below the current
/// directory and then to the loader search path. Require its first-choice
/// environment path to be explicit, absolute and present before entering
/// native code, so a missing library never reaches that fallback. A library
/// that is present there but fails to load is not detected here.
fn require_trusted_pdfium_path() -> Result<(), BackendError> {
    let configured = std::env::var("PDFIUM_DYNAMIC_LIB_PATH").unwrap_or_default();
    trusted_pdfium_library(&configured).map(|_| ())
}

/// The library file a trusted `PDFIUM_DYNAMIC_LIB_PATH` value names.
fn trusted_pdfium_library(configured: &str) -> Result<PathBuf, BackendError> {
    const HINT: &str = "set PDFIUM_DYNAMIC_LIB_PATH to the absolute path of a provisioned library";
    if !is_trusted_pdfium_path(configured) {
        return Err(BackendError::Unsupported(format!(
            "pdfium not installed at a trusted location: {HINT}"
        )));
    }
    let file = crate::backend::pdfium_backend::library_file(Path::new(configured));
    if !file.is_file() {
        return Err(BackendError::Unsupported(format!(
            "pdfium not installed at {}: {HINT}",
            file.display()
        )));
    }
    Ok(file)
}

fn is_trusted_pdfium_path(path: &str) -> bool {
    !path.is_empty() && Path::new(path).is_absolute()
}

/// Load-time configuration of one shared [`Pipeline`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PipelineKey {
    ocr: bool,
    tables: bool,
    force_ocr: bool,
}

/// Process-wide pipelines, one per configuration; models load once per key.
static PIPELINES: OnceLock<Mutex<Vec<(PipelineKey, Pipeline)>>> = OnceLock::new();

/// Convert through the shared pipeline for `key`, creating it on first use
/// (`Pipeline::new().no_ocr(!ocr).no_table_former(!tables).force_full_page_ocr(force_ocr)`).
fn convert_full(
    bytes: &[u8],
    password: Option<&str>,
    key: PipelineKey,
    window: Option<(u32, u32)>,
) -> Result<DoclingDocument, PdfError> {
    let registry = PIPELINES.get_or_init(|| Mutex::new(Vec::new()));
    let mut pipelines = registry.lock().unwrap_or_else(PoisonError::into_inner);
    let index = if let Some(index) = pipelines.iter().position(|(existing, _)| *existing == key) {
        index
    } else {
        let pipeline = Pipeline::new()?
            .ocr_engine(Some(docling_pdf::OcrEngine::PpOcr))
            .no_ocr(!key.ocr)
            .no_table_former(!key.tables)
            .force_full_page_ocr(key.force_ocr);
        pipelines.push((key, pipeline));
        pipelines.len() - 1
    };
    let Some((_, pipeline)) = pipelines.get_mut(index) else {
        return Err(PdfError::Layout(
            "docling pipeline registry lost its entry".to_string(),
        ));
    };
    pipeline.set_pages(window.map(|(first, last)| (first as usize, last as usize)));
    pipeline.convert(bytes, password, DOC_NAME)
}

/// Map a docling error raised while opening (pdfium page count).
fn open_error(err: &PdfError, has_password: bool) -> BackendError {
    match err {
        PdfError::Pdfium(message) => {
            let lower = message.to_ascii_lowercase();
            if lower.contains("not installed") {
                BackendError::Unsupported(format!("docling: {message}"))
            } else if lower.contains("password") {
                BackendError::Encrypted(if has_password {
                    EncryptionProblem::WrongPassword
                } else {
                    EncryptionProblem::PasswordRequired
                })
            } else {
                BackendError::Malformed(format!("docling: {message}"))
            }
        }
        PdfError::Layout(message) | PdfError::Ocr(message) => {
            BackendError::Unsupported(format!("docling: {message}"))
        }
    }
}

/// Message for a whole-document conversion failure. The document already
/// opened, so the failure is in docling's pipeline or its provisioning and
/// is reported as [`BackendError::Unsupported`] by `page_text`.
fn conversion_failure(err: &PdfError) -> String {
    format!("docling conversion failed: {err}")
}

/// Page size and rotation read with `lopdf`, used when docling reports no
/// page marker and for `/Rotate` (docling does not expose it).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct PageGeometry {
    width: f32,
    height: f32,
    rotation: i32,
    links_match_frame: bool,
}

/// One page of the cached conversion.
#[derive(Clone, Debug)]
struct ConvertedPage {
    width: f32,
    height: f32,
    spans: Vec<Span>,
    figures: Vec<Figure>,
    warnings: Vec<String>,
    list_items: Vec<usize>,
}

/// Pages 1..=count of the conversion; `None` where docling emitted nothing.
type Converted = Vec<Option<ConvertedPage>>;

struct DoclingSession {
    bytes: Vec<u8>,
    password: Option<String>,
    config: DoclingBackend,
    page_count: u32,
    /// Per page (index `page - 1`) from `lopdf`; may be empty in full mode.
    geometry: Vec<PageGeometry>,
    info: BTreeMap<String, String>,
    /// `None` until the first `page_text`; then the conversion or its failure message.
    converted: Option<Result<Converted, String>>,
    figure_bytes: HashMap<(u32, u32), Vec<u8>>,
    native_evidence: Result<Box<dyn DocumentSession>, String>,
}

impl DoclingSession {
    /// Run docling once over the whole document and cache the outcome.
    fn ensure_converted(&mut self) {
        if self.converted.is_some() {
            return;
        }
        let password = self.password.as_deref();
        let result = if self.config.full {
            convert_full(
                &self.bytes,
                password,
                self.config.pipeline_key(),
                self.config.window,
            )
        } else {
            docling_pdf::convert_text_layer_pages(&self.bytes, DOC_NAME, None)
        };
        self.converted = Some(match result {
            Ok(doc) => {
                let mut walker = Walker::new(self.page_count, self.config.keep_figures);
                for node in &doc.nodes {
                    walker.visit(node, None);
                }
                let (pages, figure_bytes) = walker.finish();
                self.figure_bytes = figure_bytes;
                Ok(pages)
            }
            Err(err) => Err(conversion_failure(&err)),
        });
    }
}

impl DocumentSession for DoclingSession {
    fn page_count(&self) -> u32 {
        self.page_count
    }

    /// First call converts the whole document (see the module docs); a
    /// conversion failure is returned as [`BackendError::Unsupported`].
    fn page_text(&mut self, page: u32) -> Result<PageText, BackendError> {
        let count = self.page_count;
        if page == 0 || page > count {
            return Err(BackendError::PageRange { page, count });
        }
        self.ensure_converted();
        let Some(converted) = self.converted.as_ref() else {
            return Err(BackendError::Unsupported(
                "docling: conversion did not run".to_string(),
            ));
        };
        let pages = match converted {
            Ok(pages) => pages,
            Err(message) => return Err(BackendError::Unsupported(message.clone())),
        };
        let index = (page - 1) as usize;
        let fallback = self.geometry.get(index).copied().unwrap_or_default();
        let mut width = fallback.width;
        let mut height = fallback.height;
        let mut text = if let Some(converted_page) = pages.get(index).and_then(Option::as_ref) {
            if converted_page.width > 0.0 && converted_page.height > 0.0 {
                width = converted_page.width;
                height = converted_page.height;
            }
            let mut text = PageText::new(page, width, height, fallback.rotation);
            text.spans.clone_from(&converted_page.spans);
            text.figures.clone_from(&converted_page.figures);
            text.warnings.clone_from(&converted_page.warnings);
            text
        } else {
            let mut text = PageText::new(page, width, height, fallback.rotation);
            text.warnings.push(NO_ITEMS_WARNING.to_string());
            text
        };
        if width <= 0.0 || height <= 0.0 {
            text.warnings.push(
                "extraction_incomplete: docling page size unknown (no page marker, no MediaBox)"
                    .to_string(),
            );
        }
        match &mut self.native_evidence {
            Ok(session) => match session.page_text(page) {
                Ok(native) => {
                    text.links = native.links;
                    if !fallback.links_match_frame
                        || (width - fallback.width).abs() > 0.01
                        || (height - fallback.height).abs() > 0.01
                    {
                        for link in &mut text.links {
                            link.bbox = None;
                        }
                        text.warnings.push("extraction_incomplete: docling annotation geometry frame is unverified; URI targets retained without rectangles".to_string());
                    }
                    text.warnings.extend(native.warnings);
                }
                Err(error) => text.warnings.push(format!(
                    "extraction_incomplete: native annotation evidence unavailable: {error}"
                )),
            },
            Err(error) => text.warnings.push(format!(
                "extraction_incomplete: native annotation evidence unavailable: {error}"
            )),
        }
        text.warnings.push(
            "extraction_incomplete: docling reconstruction coverage is unverified".to_string(),
        );
        crate::router::mark_incomplete(&mut text);
        if self.config.full {
            crate::reading_order::lines_in_backend_order(&mut text);
            if let Some(current) = pages.get(index).and_then(Option::as_ref)
                && let Some(next) = pages.get(index + 1).and_then(Option::as_ref)
            {
                super::docling_layout::repair_split_reference_start(
                    &mut text,
                    &current.list_items,
                    &next.spans,
                    &next.list_items,
                );
            }
        }
        Ok(text)
    }

    fn info(&self) -> BTreeMap<String, String> {
        self.info.clone()
    }

    /// Bytes of the picture region docling cropped for `(page, index)`,
    /// handed out once.
    fn take_figure_bytes(&mut self, page: u32, index: u32) -> Option<Vec<u8>> {
        self.figure_bytes.remove(&(page, index))
    }
}

/// Denormalise a docling `<location>` grid box over a `width` × `height`
/// page into bottom-left points, exactly as docling-core `json.rs`
/// (`Builder::prov_json`, grid branch) does:
/// `l = x0*w/512`, `t = h - y0*h/512`, `r = x1*w/512`, `b = h - y1*h/512`,
/// each rounded to two decimals. The all-zero box is docling's "no
/// geometry" sentinel and yields `None`.
fn grid_to_bbox(location: [u16; 4], width: f32, height: f32) -> Option<BBox> {
    if location == [0, 0, 0, 0] || width <= 0.0 || height <= 0.0 {
        return None;
    }
    let page_width = f64::from(width);
    let page_height = f64::from(height);
    let [grid_left, grid_top, grid_right, grid_bottom] = location;
    let left = round2(f64::from(grid_left) * page_width / GRID);
    let top = round2(page_height - f64::from(grid_top) * page_height / GRID);
    let right = round2(f64::from(grid_right) * page_width / GRID);
    let bottom = round2(page_height - f64::from(grid_bottom) * page_height / GRID);
    Some(BBox {
        x0: left as f32,
        y0: bottom as f32,
        x1: right as f32,
        y1: top as f32,
    })
}

/// docling's `r2`: round to two decimals.
fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

/// A docling `[l, t, r, b]` box in page points with a top-left origin
/// (`TableCell::bbox`, `Node::Prov`) to bottom-left points.
fn top_left_to_bbox(rect: [f32; 4], height: f32) -> BBox {
    let [left, top, right, bottom] = rect;
    BBox {
        x0: left,
        y0: height - bottom,
        x1: right,
        y1: height - top,
    }
}

/// Undo docling's Markdown escaping (docling-core `json.rs`, `unescape_text`,
/// same replacement order).
fn unescape_markdown(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
        .replace("\\_", "_")
}

/// Strip the `[text](uri)` wrapper docling puts around a fully linked
/// footnote; anything else is returned unchanged.
fn strip_link_wrapper(text: &str) -> &str {
    if !text.starts_with('[') || !text.ends_with(')') {
        return text;
    }
    let Some(split) = text.rfind("](") else {
        return text;
    };
    let target = &text[split + 2..text.len() - 1];
    if target.is_empty() || target.chars().any(char::is_whitespace) {
        return text;
    }
    text.get(1..split).unwrap_or(text)
}

/// Text of a docling node as span text: unescaped, placeholders removed,
/// NFC-normalised. `None` when nothing printable remains.
fn clean_text(raw: &str) -> Option<String> {
    let unescaped = unescape_markdown(raw).replace(IMAGE_PLACEHOLDER, "");
    let unlinked = strip_link_wrapper(unescaped.trim_end());
    // docling joins words with the spacing it measured, so a justified line
    // comes back with runs of spaces; one space is what was printed.
    let normalised: String = unlinked.nfc().collect::<String>();
    let mut collapsed = String::with_capacity(normalised.len());
    let mut pending_space = false;
    for c in normalised.chars() {
        if c == ' ' {
            pending_space = true;
            continue;
        }
        if pending_space && !collapsed.is_empty() {
            collapsed.push(' ');
        }
        pending_space = false;
        collapsed.push(c);
    }
    if collapsed.trim().is_empty() {
        None
    } else {
        Some(collapsed)
    }
}

/// One page being assembled from docling's nodes.
#[derive(Debug, Default)]
struct PageBuild {
    width: f32,
    height: f32,
    spans: Vec<Span>,
    /// Header/footer text, appended after the body items.
    furniture: Vec<(String, Option<BBox>)>,
    figures: Vec<Figure>,
    warnings: Vec<String>,
    formulas_undecoded: u32,
    list_items: Vec<usize>,
}

/// Walks a [`DoclingDocument`]'s nodes in order, tracking the current page
/// from `PageInfo` markers and turning text-bearing nodes into spans.
struct Walker {
    page_count: u32,
    current: Option<u32>,
    pages: BTreeMap<u32, PageBuild>,
    figure_bytes: HashMap<(u32, u32), Vec<u8>>,
    /// Retain picture bytes; otherwise only their digest and size are kept.
    keep_figures: bool,
}

impl Walker {
    fn new(page_count: u32, keep_figures: bool) -> Self {
        Self {
            page_count,
            current: None,
            pages: BTreeMap::new(),
            figure_bytes: HashMap::new(),
            keep_figures,
        }
    }

    /// The current page, created on demand. Items before the first
    /// `PageInfo` are attributed to page 1 with a warning.
    fn current_page_no(&mut self) -> u32 {
        if let Some(number) = self.current {
            return number;
        }
        self.current = Some(1);
        let page = self.pages.entry(1).or_default();
        page.warnings
            .push("extraction_incomplete: docling items before the first page marker attributed to page 1".to_string());
        1
    }

    fn page(&mut self) -> &mut PageBuild {
        let number = self.current_page_no();
        self.pages.entry(number).or_default()
    }

    fn dims(&mut self) -> (f32, f32) {
        let page = self.page();
        (page.width, page.height)
    }

    fn own_location(&mut self, location: Option<[u16; 4]>) -> Option<BBox> {
        let location = location?;
        let (width, height) = self.dims();
        grid_to_bbox(location, width, height)
    }

    fn push_text(&mut self, raw: &str, bbox: Option<BBox>) {
        let Some(text) = clean_text(raw) else {
            return;
        };
        let page = self.page();
        let seq = u32::try_from(page.spans.len()).unwrap_or(u32::MAX);
        page.spans.push(Span {
            text,
            bbox,
            font: None,
            size: None,
            seq,
        });
    }

    fn start_page(&mut self, page_no: usize, width: f32, height: f32) {
        let number = if page_no == 0 {
            self.current.map_or(1, |current| current.saturating_add(1))
        } else {
            u32::try_from(page_no).unwrap_or(u32::MAX)
        };
        self.current = Some(number);
        let page = self.pages.entry(number).or_default();
        page.width = width;
        page.height = height;
    }

    fn visit(&mut self, node: &Node, bbox: Option<BBox>) {
        match node {
            Node::PageInfo {
                page_no,
                width,
                height,
            } => self.start_page(*page_no, *width, *height),
            Node::PageBreak => {}
            Node::Located { location, inner } => {
                let (width, height) = self.dims();
                let located = grid_to_bbox(*location, width, height);
                self.visit(inner, located.or(bbox));
            }
            Node::Prov {
                bbox: rect, inner, ..
            } => {
                let (_, height) = self.dims();
                self.visit(inner, Some(top_left_to_bbox(*rect, height)));
            }
            Node::Commented { inner, .. }
            | Node::DoclangOnly(inner)
            | Node::Furniture { inner, .. } => self.visit(inner, bbox),
            Node::Group { children, .. } => {
                for child in children {
                    self.visit(child, None);
                }
            }
            Node::Paragraph { text } if text.as_str() == FORMULA_PLACEHOLDER => {
                self.page().formulas_undecoded += 1;
            }
            Node::Heading { text, .. }
            | Node::Paragraph { text }
            | Node::CheckboxItem { text, .. }
            | Node::Code { text, .. }
            | Node::Caption { text, .. }
            | Node::CommentSection { text, .. }
            | Node::TextDump(text) => self.push_text(text, bbox),
            Node::ListItem { text, location, .. } => {
                let own = self.own_location(*location);
                let before = self.page().spans.len();
                self.push_text(text, bbox.or(own));
                if self.page().spans.len() > before {
                    self.page().list_items.push(before);
                }
            }
            Node::Formula {
                latex,
                orig,
                location,
            } => {
                let own = self.own_location(*location);
                let text = if orig.trim().is_empty() { latex } else { orig };
                self.push_text(text, bbox.or(own));
            }
            Node::InlineGroup { runs, md_text, .. } => {
                if runs.is_empty() {
                    self.push_text(md_text, bbox);
                } else {
                    let joined: String = runs.iter().map(|run| run.text.as_str()).collect();
                    self.push_text(&joined, bbox);
                }
            }
            Node::FieldRegion { items } => {
                for item in items {
                    if let Some(key) = &item.key {
                        self.push_text(key, None);
                    }
                    if let Some(value) = &item.value {
                        self.push_text(value, None);
                    }
                }
            }
            Node::Table(table) => self.visit_table(table),
            Node::Chart { table, caption, .. } => {
                if let Some(caption) = caption {
                    self.push_text(caption, None);
                }
                self.visit_table(table);
            }
            Node::Picture { caption, image, .. } => {
                self.visit_picture(caption.as_deref(), image.as_ref(), bbox);
            }
            Node::PageFurniture { text, location, .. } => {
                let (width, height) = self.dims();
                let furniture_bbox = grid_to_bbox(*location, width, height);
                if let Some(text) = clean_text(text) {
                    self.page().furniture.push((text, furniture_bbox));
                }
            }
        }
    }

    /// Caption first, then one span per cell in row-major order. First-class
    /// cells (`Table::cells`, `TableFormer`) carry their own top-left box;
    /// the plain text grid has none, and the table's region box is not
    /// lent to its cells. Empty cells are skipped.
    fn visit_table(&mut self, table: &Table) {
        if let Some(caption) = &table.caption {
            self.push_text(caption, None);
        }
        let (_, height) = self.dims();
        let cells: &[TableCell] = table.cells.as_deref().unwrap_or(&[]);
        if cells.is_empty() {
            for row in &table.rows {
                for cell in row {
                    self.push_text(cell, None);
                }
            }
            return;
        }
        let mut ordered: Vec<&TableCell> = cells.iter().collect();
        ordered.sort_by_key(|cell| (cell.start_row, cell.start_col));
        for cell in ordered {
            let cell_bbox = cell.bbox.map(|rect| top_left_to_bbox(rect, height));
            self.push_text(&cell.text, cell_bbox);
        }
    }

    /// Record a [`Figure`] (`kind: "layout"`); the pixels go to the
    /// figure-bytes map, never into a span. The caption is real page text
    /// (docling consumed its region into the picture), so it becomes a span.
    fn visit_picture(
        &mut self,
        caption: Option<&str>,
        image: Option<&PictureImage>,
        bbox: Option<BBox>,
    ) {
        let page_no = self.current_page_no();
        let index = {
            let page = self.page();
            u32::try_from(page.figures.len()).unwrap_or(u32::MAX)
        };
        let mut figure = Figure {
            index,
            bbox,
            kind: "layout".to_string(),
            mime: None,
            width_px: None,
            height_px: None,
            sha256: None,
            file: None,
            caption: caption.and_then(clean_text),
        };
        if let Some(picture) = image {
            figure.mime = Some(picture.mimetype.clone());
            figure.width_px = Some(picture.width);
            figure.height_px = Some(picture.height);
            figure.sha256 = Some(sha256_hex(&picture.data));
            if self.keep_figures {
                self.figure_bytes
                    .insert((page_no, index), picture.data.clone());
            }
        }
        self.page().figures.push(figure);
        if let Some(caption) = caption {
            self.push_text(caption, None);
        }
    }

    /// Pages `1..=page_count` in order; furniture spans follow the body with
    /// continuing `seq`. Pages docling numbered outside that range are
    /// dropped (it never does for a PDF).
    fn finish(mut self) -> (Converted, HashMap<(u32, u32), Vec<u8>>) {
        let mut pages: Converted = Vec::with_capacity(self.page_count as usize);
        for page_no in 1..=self.page_count {
            let Some(mut build) = self.pages.remove(&page_no) else {
                pages.push(None);
                continue;
            };
            for (text, bbox) in std::mem::take(&mut build.furniture) {
                let seq = u32::try_from(build.spans.len()).unwrap_or(u32::MAX);
                build.spans.push(Span {
                    text,
                    bbox,
                    font: None,
                    size: None,
                    seq,
                });
            }
            if build.formulas_undecoded > 0 {
                let count = build.formulas_undecoded;
                build.warnings.push(format!(
                    "extraction_incomplete: docling {count} formula region(s) not decoded; no text emitted"
                ));
            }
            if build.spans.is_empty() && build.figures.is_empty() {
                build.warnings.push(NO_ITEMS_WARNING.to_string());
            }
            pages.push(Some(ConvertedPage {
                width: build.width,
                height: build.height,
                spans: build.spans,
                figures: build.figures,
                warnings: build.warnings,
                list_items: build.list_items,
            }));
        }
        (pages, self.figure_bytes)
    }
}

/// Parse with `lopdf`; an encrypted file needs `password` or fails with
/// [`EncryptionProblem::PasswordRequired`].
fn load_lopdf(bytes: &[u8], password: Option<&str>) -> Result<Document, BackendError> {
    let doc = match password {
        Some(password) => {
            let options = LoadOptions::with_password(password);
            Document::load_mem_with_options(bytes, options).map_err(map_load_error)?
        }
        None => Document::load_mem(bytes).map_err(map_load_error)?,
    };
    if doc.is_encrypted() {
        return Err(BackendError::Encrypted(EncryptionProblem::PasswordRequired));
    }
    if doc.catalog().is_err() {
        return Err(BackendError::Malformed("no document catalog".to_string()));
    }
    Ok(doc)
}

fn map_load_error(err: LopdfError) -> BackendError {
    match err {
        LopdfError::InvalidPassword => BackendError::Encrypted(EncryptionProblem::WrongPassword),
        LopdfError::UnsupportedSecurityHandler(_) | LopdfError::Decryption(_) => {
            BackendError::Encrypted(EncryptionProblem::UnsupportedCipher)
        }
        other => BackendError::Malformed(other.to_string()),
    }
}

/// Whether the file is encrypted when opened without a password.
fn encrypted_without_password(bytes: &[u8]) -> bool {
    Document::load_mem(bytes).is_ok_and(|doc| doc.is_encrypted())
}

/// String-valued trailer `/Info` entries, keys without the leading `/`.
fn info_entries(doc: &Document) -> BTreeMap<String, String> {
    let mut info = BTreeMap::new();
    let Ok(info_ref) = doc.trailer.get(b"Info") else {
        return info;
    };
    let Ok((_, info_obj)) = doc.dereference(info_ref) else {
        return info;
    };
    let Ok(dict) = info_obj.as_dict() else {
        return info;
    };
    for (key, value) in dict {
        if let Ok(text) = lopdf::decode_text_string(value) {
            info.insert(String::from_utf8_lossy(key).into_owned(), text);
        }
    }
    info
}

/// Look `key` up on a page dictionary, walking `/Parent` for inherited
/// attributes (`MediaBox`, `CropBox`, `Rotate`).
fn inherited<'a>(doc: &'a Document, page: &'a Dictionary, key: &[u8]) -> Option<&'a Object> {
    let mut node = page;
    for _ in 0..MAX_PARENT_DEPTH {
        if let Ok(value) = node.get(key)
            && let Ok((_, value)) = doc.dereference(value)
        {
            return Some(value);
        }
        let Ok(parent) = node.get_deref(b"Parent", doc) else {
            return None;
        };
        let Ok(parent_dict) = parent.as_dict() else {
            return None;
        };
        node = parent_dict;
    }
    None
}

/// `(width, height)` of a rectangle array `[x0 y0 x1 y1]`.
fn rect_size(obj: &Object) -> Option<(f32, f32)> {
    let array = obj.as_array().ok()?;
    if array.len() != 4 {
        return None;
    }
    let mut values: [f32; 4] = [0.0; 4];
    for (slot, item) in values.iter_mut().zip(array) {
        *slot = item.as_float().ok()?;
    }
    Some(((values[2] - values[0]).abs(), (values[3] - values[1]).abs()))
}

/// `/Rotate` normalised to 0/90/180/270; anything else reads as 0.
fn page_rotation(doc: &Document, page: &Dictionary) -> i32 {
    let Some(value) = inherited(doc, page, b"Rotate") else {
        return 0;
    };
    let Ok(rotate_degrees) = value.as_i64() else {
        return 0;
    };
    let Ok(normalised) = u32::try_from(rotate_degrees.rem_euclid(360)) else {
        return 0;
    };
    if normalised.is_multiple_of(90) {
        i32::try_from(normalised).unwrap_or(0)
    } else {
        0
    }
}

/// Geometry of every page in page-number order (`CropBox` else `MediaBox`).
fn page_geometry(doc: &Document) -> Vec<PageGeometry> {
    let mut geometry = Vec::new();
    for page_id in doc.get_pages().values() {
        let Ok(page_dict) = doc.get_dictionary(*page_id) else {
            geometry.push(PageGeometry::default());
            continue;
        };
        let crop = inherited(doc, page_dict, b"CropBox").and_then(rect_size);
        let media = inherited(doc, page_dict, b"MediaBox").and_then(rect_size);
        let (width, height) = crop.or(media).unwrap_or((0.0, 0.0));
        geometry.push(PageGeometry {
            width,
            height,
            rotation: page_rotation(doc, page_dict),
            links_match_frame: page_rotation(doc, page_dict) == 0
                && super::docling_text_backend::page_box(doc, *page_id).is_some_and(|bounds| {
                    bounds.x0.abs() <= f32::EPSILON && bounds.y0.abs() <= f32::EPSILON
                }),
        });
    }
    geometry
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use docling_core::CaptionParent;
    use lopdf::content::{Content, Operation};
    use lopdf::{Stream, dictionary};

    use super::*;

    fn close(actual: f32, expected: f32) -> bool {
        (actual - expected).abs() < 0.01
    }

    /// Build a PDF with Helvetica as `/F1` and one page per text, each shown
    /// at 12 pt from (100, 600); `/Info` carries a title.
    fn build_pdf(texts: &[&str]) -> Vec<u8> {
        let mut doc = Document::with_version("1.5");
        let tree_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
        });
        let resources_id = doc.add_object(dictionary! {
            "Font" => dictionary! { "F1" => font_id },
        });
        let mut kids: Vec<Object> = Vec::new();
        for text in texts {
            let operations = vec![
                Operation::new("BT", vec![]),
                Operation::new("Tf", vec!["F1".into(), 12_i32.into()]),
                Operation::new("Td", vec![100_i32.into(), 600_i32.into()]),
                Operation::new("Tj", vec![Object::string_literal(*text)]),
                Operation::new("ET", vec![]),
            ];
            let content = Content { operations }.encode().unwrap();
            let content_id = doc.add_object(Stream::new(dictionary! {}, content));
            let page_id = doc.add_object(dictionary! {
                "Type" => "Page",
                "Parent" => tree_id,
                "Contents" => content_id,
                "Resources" => resources_id,
            });
            kids.push(Object::Reference(page_id));
        }
        let count = i64::try_from(kids.len()).unwrap();
        let tree = dictionary! {
            "Type" => "Pages",
            "Kids" => kids,
            "Count" => count,
            "MediaBox" => vec![0_i32.into(), 0_i32.into(), 612_i32.into(), 792_i32.into()],
        };
        doc.objects.insert(tree_id, Object::Dictionary(tree));
        let info_id = doc.add_object(dictionary! {
            "Title" => Object::string_literal("Test Title"),
        });
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => tree_id,
        });
        doc.trailer.set("Root", catalog_id);
        doc.trailer.set("Info", info_id);
        let mut bytes: Vec<u8> = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        bytes
    }

    /// Two pages with enough text that docling's vestigial-layer gate
    /// (fewer than 32 chars) never fires.
    fn two_page_pdf() -> Vec<u8> {
        build_pdf(&[
            "Hello from the text layer of page one.",
            "Page two has some more text as well.",
        ])
    }

    fn forbids_markup(spans: &[Span]) {
        for span in spans {
            assert!(!span.text.contains("data:image"), "{:?}", span.text);
            assert!(!span.text.contains("base64"), "{:?}", span.text);
            assert!(!span.text.contains("<!--"), "{:?}", span.text);
        }
    }

    #[test]
    fn identity_is_stable() {
        let text = DoclingBackend::text_layer().identity();
        assert_eq!(text.name, "docling-text");
        assert_eq!(text.version, DOCLING_VERSION);
        let mut config = BTreeMap::new();
        config.insert("force_ocr".to_string(), "false".to_string());
        config.insert("full".to_string(), "false".to_string());
        config.insert("ocr".to_string(), "false".to_string());
        config.insert("provider".to_string(), "cpu".to_string());
        config.insert("ocr_engine".to_string(), "ppocr".to_string());
        config.insert("evidence_policy".to_string(), "2".to_string());
        config.insert("tables".to_string(), "false".to_string());
        assert_eq!(text.config_digest, config_digest(&config));

        let full = DoclingBackend::full().identity();
        assert_eq!(full.name, "docling");
        assert_ne!(full.config_digest, text.config_digest);
        assert!(DoclingBackend::full().ocr);
        assert!(!DoclingBackend::full().tables);
        assert!(!DoclingBackend::full().force_ocr);
        assert_eq!(DoclingBackend::default(), DoclingBackend::text_layer());
        assert!(DoclingBackend::text_layer().provides_reading_order());
        assert!(DoclingBackend::full().provides_reading_order());
        assert!(DoclingBackend::full().provides_line_layout());
        assert!(!DoclingBackend::text_layer().provides_line_layout());
    }

    #[test]
    fn docling_version_matches_cargo_lock() {
        let lock = include_str!("../../Cargo.lock");
        let mut locked: Option<&str> = None;
        for block in lock.split("[[package]]") {
            let mut name: Option<&str> = None;
            let mut version: Option<&str> = None;
            for line in block.lines() {
                if let Some(value) = line.strip_prefix("name = ") {
                    name = Some(value.trim().trim_matches('"'));
                } else if let Some(value) = line.strip_prefix("version = ") {
                    version = Some(value.trim().trim_matches('"'));
                }
            }
            if name == Some("docling-pdf") {
                locked = version;
            }
        }
        assert_eq!(
            locked,
            Some(DOCLING_VERSION),
            "Cargo.lock pins another docling-pdf; update DOCLING_VERSION (the identity key)"
        );
    }

    #[test]
    fn grid_denormalisation_matches_docling_json() {
        // Full grid over US Letter: docling divides by 512, so 511 lands at
        // 511*612/512 = 610.80 and 792 - 511*792/512 = 1.55 (2-decimal rounding).
        let full = grid_to_bbox([0, 0, 511, 511], 612.0, 792.0).unwrap();
        assert!(close(full.x0, 0.0), "x0 {}", full.x0);
        assert!(close(full.y1, 792.0), "y1 {}", full.y1);
        assert!(close(full.x1, 610.80), "x1 {}", full.x1);
        assert!(close(full.y0, 1.55), "y0 {}", full.y0);

        let quarter = grid_to_bbox([0, 0, 255, 255], 612.0, 792.0).unwrap();
        assert!(close(quarter.x0, 0.0), "x0 {}", quarter.x0);
        assert!(close(quarter.y1, 792.0), "y1 {}", quarter.y1);
        assert!(close(quarter.x1, 304.80), "x1 {}", quarter.x1);
        assert!(close(quarter.y0, 397.55), "y0 {}", quarter.y0);

        let lower_right = grid_to_bbox([256, 256, 512, 512], 512.0, 512.0).unwrap();
        assert!(close(lower_right.x0, 256.0));
        assert!(close(lower_right.x1, 512.0));
        assert!(close(lower_right.y1, 256.0));
        assert!(close(lower_right.y0, 0.0));

        assert_eq!(grid_to_bbox([0, 0, 0, 0], 612.0, 792.0), None);
        assert_eq!(grid_to_bbox([0, 0, 511, 511], 0.0, 792.0), None);

        let cell = top_left_to_bbox([10.0, 20.0, 110.0, 40.0], 792.0);
        assert!(close(cell.x0, 10.0));
        assert!(close(cell.x1, 110.0));
        assert!(close(cell.y0, 752.0));
        assert!(close(cell.y1, 772.0));
    }

    #[test]
    fn text_cleaning_removes_docling_markup() {
        assert_eq!(
            clean_text("a\\_b &amp; c &lt;d&gt;").as_deref(),
            Some("a_b & c <d>")
        );
        assert_eq!(
            clean_text("[1 https://x.org/a](https://x.org/a)").as_deref(),
            Some("1 https://x.org/a")
        );
        assert_eq!(clean_text("[a] (b)").as_deref(), Some("[a] (b)"));
        assert_eq!(clean_text("text  <!-- image -->").as_deref(), Some("text"));
        assert_eq!(clean_text("<!-- image -->"), None);
        assert_eq!(clean_text("   "), None);
        // NFC: e + combining acute becomes the precomposed letter.
        assert_eq!(clean_text("e\u{301}").as_deref(), Some("\u{e9}"));
        // DOIs survive untouched.
        assert_eq!(
            clean_text("10.1000/abc\\_def.12").as_deref(),
            Some("10.1000/abc_def.12")
        );
    }

    fn picture(caption: Option<&str>, image: Option<PictureImage>) -> Node {
        Node::Picture {
            caption: caption.map(String::from),
            caption_href: None,
            image,
            classification: None,
            caption_parent: CaptionParent::Item,
        }
    }

    fn located(location: [u16; 4], inner: Node) -> Node {
        Node::Located {
            location,
            inner: Box::new(inner),
        }
    }

    fn cell(text: &str, row: usize, col: usize, bbox: Option<[f32; 4]>) -> TableCell {
        TableCell {
            text: text.to_string(),
            bbox,
            start_row: row,
            start_col: col,
            row_span: 1,
            col_span: 1,
            column_header: row == 0,
            row_header: false,
            row_section: false,
        }
    }

    #[test]
    fn walker_maps_nodes_to_spans_and_figures() {
        let image = PictureImage {
            mimetype: "image/png".to_string(),
            width: 4,
            height: 3,
            data: vec![0x89, b'P', b'N', b'G', 1, 2, 3],
        };
        let table = Table {
            rows: vec![
                vec!["H1".to_string(), "H2".to_string()],
                vec!["b".to_string(), "a".to_string()],
            ],
            caption: Some("Table 1: Cells".to_string()),
            cells: Some(vec![
                cell("a", 1, 1, Some([300.0, 100.0, 400.0, 120.0])),
                cell("H1", 0, 0, None),
                cell("b", 1, 0, None),
                cell("H2", 0, 1, None),
                cell("", 2, 0, None),
            ]),
            ..Table::default()
        };
        let nodes = vec![
            Node::PageInfo {
                page_no: 1,
                width: 612.0,
                height: 792.0,
            },
            located(
                [0, 0, 511, 511],
                Node::Heading {
                    level: 2,
                    text: "Title \\_with &amp; escapes".to_string(),
                },
            ),
            Node::PageFurniture {
                footer: true,
                location: [0, 500, 511, 511],
                text: "Footer 1".to_string(),
            },
            located([0, 0, 255, 255], picture(Some("Figure 1"), Some(image))),
            located([0, 0, 0, 0], Node::Table(table)),
            Node::Paragraph {
                text: FORMULA_PLACEHOLDER.to_string(),
            },
            Node::Paragraph {
                text: "[1 https://x](https://x)".to_string(),
            },
            Node::PageInfo {
                page_no: 2,
                width: 612.0,
                height: 792.0,
            },
            Node::PageInfo {
                page_no: 3,
                width: 500.0,
                height: 700.0,
            },
            Node::Paragraph {
                text: "Page three".to_string(),
            },
        ];
        let mut walker = Walker::new(3, true);
        for node in &nodes {
            walker.visit(node, None);
        }
        let (pages, mut figure_bytes) = walker.finish();
        assert_eq!(pages.len(), 3);

        let first = pages[0].as_ref().unwrap();
        assert!(close(first.width, 612.0));
        let texts: Vec<&str> = first.spans.iter().map(|span| span.text.as_str()).collect();
        assert_eq!(
            texts,
            vec![
                "Title _with & escapes",
                "Figure 1",
                "Table 1: Cells",
                "H1",
                "H2",
                "b",
                "a",
                "1 https://x",
                "Footer 1",
            ]
        );
        for (index, span) in first.spans.iter().enumerate() {
            assert_eq!(span.seq, u32::try_from(index).unwrap());
            assert_eq!(span.font, None);
            assert_eq!(span.size, None);
        }
        forbids_markup(&first.spans);
        let heading_box = first.spans[0].bbox.unwrap();
        assert!(close(heading_box.x0, 0.0));
        assert!(close(heading_box.y1, 792.0));
        let cell_box = first.spans[6].bbox.unwrap();
        assert!(close(cell_box.x0, 300.0));
        assert!(close(cell_box.y0, 672.0));
        assert!(close(cell_box.y1, 692.0));
        assert_eq!(first.spans[3].bbox, None);
        let footer_box = first.spans[8].bbox.unwrap();
        assert!(close(footer_box.y1, 792.0 - 500.0 * 792.0 / 512.0));

        assert_eq!(first.figures.len(), 1);
        let figure = &first.figures[0];
        assert_eq!(figure.index, 0);
        assert_eq!(figure.kind, "layout");
        assert_eq!(figure.mime.as_deref(), Some("image/png"));
        assert_eq!(figure.width_px, Some(4));
        assert_eq!(figure.height_px, Some(3));
        assert_eq!(figure.caption.as_deref(), Some("Figure 1"));
        assert_eq!(figure.file, None);
        let expected_hash = sha256_hex(&[0x89, b'P', b'N', b'G', 1, 2, 3]);
        assert_eq!(figure.sha256.as_deref(), Some(expected_hash.as_str()));
        let picture_box = figure.bbox.unwrap();
        assert!(close(picture_box.x1, 304.80));
        assert_eq!(
            figure_bytes.remove(&(1, 0)),
            Some(vec![0x89, b'P', b'N', b'G', 1, 2, 3])
        );
        assert!(figure_bytes.is_empty());
        assert!(
            first
                .warnings
                .iter()
                .any(|w| w.contains("formula region(s) not decoded")),
            "{:?}",
            first.warnings
        );

        let second = pages[1].as_ref().unwrap();
        assert!(second.spans.is_empty());
        assert_eq!(second.warnings, vec![NO_ITEMS_WARNING.to_string()]);

        let third = pages[2].as_ref().unwrap();
        assert!(close(third.width, 500.0));
        assert_eq!(third.spans.len(), 1);
        assert_eq!(third.spans[0].text, "Page three");
    }

    #[test]
    fn walker_handles_unstamped_and_missing_page_markers() {
        let nodes = vec![
            Node::Paragraph {
                text: "before any marker".to_string(),
            },
            Node::PageInfo {
                page_no: 0,
                width: 100.0,
                height: 200.0,
            },
            Node::Paragraph {
                text: "second".to_string(),
            },
        ];
        let mut walker = Walker::new(3, true);
        for node in &nodes {
            walker.visit(node, None);
        }
        let (pages, _) = walker.finish();
        let first = pages[0].as_ref().unwrap();
        assert_eq!(first.spans[0].text, "before any marker");
        assert!(first.warnings.iter().any(|w| w.contains("page 1")));
        let second = pages[1].as_ref().unwrap();
        assert_eq!(second.spans[0].text, "second");
        assert!(close(second.height, 200.0));
        assert!(pages[2].is_none());
    }

    #[test]
    fn text_layer_mode_reads_two_pages() {
        let bytes = two_page_pdf();
        let backend = DoclingBackend::text_layer();
        let mut session = backend.open(&bytes, None).unwrap();
        assert_eq!(session.page_count(), 2);

        let page = session.page_text(1).unwrap();
        assert_eq!(page.page, 1);
        assert!(close(page.width, 612.0), "width {}", page.width);
        assert!(close(page.height, 792.0), "height {}", page.height);
        assert_eq!(page.rotation, 0);
        assert!(page.lines.is_empty());
        assert!(page.text.is_empty());
        assert!(
            page.spans.iter().any(|span| span.text.contains("Hello")),
            "{:?}",
            page.spans
        );
        for (index, span) in page.spans.iter().enumerate() {
            assert_eq!(span.seq, u32::try_from(index).unwrap());
        }
        forbids_markup(&page.spans);
        assert!(page.figures.is_empty());

        let second = session.page_text(2).unwrap();
        assert_eq!(second.page, 2);
        assert!(
            second
                .spans
                .iter()
                .any(|span| span.text.contains("Page two")),
            "{:?}",
            second.spans
        );
        assert_eq!(session.take_figure_bytes(1, 0), None);
        assert_eq!(
            session.info().get("Title").map(String::as_str),
            Some("Test Title")
        );
    }

    #[test]
    fn page_out_of_range_is_reported() {
        let bytes = build_pdf(&["Only one page of text lives here."]);
        let mut session = DoclingBackend::text_layer().open(&bytes, None).unwrap();
        match session.page_text(2) {
            Err(BackendError::PageRange { page, count }) => {
                assert_eq!(page, 2);
                assert_eq!(count, 1);
            }
            other => panic!("expected PageRange, got {other:?}"),
        }
        assert!(matches!(
            session.page_text(0),
            Err(BackendError::PageRange { .. })
        ));
    }

    #[test]
    fn malformed_bytes_are_rejected() {
        let backend = DoclingBackend::text_layer();
        let Err(err) = backend.open(b"not a pdf at all", None) else {
            panic!("expected Malformed for non-PDF bytes");
        };
        assert!(matches!(err, BackendError::Malformed(_)), "{err}");
    }

    #[test]
    fn pdfium_error_messages_map_to_backend_errors() {
        let missing = PdfError::Pdfium("the pdfium library is not installed. …".to_string());
        assert!(matches!(
            open_error(&missing, false),
            BackendError::Unsupported(_)
        ));
        let locked = PdfError::Pdfium("PdfiumLibraryInternalError(PasswordError)".to_string());
        assert!(matches!(
            open_error(&locked, false),
            BackendError::Encrypted(EncryptionProblem::PasswordRequired)
        ));
        assert!(matches!(
            open_error(&locked, true),
            BackendError::Encrypted(EncryptionProblem::WrongPassword)
        ));
        let broken = PdfError::Pdfium("FormatError".to_string());
        assert!(matches!(
            open_error(&broken, false),
            BackendError::Malformed(_)
        ));
        assert!(conversion_failure(&broken).contains("FormatError"));
        let layout = PdfError::Layout("model missing".to_string());
        assert!(matches!(
            open_error(&layout, false),
            BackendError::Unsupported(_)
        ));
    }

    #[test]
    fn full_mode_rejects_working_directory_pdfium_paths() {
        assert!(!is_trusted_pdfium_path(".pdfium/lib"));
        let absolute = std::env::current_dir().unwrap().join(".pdfium/lib");
        assert!(is_trusted_pdfium_path(absolute.to_str().unwrap()));
    }

    #[test]
    fn full_mode_requires_a_present_library_at_the_trusted_path() {
        for configured in ["", ".pdfium/lib"] {
            let err = trusted_pdfium_library(configured).unwrap_err();
            assert!(err.to_string().contains("not installed"), "{err}");
        }
        let missing = tempfile::tempdir().unwrap();
        let err = trusted_pdfium_library(missing.path().to_str().unwrap()).unwrap_err();
        assert!(err.to_string().contains("not installed"), "{err}");
        let file = missing.path().join("libpdfium.so");
        std::fs::write(&file, []).unwrap();
        assert_eq!(
            trusted_pdfium_library(file.to_str().unwrap()).unwrap(),
            file
        );
    }

    /// Whether the full pipeline can run here: a layout model and a pdfium
    /// library must both be present.
    fn native_assets_present() -> bool {
        let models_dir = std::env::var("DOCLING_RS_MODELS_DIR")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map_or_else(|| PathBuf::from(".models"), PathBuf::from);
        let model = models_dir.join("layout_heron_int8.onnx").exists()
            || Path::new(".models/layout_heron_int8.onnx").exists();
        let pdfium = std::env::var("PDFIUM_DYNAMIC_LIB_PATH")
            .is_ok_and(|value| Path::new(&value).exists())
            || Path::new(".pdfium/lib").exists();
        model && pdfium
    }

    #[test]
    fn full_mode_open_without_pdfium_is_unsupported() {
        let bytes = two_page_pdf();
        match DoclingBackend::full().open(&bytes, None) {
            Ok(session) => assert_eq!(session.page_count(), 2),
            Err(BackendError::Unsupported(message)) => {
                assert!(message.contains("not installed"), "{message}");
            }
            Err(other) => panic!("expected Unsupported or a session, got {other:?}"),
        }
    }

    #[test]
    fn full_pipeline_smoke() {
        if !native_assets_present() {
            eprintln!("skipped: docling models/pdfium not present");
            return;
        }
        let bytes = two_page_pdf();
        let backend = DoclingBackend::full();
        let mut session = backend.open(&bytes, None).unwrap();
        assert_eq!(session.page_count(), 2);
        let page = session.page_text(1).unwrap();
        assert!(close(page.width, 612.0), "width {}", page.width);
        assert!(
            page.spans.iter().any(|span| span.text.contains("Hello")),
            "{:?}",
            page.spans
        );
        forbids_markup(&page.spans);
        for (index, span) in page.spans.iter().enumerate() {
            assert_eq!(span.seq, u32::try_from(index).unwrap());
        }
        let second = session.page_text(2).unwrap();
        forbids_markup(&second.spans);
        assert!(
            second
                .spans
                .iter()
                .any(|span| span.text.contains("Page two")),
            "{:?}",
            second.spans
        );
    }
}
