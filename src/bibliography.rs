//! Bibliography-only extraction from the last page toward the first.
//!
//! This path deliberately leaves full-document metadata, in-text markers,
//! figures, and ledger publication to the existing extraction pipeline.

use serde::Serialize;

use crate::backend::{BackendError, Extractor};
use std::sync::OnceLock;

use regex::Regex;

use crate::citations::{self, ReferenceSection};
use crate::pipeline::Progress;
use crate::reading_order;
use crate::regions;
use crate::router::{self, Assessment, Route};
use crate::schema::BackendIdentity;
use crate::schema::{Link, PageText, ReferenceEntry, Status};
use crate::text_cleanup;

/// A single end-list search. `found` means the list boundary passed the
/// segmentation guard; it does not certify exact transcription of its entries.
#[derive(Debug, Serialize)]
pub struct BibliographyScan {
    pub total_pages: u32,
    pub pages_scanned: u32,
    pub found: bool,
    pub section_page: Option<u32>,
    pub heading: Option<String>,
    pub references: Vec<ReferenceEntry>,
    pub warnings: Vec<String>,
    /// What the probe found on the pages that were read (`crate::router`).
    pub assessment: Assessment,
    /// Whether the found list accounts for the labels and years printed on
    /// its pages; `true` when no list was found.
    pub plausible: bool,
}

/// The JSON record `tpe bibliography` prints per input PDF (docs/BIBLIOGRAPHY.md),
/// so any caller (the app) publishes the same shape as the CLI. `status` is
/// `found`, `not_found` or `failed`; a failed record keeps the hash and
/// backend when they are known and carries the error in `error` and
/// `warnings`.
#[derive(Debug, Serialize)]
pub struct Record {
    pub path: String,
    pub sha256: Option<String>,
    pub backend: BackendIdentity,
    pub status: &'static str,
    /// Completeness of the pages inspected, independently of list detection.
    pub extraction_status: Status,
    pub total_pages: Option<u32>,
    pub pages_scanned: Option<u32>,
    pub section_page: Option<u32>,
    pub heading: Option<String>,
    pub references: Vec<ReferenceEntry>,
    pub warnings: Vec<String>,
    /// What the probe found on the pages read (`crate::router`).
    pub assessment: Assessment,
    /// Whether the list accounts for the labels and years on its pages.
    pub plausible: bool,
    /// The paper's own record, when `--resolve` ran and it verified.
    pub paper: Option<crate::schema::Resolved>,
    /// How the entries resolved, when `--resolve` ran.
    pub resolution: Option<crate::resolve::Outcome>,
    pub elapsed_ms: f64,
    pub error: Option<String>,
}

impl Record {
    /// A record for a completed scan (`found` or `not_found`).
    pub fn from_scan(
        path: &str,
        sha256: String,
        backend: BackendIdentity,
        scan: BibliographyScan,
        elapsed_ms: f64,
    ) -> Self {
        Self {
            path: path.to_string(),
            sha256: Some(sha256),
            backend,
            status: if scan.found { "found" } else { "not_found" },
            extraction_status: scan_extraction_status(&scan),
            total_pages: Some(scan.total_pages),
            pages_scanned: Some(scan.pages_scanned),
            section_page: scan.section_page,
            heading: scan.heading,
            references: scan.references,
            warnings: scan.warnings,
            assessment: scan.assessment,
            plausible: scan.plausible,
            paper: None,
            resolution: None,
            elapsed_ms,
            error: None,
        }
    }

    /// A `failed` record; `sha256` is whatever was acquired before the failure.
    pub fn failed(
        path: &str,
        sha256: Option<String>,
        backend: BackendIdentity,
        error: String,
        elapsed_ms: f64,
    ) -> Self {
        Self {
            path: path.to_string(),
            sha256,
            backend,
            status: "failed",
            extraction_status: Status::Failed,
            total_pages: None,
            pages_scanned: None,
            section_page: None,
            heading: None,
            references: Vec::new(),
            warnings: vec![error.clone()],
            assessment: Assessment::default(),
            plausible: true,
            paper: None,
            resolution: None,
            elapsed_ms,
            error: Some(error),
        }
    }

    /// Whether the scan found a list (`not_found` and `failed` are both false).
    pub fn found(&self) -> bool {
        self.status == "found"
    }

    /// One entry per line: the printed label (or `[index]`) and the raw text,
    /// unchanged. Empty for an empty list.
    pub fn plain_text(&self) -> String {
        let mut text = String::new();
        for entry in &self.references {
            if let Some(label) = &entry.label {
                text.push_str(label);
            } else {
                text.push('[');
                text.push_str(&entry.index.to_string());
                text.push(']');
            }
            text.push(' ');
            text.push_str(&entry.raw);
            text.push('\n');
        }
        text
    }
}

/// Inspect the PDF's end, then prepend one page at a time until the start of
/// the last qualified bibliography is present. All selected pages are ordered
/// forward before segmentation; page failures abort rather than silently
/// returning an incomplete list. A headingless numbered list is supported by
/// the existing section detector. At least three segmented entries are needed
/// to accept a boundary; shorter lists are explicitly reported as not found.
pub fn scan_backward(
    extractor: &dyn Extractor,
    bytes: &[u8],
    password: Option<&str>,
) -> Result<BibliographyScan, BackendError> {
    scan_backward_observed(extractor, bytes, password, &mut |_| {})
}

/// [`scan_backward`] reporting a [`Progress`] event when the document opens
/// and after each page read from the end. `total` is the page count: how
/// many pages the scan will need is unknown until the boundary is found, so
/// `done` counts pages scanned so far and usually stops well short of it.
pub fn scan_backward_observed(
    extractor: &dyn Extractor,
    bytes: &[u8],
    password: Option<&str>,
    observe: &mut dyn FnMut(Progress),
) -> Result<BibliographyScan, BackendError> {
    scan_window(extractor, bytes, password, 1, observe)
}

/// [`scan_backward_observed`] that never reads below page `floor`: the
/// window a fallback backend was asked to convert.
fn scan_window(
    extractor: &dyn Extractor,
    bytes: &[u8],
    password: Option<&str>,
    floor: u32,
    observe: &mut dyn FnMut(Progress),
) -> Result<BibliographyScan, BackendError> {
    let mut session = extractor.open(bytes, password)?;
    let total_pages = session.page_count();
    if total_pages == 0 {
        return Err(BackendError::PageRange { page: 1, count: 0 });
    }
    let mut pages: Vec<PageText> = Vec::new();
    let mut warnings = Vec::new();
    observe(Progress::Opened {
        pages: total_pages,
        total: total_pages,
    });

    for number in (floor.max(1)..=total_pages).rev() {
        let mut page = session.page_text(number)?;
        // Assess the backend evidence before cleanup can remove unreadable text.
        router::mark_incomplete(&mut page);
        if extractor.provides_line_layout() {
            // Preserve explicitly provided layout; spans remain raw evidence.
        } else if extractor.provides_reading_order() {
            reading_order::lines_in_backend_order(&mut page);
        } else {
            reading_order::order_page(&mut page);
        }
        pages.insert(0, page);
        observe(Progress::Page {
            page: number,
            done: u32::try_from(pages.len()).unwrap_or(u32::MAX),
            total: total_pages,
        });

        // Cleanup uses the selected document context. Keep the ordered source
        // pages untouched so an earlier page can change that context safely.
        let mut checked = pages.clone();
        text_cleanup::clean_document(&mut checked);
        regions::tag_regions(&mut checked);
        warnings = checked
            .iter()
            .flat_map(|page| page.warnings.iter().cloned())
            .collect();
        for section in citations::find_reference_sections(&checked)
            .into_iter()
            .rev()
        {
            if section.first_page != number {
                continue;
            }
            let mut references = citations::segment_entries(&checked, &section);
            if references.len() < 3 {
                continue;
            }
            for (i, entry) in references.iter_mut().enumerate() {
                entry.index = u32::try_from(i + 1).unwrap_or(u32::MAX);
                citations::parse_entry(entry);
            }
            // Native links can be incomplete on individual pages. Merge the
            // selected backend's evidence with PDF annotations page by page;
            // lopdf already supplies that exact native annotation pass.
            if extractor.identity().name == "lopdf" {
                crate::resolve::attach_links(&mut references, &checked);
            } else {
                let link_pages = link_pages_via_lopdf(bytes, password, &checked, &mut warnings);
                crate::resolve::attach_links(&mut references, &link_pages);
            }
            for entry in &references {
                if entry.raw.contains('\u{fffd}') {
                    warnings.push(format!(
                        "reference {} on page {} contains a replacement character",
                        entry.index, entry.page
                    ));
                }
            }
            let assessment = router::assess(&pages);
            let plausible = list_plausible(&checked, &section, references.len());
            return Ok(BibliographyScan {
                total_pages,
                pages_scanned: u32::try_from(pages.len()).unwrap_or(u32::MAX),
                found: true,
                section_page: Some(section.first_page),
                heading: (!section.heading.is_empty()).then_some(section.heading),
                references,
                warnings,
                assessment,
                plausible,
            });
        }
    }

    let assessment = router::assess(&pages);
    warnings.push("no bibliography boundary with at least three entries found".to_string());
    Ok(BibliographyScan {
        total_pages,
        pages_scanned: u32::try_from(pages.len()).unwrap_or(u32::MAX),
        found: false,
        section_page: None,
        heading: None,
        references: Vec::new(),
        warnings,
        assessment,
        plausible: true,
    })
}

/// Preserve backend links and union native annotation evidence for each page.
/// Every box uses the schema's unrotated PDF user-space contract. A differing
/// page frame cannot establish correspondence and is reported, never guessed.
fn link_pages_via_lopdf(
    bytes: &[u8],
    password: Option<&str>,
    pages: &[PageText],
    warnings: &mut Vec<String>,
) -> Vec<PageText> {
    let mut merged: Vec<PageText> = pages
        .iter()
        .map(|page| {
            let mut links = PageText::new(page.page, page.width, page.height, page.rotation);
            links.links = unique_links(page.links.iter().cloned());
            links
        })
        .collect();
    let Some(lopdf) = router::extractor_for(Route::Lopdf) else {
        return merged;
    };
    let mut session = match lopdf.open(bytes, password) {
        Ok(session) => session,
        Err(error) => {
            warnings.push(format!(
                "link annotations: lopdf fallback unavailable: {error}"
            ));
            return merged;
        }
    };
    for page in &mut merged {
        let read = match session.page_text(page.page) {
            Ok(read) => read,
            Err(error) => {
                warnings.push(format!(
                    "link annotations: page {}: lopdf fallback failed: {error}",
                    page.page
                ));
                continue;
            }
        };
        if read.page != page.page
            || !page.width.is_finite()
            || !page.height.is_finite()
            || page.width <= 0.0
            || page.height <= 0.0
            || read.width.to_bits() != page.width.to_bits()
            || read.height.to_bits() != page.height.to_bits()
            || read.rotation.rem_euclid(360) != page.rotation.rem_euclid(360)
        {
            warnings.push(format!(
                "link annotations: page {}: lopdf fallback skipped because page geometry differs from the selected backend",
                page.page
            ));
            continue;
        }
        // Link equality includes the complete URI and optional rectangle:
        // identical targets at different placements remain independent evidence.
        page.links = unique_links(
            std::mem::take(&mut page.links)
                .into_iter()
                .chain(read.links),
        );
    }
    merged
}

/// Stable deduplication by target and rectangle, without a quadratic scan of
/// potentially large annotation lists. Signed zero denotes the same coordinate.
fn unique_links(links: impl Iterator<Item = Link>) -> Vec<Link> {
    let mut seen = std::collections::HashSet::new();
    links
        .filter(|link| {
            let rectangle = link.bbox.map(|bbox| {
                [bbox.x0, bbox.y0, bbox.x1, bbox.y1].map(|value| {
                    let bits = value.to_bits();
                    if bits.trailing_zeros() >= 31 { 0 } else { bits }
                })
            });
            seen.insert((link.uri.clone(), rectangle))
        })
        .collect()
}

/// Fewest trailing pages a fallback backend converts when the `lopdf` scan
/// found no list.
const FALLBACK_WINDOW: u32 = 12;

/// A list is implausible when the tail pages carry evidence of more entries
/// than were segmented: a printed numeric label higher than the entry count
/// (labels that `lopdf` left detached from their entries), or many more
/// four-digit years than entries (every entry prints at least one year; a
/// merged or cut-off author-year list has far fewer entries than years).
fn list_plausible(pages: &[PageText], section: &ReferenceSection, entries: usize) -> bool {
    static LABEL: OnceLock<Regex> = OnceLock::new();
    static YEAR: OnceLock<Regex> = OnceLock::new();
    let label = LABEL.get_or_init(|| Regex::new(r"^\s*\[?(\d{1,3})[.)\]]").expect("valid regex"));
    let year = YEAR.get_or_init(|| Regex::new(r"\b(?:19|20)\d\d\b").expect("valid regex"));
    let mut max_label = 0usize;
    let mut years = 0usize;
    for page in pages.iter().filter(|p| p.page >= section.first_page) {
        for (i, line) in page.lines.iter().enumerate() {
            if page.page == section.first_page && i < section.first_line {
                continue;
            }
            if line.role == "furniture" {
                continue;
            }
            if let Some(found) = label.captures(&line.text)
                && let Ok(n) = found[1].parse::<usize>()
            {
                max_label = max_label.max(n);
            }
            years += year.find_iter(&line.text).count();
        }
    }
    max_label <= entries + 1 && years <= entries + entries / 2 + 3
}

/// A backward scan with the backend that finally produced it.
#[derive(Debug)]
pub struct RoutedScan {
    pub scan: BibliographyScan,
    pub backend: BackendIdentity,
}

/// Completeness is independent of whether a bibliography boundary was found.
/// A scanned or unmapped tail remains partial even when no list was recovered.
fn scan_extraction_status(scan: &BibliographyScan) -> Status {
    if scan.assessment.scanned > 0
        || scan.assessment.unmapped > 0
        || scan.warnings.iter().any(|warning| {
            warning.starts_with("failed:")
                || warning.starts_with("resource_limit:")
                || warning.starts_with("unicode_mapping:")
                || warning.starts_with("extraction_incomplete:")
        })
    {
        Status::Partial
    } else {
        Status::Complete
    }
}

/// Rank found lists by the evidence that prompted routing: usable decoding,
/// then list plausibility, then entry count. Count alone cannot distinguish
/// repaired text from an equally long corrupted list or spurious extra entries.
fn scan_quality(scan: &BibliographyScan) -> (bool, bool, usize) {
    (
        scan_extraction_status(scan) == Status::Complete,
        scan.plausible,
        scan.references.len(),
    )
}

/// Whether the selected result needs no further fallback. This must be checked
/// on the retained scan, not on a candidate that may have been rejected.
fn scan_usable(scan: &BibliographyScan) -> bool {
    scan.found && scan_quality(scan).0 && scan.plausible
}

/// Replace `best` only when a found candidate improves the available evidence.
/// Keep the existing result on ties and preserve route history on replacement.
/// Mapping evidence from a rejected `PDFium` candidate is retained separately:
/// uncertainty must survive, but cannot justify discarding a better native list.
fn keep_better(best: &mut RoutedScan, mut candidate: RoutedScan, note: String) {
    let unresolved_pdfium = candidate.backend.name == "pdfium"
        && candidate
            .scan
            .warnings
            .iter()
            .any(|w| w.starts_with("unicode_mapping:"));
    let better = candidate.scan.found
        && (!best.scan.found || scan_quality(&candidate.scan) > scan_quality(&best.scan));
    if better || !best.scan.found {
        // Page/content warnings describe the selected extraction; route history
        // describes all attempted backends and survives a successful replacement.
        let history = best
            .scan
            .warnings
            .iter()
            .filter(|w| w.starts_with("routed:") || w.starts_with("route not taken:"))
            .cloned();
        candidate.scan.warnings.extend(history);
        *best = candidate;
    } else if unresolved_pdfium {
        for warning in &candidate.scan.warnings {
            if let Some(detail) = warning.strip_prefix("unicode_mapping:") {
                best.scan.warnings.push(format!(
                    "unicode_mapping: rejected pdfium candidate:{detail}"
                ));
            }
        }
    }
    best.scan.warnings.push(note);
}

/// [`scan_backward_observed`] with routing (`crate::router`): `lopdf` reads
/// the tail; when it finds no list, or the pages it read had unmapped fonts
/// or scans, `pdfium` re-reads the same tail, and docling (layout and OCR)
/// converts the last `max(pages read, FALLBACK_WINDOW)` pages if `pdfium`
/// did not settle it. A list that `lopdf` found but that looks cut short
/// ([`list_plausible`]) goes to docling for the pages from its heading on,
/// and decoding quality and plausibility take priority over entry count.
/// A route whose backend is missing or fails
/// is noted in the warnings and the best scan so far is returned.
/// Unresolved `PDFium` mappings retain the native scan as Partial; automatic
/// Docling recovery has not been verified and must not erase that evidence.
pub fn scan_backward_auto_observed(
    bytes: &[u8],
    password: Option<&str>,
    observe: &mut dyn FnMut(Progress),
) -> Result<RoutedScan, BackendError> {
    let lopdf = router::extractor_for(Route::Lopdf)
        .ok_or_else(|| BackendError::Unsupported("lopdf backend missing".to_string()))?;
    let mut best = RoutedScan {
        scan: scan_window(lopdf.as_ref(), bytes, password, 1, observe)?,
        backend: lopdf.identity(),
    };
    let first = best.scan.assessment;
    let mut route = first.route();
    if scan_usable(&best.scan) {
        return Ok(best);
    }
    let total = best.scan.total_pages;
    let mut floor = total.saturating_sub(best.scan.pages_scanned.max(FALLBACK_WINDOW)) + 1;
    if best.scan.found && !best.scan.plausible {
        // The heading was found; only its pages need the second opinion.
        floor = best.scan.section_page.unwrap_or(floor).max(1);
        route = Route::Docling;
    } else if route == Route::Lopdf {
        route = Route::Pdfium;
    }
    if route == Route::Pdfium {
        if let Some(pdfium) = router::extractor_for(Route::Pdfium) {
            match scan_window(pdfium.as_ref(), bytes, password, floor, observe) {
                Ok(scan) => {
                    let note = format!(
                        "routed: pdfium ({} of {} pages unmapped; lopdf {})",
                        first.unmapped,
                        first.pages,
                        if best.scan.found {
                            "found a list"
                        } else {
                            "found none"
                        }
                    );
                    keep_better(
                        &mut best,
                        RoutedScan {
                            scan,
                            backend: pdfium.identity(),
                        },
                        note,
                    );
                    if best
                        .scan
                        .warnings
                        .iter()
                        .any(|w| w.starts_with("unicode_mapping:"))
                    {
                        best.scan.warnings.push(
                            "unresolved: Unicode mapping evidence remains; best native bibliography retained because automatic OCR recovery is unverified"
                                .to_string(),
                        );
                        return Ok(best);
                    }
                    if scan_usable(&best.scan) {
                        return Ok(best);
                    }
                }
                Err(err) => best
                    .scan
                    .warnings
                    .push(format!("route not taken: pdfium failed: {err}")),
            }
        } else {
            best.scan
                .warnings
                .push("route not taken: pdfium is not compiled into this build".to_string());
        }
        route = Route::Docling;
    }
    if route == Route::Docling {
        if let Some(docling) = docling_for_window(floor, total) {
            match scan_window(docling.as_ref(), bytes, password, floor, observe) {
                Ok(scan) => {
                    let note = format!(
                        "routed: docling (pages {floor}-{total}; {} scanned, {} unmapped of {} pages; lopdf {})",
                        first.scanned,
                        first.unmapped,
                        first.pages,
                        if best.scan.found {
                            "found a list"
                        } else {
                            "found none"
                        }
                    );
                    keep_better(
                        &mut best,
                        RoutedScan {
                            scan,
                            backend: docling.identity(),
                        },
                        note,
                    );
                }
                Err(err) => best
                    .scan
                    .warnings
                    .push(format!("route not taken: docling failed: {err}")),
            }
        } else {
            best.scan
                .warnings
                .push("route not taken: docling is not compiled into this build".to_string());
        }
    }
    Ok(best)
}

/// The full docling backend limited to pages `first..=last`, without
/// retaining picture bytes; `None` when docling is not compiled in.
#[cfg(feature = "docling")]
#[allow(clippy::unnecessary_wraps)] // `None` is the answer of the other cfg
fn docling_for_window(first: u32, last: u32) -> Option<Box<dyn Extractor>> {
    use crate::backend::docling_backend::DoclingBackend;
    Some(Box::new(DoclingBackend::full().with_window(first, last)))
}

/// Docling is not compiled in.
#[cfg(not(feature = "docling"))]
fn docling_for_window(_first: u32, _last: u32) -> Option<Box<dyn Extractor>> {
    None
}

#[cfg(test)]
mod tests {
    use lopdf::content::{Content, Operation};
    use lopdf::{Document, Object, Stream, dictionary};

    use super::{Record, scan_backward, scan_backward_observed};
    use crate::backend::Extractor;
    use crate::backend::lopdf_backend::LopdfBackend;
    use crate::pipeline::Progress;

    fn routed(name: &str, count: usize, unmapped: usize, plausible: bool) -> super::RoutedScan {
        super::RoutedScan {
            backend: crate::schema::BackendIdentity {
                name: name.to_string(),
                version: "test".to_string(),
                config_digest: String::new(),
            },
            scan: super::BibliographyScan {
                total_pages: 1,
                pages_scanned: 1,
                found: count > 0,
                section_page: (count > 0).then_some(1),
                heading: None,
                references: vec![crate::schema::ReferenceEntry::default(); count],
                warnings: Vec::new(),
                assessment: crate::router::Assessment {
                    pages: 1,
                    unmapped,
                    ..crate::router::Assessment::default()
                },
                plausible,
            },
        }
    }

    #[test]
    fn limited_candidate_cannot_replace_or_certify_an_intact_list() {
        let mut best = routed("lopdf", 3, 0, true);
        let mut limited = routed("pdfium", 8, 0, true);
        limited
            .scan
            .warnings
            .push("resource_limit: retained partial page".to_string());
        assert!(!super::scan_usable(&limited.scan));
        super::keep_better(&mut best, limited, "routed: pdfium".to_string());
        assert_eq!(best.backend.name, "lopdf");
        assert_eq!(best.scan.references.len(), 3);
    }

    #[test]
    fn fallback_keeps_repaired_text_even_with_equal_entry_count() {
        let mut best = routed("lopdf", 3, 1, true);
        let mut repaired = routed("pdfium", 3, 0, true);
        repaired.scan.references[0].raw = "Repaired title".to_string();
        super::keep_better(&mut best, repaired, "routed: pdfium".to_string());
        assert_eq!(best.backend.name, "pdfium");
        assert_eq!(best.scan.references[0].raw, "Repaired title");
        assert!(super::scan_usable(&best.scan));
    }

    #[test]
    fn fallback_prefers_plausible_lists_over_spurious_extra_entries() {
        let mut best = routed("lopdf", 5, 0, false);
        super::keep_better(
            &mut best,
            routed("docling", 3, 0, true),
            "routed: docling".to_string(),
        );
        assert_eq!(best.backend.name, "docling");
        super::keep_better(
            &mut best,
            routed("pdfium", 6, 1, true),
            "routed: pdfium".to_string(),
        );
        assert_eq!(best.backend.name, "docling");
        assert_eq!(best.scan.references.len(), 3);
    }

    #[test]
    fn rejected_empty_fallback_does_not_certify_the_retained_scan() {
        let mut best = routed("lopdf", 3, 1, true);
        // A backend returning no list has no decoding warnings and is plausible
        // by convention, but cannot certify the corrupted list we retained.
        super::keep_better(
            &mut best,
            routed("pdfium", 0, 0, true),
            "routed: pdfium".to_string(),
        );
        assert_eq!(best.backend.name, "lopdf");
        assert!(!super::scan_usable(&best.scan));
    }

    #[test]
    fn unresolved_pdfium_mapping_survives_without_erasing_the_better_native_list() {
        for count in [0, 2, 3, 4] {
            let mut best = routed("lopdf", 3, 1, true);
            best.scan.references[0].raw = "Retained native citation".into();
            best.scan.warnings = vec!["routed: earlier attempt".into(), "old diagnostic".into()];
            let mut partial = routed("pdfium", count, 1, true);
            let warning = "unicode_mapping: pdfium map_errors=1, zero_unicode=0";
            partial.scan.warnings.push(warning.into());
            super::keep_better(&mut best, partial, "routed: pdfium".into());
            if count <= 3 {
                assert_eq!(best.backend.name, "lopdf");
                assert_eq!(best.scan.references.len(), 3);
                assert_eq!(best.scan.references[0].raw, "Retained native citation");
                assert!(best.scan.warnings.contains(&"old diagnostic".into()));
                assert!(
                    best.scan
                        .warnings
                        .iter()
                        .any(|w| w.starts_with("unicode_mapping: rejected pdfium candidate:"))
                );
            } else {
                assert_eq!(best.backend.name, "pdfium");
                assert_eq!(best.scan.references.len(), count);
                assert!(!best.scan.warnings.contains(&"old diagnostic".into()));
                assert!(best.scan.warnings.contains(&warning.into()));
            }
            assert!(
                best.scan
                    .warnings
                    .contains(&"routed: earlier attempt".into())
            );
            assert!(best.scan.warnings.contains(&"routed: pdfium".into()));
            assert!(!super::scan_usable(&best.scan));
            let record = Record::from_scan("p.pdf", "hash".into(), best.backend, best.scan, 0.0);
            let value = serde_json::to_value(&record).unwrap();
            assert_eq!(value["extraction_status"], "partial");
            assert_eq!(value["status"], "found");
        }
    }

    #[test]
    fn fallback_retains_ties_but_accepts_longer_equally_usable_lists() {
        let mut best = routed("lopdf", 3, 0, true);
        super::keep_better(
            &mut best,
            routed("pdfium", 3, 0, true),
            "routed: pdfium".to_string(),
        );
        assert_eq!(best.backend.name, "lopdf");
        super::keep_better(
            &mut best,
            routed("docling", 4, 0, true),
            "routed: docling".to_string(),
        );
        assert_eq!(best.backend.name, "docling");
        assert_eq!(best.scan.references.len(), 4);
        assert_eq!(best.scan.warnings, ["routed: pdfium", "routed: docling"]);
    }

    fn pdf(pages: &[&[&str]]) -> Vec<u8> {
        let mut document = Document::with_version("1.5");
        let tree_id = document.new_object_id();
        let font_id = document.add_object(dictionary! {
            "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
        });
        let resources_id = document.add_object(dictionary! {
            "Font" => dictionary! { "F1" => font_id },
        });
        let mut kids = Vec::new();
        for lines in pages {
            let mut operations = Vec::new();
            for (row, line) in lines.iter().enumerate() {
                operations.extend([
                    Operation::new("BT", vec![]),
                    Operation::new("Tf", vec!["F1".into(), 12_i32.into()]),
                    Operation::new(
                        "Td",
                        vec![
                            72_i32.into(),
                            (720 - 20 * i32::try_from(row).unwrap()).into(),
                        ],
                    ),
                    Operation::new("Tj", vec![Object::string_literal(*line)]),
                    Operation::new("ET", vec![]),
                ]);
            }
            let contents = Content { operations }.encode().unwrap();
            let content_id = document.add_object(Stream::new(dictionary! {}, contents));
            let page_id = document.add_object(dictionary! {
                "Type" => "Page", "Parent" => tree_id,
                "Contents" => content_id, "Resources" => resources_id,
            });
            kids.push(Object::Reference(page_id));
        }
        document.objects.insert(
            tree_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages", "Kids" => kids,
                "Count" => Object::Integer(i64::try_from(pages.len()).unwrap()),
                "MediaBox" => vec![0_i32.into(), 0_i32.into(), 612_i32.into(), 792_i32.into()],
            }),
        );
        let catalog_id = document.add_object(dictionary! {
            "Type" => "Catalog", "Pages" => tree_id,
        });
        document.trailer.set("Root", catalog_id);
        let mut bytes = Vec::new();
        document.save_to(&mut bytes).unwrap();
        bytes
    }

    struct PartialLinkBackend;

    struct PartialLinkSession {
        inner: Box<dyn crate::backend::DocumentSession>,
    }

    impl crate::backend::DocumentSession for PartialLinkSession {
        fn page_count(&self) -> u32 {
            self.inner.page_count()
        }

        fn page_text(
            &mut self,
            page: u32,
        ) -> Result<crate::schema::PageText, crate::backend::BackendError> {
            let mut read = self.inner.page_text(page)?;
            if page == 1 {
                // A duplicate and a backend-only unpositioned link are real
                // evidence, but do not establish coverage of any other page.
                read.links.push(read.links[0].clone());
                read.links.push(crate::schema::Link {
                    bbox: None,
                    uri: "https://example.test/backend-evidence".into(),
                });
            } else {
                read.links.clear();
            }
            Ok(read)
        }

        fn info(&self) -> std::collections::BTreeMap<String, String> {
            self.inner.info()
        }
    }

    impl Extractor for PartialLinkBackend {
        fn identity(&self) -> crate::schema::BackendIdentity {
            let mut identity = LopdfBackend::default().identity();
            identity.name = "partial-link-fixture".into();
            identity
        }

        fn open(
            &self,
            bytes: &[u8],
            password: Option<&str>,
        ) -> Result<Box<dyn crate::backend::DocumentSession>, crate::backend::BackendError>
        {
            Ok(Box::new(PartialLinkSession {
                inner: LopdfBackend::default().open(bytes, password)?,
            }))
        }
    }

    fn annotated_bibliography(rotated_crop: bool) -> Vec<u8> {
        let bytes = pdf(&[
            &["References", "[1] A. One, First cited work, 2020."],
            &[
                "[2] B. Two, Second cited work, 2021.",
                "[3] C. Three, Third cited work, 2022.",
            ],
        ]);
        let mut doc = Document::load_mem(&bytes).unwrap();
        for (number, id) in doc.get_pages() {
            let rows = if number == 1 {
                vec![(700, "first")]
            } else {
                vec![(720, "shared"), (700, "shared")]
            };
            let mut annotations = Vec::new();
            for (baseline, target) in rows {
                let uri = format!("https://doi.org/10.1000/{target}");
                let annotation = doc.add_object(dictionary! {
                    "Type" => "Annot", "Subtype" => "Link",
                    "Rect" => vec![92.into(), (baseline - 3).into(), 180.into(), (baseline + 3).into()],
                    "A" => dictionary! { "S" => "URI", "URI" => Object::string_literal(uri) },
                });
                annotations.push(Object::Reference(annotation));
            }
            let page = doc.get_object_mut(id).unwrap().as_dict_mut().unwrap();
            page.set("Annots", annotations);
            if rotated_crop {
                page.set("Rotate", 90);
                page.set(
                    "MediaBox",
                    vec![(-50).into(), 30.into(), 562.into(), 822.into()],
                );
                page.set(
                    "CropBox",
                    vec![(-20).into(), 60.into(), 540.into(), 800.into()],
                );
            }
        }
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        bytes
    }

    #[test]
    fn annotation_fallback_merges_each_page_without_losing_distinct_placements() {
        for rotated_crop in [false, true] {
            let bytes = annotated_bibliography(rotated_crop);
            let mut session = PartialLinkBackend.open(&bytes, None).unwrap();
            let pages = vec![session.page_text(1).unwrap(), session.page_text(2).unwrap()];
            assert!(pages[1].links.is_empty());
            let mut warnings = Vec::new();
            let links = super::link_pages_via_lopdf(&bytes, None, &pages, &mut warnings);
            assert!(warnings.is_empty(), "{warnings:?}");
            assert_eq!(links[0].page, 1);
            assert_eq!(links[1].page, 2);
            assert_eq!(links[0].links.len(), 2); // one native + backend-only, no duplicates
            assert_eq!(links[1].links.len(), 2); // same DOI, different genuine rectangles
            assert_eq!(links[1].links[0].uri, links[1].links[1].uri);
            assert_ne!(links[1].links[0].bbox, links[1].links[1].bbox);
            assert!(
                links[0].links.iter().any(|link| link.bbox.is_none()
                    && link.uri == "https://example.test/backend-evidence")
            );

            // A global Rotate on this horizontal-content fixture turns its
            // lines vertical. The rotated case tests raw annotation geometry;
            // the ordinary case below tests the complete bibliography scan.
            if rotated_crop {
                continue;
            }
            let scan = scan_backward(&PartialLinkBackend, &bytes, None).unwrap();
            assert!(
                scan.found,
                "rotated_crop={rotated_crop}: {:?}",
                scan.warnings
            );
            assert_eq!(scan.total_pages, 2);
            assert_eq!(scan.pages_scanned, 2);
            assert_eq!(
                scan.references
                    .iter()
                    .map(|entry| entry.page)
                    .collect::<Vec<_>>(),
                [1, 2, 2]
            );
            assert_eq!(
                scan.references
                    .iter()
                    .map(|entry| entry.doi_link.as_deref())
                    .collect::<Vec<_>>(),
                [
                    Some("10.1000/first"),
                    Some("10.1000/shared"),
                    Some("10.1000/shared")
                ]
            );
        }
    }

    #[test]
    fn annotation_fallback_does_not_invent_a_transform_or_erase_backend_evidence() {
        let bytes = annotated_bibliography(true);
        let mut session = PartialLinkBackend.open(&bytes, None).unwrap();
        let mut pages = vec![session.page_text(1).unwrap(), session.page_text(2).unwrap()];
        pages[1].width += 1.0;
        let mut warnings = Vec::new();
        let links = super::link_pages_via_lopdf(&bytes, None, &pages, &mut warnings);
        assert_eq!(links[0].links.len(), 2);
        assert!(links[1].links.is_empty());
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("page 2") && warning.contains("geometry differs"))
        );

        warnings.clear();
        let links = super::link_pages_via_lopdf(b"not a PDF", None, &pages, &mut warnings);
        assert_eq!(links[0].links.len(), 2);
        assert!(links[1].links.is_empty());
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("fallback unavailable"))
        );
    }

    #[test]
    fn zero_page_scan_is_rejected_before_any_progress_or_complete_record() {
        let bytes = pdf(&[]);
        let mut events = Vec::new();
        let result = scan_backward_observed(&LopdfBackend::default(), &bytes, None, &mut |event| {
            events.push(event);
        });
        assert!(matches!(
            result,
            Err(crate::backend::BackendError::PageRange { page: 1, count: 0 })
        ));
        assert!(events.is_empty());
    }

    #[test]
    fn selects_last_list_across_pages_without_reading_earlier_pages() {
        let bytes = pdf(&[
            &["Introduction"],
            &[
                "References",
                "[1] First author, Earlier work, 2010.",
                "[2] Second author, Earlier work, 2011.",
                "[3] Third author, Earlier work, 2012.",
            ],
            &["References", "[1] A. One, Final list first work, 2020."],
            &[
                "[2] B. Two, Final list second work, 2021.",
                "[3] C. Three, Final list third work, 2022.",
            ],
            &["Appendix", "Some supplementary prose."],
        ]);
        let scan = scan_backward(&LopdfBackend::default(), &bytes, None).unwrap();
        assert!(scan.found);
        assert_eq!(scan.total_pages, 5);
        assert_eq!(scan.pages_scanned, 3);
        assert_eq!(scan.section_page, Some(3));
        assert_eq!(scan.references.len(), 3);
        assert_eq!(scan.references[0].page, 3);
        assert_eq!(scan.references[2].page, 4);
        assert!(scan.references.iter().all(|r| !r.raw.contains("Earlier")));
    }

    #[test]
    fn progress_counts_pages_read_from_the_end() {
        let bytes = pdf(&[
            &["Introduction"],
            &["[1] A. One, First cited work, 2020."],
            &[
                "[2] B. Two, Second cited work, 2021.",
                "[3] C. Three, Third cited work, 2022.",
            ],
        ]);
        let mut events = Vec::new();
        let scan = scan_backward_observed(&LopdfBackend::default(), &bytes, None, &mut |event| {
            events.push(event);
        })
        .unwrap();
        assert!(scan.found);
        assert_eq!(scan.pages_scanned, 2);
        assert_eq!(
            events,
            [
                Progress::Opened { pages: 3, total: 3 },
                Progress::Page {
                    page: 3,
                    done: 1,
                    total: 3
                },
                Progress::Page {
                    page: 2,
                    done: 2,
                    total: 3
                },
            ]
        );
    }

    #[test]
    fn record_keeps_the_cli_shape_and_renders_plain_text() {
        let bytes = pdf(&[
            &["Introduction"],
            &["[1] A. One, First cited work, 2020."],
            &[
                "[2] B. Two, Second cited work, 2021.",
                "[3] C. Three, Third cited work, 2022.",
            ],
        ]);
        let backend = LopdfBackend::default();
        let scan = scan_backward(&backend, &bytes, None).unwrap();
        let record = Record::from_scan("p.pdf", "ab".repeat(32), backend.identity(), scan, 1.5);
        assert!(record.found());
        let value = serde_json::to_value(&record).unwrap();
        assert_eq!(value["status"], "found");
        assert_eq!(value["extraction_status"], "complete");
        assert_eq!(value["path"], "p.pdf");
        assert_eq!(value["total_pages"], 3);
        assert_eq!(value["pages_scanned"], 2);
        assert_eq!(value["section_page"], 2);
        assert_eq!(value["heading"], serde_json::Value::Null);
        assert_eq!(value["backend"]["name"], "lopdf");
        assert_eq!(value["error"], serde_json::Value::Null);
        assert_eq!(value["references"].as_array().unwrap().len(), 3);
        assert_eq!(
            record.plain_text(),
            "[1] [1] A. One, First cited work, 2020.\n[2] [2] B. Two, Second cited work, 2021.\n[3] [3] C. Three, Third cited work, 2022.\n"
        );

        let failed = Record::failed("p.pdf", None, backend.identity(), "malformed".into(), 0.5);
        assert!(!failed.found());
        let value = serde_json::to_value(&failed).unwrap();
        assert_eq!(value["status"], "failed");
        assert_eq!(value["extraction_status"], "failed");
        assert_eq!(value["sha256"], serde_json::Value::Null);
        assert_eq!(value["total_pages"], serde_json::Value::Null);
        assert_eq!(value["warnings"], serde_json::json!(["malformed"]));
        assert_eq!(value["error"], "malformed");
        assert_eq!(failed.plain_text(), "");
    }

    #[test]
    fn finds_headingless_numbered_list_after_scanning_backward() {
        let bytes = pdf(&[
            &["Introduction"],
            &["[1] A. One, First cited work, 2020."],
            &[
                "[2] B. Two, Second cited work, 2021.",
                "[3] C. Three, Third cited work, 2022.",
            ],
        ]);
        let scan = scan_backward(&LopdfBackend::default(), &bytes, None).unwrap();
        assert!(scan.found);
        assert_eq!(scan.pages_scanned, 2);
        assert_eq!(scan.section_page, Some(2));
        assert_eq!(scan.heading, None);
        assert_eq!(scan.references.len(), 3);
    }

    #[test]
    fn reports_missing_boundary_without_inventing_references() {
        let bytes = pdf(&[&["Introduction"], &["Conclusion"]]);
        let scan = scan_backward(&LopdfBackend::default(), &bytes, None).unwrap();
        assert!(!scan.found);
        assert_eq!(scan.pages_scanned, 2);
        assert!(scan.references.is_empty());
    }

    #[test]
    fn selects_the_last_qualified_heading_when_lists_share_a_page() {
        let bytes = pdf(&[&[
            "References",
            "[1] A. One, Earlier first work, 2020.",
            "[2] B. Two, Earlier second work, 2021.",
            "[3] C. Three, Earlier third work, 2022.",
            "Supplementary References",
            "[1] D. Four, Final first work, 2023.",
            "[2] E. Five, Final second work, 2024.",
            "[3] F. Six, Final third work, 2025.",
        ]]);
        let scan = scan_backward(&LopdfBackend::default(), &bytes, None).unwrap();
        assert_eq!(scan.heading.as_deref(), Some("Supplementary References"));
        assert_eq!(scan.references.len(), 3);
        assert!(
            scan.references
                .iter()
                .all(|entry| entry.raw.contains("Final"))
        );
    }
}
