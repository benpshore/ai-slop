//! The opt-in pure layout stage stays inside the existing native worker path.
#![cfg(all(
    feature = "liteparse-layout",
    any(target_os = "linux", target_os = "macos")
))]

use std::process::Command;

#[test]
fn spatial_layout_runs_through_the_bounded_extraction_and_publication_workers() {
    let Some(library) = std::env::var_os("PDFIUM_DYNAMIC_LIB_PATH") else {
        eprintln!("skipped: set PDFIUM_DYNAMIC_LIB_PATH for the supervised layout smoke test");
        return;
    };
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("probe.pdf");
    let ledger = directory.path().join("result.sqlite");
    let bytes = tpe::backend::probe_pdf().unwrap();
    std::fs::write(&input, &bytes).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_tpe"))
        .args(["extract", "--backend", "liteparse-layout", "--json", "--db"])
        .arg(&ledger)
        .arg(&input)
        .env("PDFIUM_DYNAMIC_LIB_PATH", library)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["backend"]["name"], "liteparse-layout");
    assert_eq!(value["status"], "complete");
    assert_eq!(value["pages"].as_array().unwrap().len(), 1);
    assert!(
        value["pages"][0]["text"]
            .as_str()
            .unwrap()
            .contains("probe")
    );
    assert!(value["worker_limits"]["extraction"].is_object());
    assert!(value["worker_limits"]["publication"].is_object());
    assert_eq!(std::fs::read(input).unwrap(), bytes);
}
