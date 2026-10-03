//! Public-paper regression, opt-in so the corpus is never fetched by a test.
#![cfg(feature = "pdfium")]

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
    assert!(
        result.warnings.iter().any(|warning| {
            warning == "page 12: resource_limit: superscript candidate window truncated (limit=256)"
        }),
        "the unrelated known cleanup limit must remain visible"
    );
}
