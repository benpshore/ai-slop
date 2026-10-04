//! Actual retained spans: two column headings must stay with their prose.
use std::io::Read;

use flate2::read::GzDecoder;
use serde::Deserialize;
use tpe::{citations, eval, latex_refs, reading_order, regions, schema, text_cleanup};

#[derive(Deserialize)]
struct Fixture {
    pages: Vec<schema::PageText>,
    truth: Vec<latex_refs::TruthReference>,
}

fn check_fixture(bytes: &[u8]) {
    let mut json = Vec::new();
    GzDecoder::new(bytes).read_to_end(&mut json).unwrap();
    let mut fixture: Fixture = serde_json::from_slice(&json).unwrap();
    assert_eq!(fixture.truth.len(), 29);
    let original_spans: Vec<_> = fixture
        .pages
        .iter()
        .map(|page| page.spans.clone())
        .collect();
    for page in &mut fixture.pages {
        reading_order::order_page(page);
    }
    text_cleanup::clean_document(&mut fixture.pages);
    regions::tag_regions(&mut fixture.pages);
    let page = &fixture.pages[0];
    assert_eq!(page.page, 8);
    let references = page
        .lines
        .iter()
        .position(|line| line.text == "References")
        .unwrap();
    let acknowledgments = page
        .lines
        .iter()
        .position(|line| line.text == "Acknowledgments")
        .unwrap();
    assert!(
        references > acknowledgments,
        "References precedes the other column's prose"
    );
    assert!(
        page.lines[references + 1]
            .text
            .starts_with("Albert Bandura.")
    );
    for (page, spans) in fixture.pages.iter().zip(original_spans) {
        assert_eq!(page.spans, spans, "backend spans must remain unmodified");
    }
    let (entries, _) = citations::extract_citations(&fixture.pages);
    let matches = eval::match_references(&fixture.truth, &entries);
    assert_eq!(entries.len(), 29);
    assert!(matches.iter().all(|entry| entry.extracted_index.is_some()));
    assert!(entries[0].raw.starts_with("Albert Bandura. 2013."));
    assert!(entries.iter().all(|entry| {
        !entry.raw.contains("ethical considerations")
            && !entry.raw.contains("INTEVAL is designed")
            && !entry.raw.contains("current experiments")
            && !entry.raw.contains("HPC infrastructure")
    }));
}

#[test]
fn pdfium_column_headings_preserve_all_29_references() {
    check_fixture(include_bytes!(
        "fixtures/bibliography/arxiv-2502.00857-pdfium-spans.json.gz"
    ));
}

#[test]
fn docling_text_column_headings_preserve_all_29_references() {
    check_fixture(include_bytes!(
        "fixtures/bibliography/arxiv-2502.00857-docling-text-spans.json.gz"
    ));
}
