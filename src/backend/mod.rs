//! Extraction backends. A backend opens complete immutable bytes and yields
//! per-page positioned spans; it never orders, repairs or interprets text.

use std::collections::BTreeMap;

use thiserror::Error;

use crate::schema::{BackendIdentity, PageText};

pub mod lopdf_backend;

#[derive(Debug, Error)]
pub enum BackendError {
    #[error("not a PDF or damaged: {0}")]
    Malformed(String),
    #[error("encrypted: {0}")]
    Encrypted(EncryptionProblem),
    #[error("page {page} out of range 1..={count}")]
    PageRange { page: u32, count: u32 },
    #[error("page {page}: {message}")]
    Page { page: u32, message: String },
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("limit exceeded: {0}")]
    Limit(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncryptionProblem {
    PasswordRequired,
    WrongPassword,
    UnsupportedCipher,
}

impl std::fmt::Display for EncryptionProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::PasswordRequired => "password required",
            Self::WrongPassword => "wrong password",
            Self::UnsupportedCipher => "unsupported cipher",
        })
    }
}

/// An open document held by its owning worker for the life of the job.
pub trait DocumentSession {
    fn page_count(&self) -> u32;
    /// Positioned spans for one 1-based page. `lines`/`text` are left empty.
    fn page_text(&mut self, page: u32) -> Result<PageText, BackendError>;
    /// String-valued `/Info` entries, keys without the leading `/`.
    fn info(&self) -> BTreeMap<String, String>;
}

pub trait Extractor: Send + Sync {
    fn identity(&self) -> BackendIdentity;
    fn open(
        &self,
        bytes: &[u8],
        password: Option<&str>,
    ) -> Result<Box<dyn DocumentSession>, BackendError>;
}

/// Look up a backend by CLI name.
pub fn by_name(name: &str) -> Option<Box<dyn Extractor>> {
    match name {
        "lopdf" => Some(Box::new(lopdf_backend::LopdfBackend::default())),
        _ => None,
    }
}

/// Names accepted by [`by_name`].
pub const NAMES: &[&str] = &["lopdf"];
