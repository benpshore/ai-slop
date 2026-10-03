//! Bibliography CSV publication must never change selected PDF sources.
mod common;

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

fn bibliography(csv: &Path, inputs: &[&Path]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tpe"))
        .args(["bibliography", "--backend", "lopdf", "--csv"])
        .arg(csv)
        .args(inputs)
        .output()
        .unwrap()
}

fn assert_rejected(output: &Output) {
    assert!(!output.status.success());
    assert!(
        output.stdout.is_empty(),
        "batch must fail before extraction"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("CSV output aliases input"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn csv_cannot_initialize_an_empty_selected_input() {
    let root = TempDir::new().unwrap();
    let input = root.path().join("empty.pdf");
    fs::write(&input, []).unwrap();
    assert_rejected(&bibliography(&input, &[&input]));
    assert!(fs::read(input).unwrap().is_empty());
}

#[test]
fn csv_cannot_create_a_missing_selected_input() {
    let root = TempDir::new().unwrap();
    let input = root.path().join("missing.pdf");
    assert_rejected(&bibliography(&input, &[&input]));
    assert!(!input.exists());
}

#[test]
#[cfg(unix)]
fn csv_aliases_of_later_inputs_are_rejected_before_the_first_append() {
    for symbolic in [false, true] {
        let root = TempDir::new().unwrap();
        let first = root.path().join("paper.pdf");
        let later = root.path().join("empty.pdf");
        let csv = root.path().join("output.csv");
        let original = common::synthetic_paper();
        fs::write(&first, &original).unwrap();
        fs::write(&later, []).unwrap();
        if symbolic {
            std::os::unix::fs::symlink(&later, &csv).unwrap();
        } else {
            fs::hard_link(&later, &csv).unwrap();
        }
        assert_rejected(&bibliography(&csv, &[&first, &later]));
        assert!(fs::read(&later).unwrap().is_empty());
        assert_eq!(fs::read(first).unwrap(), original);
    }
}

#[test]
fn separate_csv_still_appends_without_changing_the_pdf() {
    let root = TempDir::new().unwrap();
    let input = root.path().join("paper.pdf");
    let csv = root.path().join("output.csv");
    let original = common::synthetic_paper();
    fs::write(&input, &original).unwrap();
    for expected_rows in [4, 7] {
        let output = bibliography(&csv, &[&input]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            fs::read_to_string(&csv).unwrap().lines().count(),
            expected_rows
        );
        assert_eq!(fs::read(&input).unwrap(), original);
    }
}

#[test]
fn missing_input_lexical_aliases_are_rejected_before_extraction() {
    for alias in [
        "./missing.pdf",
        "subdir/../missing.pdf",
        "subdir/.././missing.pdf",
    ] {
        let root = TempDir::new().unwrap();
        fs::create_dir(root.path().join("subdir")).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_tpe"))
            .current_dir(root.path())
            .args([
                "bibliography",
                "--backend",
                "lopdf",
                "--csv",
                alias,
                "missing.pdf",
            ])
            .output()
            .unwrap();
        assert_rejected(&output);
        assert!(!root.path().join("missing.pdf").exists());
    }
}

#[test]
#[cfg(unix)]
fn dangling_symlink_aliases_cannot_create_selected_sources() {
    for source_is_symlink in [false, true] {
        let root = TempDir::new().unwrap();
        let first = root.path().join("first.pdf");
        let missing = root.path().join("missing.pdf");
        let alias = root.path().join("alias");
        let intermediate = root.path().join("intermediate");
        let original = common::synthetic_paper();
        fs::write(&first, &original).unwrap();
        std::os::unix::fs::symlink("missing.pdf", &intermediate).unwrap();
        std::os::unix::fs::symlink("intermediate", &alias).unwrap();
        let (csv, input) = if source_is_symlink {
            (&missing, &alias)
        } else {
            (&alias, &missing)
        };
        assert_rejected(&bibliography(csv, &[&first, input]));
        assert!(!missing.exists());
        assert_eq!(fs::read(&first).unwrap(), original);
        assert_eq!(fs::read_link(&alias).unwrap(), Path::new("intermediate"));
    }
}

#[test]
#[cfg(unix)]
fn missing_input_through_a_symlinked_directory_is_preserved() {
    let root = TempDir::new().unwrap();
    let directory = root.path().join("real");
    let alias = root.path().join("alias");
    fs::create_dir(&directory).unwrap();
    std::os::unix::fs::symlink(&directory, &alias).unwrap();
    let input = directory.join("missing.pdf");
    assert_rejected(&bibliography(&alias.join("missing.pdf"), &[&input]));
    assert!(!input.exists());
}
