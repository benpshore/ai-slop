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

    let arguments =
        tpe_app::launch::parse_args(std::env::args_os().skip(1)).unwrap_or_else(|error| {
            eprintln!("{error}");
            std::process::exit(2);
        });
    let ledger = arguments.ledger.unwrap_or_else(|| {
        let home = std::env::var_os("HOME").unwrap_or_default();
        PathBuf::from(home).join("Library/Application Support/Text Processing Engine/ledger.sqlite")
    });
    gui::run(ledger, arguments.documents);
}

#[cfg(not(target_os = "macos"))]
fn main() {
    println!("GUI is macOS-only for now");
}
