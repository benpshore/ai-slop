//! Browser *model* for the research-browser track: everything the embedded
//! Chromium (CEF) shell must decide before and after a navigation, without
//! rendering anything itself.
//!
//! - [`url`]: strict `http`/`https` URL normalisation and percent-coding helpers.
//! - [`doi`]: DOI and arXiv identifier detection in URLs, HTML and plain text.
//! - [`hosts`]: the research-mode allowlist of scholarly hosts and library-proxy handling.
//! - [`pdf`]: PDF-link classification from URLs, `Content-Type` and `Content-Disposition`.
//! - [`cookies`]: cookie jar, `Set-Cookie` parsing, Netscape `cookies.txt` and the bridge to
//!   the credential store (service `cookies`, one secret per host).
//! - [`session`]: [`BrowserSession`], which ties the above into navigation decisions.
//! - [`bundle`]: the macOS app-bundle layout a CEF application needs.
//!
//! Nothing in this crate loads or renders a page. The real embedding lives
//! behind the `cef` feature in the `embed` module and is a documented TODO;
//! see `docs/BROWSER.md`.

#![allow(
    clippy::must_use_candidate,
    clippy::module_name_repetitions,
    clippy::missing_errors_doc
)]

pub mod bundle;
pub mod cookies;
pub mod doi;
pub mod hosts;
pub mod html;
pub mod pdf;
pub mod session;
pub mod url;

pub use cookies::{Cookie, CookieJar, CookieSecretStore, MemoryCookieStore};
pub use hosts::{HostDecision, LibraryProxy, ResearchPolicy, is_scholarly_host};
pub use pdf::{LinkClassification, PdfVerdict, ResponseHints};
pub use session::{BrowserSession, Intercept, Navigation, PageFacts};
pub use url::NormalizedUrl;

/// Errors raised by the browser model.
#[derive(Debug, thiserror::Error)]
pub enum BrowserError {
    /// The text is not a URL this browser will ever visit.
    #[error("invalid url: {0}")]
    InvalidUrl(String),
    /// Only `http` and `https` are browsed; `file`, `javascript`, `data` ... are refused.
    #[error("unsupported scheme `{0}` (only http and https are browsed)")]
    UnsupportedScheme(String),
    /// A Netscape `cookies.txt` line that is neither a comment nor seven tab-separated fields.
    #[error("malformed cookies.txt line {line}: {reason}")]
    CookieLine {
        /// One-based line number.
        line: usize,
        /// What was wrong with it.
        reason: String,
    },
    /// The credential store behind the cookie bridge failed.
    #[error("cookie store: {0}")]
    Store(String),
    /// A stored cookie secret was not valid JSON.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    /// Chromium embedding is not compiled in or not implemented yet.
    #[error("embedding unavailable: {0}")]
    Unavailable(String),
}

/// Chromium embedding through the `cef` crate (tauri-apps/cef-rs).
///
/// The only offline evidence available while this spike was written was the
/// docs.rs crate page for `cef` 154.2.0+154.0.28 (crate name, version, its
/// dependency list: `cef-dll-sys`, `libloading`, `objc2`, and a one-line
/// description). No API surface could be cited, so this module deliberately
/// contains **no calls into `cef`**: it only reserves the entry points the
/// shell will need and returns [`BrowserError::Unavailable`] until the
/// integration owner fills them in against the real crate documentation.
/// The design (process model, bundle layout, cookie injection, request
/// interception) is written down in `docs/BROWSER.md`.
#[cfg(feature = "cef")]
pub mod embed {
    use crate::BrowserError;
    use crate::bundle::MacBundleLayout;

    /// Handle to an initialised CEF runtime. Placeholder: carries no state yet.
    #[derive(Debug)]
    pub struct CefRuntime {
        _private: (),
    }

    /// Initialise CEF for the given bundle layout.
    ///
    /// TODO(cef): call the `cef` crate's process-entry and initialise functions
    /// (the C API names are `cef_execute_process` and `cef_initialize`; the Rust
    /// spellings must be taken from the crate docs, not guessed), passing the
    /// framework path from `layout.cef_library` and the helper bundle names.
    pub fn initialize(layout: &MacBundleLayout) -> Result<CefRuntime, BrowserError> {
        Err(BrowserError::Unavailable(format!(
            "cef feature compiled but embedding is not implemented; expected framework at {}",
            layout.cef_library
        )))
    }

    /// Run the CEF message loop until the last browser closes.
    ///
    /// TODO(cef): `cef_run_message_loop` / `cef_shutdown` equivalents.
    pub fn run(runtime: CefRuntime) -> Result<(), BrowserError> {
        let CefRuntime { _private: () } = runtime;
        Err(BrowserError::Unavailable(
            "cef message loop is not implemented".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::BrowserError;

    #[test]
    fn error_messages_are_readable() {
        let e = BrowserError::UnsupportedScheme("javascript".to_string());
        assert_eq!(
            e.to_string(),
            "unsupported scheme `javascript` (only http and https are browsed)"
        );
        let e = BrowserError::CookieLine {
            line: 3,
            reason: "expected 7 fields".to_string(),
        };
        assert!(e.to_string().contains("line 3"));
    }
}
