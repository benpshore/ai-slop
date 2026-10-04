//! Conservative correction of a split numbered-reference opening in Docling's
//! paragraph order. Upstream can finish the left column (including the first
//! reference at the page bottom) before the right column's remaining body.
//! The original spans and their sequence/geometry remain extraction evidence.

use crate::schema::{BBox, PageText, ReferenceEntry, Span};

fn reference_like(text: &str, label: u32) -> bool {
    let mut entry = ReferenceEntry {
        raw: text.to_string(),
        label: Some(format!("[{label}]")),
        ..ReferenceEntry::default()
    };
    crate::citations::parse_entry(&mut entry);
    entry.year.is_some() && !entry.authors.is_empty()
}

fn valid_box(span: &Span, width: f32, height: f32) -> Option<BBox> {
    let bbox = span.bbox?;
    ([bbox.x0, bbox.y0, bbox.x1, bbox.y1]
        .into_iter()
        .all(f32::is_finite)
        && bbox.x0 >= 0.0
        && bbox.y0 >= 0.0
        && bbox.x1 <= width
        && bbox.y1 <= height
        && bbox.x0 < bbox.x1
        && bbox.y0 < bbox.y1)
        .then_some(bbox)
}

/// Label evidence is accepted only for spans emitted from upstream `ListItem`
/// nodes. The next page must begin with the two consecutive entries after the
/// split opening entry; an unrelated/reset list does not establish continuity.
pub(super) fn opening_label(text: &str) -> Option<u32> {
    let text = text.trim_start();
    [("[1] ", 1), ("[2] ", 2), ("[3] ", 3)]
        .into_iter()
        .find_map(|(prefix, number)| text.strip_prefix(prefix).map(|_| number))
}

fn next_page_continues(spans: &[Span], list_items: &[usize]) -> bool {
    let mut content = spans
        .iter()
        .enumerate()
        .filter(|(_, span)| !span.text.trim().is_empty());
    [2, 3].into_iter().all(|expected| {
        content.next().is_some_and(|(index, span)| {
            list_items.binary_search(&index).is_ok()
                && opening_label(&span.text) == Some(expected)
                && reference_like(&span.text, expected)
        })
    })
}

/// Correct just the opening `[1]` block when upstream list semantics and
/// geometry prove the observed split-bottom-band pattern:
/// - the next page opens with typed `[2]` and `[3]` entries containing author
///   and year fields recognized by the existing reference parser;
/// - the unfinished opening entry and a lowercase continuation occupy two
///   disjoint columns with aligned tops at the page bottom;
/// - at least two intervening right-column blocks sit above a clear gap;
/// - no unpositioned block, another list item, or low block is crossed.
///
/// Ordinary column endings and ambiguous layouts retain upstream order. This
/// does not reorder raw spans, infer coordinates or change extraction status.
pub(super) fn repair_split_reference_start(
    page: &mut PageText,
    list_items: &[usize],
    next_spans: &[Span],
    next_list_items: &[usize],
) {
    if page.rotation != 0 || !next_page_continues(next_spans, next_list_items) {
        return;
    }
    let Some(first) = list_items.iter().copied().find(|&index| {
        page.spans
            .get(index)
            .is_some_and(|span| opening_label(&span.text) == Some(1))
    }) else {
        return;
    };
    let start = &page.spans[first];
    let Some(left) = valid_box(start, page.width, page.height) else {
        return;
    };
    if left.y1 > page.height * 0.2 || start.text.trim_end().ends_with(['.', '!', '?', ':', ';']) {
        return;
    }
    let Some(tail) = (first + 3..page.spans.len()).find(|&index| {
        let span = &page.spans[index];
        list_items.binary_search(&index).is_err()
            && span
                .text
                .trim_start()
                .chars()
                .next()
                .is_some_and(char::is_lowercase)
            && valid_box(span, page.width, page.height).is_some_and(|right| {
                right.x0 > left.x1
                    && (right.y1 - left.y1).abs()
                        <= 0.5 * (left.y1 - left.y0).min(right.y1 - right.y0)
            })
    }) else {
        return;
    };
    let right = page.spans[tail].bbox.expect("validated continuation box");
    if !reference_like(&format!("{} {}", start.text, page.spans[tail].text), 1) {
        return;
    }
    let band_top = left.y1.max(right.y1);
    let gap = (left.y1 - left.y0) + (right.y1 - right.y0);
    if !page.spans[..first].iter().any(|span| {
        valid_box(span, page.width, page.height)
            .is_some_and(|bbox| bbox.x1 <= left.x1 && bbox.y0 >= band_top + gap)
    }) || (first + 1..tail).any(|index| {
        list_items.binary_search(&index).is_ok()
            || !valid_box(&page.spans[index], page.width, page.height)
                .is_some_and(|bbox| bbox.x0 > left.x1 && bbox.y0 >= band_top + gap)
    }) {
        return;
    }
    // A lower positioned item after the candidate would contradict the claim
    // that these two blocks form the final reference band of this page.
    if page.spans[tail + 1..].iter().any(|span| {
        !span.text.trim().is_empty()
            && (!span
                .text
                .chars()
                .all(|c| c.is_ascii_digit() || c.is_whitespace())
                || valid_box(span, page.width, page.height).is_none_or(|b| b.y0 <= band_top))
    }) {
        return;
    }
    let first_span = u32::try_from(first).ok();
    let tail_span = u32::try_from(tail).ok();
    let first_line = page
        .lines
        .iter()
        .position(|line| line.spans.len() == 1 && line.spans.first().copied() == first_span);
    let tail_line = page
        .lines
        .iter()
        .position(|line| line.spans.len() == 1 && line.spans.first().copied() == tail_span);
    if let (Some(first_line), Some(tail_line)) = (first_line, tail_line)
        && tail_line > first_line
    {
        let line = page.lines.remove(first_line);
        page.lines.insert(tail_line - 1, line);
        page.text = page
            .lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        citations, eval, latex_refs::TruthReference, reading_order, regions, text_cleanup,
    };

    #[derive(serde::Deserialize)]
    struct Fixture {
        pages: Vec<PageText>,
        truth: Vec<TruthReference>,
        list_items: Vec<Vec<usize>>,
    }

    fn fixture() -> Fixture {
        serde_json::from_str(include_str!(
            "../../tests/fixtures/docling_bibliography/2309.10334-reference-band.json"
        ))
        .unwrap()
    }

    fn prepared() -> Fixture {
        let mut fixture = fixture();
        for page in &mut fixture.pages {
            reading_order::lines_in_backend_order(page);
        }
        fixture
    }

    fn repair(fixture: &mut Fixture) {
        let (current, next) = fixture.pages.split_at_mut(1);
        repair_split_reference_start(
            &mut current[0],
            &fixture.list_items[0],
            &next[0].spans,
            &fixture.list_items[1],
        );
    }

    fn references(fixture: &mut Fixture) -> (usize, usize, Vec<crate::schema::ReferenceEntry>) {
        text_cleanup::clean_document(&mut fixture.pages);
        regions::tag_regions(&mut fixture.pages);
        let (references, _) = citations::extract_citations(&fixture.pages);
        let matched = eval::match_references(&fixture.truth, &references)
            .iter()
            .filter(|pair| pair.extracted_index.is_some())
            .count();
        (references.len(), matched, references)
    }

    #[test]
    fn actual_docling_reference_band_recovers_all_36_matches_without_changing_spans() {
        let mut before = prepared();
        let (count, matched, _) = references(&mut before);
        assert_eq!((count, matched), (1, 0));

        let mut fixed = prepared();
        let raw = fixed.pages[0].spans.clone();
        let warnings = fixed.pages[0].warnings.clone();
        let status = fixed.pages[0].extraction_status();
        assert_eq!(status, crate::schema::Status::Partial);
        let caption = fixed.pages[0].lines[0].clone();
        repair(&mut fixed);
        assert_eq!(fixed.pages[0].spans, raw);
        assert_eq!(fixed.pages[0].warnings, warnings);
        assert_eq!(fixed.pages[0].extraction_status(), status);
        assert_eq!(fixed.pages[0].lines[0], caption);
        let once = fixed.pages[0].clone();
        repair(&mut fixed);
        assert_eq!(fixed.pages[0], once);
        let (count, matched, entries) = references(&mut fixed);
        assert_eq!((count, matched), (36, 36));
        assert!(entries[0].raw.ends_with("physics 75 , 126001 (2012)."));
        assert!(
            entries
                .iter()
                .all(|entry| !entry.raw.contains("Bregman divergence"))
        );
    }

    #[test]
    fn normal_column_endings_and_untyped_numbered_prose_keep_upstream_order() {
        let mut fixture = prepared();
        fixture.list_items[0].clear();
        let original = fixture.pages[0].clone();
        repair(&mut fixture);
        assert_eq!(fixture.pages[0], original);
    }

    #[test]
    fn reset_or_missing_next_page_list_does_not_prove_reference_continuity() {
        for reset in [true, false] {
            let mut fixture = prepared();
            if reset {
                fixture.pages[1].spans[0].text = "[1] A different appendix list.".into();
            } else {
                fixture.list_items[1].clear();
            }
            let original = fixture.pages[0].clone();
            repair(&mut fixture);
            assert_eq!(fixture.pages[0], original);
        }
    }

    #[test]
    fn ordinary_typed_numbered_lists_do_not_qualify_as_bibliography() {
        let mut fixture = prepared();
        fixture.pages[1].spans[0].text = "[2] Continue the second step.".into();
        fixture.pages[1].spans[1].text = "[3] Complete the third step.".into();
        let original = fixture.pages[0].clone();
        repair(&mut fixture);
        assert_eq!(fixture.pages[0], original);
    }

    #[test]
    fn unknown_geometry_or_intervening_list_item_is_a_hard_order_boundary() {
        for unpositioned in [true, false] {
            let mut fixture = prepared();
            if unpositioned {
                fixture.pages[0].spans[11].bbox = None;
            } else {
                fixture.list_items[0].push(11);
            }
            let original = fixture.pages[0].clone();
            repair(&mut fixture);
            assert_eq!(fixture.pages[0], original);
        }
    }

    #[test]
    fn no_clear_gap_or_misaligned_continuation_keeps_original_order() {
        for no_gap in [true, false] {
            let mut fixture = prepared();
            if no_gap {
                fixture.pages[0].spans[14].bbox.as_mut().unwrap().y0 = 80.0;
            } else {
                fixture.pages[0].spans[15].bbox.as_mut().unwrap().y1 = 95.0;
            }
            let original = fixture.pages[0].clone();
            repair(&mut fixture);
            assert_eq!(fixture.pages[0], original);
        }
    }

    #[test]
    fn rotated_geometry_is_not_treated_as_an_upright_reference_band() {
        let mut fixture = prepared();
        fixture.pages[0].rotation = 90;
        let original = fixture.pages[0].clone();
        repair(&mut fixture);
        assert_eq!(fixture.pages[0], original);
    }
}
