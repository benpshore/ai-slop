//! Exercise the embedded version and invalidation of a real Cargo build cache.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn checked(mut command: Command) -> Output {
    let output = command.output().expect("run fixture command");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn git(root: &Path, args: &[&str]) {
    let mut command = Command::new("git");
    command.arg("-C").arg(root).args(args);
    checked(command);
}

fn fixture(root: &Path) {
    fs::create_dir(root.join("src")).expect("create fixture source");
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"identity-probe\"\nedition = \"2024\"\n",
    )
    .expect("write fixture manifest");
    fs::write(root.join("build.rs"), include_str!("../build.rs"))
        .expect("copy actual build script");
    fs::write(
        root.join("rust-toolchain.toml"),
        include_str!("../rust-toolchain.toml"),
    )
    .expect("copy toolchain pin");
    fs::write(
        root.join("src/main.rs"),
        "fn main() { println!(\"{}\", env!(\"TPE_BUILD_VERSION\")); }\n",
    )
    .expect("write fixture binary");
    fs::write(root.join("NOTE"), "clean\n").expect("write tracked non-Rust source");
    let mut command = Command::new(env!("CARGO"));
    command
        .current_dir(root)
        .args(["generate-lockfile", "--offline"]);
    checked(command);
}

fn build(root: &Path, version: Option<&str>) -> Output {
    let mut command = Command::new(env!("CARGO"));
    command
        .current_dir(root)
        .args(["build", "--release", "--locked", "--offline"])
        .env("CARGO_TARGET_DIR", root.join("target"))
        .env_remove("TPE_VERSION");
    if let Some(version) = version {
        command.env("TPE_VERSION", version);
    }
    command.output().expect("build identity probe")
}

fn assert_identity(root: &Path, version: Option<&str>, expected: &str) {
    let output = build(root, version);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = checked(Command::new(root.join("target/release/identity-probe")));
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), expected);
}

#[test]
fn actual_cli_matches_its_embedded_build_identity() {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tpe"));
    command.arg("--version");
    let output = checked(command);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        format!("tpe {}", env!("TPE_BUILD_VERSION"))
    );
}

#[test]
fn release_identity_tracks_environment_tags_and_dirty_source_in_cached_builds() {
    let dir = tempfile::tempdir().expect("fixture directory");
    let root = dir.path();
    fixture(root);
    git(root, &["init", "--quiet"]);
    git(
        root,
        &["config", "user.email", "identity-test@example.invalid"],
    );
    git(root, &["config", "user.name", "Build identity fixture"]);
    git(root, &["add", "."]);
    git(root, &["commit", "--quiet", "-m", "fixture"]);
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--short", "HEAD"]);
    let output = checked(command);
    let commit = String::from_utf8_lossy(&output.stdout);
    assert_identity(root, None, &format!("git-{}", commit.trim()));
    git(root, &["tag", "v9.8.7"]);

    assert_identity(root, None, "9.8.7");
    assert_identity(root, Some("v10.11.12"), "10.11.12");
    assert_identity(root, Some("v10.11.13"), "10.11.13");
    assert_identity(root, None, "9.8.7");
    fs::write(root.join("NOTE"), "edited\n").expect("edit tracked non-Rust source");
    assert_identity(root, None, "9.8.7-dirty");
    git(root, &["checkout", "--", "NOTE"]);
    assert_identity(root, None, "9.8.7");
    git(root, &["tag", "-a", "v9.9.0", "-m", "new release"]);
    assert_identity(root, None, "9.9.0");
    git(root, &["pack-refs", "--all", "--prune"]);
    assert_identity(root, None, "9.9.0");
    git(root, &["tag", "-d", "v9.9.0"]);
    assert_identity(root, None, "9.8.7");

    for invalid in ["\ninvalid", "", "v"] {
        let output = build(root, Some(invalid));
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("TPE_VERSION must be"));
    }
}

#[test]
fn source_archive_does_not_claim_the_manifest_version_as_a_release() {
    let dir = tempfile::tempdir().expect("archive directory");
    fixture(dir.path());
    assert_identity(dir.path(), None, "source-archive");
    assert_identity(dir.path(), Some("v12.13.14"), "12.13.14");
}

#[test]
fn cached_worktree_build_observes_common_refs_and_its_own_head() {
    let dir = tempfile::tempdir().expect("repository directory");
    let root = dir.path();
    fixture(root);
    git(root, &["init", "--quiet"]);
    git(
        root,
        &["config", "user.email", "identity-test@example.invalid"],
    );
    git(root, &["config", "user.name", "Build identity fixture"]);
    git(root, &["add", "."]);
    git(root, &["commit", "--quiet", "-m", "fixture"]);
    git(root, &["tag", "v2.3.4"]);

    let worktree = tempfile::tempdir().expect("worktree directory");
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .args(["worktree", "add", "--detach"])
        .arg(worktree.path())
        .arg("HEAD");
    checked(command);
    assert_identity(worktree.path(), None, "2.3.4");
    git(root, &["tag", "-a", "v2.3.5", "-m", "common tag"]);
    assert_identity(worktree.path(), None, "2.3.5");
    git(
        root,
        &[
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "new identity, same files",
        ],
    );
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--short", "HEAD"]);
    let output = checked(command);
    let commit = String::from_utf8_lossy(&output.stdout);
    git(worktree.path(), &["checkout", "--detach", commit.trim()]);
    assert_identity(
        worktree.path(),
        None,
        &format!("2.3.5-1-g{}", commit.trim()),
    );
}
