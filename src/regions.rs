//! Region tagging: figure text, table cells and algorithm blocks next to
//! their captions get a non-body [`Line::role`], so body-only consumers can
//! leave them out. Runs after `text_cleanup::clean_document`; never changes
//! `PageText::text`, `spans` or line order, and never retags a line whose
//! role is not `body`.
//!
//! Geometry: boxes are PDF points with the origin bottom-left, so "above"
//! means a larger `y`. Regions are found in a horizontal *band* around the
//! caption (its column on a two-column page, the whole page when the page
//! is single-column or the caption spans the middle), not by
//! `Line::column`, which is the layout block index: scattered diagram
//! labels become blocks of their own.
//!
//! Captions: a line tagged `caption` whose text starts with `Figure`,
//! `Fig.`, `Table` or `Algorithm` (any case), or an untagged `body` line in
//! that shape that is not itself prose (`TABLE I`, `Algorithm 1 Name`,
//! `Table 2 Results`: a number followed by a capitalised word).
//! Continuation lines below a caption start, with no blank separator, are
//! tagged `caption` up to the first continuation line ending a sentence
//! (at most [`CAPTION_MAX_LINES`] lines in all). A continuation stops at a
//! prose-like line that opens a sentence (an uppercase first word and at
//! least 10 words) and, under a caption spanning the middle of a
//! two-column page, at a line that does not span it too. An untagged
//! caption start is tagged `caption` once a region is found under it. A
//! caption start directly under a prose line (no blank separator) is
//! ignored.
//!
//! Regions:
//! - figure: the lines above a figure caption up to the nearest prose
//!   paragraph, tagged `figure` when the region is fragment-like (at least
//!   60 % of its lines have at most 4 words or are numeric/axis-like, or,
//!   in a band at most 60 % of the page wide, the left edges scatter by
//!   more than 15 % of the band width). A line counts as numeric/axis-like
//!   when at most one of its words is not a number, or when at least 60 %
//!   of its words are numbers, whatever its length. When nothing
//!   fragment-like lies above, the lines below the caption are tried the
//!   same way (caption above the figure), cut at the first numbered
//!   section heading (`3 Method`, `3.1 Setup`).
//! - table: the lines below a table caption (caption above, ACM/IEEE) up
//!   to the next prose paragraph, else the lines above it (Elsevier),
//!   tagged `table` when at least 50 % of the lines have at most 5 words or
//!   carry at least 2 numeric tokens. Vertical lines (a box more than 3
//!   times taller than wide, or single characters stacked one above the
//!   other) below a table caption, up to the next prose-like line, are
//!   tagged `table` too.
//! - sideways tables: on a page where at least 60 % of the lines are
//!   vertical (a landscape float, `/Rotate` or a rotated table), boxes are
//!   turned a quarter turn so the text reads left to right (clockwise, or
//!   counter-clockwise for `/Rotate 270`). Below each `Table N` label
//!   (`Table A.6 continued from previous page` included) and its wide
//!   prose continuation lines (tagged `caption`), every line that is not
//!   prose-like is tagged `table`, up to the next `Table` label or the
//!   first prose-like line spanning at least 40 % of the turned page
//!   width. The other region walks and footnotes are skipped there.
//! - shredded sideways pages: when the lines are not vertical but the
//!   spans are (at least 10 spans of 4 or more characters, 60 % of those
//!   with a telling shape, have boxes taller than wide), the lines are
//!   slices across rotated text. If the span texts, joined in stream order
//!   without whitespace, hold a `Table N` label, every `body` line that is
//!   not prose-like is tagged `table`; either way the walks and footnotes
//!   are skipped.
//! - algorithm: the lines below an `Algorithm N` caption up to the next
//!   prose paragraph, tagged `algorithm` when at least 30 % carry a marker
//!   (`Input:`, `Output:`, `Require:`, `Ensure:`, a `N:` step number, a
//!   leading `for`/`while`/`if`, `end for`, `return`, `←`, `:=`).
//!
//! - footnote: at the foot of a page (the lowest 35 %), the contiguous run
//!   of `body` lines at the bottom of a column set at most 0.92 times the
//!   page's body font size (the median size of its lines with at least 6
//!   words), from the first one opening with a footnote marker (`1 `,
//!   `*`, `†`, `‡`, `§`, `¶`, a superscript digit or letter) down. The run
//!   must have a body-size line above it and at most
//!   [`FOOTNOTE_MAX_LINES`] lines. Lines without font sizes are never
//!   footnotes. Tagged before the walks, so a walk stops at them.
//! - math: after the walks, a `body` line with at most 2 ordinary words
//!   (letter runs of 3 or more, not `log`, `max` and the like) and at least
//!   one math character (Mathematical Alphanumeric Symbols, Greek, `=`,
//!   `+`, `¬`, `×`, `‖`, `⟨⟩`, arrows or the Mathematical Operators block),
//!   whose ordinary-word letters are at most half of its other non-blank
//!   characters and which is not mostly numbers, is tagged `math`.
//!
//! Prose: a line with at least 6 words, at least half of them starting
//! lowercase and under 30 % numeric (so table rows and title-case header
//! rows are not prose; inside an algorithm walk marker lines never are).
//! A walk stops at two consecutive prose lines, at a prose line after or
//! before a blank separator, and (walking up) at a line ending a sentence
//! with a blank separator below it.
//!
//! Hard guards: a *prose-like* line (at least 7 words, at most 30 %
//! numeric tokens, at most 2 all-caps or abbreviation tokens, and at least
//! 2 words starting lowercase) is never tagged `figure`, `table` or
//! `algorithm` (pseudo-code marker lines excepted under an `Algorithm`
//! caption), and every walk stops at the first one. Figure and table
//! regions need at least [`MIN_REGION_LINES`] lines, and a region longer
//! than [`REGION_MAX_LINES`] lines is dropped, not tagged. A page that
//! already carries a `regions:` warning is not tagged again.

use std::cmp::Ordering;

use crate::reading_order::median;
use crate::schema::{BBox, Line, PageText};

/// Most lines a caption (start plus continuations) may take.
pub const CAPTION_MAX_LINES: usize = 6;
/// Fewest lines a figure or table region needs to be tagged.
pub const MIN_REGION_LINES: usize = 2;
/// Most lines a figure, table or algorithm region may take; a longer walk
/// ran through body text, so the region is dropped.
pub const REGION_MAX_LINES: usize = 40;
/// A vertical gap wider than this many median line heights is a blank
/// separator. Boxes span one font size per line and lines advance about
/// 1.2 sizes, so ordinary leading leaves a gap near 0.2.
const BLANK_GAP: f32 = 0.6;
/// Line height used when a page has no boxes.
const FALLBACK_HEIGHT: f32 = 10.0;
/// Fewest words in a prose line.
const PROSE_WORDS: usize = 6;
/// Fewest words in a prose-like line (the hard guard).
const PROSE_LIKE_WORDS: usize = 7;
/// Most all-caps or abbreviation tokens in a prose-like line.
const PROSE_LIKE_CAPS: usize = 2;
/// Fewest words starting lowercase in a prose-like line.
const PROSE_LIKE_LOWER: usize = 2;
/// Fewest words in a prose-like line that opens a sentence and so cannot
/// continue a caption.
const SENTENCE_WORDS: usize = 10;
/// Most words in a figure fragment.
const FRAGMENT_WORDS: usize = 4;
/// Most words in a table cell line.
const CELL_WORDS: usize = 5;
/// Share of the page width around the middle treated as the gutter.
const GUTTER: f32 = 0.02;
/// Widest band, as a share of the page width, where the left-edge scatter
/// test applies (one column; a whole two-column page always scatters).
const SCATTER_BAND: f32 = 0.6;
/// Prefix of the page warnings this pass adds.
const WARNING_PREFIX: &str = "regions: ";

/// A box taller than this multiple of its width holds vertical text.
const VERTICAL_RATIO: f32 = 3.0;
/// Fewest boxed lines of at least 2 characters on a sideways page.
const SIDEWAYS_MIN_LINES: usize = 5;
/// Share of the turned page width a prose-like line must span to end a
/// sideways table.
const SIDEWAYS_PARAGRAPH_WIDTH: f32 = 0.4;
/// Fewest spans of at least [`SPAN_SHAPE_CHARS`] characters, set
/// vertically, on a page whose spans show it is sideways.
const SIDEWAYS_MIN_SPANS: usize = 10;
/// Fewest characters in a span whose box shape tells its direction.
const SPAN_SHAPE_CHARS: usize = 4;
/// A span box more than this many times taller than wide runs vertically
/// (more than this many times wider than tall, horizontally).
const SPAN_ASPECT: f32 = 1.2;
/// Largest font size, as a share of the page's body size, of a footnote
/// line.
const FOOTNOTE_SIZE_RATIO: f32 = 0.92;
/// Share of the page height, from the bottom, where footnotes sit.
const FOOTNOTE_ZONE: f32 = 0.35;
/// Most lines in a footnote run; a longer small-font run at the page foot
/// is something else (a reference list set small, say).
pub const FOOTNOTE_MAX_LINES: usize = 10;
/// Fewest sized lines of at least 6 words needed to measure a page's body
/// font size.
const BODY_SIZE_MIN_LINES: usize = 3;
/// Most ordinary words in a display-math line.
const MATH_MAX_WORDS: usize = 2;
/// Shortest letter run that counts as an ordinary word in the math test.
const MATH_WORD_LETTERS: usize = 3;
/// Lower-cased letter runs that are math functions, not words.
const MATH_FUNCTIONS: [&str; 19] = [
    "inf", "sup", "log", "exp", "min", "max", "arg", "argmin", "argmax", "sin", "cos", "tan",
    "lim", "det", "mod", "var", "cov", "diag", "sgn",
];
const ROLE_BODY: &str = "body";
const ROLE_CAPTION: &str = "caption";
const ROLE_FURNITURE: &str = "furniture";
const ROLE_TABLE: &str = "table";
const ROLE_MATH: &str = "math";
const ROLE_FOOTNOTE: &str = "footnote";

/// Lower-cased markers that make a line look like pseudo-code anywhere.
const ALGORITHM_MARKERS: [&str; 13] = [
    "input:",
    "output:",
    "require:",
    "ensure:",
    "return",
    "\u{2190}",
    ":=",
    "end for",
    "end while",
    "end if",
    "end function",
    "end procedure",
    "until ",
];

/// Lower-cased first words that make a line look like pseudo-code.
const ALGORITHM_STARTS: [&str; 9] = [
    "for",
    "while",
    "if",
    "else",
    "repeat",
    "foreach",
    "procedure",
    "function",
    "do",
];

/// Lines newly tagged by [`tag_regions`], per role.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RegionReport {
    /// Caption continuation lines, and untagged caption starts, tagged `caption`.
    pub caption: usize,
    /// Lines tagged `figure`.
    pub figure: usize,
    /// Lines tagged `table`.
    pub table: usize,
    /// Lines tagged `algorithm`.
    pub algorithm: usize,
    /// Display-math fragment lines tagged `math`.
    pub math: usize,
    /// Page-foot note lines tagged `footnote`.
    pub footnote: usize,
}

/// Kind of caption a region hangs off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Figure,
    Table,
    Algorithm,
}

impl Kind {
    fn role(self) -> &'static str {
        match self {
            Self::Figure => "figure",
            Self::Table => ROLE_TABLE,
            Self::Algorithm => "algorithm",
        }
    }
}

/// Horizontal band a caption's region lives in.
#[derive(Clone, Copy, Debug)]
struct Band {
    lo: f32,
    hi: f32,
    width: f32,
    /// The gutter of a two-column page when the caption spans it: caption
    /// continuation lines must span it too.
    cross: Option<(f32, f32)>,
}

impl Band {
    /// At least half of the box's width lies inside the band.
    fn holds(self, b: BBox) -> bool {
        let w = b.x1 - b.x0;
        if w <= 0.0 {
            let centre = b.x0;
            return (self.lo..=self.hi).contains(&centre);
        }
        let overlap = b.x1.min(self.hi) - b.x0.max(self.lo);
        overlap >= 0.5 * w
    }

    /// The box spans the gutter, or the band has none to span.
    fn crossed_by(self, b: BBox) -> bool {
        self.cross.is_none_or(|(lo, hi)| b.x0 < lo && b.x1 > hi)
    }
}

/// Page-wide measures shared by every caption on the page.
struct PageGeometry {
    blank: f32,
    width: f32,
    mid: f32,
    two_column: bool,
}

/// Tag caption continuations, figure text, table cells, algorithm blocks,
/// footnotes and display-math lines on every page (see the module
/// documentation). Adds a page
/// warning such as `regions: figure text N lines` for each role with new
/// tags. Idempotent: a page already carrying such a warning is skipped,
/// and a page without one had nothing to tag.
pub fn tag_regions(pages: &mut [PageText]) -> RegionReport {
    let mut total = RegionReport::default();
    for page in pages {
        let report = tag_page(page);
        total.caption += report.caption;
        total.figure += report.figure;
        total.table += report.table;
        total.algorithm += report.algorithm;
        total.math += report.math;
        total.footnote += report.footnote;
        let counts = [
            ("caption text", report.caption),
            ("figure text", report.figure),
            ("table text", report.table),
            ("algorithm text", report.algorithm),
            ("math text", report.math),
            ("footnote text", report.footnote),
        ];
        for (name, n) in counts {
            if n > 0 {
                let msg = format!("{WARNING_PREFIX}{name} {n} lines");
                if !page.warnings.contains(&msg) {
                    page.warnings.push(msg);
                }
            }
        }
    }
    total
}

/// Tag one page; the counts are of lines newly tagged.
fn tag_page(page: &mut PageText) -> RegionReport {
    let mut report = RegionReport::default();
    if page.width <= 0.0 || page.lines.is_empty() {
        return report;
    }
    if page.warnings.iter().any(|w| w.starts_with(WARNING_PREFIX)) {
        return report;
    }
    if is_sideways(page) {
        report = tag_sideways(page);
        report.math += tag_math(page);
        return report;
    }
    if sideways_by_spans(page) {
        if spans_carry_table_label(page) {
            report.table += tag_shredded_table(page);
        }
        report.math += tag_math(page);
        return report;
    }
    let geometry = measure(page);
    report.footnote += tag_footnotes(page, &geometry);
    let captions: Vec<(usize, Kind)> = page
        .lines
        .iter()
        .enumerate()
        .filter_map(|(k, line)| caption_kind(line).map(|kind| (k, kind)))
        .collect();
    for &(k, kind) in &captions {
        let Some(b) = finite_box(&page.lines[k]) else {
            continue;
        };
        let band = band_for(&geometry, b);
        let entries = band_entries(page, band);
        let Some(pos) = entries.iter().position(|&i| i == k) else {
            continue;
        };
        if inside_prose(page, &entries, pos, geometry.blank) {
            continue;
        }
        let more = caption_continuation(page, &entries, pos, kind, band, geometry.blank);
        for i in more {
            report.caption += tag(&mut page.lines[i], ROLE_CAPTION);
        }
    }
    for &(k, kind) in &captions {
        let Some(b) = finite_box(&page.lines[k]) else {
            continue;
        };
        let band = band_for(&geometry, b);
        let entries = band_entries(page, band);
        let Some(pos) = entries.iter().position(|&i| i == k) else {
            continue;
        };
        if inside_prose(page, &entries, pos, geometry.blank) {
            continue;
        }
        let mut region: Vec<usize> = find_region(page, &entries, pos, kind, band, &geometry)
            .into_iter()
            .filter(|&i| taggable(&page.lines[i].text, kind))
            .collect();
        if kind == Kind::Table {
            region.extend(vertical_cells(page, &entries, pos));
        }
        if !region.is_empty() {
            report.caption += tag(&mut page.lines[k], ROLE_CAPTION);
        }
        let role = kind.role();
        let mut n = 0;
        for i in region {
            n += tag(&mut page.lines[i], role);
        }
        match kind {
            Kind::Figure => report.figure += n,
            Kind::Table => report.table += n,
            Kind::Algorithm => report.algorithm += n,
        }
    }
    report.math += tag_math(page);
    report
}

/// Set `role` on a `body` line; 1 when it changed, else 0.
fn tag(line: &mut Line, role: &str) -> usize {
    if line.role == ROLE_BODY {
        line.role = role.to_string();
        1
    } else {
        0
    }
}

/// A line with this text may take the region role of `kind`: it is not
/// prose-like, or it is a pseudo-code line under an `Algorithm` caption.
fn taggable(text: &str, kind: Kind) -> bool {
    !is_prose_like(text) || (kind == Kind::Algorithm && is_algorithm_line(text))
}

/// Median line height, page middle and whether the page is two-column
/// (more prose-length lines sit in one half than span the middle).
fn measure(page: &PageText) -> PageGeometry {
    let mut heights: Vec<f32> = page
        .lines
        .iter()
        .filter_map(finite_box)
        .map(|b| b.y1 - b.y0)
        .filter(|h| *h > 0.0)
        .collect();
    let height = median(&mut heights).unwrap_or(FALLBACK_HEIGHT);
    let width = page.width;
    let mid = 0.5 * width;
    let margin = GUTTER * width;
    let mut half: usize = 0;
    let mut full: usize = 0;
    for line in &page.lines {
        let Some(b) = finite_box(line) else {
            continue;
        };
        if word_count(&line.text) < PROSE_WORDS {
            continue;
        }
        if b.x1 <= mid + margin || b.x0 >= mid - margin {
            half += 1;
        } else {
            full += 1;
        }
    }
    PageGeometry {
        blank: BLANK_GAP * height,
        width,
        mid,
        two_column: half > full,
    }
}

/// The band of a caption with box `b`.
fn band_for(geometry: &PageGeometry, b: BBox) -> Band {
    let whole = Band {
        lo: f32::NEG_INFINITY,
        hi: f32::INFINITY,
        width: geometry.width,
        cross: None,
    };
    if !geometry.two_column {
        return whole;
    }
    let margin = GUTTER * geometry.width;
    let spans = b.x0 < geometry.mid - margin && b.x1 > geometry.mid + margin;
    if spans {
        return Band {
            cross: Some((geometry.mid - margin, geometry.mid + margin)),
            ..whole
        };
    }
    let half = 0.5 * geometry.width;
    if f32::midpoint(b.x0, b.x1) < geometry.mid {
        Band {
            lo: f32::NEG_INFINITY,
            hi: geometry.mid,
            width: half,
            cross: None,
        }
    } else {
        Band {
            lo: geometry.mid,
            hi: f32::INFINITY,
            width: half,
            cross: None,
        }
    }
}

/// Indices of the boxed lines in `band`, top first (then left first).
fn band_entries(page: &PageText, band: Band) -> Vec<usize> {
    let mut entries: Vec<(usize, BBox)> = page
        .lines
        .iter()
        .enumerate()
        .filter_map(|(k, line)| finite_box(line).map(|b| (k, b)))
        .filter(|(_, b)| band.holds(*b))
        .collect();
    entries.sort_by(|a, b| top_first(a.1, b.1));
    entries.into_iter().map(|(k, _)| k).collect()
}

fn top_first(a: BBox, b: BBox) -> Ordering {
    b.y1.total_cmp(&a.y1).then(a.x0.total_cmp(&b.x0))
}

/// The line's box with ordered corners, when all four are finite.
fn finite_box(line: &Line) -> Option<BBox> {
    let b = line.bbox?;
    if ![b.x0, b.y0, b.x1, b.y1].into_iter().all(f32::is_finite) {
        return None;
    }
    Some(BBox {
        x0: b.x0.min(b.x1),
        y0: b.y0.min(b.y1),
        x1: b.x0.max(b.x1),
        y1: b.y0.max(b.y1),
    })
}

/// Vertical gap between the line at `upper` and the line at `lower`.
fn gap(page: &PageText, upper: usize, lower: usize) -> f32 {
    match (
        finite_box(&page.lines[upper]),
        finite_box(&page.lines[lower]),
    ) {
        (Some(u), Some(l)) => u.y0 - l.y1,
        _ => 0.0,
    }
}

/// The line at `entries[pos]` continues a prose paragraph: the nearest
/// line above it is body prose with no blank separator between them (a
/// sentence that happens to start `Table 2. We compare`).
fn inside_prose(page: &PageText, entries: &[usize], pos: usize, blank: f32) -> bool {
    let Some(&upper) = pos.checked_sub(1).and_then(|u| entries.get(u)) else {
        return false;
    };
    let line = &page.lines[upper];
    line.role == ROLE_BODY && is_prose(&line.text) && gap(page, upper, entries[pos]) <= blank
}

/// The caption kind of a caption start line, if it is one.
fn caption_kind(line: &Line) -> Option<Kind> {
    let tagged = line.role == ROLE_CAPTION;
    if !tagged && line.role != ROLE_BODY {
        return None;
    }
    let text = line.text.trim();
    let mut words = text.split_whitespace();
    let first = words.next()?;
    let kind = if first.eq_ignore_ascii_case("figure") || first.eq_ignore_ascii_case("fig.") {
        Kind::Figure
    } else if first.eq_ignore_ascii_case("table") {
        Kind::Table
    } else if first.eq_ignore_ascii_case("algorithm") {
        Kind::Algorithm
    } else {
        return None;
    };
    if tagged {
        return Some(kind);
    }
    let number = words.next()?;
    let core = number.trim_end_matches([':', '.', '|']);
    let punctuated = core.len() < number.len();
    let digits = !core.is_empty() && core.chars().all(|c| c.is_ascii_digit() || c == '.');
    let roman = !core.is_empty() && core.chars().all(|c| matches!(c, 'I' | 'V' | 'X' | 'L'));
    if !(digits || (roman && kind == Kind::Table)) {
        return None;
    }
    if is_prose(text) {
        return None;
    }
    let next = words.next();
    let alone = next.is_none();
    let capitalised = next.is_some_and(|token| {
        let mut chars = token.chars();
        chars.next().is_some_and(char::is_uppercase) && chars.next().is_some_and(char::is_lowercase)
    });
    if punctuated || alone || capitalised || kind == Kind::Algorithm {
        Some(kind)
    } else {
        None
    }
}

/// The line may continue a caption in `band`: it does not open a prose
/// sentence and spans the gutter when the caption does.
fn continues_caption(line: &Line, band: Band) -> bool {
    !opens_sentence(&line.text) && finite_box(line).is_some_and(|b| band.crossed_by(b))
}

/// Body lines continuing the caption at `entries[pos]`: below it with no
/// blank separator, prose-like or ending a sentence, through the first
/// one that ends a sentence (the start's own full stop, as in
/// `Table 1: Main Leaderboard.`, does not end the caption). Under an
/// `Algorithm` caption a pseudo-code line ends it; so does a line that
/// cannot continue a caption (see [`continues_caption`]).
fn caption_continuation(
    page: &PageText,
    entries: &[usize],
    pos: usize,
    kind: Kind,
    band: Band,
    blank: f32,
) -> Vec<usize> {
    let mut more: Vec<usize> = Vec::new();
    let mut prev = entries[pos];
    for &i in entries.iter().skip(pos + 1) {
        if more.len() + 1 >= CAPTION_MAX_LINES {
            break;
        }
        let line = &page.lines[i];
        if line.role != ROLE_BODY || gap(page, prev, i) > blank {
            break;
        }
        if !continues_caption(line, band) {
            break;
        }
        let text = line.text.as_str();
        if kind == Kind::Algorithm && is_algorithm_line(text) {
            break;
        }
        let ends = ends_sentence(text);
        if !(ends || is_prose(text)) {
            break;
        }
        more.push(i);
        prev = i;
        if ends {
            break;
        }
    }
    more
}

/// The region to tag for the caption at `entries[pos]`, or nothing.
fn find_region(
    page: &PageText,
    entries: &[usize],
    pos: usize,
    kind: Kind,
    band: Band,
    geometry: &PageGeometry,
) -> Vec<usize> {
    let blank = geometry.blank;
    let scatter = band.width <= SCATTER_BAND * geometry.width;
    match kind {
        Kind::Figure => {
            let above = walk_up(page, entries, pos, blank);
            if fragment_like(page, &above, band, scatter) {
                return above;
            }
            let has_fragments = above.iter().any(|&i| is_fragment(&page.lines[i].text));
            if has_fragments {
                return Vec::new();
            }
            let mut below = walk_down(page, entries, pos, band, blank, false);
            if let Some(cut) = below
                .iter()
                .position(|&i| is_numbered_heading(&page.lines[i].text))
            {
                below.truncate(cut);
            }
            if fragment_like(page, &below, band, scatter) {
                below
            } else {
                Vec::new()
            }
        }
        Kind::Table => {
            let below = walk_down(page, entries, pos, band, blank, false);
            if table_like(page, &below) {
                return below;
            }
            let above = walk_up(page, entries, pos, blank);
            if table_like(page, &above) {
                above
            } else {
                Vec::new()
            }
        }
        Kind::Algorithm => {
            let below = walk_down(page, entries, pos, band, blank, true);
            if algorithm_like(page, &below) {
                below
            } else {
                Vec::new()
            }
        }
    }
}

/// Body lines above `entries[pos]` up to the nearest prose paragraph or
/// prose-like line, nearest first. Furniture is skipped; any other
/// non-body line stops. Stops once past [`REGION_MAX_LINES`] lines.
fn walk_up(page: &PageText, entries: &[usize], pos: usize, blank: f32) -> Vec<usize> {
    let mut region: Vec<usize> = Vec::new();
    let mut below = entries[pos];
    for (k, &i) in entries.iter().enumerate().take(pos).rev() {
        let line = &page.lines[i];
        if line.role == ROLE_FURNITURE {
            continue;
        }
        if line.role != ROLE_BODY {
            break;
        }
        let text = line.text.as_str();
        if is_prose_like(text) {
            break;
        }
        if ends_sentence(text) && gap(page, i, below) > blank {
            break;
        }
        let upper_prose = k
            .checked_sub(1)
            .and_then(|u| entries.get(u))
            .is_some_and(|&u| is_prose(&page.lines[u].text));
        if is_prose(text) && upper_prose {
            break;
        }
        region.push(i);
        if region.len() > REGION_MAX_LINES {
            break;
        }
        below = i;
    }
    region
}

/// Body lines below the caption at `entries[pos]` (after its continuation
/// lines, at most [`CAPTION_MAX_LINES`] in all) up to the next prose
/// paragraph or prose-like line. With `algorithm`, marker lines never
/// count as prose. Stops once past [`REGION_MAX_LINES`] lines.
fn walk_down(
    page: &PageText,
    entries: &[usize],
    pos: usize,
    band: Band,
    blank: f32,
    algorithm: bool,
) -> Vec<usize> {
    let prose = |i: usize| -> bool {
        let text = page.lines[i].text.as_str();
        is_prose(text) && !(algorithm && is_algorithm_line(text))
    };
    let prose_like = |i: usize| -> bool {
        let text = page.lines[i].text.as_str();
        is_prose_like(text) && !(algorithm && is_algorithm_line(text))
    };
    let mut region: Vec<usize> = Vec::new();
    let mut above = entries[pos];
    let mut in_caption = true;
    let mut caption_lines: usize = 1;
    for (k, &i) in entries.iter().enumerate().skip(pos + 1) {
        let line = &page.lines[i];
        if line.role == ROLE_FURNITURE {
            continue;
        }
        let gap_above = gap(page, above, i);
        if in_caption {
            let room = caption_lines < CAPTION_MAX_LINES;
            let continues = line.role == ROLE_CAPTION
                || (line.role == ROLE_BODY
                    && gap_above <= blank
                    && prose(i)
                    && continues_caption(line, band));
            if room && continues {
                caption_lines += 1;
                above = i;
                continue;
            }
            in_caption = false;
        }
        if line.role != ROLE_BODY || prose_like(i) {
            break;
        }
        if prose(i) {
            let next = entries.get(k + 1).copied();
            let next_prose = next.is_some_and(prose);
            let gap_below = next.map_or(0.0, |n| gap(page, i, n));
            let ends = ends_sentence(&line.text) && gap_below > blank;
            if next_prose || gap_above > blank || ends {
                break;
            }
        }
        region.push(i);
        if region.len() > REGION_MAX_LINES {
            break;
        }
        above = i;
    }
    region
}

/// Between [`MIN_REGION_LINES`] and [`REGION_MAX_LINES`] lines, of which at
/// least 60 % are short or axis-like, or (with `scatter`, for a one-column
/// band) whose left edges scatter by more than 15 % of the band width.
fn fragment_like(page: &PageText, region: &[usize], band: Band, scatter: bool) -> bool {
    let n = region.len();
    if !(MIN_REGION_LINES..=REGION_MAX_LINES).contains(&n) {
        return false;
    }
    let short = region
        .iter()
        .filter(|&&i| is_fragment(&page.lines[i].text))
        .count();
    if short * 10 >= n * 6 {
        return true;
    }
    if !scatter || n < 3 {
        return false;
    }
    let xs: Vec<f32> = region
        .iter()
        .filter_map(|&i| finite_box(&page.lines[i]))
        .map(|b| b.x0)
        .collect();
    if xs.len() < 3 {
        return false;
    }
    let count = xs.len() as f32;
    let mean = xs.iter().sum::<f32>() / count;
    let variance = xs.iter().map(|x| (x - mean) * (x - mean)).sum::<f32>() / count;
    variance.sqrt() > 0.15 * band.width
}

/// Between [`MIN_REGION_LINES`] and [`REGION_MAX_LINES`] lines, at least
/// 50 % of them with at most 5 words or at least 2 numeric tokens.
fn table_like(page: &PageText, region: &[usize]) -> bool {
    let n = region.len();
    if !(MIN_REGION_LINES..=REGION_MAX_LINES).contains(&n) {
        return false;
    }
    let cells = region
        .iter()
        .filter(|&&i| {
            let text = page.lines[i].text.as_str();
            word_count(text) <= CELL_WORDS || numeric_count(text) >= 2
        })
        .count();
    cells * 2 >= n
}

/// At most [`REGION_MAX_LINES`] lines, at least 30 % of them carrying a
/// pseudo-code marker.
fn algorithm_like(page: &PageText, region: &[usize]) -> bool {
    let n = region.len();
    if n == 0 || n > REGION_MAX_LINES {
        return false;
    }
    let marked = region
        .iter()
        .filter(|&&i| is_algorithm_line(&page.lines[i].text))
        .count();
    marked * 10 >= n * 3
}

fn word_count(text: &str) -> usize {
    text.split_whitespace().count()
}

/// A superscript digit such as the `²` in `10⁻²`.
fn is_superscript_digit(c: char) -> bool {
    matches!(
        c,
        '\u{2070}' | '\u{00b9}' | '\u{00b2}' | '\u{00b3}' | '\u{2074}'..='\u{2079}'
    )
}

/// A number such as `0.61`, `10⁻²`, `(400,`, `35%` or `1e-3`.
fn is_numeric_token(token: &str) -> bool {
    let core = token.trim_matches(|c: char| {
        matches!(
            c,
            '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';' | ':' | '%' | '\u{00b1}' | '*'
        )
    });
    let mut digit = false;
    for c in core.chars() {
        if c.is_ascii_digit() || is_superscript_digit(c) {
            digit = true;
        } else if !matches!(
            c,
            '.' | ','
                | '-'
                | '+'
                | '%'
                | '/'
                | '^'
                | 'e'
                | 'E'
                | '\u{2212}'
                | '\u{207b}'
                | '\u{207a}'
                | '\u{00d7}'
                | '\u{00b7}'
        ) {
            return false;
        }
    }
    digit
}

fn numeric_count(text: &str) -> usize {
    text.split_whitespace()
        .filter(|t| is_numeric_token(t))
        .count()
}

/// Numbers with at most one word, e.g. `0.0`, `Epoch 50`, `10⁻²`.
fn is_axis_like(text: &str) -> bool {
    let total = word_count(text);
    let numeric = numeric_count(text);
    numeric >= 1 && total - numeric <= 1
}

/// At least 60 % of the words are numbers, e.g.
/// `0.2 0.4 0.6 0.8 1.0 Epoch 10 20 30`.
fn is_mostly_numeric(text: &str) -> bool {
    let total = word_count(text);
    total > 0 && numeric_count(text) * 10 >= total * 6
}

/// A figure fragment: at most 4 words, axis-like, or mostly numbers
/// whatever its length.
fn is_fragment(text: &str) -> bool {
    word_count(text) <= FRAGMENT_WORDS || is_axis_like(text) || is_mostly_numeric(text)
}

/// At least 6 words, at least half starting lowercase, under 30 % numeric.
fn is_prose(text: &str) -> bool {
    let total = word_count(text);
    if total < PROSE_WORDS {
        return false;
    }
    let lower = text
        .split_whitespace()
        .filter(|t| {
            t.chars()
                .find(|c| c.is_alphanumeric())
                .is_some_and(char::is_lowercase)
        })
        .count();
    lower * 2 >= total && numeric_count(text) * 10 < total * 3
}

/// A token with at least two uppercase letters: `LLM`, `GPT-4o`, `DeepSeek`.
fn is_caps_token(token: &str) -> bool {
    token.chars().filter(|c| c.is_uppercase()).count() >= 2
}

/// The hard prose guard: at least 7 words, at most 30 % numeric tokens, at
/// most 2 all-caps or abbreviation tokens and at least 2 words starting
/// lowercase (so a title-case table header row is not prose-like).
fn is_prose_like(text: &str) -> bool {
    let total = word_count(text);
    if total < PROSE_LIKE_WORDS {
        return false;
    }
    let mut caps: usize = 0;
    let mut lower: usize = 0;
    for token in text.split_whitespace() {
        if is_caps_token(token) {
            caps += 1;
        }
        let starts_lower = token
            .chars()
            .find(|c| c.is_alphanumeric())
            .is_some_and(char::is_lowercase);
        if starts_lower {
            lower += 1;
        }
    }
    numeric_count(text) * 10 <= total * 3 && caps <= PROSE_LIKE_CAPS && lower >= PROSE_LIKE_LOWER
}

/// A prose-like line of at least 10 words whose first word starts
/// uppercase: body text opening a sentence, never a caption continuation.
fn opens_sentence(text: &str) -> bool {
    let upper = text
        .trim_start()
        .chars()
        .next()
        .is_some_and(char::is_uppercase);
    upper && word_count(text) >= SENTENCE_WORDS && is_prose_like(text)
}

/// Ends with `.`, `?` or `!`, ignoring closing quotes and brackets.
fn ends_sentence(text: &str) -> bool {
    let trimmed = text
        .trim_end()
        .trim_end_matches([')', '"', '\'', '\u{201d}', '\u{2019}']);
    trimmed.ends_with(['.', '?', '!'])
}

/// A pseudo-code line (see [`ALGORITHM_MARKERS`] and [`ALGORITHM_STARTS`]).
fn is_algorithm_line(text: &str) -> bool {
    let lower = text.trim().to_lowercase();
    if ALGORITHM_MARKERS.iter().any(|m| lower.contains(m)) {
        return true;
    }
    let mut words = lower.split_whitespace();
    let Some(first) = words.next() else {
        return false;
    };
    if let Some(step) = first.strip_suffix(':')
        && !step.is_empty()
        && step.chars().all(|c| c.is_ascii_digit())
    {
        return true;
    }
    ALGORITHM_STARTS.contains(&first)
}

fn non_space_chars(text: &str) -> usize {
    text.chars().filter(|c| !c.is_whitespace()).count()
}

/// A line of at least 2 characters whose box is more than
/// [`VERTICAL_RATIO`] times taller than wide: rotated text.
fn is_tall(line: &Line) -> bool {
    if non_space_chars(&line.text) < 2 {
        return false;
    }
    finite_box(line).is_some_and(|b| {
        let w = b.x1 - b.x0;
        w > 0.0 && b.y1 - b.y0 > VERTICAL_RATIO * w
    })
}

/// A line holding a single character.
fn is_single_glyph(line: &Line) -> bool {
    line.text.trim().chars().count() == 1
}

/// The single-character line at `i` has another single-character line
/// directly above or below it (x ranges overlapping, gap at most one line
/// height): letters stacked vertically.
fn is_stacked(page: &PageText, entries: &[usize], i: usize) -> bool {
    let line = &page.lines[i];
    if !is_single_glyph(line) {
        return false;
    }
    let Some(b) = finite_box(line) else {
        return false;
    };
    let height = b.y1 - b.y0;
    entries.iter().any(|&j| {
        if j == i || !is_single_glyph(&page.lines[j]) {
            return false;
        }
        finite_box(&page.lines[j]).is_some_and(|o| {
            let overlap = o.x0 < b.x1 && b.x0 < o.x1;
            let apart = (o.y0 - b.y1).max(b.y0 - o.y1);
            overlap && apart <= height
        })
    })
}

/// The line at `i` is set vertically: a tall box or stacked letters.
fn is_vertical(page: &PageText, entries: &[usize], i: usize) -> bool {
    is_tall(&page.lines[i]) || is_stacked(page, entries, i)
}

/// Vertical `body` lines below the table caption at `entries[pos]`, up to
/// the next caption start or the next prose-like line that is not
/// vertical. Prose-like lines are never taken.
fn vertical_cells(page: &PageText, entries: &[usize], pos: usize) -> Vec<usize> {
    let mut cells: Vec<usize> = Vec::new();
    for &i in entries.iter().skip(pos + 1) {
        let line = &page.lines[i];
        if line.role == ROLE_FURNITURE {
            continue;
        }
        if caption_kind(line).is_some() {
            break;
        }
        if line.role != ROLE_BODY {
            continue;
        }
        let prose_like = is_prose_like(&line.text);
        if is_vertical(page, entries, i) {
            if !prose_like {
                cells.push(i);
            }
            continue;
        }
        if prose_like {
            break;
        }
    }
    cells
}

/// At least [`SIDEWAYS_MIN_LINES`] boxed lines of 2 or more characters,
/// at least 60 % of them tall: a landscape float page.
fn is_sideways(page: &PageText) -> bool {
    let mut total: usize = 0;
    let mut tall: usize = 0;
    for line in &page.lines {
        if line.role == ROLE_FURNITURE
            || finite_box(line).is_none()
            || non_space_chars(&line.text) < 2
        {
            continue;
        }
        total += 1;
        if is_tall(line) {
            tall += 1;
        }
    }
    total >= SIDEWAYS_MIN_LINES && tall * 10 >= total * 6
}

/// The box `b` after a quarter turn that makes vertical text read left to
/// right: clockwise (`(x, y)` to `(y, -x)`) for text running upwards,
/// counter-clockwise (`(x, y)` to `(-y, x)`) for text running downwards.
fn turn(b: BBox, clockwise: bool) -> BBox {
    if clockwise {
        BBox {
            x0: b.y0,
            y0: -b.x1,
            x1: b.y1,
            y1: -b.x0,
        }
    } else {
        BBox {
            x0: -b.y1,
            y0: b.x0,
            x1: -b.y0,
            y1: b.x1,
        }
    }
}

/// A copy of the page's lines (same indices, no spans) with every box
/// turned: counter-clockwise under `/Rotate 270`, else clockwise (`/Rotate
/// 90` and `sidewaystable` set text running upwards).
fn turned_view(page: &PageText) -> PageText {
    let clockwise = page.rotation.rem_euclid(360) != 270;
    let mut view = PageText::new(page.page, page.height, page.width, 0);
    view.lines = page
        .lines
        .iter()
        .map(|line| Line {
            text: line.text.clone(),
            bbox: finite_box(line).map(|b| turn(b, clockwise)),
            column: line.column,
            spans: Vec::new(),
            role: line.role.clone(),
        })
        .collect();
    view
}

/// A `Table N` label anywhere in a line's start: `Table 3:`, `TABLE IV`,
/// `Table A.6 continued from previous page`.
fn is_table_label(text: &str) -> bool {
    let mut words = text.split_whitespace();
    let Some(first) = words.next() else {
        return false;
    };
    if !first.eq_ignore_ascii_case("table") {
        return false;
    }
    let Some(label) = words.next() else {
        return false;
    };
    let core = label.trim_end_matches([':', '.', '|']);
    let has_digit = core.chars().any(|c| c.is_ascii_digit());
    let numbered = has_digit
        && core
            .chars()
            .all(|c| c.is_ascii_digit() || c == '.' || c.is_ascii_uppercase());
    let roman = !core.is_empty() && core.chars().all(|c| matches!(c, 'I' | 'V' | 'X' | 'L'));
    numbered || roman
}

/// Tag a sideways (landscape) table page in the turned frame (see the
/// module documentation). Line indices of the turned view are those of
/// `page`.
fn tag_sideways(page: &mut PageText) -> RegionReport {
    let mut report = RegionReport::default();
    let view = turned_view(page);
    let blank = measure(&view).blank;
    let band = Band {
        lo: f32::NEG_INFINITY,
        hi: f32::INFINITY,
        width: view.width,
        cross: None,
    };
    let entries = band_entries(&view, band);
    let wide = SIDEWAYS_PARAGRAPH_WIDTH * view.width;
    let is_wide = |i: usize| finite_box(&view.lines[i]).is_some_and(|b| b.x1 - b.x0 >= wide);
    let mut captions: Vec<usize> = Vec::new();
    let mut cells: Vec<usize> = Vec::new();
    for (pos, &k) in entries.iter().enumerate() {
        let label = &view.lines[k];
        let labelled = label.role == ROLE_BODY || label.role == ROLE_CAPTION;
        if !labelled || !is_table_label(&label.text) {
            continue;
        }
        let mut more: Vec<usize> = Vec::new();
        let mut prev = k;
        let mut next = pos + 1;
        while let Some(&i) = entries.get(next) {
            if more.len() + 1 >= CAPTION_MAX_LINES {
                break;
            }
            let line = &view.lines[i];
            let continues = line.role == ROLE_BODY
                && gap(&view, prev, i) <= blank
                && is_wide(i)
                && is_prose(&line.text);
            if !continues {
                break;
            }
            more.push(i);
            prev = i;
            next += 1;
        }
        let mut region: Vec<usize> = Vec::new();
        for &i in entries.iter().skip(next) {
            let line = &view.lines[i];
            if line.role == ROLE_FURNITURE {
                continue;
            }
            if is_table_label(&line.text) {
                break;
            }
            if line.role != ROLE_BODY {
                continue;
            }
            if is_prose_like(&line.text) {
                if is_wide(i) {
                    break;
                }
                continue;
            }
            region.push(i);
        }
        if region.is_empty() {
            continue;
        }
        captions.push(k);
        captions.extend(more);
        cells.extend(region);
    }
    for i in captions {
        report.caption += tag(&mut page.lines[i], ROLE_CAPTION);
    }
    for i in cells {
        report.table += tag(&mut page.lines[i], ROLE_TABLE);
    }
    report
}

/// A numbered section heading: `3 Method`, `3.1 Problem Setup`, `A.2
/// Proofs`, `IV. Results` (a section number, then a capitalised word).
fn is_numbered_heading(text: &str) -> bool {
    let mut words = text.split_whitespace();
    let (Some(number), Some(next)) = (words.next(), words.next()) else {
        return false;
    };
    if !next.chars().next().is_some_and(char::is_uppercase) {
        return false;
    }
    let core = number.trim_end_matches('.');
    let roman = !core.is_empty()
        && core.len() < number.len()
        && core.chars().all(|c| matches!(c, 'I' | 'V' | 'X' | 'L'));
    if roman {
        return true;
    }
    let parts: Vec<&str> = core.split('.').collect();
    parts.iter().enumerate().all(|(k, part)| {
        let letter = k == 0
            && parts.len() > 1
            && part.len() == 1
            && part.chars().all(|c| c.is_ascii_uppercase());
        let digits = !part.is_empty()
            && part.len() <= 2
            && !part.starts_with('0')
            && part.chars().all(|c| c.is_ascii_digit());
        letter || digits
    })
}

/// The page's spans run vertically: at least [`SIDEWAYS_MIN_SPANS`] spans
/// of [`SPAN_SHAPE_CHARS`] or more characters have a box taller than wide,
/// and they are at least 60 % of the spans whose box shape tells a
/// direction. Catches rotated pages whose lines, grouped in unrotated
/// coordinates, are slices across the rotated text.
fn sideways_by_spans(page: &PageText) -> bool {
    let mut tall: usize = 0;
    let mut wide: usize = 0;
    for span in &page.spans {
        if non_space_chars(&span.text) < SPAN_SHAPE_CHARS {
            continue;
        }
        let Some(b) = span.bbox else {
            continue;
        };
        let w = (b.x1 - b.x0).abs();
        let h = (b.y1 - b.y0).abs();
        if !(w.is_finite() && h.is_finite()) {
            continue;
        }
        if h > SPAN_ASPECT * w {
            tall += 1;
        } else if w > SPAN_ASPECT * h {
            wide += 1;
        }
    }
    tall >= SIDEWAYS_MIN_SPANS && tall * 10 >= (tall + wide) * 6
}

/// `rest` (what follows `Table` in the span text) opens with a table
/// number: `3`, `S1`, `A.6`, or a roman numeral and `:` or `.`.
fn label_follows(rest: &str) -> bool {
    let mut chars = rest.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if first.is_ascii_digit() {
        return true;
    }
    let second = chars.next();
    if first.is_ascii_uppercase() {
        if second.is_some_and(|c| c.is_ascii_digit()) {
            return true;
        }
        if second == Some('.') && chars.next().is_some_and(|c| c.is_ascii_digit()) {
            return true;
        }
    }
    let roman = rest
        .chars()
        .take_while(|c| matches!(c, 'I' | 'V' | 'X' | 'L'))
        .count();
    roman > 0 && rest[roman..].starts_with([':', '.'])
}

/// The page's span texts, in content-stream order and with whitespace
/// removed, hold a `Table` label (see [`label_follows`]), however the
/// label's pieces were grouped into lines.
fn spans_carry_table_label(page: &PageText) -> bool {
    let joined: String = page
        .spans
        .iter()
        .flat_map(|span| span.text.chars())
        .filter(|c| !c.is_whitespace())
        .collect();
    ["Table", "TABLE"].iter().any(|word| {
        joined
            .match_indices(word)
            .any(|(at, _)| label_follows(&joined[at + word.len()..]))
    })
}

/// Tag every `body` line of a sideways table page whose lines are slices
/// across the rotated text `table`, except prose-like lines; the count of
/// lines newly tagged.
fn tag_shredded_table(page: &mut PageText) -> usize {
    let mut n = 0;
    for line in &mut page.lines {
        if line.role == ROLE_BODY && !is_prose_like(&line.text) {
            n += tag(line, ROLE_TABLE);
        }
    }
    n
}

/// Largest font size among the line's non-blank spans.
fn line_size(page: &PageText, line: &Line) -> Option<f32> {
    let mut best: Option<f32> = None;
    for &idx in &line.spans {
        let Some(span) = page.spans.get(idx as usize) else {
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

/// Median font size of the page's non-furniture lines with at least 6
/// words, when at least [`BODY_SIZE_MIN_LINES`] of them carry a size.
fn body_size(page: &PageText) -> Option<f32> {
    let mut sizes: Vec<f32> = page
        .lines
        .iter()
        .filter(|line| line.role != ROLE_FURNITURE && word_count(&line.text) >= PROSE_WORDS)
        .filter_map(|line| line_size(page, line))
        .collect();
    if sizes.len() < BODY_SIZE_MIN_LINES {
        return None;
    }
    median(&mut sizes)
}

/// A superscript letter such as the `ᵃ` of an affiliation mark.
fn is_superscript_letter(c: char) -> bool {
    matches!(
        c,
        '\u{02b0}'..='\u{02b8}'
            | '\u{1d2c}'..='\u{1d61}'
            | '\u{1d9c}'..='\u{1dbf}'
            | '\u{2071}'
            | '\u{207f}'
    )
}

/// The line opens with a footnote marker: `*`, `†`, `‡`, `§`, `¶`, a
/// superscript digit or letter, or 1 or 2 digits, a space and a letter
/// (`1 https://...`, `2 Work done at ...`). A mostly numeric line never
/// does.
fn starts_footnote(text: &str) -> bool {
    let text = text.trim_start();
    let Some(first) = text.chars().next() else {
        return false;
    };
    if is_mostly_numeric(text) {
        return false;
    }
    if matches!(
        first,
        '*' | '\u{2217}' | '\u{2020}' | '\u{2021}' | '\u{00a7}' | '\u{00b6}'
    ) || is_superscript_digit(first)
        || is_superscript_letter(first)
    {
        return true;
    }
    let digits = text.chars().take_while(char::is_ascii_digit).count();
    if !(1..=2).contains(&digits) {
        return false;
    }
    let rest = &text[digits..];
    rest.starts_with(' ')
        && rest
            .trim_start()
            .chars()
            .next()
            .is_some_and(char::is_alphabetic)
}

/// The column bands of a page: its two halves on a two-column page, else
/// the whole page.
fn page_bands(geometry: &PageGeometry) -> Vec<Band> {
    let whole = Band {
        lo: f32::NEG_INFINITY,
        hi: f32::INFINITY,
        width: geometry.width,
        cross: None,
    };
    if !geometry.two_column {
        return vec![whole];
    }
    let half = 0.5 * geometry.width;
    vec![
        Band {
            hi: geometry.mid,
            width: half,
            ..whole
        },
        Band {
            lo: geometry.mid,
            width: half,
            ..whole
        },
    ]
}

/// Tag page-foot notes `footnote` (see the module documentation); the
/// count of lines newly tagged.
fn tag_footnotes(page: &mut PageText, geometry: &PageGeometry) -> usize {
    if !page.height.is_finite() || page.height <= 0.0 {
        return 0;
    }
    let Some(body) = body_size(page) else {
        return 0;
    };
    let limit = FOOTNOTE_SIZE_RATIO * body;
    let zone = FOOTNOTE_ZONE * page.height;
    let mut picked: Vec<usize> = Vec::new();
    for band in page_bands(geometry) {
        let mut run: Vec<usize> = Vec::new();
        let mut body_above = false;
        for &i in band_entries(page, band).iter().rev() {
            let line = &page.lines[i];
            if line.role == ROLE_FURNITURE {
                continue;
            }
            let size = line_size(page, line);
            let small = size.is_some_and(|s| s <= limit);
            let low = finite_box(line).is_some_and(|b| f32::midpoint(b.y0, b.y1) <= zone);
            if small && low && line.role == ROLE_BODY {
                run.push(i);
                continue;
            }
            body_above = size.is_some_and(|s| s > limit);
            break;
        }
        if run.is_empty() || run.len() > FOOTNOTE_MAX_LINES || !body_above {
            continue;
        }
        run.reverse();
        if let Some(start) = run
            .iter()
            .position(|&i| starts_footnote(&page.lines[i].text))
        {
            picked.extend_from_slice(&run[start..]);
        }
    }
    let mut n = 0;
    for i in picked {
        n += tag(&mut page.lines[i], ROLE_FOOTNOTE);
    }
    n
}

fn is_greek(c: char) -> bool {
    matches!(c, '\u{0370}'..='\u{03ff}' | '\u{00b5}')
}

/// A letter from the Mathematical Alphanumeric Symbols block (`𝑥`, `𝐀`).
fn is_math_alphanumeric(c: char) -> bool {
    matches!(c, '\u{1d400}'..='\u{1d7ff}')
}

/// A character that marks math: Greek, math alphanumerics, `=`, `+`, `¬`,
/// `×`, `‖`, `⟨`, `⟩`, arrows, and the Mathematical Operators blocks
/// (`∈ ∑ ∏ ∫ ≤ ≥ ≠ ≈ ∀ ∃ ∇ ∂ ⊆ ⊂ ∪ ∩ −` and more).
fn is_math_char(c: char) -> bool {
    is_greek(c)
        || is_math_alphanumeric(c)
        || matches!(
            c,
            '=' | '+'
                | '\u{00ac}'
                | '\u{00d7}'
                | '\u{2016}'
                | '\u{2190}'..='\u{21ff}'
                | '\u{2200}'..='\u{22ff}'
                | '\u{27e8}'
                | '\u{27e9}'
                | '\u{2a00}'..='\u{2aff}'
        )
}

/// Script of a letter for the math test: `Some(true)` Greek, `Some(false)`
/// another script's letter, `None` anything else (math alphanumerics
/// included).
fn letter_script(c: char) -> Option<bool> {
    if is_greek(c) && c.is_alphabetic() {
        Some(true)
    } else if c.is_alphabetic() && !is_math_alphanumeric(c) {
        Some(false)
    } else {
        None
    }
}

/// Ordinary words in a token and their letters: runs of at least
/// [`MATH_WORD_LETTERS`] letters of one script that are not a math
/// function name (`log`, `max`, ...).
fn ordinary_words(token: &str) -> (usize, usize) {
    let mut words: usize = 0;
    let mut letters: usize = 0;
    let mut run = String::new();
    let mut script: Option<bool> = None;
    for c in token.chars().chain(std::iter::once(' ')) {
        let next = letter_script(c);
        if next.is_some() && next == script {
            run.push(c);
            continue;
        }
        let n = run.chars().count();
        if n >= MATH_WORD_LETTERS && !MATH_FUNCTIONS.contains(&run.to_lowercase().as_str()) {
            words += 1;
            letters += n;
        }
        run.clear();
        if next.is_some() {
            run.push(c);
        }
        script = next;
    }
    (words, letters)
}

/// A display-math fragment: at most [`MATH_MAX_WORDS`] ordinary words, at
/// least one math character, ordinary-word letters at most half of the
/// other non-blank characters, and not mostly numbers (a table row or an
/// axis).
fn is_math_line(text: &str) -> bool {
    if !text.chars().any(is_math_char) || is_mostly_numeric(text) {
        return false;
    }
    let mut words: usize = 0;
    let mut letters: usize = 0;
    for token in text.split_whitespace() {
        let (w, l) = ordinary_words(token);
        words += w;
        letters += l;
    }
    if words > MATH_MAX_WORDS {
        return false;
    }
    let chars = non_space_chars(text);
    letters * 2 <= chars.saturating_sub(letters)
}

/// Tag the `body` lines that are display-math fragments `math`; the count
/// of lines newly tagged.
fn tag_math(page: &mut PageText) -> usize {
    let mut n = 0;
    for line in &mut page.lines {
        if line.role == ROLE_BODY && is_math_line(&line.text) {
            n += tag(line, ROLE_MATH);
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE: f32 = 10.0;

    /// A 10 pt line with its baseline at `baseline`: the box runs from
    /// 0.2 size below to 0.8 size above, 5 pt per character wide.
    fn line(text: &str, x0: f32, baseline: f32, column: u32) -> Line {
        let width = 0.5 * SIZE * text.chars().count() as f32;
        Line {
            text: text.to_string(),
            bbox: Some(BBox {
                x0,
                y0: baseline - 0.2 * SIZE,
                x1: x0 + width,
                y1: baseline + 0.8 * SIZE,
            }),
            column,
            ..Line::default()
        }
    }

    fn captioned(text: &str, x0: f32, baseline: f32, column: u32) -> Line {
        let mut l = line(text, x0, baseline, column);
        l.role = ROLE_CAPTION.to_string();
        l
    }

    fn page_of(lines: Vec<Line>) -> PageText {
        let mut page = PageText::new(1, 612.0, 792.0, 0);
        page.lines = lines;
        page.text = page
            .lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<&str>>()
            .join("\n");
        page
    }

    fn role_of<'a>(page: &'a PageText, text: &str) -> &'a str {
        page.lines
            .iter()
            .find(|l| l.text == text)
            .map_or("missing", |l| l.role.as_str())
    }

    const LEFT_PROSE: [&str; 6] = [
        "we train the policy with a drifting objective that",
        "keeps the one step generator close to the data",
        "while the adapter adds an exact likelihood for the",
        "online updates and keeps strict single pass execution",
        "at deployment time so that the latency stays low",
        "and the whole pipeline is summarised in the figure.",
    ];

    const AFTER_PROSE: [&str; 3] = [
        "this section introduces the two stage framework that",
        "preserves single pass deployment while enabling the",
        "online policy improvement described in the next part.",
    ];

    /// Two-column page: 6 prose lines, diagram labels at scattered x (each
    /// its own layout block), a two-line `Figure 2` caption, then prose;
    /// the right column is prose throughout.
    fn figure_page() -> PageText {
        let mut lines: Vec<Line> = Vec::new();
        let mut baseline = 700.0;
        for text in LEFT_PROSE {
            lines.push(line(text, 54.0, baseline, 0));
            baseline -= 12.0;
        }
        lines.push(line("Observation sequence", 70.0, 610.0, 1));
        lines.push(line("Robot State", 180.0, 598.0, 2));
        lines.push(line("Noise Prediction", 90.0, 580.0, 3));
        lines.push(line("auxiliary patches", 200.0, 562.0, 4));
        lines.push(line("Epoch 50", 60.0, 545.0, 5));
        lines.push(captioned("Figure 2: Overview of the", 54.0, 520.0, 6));
        lines.push(line("two stage training pipeline.", 54.0, 508.0, 6));
        let mut baseline = 485.0;
        for text in AFTER_PROSE {
            lines.push(line(text, 54.0, baseline, 6));
            baseline -= 12.0;
        }
        let mut baseline = 700.0;
        for k in 0..12 {
            let text = format!("the right column carries ordinary running prose {k}");
            lines.push(line(&text, 312.0, baseline, 7));
            baseline -= 12.0;
        }
        page_of(lines)
    }

    #[test]
    fn diagram_labels_above_a_figure_caption_are_figure_text() {
        let mut pages = vec![figure_page()];
        let report = tag_regions(&mut pages);
        let page = &pages[0];
        for text in [
            "Observation sequence",
            "Robot State",
            "Noise Prediction",
            "auxiliary patches",
            "Epoch 50",
        ] {
            assert_eq!(role_of(page, text), "figure", "{text}");
        }
        for text in LEFT_PROSE.iter().chain(AFTER_PROSE.iter()) {
            assert_eq!(role_of(page, text), "body", "{text}");
        }
        assert_eq!(role_of(page, "Figure 2: Overview of the"), "caption");
        assert_eq!(role_of(page, "two stage training pipeline."), "caption");
        assert!(
            page.lines
                .iter()
                .filter(|l| l.column == 7)
                .all(|l| l.role == "body")
        );
        assert_eq!(report.figure, 5);
        assert_eq!(report.caption, 1);
        assert_eq!(report.table, 0);
        assert!(
            page.warnings
                .contains(&"regions: figure text 5 lines".to_string())
        );
        assert!(
            page.warnings
                .contains(&"regions: caption text 1 lines".to_string())
        );
    }

    #[test]
    fn tagging_is_idempotent() {
        let mut pages = vec![figure_page()];
        tag_regions(&mut pages);
        let first = pages.clone();
        let again = tag_regions(&mut pages);
        assert_eq!(again, RegionReport::default());
        assert_eq!(pages, first);
    }

    #[test]
    fn other_roles_are_never_retagged_and_stop_the_walk() {
        let mut page = figure_page();
        for l in &mut page.lines {
            if l.text == "Noise Prediction" {
                l.role = "heading".to_string();
            }
        }
        let mut pages = vec![page];
        let report = tag_regions(&mut pages);
        let page = &pages[0];
        assert_eq!(role_of(page, "Noise Prediction"), "heading");
        assert_eq!(role_of(page, "Observation sequence"), "body");
        assert_eq!(role_of(page, "Robot State"), "body");
        assert_eq!(role_of(page, "auxiliary patches"), "figure");
        assert_eq!(role_of(page, "Epoch 50"), "figure");
        assert_eq!(report.figure, 2);
    }

    #[test]
    fn a_raster_figure_with_prose_around_it_tags_nothing() {
        let mut lines: Vec<Line> = Vec::new();
        let mut baseline = 700.0;
        for text in LEFT_PROSE {
            lines.push(line(text, 72.0, baseline, 0));
            baseline -= 12.0;
        }
        lines.push(captioned("Figure 1: A photograph.", 72.0, 450.0, 1));
        let mut baseline = 420.0;
        for text in AFTER_PROSE {
            lines.push(line(text, 72.0, baseline, 2));
            baseline -= 12.0;
        }
        let mut pages = vec![page_of(lines)];
        let report = tag_regions(&mut pages);
        assert_eq!(report, RegionReport::default());
        assert!(pages[0].warnings.is_empty());
        assert!(
            pages[0]
                .lines
                .iter()
                .all(|l| l.role == "body" || l.role == "caption")
        );
    }

    /// Two-column body text in the style of the LLM papers that were
    /// over-tagged: author-year citations, names and scores, so no two
    /// neighbouring lines pass the lowercase-majority `is_prose` test and
    /// half carry two numeric tokens (cell-like), yet most are prose-like.
    /// Before the hard guards every walk below ran through it.
    const COLUMN_PROSE: [&str; 12] = [
        "Recent work by Aher, Arriaga and Kalai (2023) shows",
        "Horton (2023), Argyle, Busby and Fulda (2023) and",
        "Park, O'Brien and Cai (2024) find that LLM Agents",
        "Match Humans in Economics (Horton, 2023; Manning,",
        "2024), Political Science (Argyle et al., 2023) and",
        "Marketing (Brand, Israeli and Ngwe, 2023), while",
        "Dillion et al. (2023) and Hewitt et al. (2024) see",
        "Mixed Agreement on Moral Norms (Tjuatja, 2024). In",
        "Table 2, GPT-4o reaches 0.61 and Claude Haiku 0.58,",
        "Gemini Flash 0.52 and Mistral Nemo only 0.41, with",
        "Human Baselines of 1.00 on 9 of 12 Studies (Hu et",
        "al., 2025; Wang, 2025) and Figure 4 shows the Gaps.",
    ];

    /// `n` lines of [`COLUMN_PROSE`] (cycled) at `x0`, 12 pt leading from
    /// `top` down, as layout block `column`.
    fn prose_column(lines: &mut Vec<Line>, x0: f32, top: f32, n: usize, column: u32) {
        let mut baseline = top;
        for k in 0..n {
            let text = COLUMN_PROSE[k % COLUMN_PROSE.len()];
            lines.push(line(text, x0, baseline, column));
            baseline -= 12.0;
        }
    }

    /// Every line but the caption start keeps the role `body`.
    fn only_caption_tagged(page: &PageText, caption: &str) {
        for l in &page.lines {
            if l.text != caption {
                assert_eq!(l.role, "body", "{}", l.text);
            }
        }
    }

    #[test]
    fn column_prose_is_prose_like() {
        for text in COLUMN_PROSE {
            let chars = text.chars().count();
            assert!(chars <= 52, "{text} must fit a column");
        }
        let like = COLUMN_PROSE.iter().filter(|t| is_prose_like(t)).count();
        assert!(like >= 9, "{like}");
    }

    #[test]
    fn a_full_width_figure_caption_above_two_prose_columns_tags_nothing() {
        let caption =
            "Figure 3: Agreement between simulated and human participants across twelve studies";
        let mut lines: Vec<Line> = vec![captioned(caption, 54.0, 700.0, 0)];
        prose_column(&mut lines, 54.0, 686.0, 40, 1);
        prose_column(&mut lines, 312.0, 686.0, 40, 2);
        let mut pages = vec![page_of(lines)];
        let report = tag_regions(&mut pages);
        assert_eq!(report, RegionReport::default());
        assert!(pages[0].warnings.is_empty());
        only_caption_tagged(&pages[0], caption);
    }

    #[test]
    fn a_table_caption_followed_by_prose_tags_nothing() {
        let caption = "Table 2: Effect sizes by study.";
        let mut lines: Vec<Line> = Vec::new();
        prose_column(&mut lines, 54.0, 760.0, 5, 0);
        lines.push(captioned(caption, 54.0, 690.0, 1));
        prose_column(&mut lines, 54.0, 676.0, 30, 2);
        prose_column(&mut lines, 312.0, 760.0, 50, 3);
        let mut pages = vec![page_of(lines)];
        let report = tag_regions(&mut pages);
        assert_eq!(report, RegionReport::default());
        only_caption_tagged(&pages[0], caption);
    }

    #[test]
    fn a_caption_at_the_top_of_a_column_above_prose_tags_nothing() {
        let caption = "Figure 5: Calibration of the agents.";
        let math = "L(\u{3b8}) = \u{3a3} w\u{2083} \u{2113}";
        let mut lines: Vec<Line> = Vec::new();
        prose_column(&mut lines, 54.0, 760.0, 50, 0);
        lines.push(captioned(caption, 312.0, 760.0, 1));
        let mut baseline = 746.0;
        for k in 0..30 {
            if k % 5 == 3 {
                lines.push(line(math, 420.0, baseline, 2));
                lines.push(line("(3)", 560.0, baseline, 3));
            } else {
                let text = COLUMN_PROSE[k % COLUMN_PROSE.len()];
                lines.push(line(text, 312.0, baseline, 4));
            }
            baseline -= 12.0;
        }
        let mut pages = vec![page_of(lines)];
        let report = tag_regions(&mut pages);
        // The six display equations are math fragments (loop 10); no
        // figure, table, algorithm or caption line is tagged.
        let expected = RegionReport {
            math: 6,
            ..RegionReport::default()
        };
        assert_eq!(report, expected);
        for l in &pages[0].lines {
            if l.text == math {
                assert_eq!(l.role, "math");
            } else if l.text != caption {
                assert_eq!(l.role, "body", "{}", l.text);
            }
        }
    }

    #[test]
    fn a_region_longer_than_the_cap_is_dropped() {
        let mut lines: Vec<Line> = Vec::new();
        let mut baseline = 760.0;
        for k in 0..45 {
            lines.push(line(&format!("node {k}"), 72.0, baseline, 0));
            baseline -= 12.0;
        }
        lines.push(captioned("Figure 1: Graph.", 72.0, baseline - 4.0, 1));
        let mut pages = vec![page_of(lines)];
        let report = tag_regions(&mut pages);
        assert_eq!(report, RegionReport::default());
        assert!(pages[0].lines.iter().all(|l| l.role != "figure"));
    }

    const CELLS: [&str; 8] = [
        "Method PAS ECS",
        "GPT-4o 0.61 0.42",
        "Claude Haiku 4.5 0.58 0.40",
        "DeepSeek V3.2 0.55 0.37",
        "Gemini 3 Flash 0.52 0.35",
        "Mistral Nemo 0.41 0.22",
        "Gemma 4 26b 0.44 0.30",
        "Human baseline 1.00 1.00",
    ];

    /// Single-column page: prose, a `Table 1` caption (plus an untagged
    /// continuation line when `continued`), 8 cell lines, prose.
    fn table_page(continued: bool) -> PageText {
        let mut lines: Vec<Line> = Vec::new();
        let mut baseline = 720.0;
        for text in LEFT_PROSE {
            lines.push(line(text, 72.0, baseline, 0));
            baseline -= 12.0;
        }
        baseline -= 20.0;
        lines.push(captioned("Table 1: Main Leaderboard.", 72.0, baseline, 1));
        if continued {
            baseline -= 12.0;
            let more = "best performing models are highlighted in teal and worst in salmon.";
            lines.push(line(more, 72.0, baseline, 1));
        }
        baseline -= 18.0;
        for text in CELLS {
            lines.push(line(text, 90.0, baseline, 2));
            baseline -= 12.0;
        }
        baseline -= 20.0;
        for text in AFTER_PROSE {
            lines.push(line(text, 72.0, baseline, 3));
            baseline -= 12.0;
        }
        page_of(lines)
    }

    #[test]
    fn cells_below_a_table_caption_are_table_text() {
        let mut pages = vec![table_page(false)];
        let report = tag_regions(&mut pages);
        let page = &pages[0];
        for text in CELLS {
            assert_eq!(role_of(page, text), "table", "{text}");
        }
        for text in LEFT_PROSE.iter().chain(AFTER_PROSE.iter()) {
            assert_eq!(role_of(page, text), "body", "{text}");
        }
        assert_eq!(report.table, 8);
        assert!(
            page.warnings
                .contains(&"regions: table text 8 lines".to_string())
        );
    }

    #[test]
    fn an_untagged_caption_continuation_is_skipped_and_tagged_caption() {
        let mut pages = vec![table_page(true)];
        let report = tag_regions(&mut pages);
        let page = &pages[0];
        let more = "best performing models are highlighted in teal and worst in salmon.";
        assert_eq!(role_of(page, more), "caption");
        for text in CELLS {
            assert_eq!(role_of(page, text), "table", "{text}");
        }
        assert_eq!(report.caption, 1);
        assert_eq!(report.table, 8);
    }

    #[test]
    fn elsevier_table_with_the_caption_below_uses_the_lines_above() {
        let mut lines: Vec<Line> = Vec::new();
        let mut baseline = 720.0;
        for text in LEFT_PROSE {
            lines.push(line(text, 72.0, baseline, 0));
            baseline -= 12.0;
        }
        baseline -= 20.0;
        for text in CELLS {
            lines.push(line(text, 90.0, baseline, 1));
            baseline -= 12.0;
        }
        baseline -= 4.0;
        lines.push(captioned("Table 2: Results.", 72.0, baseline, 2));
        baseline -= 24.0;
        for text in AFTER_PROSE {
            lines.push(line(text, 72.0, baseline, 3));
            baseline -= 12.0;
        }
        let mut pages = vec![page_of(lines)];
        let report = tag_regions(&mut pages);
        for text in CELLS {
            assert_eq!(role_of(&pages[0], text), "table", "{text}");
        }
        assert_eq!(report.table, 8);
    }

    const ALGORITHM: [&str; 6] = [
        "Input: labeled data {(Xi, Yi)}, unlabeled data {Xu}, predictor f",
        "Output: confidence interval C(x0)",
        "for i = 1 to n do",
        "compute the residual ri \u{2190} Yi \u{2212} f(Xi)",
        "end for",
        "return C(x0)",
    ];

    #[test]
    fn an_algorithm_block_is_algorithm_text() {
        let mut lines: Vec<Line> = Vec::new();
        let mut baseline = 720.0;
        for text in LEFT_PROSE {
            lines.push(line(text, 72.0, baseline, 0));
            baseline -= 12.0;
        }
        baseline -= 20.0;
        let title = "Algorithm 1 Prediction-Powered Conditional Inference";
        lines.push(line(title, 72.0, baseline, 1));
        for text in ALGORITHM {
            baseline -= 12.0;
            lines.push(line(text, 80.0, baseline, 1));
        }
        baseline -= 24.0;
        for text in AFTER_PROSE {
            lines.push(line(text, 72.0, baseline, 2));
            baseline -= 12.0;
        }
        let mut pages = vec![page_of(lines)];
        let report = tag_regions(&mut pages);
        let page = &pages[0];
        for text in ALGORITHM {
            assert_eq!(role_of(page, text), "algorithm", "{text}");
        }
        assert_eq!(role_of(page, title), "caption");
        assert_eq!(report.caption, 1);
        for text in LEFT_PROSE.iter().chain(AFTER_PROSE.iter()) {
            assert_eq!(role_of(page, text), "body", "{text}");
        }
        assert_eq!(report.algorithm, 6);
        assert!(
            page.warnings
                .contains(&"regions: algorithm text 6 lines".to_string())
        );
    }

    #[test]
    fn a_caption_start_inside_a_paragraph_is_ignored() {
        let mut lines: Vec<Line> = Vec::new();
        let mut baseline = 720.0;
        for text in LEFT_PROSE.iter().take(5) {
            lines.push(line(text, 72.0, baseline, 0));
            baseline -= 12.0;
        }
        lines.push(captioned("Table 2. We compare", 72.0, baseline, 0));
        for text in AFTER_PROSE {
            baseline -= 12.0;
            lines.push(line(text, 72.0, baseline, 0));
        }
        let mut pages = vec![page_of(lines)];
        let report = tag_regions(&mut pages);
        assert_eq!(report, RegionReport::default());
        for text in AFTER_PROSE {
            assert_eq!(role_of(&pages[0], text), "body", "{text}");
        }
    }

    #[test]
    fn caption_starts() {
        let body = |text: &str| line(text, 72.0, 500.0, 0);
        assert_eq!(caption_kind(&body("TABLE I")), Some(Kind::Table));
        assert_eq!(caption_kind(&body("Fig. 3. Results")), Some(Kind::Figure));
        assert_eq!(
            caption_kind(&body("Algorithm 2 Greedy Search")),
            Some(Kind::Algorithm)
        );
        assert_eq!(caption_kind(&body("Figure 2 shows that the model")), None);
        assert_eq!(caption_kind(&body("Table 2 Results")), Some(Kind::Table));
        assert_eq!(
            caption_kind(&body("Fig. 3 Overview of pipeline")),
            Some(Kind::Figure)
        );
        assert_eq!(caption_kind(&body("Table 2 GPT")), None);
        let prose = "Algorithm 1 summarizes the procedure, followed by details of each step.";
        assert_eq!(caption_kind(&body(prose)), None);
        let tagged = captioned("Listing 1: Code.", 72.0, 500.0, 0);
        assert_eq!(caption_kind(&tagged), None);
    }

    #[test]
    fn token_and_line_classes() {
        for token in [
            "0.0",
            "10\u{207b}\u{00b2}",
            "(400,",
            "35%",
            "1e-3",
            "\u{2212}0.5",
        ] {
            assert!(is_numeric_token(token), "{token}");
        }
        for token in ["GPT-4o", "e", "Age", "-"] {
            assert!(!is_numeric_token(token), "{token}");
        }
        assert!(is_axis_like("Epoch 50"));
        assert!(is_axis_like("0.0 0.2 0.4 0.6 0.8 1.0"));
        assert!(!is_axis_like("Robot State"));
        assert!(is_prose(LEFT_PROSE[0]));
        assert!(is_prose_like(LEFT_PROSE[0]));
        assert!(is_prose_like(COLUMN_PROSE[0]));
        assert!(!is_prose_like("GPT-4o 0.61 0.42 0.33 0.55 0.21 0.18"));
        assert!(!is_prose_like(
            "Model Size Accuracy Recall Precision Latency Memory"
        ));
        assert!(!is_prose_like("the LLM uses GPT-4o and BERT for SFT"));
        assert!(opens_sentence(
            "We compare the simulated participants with the human ones here."
        ));
        assert!(!opens_sentence(COLUMN_PROSE[0]));
        assert!(!opens_sentence(
            "best performing models are highlighted in teal and worst in salmon."
        ));
        assert!(!is_prose("GPT-4o 0.61 0.42 0.33 0.55 0.21"));
        assert!(!is_prose(
            "Method Category Characteristics Advantages Limitations Example"
        ));
        assert!(is_algorithm_line(
            "1: Draw latent samples and generate G hypotheses"
        ));
        assert!(is_algorithm_line("Require: Minibatch, hypothesis count G"));
        assert!(!is_algorithm_line("the model is trained for ten epochs"));
        assert!(ends_sentence("in the figure.)"));
        assert!(!ends_sentence("Noise Prediction"));
    }

    #[test]
    fn mostly_numeric_lines_are_fragments_whatever_their_length() {
        assert!(is_fragment("0.2 0.4 0.6 0.8 1.0 Epoch 10 20 30"));
        assert!(is_mostly_numeric("0 25 50 75 100 Steps (k) 0.1 0.2"));
        assert!(!is_fragment(
            "the model reaches 0.61 on the held out test set"
        ));
        let mut lines: Vec<Line> = Vec::new();
        let mut baseline = 720.0;
        for text in LEFT_PROSE {
            lines.push(line(text, 72.0, baseline, 0));
            baseline -= 12.0;
        }
        // The two numeric lines have two words each, so they are not
        // axis-like; they count as fragments only by their numeric share.
        let axis = [
            "Validation accuracy over training time",
            "0.2 0.4 0.6 0.8 1.0 Train epoch 10 20 30",
            "0.5 1.0 1.5 2.0 2.5 Val loss 40 50 60",
            "Accuracy (%)",
        ];
        baseline -= 30.0;
        for text in axis {
            lines.push(line(text, 90.0, baseline, 1));
            baseline -= 12.0;
        }
        baseline -= 4.0;
        lines.push(captioned("Figure 6: Curves.", 72.0, baseline, 2));
        baseline -= 30.0;
        for text in AFTER_PROSE {
            lines.push(line(text, 72.0, baseline, 3));
            baseline -= 12.0;
        }
        let mut pages = vec![page_of(lines)];
        let report = tag_regions(&mut pages);
        for text in axis {
            assert_eq!(role_of(&pages[0], text), "figure", "{text}");
        }
        for text in LEFT_PROSE.iter().chain(AFTER_PROSE.iter()) {
            assert_eq!(role_of(&pages[0], text), "body", "{text}");
        }
        assert_eq!(report.figure, 4);
    }

    #[test]
    fn a_figure_below_a_mid_page_caption_is_figure_text() {
        let mut lines: Vec<Line> = Vec::new();
        let mut baseline = 720.0;
        for text in LEFT_PROSE {
            lines.push(line(text, 72.0, baseline, 0));
            baseline -= 12.0;
        }
        baseline -= 30.0;
        lines.push(captioned("Figure 4: Pipeline.", 72.0, baseline, 1));
        let labels = [
            ("Encoder", 90.0),
            ("Decoder block", 260.0),
            ("Latent z", 150.0),
            ("Loss", 330.0),
        ];
        for (text, x0) in labels {
            baseline -= 20.0;
            lines.push(line(text, x0, baseline, 2));
        }
        baseline -= 30.0;
        for text in AFTER_PROSE {
            lines.push(line(text, 72.0, baseline, 3));
            baseline -= 12.0;
        }
        let mut pages = vec![page_of(lines)];
        let report = tag_regions(&mut pages);
        for (text, _) in labels {
            assert_eq!(role_of(&pages[0], text), "figure", "{text}");
        }
        for text in LEFT_PROSE.iter().chain(AFTER_PROSE.iter()) {
            assert_eq!(role_of(&pages[0], text), "body", "{text}");
        }
        assert_eq!(report.figure, 4);
    }

    /// A line of vertical text: a box 10 pt wide and 5 pt per character
    /// tall, from (`x0`, `y0`) up.
    fn vline(text: &str, x0: f32, y0: f32) -> Line {
        let height = 5.0 * text.chars().count() as f32;
        Line {
            text: text.to_string(),
            bbox: Some(BBox {
                x0,
                y0,
                x1: x0 + 10.0,
                y1: y0 + height,
            }),
            ..Line::default()
        }
    }

    #[test]
    fn vertical_lines_below_a_table_caption_are_table_text() {
        let mut lines: Vec<Line> = Vec::new();
        let mut baseline = 760.0;
        for text in LEFT_PROSE {
            lines.push(line(text, 72.0, baseline, 0));
            baseline -= 12.0;
        }
        lines.push(captioned("Table 3: Scores.", 72.0, 680.0, 1));
        let headers = ["Accuracy", "Precision", "Recall@10"];
        for (k, text) in headers.iter().enumerate() {
            lines.push(vline(text, 100.0 + 30.0 * k as f32, 600.0));
        }
        // 42 rows: the ordinary walk runs past the region cap and drops
        // them, so only the vertical header lines are tagged.
        let mut rows: Vec<String> = Vec::new();
        let mut baseline = 585.0;
        for k in 0..42 {
            let text = format!("row {k} 0.51 0.62");
            lines.push(line(&text, 90.0, baseline, 2));
            rows.push(text);
            baseline -= 12.0;
        }
        let mut baseline = 70.0;
        for text in AFTER_PROSE {
            lines.push(line(text, 72.0, baseline, 3));
            baseline -= 12.0;
        }
        let mut pages = vec![page_of(lines)];
        let report = tag_regions(&mut pages);
        let page = &pages[0];
        for text in headers {
            assert_eq!(role_of(page, text), "table", "{text}");
        }
        for text in &rows {
            assert_eq!(role_of(page, text), "body", "{text}");
        }
        for text in LEFT_PROSE.iter().chain(AFTER_PROSE.iter()) {
            assert_eq!(role_of(page, text), "body", "{text}");
        }
        assert_eq!(report.table, 3);
    }

    #[test]
    fn stacked_single_letters_are_vertical() {
        let page = page_of(vec![
            line("G", 300.0, 500.0, 0),
            line("P", 300.0, 490.0, 0),
            line("U", 300.0, 480.0, 0),
            line("x", 100.0, 500.0, 0),
            line("Epoch", 200.0, 300.0, 0),
        ]);
        let entries: Vec<usize> = (0..page.lines.len()).collect();
        assert!(is_vertical(&page, &entries, 0));
        assert!(is_vertical(&page, &entries, 2));
        assert!(!is_vertical(&page, &entries, 3));
        assert!(!is_vertical(&page, &entries, 4));
        assert!(is_tall(&vline("Accuracy", 0.0, 0.0)));
        assert!(!is_tall(&line("Accuracy", 0.0, 0.0, 0)));
    }

    const SIDEWAYS_LABEL: &str =
        "Table A.6: Commercial systems, details of all methods and their training data";
    const SIDEWAYS_CAPTION_MORE: &str =
        "which were identified via linked publications and the vendor websites";
    const SIDEWAYS_CELLS: [&str; 6] = [
        "Population",
        "16 subjects from a",
        "WMH, ISL",
        "Manual segmentation",
        "tri-ethnic cohort",
        "Info not found.",
    ];
    const SIDEWAYS_PROSE_CELL: &str = "found by the authors in the vendor literature";
    const SIDEWAYS_PARAGRAPH: &str =
        "This paragraph after the table discusses the results of the review in more detail";
    const SIDEWAYS_LATE_CELL: &str = "Other cell text";

    /// A `/Rotate 90` page whose text runs upwards (tall boxes): a table
    /// label and its continuation at the left (the top once turned), cells
    /// in two rows, a narrow prose-like cell, a wide paragraph and a cell
    /// after it.
    fn sideways_page(labelled: bool) -> PageText {
        let mut lines: Vec<Line> = Vec::new();
        if labelled {
            lines.push(vline(SIDEWAYS_LABEL, 50.0, 100.0));
            lines.push(vline(SIDEWAYS_CAPTION_MORE, 62.0, 100.0));
        }
        for (k, text) in SIDEWAYS_CELLS.iter().enumerate() {
            let x0 = if k < 3 { 90.0 } else { 104.0 };
            let y0 = 100.0 + 180.0 * (k % 3) as f32;
            lines.push(vline(text, x0, y0));
        }
        lines.push(vline(SIDEWAYS_PROSE_CELL, 118.0, 100.0));
        lines.push(vline(SIDEWAYS_PARAGRAPH, 200.0, 100.0));
        lines.push(vline(SIDEWAYS_LATE_CELL, 220.0, 100.0));
        let mut page = page_of(lines);
        page.rotation = 90;
        page
    }

    #[test]
    fn a_sideways_table_page_is_table_text_in_the_turned_frame() {
        let mut pages = vec![sideways_page(true)];
        let report = tag_regions(&mut pages);
        let page = &pages[0];
        assert_eq!(role_of(page, SIDEWAYS_LABEL), "caption");
        assert_eq!(role_of(page, SIDEWAYS_CAPTION_MORE), "caption");
        for text in SIDEWAYS_CELLS {
            assert_eq!(role_of(page, text), "table", "{text}");
        }
        assert_eq!(role_of(page, SIDEWAYS_PROSE_CELL), "body");
        assert_eq!(role_of(page, SIDEWAYS_PARAGRAPH), "body");
        assert_eq!(role_of(page, SIDEWAYS_LATE_CELL), "body");
        assert_eq!(report.table, 6);
        assert_eq!(report.caption, 2);
        assert!(is_table_label("Table A.6 continued from previous page"));
        assert!(is_table_label("TABLE IV"));
        assert!(!is_table_label("Table of contents"));
        assert!(!is_table_label("Tables are listed below"));
    }

    #[test]
    fn a_sideways_page_without_a_table_label_tags_nothing() {
        let mut pages = vec![sideways_page(false)];
        let report = tag_regions(&mut pages);
        assert_eq!(report, RegionReport::default());
        assert!(pages[0].lines.iter().all(|l| l.role == "body"));
        assert!(pages[0].warnings.is_empty());
    }

    /// Adds a line of one span at `size` pt to `page`.
    fn push_sized(page: &mut PageText, text: &str, baseline: f32, size: f32) {
        let seq = u32::try_from(page.spans.len()).unwrap();
        let width = 0.5 * size * text.chars().count() as f32;
        let bbox = BBox {
            x0: 72.0,
            y0: baseline - 0.2 * size,
            x1: 72.0 + width,
            y1: baseline + 0.8 * size,
        };
        page.spans.push(crate::schema::Span {
            text: text.to_string(),
            bbox: Some(bbox),
            font: None,
            size: Some(size),
            seq,
        });
        page.lines.push(Line {
            text: text.to_string(),
            bbox: Some(bbox),
            spans: vec![seq],
            ..Line::default()
        });
    }

    /// Body prose at 10 pt, then `foot` at 8 pt from 120 pt up the page down.
    fn footnote_page(foot: &[&str]) -> PageText {
        let mut page = PageText::new(1, 612.0, 792.0, 0);
        let mut baseline = 720.0;
        for text in LEFT_PROSE.iter().chain(AFTER_PROSE.iter()) {
            push_sized(&mut page, text, baseline, 10.0);
            baseline -= 12.0;
        }
        let mut baseline = 120.0;
        for text in foot {
            push_sized(&mut page, text, baseline, 8.0);
            baseline -= 10.0;
        }
        page.text = page
            .lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<&str>>()
            .join("\n");
        page
    }

    #[test]
    fn small_marked_lines_at_the_page_foot_are_footnotes() {
        let foot = [
            "1 https://github.com/example/repo",
            "2 Work done while the author was at the example lab",
            "and continued later.",
        ];
        let mut pages = vec![footnote_page(&foot)];
        let report = tag_regions(&mut pages);
        for text in foot {
            assert_eq!(role_of(&pages[0], text), "footnote", "{text}");
        }
        for text in LEFT_PROSE.iter().chain(AFTER_PROSE.iter()) {
            assert_eq!(role_of(&pages[0], text), "body", "{text}");
        }
        assert_eq!(report.footnote, 3);
        assert!(
            pages[0]
                .warnings
                .contains(&"regions: footnote text 3 lines".to_string())
        );
        for text in [
            "*Corresponding author",
            "\u{2020}Equal contribution",
            "\u{00b9}Code is public",
        ] {
            assert!(starts_footnote(text), "{text}");
        }
        for text in ["2024 was a good year", "10 20 30 40", "Table 2 lists"] {
            assert!(!starts_footnote(text), "{text}");
        }
    }

    #[test]
    fn small_lines_without_a_marker_or_in_a_long_run_are_not_footnotes() {
        let plain = ["the small print of this page", "continues here"];
        let mut pages = vec![footnote_page(&plain)];
        assert_eq!(tag_regions(&mut pages), RegionReport::default());
        let owned: Vec<String> = (1..=12)
            .map(|k| format!("{k} Author, A. Things."))
            .collect();
        let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
        let mut pages = vec![footnote_page(&refs)];
        assert_eq!(tag_regions(&mut pages), RegionReport::default());
    }

    #[test]
    fn display_math_fragments_are_math() {
        for text in [
            "\u{1d70f}:",
            "\u{2212} \u{1d43c}\u{1d451}",
            "inf + 2m\u{3c1}c\u{b2}",
            "k=0 \u{3c1}k \u{2264} \u{3c1}",
            "L(\u{3b8}) = \u{3a3} w\u{2083} \u{2113}",
            "x \u{2208} X",
        ] {
            assert!(is_math_line(text), "{text}");
        }
        for text in [
            "Note that for any t = 1, . . . , K,",
            "3.2 \u{3a6}-divergence estimates",
            "\u{2212}0.5 \u{2212}0.3 0.2",
            "GPT-4o 0.61 0.42",
            "the loss is L = x + y",
            "\u{397} \u{3b3}\u{3bb}\u{3ce}\u{3c3}\u{3c3}\u{3b1} \u{3b5}\u{3af}\u{3bd}\u{3b1}\u{3b9} \u{3cc}\u{3bc}\u{3bf}\u{3c1}\u{3c6}\u{3b7}",
            "(3)",
        ] {
            assert!(!is_math_line(text), "{text}");
        }
        let math = "\u{2212} \u{1d43c}\u{1d451}";
        let mut lines: Vec<Line> = Vec::new();
        let mut baseline = 720.0;
        for text in LEFT_PROSE {
            lines.push(line(text, 72.0, baseline, 0));
            baseline -= 12.0;
        }
        lines.push(line(math, 200.0, baseline, 1));
        let mut pages = vec![page_of(lines)];
        let report = tag_regions(&mut pages);
        assert_eq!(role_of(&pages[0], math), "math");
        assert_eq!(report.math, 1);
        assert!(
            pages[0]
                .warnings
                .contains(&"regions: math text 1 lines".to_string())
        );
    }

    #[test]
    fn section_headings_below_a_figure_caption_are_not_figure_text() {
        let mut lines: Vec<Line> = Vec::new();
        let mut baseline = 720.0;
        for text in LEFT_PROSE {
            lines.push(line(text, 72.0, baseline, 0));
            baseline -= 12.0;
        }
        baseline -= 30.0;
        lines.push(captioned("Figure 4: Architecture.", 72.0, baseline, 1));
        for text in ["3 Method", "3.1 Problem Setup"] {
            baseline -= 24.0;
            lines.push(line(text, 72.0, baseline, 2));
        }
        baseline -= 24.0;
        for text in AFTER_PROSE {
            lines.push(line(text, 72.0, baseline, 3));
            baseline -= 12.0;
        }
        let mut pages = vec![page_of(lines)];
        let report = tag_regions(&mut pages);
        assert_eq!(report, RegionReport::default());
        assert_eq!(role_of(&pages[0], "3 Method"), "body");
        assert_eq!(role_of(&pages[0], "3.1 Problem Setup"), "body");
        for text in ["3 Method", "3.1 Problem Setup", "A.2 Proofs", "IV. Results"] {
            assert!(is_numbered_heading(text), "{text}");
        }
        for text in ["0.2 Train loss", "3 shows", "Encoder block", "IV Results"] {
            assert!(!is_numbered_heading(text), "{text}");
        }
    }

    const SHREDDED: [&str; 6] = [
        "found.",
        "not",
        "Info",
        "segmen tation",
        "Info (Anatomical segmen DSC = DSC =",
        "the model was trained on the data we had",
    ];

    /// A `/Rotate 90` page as the lopdf backend and reading order leave a
    /// landscape table: every span of rotated text is a tall box, and the
    /// lines are horizontal slices across the rotated lines.
    fn shredded_page(labelled: bool) -> PageText {
        let mut pieces: Vec<&str> = Vec::new();
        if labelled {
            pieces.extend(["T", "able", "A.6", "continued", "from", "previous", "page"]);
        }
        pieces.extend([
            "segmen",
            "tation",
            "found.",
            "Anatomical",
            "Validation:",
            "training",
            "subjects",
            "radiologist",
            "consensus",
            "infarcts",
            "cortical",
            "Hippocampus",
        ]);
        let mut page = PageText::new(1, 612.0, 792.0, 90);
        for (k, text) in pieces.iter().enumerate() {
            let x0 = 50.0 + 12.0 * k as f32;
            let height = 5.0 * text.chars().count() as f32;
            page.spans.push(crate::schema::Span {
                text: (*text).to_string(),
                bbox: Some(BBox {
                    x0,
                    y0: 100.0,
                    x1: x0 + 10.0,
                    y1: 100.0 + height,
                }),
                font: None,
                size: Some(10.0),
                seq: u32::try_from(k).unwrap(),
            });
        }
        let mut baseline = 700.0;
        for text in SHREDDED {
            page.lines.push(line(text, 50.0, baseline, 0));
            baseline -= 12.0;
        }
        page
    }

    #[test]
    fn a_shredded_sideways_table_page_is_table_text() {
        let mut pages = vec![shredded_page(true)];
        assert!(sideways_by_spans(&pages[0]));
        assert!(!is_sideways(&pages[0]));
        let report = tag_regions(&mut pages);
        for text in SHREDDED.iter().take(5) {
            assert_eq!(role_of(&pages[0], text), "table", "{text}");
        }
        assert_eq!(role_of(&pages[0], SHREDDED[5]), "body");
        assert_eq!(report.table, 5);
        assert!(label_follows("A.6continued"));
        assert!(label_follows("3:Results"));
        assert!(label_follows("IV.Error"));
        assert!(!label_follows("sareusedhere"));
        assert!(!label_follows("IVERSON"));
    }

    #[test]
    fn a_shredded_sideways_page_without_a_table_label_tags_nothing() {
        let mut pages = vec![shredded_page(false)];
        assert!(sideways_by_spans(&pages[0]));
        let report = tag_regions(&mut pages);
        assert_eq!(report, RegionReport::default());
        assert!(pages[0].lines.iter().all(|l| l.role == "body"));
    }
}
