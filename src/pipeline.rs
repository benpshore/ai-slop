//! Glue that runs the extraction stages for one document: acquire the bytes,
//! open a backend session, extract every requested page, order the spans,
//! summarise chunks, then derive metadata and citations. The ledger write is
//! left to the caller so that one thread can own the database.
//!
//! Figures: after each page is extracted, the bytes behind every
//! `PageText::figures` entry are taken from the session once. Their SHA-256
//! is recorded on the figure and, when the job names a `figures_dir`, they
//! are written to `<figures_dir>/<document hash>/p<page>-f<index>.<ext>`.
//! Bytes never reach the page text.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::thread::{self, ScopedJoinHandle};
use std::time::Instant;

use thiserror::Error;

use crate::acquire::{self, AcquireError};
use crate::backend::{self, BackendError, DocumentSession, Extractor};
use crate::citations;
use crate::metadata;
use crate::reading_order;
use crate::regions;
use crate::router::{self, Route};
use crate::schema::{
    BackendIdentity, CHUNK_PAGES, ChunkResult, ContentHash, Document, ExtractionResult, Job,
    PageText, SCHEMA_VERSION, StageTimings, Status, config_digest, sha256_hex,
};
use crate::text_cleanup;

/// Failure of a whole job. Per-page backend failures are not errors: they
/// become warnings and a `Partial` status instead.
#[derive(Debug, Error)]
pub enum PipelineError {
    #[error(transparent)]
    Acquire(#[from] AcquireError),
    #[error(transparent)]
    Backend(#[from] BackendError),
    #[error("unknown backend: {0}")]
    UnknownBackend(String),
}

/// A page-level progress notification for one document, delivered on the
/// calling thread while the job runs (see [`run_job_observed`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    /// The document opened: it has `pages` pages and `total` of them will be
    /// processed (fewer than `pages` for a sub-range run).
    Opened { pages: u32, total: u32 },
    /// Page `page` (1-based) finished; `done` of `total` are processed.
    Page { page: u32, done: u32, total: u32 },
}

/// Milliseconds elapsed since `start`.
fn elapsed_ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

/// Resolve the requested inclusive page range against the document's page
/// count. `None` means every page; a range is clamped to `1..=count`, and a
/// start beyond the last page is an error.
fn resolve_page_range(
    requested: Option<(u32, u32)>,
    count: u32,
) -> Result<(u32, u32), BackendError> {
    match requested {
        None => Ok((1, count)),
        Some((start, end)) => {
            let first = start.max(1);
            if first > count {
                return Err(BackendError::PageRange { page: first, count });
            }
            Ok((first, end.min(count)))
        }
    }
}

/// `config_digest` for a run that covers only pages `first..=last` of the
/// document. The backend's own digest is folded together with the page range
/// (`backend_config` and `pages` keys through [`config_digest`]) so a
/// sub-range run has a different identity from a full run of the same
/// backend and from any other sub-range.
fn sub_range_digest(backend_digest: &str, first: u32, last: u32) -> String {
    let mut config: BTreeMap<String, String> = BTreeMap::new();
    config.insert("backend_config".to_string(), backend_digest.to_string());
    config.insert("pages".to_string(), format!("{first}-{last}"));
    config_digest(&config)
}

/// File extension for exported figure bytes of MIME type `mime`.
fn figure_extension(mime: Option<&str>) -> &'static str {
    match mime {
        Some("image/png") => "png",
        Some("image/jpeg") => "jpg",
        Some("image/jp2") => "jp2",
        _ => "bin",
    }
}

/// Write `bytes` to `target`, creating its parent directory.
fn write_figure(target: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(target, bytes)
}

/// The export directory of one run, relative to the figures directory:
/// `<hash>/<backend name>-<first 8 digits of the config digest>`. Runs of
/// different backends/configurations on one document must not overwrite
/// each other's exports, so the path carries the backend identity.
fn figure_run_dir(hash: &str, identity: &BackendIdentity) -> String {
    format!(
        "{hash}/{}-{}",
        identity.name,
        &identity.config_digest[..identity.config_digest.len().min(8)]
    )
}

/// Take the bytes of every figure on `page` from `session` and record their
/// SHA-256. With `export = Some((figures_dir, run_dir))` (see
/// [`figure_run_dir`]), also write them to
/// `<figures_dir>/<run_dir>/p<page>-f<index>.<ext>` and set `file` to that
/// path relative to `figures_dir` (always `/`-separated). A failed write
/// leaves `file` unset and returns a warning (never prefixed `failed:`, which
/// is reserved for pages whose text could not be extracted).
fn collect_figures(
    session: &mut dyn DocumentSession,
    page: &mut PageText,
    export: Option<(&Path, &str)>,
) -> Vec<String> {
    let page_no = page.page;
    let mut warnings: Vec<String> = Vec::new();
    for figure in &mut page.figures {
        let index = figure.index;
        let Some(bytes) = session.take_figure_bytes(page_no, index) else {
            continue;
        };
        figure.sha256 = Some(sha256_hex(&bytes));
        let Some((dir, run_dir)) = export else {
            continue;
        };
        let ext = figure_extension(figure.mime.as_deref());
        let relative = format!("{run_dir}/p{page_no}-f{index}.{ext}");
        let target = dir.join(&relative);
        match write_figure(&target, &bytes) {
            Ok(()) => figure.file = Some(relative),
            Err(err) => warnings.push(format!(
                "figure export: page {page_no} figure {index}: {}: {err}",
                target.display()
            )),
        }
    }
    page.warnings.extend(warnings.iter().cloned());
    warnings
}

/// Compile every lazily built regex of the text stages (and of the `LaTeX`
/// ground truth used by `tpe eval`) once, so the first document does not pay
/// for it inside its stage timings. Call it before timing starts; calling it
/// again is cheap and harmless.
pub fn warm_up() {
    citations::warm_up();
    metadata::warm_up();
    text_cleanup::warm_up();
    crate::latex_refs::warm_up();
}

/// The document hash and how long computing it took.
struct TimedHash {
    hash: ContentHash,
    ms: f64,
}

/// SHA-256 of `bytes`, timed.
fn hash_timed(bytes: &[u8]) -> TimedHash {
    let start = Instant::now();
    let hash = ContentHash(sha256_hex(bytes));
    TimedHash {
        hash,
        ms: elapsed_ms(start),
    }
}

/// The document hash, either still being computed on its thread or done.
enum HashState<'scope> {
    Running(ScopedJoinHandle<'scope, TimedHash>),
    Done(TimedHash),
}

impl HashState<'_> {
    /// Wait for the hash. A panic on the hashing thread is re-raised here, so
    /// callers that catch panics per document still see it.
    fn finish(self) -> TimedHash {
        match self {
            Self::Running(handle) => match handle.join() {
                Ok(hashed) => hashed,
                Err(payload) => std::panic::resume_unwind(payload),
            },
            Self::Done(hashed) => hashed,
        }
    }
}

/// What the parse stage hands to the later stages.
struct Parsed {
    hashed: TimedHash,
    page_count: u32,
    pages: Vec<PageText>,
    warnings: Vec<String>,
    status: Status,
    info: BTreeMap<String, String>,
}

/// Open `bytes` with `extractor` and extract the job's pages while the
/// document hash is computed on a scoped thread.
///
/// The hash is joined after the page loop, so it overlaps the whole parse.
/// Figure exports are filed under the hash, so with a `figures_dir` it is
/// joined right after `open` instead. Waiting for it happens inside the
/// caller's parse timer. `identity` gets the sub-range digest when the range
/// does not cover every page (see [`run_job`]).
fn parse_while_hashing(
    extractor: &dyn Extractor,
    job: &Job,
    bytes: &[u8],
    identity: &mut BackendIdentity,
    observe: &mut dyn FnMut(Progress),
) -> Result<Parsed, PipelineError> {
    thread::scope(|scope| -> Result<Parsed, PipelineError> {
        let mut hash_state = HashState::Running(scope.spawn(move || hash_timed(bytes)));

        let mut session = extractor.open(bytes, job.password.as_deref())?;
        let page_count = session.page_count();
        let (first, last) = resolve_page_range(job.pages, page_count)?;
        let covers_all_pages = first <= 1 && last >= page_count;
        let total = last - first + 1;
        observe(Progress::Opened {
            pages: page_count,
            total,
        });

        let mut pages: Vec<PageText> = Vec::new();
        let mut warnings: Vec<String> = Vec::new();
        let mut status = Status::Complete;
        if !covers_all_pages {
            warnings.push(format!(
                "partial extraction: pages {first}-{last} of {page_count}"
            ));
            status = Status::Partial;
            identity.config_digest = sub_range_digest(&identity.config_digest, first, last);
        }
        let figures_dir: Option<&Path> = job.figures_dir.as_deref().map(Path::new);
        let mut run_dir = String::new();
        if figures_dir.is_some() {
            let hashed = hash_state.finish();
            run_dir = figure_run_dir(&hashed.hash.0, identity);
            hash_state = HashState::Done(hashed);
        }
        let export: Option<(&Path, &str)> = figures_dir.map(|dir| (dir, run_dir.as_str()));
        for page in first..=last {
            match session.page_text(page) {
                Ok(mut text) => {
                    let figure_warnings = collect_figures(session.as_mut(), &mut text, export);
                    warnings.extend(figure_warnings);
                    pages.push(text);
                }
                Err(BackendError::Page { message, .. }) => {
                    let warning = format!("failed: page {page}: {message}");
                    let mut placeholder = PageText::new(page, 0.0, 0.0, 0);
                    placeholder.warnings.push(warning.clone());
                    warnings.push(warning);
                    pages.push(placeholder);
                    status = Status::Partial;
                }
                Err(other) => return Err(PipelineError::Backend(other)),
            }
            observe(Progress::Page {
                page,
                done: u32::try_from(pages.len()).unwrap_or(u32::MAX),
                total,
            });
        }
        let info: BTreeMap<String, String> = session.info();
        Ok(Parsed {
            hashed: hash_state.finish(),
            page_count,
            pages,
            warnings,
            status,
            info,
        })
    })
}

/// Run every stage for one document and return the complete result.
///
/// A `BackendError::Page` for one page is recorded as a warning prefixed with
/// `failed:` (on the result and on a placeholder `PageText` for that page so
/// chunking stays stable) and the status becomes `Partial`. Any other backend
/// error aborts the job.
///
/// When the effective page range (after clamping) does not cover every page
/// of the document, the result is a *sub-range run*: its status is `Partial`
/// (never `Complete`), a warning `partial extraction: pages a-b of n` is
/// recorded, and `backend.config_digest` is replaced by a digest derived from
/// the backend's digest plus the range (see `sub_range_digest`). The ledger
/// keys runs by backend identity, so this keeps a sub-range run from being
/// published over, or in place of, the full-document run. A full run keeps
/// the backend's identity unchanged.
///
/// Spans are ordered geometrically ([`reading_order::order_page`]) unless the
/// backend declares `provides_reading_order`, in which case its `seq` order
/// is kept ([`reading_order::lines_in_backend_order`]). Either way the
/// document then goes through [`text_cleanup::clean_document`] (running
/// heads, page numbers, the `arXiv` stamp, script fragments and line-end
/// hyphens leave the text; spans are untouched) and
/// [`regions::tag_regions`] (figure text, table cells and algorithm blocks
/// next to their captions get a line role; the text is unchanged), both
/// timed as part of `order_ms`. Figure bytes are handled as described in the module docs.
///
/// `acquire_ms` covers reading the file only. Its SHA-256 is computed on a
/// second thread while the backend parses; that work is reported as
/// `hash_ms`, and any wait for it falls inside `parse_ms`.
pub fn run_job(job: &Job) -> Result<ExtractionResult, PipelineError> {
    run_job_observed(job, &mut |_| {})
}

/// [`run_job`] reporting a [`Progress`] event when the document opens and
/// after each page, on the calling thread.
pub fn run_job_observed(
    job: &Job,
    observe: &mut dyn FnMut(Progress),
) -> Result<ExtractionResult, PipelineError> {
    if job.backend == AUTO_BACKEND {
        return run_job_auto_observed(job, observe);
    }
    let extractor = backend::by_name(&job.backend)
        .ok_or_else(|| PipelineError::UnknownBackend(job.backend.clone()))?;
    run_job_with_observed(extractor.as_ref(), job, observe)
}

/// Backend name that routes: `lopdf` first, then `pdfium` for pages whose
/// fonts had no Unicode mapping, then docling (layout and OCR) for scans
/// or other poor text (`crate::router`). Unresolved `PDFium` mapping evidence
/// retains native Partial output: automatic OCR recovery is not verified.
pub const AUTO_BACKEND: &str = "auto";

/// Run `job` with a route chosen from what `lopdf` reports. Every page is
/// read by `lopdf`; when the assessment asks for `pdfium` or docling and that
/// backend is compiled in and works, its result replaces the `lopdf` one
/// and a `routed: …` warning records why. A missing or failing backend
/// keeps the `lopdf` result with a warning naming the route that was not
/// taken. `PDFium` mapping diagnostics instead retain the native Partial result;
/// those flags mark unverified mappings, not necessarily lost characters.
/// Progress events are reported for every pass.
pub fn run_job_auto_observed(
    job: &Job,
    observe: &mut dyn FnMut(Progress),
) -> Result<ExtractionResult, PipelineError> {
    let lopdf = backend::by_name("lopdf")
        .ok_or_else(|| PipelineError::UnknownBackend("lopdf".to_string()))?;
    let mut result = run_job_with_observed(lopdf.as_ref(), job, observe)?;
    let first = router::assess(&result.pages);
    let mut route = first.route();
    if route == Route::Pdfium {
        match rerun(job, route, observe) {
            Ok(Some(second)) => {
                let again = router::assess(&second.pages);
                result = second;
                result.warnings.push(format!(
                    "routed: pdfium ({} of {} pages had fonts lopdf could not map)",
                    first.unmapped, first.pages
                ));
                // The full Docling adapter does not prove that it repaired these
                // source-character mappings. Do not erase the evidence/Partial
                // status with another plausible text layer; keep native output.
                if result.pages.iter().any(|page| {
                    page.warnings
                        .iter()
                        .any(|w| w.starts_with("unicode_mapping:"))
                }) {
                    result.warnings.push(
                        "unresolved: pdfium Unicode mapping; native text retained because automatic OCR recovery is unverified"
                            .to_string(),
                    );
                    return Ok(result);
                }
                route = again.route_after_pdfium();
                if route != Route::Docling {
                    return Ok(result);
                }
            }
            Ok(None) => {
                result
                    .warnings
                    .push("route not taken: pdfium is not compiled into this build".to_string());
                return Ok(result);
            }
            Err(err) => {
                result
                    .warnings
                    .push(format!("route not taken: pdfium failed: {err}"));
                return Ok(result);
            }
        }
    }
    if route == Route::Docling {
        match rerun(job, route, observe) {
            Ok(Some(third)) => {
                result = third;
                result.warnings.push(format!(
                    "routed: docling ({} scanned and {} unmapped of {} pages)",
                    first.scanned, first.unmapped, first.pages
                ));
            }
            Ok(None) => result
                .warnings
                .push("route not taken: docling is not compiled into this build".to_string()),
            Err(err) => result
                .warnings
                .push(format!("route not taken: docling failed: {err}")),
        }
    }
    Ok(result)
}

/// Run `job` again with the backend for `route`; `None` when that backend
/// is not compiled in. Docling keeps figure bytes only for a job that
/// exports them.
fn rerun(
    job: &Job,
    route: Route,
    observe: &mut dyn FnMut(Progress),
) -> Result<Option<ExtractionResult>, PipelineError> {
    let extractor: Box<dyn Extractor> = match route {
        #[cfg(feature = "docling")]
        Route::Docling => Box::new(
            backend::docling_backend::DoclingBackend::full()
                .with_figures(job.figures_dir.is_some()),
        ),
        _ => match router::extractor_for(route) {
            Some(extractor) => extractor,
            None => return Ok(None),
        },
    };
    run_job_with_observed(extractor.as_ref(), job, observe).map(Some)
}

/// [`run_job`] with an already resolved backend; `job.backend` is ignored.
pub fn run_job_with(
    extractor: &dyn Extractor,
    job: &Job,
) -> Result<ExtractionResult, PipelineError> {
    run_job_with_observed(extractor, job, &mut |_| {})
}

/// [`run_job_with`] reporting [`Progress`] events like [`run_job_observed`].
pub fn run_job_with_observed(
    extractor: &dyn Extractor,
    job: &Job,
    observe: &mut dyn FnMut(Progress),
) -> Result<ExtractionResult, PipelineError> {
    let mut identity = extractor.identity();

    let mut timings = StageTimings::default();

    let acquire_start = Instant::now();
    let read = acquire::read_verified(Path::new(&job.path), job.max_bytes)?;
    timings.acquire_ms = elapsed_ms(acquire_start);

    let parse_start = Instant::now();
    let Parsed {
        hashed,
        page_count,
        mut pages,
        mut warnings,
        mut status,
        info,
    } = parse_while_hashing(extractor, job, &read.bytes, &mut identity, observe)?;
    timings.parse_ms = elapsed_ms(parse_start);
    timings.hash_ms = hashed.ms;
    let snapshot = read.into_snapshot(hashed.hash);

    let order_start = Instant::now();
    let backend_order = extractor.provides_reading_order();
    for page in &mut pages {
        if backend_order {
            reading_order::lines_in_backend_order(page);
        } else {
            reading_order::order_page(page);
        }
    }
    text_cleanup::clean_document(&mut pages);
    regions::tag_regions(&mut pages);
    timings.order_ms = elapsed_ms(order_start);

    // Limits or Unicode mapping failures can leave usable but incomplete text.
    // Keep that text, but never publish it as complete. Promote
    // page-local diagnostics so JSON consumers and the ledger's run
    // record do not have to infer completeness from nested warning strings.
    for page in &pages {
        if page.extraction_status() == Status::Partial {
            status = Status::Partial;
        }
        for warning in &page.warnings {
            if warning.starts_with("resource_limit:") || warning.starts_with("unicode_mapping:") {
                warnings.push(format!("page {}: {warning}", page.page));
            }
        }
    }

    let chunks = chunk_results(&pages, timings.parse_ms + timings.order_ms);

    let metadata_start = Instant::now();
    let meta = metadata::extract_metadata(&info, &pages);
    timings.metadata_ms = elapsed_ms(metadata_start);

    let citations_start = Instant::now();
    let (references, markers) = citations::extract_citations(&pages);
    timings.citations_ms = elapsed_ms(citations_start);

    let size = snapshot.bytes.len() as u64;
    let document = Document {
        hash: snapshot.hash,
        size,
        pages: page_count,
        sources: vec![snapshot.source],
    };

    Ok(ExtractionResult {
        schema_version: SCHEMA_VERSION,
        document,
        backend: identity,
        status,
        pages,
        chunks,
        metadata: meta,
        references,
        citations: markers,
        warnings,
        timings,
    })
}

/// Summarise pages into chunks of [`CHUNK_PAGES`] consecutive page numbers.
///
/// Pages are grouped by page number, so page 21 always lands in chunk 1 even
/// when only a sub-range was extracted. `parse_plus_order_ms` is apportioned
/// to chunks by page count. The chunk text hash covers the page texts joined
/// by `"\n\x0C\n"`. A chunk is `Partial` when any of its pages carries a
/// warning starting with `failed:`, `resource_limit:`, or `unicode_mapping:`.
pub fn chunk_results(pages: &[PageText], parse_plus_order_ms: f64) -> Vec<ChunkResult> {
    if pages.is_empty() {
        return Vec::new();
    }
    let per_page_ms = parse_plus_order_ms / pages.len() as f64;

    let mut groups: BTreeMap<u32, Vec<&PageText>> = BTreeMap::new();
    for page in pages {
        let chunk_index = page.page.saturating_sub(1) / CHUNK_PAGES;
        groups.entry(chunk_index).or_default().push(page);
    }

    let mut chunks: Vec<ChunkResult> = Vec::with_capacity(groups.len());
    for (chunk_index, mut members) in groups {
        members.sort_by_key(|page| page.page);
        let count = members.len() as f64;
        let mut first_page = u32::MAX;
        let mut last_page = 0_u32;
        let mut status = Status::Complete;
        let mut texts: Vec<&str> = Vec::with_capacity(members.len());
        for page in members {
            first_page = first_page.min(page.page);
            last_page = last_page.max(page.page);
            texts.push(page.text.as_str());
            if page.extraction_status() == Status::Partial {
                status = Status::Partial;
            }
        }
        chunks.push(ChunkResult {
            chunk_index,
            first_page,
            last_page,
            status,
            text_sha256: sha256_hex(texts.join("\n\x0C\n").as_bytes()),
            ms: per_page_ms * count,
        });
    }
    chunks
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    use lopdf::content::{Content, Operation};
    use lopdf::{Document, Object, Stream, dictionary};
    use tempfile::TempDir;

    use super::{
        PipelineError, Progress, chunk_results, figure_extension, resolve_page_range, run_job,
        run_job_observed, run_job_with, sub_range_digest,
    };
    use crate::backend::{BackendError, DocumentSession, Extractor, lopdf_backend::LopdfBackend};
    use crate::schema::{
        BBox, BackendIdentity, ContentHash, Figure, Job, PageText, Span, Status, config_digest,
        sha256_hex,
    };

    /// Bytes the fake backend hands out for figure 0 on page 1.
    const FIGURE_BYTES: &[u8] = b"\x89PNG\r\n\x1a\nfake pixels";

    /// A backend whose single page has two spans (`first` at the bottom with
    /// `seq` 0, `second` at the top with `seq` 1) and one PNG figure.
    struct FakeExtractor {
        reading_order: bool,
    }

    struct FakeSession {
        figure: Option<Vec<u8>>,
    }

    fn fake_span(text: &str, y0: f32, seq: u32) -> Span {
        Span {
            text: text.to_string(),
            bbox: Some(BBox {
                x0: 72.0,
                y0,
                x1: 120.0,
                y1: y0 + 10.0,
            }),
            font: None,
            size: Some(10.0),
            seq,
        }
    }

    impl DocumentSession for FakeSession {
        fn page_count(&self) -> u32 {
            1
        }

        fn page_text(&mut self, page: u32) -> Result<PageText, BackendError> {
            let mut text = PageText::new(page, 612.0, 792.0, 0);
            text.spans.push(fake_span("second", 700.0, 1));
            text.spans.push(fake_span("first", 100.0, 0));
            text.figures.push(Figure {
                index: 0,
                bbox: None,
                kind: "raster".to_string(),
                mime: Some("image/png".to_string()),
                width_px: Some(1),
                height_px: Some(1),
                sha256: None,
                file: None,
                caption: None,
            });
            Ok(text)
        }

        fn info(&self) -> BTreeMap<String, String> {
            BTreeMap::new()
        }

        fn take_figure_bytes(&mut self, page: u32, index: u32) -> Option<Vec<u8>> {
            if page == 1 && index == 0 {
                self.figure.take()
            } else {
                None
            }
        }
    }

    impl Extractor for FakeExtractor {
        fn identity(&self) -> BackendIdentity {
            BackendIdentity {
                name: "fake".to_string(),
                version: "0".to_string(),
                config_digest: config_digest(&BTreeMap::new()),
            }
        }

        fn open(
            &self,
            _bytes: &[u8],
            _password: Option<&str>,
        ) -> Result<Box<dyn DocumentSession>, BackendError> {
            Ok(Box::new(FakeSession {
                figure: Some(FIGURE_BYTES.to_vec()),
            }))
        }

        fn provides_reading_order(&self) -> bool {
            self.reading_order
        }
    }

    /// A non-empty input file for the fake backend (its bytes are ignored).
    fn fake_input(dir: &Path) -> PathBuf {
        let path = dir.join("fake.pdf");
        std::fs::write(&path, b"%PDF-1.5 fake").unwrap();
        path
    }

    fn fake_job(path: &Path, figures_dir: Option<&Path>) -> Job {
        Job {
            path: path.to_string_lossy().into_owned(),
            backend: "fake".to_string(),
            pages: None,
            password: None,
            max_bytes: None,
            figures_dir: figures_dir.map(|dir| dir.to_string_lossy().into_owned()),
        }
    }

    /// Build a three-page PDF with Helvetica as `/F1`; page `n` shows `Page n`.
    fn three_page_pdf() -> Vec<u8> {
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
        for n in 1..=3_i32 {
            let operations = vec![
                Operation::new("BT", vec![]),
                Operation::new("Tf", vec!["F1".into(), 12_i32.into()]),
                Operation::new("Td", vec![72_i32.into(), 720_i32.into()]),
                Operation::new("Tj", vec![Object::string_literal(format!("Page {n}"))]),
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
        let tree = dictionary! {
            "Type" => "Pages",
            "Kids" => kids,
            "Count" => Object::Integer(3),
            "MediaBox" => vec![0_i32.into(), 0_i32.into(), 612_i32.into(), 792_i32.into()],
        };
        doc.objects.insert(tree_id, Object::Dictionary(tree));
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => tree_id,
        });
        doc.trailer.set("Root", catalog_id);
        let mut bytes: Vec<u8> = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        bytes
    }

    /// Write the three-page fixture into a fresh temporary directory. Keep the
    /// `TempDir` alive for as long as the path is used.
    fn three_page_fixture() -> (TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("three.pdf");
        std::fs::write(&path, three_page_pdf()).unwrap();
        (dir, path)
    }

    fn lopdf_job(path: &Path, pages: Option<(u32, u32)>) -> Job {
        Job {
            path: path.to_string_lossy().into_owned(),
            backend: "lopdf".to_string(),
            pages,
            password: None,
            max_bytes: None,
            figures_dir: None,
        }
    }

    fn synthetic_pages(count: u32) -> Vec<PageText> {
        (1..=count)
            .map(|n| {
                let mut page = PageText::new(n, 612.0, 792.0, 0);
                page.text = format!("page {n}");
                page
            })
            .collect()
    }

    #[test]
    fn forty_five_pages_make_three_chunks() {
        let pages = synthetic_pages(45);
        let chunks = chunk_results(&pages, 90.0);
        assert_eq!(chunks.len(), 3);
        assert_eq!((chunks[0].first_page, chunks[0].last_page), (1, 20));
        assert_eq!((chunks[1].first_page, chunks[1].last_page), (21, 40));
        assert_eq!((chunks[2].first_page, chunks[2].last_page), (41, 45));
        assert_eq!(chunks[0].chunk_index, 0);
        assert_eq!(chunks[2].chunk_index, 2);
        assert!(chunks.iter().all(|c| c.status == Status::Complete));
        assert!((chunks[0].ms - 40.0).abs() < 1e-9);
        assert!((chunks[2].ms - 10.0).abs() < 1e-9);
    }

    #[test]
    fn chunk_hash_covers_joined_texts() {
        let pages = synthetic_pages(2);
        let chunks = chunk_results(&pages, 0.0);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text_sha256, sha256_hex(b"page 1\n\x0C\npage 2"));
    }

    #[test]
    fn failed_page_marks_chunk_partial() {
        let mut pages = synthetic_pages(3);
        pages[1].warnings.push("failed: page 2: boom".to_string());
        let chunks = chunk_results(&pages, 3.0);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].status, Status::Partial);
    }

    #[test]
    fn sub_range_keeps_chunk_numbering() {
        let pages: Vec<PageText> = synthetic_pages(45).into_iter().skip(20).collect();
        let chunks = chunk_results(&pages, 0.0);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].chunk_index, 1);
        assert_eq!((chunks[0].first_page, chunks[0].last_page), (21, 40));
    }

    #[test]
    fn empty_input_has_no_chunks() {
        assert!(chunk_results(&[], 1.0).is_empty());
    }

    #[test]
    fn page_range_defaults_and_clamps() {
        assert_eq!(resolve_page_range(None, 7).unwrap(), (1, 7));
        assert_eq!(resolve_page_range(Some((0, 99)), 7).unwrap(), (1, 7));
        assert_eq!(resolve_page_range(Some((2, 2)), 7).unwrap(), (2, 2));
        let err = resolve_page_range(Some((8, 9)), 7).unwrap_err();
        assert!(matches!(err, BackendError::PageRange { page: 8, count: 7 }));
    }

    #[test]
    fn unknown_backend_is_reported_before_acquire() {
        let job = Job {
            path: "/definitely/not/a/real/path.pdf".to_string(),
            backend: "no-such-backend".to_string(),
            pages: None,
            password: None,
            max_bytes: None,
            figures_dir: None,
        };
        match run_job(&job) {
            Err(PipelineError::UnknownBackend(name)) => assert_eq!(name, "no-such-backend"),
            other => panic!("expected UnknownBackend, got {other:?}"),
        }
    }

    #[test]
    fn sub_range_digest_folds_backend_digest_and_range() {
        let backend = LopdfBackend::default().identity();
        let digest = sub_range_digest(&backend.config_digest, 2, 2);
        assert_ne!(digest, backend.config_digest);

        let mut config: BTreeMap<String, String> = BTreeMap::new();
        config.insert("backend_config".to_string(), backend.config_digest.clone());
        config.insert("pages".to_string(), "2-2".to_string());
        assert_eq!(digest, config_digest(&config));

        assert_ne!(digest, sub_range_digest(&backend.config_digest, 1, 2));
        assert_ne!(digest, sub_range_digest("other-backend-digest", 2, 2));
    }

    #[test]
    fn full_run_keeps_backend_identity_and_is_complete() {
        let (_dir, path) = three_page_fixture();
        let result = run_job(&lopdf_job(&path, None)).unwrap();

        let expected = LopdfBackend::default().identity();
        assert_eq!(result.backend.config_digest, expected.config_digest);
        assert_eq!(result.backend, expected);
        assert_eq!(result.status, Status::Complete);
        assert_eq!(result.document.pages, 3);
        assert_eq!(result.pages.len(), 3);
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
    }

    #[test]
    fn document_hash_is_the_file_hash() {
        let (_dir, path) = three_page_fixture();
        let result = run_job(&lopdf_job(&path, None)).unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(result.document.hash, ContentHash(sha256_hex(&bytes)));
        assert_eq!(result.document.size, bytes.len() as u64);
        assert!(result.timings.hash_ms >= 0.0);
    }

    #[test]
    fn warm_up_is_repeatable() {
        super::warm_up();
        super::warm_up();
    }

    #[test]
    fn explicit_range_covering_every_page_is_a_full_run() {
        let (_dir, path) = three_page_fixture();
        let expected = LopdfBackend::default().identity();
        for pages in [Some((1, 3)), Some((0, 99))] {
            let result = run_job(&lopdf_job(&path, pages)).unwrap();
            assert_eq!(result.status, Status::Complete, "pages {pages:?}");
            assert_eq!(result.backend, expected, "pages {pages:?}");
            assert_eq!(result.pages.len(), 3, "pages {pages:?}");
        }
    }

    #[test]
    fn sub_range_run_is_partial_with_its_own_digest() {
        let (_dir, path) = three_page_fixture();
        let result = run_job(&lopdf_job(&path, Some((2, 2)))).unwrap();

        assert_eq!(result.status, Status::Partial);
        assert_eq!(result.document.pages, 3);
        assert_eq!(result.pages.len(), 1);
        assert_eq!(result.pages[0].page, 2);
        let expected_warning = "partial extraction: pages 2-2 of 3".to_string();
        assert!(result.warnings.contains(&expected_warning));

        let full = LopdfBackend::default().identity();
        assert_eq!(result.backend.name, full.name);
        assert_eq!(result.backend.version, full.version);
        assert_ne!(result.backend.config_digest, full.config_digest);
        assert_eq!(
            result.backend.config_digest,
            sub_range_digest(&full.config_digest, 2, 2)
        );
    }

    #[test]
    fn progress_reports_the_open_and_every_page_of_the_range() {
        let (_dir, path) = three_page_fixture();
        let mut events = Vec::new();
        let result = run_job_observed(&lopdf_job(&path, Some((2, 3))), &mut |event| {
            events.push(event);
        })
        .unwrap();

        assert_eq!(result.pages.len(), 2);
        assert_eq!(
            events,
            [
                Progress::Opened { pages: 3, total: 2 },
                Progress::Page {
                    page: 2,
                    done: 1,
                    total: 2
                },
                Progress::Page {
                    page: 3,
                    done: 2,
                    total: 2
                },
            ]
        );
    }

    #[test]
    fn different_sub_ranges_have_different_digests() {
        let (_dir, path) = three_page_fixture();
        let first_two = run_job(&lopdf_job(&path, Some((1, 2)))).unwrap();
        let last_two = run_job(&lopdf_job(&path, Some((2, 3)))).unwrap();
        assert_eq!(first_two.status, Status::Partial);
        assert_eq!(last_two.status, Status::Partial);
        assert_ne!(
            first_two.backend.config_digest,
            last_two.backend.config_digest
        );
        let want_first = "partial extraction: pages 1-2 of 3".to_string();
        let want_last = "partial extraction: pages 2-3 of 3".to_string();
        assert!(first_two.warnings.contains(&want_first));
        assert!(last_two.warnings.contains(&want_last));
    }

    #[test]
    fn figure_bytes_are_hashed_and_exported() {
        let dir = tempfile::tempdir().unwrap();
        let input = fake_input(dir.path());
        let figures = dir.path().join("figures");
        let backend = FakeExtractor {
            reading_order: false,
        };
        let result = run_job_with(&backend, &fake_job(&input, Some(&figures))).unwrap();

        assert_eq!(result.status, Status::Complete);
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        let hash = result.document.hash.0.clone();
        let figure = &result.pages[0].figures[0];
        assert_eq!(figure.sha256, Some(sha256_hex(FIGURE_BYTES)));
        let identity = result.backend.clone();
        let expected = format!(
            "{hash}/{}-{}/p1-f0.png",
            identity.name,
            &identity.config_digest[..8]
        );
        assert_eq!(figure.file.as_deref(), Some(expected.as_str()));
        let written = std::fs::read(figures.join(&expected)).unwrap();
        assert_eq!(written, FIGURE_BYTES);
        assert!(
            !result.pages[0].text.contains("PNG"),
            "{}",
            result.pages[0].text
        );
        assert!(result.pages[0].warnings.is_empty());
    }

    #[test]
    fn figure_bytes_without_dir_only_set_the_hash() {
        let dir = tempfile::tempdir().unwrap();
        let input = fake_input(dir.path());
        let backend = FakeExtractor {
            reading_order: false,
        };
        let result = run_job_with(&backend, &fake_job(&input, None)).unwrap();
        let figure = &result.pages[0].figures[0];
        assert_eq!(figure.sha256, Some(sha256_hex(FIGURE_BYTES)));
        assert_eq!(figure.file, None);
        assert!(!dir.path().join("figures").exists());
    }

    #[test]
    fn backend_reading_order_is_kept_when_declared() {
        let dir = tempfile::tempdir().unwrap();
        let input = fake_input(dir.path());
        let job = fake_job(&input, None);

        let ordered = run_job_with(
            &FakeExtractor {
                reading_order: true,
            },
            &job,
        )
        .unwrap();
        assert_eq!(ordered.pages[0].text, "first\nsecond");

        let geometric = run_job_with(
            &FakeExtractor {
                reading_order: false,
            },
            &job,
        )
        .unwrap();
        assert!(
            geometric.pages[0].text.starts_with("second"),
            "{}",
            geometric.pages[0].text
        );
    }

    #[test]
    fn figure_extensions_follow_mime() {
        assert_eq!(figure_extension(Some("image/png")), "png");
        assert_eq!(figure_extension(Some("image/jpeg")), "jpg");
        assert_eq!(figure_extension(Some("image/jp2")), "jp2");
        assert_eq!(figure_extension(Some("image/x-jp2-codestream")), "bin");
        assert_eq!(figure_extension(None), "bin");
    }
}
