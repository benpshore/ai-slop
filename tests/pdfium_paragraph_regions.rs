//! Opt-in integration of confirmed hyphen recovery and paragraph region tags.
#![cfg(feature = "pdfium")]

use tpe::backend::pdfium_backend::PdfiumBackend;
use tpe::pipeline::run_job_with;
use tpe::schema::{Job, sha256_hex};

#[test]
#[ignore = "requires the pinned public 2511.15503v5 PDF via TPE_HYPHEN_FIXTURE"]
fn dehyphenated_paragraph_tail_is_body_and_diagram_labels_remain_figure() {
    let path = std::env::var("TPE_HYPHEN_FIXTURE").expect("set TPE_HYPHEN_FIXTURE");
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(
        sha256_hex(&bytes),
        "5682c7c0c805b3145b15eb5b55a1db81d491211b7fc0155cc919462c657ad349"
    );
    let result = run_job_with(
        &PdfiumBackend::default(),
        &Job {
            path,
            backend: "pdfium".into(),
            pages: None,
            password: None,
            max_bytes: None,
            figures_dir: None,
        },
    )
    .unwrap();
    let page = &result.pages[2];
    let tail = page
        .lines
        .iter()
        .find(|line| line.text == "via the Host memory bus.")
        .expect("dehyphenation moves 'pens' into the preceding line");
    assert_eq!(tail.role, "body");
    for text in ["data", "rearrangements", "Host Memory Space", "MUL"] {
        assert!(
            page.lines
                .iter()
                .any(|line| line.text == text && line.role == "figure"),
            "diagram label {text}"
        );
    }
    assert_eq!(
        page.lines
            .iter()
            .filter(|line| line.role == "figure")
            .count(),
        34
    );
    assert_eq!(result.pages.len(), 19);
    assert_eq!(result.references.len(), 139);
    // The merged complete-window search removes the old candidate cutoff.
    // Unicode mapping diagnostics still prevent a Complete claim.
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
            .any(|warning| warning.contains("unicode_mapping:"))
    );
    eprintln!("retained public diagnostics: {:?}", result.warnings);
}
