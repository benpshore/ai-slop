//! Pure view models for the three screens. Nothing here depends on GPUI, so
//! the labels, line numbering, marker placement, keyboard-pane cycling and
//! the stale-answer guard of the Ask panel are unit tested on Linux CI even
//! though the GUI only builds on macOS.

use std::fmt::Write as _;

use tpe_common::{PaperRecord, normalize_doi};

use crate::ledger::{
    AttemptRow, CitationRow, CorpusRow, DocumentDetail, ObservationRow, ReferenceRow,
};
use crate::tpe_ai::Provider;

/// System prompt sent with every question. It tells the model to stay inside
/// the extracted text rather than invent content, matching the engine's
/// "never fabricate" rule.
pub const SYSTEM_PROMPT: &str = "You are a research assistant inside the Text Processing Engine \
workbench. Answer using only the extracted document text supplied in the user message. When the \
text does not contain the answer, say so plainly instead of guessing. Quote page numbers when \
you cite the text.";

/// User-facing lifecycle states.  `CancellationRequested` is deliberately
/// separate from `Cancelled`: requesting cancellation does not imply that a
/// worker has stopped or released the source yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkState {
    Queued,
    Stabilizing,
    Processing,
    Partial,
    Failed,
    Complete,
    CancellationRequested,
    Cancelled,
}

impl WorkState {
    pub fn from_status(status: Option<&str>) -> Self {
        match status.unwrap_or("queued") {
            "stabilizing" => Self::Stabilizing,
            "processing" | "active" | "running" => Self::Processing,
            "partial" => Self::Partial,
            "failed" => Self::Failed,
            "complete" => Self::Complete,
            "cancellation_requested" | "cancel_requested" => Self::CancellationRequested,
            "cancelled" | "canceled" => Self::Cancelled,
            _ => Self::Queued,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Queued => "Queued",
            Self::Stabilizing => "Stabilizing",
            Self::Processing => "Processing",
            Self::Partial => "Partial",
            Self::Failed => "Failed",
            Self::Complete => "Complete",
            Self::CancellationRequested => "Cancellation requested",
            Self::Cancelled => "Cancelled",
        }
    }
}

/// Fully formatted, platform-independent queue row consumed by GPUI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProgressRow {
    pub observation_id: i64,
    pub hash: String,
    pub path: String,
    pub state: WorkState,
    pub summary: String,
    pub accessibility_label: String,
}

impl ProgressRow {
    pub fn new(observation: &ObservationRow, now: i64) -> Self {
        let state = WorkState::from_status(observation.attempt.as_ref().map(|a| a.status.as_str()));
        let mut fields = vec![state.label().to_owned()];
        if let Some(attempt) = observation.attempt.as_ref() {
            append_progress(&mut fields, attempt);
            fields.push(format_elapsed(now.saturating_sub(attempt.started_at)));
            if !attempt.warnings.is_empty() {
                fields.push(format!("{} warning(s)", attempt.warnings.len()));
            }
            if !attempt.retry_history.is_empty() {
                fields.push(format!("retry {}", attempt.retry_history.len()));
            }
            if let Some(error) = attempt.terminal_error.as_deref() {
                fields.push(error.to_owned());
            }
        }
        let summary = fields.join(" · ");
        let accessibility_label = format!(
            "{}; {}; {}",
            observation.path,
            short_hash(&observation.hash),
            summary
        );
        Self {
            observation_id: observation.id,
            hash: observation.hash.clone(),
            path: observation.path.clone(),
            state,
            summary,
            accessibility_label,
        }
    }
}

fn append_progress(fields: &mut Vec<String>, attempt: &AttemptRow) {
    if let Some(position) = attempt.queue_position {
        fields.push(format!("queue position {position}"));
    }
    if let Some(stage) = attempt.current_stage.as_deref() {
        fields.push(stage.to_owned());
    }
    if let Some(total) = attempt.pages_total.filter(|total| *total > 0) {
        let done = attempt.pages_done.min(total);
        fields.push(format!("{done}/{total} pages"));
        fields.push(format!("{}%", u64::from(done) * 100 / u64::from(total)));
    } else if attempt.pages_done > 0 {
        fields.push(format!("{} pages", attempt.pages_done));
    }
    if let Some(total) = attempt.chunks_total.filter(|total| *total > 0) {
        fields.push(format!(
            "{}/{} chunks",
            attempt.chunks_done.min(total),
            total
        ));
    } else if attempt.chunks_done > 0 {
        fields.push(format!("{} chunks", attempt.chunks_done));
    }
}

pub fn format_elapsed(seconds: i64) -> String {
    let seconds = seconds.max(0);
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    }
}

/// Generation guard used by asynchronous refreshes. Older results can never
/// replace a newer snapshot, even if database reads complete out of order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RefreshModel {
    issued: u64,
    applied: u64,
    pub rows: Vec<ProgressRow>,
    pub selected_hash: Option<String>,
}

impl RefreshModel {
    pub fn begin(&mut self) -> u64 {
        self.issued += 1;
        self.issued
    }
    pub fn apply(&mut self, generation: u64, rows: Vec<ProgressRow>) -> bool {
        if generation < self.issued || generation <= self.applied {
            return false;
        }
        self.applied = generation;
        self.rows = rows;
        if self
            .selected_hash
            .as_ref()
            .is_some_and(|hash| !self.rows.iter().any(|row| &row.hash == hash))
        {
            self.selected_hash = None;
        }
        true
    }
    pub fn selected_index(&self) -> Option<usize> {
        let hash = self.selected_hash.as_deref()?;
        self.rows.iter().position(|row| row.hash == hash)
    }
}

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
        if let Some(ix) = line_index_of_offset(page_text, offset)
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

    fn observation(id: i64, hash: &str, path: &str, status: &str) -> ObservationRow {
        ObservationRow {
            id,
            hash: hash.into(),
            path: path.into(),
            attempt: Some(AttemptRow {
                id,
                status: status.into(),
                started_at: 10,
                ..AttemptRow::default()
            }),
            ..ObservationRow::default()
        }
    }

    #[test]
    fn refresh_race_rejects_an_older_snapshot() {
        let mut model = RefreshModel::default();
        let old = model.begin();
        let new = model.begin();
        assert!(model.apply(
            new,
            vec![ProgressRow::new(
                &observation(2, "new", "/new", "processing"),
                12
            )]
        ));
        assert!(!model.apply(
            old,
            vec![ProgressRow::new(
                &observation(1, "old", "/old", "complete"),
                12
            )]
        ));
        assert_eq!(model.rows[0].hash, "new");
    }

    #[test]
    fn replacement_clears_selection_but_reordering_preserves_hash() {
        let mut model = RefreshModel {
            selected_hash: Some("a".into()),
            ..RefreshModel::default()
        };
        let generation = model.begin();
        model.apply(
            generation,
            vec![
                ProgressRow::new(&observation(2, "b", "/b", "queued"), 12),
                ProgressRow::new(&observation(1, "a", "/a", "queued"), 12),
            ],
        );
        assert_eq!(model.selected_index(), Some(1));
        let generation = model.begin();
        model.apply(
            generation,
            vec![ProgressRow::new(
                &observation(3, "replacement", "/a", "queued"),
                12,
            )],
        );
        assert_eq!(model.selected_hash, None);
    }

    #[test]
    fn duplicate_paths_are_individual_observations() {
        let rows = [
            observation(1, "same", "/one.pdf", "complete"),
            observation(2, "same", "/two.pdf", "complete"),
        ];
        let shown: Vec<_> = rows.iter().map(|row| ProgressRow::new(row, 20)).collect();
        assert_eq!(shown.len(), 2);
        assert_ne!(shown[0].observation_id, shown[1].observation_id);
    }

    #[test]
    fn unknown_totals_never_show_a_percentage() {
        let mut row = observation(1, "h", "/a", "processing");
        row.attempt.as_mut().unwrap().pages_done = 3;
        let shown = ProgressRow::new(&row, 20);
        assert!(shown.summary.contains("3 pages"));
        assert!(!shown.summary.contains('%'));
    }

    #[test]
    fn stale_attempt_is_replaced_by_latest_observation_data() {
        let old = ProgressRow::new(&observation(1, "h", "/a", "processing"), 20);
        let current = ProgressRow::new(&observation(1, "h", "/a", "complete"), 30);
        assert_eq!(old.state, WorkState::Processing);
        assert_eq!(current.state, WorkState::Complete);
    }

    #[test]
    fn every_state_has_a_complete_accessibility_label() {
        for status in [
            "queued",
            "stabilizing",
            "processing",
            "partial",
            "failed",
            "complete",
            "cancellation_requested",
            "cancelled",
        ] {
            let row = ProgressRow::new(
                &observation(1, "0123456789abcdef", "/paper.pdf", status),
                20,
            );
            assert!(row.accessibility_label.contains("/paper.pdf"));
            assert!(row.accessibility_label.contains(row.state.label()));
            assert!(!row.accessibility_label.trim().is_empty());
        }
    }
}
