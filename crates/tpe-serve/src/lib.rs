//! `tpe-serve`: a local HTTP API over the `PDFTextract` job model, so the
//! app, a TUI, a browser page, an MCP server and scripts can all drive the
//! engine through one interface (docs/API.md).
//!
//! It exposes the app's two actions, *get text* and *get bibliography*, with
//! the app's semantics: [`tpe_app::jobs::run`] does the work,
//! [`tpe_app::jobs::JobList`] keeps the queue (one job at a time), and
//! [`tpe_app::jobs::CancelToken`] decides whether a stop is still possible.
//!
//! It listens only on loopback and checks, on every request, the `Host`
//! header (DNS rebinding), `Origin` and `Sec-Fetch-Site` (other web pages)
//! and a bearer token (CSRF, other local users); see `guard.rs` and the
//! threat model in docs/API.md.
//!
//! - [`Server`]: bind inside a Tokio runtime, then [`Server::run`].
//! - [`spawn`]: the same on a thread of its own, for programs without Tokio.

#![allow(
    clippy::must_use_candidate,
    clippy::module_name_repetitions,
    clippy::missing_errors_doc
)]

pub mod config;
mod error;
mod guard;
mod http;
mod paths;
pub mod routes;
mod server;
mod sse;
mod store;
pub mod token;

pub use config::{Config, ConfigError, DEFAULT_PORT, Limits};
pub use http::version_text;
pub use server::{RunningServer, Server, StartError, runtime, spawn};
pub use token::{Token, TokenError};
