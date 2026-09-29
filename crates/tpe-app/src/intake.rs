//! One intake path for command-line, Finder, and GPUI drop submissions.
//!
//! Paths are presentation locations, never document identities.  The engine
//! continues to use the SHA-256 of the bytes as identity; this queue only
//! coalesces repeated delivery of the same URL during one application run.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Component, Path, PathBuf};

use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum IntakeError {
    #[error("only local file URLs are accepted")]
    NotAFileUrl,
    #[error("invalid percent escape in URL")]
    InvalidEscape,
    #[error("the submitted file is not a PDF: {0}")]
    NotPdf(PathBuf),
}

/// Why a URL reached the application. All sources feed [`DocumentIntake`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntakeSource {
    Finder,
    Drop,
    CommandLine,
    FolderWatch,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntakeItem {
    pub path: PathBuf,
    pub source: IntakeSource,
}

/// In-process delivery queue. A duplicate updates the observed location but
/// does not create another extraction request.
#[derive(Default, Debug)]
pub struct DocumentIntake {
    pending: VecDeque<IntakeItem>,
    locations: HashMap<PathBuf, IntakeSource>,
}

impl DocumentIntake {
    pub fn submit_path(
        &mut self,
        path: impl AsRef<Path>,
        source: IntakeSource,
    ) -> Result<bool, IntakeError> {
        let path = normalize_path(path.as_ref());
        if !is_pdf(&path) {
            return Err(IntakeError::NotPdf(path));
        }
        if self.locations.insert(path.clone(), source).is_some() {
            return Ok(false);
        }
        self.pending.push_back(IntakeItem { path, source });
        Ok(true)
    }

    pub fn submit_url(&mut self, url: &str, source: IntakeSource) -> Result<bool, IntakeError> {
        self.submit_path(file_url_to_path(url)?, source)
    }

    pub fn drain(&mut self) -> impl Iterator<Item = IntakeItem> + '_ {
        self.pending.drain(..)
    }
}

pub fn normalize_path(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    // canonicalize resolves aliases such as `..` and symlinks when available.
    // Missing/iCloud-placeholder paths still need a stable lexical location.
    fs::canonicalize(&absolute).unwrap_or_else(|_| lexical_normalize(&absolute))
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

pub fn file_url_to_path(url: &str) -> Result<PathBuf, IntakeError> {
    let encoded = url
        .strip_prefix("file://localhost")
        .or_else(|| url.strip_prefix("file://"))
        .ok_or(IntakeError::NotAFileUrl)?;
    let bytes = encoded.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut ix = 0;
    while ix < bytes.len() {
        if bytes[ix] == b'%' {
            let pair = bytes
                .get(ix + 1..ix + 3)
                .ok_or(IntakeError::InvalidEscape)?;
            let text = std::str::from_utf8(pair).map_err(|_| IntakeError::InvalidEscape)?;
            decoded.push(u8::from_str_radix(text, 16).map_err(|_| IntakeError::InvalidEscape)?);
            ix += 3;
        } else {
            decoded.push(bytes[ix]);
            ix += 1;
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Ok(PathBuf::from(std::ffi::OsString::from_vec(decoded)))
    }
    #[cfg(not(unix))]
    String::from_utf8(decoded)
        .map(PathBuf::from)
        .map_err(|_| IntakeError::InvalidEscape)
}

fn is_pdf(path: &Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case("pdf"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_file_urls_and_percent_escapes() {
        let path = file_url_to_path("file:///Users/Ada/My%20Paper%23one.pdf").unwrap();
        assert_eq!(path, PathBuf::from("/Users/Ada/My Paper#one.pdf"));
        assert_eq!(
            file_url_to_path("https://example.test/a.pdf"),
            Err(IntakeError::NotAFileUrl)
        );
        assert_eq!(
            file_url_to_path("file:///tmp/%XX.pdf"),
            Err(IntakeError::InvalidEscape)
        );
    }

    #[test]
    fn intake_deduplicates_equivalent_locations_and_keeps_multiple_files() {
        let mut intake = DocumentIntake::default();
        assert!(
            intake
                .submit_path("/tmp/a/../paper.pdf", IntakeSource::Finder)
                .unwrap()
        );
        assert!(
            !intake
                .submit_url("file:///tmp/paper.pdf", IntakeSource::Drop)
                .unwrap()
        );
        assert!(
            intake
                .submit_path("/tmp/second.PDF", IntakeSource::Finder)
                .unwrap()
        );
        let items: Vec<_> = intake.drain().collect();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].path, PathBuf::from("/tmp/paper.pdf"));
    }

    #[test]
    fn rejects_non_pdf_documents() {
        let error = DocumentIntake::default()
            .submit_path("/tmp/readme.txt", IntakeSource::Drop)
            .unwrap_err();
        assert!(matches!(error, IntakeError::NotPdf(_)));
    }
}
