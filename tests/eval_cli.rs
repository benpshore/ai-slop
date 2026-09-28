//! CLI tests for `tpe corpus fetch` and `tpe eval` that never touch the
//! network: an empty cache plus `--offline` must make every item fail
//! gracefully.

use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

use tpe::corpus::{Manifest, ManifestItem, save_manifest};

/// A one-item manifest in the `dev` split, written to `dir/manifest.json`.
fn write_manifest(dir: &Path) -> PathBuf {
    let manifest = Manifest {
        version: 1,
        items: vec![ManifestItem {
            id: "arxiv:2502.00857".to_string(),
            kind: "arxiv".to_string(),
            license: "http://creativecommons.org/licenses/by/4.0/".to_string(),
            pdf_url: "https://arxiv.org/pdf/2502.00857".to_string(),
            source_url: Some("https://arxiv.org/e-print/2502.00857".to_string()),
            pdf_sha256: None,
            source_sha256: None,
            categories: vec!["cs.CL".to_string(), "cs.IR".to_string()],
            split: "dev".to_string(),
            notes: None,
        }],
    };
    let path = dir.join("manifest.json");
    save_manifest(&path, &manifest).expect("manifest written");
    path
}

/// The `tpe` binary built for this test run.
fn tpe() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tpe"))
}

#[test]
fn eval_offline_with_empty_cache_reports_one_failed_paper() {
    let dir = TempDir::new().expect("tempdir");
    let manifest = write_manifest(dir.path());
    let cache = dir.path().join("cache");
    let out = dir.path().join("out");

    let output = tpe()
        .arg("eval")
        .arg("--manifest")
        .arg(&manifest)
        .arg("--cache")
        .arg(&cache)
        .arg("--out")
        .arg(&out)
        .arg("--offline")
        .output()
        .expect("tpe eval runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "eval must exit 0 on poor metrics\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(stdout.contains("papers: 1"), "summary missing:\n{stdout}");
    assert!(stdout.contains("failed: 1"), "summary missing:\n{stdout}");

    let json = std::fs::read_to_string(out.join("report.json")).expect("report.json written");
    let report: serde_json::Value = serde_json::from_str(&json).expect("report.json is JSON");
    let papers = report["papers"].as_array().expect("papers array");
    assert_eq!(papers.len(), 1, "{report:#}");
    assert_eq!(papers[0]["id"], "arxiv:2502.00857", "{report:#}");
    let status = papers[0]["status"].as_str().expect("status string");
    assert!(
        status.starts_with("failed:"),
        "status `{status}` is not a failure"
    );
    assert_eq!(report["summary"]["papers"], 1, "{report:#}");
    assert_eq!(report["summary"]["failed"], 1, "{report:#}");
    assert!(report["host"].as_str().is_some_and(|h| !h.is_empty()));
    assert_eq!(report["backend"], "lopdf");

    let markdown = std::fs::read_to_string(out.join("report.md")).expect("report.md written");
    assert!(
        markdown.contains("arxiv:2502.00857"),
        "report.md lacks the paper:\n{markdown}"
    );
}

#[test]
fn eval_offline_empty_split_writes_empty_report() {
    let dir = TempDir::new().expect("tempdir");
    let manifest = write_manifest(dir.path());
    let out = dir.path().join("out");

    let output = tpe()
        .arg("eval")
        .arg("--manifest")
        .arg(&manifest)
        .arg("--cache")
        .arg(dir.path().join("cache"))
        .arg("--out")
        .arg(&out)
        .arg("--split")
        .arg("holdout")
        .arg("--offline")
        .output()
        .expect("tpe eval runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json = std::fs::read_to_string(out.join("report.json")).expect("report.json written");
    let report: serde_json::Value = serde_json::from_str(&json).expect("report.json is JSON");
    assert_eq!(
        report["papers"].as_array().map(Vec::len),
        Some(0),
        "{report:#}"
    );
}

#[test]
fn corpus_fetch_offline_with_empty_cache_fails() {
    let dir = TempDir::new().expect("tempdir");
    let manifest = write_manifest(dir.path());
    let cache = dir.path().join("cache");

    let output = tpe()
        .arg("corpus")
        .arg("fetch")
        .arg("--manifest")
        .arg(&manifest)
        .arg("--cache")
        .arg(&cache)
        .arg("--offline")
        .output()
        .expect("tpe corpus fetch runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !output.status.success(),
        "fetch must fail offline:\n{stdout}"
    );
    assert!(
        stdout.contains("arxiv:2502.00857\t-\tsource no\tfailed"),
        "{stdout}"
    );
    // Nothing was fetched, so the manifest is left untouched.
    let after = std::fs::read_to_string(&manifest).expect("manifest readable");
    assert!(after.contains("\"pdf_sha256\": null"), "{after}");
}

#[test]
fn eval_rejects_unknown_backend() {
    let dir = TempDir::new().expect("tempdir");
    let manifest = write_manifest(dir.path());
    let output = tpe()
        .arg("eval")
        .arg("--manifest")
        .arg(&manifest)
        .arg("--cache")
        .arg(dir.path().join("cache"))
        .arg("--out")
        .arg(dir.path().join("out"))
        .arg("--backend")
        .arg("nope")
        .arg("--offline")
        .output()
        .expect("tpe eval runs");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unknown backend"), "{stderr}");
}
