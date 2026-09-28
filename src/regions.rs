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
//! that shape that is not itself prose (`TABLE I`, `Algorithm 1 Name`).
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
//!   more than 15 % of the band width). When nothing fragment-like lies
//!   above and the caption is among the top 20 % of its band's lines, the
//!   lines below the caption are tried the same way (caption above the
//!   figure).
//! - table: the lines below a table caption (caption above, ACM/IEEE) up
//!   to the next prose paragraph, else the lines above it (Elsevier),
//!   tagged `table` when at least 50 % of the lines have at most 5 words or
//!   carry at least 2 numeric tokens.
//! - algorithm: the lines below an `Algorithm N` caption up to the next
//!   prose paragraph, tagged `algorithm` when at least 30 % carry a marker
//!   (`Input:`, `Output:`, `Require:`, `Ensure:`, a `N:` step number, a
//!   leading `for`/`while`/`if`, `end for`, `return`, `←`, `:=`).
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

const ROLE_BODY: &str = "body";
const ROLE_CAPTION: &str = "caption";
const ROLE_FURNITURE: &str = "furniture";

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
            Self::Table => "table",
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

/// Tag caption continuations, figure text, table cells and algorithm
/// blocks on every page (see the module documentation). Adds a page
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
        let counts = [
            ("caption text", report.caption),
            ("figure text", report.figure),
            ("table text", report.table),
            ("algorithm text", report.algorithm),
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
    let geometry = measure(page);
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
        let region: Vec<usize> = find_region(page, &entries, pos, kind, band, &geometry)
            .into_iter()
            .filter(|&i| taggable(&page.lines[i].text, kind))
            .collect();
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
    let alone = words.next().is_none();
    if punctuated || alone || kind == Kind::Algorithm {
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
            let near_top = pos * 5 < entries.len();
            if has_fragments || !near_top {
                return Vec::new();
            }
            let below = walk_down(page, entries, pos, band, blank, false);
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

/// A figure fragment: at most 4 words, or axis-like.
fn is_fragment(text: &str) -> bool {
    word_count(text) <= FRAGMENT_WORDS || is_axis_like(text)
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
        let mut lines: Vec<Line> = Vec::new();
        prose_column(&mut lines, 54.0, 760.0, 50, 0);
        lines.push(captioned(caption, 312.0, 760.0, 1));
        let mut baseline = 746.0;
        for k in 0..30 {
            if k % 5 == 3 {
                lines.push(line(
                    "L(\u{3b8}) = \u{3a3} w\u{2083} \u{2113}",
                    420.0,
                    baseline,
                    2,
                ));
                lines.push(line("(3)", 560.0, baseline, 3));
            } else {
                let text = COLUMN_PROSE[k % COLUMN_PROSE.len()];
                lines.push(line(text, 312.0, baseline, 4));
            }
            baseline -= 12.0;
        }
        let mut pages = vec![page_of(lines)];
        let report = tag_regions(&mut pages);
        assert_eq!(report, RegionReport::default());
        only_caption_tagged(&pages[0], caption);
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
}
