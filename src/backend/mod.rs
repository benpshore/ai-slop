//! Extraction backends. A backend opens complete immutable bytes and yields
//! per-page positioned spans; it never orders, repairs or interprets text.
//!
//! `lopdf` is always compiled in. `pdfium` (feature `pdfium`) and
//! `docling-text` / `docling` (feature `docling`, which implies `pdfium`)
//! need native artifacts at run time; see `docs/NATIVE.md`.

use std::collections::BTreeMap;

use lopdf::content::{Content, Operation};
use lopdf::{Document, Object, Stream, dictionary};
use thiserror::Error;

use crate::schema::{BackendIdentity, PageText};

#[cfg(feature = "docling")]
pub mod docling_backend;
#[cfg(any(feature = "docling", test))]
mod docling_layout;
#[cfg(feature = "docling-text")]
pub mod docling_text_backend;
#[cfg(feature = "liteparse-layout")]
pub mod liteparse_layout_backend;
pub mod lopdf_backend;
#[cfg(any(feature = "mupdf", feature = "poppler"))]
pub mod native_provider;
#[cfg(feature = "pdf-oxide")]
pub mod pdf_oxide_backend;
#[cfg(feature = "pdfium")]
pub mod pdfium_backend;

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
    /// Bytes of a figure recorded in `PageText::figures` (same page/index),
    /// taken once; `None` when the backend has no pixels for it.
    fn take_figure_bytes(&mut self, page: u32, index: u32) -> Option<Vec<u8>> {
        let _ = (page, index);
        None
    }
}

pub trait Extractor: Send + Sync {
    fn identity(&self) -> BackendIdentity;
    fn open(
        &self,
        bytes: &[u8],
        password: Option<&str>,
    ) -> Result<Box<dyn DocumentSession>, BackendError>;
    /// `true` when the spans of a page already come in reading order (by
    /// `seq`), so the engine must not re-order them geometrically.
    fn provides_reading_order(&self) -> bool {
        false
    }
    /// `true` when `page_text` also supplies the final ordered lines and text.
    /// Raw spans remain extraction evidence and must not replace those lines.
    fn provides_line_layout(&self) -> bool {
        false
    }
}

/// Look up a compiled-in backend by CLI name.
pub fn by_name(name: &str) -> Option<Box<dyn Extractor>> {
    match name {
        "lopdf" => Some(Box::new(lopdf_backend::LopdfBackend::default())),
        #[cfg(feature = "mupdf")]
        "mupdf" => Some(Box::new(native_provider::NativeProviderBackend::new(
            native_provider::Engine::MuPdf,
        ))),
        #[cfg(feature = "poppler")]
        "poppler" => Some(Box::new(native_provider::NativeProviderBackend::new(
            native_provider::Engine::Poppler,
        ))),
        #[cfg(feature = "pdf-oxide")]
        "pdf-oxide" => Some(Box::new(pdf_oxide_backend::PdfOxideBackend)),
        #[cfg(feature = "pdfium")]
        "pdfium" => Some(Box::new(pdfium_backend::PdfiumBackend::default())),
        #[cfg(feature = "liteparse-layout")]
        "liteparse-layout" => Some(Box::new(
            liteparse_layout_backend::LiteParseLayoutBackend::default(),
        )),
        #[cfg(feature = "docling-text")]
        "docling-text" => Some(Box::new(docling_text_backend::DoclingTextBackend)),
        #[cfg(feature = "docling")]
        "docling" => Some(Box::new(docling_backend::DoclingBackend::full())),
        _ => None,
    }
}

/// Names accepted by [`by_name`] in this build.
pub const NAMES: &[&str] = &[
    "lopdf",
    #[cfg(feature = "mupdf")]
    "mupdf",
    #[cfg(feature = "poppler")]
    "poppler",
    #[cfg(feature = "pdfium")]
    "pdfium",
    #[cfg(feature = "pdf-oxide")]
    "pdf-oxide",
    #[cfg(feature = "docling-text")]
    "docling-text",
    #[cfg(feature = "docling")]
    "docling",
    #[cfg(feature = "liteparse-layout")]
    "liteparse-layout",
];

/// Every backend name the engine knows, compiled in or not.
pub const ALL_KNOWN: &[&str] = &[
    "lopdf",
    "mupdf",
    "poppler",
    "pdfium",
    "pdf-oxide",
    "docling-text",
    "docling",
    "liteparse-layout",
];

/// Backend names compiled into this build (same as [`NAMES`]).
pub fn available() -> Vec<&'static str> {
    NAMES.to_vec()
}

/// Every backend name the engine knows (same as [`ALL_KNOWN`]).
pub fn all_known() -> &'static [&'static str] {
    ALL_KNOWN
}

/// Cargo feature that compiles `name` in; `None` for `lopdf` and unknown names.
pub fn feature_for(name: &str) -> Option<&'static str> {
    match name {
        "pdfium" => Some("pdfium"),
        "mupdf" => Some("mupdf"),
        "poppler" => Some("poppler"),
        "pdf-oxide" => Some("pdf-oxide"),
        "docling-text" => Some("docling-text"),
        "docling" => Some("docling"),
        "liteparse-layout" => Some("liteparse-layout"),
        _ => None,
    }
}

/// A one-page PDF (Helvetica, the word `probe`) for checking that a backend
/// opens documents, e.g. `tpe backends`.
pub fn probe_pdf() -> Result<Vec<u8>, BackendError> {
    let mut doc = Document::with_version("1.5");
    let tree_id = doc.new_object_id();
    let font_id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
    });
    let resources_id = doc.add_object(dictionary! {
        "Font" => dictionary! { "F1" => font_id },
    });
    let operations = vec![
        Operation::new("BT", vec![]),
        Operation::new("Tf", vec!["F1".into(), 12_i32.into()]),
        Operation::new("Td", vec![72_i32.into(), 720_i32.into()]),
        Operation::new("Tj", vec![Object::string_literal("probe")]),
        Operation::new("ET", vec![]),
    ];
    let content = Content { operations }
        .encode()
        .map_err(|err| BackendError::Malformed(format!("probe content: {err}")))?;
    let content_id = doc.add_object(Stream::new(dictionary! {}, content));
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => tree_id,
        "Contents" => content_id,
        "Resources" => resources_id,
    });
    let tree = dictionary! {
        "Type" => "Pages",
        "Kids" => vec![Object::Reference(page_id)],
        "Count" => Object::Integer(1),
        "MediaBox" => vec![0_i32.into(), 0_i32.into(), 612_i32.into(), 792_i32.into()],
    };
    doc.objects.insert(tree_id, Object::Dictionary(tree));
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => tree_id,
    });
    doc.trailer.set("Root", catalog_id);
    let mut bytes: Vec<u8> = Vec::new();
    doc.save_to(&mut bytes)
        .map_err(|err| BackendError::Malformed(format!("probe save: {err}")))?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::{ALL_KNOWN, NAMES, all_known, available, by_name, feature_for, probe_pdf};

    #[test]
    fn every_available_name_resolves_and_is_known() {
        assert_eq!(available(), NAMES.to_vec());
        assert_eq!(all_known(), ALL_KNOWN);
        assert!(available().contains(&"lopdf"));
        for name in available() {
            assert!(ALL_KNOWN.contains(&name), "{name}");
            let backend = by_name(name).expect("available backend resolves");
            assert_eq!(backend.identity().name, name);
        }
        for name in ALL_KNOWN {
            assert_eq!(
                by_name(name).is_some(),
                available().contains(name),
                "{name}"
            );
        }
        assert!(by_name("nope").is_none());
    }

    #[test]
    fn features_are_named_for_native_backends() {
        assert_eq!(feature_for("lopdf"), None);
        assert_eq!(feature_for("pdfium"), Some("pdfium"));
        assert_eq!(feature_for("docling-text"), Some("docling-text"));
        assert_eq!(feature_for("docling"), Some("docling"));
        assert_eq!(feature_for("liteparse-layout"), Some("liteparse-layout"));
        assert_eq!(feature_for("nope"), None);
    }

    #[test]
    fn lopdf_does_not_provide_reading_order_and_opens_the_probe() {
        let backend = by_name("lopdf").expect("lopdf is always compiled in");
        assert!(!backend.provides_reading_order());
        let bytes = probe_pdf().expect("probe builds");
        let mut session = backend.open(&bytes, None).expect("probe opens");
        assert_eq!(session.page_count(), 1);
        let page = session.page_text(1).expect("page 1");
        assert!(page.spans.iter().any(|span| span.text.contains("probe")));
        assert_eq!(session.take_figure_bytes(1, 0), None);
    }
}
