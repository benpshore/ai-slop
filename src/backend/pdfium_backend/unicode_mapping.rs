//! Mapping evidence missing from pdfium-render 0.8.37's public text wrapper.
//!
//! Use the existing library instance and immutable input bytes. The wrapper
//! hides its document/text handles, so this is a separate native document,
//! with one text page alive at a time. Never replace retained text with guessed
//! characters. Diagnostics are page-local, bounded, and survive serialization.

use pdfium_render::prelude::{FPDF_DOCUMENT, FPDF_PAGE, FPDF_TEXTPAGE, PdfiumLibraryBindings};

pub(super) struct MappingDocument<'a> {
    bindings: &'a dyn PdfiumLibraryBindings,
    handle: FPDF_DOCUMENT,
    _bytes: &'a [u8],
}

impl<'a> MappingDocument<'a> {
    pub(super) fn new(
        bindings: &'a dyn PdfiumLibraryBindings,
        bytes: &'a [u8],
        password: Option<&str>,
    ) -> Self {
        Self {
            bindings,
            handle: bindings.FPDF_LoadMemDocument64(bytes, password),
            _bytes: bytes,
        }
    }

    pub(super) fn warning(&self, index: u16) -> Option<String> {
        self.inspect(index).unwrap_or_else(|reason| {
            Some(format!(
                "unicode_mapping: pdfium evidence unavailable ({reason})"
            ))
        })
    }

    fn inspect(&self, index: u16) -> Result<Option<String>, &'static str> {
        if self.handle.is_null() {
            return Err("document open failed");
        }
        let bindings = self.bindings;
        let mut page = MappingPage {
            document: self,
            page: bindings.FPDF_LoadPage(self.handle, i32::from(index)),
            text: std::ptr::null_mut(),
        };
        if page.page.is_null() {
            return Err("page load failed");
        }
        page.text = bindings.FPDFText_LoadPage(page.page);
        if page.text.is_null() {
            return Err("text page load failed");
        }
        let count = bindings.FPDFText_CountChars(page.text);
        if count < 0 {
            return Err("character count failed");
        }
        let mut evidence = MappingEvidence::default();
        for index in 0..count {
            evidence.record(
                index,
                bindings.FPDFText_IsGenerated(page.text, index),
                bindings.FPDFText_HasUnicodeMapError(page.text, index),
                bindings.FPDFText_GetUnicode(page.text, index),
            );
        }
        Ok(evidence.warning())
    }
}

impl Drop for MappingDocument<'_> {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            self.bindings.FPDF_CloseDocument(self.handle);
        }
    }
}

// The borrowed owners keep bytes/library/document alive. Close text before
// page, including early-return and unwinding paths; never close a null handle.
struct MappingPage<'a> {
    document: &'a MappingDocument<'a>,
    page: FPDF_PAGE,
    text: FPDF_TEXTPAGE,
}

impl Drop for MappingPage<'_> {
    fn drop(&mut self) {
        if !self.text.is_null() {
            self.document.bindings.FPDFText_ClosePage(self.text);
        }
        if !self.page.is_null() {
            self.document.bindings.FPDF_ClosePage(self.page);
        }
    }
}

#[derive(Default)]
struct MappingEvidence {
    map_errors: u32,
    zero_unicode: u32,
    unavailable_flags: u32,
    // First eight native (zero-based) character indices, not output-string offsets.
    examples: Vec<i32>,
}

impl MappingEvidence {
    fn record(&mut self, index: i32, generated: i32, map_error: i32, unicode: u32) {
        if generated == 1 {
            // PDFium inserts spaces/newlines; these have no source glyph to map.
            return;
        }
        let unavailable = generated != 0 || !matches!(map_error, 0 | 1);
        self.map_errors += u32::from(map_error == 1);
        self.zero_unicode += u32::from(unicode == 0);
        self.unavailable_flags += u32::from(unavailable);
        if (map_error == 1 || unicode == 0 || unavailable) && self.examples.len() < 8 {
            self.examples.push(index);
        }
    }

    fn warning(&self) -> Option<String> {
        if self.examples.is_empty() {
            return None;
        }
        Some(format!(
            "unicode_mapping: pdfium map_errors={}, zero_unicode={}, unavailable_flags={}; \
             first affected character indices={:?} (zero-based, generated separators excluded)",
            self.map_errors, self.zero_unicode, self.unavailable_flags, self.examples
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_failure_closes_handles_and_preserves_the_next_query() {
        if std::env::var_os("PDFIUM_DYNAMIC_LIB_PATH").is_none() {
            eprintln!("skipped: set PDFIUM_DYNAMIC_LIB_PATH for native handle regression");
            return;
        }
        let pdfium = super::super::bind(None).unwrap();
        let bytes = crate::backend::probe_pdf().unwrap();
        let mapping = MappingDocument::new(pdfium.bindings(), &bytes, None);
        assert!(
            mapping
                .warning(u16::MAX)
                .unwrap()
                .contains("page load failed")
        );
        assert!(mapping.warning(0).is_none());
        {
            let invalid = MappingDocument::new(pdfium.bindings(), b"not a PDF", None);
            assert!(invalid.warning(0).unwrap().contains("document open failed"));
        }
        assert!(mapping.warning(0).is_none());
    }

    #[test]
    fn generated_separators_are_not_mapping_failures() {
        let mut evidence = MappingEvidence::default();
        for unicode in [0, 10, 13, 32] {
            evidence.record(0, 1, 1, unicode);
        }
        evidence.record(4, 0, 0, u32::from('A'));
        assert!(evidence.warning().is_none());
    }

    #[test]
    fn zero_unicode_and_failed_queries_cannot_look_successful() {
        let mut evidence = MappingEvidence::default();
        evidence.record(1, 0, 0, 0);
        evidence.record(2, -1, 0, u32::from('A'));
        evidence.record(3, 0, -1, u32::from('B'));
        evidence.record(4, 0, 1, u32::from('C'));
        assert_eq!(evidence.zero_unicode, 1);
        assert_eq!(evidence.map_errors, 1);
        assert_eq!(evidence.unavailable_flags, 2);
        assert_eq!(evidence.examples, [1, 2, 3, 4]);
        assert!(evidence.warning().unwrap().starts_with("unicode_mapping:"));
    }

    #[test]
    fn warning_size_is_bounded_but_counts_are_complete() {
        let mut evidence = MappingEvidence::default();
        for index in 0..100 {
            evidence.record(index, 0, 1, 0);
        }
        assert_eq!(evidence.examples.len(), 8);
        assert_eq!(evidence.map_errors, 100);
        assert_eq!(evidence.zero_unicode, 100);
    }
}
