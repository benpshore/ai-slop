//! Build script: bake the release version and the git commit into the
//! binary.
//!
//! `TPE_VERSION` comes from the environment variable of the same name (the
//! release workflow sets it to the tag, e.g. `v0.7.0`; a leading `v` is
//! dropped) and is `0.0.0-dev` otherwise. `TPE_GIT_SHA` is the short commit
//! hash when `git` can report one, else `unknown`. Both are read with
//! `env!` in `tpe::update`.

use std::process::Command;

/// Version reported by builds that did not come from a release tag.
const DEV_VERSION: &str = "0.0.0-dev";

/// The short commit hash of `HEAD`, when git is available and this is a
/// checkout.
fn git_short_sha() -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8(output.stdout).ok()?;
    let sha = sha.trim();
    if sha.is_empty() {
        None
    } else {
        Some(sha.to_string())
    }
}

fn main() {
    println!("cargo:rerun-if-env-changed=TPE_VERSION");
    println!("cargo:rerun-if-changed=build.rs");
    if std::path::Path::new(".git/HEAD").exists() {
        println!("cargo:rerun-if-changed=.git/HEAD");
    }
    let version = std::env::var("TPE_VERSION")
        .ok()
        .map(|raw| raw.trim().trim_start_matches('v').to_string())
        .filter(|version| !version.is_empty())
        .unwrap_or_else(|| DEV_VERSION.to_string());
    println!("cargo:rustc-env=TPE_VERSION={version}");
    let sha = git_short_sha().unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=TPE_GIT_SHA={sha}");
}
