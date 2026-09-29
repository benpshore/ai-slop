use super::*;
use std::fs;
use tempfile::tempdir;

fn config(root: &Path) -> Config {
    Config {
        watched_folders: vec![root.into()],
        output_location: root.join("out"),
        ledger_location: root.join("ledger.sqlite"),
        stable_checks: 2,
        stable_interval_ms: 1,
        ..Config::default()
    }
}

#[test]
fn repeated_content_and_duplicate_events_execute_once_but_keep_paths() {
    let dir = tempdir().unwrap();
    let cfg = config(dir.path());
    let a = dir.path().join("a.pdf");
    let b = dir.path().join("copy.pdf");
    fs::write(&a, b"%PDF same").unwrap();
    fs::copy(&a, &b).unwrap();
    let sa = wait_stable(&a, &cfg).unwrap();
    let sb = wait_stable(&b, &cfg).unwrap();
    let mut ledger = IntakeLedger::open(&dir.path().join("intake.db")).unwrap();
    assert!(ledger.observe(&sa.hash, &sa.source).unwrap());
    assert!(!ledger.observe(&sa.hash, &sa.source).unwrap());
    assert!(!ledger.observe(&sb.hash, &sb.source).unwrap());
    assert_eq!(ledger.state(&sa.hash).unwrap().as_deref(), Some("queued"));
    let count: i64 = ledger
        .conn
        .query_row("SELECT count(*) FROM observations", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 2);
}

#[test]
fn rename_into_folder_is_found_by_reconciliation() {
    let dir = tempdir().unwrap();
    let cfg = config(dir.path());
    let staging = tempdir().unwrap();
    let source = staging.path().join("paper.pdf");
    fs::write(&source, b"%PDF rename").unwrap();
    fs::rename(&source, dir.path().join("paper.pdf")).unwrap();
    assert_eq!(reconcile(&cfg).unwrap().len(), 1);
}

#[test]
fn partial_hidden_database_output_and_deleted_inputs_are_ignored() {
    let dir = tempdir().unwrap();
    let cfg = config(dir.path());
    fs::create_dir(&cfg.output_location).unwrap();
    for name in [".hidden.pdf", "download.partial.pdf", "state.sqlite"] {
        fs::write(dir.path().join(name), b"x").unwrap();
    }
    fs::write(cfg.output_location.join("made.pdf"), b"x").unwrap();
    assert!(reconcile(&cfg).unwrap().is_empty());
    assert!(wait_stable(&dir.path().join("deleted.pdf"), &cfg).is_err());
}

#[test]
fn copy_in_progress_requires_repeated_unchanged_reads() {
    let dir = tempdir().unwrap();
    let mut cfg = config(dir.path());
    cfg.stable_checks = 3;
    let path = dir.path().join("paper.pdf");
    fs::write(&path, b"%PDF stable").unwrap();
    let snapshot = wait_stable(&path, &cfg).unwrap();
    assert_eq!(snapshot.bytes, b"%PDF stable");
}

#[test]
fn restart_recovers_processing_as_queued() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("intake.db");
    let path = dir.path().join("a.pdf");
    fs::write(&path, b"%PDF").unwrap();
    let cfg = config(dir.path());
    let snap = wait_stable(&path, &cfg).unwrap();
    {
        let mut ledger = IntakeLedger::open(&db).unwrap();
        ledger.observe(&snap.hash, &snap.source).unwrap();
        ledger
            .transition(&snap.hash, State::Processing, None)
            .unwrap();
    }
    let ledger = IntakeLedger::open(&db).unwrap();
    assert_eq!(ledger.recover().unwrap().len(), 1);
    assert_eq!(ledger.state(&snap.hash).unwrap().as_deref(), Some("queued"));
}
