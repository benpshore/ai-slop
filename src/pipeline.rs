//! Glue that runs the extraction stages for one document: acquire the bytes,
//! open a backend session, extract every requested page, order the spans,
//! summarise chunks, then derive metadata and citations. The ledger write is
//! left to the caller so that one thread can own the database.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Instant;

use thiserror::Error;

use crate::acquire::{self, AcquireError};
use crate::backend::{self, BackendError};
use crate::citations;
use crate::metadata;
use crate::reading_order;
use crate::schema::{
    CHUNK_PAGES, ChunkResult, Document, ExtractionResult, Job, PageText, SCHEMA_VERSION,
    StageTimings, Status, sha256_hex,
};

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

/// Run every stage for one document and return the complete result.
///
/// A `BackendError::Page` for one page is recorded as a warning prefixed with
/// `failed:` (on the result and on a placeholder `PageText` for that page so
/// chunking stays stable) and the status becomes `Partial`. Any other backend
/// error aborts the job.
pub fn run_job(job: &Job) -> Result<ExtractionResult, PipelineError> {
    let extractor = backend::by_name(&job.backend)
        .ok_or_else(|| PipelineError::UnknownBackend(job.backend.clone()))?;
    let identity = extractor.identity();

    let mut timings = StageTimings::default();

    let acquire_start = Instant::now();
    let snapshot = acquire::snapshot(Path::new(&job.path), job.max_bytes)?;
    timings.acquire_ms = elapsed_ms(acquire_start);

    let parse_start = Instant::now();
    let mut session = extractor.open(&snapshot.bytes, job.password.as_deref())?;
    let page_count = session.page_count();
    let (first, last) = resolve_page_range(job.pages, page_count)?;

    let mut pages: Vec<PageText> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut status = Status::Complete;
    for page in first..=last {
        match session.page_text(page) {
            Ok(text) => pages.push(text),
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
    }
    let info: BTreeMap<String, String> = session.info();
    timings.parse_ms = elapsed_ms(parse_start);

    let order_start = Instant::now();
    for page in &mut pages {
        reading_order::order_page(page);
    }
    timings.order_ms = elapsed_ms(order_start);

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
/// warning starting with `failed:`.
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
            if page.warnings.iter().any(|w| w.starts_with("failed:")) {
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
    use super::{PipelineError, chunk_results, resolve_page_range, run_job};
    use crate::backend::BackendError;
    use crate::schema::{Job, PageText, Status, sha256_hex};

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
        };
        match run_job(&job) {
            Err(PipelineError::UnknownBackend(name)) => assert_eq!(name, "no-such-backend"),
            other => panic!("expected UnknownBackend, got {other:?}"),
        }
    }
}
