#![cfg(feature = "docling-text")]

#[test]
fn supervised_text_parser_ignores_ocr_and_native_runtime_environment() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("probe.pdf");
    std::fs::write(&input, tpe::backend::probe_pdf().unwrap()).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_tpe"))
        .args([
            "extract",
            "--backend",
            "docling-text",
            "--json",
            "--timeout-ms",
            "5000",
            "--db",
        ])
        .arg(directory.path().join("out.sqlite"))
        .arg(&input)
        .current_dir(directory.path())
        .env("DOCLING_RS_OCR_ENGINE", "tesseract")
        .env("PDFIUM_DYNAMIC_LIB_PATH", "/missing/native/runtime")
        .env("DOCLING_RS_MODELS_DIR", "/missing/models")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let record: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(record["status"], "complete");
    assert!(directory.path().join("out.sqlite").is_file());
}
