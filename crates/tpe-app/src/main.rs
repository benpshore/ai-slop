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

/// The probe's two helper commands, used by `probe.sh`:
/// `--probe-merge STDERR_LOG REPORT_JSON` adds GPUI's own frame timings to a
/// report, and `--probe-make-pdf PAGES OUT.pdf` writes a long synthetic PDF.
/// True when the arguments asked for one, whether or not it worked.
fn probe_tool(args: &[std::path::PathBuf]) -> bool {
    let result = match args {
        [flag, log, report] if flag == std::path::Path::new("--probe-merge") => {
            tpe_app::probe::merge_gpui_log(log, report)
                .map(|n| format!("merged {n} GPUI frame timings into {}", report.display()))
        }
        [flag, pages, out] if flag == std::path::Path::new("--probe-make-pdf") => pages
            .to_string_lossy()
            .parse::<usize>()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))
            .and_then(|pages| {
                std::fs::write(out, tpe_app::probe::synthetic_pdf(pages))
                    .map(|()| format!("wrote {pages} pages to {}", out.display()))
            }),
        _ => return false,
    };
    match result {
        Ok(message) => println!("{message}"),
        Err(error) => {
            eprintln!("probe: {error}");
            std::process::exit(1);
        }
    }
    true
}

#[cfg(target_os = "macos")]
mod gui;
#[cfg(target_os = "macos")]
mod services;

#[cfg(target_os = "macos")]
fn main() {
    use std::path::PathBuf;

    let args: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    if probe_tool(&args) {
        return;
    }
    if args
        .iter()
        .any(|arg| arg == std::path::Path::new("--version"))
    {
        println!("PDFTextract {}", version());
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
    let args: Vec<std::path::PathBuf> = std::env::args_os()
        .skip(1)
        .map(std::path::PathBuf::from)
        .collect();
    if probe_tool(&args) {
        return;
    }
    println!("PDFTextract is macOS-only for now");
}
