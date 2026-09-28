# Batch 3 contract: native backends (docling.rs, PDFium), figures, OCR, native CI

Same HARD RULES as CONTRACT.md (no cargo/rustc/compilers, no network, no git; touch only your files; clippy pedantic + rustfmt clean;
read APIs from offline crate sources under scratchpad/crates/: docling-pdf-1.69.2, docling-core-1.69.2, docling-onnx-1.69.2, docling-1.69.2 (README),
pdfium-render-0.8.37, ort-2.0.0-rc.13, lopdf-0.45.0). The engine crate `tpe` (src/) is CI-green on main/feat branches; read src/schema.rs,
src/backend/mod.rs, src/backend/lopdf_backend.rs (a complete Extractor implementation to mirror), src/pipeline.rs, src/reading_order.rs.

Product rules (Ben): text output must contain NO image blobs, NO base64, NO docling markup; images are exported separately as files and recorded as
`schema::Figure` records (already in src/schema.rs: PageText.figures, Figure{index,bbox,kind,mime,width_px,height_px,sha256,file,caption});
DOIs and reference text must survive untouched; scanned PDFs are in scope via OCR.

Facts verified from the sources (2026-09-28):
- docling-pdf 1.69.2: `pub fn convert_text_layer_pages(bytes, name, pages: Option<(usize,usize)>) -> Result<DoclingDocument, PdfError>` needs NO models and
  NO pdfium (pure Rust lopdf text parser). `Pipeline::new() -> Result<Pipeline, PdfError>`, builder `.pages(Option<(usize,usize)>)`, `.no_ocr(bool)`,
  `.no_table_former(bool)`, `.ocr_lang/.ocr_engine/.ocr_mode/.force_full_page_ocr`, `.warm_up()`, `convert(&mut self, bytes, password: Option<&str>, name) ->
  Result<DoclingDocument, PdfError>`, `convert_streaming(&mut self, bytes, password, name, emit: FnMut(Vec<Node>, Vec<(String,String)>) -> Result<(),PdfError>)`.
  `pub fn page_count(bytes, password) -> Result<usize, PdfError>` (pdfium). `PdfError::{Pdfium(String), Layout(String), Ocr(String)}`.
  Models resolve from `.models/…` relative to CWD, or `$DOCLING_RS_MODELS_DIR`, or next to the executable (docling-core assets.rs). pdfium binds from
  `$PDFIUM_DYNAMIC_LIB_PATH` (dir or file), else `.pdfium/lib`, else system (docling-pdf pdfium_backend.rs `bind()`). Missing pdfium surfaces as
  `PdfError::Pdfium("the pdfium library is not installed…")`.
- docling-core 1.69.2 `Node` enum (document.rs): Heading{level,text}, Paragraph{text}, ListItem{…text…}, Code{…}, Table(Table), Picture{caption, image: Option<PictureImage{mimetype,width,height,data: Vec<u8>}>,…},
  Formula{…}, Caption{text,href}, Located{location:[u16;4] (0–511 grid, x0,y0,x1,y1), inner: Box<Node>}, Prov{page_no,bbox:[l,t,r,b] top-left points, charspan, seq, inner},
  PageFurniture{footer, location, text}, PageBreak, PageInfo{page_no, width, height} (first node of every page in the PDF paths), Group{…}, Commented{…}, DoclangOnly(Box<Node>), TextDump(String).
  Denormalisation of the 0–511 grid to page points is in docling-core json.rs (`prov_json`, grid branch): read it and replicate exactly (BOTTOMLEFT output).
- pdfium-render 0.8.37 is the major docling-pdf pins (`pdfium-render = "0.8"`); we must use the same major so one crate instance links libpdfium.
- Model files (release https://github.com/docling-project/docling.rs/releases/download/models-v1/): layout_heron_int8.onnx 65 MB, layout_heron.onnx 164 MB, ocr_det.onnx 9 MB, ocr_rec_en.onnx 8 MB, en_dict.txt,
  ocr_rec.onnx 10 MB, ppocr_keys_v1.txt, tableformer: encoder.onnx 102 MB, decoder_int8.onnx 47 MB, bbox.onnx(+.data 37 MB), libpdfium.so (linux x64 only).
  pdfium for arm64: https://github.com/bblanchon/pdfium-binaries/releases/download/chromium%2F8066/pdfium-{linux-arm64,mac-arm64}.tgz (lib/libpdfium.so | lib/libpdfium.dylib).

## Trait additions (owner F3; F1/F2 code against them now)
In src/backend/mod.rs:
  trait Extractor { …existing…; fn provides_reading_order(&self) -> bool { false } }
  trait DocumentSession { …existing…;
     /// Bytes of a figure recorded in `PageText::figures` (same page/index), taken once; None when the backend has no pixels for it.
     fn take_figure_bytes(&mut self, page: u32, index: u32) -> Option<Vec<u8>> { let _ = (page, index); None } }
  by_name gains: "docling-text" and "docling" (cfg(feature = "docling")), "pdfium" (cfg(feature = "pdfium")); NAMES lists only compiled-in names;
  pub fn available() -> Vec<&'static str> (same as NAMES) and pub fn all_known() -> &'static [&'static str] = ["lopdf","pdfium","docling-text","docling"].
Cargo.toml (owner F3): [features] default = []; pdfium = ["dep:pdfium-render"]; docling = ["dep:docling-pdf", "dep:docling-core", "pdfium"];
  pdfium-render = { version = "0.8", optional = true }; docling-pdf = { version = "1.69", optional = true }; docling-core = { version = "1.69", optional = true }.
Pipeline (owner F3): when `backend.provides_reading_order()`, lines are built from spans in `seq` order (new `reading_order::lines_in_backend_order(page: &mut PageText)`:
  one Line per span, column 0, text = span text, paragraphs separated "\n" and "\n\n" when the span text ends a block — keep simple: each span is a line, join with "\n").
  After page extraction, for every figure the session can supply bytes for, when the job has an output dir (new `Job.figures_dir: Option<String>`), write
  `<dir>/<hash>/p<page>-f<index>.<ext>` and fill `file`/`sha256`; otherwise only sha256 if bytes available. Never inline bytes into text.

## F1 (Fable): src/backend/docling_backend.rs   (feature = "docling"; the whole file is `#![cfg(feature = "docling")]`-free: gate at the `mod` declaration in backend/mod.rs, F3 does that)
pub struct DoclingBackend { pub full: bool /* false = text layer only */, pub ocr: bool, pub tables: bool, pub force_ocr: bool }
impl DoclingBackend { pub fn text_layer() -> Self; pub fn full() -> Self }   // full(): ocr true, tables false (TableFormer models are large; enable later), force_ocr false
impl Extractor: identity name "docling-text" | "docling", version "1.69.2" (const DOCLING_VERSION checked against Cargo.lock by a test like the lopdf one),
  config_digest over {full, ocr, tables, force_ocr, provider: "cpu"}; provides_reading_order() -> true.
  open(bytes, password): text mode: page_count via lopdf (Document::load_mem + get_pages().len(), Malformed on error; encrypted → Encrypted(PasswordRequired) unless password decrypts);
  full mode: `docling_pdf::page_count(bytes, password)`, mapping PdfError::Pdfium messages containing "not installed" → BackendError::Unsupported(msg) and "password" → Encrypted.
  Session holds bytes + password + lazily converted `Option<Converted>` where Converted = per-page Vec<(PageInfo dims, Vec<Item>)> built from the DoclingDocument nodes:
  walk nodes in order, track the current page from PageInfo; for Located{location, inner}/Prov{..} wrappers compute the bbox in bottom-left points (replicate json.rs); unwrap Commented/DoclangOnly/Group recursively;
  text-bearing nodes → one Span each (Heading, Paragraph, ListItem (prefix marker "- " or the number is NOT added; text only), Code, Formula (text/latex as given), Caption, PageFurniture (append after body items of the page),
  Table → one Span per cell in row-major order with the cell bbox when present (cell text), Picture → Figure{kind:"layout", bbox, caption, mime/width/height from image if Some} and the bytes kept in a HashMap<(page,index), Vec<u8>> for take_figure_bytes;
  PictureImage bytes NEVER go into any Span. font None, size None (docling does not expose them). seq = running index per page. NFC-normalise text.
  Conversion: text mode → convert_text_layer_pages(bytes, "doc", None); full mode → a process-wide `static PIPELINE: OnceLock<Mutex<Pipeline>>` (Pipeline::new(), .no_ocr(!ocr), .no_table_former(!tables), .force_full_page_ocr(force_ocr)), convert(bytes, password, "doc");
  PdfError → BackendError::Page{page:0,message} for conversion failure of the whole document? No: conversion failure is document-level → return Err(BackendError::Unsupported(..)) from the FIRST page_text call (the trait has no other channel); document the limitation.
  A page with no nodes → PageText with the PageInfo dims and a warning "docling: no items on page".
  page_text(page) with page > count → PageRange. info(): text mode via lopdf trailer /Info like lopdf_backend (copy the helper logic; do not import private fns), full mode: same via lopdf on the bytes (cheap).
Tests: text-layer mode on a 2-page lopdf-built PDF (copy the builder pattern from lopdf_backend.rs tests): page_count 2, spans on page 1 contain "Hello" (docling may merge cells into one paragraph; assert `spans.iter().any(|s| s.text.contains("Hello"))`), dims 612x792;
  grid denormalisation unit test on a synthetic Located{[0,0,511,511]} over a 612x792 page → bbox (0,0,612,792) within 0.01, and [0,0,255,255] etc.;
  `full_pipeline_smoke` test that RETURNS EARLY (eprintln "skipped: docling models/pdfium not present") unless `.models/layout_heron_int8.onnx` (or DOCLING_RS_MODELS_DIR) and a pdfium lib (PDFIUM_DYNAMIC_LIB_PATH or .pdfium/lib) exist; when present: full mode on the same PDF yields ≥1 span containing "Hello" and NO span text containing "data:image" or "base64";
  version test against Cargo.lock; malformed bytes → Malformed.

## F2 (Fable): src/backend/pdfium_backend.rs   (feature = "pdfium")
pub struct PdfiumBackend { pub library_dir: Option<String> }  impl Default (None → env PDFIUM_DYNAMIC_LIB_PATH, then ".pdfium/lib", then system)
impl Extractor: identity name "pdfium", version from `Pdfium` if the binding exposes FPDF version else "chromium/8066-binding-0.8.37" (const, documented), config_digest over {library_dir};
  provides_reading_order() -> false (geometry; our XY-cut orders it).
  Binding: read pdfium-render-0.8.37 src (lib.rs, pdfium.rs, bindings/, page_object*.rs, page_text*.rs, error.rs, Cargo.toml features): decide and DOCUMENT how the `Pdfium` instance is shared —
  pdfium-render's `Pdfium` is !Sync; use `thread_local! { static PDFIUM: RefCell<Option<Rc<Pdfium>>> }` so each worker thread binds once (dlopen once per thread), or a global Mutex<Pdfium> if the crate marks it Send (check `thread_safe` feature and the `Pdfium` type). Binding failure (`PdfiumError::LoadLibraryError`) → BackendError::Unsupported("pdfium library not found: set PDFIUM_DYNAMIC_LIB_PATH or place it under .pdfium/lib").
  open: `pdfium.load_pdf_from_byte_slice(bytes, password)` (check the exact fn name in 0.8.37: load_pdf_from_byte_slice / load_pdf_from_byte_vec); PdfiumError::PdfiumLibraryInternalError(FPDF_ERR_PASSWORD) → Encrypted(PasswordRequired/WrongPassword); FPDF_ERR_FORMAT/FILE → Malformed.
  Lifetimes: PdfDocument<'a> borrows Pdfium; a session must own both → store the bytes and the Pdfium Rc and re-open lazily per call if the borrow makes a self-referential struct impossible (document the choice; opening from bytes is cheap in pdfium), or use `ouroboros`-free approach: hold `Rc<Pdfium>` and a `PdfDocument<'static>` via leaking is NOT allowed. Prefer: session stores bytes + Rc<Pdfium>; page_text re-opens the document each call (measure; note in doc comment).
  page_text: page dims from `page.width()/height()` (PdfPoints → f32), rotation from `page.rotation()`; iterate `page.objects().iter()`: Text objects → Span{text: object text NFC, bbox from `bounds()` (PdfRect: bottom, left, top, right), font: `font().name()`, size: `unscaled_font_size()` or `scaled_font_size()` (check names), seq};
  Image objects → Figure{kind:"raster", bbox from bounds, mime "image/png" if you export via `get_raw_image()/get_image()` as PNG bytes (image crate is NOT a dependency: if pdfium-render returns a `DynamicImage` behind its `image` feature, do NOT enable that; instead use `get_raw_image_data()`/`FPDFImageObj_GetImageDataDecoded` raw bytes and set mime None + width/height from `get_raw_metadata()`), bytes kept for take_figure_bytes; Path objects ignored except counting for a page warning "N vector paths (not exported)".
  Form XObjects: pdfium flattens form objects into `PdfPageObject::XObjectForm` — iterate its inner objects recursively if the API exposes them (check page_objects.rs), else warn.
  Text spans that come back empty from pdfium for a Tj with glyphs (missing ToUnicode) → U+FFFD per glyph and a warning; never dropped silently.
Tests: all tests skip early with eprintln when the pdfium library cannot be bound (check via a helper that tries the bind), so the default CI legs pass; when bound: 2-page lopdf-built PDF → page_count 2, dims, span "Hello" with x0 ≈ 100 (±1), font contains "Helvetica", size 12; malformed bytes → Malformed; page 0/3 → PageRange; identity stable.

## F3 (Opus, after PR #3 is committed — DO NOT START until told): src/backend/mod.rs, src/pipeline.rs, src/reading_order.rs (additive fn), Cargo.toml features, src/lib.rs (docs only), figures export in pipeline, Job.figures_dir in schema.rs, main.rs `--figures-dir` flag.
## F4 (Opus): .github/workflows/native.yml — matrix ubuntu-24.04-arm + macos-15; steps: checkout; rustup; cache cargo; cache `.models` (key: model file list) and `.pdfium` (key: chromium/8066);
   download pdfium tgz (pinned URL per OS/arch, extract lib/…), download layout_heron_int8.onnx, ocr_det.onnx, ocr_rec_en.onnx, en_dict.txt from models-v1 with sha256 recorded in `native/manifest.json` (fill the hashes with a first run: the workflow prints them; commit them after);
   `cargo build --release --features docling,pdfium`; `cargo test --features docling,pdfium` with PDFIUM_DYNAMIC_LIB_PATH=.pdfium/lib; then `tpe backends`; then `tpe eval --backend docling --split dev --offline`? (no: eval needs the corpus; run `tpe corpus fetch` first, cached); upload eval-out as artifact; NOT part of the required `ci` job. Also docs/NATIVE.md.
## F5 (Opus): tests/scanned_fixture.rs + src/testing/raster.rs? → a synthetic scanned PDF generator: draws a few words with a built-in 7x9 block-letter bitmap font at 4x scale into a 1-bit/8-bit gray image, embeds it as an image-only page via lopdf (features: check `embed_image`/`Stream` with /Filter none, /ColorSpace /DeviceGray, /BitsPerComponent 8) → used by a docling full-mode OCR test (skips without models) that asserts ≥ 80% of the characters of "HELLO WORLD 2026" are recovered; also asserts the lopdf/pdfium backends return 0 text spans and 1 figure for that page (raster classification).
## F6 (Opus): ledger.rs `figures` table (run_id, page, idx, kind, mime, width_px, height_px, sha256, file, caption, bbox cols) + write/load + stats.figures; docs/ENGINE.md update; tests.
