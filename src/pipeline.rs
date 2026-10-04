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
    if count == 0 {
        return Err(BackendError::PageRange { page: 1, count: 0 });
    }
    match requested {
        None => Ok((1, count)),
        Some((start, end)) => {
            let first = start.max(1);
            if first > count {
                return Err(BackendError::PageRange { page: first, count });
            }
            let last = end.min(count);
            if last < first {
                return Err(BackendError::Unsupported(format!(
                    "page range ends at {last} before it starts at {first}"
                )));
            }
            Ok((first, last))
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
                    router::mark_incomplete(&mut text);
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
/// `TPE_AUTO_NATIVE_FALLBACK=1` additionally permits the bounded MuPDF/Poppler
/// native cascade described in `docs/NATIVE_FALLBACK.md`.
pub const AUTO_BACKEND: &str = "auto";

const NATIVE_FALLBACK_ENV: &str = "TPE_AUTO_NATIVE_FALLBACK";
const NATIVE_FALLBACK_POLICY: &str = "mupdf-poppler-v1-max4";

fn native_fallback_enabled(value: Option<&std::ffi::OsStr>) -> Result<bool, PipelineError> {
    match value.and_then(std::ffi::OsStr::to_str) {
        None if value.is_none() => Ok(false),
        Some("0") => Ok(false),
        Some("1") => Ok(true),
        _ => Err(BackendError::Unsupported(format!("{NATIVE_FALLBACK_ENV} must be 0 or 1")).into()),
    }
}

fn record_native_fallback_policy(result: &mut ExtractionResult) {
    result.backend.config_digest = config_digest(&BTreeMap::from([
        (
            "backend_config".to_string(),
            result.backend.config_digest.clone(),
        ),
        (
            "auto_native_fallback".to_string(),
            NATIVE_FALLBACK_POLICY.to_string(),
        ),
    ]));
    result.warnings.push(format!(
        "routing policy: {NATIVE_FALLBACK_POLICY}; native fallback is opt-in; text-count guards do not verify semantic correctness"
    ));
}

/// Run `job` with a route chosen from what `lopdf` reports. Every page is
/// read by `lopdf`; when the assessment asks for `pdfium` or docling and that
/// backend is compiled in and works without observable regression, its result
/// replaces the `lopdf` one and a `routed: …` warning records why.
/// A missing, failing, or regressing backend
/// keeps the `lopdf` result with a warning naming the route that was not
/// taken. `PDFium` mapping diagnostics instead retain the native Partial result;
/// those flags mark unverified mappings, not necessarily lost characters.
/// Progress events are reported for every pass.
pub fn run_job_auto_observed(
    job: &Job,
    observe: &mut dyn FnMut(Progress),
) -> Result<ExtractionResult, PipelineError> {
    let native_fallback =
        native_fallback_enabled(std::env::var_os(NATIVE_FALLBACK_ENV).as_deref())?;
    let lopdf = backend::by_name("lopdf")
        .ok_or_else(|| PipelineError::UnknownBackend("lopdf".to_string()))?;
    let result = run_job_with_observed(lopdf.as_ref(), job, observe)?;
    let mut result = route_result_with_policy(result, native_fallback, &mut |route| {
        rerun(job, route, observe)
    });
    if native_fallback {
        record_native_fallback_policy(&mut result);
    }
    Ok(result)
}

/// Route a completed native pass without giving an unsuccessful fallback
/// permission to erase its usable text or completeness evidence.
#[cfg(test)]
fn route_result(
    result: ExtractionResult,
    rerun: &mut dyn FnMut(Route) -> Result<Option<ExtractionResult>, PipelineError>,
) -> ExtractionResult {
    route_result_with_policy(result, false, rerun)
}

fn route_result_with_policy(
    mut result: ExtractionResult,
    native_fallback: bool,
    rerun: &mut dyn FnMut(Route) -> Result<Option<ExtractionResult>, PipelineError>,
) -> ExtractionResult {
    let first = router::assess(&result.pages);
    let mut route = first.route();
    if route == Route::Pdfium {
        match rerun(route) {
            Ok(Some(second)) => {
                let regression = fallback_regression(&result, &second).or_else(|| {
                    native_fallback
                        .then(|| native_evidence_regression(&result, &second))
                        .flatten()
                });
                if let Some(reason) = regression {
                    result.warnings.push(format!(
                        "route not taken: pdfium {reason}; native text retained"
                    ));
                    return native_fallback_result(result, native_fallback, rerun);
                }
                let again = router::assess(&second.pages);
                let history = native_fallback.then(|| native_route_history(&result));
                result = second;
                if let Some(history) = history {
                    result.warnings.extend(history);
                }
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
                    if native_fallback {
                        return native_fallback_result(result, true, rerun);
                    }
                    result.warnings.push(
                        "unresolved: pdfium Unicode mapping; native text retained because automatic OCR recovery is unverified"
                            .to_string(),
                    );
                    return result;
                }
                route = again.route_after_pdfium();
                if route != Route::Docling {
                    if result.pages.iter().any(|page| {
                        page.warnings
                            .iter()
                            .any(|warning| warning.starts_with("failed:"))
                    }) {
                        return native_fallback_result(result, native_fallback, rerun);
                    }
                    return result;
                }
            }
            Ok(None) => {
                result
                    .warnings
                    .push("route not taken: pdfium is not compiled into this build".to_string());
                return native_fallback_result(result, native_fallback, rerun);
            }
            Err(err) => {
                result
                    .warnings
                    .push(format!("route not taken: pdfium failed: {err}"));
                return native_fallback_result(result, native_fallback, rerun);
            }
        }
    }
    if route == Route::Docling {
        match rerun(route) {
            Ok(Some(third)) => {
                if let Some(reason) = fallback_regression(&result, &third) {
                    result.warnings.push(format!(
                        "route not taken: docling {reason}; native text retained"
                    ));
                    return result;
                }
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
    result
}

/// A fixed native-only cascade: at most one `MuPDF` and one Poppler attempt.
/// Each accepted result stays whole, with its own backend/derived data. Nothing
/// here enters OCR, retries a backend, or changes the enclosing worker limits.
fn native_fallback_result(
    mut best: ExtractionResult,
    enabled: bool,
    rerun: &mut dyn FnMut(Route) -> Result<Option<ExtractionResult>, PipelineError>,
) -> ExtractionResult {
    if !enabled {
        return best;
    }
    for route in [Route::MuPdf, Route::Poppler] {
        let name = route.backend_name();
        match rerun(route) {
            Ok(Some(mut candidate)) => {
                let regression = fallback_regression(&best, &candidate)
                    .or_else(|| native_evidence_regression(&best, &candidate));
                if let Some(reason) = regression {
                    best.warnings.push(format!(
                        "route not taken: {name} {reason}; best native text retained"
                    ));
                    continue;
                }
                if native_result_quality(&candidate) <= native_result_quality(&best) {
                    best.warnings.push(format!(
                        "route not taken: {name} did not improve native extraction evidence; best native text retained"
                    ));
                    continue;
                }
                candidate.warnings.extend(native_route_history(&best));
                candidate.warnings.push(format!(
                    "routed: {name} (bounded native fallback after unresolved or failed PDFium; whole requested page range)"
                ));
                best = candidate;
                if best.status == Status::Complete
                    && !best.pages.iter().any(router::has_unmapped_text)
                {
                    break;
                }
            }
            Ok(None) => best.warnings.push(format!(
                "route not taken: {name} is not compiled in or both explicit native library files are not configured"
            )),
            Err(error) => best
                .warnings
                .push(format!("route not taken: {name} failed: {error}")),
        }
    }
    best
}

/// Retain prior route decisions and source-specific mapping uncertainty as
/// history. These are not warnings about the newly selected backend's text.
fn native_route_history(result: &ExtractionResult) -> Vec<String> {
    let mut history: Vec<String> = result
        .warnings
        .iter()
        .filter(|warning| {
            warning.starts_with("routed:")
                || warning.starts_with("route not taken:")
                || warning.starts_with("routing evidence:")
        })
        .cloned()
        .collect();
    for page in &result.pages {
        if router::has_unmapped_text(page) {
            let warnings: Vec<&str> = page
                .warnings
                .iter()
                .map(String::as_str)
                .filter(|warning| {
                    warning.starts_with("unicode_mapping:")
                        || warning.contains("undecodable")
                        || warning.contains("decoded as Latin-1")
                })
                .collect();
            for warning in if warnings.is_empty() {
                vec!["unresolved source-character mappings detected in text"]
            } else {
                warnings
            } {
                history.push(format!(
                    "routing evidence: {} page {}: {warning}",
                    result.backend.name, page.page
                ));
            }
        }
    }
    history
}

fn native_result_quality(result: &ExtractionResult) -> (usize, std::cmp::Reverse<usize>, usize) {
    (
        result
            .pages
            .iter()
            .filter(|page| page.extraction_status() == Status::Complete)
            .count(),
        std::cmp::Reverse(
            result
                .pages
                .iter()
                .filter(|page| router::has_unmapped_text(page))
                .count(),
        ),
        result
            .pages
            .iter()
            .map(|page| {
                page.text
                    .chars()
                    .filter(|ch| !ch.is_whitespace() && *ch != '\u{fffd}')
                    .count()
            })
            .sum(),
    )
}

fn evidence_counts<T: serde::Serialize>(
    values: impl Iterator<Item = T>,
) -> Result<BTreeMap<Vec<u8>, usize>, serde_json::Error> {
    let mut counts = BTreeMap::new();
    for value in values {
        *counts.entry(serde_json::to_vec(&value)?).or_default() += 1;
    }
    Ok(counts)
}

fn evidence_retained<T: serde::Serialize>(
    before: impl Iterator<Item = T>,
    after: impl Iterator<Item = T>,
) -> bool {
    let (Ok(before), Ok(after)) = (evidence_counts(before), evidence_counts(after)) else {
        return false;
    };
    before
        .into_iter()
        .all(|(key, count)| after.get(&key).copied().unwrap_or(0) >= count)
}

/// A whole-pass native fallback must not discard already observed annotations
/// or images. Index/file/caption are derived for each backend; source geometry,
/// image dimensions, format and captured pixel hashes must remain represented.
fn native_evidence_regression(
    native: &ExtractionResult,
    candidate: &ExtractionResult,
) -> Option<String> {
    for (before, after) in native.pages.iter().zip(&candidate.pages) {
        if !evidence_retained(before.links.iter(), after.links.iter()) {
            return Some(format!(
                "dropped native link targets or rectangles on page {}",
                before.page
            ));
        }
        let figure_key = |figure: &crate::schema::Figure| {
            (
                figure.bbox,
                figure.kind.clone(),
                figure.mime.clone(),
                figure.width_px,
                figure.height_px,
                figure.sha256.clone(),
            )
        };
        if !evidence_retained(
            before.figures.iter().map(figure_key),
            after.figures.iter().map(figure_key),
        ) {
            return Some(format!(
                "dropped native figure evidence on page {}",
                before.page
            ));
        }
    }
    None
}

/// Reject observable regressions before replacing a whole result. Keeping a
/// whole pass preserves its backend identity, figure paths, and derived data.
/// A partial fallback may improve an already partial page, but must not erase
/// decoded characters or make a previously complete page partial.
fn fallback_regression(native: &ExtractionResult, candidate: &ExtractionResult) -> Option<String> {
    if !matches!(candidate.status, Status::Complete | Status::Partial)
        || (candidate.status == Status::Complete
            && (candidate
                .pages
                .iter()
                .any(|page| page.extraction_status() != Status::Complete)
                || candidate
                    .chunks
                    .iter()
                    .any(|chunk| chunk.status != Status::Complete)))
    {
        return Some("returned an unsuccessful or inconsistent extraction status".to_string());
    }
    if native.document.hash != candidate.document.hash
        || native.document.size != candidate.document.size
    {
        return Some("read different source bytes".to_string());
    }
    if native.document.pages != candidate.document.pages
        || native.pages.len() != candidate.pages.len()
        || native
            .pages
            .iter()
            .zip(&candidate.pages)
            .any(|(before, after)| before.page != after.page)
    {
        return Some("changed the requested page coverage".to_string());
    }
    for (before, after) in native.pages.iter().zip(&candidate.pages) {
        let decoded_chars = |text: &str| {
            text.chars()
                .filter(|ch| !ch.is_whitespace() && *ch != '\u{fffd}')
                .count()
        };
        let before_chars = decoded_chars(&before.text);
        let after_chars = decoded_chars(&after.text);
        if (before.extraction_status() == Status::Complete
            && after.extraction_status() == Status::Partial)
            || (after_chars > 0 && after_chars < before_chars)
        {
            return Some(format!("regressed native text on page {}", before.page));
        }
        if after_chars == 0
            && (before_chars > 0
                || router::has_unmapped_text(before)
                || router::looks_scanned(before))
        {
            return Some(format!("did not recover text on page {}", before.page));
        }
    }
    None
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
        if extractor.provides_line_layout() {
            // The backend has projected lines while retaining its raw spans.
        } else if backend_order {
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
            if warning.starts_with("resource_limit:")
                || warning.starts_with("unicode_mapping:")
                || warning.starts_with("extraction_incomplete:")
            {
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
/// warning starting with `failed:`, `resource_limit:`, `unicode_mapping:`,
/// or `extraction_incomplete:`.
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

    #[derive(Clone)]
    struct PageExtractor(Vec<PageText>);

    impl DocumentSession for PageExtractor {
        fn page_count(&self) -> u32 {
            u32::try_from(self.0.len()).unwrap()
        }

        fn page_text(&mut self, page: u32) -> Result<PageText, BackendError> {
            Ok(self.0[page as usize - 1].clone())
        }

        fn info(&self) -> BTreeMap<String, String> {
            BTreeMap::new()
        }
    }

    impl Extractor for PageExtractor {
        fn identity(&self) -> BackendIdentity {
            FakeExtractor {
                reading_order: false,
            }
            .identity()
        }

        fn open(
            &self,
            _bytes: &[u8],
            _password: Option<&str>,
        ) -> Result<Box<dyn DocumentSession>, BackendError> {
            Ok(Box::new(self.clone()))
        }
    }

    fn run_pages(pages: Vec<PageText>) -> crate::schema::ExtractionResult {
        let dir = tempfile::tempdir().unwrap();
        run_job_with(
            &PageExtractor(pages),
            &fake_job(&fake_input(dir.path()), None),
        )
        .unwrap()
    }

    fn native_page(text: &str) -> PageText {
        let mut page = PageText::new(1, 612.0, 792.0, 0);
        if !text.is_empty() {
            page.spans.push(fake_span(text, 500.0, 0));
        }
        page
    }

    fn scanned_page() -> PageText {
        let mut page = native_page("");
        page.figures.push(Figure {
            index: 0,
            bbox: Some(BBox {
                x0: 0.0,
                y0: 0.0,
                x1: 612.0,
                y1: 792.0,
            }),
            kind: "raster".into(),
            mime: None,
            width_px: None,
            height_px: None,
            sha256: None,
            file: None,
            caption: None,
        });
        page
    }

    #[test]
    fn real_lopdf_scan_is_partial_but_a_blank_page_is_complete() {
        for scanned in [false, true] {
            let mut doc = Document::with_version("1.5");
            let tree_id = doc.new_object_id();
            let image_id = doc.add_object(Stream::new(
                dictionary! {
                    "Type" => "XObject", "Subtype" => "Image", "Width" => 1,
                    "Height" => 1, "ColorSpace" => "DeviceGray", "BitsPerComponent" => 8,
                },
                vec![255],
            ));
            let content = if scanned {
                b"q 612 0 0 792 0 0 cm /Scan Do Q".to_vec()
            } else {
                Vec::new()
            };
            let content_id = doc.add_object(Stream::new(dictionary! {}, content));
            let page_id = doc.add_object(dictionary! {
                "Type" => "Page", "Parent" => tree_id, "Contents" => content_id,
                "Resources" => dictionary! { "XObject" => dictionary! { "Scan" => image_id } },
            });
            doc.objects.insert(
                tree_id,
                Object::Dictionary(dictionary! {
                    "Type" => "Pages", "Count" => 1, "Kids" => vec![Object::Reference(page_id)],
                    "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
                }),
            );
            let catalog_id =
                doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => tree_id });
            doc.trailer.set("Root", catalog_id);
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("raster.pdf");
            doc.save(&path).unwrap();
            let mut job = lopdf_job(&path, None);
            for backend in ["lopdf", "auto"] {
                // Full OCR backends require their external models. The injected
                // route tests cover that transition without a model download.
                if backend == "auto" && cfg!(feature = "docling") {
                    continue;
                }
                job.backend = backend.into();
                let result = run_job(&job).unwrap();
                let expected = if scanned {
                    Status::Partial
                } else {
                    Status::Complete
                };
                assert_eq!(result.status, expected, "{backend}: {:?}", result.warnings);
                assert_eq!(result.pages[0].extraction_status(), expected);
                assert_eq!(result.chunks[0].status, expected);
            }
        }
    }

    #[test]
    fn scan_evidence_survives_pipeline_chunks_json_and_ledger() {
        let result = run_pages(vec![scanned_page()]);
        assert_eq!(result.status, Status::Partial);
        assert_eq!(result.pages[0].extraction_status(), Status::Partial);
        assert_eq!(result.chunks[0].status, Status::Partial);
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.starts_with("page 1: extraction_incomplete:"))
        );
        let json = serde_json::to_string(&result).unwrap();
        let restored: crate::schema::ExtractionResult = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, result);
        let mut ledger = crate::ledger::Ledger::open_in_memory().unwrap();
        let run = ledger.write_result(&result).unwrap();
        let stored = ledger.load_result(run).unwrap();
        assert_eq!(stored.status, Status::Partial);
        assert_eq!(stored.chunks[0].status, Status::Partial);
        assert_eq!(stored.pages[0].warnings, result.pages[0].warnings);
    }

    #[test]
    fn actual_blank_stays_complete() {
        let result = run_pages(vec![native_page("")]);
        assert_eq!(result.status, Status::Complete);
        assert_eq!(result.chunks[0].status, Status::Complete);
        assert!(result.warnings.is_empty());
    }

    #[test]
    fn replacement_text_is_partial_even_without_backend_warning() {
        let result = run_pages(vec![native_page("Readable words with \u{fffd} glyphs")]);
        assert_eq!(result.status, Status::Partial);
        assert_eq!(result.chunks[0].status, Status::Partial);
        assert!(
            result.pages[0]
                .warnings
                .iter()
                .any(|w| w.starts_with("unicode_mapping:"))
        );
    }

    #[test]
    fn absent_or_failed_fallback_retains_partial_native_text() {
        for scan in [false, true] {
            let original = run_pages(vec![if scan {
                scanned_page()
            } else {
                native_page("Text \u{fffd}")
            }]);
            for failed in [false, true] {
                let mut calls = 0;
                let routed = super::route_result(original.clone(), &mut |route| {
                    calls += 1;
                    assert_eq!(
                        route,
                        if scan {
                            crate::router::Route::Docling
                        } else {
                            crate::router::Route::Pdfium
                        }
                    );
                    if failed {
                        Err(BackendError::Unsupported("unavailable runtime".into()).into())
                    } else {
                        Ok(None)
                    }
                });
                assert_eq!(calls, 1);
                assert_eq!(routed.status, Status::Partial);
                assert_eq!(routed.backend, original.backend);
                assert_eq!(routed.pages, original.pages);
                assert_eq!(routed.chunks, original.chunks);
            }
        }
    }

    #[test]
    fn empty_fallback_cannot_clear_scan_or_unicode_evidence() {
        for page in [scanned_page(), native_page("Readable text \u{fffd}")] {
            let original = run_pages(vec![page]);
            let candidate = run_pages(vec![native_page("")]);
            let routed =
                super::route_result(original.clone(), &mut |_| Ok(Some(candidate.clone())));
            assert_eq!(routed.status, Status::Partial);
            assert_eq!(routed.pages, original.pages);
            assert_eq!(routed.backend, original.backend);
            assert!(
                routed
                    .warnings
                    .iter()
                    .any(|w| w.contains("did not recover text"))
            );
        }
    }

    #[test]
    fn fallback_cannot_drop_decoded_native_text_regardless_of_status() {
        let original = run_pages(vec![native_page(
            "Substantial native text with \u{fffd} glyphs",
        )]);
        for text in ["Short \u{fffd}", "Short"] {
            let candidate = run_pages(vec![native_page(text)]);
            let routed =
                super::route_result(original.clone(), &mut |_| Ok(Some(candidate.clone())));
            assert_eq!(routed.pages, original.pages);
            assert_eq!(routed.chunks, original.chunks);
            assert!(
                routed
                    .warnings
                    .iter()
                    .any(|w| w.contains("regressed native text"))
            );
        }
    }

    #[test]
    fn fallback_cannot_launder_failed_or_inconsistent_status() {
        let original = run_pages(vec![native_page("Readable \u{fffd}")]);
        for status in [Status::Failed, Status::Deferred, Status::Complete] {
            let mut candidate = original.clone();
            candidate.status = status;
            let routed =
                super::route_result(original.clone(), &mut |_| Ok(Some(candidate.clone())));
            assert_eq!(routed.pages, original.pages);
            assert_eq!(routed.status, Status::Partial);
            assert!(
                routed
                    .warnings
                    .iter()
                    .any(|w| w.contains("inconsistent extraction status"))
            );
        }
    }

    #[test]
    fn fallback_cannot_make_a_good_native_page_partial() {
        let mut second = native_page("Stable known readable native page");
        second.page = 2;
        let original = run_pages(vec![native_page("Unreadable \u{fffd}"), second.clone()]);
        second.warnings.push("resource_limit: truncated".into());
        let candidate = run_pages(vec![native_page("Recovered native text"), second]);
        let routed = super::route_result(original.clone(), &mut |_| Ok(Some(candidate.clone())));
        assert_eq!(routed.pages, original.pages);
        assert!(routed.warnings.iter().any(|w| w.contains("page 2")));
    }

    #[test]
    fn complete_fallback_cannot_drop_good_native_page_text() {
        let mut second = native_page("Stable known readable native page");
        second.page = 2;
        let original = run_pages(vec![native_page("Unreadable \u{fffd}"), second]);
        let mut shorter = native_page("Lost");
        shorter.page = 2;
        let candidate = run_pages(vec![native_page("Recovered native text"), shorter]);
        assert_eq!(candidate.status, Status::Complete);
        let routed = super::route_result(original.clone(), &mut |_| Ok(Some(candidate.clone())));
        assert_eq!(routed.pages, original.pages);
        assert_eq!(routed.status, Status::Partial);
        assert!(
            routed
                .warnings
                .iter()
                .any(|w| w.contains("regressed native text on page 2"))
        );
    }

    #[test]
    fn mixed_scan_and_unicode_evidence_cannot_jump_to_ocr() {
        let mut second = scanned_page();
        second.page = 2;
        let original = run_pages(vec![native_page("Readable \u{fffd}"), second]);
        let mut recovered = native_page("Scanned page text has now been recovered");
        recovered.page = 2;
        let mut candidate = run_pages(vec![native_page("Readable \u{fffd}"), recovered]);
        candidate.backend.name = "pdfium".into();
        let mut calls = 0;
        let routed = super::route_result(original, &mut |route| {
            calls += 1;
            assert_eq!(route, crate::router::Route::Pdfium);
            Ok(Some(candidate.clone()))
        });
        assert_eq!(calls, 1);
        assert_eq!(routed.backend.name, "pdfium");
        assert_eq!(routed.status, Status::Partial);
        assert!(
            routed
                .warnings
                .iter()
                .any(|w| w.starts_with("unresolved: pdfium Unicode mapping"))
        );
    }

    #[test]
    fn mapped_native_fallback_can_complete_a_partial_page() {
        let original = run_pages(vec![native_page("Readable \u{fffd}")]);
        let mut candidate = run_pages(vec![native_page("Readable repaired text")]);
        candidate.backend.name = "pdfium".into();
        let routed = super::route_result(original, &mut |_| Ok(Some(candidate.clone())));
        assert_eq!(routed.backend.name, "pdfium");
        assert_eq!(routed.status, Status::Complete);
        assert_eq!(routed.pages, candidate.pages);
    }

    #[test]
    fn fallback_must_read_the_same_source_and_page_range() {
        let original = run_pages(vec![native_page("Text \u{fffd}")]);
        for different_source in [false, true] {
            let mut candidate = original.clone();
            if different_source {
                candidate.document.hash = ContentHash(sha256_hex(b"changed"));
            } else {
                candidate.pages[0].page = 2;
            }
            let routed =
                super::route_result(original.clone(), &mut |_| Ok(Some(candidate.clone())));
            assert_eq!(routed.pages, original.pages);
            assert_eq!(routed.document, original.document);
            assert!(
                routed
                    .warnings
                    .iter()
                    .any(|w| w.starts_with("route not taken:"))
            );
        }
    }

    #[test]
    fn native_cascade_policy_is_explicit_and_has_a_distinct_ledger_identity() {
        use std::ffi::OsStr;
        assert!(!super::native_fallback_enabled(None).unwrap());
        assert!(!super::native_fallback_enabled(Some(OsStr::new("0"))).unwrap());
        assert!(super::native_fallback_enabled(Some(OsStr::new("1"))).unwrap());
        for value in ["", "true", "2", " 1"] {
            assert!(super::native_fallback_enabled(Some(OsStr::new(value))).is_err());
        }
        let original = run_pages(vec![native_page("Mapped source text")]);
        let mut enabled = original.clone();
        super::record_native_fallback_policy(&mut enabled);
        assert_ne!(
            enabled.backend.config_digest,
            original.backend.config_digest
        );
        assert_eq!(enabled.backend.name, original.backend.name);
        assert_eq!(enabled.backend.version, original.backend.version);
        assert_eq!(enabled.pages, original.pages);
        let mut repeated = original;
        super::record_native_fallback_policy(&mut repeated);
        assert_eq!(enabled.backend, repeated.backend);
    }

    #[test]
    fn native_cascade_recovers_after_missing_failed_or_regressing_pdfium() {
        use crate::router::Route;
        for failure in ["missing", "failed", "regressing"] {
            let original = run_pages(vec![native_page("Readable source text \u{fffd}")]);
            let mut repaired = run_pages(vec![native_page("Readable source text repaired")]);
            repaired.backend.name = "mupdf".into();
            let mut calls = Vec::new();
            let result = super::route_result_with_policy(original.clone(), true, &mut |route| {
                calls.push(route);
                match route {
                    Route::Pdfium if failure == "missing" => Ok(None),
                    Route::Pdfium if failure == "failed" => {
                        Err(BackendError::Unsupported("native open failed".into()).into())
                    }
                    Route::Pdfium => Ok(Some(run_pages(vec![native_page("")]))),
                    Route::MuPdf => Ok(Some(repaired.clone())),
                    _ => panic!("successful native recovery must stop the cascade"),
                }
            });
            assert_eq!(calls, [Route::Pdfium, Route::MuPdf]);
            assert_eq!(result.pages, repaired.pages);
            assert_eq!(result.status, Status::Complete);
            assert_eq!(result.backend.name, "mupdf");
            assert!(
                result
                    .warnings
                    .iter()
                    .any(|warning| warning.starts_with("route not taken: pdfium"))
            );
            assert!(
                result
                    .warnings
                    .iter()
                    .any(|warning| warning.starts_with("routing evidence:"))
            );
        }
    }

    #[test]
    fn native_cascade_preserves_best_partial_result_and_never_enters_ocr() {
        use crate::router::Route;
        let original = run_pages(vec![native_page("Source \u{fffd}")]);
        let mut pdfium = run_pages(vec![native_page("Longer PDFium source \u{fffd}")]);
        pdfium.backend.name = "pdfium".into();
        let mut mupdf = run_pages(vec![native_page("Even longer MuPDF source text \u{fffd}")]);
        mupdf.backend.name = "mupdf".into();
        let mut calls = Vec::new();
        let result = super::route_result_with_policy(original, true, &mut |route| {
            calls.push(route);
            match route {
                Route::Pdfium => Ok(Some(pdfium.clone())),
                Route::MuPdf => Ok(Some(mupdf.clone())),
                Route::Poppler => Err(BackendError::Unsupported("runtime failure".into()).into()),
                _ => panic!("unresolved native mapping cannot enter OCR"),
            }
        });
        assert_eq!(calls, [Route::Pdfium, Route::MuPdf, Route::Poppler]);
        assert_eq!(result.backend.name, "mupdf");
        assert_eq!(result.pages, mupdf.pages);
        assert_eq!(result.chunks, mupdf.chunks);
        assert_eq!(result.status, Status::Partial);
        assert!(
            result
                .warnings
                .iter()
                .any(|warning| warning.starts_with("route not taken: poppler failed:"))
        );
        assert!(
            result
                .warnings
                .iter()
                .any(|warning| warning.starts_with("routing evidence: pdfium"))
        );
        assert!(
            result
                .warnings
                .iter()
                .any(|warning| warning.starts_with("routed: pdfium"))
        );
    }

    #[test]
    fn native_cascade_tries_poppler_once_after_mupdf_cannot_improve() {
        use crate::router::Route;
        let original = run_pages(vec![native_page("Readable \u{fffd}")]);
        let mut candidate = run_pages(vec![native_page("Readable recovered source characters")]);
        candidate.backend.name = "poppler".into();
        let mut calls = Vec::new();
        let result = super::route_result_with_policy(original.clone(), true, &mut |route| {
            calls.push(route);
            match route {
                Route::Pdfium => Ok(None),
                Route::MuPdf => Ok(Some(original.clone())),
                Route::Poppler => Ok(Some(candidate.clone())),
                _ => panic!("native route budget exceeded"),
            }
        });
        assert_eq!(calls, [Route::Pdfium, Route::MuPdf, Route::Poppler]);
        assert_eq!(result.backend.name, "poppler");
        assert_eq!(result.status, Status::Complete);
        assert!(
            result
                .warnings
                .iter()
                .any(|warning| warning.contains("mupdf did not improve"))
        );
    }

    #[test]
    fn native_cascade_cannot_change_source_coverage_status_or_known_evidence() {
        use crate::router::Route;
        let mut page = native_page("Readable native source \u{fffd}");
        page.links.push(crate::schema::Link {
            uri: "https://doi.org/10.1000/reference".into(),
            bbox: page.spans[0].bbox,
        });
        page.figures.push(Figure {
            index: 0,
            bbox: page.spans[0].bbox,
            kind: "raster".into(),
            mime: Some("image/png".into()),
            width_px: Some(20),
            height_px: Some(30),
            sha256: Some(sha256_hex(b"native image")),
            file: None,
            caption: None,
        });
        let original = run_pages(vec![page]);
        for damage in [
            "hash",
            "size",
            "total_pages",
            "selected_page",
            "failed",
            "status",
            "text",
            "link",
            "rectangle",
            "image",
            "image_hash",
        ] {
            let mut candidate = original.clone();
            candidate.backend.name = "mupdf".into();
            match damage {
                "hash" => candidate.document.hash = ContentHash(sha256_hex(b"different input")),
                "size" => candidate.document.size += 1,
                "total_pages" => candidate.document.pages += 1,
                "selected_page" => candidate.pages[0].page += 1,
                "failed" => candidate.status = Status::Failed,
                "status" => candidate.status = Status::Complete,
                "text" => candidate.pages[0].text = "short".into(),
                "link" => candidate.pages[0].links.clear(),
                "rectangle" => candidate.pages[0].links[0].bbox = None,
                "image" => candidate.pages[0].figures.clear(),
                "image_hash" => candidate.pages[0].figures[0].sha256 = None,
                _ => unreachable!(),
            }
            let mut calls = Vec::new();
            let result = super::route_result_with_policy(original.clone(), true, &mut |route| {
                calls.push(route);
                match route {
                    Route::Pdfium | Route::Poppler => Ok(None),
                    Route::MuPdf => Ok(Some(candidate.clone())),
                    _ => panic!("native route budget exceeded"),
                }
            });
            assert_eq!(calls, [Route::Pdfium, Route::MuPdf, Route::Poppler]);
            assert_eq!(result.backend, original.backend, "{damage}");
            assert_eq!(result.pages, original.pages, "{damage}");
            assert_eq!(result.chunks, original.chunks, "{damage}");
            assert!(
                result
                    .warnings
                    .iter()
                    .any(|warning| warning.starts_with("route not taken: mupdf")),
                "{damage}"
            );
        }
    }

    #[test]
    fn native_cascade_does_not_drop_link_evidence_during_pdfium_replacement() {
        use crate::router::Route;
        let mut page = native_page("Readable native source \u{fffd}");
        page.links.push(crate::schema::Link {
            uri: "https://doi.org/10.1000/reference".into(),
            bbox: page.spans[0].bbox,
        });
        let original = run_pages(vec![page]);
        let candidate = run_pages(vec![native_page(
            "Readable native source repaired by PDFium",
        )]);
        let result =
            super::route_result_with_policy(original.clone(), true, &mut |route| match route {
                Route::Pdfium => Ok(Some(candidate.clone())),
                Route::MuPdf | Route::Poppler => Ok(None),
                _ => panic!("unexpected route"),
            });
        assert_eq!(result.pages, original.pages);
        assert!(
            result
                .warnings
                .iter()
                .any(|warning| warning.contains("pdfium dropped native link"))
        );
    }

    #[test]
    fn native_cascade_keeps_mapped_lopdf_and_successfully_repaired_pdfium() {
        use crate::router::Route;
        let mapped = run_pages(vec![native_page("Known mapped native source")]);
        let result = super::route_result_with_policy(mapped.clone(), true, &mut |_| {
            panic!("mapped lopdf should not run another backend")
        });
        assert_eq!(result, mapped);
        let original = run_pages(vec![native_page("Source \u{fffd}")]);
        let mut calls = Vec::new();
        let result = super::route_result_with_policy(original, true, &mut |route| {
            calls.push(route);
            assert_eq!(route, Route::Pdfium);
            Ok(Some(mapped.clone()))
        });
        assert_eq!(calls, [Route::Pdfium]);
        assert_eq!(result.pages, mapped.pages);
        assert_eq!(result.status, Status::Complete);
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
    fn zero_page_document_returns_an_error_without_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let result = run_job_with(
            &PageExtractor(Vec::new()),
            &fake_job(&fake_input(dir.path()), None),
        );
        assert!(matches!(
            result,
            Err(PipelineError::Backend(BackendError::PageRange {
                page: 1,
                count: 0
            }))
        ));
    }

    #[test]
    fn reversed_page_range_returns_an_error_without_panicking() {
        let (_dir, path) = three_page_fixture();
        for pages in [(3, 1), (1, 0)] {
            assert!(run_job(&lopdf_job(&path, Some(pages))).is_err());
        }
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
