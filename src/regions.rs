//! Region tagging: figure text, table cells and algorithm blocks next to
//! their captions, text inside figure boxes and monospace code listings
//! get a non-body [`Line::role`], so body-only consumers can
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
//! `Table 2 Results`: a label followed by a capitalised word). Labels are
//! numbers (`2`, `3.1`), a capital letter and a number (`A.1`, `S2`,
//! `B3`), roman numerals (`II`) or a number and a lowercase letter
//! (`2a`); `Table 1 continued from previous page` and `Table 3
//! (continued)` are table caption starts too. Continuation lines below a
//! caption start, in its band, with no blank separator, are tagged
//! `caption` (at most [`CAPTION_MAX_LINES`] lines in all) while they are
//! prose or end a sentence (or, in a closed block, are prose-like), are
//! not numbered headings or caption starts, and keep the start's font
//! size (when both sizes are known). The block is *closed* when the lines
//! from the start down to the first blank separator are at most
//! [`CAPTION_MAX_LINES`]: a caption set off from what follows. After a
//! line ending a sentence (the start's own full stop included), the next
//! line continues only when it starts lowercase or the block is closed;
//! otherwise a prose-like line that opens a sentence (an uppercase first
//! word and at least 10 words) stops it unless the block is closed. Under
//! a caption spanning the middle of a two-column page, a line that does
//! not span it too stops it. An untagged caption start is tagged `caption`
//! once a region is found under it. A caption start directly under a prose
//! line (no blank separator) is ignored.
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
//!   A short sentence ending aligned with the preceding unfinished prose
//!   line, at the same height and ordinary leading, stays with its paragraph.
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
//!   width (or another caption start). The other region walks and
//!   footnotes are skipped there. A sideways page without a table region
//!   of its own, following a sideways page with `table` lines, continues
//!   that table when the column starts of its `body` lines match those of
//!   the previous page's `table` lines (at least 2 starts within 6 pt,
//!   and at least half of the larger set): every line from the top is
//!   taken the same way.
//! - continued tables: on a page whose `Table N continued from previous
//!   page` (or `Table N (continued)`) label has no prose-like `body` line
//!   above it, every `body` line above the label, and below it every line
//!   that is not prose-like up to the next caption start or the first
//!   prose-like line spanning at least 40 % of the page width (a
//!   `longtable` page), is tagged `table` and the label `caption`, before
//!   the other walks and with no line cap.
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
//! - tables with paragraph cells: after the walks, next to a table
//!   caption (below it, else above it), a ruled table is the lines between
//!   the first rule at most 80 pt from the caption block (no prose-like
//!   line between) and the farthest rule of the same width before the
//!   next caption, heading or footnote; rules are `PageText::figures` of
//!   kind `rule`, or any figure box under 3 pt tall and at least 30 pt
//!   wide. Without rules, the lines next to the caption (up to a gap of 3
//!   line heights, a caption start, a numbered heading or a line of
//!   another role) are a table when, up to the first two consecutive prose
//!   or prose-like lines reaching across a column start other than the first (body text
//!   resumed), their left edges form at least 3 column starts (each shared
//!   by 2 lines, at least 20 pt apart) and at least 2 rows hold lines of 3
//!   columns side by side. Every `body` line of such a table is tagged
//!   `table`, prose cells included.
//! - code: a run of at least 3 consecutive `body` lines (furniture
//!   skipped) with at least 80 % of their characters in a monospace font
//!   (a name containing `Mono`, `Courier`, `Consol`, `Menlo`,
//!   `Typewriter`, `CMTT`, `SFTT` or `TXTT`, any case) is tagged `code`.
//! - figure boxes: then a `body` line with at least 70 % of its box inside
//!   a figure box (`PageText::figures` other than rules) at least 60 pt
//!   tall is tagged with the role of the caption the box belongs to (the
//!   nearest caption block at most 36 pt above or below it with an
//!   overlapping x range, or through a box stacked that close to such a
//!   box), `figure` when it has none. Prose-like lines are tagged only when
//!   the box has a caption (a framed prompt or dialogue box in a figure
//!   float) or is a frame: a `vector` box at least 100 pt tall holding at
//!   least 3 short lines (at most 4 words, or numeric) on a page with no
//!   caption. A box covering at least 70 % of the page width and height
//!   needs a caption to be used at all, and one covering 95 % of the page
//!   area (a scan under its text layer) is never used. In a box with a
//!   caption, a run of at least 3 consecutive prose-like lines of at least
//!   12 words lying more than 36 pt from every caption block overlapping
//!   the box's x range stays body (column prose a backend box happens to
//!   cover), unless the box is a framed text box: a `vector` box whose
//!   lines all sit at least 3 pt inside its left and right sides with at
//!   least 90 % of their box inside it, and which is no wider than 55 % of
//!   the page on a two-column page (95 % otherwise) or holds at least 3
//!   short lines. Every line of a framed text box with a caption is
//!   tagged, however long its prose runs (a tall prompt or dialogue float
//!   whose caption sits below the whole box).
//! - graphics labels: at least 4 `body` lines of at most 4 words (not
//!   numbered headings) whose boxes overlap a figure box at least 20 pt
//!   wide and tall, lie within 24 pt of it (not ending a sentence), or lie
//!   between it and its `Figure` caption (at most 120 pt below or above
//!   it, with no prose-like line between), are tagged `figure`; so are
//!   stacked labels: at least 3 single-word `body` lines, each directly
//!   above or below another (x ranges overlapping, at most one line
//!   height apart), at least one of them within 24 pt of such a box.
//! - math: last, a `body` line with at most 2 ordinary words (letter runs
//!   of 3 or more, not `log`, `max` and the like), at most 1 of them of 4
//!   or more letters, and at least
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
//! `algorithm` by the caption walks (pseudo-code marker lines excepted
//! under an `Algorithm` caption), and every walk stops at the first one.
//! The exceptions rest on drawn evidence, not on a walk: lines inside a
//! captioned figure box or a frame, between the rules or in the column
//! pattern of a table with paragraph cells, and monospace code lines.
//! Figure and table regions need at least [`MIN_REGION_LINES`] lines, and
//! a region longer than [`REGION_MAX_LINES`] lines is dropped, not tagged
//! (continued tables excepted). A page that
//! already carries a `regions:` warning is not tagged again.

use std::cmp::Ordering;

use crate::reading_order::median;
use crate::schema::{BBox, Line, PageText};

/// Most lines a caption (start plus continuations) may take.
pub const CAPTION_MAX_LINES: usize = 14;
/// Most caption candidates processed on one page. Real pages contain far
/// fewer; bounding attacker-controlled candidates prevents repeated region
/// walks from becoming quadratic in the number of extracted lines.
pub const CAPTION_CANDIDATE_MAX: usize = 64;
/// Most caption lines through which untagged prose lines are skipped by
/// the walk below a caption, and wide prose lines are taken under a
/// sideways table label.
const PROSE_CAPTION_LINES: usize = 6;
/// Largest font size difference, in points, between a caption start and
/// its continuation lines.
const CAPTION_SIZE_SLACK: f32 = 0.5;
/// Largest distance, in points, between two table column starts matched
/// across pages.
const COLUMN_MATCH: f32 = 6.0;
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
/// Alignment and line-height slack for a short paragraph-ending line.
const PARAGRAPH_EDGE_SLACK: f32 = 2.0;
const PARAGRAPH_HEIGHT_SLACK: f32 = 1.0;
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
/// Most ordinary words of at least [`MATH_LONG_LETTERS`] letters in a
/// display-math line.
const MATH_MAX_LONG_WORDS: usize = 1;
/// Letters in an ordinary word that counts against [`MATH_MAX_LONG_WORDS`].
const MATH_LONG_LETTERS: usize = 4;
/// Share of a line's box area that must lie inside a figure box for the
/// line to be text inside the figure.
const FIGURE_INSIDE: f32 = 0.7;
/// Lowest figure box, in points, whose inside lines are figure text.
const FIGURE_MIN_HEIGHT: f32 = 60.0;
/// A figure box covering at least this share of both the page width and
/// the page height is the whole page.
const WHOLE_PAGE: f32 = 0.7;
/// A figure box covering at least this share of the page area is a page
/// background (a scanned page under its text layer) and is never used.
const PAGE_BACKGROUND: f32 = 0.95;
/// Largest vertical distance, in points, between a figure box and a
/// caption block (or another adjacent box) for the two to belong together.
const CAPTION_REACH: f32 = 36.0;
/// Fewest short lines in a graphics-label cluster.
const LABEL_MIN_LINES: usize = 4;
/// Largest distance, in points, from a figure box to graphics labels that
/// do not touch it.
const LABEL_NEAR: f32 = 24.0;
/// Fewest single-word lines in a column of stacked labels.
const STACK_MIN_LINES: usize = 3;
/// Most lines inspected for graphics-label clusters on one page.
const LABEL_MAX_LINES: usize = 20_000;
/// Most figure boxes inspected for graphics-label clusters on one page.
const LABEL_MAX_FIGURES: usize = 256;
/// Most pair comparisons used to connect stacked single-word labels.
const STACK_MAX_COMPARISONS: usize = 1_000_000;
/// Lowest `vector` figure box, in points, framing figure text on a page
/// without a caption.
const FRAME_MIN_HEIGHT: f32 = 100.0;
/// Fewest short lines inside such a frame.
const FRAME_MIN_SHORT: usize = 3;
/// Fewest consecutive long prose-like lines in a captioned figure box that
/// are column prose when far from the caption.
const COLUMN_RUN_MIN_LINES: usize = 3;
/// Fewest words in a line of such a run.
const COLUMN_RUN_WORDS: usize = 12;
/// Smallest inset, in points, of every line from the left and right sides
/// of a framed text box.
const TEXT_BOX_INSET: f32 = 3.0;
/// Share of a line's box area that must lie inside a framed text box.
const TEXT_BOX_INSIDE: f32 = 0.9;
/// Widest framed text box on a two-column page, as a share of the page
/// width.
const TEXT_BOX_COLUMN_WIDTH: f32 = 0.55;
/// Widest framed text box on any other page, as a share of the page width.
const TEXT_BOX_PAGE_WIDTH: f32 = 0.95;
/// Figure kind the backend gives a cluster of painted paths.
const KIND_VECTOR: &str = "vector";
/// Tallest gap, in points, between a figure box and its caption in which
/// graphics labels are looked for.
const LABEL_ZONE_MAX: f32 = 120.0;
/// Line centres closer than this, in points, lie on one table row.
const ROW_TOLERANCE: f32 = 4.0;
/// Smallest width and height, in points, of a figure box graphics labels
/// sit on.
const LABEL_BOX_MIN: f32 = 20.0;
/// Figure kind the backend gives each thin horizontal rule.
const KIND_RULE: &str = "rule";
/// A figure box lower than this, in points, and at least
/// [`RULE_MIN_WIDTH`] wide is a rule whatever its kind.
const RULE_HEIGHT: f32 = 3.0;
/// Narrowest rule, in points.
const RULE_MIN_WIDTH: f32 = 30.0;
/// Tolerance, in points, when matching rule ends and column starts.
const X_TOLERANCE: f32 = 3.0;
/// Most distance, in points, from a table caption block to the first rule
/// of its table.
const RULE_REACH: f32 = 80.0;
/// Smallest distance, in points, between two distinct table column starts.
const COLUMN_MIN_GAP: f32 = 20.0;
/// Fewest distinct column starts (each shared by at least 2 lines) of a
/// table with paragraph cells.
const TABLE_MIN_COLUMNS: usize = 3;
/// Fewest rows in which lines of [`TABLE_MIN_COLUMNS`] columns sit side by
/// side.
const TABLE_MIN_ROWS: usize = 2;
/// Most lines in a table block with paragraph cells.
pub const PARAGRAPH_TABLE_MAX_LINES: usize = 150;
/// A vertical gap wider than this many median line heights ends a table
/// block.
const TABLE_BREAK: f32 = 3.0;
/// Fewest consecutive monospace lines tagged `code`.
const CODE_MIN_LINES: usize = 3;
/// Share of a line's non-blank characters set in a monospace font for the
/// line to be code, as `numerator / 10`.
const CODE_SHARE_TENTHS: usize = 8;
/// Lower-cased substrings of monospace font names.
const MONOSPACE_MARKERS: [&str; 8] = [
    "mono",
    "courier",
    "consol",
    "menlo",
    "typewriter",
    "cmtt",
    "sftt",
    "txtt",
];
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
const ROLE_CODE: &str = "code";

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
    /// Monospace listing lines tagged `code`.
    pub code: usize,
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
    /// Median line height.
    height: f32,
    width: f32,
    mid: f32,
    two_column: bool,
}

/// Tag caption continuations, figure text, table cells, algorithm blocks,
/// code listings, footnotes and display-math lines on every page (see the module
/// documentation). Adds a page
/// warning such as `regions: figure text N lines` for each role with new
/// tags. Idempotent: a page already carrying such a warning is skipped,
/// and a page without one had nothing to tag.
pub fn tag_regions(pages: &mut [PageText]) -> RegionReport {
    let mut total = RegionReport::default();
    let mut carry: Option<Vec<f32>> = None;
    for page in pages {
        let report = tag_page(page, carry.as_deref());
        carry = sideways_table_starts(page);
        total.caption += report.caption;
        total.figure += report.figure;
        total.table += report.table;
        total.algorithm += report.algorithm;
        total.math += report.math;
        total.footnote += report.footnote;
        total.code += report.code;
        let counts = [
            ("caption text", report.caption),
            ("figure text", report.figure),
            ("table text", report.table),
            ("algorithm text", report.algorithm),
            ("math text", report.math),
            ("footnote text", report.footnote),
            ("code text", report.code),
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

/// Tag one page; the counts are of lines newly tagged. `carry` holds the
/// table column starts of the previous page when it was a sideways table
/// page (see [`sideways_table_starts`]).
fn tag_page(page: &mut PageText, carry: Option<&[f32]>) -> RegionReport {
    let mut report = RegionReport::default();
    if page.width <= 0.0 || page.lines.is_empty() {
        return report;
    }
    if page
        .warnings
        .iter()
        .any(|w| w.starts_with(WARNING_PREFIX) || w.starts_with("resource_limit: regions:"))
    {
        return report;
    }
    if is_sideways(page) {
        report = tag_sideways(page);
        if report.table == 0
            && let Some(starts) = carry
        {
            report.table += tag_sideways_continuation(page, starts);
        }
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
    let (caption, table) = tag_continued_table(page);
    report.caption += caption;
    report.table += table;
    let mut captions: Vec<(usize, Kind)> = page
        .lines
        .iter()
        .enumerate()
        .filter_map(|(k, line)| caption_kind(line).map(|kind| (k, kind)))
        .take(CAPTION_CANDIDATE_MAX + 1)
        .collect();
    if captions.len() > CAPTION_CANDIDATE_MAX {
        // Do not process a partial set: which captions happened to occur first
        // must not determine the roles on a deliberately pathological page.
        captions.clear();
        page.warnings.push(format!(
            "resource_limit: {WARNING_PREFIX}more than {CAPTION_CANDIDATE_MAX} caption candidates; caption-based tagging skipped"
        ));
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
    let boxes = caption_boxes(page, &geometry, &captions);
    tag_paragraph_tables(page, &geometry, &boxes, &mut report);
    report.code += tag_code(page);
    tag_figure_boxes(page, &geometry, &boxes, &mut report);
    report.figure += tag_label_clusters(page, &boxes);
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
        height,
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
    line.bbox.and_then(ordered_box)
}

/// `b` with ordered corners, when all four are finite.
fn ordered_box(b: BBox) -> Option<BBox> {
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
    if !is_label_number(core) {
        return None;
    }
    if kind == Kind::Table && continued_marker(text) {
        return Some(kind);
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

/// A caption label: a number (`2`, `3.1`), a capital letter and a number
/// (`A.1`, `S2`, `B3`), a roman numeral (`II`) or a number and one
/// lowercase letter (`2a`).
fn is_label_number(core: &str) -> bool {
    if core.is_empty() {
        return false;
    }
    if core.chars().all(|c| matches!(c, 'I' | 'V' | 'X' | 'L')) {
        return true;
    }
    let rest = core
        .strip_prefix(|c: char| c.is_ascii_uppercase())
        .map_or(core, |r| r.strip_prefix('.').unwrap_or(r));
    let number = rest
        .strip_suffix(|c: char| c.is_ascii_lowercase())
        .unwrap_or(rest);
    number.starts_with(|c: char| c.is_ascii_digit())
        && number.chars().all(|c| c.is_ascii_digit() || c == '.')
}

/// One of the two words after the label is `continued` (punctuation
/// ignored): `Table 1 continued from previous page`, `Table 3
/// (continued)`, `Table 2: Continued`.
fn continued_marker(text: &str) -> bool {
    text.split_whitespace().skip(2).take(2).any(|token| {
        token
            .trim_matches(|c: char| !c.is_alphanumeric())
            .eq_ignore_ascii_case("continued")
    })
}

/// The first letter or digit of `text` is lowercase.
fn starts_lowercase(text: &str) -> bool {
    text.chars()
        .find(|c| c.is_alphanumeric())
        .is_some_and(char::is_lowercase)
}

/// The lines from the caption start at `entries[pos]` down to the first
/// blank separator (furniture skipped, the end of the band counting as
/// one) are at most [`CAPTION_MAX_LINES`]: the caption is set off from
/// what follows it.
fn caption_block_closed(page: &PageText, entries: &[usize], pos: usize, blank: f32) -> bool {
    let mut count: usize = 1;
    let mut prev = entries[pos];
    for &i in entries.iter().skip(pos + 1) {
        if page.lines[i].role == ROLE_FURNITURE {
            continue;
        }
        if gap(page, prev, i) > blank {
            return true;
        }
        count += 1;
        if count > CAPTION_MAX_LINES {
            return false;
        }
        prev = i;
    }
    true
}

/// The line may continue a caption in `band`: it does not open a prose
/// sentence and spans the gutter when the caption does.
fn continues_caption(line: &Line, band: Band) -> bool {
    !opens_sentence(&line.text) && finite_box(line).is_some_and(|b| band.crossed_by(b))
}

/// Body lines continuing the caption at `entries[pos]` (see the module
/// documentation): below it in `band` with no blank separator, through
/// sentence ends, at most [`CAPTION_MAX_LINES`] lines in all. Under an
/// `Algorithm` caption a pseudo-code line ends it.
fn caption_continuation(
    page: &PageText,
    entries: &[usize],
    pos: usize,
    kind: Kind,
    band: Band,
    blank: f32,
) -> Vec<usize> {
    let start = entries[pos];
    let start_size = line_size(page, &page.lines[start]);
    let closed = caption_block_closed(page, entries, pos, blank);
    let mut sentence_done = ends_sentence(&page.lines[start].text);
    let mut more: Vec<usize> = Vec::new();
    let mut prev = start;
    for &i in entries.iter().skip(pos + 1) {
        if more.len() + 1 >= CAPTION_MAX_LINES {
            break;
        }
        let line = &page.lines[i];
        if line.role != ROLE_BODY || gap(page, prev, i) > blank {
            break;
        }
        let text = line.text.as_str();
        let crossed = finite_box(line).is_some_and(|b| band.crossed_by(b));
        if !crossed || is_numbered_heading(text) || caption_kind(line).is_some() {
            break;
        }
        if kind == Kind::Algorithm && is_algorithm_line(text) {
            break;
        }
        let resized = match (start_size, line_size(page, line)) {
            (Some(a), Some(b)) => (a - b).abs() > CAPTION_SIZE_SLACK,
            _ => false,
        };
        if resized {
            break;
        }
        let ends = ends_sentence(text);
        let wordy = ends || is_prose(text) || (closed && is_prose_like(text));
        let fits = if sentence_done {
            closed || starts_lowercase(text)
        } else {
            closed || !opens_sentence(text)
        };
        if !(wordy && fits) {
            break;
        }
        more.push(i);
        prev = i;
        sentence_done = ends;
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
        let upper = k.checked_sub(1).and_then(|u| entries.get(u));
        if upper.is_some_and(|&u| {
            (is_prose(text) && is_prose(&page.lines[u].text))
                || finishes_paragraph(page, u, i, blank)
        }) {
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

/// A short final sentence line belongs to the preceding prose paragraph
/// when its geometry still follows that paragraph. Dehyphenation can move
/// a word fragment out of this line, so its word count is not a boundary.
fn finishes_paragraph(page: &PageText, upper: usize, lower: usize, blank: f32) -> bool {
    let before = &page.lines[upper];
    let after = &page.lines[lower];
    if before.role != ROLE_BODY
        || !is_prose(&before.text)
        || ends_sentence(&before.text)
        || before.text.trim_end().ends_with([':', ';'])
        || !ends_sentence(&after.text)
        || !after.text.trim_start().starts_with(char::is_lowercase)
    {
        return false;
    }
    let (Some(a), Some(b)) = (finite_box(before), finite_box(after)) else {
        return false;
    };
    let leading = a.y0 - b.y1;
    (a.x0 - b.x0).abs() <= PARAGRAPH_EDGE_SLACK
        && ((a.y1 - a.y0) - (b.y1 - b.y0)).abs() <= PARAGRAPH_HEIGHT_SLACK
        && (0.0..=blank).contains(&leading)
}

/// Body lines below the caption at `entries[pos]` (after its continuation
/// lines: `caption` lines up to [`CAPTION_MAX_LINES`] in all, untagged
/// prose lines up to [`PROSE_CAPTION_LINES`]) up to the next prose
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
            let tagged = line.role == ROLE_CAPTION && caption_lines < CAPTION_MAX_LINES;
            let skipped = line.role == ROLE_BODY
                && caption_lines < PROSE_CAPTION_LINES
                && gap_above <= blank
                && prose(i)
                && continues_caption(line, band);
            if tagged || skipped {
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
            if more.len() + 1 >= PROSE_CAPTION_LINES {
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
        let region = table_run(&view, &entries, next, wide);
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

/// The band holding every line of a page `width` points wide.
fn whole_band(width: f32) -> Band {
    Band {
        lo: f32::NEG_INFINITY,
        hi: f32::INFINITY,
        width,
        cross: None,
    }
}

/// Table lines of `view` from `entries[from]` down: every `body` line that
/// is not prose-like, up to a `Table` label, another caption start or the
/// first prose-like line at least `wide` points wide. Narrow prose-like
/// lines are left out; furniture and lines of other roles are skipped.
fn table_run(view: &PageText, entries: &[usize], from: usize, wide: f32) -> Vec<usize> {
    let mut region: Vec<usize> = Vec::new();
    for &i in entries.iter().skip(from) {
        let line = &view.lines[i];
        if line.role == ROLE_FURNITURE {
            continue;
        }
        if is_table_label(&line.text) || caption_kind(line).is_some() {
            break;
        }
        if line.role != ROLE_BODY {
            continue;
        }
        if is_prose_like(&line.text) {
            if finite_box(line).is_some_and(|b| b.x1 - b.x0 >= wide) {
                break;
            }
            continue;
        }
        region.push(i);
    }
    region
}

/// At least 2 of the column starts `b` lie within [`COLUMN_MATCH`] of a
/// start in `a`, and they are at least half of the larger set.
fn same_columns(a: &[f32], b: &[f32]) -> bool {
    let matched = b
        .iter()
        .filter(|&&x| a.iter().any(|&y| (x - y).abs() <= COLUMN_MATCH))
        .count();
    matched >= 2 && matched * 2 >= a.len().max(b.len())
}

/// Column starts, in the turned frame, of the `table` lines of a sideways
/// page, when there are at least 2: what a following sideways page
/// without a table label is matched against.
fn sideways_table_starts(page: &PageText) -> Option<Vec<f32>> {
    if page.width <= 0.0 || !is_sideways(page) {
        return None;
    }
    let view = turned_view(page);
    let lines: Vec<usize> = view
        .lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.role == ROLE_TABLE)
        .map(|(k, _)| k)
        .collect();
    let starts = column_starts(&view, &lines);
    (starts.len() >= 2).then_some(starts)
}

/// Continue the previous page's sideways table on this sideways page when
/// the column starts of its `body` lines match `starts` (see
/// [`same_columns`]): its table lines from the top (see [`table_run`]) are
/// tagged `table`; the count of lines newly tagged.
fn tag_sideways_continuation(page: &mut PageText, starts: &[f32]) -> usize {
    let view = turned_view(page);
    let entries = band_entries(&view, whole_band(view.width));
    let body: Vec<usize> = entries
        .iter()
        .copied()
        .filter(|&i| view.lines[i].role == ROLE_BODY)
        .collect();
    if !same_columns(starts, &column_starts(&view, &body)) {
        return 0;
    }
    let region = table_run(&view, &entries, 0, SIDEWAYS_PARAGRAPH_WIDTH * view.width);
    let mut n = 0;
    for i in region {
        n += tag(&mut page.lines[i], ROLE_TABLE);
    }
    n
}

/// A table caption start marked as continued (see [`continued_marker`]).
fn is_continued_label(line: &Line) -> bool {
    caption_kind(line) == Some(Kind::Table) && continued_marker(&line.text)
}

/// Tag a `longtable` continuation page (see the module documentation);
/// the counts of caption and table lines newly tagged.
fn tag_continued_table(page: &mut PageText) -> (usize, usize) {
    let entries = band_entries(page, whole_band(page.width));
    let Some(pos) = entries
        .iter()
        .position(|&i| is_continued_label(&page.lines[i]))
    else {
        return (0, 0);
    };
    let prose_above = entries.iter().take(pos).any(|&i| {
        let line = &page.lines[i];
        line.role == ROLE_BODY && is_prose_like(&line.text)
    });
    if prose_above {
        return (0, 0);
    }
    let mut region: Vec<usize> = entries
        .iter()
        .take(pos)
        .copied()
        .filter(|&i| page.lines[i].role == ROLE_BODY)
        .collect();
    let wide = SIDEWAYS_PARAGRAPH_WIDTH * page.width;
    region.extend(table_run(page, &entries, pos + 1, wide));
    if region.is_empty() {
        return (0, 0);
    }
    let caption = tag(&mut page.lines[entries[pos]], ROLE_CAPTION);
    let mut table = 0;
    for i in region {
        table += tag(&mut page.lines[i], ROLE_TABLE);
    }
    (caption, table)
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

/// Ordinary words in a token, their letters, and how many of them have at
/// least [`MATH_LONG_LETTERS`] letters: runs of at least
/// [`MATH_WORD_LETTERS`] letters of one script that are not a math
/// function name (`log`, `max`, ...).
fn ordinary_words(token: &str) -> (usize, usize, usize) {
    let mut words: usize = 0;
    let mut letters: usize = 0;
    let mut long: usize = 0;
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
            if n >= MATH_LONG_LETTERS {
                long += 1;
            }
        }
        run.clear();
        if next.is_some() {
            run.push(c);
        }
        script = next;
    }
    (words, letters, long)
}

/// A display-math fragment: at most [`MATH_MAX_WORDS`] ordinary words, of
/// which at most [`MATH_MAX_LONG_WORDS`] have 4 or more letters, at least
/// one math character, ordinary-word letters at most half of the
/// other non-blank characters, and not mostly numbers (a table row or an
/// axis).
fn is_math_line(text: &str) -> bool {
    if !text.chars().any(is_math_char) || is_mostly_numeric(text) {
        return false;
    }
    let mut words: usize = 0;
    let mut letters: usize = 0;
    let mut long: usize = 0;
    for token in text.split_whitespace() {
        let (w, l, n) = ordinary_words(token);
        words += w;
        letters += l;
        long += n;
    }
    if words > MATH_MAX_WORDS || long > MATH_MAX_LONG_WORDS {
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

/// A caption start and the box of its whole caption block.
#[derive(Clone, Copy, Debug)]
struct CaptionBox {
    line: usize,
    kind: Kind,
    bbox: BBox,
}

/// The smallest box holding `a` and `b`.
fn union(a: BBox, b: BBox) -> BBox {
    BBox {
        x0: a.x0.min(b.x0),
        y0: a.y0.min(b.y0),
        x1: a.x1.max(b.x1),
        y1: a.y1.max(b.y1),
    }
}

/// Area of an ordered box.
fn area(b: BBox) -> f32 {
    (b.x1 - b.x0).max(0.0) * (b.y1 - b.y0).max(0.0)
}

/// Area of the intersection of two ordered boxes.
fn overlap_area(a: BBox, b: BBox) -> f32 {
    let w = (a.x1.min(b.x1) - a.x0.max(b.x0)).max(0.0);
    let h = (a.y1.min(b.y1) - a.y0.max(b.y0)).max(0.0);
    w * h
}

/// The x ranges of two ordered boxes overlap.
fn x_overlap(a: BBox, b: BBox) -> bool {
    a.x0 < b.x1 && b.x0 < a.x1
}

/// Vertical distance between two ordered boxes; negative when their y
/// ranges overlap.
fn vertical_distance(a: BBox, b: BBox) -> f32 {
    (a.y0 - b.y1).max(b.y0 - a.y1)
}

/// Vertical centre of an ordered box.
fn centre_y(b: BBox) -> f32 {
    f32::midpoint(b.y0, b.y1)
}

/// At least [`FIGURE_INSIDE`] of the box's area lies inside `frame` (its
/// centre, for a box without area).
fn mostly_inside(b: BBox, frame: BBox) -> bool {
    let a = area(b);
    if a <= 0.0 {
        let cx = f32::midpoint(b.x0, b.x1);
        let cy = centre_y(b);
        return (frame.x0..=frame.x1).contains(&cx) && (frame.y0..=frame.y1).contains(&cy);
    }
    overlap_area(b, frame) >= FIGURE_INSIDE * a
}

/// A thin horizontal box: lower than [`RULE_HEIGHT`] and at least
/// [`RULE_MIN_WIDTH`] wide.
fn is_rule_box(b: BBox) -> bool {
    b.y1 - b.y0 < RULE_HEIGHT && b.x1 - b.x0 >= RULE_MIN_WIDTH
}

/// The page's figure boxes with ordered, finite corners, split into rules
/// (kind `rule`, or any thin horizontal box) and regions (every other
/// kind: `vector`, `raster`, `layout`), each with whether it is `vector`.
/// A region covering at least [`PAGE_BACKGROUND`] of the page is a
/// background and left out.
fn page_figures(page: &PageText) -> (Vec<BBox>, Vec<(BBox, bool)>) {
    let page_area = page.width.max(0.0) * page.height.max(0.0);
    let mut rules: Vec<BBox> = Vec::new();
    let mut regions: Vec<(BBox, bool)> = Vec::new();
    for figure in &page.figures {
        let Some(b) = figure.bbox.and_then(ordered_box) else {
            continue;
        };
        if figure.kind == KIND_RULE || is_rule_box(b) {
            rules.push(b);
            continue;
        }
        if page_area > 0.0 && area(b) >= PAGE_BACKGROUND * page_area {
            continue;
        }
        regions.push((b, figure.kind == KIND_VECTOR));
    }
    (rules, regions)
}

/// Add `n` newly tagged lines to the count of `kind`.
fn count_kind(report: &mut RegionReport, kind: Kind, n: usize) {
    match kind {
        Kind::Figure => report.figure += n,
        Kind::Table => report.table += n,
        Kind::Algorithm => report.algorithm += n,
    }
}

/// The caption blocks of the page's caption starts: the start plus the
/// `caption` lines directly below it in its band (no blank separator, at
/// most [`CAPTION_MAX_LINES`] lines in all). Starts inside a prose
/// paragraph are left out.
fn caption_boxes(
    page: &PageText,
    geometry: &PageGeometry,
    captions: &[(usize, Kind)],
) -> Vec<CaptionBox> {
    let mut boxes: Vec<CaptionBox> = Vec::new();
    for &(k, kind) in captions {
        let Some(b) = finite_box(&page.lines[k]) else {
            continue;
        };
        let band = band_for(geometry, b);
        let entries = band_entries(page, band);
        let Some(pos) = entries.iter().position(|&i| i == k) else {
            continue;
        };
        if inside_prose(page, &entries, pos, geometry.blank) {
            continue;
        }
        let mut bbox = b;
        let mut prev = k;
        for &i in entries.iter().skip(pos + 1).take(CAPTION_MAX_LINES - 1) {
            let line = &page.lines[i];
            if line.role != ROLE_CAPTION
                || caption_kind(line).is_some()
                || gap(page, prev, i) > geometry.blank
            {
                break;
            }
            if let Some(o) = finite_box(line) {
                bbox = union(bbox, o);
            }
            prev = i;
        }
        boxes.push(CaptionBox {
            line: k,
            kind,
            bbox,
        });
    }
    boxes
}

/// The caption kind each region box belongs to: that of the nearest caption
/// block at most [`CAPTION_REACH`] above or below it (x ranges
/// overlapping), else that of a box already assigned at most that far
/// (boxes stacked in one float), else `None`.
fn box_kinds(regions: &[BBox], captions: &[CaptionBox]) -> Vec<Option<Kind>> {
    let mut kinds: Vec<Option<Kind>> = regions
        .iter()
        .map(|&r| {
            let mut best: Option<(f32, Kind)> = None;
            for c in captions {
                if !x_overlap(r, c.bbox) {
                    continue;
                }
                let d = vertical_distance(r, c.bbox);
                if d > CAPTION_REACH {
                    continue;
                }
                if best.is_none_or(|(bd, _)| d < bd) {
                    best = Some((d, c.kind));
                }
            }
            best.map(|(_, kind)| kind)
        })
        .collect();
    // Spread kinds outward one breadth level at a time. A region reached
    // from two chains in the same level takes the kind of the lower-index
    // assigned neighbour, the tie-break of the round-based propagation this
    // replaces; every region is a source exactly once, so the work is
    // bounded by regions squared instead of cubed.
    let mut frontier: Vec<usize> = (0..regions.len()).filter(|&i| kinds[i].is_some()).collect();
    while !frontier.is_empty() {
        let mut next: Vec<usize> = Vec::new();
        for &i in &frontier {
            let kind = kinds[i].expect("frontier regions have a kind");
            for (j, &other) in regions.iter().enumerate() {
                if kinds[j].is_none()
                    && x_overlap(regions[i], other)
                    && vertical_distance(regions[i], other) <= CAPTION_REACH
                {
                    kinds[j] = Some(kind);
                    next.push(j);
                }
            }
        }
        next.sort_unstable();
        frontier = next;
    }
    kinds
}

/// Tag the `body` lines inside figure boxes (see the module
/// documentation): with the role of the caption the box belongs to
/// (`figure` when none), prose-like lines only when the box has a caption
/// (then every inside line but runs of column prose far from the caption
/// in a box that is no framed text box) or is a frame on a page without
/// captions, and in a box covering the whole page only when it has a
/// caption.
fn tag_figure_boxes(
    page: &mut PageText,
    geometry: &PageGeometry,
    captions: &[CaptionBox],
    report: &mut RegionReport,
) {
    let (_, figures) = page_figures(page);
    if figures.is_empty() {
        return;
    }
    let regions: Vec<BBox> = figures.iter().map(|&(b, _)| b).collect();
    let kinds = box_kinds(&regions, captions);
    let mut picked: Vec<(usize, Kind)> = Vec::new();
    for (&(r, vector), &kind) in figures.iter().zip(kinds.iter()) {
        let width = r.x1 - r.x0;
        let height = r.y1 - r.y0;
        if height < FIGURE_MIN_HEIGHT {
            continue;
        }
        let whole = width >= WHOLE_PAGE * page.width && height >= WHOLE_PAGE * page.height;
        if whole && kind.is_none() {
            continue;
        }
        let inside: Vec<usize> = page
            .lines
            .iter()
            .enumerate()
            .filter(|(_, line)| {
                line.role == ROLE_BODY && finite_box(line).is_some_and(|b| mostly_inside(b, r))
            })
            .map(|(k, _)| k)
            .collect();
        let short = inside
            .iter()
            .filter(|&&k| {
                let text = page.lines[k].text.as_str();
                !text.trim().is_empty() && is_fragment(text)
            })
            .count();
        let framed = kind.is_none()
            && captions.is_empty()
            && vector
            && height >= FRAME_MIN_HEIGHT
            && short >= FRAME_MIN_SHORT;
        let text_box = vector && is_text_box(page, geometry, r, &inside, short);
        let column_prose: Vec<usize> = if kind.is_some() && !text_box {
            far_prose_runs(page, &inside, r, captions)
        } else {
            Vec::new()
        };
        for k in inside {
            if kind.is_none() && !framed && is_prose_like(&page.lines[k].text) {
                continue;
            }
            if column_prose.contains(&k) {
                continue;
            }
            picked.push((k, kind.unwrap_or(Kind::Figure)));
        }
    }
    for (k, kind) in picked {
        let n = tag(&mut page.lines[k], kind.role());
        count_kind(report, kind, n);
    }
}

/// Box `r` frames its `inside` lines as a text box: each is inset at least
/// [`TEXT_BOX_INSET`] from the left and right sides of `r` with at least
/// [`TEXT_BOX_INSIDE`] of its box inside `r`, and `r` is no wider than its
/// column ([`TEXT_BOX_COLUMN_WIDTH`] of the page on a two-column page,
/// [`TEXT_BOX_PAGE_WIDTH`] otherwise) or holds at least
/// [`FRAME_MIN_SHORT`] short lines (`short` of them).
fn is_text_box(
    page: &PageText,
    geometry: &PageGeometry,
    r: BBox,
    inside: &[usize],
    short: usize,
) -> bool {
    let inset = inside.iter().all(|&k| {
        finite_box(&page.lines[k]).is_some_and(|b| {
            b.x0 >= r.x0 + TEXT_BOX_INSET
                && b.x1 <= r.x1 - TEXT_BOX_INSET
                && overlap_area(b, r) >= TEXT_BOX_INSIDE * area(b)
        })
    });
    let share = if geometry.two_column {
        TEXT_BOX_COLUMN_WIDTH
    } else {
        TEXT_BOX_PAGE_WIDTH
    };
    inset && (r.x1 - r.x0 <= share * geometry.width || short >= FRAME_MIN_SHORT)
}

/// The lines of `inside` (lines inside box `r`) in runs of at least
/// [`COLUMN_RUN_MIN_LINES`] consecutive prose-like lines of at least
/// [`COLUMN_RUN_WORDS`] words each whose union lies more than
/// [`CAPTION_REACH`] from every caption block overlapping the x range of
/// `r`: column prose that a figure box happens to cover.
fn far_prose_runs(
    page: &PageText,
    inside: &[usize],
    r: BBox,
    captions: &[CaptionBox],
) -> Vec<usize> {
    let mut runs: Vec<Vec<usize>> = Vec::new();
    let mut run: Vec<usize> = Vec::new();
    for &k in inside {
        let text = page.lines[k].text.as_str();
        if word_count(text) >= COLUMN_RUN_WORDS && is_prose_like(text) {
            run.push(k);
        } else if !run.is_empty() {
            runs.push(std::mem::take(&mut run));
        }
    }
    if !run.is_empty() {
        runs.push(run);
    }
    let mut out: Vec<usize> = Vec::new();
    for run in runs {
        if run.len() < COLUMN_RUN_MIN_LINES {
            continue;
        }
        let Some(b) = run
            .iter()
            .filter_map(|&k| finite_box(&page.lines[k]))
            .reduce(union)
        else {
            continue;
        };
        let far = captions
            .iter()
            .filter(|c| x_overlap(r, c.bbox))
            .all(|c| vertical_distance(b, c.bbox) > CAPTION_REACH);
        if far {
            out.extend(run);
        }
    }
    out
}

/// The vertical span between region box `r` and the nearest `Figure`
/// caption block below or above it (x ranges overlapping), at most
/// [`LABEL_ZONE_MAX`] tall, when no prose-like `body` line lies in it.
fn label_zone(page: &PageText, r: BBox, captions: &[CaptionBox]) -> Option<(f32, f32)> {
    let mut best: Option<(f32, f32)> = None;
    for c in captions {
        if c.kind != Kind::Figure || !x_overlap(r, c.bbox) {
            continue;
        }
        let zone = if c.bbox.y1 <= r.y0 {
            (c.bbox.y1, r.y0)
        } else if c.bbox.y0 >= r.y1 {
            (r.y1, c.bbox.y0)
        } else {
            continue;
        };
        let size = zone.1 - zone.0;
        if size > LABEL_ZONE_MAX {
            continue;
        }
        if best.is_none_or(|(lo, hi)| size < hi - lo) {
            best = Some(zone);
        }
    }
    let (lo, hi) = best?;
    let prose = page.lines.iter().any(|line| {
        line.role == ROLE_BODY
            && is_prose_like(&line.text)
            && finite_box(line).is_some_and(|b| {
                let cy = centre_y(b);
                cy > lo && cy < hi && x_overlap(b, r)
            })
    });
    if prose { None } else { Some((lo, hi)) }
}

/// `b` grown by `d` on every side.
fn grown(b: BBox, d: f32) -> BBox {
    BBox {
        x0: b.x0 - d,
        y0: b.y0 - d,
        x1: b.x1 + d,
        y1: b.y1 + d,
    }
}

/// Stacked labels near a figure box: groups of at least
/// [`STACK_MIN_LINES`] single-word `body` lines (not caption starts)
/// linked by lines directly above or below one another (x ranges
/// overlapping, at most one line height apart), with at least one line
/// overlapping a figure's nearby area. Groups are built once from the
/// bounded set of lines inspected by [`tag_label_clusters`] and reused for
/// every figure, so a shorter label ending before the nearby area still
/// counts without repeating the component search for every figure.
fn stacked_word_groups(page: &PageText) -> Vec<Vec<usize>> {
    let cands: Vec<(usize, BBox)> = page
        .lines
        .iter()
        .take(LABEL_MAX_LINES)
        .enumerate()
        .filter(|(_, line)| {
            line.role == ROLE_BODY && word_count(&line.text) == 1 && caption_kind(line).is_none()
        })
        .filter_map(|(k, line)| finite_box(line).map(|b| (k, b)))
        .collect();
    let touches = |a: BBox, b: BBox| -> bool {
        let reach = (a.y1 - a.y0).max(b.y1 - b.y0);
        x_overlap(a, b) && vertical_distance(a, b) <= reach
    };
    let n = cands.len();
    let mut seen: Vec<bool> = vec![false; n];
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut comparisons = 0usize;
    let mut start: usize = 0;
    while start < n {
        if seen[start] {
            start += 1;
            continue;
        }
        seen[start] = true;
        let mut stack: Vec<usize> = vec![start];
        let mut group: Vec<usize> = Vec::new();
        while let Some(a) = stack.pop() {
            group.push(cands[a].0);
            for (b, &(_, other)) in cands.iter().enumerate() {
                comparisons += 1;
                if comparisons > STACK_MAX_COMPARISONS {
                    // Hostile pages can otherwise turn this component walk
                    // into an unbounded all-pairs operation. The ordinary
                    // short-label detector below still handles nearby text.
                    return Vec::new();
                }
                if !seen[b] && touches(cands[a].1, other) {
                    seen[b] = true;
                    stack.push(b);
                }
            }
        }
        if group.len() >= STACK_MIN_LINES {
            groups.push(group);
        }
        start += 1;
    }
    groups
}

/// Tag clusters of graphics labels `figure`: at least
/// [`LABEL_MIN_LINES`] `body` lines of at most 4 words whose boxes overlap
/// a figure box, lie within [`LABEL_NEAR`] of it (not ending a sentence),
/// or lie between it and its `Figure` caption (see [`label_zone`]), and
/// stacked labels near it (see [`stacked_word_groups`]); the count of lines
/// newly tagged.
fn tag_label_clusters(page: &mut PageText, captions: &[CaptionBox]) -> usize {
    let (_, figures) = page_figures(page);
    let stacked = stacked_word_groups(page);
    let mut picked = vec![false; page.lines.len()];
    for &(r, _) in figures
        .iter()
        .filter(|(r, _)| r.x1 - r.x0 >= LABEL_BOX_MIN && r.y1 - r.y0 >= LABEL_BOX_MIN)
        .take(LABEL_MAX_FIGURES)
    {
        let zone = label_zone(page, r, captions);
        let near = grown(r, LABEL_NEAR);
        let cluster: Vec<usize> = page
            .lines
            .iter()
            .take(LABEL_MAX_LINES)
            .enumerate()
            .filter(|(_, line)| {
                line.role == ROLE_BODY
                    && !line.text.trim().is_empty()
                    && word_count(&line.text) <= FRAGMENT_WORDS
                    && !is_numbered_heading(&line.text)
                    && caption_kind(line).is_none()
            })
            .filter_map(|(k, line)| {
                let b = finite_box(line)?;
                let on_box = overlap_area(b, r) > 0.0;
                let close = overlap_area(b, near) > 0.0 && !ends_sentence(&line.text);
                let between = zone.is_some_and(|(lo, hi)| {
                    let cy = centre_y(b);
                    cy > lo && cy < hi && x_overlap(b, r)
                });
                (on_box || close || between).then_some(k)
            })
            .collect();
        if cluster.len() >= LABEL_MIN_LINES {
            for k in cluster {
                picked[k] = true;
            }
        }
        for group in &stacked {
            if group
                .iter()
                .any(|&k| finite_box(&page.lines[k]).is_some_and(|b| overlap_area(b, near) > 0.0))
            {
                for &k in group {
                    picked[k] = true;
                }
            }
        }
    }
    let mut n = 0;
    for (k, selected) in picked.into_iter().enumerate() {
        if selected {
            n += tag(&mut page.lines[k], Kind::Figure.role());
        }
    }
    n
}

/// A line that ends a ruled table's extent: another caption start, a
/// numbered section heading, or a heading, footnote, front-matter or
/// contents line.
fn stops_table(line: &Line) -> bool {
    caption_kind(line).is_some()
        || is_numbered_heading(&line.text)
        || matches!(
            line.role.as_str(),
            "heading" | ROLE_FOOTNOTE | "front" | "toc"
        )
}

/// The `body` lines of a ruled table next to the caption block `cap`
/// (below it with `down`, else above): the first rule at most
/// [`RULE_REACH`] from the caption (x ranges overlapping, no prose-like
/// line between), and the farthest rule of the same width (ends within
/// [`X_TOLERANCE`] plus 1 pt) more than [`CAPTION_REACH`] before the
/// next caption, heading or footnote. Every line whose centre lies between the two rules and whose
/// box lies at least half inside their x range is taken, prose included.
/// Empty when fewer than 2 such rules exist or more than
/// [`PARAGRAPH_TABLE_MAX_LINES`] lines lie between them.
fn ruled_table(page: &PageText, rules: &[BBox], cap: BBox, down: bool) -> Vec<usize> {
    let reach = |r: BBox| -> Option<f32> {
        if !x_overlap(r, cap) {
            return None;
        }
        let d = if down { cap.y0 - r.y1 } else { r.y0 - cap.y1 };
        (-X_TOLERANCE..=RULE_REACH).contains(&d).then_some(d)
    };
    let Some(first) = rules
        .iter()
        .filter_map(|&r| reach(r).map(|d| (d, r)))
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, r)| r)
    else {
        return Vec::new();
    };
    let first_y = centre_y(first);
    let cap_edge = if down { cap.y0 } else { cap.y1 };
    let (gap_lo, gap_hi) = (first_y.min(cap_edge), first_y.max(cap_edge));
    let blocked = page.lines.iter().any(|line| {
        line.role == ROLE_BODY
            && is_prose_like(&line.text)
            && finite_box(line).is_some_and(|b| {
                let cy = centre_y(b);
                cy > gap_lo && cy < gap_hi && x_overlap(b, first)
            })
    });
    if blocked {
        return Vec::new();
    }
    let mut limit = if down {
        f32::NEG_INFINITY
    } else {
        f32::INFINITY
    };
    for line in &page.lines {
        if !stops_table(line) {
            continue;
        }
        let Some(b) = finite_box(line) else {
            continue;
        };
        if !x_overlap(b, first) {
            continue;
        }
        let cy = centre_y(b);
        if down && cy < first_y {
            limit = limit.max(cy);
        } else if !down && cy > first_y {
            limit = limit.min(cy);
        }
    }
    let slack = X_TOLERANCE + 1.0;
    let mut last_y = first_y;
    let mut more = false;
    for &r in rules {
        let same = (r.x0 - first.x0).abs() <= slack && (r.x1 - first.x1).abs() <= slack;
        if !same {
            continue;
        }
        let cy = centre_y(r);
        // A rule this close to the stopper belongs to the next float (the
        // top rule of a ruled algorithm, say).
        let inside = if down {
            cy < first_y && cy > limit + CAPTION_REACH
        } else {
            cy > first_y && cy < limit - CAPTION_REACH
        };
        let further = if down { cy < last_y } else { cy > last_y };
        if inside && further {
            last_y = cy;
            more = true;
        }
    }
    if !more {
        return Vec::new();
    }
    let (lo, hi) = (first_y.min(last_y), first_y.max(last_y));
    let band = Band {
        lo: first.x0 - X_TOLERANCE,
        hi: first.x1 + X_TOLERANCE,
        width: first.x1 - first.x0,
        cross: None,
    };
    let region: Vec<usize> = page
        .lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.role == ROLE_BODY && caption_kind(line).is_none())
        .filter_map(|(k, line)| {
            let b = finite_box(line)?;
            let cy = centre_y(b);
            (cy > lo && cy < hi && band.holds(b)).then_some(k)
        })
        .collect();
    if region.len() > PARAGRAPH_TABLE_MAX_LINES {
        return Vec::new();
    }
    region
}

/// Lines next to the caption at `entries[pos]` (below it with `down`,
/// nearest first), after its `caption` continuation lines: through `body`
/// and already tagged region lines, up to a vertical gap wider than
/// [`TABLE_BREAK`] line heights, another caption start, a numbered section
/// heading or a line of any other role; at most
/// [`PARAGRAPH_TABLE_MAX_LINES`] lines.
fn table_sequence(
    page: &PageText,
    entries: &[usize],
    pos: usize,
    down: bool,
    geometry: &PageGeometry,
) -> Vec<usize> {
    let order: Vec<usize> = if down {
        entries.iter().skip(pos + 1).copied().collect()
    } else {
        entries.iter().take(pos).rev().copied().collect()
    };
    let limit = TABLE_BREAK * geometry.height;
    let mut seq: Vec<usize> = Vec::new();
    let mut prev = entries[pos];
    let mut leading = true;
    for i in order {
        let line = &page.lines[i];
        if line.role == ROLE_FURNITURE {
            continue;
        }
        let space = if down {
            gap(page, prev, i)
        } else {
            gap(page, i, prev)
        };
        if space > limit {
            break;
        }
        if leading && down && line.role == ROLE_CAPTION && caption_kind(line).is_none() {
            prev = i;
            continue;
        }
        leading = false;
        if caption_kind(line).is_some() || is_numbered_heading(&line.text) {
            break;
        }
        let open = matches!(
            line.role.as_str(),
            ROLE_BODY | ROLE_TABLE | ROLE_MATH | "figure" | "algorithm"
        );
        if !open {
            break;
        }
        seq.push(i);
        prev = i;
        if seq.len() >= PARAGRAPH_TABLE_MAX_LINES {
            break;
        }
    }
    seq
}

/// Distinct column starts of `lines`, ascending: left edges shared (within
/// [`X_TOLERANCE`]) by at least 2 lines, each at least
/// [`COLUMN_MIN_GAP`] right of the previous start kept.
fn column_starts(page: &PageText, lines: &[usize]) -> Vec<f32> {
    let mut xs: Vec<f32> = lines
        .iter()
        .filter_map(|&i| finite_box(&page.lines[i]))
        .map(|b| b.x0)
        .collect();
    xs.sort_by(f32::total_cmp);
    let mut clusters: Vec<(f32, usize)> = Vec::new();
    let mut last: Option<f32> = None;
    for x in xs {
        let joins = last.is_some_and(|l| x - l <= X_TOLERANCE);
        if joins && let Some(cluster) = clusters.last_mut() {
            cluster.1 += 1;
        } else {
            clusters.push((x, 1));
        }
        last = Some(x);
    }
    let mut starts: Vec<f32> = Vec::new();
    for (x, n) in clusters {
        if n >= 2 && starts.last().is_none_or(|&s| x - s >= COLUMN_MIN_GAP) {
            starts.push(x);
        }
    }
    starts
}

/// Index of the column start a left edge `x` belongs to: the last start at
/// most [`X_TOLERANCE`] right of it and less than [`COLUMN_MIN_GAP`] left
/// of it.
fn column_of(x: f32, starts: &[f32]) -> Option<usize> {
    starts
        .iter()
        .rposition(|&s| (s - X_TOLERANCE..s + COLUMN_MIN_GAP).contains(&x))
}

/// Rows of `lines` in which lines of at least [`TABLE_MIN_COLUMNS`]
/// distinct columns sit side by side: line centres lying inside the boxes
/// of lines from that many columns, centres closer than
/// [`ROW_TOLERANCE`] counted as one row.
fn full_rows(page: &PageText, lines: &[usize], starts: &[f32]) -> usize {
    let boxed: Vec<(BBox, Option<usize>)> = lines
        .iter()
        .filter_map(|&i| finite_box(&page.lines[i]))
        .map(|b| (b, column_of(b.x0, starts)))
        .collect();
    let mut centres: Vec<f32> = Vec::new();
    for &(b, _) in &boxed {
        let cy = centre_y(b);
        let mut seen: Vec<usize> = Vec::new();
        for &(o, column) in &boxed {
            if let Some(column) = column
                && (o.y0..=o.y1).contains(&cy)
                && !seen.contains(&column)
            {
                seen.push(column);
            }
        }
        if seen.len() >= TABLE_MIN_COLUMNS {
            centres.push(cy);
        }
    }
    centres.sort_by(f32::total_cmp);
    let mut rows: usize = 0;
    let mut last: Option<f32> = None;
    for cy in centres {
        if last.is_none_or(|l| cy - l > ROW_TOLERANCE) {
            rows += 1;
        }
        last = Some(cy);
    }
    rows
}

/// A table with paragraph cells next to the caption at `entries[pos]`
/// (below it with `down`, else above; see [`table_sequence`]): the lines
/// up to the first two consecutive prose or prose-like lines that reach
/// across a column start other than the first (body text resumed), when they hold at least
/// [`TABLE_MIN_COLUMNS`] column starts and at least [`TABLE_MIN_ROWS`]
/// rows with that many columns side by side. Prose cells are taken.
fn column_table(
    page: &PageText,
    entries: &[usize],
    pos: usize,
    down: bool,
    geometry: &PageGeometry,
) -> Vec<usize> {
    let seq = table_sequence(page, entries, pos, down, geometry);
    let columns = column_starts(page, &seq);
    if columns.len() < TABLE_MIN_COLUMNS {
        return Vec::new();
    }
    let crossing = |i: usize| -> bool {
        let line = &page.lines[i];
        (is_prose(&line.text) || is_prose_like(&line.text))
            && finite_box(line).is_some_and(|b| {
                columns
                    .iter()
                    .skip(1)
                    .any(|&c| b.x0 < c - X_TOLERANCE && b.x1 > c + X_TOLERANCE)
            })
    };
    let stop = seq
        .windows(2)
        .position(|w| crossing(w[0]) && crossing(w[1]))
        .unwrap_or(seq.len());
    let block = &seq[..stop];
    let starts = column_starts(page, block);
    if starts.len() < TABLE_MIN_COLUMNS || full_rows(page, block, &starts) < TABLE_MIN_ROWS {
        return Vec::new();
    }
    block.to_vec()
}

/// The lines of a table with paragraph cells next to the table caption
/// `caption`: a ruled table below, then above it, else a column-pattern
/// table below, then above it.
fn paragraph_table(
    page: &PageText,
    geometry: &PageGeometry,
    rules: &[BBox],
    caption: CaptionBox,
) -> Vec<usize> {
    for down in [true, false] {
        let region = ruled_table(page, rules, caption.bbox, down);
        if !region.is_empty() {
            return region;
        }
    }
    let Some(b) = finite_box(&page.lines[caption.line]) else {
        return Vec::new();
    };
    let band = band_for(geometry, b);
    let entries = band_entries(page, band);
    let Some(pos) = entries.iter().position(|&i| i == caption.line) else {
        return Vec::new();
    };
    for down in [true, false] {
        let region = column_table(page, &entries, pos, down, geometry);
        if !region.is_empty() {
            return region;
        }
    }
    Vec::new()
}

/// Tag tables with paragraph cells (see [`paragraph_table`]) `table`, and
/// their caption starts `caption`.
fn tag_paragraph_tables(
    page: &mut PageText,
    geometry: &PageGeometry,
    captions: &[CaptionBox],
    report: &mut RegionReport,
) {
    let (rules, _) = page_figures(page);
    let mut picked: Vec<(usize, Vec<usize>)> = Vec::new();
    for &caption in captions.iter().filter(|c| c.kind == Kind::Table) {
        let region = paragraph_table(page, geometry, &rules, caption);
        if !region.is_empty() {
            picked.push((caption.line, region));
        }
    }
    for (k, region) in picked {
        report.caption += tag(&mut page.lines[k], ROLE_CAPTION);
        for i in region {
            report.table += tag(&mut page.lines[i], ROLE_TABLE);
        }
    }
}

/// A font name of a monospace face (see [`MONOSPACE_MARKERS`]).
fn is_monospace_font(name: &str) -> bool {
    let lower = name.to_lowercase();
    MONOSPACE_MARKERS.iter().any(|m| lower.contains(m))
}

/// At least 80 % of the line's non-blank characters are set in a monospace
/// font.
fn is_code_line(page: &PageText, line: &Line) -> bool {
    let mut total: usize = 0;
    let mut mono: usize = 0;
    for &idx in &line.spans {
        let Some(span) = page.spans.get(idx as usize) else {
            continue;
        };
        let n = non_space_chars(&span.text);
        total += n;
        if span.font.as_deref().is_some_and(is_monospace_font) {
            mono += n;
        }
    }
    total > 0 && mono * 10 >= total * CODE_SHARE_TENTHS
}

/// Tag runs of at least [`CODE_MIN_LINES`] consecutive `body` lines set in
/// a monospace font (furniture skipped) `code`; the count of lines newly
/// tagged.
fn tag_code(page: &mut PageText) -> usize {
    let flags: Vec<bool> = page
        .lines
        .iter()
        .map(|line| line.role == ROLE_BODY && is_code_line(page, line))
        .collect();
    let mut picked: Vec<usize> = Vec::new();
    let mut run: Vec<usize> = Vec::new();
    for (k, line) in page.lines.iter().enumerate() {
        if line.role == ROLE_FURNITURE {
            continue;
        }
        if flags[k] {
            run.push(k);
            continue;
        }
        if run.len() >= CODE_MIN_LINES {
            picked.extend_from_slice(&run);
        }
        run.clear();
    }
    if run.len() >= CODE_MIN_LINES {
        picked.extend_from_slice(&run);
    }
    let mut n = 0;
    for k in picked {
        n += tag(&mut page.lines[k], ROLE_CODE);
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::Figure;

    const SIZE: f32 = 10.0;

    #[test]
    fn competing_caption_chains_keep_the_lowest_index_tie_break() {
        let stack = |y0: f32| BBox {
            x0: 0.0,
            y0,
            x1: 10.0,
            y1: y0 + 10.0,
        };
        // Index order: A (figure end), B (table end), Y (next to B),
        // X (next to A), Z (between X and Y, reached by both in one level).
        let regions = [
            stack(200.0),
            stack(40.0),
            stack(80.0),
            stack(160.0),
            stack(120.0),
        ];
        let captions = [
            CaptionBox {
                line: 0,
                kind: Kind::Figure,
                bbox: stack(225.0),
            },
            CaptionBox {
                line: 1,
                kind: Kind::Table,
                bbox: stack(5.0),
            },
        ];

        assert_eq!(
            box_kinds(&regions, &captions),
            [
                Some(Kind::Figure),
                Some(Kind::Table),
                Some(Kind::Table),
                Some(Kind::Figure),
                Some(Kind::Table),
            ]
        );
    }

    #[test]
    fn caption_kind_propagates_along_a_long_box_chain() {
        let regions: Vec<BBox> = (0..500)
            .map(|i| BBox {
                x0: 0.0,
                y0: -(i as f32) * CAPTION_REACH,
                x1: 10.0,
                y1: 10.0 - (i as f32) * CAPTION_REACH,
            })
            .collect();
        let captions = [CaptionBox {
            line: 0,
            kind: Kind::Figure,
            bbox: BBox {
                x0: 0.0,
                y0: 20.0,
                x1: 10.0,
                y1: 30.0,
            },
        }];

        assert_eq!(
            box_kinds(&regions, &captions),
            vec![Some(Kind::Figure); regions.len()]
        );
    }

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
    fn a_short_paragraph_tail_above_diagram_labels_stays_body() {
        let mut page = figure_page();
        page.lines[5].text = "and communication between the devices typically happens".to_string();
        // Normal paragraph leading above the tail, followed closely by a
        // smaller diagram gap: the tail must not join the figure's labels.
        let first_label = page.lines[6].bbox.as_mut().unwrap();
        first_label.y0 += 4.0;
        first_label.y1 += 4.0;
        page.lines
            .insert(6, line("via the Host memory bus.", 54.0, 628.0, 0));
        let mut pages = vec![page_of(page.lines)];
        let original_text = pages[0].text.clone();

        let report = tag_regions(&mut pages);

        assert_eq!(role_of(&pages[0], "via the Host memory bus."), ROLE_BODY);
        for text in [
            "Observation sequence",
            "Robot State",
            "Noise Prediction",
            "auxiliary patches",
            "Epoch 50",
        ] {
            assert_eq!(role_of(&pages[0], text), "figure", "{text}");
        }
        assert_eq!(report.figure, 5);
        assert_eq!(pages[0].text, original_text);
    }

    #[test]
    fn short_diagram_labels_near_prose_still_belong_to_the_figure() {
        for variant in 0..3 {
            let mut page = figure_page();
            page.lines[5].text = if variant == 0 {
                "and communication between the devices proceeds as follows:".to_string()
            } else {
                "and communication between the devices typically happens".to_string()
            };
            let first_label = page.lines[6].bbox.as_mut().unwrap();
            first_label.y0 += 4.0;
            first_label.y1 += 4.0;
            let mut label = line("data output.", 54.0, 628.0, 0);
            let bounds = label.bbox.as_mut().unwrap();
            if variant == 1 {
                bounds.x0 += 16.0;
                bounds.x1 += 16.0;
            } else if variant == 2 {
                bounds.y1 = bounds.y0 + 6.0;
            }
            page.lines.insert(6, label);
            let mut pages = vec![page_of(page.lines)];

            let report = tag_regions(&mut pages);

            assert_eq!(role_of(&pages[0], "data output."), "figure", "{variant}");
            assert_eq!(report.figure, 6, "{variant}");
        }
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
    fn excessive_caption_candidates_skip_caption_based_tagging() {
        let lines = (0..=CAPTION_CANDIDATE_MAX)
            .map(|k| line("Algorithm 1", 72.0, 760.0 - k as f32 * 10.0, k as u32))
            .collect();
        let mut pages = vec![page_of(lines)];

        assert_eq!(tag_regions(&mut pages), RegionReport::default());
        assert!(pages[0].lines.iter().all(|line| line.role == ROLE_BODY));
        assert!(pages[0].warnings.contains(&format!(
            "resource_limit: {WARNING_PREFIX}more than {CAPTION_CANDIDATE_MAX} caption candidates; caption-based tagging skipped"
        )));

        let first = pages.clone();
        assert_eq!(tag_regions(&mut pages), RegionReport::default());
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

    /// A figure record of `kind` with box (`x0`, `y0`, `x1`, `y1`).
    fn figure(index: u32, kind: &str, x0: f32, y0: f32, x1: f32, y1: f32) -> Figure {
        Figure {
            index,
            bbox: Some(BBox { x0, y0, x1, y1 }),
            kind: kind.to_string(),
            mime: None,
            width_px: None,
            height_px: None,
            sha256: None,
            file: None,
            caption: None,
        }
    }

    const PROMPT_TITLE: &str = "System prompt";
    const PROMPT_LINES: [&str; 4] = [
        "You are a helpful assistant who answers the question below",
        "and you keep every answer short and polite for the user",
        "Client: I have been feeling a bit down lately because of",
        "the thoughts that my family is disappointed in me again",
    ];

    /// Single-column page: prose, a framed box (a `vector` figure 75 pt
    /// tall) holding a title and four prose-like prompt lines, with a
    /// `Figure 7` caption 12 pt below it when `captioned_box`, then prose.
    fn prompt_page(captioned_box: bool, frame: Figure) -> PageText {
        let mut lines: Vec<Line> = Vec::new();
        let mut baseline = 720.0;
        for text in LEFT_PROSE {
            lines.push(line(text, 72.0, baseline, 0));
            baseline -= 12.0;
        }
        lines.push(line(PROMPT_TITLE, 90.0, 612.0, 1));
        let mut baseline = 600.0;
        for text in PROMPT_LINES {
            lines.push(line(text, 90.0, baseline, 1));
            baseline -= 12.0;
        }
        if captioned_box {
            lines.push(captioned("Figure 7: Prompt template.", 72.0, 530.0, 2));
        }
        let mut baseline = 500.0;
        for text in AFTER_PROSE {
            lines.push(line(text, 72.0, baseline, 3));
            baseline -= 12.0;
        }
        let mut page = page_of(lines);
        page.figures.push(frame);
        page
    }

    fn prompt_frame() -> Figure {
        figure(0, "vector", 80.0, 550.0, 560.0, 625.0)
    }

    #[test]
    fn a_framed_prose_box_above_a_figure_caption_is_figure_text() {
        for text in PROMPT_LINES {
            assert!(is_prose_like(text), "{text}");
        }
        let mut pages = vec![prompt_page(true, prompt_frame())];
        let report = tag_regions(&mut pages);
        let page = &pages[0];
        assert_eq!(role_of(page, PROMPT_TITLE), "figure");
        for text in PROMPT_LINES {
            assert_eq!(role_of(page, text), "figure", "{text}");
        }
        for text in LEFT_PROSE.iter().chain(AFTER_PROSE.iter()) {
            assert_eq!(role_of(page, text), "body", "{text}");
        }
        assert_eq!(role_of(page, "Figure 7: Prompt template."), "caption");
        assert_eq!(report.figure, 5);
        assert_eq!(report.table, 0);
    }

    #[test]
    fn a_box_without_a_caption_keeps_its_prose_and_a_whole_page_box_is_unused() {
        let mut pages = vec![prompt_page(false, prompt_frame())];
        let report = tag_regions(&mut pages);
        let page = &pages[0];
        assert_eq!(role_of(page, PROMPT_TITLE), "figure");
        for text in PROMPT_LINES {
            assert_eq!(role_of(page, text), "body", "{text}");
        }
        assert_eq!(report.figure, 1);

        // 93 % of the width and 91 % of the height, 85 % of the area: the
        // whole page, used only with a caption.
        let whole = figure(0, "vector", 20.0, 40.0, 592.0, 760.0);
        let mut pages = vec![prompt_page(false, whole)];
        assert_eq!(tag_regions(&mut pages), RegionReport::default());
        assert!(pages[0].lines.iter().all(|l| l.role == "body"));

        // A scanned page's full-page raster is never used, caption or not.
        let scan = figure(0, "raster", 0.0, 0.0, 612.0, 792.0);
        let mut pages = vec![prompt_page(true, scan)];
        assert_eq!(tag_regions(&mut pages), RegionReport::default());
        assert_eq!(role_of(&pages[0], PROMPT_TITLE), "body");
    }

    #[test]
    fn a_raster_figure_with_no_lines_inside_tags_nothing() {
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
        let mut page = page_of(lines);
        page.figures
            .push(figure(0, "raster", 72.0, 470.0, 540.0, 630.0));
        let mut pages = vec![page];
        assert_eq!(tag_regions(&mut pages), RegionReport::default());
        assert!(pages[0].warnings.is_empty());
    }

    const WIDE_ROWS: [&str; 4] = [
        "Alpha Beta Gamma Delta Epsilon Zeta",
        "Eta Theta Iota Kappa Lambda Mu",
        "Nu Xi Omicron Pi Rho Sigma",
        "Tau Upsilon Phi Chi Psi Omega",
    ];

    #[test]
    fn graphics_labels_on_a_figure_box_and_above_its_caption_are_figure_text() {
        let mut lines: Vec<Line> = Vec::new();
        let mut baseline = 720.0;
        for text in LEFT_PROSE {
            lines.push(line(text, 72.0, baseline, 0));
            baseline -= 12.0;
        }
        // Title-case rows beside the figure (not fragments) make the walk
        // above the caption fail, so only the label cluster tags.
        for (k, text) in WIDE_ROWS.iter().enumerate() {
            lines.push(line(text, 72.0, 590.0 - 15.0 * k as f32, 1));
        }
        let labels = [
            ("Encoder", 310.0, 585.0),
            ("Decoder", 450.0, 585.0),
            ("(a) Input", 320.0, 545.0),
            ("(b) Output", 450.0, 545.0),
        ];
        for (text, x0, baseline) in labels {
            lines.push(line(text, x0, baseline, 2));
        }
        lines.push(captioned("Figure 2: Model.", 300.0, 520.0, 3));
        let mut baseline = 490.0;
        for text in AFTER_PROSE {
            lines.push(line(text, 72.0, baseline, 4));
            baseline -= 12.0;
        }
        let mut page = page_of(lines);
        page.figures
            .push(figure(0, "vector", 300.0, 560.0, 550.0, 600.0));
        let mut pages = vec![page];
        let report = tag_regions(&mut pages);
        let page = &pages[0];
        for (text, _, _) in labels {
            assert_eq!(role_of(page, text), "figure", "{text}");
        }
        for text in WIDE_ROWS
            .iter()
            .chain(LEFT_PROSE.iter())
            .chain(AFTER_PROSE.iter())
        {
            assert_eq!(role_of(page, text), "body", "{text}");
        }
        assert_eq!(report.figure, 4);
    }

    /// Cells of a three-column table with paragraph cells: (text, x0,
    /// baseline).
    const PARAGRAPH_CELLS: [(&str, f32, f32); 13] = [
        ("GPT-4o", 72.0, 610.0),
        ("generate a detailed review for the", 160.0, 610.0),
        ("the ornament is a lovely gift and", 360.0, 610.0),
        ("product with the following short", 160.0, 598.0),
        ("arrives well packed for the season", 360.0, 598.0),
        ("description of the ornament", 160.0, 586.0),
        ("Claude", 72.0, 570.0),
        ("generate an abstract for the given", 160.0, 570.0),
        ("in this paper we propose a novel", 360.0, 570.0),
        ("title about job shop scheduling", 160.0, 558.0),
        ("teaching learning based method for", 360.0, 558.0),
        ("the flexible job shop problem", 360.0, 546.0),
        ("with fuzzy processing times", 160.0, 546.0),
    ];

    /// Prose, a `Table 3` caption, the [`PARAGRAPH_CELLS`] and then
    /// `after` at x = 72 from baseline 520 down.
    fn paragraph_table_page(after: &[&str]) -> PageText {
        let mut lines: Vec<Line> = Vec::new();
        let mut baseline = 720.0;
        for text in LEFT_PROSE {
            lines.push(line(text, 72.0, baseline, 0));
            baseline -= 12.0;
        }
        lines.push(captioned("Table 3: Generated examples.", 72.0, 630.0, 1));
        for (text, x0, baseline) in PARAGRAPH_CELLS {
            lines.push(line(text, x0, baseline, 2));
        }
        let mut baseline = 520.0;
        for text in after {
            lines.push(line(text, 72.0, baseline, 3));
            baseline -= 12.0;
        }
        page_of(lines)
    }

    #[test]
    fn body_text_after_a_paragraph_table_is_not_table_text() {
        // Citation-heavy prose: prose-like, but no two neighbouring lines
        // pass the lowercase-majority prose test.
        let after = &COLUMN_PROSE[..8];
        let mut pages = vec![paragraph_table_page(after)];
        let report = tag_regions(&mut pages);
        let page = &pages[0];
        for (text, _, _) in PARAGRAPH_CELLS {
            assert_eq!(role_of(page, text), "table", "{text}");
        }
        for text in after {
            assert_eq!(role_of(page, text), "body", "{text}");
        }
        assert_eq!(report.table, PARAGRAPH_CELLS.len());
    }

    #[test]
    fn a_table_with_paragraph_cells_in_three_columns_is_table_text() {
        let mut lines: Vec<Line> = Vec::new();
        let mut baseline = 720.0;
        for text in LEFT_PROSE {
            lines.push(line(text, 72.0, baseline, 0));
            baseline -= 12.0;
        }
        lines.push(captioned("Table 3: Generated examples.", 72.0, 630.0, 1));
        for (text, x0, baseline) in PARAGRAPH_CELLS {
            lines.push(line(text, x0, baseline, 2));
        }
        let mut baseline = 520.0;
        for text in AFTER_PROSE {
            lines.push(line(text, 72.0, baseline, 3));
            baseline -= 12.0;
        }
        let mut pages = vec![page_of(lines)];
        let report = tag_regions(&mut pages);
        let page = &pages[0];
        for (text, _, _) in PARAGRAPH_CELLS {
            assert_eq!(role_of(page, text), "table", "{text}");
        }
        for text in LEFT_PROSE.iter().chain(AFTER_PROSE.iter()) {
            assert_eq!(role_of(page, text), "body", "{text}");
        }
        assert_eq!(report.table, PARAGRAPH_CELLS.len());
    }

    #[test]
    fn two_prose_columns_under_a_full_width_table_caption_are_no_table() {
        let caption =
            "Table 4: Agreement between simulated and human participants across twelve studies";
        let mut lines: Vec<Line> = vec![captioned(caption, 54.0, 700.0, 0)];
        let mut baseline = 686.0;
        for k in 0..20 {
            let x0 = if k % 5 == 2 { 80.0 } else { 54.0 };
            let text = COLUMN_PROSE[k % COLUMN_PROSE.len()];
            lines.push(line(text, x0, baseline, 1));
            baseline -= 12.0;
        }
        prose_column(&mut lines, 312.0, 686.0, 20, 2);
        let mut pages = vec![page_of(lines)];
        let report = tag_regions(&mut pages);
        assert_eq!(report, RegionReport::default());
        only_caption_tagged(&pages[0], caption);
    }

    const RULED_CELLS: [(&str, f32, f32); 6] = [
        ("Prompt", 80.0, 600.0),
        (
            "generate a detailed review for the product with the",
            160.0,
            600.0,
        ),
        (
            "following description and keep it short please",
            160.0,
            588.0,
        ),
        ("Response", 80.0, 560.0),
        (
            "the ornament is a lovely gift and arrives well packed",
            160.0,
            560.0,
        ),
        (
            "for the holiday season with a ribbon on the top",
            160.0,
            548.0,
        ),
    ];

    #[test]
    fn prose_cells_between_table_rules_are_table_text() {
        let mut lines: Vec<Line> = Vec::new();
        let mut baseline = 720.0;
        for text in LEFT_PROSE {
            lines.push(line(text, 72.0, baseline, 0));
            baseline -= 12.0;
        }
        lines.push(captioned("Table 5: Example prompts.", 72.0, 630.0, 1));
        for (text, x0, baseline) in RULED_CELLS {
            lines.push(line(text, x0, baseline, 2));
        }
        let mut baseline = 500.0;
        for text in AFTER_PROSE {
            lines.push(line(text, 72.0, baseline, 3));
            baseline -= 12.0;
        }
        let mut page = page_of(lines);
        page.figures = vec![
            figure(0, "rule", 72.0, 615.0, 540.0, 615.5),
            figure(1, "rule", 72.0, 575.0, 540.0, 575.5),
            figure(2, "rule", 72.0, 525.0, 540.0, 525.5),
            // A footnote rule: a different width, not part of the table.
            figure(3, "rule", 72.0, 100.0, 200.0, 100.4),
        ];
        let mut pages = vec![page];
        let report = tag_regions(&mut pages);
        let page = &pages[0];
        for (text, _, _) in RULED_CELLS {
            assert_eq!(role_of(page, text), "table", "{text}");
        }
        for text in LEFT_PROSE.iter().chain(AFTER_PROSE.iter()) {
            assert_eq!(role_of(page, text), "body", "{text}");
        }
        assert_eq!(report.table, RULED_CELLS.len());
        // A single rule is not a table.
        let mut lone = pages[0].clone();
        lone.warnings.clear();
        for l in &mut lone.lines {
            if l.role == "table" {
                l.role = "body".to_string();
            }
        }
        lone.figures.truncate(1);
        let mut pages = vec![lone];
        assert_eq!(tag_regions(&mut pages).table, 0);
    }

    #[test]
    fn math_lines_allow_one_long_word() {
        let two_long = "\u{1d453}(\u{1d465}) = \u{1d454}(\u{1d466}) + \u{1d462}(\u{1d467}) \
                        \u{2212} \u{1d458}(\u{1d464}) when \u{1d465} \u{2208} \u{1d44b} holds";
        assert!(!is_math_line(two_long));
        let short_words = "\u{1d453}(\u{1d465}) = \u{1d454}(\u{1d466}) + \u{1d462}(\u{1d467}) \
                           \u{2212} \u{1d458}(\u{1d464}) for all \u{1d465} \u{2208} \u{1d44b}";
        assert!(is_math_line(short_words));
        let one_long = "\u{1d453}(\u{1d465}) = \u{1d454}(\u{1d466}) + \u{1d462}(\u{1d467}) \
                        \u{2212} \u{1d458}(\u{1d464}) when \u{1d465} \u{2208} \u{1d44b}";
        assert!(is_math_line(one_long));
        assert_eq!(ordinary_words("when"), (1, 4, 1));
        assert_eq!(ordinary_words("for"), (1, 3, 0));
        assert_eq!(ordinary_words("log"), (0, 0, 0));
    }

    /// Adds a 10 pt line at `baseline` made of one span per piece, each
    /// piece in its font.
    fn push_fonted(page: &mut PageText, pieces: &[(&str, &str)], baseline: f32) {
        let mut x0 = 72.0;
        let mut spans: Vec<u32> = Vec::new();
        let mut text = String::new();
        for (piece, font) in pieces {
            let seq = u32::try_from(page.spans.len()).unwrap();
            let width = 5.0 * piece.chars().count() as f32;
            page.spans.push(crate::schema::Span {
                text: (*piece).to_string(),
                bbox: Some(BBox {
                    x0,
                    y0: baseline - 2.0,
                    x1: x0 + width,
                    y1: baseline + 8.0,
                }),
                font: Some((*font).to_string()),
                size: Some(10.0),
                seq,
            });
            spans.push(seq);
            if !text.is_empty() {
                text.push(' ');
            }
            text.push_str(piece);
            x0 += width + 5.0;
        }
        let right = x0 - 5.0;
        page.lines.push(Line {
            text,
            bbox: Some(BBox {
                x0: 72.0,
                y0: baseline - 2.0,
                x1: right,
                y1: baseline + 8.0,
            }),
            spans,
            ..Line::default()
        });
    }

    #[test]
    fn three_consecutive_monospace_lines_are_code() {
        let serif = "Times-Roman";
        let mut page = PageText::new(1, 612.0, 792.0, 0);
        let mut baseline = 720.0;
        for text in LEFT_PROSE.iter().take(3) {
            push_fonted(&mut page, &[(*text, serif)], baseline);
            baseline -= 12.0;
        }
        let code = ["def main(args):", "result = run(args)", "print(result)"];
        for text in code {
            push_fonted(&mut page, &[(text, "ABCDEF+CMTT10")], baseline);
            baseline -= 12.0;
        }
        push_fonted(&mut page, &[(AFTER_PROSE[0], serif)], baseline);
        baseline -= 12.0;
        // Two monospace lines around a mixed line: no run of three.
        push_fonted(
            &mut page,
            &[("x = load()", "Inconsolata-Regular")],
            baseline,
        );
        baseline -= 12.0;
        let mixed = [
            ("call", "Inconsolata"),
            ("the loader once per batch", serif),
        ];
        push_fonted(&mut page, &mixed, baseline);
        baseline -= 12.0;
        push_fonted(&mut page, &[("y = save(x)", "Inconsolata")], baseline);
        page.text = page
            .lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<&str>>()
            .join("\n");
        let mut pages = vec![page];
        let report = tag_regions(&mut pages);
        let page = &pages[0];
        for text in code {
            assert_eq!(role_of(page, text), "code", "{text}");
        }
        for text in [
            "x = load()",
            "call the loader once per batch",
            "y = save(x)",
        ] {
            assert_ne!(role_of(page, text), "code", "{text}");
        }
        assert_eq!(report.code, 3);
        assert!(
            page.warnings
                .contains(&"regions: code text 3 lines".to_string())
        );
        assert!(is_monospace_font("LMMono10-Regular"));
        assert!(is_monospace_font("NimbusMonoPS-Regular"));
        assert!(is_monospace_font("Courier-Bold"));
        assert!(!is_monospace_font("CMR10"));
    }

    const CAPTION_START: &str = "Figure 3: Accuracy per seed over the training run, for each of";
    const CAPTION_MORE: [&str; 3] = [
        "the five seeds that we report in the main text of this paper.",
        "The shaded band shows one standard deviation around the mean and the",
        "dashed line marks the human baseline from the earlier study.",
    ];

    /// Prose, a tagged caption start at baseline 620, `more` below it at
    /// 12 pt leading, then `after` (capitalised prose, no blank separator)
    /// and [`AFTER_PROSE`] after a blank separator.
    fn caption_page(more: &[&str], after: &[String]) -> PageText {
        let mut lines: Vec<Line> = Vec::new();
        let mut baseline = 720.0;
        for text in LEFT_PROSE {
            lines.push(line(text, 72.0, baseline, 0));
            baseline -= 12.0;
        }
        lines.push(captioned(CAPTION_START, 72.0, 620.0, 1));
        let mut baseline = 620.0;
        for text in more {
            baseline -= 12.0;
            lines.push(line(text, 72.0, baseline, 1));
        }
        for text in after {
            baseline -= 12.0;
            lines.push(line(text, 72.0, baseline, 1));
        }
        baseline -= 36.0;
        for text in AFTER_PROSE {
            lines.push(line(text, 72.0, baseline, 2));
            baseline -= 12.0;
        }
        page_of(lines)
    }

    #[test]
    fn a_caption_continues_through_sentence_ends_in_a_closed_block() {
        let mut pages = vec![caption_page(&CAPTION_MORE, &[])];
        let report = tag_regions(&mut pages);
        let page = &pages[0];
        for text in CAPTION_MORE {
            assert_eq!(role_of(page, text), "caption", "{text}");
        }
        for text in LEFT_PROSE.iter().chain(AFTER_PROSE.iter()) {
            assert_eq!(role_of(page, text), "body", "{text}");
        }
        assert_eq!(report.caption, 3);
        assert_eq!(report.figure, 0);

        // Capitalised prose glued below (the block runs past the cap): the
        // caption ends at its first sentence end.
        let glued: Vec<String> = (0..16)
            .map(|k| format!("Line {k} of the body text runs on with more words here"))
            .collect();
        let mut pages = vec![caption_page(&CAPTION_MORE[..2], &glued)];
        let report = tag_regions(&mut pages);
        let page = &pages[0];
        assert_eq!(role_of(page, CAPTION_MORE[0]), "caption");
        assert_eq!(role_of(page, CAPTION_MORE[1]), "body");
        for text in &glued {
            assert_eq!(role_of(page, text), "body", "{text}");
        }
        assert_eq!(report.caption, 1);
    }

    #[test]
    fn a_caption_stops_at_a_line_in_another_font_size() {
        let mut page = PageText::new(1, 612.0, 792.0, 0);
        let mut baseline = 720.0;
        for text in LEFT_PROSE {
            push_sized(&mut page, text, baseline, 10.0);
            baseline -= 12.0;
        }
        let start = "Figure 3: Accuracy per seed.";
        let more = "the shaded band shows one standard deviation around the mean.";
        let body = "The next paragraph is set in the body size and it starts right here.";
        push_sized(&mut page, start, 620.0, 9.0);
        push_sized(&mut page, more, 608.0, 9.0);
        push_sized(&mut page, body, 596.0, 10.0);
        let mut baseline = 560.0;
        for text in AFTER_PROSE {
            push_sized(&mut page, text, baseline, 10.0);
            baseline -= 12.0;
        }
        page.text = page
            .lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<&str>>()
            .join("\n");
        let mut pages = vec![page];
        let report = tag_regions(&mut pages);
        assert_eq!(role_of(&pages[0], more), "caption");
        assert_eq!(role_of(&pages[0], body), "body");
        assert_eq!(report.caption, 1);
    }

    #[test]
    fn letter_numbered_and_continued_labels_are_caption_starts() {
        let body = |text: &str| line(text, 72.0, 500.0, 0);
        for (text, kind) in [
            ("Figure A.1: Rubbermind deal closure", Kind::Figure),
            ("Table S2: Results", Kind::Table),
            ("Fig. B3. Overview", Kind::Figure),
            ("Table II: Results", Kind::Table),
            ("Figure II: Overview", Kind::Figure),
            ("Figure 2a: Panel", Kind::Figure),
            ("Table 1 continued from previous page", Kind::Table),
            ("Table 3 (continued)", Kind::Table),
        ] {
            assert_eq!(caption_kind(&body(text)), Some(kind), "{text}");
        }
        for text in [
            "Figure A shows the model",
            "Table S2 lists the",
            "Figure 2a shows the results of the model",
            "Figure 1 continued",
        ] {
            assert_eq!(caption_kind(&body(text)), None, "{text}");
        }
        for core in ["3", "3.1", "A.1", "A.12", "S2", "S1.2", "B3", "II", "2a"] {
            assert!(is_label_number(core), "{core}");
        }
        for core in ["", "A", "ab", "2ab", "a1", "Ia"] {
            assert!(!is_label_number(core), "{core}");
        }
    }

    const CONTINUED_LABEL: &str = "Table 1 continued from previous page";
    const CONTINUED_PARAGRAPH: [&str; 2] = [
        "the paragraph after the table discusses the review results in more detail",
        "and the next lines of this page continue the running text of the review",
    ];

    /// A `longtable` continuation page: a header cell above the
    /// [`CONTINUED_LABEL`], 45 rows of three cells, a two-line paragraph
    /// and a short line after it; `prose_above` puts a prose line on top.
    fn continued_page(prose_above: bool) -> (PageText, Vec<String>) {
        let mut lines: Vec<Line> = Vec::new();
        if prose_above {
            lines.push(line(LEFT_PROSE[0], 72.0, 776.0, 0));
        }
        lines.push(line("Company", 72.0, 760.0, 0));
        lines.push(line(CONTINUED_LABEL, 72.0, 740.0, 1));
        let mut cells: Vec<String> = Vec::new();
        let mut baseline = 720.0;
        for k in 0..45 {
            for (c, x0) in [(0, 72.0), (1, 200.0), (2, 330.0)] {
                let text = format!("cell {k}.{c} value");
                lines.push(line(&text, x0, baseline, 2 + c));
                cells.push(text);
            }
            baseline -= 12.0;
        }
        for text in CONTINUED_PARAGRAPH {
            lines.push(line(text, 72.0, baseline, 5));
            baseline -= 12.0;
        }
        lines.push(line("Other cell text", 72.0, baseline - 4.0, 6));
        (page_of(lines), cells)
    }

    #[test]
    fn a_continued_table_page_is_table_text_through_its_last_row() {
        let (page, cells) = continued_page(false);
        let mut pages = vec![page];
        let report = tag_regions(&mut pages);
        let page = &pages[0];
        assert_eq!(role_of(page, CONTINUED_LABEL), "caption");
        assert_eq!(role_of(page, "Company"), "table");
        for text in &cells {
            assert_eq!(role_of(page, text), "table", "{text}");
        }
        for text in CONTINUED_PARAGRAPH {
            assert_eq!(role_of(page, text), "body", "{text}");
        }
        assert_eq!(role_of(page, "Other cell text"), "body");
        assert_eq!(report.table, cells.len() + 1);
        assert_eq!(report.caption, 1);

        let (mut page, _) = continued_page(true);
        assert_eq!(tag_continued_table(&mut page), (0, 0));
    }

    #[test]
    fn a_sideways_page_after_a_sideways_table_page_continues_it() {
        let mut pages = vec![sideways_page(true), sideways_page(false)];
        let report = tag_regions(&mut pages);
        let next = &pages[1];
        for text in SIDEWAYS_CELLS {
            assert_eq!(role_of(next, text), "table", "{text}");
        }
        assert_eq!(role_of(next, SIDEWAYS_PROSE_CELL), "body");
        assert_eq!(role_of(next, SIDEWAYS_PARAGRAPH), "body");
        assert_eq!(role_of(next, SIDEWAYS_LATE_CELL), "body");
        assert_eq!(report.table, 2 * SIDEWAYS_CELLS.len());

        // Other column starts: no continuation.
        let mut moved = sideways_page(false);
        for l in &mut moved.lines {
            if let Some(b) = l.bbox.as_mut() {
                b.y0 += 50.0;
                b.y1 += 50.0;
            }
        }
        let mut pages = vec![sideways_page(true), moved];
        let report = tag_regions(&mut pages);
        assert_eq!(report.table, SIDEWAYS_CELLS.len());
        assert!(pages[1].lines.iter().all(|l| l.role == "body"));

        // A sideways page without table lines carries nothing.
        let mut pages = vec![sideways_page(false), sideways_page(false)];
        assert_eq!(tag_regions(&mut pages), RegionReport::default());
        assert!(same_columns(&[100.0, 280.0, 460.0], &[102.0, 279.0, 600.0]));
        assert!(!same_columns(
            &[100.0, 280.0, 460.0],
            &[150.0, 330.0, 510.0]
        ));
    }

    /// Prose above and below a `kind` figure box (100..500 x 400..560).
    fn label_page(kind: &str, labels: &[(&str, f32, f32)]) -> PageText {
        let mut lines: Vec<Line> = Vec::new();
        let mut baseline = 720.0;
        for text in LEFT_PROSE {
            lines.push(line(text, 72.0, baseline, 0));
            baseline -= 12.0;
        }
        for (text, x0, baseline) in labels {
            lines.push(line(text, *x0, *baseline, 1));
        }
        let mut baseline = 340.0;
        for text in AFTER_PROSE {
            lines.push(line(text, 72.0, baseline, 2));
            baseline -= 12.0;
        }
        let mut page = page_of(lines);
        page.figures
            .push(figure(0, kind, 100.0, 400.0, 500.0, 560.0));
        page
    }

    #[test]
    fn stacked_and_nearby_labels_of_a_figure_box_are_figure_text() {
        // Three stacked words 10 pt left of the box, and a lone word.
        let stacked = [
            ("Material", 50.0, 500.0),
            ("Category", 50.0, 490.0),
            ("Type", 50.0, 480.0),
            ("Legend", 300.0, 300.0),
        ];
        let mut pages = vec![label_page("vector", &stacked)];
        let report = tag_regions(&mut pages);
        for (text, _, _) in &stacked[..3] {
            assert_eq!(role_of(&pages[0], text), "figure", "{text}");
        }
        assert_eq!(role_of(&pages[0], "Legend"), "body");
        assert_eq!(report.figure, 3);

        // A legend 7 pt under the box, and a paragraph end 8 pt above it.
        let legend = [
            ("Baseline", 110.0, 385.0),
            ("Ours (full)", 200.0, 385.0),
            ("Oracle", 290.0, 385.0),
            ("Random seed", 380.0, 385.0),
            ("the results.", 110.0, 570.0),
        ];
        let mut pages = vec![label_page("vector", &legend)];
        let report = tag_regions(&mut pages);
        for (text, _, _) in &legend[..4] {
            assert_eq!(role_of(&pages[0], text), "figure", "{text}");
        }
        assert_eq!(role_of(&pages[0], "the results."), "body");
        for text in LEFT_PROSE.iter().chain(AFTER_PROSE.iter()) {
            assert_eq!(role_of(&pages[0], text), "body", "{text}");
        }
        assert_eq!(report.figure, 4);
    }

    const FRAME_SHORT: [&str; 3] = ["Input", "Query rewrite", "Answer"];
    const FRAME_PROSE: [&str; 2] = [
        "An example showing the workflow for creating a custom dataset",
        "with two instances and the hints that were written for them",
    ];

    /// A page without captions: prose, a `kind` box (80..560 x 450..600)
    /// holding [`FRAME_SHORT`] and [`FRAME_PROSE`], prose.
    fn frame_page(kind: &str) -> PageText {
        let mut lines: Vec<Line> = Vec::new();
        let mut baseline = 720.0;
        for text in LEFT_PROSE {
            lines.push(line(text, 72.0, baseline, 0));
            baseline -= 12.0;
        }
        let mut baseline = 580.0;
        for text in FRAME_SHORT {
            lines.push(line(text, 90.0, baseline, 1));
            baseline -= 20.0;
        }
        let mut baseline = 510.0;
        for text in FRAME_PROSE {
            lines.push(line(text, 90.0, baseline, 1));
            baseline -= 12.0;
        }
        let mut baseline = 420.0;
        for text in AFTER_PROSE {
            lines.push(line(text, 72.0, baseline, 2));
            baseline -= 12.0;
        }
        let mut page = page_of(lines);
        page.figures
            .push(figure(0, kind, 80.0, 450.0, 560.0, 600.0));
        page
    }

    #[test]
    fn a_tall_vector_frame_without_a_caption_is_figure_text() {
        for text in FRAME_PROSE {
            assert!(is_prose_like(text), "{text}");
        }
        let mut pages = vec![frame_page("vector")];
        let report = tag_regions(&mut pages);
        for text in FRAME_SHORT.iter().chain(FRAME_PROSE.iter()) {
            assert_eq!(role_of(&pages[0], text), "figure", "{text}");
        }
        for text in LEFT_PROSE.iter().chain(AFTER_PROSE.iter()) {
            assert_eq!(role_of(&pages[0], text), "body", "{text}");
        }
        assert_eq!(report.figure, 5);

        // A raster box is no frame: its prose stays body.
        let mut pages = vec![frame_page("raster")];
        let report = tag_regions(&mut pages);
        for text in FRAME_PROSE {
            assert_eq!(role_of(&pages[0], text), "body", "{text}");
        }
        assert_eq!(report.figure, FRAME_SHORT.len());
    }

    const BOX_RUN: [&str; 3] = [
        "we describe the running text of the section that a large figure box happens to cover",
        "and the reader should still see these lines in the body text of the paper here",
        "because they are ordinary prose that runs on across the width of the whole column",
    ];
    const BOX_LINE: &str = "the decoder maps the latent code back to the image";

    /// Twelve prose-like lines of 15 words each for the tall-box test.
    const TALL_LINES: [&str; 12] = [
        "you are a careful assistant and you answer every question with a short kind reply",
        "the user asks about the weather in the city and you give them plain answers",
        "then the user wants to know which coat to wear and you suggest a jacket",
        "the assistant keeps the tone warm and never adds facts the user did not want",
        "when the user thanks you for the help you reply with a friendly closing line",
        "the second turn starts when the user shares a worry about work this long week",
        "you listen to the worry and reflect it back in words the user can recognise",
        "the user says that the manager seems unhappy with the report that was sent today",
        "you ask a gentle question about what the manager said and how it felt then",
        "the user explains that no one gave clear feedback and the silence made it worse",
        "you suggest asking the manager for a short meeting to talk about the report soon",
        "the dialogue ends when the user agrees to write a note and you say goodbye",
    ];

    const BOX_CAPTION: &str = "Figure 3: The encoder and the decoder.";

    /// Single-column page: prose, three long prose lines of the column
    /// (x from 70), a diagram label and one prose-like line inside
    /// `frame`, with a `Figure 3` caption 2 pt below a box ending at y 300
    /// when `captioned_box`, then prose.
    fn box_run_page(captioned_box: bool, frame: Figure) -> PageText {
        let mut lines: Vec<Line> = Vec::new();
        let mut baseline = 760.0;
        for text in LEFT_PROSE {
            lines.push(line(text, 72.0, baseline, 0));
            baseline -= 12.0;
        }
        let mut baseline = 540.0;
        for text in BOX_RUN {
            lines.push(line(text, 70.0, baseline, 1));
            baseline -= 12.0;
        }
        lines.push(line("Encoder", 100.0, 420.0, 2));
        lines.push(line(BOX_LINE, 100.0, 320.0, 3));
        if captioned_box {
            lines.push(captioned(BOX_CAPTION, 72.0, 290.0, 4));
        }
        let mut baseline = 250.0;
        for text in AFTER_PROSE {
            lines.push(line(text, 72.0, baseline, 5));
            baseline -= 12.0;
        }
        let mut page = page_of(lines);
        page.figures.push(frame);
        page
    }

    #[test]
    fn prose_inside_a_figure_box_without_a_caption_stays_body() {
        for text in BOX_RUN {
            assert!(is_prose_like(text), "{text}");
        }
        assert!(is_prose_like(BOX_LINE));
        let frame = figure(0, "vector", 60.0, 300.0, 560.0, 560.0);
        let mut pages = vec![box_run_page(false, frame)];
        let report = tag_regions(&mut pages);
        let page = &pages[0];
        for text in BOX_RUN {
            assert_eq!(role_of(page, text), "body", "{text}");
        }
        assert_eq!(role_of(page, "Encoder"), "figure");
        assert_eq!(role_of(page, BOX_LINE), "body");
        for text in LEFT_PROSE.iter().chain(AFTER_PROSE.iter()) {
            assert_eq!(role_of(page, text), "body", "{text}");
        }
        assert_eq!(report.figure, 1);
    }

    #[test]
    fn column_prose_far_above_the_caption_of_a_box_that_is_no_text_box_stays_body() {
        for text in BOX_RUN {
            assert!(word_count(text) >= COLUMN_RUN_WORDS, "{text}");
        }
        // A vector box the column lines run out of (x1 up to 490 past a
        // frame ending at 460), and a raster box with no frame at all.
        let frames = [
            figure(0, "vector", 60.0, 300.0, 460.0, 560.0),
            figure(0, "raster", 60.0, 300.0, 560.0, 560.0),
        ];
        for frame in frames {
            let mut pages = vec![box_run_page(true, frame)];
            let report = tag_regions(&mut pages);
            let page = &pages[0];
            for text in BOX_RUN {
                assert_eq!(role_of(page, text), "body", "{text}");
            }
            assert_eq!(role_of(page, "Encoder"), "figure");
            assert_eq!(role_of(page, BOX_LINE), "figure");
            assert_eq!(role_of(page, BOX_CAPTION), "caption");
            for text in LEFT_PROSE.iter().chain(AFTER_PROSE.iter()) {
                assert_eq!(role_of(page, text), "body", "{text}");
            }
            assert_eq!(report.figure, 2);
        }
    }

    #[test]
    fn every_line_of_a_tall_captioned_prompt_box_is_figure_text() {
        for text in TALL_LINES {
            assert_eq!(word_count(text), 15, "{text}");
            assert!(is_prose_like(text), "{text}");
        }
        let mut lines: Vec<Line> = Vec::new();
        let mut baseline = 760.0;
        for text in LEFT_PROSE {
            lines.push(line(text, 72.0, baseline, 0));
            baseline -= 12.0;
        }
        // 12 lines in the upper part of a 400 pt box (y 280 to 680).
        let mut baseline = 660.0;
        for text in TALL_LINES {
            lines.push(line(text, 80.0, baseline, 1));
            baseline -= 12.0;
        }
        // Caption box y 250 to 260: 20 pt below the box.
        lines.push(captioned("Figure 2: A long dialogue.", 72.0, 252.0, 2));
        let mut baseline = 220.0;
        for text in AFTER_PROSE {
            lines.push(line(text, 72.0, baseline, 3));
            baseline -= 12.0;
        }
        let mut page = page_of(lines);
        page.figures
            .push(figure(0, "vector", 60.0, 280.0, 560.0, 680.0));
        let mut pages = vec![page];
        let report = tag_regions(&mut pages);
        let page = &pages[0];
        for text in TALL_LINES {
            assert_eq!(role_of(page, text), "figure", "{text}");
        }
        assert_eq!(role_of(page, "Figure 2: A long dialogue."), "caption");
        for text in LEFT_PROSE.iter().chain(AFTER_PROSE.iter()) {
            assert_eq!(role_of(page, text), "body", "{text}");
        }
        assert_eq!(report.figure, 12);
    }
}
