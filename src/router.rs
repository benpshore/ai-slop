//! Backend routing: decide, from what the cheap `lopdf` pass reports about a
//! page, whether a page needs a stronger backend.
//!
//! `lopdf` parses the PDF structure without rendering, so it is the probe:
//! it says whether a page has text at all, whether that text came out of
//! fonts it could map to Unicode, and whether the page is a full-page
//! image. Three routes follow from that:
//!
//! * [`Route::Lopdf`]: mapped text, keep the fast result;
//! * [`Route::Pdfium`]: text is present but some font had no usable
//!   Unicode mapping (`undecodable`, `decoded as Latin-1`, or a share of
//!   U+FFFD in the text). `pdfium` reads the embedded font program's own
//!   tables and usually recovers the characters;
//! * [`Route::Docling`]: no text layer under a page-sized image (a scan), or
//!   `pdfium` still produced unmapped text. Only OCR can name those glyphs.
//!
//! Nothing here touches geometry or reading order.

use serde::Serialize;

use crate::backend::{self, Extractor};
use crate::schema::PageText;

/// Below this many non-whitespace characters a page is "without text".
const MIN_TEXT_CHARS: usize = 20;
/// A raster covering at least this share of the page area makes a text-less
/// page a scan.
const SCAN_COVER: f32 = 0.5;
/// Share of U+FFFD among a page's characters above which its text is unmapped.
const FFFD_SHARE: f32 = 0.01;

/// Where a document (or a page window) should go next.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Route {
    /// The `lopdf` result is usable.
    Lopdf,
    /// Re-read with `pdfium`: fonts without a usable Unicode mapping.
    Pdfium,
    /// Re-read with docling (layout + OCR): scanned pages or still-unmapped text.
    Docling,
}

impl Route {
    /// The backend name for [`crate::backend::by_name`].
    #[must_use]
    pub fn backend_name(self) -> &'static str {
        match self {
            Self::Lopdf => "lopdf",
            Self::Pdfium => "pdfium",
            Self::Docling => "docling",
        }
    }
}

/// What the probe found on a set of pages.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Assessment {
    pub pages: usize,
    /// Pages with (almost) no text under a page-sized raster.
    pub scanned: usize,
    /// Pages whose text came, in part, from fonts without a Unicode mapping.
    pub unmapped: usize,
    /// Pages with neither text nor a page-sized raster (blank, vector art).
    pub empty: usize,
}

impl Assessment {
    /// The route for pages assessed like this after a `lopdf` pass.
    #[must_use]
    pub fn route(self) -> Route {
        if self.scanned > 0 {
            Route::Docling
        } else if self.unmapped > 0 {
            Route::Pdfium
        } else {
            Route::Lopdf
        }
    }

    /// The route after a `pdfium` pass: unmapped text that `pdfium` could not
    /// repair goes to OCR; scans always do.
    #[must_use]
    pub fn route_after_pdfium(self) -> Route {
        if self.scanned > 0 || self.unmapped > 0 {
            Route::Docling
        } else {
            Route::Pdfium
        }
    }
}

/// The compiled-in extractor for `route`, if this build has it.
#[must_use]
pub fn extractor_for(route: Route) -> Option<Box<dyn Extractor>> {
    backend::by_name(route.backend_name())
}

/// Non-whitespace characters and U+FFFD characters of `text`.
fn char_counts(text: &str) -> (usize, usize) {
    let mut chars = 0;
    let mut fffd = 0;
    for ch in text.chars() {
        if ch.is_whitespace() {
            continue;
        }
        chars += 1;
        if ch == '\u{fffd}' {
            fffd += 1;
        }
    }
    (chars, fffd)
}

/// Is `page` a scan: text below [`MIN_TEXT_CHARS`] and a raster covering
/// at least [`SCAN_COVER`] of the page? Uses the spans when the page has
/// not been ordered yet (empty `text`).
#[must_use]
pub fn looks_scanned(page: &PageText) -> bool {
    let text: String = if page.text.is_empty() {
        page.spans.iter().map(|s| s.text.as_str()).collect()
    } else {
        page.text.clone()
    };
    let (chars, _) = char_counts(&text);
    if chars >= MIN_TEXT_CHARS {
        return false;
    }
    let area = page.width * page.height;
    if area <= 0.0 {
        return false;
    }
    page.figures.iter().any(|f| {
        f.kind == "raster"
            && f.bbox.is_some_and(|b| {
                let cover = ((b.x1 - b.x0).max(0.0) * (b.y1 - b.y0).max(0.0)) / area;
                cover >= SCAN_COVER
            })
    })
}

/// Did some of `page`'s text come from a font the backend could not map to
/// Unicode? True on the backend's own warning, or on a share of U+FFFD.
#[must_use]
pub fn has_unmapped_text(page: &PageText) -> bool {
    if page.warnings.iter().any(|w| {
        w.starts_with("unicode_mapping:")
            || w.contains("undecodable")
            || w.contains("decoded as Latin-1")
    }) {
        return true;
    }
    let text: String = if page.text.is_empty() {
        page.spans.iter().map(|s| s.text.as_str()).collect()
    } else {
        page.text.clone()
    };
    let (chars, fffd) = char_counts(&text);
    chars > 0 && (fffd as f32) / (chars as f32) >= FFFD_SHARE
}

/// Assess `pages` (any subset of a document, ordered or not).
#[must_use]
pub fn assess(pages: &[PageText]) -> Assessment {
    let mut a = Assessment {
        pages: pages.len(),
        ..Assessment::default()
    };
    for page in pages {
        if looks_scanned(page) {
            a.scanned += 1;
        } else if has_unmapped_text(page) {
            a.unmapped += 1;
        } else {
            let text: String = if page.text.is_empty() {
                page.spans.iter().map(|s| s.text.as_str()).collect()
            } else {
                page.text.clone()
            };
            if char_counts(&text).0 < MIN_TEXT_CHARS {
                a.empty += 1;
            }
        }
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{BBox, Figure, Span};

    fn page(text: &str) -> PageText {
        let mut p = PageText::new(1, 600.0, 800.0, 0);
        p.spans.push(Span {
            text: text.to_string(),
            bbox: Some(BBox {
                x0: 50.0,
                y0: 700.0,
                x1: 300.0,
                y1: 712.0,
            }),
            font: None,
            size: Some(10.0),
            seq: 0,
        });
        p
    }

    fn raster(cover: f32) -> Figure {
        Figure {
            index: 0,
            bbox: Some(BBox {
                x0: 0.0,
                y0: 0.0,
                x1: 600.0,
                y1: 800.0 * cover,
            }),
            kind: "raster".to_string(),
            mime: None,
            width_px: Some(2000),
            height_px: Some(2600),
            sha256: None,
            file: None,
            caption: None,
        }
    }

    #[test]
    fn mapped_text_stays_on_lopdf() {
        let p = page("This page has ordinary mapped text of some length.");
        assert!(!looks_scanned(&p));
        assert!(!has_unmapped_text(&p));
        assert_eq!(assess(&[p]).route(), Route::Lopdf);
    }

    #[test]
    fn page_sized_raster_without_text_is_a_scan() {
        let mut p = page("");
        p.figures.push(raster(0.9));
        assert!(looks_scanned(&p));
        assert_eq!(assess(std::slice::from_ref(&p)).route(), Route::Docling);
        let mut small = page("");
        small.figures.push(raster(0.2));
        assert!(!looks_scanned(&small));
        assert_eq!(assess(&[small]).empty, 1);
    }

    #[test]
    fn undecodable_font_warning_routes_to_pdfium() {
        let mut p = page("Readable text next to a font that failed.");
        p.warnings
            .push("font F3: undecodable; U+FFFD substituted".to_string());
        assert!(has_unmapped_text(&p));
        let a = assess(std::slice::from_ref(&p));
        assert_eq!(a.route(), Route::Pdfium);
        assert_eq!(a.route_after_pdfium(), Route::Docling);
    }

    #[test]
    fn replacement_share_routes_to_pdfium() {
        let p = page("abc\u{fffd}\u{fffd}\u{fffd}defghijklmnopqrstuvwxyz");
        assert!(has_unmapped_text(&p));
        let clean = page(&format!(
            "one stray \u{fffd} then fine text {}",
            "and on ".repeat(60)
        ));
        assert!(!has_unmapped_text(&clean));
    }

    #[test]
    fn route_names_match_backends() {
        assert_eq!(Route::Lopdf.backend_name(), "lopdf");
        assert_eq!(Route::Pdfium.backend_name(), "pdfium");
        assert_eq!(Route::Docling.backend_name(), "docling");
    }
}
