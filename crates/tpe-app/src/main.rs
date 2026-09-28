//! `tpe-app` binary: the GPUI workbench (macOS only).
//!
//! `tpe-app <ledger.sqlite>` (or `TPE_LEDGER=<file> tpe-app`) opens the ledger
//! read-only and shows the corpus list, the document view and the Ask panel.
//! On other operating systems the binary prints a notice and exits 0 so the
//! workspace builds and the library tests run everywhere.

#![allow(
    clippy::must_use_candidate,
    clippy::module_name_repetitions,
    clippy::missing_errors_doc
)]

#[cfg(target_os = "macos")]
mod gui;

#[cfg(target_os = "macos")]
fn main() {
    use std::path::PathBuf;

    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("TPE_LEDGER").map(PathBuf::from));
    let Some(path) = path else {
        eprintln!("usage: tpe-app <ledger.sqlite>    (or set TPE_LEDGER)");
        std::process::exit(2);
    };
    gui::run(path);
}

#[cfg(not(target_os = "macos"))]
fn main() {
    println!("GUI is macOS-only for now");
}
