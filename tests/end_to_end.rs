//! End-to-end tests: synthetic paper -> `run_job` -> ledger round trip.

mod common;

use std::path::Path;

use tpe::backend::BackendError;
use tpe::ledger::Ledger;
use tpe::pipeline::{PipelineError, chunk_results, run_job};
use tpe::schema::{Job, PageText, Status};

use common::{AUTHORS, DOI, TITLE, synthetic_paper, write_temp_pdf};

/// A job for the default backend over `path`.
fn job_for(path: &Path, pages: Option<(u32, u32)>) -> Job {
    Job {
        path: path.to_string_lossy().into_owned(),
        backend: "lopdf".to_string(),
        pages,
        password: None,
        max_bytes: None,
    }
}

#[test]
fn synthetic_paper_is_a_pdf() {
    let bytes = synthetic_paper();
    assert!(bytes.starts_with(b"%PDF-1.5"), "fixture must be a PDF");
}

#[test]
fn extracts_synthetic_paper_end_to_end() {
    let (_dir, path) = write_temp_pdf(&synthetic_paper());
    let result = run_job(&job_for(&path, None)).expect("pipeline succeeds");

    assert!(result.warnings.is_empty(), "{:?}", result.warnings);
    assert_eq!(result.status, Status::Complete);
    assert_eq!(result.document.pages, 2);
    assert_eq!(result.pages.len(), 2);
    assert_eq!(result.chunks.len(), 1);
    let chunk = &result.chunks[0];
    assert_eq!((chunk.first_page, chunk.last_page), (1, 2));
    assert_eq!(chunk.status, Status::Complete);

    let page1 = &result.pages[0].text;
    assert!(page1.contains(TITLE), "page 1 lacks title:\n{page1}");
    assert!(page1.contains("Abstract"), "missing Abstract:\n{page1}");
    let left = page1
        .find("Academic PDFs encode text")
        .expect("left column text present");
    let right = page1
        .find("Our contribution is a pipeline")
        .expect("right column text present");
    assert!(left < right, "left column must come first:\n{page1}");

    let meta = &result.metadata;
    assert_eq!(meta.title.as_deref(), Some(TITLE), "metadata: {meta:#?}");
    let names: Vec<&str> = meta.authors.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names, AUTHORS, "metadata: {meta:#?}");
    assert_eq!(meta.doi.as_deref(), Some(DOI), "metadata: {meta:#?}");

    let refs = &result.references;
    assert_eq!(refs.len(), 3, "references: {refs:#?}");
    let indices: Vec<u32> = refs.iter().map(|r| r.index).collect();
    assert_eq!(indices, [1, 2, 3]);
    let entry = &refs[1];
    assert_eq!(entry.doi.as_deref(), Some("10.1000/abc456"), "{entry:#?}");
    assert_eq!(entry.year, Some(1952), "{entry:#?}");
    assert_eq!(entry.volume.as_deref(), Some("3"), "{entry:#?}");
    let entry = &refs[2];
    assert_eq!(entry.arxiv_id.as_deref(), Some("1936.00001"), "{entry:#?}");

    let cites = &result.citations;
    let multi = cites
        .iter()
        .find(|c| c.text == "[2, 3]")
        .expect("marker [2, 3] found");
    assert_eq!(multi.targets, [2, 3], "citations: {cites:#?}");
    let single = cites
        .iter()
        .find(|c| c.text == "[1]")
        .expect("marker [1] found");
    assert_eq!(single.targets, [1], "citations: {cites:#?}");
}

#[test]
fn ledger_round_trips_the_result() {
    let (_dir, path) = write_temp_pdf(&synthetic_paper());
    let result = run_job(&job_for(&path, None)).expect("pipeline succeeds");

    let mut ledger = Ledger::open_in_memory().expect("in-memory ledger opens");
    let source = result.document.sources.first().expect("one source");
    ledger
        .record_source(&result.document.hash, result.document.size, source)
        .expect("source recorded");
    let run = ledger.write_result(&result).expect("result written");
    let loaded = ledger.load_result(run).expect("result loaded");
    assert_eq!(loaded, result);

    let stats = ledger.stats().expect("stats available");
    assert_eq!(stats.references, 3);
    assert_eq!(stats.runs, 1);
    assert_eq!(stats.documents, 1);

    let found = ledger
        .find_run(&result.document.hash, &result.backend)
        .expect("find_run succeeds");
    assert_eq!(found.map(|summary| summary.id), Some(run));
}

#[test]
fn running_twice_writes_one_run() {
    let (_dir, path) = write_temp_pdf(&synthetic_paper());
    let job = job_for(&path, None);
    let first = run_job(&job).expect("first run succeeds");
    let second = run_job(&job).expect("second run succeeds");
    assert_eq!(first.document.hash, second.document.hash);

    let mut ledger = Ledger::open_in_memory().expect("in-memory ledger opens");
    for result in [&first, &second] {
        let source = result.document.sources.first().expect("one source");
        ledger
            .record_source(&result.document.hash, result.document.size, source)
            .expect("source recorded");
        ledger.write_result(result).expect("result written");
    }

    let stats = ledger.stats().expect("stats available");
    assert_eq!(stats.runs, 1);
    assert_eq!(stats.documents, 1);
    assert_eq!(stats.references, 3);
}

#[test]
fn non_pdf_input_is_malformed() {
    let (_dir, path) = write_temp_pdf(b"this is not a pdf file\n");
    let err = run_job(&job_for(&path, None)).expect_err("non-PDF input must fail");
    assert!(
        matches!(err, PipelineError::Backend(BackendError::Malformed(_))),
        "expected Malformed, got {err:?}"
    );
}

#[test]
fn page_range_selects_a_single_page() {
    let (_dir, path) = write_temp_pdf(&synthetic_paper());
    let result = run_job(&job_for(&path, Some((2, 2)))).expect("pipeline succeeds");
    assert_eq!(result.pages.len(), 1);
    assert_eq!(result.pages[0].page, 2);
    assert_eq!(result.document.pages, 2);
    assert_eq!(result.chunks.len(), 1);
    let chunk = &result.chunks[0];
    assert_eq!((chunk.first_page, chunk.last_page), (2, 2));
    let text = &result.pages[0].text;
    assert!(text.contains("References"), "missing References:\n{text}");
}

#[test]
fn page_range_beyond_document_is_an_error() {
    let (_dir, path) = write_temp_pdf(&synthetic_paper());
    let err = run_job(&job_for(&path, Some((5, 9)))).expect_err("range past the end fails");
    assert!(
        matches!(
            err,
            PipelineError::Backend(BackendError::PageRange { page: 5, count: 2 })
        ),
        "expected PageRange, got {err:?}"
    );
}

#[test]
fn forty_five_pages_chunk_into_three() {
    let pages: Vec<PageText> = (1..=45)
        .map(|n| PageText::new(n, 612.0, 792.0, 0))
        .collect();
    let chunks = chunk_results(&pages, 45.0);
    assert_eq!(chunks.len(), 3);
    let bounds: Vec<(u32, u32)> = chunks
        .iter()
        .map(|chunk| (chunk.first_page, chunk.last_page))
        .collect();
    assert_eq!(bounds, [(1, 20), (21, 40), (41, 45)]);
    assert!(chunks.iter().all(|c| c.status == Status::Complete));
}
