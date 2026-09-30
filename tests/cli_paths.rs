//! End-to-end tests of the default command: `tpe PATH...` (text mode) and
//! `tpe --bib PATH...` (bibliography mode), plus `--version` and the
//! offline behaviour of `tpe update --check`.
//!
//! Every invocation sets `TPE_NO_UPDATE_CHECK=1`, so no test reaches the
//! network.

mod common;

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use common::raster::{SCAN_SCALE, scanned_page_pdf};
use common::{TITLE, synthetic_paper};

/// The `tpe` binary with the passive update check disabled.
fn tpe() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tpe"));
    command.env("TPE_NO_UPDATE_CHECK", "1");
    command
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Write `bytes` at `path`, creating parent directories.
fn put(path: &Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

/// Parse the JSON file at `path`.
fn read_json(path: &Path) -> serde_json::Value {
    let text = fs::read_to_string(path).unwrap();
    serde_json::from_str(&text).unwrap()
}

/// Number of lines of `text` starting with `prefix`.
fn count_lines(text: &str, prefix: &str) -> usize {
    let matching = text.lines().filter(|line| line.starts_with(prefix));
    matching.count()
}

/// Whether some line of `text` starts with `prefix` and ends with `suffix`.
fn has_line(text: &str, prefix: &str, suffix: &str) -> bool {
    text.lines()
        .any(|line| line.starts_with(prefix) && line.ends_with(suffix))
}

/// The last line of `text` (the summary line of a run).
fn last_line(text: &str) -> &str {
    text.lines().last().unwrap_or_default()
}

/// A one-page PDF whose only content is a raster covering well over half
/// the page, with no text layer.
fn scanned_pdf() -> Vec<u8> {
    let lines = ["HELLO WORLD"; 26];
    scanned_page_pdf(&lines, SCAN_SCALE)
}

/// A minimal ZIP local file header naming `word/document.xml` (a docx).
fn fake_docx() -> Vec<u8> {
    let name = b"word/document.xml";
    let mut bytes = vec![0x50, 0x4B, 0x03, 0x04];
    bytes.extend_from_slice(&[0u8; 22]);
    bytes.extend_from_slice(&u16::try_from(name.len()).unwrap().to_le_bytes());
    bytes.extend_from_slice(&[0, 0]);
    bytes.extend_from_slice(name);
    bytes.extend_from_slice(&[0u8; 64]);
    bytes
}

/// Bytes that match no known signature.
fn noise() -> Vec<u8> {
    (0u32..4096)
        .map(|i| u8::try_from(i.wrapping_mul(2_654_435_761) >> 24).unwrap())
        .collect()
}

/// One JSON value per stdout line.
fn json_lines(output: &Output) -> Vec<serde_json::Value> {
    stdout(output)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|err| panic!("{line}: {err}")))
        .collect()
}

/// Parse every stderr line of a `--progress` run as a JSON object.
fn stderr_events(output: &Output) -> Vec<serde_json::Value> {
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|err| panic!("{line}: {err}")))
        .collect()
}

/// The record whose `path` ends with `name`.
fn record_named<'a>(records: &'a [serde_json::Value], name: &str) -> &'a serde_json::Value {
    records
        .iter()
        .find(|record| record["path"].as_str().unwrap().ends_with(name))
        .unwrap()
}

#[test]
fn directory_input_mirrors_structure_and_writes_text() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in");
    put(&input.join("a/paper.pdf"), &synthetic_paper());
    put(&input.join("b/sub/second.PDF"), &synthetic_paper());
    put(&input.join("notes.txt"), b"not a pdf");
    put(&input.join("._paper.pdf"), b"AppleDouble junk");
    let out = dir.path().join("out");
    let output = tpe().arg(&input).arg("--out").arg(&out).output().unwrap();
    assert!(output.status.success(), "{}", stderr(&output));

    let first = fs::read_to_string(out.join("a/paper.txt")).unwrap();
    assert!(first.contains(TITLE), "{first}");
    assert!(first.contains('\u{c}'), "form feed between pages");
    assert!(out.join("b/sub/second.txt").is_file());
    assert!(!out.join("a/paper.json").exists(), "no JSON by default");
    assert!(!out.join("notes.txt").exists());
    assert!(!out.join("._paper.txt").exists());

    let err = stderr(&output);
    assert_eq!(count_lines(&err, "ok  2p  "), 2, "{err}");
    assert!(last_line(&err).starts_with("2 ok, 0 failed, "), "{err}");
    assert!(output.stdout.is_empty());
}

#[test]
fn json_flag_writes_the_full_result() {
    let dir = tempfile::tempdir().unwrap();
    let pdf = dir.path().join("paper.pdf");
    put(&pdf, &synthetic_paper());
    let out = dir.path().join("out");
    let output = tpe()
        .arg(&pdf)
        .args(["--json", "--no-images", "--out"])
        .arg(&out)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    let json = read_json(&out.join("paper.json"));
    assert_eq!(json["pages"].as_array().unwrap().len(), 2);
    assert_eq!(json["document"]["hash"].as_str().unwrap().len(), 64);
    assert_eq!(json["references"].as_array().unwrap().len(), 3);
    assert!(out.join("paper.txt").is_file());
}

#[test]
fn stdout_prints_text_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let pdf = dir.path().join("paper.pdf");
    put(&pdf, &synthetic_paper());
    let output = tpe()
        .current_dir(dir.path())
        .args(["paper.pdf", "--stdout"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains(TITLE), "{text}");
    assert!(text.contains('\u{c}'));
    let entries = fs::read_dir(dir.path()).unwrap().count();
    assert_eq!(entries, 1, "no tpe-out directory");
}

#[test]
fn bib_stdout_has_the_same_keys_as_the_bibliography_subcommand() {
    let dir = tempfile::tempdir().unwrap();
    let pdf = dir.path().join("paper.pdf");
    put(&pdf, &synthetic_paper());
    let new = tpe().arg("--bib").arg(&pdf).output().unwrap();
    assert!(new.status.success(), "{}", stderr(&new));
    let old = tpe().arg("bibliography").arg(&pdf).output().unwrap();
    assert!(old.status.success(), "{}", stderr(&old));
    let new_lines = json_lines(&new);
    let old_lines = json_lines(&old);
    assert_eq!(new_lines.len(), 1);
    assert_eq!(old_lines.len(), 1);
    let new_keys: Vec<&String> = new_lines[0].as_object().unwrap().keys().collect();
    let old_keys: Vec<&String> = old_lines[0].as_object().unwrap().keys().collect();
    assert_eq!(new_keys, old_keys);
    assert_eq!(new_lines[0]["status"], "found");
    assert_eq!(new_lines[0]["references"], old_lines[0]["references"]);
    assert_eq!(new_lines[0]["sha256"], old_lines[0]["sha256"]);
    let err = stderr(&new);
    assert_eq!(count_lines(&err, "ok  2p  "), 1, "{err}");
}

#[test]
fn bib_db_stores_papers_and_refs_and_reruns_replace() {
    let dir = tempfile::tempdir().unwrap();
    let pdf = dir.path().join("paper.pdf");
    put(&pdf, &synthetic_paper());
    let db = dir.path().join("bib.sqlite");
    for _ in 0..2 {
        let output = tpe()
            .args(["--bib", "--db"])
            .arg(&db)
            .arg(&pdf)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", stderr(&output));
    }
    let conn = rusqlite::Connection::open(&db).unwrap();
    let papers: i64 = conn
        .query_row("SELECT COUNT(*) FROM papers", [], |row| row.get(0))
        .unwrap();
    assert_eq!(papers, 1);
    let refs: i64 = conn
        .query_row("SELECT COUNT(*) FROM refs", [], |row| row.get(0))
        .unwrap();
    assert_eq!(refs, 3);
    let (idx, year, sha): (i64, i64, String) = conn
        .query_row(
            "SELECT idx, year, sha256 FROM refs WHERE doi = '10.1000/abc456'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!((idx, year), (2, 1952));
    assert_eq!(sha.len(), 64);
    let (status, pages, scanned_at, first_author): (String, i64, String, String) = conn
        .query_row(
            "SELECT p.status, p.total_pages, p.scanned_at, r.first_author \
             FROM papers p JOIN refs r ON r.sha256 = p.sha256 WHERE r.idx = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(status, "found");
    assert_eq!(pages, 2);
    assert!(scanned_at.ends_with('Z'), "{scanned_at}");
    assert!(first_author.contains("Lovelace"), "{first_author}");

    let jsonl = dir.path().join("records.jsonl");
    let output = tpe()
        .args(["--bib", "--out"])
        .arg(&jsonl)
        .arg(&pdf)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(output.stdout.is_empty());
    let text = fs::read_to_string(&jsonl).unwrap();
    assert_eq!(text.lines().count(), 1);
    let value: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
    assert_eq!(value["status"], "found");
}

#[test]
fn failing_pdf_does_not_stop_the_batch() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in");
    put(&input.join("bad.pdf"), b"%PDF-1.4 but nothing else\n");
    put(&input.join("good.pdf"), &synthetic_paper());
    let out = dir.path().join("out");
    let output = tpe().arg(&input).arg("--out").arg(&out).output().unwrap();
    assert!(!output.status.success());
    assert!(out.join("good.txt").is_file());
    assert!(!out.join("bad.txt").exists());
    let err = stderr(&output);
    assert!(has_line(&err, "FAILED  ", "bad.pdf"), "{err}");
    assert_eq!(count_lines(&err, "FAILED  "), 1, "{err}");
    assert_eq!(count_lines(&err, "ok  2p  "), 1, "{err}");
    assert!(last_line(&err).starts_with("1 ok, 1 failed, "), "{err}");
}

#[test]
fn empty_directory_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    put(&dir.path().join("in/notes.txt"), b"text");
    let output = tpe().arg(dir.path().join("in")).output().unwrap();
    assert!(!output.status.success());
    let err = stderr(&output);
    assert!(err.contains("no PDF files found"), "{err}");
}

#[test]
fn version_prints_name_version_and_sha() {
    let output = tpe().arg("--version").output().unwrap();
    assert!(output.status.success());
    let text = stdout(&output);
    let text = text.trim_end();
    assert!(text.starts_with("tpe "), "{text}");
    let (version, sha) = text[4..].split_once(" (").unwrap();
    let digit = version.chars().next().is_some_and(|c| c.is_ascii_digit());
    assert!(digit, "{text}");
    assert!(sha.ends_with(')') && sha.len() > 1, "{text}");
}

#[test]
fn no_arguments_prints_help_and_exits_2() {
    let output = tpe().output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    let text = format!("{}{}", stdout(&output), stderr(&output));
    assert!(text.contains("Usage"), "{text}");
    assert!(text.contains("--bib"), "{text}");
}

#[test]
fn progress_emits_json_events() {
    let dir = tempfile::tempdir().unwrap();
    let pdf = dir.path().join("paper.pdf");
    put(&pdf, &synthetic_paper());
    let out = dir.path().join("out");
    let output = tpe()
        .arg(&pdf)
        .args(["--progress", "--out"])
        .arg(&out)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    let events = stderr_events(&output);
    let kinds: Vec<&str> = events
        .iter()
        .map(|event| event["event"].as_str().unwrap())
        .collect();
    let expected = ["opened", "page", "page", "done", "summary"];
    assert_eq!(kinds, expected, "{events:?}");
    assert_eq!(events[3]["status"], "ok");
    assert_eq!(events[3]["pages"], 2);
    assert_eq!(events[4]["ok"], 1);
    assert_eq!(events[4]["failed"], 0);
}

#[test]
fn output_collisions_get_a_hash_suffix() {
    let dir = tempfile::tempdir().unwrap();
    let first = dir.path().join("one/paper.pdf");
    let second = dir.path().join("two/paper.pdf");
    put(&first, &synthetic_paper());
    put(&second, &synthetic_paper());
    let out = dir.path().join("out");
    let output = tpe()
        .arg(&first)
        .arg(&second)
        .arg("--out")
        .arg(&out)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    let names: Vec<String> = fs::read_dir(&out)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".txt"))
        .collect();
    assert_eq!(names.len(), 2, "{names:?}");
    assert!(names.contains(&"paper.txt".to_string()), "{names:?}");
    let suffixed = names
        .iter()
        .find(|name| name.as_str() != "paper.txt")
        .unwrap();
    assert!(suffixed.starts_with("paper-"), "{suffixed}");
    let expected_len = "paper-".len() + 12 + ".txt".len();
    assert_eq!(suffixed.len(), expected_len, "{suffixed}");
    let err = stderr(&output);
    assert!(err.contains("note  "), "{err}");
}

#[test]
fn update_check_reports_an_unreachable_api_cleanly() {
    let output = tpe()
        .env("TPE_RELEASES_API", "http://127.0.0.1:9/releases/latest")
        .args(["update", "--check"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let err = stderr(&output);
    assert!(err.contains("127.0.0.1:9"), "{err}");
    assert!(!err.contains("panicked"), "{err}");
}

#[test]
fn unsupported_inputs_are_refused_and_the_batch_continues() {
    let dir = tempfile::tempdir().unwrap();
    let paper = dir.path().join("paper.pdf");
    let photo = dir.path().join("photo.pdf");
    let report = dir.path().join("report.docx");
    let junk = dir.path().join("noise.pdf");
    put(&paper, &synthetic_paper());
    put(&photo, &[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46]);
    put(&report, &fake_docx());
    put(&junk, &noise());
    let out = dir.path().join("out");
    let output = tpe()
        .args([&paper, &photo, &report, &junk])
        .args(["--json", "--out"])
        .arg(&out)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let err = stderr(&output);
    assert!(has_line(&err, "SKIP  image  ", "photo.pdf"), "{err}");
    assert!(has_line(&err, "SKIP  docx  ", "report.docx"), "{err}");
    assert!(has_line(&err, "SKIP  unknown  ", "noise.pdf"), "{err}");
    assert_eq!(count_lines(&err, "ok  2p  "), 1, "{err}");
    assert!(last_line(&err).starts_with("1 ok, 3 failed, "), "{err}");
    assert!(out.join("paper.txt").is_file());
    assert!(!out.join("photo.txt").exists());
    let record = read_json(&out.join("photo.json"));
    assert_eq!(record["status"], "unsupported");
    assert_eq!(record["kind"], "image");
    let reason = record["reason"].as_str().unwrap();
    assert!(reason.contains("OCR"), "{record}");
    let record = read_json(&out.join("report.json"));
    assert_eq!(record["kind"], "docx");
}

#[test]
fn scanned_pdf_is_marked_scanned_in_text_mode() {
    let dir = tempfile::tempdir().unwrap();
    let scan = dir.path().join("scan.pdf");
    let paper = dir.path().join("paper.pdf");
    put(&scan, &scanned_pdf());
    put(&paper, &synthetic_paper());
    let out = dir.path().join("out");
    let output = tpe()
        .args([&scan, &paper])
        .args(["--json", "--out"])
        .arg(&out)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let err = stderr(&output);
    assert!(has_line(&err, "SCAN  1p  ", "scan.pdf"), "{err}");
    assert!(last_line(&err).starts_with("1 ok, 1 failed, "), "{err}");
    assert!(!out.join("scan.txt").exists(), "no text file for a scan");
    assert!(out.join("paper.txt").is_file());
    let record = read_json(&out.join("scan.json"));
    assert_eq!(record["status"], "scanned");
    let reason = record["reason"].as_str().unwrap();
    assert!(reason.contains("no text layer"), "{record}");
    assert_eq!(record["pages"], 1);
}

#[test]
fn bib_mode_marks_unsupported_and_scanned_inputs() {
    let dir = tempfile::tempdir().unwrap();
    let photo = dir.path().join("photo.pdf");
    let scan = dir.path().join("scan.pdf");
    let paper = dir.path().join("paper.pdf");
    put(&photo, &[0xFF, 0xD8, 0xFF, 0xE1, 0, 0]);
    put(&scan, &scanned_pdf());
    put(&paper, &synthetic_paper());
    let db = dir.path().join("bib.sqlite");
    let output = tpe()
        .args(["--bib", "--jobs", "1", "--db"])
        .arg(&db)
        .args([&photo, &scan, &paper])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let records = json_lines(&output);
    assert_eq!(records.len(), 3, "{records:?}");

    let photo_record = record_named(&records, "photo.pdf");
    assert_eq!(photo_record["status"], "unsupported");
    assert_eq!(photo_record["kind"], "image");
    let reason = photo_record["reason"].as_str().unwrap();
    assert!(reason.contains("OCR"), "{photo_record}");
    assert!(photo_record["sha256"].is_null());

    let scan_record = record_named(&records, "scan.pdf");
    assert_eq!(scan_record["status"], "not_found", "{scan_record}");
    assert_eq!(scan_record["reason"], "scanned");
    let warnings = scan_record["warnings"].as_array().unwrap();
    let expected = serde_json::json!("scanned document: no text layer");
    assert!(warnings.contains(&expected), "{warnings:?}");

    let paper_record = record_named(&records, "paper.pdf");
    assert_eq!(paper_record["status"], "found");
    assert!(paper_record.get("reason").is_none(), "{paper_record}");

    let err = stderr(&output);
    assert_eq!(count_lines(&err, "SKIP  image  "), 1, "{err}");
    assert_eq!(count_lines(&err, "SCAN  1p  "), 1, "{err}");
    assert!(last_line(&err).starts_with("1 ok, 2 failed, "), "{err}");

    let conn = rusqlite::Connection::open(&db).unwrap();
    let statuses: Vec<String> = conn
        .prepare("SELECT status FROM papers ORDER BY status")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(statuses, ["found", "not_found"], "{statuses:?}");
}

/// Paths passed on the command line are used as given, so the outputs of a
/// file named directly mirror only its file name.
#[test]
fn direct_files_keep_only_their_name() {
    let dir = tempfile::tempdir().unwrap();
    let pdf = dir.path().join("deep/er/paper.pdf");
    put(&pdf, &synthetic_paper());
    let out = dir.path().join("out");
    let output = tpe().arg(&pdf).arg("--out").arg(&out).output().unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(out.join("paper.txt").is_file());
}
