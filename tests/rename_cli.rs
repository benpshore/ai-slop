//! `tpe rename` end to end on the synthetic paper: dry run by default, apply,
//! collision, undo, and refusal of symlinks. Never touches the network
//! (`--online` is not passed).

mod common;

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use common::{synthetic_paper, write_temp_pdf};

fn tpe(args: &[&str], cwd: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tpe"))
        .arg("rename")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn names(dir: &Path) -> Vec<String> {
    let mut n: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    n.sort();
    n
}

#[test]
fn dry_run_is_the_default_and_changes_nothing() {
    let (dir, path) = write_temp_pdf(&synthetic_paper());
    let out = tpe(&[path.to_str().unwrap()], dir.path());
    assert!(out.status.success(), "{}", text(&out.stderr));
    let stdout = text(&out.stdout);
    assert!(stdout.contains("rename  "), "{stdout}");
    assert!(stdout.contains("dry run; pass --apply"), "{stdout}");
    assert_eq!(names(dir.path()).len(), 1);
    assert!(path.exists());
    // Not even a journal is written.
    assert!(
        !names(dir.path())
            .iter()
            .any(|n| n.starts_with("tpe-rename-"))
    );
}

#[test]
fn apply_writes_a_journal_and_undo_restores_exactly() {
    let (dir, path) = write_temp_pdf(&synthetic_paper());
    let original = fs::read(&path).unwrap();
    let original_name = path.file_name().unwrap().to_str().unwrap().to_string();
    let journal = dir.path().join("journal.json");
    let out = tpe(
        &[
            "--apply",
            "--journal",
            journal.to_str().unwrap(),
            path.to_str().unwrap(),
        ],
        dir.path(),
    );
    assert!(out.status.success(), "{}", text(&out.stderr));
    let renamed = dir.path().join(
        names(dir.path())
            .iter()
            .find(|n| n.starts_with("Lovelace"))
            .unwrap(),
    );
    let new_name = renamed.file_name().unwrap().to_str().unwrap();
    assert!(new_name.starts_with("Lovelace et al. - "), "{new_name}");
    assert!(new_name.ends_with(" - Faithful Extraction of Citations from Academic PDFs.pdf"));
    assert!(!path.exists());
    assert_eq!(fs::read(&renamed).unwrap(), original);
    let recorded: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&journal).unwrap()).unwrap();
    assert_eq!(recorded["entries"][0]["from"], original_name);
    assert_eq!(recorded["entries"][0]["to"], new_name);

    // Undo is a dry run without --apply.
    let out = tpe(&["--undo", journal.to_str().unwrap()], dir.path());
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(text(&out.stdout).contains("dry run"));
    assert!(renamed.exists());

    let out = tpe(
        &["--undo", journal.to_str().unwrap(), "--apply"],
        dir.path(),
    );
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(path.exists());
    assert!(!renamed.exists());
    assert_eq!(fs::read(&path).unwrap(), original);

    // A second undo has nothing to do and still succeeds.
    let out = tpe(
        &["--undo", journal.to_str().unwrap(), "--apply"],
        dir.path(),
    );
    assert!(out.status.success());
    assert!(text(&out.stdout).contains("nothing"));
}

#[test]
fn a_journal_is_never_overwritten() {
    let (dir, path) = write_temp_pdf(&synthetic_paper());
    let journal = dir.path().join("journal.json");
    fs::write(&journal, "precious").unwrap();
    let out = tpe(
        &[
            "--apply",
            "--journal",
            journal.to_str().unwrap(),
            path.to_str().unwrap(),
        ],
        dir.path(),
    );
    assert!(!out.status.success());
    assert_eq!(fs::read_to_string(&journal).unwrap(), "precious");
    assert!(
        path.exists(),
        "nothing may be renamed when the journal cannot be written"
    );
}

#[test]
fn existing_name_is_never_overwritten_and_gets_a_suffix() {
    let (dir, path) = write_temp_pdf(&synthetic_paper());
    // The synthetic paper prints no year, so the name has no year part.
    let occupied = dir
        .path()
        .join("Lovelace et al. - Faithful Extraction of Citations from Academic PDFs.pdf");
    fs::write(&occupied, "someone else's file").unwrap();
    let out = tpe(&["--apply", path.to_str().unwrap()], dir.path());
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(
        fs::read_to_string(&occupied).unwrap(),
        "someone else's file"
    );
    assert!(
        names(dir.path())
            .iter()
            .any(|n| n.ends_with("Academic PDFs (2).pdf")),
        "{:?}",
        names(dir.path())
    );
}

#[cfg(unix)]
#[test]
fn symlinks_are_refused() {
    let (dir, path) = write_temp_pdf(&synthetic_paper());
    let link = dir.path().join("link.pdf");
    std::os::unix::fs::symlink(&path, &link).unwrap();
    let out = tpe(&["--apply", link.to_str().unwrap()], dir.path());
    assert!(out.status.success());
    assert!(
        text(&out.stdout).contains("symlink"),
        "{}",
        text(&out.stdout)
    );
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(path.exists());
}

#[test]
fn non_pdf_is_skipped_not_renamed() {
    let dir = tempfile::TempDir::new().unwrap();
    let junk = dir.path().join("notes.pdf");
    fs::write(&junk, "not a pdf").unwrap();
    let out = tpe(&["--apply", junk.to_str().unwrap()], dir.path());
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(text(&out.stdout).contains("skip"));
    assert_eq!(names(dir.path()), ["notes.pdf"]);
}

#[test]
fn undo_conflicts_with_paths() {
    let dir = tempfile::TempDir::new().unwrap();
    let out = tpe(&["--undo", "j.json", "a.pdf"], dir.path());
    assert!(!out.status.success());
    let out = tpe(&[], dir.path());
    assert!(!out.status.success(), "PATH is required without --undo");
}
