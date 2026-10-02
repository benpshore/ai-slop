//! Regressions for accepting `PDFium`'s character-code echoes as repaired Unicode.
#![cfg(feature = "pdfium")]

use std::path::Path;

use tpe::backend::{Extractor, pdfium_backend::PdfiumBackend};
use tpe::pipeline::run_job;
use tpe::router;
use tpe::schema::{ExtractionResult, Job, Status};

fn native_available() -> bool {
    if std::env::var_os("PDFIUM_DYNAMIC_LIB_PATH").is_none() {
        eprintln!("skipped: set PDFIUM_DYNAMIC_LIB_PATH for real PDFium Unicode regressions");
        return false;
    }
    true
}

fn fixture(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/pdfium-unicode")
        .join(name)
}

fn extract(name: &str, backend: &str) -> ExtractionResult {
    run_job(&Job {
        path: fixture(name).to_string_lossy().into_owned(),
        backend: backend.into(),
        pages: None,
        password: None,
        max_bytes: None,
        figures_dir: None,
    })
    .unwrap()
}

#[test]
fn pdfium_mapping_failures_survive_adapter_result_and_json() {
    if !native_available() {
        return;
    }
    let bytes = std::fs::read(fixture("partial-cmap.pdf")).unwrap();
    let mut session = PdfiumBackend::default().open(&bytes, None).unwrap();
    let page = session.page_text(1).unwrap();
    assert!(router::has_unmapped_text(&page), "{:?}", page.warnings);
    assert!(page.warnings.iter().any(|w| w.contains("map_errors=11")));
    assert!(!page.spans.is_empty(), "retain usable native output");

    let result = extract("partial-cmap.pdf", "pdfium");
    assert_eq!(result.status, Status::Partial);
    assert_eq!(result.pages[0].extraction_status(), Status::Partial);
    assert_eq!(result.chunks[0].status, Status::Partial);
    assert!(!result.pages[0].text.is_empty());
    // Render inspection established ALPHA BETA GAMMA; plausible output is not recovery.
    assert_ne!(result.pages[0].text.trim(), "ALPHA BETA GAMMA");
    assert!(
        result
            .warnings
            .iter()
            .any(|w| w.starts_with("page 1: unicode_mapping:"))
    );
    assert_eq!(
        router::assess(&result.pages).route_after_pdfium(),
        router::Route::Docling
    );
    let json = serde_json::to_value(&result).unwrap();
    assert_eq!(json["status"], "partial");
    assert_eq!(json["chunks"][0]["status"], "partial");
    assert!(
        json["pages"][0]["warnings"]
            .to_string()
            .contains("map_errors=11")
    );
}

#[test]
fn zero_unicode_is_reported_even_when_object_text_omits_it() {
    if !native_available() {
        return;
    }
    let mut doc = lopdf::Document::load(fixture("partial-cmap.pdf")).unwrap();
    // Change only the CMap's A mapping to NUL, including when its stream is compressed.
    let source = b"<0024> <0041>";
    let mut changed = false;
    for object in doc.objects.values_mut() {
        if let Ok(stream) = object.as_stream_mut() {
            let mut content = stream.get_plain_content().unwrap();
            if let Some(offset) = content.windows(source.len()).position(|w| w == source) {
                content[offset..offset + source.len()].copy_from_slice(b"<0024> <0000>");
                *stream = lopdf::Stream::new(lopdf::Dictionary::new(), content);
                changed = true;
                break;
            }
        }
    }
    assert!(changed, "fixture must contain the single A mapping");
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    let mut session = PdfiumBackend::default().open(&bytes, None).unwrap();
    let page = session.page_text(1).unwrap();
    assert!(
        page.warnings.iter().any(|w| w.contains("zero_unicode=5")),
        "{:?}",
        page.warnings
    );
    assert_eq!(page.extraction_status(), Status::Partial);
    assert!(router::has_unmapped_text(&page));
    assert!(!page.spans.is_empty());
}

#[test]
fn auto_does_not_accept_unresolved_pdfium_mapping_as_complete() {
    if !native_available() {
        return;
    }
    let result = extract("partial-cmap.pdf", "auto");
    assert_eq!(result.backend.name, "pdfium");
    assert_eq!(result.status, Status::Partial);
    assert_eq!(result.chunks[0].status, Status::Partial);
    assert!(
        result
            .warnings
            .iter()
            .any(|w| w.starts_with("page 1: unicode_mapping:"))
    );
    assert!(
        result
            .warnings
            .iter()
            .any(|w| w.starts_with("unresolved: pdfium Unicode mapping; native text retained"))
    );
    assert!(!result.pages[0].text.is_empty());
}

#[test]
fn normal_text_and_existing_invisible_ocr_remain_complete() {
    if !native_available() {
        return;
    }
    for name in ["native.pdf", "existing-ocr.pdf"] {
        for backend in ["pdfium", "auto"] {
            let result = extract(name, backend);
            assert_eq!(
                result.status,
                Status::Complete,
                "{name} {backend}: {:?}",
                result.warnings
            );
            assert_eq!(result.chunks[0].status, Status::Complete);
            assert!(!router::has_unmapped_text(&result.pages[0]));
            assert_eq!(
                result.backend.name,
                if backend == "auto" { "lopdf" } else { "pdfium" }
            );
            for line in [
                "Faithful native text remains available.",
                "Existing OCR already reads this sentence.",
                "Numbers 12345 and alpha beta gamma.",
            ] {
                assert!(
                    result.pages[0].text.contains(line),
                    "{name} {backend}: {:?}",
                    result.pages[0].text
                );
            }
        }
    }
}
