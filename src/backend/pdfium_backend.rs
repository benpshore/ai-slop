//! Native backend built on `pdfium-render` 0.8 (Google's `pdfium` loaded as a
//! dynamic library). `pdfium` interprets the content streams for us and hands
//! back positioned page objects; this module turns text objects into
//! [`Span`]s and image objects into [`Figure`]s. Nothing is ordered or
//! repaired here, and no pixel data ever reaches a span.
//!
//! # Binding and threading
//!
//! `Pdfium` is neither `Send` nor `Sync`: with the crate's default
//! `thread_safe` feature its bindings hold a `RefCell<Option<MutexGuard>>`,
//! and the crate only adds `unsafe impl Send + Sync` under its non-default
//! `sync` feature. Worse for a per-thread cache, that `thread_safe` wrapper
//! takes a process-wide mutex in `FPDF_InitLibrary` and releases it in
//! `FPDF_DestroyLibrary`, i.e. for the whole life of a `Pdfium` value, so a
//! second `Pdfium::new` on another thread blocks until the first is dropped.
//! A `thread_local!` cache would therefore stall every worker but one.
//!
//! Instead each [`DocumentSession`] owns its own `Pdfium` (one `dlopen` and
//! `FPDF_InitLibrary` per opened document) and holds `SESSION_GATE` for its
//! lifetime, which serialises `pdfium` use across worker threads whatever the
//! crate features are. Consequence: a thread must drop one `pdfium` session
//! before opening another, or the second `open` blocks.
//!
//! # Document ownership
//!
//! `PdfDocument<'a>` borrows both the `Pdfium` and the byte slice it was
//! loaded from, so it cannot live next to its owners without a
//! self-referential struct. The session keeps the bytes and re-opens the
//! document from memory on every [`DocumentSession::page_text`] call.
//! `FPDF_LoadMemDocument64` parses only the trailer and xref (pages are parsed
//! on demand anyway), so the per-call cost is small; measure before
//! optimising this.
//!
//! # Library compatibility
//!
//! Dynamic binding resolves every `FPDF_*` symbol of the compiled-in API
//! version (`pdfium_latest`) at bind time, so the `libpdfium` found must be at
//! least that recent. The pinned `chromium/8066` build is.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use pdfium_render::prelude::{
    PdfDocument, PdfDocumentMetadataTagType, PdfMatrix, PdfPageImageObject, PdfPageObject,
    PdfPageObjectCommon, PdfPageObjectsCommon, PdfPageObjectsIterator, PdfPageRenderRotation,
    PdfPageText, PdfPageTextObject, PdfRect, Pdfium, PdfiumError, PdfiumInternalError,
};
use unicode_normalization::UnicodeNormalization;

use crate::backend::{BackendError, DocumentSession, EncryptionProblem, Extractor};
use crate::schema::{BBox, BackendIdentity, Figure, PageText, Span, config_digest};

/// The `pdfium` binary release the native CI leg pins. `pdfium` has no
/// runtime version call (`pdfium-render`'s `version()` only echoes the
/// compile-time API selection), so this is a constant.
const PDFIUM_BINARY_VERSION: &str = "chromium/8066";
/// The `pdfium-render` release this backend is written against; a unit test
/// ties it to `Cargo.lock`.
const PDFIUM_RENDER_VERSION: &str = "0.8.37";
/// Environment variable naming the library directory or file.
const ENV_LIBRARY_PATH: &str = "PDFIUM_DYNAMIC_LIB_PATH";
/// Directory tried after the environment variable.
const DEFAULT_LIBRARY_DIR: &str = ".pdfium/lib";
/// Bound on nested Form `XObject` traversal.
const MAX_FORM_DEPTH: u32 = 8;

/// Process-wide serialisation of `pdfium` use; see the module docs.
static SESSION_GATE: Mutex<()> = Mutex::new(());

fn acquire_gate() -> MutexGuard<'static, ()> {
    SESSION_GATE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Version recorded in the [`BackendIdentity`]:
/// `<binary release>-binding-<pdfium-render release>`.
fn version_string() -> String {
    format!("{PDFIUM_BINARY_VERSION}-binding-{PDFIUM_RENDER_VERSION}")
}

/// The `pdfium` extractor.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PdfiumBackend {
    /// Directory holding `libpdfium.so` / `libpdfium.dylib` / `pdfium.dll`.
    /// `None` tries `$PDFIUM_DYNAMIC_LIB_PATH` (a directory or the file
    /// itself), then `.pdfium/lib`, then the system library search path.
    pub library_dir: Option<String>,
}

impl Extractor for PdfiumBackend {
    /// Name `pdfium`, version from [`version_string`], digest over `library_dir`.
    fn identity(&self) -> BackendIdentity {
        let mut config = BTreeMap::new();
        config.insert(
            "library_dir".to_string(),
            self.library_dir.clone().unwrap_or_default(),
        );
        BackendIdentity {
            name: "pdfium".to_string(),
            version: version_string(),
            config_digest: config_digest(&config),
        }
    }

    /// Geometry only; the engine's XY-cut orders the spans.
    fn provides_reading_order(&self) -> bool {
        false
    }

    /// Bind the library, load the bytes once to validate them, count pages
    /// and read `/Info`, then keep bytes and binding for per-page re-opens.
    /// Blocks while another session is alive (see the module docs).
    fn open(
        &self,
        bytes: &[u8],
        password: Option<&str>,
    ) -> Result<Box<dyn DocumentSession>, BackendError> {
        let gate = acquire_gate();
        let pdfium = bind(self.library_dir.as_deref())?;
        let owned: Vec<u8> = bytes.to_vec();
        let (page_count, info) = {
            let doc = load(&pdfium, &owned, password)?;
            (u32::from(doc.pages().len()), read_info(&doc))
        };
        Ok(Box::new(PdfiumSession {
            pdfium,
            bytes: owned,
            password: password.map(str::to_owned),
            page_count,
            info,
            figure_bytes: HashMap::new(),
            _gate: gate,
        }))
    }
}

/// Bind `libpdfium` following the search order documented on
/// [`PdfiumBackend::library_dir`]. An explicit directory is strict: the
/// system fallback is only tried when nothing was configured.
fn bind(library_dir: Option<&str>) -> Result<Pdfium, BackendError> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(dir) = library_dir {
        candidates.push(library_file(Path::new(dir)));
    } else {
        if let Ok(configured) = std::env::var(ENV_LIBRARY_PATH)
            && !configured.is_empty()
        {
            candidates.push(library_file(Path::new(&configured)));
        }
        candidates.push(library_file(Path::new(DEFAULT_LIBRARY_DIR)));
    }
    let mut failures: Vec<String> = Vec::new();
    for candidate in &candidates {
        match Pdfium::bind_to_library(candidate) {
            Ok(bindings) => return Ok(Pdfium::new(bindings)),
            Err(err) => failures.push(format!("{}: {err:?}", candidate.display())),
        }
    }
    if library_dir.is_none() {
        match Pdfium::bind_to_system_library() {
            Ok(bindings) => return Ok(Pdfium::new(bindings)),
            Err(err) => failures.push(format!("system library: {err:?}")),
        }
    }
    let detail = failures.join("; ");
    let hint = format!("set {ENV_LIBRARY_PATH} or place it under {DEFAULT_LIBRARY_DIR}");
    Err(BackendError::Unsupported(format!(
        "pdfium library not found: {hint} ({detail})"
    )))
}

/// `path` itself when it names a file, else the platform library file name
/// inside that directory.
fn library_file(path: &Path) -> PathBuf {
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

struct PdfiumSession {
    pdfium: Pdfium,
    bytes: Vec<u8>,
    password: Option<String>,
    page_count: u32,
    info: BTreeMap<String, String>,
    /// Raw image bytes keyed by `(page, figure index)`, handed out once.
    figure_bytes: HashMap<(u32, u32), Vec<u8>>,
    /// Declared last so it is released after `pdfium` has been dropped
    /// (fields drop in declaration order).
    _gate: MutexGuard<'static, ()>,
}

impl DocumentSession for PdfiumSession {
    fn page_count(&self) -> u32 {
        self.page_count
    }

    fn page_text(&mut self, page: u32) -> Result<PageText, BackendError> {
        let count = self.page_count;
        if page == 0 || page > count {
            return Err(BackendError::PageRange { page, count });
        }
        let Ok(index) = u16::try_from(page - 1) else {
            return Err(BackendError::PageRange { page, count });
        };
        let extracted = {
            let doc = load(&self.pdfium, &self.bytes, self.password.as_deref())
                .map_err(|err| page_error(page, format!("reopen: {err}")))?;
            extract_page(&doc, page, index)?
        };
        for (figure_index, bytes) in extracted.figures {
            self.figure_bytes.insert((page, figure_index), bytes);
        }
        Ok(extracted.page)
    }

    fn info(&self) -> BTreeMap<String, String> {
        self.info.clone()
    }

    fn take_figure_bytes(&mut self, page: u32, index: u32) -> Option<Vec<u8>> {
        self.figure_bytes.remove(&(page, index))
    }
}

fn page_error(page: u32, message: String) -> BackendError {
    BackendError::Page { page, message }
}

/// One page's spans and figures plus the raw bytes behind the figures.
struct Extracted {
    page: PageText,
    figures: Vec<(u32, Vec<u8>)>,
}

fn extract_page(doc: &PdfDocument<'_>, page: u32, index: u16) -> Result<Extracted, BackendError> {
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
    let mut collector = Collector {
        page: PageText::new(
            page,
            pdf_page.width().value,
            pdf_page.height().value,
            rotation,
        ),
        figures: Vec::new(),
        seq: 0,
        paths: 0,
        shadings: 0,
        unsupported: 0,
    };
    collector.visit(pdf_page.objects().iter(), &text_page, None, 0);
    Ok(collector.finish())
}

/// Accumulates spans, figures and object counts while walking a page.
struct Collector {
    page: PageText,
    figures: Vec<(u32, Vec<u8>)>,
    seq: u32,
    paths: u32,
    shadings: u32,
    unsupported: u32,
}

impl Collector {
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
        let raw = text_page.for_object(text);
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
            raw.nfc().collect()
        };

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
        if bytes.is_empty() {
            self.warn(format!("figure {index}: pdfium returned no image data"));
        } else {
            self.figures.push((index, bytes));
        }
        self.page.figures.push(Figure {
            index,
            bbox,
            kind: "raster".to_string(),
            mime,
            width_px,
            height_px,
            sha256: None,
            file: None,
            caption: None,
        });
    }

    fn finish(mut self) -> Extracted {
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
        Extracted {
            page: self.page,
            figures: self.figures,
        }
    }
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
        let identity = backend.identity();
        assert_eq!(identity.name, "pdfium");
        assert_eq!(identity.version, "chromium/8066-binding-0.8.37");
        let mut config = BTreeMap::new();
        config.insert("library_dir".to_string(), String::new());
        assert_eq!(identity.config_digest, config_digest(&config));

        let configured = PdfiumBackend {
            library_dir: Some("/opt/pdfium".to_string()),
        };
        assert_ne!(configured.identity().config_digest, identity.config_digest);
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

        // Gather everything, drop the session, then assert: a panic while the
        // session is alive would poison the process-wide pdfium locks.
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
        assert_eq!(figure.sha256, None);
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
