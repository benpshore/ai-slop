//! Document-level text cleanup, run after reading order and before metadata
//! and citations: running headers, footers and page numbers, the rotated
//! `arXiv` margin stamp, sub/superscript fragment lines and line-end
//! hyphenation.
//!
//! The pass edits `PageText::lines` and `PageText::text` only. Spans are
//! evidence and are never changed or removed:
//! - a removed line (furniture or stamp) stays in `lines`, moved after the
//!   body lines, and only leaves `text`; the page gets the warning
//!   `furniture removed: N`;
//! - a script line merged into its base line hands its span indices to
//!   that line;
//! - a hyphen join moves the first word of the next line onto the line that
//!   ends with the hyphen, in both line texts and page text.
//!
//! A page whose `text` is not its line texts joined by whitespace (so the
//! separators cannot be recovered) is left untouched. Lines after the last
//! one found in `text` are treated as furniture removed by an earlier run,
//! which keeps the pass idempotent.
//!
//! The pass also sets `Line::role` (it only tags; `text` keeps every line
//! that is not furniture): removed lines become `furniture`, table-of-contents
//! lines with dot leaders `toc`, lines opening with `Figure N:`-style labels
//! `caption`, and on page 1 the lines before the abstract `front` (the
//! standalone `Abstract` line itself `heading`). Only lines still tagged
//! `body` are retagged, except that `furniture` wins over any tag.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use regex::Regex;

use crate::schema::{BBox, Line, PageText};

/// Share of the page height at the top and at the bottom that holds
/// running headers, footers and page numbers.
const EDGE_BAND: f32 = 0.08;
/// Wider edge band for running heads set further from the edge (LNCS-style
/// classes put them 11-14 % down). A line in this band but outside
/// `EDGE_BAND` needs the stronger repetition evidence of [`strongly_repeated`].
const EDGE_BAND_WIDE: f32 = 0.15;
/// Pages of one parity (recto or verso) a running head must repeat on.
const PARITY_MIN_PAGES: usize = 3;
/// Share of the document's pages, as `numerator / denominator` (40 %), a
/// running head must repeat on regardless of parity, and never fewer than
/// `PARITY_MIN_PAGES` pages.
const SHARE_NUMERATOR: usize = 2;
const SHARE_DENOMINATOR: usize = 5;
/// The abstract must start within this many non-furniture lines of page 1
/// for the front-matter rule to use it.
const FRONT_MAX_LINES: usize = 60;
/// Dot-leader runs a table-of-contents line has at least.
const TOC_MIN_LEADERS: usize = 4;
const ROLE_BODY: &str = "body";
const ROLE_FURNITURE: &str = "furniture";
const ROLE_TOC: &str = "toc";
const ROLE_CAPTION: &str = "caption";
const ROLE_FRONT: &str = "front";
const ROLE_HEADING: &str = "heading";
/// A repeated edge line counts as furniture only up to this multiple of the
/// document's median span size (a display title is not a running head).
const HEADER_SIZE_SLACK: f32 = 1.1;
/// A script line is at most this share of its base line's font size.
const SCRIPT_RATIO: f32 = 0.75;
/// Minimum vertical overlap with the base line, as a share of the script
/// line's own height.
const SCRIPT_OVERLAP: f32 = 0.5;
/// Longest script fragment, in characters and in words.
const SCRIPT_MAX_CHARS: usize = 16;
const SCRIPT_MAX_WORDS: usize = 3;
/// A span is vertical text when its box is this many times taller than wide.
const VERTICAL_RATIO: f32 = 3.0;
/// Shortest line the vertical-text stamp rule applies to (a lone `l` or `1`
/// has a tall, narrow box too).
const VERTICAL_MIN_CHARS: usize = 10;
/// Hyphen characters that can end a line.
const HYPHENS: [char; 3] = ['-', '\u{2010}', '\u{00AD}'];
/// Opening punctuation allowed before a hyphenated word.
const OPENERS: [char; 7] = ['(', '[', '{', '"', '\'', '\u{201C}', '\u{2018}'];
/// Shortest word half that counts as attested on its own when deciding to
/// keep a line-end hyphen (`in-` + `formation` must still join).
const MIN_ATTESTED_HALF: usize = 3;
/// Prefixes that usually form real compounds (`self-supervised`); a line-end
/// hyphen after one is kept unless the joined word is attested.
const COMPOUND_PREFIXES: [&str; 25] = [
    "self", "non", "pre", "post", "co", "multi", "semi", "anti", "re", "low", "high", "well",
    "state", "end", "long", "short", "real", "time", "data", "open", "cross", "inter", "intra",
    "sub", "super",
];

/// What the cleanup pass changed, summed over the document.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CleanupReport {
    /// Edge lines whose digit-normalised text repeats on two or more pages.
    pub running_lines: usize,
    /// Lines in the wider edge band (outside the 8 % band) whose
    /// digit-normalised text repeats on three or more pages of one parity or
    /// on at least 40 % (and three or more) of the pages.
    pub running_lines_wide: usize,
    /// Edge lines that are only a page number.
    pub page_numbers: usize,
    /// `arXiv` margin stamps on page 1.
    pub stamps: usize,
    /// Sub/superscript lines merged into their base line.
    pub scripts_merged: usize,
    /// Line-end hyphens removed by joining the word halves.
    pub hyphens_joined: usize,
    /// Line-end hyphens kept as compounds.
    pub hyphens_kept: usize,
    /// Pages left untouched because `text` does not match `lines`.
    pub pages_skipped: usize,
    /// Lines newly tagged `furniture` (removed from `text`).
    pub role_furniture: usize,
    /// Lines newly tagged `toc`.
    pub role_toc: usize,
    /// Lines newly tagged `caption`.
    pub role_caption: usize,
    /// Page-1 lines newly tagged `front`.
    pub role_front: usize,
    /// Lines newly tagged `heading` (the standalone `Abstract` line).
    pub role_heading: usize,
}

/// Role of a line during the pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Body,
    Furniture,
    Merged,
}

/// Per-page working state.
struct PageWork {
    eligible: bool,
    /// Separator in `text` before each line found there.
    seps: Vec<String>,
    state: Vec<State>,
    changed: bool,
    /// Lines newly removed from `text` as furniture on this page.
    removed: usize,
}

impl PageWork {
    fn is_body(&self, index: usize) -> bool {
        self.state.get(index) == Some(&State::Body)
    }

    fn mark(&mut self, index: usize, state: State) {
        if let Some(slot) = self.state.get_mut(index) {
            *slot = state;
            self.changed = true;
        }
    }
}

/// Outcome of looking at one line-end hyphen.
enum Decision {
    NotApplicable,
    Keep,
    /// New texts of the first and the second line.
    Join(String, String),
}

/// Lower-cased words and hyphenated pairs seen in the document's body lines.
/// Word halves at a line-end hyphen (the last word before it and the first
/// word of the next body line) are not recorded as words, so a split
/// `cost-` / `effective` does not attest its own halves.
struct Vocabulary {
    words: BTreeSet<String>,
    compounds: BTreeSet<String>,
}

fn page_number_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)^(?:(?:page|p\.)\s*\d{1,4}(?:\s*(?:of|/)\s*\d{1,4})?|[-–—]?\s*\d{1,4}\s*[-–—]?|\d{1,4}\s*/\s*\d{1,4})$",
        )
        .expect("valid regex")
    })
}

fn roman_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^x{0,3}(?:ix|iv|v?i{0,3})$").expect("valid regex"))
}

fn stamp_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^arXiv:\d{4}\.\d{4,5}v\d+\s+\[[A-Za-z.\-]+\]\s+\d{1,2}\s+[A-Z][a-z]{2}\s+\d{4}$",
        )
        .expect("valid regex")
    })
}

fn abstract_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)^a\s?b\s?s\s?t\s?r\s?a\s?c\s?t\b").expect("valid regex"))
}

fn abstract_heading_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)^a\s?b\s?s\s?t\s?r\s?a\s?c\s?t\s*[.:\x{2014}\x{2013}-]?$")
            .expect("valid regex")
    })
}

fn introduction_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)^(?:(?:\d+|[ivx]+)\.?\s*)?introduction\s*$").expect("valid regex")
    })
}

fn caption_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^(?:Figure|FIGURE|Fig\.|FIG\.|Table|TABLE|Algorithm|ALGORITHM|Listing|LISTING)\s*(?:[A-Z]?\d+(?:\.\d+)*|[IVXL]+)\s*[.:|]",
        )
        .expect("valid regex")
    })
}

fn norm(b: BBox) -> BBox {
    BBox {
        x0: b.x0.min(b.x1),
        y0: b.y0.min(b.y1),
        x1: b.x0.max(b.x1),
        y1: b.y0.max(b.y1),
    }
}

fn union(a: BBox, b: BBox) -> BBox {
    BBox {
        x0: a.x0.min(b.x0),
        y0: a.y0.min(b.y0),
        x1: a.x1.max(b.x1),
        y1: a.y1.max(b.y1),
    }
}

/// Largest font size among the line's non-blank spans.
fn line_size(page: &PageText, line: &Line) -> Option<f32> {
    let mut best: Option<f32> = None;
    for idx in &line.spans {
        let Some(span) = page.spans.get(*idx as usize) else {
            continue;
        };
        if span.text.trim().is_empty() {
            continue;
        }
        if let Some(size) = span.size.filter(|s| s.is_finite() && *s > 0.0) {
            best = Some(best.map_or(size, |b| b.max(size)));
        }
    }
    best
}

/// Median font size of all non-blank spans in the document.
fn median_span_size(pages: &[PageText]) -> Option<f32> {
    let mut sizes: Vec<f32> = pages
        .iter()
        .flat_map(|page| page.spans.iter())
        .filter(|span| !span.text.trim().is_empty())
        .filter_map(|span| span.size)
        .filter(|s| s.is_finite() && *s > 0.0)
        .collect();
    if sizes.is_empty() {
        return None;
    }
    sizes.sort_unstable_by(f32::total_cmp);
    Some(sizes[sizes.len() / 2])
}

/// True when the line's box centre lies in the top or bottom `band` share
/// of an unrotated page.
fn in_edge_band(page: &PageText, line: &Line, band: f32) -> bool {
    if page.rotation != 0 || !page.height.is_finite() || page.height <= 0.0 {
        return false;
    }
    let Some(b) = line.bbox.map(norm) else {
        return false;
    };
    let centre = b.y0.midpoint(b.y1);
    centre >= page.height * (1.0 - band) || centre <= page.height * band
}

/// Rule 1 evidence for a line in the wide edge band: its key repeats on
/// `PARITY_MIN_PAGES` pages of one parity (running heads alternate between
/// recto and verso), or on at least 40 % of the document's `total` pages and
/// never fewer than `PARITY_MIN_PAGES`.
fn strongly_repeated(pages_seen: &BTreeSet<u32>, total: usize) -> bool {
    let odd = pages_seen.iter().filter(|n| **n % 2 == 1).count();
    let even = pages_seen.len() - odd;
    let share = pages_seen.len() >= PARITY_MIN_PAGES
        && pages_seen.len() * SHARE_DENOMINATOR >= total * SHARE_NUMERATOR;
    odd >= PARITY_MIN_PAGES || even >= PARITY_MIN_PAGES || share
}

/// Text with every run of ASCII digits replaced by `#` and whitespace
/// collapsed, so `Journal 12` and `Journal 13` compare equal.
fn digit_key(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (w, word) in text.split_whitespace().enumerate() {
        if w > 0 {
            out.push(' ');
        }
        let mut in_digits = false;
        for c in word.chars() {
            if c.is_ascii_digit() {
                if !in_digits {
                    out.push('#');
                }
                in_digits = true;
            } else {
                out.push(c);
                in_digits = false;
            }
        }
    }
    out
}

/// A bare page number: `12`, `- 12 -`, `Page 12 of 30`, `12 / 30`, or a
/// lower-case roman numeral up to `xxxix`.
fn is_page_number(text: &str) -> bool {
    let text = text.trim();
    page_number_re().is_match(text) || (!text.is_empty() && roman_re().is_match(text))
}

/// The `arXiv` margin stamp: its exact text, or a line of at least
/// `VERTICAL_MIN_CHARS` characters whose spans are all vertical.
fn is_stamp(page: &PageText, line: &Line) -> bool {
    let text = line.text.trim();
    if stamp_re().is_match(text) {
        return true;
    }
    if text.chars().count() < VERTICAL_MIN_CHARS {
        return false;
    }
    let mut seen = false;
    for idx in &line.spans {
        let Some(span) = page.spans.get(*idx as usize) else {
            return false;
        };
        if span.text.trim().is_empty() {
            continue;
        }
        let Some(b) = span.bbox.map(norm) else {
            return false;
        };
        if b.y1 - b.y0 <= VERTICAL_RATIO * (b.x1 - b.x0) {
            return false;
        }
        seen = true;
    }
    seen
}

/// Separator before each line in `page.text` and the number of lines found
/// there, in order. `None` when `text` holds anything else.
fn separators(page: &PageText) -> Option<(Vec<String>, usize)> {
    let text = page.text.as_str();
    let mut seps: Vec<String> = Vec::with_capacity(page.lines.len());
    let mut cursor: usize = 0;
    for line in &page.lines {
        let rest = text.get(cursor..)?;
        let body = rest.trim_start();
        if !body.starts_with(line.text.as_str()) {
            break;
        }
        let gap = rest.len() - body.len();
        seps.push(rest[..gap].to_string());
        cursor += gap + line.text.len();
    }
    if !text.get(cursor..)?.trim().is_empty() {
        return None;
    }
    let found = seps.len();
    Some((seps, found))
}

fn prepare(page: &PageText) -> PageWork {
    match separators(page) {
        Some((seps, found)) => {
            let mut state = vec![State::Body; found];
            state.resize(page.lines.len(), State::Furniture);
            PageWork {
                eligible: true,
                seps,
                state,
                changed: false,
                removed: 0,
            }
        }
        None => PageWork {
            eligible: false,
            seps: Vec::new(),
            state: vec![State::Body; page.lines.len()],
            changed: false,
            removed: 0,
        },
    }
}

/// Rule 4: the `arXiv` stamp on page 1 leaves the text.
fn mark_stamps(pages: &[PageText], work: &mut [PageWork], report: &mut CleanupReport) {
    for (page, w) in pages.iter().zip(work.iter_mut()) {
        if page.page != 1 || !w.eligible {
            continue;
        }
        for (k, line) in page.lines.iter().enumerate() {
            if w.is_body(k) && is_stamp(page, line) {
                w.mark(k, State::Furniture);
                w.removed += 1;
                report.stamps += 1;
            }
        }
    }
}

/// Rule 1: page numbers and running headers/footers in the edge bands. A
/// line in the 8 % band goes when its digit-normalised text repeats on two
/// or more pages; a line further in, up to 15 %, only when the repetition is
/// strong (see [`strongly_repeated`]).
fn mark_furniture(pages: &[PageText], work: &mut [PageWork], report: &mut CleanupReport) {
    let body_size = median_span_size(pages);
    let mut seen: BTreeMap<String, BTreeSet<u32>> = BTreeMap::new();
    // (page index, line index, key, inside the narrow band)
    let mut candidates: Vec<(usize, usize, String, bool)> = Vec::new();
    let mut numbers: Vec<(usize, usize)> = Vec::new();
    for (p, (page, w)) in pages.iter().zip(work.iter()).enumerate() {
        if !w.eligible {
            continue;
        }
        for (k, line) in page.lines.iter().enumerate() {
            if !w.is_body(k) || !in_edge_band(page, line, EDGE_BAND_WIDE) {
                continue;
            }
            let narrow = in_edge_band(page, line, EDGE_BAND);
            if narrow && is_page_number(&line.text) {
                numbers.push((p, k));
                continue;
            }
            let key = digit_key(&line.text);
            if !key.chars().any(char::is_alphabetic) {
                continue;
            }
            let small = match (line_size(page, line), body_size) {
                (Some(size), Some(body)) => size <= HEADER_SIZE_SLACK * body,
                _ => true,
            };
            if small {
                seen.entry(key.clone()).or_default().insert(page.page);
                candidates.push((p, k, key, narrow));
            }
        }
    }
    let total = pages.len();
    for (p, k, key, narrow) in candidates {
        let Some(pages_seen) = seen.get(&key) else {
            continue;
        };
        let repeated = if narrow {
            pages_seen.len() >= 2
        } else {
            strongly_repeated(pages_seen, total)
        };
        if repeated && let Some(w) = work.get_mut(p) {
            w.mark(k, State::Furniture);
            w.removed += 1;
            if narrow {
                report.running_lines += 1;
            } else {
                report.running_lines_wide += 1;
            }
        }
    }
    for (p, k) in numbers {
        let alone = pages
            .get(p)
            .zip(work.get(p))
            .is_some_and(|(page, w)| alone_on_row(page, w, k));
        if alone && let Some(w) = work.get_mut(p) {
            w.mark(k, State::Furniture);
            w.removed += 1;
            report.page_numbers += 1;
        }
    }
}

/// True when no body line other than `index` shares its printed row (a
/// page number sits alone; `2026` at the end of an OCR'd title does not).
/// Lines already marked as running headers do not count.
fn alone_on_row(page: &PageText, w: &PageWork, index: usize) -> bool {
    let Some(own) = page.lines.get(index).and_then(|line| line.bbox).map(norm) else {
        return false;
    };
    let own_height = own.y1 - own.y0;
    page.lines.iter().enumerate().all(|(j, other)| {
        if j == index || !w.is_body(j) {
            return true;
        }
        let Some(b) = other.bbox.map(norm) else {
            return true;
        };
        let overlap = own.y1.min(b.y1) - own.y0.max(b.y0);
        overlap < 0.5 * own_height.min(b.y1 - b.y0)
    })
}

/// Nearest body line before (`forward == false`) or after `index`.
fn neighbour(w: &PageWork, index: usize, forward: bool) -> Option<usize> {
    if forward {
        (index + 1..w.state.len()).find(|j| w.is_body(*j))
    } else {
        (0..index).rev().find(|j| w.is_body(*j))
    }
}

/// Rule 3: the body line a script fragment at `index` belongs to, if any.
fn script_target(page: &PageText, w: &PageWork, index: usize) -> Option<usize> {
    let line = page.lines.get(index)?;
    let text = line.text.trim();
    if text.is_empty()
        || text.chars().count() > SCRIPT_MAX_CHARS
        || text.split_whitespace().count() > SCRIPT_MAX_WORDS
    {
        return None;
    }
    let bbox = norm(line.bbox?);
    let size = line_size(page, line)?;
    let height = (bbox.y1 - bbox.y0).max(f32::EPSILON);
    let mut best: Option<(usize, f32)> = None;
    let candidates = [neighbour(w, index, true), neighbour(w, index, false)];
    for cand in candidates.into_iter().flatten() {
        let Some(other) = page.lines.get(cand) else {
            continue;
        };
        if other.column != line.column {
            continue;
        }
        let (Some(ob), Some(other_size)) = (other.bbox.map(norm), line_size(page, other)) else {
            continue;
        };
        if size > SCRIPT_RATIO * other_size {
            continue;
        }
        let overlap = bbox.y1.min(ob.y1) - bbox.y0.max(ob.y0);
        if overlap < SCRIPT_OVERLAP * height {
            continue;
        }
        if bbox.x1 < ob.x0 - other_size || bbox.x0 > ob.x1 + other_size {
            continue;
        }
        let share = overlap / height;
        if best.is_none_or(|(_, s)| share > s) {
            best = Some((cand, share));
        }
    }
    best.map(|(cand, _)| cand)
}

/// Byte offset in `line.text` and slot in `line.spans` where content that
/// starts at `x0` goes: before the first span starting at or right of
/// `x0`, else at the end. Spans whose text cannot be located (composed
/// accents) are skipped.
fn insertion_point(page: &PageText, line: &Line, x0: f32) -> (usize, usize) {
    let mut cursor: usize = 0;
    for (slot, idx) in line.spans.iter().enumerate() {
        let Some(span) = page.spans.get(*idx as usize) else {
            continue;
        };
        let piece = span.text.trim();
        if piece.is_empty() {
            continue;
        }
        let Some(rel) = line.text.get(cursor..).and_then(|rest| rest.find(piece)) else {
            continue;
        };
        let start = cursor + rel;
        if span.bbox.is_some_and(|b| norm(b).x0 >= x0) {
            return (start, slot);
        }
        cursor = start + piece.len();
    }
    (line.text.len(), line.spans.len())
}

/// Insert the script line `script` into line `target` at its horizontal
/// position. A space separates it from a letter or digit on either side,
/// so `G` + raised `hom` + `(A)` reads `G hom(A)`.
fn merge_script(page: &mut PageText, script: usize, target: usize) {
    let Some(line) = page.lines.get(script) else {
        return;
    };
    let text = line.text.trim().to_string();
    let spans = line.spans.clone();
    let bbox = line.bbox;
    let x0 = bbox.map_or(f32::INFINITY, |b| norm(b).x0);
    let Some(base) = page.lines.get(target) else {
        return;
    };
    let (byte, slot) = insertion_point(page, base, x0);
    let Some(base) = page.lines.get_mut(target) else {
        return;
    };
    let before = base.text.get(..byte).and_then(|s| s.chars().next_back());
    let after = base.text.get(byte..).and_then(|s| s.chars().next());
    let mut piece = String::new();
    if before.is_some_and(char::is_alphanumeric) {
        piece.push(' ');
    }
    piece.push_str(&text);
    if after.is_some_and(char::is_alphanumeric) {
        piece.push(' ');
    }
    base.text.insert_str(byte, &piece);
    let slot = slot.min(base.spans.len());
    let mut merged: Vec<u32> = base.spans[..slot].to_vec();
    merged.extend(spans);
    merged.extend_from_slice(&base.spans[slot..]);
    base.spans = merged;
    base.bbox = match (base.bbox, bbox) {
        (Some(a), Some(b)) => Some(union(norm(a), norm(b))),
        (a, b) => a.or(b),
    };
}

/// Rule 3 over one page; returns the number of merged script lines.
fn merge_scripts(page: &mut PageText, w: &mut PageWork) -> usize {
    let mut merged = 0;
    for k in 0..page.lines.len() {
        if !w.is_body(k) {
            continue;
        }
        if let Some(target) = script_target(page, w, k) {
            merge_script(page, k, target);
            w.mark(k, State::Merged);
            merged += 1;
        }
    }
    merged
}

/// Words and hyphenated word pairs of every body line, lower-cased, in
/// reading order (see [`Vocabulary`] for the halves left out).
fn vocabulary(pages: &[PageText], work: &[PageWork]) -> Vocabulary {
    let mut vocab = Vocabulary {
        words: BTreeSet::new(),
        compounds: BTreeSet::new(),
    };
    let alphabetic = |piece: &str| !piece.is_empty() && piece.chars().all(char::is_alphabetic);
    let mut after_hyphen = false;
    for (page, w) in pages.iter().zip(work) {
        if !w.eligible {
            after_hyphen = false;
            continue;
        }
        for (k, line) in page.lines.iter().enumerate() {
            if !w.is_body(k) {
                continue;
            }
            let ends_hyphen = strip_final_hyphen(&line.text).is_some();
            let tokens: Vec<&str> = line.text.split_whitespace().collect();
            let last_token = tokens.len().saturating_sub(1);
            for (t, token) in tokens.iter().enumerate() {
                let core = token.trim_matches(|c: char| !c.is_alphanumeric());
                let parts: Vec<&str> = core.split(HYPHENS).collect();
                let last_part = parts.len().saturating_sub(1);
                for (i, part) in parts.iter().enumerate() {
                    let words: Vec<&str> = part
                        .split(|c: char| !c.is_alphabetic())
                        .filter(|word| !word.is_empty())
                        .collect();
                    let last_word = words.len().saturating_sub(1);
                    for (j, word) in words.iter().enumerate() {
                        let split_head = after_hyphen && t == 0 && i == 0 && j == 0;
                        let split_tail =
                            ends_hyphen && t == last_token && i == last_part && j == last_word;
                        if !split_head && !split_tail {
                            vocab.words.insert(word.to_lowercase());
                        }
                    }
                }
                for pair in parts.windows(2) {
                    if alphabetic(pair[0]) && alphabetic(pair[1]) {
                        let first = pair[0].to_lowercase();
                        let second = pair[1].to_lowercase();
                        vocab.compounds.insert(format!("{first}-{second}"));
                    }
                }
            }
            after_hyphen = ends_hyphen;
        }
    }
    vocab
}

/// `text` without its final hyphen, when it ends with one.
fn strip_final_hyphen(text: &str) -> Option<&str> {
    let text = text.trim_end();
    let last = text.chars().next_back()?;
    if HYPHENS.contains(&last) {
        text.get(..text.len() - last.len_utf8())
    } else {
        None
    }
}

/// Rule 2 for one pair of consecutive lines. The halves join when the
/// joined word is attested; otherwise the hyphen stays when the hyphenated
/// pair is attested, when both halves are attested as whole words
/// (`cost-` / `effective`), or when the left half is a compound prefix; any
/// other split (`algo-` / `rithm`) joins.
fn hyphen_decision(first: &str, second: &str, vocab: &Vocabulary) -> Decision {
    let Some(stem) = strip_final_hyphen(first) else {
        return Decision::NotApplicable;
    };
    let token = stem.rsplit(char::is_whitespace).next().unwrap_or(stem);
    let word = token.trim_start_matches(OPENERS);
    if word.is_empty() || !word.chars().all(char::is_alphabetic) {
        return Decision::NotApplicable;
    }
    let rest = second.trim_start();
    let Some(head) = rest.split_whitespace().next() else {
        return Decision::NotApplicable;
    };
    if !head.chars().next().is_some_and(char::is_lowercase) {
        return Decision::NotApplicable;
    }
    let suffix: String = head.chars().take_while(|c| c.is_alphabetic()).collect();
    if suffix.is_empty() {
        return Decision::NotApplicable;
    }
    let left = word.to_lowercase();
    let right = suffix.to_lowercase();
    let joined = format!("{left}{right}");
    let hyphenated = format!("{left}-{right}");
    let whole_word =
        |half: &str| half.chars().count() >= MIN_ATTESTED_HALF && vocab.words.contains(half);
    let attested = vocab.words.contains(&joined);
    let compound = vocab.compounds.contains(&hyphenated)
        || (whole_word(left.as_str()) && whole_word(right.as_str()))
        || COMPOUND_PREFIXES.contains(&left.as_str());
    if attested || !compound {
        let tail = rest
            .get(head.len()..)
            .unwrap_or("")
            .trim_start()
            .to_string();
        Decision::Join(format!("{stem}{head}"), tail)
    } else {
        Decision::Keep
    }
}

fn line_at(pages: &[PageText], at: (usize, usize)) -> Option<&Line> {
    pages.get(at.0).and_then(|page| page.lines.get(at.1))
}

fn line_at_mut(pages: &mut [PageText], at: (usize, usize)) -> Option<&mut Line> {
    pages
        .get_mut(at.0)
        .and_then(|page| page.lines.get_mut(at.1))
}

/// Whether a separator in `text` is a paragraph break (two or more line
/// breaks).
fn is_paragraph_break(sep: &str) -> bool {
    sep.matches('\n').count() >= 2
}

/// Rule 2 decision for body line `second` following body line `first`:
/// same page and column with no paragraph break between them in `text`,
/// or the last and first body lines of consecutive pages.
fn plan_join(
    pages: &[PageText],
    work: &[PageWork],
    first: (usize, usize),
    second: (usize, usize),
    vocab: &Vocabulary,
) -> Decision {
    let (Some(a), Some(b)) = (line_at(pages, first), line_at(pages, second)) else {
        return Decision::NotApplicable;
    };
    let continues = if first.0 == second.0 {
        let paragraph_break = work.get(first.0).is_some_and(|w| {
            (first.1 + 1..=second.1)
                .filter_map(|k| w.seps.get(k))
                .any(|sep| is_paragraph_break(sep))
        });
        a.column == b.column && !paragraph_break
    } else {
        let earlier_number = pages.get(first.0).map(|page| page.page);
        let later_number = pages.get(second.0).map(|page| page.page);
        earlier_number
            .and_then(|n| n.checked_add(1))
            .is_some_and(|n| Some(n) == later_number)
    };
    if !continues {
        return Decision::NotApplicable;
    }
    hyphen_decision(&a.text, &b.text, vocab)
}

/// Rule 2 over the document, in reading order.
fn join_hyphens(
    pages: &mut [PageText],
    work: &mut [PageWork],
    vocab: &Vocabulary,
    report: &mut CleanupReport,
) {
    let mut prev: Option<(usize, usize)> = None;
    for page_idx in 0..pages.len() {
        let eligible = work.get(page_idx).is_some_and(|w| w.eligible);
        if !eligible {
            prev = None;
            continue;
        }
        let line_count = pages.get(page_idx).map_or(0, |page| page.lines.len());
        for line_idx in 0..line_count {
            let current = (page_idx, line_idx);
            if !work.get(page_idx).is_some_and(|w| w.is_body(line_idx)) {
                continue;
            }
            let mut consumed = false;
            if let Some(earlier) = prev {
                match plan_join(pages, work, earlier, current, vocab) {
                    Decision::NotApplicable => {}
                    Decision::Keep => report.hyphens_kept += 1,
                    Decision::Join(first_text, second_text) => {
                        report.hyphens_joined += 1;
                        consumed = second_text.is_empty();
                        if let Some(line) = line_at_mut(pages, earlier) {
                            line.text = first_text;
                        }
                        let mut moved: Vec<u32> = Vec::new();
                        if let Some(line) = line_at_mut(pages, current) {
                            line.text = second_text;
                            if consumed && earlier.0 == page_idx {
                                moved = std::mem::take(&mut line.spans);
                            }
                        }
                        if let Some(line) = line_at_mut(pages, earlier) {
                            line.spans.extend(moved);
                        }
                        if let Some(w) = work.get_mut(earlier.0) {
                            w.changed = true;
                        }
                        if let Some(w) = work.get_mut(page_idx) {
                            w.changed = true;
                            if consumed {
                                w.mark(line_idx, State::Merged);
                            }
                        }
                    }
                }
            }
            if !consumed {
                prev = Some(current);
            }
        }
    }
}

/// The separator with more line breaks (a paragraph break wins).
fn stronger<'a>(a: &'a str, b: &'a str) -> &'a str {
    if b.matches('\n').count() > a.matches('\n').count() {
        b
    } else {
        a
    }
}

/// Rebuild `lines` (body lines, then furniture) and `text` (body lines
/// only, with their original separators).
fn rebuild(page: &mut PageText, w: &PageWork) {
    let old = std::mem::take(&mut page.lines);
    let mut body: Vec<Line> = Vec::with_capacity(old.len());
    let mut furniture: Vec<Line> = Vec::new();
    let mut text = String::new();
    let mut pending: &str = "";
    for (k, line) in old.into_iter().enumerate() {
        let state = w.state.get(k).copied().unwrap_or(State::Furniture);
        let sep = w.seps.get(k).map_or("", String::as_str);
        match state {
            State::Body => {
                if !body.is_empty() {
                    text.push_str(stronger(pending, sep));
                }
                pending = "";
                text.push_str(&line.text);
                body.push(line);
            }
            State::Furniture => {
                pending = stronger(pending, sep);
                furniture.push(line);
            }
            State::Merged => {
                pending = stronger(pending, sep);
            }
        }
    }
    body.extend(furniture);
    page.lines = body;
    page.text = text;
}

/// Set `line.role` to `role` when the line is still `body` (or untagged), or
/// when `role` is `furniture`. True when the role changed.
fn tag(line: &mut Line, role: &str) -> bool {
    if line.role == role {
        return false;
    }
    let untagged = line.role.is_empty() || line.role == ROLE_BODY;
    if untagged || role == ROLE_FURNITURE {
        line.role = role.to_string();
        true
    } else {
        false
    }
}

/// Tag every line of `page` whose state is `Furniture` (before `rebuild`
/// moves them), counting the new tags.
fn tag_furniture(page: &mut PageText, w: &PageWork, report: &mut CleanupReport) {
    for (k, line) in page.lines.iter_mut().enumerate() {
        if w.state.get(k) == Some(&State::Furniture) && tag(line, ROLE_FURNITURE) {
            report.role_furniture += 1;
        }
    }
}

/// A table-of-contents entry: at least `TOC_MIN_LEADERS` dot-leader runs
/// (` .`, `..` or `…`) and a page number at the end.
fn is_toc(text: &str) -> bool {
    let text = text.trim();
    if !text.chars().next_back().is_some_and(|c| c.is_ascii_digit()) {
        return false;
    }
    let leaders =
        text.matches(" .").count() + text.matches("..").count() + text.matches('\u{2026}').count();
    leaders >= TOC_MIN_LEADERS
}

/// A caption's first line: `Figure`, `Fig.`, `Table`, `Algorithm` or
/// `Listing`, a number (`3`, `S1`, `A1`, `2.1`, `IV`) and then `.`, `:` or
/// `|`.
fn is_caption(text: &str) -> bool {
    caption_re().is_match(text.trim())
}

/// Page-1 front matter: the non-furniture lines before the abstract when it
/// starts within `FRONT_MAX_LINES` lines (a standalone `Abstract` line is
/// tagged `heading`), else those before an `Introduction` heading.
fn tag_front(page: &mut PageText, report: &mut CleanupReport) {
    let order: Vec<usize> = page
        .lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.role != ROLE_FURNITURE)
        .map(|(k, _)| k)
        .collect();
    let text_of = |k: usize| page.lines.get(k).map_or("", |line| line.text.trim());
    let abstract_at = order
        .iter()
        .take(FRONT_MAX_LINES)
        .position(|k| abstract_re().is_match(text_of(*k)));
    let (end, heading) = if let Some(pos) = abstract_at {
        let standalone = order
            .get(pos)
            .copied()
            .filter(|k| abstract_heading_re().is_match(text_of(*k)));
        (pos, standalone)
    } else if let Some(pos) = order
        .iter()
        .position(|k| introduction_re().is_match(text_of(*k)))
    {
        (pos, None)
    } else {
        return;
    };
    for k in order.iter().take(end) {
        if let Some(line) = page.lines.get_mut(*k)
            && tag(line, ROLE_FRONT)
        {
            report.role_front += 1;
        }
    }
    if let Some(k) = heading
        && let Some(line) = page.lines.get_mut(k)
        && tag(line, ROLE_HEADING)
    {
        report.role_heading += 1;
    }
}

/// Text-based roles on the final lines: `toc`, `caption`, and page-1
/// `front`/`heading`. Never changes `text`.
fn tag_roles(page: &mut PageText, report: &mut CleanupReport) {
    for line in &mut page.lines {
        if line.role == ROLE_FURNITURE {
            continue;
        }
        if is_toc(&line.text) {
            if tag(line, ROLE_TOC) {
                report.role_toc += 1;
            }
        } else if is_caption(&line.text) && tag(line, ROLE_CAPTION) {
            report.role_caption += 1;
        }
    }
    if page.page == 1 {
        tag_front(page, report);
    }
}

/// Clean the ordered text of a whole document in place: rule 4 (`arXiv`
/// stamp on page 1), rule 1 (page numbers and running headers/footers in
/// the top or bottom 8 % of the page, or up to 15 % for strongly repeated
/// running heads), rule 3 (sub/superscript fragments
/// merged into their base line) and rule 2 (line-end hyphenation within a
/// column and across a page break), in that order; then tags line roles
/// (`furniture`, `toc`, `caption`, page-1 `front` and `heading`) without
/// changing `text`. Requires `lines` and `text` from `reading_order`; never
/// touches `spans`. See the module documentation for how removed lines are
/// kept.
pub fn clean_document(pages: &mut [PageText]) -> CleanupReport {
    let mut report = CleanupReport::default();
    let mut work: Vec<PageWork> = pages.iter().map(prepare).collect();
    for (page, w) in pages.iter().zip(&work) {
        if !w.eligible && !page.lines.is_empty() {
            report.pages_skipped += 1;
        }
    }
    mark_stamps(pages, &mut work, &mut report);
    mark_furniture(pages, &mut work, &mut report);
    for (page, w) in pages.iter_mut().zip(work.iter_mut()) {
        if w.eligible {
            report.scripts_merged += merge_scripts(page, w);
        }
    }
    let vocab = vocabulary(pages, &work);
    join_hyphens(pages, &mut work, &vocab, &mut report);
    for (page, w) in pages.iter_mut().zip(&work) {
        if w.eligible {
            tag_furniture(page, w, &mut report);
        }
        if !w.changed {
            continue;
        }
        rebuild(page, w);
        if w.removed > 0 {
            let removed = w.removed;
            let msg = format!("furniture removed: {removed}");
            if !page.warnings.contains(&msg) {
                page.warnings.push(msg);
            }
        }
    }
    for page in pages.iter_mut() {
        tag_roles(page, &mut report);
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::Span;

    fn span_at(text: &str, x0: f32, y0: f32, size: f32, seq: u32) -> Span {
        let width = 0.5 * size * text.chars().count() as f32;
        Span {
            text: text.to_string(),
            bbox: Some(BBox {
                x0,
                y0,
                x1: x0 + width,
                y1: y0 + size,
            }),
            font: None,
            size: Some(size),
            seq,
        }
    }

    fn joined(lines: &[Line]) -> String {
        let texts: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
        texts.join("\n")
    }

    /// A US Letter page with one 10 pt span and one line per row
    /// `(text, x0, y0, column)`; `text` is the line texts joined by `\n`.
    fn page_of(number: u32, rows: &[(&str, f32, f32, u32)]) -> PageText {
        let mut page = PageText::new(number, 612.0, 792.0, 0);
        for (i, (text, x0, y0, column)) in rows.iter().enumerate() {
            let seq = u32::try_from(i).unwrap();
            let span = span_at(text, *x0, *y0, 10.0, seq);
            page.lines.push(Line {
                text: (*text).to_string(),
                bbox: span.bbox,
                column: *column,
                spans: vec![seq],
                role: ROLE_BODY.to_string(),
            });
            page.spans.push(span);
        }
        page.text = joined(&page.lines);
        page
    }

    fn texts(page: &PageText) -> Vec<&str> {
        page.lines.iter().map(|l| l.text.as_str()).collect()
    }

    fn furniture_pages() -> Vec<PageText> {
        (1..=3)
            .map(|n| {
                let header = format!("Journal of Testing, vol. {n}");
                let body = format!("Body line one of page {n}");
                let number = n.to_string();
                page_of(
                    n,
                    &[
                        (header.as_str(), 60.0, 760.0, 0),
                        (body.as_str(), 60.0, 600.0, 1),
                        ("Body line two", 60.0, 588.0, 1),
                        (number.as_str(), 300.0, 30.0, 2),
                    ],
                )
            })
            .collect()
    }

    #[test]
    fn page_number_forms() {
        assert!(is_page_number("12"));
        assert!(is_page_number("- 12 -"));
        assert!(is_page_number("– 7 –"));
        assert!(is_page_number("Page 12 of 30"));
        assert!(is_page_number("12 / 30"));
        assert!(is_page_number("iv"));
        assert!(is_page_number("xii"));
        assert!(!is_page_number("civil"));
        assert!(!is_page_number("ill"));
        assert!(!is_page_number("12 apples"));
        assert!(!is_page_number("Figure 3"));
        assert!(!is_page_number(""));
        assert_eq!(digit_key("Journal  12, vol. 345"), "Journal #, vol. #");
    }

    #[test]
    fn running_header_and_page_number_leave_the_text_only() {
        let mut pages = furniture_pages();
        let spans_before: Vec<Vec<Span>> = pages.iter().map(|p| p.spans.clone()).collect();
        let report = clean_document(&mut pages);
        assert_eq!(report.running_lines, 3);
        assert_eq!(report.page_numbers, 3);
        for (i, page) in pages.iter().enumerate() {
            let n = i + 1;
            assert_eq!(
                page.text,
                format!("Body line one of page {n}\nBody line two")
            );
            assert_eq!(page.lines.len(), 4, "furniture stays in lines");
            assert_eq!(page.lines[2].text, format!("Journal of Testing, vol. {n}"));
            assert_eq!(page.lines[3].text, n.to_string());
            assert_eq!(page.lines[0].role, "body");
            assert_eq!(page.lines[2].role, "furniture");
            assert_eq!(page.lines[3].role, "furniture");
            assert!(page.warnings.contains(&"furniture removed: 2".to_string()));
            assert_eq!(page.spans, spans_before[i], "spans are evidence");
        }
    }

    #[test]
    fn number_sharing_its_row_with_text_is_not_a_page_number() {
        let mut pages = vec![page_of(
            1,
            &[
                ("HELLO WORLD SCAN", 60.0, 760.0, 0),
                ("2026", 250.0, 760.0, 0),
                ("Body text", 60.0, 600.0, 0),
            ],
        )];
        let before = pages[0].text.clone();
        let report = clean_document(&mut pages);
        assert_eq!(report.page_numbers, 0);
        assert_eq!(pages[0].text, before);
    }

    #[test]
    fn cleanup_is_idempotent() {
        let mut pages = furniture_pages();
        clean_document(&mut pages);
        let once = pages.clone();
        let report = clean_document(&mut pages);
        assert_eq!(report, CleanupReport::default());
        assert_eq!(pages, once);
    }

    #[test]
    fn single_edge_line_stays() {
        let mut pages = vec![
            page_of(
                1,
                &[
                    ("A Title In The Top Band", 60.0, 760.0, 0),
                    ("Body text on page one", 60.0, 600.0, 1),
                ],
            ),
            page_of(2, &[("Body text on page two", 60.0, 600.0, 0)]),
        ];
        let before: Vec<String> = pages.iter().map(|p| p.text.clone()).collect();
        let report = clean_document(&mut pages);
        assert_eq!(report, CleanupReport::default());
        assert_eq!(pages[0].text, before[0]);
        assert_eq!(pages[1].text, before[1]);
        assert!(pages[0].warnings.is_empty());
    }

    #[test]
    fn hyphen_joins_when_the_word_is_attested() {
        let mut pages = vec![page_of(
            1,
            &[
                ("We provide a comprehen-", 60.0, 600.0, 0),
                ("sive review of the field.", 60.0, 588.0, 0),
                ("A comprehensive survey follows.", 60.0, 576.0, 0),
            ],
        )];
        let report = clean_document(&mut pages);
        assert_eq!(report.hyphens_joined, 1);
        assert_eq!(
            pages[0].text,
            "We provide a comprehensive\nreview of the field.\nA comprehensive survey follows."
        );
        assert_eq!(
            &texts(&pages[0])[..2],
            ["We provide a comprehensive", "review of the field."]
        );
    }

    #[test]
    fn hyphen_joins_without_evidence_unless_a_compound_prefix() {
        let mut pages = vec![page_of(
            1,
            &[
                ("the proces-", 60.0, 600.0, 0),
                ("sing step is fast, and a self-", 60.0, 588.0, 0),
                ("supervised model is used.", 60.0, 576.0, 0),
            ],
        )];
        let report = clean_document(&mut pages);
        assert_eq!(report.hyphens_joined, 1);
        assert_eq!(report.hyphens_kept, 1);
        assert_eq!(
            pages[0].text,
            "the processing\nstep is fast, and a self-\nsupervised model is used."
        );
    }

    #[test]
    fn attested_hyphenated_form_keeps_the_hyphen() {
        let mut pages = vec![page_of(
            1,
            &[
                ("we train the gas-", 60.0, 600.0, 0),
                ("leak detector on a gas-leak dataset.", 60.0, 588.0, 0),
            ],
        )];
        let before = pages[0].text.clone();
        let report = clean_document(&mut pages);
        assert_eq!(report.hyphens_kept, 1);
        assert_eq!(report.hyphens_joined, 0);
        assert_eq!(pages[0].text, before);
    }

    #[test]
    fn hyphen_stays_when_both_halves_are_words_elsewhere() {
        let mut pages = vec![page_of(
            1,
            &[
                ("a cost-", 60.0, 600.0, 0),
                ("effective method at low cost.", 60.0, 588.0, 0),
                ("It is effective in practice.", 60.0, 576.0, 0),
                ("the Dual-", 60.0, 564.0, 0),
                ("channel design uses a dual layout", 60.0, 552.0, 0),
                ("and one channel per link.", 60.0, 540.0, 0),
            ],
        )];
        let before = pages[0].text.clone();
        let report = clean_document(&mut pages);
        assert_eq!(report.hyphens_kept, 2);
        assert_eq!(report.hyphens_joined, 0);
        assert_eq!(pages[0].text, before);
        assert_eq!(texts(&pages[0])[0], "a cost-");
        assert_eq!(texts(&pages[0])[3], "the Dual-");
    }

    #[test]
    fn hyphen_joins_when_attested_or_when_halves_are_not_words() {
        let mut pages = vec![page_of(
            1,
            &[
                ("the opti-", 60.0, 600.0, 0),
                ("mization step runs first.", 60.0, 588.0, 0),
                ("Our optimization is fast.", 60.0, 576.0, 0),
                ("the algo-", 60.0, 564.0, 0),
                ("rithm ends.", 60.0, 552.0, 0),
            ],
        )];
        let report = clean_document(&mut pages);
        assert_eq!(report.hyphens_joined, 2);
        assert_eq!(report.hyphens_kept, 0);
        assert_eq!(
            pages[0].text,
            "the optimization\nstep runs first.\nOur optimization is fast.\n\
             the algorithm\nends."
        );
    }

    #[test]
    fn hyphen_is_not_joined_across_a_paragraph_break() {
        let mut pages = vec![page_of(
            1,
            &[
                ("a list ends with comprehen-", 60.0, 600.0, 0),
                ("sive text starts a new paragraph.", 60.0, 580.0, 0),
            ],
        )];
        pages[0].text =
            "a list ends with comprehen-\n\nsive text starts a new paragraph.".to_string();
        let before = pages[0].text.clone();
        let report = clean_document(&mut pages);
        assert_eq!(report.hyphens_joined, 0);
        assert_eq!(report.hyphens_kept, 0);
        assert_eq!(pages[0].text, before);
    }

    #[test]
    fn hyphen_is_not_joined_across_columns_urls_or_capitals() {
        let mut pages = vec![page_of(
            1,
            &[
                ("a comprehen-", 60.0, 600.0, 0),
                (
                    "sive review, see https://example.org/data-",
                    320.0,
                    600.0,
                    1,
                ),
                ("set for the files of the Proto-", 320.0, 588.0, 1),
                ("Indo text.", 320.0, 576.0, 1),
            ],
        )];
        let before = pages[0].text.clone();
        let report = clean_document(&mut pages);
        assert_eq!(report.hyphens_joined, 0);
        assert_eq!(pages[0].text, before);
    }

    #[test]
    fn fully_consumed_line_hands_its_spans_over() {
        let mut pages = vec![page_of(
            1,
            &[
                ("a comprehen-", 60.0, 600.0, 0),
                ("sive", 60.0, 588.0, 0),
                ("next line here", 60.0, 576.0, 0),
            ],
        )];
        clean_document(&mut pages);
        assert_eq!(pages[0].text, "a comprehensive\nnext line here");
        assert_eq!(texts(&pages[0]), ["a comprehensive", "next line here"]);
        assert_eq!(pages[0].lines[0].spans, [0, 1]);
        assert_eq!(pages[0].spans.len(), 3);
    }

    #[test]
    fn hyphen_joins_across_a_page_break_past_the_page_number() {
        let mut pages = vec![
            page_of(
                1,
                &[
                    ("Body text with a comprehen-", 60.0, 400.0, 0),
                    ("1", 300.0, 30.0, 1),
                ],
            ),
            page_of(
                2,
                &[
                    ("sive review follows.", 60.0, 700.0, 0),
                    ("2", 300.0, 30.0, 1),
                ],
            ),
        ];
        let report = clean_document(&mut pages);
        assert_eq!(report.page_numbers, 2);
        assert_eq!(report.hyphens_joined, 1);
        assert_eq!(pages[0].text, "Body text with a comprehensive");
        assert_eq!(pages[1].text, "review follows.");
    }

    /// Base line `the class G (A) of graphs` with a raised `hom` of
    /// `script_size` between `G` and `(A)`, emitted as its own line above.
    fn script_page(script_size: f32) -> PageText {
        let mut page = PageText::new(1, 612.0, 792.0, 0);
        page.spans = vec![
            span_at("the class G", 50.0, 398.0, 10.0, 0),
            span_at("(A) of graphs", 116.0, 398.0, 10.0, 1),
            span_at("hom", 105.0, 402.0, script_size, 2),
        ];
        let base_box = union(page.spans[0].bbox.unwrap(), page.spans[1].bbox.unwrap());
        page.lines = vec![
            Line {
                text: "hom".to_string(),
                bbox: page.spans[2].bbox,
                column: 0,
                spans: vec![2],
                role: ROLE_BODY.to_string(),
            },
            Line {
                text: "the class G (A) of graphs".to_string(),
                bbox: Some(base_box),
                column: 0,
                spans: vec![0, 1],
                role: ROLE_BODY.to_string(),
            },
        ];
        page.text = joined(&page.lines);
        page
    }

    #[test]
    fn superscript_line_merges_into_its_base_line() {
        let mut pages = vec![script_page(7.0)];
        let report = clean_document(&mut pages);
        assert_eq!(report.scripts_merged, 1);
        assert_eq!(pages[0].text, "the class G hom(A) of graphs");
        assert_eq!(pages[0].lines.len(), 1);
        assert_eq!(pages[0].lines[0].spans, [0, 2, 1]);
        assert_eq!(pages[0].spans.len(), 3);
    }

    #[test]
    fn near_body_size_fragment_is_not_a_script() {
        let mut pages = vec![script_page(9.5)];
        let before = pages[0].clone();
        let report = clean_document(&mut pages);
        assert_eq!(report.scripts_merged, 0);
        assert_eq!(pages[0], before);
    }

    #[test]
    fn arxiv_stamp_leaves_page_one_text() {
        let stamp = "arXiv:2507.14211v1  [cs.NI]  15 Jul 2025";
        let mut first = page_of(
            1,
            &[
                ("Abstract text here.", 60.0, 600.0, 0),
                (stamp, 60.0, 500.0, 0),
                ("More body text.", 60.0, 588.0, 0),
            ],
        );
        // Make the stamp span vertical, as the rotated margin stamp is.
        let vertical = Some(BBox {
            x0: 18.0,
            y0: 200.0,
            x1: 30.0,
            y1: 600.0,
        });
        first.spans[1].bbox = vertical;
        first.lines[1].bbox = vertical;
        let second = page_of(2, &[(stamp, 60.0, 500.0, 0)]);
        let mut pages = vec![first, second];
        let report = clean_document(&mut pages);
        assert_eq!(report.stamps, 1);
        assert_eq!(pages[0].text, "Abstract text here.\nMore body text.");
        assert_eq!(pages[0].lines[2].text, stamp);
        assert!(
            pages[0]
                .warnings
                .contains(&"furniture removed: 1".to_string())
        );
        assert_eq!(pages[1].text, stamp, "only page 1 carries the stamp rule");
    }

    #[test]
    fn vertical_text_line_is_a_stamp_and_short_lines_are_not() {
        let mut page = page_of(
            1,
            &[
                ("Preprint under review", 18.0, 200.0, 0),
                ("l", 60.0, 400.0, 0),
            ],
        );
        for span in &mut page.spans {
            let b = span.bbox.unwrap();
            span.bbox = Some(BBox {
                x0: b.x0,
                y0: b.y0,
                x1: b.x0 + 2.0,
                y1: b.y0 + 300.0,
            });
        }
        assert!(is_stamp(&page, &page.lines[0]));
        assert!(!is_stamp(&page, &page.lines[1]));
        let horizontal = page_of(1, &[("arXiv:2507.14211 is our identifier", 60.0, 400.0, 0)]);
        assert!(!is_stamp(&horizontal, &horizontal.lines[0]));
    }

    #[test]
    fn page_whose_text_does_not_match_its_lines_is_skipped() {
        let mut page = page_of(1, &[("1", 300.0, 30.0, 0)]);
        page.text = "something else".to_string();
        let mut pages = vec![page];
        let report = clean_document(&mut pages);
        assert_eq!(report.pages_skipped, 1);
        assert_eq!(report.page_numbers, 0);
        assert_eq!(pages[0].text, "something else");
    }

    fn roles(page: &PageText) -> Vec<&str> {
        page.lines.iter().map(|l| l.role.as_str()).collect()
    }

    /// Six pages whose running heads sit at 690 pt (inside the 15 % band,
    /// outside the 8 % one): recto pages carry the title with the page number
    /// in it, verso pages the authors; `extra` adds a line at the same height
    /// on pages 2 and 4 only.
    fn recto_verso_pages(extra: bool) -> Vec<PageText> {
        (1..=6)
            .map(|n| {
                let head = if n % 2 == 1 {
                    format!("Individual Rationality in Constrained Hedonic Games {n}")
                } else {
                    "Ann Author and Bob Writer".to_string()
                };
                let body = format!("Body text of page {n}.");
                let mut rows: Vec<(&str, f32, f32, u32)> = vec![
                    (head.as_str(), 60.0, 690.0, 0),
                    (body.as_str(), 60.0, 600.0, 1),
                ];
                if extra && (n == 2 || n == 4) {
                    rows.push(("A short repeated label", 300.0, 690.0, 2));
                }
                page_of(n, &rows)
            })
            .collect()
    }

    #[test]
    fn alternating_running_heads_in_the_wide_band_are_furniture() {
        let mut pages = recto_verso_pages(true);
        let report = clean_document(&mut pages);
        assert_eq!(report.running_lines_wide, 6);
        assert_eq!(report.running_lines, 0);
        for (i, page) in pages.iter().enumerate() {
            let n = i + 1;
            if n == 2 || n == 4 {
                assert_eq!(
                    page.text,
                    format!("Body text of page {n}.\nA short repeated label"),
                    "a wide-band line on two pages stays"
                );
            } else {
                assert_eq!(page.text, format!("Body text of page {n}."));
            }
            assert_eq!(
                page.lines.last().map(|l| l.role.as_str()),
                Some("furniture")
            );
        }
        let once = pages.clone();
        assert_eq!(clean_document(&mut pages), CleanupReport::default());
        assert_eq!(pages, once);
    }

    #[test]
    fn strong_repetition_rule() {
        fn pages(list: &[u32]) -> BTreeSet<u32> {
            list.iter().copied().collect()
        }
        assert!(strongly_repeated(&pages(&[1, 3, 5]), 20));
        assert!(strongly_repeated(&pages(&[2, 4, 6]), 20));
        assert!(!strongly_repeated(&pages(&[1, 2, 3]), 20));
        assert!(strongly_repeated(&pages(&[1, 2, 3]), 7));
        assert!(!strongly_repeated(&pages(&[1, 2]), 2));
    }

    #[test]
    fn dot_leader_lines_are_toc() {
        let mut pages = vec![
            page_of(1, &[("Body text on page one", 60.0, 600.0, 0)]),
            page_of(
                2,
                &[
                    ("Contents", 60.0, 600.0, 0),
                    ("1 Introduction . . . . . . . . . 3", 60.0, 588.0, 0),
                    (
                        "1.1 Research Backgrounds of Multi-Agent Decision-Making . . . . . . . 4",
                        60.0,
                        576.0,
                        0,
                    ),
                    ("2 Methods ........ 12", 60.0, 564.0, 0),
                    ("Values of x . y . z are 5", 60.0, 552.0, 0),
                ],
            ),
        ];
        let before = pages[1].text.clone();
        let report = clean_document(&mut pages);
        assert_eq!(report.role_toc, 3);
        assert_eq!(roles(&pages[1]), ["body", "toc", "toc", "toc", "body"]);
        assert_eq!(pages[1].text, before, "tags never remove text");
    }

    #[test]
    fn page_one_lines_before_the_abstract_are_front_matter() {
        let mut pages = vec![page_of(
            1,
            &[
                ("A Study of Things", 60.0, 700.0, 0),
                ("Ann Author, Bob Writer", 60.0, 680.0, 0),
                ("University of Somewhere", 60.0, 668.0, 0),
                ("Abstract", 60.0, 640.0, 0),
                ("We study things.", 60.0, 628.0, 0),
                ("1 Introduction", 60.0, 600.0, 0),
                ("Things matter.", 60.0, 588.0, 0),
            ],
        )];
        let before = pages[0].text.clone();
        let report = clean_document(&mut pages);
        assert_eq!(report.role_front, 3);
        assert_eq!(report.role_heading, 1);
        assert_eq!(
            roles(&pages[0]),
            ["front", "front", "front", "heading", "body", "body", "body"]
        );
        assert_eq!(pages[0].text, before);

        let mut run_in = vec![page_of(
            1,
            &[
                ("Abstractive Summaries Revisited", 60.0, 700.0, 0),
                ("Ann Author", 60.0, 680.0, 0),
                ("Abstract\u{2014}We revisit summaries.", 60.0, 640.0, 0),
            ],
        )];
        clean_document(&mut run_in);
        assert_eq!(roles(&run_in[0]), ["front", "front", "body"]);
    }

    #[test]
    fn without_an_abstract_front_matter_ends_at_the_introduction() {
        let mut pages = vec![
            page_of(
                1,
                &[
                    ("A Study of Things", 60.0, 700.0, 0),
                    ("Ann Author", 60.0, 680.0, 0),
                    ("I. INTRODUCTION", 60.0, 640.0, 0),
                    ("Things matter.", 60.0, 628.0, 0),
                ],
            ),
            page_of(2, &[("Ann Author", 60.0, 600.0, 0)]),
        ];
        let before = pages[0].text.clone();
        let report = clean_document(&mut pages);
        assert_eq!(report.role_front, 2);
        assert_eq!(report.role_heading, 0);
        assert_eq!(roles(&pages[0]), ["front", "front", "body", "body"]);
        assert_eq!(roles(&pages[1]), ["body"], "only page 1 has front matter");
        assert_eq!(pages[0].text, before);
    }

    #[test]
    fn caption_labels_are_tagged() {
        let mut pages = vec![
            page_of(1, &[("Body text on page one", 60.0, 600.0, 0)]),
            page_of(
                2,
                &[
                    ("Figure 1: Overview of the system.", 60.0, 600.0, 0),
                    ("Fig. 2. Results on the test set.", 60.0, 588.0, 0),
                    ("Table S1 | Data sources.", 60.0, 576.0, 0),
                    ("Algorithm 3: Greedy search", 60.0, 564.0, 0),
                    ("TABLE IV. Error rates", 60.0, 552.0, 0),
                    ("Figure 3 shows the results.", 60.0, 540.0, 0),
                    ("Tables are listed below.", 60.0, 528.0, 0),
                ],
            ),
        ];
        let before = pages[1].text.clone();
        let report = clean_document(&mut pages);
        assert_eq!(report.role_caption, 5);
        assert_eq!(
            roles(&pages[1]),
            [
                "caption", "caption", "caption", "caption", "caption", "body", "body"
            ]
        );
        assert_eq!(pages[1].text, before);
    }

    #[test]
    fn existing_tags_survive_and_furniture_wins() {
        let mut line = Line {
            role: "figure".to_string(),
            ..Line::default()
        };
        assert!(!tag(&mut line, ROLE_CAPTION));
        assert_eq!(line.role, "figure");
        assert!(tag(&mut line, ROLE_FURNITURE));
        assert_eq!(line.role, "furniture");
    }
}
