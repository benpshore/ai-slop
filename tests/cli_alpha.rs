//! Exercise the shipped CLI boundary, including real worker termination.
mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use lopdf::{Object, Stream, dictionary};
use serde_json::Value;
use tempfile::TempDir;

fn command(root: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tpe"));
    cmd.args(["extract", "--backend", "lopdf", "--json", "--db"])
        .arg(root.join("ledger.sqlite"))
        .arg("--out")
        .arg(root.join("out"));
    cmd
}

fn record(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "{e}: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/pdfium-unicode")
        .join(name)
}

#[test]
fn native_and_existing_ocr_are_retained_with_collision_safe_receipts() {
    for name in ["native.pdf", "existing-ocr.pdf"] {
        let root = TempDir::new().unwrap();
        let input = fixture(name);
        let original = fs::read(&input).unwrap();
        let first = command(root.path()).arg(&input).output().unwrap();
        assert!(
            first.status.success(),
            "{}",
            String::from_utf8_lossy(&first.stderr)
        );
        let a = record(&first);
        assert_eq!(a["status"], "complete");
        assert!(
            a["pages"][0]["text"]
                .as_str()
                .unwrap()
                .contains("Existing OCR already reads this sentence.")
        );
        let first_paths = a["outputs"].as_array().unwrap();
        assert_eq!(first_paths.len(), 2);
        let first_bytes: Vec<_> = first_paths
            .iter()
            .map(|p| fs::read(p.as_str().unwrap()).unwrap())
            .collect();
        let second = command(root.path()).arg(&input).output().unwrap();
        assert!(second.status.success());
        let b = record(&second);
        assert_ne!(a["outputs"], b["outputs"]);
        for (p, bytes) in first_paths.iter().zip(first_bytes) {
            assert_eq!(fs::read(p.as_str().unwrap()).unwrap(), bytes);
        }
        assert_eq!(fs::read(input).unwrap(), original);
        assert_eq!(fs::read_dir(root.path().join("out")).unwrap().count(), 4);
    }
}

#[test]
fn unmapped_text_is_retained_as_partial_in_json_chunks_and_ledger() {
    let root = TempDir::new().unwrap();
    let output = command(root.path())
        .arg(fixture("partial-cmap.pdf"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    let value = record(&output);
    assert_eq!(value["status"], "partial");
    assert_eq!(value["pages"].as_array().unwrap().len(), 1);
    assert!(!value["pages"][0]["text"].as_str().unwrap().is_empty());
    assert_eq!(value["chunks"][0]["status"], "partial");
    assert!(
        value["pages"][0]["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().starts_with("unresolved_text:"))
    );
    let conn = rusqlite::Connection::open(root.path().join("ledger.sqlite")).unwrap();
    let stored: String = conn
        .query_row("SELECT status FROM runs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(stored, "partial");
    let exported: Value =
        serde_json::from_slice(&fs::read(value["outputs"][0].as_str().unwrap()).unwrap()).unwrap();
    assert_eq!(exported["status"], "partial");
}

fn mixed_pdf() -> Vec<u8> {
    let mut doc = lopdf::Document::load_mem(&common::synthetic_paper()).unwrap();
    let image = doc.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject", "Subtype" => "Image", "Width" => 8, "Height" => 8,
            "ColorSpace" => "DeviceGray", "BitsPerComponent" => 8,
        },
        vec![128; 64],
    ));
    let content = doc.add_object(Stream::new(
        dictionary! {},
        b"q 612 0 0 792 0 0 cm /Scan Do Q".to_vec(),
    ));
    let page = doc.get_pages()[&2];
    let dictionary = doc.get_object_mut(page).unwrap().as_dict_mut().unwrap();
    dictionary.set(
        "Resources",
        dictionary! { "XObject" => dictionary! { "Scan" => image } },
    );
    dictionary.set("Contents", content);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    bytes
}

#[test]
fn minority_image_page_does_not_hide_behind_native_text() {
    let root = TempDir::new().unwrap();
    let input = root.path().join("mixed.pdf");
    let bytes = mixed_pdf();
    fs::write(&input, &bytes).unwrap();
    let output = command(root.path()).arg(&input).output().unwrap();
    assert!(!output.status.success());
    let value = record(&output);
    assert_eq!(value["status"], "partial");
    assert_eq!(value["document"]["pages"], 2);
    assert_eq!(value["pages"].as_array().unwrap().len(), 2);
    assert!(
        value["pages"][0]["text"]
            .as_str()
            .unwrap()
            .contains(common::TITLE)
    );
    assert!(value["pages"][1]["text"].as_str().unwrap().is_empty());
    assert!(
        value["pages"][1]["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().starts_with("unresolved_text:"))
    );
    assert_eq!(fs::read(input).unwrap(), bytes);
}

#[test]
fn folder_is_nonrecursive_and_bad_files_do_not_stop_later_inputs() {
    let root = TempDir::new().unwrap();
    let folder = root.path().join("input");
    fs::create_dir_all(folder.join("nested")).unwrap();
    fs::write(folder.join("a.pdf"), b"not a PDF").unwrap();
    fs::copy(fixture("native.pdf"), folder.join("b.PDF")).unwrap();
    fs::write(folder.join("ignored.txt"), b"not a PDF").unwrap();
    fs::write(folder.join("nested/c.pdf"), b"not a PDF").unwrap();
    let output = command(root.path()).arg(folder).output().unwrap();
    assert!(!output.status.success());
    let rows: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["status"], "failed");
    assert!(rows[0]["error"].is_string());
    assert_eq!(rows[1]["status"], "complete");
}

#[test]
fn limits_are_explicit_and_no_failed_worker_commits_outputs() {
    for flags in [
        ["--max-bytes", "1"],
        ["--max-output-bytes", "1024"],
        ["--timeout-ms", "1"],
    ] {
        let root = TempDir::new().unwrap();
        let captures = root.path().join("captures");
        fs::create_dir(&captures).unwrap();
        let output = command(root.path())
            .args(flags)
            .arg(fixture("native.pdf"))
            .env("TMPDIR", &captures)
            .env("TMP", &captures)
            .output()
            .unwrap();
        assert!(!output.status.success(), "{flags:?}");
        let value = record(&output);
        assert_eq!(value["status"], "failed", "{flags:?}: {value}");
        assert!(value["error"].is_string());
        assert_eq!(fs::read_dir(root.path().join("out")).unwrap().count(), 0);
        assert_eq!(
            fs::read_dir(captures).unwrap().count(),
            0,
            "worker capture cleanup"
        );
        let ledger = tpe::ledger::Ledger::open(&root.path().join("ledger.sqlite")).unwrap();
        assert_eq!(ledger.stats().unwrap().runs, 0);
    }
}

#[test]
fn excessive_folder_selection_fails_before_writes() {
    let root = TempDir::new().unwrap();
    let input = root.path().join("input");
    fs::create_dir(&input).unwrap();
    for name in ["a.pdf", "b.pdf"] {
        fs::copy(fixture("native.pdf"), input.join(name)).unwrap();
    }
    let output = command(root.path())
        .args(["--max-files", "1"])
        .arg(input)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no files were processed"));
    assert!(!root.path().join("ledger.sqlite").exists());
    assert!(!root.path().join("out").exists());
}

#[test]
fn selected_page_range_is_explicit_document_partial_without_clobbering_full_export() {
    let root = TempDir::new().unwrap();
    let input = root.path().join("paper.pdf");
    fs::write(&input, common::synthetic_paper()).unwrap();
    let full = command(root.path()).arg(&input).output().unwrap();
    assert!(full.status.success());
    let full = record(&full);
    let prior_path = full["outputs"][0].as_str().unwrap();
    let prior = fs::read(prior_path).unwrap();
    let selected = command(root.path())
        .args(["--pages", "2"])
        .arg(&input)
        .output()
        .unwrap();
    assert!(!selected.status.success());
    let selected = record(&selected);
    assert_eq!(selected["status"], "partial");
    assert_eq!(selected["document"]["pages"], 2);
    assert_eq!(selected["pages"].as_array().unwrap().len(), 1);
    assert_eq!(selected["pages"][0]["page"], 2);
    assert_eq!(selected["chunks"][0]["status"], "complete");
    assert_eq!(fs::read(prior_path).unwrap(), prior);
}

#[test]
fn failed_page_keeps_its_placeholder_and_other_page_text() {
    let root = TempDir::new().unwrap();
    let mut doc = lopdf::Document::load_mem(&common::synthetic_paper()).unwrap();
    let page = doc.get_pages()[&2];
    doc.get_object_mut(page)
        .unwrap()
        .as_dict_mut()
        .unwrap()
        .set("Contents", Object::Reference((999_999, 0)));
    let input = root.path().join("broken-page.pdf");
    doc.save(&input).unwrap();
    let output = command(root.path()).arg(&input).output().unwrap();
    assert!(!output.status.success());
    let value = record(&output);
    assert_eq!(value["status"], "partial");
    assert_eq!(value["pages"].as_array().unwrap().len(), 2);
    assert!(
        value["pages"][0]["text"]
            .as_str()
            .unwrap()
            .contains(common::TITLE)
    );
    assert!(!value["pages"][1]["warnings"].as_array().unwrap().is_empty());
}

#[cfg(unix)]
#[test]
fn worker_exits_when_supervisor_lease_disappears_even_during_blocked_request_read() {
    let root = TempDir::new().unwrap();
    let fifo = root.path().join("blocked-request");
    assert!(
        Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    let mut worker = Command::new(env!("CARGO_BIN_EXE_tpe"))
        .arg("extract-worker")
        .arg(&fifo)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let lease = worker.stdin.take().unwrap();
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        worker.try_wait().unwrap().is_none(),
        "worker should be blocked while lease is held"
    );
    drop(lease);
    let deadline = Instant::now() + Duration::from_secs(3);
    while worker.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            worker.kill().unwrap();
            worker.wait().unwrap();
            panic!("worker survived loss of supervisor");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
#[test]
fn deadline_kills_and_reaps_a_stopped_worker() {
    let root = TempDir::new().unwrap();
    let mut doc = lopdf::Document::load_mem(&common::synthetic_paper()).unwrap();
    let template = doc.get_object(doc.get_pages()[&1]).unwrap().clone();
    let parent = template
        .as_dict()
        .unwrap()
        .get(b"Parent")
        .unwrap()
        .as_reference()
        .unwrap();
    let kids: Vec<Object> = (0..2000)
        .map(|_| Object::Reference(doc.add_object(template.clone())))
        .collect();
    let pages = doc.get_object_mut(parent).unwrap().as_dict_mut().unwrap();
    pages.set("Kids", kids);
    pages.set("Count", 2000);
    let input = root.path().join("many-pages.pdf");
    doc.save(&input).unwrap();
    let output_path = root.path().join("stdout");
    let mut child = command(root.path())
        .args(["--timeout-ms", "1000"])
        .arg(&input)
        .stdout(fs::File::create(&output_path).unwrap())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let parent_pid = child.id().to_string();
    let started = Instant::now();
    let worker_pid = loop {
        let ps = Command::new("ps")
            .args(["-axo", "pid=,ppid="])
            .output()
            .unwrap();
        let found = String::from_utf8(ps.stdout)
            .unwrap()
            .lines()
            .find_map(|line| {
                let mut fields = line.split_whitespace();
                let pid = fields.next()?;
                (fields.next()? == parent_pid).then(|| pid.to_owned())
            });
        if let Some(pid) = found {
            break pid;
        }
        if started.elapsed() > Duration::from_secs(3) || child.try_wait().unwrap().is_some() {
            let _ = child.kill();
            let _ = child.wait();
            panic!("could not observe worker before completion");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(
        Command::new("kill")
            .args(["-STOP", &worker_pid])
            .status()
            .unwrap()
            .success()
    );
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if started.elapsed() > Duration::from_secs(5) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = Command::new("kill").args(["-KILL", &worker_pid]).status();
            panic!("supervisor failed to enforce deadline");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(!status.success());
    let value: Value = serde_json::from_slice(&fs::read(output_path).unwrap()).unwrap();
    assert_eq!(value["status"], "failed");
    assert!(value["error"].as_str().unwrap().contains("timed out"));
    assert!(
        !Command::new("kill")
            .args(["-0", &worker_pid])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success(),
        "worker must have been killed and reaped"
    );
    assert_eq!(fs::read_dir(root.path().join("out")).unwrap().count(), 0);
}

#[test]
fn broken_stream_array_retains_other_streams_but_absent_contents_can_be_blank() {
    for damaged in [false, true] {
        let root = TempDir::new().unwrap();
        let mut doc = lopdf::Document::load_mem(&common::synthetic_paper()).unwrap();
        let page = doc.get_pages()[&2];
        let dict = doc.get_object_mut(page).unwrap().as_dict_mut().unwrap();
        if damaged {
            let valid = dict.get(b"Contents").unwrap().clone();
            dict.set(
                "Contents",
                Object::Array(vec![valid, Object::Reference((999_999, 0))]),
            );
        } else {
            dict.remove(b"Contents");
        }
        let input = root.path().join("content.pdf");
        doc.save(&input).unwrap();
        let output = command(root.path()).arg(input).output().unwrap();
        let value = record(&output);
        assert_eq!(output.status.success(), !damaged);
        assert_eq!(
            value["status"],
            if damaged { "partial" } else { "complete" }
        );
        let page_text = value["pages"][1]["text"].as_str().unwrap();
        if damaged {
            assert!(page_text.contains("References"), "{value}");
            assert!(
                value["warnings"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|w| w.as_str().unwrap().contains("page Contents"))
            );
        } else {
            assert!(page_text.is_empty());
        }
    }
}

#[test]
fn missing_form_resource_marks_partial_without_discarding_native_text() {
    let root = TempDir::new().unwrap();
    let mut doc = lopdf::Document::load_mem(&common::synthetic_paper()).unwrap();
    let missing_form = doc.add_object(Stream::new(dictionary! {}, b"/MissingForm Do".to_vec()));
    let page = doc.get_pages()[&2];
    let dict = doc.get_object_mut(page).unwrap().as_dict_mut().unwrap();
    let valid = dict.get(b"Contents").unwrap().clone();
    dict.set(
        "Contents",
        Object::Array(vec![valid, Object::Reference(missing_form)]),
    );
    let input = root.path().join("missing-form.pdf");
    doc.save(&input).unwrap();
    let output = command(root.path()).arg(input).output().unwrap();
    assert!(!output.status.success());
    let value = record(&output);
    assert_eq!(value["status"], "partial");
    assert!(
        value["pages"][1]["text"]
            .as_str()
            .unwrap()
            .contains("References")
    );
    assert!(
        value["pages"][1]["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w
                .as_str()
                .unwrap()
                .contains("missing or undecodable page resources"))
    );
}
