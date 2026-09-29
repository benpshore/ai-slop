//! `tpe-app` binary: `PDFTextract`, the GPUI app (macOS only).
//!
//! `tpe-app [paper.pdf ...]` opens the window and queues any given PDFs for
//! text extraction; `tpe-app --version` prints the version and exits: the
//! tag-derived version `bundle.sh` passes as `PDFTEXTRACT_VERSION` at build
//! time (the version is the git tag, AGENTS.md), else the manifest's. On
//! other operating systems the binary prints a notice and exits 0 so the
//! workspace builds and the library tests run everywhere.

#![allow(
    clippy::must_use_candidate,
    clippy::module_name_repetitions,
    clippy::missing_errors_doc
)]

#[cfg(target_os = "macos")]
mod gui;
#[cfg(target_os = "macos")]
mod services;

#[cfg(target_os = "macos")]
fn main() {
    use std::path::PathBuf;

    let args: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    if args
        .iter()
        .any(|arg| arg == std::path::Path::new("--version"))
    {
        println!("tpe-app {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    gui::run(args);
}

/// The tag-derived version when built by `bundle.sh`, else the manifest's.
#[cfg(target_os = "macos")]
fn version() -> &'static str {
    option_env!("PDFTEXTRACT_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"))
}

#[cfg(not(target_os = "macos"))]
fn main() {
    println!("PDFTextract is macOS-only for now");
}
