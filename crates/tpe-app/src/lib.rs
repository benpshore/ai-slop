//! `tpe-app`: the GPUI workbench skeleton for the text-processing-engine.
//!
//! The crate is split so that everything testable lives in this library and
//! compiles on every platform, while the GPUI front end (`src/gui.rs` in the
//! binary) is built on macOS and Linux (Wayland and X11):
//!
//! - [`ledger`]: read-only access to the engine ledger (`tpe extract --db`),
//!   producing plain rows for the corpus list and the document view.
//! - [`view`]: pure view models (labels, reading-order line numbering, citation
//!   marker placement, text scale, pane cycling, the model context string).
//! - [`tpe_ai`]: request builders, response parsers and the blocking `ask()`
//!   call for the Anthropic Messages API and the `OpenAI` chat completions API.
//! - [`keys`]: the `KeyProvider` trait the GUI uses to obtain API keys, with an
//!   environment-variable implementation. The credentials crate adapter is
//!   wired by the integration owner once both crates are on `main`.
//!
//! No test in this crate touches the network or the file system outside a
//! temporary in-memory `SQLite` database.

#![allow(
    clippy::must_use_candidate,
    clippy::module_name_repetitions,
    clippy::missing_errors_doc
)]

pub mod keys;
pub mod ledger;
pub mod tpe_ai;
pub mod view;
