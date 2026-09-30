//! Pure view models for the three screens. Nothing here depends on GPUI, so
//! the labels, line numbering, marker placement, keyboard-pane cycling and
//! the stale-answer guard of the Ask panel are unit tested on Linux CI even
//! though the GUI only builds on macOS.

use std::fmt::Write as _;

use tpe_common::{PaperRecord, normalize_doi};

use crate::ledger::{CitationRow, CorpusRow, DocumentDetail, ReferenceRow};
use crate::tpe_ai::Provider;

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

    /// The scale a stored value names: a number, rounded to the nearest step
    /// and clamped to the allowed range. Anything else (empty, garbled,
    /// not finite) is `None`, so a damaged settings file never breaks launch.
    pub fn parse(text: &str) -> Option<Self> {
        let value: f32 = text.trim().parse().ok()?;
        if !value.is_finite() {
            return None;
        }
        let steps = ((value.clamp(Self::MIN, Self::MAX) - Self::MIN) / Self::STEP).round();
        Some(Self(
            (Self::MIN + steps * Self::STEP).clamp(Self::MIN, Self::MAX),
        ))
    }

    /// The scale stored in `path`, or the default when there is none or it is
    /// unreadable.
    pub fn load(path: &std::path::Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| Self::parse(&text))
            .unwrap_or_default()
    }

    /// Store the scale in `path`, creating its directory. Failure is
    /// returned, not fatal: the scale then simply does not persist.
    ///
    /// # Errors
    /// The file system's error.
    pub fn save(self, path: &std::path::Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, format!("{}\n", self.0))
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
    // Build the character offsets of line breaks once. Citation offsets are
    // attacker-influenced, so rescanning the page for every marker would make
    // rendering quadratic when a page contains many citations.
    let mut newline_offsets = Vec::new();
    let mut char_count = 0usize;
    for ch in page_text.chars() {
        if ch == '\n' {
            newline_offsets.push(char_count);
        }
        char_count += 1;
    }
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
        let line_index = (offset <= char_count)
            .then(|| newline_offsets.partition_point(|newline| *newline < offset));
        if let Some(ix) = line_index
            && let Some(line) = lines.get_mut(ix)
        {
            line.markers.push(marker_label(citation));
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
    // Writing to a `String` cannot fail, so the `fmt::Result`s are dropped.
    let mut header = String::new();
    let _ = writeln!(header, "Document {}", short_hash(&detail.hash));
    if let Some(title) = detail.title.as_deref() {
        let _ = writeln!(header, "Title: {title}");
    }
    if !detail.authors.is_empty() {
        let _ = writeln!(header, "Authors: {}", detail.authors.join(", "));
    }
    if let Some(doi) = detail.doi.as_deref() {
        let _ = writeln!(header, "DOI: {doi}");
    }
    if let Some(year) = detail.year {
        let _ = writeln!(header, "Year: {year}");
    }
    let _ = writeln!(
        header,
        "References extracted: {}. Citation markers: {}.\n",
        detail.references.len(),
        detail.citations.len()
    );
    let mut body = String::new();
    for page in &detail.pages {
        let _ = writeln!(body, "=== Page {} ===\n{}\n", page.page, page.text);
    }
    let budget = max_chars.saturating_sub(header.chars().count());
    header.push_str(&truncate_chars(&body, budget));
    header
}

/// One question sent to a provider, tagged with the state it was issued for
/// so that an answer arriving late can be recognised as stale.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AskRequest {
    /// Monotonically increasing per [`AskTracker`]; the latest issued wins.
    pub id: u64,
    /// Provider the question was sent to.
    pub provider: Provider,
    /// Content hash of the document the question was about, if one was loaded.
    pub document: Option<String>,
}

impl AskRequest {
    /// `Claude · 0123456789ab`, or `Claude · no document`.
    pub fn label(&self) -> String {
        match self.document.as_deref() {
            Some(hash) => format!("{} · {}", self.provider.label(), short_hash(hash)),
            None => format!("{} · no document", self.provider.label()),
        }
    }
}

/// What to do with the answer of a request that has just completed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompletionVerdict {
    /// Latest request, and the workbench still shows the document and
    /// provider it was issued for: show the answer.
    Apply,
    /// Latest request, but the document or provider changed meanwhile: drop
    /// the answer and tell the user.
    Stale,
    /// A newer request was issued since (or none is awaited): drop silently.
    Superseded,
}

/// Pure guard for a completed request `done`: `latest` is the request whose
/// answer is awaited (`None` when none is), `provider` and `document` (content
/// hash) are what the workbench shows at the moment the answer arrives.
pub fn completion_verdict(
    latest: Option<&AskRequest>,
    done: &AskRequest,
    provider: Provider,
    document: Option<&str>,
) -> CompletionVerdict {
    match latest {
        Some(latest) if latest.id == done.id => {
            if done.provider == provider && done.document.as_deref() == document {
                CompletionVerdict::Apply
            } else {
                CompletionVerdict::Stale
            }
        }
        _ => CompletionVerdict::Superseded,
    }
}

/// Issues [`AskRequest`]s with increasing ids and remembers the one in flight.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AskTracker {
    next_id: u64,
    inflight: Option<AskRequest>,
}

impl AskTracker {
    /// The request whose answer is awaited, if any.
    pub fn inflight(&self) -> Option<&AskRequest> {
        self.inflight.as_ref()
    }

    /// True while a request for exactly this provider and document is
    /// awaited. A request issued for another document or provider does not
    /// block a new one; its answer is dropped as superseded when it arrives.
    pub fn is_busy_for(&self, provider: Provider, document: Option<&str>) -> bool {
        self.inflight.as_ref().is_some_and(|request| {
            request.provider == provider && request.document.as_deref() == document
        })
    }

    /// Issues the next request; any earlier in-flight request is superseded.
    pub fn issue(&mut self, provider: Provider, document: Option<&str>) -> AskRequest {
        self.next_id += 1;
        let request = AskRequest {
            id: self.next_id,
            provider,
            document: document.map(str::to_owned),
        };
        self.inflight = Some(request.clone());
        request
    }

    /// Records that `done` finished and returns the verdict for its answer
    /// (see [`completion_verdict`]). The in-flight slot is cleared unless a
    /// newer request owns it.
    pub fn complete(
        &mut self,
        done: &AskRequest,
        provider: Provider,
        document: Option<&str>,
    ) -> CompletionVerdict {
        let verdict = completion_verdict(self.inflight.as_ref(), done, provider, document);
        if verdict != CompletionVerdict::Superseded {
            self.inflight = None;
        }
        verdict
    }
}

/// Title of the answer panel: names the provider and document the shown
/// answer belongs to, or just `Answer` when nothing has been answered yet.
pub fn answer_title(answered: Option<&AskRequest>) -> String {
    match answered {
        Some(request) => format!("Answer from {}", request.label()),
        None => String::from("Answer"),
    }
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
    fn text_scale_reads_only_sensible_stored_values() {
        assert_eq!(TextScale::parse("1.25\n"), Some(TextScale(1.25)));
        assert_eq!(TextScale::parse(" 1 "), Some(TextScale(1.0)));
        assert_eq!(
            TextScale::parse("1.3"),
            Some(TextScale(1.25)),
            "rounded to a step"
        );
        assert_eq!(TextScale::parse("9"), Some(TextScale(TextScale::MAX)));
        assert_eq!(TextScale::parse("0.01"), Some(TextScale(TextScale::MIN)));
        for bad in ["", "big", "NaN", "inf", "-inf", "1,5"] {
            assert_eq!(TextScale::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn text_scale_persists_and_survives_a_damaged_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings").join("text-scale");
        assert_eq!(
            TextScale::load(&path),
            TextScale::default(),
            "no file: the default"
        );
        TextScale(1.5).save(&path).unwrap();
        assert_eq!(TextScale::load(&path), TextScale(1.5));
        std::fs::write(&path, b"\xff\xfe not a number").unwrap();
        assert_eq!(
            TextScale::load(&path),
            TextScale::default(),
            "a damaged file: the default"
        );
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
    fn numbered_lines_maps_offsets_with_one_page_index() {
        let text = "hé\nbody";
        let mut cites = Vec::new();
        for offset in 0..=u32::try_from(text.chars().count()).unwrap() {
            cites.push(citation(1, offset, &format!("[{offset}]"), &[offset]));
        }
        cites.push(citation(1, 99, "[outside]", &[]));

        let lines = numbered_lines(text, &cites, 1);

        assert_eq!(lines[0].markers, ["[0] -> 0", "[1] -> 1", "[2] -> 2"]);
        assert_eq!(
            lines[1].markers,
            ["[3] -> 3", "[4] -> 4", "[5] -> 5", "[6] -> 6", "[7] -> 7"]
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

    fn request(id: u64, provider: Provider, document: Option<&str>) -> AskRequest {
        AskRequest {
            id,
            provider,
            document: document.map(str::to_owned),
        }
    }

    #[test]
    fn completion_applies_only_to_the_latest_request_in_the_same_context() {
        let done = request(3, Provider::Anthropic, Some("abc"));
        let apply = completion_verdict(Some(&done), &done, Provider::Anthropic, Some("abc"));
        assert_eq!(apply, CompletionVerdict::Apply);
        assert_eq!(
            completion_verdict(Some(&done), &done, Provider::OpenAI, Some("abc")),
            CompletionVerdict::Stale,
            "provider changed"
        );
        assert_eq!(
            completion_verdict(Some(&done), &done, Provider::Anthropic, Some("other")),
            CompletionVerdict::Stale,
            "document changed"
        );
        assert_eq!(
            completion_verdict(Some(&done), &done, Provider::Anthropic, None),
            CompletionVerdict::Stale,
            "document unloaded"
        );
        let newer = request(4, Provider::Anthropic, Some("abc"));
        assert_eq!(
            completion_verdict(Some(&newer), &done, Provider::Anthropic, Some("abc")),
            CompletionVerdict::Superseded,
            "a newer request owns the slot"
        );
        assert_eq!(
            completion_verdict(None, &done, Provider::Anthropic, Some("abc")),
            CompletionVerdict::Superseded,
            "nothing awaited"
        );
        let no_document = request(5, Provider::OpenAI, None);
        assert_eq!(
            completion_verdict(Some(&no_document), &no_document, Provider::OpenAI, None),
            CompletionVerdict::Apply
        );
    }

    #[test]
    fn tracker_issues_increasing_ids_and_clears_on_completion() {
        let mut tracker = AskTracker::default();
        assert!(tracker.inflight().is_none());
        assert!(!tracker.is_busy_for(Provider::Anthropic, Some("abc")));

        let first = tracker.issue(Provider::Anthropic, Some("abc"));
        assert_eq!(first.id, 1);
        assert_eq!(first.document.as_deref(), Some("abc"));
        assert_eq!(tracker.inflight(), Some(&first));
        assert!(tracker.is_busy_for(Provider::Anthropic, Some("abc")));
        assert!(
            !tracker.is_busy_for(Provider::OpenAI, Some("abc")),
            "switching provider allows a new question"
        );
        assert!(
            !tracker.is_busy_for(Provider::Anthropic, Some("def")),
            "switching document allows a new question"
        );

        let second = tracker.issue(Provider::OpenAI, Some("abc"));
        assert_eq!(second.id, 2);
        assert_eq!(tracker.inflight(), Some(&second));
        assert_eq!(
            tracker.complete(&first, Provider::OpenAI, Some("abc")),
            CompletionVerdict::Superseded
        );
        assert_eq!(
            tracker.inflight(),
            Some(&second),
            "still awaiting the second"
        );
        assert_eq!(
            tracker.complete(&second, Provider::OpenAI, Some("abc")),
            CompletionVerdict::Apply
        );
        assert!(tracker.inflight().is_none());

        let third = tracker.issue(Provider::Anthropic, None);
        assert_eq!(third.id, 3, "ids keep increasing after completion");
        assert_eq!(
            tracker.complete(&third, Provider::Anthropic, Some("abc")),
            CompletionVerdict::Stale
        );
        assert!(tracker.inflight().is_none(), "a stale request is over too");
    }

    #[test]
    fn answer_title_names_provider_and_document() {
        assert_eq!(answer_title(None), "Answer");
        let with_document = request(1, Provider::Anthropic, Some("0123456789abcdef"));
        assert_eq!(with_document.label(), "Claude · 0123456789ab");
        assert_eq!(
            answer_title(Some(&with_document)),
            "Answer from Claude · 0123456789ab"
        );
        let without = request(2, Provider::OpenAI, None);
        assert_eq!(
            answer_title(Some(&without)),
            "Answer from ChatGPT · no document"
        );
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
