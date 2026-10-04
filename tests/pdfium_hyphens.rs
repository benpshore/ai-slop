//! Public-paper regression, opt-in so the corpus is never fetched by a test.
#![cfg(feature = "pdfium")]

use pdfium_render::prelude::*;

use tpe::backend::pdfium_backend::PdfiumBackend;
use tpe::pipeline::run_job_with;
use tpe::schema::{Job, sha256_hex};

#[test]
#[ignore = "requires the pinned public 2511.15503v5 PDF via TPE_HYPHEN_FIXTURE"]
fn pinned_public_references_preserve_text_and_parse_fields() {
    let path = std::env::var("TPE_HYPHEN_FIXTURE").expect("set TPE_HYPHEN_FIXTURE to 2511.15503v5");
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(
        sha256_hex(&bytes),
        "5682c7c0c805b3145b15eb5b55a1db81d491211b7fc0155cc919462c657ad349"
    );
    assert_native_krishnamurthy_marker(&bytes);
    let job = Job {
        path,
        backend: "pdfium".into(),
        pages: None,
        password: None,
        max_bytes: None,
        figures_dir: None,
    };
    let result = run_job_with(&PdfiumBackend::default(), &job).unwrap();
    assert_eq!(result.pages.len(), 19);
    assert_eq!(result.references.len(), 139);
    for (index, name, title) in [
        (
            73,
            "A. Krishnamurthy",
            "Learning to Optimize Tensor Programs",
        ),
        (
            137,
            "M. Interlandi",
            "A Tensor Compiler for Unified Machine Learning Prediction Serving",
        ),
    ] {
        let entry = result
            .references
            .iter()
            .find(|entry| entry.index == index)
            .unwrap();
        assert_eq!(entry.label.as_deref(), Some(format!("[{index}]").as_str()));
        assert!(!entry.raw.contains('\u{0002}'), "{}", entry.raw);
        assert!(entry.raw.contains(name), "{}", entry.raw);
        assert_eq!(entry.title.as_deref(), Some(title));
        assert!(
            entry.authors.iter().any(|author| author == name),
            "{:?}",
            entry.authors
        );
        eprintln!(
            "reference {index}: {}",
            serde_json::to_string(entry).unwrap()
        );
    }
    let text = result
        .pages
        .iter()
        .map(|page| page.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!text.contains('\u{0002}'));
    assert!(
        text.contains("Processing-in-Memory"),
        "genuine compound hyphens survive"
    );
    // PR173 searches the complete superscript window instead of truncating at
    // 256 candidates. Mapping uncertainty is separate and must remain Partial.
    assert!(
        !result
            .warnings
            .iter()
            .any(|warning| { warning.contains("superscript candidate window truncated") })
    );
    assert_eq!(result.status, tpe::schema::Status::Partial);
    assert!(
        result
            .warnings
            .iter()
            .any(|warning| warning.contains("unicode_mapping:")),
        "{:?}",
        result.warnings
    );
    eprintln!("retained public diagnostics: {:?}", result.warnings);
}

// Inspect native evidence independently, dropping the binding before engine extraction.
fn assert_native_krishnamurthy_marker(bytes: &[u8]) {
    let directory = std::env::var("PDFIUM_DYNAMIC_LIB_PATH").unwrap();
    let library = std::path::Path::new(&directory);
    let library = if library.is_dir() {
        Pdfium::pdfium_platform_library_name_at_path(library)
    } else {
        library.to_path_buf()
    };
    let pdfium = Pdfium::new(Pdfium::bind_to_library(library).unwrap());
    let document = pdfium.load_pdf_from_byte_slice(bytes, None).unwrap();
    let mut found = false;
    for page in document.pages().iter() {
        let text = page.text().unwrap();
        for object in page.objects().iter() {
            let Some(object) = object.as_text_object() else {
                continue;
            };
            let raw = text.for_object(object);
            if raw.contains("Krishna\u{0002}") {
                let chars = text.chars_for_object(object).unwrap();
                let markers: Vec<_> = chars.iter().filter(|ch| ch.unicode_value() == 2).collect();
                assert_eq!(markers.len(), raw.matches('\u{0002}').count());
                assert!(markers.iter().all(|ch| ch.is_hyphen().unwrap()));
                eprintln!("independent native reference 73 marker: {raw:?}");
                found = true;
            }
        }
    }
    assert!(found, "the pinned PDF must reproduce Krishna + U+0002");
}
