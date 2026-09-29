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
//!   that line; a detached superscript citation number (`5`, `5–7`,
//!   `10,11`) or a subscript digit is written with Unicode super- or
//!   subscript characters (`literature.⁵`, `Initiative⁵⁻⁷`, `NH₃`);
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
//! (or `Fig. 3 Overview`-style ones, a number and a capitalised word, when
//! they do not continue the paragraph above) `caption`, and on page 1 the
//! lines before the abstract `front` (the standalone `Abstract` line itself
//! `heading`), stopping at the first run of prose lines and skipping long
//! lines (or sentence lines of 12 words with a verb-like word) that are
//! neither affiliations nor lists of names. Only lines still tagged
//! `body` are retagged, except that `furniture` wins over any tag.

use std::collections::{BTreeMap, BTreeSet, HashSet};
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
/// Fewest words in each line of the prose run that ends page-1 front
/// matter even without an `Abstract` line.
const FRONT_RUN_WORDS: usize = 10;
/// Consecutive prose lines of at least `FRONT_RUN_WORDS` words that end the
/// front matter.
const FRONT_RUN_LINES: usize = 2;
/// A page-1 line with at least this many words is never front matter unless
/// it carries an affiliation signal or reads like a list of names.
const FRONT_LONG_WORDS: usize = 14;
/// A page-1 line with at least this many words and a verb-like word (see
/// [`has_verb_ending`]) is never front matter unless it carries an
/// affiliation signal or reads like a list of names.
const FRONT_SENTENCE_WORDS: usize = 12;
/// Fewest words in each line of the shorter prose run that also ends the
/// front matter (an abstract set in a narrow column).
const FRONT_SHORT_RUN_WORDS: usize = 6;
/// Consecutive prose lines of at least `FRONT_SHORT_RUN_WORDS` words that
/// end the front matter.
const FRONT_SHORT_RUN_LINES: usize = 3;
/// Substrings that mark an affiliation or contact line in the front matter.
const AFFILIATION_SIGNALS: [&str; 6] = [
    "@",
    "University",
    "Institute",
    "Department",
    "Laboratory",
    "Corresponding",
];
/// Fewest words in a line directly above a bare caption start (`Figure 3
/// The ...`) for that line to read as a paragraph the start continues.
const CAPTION_PARAGRAPH_WORDS: usize = 6;
/// Largest gap, in heights of the lower line, between a line and the bare
/// caption start below it for the two to belong to one paragraph.
const CAPTION_PARAGRAPH_GAP: f32 = 0.6;
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
/// Longest superscript or subscript fragment (`10, 11, 12`), in characters.
const SUPERSCRIPT_MAX_CHARS: usize = 12;
/// Rounding slack on `SCRIPT_RATIO` for the superscript rule: sizes taken
/// from scaled text matrices come out as 5.98 on 7.97 for a 6 on 8 pt pair.
const RATIO_SLACK: f32 = 0.005;
/// Distance from a span box's bottom up to its baseline, as a share of its
/// font size (the backends' descent estimate).
const DESCENT_SHARE: f32 = 0.2;
/// Window for a superscript's box bottom above the base line's baseline, in
/// base font sizes. A superscript sits so far above the base line's
/// x-height that its box barely overlaps the base line's box, and reading
/// order gives it a line of its own.
const RAISED_LOW: f32 = -0.2;
const RAISED_HIGH: f32 = 0.9;
/// Lowest box bottom of a subscript, in base font sizes from the baseline
/// (a subscript's bottom lies below `RAISED_LOW`).
const LOWERED_LOW: f32 = -0.7;
/// Typical box-bottom offsets of a superscript and a subscript, used to
/// pick between two candidate base lines (a subscript of one line also lies
/// in the superscript window of the line below).
const RAISED_IDEAL: f32 = 0.3;
const LOWERED_IDEAL: f32 = -0.4;
/// How far outside the base line's box a superscript may sit, in base font
/// sizes.
const SUPERSCRIPT_REACH: f32 = 0.5;
/// Unicode superscript and subscript digits, indexed by value.
const SUPERSCRIPT_DIGITS: [char; 10] = [
    '\u{2070}', '\u{00B9}', '\u{00B2}', '\u{00B3}', '\u{2074}', '\u{2075}', '\u{2076}', '\u{2077}',
    '\u{2078}', '\u{2079}',
];
const SUBSCRIPT_DIGITS: [char; 10] = [
    '\u{2080}', '\u{2081}', '\u{2082}', '\u{2083}', '\u{2084}', '\u{2085}', '\u{2086}', '\u{2087}',
    '\u{2088}', '\u{2089}',
];
/// Superscript minus, written for `-`, `–` and `−` in a raised range.
const SUPERSCRIPT_MINUS: char = '\u{207B}';
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
/// keep a line-end hyphen between two words (`cost-` + `effective`,
/// `web-` + `based`); shorter halves (`in-` + `formation`) never keep it by
/// this rule.
const MIN_ATTESTED_HALF: usize = 3;
/// Short halves that commonly form one word with the other half: word
/// endings (`with-` + `out`, `learn-` + `ing`) and word beginnings (`con-` +
/// `tent`, `out-` + `put`). On either side of the hyphen they never count
/// as attested for the keep rule.
const AMBIGUOUS_HALVES: &[&str] = &[
    "out", "ing", "ers", "est", "ion", "ity", "ful", "ess", "ant", "ent", "ure", "age", "ive",
    "ous", "ise", "ize", "ism", "ist", "ate", "ify", "ary", "ory", "ial", "ual", "pre", "pro",
    "con", "com", "dis", "mis", "sub", "non", "per", "for", "ver", "sur", "int",
];
/// Left halves that form real compounds (`self-supervised`,
/// `cross-domain`): a line-end hyphen after one is kept unless the joined
/// word is attested.
const COMPOUND_PREFIXES: &[&str] = &["self", "cross", "well", "high", "low", "long", "short"];
/// Bound prefixes normally written solid (`preserving`, `nonlinear`,
/// `multimodal`): a line-end hyphen after one is dropped when the right
/// half is lowercase and an attested word or at least
/// [`PREFIX_JOIN_MIN_RIGHT`] letters long.
const JOIN_PREFIXES: &[&str] = &[
    "pre", "re", "non", "un", "sub", "multi", "semi", "inter", "intra", "over", "under", "micro",
    "nano", "co", "de", "dis", "mis", "anti", "auto", "bio", "counter", "hyper", "meta", "post",
    "pseudo", "super", "trans", "ultra", "extra", "infra", "pro",
];
/// Shortest unattested right half that still joins after a
/// [`JOIN_PREFIXES`] entry (`pre-` + `serving`).
const PREFIX_JOIN_MIN_RIGHT: usize = 5;
/// Longest right half that is never a typeset word break (`TeX` leaves at
/// least three letters after a break), so `most-` + `dl` stays a compound.
const MAX_UNBREAKABLE_RIGHT: usize = 2;

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
    /// Sub/superscript lines merged into their base line as they read
    /// (the general rule; not counting `superscripts_merged`).
    pub scripts_merged: usize,
    /// Detached superscript numbers (`5`, `5–7`, `10,11`), single-letter or
    /// asterisk marks and subscript digits merged into their base line in
    /// Unicode super- or subscript form.
    pub superscripts_merged: usize,
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
    words: HashSet<String>,
    compounds: HashSet<String>,
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

/// A caption start without punctuation after the number, followed by a
/// capitalised word: `Fig. 3 Overview of`, `Table 2 Results on`.
fn caption_bare_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^(?:Figure|FIGURE|Fig\.|FIG\.|Table|TABLE)\s*(?:[A-Z]?\d+(?:\.\d+)*|[IVXL]+)\s+\p{Lu}\p{Ll}",
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
fn script_target(page: &PageText, w: &PageWork, geom: &PageGeom, index: usize) -> Option<usize> {
    let line = page.lines.get(index)?;
    let text = line.text.trim();
    if text.is_empty()
        || text.chars().count() > SCRIPT_MAX_CHARS
        || text.split_whitespace().count() > SCRIPT_MAX_WORDS
    {
        return None;
    }
    let bbox = norm(line.bbox?);
    let size = geom.lines.get(index)?.size?;
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
        let other_size = geom.lines.get(cand).and_then(|g| g.size);
        let (Some(ob), Some(other_size)) = (other.bbox.map(norm), other_size) else {
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
        let Some((start, len)) = locate(&line.text, cursor, piece) else {
            continue;
        };
        if span.bbox.is_some_and(|b| norm(b).x0 >= x0) {
            return (start, slot);
        }
        cursor = start + len;
    }
    (line.text.len(), line.spans.len())
}

/// Byte offset at or after `cursor` where `piece` shows in `text`, and the
/// byte length it shows with: as itself or, for a merged fragment, in its
/// super- or subscript form, whichever comes first.
fn locate(text: &str, cursor: usize, piece: &str) -> Option<(usize, usize)> {
    let rest = text.get(cursor..)?;
    let mut best: Option<(usize, usize)> = rest.find(piece).map(|at| (at, piece.len()));
    for raised in [true, false] {
        let Some(form) = script_form(piece, raised) else {
            continue;
        };
        let found = rest
            .find(form.as_str())
            .filter(|at| best.is_none_or(|(b, _)| *at < b));
        if let Some(at) = found {
            best = Some((at, form.len()));
        }
    }
    best.map(|(at, len)| (cursor + at, len))
}

/// `text` written as a superscript (`raised`) or subscript fragment, when
/// it is one: at most `SUPERSCRIPT_MAX_CHARS` characters of digits with
/// optional `,`, `-`, `–`, `−` and spaces (raised only; a subscript is
/// digits alone), or, raised, a single letter or asterisk. Digits become
/// Unicode super- or subscript digits, dashes the superscript minus, spaces
/// are dropped; a lowercase letter becomes its Unicode superscript letter (none for `q`), an asterisk stays as it is. `None` for anything
/// else, words of two or more letters included.
/// The Unicode superscript form of a lowercase Latin letter, when one
/// exists (there is no superscript `q`).
fn superscript_letter(letter: char) -> Option<char> {
    let mapped = match letter {
        'a' => '\u{1D43}',
        'b' => '\u{1D47}',
        'c' => '\u{1D9C}',
        'd' => '\u{1D48}',
        'e' => '\u{1D49}',
        'f' => '\u{1DA0}',
        'g' => '\u{1D4D}',
        'h' => '\u{02B0}',
        'i' => '\u{2071}',
        'j' => '\u{02B2}',
        'k' => '\u{1D4F}',
        'l' => '\u{02E1}',
        'm' => '\u{1D50}',
        'n' => '\u{207F}',
        'o' => '\u{1D52}',
        'p' => '\u{1D56}',
        'r' => '\u{02B3}',
        's' => '\u{02E2}',
        't' => '\u{1D57}',
        'u' => '\u{1D58}',
        'v' => '\u{1D5B}',
        'w' => '\u{02B7}',
        'x' => '\u{02E3}',
        'y' => '\u{02B8}',
        'z' => '\u{1DBB}',
        _ => return None,
    };
    Some(mapped)
}

fn script_form(text: &str, raised: bool) -> Option<String> {
    let text = text.trim();
    if text.is_empty() || text.chars().count() > SUPERSCRIPT_MAX_CHARS {
        return None;
    }
    let mut chars = text.chars();
    if let (Some(only), None) = (chars.next(), chars.next())
        && raised
    {
        if matches!(only, '*' | '\u{2217}') {
            return Some(only.to_string());
        }
        if only.is_alphabetic() {
            // Only letters with a real superscript form are merged; a raised
            // `q` or capital is left to the general script rule so `xⁿ`
            // never collapses into `xn`.
            return superscript_letter(only).map(|c| c.to_string());
        }
    }
    let mut out = String::with_capacity(3 * text.len());
    let mut digits: usize = 0;
    for ch in text.chars() {
        if ch.is_whitespace() && raised {
            continue;
        }
        let mapped = match ch {
            '0'..='9' => {
                digits += 1;
                let index = usize::try_from(ch.to_digit(10)?).ok()?;
                let table = if raised {
                    &SUPERSCRIPT_DIGITS
                } else {
                    &SUBSCRIPT_DIGITS
                };
                table.get(index).copied()?
            }
            '-' | '\u{2013}' | '\u{2212}' if raised => SUPERSCRIPT_MINUS,
            ',' if raised => ',',
            _ => return None,
        };
        out.push(mapped);
    }
    (digits > 0).then_some(out)
}

/// True for a character `script_form` writes for a digit or a dash.
fn is_script_char(ch: char) -> bool {
    ch == SUPERSCRIPT_MINUS || SUPERSCRIPT_DIGITS.contains(&ch) || SUBSCRIPT_DIGITS.contains(&ch)
}

/// Baseline of the line and its dominant font size, from the first
/// non-blank span of that size (the line box grows with merged scripts).
fn baseline_of(page: &PageText, line: &Line) -> Option<(f32, f32)> {
    let dominant = line_size(page, line)?;
    line.spans.iter().find_map(|idx| {
        let span = page.spans.get(*idx as usize)?;
        if span.text.trim().is_empty() {
            return None;
        }
        let size = span
            .size
            .filter(|s| s.is_finite() && *s >= 0.9 * dominant)?;
        let bbox = norm(span.bbox?);
        Some((bbox.y0 + DESCENT_SHARE * size, dominant))
    })
}

/// Geometry of one line for rules 3 and 3a: its normalised box, its
/// dominant size ([`line_size`]) and its baseline with that size
/// ([`baseline_of`]).
#[derive(Clone, Copy)]
struct LineGeom {
    bbox: Option<BBox>,
    size: Option<f32>,
    base: Option<(f32, f32)>,
}

impl LineGeom {
    fn of(page: &PageText, line: &Line) -> Self {
        Self {
            bbox: line.bbox.map(norm),
            size: line_size(page, line),
            base: baseline_of(page, line),
        }
    }

    /// Baseline and size bits of a line that can be a base line (it has a
    /// box and a finite baseline), for telling whether the index changes.
    fn index_key(&self) -> Option<(u32, u32)> {
        self.bbox?;
        let (baseline, size) = self.base?;
        baseline
            .is_finite()
            .then_some((baseline.to_bits(), size.to_bits()))
    }
}

/// Per-page cache for rules 3 and 3a, computed once per page and refreshed
/// for the target line of each merge: the geometry of every line, the
/// lines that can be a base line as `(baseline, index)` sorted by
/// baseline, and the largest dominant size among them.
struct PageGeom {
    lines: Vec<LineGeom>,
    by_baseline: Vec<(f32, usize)>,
    max_size: f32,
}

impl PageGeom {
    fn new(page: &PageText) -> Self {
        let lines: Vec<LineGeom> = page
            .lines
            .iter()
            .map(|line| LineGeom::of(page, line))
            .collect();
        let mut geom = Self {
            lines,
            by_baseline: Vec::new(),
            max_size: 0.0,
        };
        geom.index();
        geom
    }

    fn index(&mut self) {
        self.by_baseline.clear();
        self.max_size = 0.0;
        for (j, g) in self.lines.iter().enumerate() {
            if g.index_key().is_some()
                && let Some((baseline, size)) = g.base
            {
                self.by_baseline.push((baseline, j));
                self.max_size = self.max_size.max(size);
            }
        }
        self.by_baseline.sort_by(|a, b| a.0.total_cmp(&b.0));
    }

    /// Recompute line `index` after a merge changed it; the baseline index
    /// is rebuilt only when its baseline or size changed.
    fn refresh(&mut self, page: &PageText, index: usize) {
        let (Some(slot), Some(line)) = (self.lines.get_mut(index), page.lines.get(index)) else {
            return;
        };
        let before = slot.index_key();
        *slot = LineGeom::of(page, line);
        if slot.index_key() != before {
            self.index();
        }
    }

    /// Indices of the base-line candidates with a baseline in
    /// `low..=high`, ascending, into `out`.
    fn window(&self, low: f32, high: f32, out: &mut Vec<usize>) {
        out.clear();
        let start = self.by_baseline.partition_point(|(b, _)| *b < low);
        for &(baseline, j) in &self.by_baseline[start..] {
            if baseline > high {
                break;
            }
            out.push(j);
        }
        out.sort_unstable();
    }
}

/// Rule 3a: the body line a detached superscript or subscript fragment at
/// `index` belongs to, with the fragment's rendered form. Every body line of
/// the page is a candidate, not only the neighbours in reading order:
/// fragments printed on one row follow each other (`8`, `9`, `10,11`), and
/// when two columns interleave the base line can be further away. The base
/// line has a font size of at least `1 / SCRIPT_RATIO` times the
/// fragment's, reaches the fragment horizontally within
/// `SUPERSCRIPT_REACH`, and has its baseline at a box-bottom offset in the
/// raised window (superscript) or, for digits only, just below it
/// (subscript). The candidate closest to the typical offset wins (the
/// lowest line index on a tie). Only lines whose baseline lies within the
/// offset range at the page's largest size are looked at (`geom`, with
/// `window` as scratch space); every other line fails the offset test.
fn superscript_target(
    page: &PageText,
    w: &PageWork,
    geom: &PageGeom,
    index: usize,
    window: &mut Vec<usize>,
) -> Option<(usize, String)> {
    let line = page.lines.get(index)?;
    let raised_form = script_form(&line.text, true);
    let lowered_form = script_form(&line.text, false);
    if raised_form.is_none() && lowered_form.is_none() {
        return None;
    }
    let own = geom.lines.get(index)?;
    let bbox = own.bbox?;
    let size = own.size?;
    if !bbox.y0.is_finite() {
        return None;
    }
    // A candidate passes only with `LOWERED_LOW <= (y0 - baseline) / size
    // <= RAISED_HIGH` and `size <= max_size`; the slack covers rounding.
    let reach = RAISED_HIGH.abs().max(LOWERED_LOW.abs()) * geom.max_size;
    let slack = 1e-3 * (bbox.y0.abs() + reach) + 1e-3;
    geom.window(bbox.y0 - reach - slack, bbox.y0 + reach + slack, window);
    let mut best: Option<(usize, f32, bool)> = None;
    for &j in &*window {
        if j == index || !w.is_body(j) {
            continue;
        }
        let Some(other) = geom.lines.get(j) else {
            continue;
        };
        let (Some(ob), Some((baseline, other_size))) = (other.bbox, other.base) else {
            continue;
        };
        if size > (SCRIPT_RATIO + RATIO_SLACK) * other_size {
            continue;
        }
        let gap = (ob.x0 - bbox.x1).max(bbox.x0 - ob.x1).max(0.0);
        if gap > SUPERSCRIPT_REACH * other_size {
            continue;
        }
        let offset = (bbox.y0 - baseline) / other_size;
        let (raised, miss) =
            if raised_form.is_some() && (RAISED_LOW..=RAISED_HIGH).contains(&offset) {
                (true, (offset - RAISED_IDEAL).abs())
            } else if lowered_form.is_some() && (LOWERED_LOW..RAISED_LOW).contains(&offset) {
                (false, (offset - LOWERED_IDEAL).abs())
            } else {
                continue;
            };
        let score = miss + gap / other_size;
        if best.is_none_or(|(_, s, _)| score < s) {
            best = Some((j, score, raised));
        }
    }
    let (target, _, raised) = best?;
    let form = if raised { raised_form } else { lowered_form };
    form.map(|f| (target, f))
}

/// Insert the rendered fragment `form` of line `script` into line `target`
/// at its horizontal position, attached to the word before it with no
/// space (`literature.⁵`, `Initiative⁶,`), or to the word after it when it
/// opens the line (`⁵Prein`). The fragment's span indices go into the
/// target line's spans at the same place.
fn merge_superscript(page: &mut PageText, script: usize, target: usize, form: &str) {
    let Some(line) = page.lines.get(script) else {
        return;
    };
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
    let Some(head) = base.text.get(..byte) else {
        return;
    };
    let tail = base.text.get(byte..).unwrap_or("");
    let head = head.trim_end();
    let mut text = String::with_capacity(base.text.len() + form.len() + 1);
    text.push_str(head);
    text.push_str(form);
    if head.is_empty() {
        text.push_str(tail.trim_start());
    } else {
        let word_follows = tail
            .chars()
            .next()
            .is_some_and(|c| c.is_alphanumeric() && !is_script_char(c));
        if word_follows {
            text.push(' ');
        }
        text.push_str(tail);
    }
    base.text = text;
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

/// Rules 3a and 3 over one page; returns the number of lines merged by
/// the general script rule and by the superscript rule.
fn merge_scripts(page: &mut PageText, w: &mut PageWork) -> (usize, usize) {
    let mut merged: usize = 0;
    let mut superscripts: usize = 0;
    let mut geom = PageGeom::new(page);
    let mut window: Vec<usize> = Vec::new();
    for k in 0..page.lines.len() {
        if !w.is_body(k) {
            continue;
        }
        if let Some((target, form)) = superscript_target(page, w, &geom, k, &mut window) {
            merge_superscript(page, k, target, &form);
            geom.refresh(page, target);
            w.mark(k, State::Merged);
            superscripts += 1;
        } else if let Some(target) = script_target(page, w, &geom, k) {
            merge_script(page, k, target);
            geom.refresh(page, target);
            w.mark(k, State::Merged);
            merged += 1;
        }
    }
    (merged, superscripts)
}

/// Append `word` lower-cased to `buf`, as `str::to_lowercase` does (ASCII
/// in place, anything else through `to_lowercase`).
fn push_lowercase(buf: &mut String, word: &str) {
    if word.is_ascii() {
        let start = buf.len();
        buf.push_str(word);
        if let Some(tail) = buf.get_mut(start..) {
            tail.make_ascii_lowercase();
        }
    } else {
        buf.push_str(&word.to_lowercase());
    }
}

/// Insert `key` into `set` unless it is there already (one allocation per
/// new entry only).
fn insert_new(set: &mut HashSet<String>, key: &str) {
    if !set.contains(key) {
        set.insert(key.to_owned());
    }
}

/// Words and hyphenated word pairs of every body line, lower-cased, in
/// reading order (see [`Vocabulary`] for the halves left out).
fn vocabulary(pages: &[PageText], work: &[PageWork]) -> Vocabulary {
    let mut vocab = Vocabulary {
        words: HashSet::new(),
        compounds: HashSet::new(),
    };
    let alphabetic = |piece: &str| !piece.is_empty() && piece.chars().all(char::is_alphabetic);
    let mut buf = String::new();
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
            let mut tokens = line.text.split_whitespace().peekable();
            let mut first_token = true;
            while let Some(token) = tokens.next() {
                let last_token = tokens.peek().is_none();
                let core = token.trim_matches(|c: char| !c.is_alphanumeric());
                let mut parts = core.split(HYPHENS).peekable();
                let mut first_part = true;
                let mut prev_part: Option<&str> = None;
                while let Some(part) = parts.next() {
                    let last_part = parts.peek().is_none();
                    let mut words = part
                        .split(|c: char| !c.is_alphabetic())
                        .filter(|word| !word.is_empty())
                        .peekable();
                    let mut first_word = true;
                    while let Some(word) = words.next() {
                        let last_word = words.peek().is_none();
                        let split_head = after_hyphen && first_token && first_part && first_word;
                        let split_tail = ends_hyphen && last_token && last_part && last_word;
                        if !split_head && !split_tail {
                            buf.clear();
                            push_lowercase(&mut buf, word);
                            insert_new(&mut vocab.words, &buf);
                        }
                        first_word = false;
                    }
                    if let Some(prev) = prev_part
                        && alphabetic(prev)
                        && alphabetic(part)
                    {
                        buf.clear();
                        push_lowercase(&mut buf, prev);
                        buf.push('-');
                        push_lowercase(&mut buf, part);
                        insert_new(&mut vocab.compounds, &buf);
                    }
                    prev_part = Some(part);
                    first_part = false;
                }
                first_token = false;
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

/// Outcome of [`hyphen_policy`] for one line-end hyphen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HyphenPolicy {
    /// A word broken for justification: `opti-` + `mization` → `optimization`.
    Join,
    /// A real compound: `cost-` + `effective` → `cost-effective`.
    Keep,
}

/// Whether a right half of three letters has no vowel (`cnn`), so it reads
/// as an acronym rather than a word ending (`ing`, `ves`).
fn vowelless(right: &str) -> bool {
    !right
        .chars()
        .any(|c| matches!(c.to_ascii_lowercase(), 'a' | 'e' | 'i' | 'o' | 'u' | 'y'))
}

/// Whether the printed halves themselves mark a real compound: the right
/// half starts with a capital or has a digit, the left half is one letter
/// (`k-space`, `x-ray`) or an all-capital acronym (`MRI-guided`), or the
/// right half is too short to be a typeset break (`most-dl`, `state-of`) or
/// is a three-letter acronym after a short left half (`deep-cnn`).
fn printed_compound(left: &str, right: &str) -> bool {
    let left_len = left.chars().count();
    let right_len = right.chars().count();
    right.chars().next().is_some_and(char::is_uppercase)
        || right.chars().any(char::is_numeric)
        || left.chars().any(char::is_numeric)
        || left_len == 1
        || (left_len >= 2 && left.chars().all(char::is_uppercase))
        || right_len <= MAX_UNBREAKABLE_RIGHT
        || (left_len <= 5 && right_len == 3 && vowelless(right))
}

/// Decide a line-end hyphen between `left`, the word before the hyphen, and
/// `right`, the word that starts the next line, both as printed.
/// `attested(piece)` tells whether the lower-cased word (`optimization`) or
/// hyphenated pair (`noise-regularized`) occurs elsewhere in the document
/// as a whole word. The first matching rule wins:
/// 1. the joined word is attested → join (`with-` + `out`, `without` seen);
/// 2. the hyphenated pair is attested → keep (`noise-regularized` seen);
/// 3. the left half is a `COMPOUND_PREFIXES` entry (`self-`) → keep;
/// 4. the left half is a `JOIN_PREFIXES` entry and the right half is all
///    lowercase and an attested word or at least `PREFIX_JOIN_MIN_RIGHT`
///    letters → join (`pre-` + `serving`);
/// 5. the printed halves mark a compound (see `printed_compound`) → keep;
/// 6. both halves are attested words of at least `MIN_ATTESTED_HALF`
///    letters, neither an `AMBIGUOUS_HALVES` entry → keep (`cost-` +
///    `effective`, `web-` + `based`, `Dual-` + `domain`);
/// 7. otherwise join (`algo-` + `rithm`).
pub fn hyphen_policy(left: &str, right: &str, attested: &dyn Fn(&str) -> bool) -> HyphenPolicy {
    let lower_left = left.to_lowercase();
    let lower_right = right.to_lowercase();
    if attested(&format!("{lower_left}{lower_right}")) {
        return HyphenPolicy::Join;
    }
    if attested(&format!("{lower_left}-{lower_right}")) {
        return HyphenPolicy::Keep;
    }
    if COMPOUND_PREFIXES.contains(&lower_left.as_str()) {
        return HyphenPolicy::Keep;
    }
    let lowercase_word = !right.is_empty() && right.chars().all(char::is_lowercase);
    if JOIN_PREFIXES.contains(&lower_left.as_str())
        && lowercase_word
        && (attested(&lower_right) || right.chars().count() >= PREFIX_JOIN_MIN_RIGHT)
    {
        return HyphenPolicy::Join;
    }
    if printed_compound(left, right) {
        return HyphenPolicy::Keep;
    }
    let word = |half: &str| half.chars().count() >= MIN_ATTESTED_HALF && attested(half);
    if word(&lower_left)
        && word(&lower_right)
        && !AMBIGUOUS_HALVES.contains(&lower_left.as_str())
        && !AMBIGUOUS_HALVES.contains(&lower_right.as_str())
    {
        HyphenPolicy::Keep
    } else {
        HyphenPolicy::Join
    }
}

/// Rule 2 for one pair of consecutive lines: a lowercase continuation after
/// an alphabetic word and a line-end hyphen is decided by [`hyphen_policy`]
/// against the document vocabulary.
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
    let right: String = head.chars().take_while(|c| c.is_alphanumeric()).collect();
    if right.is_empty() {
        return Decision::NotApplicable;
    }
    let attested = |piece: &str| vocab.words.contains(piece) || vocab.compounds.contains(piece);
    match hyphen_policy(word, &right, &attested) {
        HyphenPolicy::Join => {
            let tail = rest
                .get(head.len()..)
                .unwrap_or("")
                .trim_start()
                .to_string();
            Decision::Join(format!("{stem}{head}"), tail)
        }
        HyphenPolicy::Keep => Decision::Keep,
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

/// A bare caption start: `Figure`, `Fig.` or `Table`, a number and then a
/// capitalised word with no punctuation between (`Fig. 3 Overview of the`,
/// `Table 2 Results`). `Figure 3 shows` stays body.
fn is_bare_caption(text: &str) -> bool {
    caption_bare_re().is_match(text.trim())
}

/// Words in `text` and how many of them start with a lowercase letter.
fn lowercase_words(text: &str) -> (usize, usize) {
    let mut total: usize = 0;
    let mut lower: usize = 0;
    for token in text.split_whitespace() {
        total += 1;
        let starts_lower = token
            .chars()
            .find(|c| c.is_alphanumeric())
            .is_some_and(char::is_lowercase);
        if starts_lower {
            lower += 1;
        }
    }
    (total, lower)
}

/// The line at `index` directly continues the paragraph of the nearest
/// earlier non-furniture line: that line has at least
/// `CAPTION_PARAGRAPH_WORDS` words, does not end a sentence or a label, and
/// sits just above it with overlapping x ranges.
fn continues_paragraph(page: &PageText, index: usize) -> bool {
    let Some(line) = page.lines.get(index) else {
        return false;
    };
    let Some(prev) = page.lines[..index]
        .iter()
        .rev()
        .find(|l| l.role != ROLE_FURNITURE)
    else {
        return false;
    };
    let (words, _) = lowercase_words(&prev.text);
    if words < CAPTION_PARAGRAPH_WORDS || prev.text.trim_end().ends_with(['.', ':', '!', '?']) {
        return false;
    }
    let (Some(upper), Some(lower)) = (prev.bbox.map(norm), line.bbox.map(norm)) else {
        return false;
    };
    let height = lower.y1 - lower.y0;
    let gap = upper.y0 - lower.y1;
    let overlap = upper.x0 < lower.x1 && lower.x0 < upper.x1;
    overlap && height > 0.0 && gap >= -0.5 * height && gap <= CAPTION_PARAGRAPH_GAP * height
}

/// The line carries an affiliation or contact signal (see
/// `AFFILIATION_SIGNALS`).
fn has_affiliation_signal(text: &str) -> bool {
    AFFILIATION_SIGNALS.iter().any(|s| text.contains(s))
}

/// A line of the prose run that ends the front matter: at least
/// `FRONT_RUN_WORDS` words, at least half of them starting lowercase, and
/// no affiliation signal.
fn is_front_prose(text: &str) -> bool {
    let (total, lower) = lowercase_words(text);
    total >= FRONT_RUN_WORDS && lower * 2 >= total && !has_affiliation_signal(text)
}

/// A line of the shorter prose run that ends the front matter: at least
/// `FRONT_SHORT_RUN_WORDS` words, at least half of them starting lowercase,
/// and no affiliation signal.
fn is_short_front_prose(text: &str) -> bool {
    let (total, lower) = lowercase_words(text);
    total >= FRONT_SHORT_RUN_WORDS && lower * 2 >= total && !has_affiliation_signal(text)
}

/// The line holds a verb-like word: all letters, at least 4 of them,
/// starting lowercase and ending in `ed`, `ing` or `s`.
fn has_verb_ending(text: &str) -> bool {
    text.split_whitespace().any(|token| {
        let word = token.trim_matches(|c: char| !c.is_alphabetic());
        word.chars().count() >= 4
            && word.chars().all(char::is_alphabetic)
            && word.chars().next().is_some_and(char::is_lowercase)
            && (word.ends_with("ed") || word.ends_with("ing") || word.ends_with('s'))
    })
}

/// A long page-1 line that is not front matter: no affiliation signal, not
/// a list of names (at least 30 % of its words start lowercase), and at
/// least `FRONT_LONG_WORDS` words, or at least `FRONT_SENTENCE_WORDS` with
/// a verb-like word (see [`has_verb_ending`]).
fn is_long_body_line(text: &str) -> bool {
    let (total, lower) = lowercase_words(text);
    if has_affiliation_signal(text) || lower * 10 < total * 3 {
        return false;
    }
    total >= FRONT_LONG_WORDS || (total >= FRONT_SENTENCE_WORDS && has_verb_ending(text))
}

/// Page-1 front matter: the non-furniture lines before the abstract when it
/// starts within `FRONT_MAX_LINES` lines (a standalone `Abstract` line is
/// tagged `heading`), else those before an `Introduction` heading. Either
/// way the front matter also stops at the first run of `FRONT_RUN_LINES`
/// consecutive prose lines (see [`is_front_prose`]) or of
/// `FRONT_SHORT_RUN_LINES` shorter ones (see [`is_short_front_prose`]), an
/// unlabelled abstract or first paragraph, and a long line that is neither
/// an affiliation nor a list of names (see [`is_long_body_line`]) is never
/// tagged.
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
    let run_at = order
        .windows(FRONT_RUN_LINES)
        .position(|w| w.iter().all(|k| is_front_prose(text_of(*k))));
    let short_run_at = order
        .windows(FRONT_SHORT_RUN_LINES)
        .position(|w| w.iter().all(|k| is_short_front_prose(text_of(*k))));
    let end = run_at.map_or(end, |run| end.min(run));
    let end = short_run_at.map_or(end, |run| end.min(run));
    let long: Vec<bool> = order
        .iter()
        .map(|k| is_long_body_line(text_of(*k)))
        .collect();
    for (pos, k) in order.iter().enumerate().take(end) {
        if long[pos] {
            continue;
        }
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
/// `front`/`heading`. A bare caption start (see [`is_bare_caption`]) is a
/// caption only when it does not continue the paragraph above it (see
/// [`continues_paragraph`]). Never changes `text`.
fn tag_roles(page: &mut PageText, report: &mut CleanupReport) {
    let kinds: Vec<(bool, bool)> = page
        .lines
        .iter()
        .enumerate()
        .map(|(k, line)| {
            let text = line.text.as_str();
            let toc = is_toc(text);
            let bare = is_bare_caption(text) && !continues_paragraph(page, k);
            (toc, !toc && (is_caption(text) || bare))
        })
        .collect();
    for (line, (toc, caption)) in page.lines.iter_mut().zip(kinds) {
        if line.role == ROLE_FURNITURE {
            continue;
        }
        if toc {
            if tag(line, ROLE_TOC) {
                report.role_toc += 1;
            }
        } else if caption && tag(line, ROLE_CAPTION) {
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
            let (merged, superscripts) = merge_scripts(page, w);
            report.scripts_merged += merged;
            report.superscripts_merged += superscripts;
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

/// Compile every regex this module uses, so the first document does not pay
/// for it inside its stage timings. Repeated calls are cheap.
pub fn warm_up() {
    let accessors: &[fn() -> &'static Regex] = &[
        page_number_re,
        roman_re,
        stamp_re,
        abstract_re,
        abstract_heading_re,
        introduction_re,
        caption_re,
        caption_bare_re,
    ];
    for accessor in accessors {
        accessor();
    }
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

    /// [`hyphen_policy`] against a fixed document vocabulary.
    fn policy(left: &str, right: &str, seen: &[&str]) -> HyphenPolicy {
        let vocab: BTreeSet<String> = seen.iter().map(|word| String::from(*word)).collect();
        hyphen_policy(left, right, &|piece: &str| vocab.contains(piece))
    }

    /// Rule 1: an attested joined word wins over every keep rule.
    #[test]
    fn hyphen_policy_joins_an_attested_word_first() {
        use HyphenPolicy::Join;
        assert_eq!(policy("with", "out", &["with", "out", "without"]), Join);
        assert_eq!(policy("work", "flow", &["work", "flow", "workflow"]), Join);
        assert_eq!(
            policy(
                "noise",
                "regularized",
                &["noiseregularized", "noise-regularized"]
            ),
            Join
        );
        assert_eq!(policy("opti", "mization", &["optimization"]), Join);
        assert_eq!(policy("self", "supervised", &["selfsupervised"]), Join);
    }

    /// Rule 2: an attested hyphenated pair keeps the hyphen.
    #[test]
    fn hyphen_policy_keeps_an_attested_compound() {
        use HyphenPolicy::Keep;
        assert_eq!(policy("noise", "regularized", &["noise-regularized"]), Keep);
        assert_eq!(policy("pre", "serving", &["pre-serving"]), Keep);
    }

    /// Rules 3 and 4: compound prefixes keep, bound prefixes join.
    #[test]
    fn hyphen_policy_prefix_lists() {
        use HyphenPolicy::{Join, Keep};
        assert_eq!(policy("self", "supervised", &[]), Keep);
        assert_eq!(policy("Cross", "domain", &[]), Keep);
        assert_eq!(policy("well", "known", &[]), Keep);
        assert_eq!(policy("pre", "serving", &[]), Join);
        assert_eq!(policy("pre", "serving", &["pre", "serving"]), Join);
        assert_eq!(policy("non", "linear", &[]), Join);
        assert_eq!(policy("multi", "modal", &[]), Join);
        assert_eq!(policy("re", "use", &["use"]), Join);
        // A capitalised right half is not a bound-prefix join.
        assert_eq!(policy("pre", "MRI", &["mri"]), Keep);
        // Too short and unattested: decided by the later rules.
        assert_eq!(policy("co", "rn", &[]), Keep);
    }

    /// Rule 5: capitals, digits, one-letter or acronym left halves and
    /// right halves too short for a typeset break keep the hyphen.
    #[test]
    fn hyphen_policy_keeps_printed_compounds() {
        use HyphenPolicy::{Join, Keep};
        assert_eq!(policy("most", "dl", &["most"]), Keep);
        assert_eq!(policy("MOST", "dl", &[]), Keep);
        assert_eq!(policy("deep", "ai", &[]), Keep);
        assert_eq!(policy("deep", "cnn", &[]), Keep);
        assert_eq!(policy("k", "space", &["space"]), Keep);
        assert_eq!(policy("MRI", "guided", &[]), Keep);
        assert_eq!(policy("resnet", "v2", &[]), Keep);
        assert_eq!(policy("state", "of", &[]), Keep);
        // Three-letter word endings are ordinary breaks.
        assert_eq!(policy("learn", "ing", &[]), Join);
        assert_eq!(policy("cur", "ves", &[]), Join);
    }

    /// Rule 6 and the fallback join.
    #[test]
    fn hyphen_policy_keeps_two_attested_words_and_joins_the_rest() {
        use HyphenPolicy::{Join, Keep};
        assert_eq!(policy("cost", "effective", &["cost", "effective"]), Keep);
        assert_eq!(policy("Dual", "domain", &["dual", "domain"]), Keep);
        assert_eq!(policy("Dual", "channel", &["dual", "channel"]), Keep);
        assert_eq!(
            policy("noise", "regularized", &["noise", "regularized"]),
            Keep
        );
        // A three-letter half counts when it is not an ambiguous one.
        assert_eq!(policy("web", "based", &["web", "based"]), Keep);
        // `out` is an ambiguous short half: `without` unseen still joins.
        assert_eq!(policy("with", "out", &["with", "out"]), Join);
        assert_eq!(policy("con", "tent", &["con", "tent"]), Join);
        assert_eq!(policy("out", "put", &["out", "put"]), Join);
        assert_eq!(policy("in", "formation", &["in", "formation"]), Join);
        assert_eq!(policy("cost", "effective", &["cost"]), Join);
        assert_eq!(policy("opti", "mization", &[]), Join);
        assert_eq!(policy("algo", "rithm", &[]), Join);
        assert_eq!(policy("noise", "regularized", &[]), Join);
    }

    /// The observed reference-title cases through the whole pass.
    #[test]
    fn hyphen_policy_in_the_document_pass() {
        let mut pages = vec![page_of(
            1,
            &[
                ("structure pre-", 60.0, 600.0, 0),
                ("serving reconstruction of scans.", 60.0, 588.0, 0),
                ("the with-", 60.0, 576.0, 0),
                (
                    "out step, with and without it, out of range.",
                    60.0,
                    564.0,
                    0,
                ),
                ("a noise-", 60.0, 552.0, 0),
                (
                    "regularized prior and a noise-regularized loss.",
                    60.0,
                    540.0,
                    0,
                ),
                ("the most-", 60.0, 528.0, 0),
                ("dl network.", 60.0, 516.0, 0),
            ],
        )];
        let report = clean_document(&mut pages);
        assert_eq!(report.hyphens_joined, 2);
        assert_eq!(report.hyphens_kept, 2);
        assert_eq!(
            pages[0].text,
            "structure preserving\nreconstruction of scans.\n\
             the without\nstep, with and without it, out of range.\n\
             a noise-\nregularized prior and a noise-regularized loss.\n\
             the most-\ndl network."
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
    fn bare_caption_starts_are_tagged_unless_they_continue_a_paragraph() {
        let mut pages = vec![
            page_of(1, &[("Body text on page one", 60.0, 600.0, 0)]),
            page_of(
                2,
                &[
                    (
                        "Fig. 3 Overview of the judge and extractor choice",
                        60.0,
                        700.0,
                        0,
                    ),
                    ("Table 2 Results on the benchmark", 60.0, 660.0, 0),
                    (
                        "Figure 1 The overall framework of the model",
                        60.0,
                        620.0,
                        0,
                    ),
                    ("Figure 3 shows the results.", 60.0, 580.0, 0),
                    ("TABLE IV Error Rates", 60.0, 540.0, 0),
                    (
                        "the accuracy of the two systems is compared in",
                        60.0,
                        500.0,
                        0,
                    ),
                    ("Table 5 The numbers there are averages.", 60.0, 488.0, 0),
                ],
            ),
        ];
        let before = pages[1].text.clone();
        let report = clean_document(&mut pages);
        assert_eq!(report.role_caption, 4);
        assert_eq!(
            roles(&pages[1]),
            [
                "caption", "caption", "caption", "body", "caption", "body", "body"
            ]
        );
        assert_eq!(pages[1].text, before);
        assert!(is_bare_caption("Fig. 3 Overview of the judge"));
        assert!(is_bare_caption("Table S1 Data sources"));
        assert!(!is_bare_caption("Table 2 shows the gains"));
        assert!(!is_bare_caption("Figure 3 GPT results"));
        assert!(!is_bare_caption("Algorithm 1 Greedy Search"));
    }

    #[test]
    fn a_prose_run_ends_front_matter_without_an_abstract_line() {
        let affiliation = "Department of Physics, University of Somewhere, 1 Main Street, \
                           Some City, Some Country, Earth";
        let names = "Carl Coauthor, Dana Doe, Eve Example, Finn Fourth, Gail Fifth, \
                     Hal Sixth, Ida Seventh";
        let mut pages = vec![page_of(
            1,
            &[
                ("A Study of Things", 60.0, 700.0, 0),
                ("Ann Author, Bob Writer", 60.0, 688.0, 0),
                (affiliation, 60.0, 676.0, 0),
                (names, 60.0, 664.0, 0),
                (
                    "we study the dynamics of chemical reaction networks with the goal of",
                    60.0,
                    640.0,
                    0,
                ),
                (
                    "deriving an upper bound on their rates, which is hard because",
                    60.0,
                    628.0,
                    0,
                ),
                ("1 Introduction", 60.0, 600.0, 0),
                ("Things matter.", 60.0, 588.0, 0),
            ],
        )];
        let before = pages[0].text.clone();
        let report = clean_document(&mut pages);
        assert_eq!(report.role_front, 4);
        assert_eq!(
            roles(&pages[0]),
            [
                "front", "front", "front", "front", "body", "body", "body", "body"
            ]
        );
        assert_eq!(pages[0].text, before);
    }

    #[test]
    fn a_long_line_without_an_affiliation_signal_is_not_front_matter() {
        let notice = "This work has been submitted to the IEEE for possible publication \
                      and may change without notice";
        let mut pages = vec![page_of(
            1,
            &[
                ("A Study of Things", 60.0, 700.0, 0),
                (notice, 60.0, 688.0, 0),
                ("Ann Author", 60.0, 676.0, 0),
                ("Abstract", 60.0, 640.0, 0),
                ("We study things.", 60.0, 628.0, 0),
            ],
        )];
        let report = clean_document(&mut pages);
        assert_eq!(report.role_front, 2);
        assert_eq!(
            roles(&pages[0]),
            ["front", "body", "front", "heading", "body"]
        );
        assert!(is_front_prose(
            "we study the dynamics of chemical reaction networks with the goal of"
        ));
        assert!(!is_front_prose(
            "Carl Coauthor, Dana Doe, Eve Example, Finn Fourth, Gail Fifth, Hal Sixth"
        ));
        assert!(!is_front_prose(
            "we thank the department of physics at the University of Somewhere for"
        ));
        assert!(!is_long_body_line(
            "Carl Coauthor, Dana Doe, Eve Example, Finn Fourth, Gail Fifth, Hal Sixth, Ida Seventh"
        ));
    }

    #[test]
    fn a_twelve_word_sentence_line_is_not_front_matter() {
        let notice = "This paper was accepted at the main conference and presented there in person";
        let affiliation = "Department of Computer Science, University of Somewhere, housed in the \
                           old buildings";
        let mut pages = vec![page_of(
            1,
            &[
                ("A Study of Things", 60.0, 700.0, 0),
                (notice, 60.0, 688.0, 0),
                ("Ann Author", 60.0, 676.0, 0),
                (affiliation, 60.0, 664.0, 0),
                ("Abstract", 60.0, 640.0, 0),
                ("We study things.", 60.0, 628.0, 0),
            ],
        )];
        let report = clean_document(&mut pages);
        assert_eq!(report.role_front, 3);
        assert_eq!(
            roles(&pages[0]),
            ["front", "body", "front", "front", "heading", "body"]
        );
        assert!(has_verb_ending(notice));
        assert!(!has_verb_ending("A Study of Things and Their Uses"));
        assert!(!has_verb_ending("we saw it all"));
    }

    #[test]
    fn a_run_of_three_shorter_prose_lines_ends_front_matter() {
        let mut pages = vec![page_of(
            1,
            &[
                ("A Study of Things", 60.0, 700.0, 0),
                ("Ann Author", 60.0, 688.0, 0),
                ("University of Toronto & Vector Institute", 60.0, 676.0, 0),
                ("kernels and the attention layers with", 60.0, 652.0, 0),
                ("kernels which, when executed on the", 60.0, 640.0, 0),
                ("host processors, can be very slow", 60.0, 628.0, 0),
                ("1 Introduction", 60.0, 600.0, 0),
                ("Things matter.", 60.0, 588.0, 0),
            ],
        )];
        let report = clean_document(&mut pages);
        assert_eq!(report.role_front, 3);
        assert_eq!(
            roles(&pages[0]),
            [
                "front", "front", "front", "body", "body", "body", "body", "body"
            ]
        );
        assert!(is_short_front_prose(
            "kernels and the attention layers with"
        ));
        assert!(!is_short_front_prose(
            "University of Toronto & Vector Institute"
        ));
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

    /// Page 1 with `spans` and one body line per entry of `lines` (span
    /// indices); a line's text is its span texts joined by spaces.
    fn page_with(spans: Vec<Span>, lines: &[&[u32]]) -> PageText {
        let mut page = PageText::new(1, 612.0, 792.0, 0);
        page.spans = spans;
        for members in lines {
            let texts: Vec<&str> = members
                .iter()
                .map(|i| page.spans[*i as usize].text.as_str())
                .collect();
            let text = texts.join(" ");
            let mut bbox: Option<BBox> = None;
            for i in *members {
                let b = page.spans[*i as usize].bbox.unwrap();
                bbox = Some(bbox.map_or(b, |a| union(a, b)));
            }
            page.lines.push(Line {
                text,
                bbox,
                column: 0,
                spans: members.to_vec(),
                role: ROLE_BODY.to_string(),
            });
        }
        page.text = joined(&page.lines);
        page
    }

    #[test]
    fn script_forms() {
        assert_eq!(script_form("5", true).as_deref(), Some("\u{2075}"));
        assert_eq!(
            script_form("5\u{2013}7", true).as_deref(),
            Some("\u{2075}\u{207B}\u{2077}")
        );
        assert_eq!(
            script_form("10, 11", true).as_deref(),
            Some("\u{00B9}\u{2070},\u{00B9}\u{00B9}")
        );
        assert_eq!(script_form("a", true).as_deref(), Some("\u{1D43}"));
        assert_eq!(script_form("n", true).as_deref(), Some("\u{207F}"));
        assert_eq!(script_form("q", true), None);
        assert_eq!(script_form("N", true), None);
        assert_eq!(script_form("*", true).as_deref(), Some("*"));
        assert_eq!(script_form("3", false).as_deref(), Some("\u{2083}"));
        assert_eq!(script_form("a", false), None);
        assert_eq!(script_form("ing", true), None);
        assert_eq!(script_form("ab", true), None);
        assert_eq!(script_form("-", true), None);
        assert_eq!(script_form("1234567890123", true), None);
        assert_eq!(script_form("1-2", false), None);
    }

    #[test]
    fn raised_number_attaches_to_the_word_before_it() {
        let spans = vec![
            span_at("the literature.", 50.0, 398.0, 10.0, 0),
            span_at("5", 125.5, 401.0, 6.0, 1),
        ];
        let mut pages = vec![page_with(spans, &[&[1], &[0]])];
        let report = clean_document(&mut pages);
        assert_eq!(report.superscripts_merged, 1);
        assert_eq!(report.scripts_merged, 0);
        assert_eq!(pages[0].text, "the literature.\u{2075}");
        assert_eq!(pages[0].lines.len(), 1);
        assert_eq!(pages[0].lines[0].spans, [0, 1]);
        assert_eq!(pages[0].spans.len(), 2);
        let once = pages.clone();
        let again = clean_document(&mut pages);
        assert_eq!(again.superscripts_merged, 0);
        assert_eq!(pages, once);
    }

    #[test]
    fn raised_range_becomes_superscript_digits_and_minus() {
        let spans = vec![
            span_at("the Initiative", 50.0, 398.0, 10.0, 0),
            span_at("5\u{2013}7", 120.5, 401.0, 6.0, 1),
        ];
        let mut pages = vec![page_with(spans, &[&[1], &[0]])];
        let report = clean_document(&mut pages);
        assert_eq!(report.superscripts_merged, 1);
        assert_eq!(pages[0].text, "the Initiative\u{2075}\u{207B}\u{2077}");
    }

    #[test]
    fn raised_number_before_a_comma_span_drops_the_space() {
        let spans = vec![
            span_at("the Initiative", 50.0, 398.0, 10.0, 0),
            span_at(",", 124.0, 398.0, 10.0, 1),
            span_at("6", 120.5, 401.0, 6.0, 2),
        ];
        let mut pages = vec![page_with(spans, &[&[2], &[0, 1]])];
        let report = clean_document(&mut pages);
        assert_eq!(report.superscripts_merged, 1);
        assert_eq!(pages[0].text, "the Initiative\u{2076},");
        assert_eq!(pages[0].lines[0].spans, [0, 2, 1]);
    }

    #[test]
    fn raised_number_at_line_start_attaches_to_the_next_word() {
        let spans = vec![
            span_at("Prior work by Smith et al.", 50.0, 412.0, 10.0, 0),
            span_at("38", 50.0, 401.0, 6.0, 1),
            span_at("Prein and co", 50.0, 398.0, 10.0, 2),
        ];
        let mut pages = vec![page_with(spans, &[&[0], &[1], &[2]])];
        let report = clean_document(&mut pages);
        assert_eq!(report.superscripts_merged, 1);
        assert_eq!(
            pages[0].text,
            "Prior work by Smith et al.\n\u{00B3}\u{2078}Prein and co"
        );
        assert_eq!(pages[0].lines[1].spans, [1, 2]);
    }

    #[test]
    fn fragments_on_one_row_merge_at_their_positions() {
        let spans = vec![
            span_at("papers from arXiv", 50.0, 398.0, 10.0, 0),
            span_at(", ChemRxiv", 140.0, 398.0, 10.0, 1),
            span_at(", and 1999 data", 195.0, 398.0, 10.0, 2),
            span_at("9", 135.5, 401.0, 6.0, 3),
            span_at("10, 11", 190.5, 401.0, 6.0, 4),
        ];
        let mut pages = vec![page_with(spans, &[&[3], &[4], &[0, 1, 2]])];
        let report = clean_document(&mut pages);
        assert_eq!(report.superscripts_merged, 2);
        assert_eq!(
            pages[0].text,
            "papers from arXiv\u{2079}, ChemRxiv\u{00B9}\u{2070},\u{00B9}\u{00B9}, and 1999 data"
        );
        assert_eq!(pages[0].lines[0].spans, [0, 3, 1, 4, 2]);
    }

    #[test]
    fn raised_letters_and_body_size_numbers_stay() {
        // `ing` sits in the raised window but overlaps the base line too
        // little for the general script rule.
        for (text, size, y0) in [("ing", 6.0, 406.0), ("38", 10.0, 401.0)] {
            let spans = vec![
                span_at("the literature.", 50.0, 398.0, 10.0, 0),
                span_at(text, 125.5, y0, size, 1),
            ];
            let mut pages = vec![page_with(spans, &[&[1], &[0]])];
            let before = pages[0].clone();
            let report = clean_document(&mut pages);
            assert_eq!(report.superscripts_merged, 0, "{text}");
            assert_eq!(report.scripts_merged, 0, "{text}");
            assert_eq!(pages[0], before, "{text}");
        }
    }

    #[test]
    fn lowered_digit_becomes_a_subscript_of_its_own_line() {
        // The `3` also lies in the raised window of the line below; the
        // offset closer to a typical subscript wins.
        let spans = vec![
            span_at("NH", 50.0, 398.0, 10.0, 0),
            span_at("3", 60.5, 396.0, 6.0, 1),
            span_at("and water", 50.0, 386.0, 10.0, 2),
        ];
        let mut pages = vec![page_with(spans, &[&[0], &[1], &[2]])];
        let report = clean_document(&mut pages);
        assert_eq!(report.superscripts_merged, 1);
        assert_eq!(pages[0].text, "NH\u{2083}\nand water");
        assert_eq!(pages[0].lines[0].spans, [0, 1]);
    }

    /// The scan over every line of the page that `superscript_target`
    /// replaced, kept as the oracle for its window search.
    fn naive_target(page: &PageText, w: &PageWork, index: usize) -> Option<(usize, String)> {
        let line = page.lines.get(index)?;
        let raised_form = script_form(&line.text, true);
        let lowered_form = script_form(&line.text, false);
        if raised_form.is_none() && lowered_form.is_none() {
            return None;
        }
        let bbox = norm(line.bbox?);
        let size = line_size(page, line)?;
        let mut best: Option<(usize, f32, bool)> = None;
        for (j, other) in page.lines.iter().enumerate() {
            if j == index || !w.is_body(j) {
                continue;
            }
            let (Some(ob), Some((baseline, other_size))) =
                (other.bbox.map(norm), baseline_of(page, other))
            else {
                continue;
            };
            if size > (SCRIPT_RATIO + RATIO_SLACK) * other_size {
                continue;
            }
            let gap = (ob.x0 - bbox.x1).max(bbox.x0 - ob.x1).max(0.0);
            if gap > SUPERSCRIPT_REACH * other_size {
                continue;
            }
            let offset = (bbox.y0 - baseline) / other_size;
            let (raised, miss) =
                if raised_form.is_some() && (RAISED_LOW..=RAISED_HIGH).contains(&offset) {
                    (true, (offset - RAISED_IDEAL).abs())
                } else if lowered_form.is_some() && (LOWERED_LOW..RAISED_LOW).contains(&offset) {
                    (false, (offset - LOWERED_IDEAL).abs())
                } else {
                    continue;
                };
            let score = miss + gap / other_size;
            if best.is_none_or(|(_, s, _)| score < s) {
                best = Some((j, score, raised));
            }
        }
        let (target, _, raised) = best?;
        let form = if raised { raised_form } else { lowered_form };
        form.map(|f| (target, f))
    }

    /// `superscript_target` agrees with [`naive_target`] on every line.
    fn assert_matches_naive(page: &PageText) {
        let w = prepare(page);
        assert!(w.eligible);
        let geom = PageGeom::new(page);
        let mut window: Vec<usize> = Vec::new();
        for k in 0..page.lines.len() {
            assert_eq!(
                superscript_target(page, &w, &geom, k, &mut window),
                naive_target(page, &w, k),
                "line {k}"
            );
        }
    }

    #[test]
    fn window_search_matches_a_scan_of_every_line() {
        let spans = vec![
            // 0: best base is the 10 pt line; the 20 pt line also reaches it.
            span_at("5", 85.0, 401.0, 6.0, 0),
            // 1: reached only by a 20 pt line whose baseline lies outside
            // the window a 10 pt line would give.
            span_at("7", 300.0, 401.0, 6.0, 1),
            // 2: 10 pt lines just inside and just outside both edges.
            span_at("3", 450.0, 401.0, 6.0, 2),
            span_at("the literature.", 50.0, 398.0, 10.0, 3),
            span_at("Big", 50.0, 387.0, 20.0, 4),
            span_at("Big", 270.0, 387.0, 20.0, 5),
            // Offsets from fragment 2: 0.89, 0.91, -0.69 and -0.71.
            span_at("inside high", 400.0, 390.1, 10.0, 6),
            span_at("outside high", 400.0, 389.9, 10.0, 7),
            span_at("inside low", 400.0, 405.9, 10.0, 8),
            span_at("outside low", 400.0, 406.1, 10.0, 9),
            // A second column with its fragment first in reading order.
            span_at("12", 380.5, 501.0, 6.0, 10),
            span_at("in two studies", 320.0, 498.0, 10.0, 11),
        ];
        let page = page_with(
            spans,
            &[
                &[0],
                &[1],
                &[2],
                &[3],
                &[4],
                &[5],
                &[6],
                &[7],
                &[8],
                &[9],
                &[10],
                &[11],
            ],
        );
        assert_matches_naive(&page);
        let w = prepare(&page);
        let geom = PageGeom::new(&page);
        let mut window: Vec<usize> = Vec::new();
        let mut target = |k: usize| superscript_target(&page, &w, &geom, k, &mut window);
        assert_eq!(target(0), Some((3, "\u{2075}".to_string())));
        assert_eq!(target(1), Some((5, "\u{2077}".to_string())));
        assert_eq!(target(2), Some((8, "\u{2083}".to_string())));
        assert_eq!(target(10), Some((11, "\u{00B9}\u{00B2}".to_string())));
    }

    #[test]
    fn fragments_far_from_their_base_lines_in_reading_order_merge() {
        // Both fragments come first in reading order and their base lines
        // last, with a column of filler lines in between.
        let mut spans = vec![
            span_at("7", 125.5, 401.0, 6.0, 0),
            span_at("12", 385.5, 501.0, 6.0, 1),
        ];
        for k in 0..14u16 {
            let y = 680.0 - 20.0 * f32::from(k);
            let seq = u32::from(k) + 2;
            spans.push(span_at("filler text of the body", 50.0, y, 10.0, seq));
        }
        spans.push(span_at("the literature.", 50.0, 398.0, 10.0, 16));
        spans.push(span_at("in two studies", 320.0, 498.0, 10.0, 17));
        let members: Vec<Vec<u32>> = (0..18u32).map(|i| vec![i]).collect();
        let lines: Vec<&[u32]> = members.iter().map(Vec::as_slice).collect();
        let page = page_with(spans, &lines);
        assert_matches_naive(&page);
        let mut pages = vec![page];
        let report = clean_document(&mut pages);
        assert_eq!(report.superscripts_merged, 2);
        assert_eq!(report.scripts_merged, 0);
        assert_eq!(pages[0].lines.len(), 16);
        assert!(pages[0].text.contains("the literature.\u{2077}"));
        assert!(pages[0].text.contains("in two studies\u{00B9}\u{00B2}"));
        assert_eq!(pages[0].lines[14].spans, [16, 0]);
        assert_eq!(pages[0].lines[15].spans, [17, 1]);
    }
}
