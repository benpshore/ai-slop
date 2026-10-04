//! `tpe`: PDF text, metadata and citation extraction engine.
//!
//! Module ownership for the baseline PR (each module has one owner and one
//! integration point, the types in [`schema`]):
//! - `schema`: shared versioned record types (the contract).
//! - `acquire`: immutable snapshot + content hash of an input file.
//! - `backend`: extractor trait plus the pure-Rust `lopdf` backend.
//! - `reading_order`: positioned spans -> ordered lines and page text.
//! - `text_cleanup`: running heads, page numbers, stamps, scripts and hyphens out of the text.
//! - `metadata`: title/authors/DOI/arXiv/year from the Info dict and page 1.
//! - `citations`: reference-list segmentation, entry parsing, in-text markers.
//! - `ledger`: `SQLite` schema and idempotent writer.
//! - `pipeline`: glue that runs the stages for one document.
//!
//! Evaluation harness against `arXiv` `LaTeX` ground truth (batch 2):
//! - `corpus`: manifest of CC-BY papers, cached download and source unpacking.
//! - `latex_refs`: `.bbl`/`.bib`/`\cite` parsing into reference ground truth.
//! - `eval`: reference matching, field accuracy, marker resolution, report.
//!
//! Native backends (batch 3), each behind a Cargo feature so the default
//! build stays pure Rust: `backend::pdfium_backend` (feature `pdfium`) and
//! `backend::docling_backend` (feature `docling`, implies `pdfium`). Figure
//! bytes are exported by `pipeline`, never inlined into text.

#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::module_name_repetitions,
    clippy::must_use_candidate,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::too_many_lines
)]

pub mod acquire;
pub mod backend;
pub mod bibliography;
pub mod citations;
pub mod corpus;
pub mod eval;
#[cfg(feature = "grobid")]
pub mod grobid;
pub mod latex_refs;
pub mod ledger;
pub mod metadata;
pub mod pipeline;
pub mod reading_order;
pub mod regions;
pub mod resolve;
pub mod router;
pub mod schema;
pub mod text_cleanup;

/// Complete-file publication shared by the CLI and application jobs.
pub mod publication;
