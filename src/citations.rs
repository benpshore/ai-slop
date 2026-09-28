//! Reference-list segmentation, reference-entry parsing and in-text citation
//! markers.
//!
//! The reference list is the core product: every entry is captured with its
//! raw text preserved, and the parsed fields are best-effort readings of that
//! raw text. Nothing is invented: a field stays `None` unless the raw text
//! contains it. All functions work on the `lines` and `text` that the
//! reading-order pass filled in; marker offsets are char offsets into
//! `PageText::text`.

use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::OnceLock;

use regex::Regex;

use crate::schema::{CitationMarker, Line, PageText, ReferenceEntry};

/// Where the reference list starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReferenceSection {
    /// Page number (as printed by the backend) of the heading line.
    pub first_page: u32,
    /// Index of the heading line in that page's `lines`.
    pub first_line: usize,
    /// Heading text as printed, trimmed.
    pub heading: String,
}

/// Reference-list numbering style detected from the first entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Style {
    /// `[12]`
    Bracket,
    /// `12.`
    Dot,
    /// `12)`
    Paren,
    /// `Smith, A. (2020)` and friends.
    AuthorYear,
}

/// One line of the reference section with the layout evidence needed for
/// segmentation.
#[derive(Clone, Debug)]
struct SectionLine {
    page: u32,
    column: u32,
    x0: Option<f32>,
    y0: Option<f32>,
    size: Option<f32>,
    /// Line sits in the top or bottom margin band (or has no bbox).
    edge: bool,
    text: String,
}

impl SectionLine {
    /// Is `other` a fragment of the same printed row (same page and column,
    /// baselines within 0.4 × the font size)?
    fn same_row(&self, other: &Self) -> bool {
        if self.page != other.page || self.column != other.column {
            return false;
        }
        let (Some(a), Some(b)) = (self.y0, other.y0) else {
            return false;
        };
        let size = self.size.or(other.size).unwrap_or(10.0);
        (a - b).abs() <= 0.4 * size
    }

    /// Append the fragment `other` to this row.
    fn absorb(&mut self, other: &Self) {
        if !self.text.is_empty() && !other.text.is_empty() {
            self.text.push(' ');
        }
        self.text.push_str(&other.text);
        self.x0 = match (self.x0, other.x0) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        self.size = match (self.size, other.size) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        self.edge = self.edge && other.edge;
    }
}

/// `text` with every digit replaced by `#`, so that `Page 30 of 35` and
/// `Page 31 of 35` compare equal.
fn digit_key(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_ascii_digit() { '#' } else { c })
        .collect()
}

/// Largest gap in points between an entry start and its continuation lines
/// that still counts as "the same indent".
const INDENT_TOLERANCE: f32 = 1.0;
/// Widest hanging indent (points) between an entry start and its
/// continuation lines.
const MAX_HANGING_INDENT: f32 = 40.0;
/// Smallest share of the section's lines that must sit at a hanging indent
/// before the layout is trusted over the text patterns.
const MIN_INDENTED_SHARE: f32 = 0.25;
/// Longest run of numbers accepted from a `[a–b]` range marker.
const MAX_RANGE_SPAN: u32 = 50;
/// Longest token accepted as the continuation of a DOI or URL broken by a
/// line wrap.
const MAX_WRAP_TOKEN: usize = 64;

/// Prefixes that usually keep their hyphen when a compound is broken at a
/// line end (`multi- task` → `multi-task`).
const COMPOUND_PREFIXES: &[&str] = &[
    "anti", "auto", "bi", "co", "cross", "e", "fine", "high", "hyper", "inter", "intra", "long",
    "low", "meta", "micro", "multi", "non", "of", "one", "post", "pre", "pseudo", "quasi", "real",
    "self", "semi", "short", "spatio", "sub", "super", "the", "three", "two", "ultra", "well",
    "zero",
];
/// Second halves that usually keep their hyphen (`privacy- preserving`).
const COMPOUND_HEADS: &[&str] = &[
    "agnostic",
    "augmented",
    "aware",
    "based",
    "box",
    "dimensional",
    "driven",
    "efficient",
    "end",
    "form",
    "free",
    "generated",
    "grained",
    "intensive",
    "language",
    "level",
    "like",
    "linear",
    "machine",
    "order",
    "oriented",
    "preserving",
    "scale",
    "shot",
    "specific",
    "task",
    "time",
    "to",
    "wise",
    "world",
];

fn heading_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^\s*(?:(?:\d+|[IVX]+)\.?\s*)?(?:References|REFERENCES|Reference List|Bibliography|BIBLIOGRAPHY|Works Cited|WORKS CITED|Literature Cited|LITERATURE CITED)\s*:?\s*$",
        )
        .expect("valid regex")
    })
}

fn end_heading_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)^\s*[-–—\s]*(?:(?:\d+|[A-Z]|[IVX]+)[.:]?\s+)?(?:appendix|appendices|supplementary|supplemental|supporting information|acknowledg\w*|author biograph\w*|biograph\w*)\b",
        )
        .expect("valid regex")
    })
}

/// `Table 5: ...` / `Figure 2.` captions that follow a reference list.
fn caption_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^\s*(?:Table|Figure|Fig\.|TABLE|FIGURE)\s+\d+").expect("valid regex")
    })
}

/// A table row: three or more purely numeric cells.
fn numeric_row_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^\s*(?:[-+±]?\d+(?:[.,]\d+)?%?\s+){2,}[-+±]?\d+(?:[.,]\d+)?%?\s*$")
            .expect("valid regex")
    })
}

/// First line of an author biography: a name of at least two capitalised
/// words followed by a biography verb phrase (`received the`, `is currently`,
/// `is a Professor`, `was born`).
fn biography_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^\s*\p{Lu}[\p{L}.'’\-]*(?:\s+\p{Lu}[\p{L}.'’\-]*){1,6}\s+(?:\([^)]{1,40}\)\s+)?(?:received (?:the|his|her|a|an)\b|is currently\b|was born\b|is with\b|obtained (?:the|his|her|a)\b|holds (?:a|an|the)\b|earned (?:the|his|her|a|an)\b|is (?:a|an) (?:Postdoctoral|Professor|Ph\.?D|Research|Senior|Principal|Lecturer|Assistant|Associate|Full|Distinguished|Staff|student|graduate|member|Member|faculty|postdoc|Postdoc)\b)",
        )
        .expect("valid regex")
    })
}

/// A line that may open an author-year entry: capital letter, opening quote
/// or bracket, or a lowercase surname particle.
fn entry_start_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"^(?:\p{Lu}|[“"„‘\[(]|(?:van|von|de|der|den|del|di|da|la|le|du)\s)"#)
            .expect("valid regex")
    })
}

fn bracket_label_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*\[(\d+)\]\s*").expect("valid regex"))
}

fn dot_label_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*(\d+)\.\s+").expect("valid regex"))
}

fn paren_label_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*(\d+)\)\s+").expect("valid regex"))
}

fn page_number_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*\d{1,4}\s*$").expect("valid regex"))
}

/// Start of an author-year entry: `Smith, A.`, `Smith, John`, `Smith AB,`,
/// `van der Maaten, L.`.
fn author_start_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^\s*(?:(?:van|von|de|der|den|del|di|da|la|le|du)\s+)*\p{Lu}[\p{L}'’\-]*(?:\s+\p{Lu}[\p{L}'’\-]*)?(?:,\s*\p{Lu}(?:\.|\p{L}+)|\s+\p{Lu}{1,3}\b[,.]?)",
        )
        .expect("valid regex")
    })
}

/// Leading surname of an author-year entry (used for the `Smith2020` label).
fn surname_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^\s*((?:(?:van|von|de|der|den|del|di|da|la|le|du)\s+)*\p{Lu}[\p{L}'’\-]+)")
            .expect("valid regex")
    })
}

fn year_paren_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\(((?:19|20)\d{2})[a-z]?\)").expect("valid regex"))
}

fn year_bare_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?:^|[^\d–\-—])((?:19|20)\d{2})[a-z]?(?:[^\d–\-—]|$)").expect("valid regex")
    })
}

/// Start of a DOI, tolerating the single space a line wrap leaves after
/// `10.` or before `/`.
fn doi_start_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\b10\.\s?\d{4,9}\s?/").expect("valid regex"))
}

/// A bare year token (`2020`, `2020a`).
fn year_token_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(?:19|20)\d{2}[a-z]?$").expect("valid regex"))
}

/// A year that opens the text after the author list (`2019. Title` in ACM
/// style, `(2019). Title` without an author-only prefix).
fn leading_year_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*\(?(?:19|20)\d{2}[a-z]?\)?[.,:]?\s+").expect("valid regex"))
}

/// `A.`, `A.B.`, `J.-M.` or a broken `P.-` before a line-wrapped `Y.`.
fn initial_token_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\p{Lu}\.(?:-?\p{Lu}\.)*-?$").expect("valid regex"))
}

/// A capitalised word: `Smith`, `O'Brien`, `Ahmadi-Asl`, `IEEE`.
fn cap_word_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\p{Lu}[\p{L}'’\-]*$").expect("valid regex"))
}

/// Up to three capitals: Vancouver initials (`AB`) or a short acronym.
fn caps_block_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\p{Lu}{1,3}$").expect("valid regex"))
}

/// ` and ` / ` & ` between two names inside one comma-delimited part.
fn and_split_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\s+(?:and|&)\s+").expect("valid regex"))
}

/// Trailing `et al.` / `and others` of a name part.
fn et_al_tail_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\s+(?:et\s+al\.?|and\s+others)$").expect("valid regex"))
}

/// `et al.` opening the part that follows the last comma of the author list.
fn et_al_lead_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(?i:et\s+al\.?)\s+").expect("valid regex"))
}

/// A year at the start of a comma-delimited part.
fn year_lead_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\(?(?:19|20)\d{2}").expect("valid regex"))
}

/// Words that open the venue part after a comma-delimited title.
fn venue_lead_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^(?i:in\b|pp?\.|pages?\b|vol\b|volume\b|arxiv|http|doi\b|eds?\b|edited\b|editors?\b|tech\b|technical\b|phd\b|master|chapter\b|ch\.|no\.|preprint|proc\b|proceedings\b|submitted\b|to appear|available|url\b|accessed|retrieved|ser\.|series\b|version\b|v\d)",
        )
        .expect("valid regex")
    })
}

/// A comma inside an unquoted title that is followed by venue words
/// (`Title, volume 6.` / `Title, pp. 3–9` / `Title, in Proceedings`).
fn comma_venue_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r",\s+(?:(?i:vol(?:ume)?\b|pp?\.|pages?\b|in:|eds?\.|edited\b|editors?\b|no\.|chapter\b|ch\.|tech\.|technical\b|version\b|arxiv)|in\s+\p{Lu})",
        )
        .expect("valid regex")
    })
}

fn arxiv_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)(?:arxiv[\s:.]*|abs/)(\d{4}\.\d{4,5}(?:v\d+)?|[a-z\-]+(?:\.[a-z]{2})?/\d{7})",
        )
        .expect("valid regex")
    })
}

fn url_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"https?://[^\s"<>]+"#).expect("valid regex"))
}

fn pages_labelled_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\b(?:pp?\.?\s*|pages?\s+)(\d+)(?:\s*[–\-—]\s*(\d+))?")
            .expect("valid regex")
    })
}

fn vol_issue_pages_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\b(\d+)\s*\((\d+(?:[–\-]\d+)?)\)\s*[:,]\s*(\d+)(?:\s*[–\-—]\s*(\d+))?")
            .expect("valid regex")
    })
}

fn vol_colon_pages_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\b(\d+)\s*:\s*(\d+)\s*[–\-—]\s*(\d+)").expect("valid regex"))
}

fn vol_comma_pages_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\b(\d+),\s*(\d+)\s*[–\-—]\s*(\d+)\b").expect("valid regex"))
}

fn vol_labelled_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)\bvol(?:ume)?\.?\s*(\d+)").expect("valid regex"))
}

fn issue_labelled_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)\b(?:no|number|issue)\.?\s*(\d+)").expect("valid regex"))
}

fn vol_issue_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\b(\d+)\s*\((\d+(?:[–\-]\d+)?)\)").expect("valid regex"))
}

/// Volume (and issue) before a blanked year: `35 (    ) 61–70`,
/// `20 (2) (    ) 130–141`, `22 (    ), pp.`.
fn vol_before_year_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"\b(\d+)\s*(?:\((\d+(?:[–\-]\d+)?)\)\s*)?\(\s+[a-z]?\)(?:\s*[,:]?\s*(\d+)\s*[–\-—]\s*(\d+))?",
        )
        .expect("valid regex")
    })
}

fn dash_range_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\b(\d+)\s*[–—]\s*(\d+)\b").expect("valid regex"))
}

fn in_venue_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^(.*?)(?:,|\(|\s+vol\b|\s+pp?\.|\s+pages\b|\.\s+(?:\d|pp?\.|vol\b|pages\b)|$)")
            .expect("valid regex")
    })
}

fn journal_venue_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(.*?)(?:,|\(|\d|;|\s+vol\b|\s+pp?\.|$)").expect("valid regex"))
}

fn publisher_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^([^:\d]{2,60}):\s+(\p{Lu}[^.]{1,80})\.?\s*$").expect("valid regex")
    })
}

fn author_sep_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\s*(?:,|;|&|\band\b)\s*").expect("valid regex"))
}

fn initials_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\p{Lu}\.?(?:[\s\-]*\p{Lu}\.?)*$").expect("valid regex"))
}

/// `Smith, A.` / `Smith, John,` / `Lee, J. and`: a surname-first author list.
/// `Hideo Bannai, Mitsuru Funakoshi` (two full names) is not one.
fn surname_first_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^\p{Lu}[\p{L}'’\-]+(?:\s+\p{Lu}[\p{L}'’\-]+)?,\s*(?:\p{Lu}\.|\p{Lu}[\p{L}'’\-]+\s*(?:,|\band\b|&|\(|$))",
        )
        .expect("valid regex")
    })
}

/// `Smith AB, Jones C.` (Vancouver initials without periods). `Brent N.
/// Clark` is not Vancouver: a single initial with a period is a middle name.
fn vancouver_start_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^\p{Lu}[\p{L}'’\-]+\s+(?:\p{Lu}{1,3},|\p{Lu}{2,3}\.\s)").expect("valid regex")
    })
}

fn numeric_marker_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\[(\s*\d+\s*(?:[–\-—]\s*\d+\s*)?(?:[,;]\s*\d+\s*(?:[–\-—]\s*\d+\s*)?)*)\]")
            .expect("valid regex")
    })
}

fn numeric_item_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(\d+)\s*(?:[–\-—]\s*(\d+))?").expect("valid regex"))
}

fn narrative_marker_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(\p{Lu}[\p{L}'’\-]+(?:\s+(?:and|&)\s+\p{Lu}[\p{L}'’\-]+|\s+et\s+al\.?)?)\s+\(((?:19|20)\d{2})([a-z]?)\)",
        )
        .expect("valid regex")
    })
}

fn parenthetical_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\(([^()]*?(?:19|20)\d{2}[a-z]?[^()]*)\)").expect("valid regex"))
}

fn clause_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^\s*(?:(?:see|e\.g\.|cf\.|also|and|but|in)\s*,?\s*)*(\p{Lu}[\p{L}'’\-]+(?:\s+(?:and|&)\s+\p{Lu}[\p{L}'’\-]+|\s+et\s+al\.?)?),?\s*((?:19|20)\d{2})([a-z]?)",
        )
        .expect("valid regex")
    })
}

fn numbered_label_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\[?(\d+)[\].)]?$").expect("valid regex"))
}

/// Find the reference-list heading: the LAST line matching
/// `^(\d+\.?\s*)?(References|Bibliography|Works Cited|Literature Cited)\s*$`.
pub fn find_reference_section(pages: &[PageText]) -> Option<ReferenceSection> {
    let mut found: Option<ReferenceSection> = None;
    for page in pages {
        for (i, line) in page.lines.iter().enumerate() {
            if heading_re().is_match(&line.text) {
                found = Some(ReferenceSection {
                    first_page: page.page,
                    first_line: i,
                    heading: line.text.trim().to_string(),
                });
            }
        }
    }
    found
}

/// Largest font size among the spans of `line`.
fn line_size(page: &PageText, line: &Line) -> Option<f32> {
    let mut best: Option<f32> = None;
    for idx in &line.spans {
        if let Some(size) = page.spans.get(*idx as usize).and_then(|s| s.size) {
            best = Some(best.map_or(size, |b| b.max(size)));
        }
    }
    best
}

/// Lines of the reference section in reading order, across pages, with page
/// furniture (page numbers, repeated running headers/footers) removed.
fn section_lines(pages: &[PageText], section: &ReferenceSection) -> Vec<SectionLine> {
    let mut lines: Vec<SectionLine> = Vec::new();
    for page in pages {
        if page.page < section.first_page {
            continue;
        }
        let skip = if page.page == section.first_page {
            section.first_line + 1
        } else {
            0
        };
        for line in page.lines.iter().skip(skip) {
            let edge = line
                .bbox
                .is_none_or(|b| b.y1 > page.height * 0.92 || b.y0 < page.height * 0.08);
            let fragment = SectionLine {
                page: page.page,
                column: line.column,
                x0: line.bbox.map(|b| b.x0),
                y0: line.bbox.map(|b| b.y0),
                size: line_size(page, line),
                edge,
                text: line.text.trim().to_string(),
            };
            // Justified columns leave gaps wider than the layout pass joins,
            // so one printed row can arrive as several lines: re-join them.
            let same_row = lines.last().is_some_and(|last| last.same_row(&fragment));
            if same_row && let Some(last) = lines.last_mut() {
                last.absorb(&fragment);
            } else {
                lines.push(fragment);
            }
        }
    }
    // Texts that repeat on several pages near the page edge are running
    // headers or footers (`Page 30 of 35` counts as repeating).
    let mut pages_per_text: BTreeMap<String, Vec<u32>> = BTreeMap::new();
    for line in &lines {
        let seen = pages_per_text.entry(digit_key(&line.text)).or_default();
        if !seen.contains(&line.page) {
            seen.push(line.page);
        }
    }
    let repeated: Vec<String> = pages_per_text
        .iter()
        .filter(|(_, seen)| seen.len() >= 2)
        .map(|(text, _)| text.clone())
        .collect();
    lines.retain(|line| {
        !(line.text.is_empty()
            || page_number_re().is_match(&line.text)
            || (line.edge && repeated.contains(&digit_key(&line.text))))
    });
    lines
}

/// Printed number and label of a numbered entry start, per style.
fn numbered_label(style: Style, text: &str) -> Option<(u32, String)> {
    let re = match style {
        Style::Bracket => bracket_label_re(),
        Style::Dot => dot_label_re(),
        Style::Paren => paren_label_re(),
        Style::AuthorYear => return None,
    };
    let caps = re.captures(text)?;
    let number: u32 = caps.get(1)?.as_str().parse().ok()?;
    let label = match style {
        Style::Bracket => format!("[{number}]"),
        Style::Dot => format!("{number}."),
        Style::Paren => format!("{number})"),
        Style::AuthorYear => return None,
    };
    Some((number, label))
}

/// Numbering style from the first three lines (the first line may be a
/// stray fragment or a column artefact).
fn detect_style(lines: &[SectionLine]) -> Style {
    for line in lines.iter().take(3) {
        if bracket_label_re().is_match(&line.text) {
            return Style::Bracket;
        }
        if dot_label_re().is_match(&line.text) {
            return Style::Dot;
        }
        if paren_label_re().is_match(&line.text) {
            return Style::Paren;
        }
    }
    Style::AuthorYear
}

fn median_size(lines: &[SectionLine]) -> Option<f32> {
    let mut sizes: Vec<f32> = lines.iter().filter_map(|l| l.size).collect();
    if sizes.is_empty() {
        return None;
    }
    sizes.sort_by(f32::total_cmp);
    Some(sizes[sizes.len() / 2])
}

/// A heading or block that ends the reference list: `Appendix`,
/// `Supplementary`, a table caption or a row of numbers, the first line of
/// an author biography, or, when sizes are known, a short line set clearly
/// larger than the body.
fn is_end_heading(line: &SectionLine, style: Style, median: Option<f32>) -> bool {
    let short = line.text.chars().count() <= 80;
    if short && (end_heading_re().is_match(&line.text) || caption_re().is_match(&line.text)) {
        return true;
    }
    if numbered_label(style, &line.text).is_some() {
        return false;
    }
    if numeric_row_re().is_match(&line.text) || biography_re().is_match(&line.text) {
        return true;
    }
    let (Some(size), Some(typical)) = (line.size, median) else {
        return false;
    };
    short && size >= typical * 1.15 && line.text.chars().next().is_some_and(char::is_uppercase)
}

/// Split the reference section into entries. `raw`, `label`, `index` and
/// `page` are filled; call [`parse_entry`] for the parsed fields.
pub fn segment_entries(pages: &[PageText], section: &ReferenceSection) -> Vec<ReferenceEntry> {
    let lines = section_lines(pages, section);
    let style = detect_style(&lines);
    let median = median_size(&lines);
    let context: String = lines
        .iter()
        .map(|l| l.text.as_str())
        .collect::<Vec<&str>>()
        .join("\n");
    if style == Style::AuthorYear {
        segment_author_year(&lines, median, &context)
    } else {
        segment_numbered(&lines, style, median, &context)
    }
}

fn push_entry(entries: &mut Vec<ReferenceEntry>, label: Option<String>, text: &str, page: u32) {
    let index = u32::try_from(entries.len() + 1).unwrap_or(u32::MAX);
    entries.push(ReferenceEntry {
        index,
        label,
        raw: text.to_string(),
        page,
        ..ReferenceEntry::default()
    });
}

/// Alphanumeric run that ends `text` (the first half of a word broken at a
/// line end).
fn word_tail(text: &str) -> &str {
    let start = text
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_alphanumeric())
        .last()
        .map_or(text.len(), |(i, _)| i);
    &text[start..]
}

/// Alphanumeric run that starts `text` (the second half of a broken word).
fn word_head(text: &str) -> &str {
    let end = text
        .char_indices()
        .find(|(_, c)| !c.is_alphanumeric())
        .map_or(text.len(), |(i, _)| i);
    &text[..end]
}

/// How a hyphen at the end of a line is resolved when the next line is
/// joined on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HyphenJoin {
    /// A compound: `multi-` + `task` → `multi-task`.
    Keep,
    /// A word broken for justification: `recon-` + `struction` → `reconstruction`.
    Drop,
    /// Not a word break (`x -` + `y`): join with a space as usual.
    Separate,
}

/// Resolve the hyphen that ends `previous` against the line `next`.
///
/// Without a dictionary the rule is: keep when the continuation is not
/// lowercase; keep when the hyphenated form occurs unbroken elsewhere in
/// `context`; drop when the joined form occurs; else keep for common compound
/// prefixes and heads; else drop.
fn hyphen_break(previous: &str, next: &str, context: &str) -> HyphenJoin {
    let Some(head_text) = previous.strip_suffix('-') else {
        return HyphenJoin::Separate;
    };
    let first = word_tail(head_text);
    let second = word_head(next);
    if first.is_empty() {
        return HyphenJoin::Separate;
    }
    if !next.starts_with(|c: char| c.is_lowercase()) {
        return HyphenJoin::Keep;
    }
    let hyphenated = format!("{first}-{second}");
    if context.contains(&hyphenated) {
        return HyphenJoin::Keep;
    }
    let joined = format!("{first}{second}");
    if context.contains(&joined) {
        return HyphenJoin::Drop;
    }
    let first_lower = first.to_lowercase();
    let second_lower = second.to_lowercase();
    if COMPOUND_PREFIXES.contains(&first_lower.as_str())
        || COMPOUND_HEADS.contains(&second_lower.as_str())
    {
        return HyphenJoin::Keep;
    }
    HyphenJoin::Drop
}

/// Append a continuation line to the last entry, resolving a word broken by
/// a hyphen at the line end (see [`hyphen_break`]).
fn append_continuation(entries: &mut [ReferenceEntry], text: &str, context: &str) {
    let Some(last) = entries.last_mut() else {
        return;
    };
    if last.raw.is_empty() {
        last.raw.push_str(text);
        return;
    }
    let join = if text.is_empty() {
        HyphenJoin::Separate
    } else {
        hyphen_break(&last.raw, text, context)
    };
    match join {
        HyphenJoin::Keep => {}
        HyphenJoin::Drop => {
            last.raw.pop();
        }
        HyphenJoin::Separate => last.raw.push(' '),
    }
    last.raw.push_str(text);
}

fn segment_numbered(
    lines: &[SectionLine],
    style: Style,
    median: Option<f32>,
    context: &str,
) -> Vec<ReferenceEntry> {
    let mut entries: Vec<ReferenceEntry> = Vec::new();
    let mut expected: Option<u32> = None;
    for line in lines {
        if is_end_heading(line, style, median) {
            break;
        }
        if let Some((number, label)) = numbered_label(style, &line.text) {
            let starts = expected.is_none_or(|e| (e..=e + 2).contains(&number));
            if starts {
                push_entry(&mut entries, Some(label), &line.text, line.page);
                expected = Some(number + 1);
                continue;
            }
            // A list that restarts at 1 is a second (supplementary) list.
            if number == 1 && expected.is_some_and(|e| e > 3) {
                break;
            }
        }
        append_continuation(&mut entries, &line.text, context);
    }
    entries
}

/// One cluster of line starts at (nearly) the same x position.
struct XLevel {
    x: f32,
    last: f32,
    count: usize,
}

/// Hanging-indent evidence for every line, from the x positions of the
/// whole section rather than the neighbouring line (which may be a row
/// fragment sitting anywhere in the column).
///
/// Line starts are clustered by x; a cluster is an entry-start level when it
/// is populated and no populated cluster lies within [`MAX_HANGING_INDENT`]
/// to its left; the populated clusters within that distance to the right of
/// a start level are the continuation levels. `Some(true)` marks lines at a
/// start level, `Some(false)` every other line with a position, `None` all
/// lines when the section shows no hanging indent at all.
fn layout_starts(lines: &[SectionLine]) -> Vec<Option<bool>> {
    let mut order: Vec<(f32, usize)> = lines
        .iter()
        .enumerate()
        .filter_map(|(i, l)| l.x0.map(|x| (x, i)))
        .collect();
    order.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut cluster_of: Vec<Option<usize>> = vec![None; lines.len()];
    let mut clusters: Vec<XLevel> = Vec::new();
    for &(x, i) in &order {
        let near = clusters
            .last()
            .is_some_and(|level| x - level.last <= INDENT_TOLERANCE);
        if near && let Some(level) = clusters.last_mut() {
            level.last = x;
            level.count += 1;
        } else {
            clusters.push(XLevel {
                x,
                last: x,
                count: 1,
            });
        }
        cluster_of[i] = Some(clusters.len() - 1);
    }
    let threshold = (order.len() / 20).max(2);
    let populated = |level: &XLevel| level.count >= threshold;
    let is_start: Vec<bool> = clusters
        .iter()
        .map(|level| {
            populated(level)
                && !clusters.iter().any(|other| {
                    populated(other)
                        && level.x - other.x > 0.0
                        && level.x - other.x <= MAX_HANGING_INDENT
                })
        })
        .collect();
    let indented: usize = clusters
        .iter()
        .enumerate()
        .filter(|&(c, level)| {
            !is_start[c]
                && populated(level)
                && clusters.iter().enumerate().any(|(other, start)| {
                    is_start[other]
                        && level.x - start.x > 0.0
                        && level.x - start.x <= MAX_HANGING_INDENT
                })
        })
        .map(|(_, level)| level.count)
        .sum();
    if order.is_empty() || (indented as f32) < MIN_INDENTED_SHARE * (order.len() as f32) {
        return vec![None; lines.len()];
    }
    cluster_of
        .iter()
        .map(|cluster| cluster.map(|c| is_start[c]))
        .collect()
}

fn ends_like_entry(text: &str) -> bool {
    text.trim_end()
        .chars()
        .next_back()
        .is_some_and(|c| matches!(c, '.' | ')' | ']' | '}') || c.is_ascii_digit())
}

fn author_year_label(raw: &str) -> Option<String> {
    let surname = surname_re().captures(raw)?.get(1)?.as_str();
    let surname: String = surname.split_whitespace().collect::<Vec<&str>>().join(" ");
    let year = year_paren_re()
        .captures(raw)
        .or_else(|| year_bare_re().captures(raw))
        .and_then(|caps| caps.get(1))
        .map_or_else(String::new, |m| m.as_str().to_string());
    Some(format!("{surname}{year}"))
}

/// Does `raw` carry the evidence every reference has: a year, a DOI, an
/// `arXiv` id or a URL?
fn has_reference_evidence(raw: &str) -> bool {
    year_paren_re().is_match(raw)
        || year_bare_re().is_match(raw)
        || doi_start_re().is_match(raw)
        || arxiv_re().is_match(raw)
        || url_re().is_match(raw)
}

/// Fold entries without any reference evidence into the entry before them
/// (a false start such as `Series A, containing papers ...`), and drop the
/// run of such entries at the end of the list (table rows, appendix text).
/// A list where no entry has evidence is left alone.
fn apply_evidence_guard(entries: Vec<ReferenceEntry>, context: &str) -> Vec<ReferenceEntry> {
    let evidence: Vec<bool> = entries
        .iter()
        .map(|e| has_reference_evidence(&e.raw))
        .collect();
    let Some(last_ok) = evidence.iter().rposition(|&ok| ok) else {
        return entries;
    };
    let mut out: Vec<ReferenceEntry> = Vec::new();
    for (entry, ok) in entries.into_iter().zip(evidence).take(last_ok + 1) {
        if ok || out.is_empty() {
            let index = u32::try_from(out.len() + 1).unwrap_or(u32::MAX);
            out.push(ReferenceEntry { index, ..entry });
        } else {
            append_continuation(&mut out, &entry.raw, context);
        }
    }
    out
}

fn segment_author_year(
    lines: &[SectionLine],
    median: Option<f32>,
    context: &str,
) -> Vec<ReferenceEntry> {
    let layout = layout_starts(lines);
    let mut entries: Vec<ReferenceEntry> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if is_end_heading(line, Style::AuthorYear, median) {
            break;
        }
        let can_start = entry_start_re().is_match(&line.text);
        let starts = if entries.is_empty() {
            true
        } else {
            match layout[i] {
                Some(true) => can_start,
                Some(false) => false,
                None => {
                    can_start
                        && author_start_re().is_match(&line.text)
                        && entries.last().is_some_and(|e| ends_like_entry(&e.raw))
                }
            }
        };
        if starts {
            push_entry(&mut entries, None, &line.text, line.page);
        } else {
            append_continuation(&mut entries, &line.text, context);
        }
    }
    let mut entries = apply_evidence_guard(entries, context);
    for entry in &mut entries {
        entry.label = author_year_label(&entry.raw);
    }
    entries
}

/// Replace the bytes of every range with spaces (byte length preserved, so
/// offsets into the result are valid offsets into the original).
fn mask_ranges(text: &str, ranges: &[Range<usize>]) -> String {
    let mut out = String::with_capacity(text.len());
    for (byte, ch) in text.char_indices() {
        if ranges.iter().any(|r| r.contains(&byte)) {
            for _ in 0..ch.len_utf8() {
                out.push(' ');
            }
        } else {
            out.push(ch);
        }
    }
    out
}

fn trim_trailing_punct(text: &str) -> &str {
    text.trim_end_matches(['.', ',', ';', ')', ']', ':', '}', '\''])
}

/// Can `token` be the rest of a DOI or URL that a line wrap split off
/// (`6040.`, `BF01504345.`, `forum?id=abc`)? A bare year, a word, or
/// anything that starts a new URL or parenthesis cannot.
fn wrapped_id_token(token: &str, allow_no_digit: bool) -> bool {
    let core = token.trim_end_matches(['.', ',', ';', ')']);
    if core.is_empty() || core.chars().count() > MAX_WRAP_TOKEN || core.starts_with('(') {
        return false;
    }
    let has_digit = core.chars().any(|c| c.is_ascii_digit());
    let url_shaped = allow_no_digit && core.contains(['/', '.', '=', '_', '-']);
    if !has_digit && !url_shaped {
        return false;
    }
    !year_token_re().is_match(core) && !core.to_ascii_lowercase().starts_with("http")
}

/// End of the identifier that starts at `start` and reaches `end` so far,
/// extended across the single spaces that line wraps leave inside it. A
/// piece is joined when the identifier so far ends in a separator, when the
/// piece starts with a digit, or when `lenient` (a `doi.org/` URL) — and the
/// piece itself looks like identifier text ([`wrapped_id_token`], which
/// accepts digit-free pieces only when `url_shaped` is allowed).
fn extend_across_wraps(
    text: &str,
    start: usize,
    end: usize,
    lenient: bool,
    url_shaped: bool,
) -> usize {
    let mut end = end;
    loop {
        end = text[end..]
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '<' | '>'))
            .map_or(text.len(), |rel| end + rel);
        let Some(rest) = text[end..].strip_prefix(' ') else {
            break;
        };
        let Some(token) = rest.split_whitespace().next() else {
            break;
        };
        if rest.starts_with(' ') {
            break;
        }
        let consumed = &text[start..end];
        if consumed.ends_with([',', ';']) {
            break;
        }
        let joinable = lenient
            || consumed.ends_with(['/', '.', '-', '_', '(', ')', ':', '=', '&', '?'])
            || token.starts_with(|c: char| c.is_ascii_digit());
        if !joinable || !wrapped_id_token(token, url_shaped) {
            break;
        }
        end += 1;
    }
    end
}

/// First DOI with its byte range in `text`. Line wraps inside the DOI
/// (`10.1007/ BF01504345`, `10. 1145/3292500`, `364399 1.3648400` after
/// `doi.org/`) are closed up.
fn find_doi(text: &str) -> Option<(Range<usize>, String)> {
    let found = doi_start_re().find(text)?;
    let start = found.start();
    let lenient = text[..start].to_ascii_lowercase().ends_with("doi.org/");
    let end = extend_across_wraps(text, start, found.end(), lenient, false);
    let joined: String = text[start..end]
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let trimmed = trim_trailing_punct(&joined);
    let suffix_ok = trimmed
        .rsplit('/')
        .next()
        .is_some_and(|s| s.chars().any(char::is_alphanumeric));
    if trimmed.len() < 8 || !suffix_ok {
        return None;
    }
    let tail = joined.len() - trimmed.len();
    Some((start..end - tail, trimmed.to_string()))
}

/// First arXiv identifier (after `arXiv:` or `abs/`) with its byte range.
fn find_arxiv(text: &str) -> Option<(Range<usize>, String)> {
    let caps = arxiv_re().captures(text)?;
    let whole = caps.get(0)?;
    let id = caps.get(1)?;
    Some((whole.range(), id.as_str().to_string()))
}

/// First URL with its byte range, closed up across line wraps
/// (`https://openreview.net/ forum?id=abc`).
fn find_url(text: &str) -> Option<(Range<usize>, String)> {
    let found = url_re().find(text)?;
    let start = found.start();
    let end = extend_across_wraps(text, start, found.end(), false, true);
    let joined: String = text[start..end]
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let trimmed = trim_trailing_punct(&joined);
    let tail = joined.len() - trimmed.len();
    Some((start..end - tail, trimmed.to_string()))
}

/// Year in `text`: a parenthesised `(2020)` first, else the first bare
/// `19xx`/`20xx` not glued to a page range. Returns the byte range of the
/// four digits and the value.
fn find_year(text: &str) -> Option<(Range<usize>, u16)> {
    let caps = year_paren_re()
        .captures(text)
        .or_else(|| year_bare_re().captures(text))?;
    let digits = caps.get(1)?;
    let year: u16 = digits.as_str().parse().ok()?;
    Some((digits.range(), year))
}

/// Byte range and content of the first quoted title `“...”` or `"..."`.
/// The closing quote must match the opening one, so an apostrophe inside
/// `“developers’ conversations”` does not end the title. A short quotation
/// that runs straight on into lowercase text (`"collaborating" with ai`) is
/// part of an unquoted title, not a quoted title.
fn find_quoted(text: &str) -> Option<(Range<usize>, String)> {
    let open = text.find(['“', '"', '„', '‘'])?;
    let open_char = text[open..].chars().next()?;
    let closers: &[char] = match open_char {
        '“' => &['”', '"'],
        '"' => &['"', '”'],
        '„' => &['“', '”', '"'],
        _ => &['’'],
    };
    let inner_start = open + open_char.len_utf8();
    let close_rel = text[inner_start..].find(closers)?;
    let close = inner_start + close_rel;
    let close_len = text[close..].chars().next().map_or(1, char::len_utf8);
    let raw_inner = text[inner_start..close].trim();
    let inner = raw_inner.trim_end_matches([',', '.', ';']).trim();
    if inner.is_empty() {
        return None;
    }
    let unpunctuated = raw_inner == raw_inner.trim_end_matches([',', '.', ';', '?', '!']);
    let after = text[close + close_len..].trim_start();
    if unpunctuated && after.starts_with(|c: char| c.is_lowercase()) {
        return None;
    }
    Some((open..close + close_len, inner.to_string()))
}

/// Alphanumeric run that ends right before byte `end`.
fn word_before(text: &str, end: usize) -> &str {
    let head = &text[..end];
    let start = head
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_alphanumeric())
        .last()
        .map_or(end, |(i, _)| i);
    &head[start..]
}

/// Is the period at byte `dot` the end of an initial or an abbreviation
/// (`A.`, `Jr.`, `St.`, `al.`, `vs.`, `pp.`) rather than a sentence end? `4o.` and
/// `4.3.` are sentence ends: a lone digit is not an initial.
fn period_is_abbreviation(text: &str, dot: usize) -> bool {
    let word = word_before(text, dot);
    let mut chars = word.chars();
    let single_letter =
        matches!((chars.next(), chars.next()), (Some(c), None) if c.is_alphabetic());
    single_letter
        || matches!(
            word.to_ascii_lowercase().as_str(),
            "jr" | "sr"
                | "st"
                | "al"
                | "eds"
                | "ed"
                | "vs"
                | "pp"
                | "vol"
                | "no"
                | "ch"
                | "cf"
                | "fig"
        )
}

/// True when `segment` reads as an author list only: every `. ` inside it
/// closes an initial or abbreviation.
fn is_author_only(segment: &str) -> bool {
    let trimmed = segment.trim_end();
    let trimmed = trimmed.trim_end_matches(['.', ',', '(', ' ']);
    if trimmed.is_empty() || !trimmed.chars().next().is_some_and(char::is_uppercase) {
        return false;
    }
    let mut ok = true;
    let mut search = 0usize;
    while let Some(rel) = trimmed[search..].find(". ") {
        let dot = search + rel;
        if !period_is_abbreviation(trimmed, dot) {
            ok = false;
            break;
        }
        search = dot + 2;
    }
    ok && trimmed.chars().count() <= 400
}

/// Byte offset just past the terminator (`. `, `? `, `! `) that ends the
/// author list when it is followed by the title, honouring initials.
fn author_terminator(body: &str) -> Option<usize> {
    let vancouver = vancouver_start_re().is_match(body);
    let surname_first = surname_first_re().is_match(body);
    let mut search = 0usize;
    loop {
        let rel = body[search..].find(['.', '?', '!'])?;
        let pos = search + rel;
        search = pos + 1;
        if !body[pos + 1..].starts_with(' ') {
            continue;
        }
        let abbreviation = body.as_bytes()[pos] == b'.' && period_is_abbreviation(body, pos);
        if !abbreviation || vancouver {
            return Some(pos + 1);
        }
        // `et al.` ends the list unless more names follow; in surname-first
        // style an initial does too (`Smith, A. Title`).
        let et_al = word_before(body, pos).eq_ignore_ascii_case("al");
        if (et_al || surname_first) && !continues_author_list(body, pos + 1) {
            return Some(pos + 1);
        }
    }
}

/// After an initial such as `B. `, does the text go on with more authors
/// (`and`, `&`, another initial, or a surname followed by a comma) rather
/// than start the title?
fn continues_author_list(text: &str, from: usize) -> bool {
    let rest = text[from..].trim_start();
    let Some(word) = rest.split_whitespace().next() else {
        return false;
    };
    let lower = word.trim_end_matches(',').to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "and" | "&" | "et" | "al" | "al." | "jr" | "jr."
    ) {
        return true;
    }
    if word.ends_with(',') {
        return true;
    }
    let after_word = rest[word.len()..].trim_start();
    if after_word.starts_with(',') || after_word.starts_with('&') {
        return true;
    }
    is_initials(word)
}

/// End (byte offset, exclusive) of a title that starts at byte 0 of `text`:
/// the first `. ` that does not close an abbreviation, or a `? ` / `! `
/// that is not followed by a lowercase continuation (`negotiate? nego-
/// tiationarena platform`).
fn title_end(text: &str) -> usize {
    let mut search = 0usize;
    while let Some(rel) = text[search..].find(['.', '?', '!']) {
        let pos = search + rel;
        if text[pos + 1..].starts_with(' ') {
            let terminal = if text.as_bytes()[pos] == b'.' {
                !period_is_abbreviation(text, pos)
            } else {
                !text[pos + 2..].starts_with(|c: char| c.is_lowercase())
            };
            if terminal {
                return pos;
            }
        }
        search = pos + 1;
    }
    text.len()
}

fn is_et_al(part: &str) -> bool {
    matches!(
        part.trim_end_matches('.').to_ascii_lowercase().as_str(),
        "et al" | "et al." | "others" | "et alii"
    )
}

fn is_name_suffix(part: &str) -> bool {
    matches!(
        part.trim_end_matches('.').to_ascii_lowercase().as_str(),
        "jr" | "sr" | "ii" | "iii" | "iv"
    )
}

fn is_initials(part: &str) -> bool {
    part.chars().count() <= 8 && initials_re().is_match(part)
}

fn looks_like_name(part: &str) -> bool {
    let count = part.chars().count();
    (2..=80).contains(&count)
        && part.chars().any(char::is_alphabetic)
        && !part.chars().any(|c| c.is_ascii_digit())
}

/// Drop a trailing sentence period but keep the period of a final initial.
fn trim_author_period(text: &str) -> &str {
    let trimmed = text.trim_end();
    if let Some(head) = trimmed.strip_suffix('.')
        && !period_is_abbreviation(trimmed, head.len())
    {
        return head.trim_end();
    }
    trimmed
}

/// Split an author segment into names as printed: `A. B. Smith, C. Jones, and
/// D. Lee`, `Smith, A. B., Jones, C.`, `Smith AB, Jones C`, `Smith, John, and
/// Jane Doe`. Initial groups are re-attached to the preceding surname.
fn split_authors(segment: &str) -> Vec<String> {
    let cleaned = segment.trim();
    let cleaned = cleaned.trim_end_matches([',', ';', ':', '(', ' ']);
    // Vancouver style (`Smith AB, Jones C.`) has no initial periods: a trailing
    // period is always the sentence end.
    let cleaned = if vancouver_start_re().is_match(cleaned) {
        cleaned.trim_end_matches('.')
    } else {
        trim_author_period(cleaned)
    };
    let surname_first = surname_first_re().is_match(cleaned);
    let mut names: Vec<String> = Vec::new();
    for part in author_sep_re().split(cleaned) {
        let part = part.trim().trim_matches(',').trim();
        if part.is_empty() || is_et_al(part) {
            continue;
        }
        if (is_initials(part) || is_name_suffix(part))
            && let Some(last) = names.last_mut()
        {
            last.push_str(", ");
            last.push_str(part);
            continue;
        }
        // Chicago style inverts only the first author: `Smith, John, and Jane Doe`.
        if surname_first
            && names.len() == 1
            && !names[0].contains([' ', ','])
            && !part.contains([' ', '.'])
            && part.chars().next().is_some_and(char::is_uppercase)
        {
            names[0].push_str(", ");
            names[0].push_str(part);
            continue;
        }
        if looks_like_name(part) {
            names.push(part.to_string());
        }
    }
    names
}

fn dash_range(first: &str, last: Option<&str>) -> String {
    last.map_or_else(|| first.to_string(), |last| format!("{first}–{last}"))
}

/// Venue text cleaned of surrounding punctuation; `None` when it is not a
/// plausible venue (empty, numeric, an access note, ...).
fn clean_venue(text: &str) -> Option<String> {
    let trimmed = text.trim().trim_matches([',', ';', ':', ' ']);
    let trimmed = if trimmed.matches('.').count() == 1 {
        trimmed.trim_end_matches('.')
    } else {
        trimmed
    };
    let collapsed = trimmed.split_whitespace().collect::<Vec<&str>>().join(" ");
    let lower = collapsed.to_lowercase();
    if collapsed.chars().count() < 2
        || collapsed.chars().count() > 200
        || !collapsed.chars().next().is_some_and(char::is_alphabetic)
        || lower.starts_with("available")
        || lower.starts_with("retrieved")
        || lower.starts_with("accessed")
        || lower.starts_with("online")
        || lower.starts_with("url")
        || lower.starts_with("http")
        || lower.starts_with("arxiv")
        || lower.starts_with("doi")
        || matches!(lower.as_str(), "p" | "pp" | "vol" | "no" | "in")
    {
        return None;
    }
    Some(collapsed)
}

/// Venue from the text that follows the title.
fn parse_venue(rest: &str) -> Option<String> {
    let rest = rest.trim_start_matches(|c: char| c == ',' || c == '.' || c.is_whitespace());
    if rest.is_empty() {
        return None;
    }
    let lower = rest.to_lowercase();
    if !lower.starts_with("in ")
        && !lower.starts_with("in:")
        && !lower.contains("proceedings")
        && let Some(caps) = publisher_re().captures(rest)
        && let Some(publisher) = caps.get(2)
    {
        return clean_venue(publisher.as_str());
    }
    let after_in = if lower.starts_with("in: ") {
        Some(&rest[4..])
    } else if lower.starts_with("in ") {
        Some(&rest[3..])
    } else if lower.starts_with("proceedings") || lower.starts_with("proc.") {
        Some(rest)
    } else {
        None
    };
    if let Some(after) = after_in {
        let caps = in_venue_re().captures(after)?;
        return clean_venue(caps.get(1)?.as_str());
    }
    let caps = journal_venue_re().captures(rest)?;
    clean_venue(caps.get(1)?.as_str())
}

/// Volume, issue and pages from the text that follows the title (with DOI,
/// URL, arXiv id and year already masked).
fn parse_numbers(rest: &str) -> (Option<String>, Option<String>, Option<String>) {
    let mut volume: Option<String> = None;
    let mut issue: Option<String> = None;
    let mut pages: Option<String> = None;
    if let Some(caps) = vol_issue_pages_re().captures(rest) {
        volume = caps.get(1).map(|m| m.as_str().to_string());
        issue = caps.get(2).map(|m| m.as_str().to_string());
        if let Some(first) = caps.get(3) {
            pages = Some(dash_range(first.as_str(), caps.get(4).map(|m| m.as_str())));
        }
        return (volume, issue, pages);
    }
    if let Some(caps) = pages_labelled_re().captures(rest)
        && let Some(first) = caps.get(1)
    {
        pages = Some(dash_range(first.as_str(), caps.get(2).map(|m| m.as_str())));
    }
    if let Some(caps) = vol_labelled_re().captures(rest) {
        volume = caps.get(1).map(|m| m.as_str().to_string());
    }
    if let Some(caps) = issue_labelled_re().captures(rest) {
        issue = caps.get(1).map(|m| m.as_str().to_string());
    }
    if volume.is_none()
        && let Some(caps) = vol_colon_pages_re().captures(rest)
    {
        volume = caps.get(1).map(|m| m.as_str().to_string());
        if pages.is_none()
            && let (Some(a), Some(b)) = (caps.get(2), caps.get(3))
        {
            pages = Some(dash_range(a.as_str(), Some(b.as_str())));
        }
    }
    if volume.is_none()
        && let Some(caps) = vol_comma_pages_re().captures(rest)
    {
        volume = caps.get(1).map(|m| m.as_str().to_string());
        if pages.is_none()
            && let (Some(a), Some(b)) = (caps.get(2), caps.get(3))
        {
            pages = Some(dash_range(a.as_str(), Some(b.as_str())));
        }
    }
    if volume.is_none()
        && let Some(caps) = vol_issue_re().captures(rest)
    {
        volume = caps.get(1).map(|m| m.as_str().to_string());
        if issue.is_none() {
            issue = caps.get(2).map(|m| m.as_str().to_string());
        }
    }
    // Elsevier / SIAM: `35 (1992) 61–70`, `20 (2) (1963) 130–141`,
    // `22 (2022), pp. 35–76` — the year digits are already blanked.
    if volume.is_none()
        && let Some(caps) = vol_before_year_re().captures(rest)
    {
        volume = caps.get(1).map(|m| m.as_str().to_string());
        if issue.is_none() {
            issue = caps.get(2).map(|m| m.as_str().to_string());
        }
        if pages.is_none()
            && let (Some(a), Some(b)) = (caps.get(3), caps.get(4))
        {
            pages = Some(dash_range(a.as_str(), Some(b.as_str())));
        }
    }
    if pages.is_none()
        && let Some(caps) = dash_range_re().captures(rest)
        && let (Some(a), Some(b)) = (caps.get(1), caps.get(2))
    {
        let lo: u64 = a.as_str().parse().unwrap_or(0);
        let hi: u64 = b.as_str().parse().unwrap_or(0);
        if lo < hi {
            pages = Some(dash_range(a.as_str(), Some(b.as_str())));
        }
    }
    (volume, issue, pages)
}

/// Every year in `text` (parenthesised or bare) with the byte range of its
/// digits, in order of position.
fn all_years(text: &str) -> Vec<(Range<usize>, u16)> {
    let mut years: Vec<(Range<usize>, u16)> = Vec::new();
    for caps in year_paren_re()
        .captures_iter(text)
        .chain(year_bare_re().captures_iter(text))
    {
        if let Some(digits) = caps.get(1)
            && let Ok(value) = digits.as_str().parse::<u16>()
        {
            years.push((digits.range(), value));
        }
    }
    years.sort_by_key(|(range, _)| range.start);
    years.dedup_by_key(|(range, _)| range.start);
    years
}

fn is_particle(token: &str) -> bool {
    matches!(
        token,
        "van" | "von" | "de" | "der" | "den" | "del" | "di" | "da" | "la" | "le" | "du" | "y"
    )
}

/// `x- y` (a word broken at a line end) closed up, for classifying a
/// fragment; the text itself is left as printed.
fn close_word_breaks(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find("- ") {
        let before_is_letter = rest[..pos]
            .chars()
            .next_back()
            .is_some_and(char::is_alphabetic);
        let after_is_lower = rest[pos + 2..].starts_with(|c: char| c.is_lowercase());
        out.push_str(&rest[..pos]);
        if !(before_is_letter && after_is_lower) {
            out.push_str("- ");
        }
        rest = &rest[pos + 2..];
    }
    out.push_str(rest);
    out
}

/// Does a comma-delimited part read as one or two author names in an
/// initials-first style: `A. B. Smith`, `and C. Jones`, `Smith AB`,
/// `J. Doe and K. Roe`, `et al.`? `Nonlinear Systems` and `Cambridge
/// University Press` do not (no initials, not Vancouver).
fn is_name_part(part: &str) -> bool {
    let mut text = part.trim();
    if let Some(rest) = text
        .strip_prefix("and ")
        .or_else(|| text.strip_prefix("& "))
    {
        text = rest.trim_start();
    }
    if text.is_empty() {
        return false;
    }
    if is_et_al(text) || is_name_suffix(text) {
        return true;
    }
    if let Some(tail) = et_al_tail_re().find(text) {
        text = text[..tail.start()].trim_end();
    }
    let subs: Vec<&str> = and_split_re().split(text).collect();
    if subs.len() > 1 {
        return subs.len() <= 2 && subs.iter().all(|sub| is_name_part(sub));
    }
    let closed = close_word_breaks(text);
    let tokens: Vec<&str> = closed.split_whitespace().collect();
    if tokens.is_empty() || tokens.len() > 6 {
        return false;
    }
    let mut has_initial = false;
    let mut names = 0usize;
    for token in &tokens {
        if initial_token_re().is_match(token) {
            has_initial = true;
        } else if is_particle(token) || cap_word_re().is_match(token) {
            names += 1;
        } else {
            return false;
        }
    }
    if names == 0 {
        return false;
    }
    let vancouver = tokens.len() == 2
        && caps_block_re().is_match(tokens[1])
        && !caps_block_re().is_match(tokens[0]);
    has_initial || vancouver
}

/// Does the first comma-delimited part open an initials-first author list
/// (`A. Smith`, `A. B. Smith and C. Jones`, `Smith AB`)?
fn initials_first(part: &str) -> bool {
    let tokens: Vec<&str> = part.split_whitespace().collect();
    match tokens.as_slice() {
        [first, ..] if initial_token_re().is_match(first) => true,
        [surname, initials] => {
            cap_word_re().is_match(surname)
                && caps_block_re().is_match(initials)
                && !caps_block_re().is_match(surname)
        }
        _ => false,
    }
}

/// Does a comma-delimited part that follows the title open the venue
/// (`Communications of the ACM 35`, `in: Proceedings`, `pp. 3–9`, `2020`)?
/// A part starting in lowercase is title text unless it is a venue word.
fn venue_like(part: &str) -> bool {
    let text = part.trim_start();
    let Some(first) = text.chars().next() else {
        return true;
    };
    first.is_uppercase()
        || first.is_ascii_digit()
        || matches!(first, '“' | '"' | '„' | '‘' | '(' | '[')
        || venue_lead_re().is_match(text)
}

/// Byte ranges of the `, `-delimited parts of `text`.
fn comma_parts(text: &str) -> Vec<Range<usize>> {
    let mut parts: Vec<Range<usize>> = Vec::new();
    let mut start = 0usize;
    for (pos, _) in text.match_indices(", ") {
        parts.push(start..pos);
        start = pos + 2;
    }
    parts.push(start..text.len());
    parts
}

/// Where a comma-delimited entry splits.
struct CommaSplit {
    /// End of the author list (the comma before the title).
    authors_end: usize,
    /// First byte of the title.
    title_start: usize,
    /// End of the title (exclusive).
    title_end: usize,
}

/// Elsevier, SIAM and IEEE-book entries put an unquoted title after the
/// comma that closes an initials-first author list:
/// `D. Goldberg, D. Nichols, Using collaborative filtering ..., Communications
/// of the ACM 35 (1992) 61–70.` The authors are the leading name-like parts;
/// the title runs to the first sentence end or the comma before a venue-like
/// part. `None` when the entry is not of this shape (a quoted title, a
/// sentence period after the authors, a year right after them).
fn comma_style(masked: &str) -> Option<CommaSplit> {
    let parts = comma_parts(masked);
    if parts.len() < 2 {
        return None;
    }
    let first = &masked[parts[0].clone()];
    if !initials_first(first) || !is_name_part(first) || title_end(first) < first.len() {
        return None;
    }
    let mut k = 1usize;
    while k < parts.len() {
        let part = &masked[parts[k].clone()];
        if title_end(part) < part.len() || !is_name_part(part) {
            break;
        }
        k += 1;
    }
    if k >= parts.len() {
        return None;
    }
    let range = parts[k].clone();
    let part = &masked[range.clone()];
    let stripped = part.trim_start();
    let first_char = stripped.chars().next()?;
    if matches!(first_char, '“' | '"' | '„' | '‘') {
        return None;
    }
    let lower = stripped.to_lowercase();
    if lower.starts_with("and ") || lower.starts_with("& ") || year_lead_re().is_match(stripped) {
        return None;
    }
    // A name followed by a period (`Jones C. Title`, `A. B. Smith. Title`)
    // is the last author of a period-delimited entry, not a title.
    let stop = title_end(part);
    let mut search = 0usize;
    while let Some(rel) = part[search..].find(". ") {
        let pos = search + rel;
        if pos > stop {
            break;
        }
        let prefix = part[..pos].trim();
        if is_name_part(prefix) && !is_et_al(prefix) {
            return None;
        }
        search = pos + 2;
    }
    if !first_char.is_uppercase() && venue_lead_re().is_match(stripped) {
        return None;
    }
    // A bare name (capitalised words only) followed by another author part
    // is a full name, not a title: `M. Nori, Sangseok Yun, and Il Kim.`
    let tokens: Vec<&str> = stripped.split_whitespace().collect();
    if (1..=3).contains(&tokens.len()) && tokens.iter().all(|t| cap_word_re().is_match(t)) {
        let next = parts
            .get(k + 1)
            .map_or("", |r| masked[r.clone()].trim_start());
        let next_lower = next.to_lowercase();
        if next_lower.starts_with("and ")
            || next_lower.starts_with("& ")
            || is_name_part(&next[..title_end(next)])
        {
            return None;
        }
    }
    // `et al.` opening the part belongs to the authors.
    let mut title_start = range.start + (part.len() - stripped.len());
    if let Some(lead) = et_al_lead_re().find(stripped) {
        title_start += lead.end();
    }
    let mut end = title_start + title_end(&masked[title_start..]);
    if end <= title_start {
        return None;
    }
    for later in parts.iter().skip(k + 1) {
        let comma = later.start.saturating_sub(2);
        if comma >= end {
            break;
        }
        if venue_like(&masked[later.clone()]) {
            end = comma;
            break;
        }
    }
    Some(CommaSplit {
        authors_end: range.start,
        title_start,
        title_end: end,
    })
}

/// `title` without a trailing comma and without the quotes that enclose the
/// whole of it (`“Title”.` in biblatex); quotes inside stay.
fn strip_wrapping_quotes(title: &str) -> &str {
    let trimmed = title.trim().trim_end_matches(',').trim();
    let opens = trimmed.starts_with(['“', '"', '„', '‘']);
    let closes = trimmed.ends_with(['”', '"', '’']);
    if opens && closes && trimmed.chars().count() > 2 {
        return trimmed
            .trim_start_matches(['“', '"', '„', '‘'])
            .trim_end_matches(['”', '"', '’'])
            .trim();
    }
    trimmed
}

/// Body of the entry without the printed label.
fn strip_label(entry: &ReferenceEntry) -> &str {
    let raw = entry.raw.trim();
    if let Some(label) = entry.label.as_deref()
        && numbered_label_re().is_match(label)
        && let Some(rest) = raw.strip_prefix(label)
    {
        return rest.trim_start();
    }
    raw
}

/// Fill the parsed fields of `entry` from its `raw` text.
///
/// Authors end before a parenthesised year, before a quoted title, or at the
/// first sentence period that does not close an initial; the title is the
/// segment that follows, up to the next `. `; venue, volume/issue/pages, DOI,
/// arXiv id and URL are read from the remainder. Fields without evidence stay
/// `None`.
pub fn parse_entry(entry: &mut ReferenceEntry) {
    let body: String = strip_label(entry).to_string();
    if body.is_empty() {
        return;
    }
    let mut masked_ranges: Vec<Range<usize>> = Vec::new();
    if let Some((range, doi)) = find_doi(&body) {
        entry.doi = Some(doi);
        masked_ranges.push(range);
    }
    if let Some((range, id)) = find_arxiv(&body) {
        entry.arxiv_id = Some(id);
        masked_ranges.push(range);
    }
    if let Some((range, url)) = find_url(&body) {
        entry.url = Some(url);
        masked_ranges.push(range);
    }
    let masked = mask_ranges(&body, &masked_ranges);

    let year = find_year(&masked);
    if let Some((_, value)) = &year {
        entry.year = Some(*value);
    }
    let quoted = find_quoted(&masked);

    // Where the author list ends and where the title starts.
    let mut authors_end: Option<usize> = None;
    let mut title_start: usize = 0;
    let mut title_limit: Option<usize> = None;
    let mut quoted_title: Option<(Range<usize>, String)> = None;
    if let Some(split) = comma_style(&masked) {
        authors_end = Some(split.authors_end);
        title_start = split.title_start;
        title_limit = Some(split.title_end);
    } else {
        // The first year (in position) preceded by an author list only:
        // `Smith, A. (2020). Title`, `Aji and Heafield. 2017. Title`.
        for (range, _) in all_years(&masked) {
            if quoted.as_ref().is_some_and(|(q, _)| range.start > q.start) {
                break;
            }
            if !is_author_only(&masked[..range.start]) {
                continue;
            }
            authors_end = Some(range.start);
            // Skip a year suffix (`2020a`) and the punctuation closing the year.
            let mut after = &masked[range.end..];
            if after.starts_with(|c: char| c.is_ascii_lowercase()) {
                after = &after[1..];
            }
            after = after.trim_start_matches([')', '.', ',', ':', ' ']);
            title_start = masked.len() - after.len();
            if after.starts_with(['“', '"', '„', '‘'])
                && let Some((q, text)) = find_quoted(after)
            {
                quoted_title = Some((title_start + q.start..title_start + q.end, text));
            }
            break;
        }
        if authors_end.is_none()
            && let Some((range, text)) = &quoted
            && is_author_only(&masked[..range.start])
        {
            authors_end = Some(range.start);
            quoted_title = Some((range.clone(), text.clone()));
            title_start = range.end;
        }
        if authors_end.is_none()
            && let Some(end) = author_terminator(&masked)
        {
            authors_end = Some(end);
            title_start = end;
            // ACM style puts the year as its own sentence: `Authors. 2019. Title.`
            if let Some(lead) = leading_year_re().find(&masked[end..]) {
                title_start = end + lead.end();
            }
        }
    }

    let Some(end) = authors_end else {
        // No author/title structure: only the numeric evidence is safe to read.
        let (volume, issue, pages) = parse_numbers(&mask_year(&masked, year.as_ref()));
        entry.volume = volume;
        entry.issue = issue;
        entry.pages = pages;
        return;
    };
    let author_segment = &body[..end];
    if author_segment
        .chars()
        .next()
        .is_some_and(char::is_uppercase)
    {
        entry.authors = split_authors(author_segment);
    }

    let rest_start: usize = if let Some((range, text)) = quoted_title {
        entry.title = Some(text);
        range.end
    } else {
        let title_masked = &masked[title_start..];
        let mut stop = title_end(title_masked);
        if let Some(limit) = title_limit {
            stop = stop.min(limit.saturating_sub(title_start));
        } else if let Some(venue) = comma_venue_re().find(title_masked) {
            stop = stop.min(venue.start());
        }
        let title = strip_wrapping_quotes(body[title_start..title_start + stop].trim());
        if !title.is_empty() && title.chars().count() <= 500 {
            entry.title = Some(title.to_string());
        }
        (title_start + stop + 1).min(body.len())
    };
    let rest_masked = &masked[rest_start.min(masked.len())..];
    entry.venue = parse_venue(rest_masked);
    let year_in_rest = year
        .as_ref()
        .filter(|(range, _)| range.start >= rest_start)
        .map(|(range, value)| (range.start - rest_start..range.end - rest_start, *value));
    let (volume, issue, pages) = parse_numbers(&mask_year(rest_masked, year_in_rest.as_ref()));
    entry.volume = volume;
    entry.issue = issue;
    entry.pages = pages;
}

/// `text` with the year digits blanked so they are not read as a volume.
fn mask_year(text: &str, year: Option<&(Range<usize>, u16)>) -> String {
    if let Some((range, _)) = year
        && range.end <= text.len()
    {
        mask_ranges(text, std::slice::from_ref(range))
    } else {
        text.to_string()
    }
}

/// Lookup tables for resolving markers to `ReferenceEntry::index`.
struct RefIndex {
    /// The list is numbered (`[n]`, `n.`, `n)`); markers are numeric.
    numbered: bool,
    /// Printed number -> entry index.
    by_number: BTreeMap<u32, u32>,
    /// (first-author surname, lower case; year; entry index).
    by_author_year: Vec<(String, u16, u32)>,
}

/// Surname of a printed author name: the part before a comma, else the last
/// token, else the first token when the last one is a block of initials.
fn author_surname(name: &str) -> String {
    if let Some((before, _)) = name.split_once(',') {
        return before.trim().to_lowercase();
    }
    let tokens: Vec<&str> = name
        .split_whitespace()
        .filter(|t| !is_name_suffix(t))
        .collect();
    let Some(last) = tokens.last() else {
        return String::new();
    };
    let last_is_initials =
        last.chars().count() <= 3 && last.chars().all(|c| c.is_uppercase() || c == '.');
    let pick = if last_is_initials {
        tokens.first().copied().unwrap_or("")
    } else {
        last
    };
    pick.trim_matches('.').to_lowercase()
}

impl RefIndex {
    fn build(refs: &[ReferenceEntry]) -> Self {
        let mut by_number: BTreeMap<u32, u32> = BTreeMap::new();
        let mut by_author_year: Vec<(String, u16, u32)> = Vec::new();
        for entry in refs {
            if let Some(label) = entry.label.as_deref()
                && let Some(caps) = numbered_label_re().captures(label)
                && let Some(number) = caps.get(1).and_then(|m| m.as_str().parse::<u32>().ok())
            {
                by_number.entry(number).or_insert(entry.index);
            }
            let surname = entry
                .authors
                .first()
                .map(|name| author_surname(name.as_str()))
                .or_else(|| {
                    surname_re()
                        .captures(&entry.raw)
                        .and_then(|caps| caps.get(1))
                        .map(|m| m.as_str().to_lowercase())
                });
            if let Some(surname) = surname
                && let Some(year) = entry.year
                && !surname.is_empty()
            {
                by_author_year.push((surname, year, entry.index));
            }
        }
        Self {
            numbered: !by_number.is_empty(),
            by_number,
            by_author_year,
        }
    }

    /// Entries whose first author surname and year match. A suffix letter
    /// (`2020b`) picks the n-th of several same-year entries.
    fn resolve_author_year(&self, surname: &str, year: u16, suffix: &str) -> Vec<u32> {
        let needle = surname.to_lowercase();
        let tail = format!(" {needle}");
        let mut candidates: Vec<u32> = self
            .by_author_year
            .iter()
            .filter(|(s, y, _)| *y == year && (*s == needle || s.ends_with(&tail)))
            .map(|(_, _, idx)| *idx)
            .collect();
        candidates.sort_unstable();
        candidates.dedup();
        if candidates.len() > 1
            && let Some(letter) = suffix.chars().next()
        {
            let pos = u32::from(letter).saturating_sub(u32::from('a'));
            let pos = usize::try_from(pos).unwrap_or(0);
            if let Some(&idx) = candidates.get(pos) {
                return vec![idx];
            }
        }
        candidates
    }
}

/// Surname to look up for a marker name such as `Smith et al.` or `Lee and Kim`.
fn marker_surname(name: &str) -> &str {
    name.split_whitespace().next().unwrap_or("")
}

/// Byte offset in `page.text` where the heading line `first_line` starts;
/// the whole text when the line cannot be located.
fn heading_byte_offset(page: &PageText, first_line: usize) -> usize {
    let mut cursor = 0usize;
    for (i, line) in page.lines.iter().enumerate().take(first_line + 1) {
        let needle = line.text.trim();
        if needle.is_empty() {
            continue;
        }
        if let Some(rel) = page.text.get(cursor..).and_then(|rest| rest.find(needle)) {
            let start = cursor + rel;
            if i == first_line {
                return start;
            }
            cursor = start + needle.len();
        } else if i == first_line {
            return page.text.len();
        }
    }
    page.text.len()
}

type Found = (Range<usize>, String, Vec<u32>);

fn numeric_markers(text: &str, index: &RefIndex) -> Vec<Found> {
    let mut out: Vec<Found> = Vec::new();
    for found in numeric_marker_re().find_iter(text) {
        let inner = &text[found.start() + 1..found.end() - 1];
        let mut targets: Vec<u32> = Vec::new();
        for item in inner.split([',', ';']) {
            let Some(caps) = numeric_item_re().captures(item) else {
                continue;
            };
            let Some(lo) = caps.get(1).and_then(|m| m.as_str().parse::<u32>().ok()) else {
                continue;
            };
            let hi = caps
                .get(2)
                .and_then(|m| m.as_str().parse::<u32>().ok())
                .unwrap_or(lo);
            if hi < lo || hi - lo > MAX_RANGE_SPAN {
                continue;
            }
            for number in lo..=hi {
                if let Some(&idx) = index.by_number.get(&number)
                    && !targets.contains(&idx)
                {
                    targets.push(idx);
                }
            }
        }
        if targets.is_empty() {
            continue;
        }
        out.push((found.range(), found.as_str().to_string(), targets));
    }
    out
}

fn author_year_markers(text: &str, index: &RefIndex) -> Vec<Found> {
    let mut out: Vec<Found> = Vec::new();
    for caps in narrative_marker_re().captures_iter(text) {
        let (Some(whole), Some(name), Some(year)) = (caps.get(0), caps.get(1), caps.get(2)) else {
            continue;
        };
        let Ok(year_value) = year.as_str().parse::<u16>() else {
            continue;
        };
        let suffix = caps.get(3).map_or("", |m| m.as_str());
        let targets = index.resolve_author_year(marker_surname(name.as_str()), year_value, suffix);
        if targets.is_empty() {
            continue;
        }
        out.push((whole.range(), whole.as_str().to_string(), targets));
    }
    for found in parenthetical_re().find_iter(text) {
        let overlaps = out
            .iter()
            .any(|(range, _, _)| range.start < found.end() && found.start() < range.end);
        if overlaps {
            continue;
        }
        let inner = &text[found.start() + 1..found.end() - 1];
        let mut targets: Vec<u32> = Vec::new();
        let mut clauses = 0usize;
        for clause in inner.split(';') {
            let Some(caps) = clause_re().captures(clause) else {
                continue;
            };
            let (Some(name), Some(year)) = (caps.get(1), caps.get(2)) else {
                continue;
            };
            let Ok(year_value) = year.as_str().parse::<u16>() else {
                continue;
            };
            clauses += 1;
            let suffix = caps.get(3).map_or("", |m| m.as_str());
            let surname = marker_surname(name.as_str());
            for idx in index.resolve_author_year(surname, year_value, suffix) {
                if !targets.contains(&idx) {
                    targets.push(idx);
                }
            }
        }
        if clauses == 0 {
            continue;
        }
        out.push((found.range(), found.as_str().to_string(), targets));
    }
    out
}

/// In-text citation markers on the body pages, resolved against `refs`.
///
/// Numeric lists get `[1]`, `[2, 3]`, `[4–6]` markers (superscript digits are
/// not attempted); author-year lists get `(Smith, 2020)`,
/// `(Smith et al., 2020; Lee and Kim, 2019)` and `Smith (2020)`. Only the
/// pages before the reference section, plus the part of the section's first
/// page above the heading, are searched. `offset` is a char offset into
/// `PageText::text`.
pub fn find_citation_markers(pages: &[PageText], refs: &[ReferenceEntry]) -> Vec<CitationMarker> {
    if refs.is_empty() {
        return Vec::new();
    }
    let index = RefIndex::build(refs);
    let section = find_reference_section(pages);
    let mut markers: Vec<CitationMarker> = Vec::new();
    for page in pages {
        let scan_len = match &section {
            Some(s) if page.page > s.first_page => continue,
            Some(s) if page.page == s.first_page => heading_byte_offset(page, s.first_line),
            _ => page.text.len(),
        };
        let text = &page.text[..scan_len];
        let mut found = if index.numbered {
            numeric_markers(text, &index)
        } else {
            author_year_markers(text, &index)
        };
        found.sort_by_key(|(range, _, _)| range.start);
        let mut byte_cursor = 0usize;
        let mut char_cursor = 0usize;
        for (range, marker_text, targets) in found {
            if range.start < byte_cursor {
                continue;
            }
            char_cursor += page.text[byte_cursor..range.start].chars().count();
            byte_cursor = range.start;
            markers.push(CitationMarker {
                page: page.page,
                offset: u32::try_from(char_cursor).unwrap_or(u32::MAX),
                text: marker_text,
                targets,
            });
        }
    }
    markers
}

/// Convenience: find the section, segment and parse every entry, then find
/// the markers. No reference section gives two empty vectors.
pub fn extract_citations(pages: &[PageText]) -> (Vec<ReferenceEntry>, Vec<CitationMarker>) {
    let Some(section) = find_reference_section(pages) else {
        return (Vec::new(), Vec::new());
    };
    let mut refs = segment_entries(pages, &section);
    for entry in &mut refs {
        parse_entry(entry);
    }
    let markers = find_citation_markers(pages, &refs);
    (refs, markers)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::BBox;

    /// A line at `x0` in `column`, `y` points up the page, 10 pt tall.
    fn line_at(text: &str, column: u32, x0: f32, y: f32) -> Line {
        let width = text.chars().count() as f32 * 5.0;
        Line {
            text: text.to_string(),
            bbox: Some(BBox {
                x0,
                y0: y,
                x1: x0 + width,
                y1: y + 10.0,
            }),
            column,
            spans: Vec::new(),
        }
    }

    /// Lines without layout evidence.
    fn bare_line(text: &str) -> Line {
        Line {
            text: text.to_string(),
            bbox: None,
            column: 0,
            spans: Vec::new(),
        }
    }

    /// A page whose `text` is its lines joined by newlines.
    fn page_of(number: u32, lines: Vec<Line>) -> PageText {
        let mut page = PageText::new(number, 612.0, 792.0, 0);
        page.text = lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<&str>>()
            .join("\n");
        page.lines = lines;
        page
    }

    /// Column-0 lines at x0 = 72, laid out top to bottom, 14 pt apart.
    fn column_page(number: u32, texts: &[&str]) -> PageText {
        let lines: Vec<Line> = texts
            .iter()
            .enumerate()
            .map(|(i, t)| line_at(t, 0, 72.0, 740.0 - 14.0 * i as f32))
            .collect();
        page_of(number, lines)
    }

    /// `text[offset..offset + len]` by char offsets.
    fn slice_chars(text: &str, offset: usize, len: usize) -> String {
        text.chars().skip(offset).take(len).collect()
    }

    fn assert_marker_offsets(page: &PageText, markers: &[CitationMarker]) {
        for marker in markers.iter().filter(|m| m.page == page.page) {
            let got = slice_chars(
                &page.text,
                marker.offset as usize,
                marker.text.chars().count(),
            );
            assert_eq!(
                got, marker.text,
                "offset {} on page {}",
                marker.offset, page.page
            );
        }
    }

    fn parsed(raw: &str, label: Option<&str>) -> ReferenceEntry {
        let mut entry = ReferenceEntry {
            index: 1,
            label: label.map(str::to_string),
            raw: raw.to_string(),
            page: 1,
            ..ReferenceEntry::default()
        };
        parse_entry(&mut entry);
        entry
    }

    #[test]
    fn empty_input_is_harmless() {
        assert_eq!(find_reference_section(&[]), None);
        assert!(find_citation_markers(&[], &[]).is_empty());
        let (refs, markers) = extract_citations(&[]);
        assert!(refs.is_empty());
        assert!(markers.is_empty());
        let empty = PageText::new(1, 612.0, 792.0, 0);
        let (refs, markers) = extract_citations(&[empty]);
        assert!(refs.is_empty());
        assert!(markers.is_empty());
    }

    #[test]
    fn no_reference_section_gives_empty_vectors() {
        let page = column_page(1, &["Introduction", "Some text [1] here.", "1 Method"]);
        assert_eq!(find_reference_section(std::slice::from_ref(&page)), None);
        let (refs, markers) = extract_citations(&[page]);
        assert!(refs.is_empty());
        assert!(markers.is_empty());
    }

    #[test]
    fn last_heading_wins() {
        let toc = column_page(1, &["Contents", "1 Introduction", "References"]);
        let body = column_page(
            5,
            &[
                "Final words.",
                "7. References",
                "[1] A. Author. Title. Venue, 2020.",
            ],
        );
        let section = find_reference_section(&[toc, body]).expect("section");
        assert_eq!(section.first_page, 5);
        assert_eq!(section.first_line, 1);
        assert_eq!(section.heading, "7. References");
    }

    #[test]
    fn numbered_references_across_pages_with_continuations() {
        let page2 = column_page(
            2,
            &[
                "We build on [1] and on [2, 3]; see also [4–6] and [4-6].",
                "References",
                "[1] A. Vaswani, N. Shazeer, and I. Polosukhin. Attention is all you need. In Proceedings",
                "of the 31st Conference (NIPS ’17), pages 5998–6008, 2017.",
                "[2] J. Doe and J. Smith. Deep widgets. arXiv preprint",
            ],
        );
        let mut page3 = column_page(
            3,
            &[
                "arXiv:2001.01234, 2020.",
                "[3] Smith AB, Jones C. Deep widgets in practice. J Widgets. 2020;12(3):45-67.",
                "[4] B. Lee. Fourth. Venue, 2018.",
                "[5] C. Kim. Fifth. Venue, 2019.",
                "[6] D. Park. Sixth. Venue, 2021.",
            ],
        );
        // A page number in the footer must not be glued to the last entry.
        page3.lines.push(line_at("3", 0, 300.0, 20.0));
        page3.text.push_str("\n3");
        let (refs, markers) = extract_citations(&[page2.clone(), page3]);

        assert_eq!(refs.len(), 6);
        let labels: Vec<&str> = refs.iter().filter_map(|r| r.label.as_deref()).collect();
        assert_eq!(labels, vec!["[1]", "[2]", "[3]", "[4]", "[5]", "[6]"]);
        let indices: Vec<u32> = refs.iter().map(|r| r.index).collect();
        assert_eq!(indices, vec![1, 2, 3, 4, 5, 6]);
        let expected_first = "[1] A. Vaswani, N. Shazeer, and I. Polosukhin. Attention is all you \
                              need. In Proceedings of the 31st Conference (NIPS ’17), pages \
                              5998–6008, 2017.";
        assert_eq!(refs[0].raw, expected_first);
        assert_eq!(
            refs[1].raw,
            "[2] J. Doe and J. Smith. Deep widgets. arXiv preprint arXiv:2001.01234, 2020."
        );
        assert_eq!(refs[1].page, 2);
        assert_eq!(refs[1].arxiv_id.as_deref(), Some("2001.01234"));
        assert_eq!(refs[1].year, Some(2020));
        assert_eq!(refs[2].page, 3);
        assert_eq!(refs[5].raw, "[6] D. Park. Sixth. Venue, 2021.");
        assert_eq!(refs[0].pages.as_deref(), Some("5998–6008"));

        let texts: Vec<&str> = markers.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, vec!["[1]", "[2, 3]", "[4–6]", "[4-6]"]);
        assert_eq!(markers[0].targets, vec![1]);
        assert_eq!(markers[1].targets, vec![2, 3]);
        assert_eq!(markers[2].targets, vec![4, 5, 6]);
        assert_eq!(markers[3].targets, vec![4, 5, 6]);
        assert!(markers.iter().all(|m| m.page == 2));
        assert_eq!(markers[0].offset, 12);
        assert_marker_offsets(&page2, &markers);
    }

    #[test]
    fn author_year_references_with_hanging_indent() {
        let body = column_page(
            1,
            &[
                "As shown by Smith et al. (2020), earlier work (Smith et al., 2020; Lee and Kim, 2019)",
                "and Smith (2018) agree. Equation (3) is unrelated, as is (see Table 2).",
            ],
        );
        let refs_page = page_of(
            2,
            vec![
                line_at("References", 0, 72.0, 740.0),
                line_at(
                    "Lee, J. and Kim, S. (2019). Fast things. Journal of Widgets, 12(3), 45–67.",
                    0,
                    72.0,
                    726.0,
                ),
                line_at(
                    "Smith, A., Jones, B., and Lee, C. (2020). Slow things: a survey. In Proceedings of the",
                    0,
                    72.0,
                    712.0,
                ),
                line_at("Conference on Things, pages 1–10.", 0, 86.0, 698.0),
                line_at(
                    "Smith, A. (2018). Solo work. Nature 500, 1–5.",
                    0,
                    72.0,
                    684.0,
                ),
            ],
        );
        let (refs, markers) = extract_citations(&[body.clone(), refs_page]);

        assert_eq!(refs.len(), 3);
        assert_eq!(refs[0].label.as_deref(), Some("Lee2019"));
        assert_eq!(refs[1].label.as_deref(), Some("Smith2020"));
        assert_eq!(refs[2].label.as_deref(), Some("Smith2018"));
        let expected_second = "Smith, A., Jones, B., and Lee, C. (2020). Slow things: a survey. \
                               In Proceedings of the Conference on Things, pages 1–10.";
        assert_eq!(refs[1].raw, expected_second);
        assert_eq!(refs[0].authors, vec!["Lee, J.", "Kim, S."]);
        assert_eq!(refs[0].title.as_deref(), Some("Fast things"));
        assert_eq!(refs[0].venue.as_deref(), Some("Journal of Widgets"));
        assert_eq!(refs[0].volume.as_deref(), Some("12"));
        assert_eq!(refs[0].issue.as_deref(), Some("3"));
        assert_eq!(refs[0].pages.as_deref(), Some("45–67"));
        assert_eq!(refs[0].year, Some(2019));
        assert_eq!(refs[1].authors, vec!["Smith, A.", "Jones, B.", "Lee, C."]);
        assert_eq!(refs[1].title.as_deref(), Some("Slow things: a survey"));
        assert_eq!(
            refs[1].venue.as_deref(),
            Some("Proceedings of the Conference on Things")
        );
        assert_eq!(refs[1].pages.as_deref(), Some("1–10"));
        assert_eq!(refs[2].venue.as_deref(), Some("Nature"));
        assert_eq!(refs[2].volume.as_deref(), Some("500"));
        assert_eq!(refs[2].pages.as_deref(), Some("1–5"));

        let texts: Vec<&str> = markers.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(
            texts,
            vec![
                "Smith et al. (2020)",
                "(Smith et al., 2020; Lee and Kim, 2019)",
                "Smith (2018)",
            ]
        );
        assert_eq!(markers[0].targets, vec![2]);
        assert_eq!(markers[1].targets, vec![2, 1]);
        assert_eq!(markers[2].targets, vec![3]);
        assert_eq!(markers[0].offset, 12);
        assert_marker_offsets(&body, &markers);
    }

    #[test]
    fn author_year_without_layout_uses_the_name_pattern() {
        let page = page_of(
            1,
            vec![
                bare_line("References"),
                bare_line("Smith, A. (2020). Title one. Venue."),
                bare_line("continued text of the first entry."),
                bare_line("Jones, B. (2019). Title two. Venue."),
            ],
        );
        let section = find_reference_section(std::slice::from_ref(&page)).expect("section");
        let refs = segment_entries(&[page], &section);
        assert_eq!(refs.len(), 2);
        assert_eq!(
            refs[0].raw,
            "Smith, A. (2020). Title one. Venue. continued text of the first entry."
        );
        assert_eq!(refs[1].raw, "Jones, B. (2019). Title two. Venue.");
    }

    #[test]
    fn section_ends_at_appendix_and_furniture_is_dropped() {
        let mut page = column_page(
            1,
            &[
                "References",
                "[1] A. Author. Title. Venue, 2020.",
                "[2] B. Author. Title. Venue, 2021.",
                "Appendix A",
                "Appendix text that is not a reference.",
            ],
        );
        page.lines
            .insert(1, line_at("Running header", 0, 72.0, 780.0));
        page.text = page
            .lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<&str>>()
            .join("\n");
        let mut other = column_page(
            2,
            &["More body text.", "3. Numbers", "3) Paren", "Nothing else."],
        );
        other
            .lines
            .insert(0, line_at("Running header", 0, 72.0, 780.0));
        let pages = vec![page, other];
        let section = find_reference_section(&pages).expect("section");
        let refs = segment_entries(&pages, &section);
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].raw, "[1] A. Author. Title. Venue, 2020.");
        assert_eq!(refs[1].raw, "[2] B. Author. Title. Venue, 2021.");
    }

    #[test]
    fn dot_and_paren_numbering_styles() {
        let dot = column_page(
            1,
            &[
                "References",
                "1. A. Author. Title. Venue, 2020.",
                "2. B. Author. Two. Venue, 2021.",
            ],
        );
        let section = find_reference_section(std::slice::from_ref(&dot)).expect("section");
        let refs = segment_entries(&[dot], &section);
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].label.as_deref(), Some("1."));
        assert_eq!(refs[1].label.as_deref(), Some("2."));
        assert_eq!(refs[0].raw, "1. A. Author. Title. Venue, 2020.");

        let paren = column_page(
            1,
            &[
                "References",
                "1) A. Author. Title. Venue, 2020.",
                "wrapped line.",
                "2) B. Author. Two. Venue, 2021.",
            ],
        );
        let section = find_reference_section(std::slice::from_ref(&paren)).expect("section");
        let refs = segment_entries(&[paren], &section);
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].label.as_deref(), Some("1)"));
        assert_eq!(
            refs[0].raw,
            "1) A. Author. Title. Venue, 2020. wrapped line."
        );
        assert_eq!(refs[1].page, 1);
    }

    #[test]
    fn parse_acm_conference_paper() {
        let entry = parsed(
            "[1] A. Vaswani, N. Shazeer, and I. Polosukhin. Attention is all you need. In Proceedings \
             of the 31st International Conference on Neural Information Processing Systems (NIPS ’17), \
             pages 5998–6008, 2017.",
            Some("[1]"),
        );
        assert_eq!(
            entry.authors,
            vec!["A. Vaswani", "N. Shazeer", "I. Polosukhin"]
        );
        assert_eq!(entry.title.as_deref(), Some("Attention is all you need"));
        assert_eq!(
            entry.venue.as_deref(),
            Some(
                "Proceedings of the 31st International Conference on Neural Information Processing Systems"
            )
        );
        assert_eq!(entry.pages.as_deref(), Some("5998–6008"));
        assert_eq!(entry.year, Some(2017));
        assert_eq!(entry.volume, None);
        assert_eq!(entry.issue, None);
        assert_eq!(entry.doi, None);
        assert_eq!(entry.arxiv_id, None);
        assert_eq!(entry.url, None);
        assert_eq!(entry.label.as_deref(), Some("[1]"));
    }

    #[test]
    fn parse_ieee_journal_article() {
        let entry = parsed(
            "[2] A. Vaswani, N. Shazeer, and I. Polosukhin, “Attention is all you need,” IEEE Trans. \
             Pattern Anal. Mach. Intell., vol. 42, no. 3, pp. 1–10, Mar. 2020, doi: \
             10.1109/TPAMI.2020.1234567.",
            Some("[2]"),
        );
        assert_eq!(
            entry.authors,
            vec!["A. Vaswani", "N. Shazeer", "I. Polosukhin"]
        );
        assert_eq!(entry.title.as_deref(), Some("Attention is all you need"));
        assert_eq!(
            entry.venue.as_deref(),
            Some("IEEE Trans. Pattern Anal. Mach. Intell.")
        );
        assert_eq!(entry.volume.as_deref(), Some("42"));
        assert_eq!(entry.issue.as_deref(), Some("3"));
        assert_eq!(entry.pages.as_deref(), Some("1–10"));
        assert_eq!(entry.year, Some(2020));
        assert_eq!(entry.doi.as_deref(), Some("10.1109/TPAMI.2020.1234567"));
        assert_eq!(entry.url, None);
    }

    #[test]
    fn parse_vancouver_article_with_vol_issue_pages() {
        let entry = parsed(
            "[4] Smith AB, Jones C. Deep widgets in practice. J Widgets. 2020;12(3):45-67. \
             doi:10.1000/jw.2020.1",
            Some("[4]"),
        );
        assert_eq!(entry.authors, vec!["Smith AB", "Jones C"]);
        assert_eq!(entry.title.as_deref(), Some("Deep widgets in practice"));
        assert_eq!(entry.venue.as_deref(), Some("J Widgets"));
        assert_eq!(entry.volume.as_deref(), Some("12"));
        assert_eq!(entry.issue.as_deref(), Some("3"));
        assert_eq!(entry.pages.as_deref(), Some("45–67"));
        assert_eq!(entry.year, Some(2020));
        assert_eq!(entry.doi.as_deref(), Some("10.1000/jw.2020.1"));
    }

    #[test]
    fn parse_arxiv_preprint() {
        let entry = parsed(
            "[3] J. Doe and J. Smith. Deep widgets. arXiv preprint arXiv:2001.01234, 2020.",
            Some("[3]"),
        );
        assert_eq!(entry.authors, vec!["J. Doe", "J. Smith"]);
        assert_eq!(entry.title.as_deref(), Some("Deep widgets"));
        assert_eq!(entry.arxiv_id.as_deref(), Some("2001.01234"));
        assert_eq!(entry.year, Some(2020));
        assert_eq!(entry.venue, None);
        assert_eq!(entry.doi, None);
        assert_eq!(entry.pages, None);
    }

    #[test]
    fn parse_book_with_publisher() {
        let entry = parsed(
            "Smith, J. (2015). The Book of Widgets (2nd ed.). Cambridge, MA: MIT Press.",
            None,
        );
        assert_eq!(entry.authors, vec!["Smith, J."]);
        assert_eq!(entry.year, Some(2015));
        assert_eq!(
            entry.title.as_deref(),
            Some("The Book of Widgets (2nd ed.)")
        );
        assert_eq!(entry.venue.as_deref(), Some("MIT Press"));
        assert_eq!(entry.volume, None);
        assert_eq!(entry.issue, None);
        assert_eq!(entry.pages, None);
    }

    #[test]
    fn parse_nature_style_entry() {
        let entry = parsed("Smith, A. & Lee, B. Title. Nature 500, 1–5 (2020).", None);
        assert_eq!(entry.authors, vec!["Smith, A.", "Lee, B."]);
        assert_eq!(entry.title.as_deref(), Some("Title"));
        assert_eq!(entry.venue.as_deref(), Some("Nature"));
        assert_eq!(entry.volume.as_deref(), Some("500"));
        assert_eq!(entry.issue, None);
        assert_eq!(entry.pages.as_deref(), Some("1–5"));
        assert_eq!(entry.year, Some(2020));
    }

    #[test]
    fn parse_web_page_with_url_and_access_date() {
        let entry = parsed(
            "World Health Organization. Coronavirus disease (COVID-19) dashboard. \
             https://covid19.who.int, accessed 12 March 2021.",
            None,
        );
        assert_eq!(entry.authors, vec!["World Health Organization"]);
        assert_eq!(
            entry.title.as_deref(),
            Some("Coronavirus disease (COVID-19) dashboard")
        );
        assert_eq!(entry.url.as_deref(), Some("https://covid19.who.int"));
        assert_eq!(entry.year, Some(2021));
        assert_eq!(entry.venue, None);
        assert_eq!(entry.doi, None);
        assert_eq!(entry.pages, None);
        assert_eq!(entry.volume, None);
    }

    #[test]
    fn parse_apa_article_with_doi_url() {
        let entry = parsed(
            "Smith, A. B., & Jones, C. (2020). Deep widgets. Journal of Widgets, 12(3), 45–67. \
             https://doi.org/10.1000/jw.2020.1",
            None,
        );
        assert_eq!(entry.authors, vec!["Smith, A. B.", "Jones, C."]);
        assert_eq!(entry.title.as_deref(), Some("Deep widgets"));
        assert_eq!(entry.venue.as_deref(), Some("Journal of Widgets"));
        assert_eq!(entry.volume.as_deref(), Some("12"));
        assert_eq!(entry.issue.as_deref(), Some("3"));
        assert_eq!(entry.pages.as_deref(), Some("45–67"));
        assert_eq!(entry.year, Some(2020));
        assert_eq!(entry.doi.as_deref(), Some("10.1000/jw.2020.1"));
        assert_eq!(
            entry.url.as_deref(),
            Some("https://doi.org/10.1000/jw.2020.1")
        );
    }

    #[test]
    fn parse_keeps_raw_and_never_invents() {
        let raw = "Some unparseable fragment";
        let entry = parsed(raw, None);
        assert_eq!(entry.raw, raw);
        assert!(entry.authors.is_empty());
        assert_eq!(entry.title, None);
        assert_eq!(entry.year, None);
        assert_eq!(entry.venue, None);
        assert_eq!(entry.doi, None);
    }

    #[test]
    fn author_splitting_forms() {
        assert_eq!(
            split_authors("A. B. Smith, C. Jones, and D. Lee"),
            vec!["A. B. Smith", "C. Jones", "D. Lee"]
        );
        assert_eq!(
            split_authors("Smith, A. B., Jones, C."),
            vec!["Smith, A. B.", "Jones, C."]
        );
        assert_eq!(
            split_authors("Smith AB, Jones C"),
            vec!["Smith AB", "Jones C"]
        );
        assert_eq!(
            split_authors("Smith, John, and Jane Doe"),
            vec!["Smith, John", "Jane Doe"]
        );
        assert_eq!(split_authors("Smith, A., et al."), vec!["Smith, A."]);
        assert_eq!(author_surname("A. B. Smith"), "smith");
        assert_eq!(author_surname("Smith AB"), "smith");
        assert_eq!(author_surname("van der Maaten, L."), "van der maaten");
    }

    #[test]
    fn year_suffix_picks_among_same_year_entries() {
        let mut refs = vec![
            parsed("Smith, A. (2020a). First. Venue.", None),
            parsed("Smith, A. (2020b). Second. Venue.", None),
        ];
        refs[1].index = 2;
        let index = RefIndex::build(&refs);
        assert_eq!(index.resolve_author_year("Smith", 2020, "b"), vec![2]);
        assert_eq!(index.resolve_author_year("Smith", 2020, ""), vec![1, 2]);
        assert!(index.resolve_author_year("Jones", 2020, "").is_empty());
    }

    // ---- Real-data tests: verbatim snippets from the evaluation corpus. ----

    /// Segment and parse `texts` as one column page under a `References`
    /// heading, without layout evidence for the entry starts.
    fn refs_from_lines(texts: &[&str]) -> Vec<ReferenceEntry> {
        let mut lines: Vec<Line> = vec![bare_line("References")];
        lines.extend(texts.iter().map(|t| bare_line(t)));
        let page = page_of(1, lines);
        let (refs, _) = extract_citations(&[page]);
        refs
    }

    fn doi_of(raw: &str) -> Option<String> {
        find_doi(raw).map(|(_, doi)| doi)
    }

    /// Elsevier style (arXiv:2305.13843): initials-first authors, then the
    /// unquoted title after a comma, then the journal with `35 (1992) 61–70`
    /// or `in: Proceedings ..., volume 34, 2020, pp. 8936–8943`.
    #[test]
    fn elsevier_comma_delimited_titles() {
        let refs = refs_from_lines(&[
            "[1] D. Goldberg, D. Nichols, B. M. Oki, D. Terry, Using collaborative",
            "ﬁltering to weave an information tapestry, Communications of the",
            "ACM 35 (1992) 61–70.",
            "[2] T. Sun, Y. Shao, X. Li, P. Liu, H. Yan, X. Qiu, X. Huang, Learning",
            "sparse sharing architectures for multiple tasks, in: Proceedings of",
            "the AAAI conference on artiﬁcial intelligence, volume 34, 2020, pp.",
            "8936–8943.",
        ]);
        assert_eq!(refs.len(), 2);
        assert_eq!(
            refs[0].title.as_deref(),
            Some("Using collaborative ﬁltering to weave an information tapestry")
        );
        assert_eq!(
            refs[0].authors,
            vec!["D. Goldberg", "D. Nichols", "B. M. Oki", "D. Terry"]
        );
        assert_eq!(refs[0].venue.as_deref(), Some("Communications of the ACM"));
        assert_eq!(refs[0].volume.as_deref(), Some("35"));
        assert_eq!(refs[0].pages.as_deref(), Some("61–70"));
        assert_eq!(refs[0].year, Some(1992));
        assert_eq!(
            refs[1].title.as_deref(),
            Some("Learning sparse sharing architectures for multiple tasks")
        );
        assert_eq!(refs[1].authors.len(), 7);
        assert_eq!(refs[1].authors[6], "X. Huang");
        assert_eq!(
            refs[1].venue.as_deref(),
            Some("Proceedings of the AAAI conference on artiﬁcial intelligence")
        );
        assert_eq!(refs[1].volume.as_deref(), Some("34"));
        assert_eq!(refs[1].pages.as_deref(), Some("8936–8943"));
        assert_eq!(refs[1].year, Some(2020));
    }

    /// SIAM style (arXiv:2603.21379): title after the author comma, a comma
    /// inside a title, `Journal, 46 (2020)`, and the DOI as a trailing URL.
    /// `De-` + `composition` is closed up because `Decomposition` occurs
    /// elsewhere in the section; `Univer-` + `sity` because nothing says
    /// otherwise.
    #[test]
    fn siam_comma_delimited_titles_with_dois() {
        let refs = refs_from_lines(&[
            "[1] S. Ahmadi-Asl, S. Abukhovich, M. G. Asante-Mensah, A. Cichocki, A. H. Phan,",
            "T. Tanaka, and I. Oseledets, Randomized Algorithms for Computation of Tucker De-",
            "composition and Higher Order SVD (HOSVD), IEEE Access, 9 (2021), pp. 28684–28706,",
            "https://doi.org/10.1109/ACCESS.2021.3058103.",
            "[2] G. Ballard, A. Klinvex, and T. G. Kolda, TuckerMPI: A Parallel C++/MPI Software",
            "Package for Large-scale Data Compression via the Tucker Tensor Decomposition, ACM",
            "Transactions on Mathematical Software, 46 (2020), https://doi.org/10.1145/3378445.",
            "[3] G. Ballard and T. G. Kolda, Tensor Decompositions for Data Science, Cambridge Univer-",
            "sity Press, 2025, https://doi.org/10.1017/9781009471664.",
            "[4] C. Boutsidis and D. P. Woodruff, Optimal CUR Matrix Decompositions, SIAM Journal on",
            "Computing, 46 (2017), pp. 543–589, https://doi.org/10.1137/140977898.",
        ]);
        assert_eq!(refs.len(), 4);
        assert_eq!(
            refs[0].title.as_deref(),
            Some(
                "Randomized Algorithms for Computation of Tucker Decomposition and Higher \
                 Order SVD (HOSVD)"
            )
        );
        assert_eq!(refs[0].authors.len(), 7);
        assert_eq!(refs[0].authors[0], "S. Ahmadi-Asl");
        assert_eq!(refs[0].authors[6], "I. Oseledets");
        assert_eq!(refs[0].doi.as_deref(), Some("10.1109/ACCESS.2021.3058103"));
        assert_eq!(refs[0].venue.as_deref(), Some("IEEE Access"));
        assert_eq!(refs[0].volume.as_deref(), Some("9"));
        assert_eq!(refs[0].pages.as_deref(), Some("28684–28706"));
        assert_eq!(refs[0].year, Some(2021));
        assert_eq!(
            refs[1].title.as_deref(),
            Some(
                "TuckerMPI: A Parallel C++/MPI Software Package for Large-scale Data \
                 Compression via the Tucker Tensor Decomposition"
            )
        );
        assert_eq!(refs[1].doi.as_deref(), Some("10.1145/3378445"));
        assert_eq!(
            refs[1].venue.as_deref(),
            Some("ACM Transactions on Mathematical Software")
        );
        assert_eq!(refs[1].volume.as_deref(), Some("46"));
        assert_eq!(
            refs[2].title.as_deref(),
            Some("Tensor Decompositions for Data Science")
        );
        assert_eq!(refs[2].authors, vec!["G. Ballard", "T. G. Kolda"]);
        assert_eq!(refs[2].venue.as_deref(), Some("Cambridge University Press"));
        assert_eq!(refs[2].doi.as_deref(), Some("10.1017/9781009471664"));
        assert_eq!(refs[2].year, Some(2025));
        assert_eq!(
            refs[3].title.as_deref(),
            Some("Optimal CUR Matrix Decompositions")
        );
        assert_eq!(refs[3].venue.as_deref(), Some("SIAM Journal on Computing"));
        assert_eq!(refs[3].pages.as_deref(), Some("543–589"));
        assert_eq!(refs[3].doi.as_deref(), Some("10.1137/140977898"));
    }

    /// SIAM style with `in VENUE, year` and `Journal, 22 (2022), pp.`
    /// (arXiv:2504.09409).
    #[test]
    fn siam_titles_before_in_venue() {
        let refs = refs_from_lines(&[
            "[1] A. Alacaoglu and S. J. Wright, Complexity of single loop algorithms for nonlinear programming",
            "with stochastic objective and constraints, in AISTATS, 2024.",
            "[2] K. Balasubramanian and S. Ghadimi, Zeroth-order (non)-convex stochastic optimization via con-",
            "ditional gradient and gradient updates, in NeurIPS, 2018.",
            "[3] K. Balasubramanian and S. Ghadimi, Zeroth-order nonconvex stochastic optimization: Handling",
            "constraints, high-dimensionality and saddle-points, Found. Comput. Math., 22 (2022), pp. 35–76.",
            "[4] A. Beck, First-order methods in optimization, SIAM, 2017.",
        ]);
        assert_eq!(refs.len(), 4);
        assert_eq!(
            refs[0].title.as_deref(),
            Some(
                "Complexity of single loop algorithms for nonlinear programming with \
                 stochastic objective and constraints"
            )
        );
        assert_eq!(refs[0].authors, vec!["A. Alacaoglu", "S. J. Wright"]);
        assert_eq!(refs[0].venue.as_deref(), Some("AISTATS"));
        assert_eq!(refs[0].year, Some(2024));
        assert_eq!(
            refs[1].title.as_deref(),
            Some(
                "Zeroth-order (non)-convex stochastic optimization via conditional gradient \
                 and gradient updates"
            )
        );
        assert_eq!(refs[1].venue.as_deref(), Some("NeurIPS"));
        assert_eq!(
            refs[2].title.as_deref(),
            Some(
                "Zeroth-order nonconvex stochastic optimization: Handling constraints, \
                 high-dimensionality and saddle-points"
            )
        );
        assert_eq!(refs[2].venue.as_deref(), Some("Found. Comput. Math."));
        assert_eq!(refs[2].volume.as_deref(), Some("22"));
        assert_eq!(refs[2].pages.as_deref(), Some("35–76"));
        assert_eq!(refs[2].year, Some(2022));
        assert_eq!(
            refs[3].title.as_deref(),
            Some("First-order methods in optimization")
        );
        assert_eq!(refs[3].authors, vec!["A. Beck"]);
        assert_eq!(refs[3].venue.as_deref(), Some("SIAM"));
        assert_eq!(refs[3].year, Some(2017));
    }

    /// ACM style (arXiv:2412.06210): `Authors. 2019. Title. Venue (2019)`;
    /// the year sentence after the authors is not the title, and the
    /// parenthesised year at the end does not win over it.
    #[test]
    fn acm_year_sentence_between_authors_and_title() {
        let refs = refs_from_lines(&[
            "[1] Alham Fikri Aji and Kenneth Heaﬁeld. 2017. Sparse communication for dis-",
            "tributed gradient descent. arXiv preprint arXiv:1704.05021 (2017).",
            "[2] Leonidas G Anthopoulos. 2015. Understanding the smart city domain: A literature",
            "review. Transforming city governments for successful smart cities (2015), 9–21.",
        ]);
        assert_eq!(refs.len(), 2);
        assert_eq!(
            refs[0].title.as_deref(),
            Some("Sparse communication for distributed gradient descent")
        );
        assert_eq!(refs[0].authors, vec!["Alham Fikri Aji", "Kenneth Heaﬁeld"]);
        assert_eq!(refs[0].year, Some(2017));
        assert_eq!(refs[0].arxiv_id.as_deref(), Some("1704.05021"));
        assert_eq!(
            refs[1].title.as_deref(),
            Some("Understanding the smart city domain: A literature review")
        );
        assert_eq!(refs[1].authors, vec!["Leonidas G Anthopoulos"]);
        assert_eq!(refs[1].year, Some(2015));
        assert_eq!(refs[1].pages.as_deref(), Some("9–21"));

        let entry = parsed(
            "[40] Qiang Yang, Yang Liu, Tianjian Chen, and Yongxin Tong. 2019. Federated \
             machine learning: Concept and applications. ACM Transactions on Intelligent \
             Systems and Technology (TIST) 10, 2 (2019), 1–19.",
            Some("[40]"),
        );
        assert_eq!(
            entry.title.as_deref(),
            Some("Federated machine learning: Concept and applications")
        );
        assert_eq!(
            entry.authors,
            vec!["Qiang Yang", "Yang Liu", "Tianjian Chen", "Yongxin Tong"]
        );
        assert_eq!(entry.year, Some(2019));
        assert_eq!(
            entry.venue.as_deref(),
            Some("ACM Transactions on Intelligent Systems and Technology")
        );
    }

    /// DOIs broken by line wraps (arXiv:2108.04588, 2305.13843, 2509.10402,
    /// 2501.17300): after `/`, after `10.`, after `.`, mid-number inside a
    /// `doi.org` URL, after `)`; a sentence or a year after the DOI is not
    /// glued on.
    #[test]
    fn doi_closed_up_across_line_wraps() {
        assert_eq!(
            doi_of(
                "[7] Sergio Cabello and Miha Jejčič. Reﬁning the hierarchies of classes of \
                 geometric intersection graphs. Electronic Journal of Combinatorics, \
                 24(1):P1.33, 19 pp., 2017. doi:10.37236/ 6040."
            )
            .as_deref(),
            Some("10.37236/6040")
        );
        assert_eq!(
            doi_of("Mathematische Zeitschrift, 17:228–249, 1923. doi:10.1007/ BF01504345.")
                .as_deref(),
            Some("10.1007/BF01504345")
        );
        assert_eq!(
            doi_of("Discovery & Data Mining, 2019, pp. 1123–1131. doi: 10. 1145/3292500.3330861.")
                .as_deref(),
            Some("10.1145/3292500.3330861")
        );
        assert_eq!(
            doi_of("Journal of Combinatorial Theory, Series B, 103(1):114–143, 2013. doi:10.1016/j.jctb. 2012.09.004.")
                .as_deref(),
            Some("10.1016/j.jctb.2012.09.004")
        );
        assert_eq!(
            doi_of(
                "2024, p. 227–230. [Online]. Available: https://doi.org/10.1145/364399 1.3648400"
            )
            .as_deref(),
            Some("10.1145/3643991.3648400")
        );
        assert_eq!(
            doi_of(
                "Discrete Mathematics, 262(1–3):221–227, 2003. doi:10.1016/S0012-365X(02) 00501-0."
            )
            .as_deref(),
            Some("10.1016/S0012-365X(02)00501-0")
        );
        assert_eq!(
            doi_of("Oxford University Press. doi: 10.1093/acprof:oso/9780199591565. 001.0001.")
                .as_deref(),
            Some("10.1093/acprof:oso/9780199591565.001.0001")
        );
        assert_eq!(
            doi_of("doi:10.1000/abc. Accessed 12 March 2021.").as_deref(),
            Some("10.1000/abc")
        );
        assert_eq!(
            doi_of("doi:10.1000/xyz. 2020.").as_deref(),
            Some("10.1000/xyz")
        );
        assert_eq!(doi_of("see 10.1234/ and nothing"), None);

        // The masked range covers the whole wrapped DOI, so no piece of it
        // is read as a volume or page number.
        let entry = parsed(
            "[15] Mihály Fekete. Über die Verteilung der Wurzeln bei gewissen algebraischen \
             Gleichungen mit ganzzahligen Koeﬃzienten. Mathematische Zeitschrift, \
             17:228–249, 1923. doi:10.1007/ BF01504345.",
            Some("[15]"),
        );
        assert_eq!(entry.doi.as_deref(), Some("10.1007/BF01504345"));
        assert_eq!(entry.volume.as_deref(), Some("17"));
        assert_eq!(entry.pages.as_deref(), Some("228–249"));
        assert_eq!(entry.year, Some(1923));
        assert_eq!(entry.venue.as_deref(), Some("Mathematische Zeitschrift"));
    }

    /// `Brent N. Clark` is a middle initial, not Vancouver `Smith AB`
    /// (arXiv:2108.04588 [13]).
    #[test]
    fn middle_initial_is_not_vancouver() {
        let entry = parsed(
            "[13] Brent N. Clark, Charles J. Colbourn, and David S. Johnson. Unit disk graphs. \
             Discrete Mathematics, 86(1–3):165–177, 1990. doi:10.1016/0012-365X(90)90358-O.",
            Some("[13]"),
        );
        assert_eq!(
            entry.authors,
            vec!["Brent N. Clark", "Charles J. Colbourn", "David S. Johnson"]
        );
        assert_eq!(entry.title.as_deref(), Some("Unit disk graphs"));
        assert_eq!(entry.venue.as_deref(), Some("Discrete Mathematics"));
        assert_eq!(entry.volume.as_deref(), Some("86"));
        assert_eq!(entry.issue.as_deref(), Some("1–3"));
        assert_eq!(entry.pages.as_deref(), Some("165–177"));
        assert_eq!(entry.year, Some(1990));
        assert_eq!(entry.doi.as_deref(), Some("10.1016/0012-365X(90)90358-O"));
    }

    /// An apostrophe inside `“...”` does not close the quoted title, and the
    /// `doi.org` URL after `[Online]. Available:` is read (arXiv:2509.10402).
    #[test]
    fn quoted_title_keeps_inner_apostrophe() {
        let refs = refs_from_lines(&[
            "[57] H. Hao, K. A. Hasan, H. Qin, M. Macedo, Y. Tian, S. H. H.",
            "Ding, and A. E. Hassan, “An empirical study on developers’ shared",
            "conversations with chatgpt in github pull requests and issues,”",
            "Empirical Softw. Engg., vol. 29, no. 6, Sep. 2024. [Online]. Available:",
            "https://doi.org/10.1007/s10664-024-10540-x",
        ]);
        assert_eq!(refs.len(), 1);
        assert_eq!(
            refs[0].title.as_deref(),
            Some(
                "An empirical study on developers’ shared conversations with chatgpt in \
                 github pull requests and issues"
            )
        );
        assert_eq!(refs[0].authors.len(), 7);
        assert_eq!(refs[0].authors[5], "S. H. H. Ding");
        assert_eq!(refs[0].venue.as_deref(), Some("Empirical Softw. Engg."));
        assert_eq!(refs[0].volume.as_deref(), Some("29"));
        assert_eq!(refs[0].issue.as_deref(), Some("6"));
        assert_eq!(refs[0].year, Some(2024));
        assert_eq!(refs[0].doi.as_deref(), Some("10.1007/s10664-024-10540-x"));
        assert_eq!(
            refs[0].url.as_deref(),
            Some("https://doi.org/10.1007/s10664-024-10540-x")
        );
    }

    /// IEEE books (arXiv:2503.15734): `H. Khalil, Nonlinear Systems. Publisher`
    /// and `E. D. Sontag, Contractive Systems with Inputs, pp. 217–228.`; a
    /// quoted title broken as `sta-` + `bilization` is closed up.
    #[test]
    fn ieee_book_titles_before_publisher() {
        let refs = refs_from_lines(&[
            "[2] M. Jankovic, “Robust control barrier functions for constrained sta-",
            "bilization of nonlinear systems,” Automatica, vol. 96, pp. 359–367,",
            "2018.",
        ]);
        assert_eq!(refs.len(), 1);
        assert_eq!(
            refs[0].title.as_deref(),
            Some(
                "Robust control barrier functions for constrained stabilization of nonlinear systems"
            )
        );
        assert_eq!(refs[0].venue.as_deref(), Some("Automatica"));
        assert_eq!(refs[0].volume.as_deref(), Some("96"));
        assert_eq!(refs[0].pages.as_deref(), Some("359–367"));
        assert_eq!(refs[0].year, Some(2018));

        let khalil = parsed(
            "[21] H. Khalil, Nonlinear Systems. Pearson Education, Prentice Hall, 2 ed., 2002.",
            Some("[21]"),
        );
        assert_eq!(khalil.title.as_deref(), Some("Nonlinear Systems"));
        assert_eq!(khalil.authors, vec!["H. Khalil"]);
        assert_eq!(khalil.year, Some(2002));

        let sontag = parsed(
            "[24] E. D. Sontag, Contractive Systems with Inputs, pp. 217–228. Berlin, \
             Heidelberg: Springer Berlin Heidelberg, 2010.",
            Some("[24]"),
        );
        assert_eq!(
            sontag.title.as_deref(),
            Some("Contractive Systems with Inputs")
        );
        assert_eq!(sontag.authors, vec!["E. D. Sontag"]);
        assert_eq!(sontag.pages.as_deref(), Some("217–228"));
        assert_eq!(sontag.year, Some(2010));
    }

    /// `et al.` closes the author list when the title follows it directly,
    /// and a quotation inside an unquoted title is not the title
    /// (arXiv:2511.13979, alpha style without hanging indent evidence).
    #[test]
    fn et_al_and_inline_quotation_before_unquoted_title() {
        let refs = refs_from_lines(&[
            "K. M. Collins, I. Sucholutsky, U. Bhatt, K. Chandra, L. Wong, M. Lee, C. E. Zhang, T. Zhi-Xuan, M. Ho,",
            "V. Mansinghka, et al. Building machines that learn and think with people. Nature Human Behaviour, 8",
            "(10):1851–1863, 2024.",
        ]);
        assert_eq!(refs.len(), 1);
        assert_eq!(
            refs[0].title.as_deref(),
            Some("Building machines that learn and think with people")
        );
        assert_eq!(refs[0].authors.len(), 10);
        assert_eq!(refs[0].authors[0], "K. M. Collins");
        assert_eq!(refs[0].authors[9], "V. Mansinghka");
        assert_eq!(refs[0].venue.as_deref(), Some("Nature Human Behaviour"));
        assert_eq!(refs[0].volume.as_deref(), Some("8"));
        assert_eq!(refs[0].issue.as_deref(), Some("10"));
        assert_eq!(refs[0].pages.as_deref(), Some("1851–1863"));
        assert_eq!(refs[0].year, Some(2024));

        let entry = parsed(
            r#"C. Anthony, B. A. Bechky, and A.-L. Fayard. "collaborating" with ai: Taking a system view to explore the future of work. Organization Science, 34(5):1672–1694, 2023. doi: 10.1287/orsc.2022.1651."#,
            None,
        );
        assert_eq!(
            entry.title.as_deref(),
            Some(r#""collaborating" with ai: Taking a system view to explore the future of work"#)
        );
        assert_eq!(
            entry.authors,
            vec!["C. Anthony", "B. A. Bechky", "A.-L. Fayard"]
        );
        assert_eq!(entry.venue.as_deref(), Some("Organization Science"));
        assert_eq!(entry.volume.as_deref(), Some("34"));
        assert_eq!(entry.issue.as_deref(), Some("5"));
        assert_eq!(entry.pages.as_deref(), Some("1672–1694"));
        assert_eq!(entry.year, Some(2023));
        assert_eq!(entry.doi.as_deref(), Some("10.1287/orsc.2022.1651"));
    }

    /// A URL broken after `/` is closed up (arXiv:2511.13979).
    #[test]
    fn url_closed_up_across_line_wrap() {
        let refs = refs_from_lines(&[
            "OpenAI. Customizing your ChatGPT personality. https://help.openai.com/en/articles/",
            "11899719-customizing-your-chatgpt-personality, 2025a. Accessed: 2025-08-29.",
        ]);
        assert_eq!(refs.len(), 1);
        assert_eq!(
            refs[0].url.as_deref(),
            Some(
                "https://help.openai.com/en/articles/11899719-customizing-your-chatgpt-personality"
            )
        );
        assert_eq!(
            refs[0].title.as_deref(),
            Some("Customizing your ChatGPT personality")
        );
        assert_eq!(refs[0].authors, vec!["OpenAI"]);
        assert_eq!(refs[0].year, Some(2025));
    }

    /// Justified ACL columns arrive as row fragments (`and Jingren Zhou.
    /// 2023.` | `Qwen-vl: A versatile` on one baseline) and the neighbouring
    /// line can no longer say whether a line is outdented; the section-wide
    /// x levels can (arXiv:2505.16990). Hyphenated line ends are resolved
    /// when the lines are joined.
    #[test]
    fn row_fragments_and_section_wide_indent_levels() {
        let rows: Vec<(&str, f32, Option<&str>)> = vec![
            (
                "Jinze Bai, Shuai Bai, Shusheng Yang, Shijie Wang,",
                72.0,
                None,
            ),
            ("Sinan Tan, Peng Wang, Junyang Lin, Chang Zhou,", 86.0, None),
            (
                "and Jingren Zhou. 2023.",
                86.0,
                Some("Qwen-vl: A versatile"),
            ),
            (
                "vision-language model for understanding, localiza-",
                86.0,
                None,
            ),
            (
                "tion, text reading, and beyond.",
                86.0,
                Some("arXiv preprint"),
            ),
            ("arXiv:2308.12966.", 86.0, None),
            (
                "Shuai Bai, Keqin Chen, Xuejing Liu, Jialin Wang, Wen-",
                72.0,
                None,
            ),
            ("bin Ge, Sibo Song, Kai Dang, Peng Wang, Shi-", 86.0, None),
            ("jie Wang, Jun Tang, Humen Zhong, Yuanzhi Zhu,", 86.0, None),
            (
                "Mingkun Yang, Zhaohai Li, Jianqiang Wan, Pengfei",
                86.0,
                None,
            ),
            (
                "Wang, Wei Ding, Zheren Fu, Yiheng Xu, and 8 oth-",
                86.0,
                None,
            ),
            (
                "ers. 2025. Qwen2.5-vl technical report. Preprint,",
                86.0,
                None,
            ),
            ("arXiv:2502.13923.", 86.0, None),
            (
                "Zalán Borsos, Matt Shariﬁ, Damien Vincent, Eugene",
                72.0,
                None,
            ),
            (
                "Kharitonov, Neil Zeghidour, and Marco Tagliasacchi.",
                86.0,
                None,
            ),
            (
                "2023. Soundstorm: Efﬁcient parallel audio genera-",
                86.0,
                None,
            ),
            ("tion. Preprint, arXiv:2305.09636.", 86.0, None),
        ];
        let mut lines: Vec<Line> = vec![line_at("References", 0, 72.0, 754.0)];
        for (i, (text, x0, fragment)) in rows.iter().enumerate() {
            let y = 740.0 - 14.0 * i as f32;
            lines.push(line_at(text, 0, *x0, y));
            if let Some(fragment) = fragment {
                lines.push(line_at(fragment, 0, 220.0, y));
            }
        }
        let page = page_of(9, lines);
        let (refs, _) = extract_citations(&[page]);

        assert_eq!(refs.len(), 3);
        assert_eq!(
            refs[0].raw,
            "Jinze Bai, Shuai Bai, Shusheng Yang, Shijie Wang, Sinan Tan, Peng Wang, \
             Junyang Lin, Chang Zhou, and Jingren Zhou. 2023. Qwen-vl: A versatile \
             vision-language model for understanding, localization, text reading, and \
             beyond. arXiv preprint arXiv:2308.12966."
        );
        assert_eq!(
            refs[0].title.as_deref(),
            Some(
                "Qwen-vl: A versatile vision-language model for understanding, localization, \
                 text reading, and beyond"
            )
        );
        assert_eq!(refs[0].year, Some(2023));
        assert_eq!(refs[0].arxiv_id.as_deref(), Some("2308.12966"));
        assert_eq!(refs[0].label.as_deref(), Some("Jinze2023"));
        assert!(
            refs[1]
                .raw
                .contains("Wenbin Ge, Sibo Song, Kai Dang, Peng Wang, Shijie Wang")
        );
        assert!(refs[1].raw.contains("and 8 others. 2025."));
        assert_eq!(
            refs[1].title.as_deref(),
            Some("Qwen2.5-vl technical report")
        );
        assert_eq!(refs[1].arxiv_id.as_deref(), Some("2502.13923"));
        assert_eq!(
            refs[2].title.as_deref(),
            Some("Soundstorm: Efﬁcient parallel audio generation")
        );
        assert_eq!(refs[2].year, Some(2023));
        assert_eq!(refs[2].arxiv_id.as_deref(), Some("2305.09636"));
    }

    /// Line-end hyphens: a compound that occurs unbroken elsewhere keeps its
    /// hyphen, a known compound prefix keeps it, a word that occurs joined
    /// elsewhere drops it, an uppercase continuation keeps it, a dash that
    /// is not a word break stays separate.
    #[test]
    fn hyphen_breaks_are_resolved_at_line_joins() {
        let context = "multi-task learning\nreconstruction of images";
        assert_eq!(
            hyphen_break("Learning multi-", "task models", context),
            HyphenJoin::Keep
        );
        assert_eq!(
            hyphen_break("MRI recon-", "struction", context),
            HyphenJoin::Drop
        );
        assert_eq!(
            hyphen_break("a self-", "supervised model", ""),
            HyphenJoin::Keep
        );
        assert_eq!(hyphen_break("privacy-", "preserving", ""), HyphenJoin::Keep);
        assert_eq!(hyphen_break("Ming-", "Hsuan Yang", ""), HyphenJoin::Keep);
        assert_eq!(hyphen_break("pp. 911-", "935", ""), HyphenJoin::Keep);
        assert_eq!(hyphen_break("sta-", "bilization", ""), HyphenJoin::Drop);
        assert_eq!(
            hyphen_break("Attention -", "is all", ""),
            HyphenJoin::Separate
        );
    }

    /// `Series A, containing papers ...` looks like `Surname A,` and follows
    /// a period, but carries no year, DOI, arXiv id or URL: it is a
    /// continuation of the previous entry (arXiv:2306.11313).
    #[test]
    fn false_start_without_evidence_folds_into_previous_entry() {
        let refs = refs_from_lines(&[
            "Mercer, J. (1909). Xvi. functions of positive and negative type, and their connection the",
            "theory of integral equations. Philosophical Transactions of the Royal Society of London.",
            "Series A, containing papers of a mathematical or physical character, 209(441-458):415–446.",
            "Moradi, M. M. and Mateu, J. (2020). First-and second-order characteristics of spatio-",
            "temporal point processes on linear networks. Journal of Computational and Graphical Statistics, 29(3):432–443.",
        ]);
        assert_eq!(refs.len(), 2);
        assert!(refs[0].raw.ends_with("209(441-458):415–446."));
        assert_eq!(refs[0].label.as_deref(), Some("Mercer1909"));
        assert!(refs[1].raw.contains("spatio-temporal point processes"));
        assert_eq!(refs[1].label.as_deref(), Some("Moradi2020"));
        assert_eq!(refs[1].volume.as_deref(), Some("29"));
        assert_eq!(refs[1].pages.as_deref(), Some("432–443"));
    }

    /// Table cells after the last reference sit at the entry-start x level
    /// but carry no reference evidence: the trailing run is dropped
    /// (arXiv:2502.00857).
    #[test]
    fn trailing_table_cells_are_dropped() {
        let texts: Vec<(&str, f32)> = vec![
            ("Asahi Ushio, Fernando Alva-Manchego, and Jose", 72.0),
            ("Camacho-Collados. 2023. A practical toolkit for", 86.0),
            ("multilingual question and answer generation. In Pro-", 86.0),
            ("ceedings of the 61st Annual Meeting of the Associa-", 86.0),
            ("tion for Computational Linguistics (Volume 3: Sys-", 86.0),
            ("tem Demonstrations), pages 86–94, Toronto, Canada.", 86.0),
            ("Association for Computational Linguistics.", 86.0),
            ("Preferred", 72.0),
            ("Cost", 72.0),
            ("Execution", 72.0),
            ("Metric", 72.0),
            ("Method", 72.0),
            ("Accuracy", 72.0),
            ("Device", 72.0),
            ("Effectiveness", 72.0),
        ];
        let mut lines: Vec<Line> = vec![line_at("References", 0, 72.0, 754.0)];
        for (i, (text, x0)) in texts.iter().enumerate() {
            lines.push(line_at(text, 0, *x0, 740.0 - 14.0 * i as f32));
        }
        let page = page_of(10, lines);
        let (refs, _) = extract_citations(&[page]);
        assert_eq!(refs.len(), 1);
        assert_eq!(
            refs[0].raw,
            "Asahi Ushio, Fernando Alva-Manchego, and Jose Camacho-Collados. 2023. A \
             practical toolkit for multilingual question and answer generation. In \
             Proceedings of the 61st Annual Meeting of the Association for Computational \
             Linguistics (Volume 3: System Demonstrations), pages 86–94, Toronto, Canada. \
             Association for Computational Linguistics."
        );
        assert_eq!(
            refs[0].title.as_deref(),
            Some("A practical toolkit for multilingual question and answer generation")
        );
        assert_eq!(refs[0].pages.as_deref(), Some("86–94"));
    }

    /// Section-end markers seen after real reference lists: `- Supplementary
    /// Material -` (arXiv:2505.16990), an IEEE author biography
    /// (arXiv:2503.15734), a table caption and a row of numbers. Ordinary
    /// entries and titles containing `is a` are not markers.
    #[test]
    fn section_end_markers() {
        let line = |text: &str| SectionLine {
            page: 1,
            column: 0,
            x0: Some(72.0),
            y0: Some(700.0),
            size: Some(10.0),
            edge: false,
            text: text.to_string(),
        };
        let ends = |text: &str| is_end_heading(&line(text), Style::Bracket, Some(10.0));
        assert!(ends("- Supplementary Material -"));
        assert!(ends(
            "DAVID E. J. VAN WIJK is a Postdoctoral Scholar in the Department"
        ));
        assert!(ends("Jane Doe received the B.S. degree in 2001."));
        assert!(ends(
            "Table 5: Qualitative comparison of hint generation methods."
        ));
        assert!(ends("0.85 0.91 0.77"));
        assert!(ends("Appendix A"));
        assert!(!ends(
            "[3] A. Author, Deep Learning is a Robust Method, Venue, 2020."
        ));
        assert!(!ends(
            "Deep Learning is a Robust Method. In Proceedings of Things, 2020."
        ));
        assert!(!ends("pp. 3615–3620, 2023."));
    }

    /// `Page 30 of 35` / `Page 31 of 35` footers differ only in their digits
    /// and are page furniture (arXiv:2305.13843).
    #[test]
    fn page_footers_with_changing_numbers_are_furniture() {
        let mut first = column_page(
            30,
            &[
                "References",
                "[1] D. Goldberg, D. Nichols, B. M. Oki, D. Terry, Using collaborative",
                "ﬁltering to weave an information tapestry, Communications of the",
                "ACM 35 (1992) 61–70.",
            ],
        );
        first.lines.push(line_at("Page 30 of 35", 0, 250.0, 30.0));
        let mut second = column_page(
            31,
            &[
                "[2] H. Guo, R. Tang, Y. Ye, Z. Li, X. He, Deepfm: a factorization-",
                "machine based neural network for ctr prediction, arXiv preprint",
                "arXiv:1703.04247 (2017).",
            ],
        );
        second.lines.push(line_at("Page 31 of 35", 0, 250.0, 30.0));
        let (refs, _) = extract_citations(&[first, second]);
        assert_eq!(refs.len(), 2);
        assert!(refs[0].raw.ends_with("ACM 35 (1992) 61–70."));
        assert!(refs[1].raw.ends_with("arXiv:1703.04247 (2017)."));
        assert!(refs.iter().all(|r| !r.raw.contains("Page 3")));
        assert!(refs[1].raw.contains("factorization-machine"));
        assert_eq!(refs[1].arxiv_id.as_deref(), Some("1703.04247"));
        assert_eq!(refs[1].year, Some(2017));
        assert_eq!(refs[1].page, 31);
    }

    /// biblatex style (arXiv:2501.17300): `Authors (2005). “Title”. In:` — the
    /// quoted title after the year is read without its quotes; a title
    /// ending in `?` before the closing quote is complete.
    #[test]
    fn quoted_title_after_year_and_question_mark_titles() {
        let entry = parsed(
            "Pujol, J. M., J. Delgado, R. Sangüesa, and A. Flache (2005). “The role of \
             clustering on the emergence of eﬀicient social conventions”. In: Proceedings \
             of the 19th international joint conference on Artificial intelligence, pp. \
             965–970.",
            None,
        );
        assert_eq!(
            entry.title.as_deref(),
            Some("The role of clustering on the emergence of eﬀicient social conventions")
        );
        assert_eq!(entry.year, Some(2005));
        assert_eq!(entry.pages.as_deref(), Some("965–970"));
        assert_eq!(entry.authors[0], "Pujol, J. M.");

        let mary = parsed(
            "[8] P. Mary, J.-M. Gorce, A. Unsal, and H. V. Poor, “Finite blocklength \
             information theory: What is the practical impact on wireless communications?” \
             in 2016 IEEE Globecom Workshops (GC Wkshps), 2016, pp. 1–6.",
            Some("[8]"),
        );
        assert_eq!(
            mary.title.as_deref(),
            Some(
                "Finite blocklength information theory: What is the practical impact on \
                 wireless communications?"
            )
        );
        assert_eq!(mary.year, Some(2016));
        assert_eq!(mary.pages.as_deref(), Some("1–6"));

        let bianchi = parsed(
            "Federico Bianchi, Patrick John Chia, Mert Yuksekgonul, Jacopo Tagliabue, Dan \
             Jurafsky, and James Zou. 2024. How well can llms negotiate? negotiationarena \
             platform and analysis. arXiv preprint arXiv:2402.05863.",
            None,
        );
        assert_eq!(
            bianchi.title.as_deref(),
            Some("How well can llms negotiate? negotiationarena platform and analysis")
        );
        assert_eq!(bianchi.arxiv_id.as_deref(), Some("2402.05863"));
    }
}
