//! Pure view models for the three screens. Nothing here depends on GPUI, so
//! the labels, line numbering, marker placement and keyboard-pane cycling are
//! unit tested on Linux CI even though the GUI only builds on macOS.

use tpe_common::{PaperRecord, normalize_doi};

use crate::ledger::{CitationRow, CorpusRow, DocumentDetail, ReferenceRow};

/// System prompt sent with every question. It tells the model to stay inside
/// the extracted text rather than invent content, matching the engine's
/// "never fabricate" rule.
pub const SYSTEM_PROMPT: &str = "You are a research assistant inside the Text Processing Engine \
workbench. Answer using only the extracted document text supplied in the user message. When the \
text does not contain the answer, say so plainly instead of guessing. Quote page numbers when \
you cite the text.";

/// The three keyboard-navigable panes, in tab order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Pane {
    /// Corpus list (left).
    #[default]
    Corpus,
    /// Document view (centre).
    Document,
    /// Ask Claude / `ChatGPT` panel (right).
    Ask,
}

impl Pane {
    /// Pane after this one, wrapping around.
    #[must_use]
    pub fn next(self) -> Self {
        match self {
            Self::Corpus => Self::Document,
            Self::Document => Self::Ask,
            Self::Ask => Self::Corpus,
        }
    }

    /// Pane before this one, wrapping around.
    #[must_use]
    pub fn prev(self) -> Self {
        match self {
            Self::Corpus => Self::Ask,
            Self::Document => Self::Corpus,
            Self::Ask => Self::Document,
        }
    }

    /// Visible label of the pane.
    pub fn label(self) -> &'static str {
        match self {
            Self::Corpus => "Corpus",
            Self::Document => "Document",
            Self::Ask => "Ask",
        }
    }
}

/// User-adjustable text scale. GPUI sizes text and spacing in `rem`, so the
/// GUI applies the scale with `Window::set_rem_size(px(scale.rem_px()))`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextScale(pub f32);

impl TextScale {
    /// Smallest allowed scale.
    pub const MIN: f32 = 0.75;
    /// Largest allowed scale.
    pub const MAX: f32 = 2.0;
    /// Increment per step.
    pub const STEP: f32 = 0.125;
    /// Root font size in CSS pixels at scale 1.0.
    pub const BASE_REM_PX: f32 = 16.0;

    /// One step larger, clamped to [`Self::MAX`].
    #[must_use]
    pub fn larger(self) -> Self {
        Self((self.0 + Self::STEP).min(Self::MAX))
    }

    /// One step smaller, clamped to [`Self::MIN`].
    #[must_use]
    pub fn smaller(self) -> Self {
        Self((self.0 - Self::STEP).max(Self::MIN))
    }

    /// Root font size in pixels for this scale.
    pub fn rem_px(self) -> f32 {
        Self::BASE_REM_PX * self.0
    }

    /// Human-readable percentage, e.g. `125%`.
    pub fn percent_label(self) -> String {
        let percent = (self.0 * 100.0).round();
        format!("{percent:.0}%")
    }
}

impl Default for TextScale {
    fn default() -> Self {
        Self(1.0)
    }
}

/// One displayed line of page text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NumberedLine {
    /// 1-based reading-order line number; `None` for blank paragraph breaks.
    pub number: Option<usize>,
    /// The line text.
    pub text: String,
    /// Citation markers whose offset falls on this line, as `[4] -> 4`.
    pub markers: Vec<String>,
}

/// Index of the line (0-based) that contains char `offset` of `text`, or
/// `None` when the offset is past the end of the text.
pub fn line_index_of_offset(text: &str, offset: usize) -> Option<usize> {
    let mut line = 0usize;
    let mut seen = 0usize;
    for ch in text.chars() {
        if seen == offset {
            return Some(line);
        }
        if ch == '\n' {
            line += 1;
        }
        seen += 1;
    }
    (seen == offset).then_some(line)
}

/// Splits page text into numbered lines and attaches the citation markers of
/// `page` to the line each marker's char offset falls on.
pub fn numbered_lines(page_text: &str, citations: &[CitationRow], page: u32) -> Vec<NumberedLine> {
    let mut lines: Vec<NumberedLine> = Vec::new();
    let mut number = 0usize;
    for raw in page_text.split('\n') {
        let blank = raw.trim().is_empty();
        if !blank {
            number += 1;
        }
        lines.push(NumberedLine {
            number: (!blank).then_some(number),
            text: raw.to_owned(),
            markers: Vec::new(),
        });
    }
    for citation in citations.iter().filter(|c| c.page == page) {
        let offset = usize::try_from(citation.offset).unwrap_or(usize::MAX);
        if let Some(ix) = line_index_of_offset(page_text, offset) {
            if let Some(line) = lines.get_mut(ix) {
                line.markers.push(marker_label(citation));
            }
        }
    }
    lines
}

/// Short form of a content hash for labels.
pub fn short_hash(hash: &str) -> &str {
    hash.get(..12).unwrap_or(hash)
}

/// Label for a corpus row: title (or short hash), DOI when known, and status.
pub fn corpus_label(row: &CorpusRow) -> String {
    let title = row
        .title
        .as_deref()
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| short_hash(&row.hash));
    let status = row.status.as_deref().unwrap_or("not extracted");
    match row.doi.as_deref() {
        Some(doi) => format!("{title}  ·  {doi}  ·  {status}"),
        None => format!("{title}  ·  {status}"),
    }
}

/// Label for a reference entry built only from parsed fields; falls back to
/// the raw text when nothing was parsed.
pub fn reference_label(entry: &ReferenceRow) -> String {
    let label = entry
        .label
        .clone()
        .unwrap_or_else(|| format!("#{}", entry.index));
    let mut parts: Vec<String> = Vec::new();
    if !entry.authors.is_empty() {
        parts.push(entry.authors.join("; "));
    }
    if let Some(year) = entry.year {
        parts.push(format!("({year})"));
    }
    if let Some(title) = entry.title.as_deref() {
        parts.push(title.to_owned());
    }
    if let Some(venue) = entry.venue.as_deref() {
        parts.push(venue.to_owned());
    }
    if let Some(doi) = entry.doi.as_deref() {
        parts.push(format!("doi:{doi}"));
    }
    if parts.is_empty() {
        format!("{label} {}", entry.raw)
    } else {
        format!("{label} {}", parts.join(" "))
    }
}

/// `[4] -> 4, 5` or `[4] -> unresolved`.
pub fn marker_label(citation: &CitationRow) -> String {
    if citation.targets.is_empty() {
        format!("{} -> unresolved", citation.text)
    } else {
        let targets: Vec<String> = citation.targets.iter().map(u32::to_string).collect();
        format!("{} -> {}", citation.text, targets.join(", "))
    }
}

/// Label for the citation list: `p.1 @16  [4] -> 4`.
pub fn citation_label(citation: &CitationRow) -> String {
    format!(
        "p.{} @{}  {}",
        citation.page,
        citation.offset,
        marker_label(citation)
    )
}

/// Truncates to at most `max_chars` chars on a char boundary, appending an
/// ellipsis marker when something was cut.
pub fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut out: String = text.chars().take(max_chars).collect();
    if text.chars().nth(max_chars).is_some() {
        out.push_str("\n[... truncated]");
    }
    out
}

/// The document context sent to the model: metadata header plus the page
/// texts (each prefixed with its page number), truncated to `max_chars`.
pub fn document_context(detail: &DocumentDetail, max_chars: usize) -> String {
    let mut header = String::new();
    header.push_str(&format!("Document {}\n", short_hash(&detail.hash)));
    if let Some(title) = detail.title.as_deref() {
        header.push_str(&format!("Title: {title}\n"));
    }
    if !detail.authors.is_empty() {
        header.push_str(&format!("Authors: {}\n", detail.authors.join(", ")));
    }
    if let Some(doi) = detail.doi.as_deref() {
        header.push_str(&format!("DOI: {doi}\n"));
    }
    if let Some(year) = detail.year {
        header.push_str(&format!("Year: {year}\n"));
    }
    header.push_str(&format!(
        "References extracted: {}. Citation markers: {}.\n\n",
        detail.references.len(),
        detail.citations.len()
    ));
    let mut body = String::new();
    for page in &detail.pages {
        body.push_str(&format!("=== Page {} ===\n{}\n\n", page.page, page.text));
    }
    let budget = max_chars.saturating_sub(header.chars().count());
    header.push_str(&truncate_chars(&body, budget));
    header
}

/// Maps the loaded document to the shared record type other tracks consume.
/// Only fields the engine extracted are set; nothing is inferred.
pub fn to_paper_record(detail: &DocumentDetail) -> PaperRecord {
    PaperRecord {
        title: detail.title.clone().unwrap_or_default(),
        authors: detail.authors.clone(),
        year: detail.year,
        venue: detail.venue.clone(),
        doi: detail
            .doi
            .as_deref()
            .map(|d| normalize_doi(d).unwrap_or_else(|| d.to_owned())),
        arxiv_id: detail.arxiv_id.clone(),
        pmid: None,
        pmcid: None,
        url: None,
        abstract_text: detail.abstract_text.clone(),
        source: "tpe".to_owned(),
        source_id: Some(detail.hash.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::PageRow;

    fn citation(page: u32, offset: u32, text: &str, targets: &[u32]) -> CitationRow {
        CitationRow {
            page,
            offset,
            text: text.to_owned(),
            targets: targets.to_vec(),
        }
    }

    #[test]
    fn panes_cycle_in_both_directions() {
        assert_eq!(Pane::Corpus.next(), Pane::Document);
        assert_eq!(Pane::Document.next(), Pane::Ask);
        assert_eq!(Pane::Ask.next(), Pane::Corpus);
        assert_eq!(Pane::Corpus.prev(), Pane::Ask);
        assert_eq!(Pane::Ask.prev().prev(), Pane::Corpus);
        for pane in [Pane::Corpus, Pane::Document, Pane::Ask] {
            assert_eq!(pane.next().prev(), pane);
            assert!(!pane.label().is_empty());
        }
    }

    #[test]
    fn text_scale_steps_and_clamps() {
        let base = TextScale::default();
        assert!((base.rem_px() - 16.0).abs() < f32::EPSILON);
        assert!((base.larger().0 - 1.125).abs() < f32::EPSILON);
        assert!((base.smaller().0 - 0.875).abs() < f32::EPSILON);
        let mut big = base;
        for _ in 0..40 {
            big = big.larger();
        }
        assert!((big.0 - TextScale::MAX).abs() < f32::EPSILON);
        let mut small = base;
        for _ in 0..40 {
            small = small.smaller();
        }
        assert!((small.0 - TextScale::MIN).abs() < f32::EPSILON);
        assert_eq!(TextScale(1.25).percent_label(), "125%");
    }

    #[test]
    fn offsets_map_to_lines() {
        let text = "ab\ncd\n\nef";
        assert_eq!(line_index_of_offset(text, 0), Some(0));
        assert_eq!(line_index_of_offset(text, 2), Some(0), "the newline itself");
        assert_eq!(line_index_of_offset(text, 3), Some(1));
        assert_eq!(line_index_of_offset(text, 6), Some(2), "blank line");
        assert_eq!(line_index_of_offset(text, 8), Some(3));
        assert_eq!(line_index_of_offset(text, 9), Some(3), "end of text");
        assert_eq!(line_index_of_offset(text, 10), None);
        assert_eq!(line_index_of_offset("héllo\nx", 6), Some(1), "char offsets");
    }

    #[test]
    fn lines_are_numbered_in_reading_order_with_markers() {
        let text = "Title\n\nBody [1] and [2].\nMore";
        let cites = [
            citation(1, 12, "[1]", &[0]),
            citation(1, 20, "[2]", &[]),
            citation(2, 0, "[9]", &[8]),
            citation(1, 999, "[x]", &[1]),
        ];
        let lines = numbered_lines(text, &cites, 1);
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0].number, Some(1));
        assert_eq!(lines[1].number, None, "blank paragraph break unnumbered");
        assert_eq!(lines[2].number, Some(2));
        assert_eq!(lines[3].number, Some(3));
        assert_eq!(lines[2].markers, vec!["[1] -> 0", "[2] -> unresolved"]);
        assert!(lines[0].markers.is_empty());
        assert!(
            lines[3].markers.is_empty(),
            "other page and out of range ignored"
        );
    }

    #[test]
    fn corpus_labels_fall_back_to_hash_and_status() {
        let row = CorpusRow {
            hash: "0123456789abcdef".to_owned(),
            title: Some("A Title".to_owned()),
            doi: Some("10.1/x".to_owned()),
            status: Some("complete".to_owned()),
            ..CorpusRow::default()
        };
        assert_eq!(corpus_label(&row), "A Title  ·  10.1/x  ·  complete");
        let bare = CorpusRow {
            hash: "0123456789abcdef".to_owned(),
            ..CorpusRow::default()
        };
        assert_eq!(corpus_label(&bare), "0123456789ab  ·  not extracted");
        assert_eq!(short_hash("abc"), "abc");
    }

    #[test]
    fn reference_labels_use_only_parsed_fields() {
        let parsed = ReferenceRow {
            index: 3,
            label: Some("[4]".to_owned()),
            raw: "raw text".to_owned(),
            authors: vec!["Smith, J.".to_owned(), "Lee, K.".to_owned()],
            title: Some("A Study".to_owned()),
            year: Some(2020),
            venue: Some("Nature".to_owned()),
            doi: Some("10.1/abc".to_owned()),
            page: 9,
        };
        assert_eq!(
            reference_label(&parsed),
            "[4] Smith, J.; Lee, K. (2020) A Study Nature doi:10.1/abc"
        );
        let unparsed = ReferenceRow {
            index: 0,
            raw: "Some raw entry".to_owned(),
            ..ReferenceRow::default()
        };
        assert_eq!(reference_label(&unparsed), "#0 Some raw entry");
        assert_eq!(
            citation_label(&citation(2, 7, "(Smith, 2020)", &[1, 2])),
            "p.2 @7  (Smith, 2020) -> 1, 2"
        );
    }

    #[test]
    fn truncation_respects_char_boundaries() {
        assert_eq!(truncate_chars("héllo", 10), "héllo");
        assert_eq!(truncate_chars("héllo", 2), "hé\n[... truncated]");
        assert_eq!(truncate_chars("", 0), "");
    }

    #[test]
    fn document_context_has_header_pages_and_budget() {
        let detail = DocumentDetail {
            hash: "abcdef0123456789".to_owned(),
            title: Some("T".to_owned()),
            doi: Some("10.1/x".to_owned()),
            year: Some(2021),
            authors: vec!["A".to_owned(), "B".to_owned()],
            pages: vec![
                PageRow {
                    page: 1,
                    text: "one".to_owned(),
                },
                PageRow {
                    page: 2,
                    text: "two".to_owned(),
                },
            ],
            ..DocumentDetail::default()
        };
        let full = document_context(&detail, 10_000);
        assert!(full.starts_with(
            "Document abcdef012345\nTitle: T\nAuthors: A, B\nDOI: 10.1/x\nYear: 2021\n"
        ));
        assert!(full.contains("=== Page 1 ===\none\n"));
        assert!(full.contains("=== Page 2 ===\ntwo\n"));
        assert!(!full.contains("truncated"));
        let cut = document_context(&detail, 120);
        assert!(cut.contains("[... truncated]"));
        assert!(cut.chars().count() < full.chars().count());
    }

    #[test]
    fn paper_record_maps_only_extracted_fields() {
        let detail = DocumentDetail {
            hash: "h".to_owned(),
            title: Some("T".to_owned()),
            doi: Some("https://doi.org/10.1000/ABC".to_owned()),
            authors: vec!["A".to_owned()],
            year: Some(2020),
            ..DocumentDetail::default()
        };
        let record = to_paper_record(&detail);
        assert_eq!(record.title, "T");
        assert_eq!(record.doi.as_deref(), Some("10.1000/abc"));
        assert_eq!(record.source, "tpe");
        assert_eq!(record.source_id.as_deref(), Some("h"));
        assert_eq!(record.pmid, None);
        assert_eq!(record.url, None);
    }
}
