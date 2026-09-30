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
use crate::schema::{PageText, ReferenceEntry};
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
            total_pages: Some(scan.total_pages),
            pages_scanned: Some(scan.pages_scanned),
            section_page: scan.section_page,
            heading: scan.heading,
            references: scan.references,
            warnings: scan.warnings,
            assessment: scan.assessment,
            plausible: scan.plausible,
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
            total_pages: None,
            pages_scanned: None,
            section_page: None,
            heading: None,
            references: Vec::new(),
            warnings: vec![error.clone()],
            assessment: Assessment::default(),
            plausible: true,
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
    let mut pages: Vec<PageText> = Vec::new();
    observe(Progress::Opened {
        pages: total_pages,
        total: total_pages,
    });

    for number in (floor.max(1)..=total_pages).rev() {
        let mut page = session.page_text(number)?;
        if extractor.provides_reading_order() {
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
            let mut warnings: Vec<String> = checked
                .iter()
                .flat_map(|page| page.warnings.iter().cloned())
                .collect();
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
    Ok(BibliographyScan {
        total_pages,
        pages_scanned: u32::try_from(pages.len()).unwrap_or(u32::MAX),
        found: false,
        section_page: None,
        heading: None,
        references: Vec::new(),
        warnings: vec!["no bibliography boundary with at least three entries found".to_string()],
        assessment,
        plausible: true,
    })
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

/// Replace `best` by `candidate` when the candidate is the better list: a
/// found list beats none, and among found lists the longer one wins (the
/// second opinion is only asked for when the first looked cut short).
fn keep_better(best: &mut RoutedScan, candidate: RoutedScan, note: String) {
    let better = candidate.scan.found
        && (!best.scan.found || candidate.scan.references.len() > best.scan.references.len());
    if better || !best.scan.found {
        *best = candidate;
    }
    best.scan.warnings.push(note);
}

/// [`scan_backward_observed`] with routing (`crate::router`): `lopdf` reads
/// the tail; when it finds no list, or the pages it read had unmapped fonts
/// or scans, `pdfium` re-reads the same tail, and docling (layout and OCR)
/// converts the last `max(pages read, FALLBACK_WINDOW)` pages if `pdfium`
/// did not settle it. A list that `lopdf` found but that looks cut short
/// ([`list_plausible`]) goes to docling for the pages from its heading on,
/// and the longer list is kept. A route whose backend is missing or fails
/// is noted in the warnings and the best scan so far is returned.
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
    if best.scan.found && best.scan.plausible && route == Route::Lopdf {
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
                    let again = scan.assessment;
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
                    let plausible = scan.plausible;
                    keep_better(
                        &mut best,
                        RoutedScan {
                            scan,
                            backend: pdfium.identity(),
                        },
                        note,
                    );
                    if best.scan.found && plausible && again.route_after_pdfium() != Route::Docling
                    {
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
