//! Native backend built on `pdfium-render` 0.8 (Google's `pdfium` loaded as a
//! dynamic library). `pdfium` interprets the content streams for us and hands
//! back positioned page objects; this module turns text objects into
//! [`Span`]s and image objects into [`Figure`]s. Nothing is ordered or
//! repaired here, and no pixel data ever reaches a span.
//!
//! # Binding and threading
//!
//! The crate is built with its `thread_safe` feature (`Cargo.toml`), whose
//! wrapper (`bindings/thread_safe.rs`) takes one process-wide mutex,
//! `PDFIUM_THREAD_MARSHALL`, inside `FPDF_InitLibrary` (lines 120-128) and
//! releases it inside `FPDF_DestroyLibrary` (lines 138-146). `Pdfium::new`
//! calls the former (`pdfium.rs:151-155`) and `Drop for Pdfium` the latter
//! (`pdfium.rs:427-433`), so the lock is held **per instance, for the whole
//! life of a `Pdfium` value**, not per call. Two consequences:
//!
//! * The crate already serialises every `Pdfium` in the process: a second
//!   `Pdfium::new`, on any thread, blocks until the first value is dropped.
//!   No gate of our own is needed on top of that.
//! * A long-lived `Pdfium` (a `thread_local!` cache, a static, a dedicated
//!   thread) would keep that mutex locked. `docling-pdf` binds its own
//!   `Pdfium` inside every document open (`docling-pdf/src/pdfium_backend.rs`
//!   `bind()`, lines 224-239, called from `PdfDocument::open`, 247-249), and
//!   `tpe backends` probes `pdfium` and then `docling` on the same thread.
//!   Re-locking a `std::sync::Mutex` from the thread that holds it deadlocks
//!   or panics, and other threads would block for as long as the caching
//!   thread lived. So the binding is deliberately **not** cached: each
//!   [`Extractor::open`] binds the library (`dlopen` plus `FPDF_InitLibrary`),
//!   extracts everything, and drops the binding before returning. Nothing
//!   `pdfium`-related outlives `open`, so sessions are plain data and any
//!   number of them may coexist on any threads. (Caching would only become
//!   safe with the crate's `thread_safe` feature off and a gate of our own,
//!   which is a `Cargo.toml` decision shared with `docling-pdf`.)
//!
//! # Document ownership and eager extraction
//!
//! `PdfDocument<'a>` borrows both the `Pdfium` and the byte slice it was
//! loaded from, so it cannot be stored next to its owners without a
//! self-referential struct. Re-opening the document for every page (the
//! previous design) made `pdfium` re-parse the xref, page tree and every
//! font used by the page on each call, which measured at about 259 ms of
//! parse time per document against 45 ms for `lopdf`. Instead `open` walks
//! all pages once, while the document is alive, and the session keeps only
//! the results: one [`PageText`] of spans per page plus the raw figure
//! streams. [`DocumentSession::page_text`] clones from that cache.
//!
//! Distinct retained image streams are capped at 256 MiB and deduplicated
//! as each image is visited, so a page does not accumulate copies before
//! applying the cap. A single raw stream is still allocated before hashing;
//! this cap is not a total-memory or peak-RSS bound. All page spans and
//! figure metadata remain resident, and `pdfium` can allocate its own caches
//! until the document is dropped at the end of `open`. Extraction of a page
//! range still visits the whole document. Bounded page-window extraction
//! remains a separate refinement.
//!
//! # Text granularity
//!
//! `pdfium` yields one text object per `Tj`/`TJ` operator, so a span here
//! covers a whole kerned run (`lopdf` emits one span per `TJ` string piece).
//! That is coarser, never finer: the reading order joins spans on a
//! baseline and re-inserts word spaces from geometry, so both granularities
//! assemble the same line text. The text of an object comes from the page's
//! text layer (`FPDFTextObj_GetText`), which may carry the `\r`/`\n`
//! separators that layer inserts between runs; they are not glyphs and are
//! replaced by a space (see `clean_object_text`). Spaces are kept exactly
//! as reported. U+0002 is restored to a hyphen only when the object's
//! corresponding characters are confirmed by `FPDFText_IsHyphen`. This keeps
//! visible hyphens in spans; the existing cleanup decides whether to join a
//! word across a line break.
//!
//! # Library compatibility
//!
//! Dynamic binding resolves every `FPDF_*` symbol of the compiled-in API
//! version (`pdfium_latest`) at bind time, so the `libpdfium` found must be at
//! least that recent. The pinned `chromium/8066` build is.
//!
//! The backend identity records the binding version, not an assumed native
//! release: `PDFium` exposes no runtime version query. Its configuration digest
//! includes the effective absolute library path and a streaming SHA-256 of
//! its configured bytes (at most 64 MiB). Keep that trusted deployment file
//! unchanged during a job. This fingerprint is not an immutable snapshot and
//! does not cover concurrent replacement or the library's own dependencies.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use pdfium_render::prelude::{
    PdfDocument, PdfDocumentMetadataTagType, PdfMatrix, PdfPageImageObject, PdfPageObject,
    PdfPageObjectCommon, PdfPageObjectsCommon, PdfPageObjectsIterator, PdfPageRenderRotation,
    PdfPageText, PdfPageTextObject, PdfRect, Pdfium, PdfiumError, PdfiumInternalError,
};
use unicode_normalization::UnicodeNormalization;

mod unicode_mapping;
use unicode_mapping::MappingDocument;

use crate::backend::{BackendError, DocumentSession, EncryptionProblem, Extractor};
use crate::schema::{BBox, BackendIdentity, Figure, PageText, Span, config_digest, sha256_hex};

/// The `pdfium-render` release this backend is written against; a unit test
/// ties it to `Cargo.lock`.
const PDFIUM_RENDER_VERSION: &str = "0.8.37";
/// Environment variable naming the library directory or file.
const ENV_LIBRARY_PATH: &str = "PDFIUM_DYNAMIC_LIB_PATH";
/// Bound on nested Form `XObject` traversal.
const MAX_FORM_DEPTH: u32 = 8;
/// Text interpretation is part of the result identity, independently of the
/// dynamically loaded library version.
const TEXT_POLICY: &str = "confirmed-hyphen-markers-v1";
/// `chars_for_object` scans the whole text page. Bound the additional work
/// across marked objects without discarding any text when the budget runs out.
const HYPHEN_SCAN_WORK_LIMIT: usize = 1_000_000;
/// Bound native-library fingerprint work independently of document size.
const MAX_LIBRARY_BYTES: u64 = 64 * 1024 * 1024;

/// Binding identity; it does not claim a runtime `PDFium` release.
fn version_string() -> String {
    format!("dynamic-binding-{PDFIUM_RENDER_VERSION}")
}

/// Upper bound on distinct image bytes retained by one session (256 MiB).
const FIGURE_BYTES_CAP: usize = 256 * 1024 * 1024;

/// The `pdfium` extractor.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PdfiumBackend {
    /// Directory holding `libpdfium.so` / `libpdfium.dylib` / `pdfium.dll`.
    /// `None` uses `$PDFIUM_DYNAMIC_LIB_PATH` (an absolute directory or file).
    /// Relative paths and implicit loader searches are rejected so an
    /// attacker-controlled working directory cannot supply native code.
    pub library_dir: Option<String>,
}

impl Extractor for PdfiumBackend {
    /// Fingerprint the effective configured library without loading native code.
    fn identity(&self) -> BackendIdentity {
        let environment = std::env::var(ENV_LIBRARY_PATH).ok();
        library_identity(self.library_dir.as_deref(), environment.as_deref())
    }

    /// Geometry only; the engine's XY-cut orders the spans.
    fn provides_reading_order(&self) -> bool {
        false
    }

    /// Bind the library, load the bytes, count pages, read `/Info` and
    /// extract every page's spans and figures, then drop the document and
    /// the binding before returning (see the module docs). A page whose
    /// extraction fails is recorded and reported by `page_text` for that
    /// page only. Blocks while another `Pdfium` is alive anywhere in the
    /// process.
    fn open(
        &self,
        bytes: &[u8],
        password: Option<&str>,
    ) -> Result<Box<dyn DocumentSession>, BackendError> {
        self.open_session(bytes, password, FIGURE_BYTES_CAP)
            .map(|session| Box::new(session) as Box<dyn DocumentSession>)
    }
}

fn library_identity(configured: Option<&str>, environment: Option<&str>) -> BackendIdentity {
    let location = configured.or(environment.filter(|value| !value.is_empty()));
    let file = location.and_then(|path| configured_library_file(path).ok());
    let mut config = BTreeMap::new();
    config.insert(
        "library_path".to_string(),
        file.as_ref().map_or_else(
            || location.unwrap_or_default().to_string(),
            |path| path.to_string_lossy().into_owned(),
        ),
    );
    config.insert(
        "library_sha256".to_string(),
        file.as_deref()
            .and_then(|path| fingerprint_library(path).ok())
            .unwrap_or_else(|| "unavailable".to_string()),
    );
    config.insert("unicode_mapping_policy".to_string(), "1".to_string());
    config.insert("text_policy".to_string(), TEXT_POLICY.to_string());
    BackendIdentity {
        name: "pdfium".to_string(),
        version: version_string(),
        config_digest: config_digest(&config),
    }
}

fn checked_library_file(path: &Path) -> io::Result<File> {
    let metadata = std::fs::metadata(path)?;
    if !metadata.is_file() || metadata.len() > MAX_LIBRARY_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "PDFium library must be a regular file no larger than 64 MiB",
        ));
    }
    File::open(path)
}

fn fingerprint_library(path: &Path) -> io::Result<String> {
    use sha2::{Digest, Sha256};
    let mut source = checked_library_file(path)?.take(MAX_LIBRARY_BYTES + 1);
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 16 * 1024];
    let mut total = 0_u64;
    loop {
        let count = source.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total += count as u64;
        if total > MAX_LIBRARY_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "PDFium library exceeds 64 MiB",
            ));
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hex::encode(hasher.finalize()))
}

impl PdfiumBackend {
    /// The cap is private and injectable so boundary tests need only tiny images.
    fn open_session(
        &self,
        bytes: &[u8],
        password: Option<&str>,
        figure_cap: usize,
    ) -> Result<PdfiumSession, BackendError> {
        let pdfium = bind(self.library_dir.as_deref())?;
        // `doc` borrows `pdfium` and `bytes`; locals drop in reverse order.
        let doc = load(&pdfium, bytes, password)?;
        let mapping = MappingDocument::new(pdfium.bindings(), bytes, password);
        // The wrapper truncates native page counts to u16. Check the raw
        // count through our existing native document before trusting it.
        let page_count = mapping.page_count()?;
        let info = read_info(&doc);
        let mut pages = Vec::new();
        let mut figures = FigureStore::new(figure_cap);
        for page in 1..=page_count {
            let mut extracted = extract_numbered(&doc, page, &mut figures);
            if let (Ok(text), Ok(index)) = (&mut extracted, u16::try_from(page - 1))
                && let Some(warning) = mapping.warning(index)
            {
                text.warnings.push(warning);
            }
            pages.push(extracted);
        }
        Ok(PdfiumSession {
            page_count,
            info,
            pages,
            figures,
        })
    }
}

/// Distinct image payload plus the number of occurrences still available.
struct FigureBlob {
    bytes: Vec<u8>,
    remaining: usize,
}

/// Apply the session cap before visiting the next image, including images
/// on the same page. Metadata is kept by the collector even on overflow.
struct FigureStore {
    blobs: HashMap<String, FigureBlob>,
    keys: HashMap<(u32, u32), String>,
    retained: usize,
    cap: usize,
}

impl FigureStore {
    fn new(cap: usize) -> Self {
        Self {
            blobs: HashMap::new(),
            keys: HashMap::new(),
            retained: 0,
            cap,
        }
    }

    /// Return the digest even when the payload cannot be retained.
    fn retain(&mut self, page: u32, index: u32, bytes: Vec<u8>) -> (String, bool) {
        let key = sha256_hex(&bytes);
        if let Some(blob) = self.blobs.get_mut(&key) {
            blob.remaining += 1;
        } else if bytes.len() <= self.cap.saturating_sub(self.retained) {
            self.retained += bytes.len();
            self.blobs.insert(
                key.clone(),
                FigureBlob {
                    bytes,
                    remaining: 1,
                },
            );
        } else {
            return (key, false);
        }
        self.keys.insert((page, index), key.clone());
        (key, true)
    }

    fn take(&mut self, page: u32, index: u32) -> Option<Vec<u8>> {
        let key = self.keys.remove(&(page, index))?;
        let blob = self.blobs.get_mut(&key)?;
        blob.remaining -= 1;
        if blob.remaining == 0 {
            let blob = self.blobs.remove(&key)?;
            self.retained -= blob.bytes.len();
            Some(blob.bytes)
        } else {
            Some(blob.bytes.clone())
        }
    }
}

/// Extract 1-based `page` from an open document, reducing any failure to
/// the message `page_text` will later wrap in [`BackendError::Page`].
fn extract_numbered(
    doc: &PdfDocument<'_>,
    page: u32,
    figures: &mut FigureStore,
) -> Result<PageText, String> {
    let Ok(index) = u16::try_from(page - 1) else {
        return Err("page index beyond pdfium's 16-bit range".to_string());
    };
    extract_page(doc, page, index, figures).map_err(|err| match err {
        BackendError::Page { message, .. } => message,
        other => other.to_string(),
    })
}

/// Bind `libpdfium` following the search order documented on
/// [`PdfiumBackend::library_dir`]. Only an explicitly configured absolute
/// location is accepted. Each call is one `dlopen` and one
/// `FPDF_InitLibrary`, and it blocks until no other `Pdfium` exists in the
/// process (module docs).
fn bind(library_dir: Option<&str>) -> Result<Pdfium, BackendError> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(dir) = library_dir {
        candidates.push(configured_library_file(dir)?);
    } else if let Ok(configured) = std::env::var(ENV_LIBRARY_PATH)
        && !configured.is_empty()
    {
        candidates.push(configured_library_file(&configured)?);
    }
    let mut failures: Vec<String> = Vec::new();
    for candidate in &candidates {
        // Keep successful loads within the same regular-file/size contract as
        // identity fingerprinting. No implicit loader fallback is permitted.
        if let Err(err) = checked_library_file(candidate) {
            failures.push(format!("{}: {err}", candidate.display()));
            continue;
        }
        match Pdfium::bind_to_library(candidate) {
            Ok(bindings) => return Ok(Pdfium::new(bindings)),
            Err(err) => failures.push(format!("{}: {err:?}", candidate.display())),
        }
    }
    let detail = failures.join("; ");
    let hint = format!("set {ENV_LIBRARY_PATH} to an absolute trusted path");
    Err(BackendError::Unsupported(format!(
        "pdfium library not found: {hint} ({detail})"
    )))
}

/// Resolve an explicitly trusted library location without consulting the
/// process working directory.
fn configured_library_file(path: &str) -> Result<PathBuf, BackendError> {
    let path = Path::new(path);
    if !path.is_absolute() {
        return Err(BackendError::Unsupported(format!(
            "pdfium library path must be absolute: {}",
            path.display()
        )));
    }
    Ok(library_file(path))
}

/// `path` itself when it names a file, else the platform library file name
/// inside that directory.
pub(crate) fn library_file(path: &Path) -> PathBuf {
    if path.is_file() {
        path.to_path_buf()
    } else {
        Pdfium::pdfium_platform_library_name_at_path(path)
    }
}

/// Open `bytes` with `pdfium`; the document borrows both arguments.
fn load<'a>(
    pdfium: &'a Pdfium,
    bytes: &'a [u8],
    password: Option<&str>,
) -> Result<PdfDocument<'a>, BackendError> {
    pdfium
        .load_pdf_from_byte_slice(bytes, password)
        .map_err(|err| map_load_error(&err, password.is_some()))
}

/// `FPDF_ERR_PASSWORD` covers both "needs a password" and "wrong password";
/// only the presence of a supplied password tells them apart.
fn map_load_error(err: &PdfiumError, had_password: bool) -> BackendError {
    match err {
        PdfiumError::PdfiumLibraryInternalError(PdfiumInternalError::PasswordError) => {
            let problem = if had_password {
                EncryptionProblem::WrongPassword
            } else {
                EncryptionProblem::PasswordRequired
            };
            BackendError::Encrypted(problem)
        }
        PdfiumError::PdfiumLibraryInternalError(PdfiumInternalError::SecurityError) => {
            BackendError::Encrypted(EncryptionProblem::UnsupportedCipher)
        }
        other => BackendError::Malformed(format!("pdfium: {other:?}")),
    }
}

/// `/Info` entries as `pdfium` exposes them, keyed like the PDF dictionary.
fn read_info(doc: &PdfDocument<'_>) -> BTreeMap<String, String> {
    let mut info = BTreeMap::new();
    for tag in doc.metadata().iter() {
        let key = match tag.tag_type() {
            PdfDocumentMetadataTagType::Title => "Title",
            PdfDocumentMetadataTagType::Author => "Author",
            PdfDocumentMetadataTagType::Subject => "Subject",
            PdfDocumentMetadataTagType::Keywords => "Keywords",
            PdfDocumentMetadataTagType::Creator => "Creator",
            PdfDocumentMetadataTagType::Producer => "Producer",
            PdfDocumentMetadataTagType::CreationDate => "CreationDate",
            PdfDocumentMetadataTagType::ModificationDate => "ModDate",
        };
        info.insert(key.to_string(), tag.value().to_string());
    }
    info
}

/// Everything `open` extracted; holds no `pdfium` state (module docs).
struct PdfiumSession {
    page_count: u32,
    info: BTreeMap<String, String>,
    /// One entry per page in order: the spans and figures, or the message
    /// of the failure that page hit.
    pages: Vec<Result<PageText, String>>,
    /// Shared image payloads, released after their final occurrence is taken.
    figures: FigureStore,
}

impl DocumentSession for PdfiumSession {
    fn page_count(&self) -> u32 {
        self.page_count
    }

    /// A clone of the cached page; the same page may be asked for again.
    fn page_text(&mut self, page: u32) -> Result<PageText, BackendError> {
        let count = self.page_count;
        if page == 0 || page > count {
            return Err(BackendError::PageRange { page, count });
        }
        let entry = usize::try_from(page - 1)
            .ok()
            .and_then(|index| self.pages.get(index));
        match entry {
            Some(Ok(text)) => Ok(text.clone()),
            Some(Err(message)) => Err(page_error(page, message.clone())),
            None => Err(BackendError::PageRange { page, count }),
        }
    }

    fn info(&self) -> BTreeMap<String, String> {
        self.info.clone()
    }

    fn take_figure_bytes(&mut self, page: u32, index: u32) -> Option<Vec<u8>> {
        self.figures.take(page, index)
    }
}

fn page_error(page: u32, message: String) -> BackendError {
    BackendError::Page { page, message }
}

fn extract_page(
    doc: &PdfDocument<'_>,
    page: u32,
    index: u16,
    figures: &mut FigureStore,
) -> Result<PageText, BackendError> {
    let pdf_page = doc
        .pages()
        .get(index)
        .map_err(|err| page_error(page, format!("load page: {err:?}")))?;
    let rotation = match pdf_page.rotation() {
        Ok(PdfPageRenderRotation::Degrees90) => 90,
        Ok(PdfPageRenderRotation::Degrees180) => 180,
        Ok(PdfPageRenderRotation::Degrees270) => 270,
        Ok(PdfPageRenderRotation::None) | Err(_) => 0,
    };
    let text_page = pdf_page
        .text()
        .map_err(|err| page_error(page, format!("text page: {err:?}")))?;
    let (width, height) = unrotated_size(pdf_page.width().value, pdf_page.height().value, rotation);
    let mut collector = Collector {
        page: PageText::new(page, width, height, rotation),
        figures,
        seq: 0,
        paths: 0,
        shadings: 0,
        unsupported: 0,
        hyphen_work_left: HYPHEN_SCAN_WORK_LIMIT,
    };
    collector.visit(pdf_page.objects().iter(), &text_page, None, 0);
    Ok(collector.finish())
}

/// The unrotated page size from the size `pdfium` reports, so that
/// [`PageText`] matches the `lopdf` backend: `width`/`height` are the
/// `CropBox`/`MediaBox` extent before `/Rotate`, and `rotation` carries the
/// turn that `reading_order` applies.
///
/// `FPDF_GetPageWidthF`/`FPDF_GetPageHeightF` (behind `PdfPage::width` and
/// `PdfPage::height`) return the displayed size: the `pdfium-render` 0.8.37
/// binding docs say "changing the rotation of the page affects the return
/// value" (`bindings.rs`, the two getters), because `pdfium` swaps the box
/// extents for an odd number of quarter turns. So 90 and 270 swap back here.
///
/// Span and figure boxes need no such treatment: `PdfPageObjectCommon::bounds`
/// calls `FPDFPageObj_GetRotatedBounds` or `FPDFPageObj_GetBounds` directly
/// (`object/private.rs`, `bounds_impl`) without consulting the page's
/// `/Rotate`; those return the object's box in PDF user space (the "rotated"
/// refers to the object's own matrix), the same frame as the text matrix
/// origin that `add_text` widens the box with.
fn unrotated_size(width: f32, height: f32, rotation: i32) -> (f32, f32) {
    match rotation {
        90 | 270 => (height, width),
        _ => (width, height),
    }
}

/// Accumulates spans, figures and object counts while walking a page.
struct Collector<'a> {
    page: PageText,
    figures: &'a mut FigureStore,
    seq: u32,
    paths: u32,
    shadings: u32,
    unsupported: u32,
    hyphen_work_left: usize,
}

impl Collector<'_> {
    fn warn(&mut self, message: String) {
        if !self.page.warnings.contains(&message) {
            self.page.warnings.push(message);
        }
    }

    /// Walk `objects` in content-stream order. `placement` maps the current
    /// coordinate space to page space: `pdfium` reports the children of a
    /// Form `XObject` in the form's own space and keeps the `cm`-time matrix
    /// on the form object (`FPDFPageObj_GetMatrix`), so it is composed here.
    fn visit(
        &mut self,
        objects: PdfPageObjectsIterator<'_>,
        text_page: &PdfPageText<'_>,
        placement: Option<PdfMatrix>,
        depth: u32,
    ) {
        for object in objects {
            match &object {
                PdfPageObject::Text(text) => self.add_text(text, text_page, placement),
                PdfPageObject::Image(image) => self.add_figure(image, placement),
                PdfPageObject::Path(_) => self.paths = self.paths.saturating_add(1),
                PdfPageObject::Shading(_) => self.shadings = self.shadings.saturating_add(1),
                PdfPageObject::Unsupported(_) => {
                    self.unsupported = self.unsupported.saturating_add(1);
                }
                PdfPageObject::XObjectForm(form) => {
                    if depth >= MAX_FORM_DEPTH {
                        self.warn(format!(
                            "form XObject nesting deeper than {MAX_FORM_DEPTH}; skipped"
                        ));
                    } else {
                        let local = if let Ok(matrix) = form.matrix() {
                            Some(matrix)
                        } else {
                            self.warn(
                                "form XObject: matrix unavailable; children left in form space"
                                    .to_string(),
                            );
                            None
                        };
                        self.visit(form.iter(), text_page, compose(local, placement), depth + 1);
                    }
                }
            }
        }
    }

    /// One [`Span`] per text object. The box is `pdfium`'s tight glyph bound
    /// widened to include the pen origin, so a run starts at its text
    /// position rather than at the first glyph's left bearing (matching the
    /// `lopdf` backend). Size is the `Tf` size scaled by the text matrix.
    fn add_text(
        &mut self,
        text: &PdfPageTextObject<'_>,
        text_page: &PdfPageText<'_>,
        placement: Option<PdfMatrix>,
    ) {
        let mut raw = text_page.for_object(text);
        if raw.contains('\u{0002}') {
            // Only objects containing the native marker need character-level
            // inspection. A control character alone is not recovery evidence.
            let cost = usize::try_from(text_page.len()).unwrap_or(usize::MAX);
            let can_inspect = cost <= self.hyphen_work_left;
            self.hyphen_work_left = self.hyphen_work_left.saturating_sub(cost);
            let confirmed = can_inspect
                .then(|| text_page.chars_for_object(text).ok())
                .flatten()
                .map(|chars| {
                    chars
                        .iter()
                        .filter(|ch| ch.unicode_value() == 2)
                        .map(|ch| ch.is_hyphen().unwrap_or(false))
                        .collect::<Vec<_>>()
                });
            if let Some(restored) = confirmed
                .as_deref()
                .and_then(|confirmed| restore_hyphen_markers(&raw, confirmed))
            {
                raw = restored;
            } else {
                self.warn(if can_inspect {
                    "unverified PDFium hyphen marker; original text retained".to_string()
                } else {
                    "resource_limit: PDFium hyphen inspection budget exhausted; original text retained".to_string()
                });
            }
        }
        let content: String = if raw.is_empty() {
            let glyphs = text_page
                .chars_for_object(text)
                .map_or(0, |chars| chars.len());
            if glyphs == 0 {
                return;
            }
            self.warn(
                "text object without extractable text (no ToUnicode?); U+FFFD per glyph"
                    .to_string(),
            );
            std::iter::repeat_n('\u{FFFD}', glyphs).collect()
        } else {
            clean_object_text(&raw)
        };
        if content.is_empty() {
            // Only text-layer separators: nothing to place.
            return;
        }

        let matrix = compose(text.matrix().ok(), placement);
        let scaled = text.unscaled_font_size().value * matrix.map_or(1.0, y_scale);
        let size = if scaled.is_finite() && scaled > 0.0 {
            Some(scaled)
        } else {
            None
        };

        let mut bbox = text
            .bounds()
            .ok()
            .map(|quad| to_bbox(place(quad.to_rect(), placement)));
        if let (Some(rect), Some(matrix)) = (bbox.as_mut(), matrix) {
            rect.x0 = rect.x0.min(matrix.e());
            rect.x1 = rect.x1.max(matrix.e());
            rect.y0 = rect.y0.min(matrix.f());
            rect.y1 = rect.y1.max(matrix.f());
        }

        self.page.spans.push(Span {
            text: content,
            bbox,
            font: font_name(text),
            size,
            seq: self.seq,
        });
        self.seq = self.seq.saturating_add(1);
    }

    /// One `raster` [`Figure`] per image object. Bytes are the stream data
    /// exactly as stored (`FPDFImageObj_GetImageDataRaw`): a complete JPEG or
    /// JPEG 2000 file when the stream uses that filter (then `mime` is set),
    /// otherwise opaque, possibly still compressed samples (`mime` is `None`).
    fn add_figure(&mut self, image: &PdfPageImageObject<'_>, placement: Option<PdfMatrix>) {
        let index = u32::try_from(self.page.figures.len()).unwrap_or(u32::MAX);
        let bbox = image
            .bounds()
            .ok()
            .map(|quad| to_bbox(place(quad.to_rect(), placement)));
        // Inherent `width`/`height` (pixels from the image metadata) shadow
        // the `PdfPageObjectCommon` methods of the same name (points).
        let width_px = image
            .width()
            .ok()
            .and_then(|value| u32::try_from(value).ok());
        let height_px = image
            .height()
            .ok()
            .and_then(|value| u32::try_from(value).ok());
        let bytes = image.get_raw_image_data().unwrap_or_default();
        let mime = sniff_mime(&bytes).map(str::to_string);
        let sha256 = if bytes.is_empty() {
            self.warn(format!("figure {index}: pdfium returned no image data"));
            None
        } else {
            let (digest, retained) = self.figures.retain(self.page.page, index, bytes);
            if !retained {
                self.page.warnings.push(format!(
                    "figure {index}: bytes not retained (cap of {} bytes reached)",
                    self.figures.cap
                ));
            }
            Some(digest)
        };
        self.page.figures.push(Figure {
            index,
            bbox,
            kind: "raster".to_string(),
            mime,
            width_px,
            height_px,
            sha256,
            file: None,
            caption: None,
        });
    }

    fn finish(mut self) -> PageText {
        let paths = self.paths;
        if paths > 0 {
            self.warn(format!("{paths} vector paths (not exported)"));
        }
        let shadings = self.shadings;
        if shadings > 0 {
            self.warn(format!("{shadings} shadings (not exported)"));
        }
        let unsupported = self.unsupported;
        if unsupported > 0 {
            self.warn(format!("{unsupported} unsupported page objects"));
        }
        self.page
    }
}

/// An object's text-layer string NFC-normalised, with the `\r`/`\n`
/// separators the `pdfium` text page inserts around runs of other objects
/// turned into one space (dropped at either end, and not doubled next to
/// an existing space). Every other character, spaces included, is kept as
/// reported. Pure ASCII without separators is returned unchanged.
fn clean_object_text(raw: &str) -> String {
    let has_break = raw.contains(['\r', '\n']);
    if !has_break {
        return raw.nfc().collect();
    }
    let mut out = String::with_capacity(raw.len());
    let mut pending_break = false;
    for ch in raw.nfc() {
        if ch == '\r' || ch == '\n' {
            pending_break = !out.is_empty();
        } else {
            if pending_break && ch != ' ' && !out.ends_with(' ') {
                out.push(' ');
            }
            pending_break = false;
            out.push(ch);
        }
    }
    out
}

/// Object text and its character evidence must agree before interpreting a
/// `PDFium` marker. Never delete a marker or an ordinary/soft hyphen here.
fn restore_hyphen_markers(raw: &str, confirmed: &[bool]) -> Option<String> {
    (raw.chars().filter(|&ch| ch == '\u{0002}').count() == confirmed.len()
        && confirmed.iter().all(|&hyphen| hyphen))
    .then(|| raw.replace('\u{0002}', "-"))
}

/// `/BaseFont` when `pdfium` knows it, else the substituted family name.
fn font_name(text: &PdfPageTextObject<'_>) -> Option<String> {
    let font = text.font();
    let base = font.name();
    let chosen = if base.is_empty() { font.family() } else { base };
    if chosen.is_empty() {
        None
    } else {
        Some(chosen)
    }
}

/// MIME type of a self-contained image file, judged from its signature.
fn sniff_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(&[0x00, 0x00, 0x00, 0x0C, 0x6A, 0x50, 0x20, 0x20]) {
        Some("image/jp2")
    } else if bytes.starts_with(&[0xFF, 0x4F, 0xFF, 0x51]) {
        Some("image/x-jp2-codestream")
    } else {
        None
    }
}

/// Row-vector composition: apply `first`, then `then`. `None` is the identity.
fn compose(first: Option<PdfMatrix>, then: Option<PdfMatrix>) -> Option<PdfMatrix> {
    match (first, then) {
        (Some(inner), Some(outer)) => Some(PdfMatrix::new(
            inner.a() * outer.a() + inner.b() * outer.c(),
            inner.a() * outer.b() + inner.b() * outer.d(),
            inner.c() * outer.a() + inner.d() * outer.c(),
            inner.c() * outer.b() + inner.d() * outer.d(),
            inner.e() * outer.a() + inner.f() * outer.c() + outer.e(),
            inner.e() * outer.b() + inner.f() * outer.d() + outer.f(),
        )),
        (Some(only), None) | (None, Some(only)) => Some(only),
        (None, None) => None,
    }
}

/// How one unit of text-space height maps to user space.
fn y_scale(matrix: PdfMatrix) -> f32 {
    (matrix.b() * matrix.b() + matrix.d() * matrix.d()).sqrt()
}

/// `rect` moved into page space by `placement` (identity when `None`).
fn place(rect: PdfRect, placement: Option<PdfMatrix>) -> PdfRect {
    placement.map_or(rect, |matrix| rect.transform(matrix))
}

fn to_bbox(rect: PdfRect) -> BBox {
    BBox {
        x0: rect.left().value,
        y0: rect.bottom().value,
        x1: rect.right().value,
        y1: rect.top().value,
    }
}

#[cfg(test)]
mod tests {
    use lopdf::content::{Content, Operation};
    use lopdf::{Dictionary, Document, Object, Stream, dictionary};

    use super::*;
    use crate::reading_order::group_lines;

    /// 2x2 8-bit gray samples of the test image.
    const IMAGE_SAMPLES: [u8; 4] = [0, 255, 128, 64];

    fn close(actual: f32, expected: f32, tolerance: f32) -> bool {
        (actual - expected).abs() <= tolerance
    }

    /// True when a `pdfium` library binds; otherwise says why so the caller
    /// can return early. The probe binding is dropped before returning so the
    /// session opened next does not block on it.
    fn pdfium_available() -> bool {
        match bind(None) {
            Ok(_) => true,
            Err(err) => {
                eprintln!("skipped: {err}");
                false
            }
        }
    }

    /// `BT /F1 size Tf x y Td (text) Tj ET`.
    fn text_ops(size: i32, x: i32, y: i32, text: &str) -> Vec<Operation> {
        vec![
            Operation::new("BT", vec![]),
            Operation::new("Tf", vec!["F1".into(), size.into()]),
            Operation::new("Td", vec![x.into(), y.into()]),
            Operation::new("Tj", vec![Object::string_literal(text)]),
            Operation::new("ET", vec![]),
        ]
    }

    /// `q a 0 0 d tx ty cm /name Do Q`.
    fn placed_xobject_ops(
        name: &str,
        scale_x: i32,
        scale_y: i32,
        tx: i32,
        ty: i32,
    ) -> Vec<Operation> {
        let matrix = vec![
            scale_x.into(),
            0.into(),
            0.into(),
            scale_y.into(),
            tx.into(),
            ty.into(),
        ];
        vec![
            Operation::new("q", vec![]),
            Operation::new("cm", matrix),
            Operation::new("Do", vec![name.into()]),
            Operation::new("Q", vec![]),
        ]
    }

    /// Build a PDF with Helvetica as `/F1`, one page per operation list, an
    /// optional Form `XObject` `/X1` holding `form`, and optionally a 2x2 gray
    /// image `XObject` `/Im1`. The Info dictionary carries a title.
    fn build_pdf(
        pages: Vec<Vec<Operation>>,
        form: Option<Vec<Operation>>,
        with_image: bool,
    ) -> Vec<u8> {
        let mut doc = Document::with_version("1.5");
        let tree_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
        });
        let mut xobjects = Dictionary::new();
        if let Some(operations) = form {
            let form_content = Content { operations }.encode().unwrap();
            let form_dict = dictionary! {
                "Type" => "XObject",
                "Subtype" => "Form",
                "BBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
                "Resources" => dictionary! { "Font" => dictionary! { "F1" => font_id } },
            };
            let form_id = doc.add_object(Stream::new(form_dict, form_content));
            xobjects.set("X1", form_id);
        }
        if with_image {
            let image_dict = dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => 2_i32,
                "Height" => 2_i32,
                "ColorSpace" => "DeviceGray",
                "BitsPerComponent" => 8_i32,
            };
            let image_id = doc.add_object(Stream::new(image_dict, IMAGE_SAMPLES.to_vec()));
            xobjects.set("Im1", image_id);
        }
        let resources_id = doc.add_object(dictionary! {
            "Font" => dictionary! { "F1" => font_id },
            "XObject" => xobjects,
        });
        let mut kids = Vec::new();
        for operations in pages {
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
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
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
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        bytes
    }

    #[test]
    fn identity_is_stable() {
        let backend = PdfiumBackend::default();
        assert_eq!(backend.library_dir, None);
        assert!(!backend.provides_reading_order());
        let identity = library_identity(None, None);
        assert_eq!(identity.name, "pdfium");
        assert_eq!(identity.version, "dynamic-binding-0.8.37");
        let mut config = BTreeMap::new();
        config.insert("library_path".to_string(), String::new());
        config.insert("library_sha256".to_string(), "unavailable".to_string());
        config.insert("unicode_mapping_policy".to_string(), "1".to_string());
        config.insert("text_policy".to_string(), TEXT_POLICY.to_string());
        assert_eq!(identity.config_digest, config_digest(&config));

        let configured = PdfiumBackend {
            library_dir: Some("/opt/pdfium".to_string()),
        };
        assert_ne!(configured.identity().config_digest, identity.config_digest);
    }

    #[test]
    fn effective_library_path_and_bytes_distinguish_run_identity() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first.so");
        let second = directory.path().join("second.so");
        std::fs::write(&first, b"first library").unwrap();
        std::fs::write(&second, b"second library").unwrap();
        let first_path = first.to_str().unwrap();
        let second_path = second.to_str().unwrap();
        let first_identity = library_identity(None, Some(first_path));
        let second_identity = library_identity(None, Some(second_path));
        assert_ne!(first_identity.config_digest, second_identity.config_digest);
        // Explicit configuration wins over the environment, without mutating
        // process-global variables in concurrently executing tests.
        assert_eq!(
            first_identity.config_digest,
            library_identity(Some(first_path), Some(second_path)).config_digest
        );
        assert_eq!(
            fingerprint_library(&first).unwrap(),
            sha256_hex(b"first library")
        );
        std::fs::write(&first, b"changed bytes at the same path").unwrap();
        assert_ne!(
            first_identity.config_digest,
            library_identity(None, Some(first_path)).config_digest
        );
    }

    #[test]
    fn oversized_library_is_rejected_before_fingerprinting_or_loading() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("oversized.so");
        File::create(&file)
            .unwrap()
            .set_len(MAX_LIBRARY_BYTES + 1)
            .unwrap();
        assert!(fingerprint_library(&file).is_err());
        let error = bind(file.to_str()).err().unwrap();
        assert!(error.to_string().contains("no larger than 64 MiB"));
    }

    #[test]
    fn configured_pdfium_path_must_be_absolute() {
        let err = configured_library_file(".pdfium/lib").unwrap_err();
        assert!(matches!(err, BackendError::Unsupported(_)));
        assert!(err.to_string().contains("must be absolute"));
    }

    #[test]
    fn pdfium_render_version_matches_cargo_lock() {
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
            if name == Some("pdfium-render") {
                locked = version;
            }
        }
        assert_eq!(
            locked,
            Some(PDFIUM_RENDER_VERSION),
            "Cargo.lock pins another pdfium-render; update PDFIUM_RENDER_VERSION (the identity key)"
        );
        let suffix = format!("-binding-{PDFIUM_RENDER_VERSION}");
        let version = version_string();
        assert!(version.ends_with(&suffix), "{version}");
    }

    #[test]
    fn two_page_pdf_yields_positioned_spans() {
        if !pdfium_available() {
            return;
        }
        let ops = vec![
            Operation::new("BT", vec![]),
            Operation::new("Tf", vec!["F1".into(), 12.into()]),
            Operation::new("Td", vec![100.into(), 600.into()]),
            Operation::new("Tj", vec![Object::string_literal("Hello")]),
            Operation::new("Tf", vec!["F1".into(), 24.into()]),
            Operation::new("Td", vec![0.into(), (-40).into()]),
            Operation::new("Tj", vec![Object::string_literal("World")]),
            Operation::new("ET", vec![]),
        ];
        let bytes = build_pdf(vec![ops, text_ops(10, 72, 700, "Page two")], None, false);

        // Gather everything, then assert: a panic inside `open` (while a
        // `Pdfium` is alive) would poison the crate's process-wide lock.
        let (count, first, second) = {
            let mut session = PdfiumBackend::default().open(&bytes, None).unwrap();
            let count = session.page_count();
            let first = session.page_text(1).unwrap();
            let second = session.page_text(2).unwrap();
            (count, first, second)
        };

        assert_eq!(count, 2);
        assert_eq!(first.page, 1);
        assert!(close(first.width, 612.0, 0.5), "width {}", first.width);
        assert!(close(first.height, 792.0, 0.5), "height {}", first.height);
        assert_eq!(first.rotation, 0);
        assert!(first.warnings.is_empty(), "{:?}", first.warnings);
        assert!(first.lines.is_empty());
        assert!(first.text.is_empty());
        assert!(first.figures.is_empty());

        let texts: Vec<&str> = first.spans.iter().map(|span| span.text.as_str()).collect();
        assert_eq!(texts, vec!["Hello", "World"]);

        let hello = &first.spans[0];
        let hello_box = hello.bbox.unwrap();
        assert!(close(hello_box.x0, 100.0, 1.0), "x0 {}", hello_box.x0);
        assert!(hello_box.x1 > hello_box.x0 + 20.0, "x1 {}", hello_box.x1);
        assert!(
            hello_box.y0 <= 600.5 && hello_box.y0 >= 596.0,
            "y0 {}",
            hello_box.y0
        );
        assert!(hello_box.y1 > 605.0, "y1 {}", hello_box.y1);
        assert!(
            close(hello.size.unwrap(), 12.0, 0.01),
            "size {:?}",
            hello.size
        );
        let font = hello.font.clone().unwrap_or_default();
        assert!(font.contains("Helvetica"), "font {font}");
        assert_eq!(hello.seq, 0);

        let world = &first.spans[1];
        assert!(
            close(world.size.unwrap(), 24.0, 0.01),
            "size {:?}",
            world.size
        );
        assert_eq!(world.seq, 1);

        assert_eq!(second.page, 2);
        assert_eq!(second.spans.len(), 1);
        assert_eq!(second.spans[0].text, "Page two");
        let second_box = second.spans[0].bbox.unwrap();
        assert!(close(second_box.x0, 72.0, 1.0), "x0 {}", second_box.x0);
        assert!(close(second.spans[0].size.unwrap(), 10.0, 0.01));
    }

    #[test]
    fn info_exposes_title() {
        if !pdfium_available() {
            return;
        }
        let bytes = build_pdf(vec![text_ops(12, 10, 10, "x")], None, false);
        let info = {
            let session = PdfiumBackend::default().open(&bytes, None).unwrap();
            session.info()
        };
        assert_eq!(info.get("Title").map(String::as_str), Some("Test Title"));
    }

    #[test]
    fn page_out_of_range_is_reported() {
        if !pdfium_available() {
            return;
        }
        let bytes = build_pdf(vec![text_ops(12, 10, 10, "x")], None, false);
        let (count, beyond, zero) = {
            let mut session = PdfiumBackend::default().open(&bytes, None).unwrap();
            let beyond = session.page_text(2).err();
            let zero = session.page_text(0).err();
            (session.page_count(), beyond, zero)
        };
        assert_eq!(count, 1);
        assert!(
            matches!(beyond, Some(BackendError::PageRange { page: 2, count: 1 })),
            "{beyond:?}"
        );
        assert!(
            matches!(zero, Some(BackendError::PageRange { page: 0, count: 1 })),
            "{zero:?}"
        );
    }

    #[test]
    fn native_page_count_beyond_binding_range_is_rejected() {
        if !pdfium_available() {
            return;
        }
        // pdfium-render 0.8.37 truncates FPDF_GetPageCount to u16. A real
        // 65537-page document must never become a successful one-page session.
        let bytes = build_pdf(vec![Vec::new(); usize::from(u16::MAX) + 2], None, false);
        let result = PdfiumBackend::default().open(&bytes, None).map(|_| ());
        assert!(
            matches!(&result, Err(BackendError::Limit(message)) if message.contains("65537")),
            "{result:?}"
        );
        // The rejected document closes before another library instance opens.
        let bytes = build_pdf(vec![text_ops(12, 10, 10, "next document")], None, false);
        assert_eq!(
            PdfiumBackend::default()
                .open(&bytes, None)
                .unwrap()
                .page_count(),
            1
        );
    }

    #[test]
    fn malformed_bytes_are_rejected() {
        if !pdfium_available() {
            return;
        }
        let outcome = PdfiumBackend::default()
            .open(b"not a pdf at all", None)
            .map(|_| ())
            .err();
        assert!(
            matches!(outcome, Some(BackendError::Malformed(_))),
            "{outcome:?}"
        );
    }

    #[test]
    fn image_becomes_figure_with_bytes_kept_out_of_text() {
        if !pdfium_available() {
            return;
        }
        let bytes = build_pdf(
            vec![placed_xobject_ops("Im1", 100, 50, 200, 300)],
            None,
            true,
        );
        let (page, first_take, second_take) = {
            let mut session = PdfiumBackend::default().open(&bytes, None).unwrap();
            let page = session.page_text(1).unwrap();
            let first_take = session.take_figure_bytes(1, 0);
            let second_take = session.take_figure_bytes(1, 0);
            (page, first_take, second_take)
        };

        assert!(page.spans.is_empty(), "{:?}", page.spans);
        assert_eq!(page.figures.len(), 1);
        let figure = &page.figures[0];
        assert_eq!(figure.index, 0);
        assert_eq!(figure.kind, "raster");
        assert_eq!(figure.mime, None);
        assert_eq!(figure.width_px, Some(2));
        assert_eq!(figure.height_px, Some(2));
        assert!(
            figure.sha256.is_some(),
            "the session records the stream digest"
        );
        assert_eq!(figure.file, None);
        let figure_box = figure.bbox.unwrap();
        assert!(close(figure_box.x0, 200.0, 0.5), "x0 {}", figure_box.x0);
        assert!(close(figure_box.y0, 300.0, 0.5), "y0 {}", figure_box.y0);
        assert!(close(figure_box.x1, 300.0, 0.5), "x1 {}", figure_box.x1);
        assert!(close(figure_box.y1, 350.0, 0.5), "y1 {}", figure_box.y1);
        assert_eq!(first_take, Some(IMAGE_SAMPLES.to_vec()));
        assert_eq!(second_take, None);
    }

    #[test]
    fn repeated_images_share_the_cap_and_release_after_the_last_take() {
        let mut store = FigureStore::new(IMAGE_SAMPLES.len());
        let expected = sha256_hex(&IMAGE_SAMPLES);
        // Reuse within a page and across pages, with the cap exactly full.
        for (page, index) in [(1, 0), (1, 1), (2, 0)] {
            assert_eq!(
                store.retain(page, index, IMAGE_SAMPLES.to_vec()),
                (expected.clone(), true)
            );
        }
        assert_eq!(store.blobs.len(), 1);
        assert_eq!(store.retained, IMAGE_SAMPLES.len());
        let retained_pointer = store.blobs[&expected].bytes.as_ptr();
        // Requests need not follow page order; each occurrence is single-use.
        assert_eq!(store.take(2, 0), Some(IMAGE_SAMPLES.to_vec()));
        assert_eq!(store.take(2, 0), None);
        assert_eq!(store.take(1, 0), Some(IMAGE_SAMPLES.to_vec()));
        assert_eq!(store.retained, IMAGE_SAMPLES.len());
        let last = store.take(1, 1).unwrap();
        assert_eq!(last, IMAGE_SAMPLES);
        assert_eq!(
            last.as_ptr(),
            retained_pointer,
            "final take must move, not copy"
        );
        assert!(store.blobs.is_empty());
        assert!(store.keys.is_empty());
        assert_eq!(store.retained, 0);
    }

    #[test]
    fn image_cap_rejects_distinct_overflow_without_losing_retained_duplicates() {
        let mut store = FigureStore::new(4);
        assert!(store.retain(1, 0, vec![1, 2, 3]).1);
        // Distinct payload over the remaining budget; digest is still available.
        let rejected = vec![4, 5];
        assert_eq!(
            store.retain(1, 1, rejected.clone()),
            (sha256_hex(&rejected), false)
        );
        assert!(store.retain(1, 2, vec![6]).1, "exact cap must fit");
        assert!(store.retain(2, 0, vec![1, 2, 3]).1);
        assert_eq!(store.retained, 4);
        assert_eq!(store.blobs.len(), 2);
        assert_eq!(store.take(1, 1), None);
        assert_eq!(store.take(1, 0), Some(vec![1, 2, 3]));
        assert_eq!(store.take(2, 0), Some(vec![1, 2, 3]));
        assert_eq!(store.take(1, 2), Some(vec![6]));
        assert_eq!(store.retained, 0);
    }

    #[test]
    fn oversized_image_and_zero_cap_keep_no_payload() {
        for cap in [0, IMAGE_SAMPLES.len() - 1] {
            let mut store = FigureStore::new(cap);
            assert_eq!(
                store.retain(1, 0, IMAGE_SAMPLES.to_vec()),
                (sha256_hex(&IMAGE_SAMPLES), false)
            );
            assert!(store.blobs.is_empty());
            assert!(store.keys.is_empty());
            assert_eq!(store.retained, 0);
        }
    }

    #[test]
    fn repeated_image_pdf_retains_one_blob_across_and_within_pages() {
        if !pdfium_available() {
            return;
        }
        let mut ops = placed_xobject_ops("Im1", 100, 50, 200, 300);
        ops.extend(placed_xobject_ops("Im1", 50, 25, 20, 30));
        let bytes = build_pdf(vec![ops.clone(), ops], None, true);
        let mut session = PdfiumBackend::default()
            .open_session(&bytes, None, IMAGE_SAMPLES.len())
            .unwrap();
        assert_eq!(session.figures.blobs.len(), 1);
        assert_eq!(session.figures.retained, IMAGE_SAMPLES.len());
        for page in 1..=2 {
            let extracted = session.page_text(page).unwrap();
            assert!(extracted.warnings.is_empty(), "{:?}", extracted.warnings);
            assert_eq!(extracted.figures.len(), 2);
            for figure in &extracted.figures {
                assert_eq!(figure.sha256, Some(sha256_hex(&IMAGE_SAMPLES)));
                assert_eq!(
                    session.take_figure_bytes(page, figure.index),
                    Some(IMAGE_SAMPLES.to_vec())
                );
                assert_eq!(session.take_figure_bytes(page, figure.index), None);
            }
        }
        assert_eq!(session.figures.retained, 0);
        assert!(session.figures.blobs.is_empty());
    }

    #[test]
    fn capped_image_pdf_preserves_geometry_digest_and_text_with_warning() {
        if !pdfium_available() {
            return;
        }
        let mut ops = text_ops(12, 72, 700, "Text survives the image cap");
        ops.extend(placed_xobject_ops("Im1", 100, 50, 200, 300));
        let bytes = build_pdf(vec![ops], None, true);
        let backend = PdfiumBackend::default();
        let expected = backend
            .open_session(&bytes, None, IMAGE_SAMPLES.len())
            .unwrap()
            .page_text(1)
            .unwrap();
        let mut capped = backend
            .open_session(&bytes, None, IMAGE_SAMPLES.len() - 1)
            .unwrap();
        let actual = capped.page_text(1).unwrap();
        assert_eq!(actual.spans, expected.spans);
        assert_eq!(actual.figures, expected.figures);
        assert_eq!(actual.figures[0].sha256, Some(sha256_hex(&IMAGE_SAMPLES)));
        assert_eq!(
            actual.warnings,
            vec!["figure 0: bytes not retained (cap of 3 bytes reached)"]
        );
        assert_eq!(capped.take_figure_bytes(1, 0), None);
        assert!(capped.figures.blobs.is_empty());
        assert_eq!(capped.figures.retained, 0);
    }

    #[test]
    fn form_xobject_children_are_placed_on_the_page() {
        if !pdfium_available() {
            return;
        }
        let ops = placed_xobject_ops("X1", 1, 1, 200, 300);
        let form = text_ops(10, 50, 50, "Form");
        let bytes = build_pdf(vec![ops], Some(form), false);
        let page = {
            let mut session = PdfiumBackend::default().open(&bytes, None).unwrap();
            session.page_text(1).unwrap()
        };
        assert!(page.warnings.is_empty(), "{:?}", page.warnings);
        assert_eq!(page.spans.len(), 1);
        assert_eq!(page.spans[0].text, "Form");
        let form_box = page.spans[0].bbox.unwrap();
        assert!(close(form_box.x0, 250.0, 1.5), "x0 {}", form_box.x0);
        assert!(
            form_box.y0 <= 350.5 && form_box.y0 >= 345.0,
            "y0 {}",
            form_box.y0
        );
        assert!(form_box.y1 > 355.0, "y1 {}", form_box.y1);
        assert!(close(page.spans[0].size.unwrap(), 10.0, 0.01));
    }

    #[test]
    fn adjacent_text_objects_on_one_baseline_form_one_line() {
        if !pdfium_available() {
            return;
        }
        // Three `Tj` objects on one baseline in 12 pt Helvetica: "lo" starts
        // exactly where the advance of "Hel" (18 pt) ends, and "world"
        // follows the advance of "lo" (9.3 pt) after a word-sized gap.
        let ops = vec![
            Operation::new("BT", vec![]),
            Operation::new("Tf", vec!["F1".into(), 12.into()]),
            Operation::new("Td", vec![100.into(), 600.into()]),
            Operation::new("Tj", vec![Object::string_literal("Hel")]),
            Operation::new("Td", vec![18.into(), 0.into()]),
            Operation::new("Tj", vec![Object::string_literal("lo")]),
            Operation::new("Td", vec![14.into(), 0.into()]),
            Operation::new("Tj", vec![Object::string_literal("world")]),
            Operation::new("ET", vec![]),
        ];
        let bytes = build_pdf(vec![ops], None, false);
        let page = {
            let mut session = PdfiumBackend::default().open(&bytes, None).unwrap();
            session.page_text(1).unwrap()
        };

        // One span per text object, in content-stream order; the text layer
        // may attach a boundary space, never other objects' glyphs.
        let texts: Vec<&str> = page.spans.iter().map(|span| span.text.trim()).collect();
        assert_eq!(texts, vec!["Hel", "lo", "world"]);
        let boxes: Vec<BBox> = page
            .spans
            .iter()
            .map(|span| span.bbox.expect("positioned"))
            .collect();
        assert!(close(boxes[0].x0, 100.0, 1.0), "x0 {}", boxes[0].x0);
        assert!(close(boxes[1].x0, 118.0, 1.0), "x0 {}", boxes[1].x0);
        assert!(close(boxes[2].x0, 132.0, 1.0), "x0 {}", boxes[2].x0);
        for (span, bbox) in page.spans.iter().zip(&boxes) {
            assert!(close(span.size.unwrap(), 12.0, 0.01), "{:?}", span.size);
            assert!(close(bbox.y0, 600.0, 0.5), "baseline {}", bbox.y0);
        }

        // The engine joins the touching pieces without a space and inserts
        // one across the word gap, exactly as it does for lopdf's pieces.
        let lines = group_lines(&page.spans);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0].text, "Hello world");
        assert_eq!(lines[0].spans, vec![0, 1, 2]);
    }

    /// Copy of `Turn::point` in `reading_order.rs` (private there): the
    /// user-space point `(x, y)` of an unrotated `width` x `height` page in
    /// the frame turned clockwise by `degrees`, or back with `back`.
    fn turn_point(
        degrees: i32,
        width: f32,
        height: f32,
        point: (f32, f32),
        back: bool,
    ) -> (f32, f32) {
        let (x, y) = point;
        match (degrees, back) {
            (90, false) => (y, width - x),
            (90, true) => (width - y, x),
            (270, false) => (height - y, x),
            (270, true) => (y, height - x),
            (180, _) => (width - x, height - y),
            _ => (x, y),
        }
    }

    #[test]
    fn unrotated_size_matches_the_reading_order_turn() {
        // A portrait 612 x 792 page as `pdfium` reports it when displayed.
        for (degrees, display_width, display_height) in [
            (0, 612.0, 792.0),
            (90, 792.0, 612.0),
            (180, 612.0, 792.0),
            (270, 792.0, 612.0),
        ] {
            let (width, height) = unrotated_size(display_width, display_height, degrees);
            assert!(close(width, 612.0, 1e-3), "{degrees}: width {width}");
            assert!(close(height, 792.0, 1e-3), "{degrees}: height {height}");
            // Near the top-left corner of the unrotated page.
            let point = (72.0_f32, 700.0_f32);
            let shown = turn_point(degrees, width, height, point, false);
            assert!(
                (0.0..=display_width).contains(&shown.0)
                    && (0.0..=display_height).contains(&shown.1),
                "{degrees}: {shown:?} outside {display_width} x {display_height}"
            );
            let back = turn_point(degrees, width, height, shown, true);
            assert!(
                close(back.0, point.0, 1e-3) && close(back.1, point.1, 1e-3),
                "{degrees}: {back:?}"
            );
        }
        // A quarter turn clockwise moves the top edge to the right and the
        // left edge to the top: (x, y) shows at (y, width - x).
        let shown = turn_point(90, 612.0, 792.0, (72.0, 700.0), false);
        assert!(close(shown.0, 700.0, 1e-3) && close(shown.1, 540.0, 1e-3));
    }

    #[test]
    fn rotated_page_reports_unrotated_geometry() {
        if !pdfium_available() {
            return;
        }
        let plain = build_pdf(vec![text_ops(10, 72, 700, "Turned")], None, false);
        let mut doc = Document::load_mem(&plain).unwrap();
        for object in doc.objects.values_mut() {
            if let Ok(dict) = object.as_dict_mut()
                && dict.get(b"Type").and_then(Object::as_name).ok() == Some(b"Page".as_slice())
            {
                dict.set("Rotate", 90_i64);
            }
        }
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();

        let page = {
            let mut session = PdfiumBackend::default().open(&bytes, None).unwrap();
            session.page_text(1).unwrap()
        };
        assert_eq!(page.rotation, 90);
        // Unrotated MediaBox, as the lopdf backend reports it.
        assert!(close(page.width, 612.0, 0.5), "width {}", page.width);
        assert!(close(page.height, 792.0, 0.5), "height {}", page.height);
        assert_eq!(page.spans.len(), 1);
        // User-space box: displayed-frame bounds would put x0 near 700 and
        // y0 near 540 instead.
        let bbox = page.spans[0].bbox.unwrap();
        assert!(close(bbox.x0, 72.0, 1.0), "box {bbox:?}");
        assert!(bbox.y0 <= 700.5 && bbox.y0 >= 696.0, "box {bbox:?}");
    }

    #[test]
    fn text_layer_separators_become_single_spaces() {
        assert_eq!(clean_object_text("plain"), "plain");
        assert_eq!(clean_object_text("\r\nHello\r\nworld\r\n"), "Hello world");
        assert_eq!(clean_object_text("a \r\nb"), "a b");
        assert_eq!(clean_object_text("a\r\n b"), "a b");
        assert_eq!(clean_object_text("\n"), "");
        assert_eq!(clean_object_text("keep  spaces "), "keep  spaces ");
        // NFC composes a combining acute onto its base.
        assert_eq!(clean_object_text("e\u{0301}\r\nx"), "\u{00E9} x");
    }

    #[test]
    fn hyphen_markers_require_native_evidence_and_preserve_other_hyphens() {
        let raw = "Krishna\u{0002}\r\nmurthy and Processing-in-Memory soft\u{00ad}hyphen";
        assert_eq!(
            restore_hyphen_markers(raw, &[true]).as_deref(),
            Some("Krishna-\r\nmurthy and Processing-in-Memory soft\u{00ad}hyphen")
        );
        assert!(restore_hyphen_markers(raw, &[false]).is_none());
        assert!(restore_hyphen_markers(raw, &[]).is_none());
        assert!(restore_hyphen_markers(raw, &[true, true]).is_none());
        assert!(restore_hyphen_markers("a\u{0002}b\u{0002}c", &[true, false]).is_none());
    }

    #[test]
    fn native_line_end_hyphens_survive_as_spans_and_reference_fields() {
        if !pdfium_available() {
            return;
        }
        let mut ops = text_ops(12, 45, 740, "References");
        for (y, line) in [
            (
                700,
                "[1] T. Chen, L. Zheng, E. Yan, Z. Jiang, T. Moreau, L. Ceze, C. Guestrin, and A. Krishna-",
            ),
            (
                690,
                "murthy, \"Learning to Optimize Tensor Programs,\" NeurIPS, 2018.",
            ),
            (
                660,
                "[2] S. Nakandala, K. Saur, G.-I. Yu, K. Karanasos, C. Curino, M. Weimer, and M. Inter-",
            ),
            (
                650,
                "landi, \"A Tensor Compiler for Unified Machine Learning Prediction Serving,\" in OSDI, 2020.",
            ),
            (620, "[3] A. Smith, \"Processing-in-Memory and self-"),
            (610, "attention for task-mapping,\" NeurIPS, 2020."),
        ] {
            ops.extend(text_ops(8, 45, y, line));
        }
        let bytes = build_pdf(vec![ops], None, false);
        // Assert that this fixture actually exercises PDFium's special marker.
        let native_markers = {
            let pdfium = bind(None).unwrap();
            let doc = load(&pdfium, &bytes, None).unwrap();
            let page = doc.pages().get(0).unwrap();
            let text = page.text().unwrap();
            text.chars()
                .iter()
                .filter(|ch| ch.unicode_value() == 2 && ch.is_hyphen().unwrap_or(false))
                .count()
        };
        assert_eq!(
            native_markers, 3,
            "all three line-end hyphens exercise the marker"
        );
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("references.pdf");
        std::fs::write(&path, bytes).unwrap();
        let job = crate::schema::Job {
            path: path.to_string_lossy().into_owned(),
            backend: "pdfium".into(),
            pages: None,
            password: None,
            max_bytes: None,
            figures_dir: None,
        };
        let result = crate::pipeline::run_job_with(&PdfiumBackend::default(), &job).unwrap();
        assert!(
            result.pages[0]
                .spans
                .iter()
                .any(|span| span.text.ends_with("Krishna-"))
        );
        assert!(!result.pages[0].text.contains('\u{0002}'));
        for (index, name, title) in [
            (
                1,
                "A. Krishnamurthy",
                "Learning to Optimize Tensor Programs",
            ),
            (
                2,
                "M. Interlandi",
                "A Tensor Compiler for Unified Machine Learning Prediction Serving",
            ),
            (
                3,
                "A. Smith",
                "Processing-in-Memory and self-attention for task-mapping",
            ),
        ] {
            let entry = result
                .references
                .iter()
                .find(|entry| entry.label.as_deref() == Some(format!("[{index}]").as_str()))
                .unwrap_or_else(|| panic!("missing [{index}]: {:?}", result.references));
            assert!(entry.raw.contains(name), "{}", entry.raw);
            assert_eq!(entry.title.as_deref(), Some(title));
            assert!(
                entry.authors.iter().any(|author| author == name),
                "{:?}",
                entry.authors
            );
        }
    }

    #[test]
    fn hyphen_inspection_exhaustion_keeps_text_and_marks_partial() {
        if !pdfium_available() {
            return;
        }
        let mut ops = text_ops(12, 45, 700, "Krishna-");
        ops.extend(text_ops(12, 45, 685, "murthy"));
        ops.extend(text_ops(12, 45, 650, "self-"));
        ops.extend(text_ops(12, 45, 635, "attention"));
        let bytes = build_pdf(vec![ops], None, false);
        let page = {
            let pdfium = bind(None).unwrap();
            let doc = load(&pdfium, &bytes, None).unwrap();
            let pdf_page = doc.pages().get(0).unwrap();
            let text_page = pdf_page.text().unwrap();
            let mut figures = FigureStore::new(0);
            let mut collector = Collector {
                page: PageText::new(1, 612.0, 792.0, 0),
                figures: &mut figures,
                seq: 0,
                paths: 0,
                shadings: 0,
                unsupported: 0,
                // Allow exactly one marked object; the second must retain its
                // original text instead of adding an unbounded page scan.
                hyphen_work_left: usize::try_from(text_page.len()).unwrap(),
            };
            collector.visit(pdf_page.objects().iter(), &text_page, None, 0);
            collector.finish()
        };
        let texts: Vec<_> = page.spans.iter().map(|span| span.text.as_str()).collect();
        assert_eq!(texts, ["Krishna-", "murthy", "self\u{0002}", "attention"]);
        assert_eq!(page.extraction_status(), crate::schema::Status::Partial);
        assert_eq!(
            page.warnings,
            ["resource_limit: PDFium hyphen inspection budget exhausted; original text retained"]
        );
    }

    #[test]
    fn mime_is_sniffed_from_signatures() {
        assert_eq!(
            sniff_mime(&[0xFF, 0xD8, 0xFF, 0xE0, 0x00]),
            Some("image/jpeg")
        );
        assert_eq!(
            sniff_mime(&[0x00, 0x00, 0x00, 0x0C, 0x6A, 0x50, 0x20, 0x20, 0x0D]),
            Some("image/jp2")
        );
        assert_eq!(
            sniff_mime(&[0xFF, 0x4F, 0xFF, 0x51, 0x00]),
            Some("image/x-jp2-codestream")
        );
        assert_eq!(sniff_mime(&IMAGE_SAMPLES), None);
        assert_eq!(sniff_mime(&[]), None);
    }

    #[test]
    fn matrix_composition_applies_first_then_second() {
        let translate = PdfMatrix::new(1.0, 0.0, 0.0, 1.0, 200.0, 300.0);
        let scale = PdfMatrix::new(2.0, 0.0, 0.0, 3.0, 0.0, 0.0);
        // Scale first, then translate: (1, 1) -> (2, 3) -> (202, 303).
        let composed = compose(Some(scale), Some(translate)).unwrap();
        assert!(close(composed.e(), 200.0, 1e-6));
        assert!(close(composed.f(), 300.0, 1e-6));
        assert!(close(composed.a(), 2.0, 1e-6));
        assert!(close(composed.d(), 3.0, 1e-6));
        // Translate first, then scale: (1, 1) -> (201, 301) -> (402, 903).
        let reversed = compose(Some(translate), Some(scale)).unwrap();
        assert!(close(reversed.e(), 400.0, 1e-6));
        assert!(close(reversed.f(), 900.0, 1e-6));
        assert!(compose(None, Some(scale)).is_some());
        assert!(compose(None, None).is_none());
        assert!(close(y_scale(scale), 3.0, 1e-6));
    }
}
