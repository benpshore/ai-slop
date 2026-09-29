//! Bibliography-only extraction from the last page toward the first.
//!
//! This path deliberately leaves full-document metadata, in-text markers,
//! figures, and ledger publication to the existing extraction pipeline.

use serde::Serialize;
use std::collections::VecDeque;

use crate::backend::{BackendError, Extractor};
use crate::citations;
use crate::reading_order;
use crate::regions;
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
}

// Preserve the quick, page-at-a-time search close to the end of a document,
// then inspect geometrically growing suffixes. This bounds the total number
// of pages copied and analysed by the checkpoints to a constant multiple of
// the document length.
const LINEAR_SCAN_PAGES: usize = 8;

fn should_check(scanned: usize, total: usize) -> bool {
    scanned <= LINEAR_SCAN_PAGES || scanned == total || scanned.is_power_of_two()
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
    let mut session = extractor.open(bytes, password)?;
    let total_pages = session.page_count();
    let total_pages_usize = usize::try_from(total_pages).unwrap_or(usize::MAX);
    let mut pages: VecDeque<PageText> = VecDeque::new();

    for number in (1..=total_pages).rev() {
        let mut page = session.page_text(number)?;
        if extractor.provides_reading_order() {
            reading_order::lines_in_backend_order(&mut page);
        } else {
            reading_order::order_page(&mut page);
        }
        pages.push_front(page);

        // After a small exact search near the end, only re-run the
        // whole-suffix analysis when its size doubles (and at EOF). Running it
        // after every page makes a no-bibliography document quadratic.
        if !should_check(pages.len(), total_pages_usize) {
            continue;
        }

        // Cleanup uses the selected document context. Keep the ordered source
        // pages untouched so an earlier page can change that context safely.
        let mut checked: Vec<PageText> = pages.iter().cloned().collect();
        text_cleanup::clean_document(&mut checked);
        regions::tag_regions(&mut checked);
        for section in citations::find_reference_sections(&checked)
            .into_iter()
            .rev()
        {
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
            return Ok(BibliographyScan {
                total_pages,
                pages_scanned: u32::try_from(pages.len()).unwrap_or(u32::MAX),
                found: true,
                section_page: Some(section.first_page),
                heading: (!section.heading.is_empty()).then_some(section.heading),
                references,
                warnings,
            });
        }
    }

    Ok(BibliographyScan {
        total_pages,
        pages_scanned: u32::try_from(pages.len()).unwrap_or(u32::MAX),
        found: false,
        section_page: None,
        heading: None,
        references: Vec::new(),
        warnings: vec!["no bibliography boundary with at least three entries found".to_string()],
    })
}

#[cfg(test)]
mod tests {
    use lopdf::content::{Content, Operation};
    use lopdf::{Document, Object, Stream, dictionary};

    use super::{LINEAR_SCAN_PAGES, scan_backward, should_check};
    use crate::backend::lopdf_backend::LopdfBackend;

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

    #[test]
    fn suffix_analysis_work_is_linear() {
        let total = 8_000;
        let analysed_pages: usize = (1..=total)
            .filter(|&scanned| should_check(scanned, total))
            .sum();

        assert!(analysed_pages <= total * 3 + LINEAR_SCAN_PAGES * LINEAR_SCAN_PAGES);
        assert!(should_check(total, total));
    }
}
