//! Offline replay of retained backend spans; never a new PDF extraction.
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde_json::json;
use tpe::{citations, eval, reading_order, regions, schema, text_cleanup};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    ensure!(
        args.len() == 2,
        "usage: bibliography_replay CASE_DIRECTORY OUTPUT_DIRECTORY"
    );
    let input = Path::new(&args[0]);
    let output = Path::new(&args[1]);
    ensure!(!output.exists(), "output directory must not exist");
    let source_result = fs::read(input.join("result.json"))?;
    let source_dump = fs::read(input.join("dump.json"))?;
    let mut result: schema::ExtractionResult = serde_json::from_slice(&source_result)?;
    ensure!(
        matches!(result.backend.name.as_str(), "pdfium" | "docling-text"),
        "replay supports PDFium and Docling-text only; full Docling requires its adapter-projected lines"
    );
    let dump: eval::PaperDump = serde_json::from_slice(&source_dump)?;
    let original_references = result.references.len();
    let source_page_warnings: Vec<_> = result
        .pages
        .iter()
        .map(|page| (page.page, page.warnings.clone()))
        .collect();
    for page in &mut result.pages {
        page.lines.clear();
        page.text.clear();
        page.warnings.clear();
        reading_order::order_page(page);
    }
    text_cleanup::clean_document(&mut result.pages);
    regions::tag_regions(&mut result.pages);
    (result.references, result.citations) = citations::extract_citations(&result.pages);
    let pairings = eval::match_references(&dump.truth, &result.references);
    let matched_count = pairings
        .iter()
        .filter(|entry| entry.extracted_index.is_some())
        .count();
    let matched_entries: BTreeSet<_> = pairings
        .iter()
        .filter_map(|entry| entry.extracted_index)
        .collect();
    let summary = json!({
        "scope": "offline retained-span replay, not a fresh PDF extraction",
        "source_case": input.to_str().context("non-UTF8 input path")?,
        "source_case_id": dump.id,
        "source_result_sha256": schema::sha256_hex(&source_result),
        "source_dump_sha256": schema::sha256_hex(&source_dump),
        "source_document": result.document,
        "source_backend": result.backend,
        "source_status": result.status,
        "truth": dump.truth.len(), "original_extracted": original_references,
        "original_matched": dump.matches.iter().filter(|entry| entry.extracted_index.is_some()).count(),
        "extracted": result.references.len(), "matched": matched_count,
        "spurious": result.references.iter().filter(|entry| !matched_entries.contains(&entry.index)).count(),
        "first_reference": result.references.first().map(|entry| &entry.raw),
        "matches": pairings,
    });
    fs::create_dir_all(output)?;
    serde_json::to_writer_pretty(File::create(output.join("summary.json"))?, &summary)?;
    serde_json::to_writer_pretty(
        File::create(output.join("replay.json"))?,
        &json!({
            "scope": "offline order/cleanup/regions/citations replay; no fresh native parse, extraction status, metadata or timings",
            "source_result_sha256": schema::sha256_hex(&source_result),
            "source_page_warnings": source_page_warnings,
            "pages": result.pages, "references": result.references, "citations": result.citations,
        }),
    )?;
    println!("{}", serde_json::to_string(&summary)?);
    Ok(())
}
