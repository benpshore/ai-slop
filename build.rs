//! Embed the supplied release tag or the checked-out source identity.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn watch(path: &Path) {
    if path.exists() {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

fn git_identity(root: &Path) -> Option<String> {
    let top = PathBuf::from(git(root, &["rev-parse", "--show-toplevel"])?);
    if top.canonicalize().ok()? != root.canonicalize().ok()? {
        return None;
    }
    // Worktrees keep HEAD/index in their own git directory and tags/branch
    // refs in the common directory. Watch loose and packed refs, not just .git.
    for name in [
        "HEAD",
        "index",
        "refs",
        "packed-refs",
        "shallow",
        "commondir",
    ] {
        if let Some(path) = git(root, &["rev-parse", "--git-path", name]) {
            watch(&root.join(path));
        }
    }
    // describe --dirty observes tracked content, including non-Rust files.
    // Untracked output does not change the identity or invalidate this cache.
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-z"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    for name in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
    {
        let name = std::str::from_utf8(name).ok()?;
        // Missing tracked files also make the tree dirty. Cargo must keep
        // watching their path so deletion/restoration cannot retain a tag.
        println!("cargo:rerun-if-changed={}", root.join(name).display());
    }
    git(
        root,
        &[
            "describe", "--tags", "--match", "v[0-9]*", "--always", "--dirty",
        ],
    )
    .map(|description| {
        description
            .strip_prefix('v')
            .map_or_else(|| format!("git-{description}"), str::to_owned)
    })
}

fn main() {
    println!("cargo:rerun-if-env-changed=TPE_VERSION");
    println!("cargo:rerun-if-changed=build.rs");
    let root = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").expect("Cargo supplies its manifest directory"),
    );
    watch(&root.join(".git"));
    let source = git_identity(&root).unwrap_or_else(|| "source-archive".to_owned());
    let version = match env::var("TPE_VERSION") {
        Ok(value) => {
            let normalized = value.trim().strip_prefix('v').unwrap_or(value.trim());
            assert!(
                !normalized.is_empty() && !value.chars().any(char::is_control),
                "TPE_VERSION must be a nonempty single-line build identity"
            );
            normalized.to_owned()
        }
        Err(env::VarError::NotPresent) => source,
        Err(env::VarError::NotUnicode(_)) => panic!("TPE_VERSION must be valid Unicode"),
    };
    println!("cargo:rustc-env=TPE_BUILD_VERSION={version}");
}
