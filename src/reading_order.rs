//! Reading order recovery: positioned spans -> lines -> ordered page text.
//!
//! The backend emits one [`Span`] per string operator in content-stream
//! order, which on multi-column pages is rarely the reading order. This
//! module groups spans into lines by baseline, orders the lines with a
//! recursive XY-cut over their boxes (columns before rows, except that
//! full-width lines at the top or bottom of a region are split off first,
//! that a region is cut into bands at lines bridging its gutter, and that
//! a row gap across the columns is cut first when the line texts read on
//! better by rows) and joins them into `PageText::text`.
//! Coordinates are PDF user space (origin bottom-left, `y` grows upwards).
//! On a page with `/Rotate` 90, 180 or 270 the spans are grouped and
//! ordered in the turned frame the page is displayed in, and the line
//! boxes are turned back into user space. No text repair of any kind is
//! performed.
//!
//! A line bridges the gutter of a two-column region (a figure caption, a
//! table or an equation set across both columns, or a row of the two
//! columns fused into one line) when it runs past the gutter by more than
//! `BRIDGE_REACH` of the region's width on both sides. Such a line blocks
//! every column cut through the region, and the whitespace above and below
//! it is often narrower than a row gap, so the region is first cut into
//! bands where bridging and other lines meet (see [`bridge_row_cut`]):
//! the band above reads left column then right column, then the bridging
//! lines, then the band below.
//!
//! Fonts without precomposed accented letters (`pdfTeX` with the OT1
//! encoding, say) set an accent as a glyph of its own, placed slightly
//! above the letter it belongs to and often shown before that letter. Such
//! an accent span is not a line of its own: it is attached to the line
//! whose glyph it sits over, listed right after that glyph's span, and its
//! spacing accent is composed onto the letter under it as the combining
//! mark it stands for (`Verdu` + acute over the `u` reads `Verdú`). That
//! is a rendering of what the page shows, not a repair of it.
//!
//! `pdfTeX` mostly does not show the accent on its own: when no kern
//! precedes it the accent closes the string of the text before it, and the
//! letter opens the next string after a kern back under the accent
//! (`[(H\177)500(older)]TJ` sets `Hölder`). Such a span ends in an accent
//! glyph and the span under that glyph's tail holds the letter, which is
//! composed the same way once the overlap is confirmed; without a letter
//! under it the span's text is kept as shown.
//!
//! Vertical text (the rotated `arXiv` stamp in the left margin, rotated
//! axis labels) never joins a horizontal line. A span is vertical when it
//! shows at least two glyphs in a box more than `VERTICAL_RATIO` times
//! taller than wide and `VERTICAL_MIN_HEIGHT` sizes tall, or when it is one
//! rotated glyph (a box about as wide as tall) in a run of at least
//! `STACK_MIN` such glyphs stacked in the page margin in content-stream
//! order. Vertical spans are grouped into lines of their own in
//! content-stream order; such a line in the margin band (`MARGIN_FRACTION`
//! of the page width from either edge) is placed after all other lines with
//! a page warning, anywhere else it is ordered like any other line.
//!
//! Spans on a shared baseline in the two columns of a page can end up in
//! one line when the gutter is narrower than `LINE_REACH`. A line whose
//! widest gap is wider than `GUTTER_SPACES` space widths, straddles the
//! vertical midline of the page (or of the text on it) and has at least
//! `GUTTER_WORDS` words on either side is split there. A line wider than
//! `SPANNING_PAGE` of the page (a title, or a fused row of two justified
//! columns) is split only when that gap also covers the gutter the other
//! lines of the page leave open, so a title's stretched word space is kept.
//!
//! A column gutter is often narrowed or crossed by a few lines of the left
//! column that run past its right edge (an overfull line, a protruding
//! hyphen), which leaves no clean vertical whitespace through the region.
//! When no clean column cut exists, up to `OVERHANG_LINES` such lines are
//! tolerated (see [`overhang_column_cut`]); a line centred on the gutter or
//! reaching deep into the right column never is.
//!
//! A page header or footer set as short lines over or under the columns (a
//! running head over one column, the page number over the other) is cut
//! off at the row gap that separates it from the columns before the
//! columns are read (see [`furniture_bands`]), so it does not stand in for
//! the first or last line of a column when the text flow is weighed.

use std::borrow::Cow;
use std::cmp::Ordering;

use unicode_normalization::UnicodeNormalization;

use crate::schema::{BBox, Line, PageText, Span};

mod line_index;
use line_index::{LineIndex, MAX_HORIZONTAL_WORK};

/// Maximum XY-cut recursion depth.
const MAX_DEPTH: u32 = 64;
/// Maximum number of lines laid out on one page; the rest is appended as is.
const MAX_LINES: usize = 20_000;
/// Maximum estimated character/member inspections spent composing accents
/// on one page. Above this bound the glyphs are kept verbatim instead.
const MAX_ACCENT_WORK: usize = 1_000_000;
/// Maximum prior vertical groups examined on one page. Once exhausted,
/// remaining spans stay separate rather than allowing hostile geometry to
/// make grouping quadratic.
const MAX_VERTICAL_GROUP_COMPARISONS: usize = MAX_LINES * 8;
/// Baseline tolerance for joining spans into one line (multiple of the size).
const BASELINE_TOLERANCE: f32 = 0.4;
/// Horizontal reach for joining spans into one line (multiple of the size).
const LINE_REACH: f32 = 1.0;
/// Gap between neighbouring spans that counts as a word space (multiple of the size).
const SPACE_GAP: f32 = 0.15;
/// Vertical whitespace wider than this many median line heights splits rows.
const ROW_GAP: f32 = 1.0;
/// Horizontal whitespace wider than this many median character widths splits columns.
const COLUMN_GAP: f32 = 2.0;
/// Vertical gap inside a block wider than this many median line heights is a paragraph.
const PARAGRAPH_GAP: f32 = 1.5;
/// Font size assumed when nothing on the page carries a size or a height.
const FALLBACK_SIZE: f32 = 10.0;
/// Highest an accent glyph sits above the baseline of its letter (multiple of the size).
const ACCENT_RISE: f32 = 1.5;
/// Lowest an accent glyph sits below the baseline of its letter (multiple of
/// the size); absorbs rounding in the accent's placement.
const ACCENT_DIP: f32 = 0.1;
/// How far below its letter's baseline a below-base mark (cedilla, ogonek)
/// may sit, in multiples of the font size.
const ACCENT_DIP_BELOW: f32 = 0.6;
/// Horizontal slack when matching an accent's centre to the glyph span under
/// it (multiple of the size); bridges kerning between two spans of one word.
const ACCENT_SLACK: f32 = 0.1;
/// Fraction of a region's width above which a line counts as spanning it
/// (a title, running header or footer) for the XY-cut.
const SPANNING_WIDTH: f32 = 0.6;
/// Fraction of a region's width below which a line centred on the region
/// (a page number or short running footer in the gutter) counts as a
/// margin line for the XY-cut.
const GUTTER_WIDTH: f32 = 0.15;
/// Largest distance, as a fraction of the region's width, between a narrow
/// margin line's centre and the region's horizontal midpoint.
const GUTTER_OFFSET: f32 = 0.1;
/// Smallest vertical overlap of the two sides of a column cut, as a
/// fraction of the shorter side's vertical extent, for them to count as
/// columns standing side by side.
const COEXIST_OVERLAP: f32 = 0.5;
/// Fewest lines each side of a column cut needs inside the vertically
/// overlapping band for the sides to count as columns.
const COEXIST_LINES: usize = 2;
/// A span of at least two glyphs whose box is more than this many times
/// taller than wide is vertical text.
const VERTICAL_RATIO: f32 = 3.0;
/// Smallest height of a vertical span's box, in multiples of its size (a
/// horizontal box is one size tall, whatever its advance).
const VERTICAL_MIN_HEIGHT: f32 = 2.0;
/// Fraction of the page width, from either edge, that counts as the page
/// margin for vertical text (the `arXiv` stamp) and stacked glyphs.
const MARGIN_FRACTION: f32 = 0.07;
/// Fewest single glyphs stacked in the margin that make vertical text.
const STACK_MIN: usize = 4;
/// Largest height of a rotated glyph's box, in multiples of its width (the
/// width is the em of the rotated glyph, the height its advance).
const STACK_SHAPE: f32 = 1.2;
/// Smallest horizontal overlap of two stacked glyph boxes, as a fraction of
/// the narrower one.
const STACK_OVERLAP: f32 = 0.5;
/// Largest vertical gap between two stacked glyphs, in box widths (ems).
const STACK_GAP: f32 = 1.0;
/// Largest `seq` step between two stacked glyphs (room for blank spaces).
const STACK_SEQ_STEP: u32 = 3;
/// Gaps between the spans of a line wider than this many sizes are not
/// word spaces when the page's space width is measured.
const SPACE_MAX: f32 = 0.6;
/// Space width, in multiples of the size, when no word gap is measured.
const DEFAULT_SPACE: f32 = 0.25;
/// A gap in a line wider than this many space widths may be a gutter.
const GUTTER_SPACES: f32 = 2.5;
/// Fewest words each side of a gutter gap needs for the line to be split.
const GUTTER_WORDS: usize = 3;
/// Fraction of the page width above which a line with a gutter gap is
/// split only when the gap covers the gutter of the page's other lines.
const SPANNING_PAGE: f32 = 0.7;
/// Slack, as a fraction of the page width, for a gap straddling the midline.
const MIDLINE_SLACK: f32 = 0.02;
/// Fewest other lines wholly left and wholly right of a gap that locate the
/// page's gutter.
const GUTTER_EVIDENCE: usize = 3;
/// Fewest other rows with a gap at the same place that locate the gutter
/// when too few lines lie wholly to either side of it (all rows fused).
const GUTTER_ROWS: usize = 2;
/// Tolerance, in points, of a gap covering the page's gutter.
const GUTTER_COVER: f32 = 1.0;
/// Fraction of a region's width by which a line must run past the gutter
/// on both sides to bridge it.
const BRIDGE_REACH: f32 = 0.2;
/// Fraction of a region's width that the lines of a column of prose reach;
/// each side of a bridged gutter needs `COEXIST_LINES` lines this wide.
const BRIDGE_COLUMN: f32 = 0.3;
/// Largest vertical overlap, in median line heights, of the boxes on either
/// side of a cut at a bridging line (touching lines of tight leading).
const BRIDGE_OVERLAP: f32 = 0.25;
/// Most lines of the left side of a column cut that may run into or across
/// its gutter (overfull lines, protruding hyphens).
const OVERHANG_LINES: usize = 3;
/// One more overhanging line is tolerated per this many lines of the left
/// side of a column cut (at least one, at most `OVERHANG_LINES`).
const OVERHANG_SHARE: usize = 5;
/// Most lines of a page header or footer band over or under the columns.
const BAND_LINES: usize = 4;
/// Fraction of a region's width below which every line of a page header or
/// footer band stays.
const BAND_WIDTH: f32 = 0.4;
/// A column heading stays associated with nearby prose only within two
/// normal line heights; more widely separated running heads remain bands.
const COLUMN_HEADING_GAP: f32 = 2.0;
/// Tolerance in points for a heading aligned with its column's left edge.
const COLUMN_HEADING_ALIGN: f32 = 1.0;

/// Thresholds of one XY-cut run, in points.
#[allow(clippy::struct_field_names)]
struct CutParams {
    row_gap: f32,
    column_gap: f32,
    /// Smallest (negative) gap of a cut at a bridging line.
    bridge_gap: f32,
}

/// An accent glyph attached to a line: its span index and box, the span
/// index of the glyph it sits over, and the combining marks it stands for.
/// `cut` is `Some(offset)` when the accent glyphs are the tail of the text
/// of span `index` from that byte offset on (the span itself is a glyph
/// member of the line and `bbox` is the estimated box of its tail), `None`
/// when the span shows nothing but the accent.
struct Accent {
    index: usize,
    bbox: BBox,
    base: usize,
    marks: String,
    cut: Option<usize>,
}

/// A line under construction while spans are grouped.
struct LineBuild {
    baseline: f32,
    size: f32,
    bbox: BBox,
    spans: Vec<(usize, BBox)>,
    accents: Vec<Accent>,
}

/// Median of `values` (sorts the slice in place); `None` when empty.
pub fn median(values: &mut [f32]) -> Option<f32> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f32::total_cmp);
    let n = values.len();
    let mid = n / 2;
    if n % 2 == 1 {
        Some(values[mid])
    } else {
        Some(values[mid - 1].midpoint(values[mid]))
    }
}

/// True when all four coordinates are finite numbers.
fn is_finite_box(b: BBox) -> bool {
    [b.x0, b.y0, b.x1, b.y1].into_iter().all(f32::is_finite)
}

/// The same box with `x0 <= x1` and `y0 <= y1`.
fn normalized(b: BBox) -> BBox {
    BBox {
        x0: b.x0.min(b.x1),
        y0: b.y0.min(b.y1),
        x1: b.x0.max(b.x1),
        y1: b.y0.max(b.y1),
    }
}

/// Smallest box containing both `a` and `b`.
fn union(a: BBox, b: BBox) -> BBox {
    BBox {
        x0: a.x0.min(b.x0),
        y0: a.y0.min(b.y0),
        x1: a.x1.max(b.x1),
        y1: a.y1.max(b.y1),
    }
}

/// The span's font size when it is a usable positive number, else `fallback`.
fn span_size(span: &Span, fallback: f32) -> f32 {
    positive(span.size).unwrap_or(fallback)
}

/// Keep the value only when it is a finite positive number.
fn positive(value: Option<f32>) -> Option<f32> {
    value.filter(|v| v.is_finite() && *v > 0.0)
}

/// Top-to-bottom (`y1` descending), then left-to-right (`x0` ascending).
fn top_first(a: &BBox, b: &BBox) -> Ordering {
    b.y1.total_cmp(&a.y1).then(a.x0.total_cmp(&b.x0))
}

/// Left-to-right (`x0` ascending), then top-to-bottom (`y1` descending).
fn left_first(a: &BBox, b: &BBox) -> Ordering {
    a.x0.total_cmp(&b.x0).then(b.y1.total_cmp(&a.y1))
}

/// Non-blank spans with a finite box, as `(index, normalised box)`.
fn positioned(spans: &[Span]) -> Vec<(usize, BBox)> {
    let mut out: Vec<(usize, BBox)> = Vec::new();
    for (i, span) in spans.iter().enumerate() {
        if span.text.trim().is_empty() {
            continue;
        }
        let Some(b) = span.bbox else {
            continue;
        };
        if is_finite_box(b) {
            out.push((i, normalized(b)));
        }
    }
    out
}

/// Typical font size of the positioned spans: median declared size, else
/// median box height, else `FALLBACK_SIZE`.
fn typical_size(spans: &[Span], candidates: &[(usize, BBox)]) -> f32 {
    let mut sizes: Vec<f32> = Vec::new();
    for (i, _) in candidates {
        if let Some(s) = positive(spans[*i].size) {
            sizes.push(s);
        }
    }
    if let Some(m) = median(&mut sizes) {
        return m;
    }
    let mut heights: Vec<f32> = Vec::new();
    for (_, b) in candidates {
        if b.y1 - b.y0 > 0.0 {
            heights.push(b.y1 - b.y0);
        }
    }
    median(&mut heights).unwrap_or(FALLBACK_SIZE)
}

/// Baseline-first order for grouping: `y0` descending, then `x0` ascending.
fn baseline_first(a: &(usize, BBox), b: &(usize, BBox)) -> Ordering {
    b.1.y0.total_cmp(&a.1.y0).then(a.1.x0.total_cmp(&b.1.x0))
}

/// Top-to-bottom order of lines; lines without a box compare equal.
fn line_top_first(a: &Line, b: &Line) -> Ordering {
    let (Some(x), Some(y)) = (a.bbox, b.bbox) else {
        return Ordering::Equal;
    };
    top_first(&x, &y)
}

/// Index of the line under construction that a span with `bbox` belongs to:
/// same baseline within `BASELINE_TOLERANCE` and x ranges within
/// `LINE_REACH` of each other. `largest` bounds the search.
#[cfg(test)]
fn find_line(builds: &[LineBuild], bbox: BBox, size: f32, largest: f32) -> Option<usize> {
    for (k, line) in builds.iter().enumerate().rev() {
        if line.baseline - bbox.y0 > BASELINE_TOLERANCE * largest {
            break;
        }
        let reference = size.max(line.size);
        let same_baseline = (line.baseline - bbox.y0).abs() <= BASELINE_TOLERANCE * reference;
        let reach = LINE_REACH * reference;
        let near = bbox.x0 <= line.bbox.x1 + reach && bbox.x1 >= line.bbox.x0 - reach;
        if same_baseline && near {
            return Some(k);
        }
    }
    None
}

/// True for a Unicode combining diacritical mark (U+0300 to U+036F).
fn is_combining(ch: char) -> bool {
    ('\u{0300}'..='\u{036F}').contains(&ch)
}

/// The combining mark a spacing accent glyph stands for: the spacing
/// accents of the Latin-1 and Spacing Modifier blocks as `lopdf` decodes
/// the accent glyph names (`acute`, `dieresis`, `circumflex`, `tilde`,
/// `caron`, ...), the ASCII grave, circumflex and tilde, and the modifier
/// acute and grave. A combining mark stands for itself.
fn combining_accent(ch: char) -> Option<char> {
    let mark = match ch {
        '\u{00B4}' | '\u{02CA}' => '\u{0301}',
        '\u{0060}' | '\u{02CB}' => '\u{0300}',
        '\u{00A8}' => '\u{0308}',
        '\u{005E}' | '\u{02C6}' => '\u{0302}',
        '\u{007E}' | '\u{02DC}' => '\u{0303}',
        '\u{00AF}' => '\u{0304}',
        '\u{02D8}' => '\u{0306}',
        '\u{02D9}' => '\u{0307}',
        '\u{02DA}' => '\u{030A}',
        '\u{02DB}' => '\u{0328}',
        '\u{02DD}' => '\u{030B}',
        '\u{00B8}' => '\u{0327}',
        '\u{02C7}' => '\u{030C}',
        mark if is_combining(mark) => mark,
        _ => return None,
    };
    Some(mark)
}

/// The combining marks of a span that shows nothing but accent glyphs;
/// `None` for any other text, blank text included.
fn accent_marks(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut marks = String::with_capacity(trimmed.len());
    for ch in trimmed.chars() {
        marks.push(combining_accent(ch)?);
    }
    Some(marks)
}

/// The byte offset at which the trailing spacing accent glyphs of `text`
/// begin, with the combining marks they stand for; `None` when `text` does
/// not end in a spacing accent or shows nothing else. A combining mark at
/// the end already belongs to the letter before it and is not a tail.
fn trailing_accent(text: &str) -> Option<(usize, String)> {
    let mut cut = text.len();
    let mut marks: Vec<char> = Vec::new();
    for (at, ch) in text.char_indices().rev() {
        if is_combining(ch) {
            break;
        }
        let Some(mark) = combining_accent(ch) else {
            break;
        };
        marks.push(mark);
        cut = at;
    }
    if marks.is_empty() || text[..cut].trim().is_empty() {
        return None;
    }
    marks.reverse();
    Some((cut, marks.into_iter().collect()))
}

/// Estimated box of the tail of `text` from byte `cut` on, taking the
/// characters of the span (combining marks not counted) to share `b` evenly.
fn tail_box(text: &str, cut: usize, b: BBox) -> BBox {
    let count = text.chars().filter(|ch| !is_combining(*ch)).count();
    let tail = text[cut..].chars().filter(|ch| !is_combining(*ch)).count();
    if count == 0 || tail >= count {
        return b;
    }
    let share = (b.x1 - b.x0) * tail as f32 / count as f32;
    BBox {
        x0: b.x1 - share,
        y0: b.y0,
        x1: b.x1,
        y1: b.y1,
    }
}

/// Horizontal centre of a box.
fn centre_x(b: BBox) -> f32 {
    b.x0.midpoint(b.x1)
}

/// Position in `members` of the glyph span an accent centred at `cx` sits
/// over: the span whose x range, widened by `slack`, contains `cx`; the
/// nearer range when two do. The span with index `skip` (the one whose
/// tail the accent is) is never chosen.
fn base_member(
    members: &[(usize, BBox)],
    cx: f32,
    slack: f32,
    skip: Option<usize>,
) -> Option<usize> {
    let mut best: Option<(usize, f32)> = None;
    for (m, (i, b)) in members.iter().enumerate() {
        if skip == Some(*i) || !(b.x0 - slack..=b.x1 + slack).contains(&cx) {
            continue;
        }
        let distance = (b.x0 - cx).max(cx - b.x1).max(0.0);
        if best.is_none_or(|(_, d)| distance < d) {
            best = Some((m, distance));
        }
    }
    best.map(|(m, _)| m)
}

/// Index of the base character (combining marks not counted) of `piece`
/// under `cx`, taking the characters to share the box `b` evenly.
fn base_char_slot(piece: &str, b: BBox, cx: f32) -> usize {
    let count = piece.chars().filter(|ch| !is_combining(*ch)).count();
    if count < 2 {
        return 0;
    }
    let width = b.x1 - b.x0;
    if width <= 0.0 {
        return count - 1;
    }
    let fraction = ((cx - b.x0) / width).clamp(0.0, 1.0);
    let slot = (fraction * count as f32) as usize;
    slot.min(count - 1)
}

/// Insert `marks` after the `slot`-th base character of `piece`, behind any
/// marks that character already carries; a blank character there yields to
/// the nearest non-blank one. Appends when `piece` has no base character.
fn insert_marks(piece: &mut String, slot: usize, marks: &str) {
    let bases: Vec<(usize, char)> = piece
        .char_indices()
        .filter(|(_, ch)| !is_combining(*ch))
        .collect();
    let Some(last) = bases.len().checked_sub(1) else {
        piece.push_str(marks);
        return;
    };
    let mut at = slot.min(last);
    if bases[at].1.is_whitespace() {
        let before = (0..at).rev().find(|j| !bases[*j].1.is_whitespace());
        let after = (at + 1..=last).find(|j| !bases[*j].1.is_whitespace());
        at = match (before, after) {
            (Some(prev), Some(next)) if next - at < at - prev => next,
            (Some(prev), _) => prev,
            (None, Some(next)) => next,
            (None, None) => at,
        };
    }
    let start = bases[at].0 + bases[at].1.len_utf8();
    let end = piece[start..]
        .find(|ch: char| !is_combining(ch))
        .map_or(piece.len(), |offset| start + offset);
    piece.insert_str(end, marks);
}

/// `(start, end)` of the lines in `builds` (baselines descending) whose
/// baseline lies in `[low, high]`.
fn baseline_window(builds: &[LineBuild], low: f32, high: f32) -> (usize, usize) {
    let start = builds.partition_point(|line| line.baseline > high);
    let end = builds.partition_point(|line| line.baseline >= low);
    (start, end.max(start))
}

/// Marks that hang below the letter (cedilla, ogonek) rather than above it.
fn is_below_mark(marks: &str) -> bool {
    !marks.is_empty() && marks.chars().all(|c| matches!(c, '\u{0327}' | '\u{0328}'))
}

/// The line an accent glyph with `bbox` sits over: its baseline at most
/// `ACCENT_RISE` sizes below the accent's (or `ACCENT_DIP` above it) and one
/// of its glyph spans under the accent's centre; the nearest baseline wins.
/// Returns the line's index in `builds` and the span index of that glyph.
fn find_base_line(
    builds: &[LineBuild],
    bbox: BBox,
    size: f32,
    largest: f32,
    below: bool,
) -> Option<(usize, usize)> {
    let cx = centre_x(bbox);
    let dip = if below { ACCENT_DIP_BELOW } else { ACCENT_DIP };
    let low = bbox.y0 - ACCENT_RISE * largest;
    let high = bbox.y0 + dip * largest;
    let (start, end) = baseline_window(builds, low, high);
    let mut best: Option<(usize, usize, f32)> = None;
    for (offset, line) in builds[start..end].iter().enumerate() {
        let reference = size.max(line.size);
        let rise = bbox.y0 - line.baseline;
        if !(-dip * reference..=ACCENT_RISE * reference).contains(&rise) {
            continue;
        }
        let Some(m) = base_member(&line.spans, cx, ACCENT_SLACK * reference, None) else {
            continue;
        };
        let distance = rise.abs();
        if best.is_none_or(|(_, _, d)| distance < d) {
            best = Some((start + offset, line.spans[m].0, distance));
        }
    }
    best.map(|(k, base, _)| (k, base))
}

/// Ordinary line membership (the rule of `find_line`) for an accent glyph
/// that sits over no glyph span, searched from the lowest matching baseline
/// like `find_line` does.
fn find_same_baseline(builds: &[LineBuild], bbox: BBox, size: f32, largest: f32) -> Option<usize> {
    let low = bbox.y0 - BASELINE_TOLERANCE * largest;
    let high = bbox.y0 + BASELINE_TOLERANCE * largest;
    let (start, end) = baseline_window(builds, low, high);
    for (offset, line) in builds[start..end].iter().enumerate().rev() {
        let reference = size.max(line.size);
        let same_baseline = (line.baseline - bbox.y0).abs() <= BASELINE_TOLERANCE * reference;
        let reach = LINE_REACH * reference;
        let near = bbox.x0 <= line.bbox.x1 + reach && bbox.x1 >= line.bbox.x0 - reach;
        if same_baseline && near {
            return Some(start + offset);
        }
    }
    None
}

/// Sort a line's glyph spans left-to-right and join their texts, inserting
/// one space where the horizontal gap exceeds `SPACE_GAP` times the size and
/// neither neighbour already has a boundary space. Each accent of the line
/// is listed right after the glyph span it sits over and its combining marks
/// are composed onto the character under it (an accent that is the tail of
/// a glyph span is cut off that span's text instead), after which the line
/// text is put in NFC; a line without accents keeps its text exactly as
/// joined.
fn finish_line(spans: &[Span], build: LineBuild, fallback: f32) -> Line {
    let mut members = build.spans;
    members.sort_by(|a, b| {
        let by_x = a.1.x0.total_cmp(&b.1.x0);
        by_x.then(spans[a.0].seq.cmp(&spans[b.0].seq))
    });
    // Span texts are borrowed; only a piece that gets accent marks or
    // loses an accent tail is copied.
    let mut pieces: Vec<Cow<'_, str>> = members
        .iter()
        .map(|(i, _)| Cow::Borrowed(spans[*i].text.as_str()))
        .collect();
    let mut attached: Vec<Vec<usize>> = vec![Vec::new(); members.len()];
    let mut accents = build.accents;
    accents.sort_by(|a, b| {
        let by_x = centre_x(a.bbox).total_cmp(&centre_x(b.bbox));
        by_x.then(spans[a.index].seq.cmp(&spans[b.index].seq))
    });
    let composed = !accents.is_empty();
    // Tails are cut before any mark is inserted, so the byte offsets of
    // `cut` still refer to the text as the span shows it.
    for accent in &accents {
        if let Some(cut) = accent.cut
            && let Some(h) = members.iter().position(|(i, _)| *i == accent.index)
        {
            pieces[h].to_mut().truncate(cut);
        }
    }
    for accent in accents {
        let last = members.len().saturating_sub(1);
        let m = members
            .iter()
            .position(|(i, _)| *i == accent.base)
            .unwrap_or(last);
        let slot = base_char_slot(&pieces[m], members[m].1, centre_x(accent.bbox));
        insert_marks(pieces[m].to_mut(), slot, &accent.marks);
        if accent.cut.is_none() {
            attached[m].push(accent.index);
        }
    }

    let capacity: usize = pieces.iter().map(|piece| piece.len() + 1).sum();
    let mut joined = String::with_capacity(capacity);
    let mut order: Vec<usize> = Vec::with_capacity(members.len());
    let mut prev_x1: Option<f32> = None;
    for (m, (i, bbox)) in members.iter().enumerate() {
        let span = &spans[*i];
        let piece: &str = &pieces[m];
        if let Some(x1) = prev_x1 {
            let has_space =
                joined.ends_with(char::is_whitespace) || piece.starts_with(char::is_whitespace);
            if bbox.x0 - x1 > SPACE_GAP * span_size(span, fallback) && !has_space {
                joined.push(' ');
            }
        }
        joined.push_str(piece);
        prev_x1 = Some(bbox.x1);
        order.push(*i);
        order.extend(attached[m].iter().copied());
    }
    let mut text: String = if composed {
        joined.nfc().collect()
    } else {
        joined
    };
    // `trim` in place: the line text is built once.
    let end = text.trim_end().len();
    text.truncate(end);
    let start = text.len() - text.trim_start().len();
    text.drain(..start);
    Line {
        text,
        bbox: Some(build.bbox),
        column: 0,
        spans: order
            .iter()
            .map(|i| u32::try_from(*i).unwrap_or(u32::MAX))
            .collect(),
        role: crate::schema::default_line_role(),
    }
}

/// A span of at least two glyphs set vertically: its box is more than
/// `VERTICAL_RATIO` times taller than wide and more than
/// `VERTICAL_MIN_HEIGHT` sizes tall.
fn is_tall_text(span: &Span, b: BBox, fallback: f32) -> bool {
    let glyphs = span.text.chars().filter(|ch| !ch.is_whitespace()).count();
    let width = b.x1 - b.x0;
    let height = b.y1 - b.y0;
    glyphs >= 2
        && height > VERTICAL_RATIO * width
        && height > VERTICAL_MIN_HEIGHT * span_size(span, fallback)
}

/// Whether a box's centre lies in the margin band of a page `width` wide.
fn in_margin(b: BBox, width: f32) -> bool {
    let cx = centre_x(b);
    cx < MARGIN_FRACTION * width || cx > (1.0 - MARGIN_FRACTION) * width
}

/// A single glyph in the page margin whose box is shaped like a rotated
/// glyph: at most `STACK_SHAPE` times taller than wide.
fn is_margin_glyph(span: &Span, b: BBox, width: f32) -> bool {
    let mut glyphs = span.text.trim().chars().filter(|ch| !is_combining(*ch));
    let single = glyphs.next().is_some() && glyphs.next().is_none();
    let w = b.x1 - b.x0;
    single && w > 0.0 && b.y1 - b.y0 <= STACK_SHAPE * w && in_margin(b, width)
}

/// Set `flags[p]` for every member of `run` (positions in `all`) when the
/// run is long enough to be a stack of rotated glyphs.
fn close_stack(run: &[usize], flags: &mut [bool]) {
    if run.len() >= STACK_MIN {
        for &p in run {
            flags[p] = true;
        }
    }
}

/// Flag the single margin glyphs of `all` (see [`is_margin_glyph`]) that
/// are stacked into vertical text: at least `STACK_MIN` of them in
/// content-stream order, each at most `STACK_SEQ_STEP` spans after the
/// previous one in the stream, overlapping the previous one horizontally by
/// `STACK_OVERLAP` of the narrower box, at most `STACK_GAP` ems above or
/// below it, and all moving the same way.
fn flag_glyph_stacks(spans: &[Span], all: &[(usize, BBox)], width: f32, flags: &mut [bool]) {
    let mut glyphs: Vec<usize> = (0..all.len())
        .filter(|&p| !flags[p] && is_margin_glyph(&spans[all[p].0], all[p].1, width))
        .collect();
    if glyphs.len() < STACK_MIN {
        return;
    }
    glyphs.sort_by_key(|&p| (spans[all[p].0].seq, all[p].0));
    let mut run: Vec<usize> = Vec::new();
    let mut upward: Option<bool> = None;
    for p in glyphs {
        let b = all[p].1;
        let mut stacked = false;
        if let Some(&last) = run.last() {
            let a = all[last].1;
            let step = b.y0.midpoint(b.y1) - a.y0.midpoint(a.y1);
            let narrow = (a.x1 - a.x0).min(b.x1 - b.x0);
            let em = (a.x1 - a.x0).max(b.x1 - b.x0);
            let overlap = a.x1.min(b.x1) - a.x0.max(b.x0);
            let gap = (b.y0 - a.y1).max(a.y0 - b.y1);
            let up = step > 0.0;
            // Consecutive in the stream, blank space spans aside.
            let next_in_stream =
                spans[all[p].0].seq.saturating_sub(spans[all[last].0].seq) <= STACK_SEQ_STEP;
            if next_in_stream
                && overlap >= STACK_OVERLAP * narrow
                && step.abs() > 0.0
                && upward.is_none_or(|was_up| was_up == up)
                && gap <= STACK_GAP * em
            {
                upward = Some(up);
                stacked = true;
            }
        }
        if !stacked {
            close_stack(&run, flags);
            run.clear();
            upward = None;
        }
        run.push(p);
    }
    close_stack(&run, flags);
}

/// Split `all` into vertical spans (see [`is_tall_text`] and
/// [`flag_glyph_stacks`]) and the rest, each keeping the order of `all`.
/// Indexed boxes: a span index and the box it draws.
type Placed = Vec<(usize, BBox)>;

fn split_vertical(spans: &[Span], all: Placed, fallback: f32, width: f32) -> (Placed, Placed) {
    let mut flags: Vec<bool> = all
        .iter()
        .map(|(i, b)| is_tall_text(&spans[*i], *b, fallback))
        .collect();
    flag_glyph_stacks(spans, &all, width, &mut flags);
    let mut vertical: Vec<(usize, BBox)> = Vec::new();
    let mut rest: Vec<(usize, BBox)> = Vec::with_capacity(all.len());
    for (member, flag) in all.into_iter().zip(flags) {
        if flag {
            vertical.push(member);
        } else {
            rest.push(member);
        }
    }
    (vertical, rest)
}

/// One line of vertical text from `members` (in content-stream order): the
/// span texts joined in that order, with one space where the vertical gap
/// between neighbours exceeds `SPACE_GAP` ems and neither side already has
/// a boundary space.
fn vertical_line(spans: &[Span], bbox: BBox, members: &[(usize, BBox)]) -> Line {
    let mut text = String::new();
    let mut prev: Option<BBox> = None;
    for (i, b) in members {
        let piece = spans[*i].text.as_str();
        if let Some(p) = prev {
            let gap = (b.y0 - p.y1).max(p.y0 - b.y1);
            let em = (b.x1 - b.x0).max(p.x1 - p.x0);
            let has_space =
                text.ends_with(char::is_whitespace) || piece.starts_with(char::is_whitespace);
            if gap > SPACE_GAP * em && !has_space {
                text.push(' ');
            }
        }
        text.push_str(piece);
        prev = Some(*b);
    }
    Line {
        text: text.trim().to_string(),
        bbox: Some(bbox),
        column: 0,
        spans: members
            .iter()
            .map(|(i, _)| u32::try_from(*i).unwrap_or(u32::MAX))
            .collect(),
        role: crate::schema::default_line_role(),
    }
}

/// Group vertical spans into lines: in content-stream order, a span joins
/// the latest group it overlaps horizontally by `STACK_OVERLAP` of the
/// narrower box and lies at most `STACK_GAP` ems above or below.
fn vertical_lines(spans: &[Span], vertical: &[(usize, BBox)]) -> (Vec<Line>, bool) {
    vertical_lines_with_budget(spans, vertical, MAX_VERTICAL_GROUP_COMPARISONS)
}

fn vertical_lines_with_budget(
    spans: &[Span],
    vertical: &[(usize, BBox)],
    mut comparisons_left: usize,
) -> (Vec<Line>, bool) {
    let mut limited = false;
    let mut order: Vec<(usize, BBox)> = vertical.to_vec();
    order.sort_by_key(|(i, _)| (spans[*i].seq, *i));
    let mut groups: Vec<(BBox, Vec<(usize, BBox)>)> = Vec::new();
    for (i, b) in order {
        let mut found = None;
        for (k, (g, _)) in groups.iter().enumerate().rev() {
            if comparisons_left == 0 {
                limited = true;
                break;
            }
            comparisons_left -= 1;
            let narrow = (g.x1 - g.x0).min(b.x1 - b.x0);
            let em = (g.x1 - g.x0).max(b.x1 - b.x0);
            let overlap = g.x1.min(b.x1) - g.x0.max(b.x0);
            let gap = (b.y0 - g.y1).max(g.y0 - b.y1);
            if overlap > 0.0 && overlap >= STACK_OVERLAP * narrow && gap <= STACK_GAP * em {
                found = Some(k);
                break;
            }
        }
        if let Some(k) = found {
            groups[k].0 = union(groups[k].0, b);
            groups[k].1.push((i, b));
        } else {
            groups.push((b, vec![(i, b)]));
        }
    }
    let lines = groups
        .into_iter()
        .map(|(bbox, members)| vertical_line(spans, bbox, &members))
        .collect();
    (lines, limited)
}

/// A line's glyph spans sorted left-to-right (then by `seq`).
fn sorted_members(spans: &[Span], build: &LineBuild) -> Vec<(usize, BBox)> {
    let mut members = build.spans.clone();
    members.sort_by(|a, b| {
        let by_x = a.1.x0.total_cmp(&b.1.x0);
        by_x.then(spans[a.0].seq.cmp(&spans[b.0].seq))
    });
    members
}

/// The widest gap between neighbours of `members` (sorted left-to-right):
/// the position of the first member right of it and the gap's left and
/// right edge. `None` when no two members leave a gap.
fn widest_gap(members: &[(usize, BBox)]) -> Option<(usize, f32, f32)> {
    let (_, first) = members.first()?;
    let mut right = first.x1;
    let mut best: Option<(usize, f32, f32)> = None;
    for (pos, (_, b)) in members.iter().enumerate().skip(1) {
        let gap = b.x0 - right;
        if gap > 0.0 && best.is_none_or(|(_, x0, x1)| gap > x1 - x0) {
            best = Some((pos, right, b.x0));
        }
        right = right.max(b.x1);
    }
    best
}

/// Median word space of the page: gaps between neighbouring spans of a
/// line wider than `SPACE_GAP` and at most `SPACE_MAX` times the line's
/// size; `DEFAULT_SPACE` times `fallback` when there are none.
fn typical_space(sorted: &[Vec<(usize, BBox)>], builds: &[LineBuild], fallback: f32) -> f32 {
    let mut gaps: Vec<f32> = Vec::new();
    for (members, build) in sorted.iter().zip(builds) {
        let low = SPACE_GAP * build.size;
        let high = SPACE_MAX * build.size;
        for pair in members.windows(2) {
            let gap = pair[1].1.x0 - pair[0].1.x1;
            if gap > low && gap <= high {
                gaps.push(gap);
            }
        }
    }
    positive(median(&mut gaps)).unwrap_or(DEFAULT_SPACE * fallback)
}

/// Number of words in `members` (sorted left-to-right) joined the way
/// `finish_line` joins them.
fn word_count(spans: &[Span], members: &[(usize, BBox)], fallback: f32) -> usize {
    let mut joined = String::new();
    let mut prev_x1: Option<f32> = None;
    for (i, b) in members {
        let span = &spans[*i];
        if let Some(x1) = prev_x1
            && b.x0 - x1 > SPACE_GAP * span_size(span, fallback)
        {
            joined.push(' ');
        }
        joined.push_str(&span.text);
        prev_x1 = Some(b.x1);
    }
    joined.split_whitespace().count()
}

/// The gutter the lines of the page other than `skip` leave open around
/// `centre`: from the right edge of the lines wholly left of it to the left
/// edge of the lines wholly right of it when at least `GUTTER_EVIDENCE`
/// lie on either side; else the common part of the gaps (at least
/// `min_gap` wide) around `centre` of at least `GUTTER_ROWS` other rows.
fn gutter_band(
    builds: &[LineBuild],
    gaps: &[Option<(usize, f32, f32)>],
    skip: usize,
    centre: f32,
    min_gap: f32,
) -> Option<(f32, f32)> {
    let (mut left_count, mut right_count, mut row_count) = (0_usize, 0_usize, 0_usize);
    let mut left_edge = f32::NEG_INFINITY;
    let mut right_edge = f32::INFINITY;
    let mut common = (f32::NEG_INFINITY, f32::INFINITY);
    for (k, line) in builds.iter().enumerate() {
        if k == skip {
            continue;
        }
        let b = line.bbox;
        if b.x1 <= centre {
            left_count += 1;
            left_edge = left_edge.max(b.x1);
        } else if b.x0 >= centre {
            right_count += 1;
            right_edge = right_edge.min(b.x0);
        } else if let Some((_, gap_left, gap_right)) = gaps[k]
            && gap_right - gap_left > min_gap
            && (gap_left..=gap_right).contains(&centre)
        {
            row_count += 1;
            common = (common.0.max(gap_left), common.1.min(gap_right));
        }
    }
    if left_count >= GUTTER_EVIDENCE && right_count >= GUTTER_EVIDENCE {
        Some((left_edge, right_edge))
    } else if row_count >= GUTTER_ROWS && common.0 <= common.1 {
        Some(common)
    } else {
        None
    }
}

/// A line under construction made of `members`, on `baseline`.
fn build_of(
    spans: &[Span],
    baseline: f32,
    members: Vec<(usize, BBox)>,
    fallback: f32,
) -> LineBuild {
    let mut bbox = members[0].1;
    let mut size: f32 = 0.0;
    for (i, b) in &members {
        bbox = union(bbox, *b);
        size = size.max(span_size(&spans[*i], fallback));
    }
    LineBuild {
        baseline,
        size,
        bbox,
        spans: members,
        accents: Vec::new(),
    }
}

/// Split every line that joins the two columns of a page across the gutter
/// (see the module documentation) into its left and right part. Both parts
/// keep the line's baseline, so the order of `builds` by baseline is kept.
/// `width` is the page width. Runs before accents are attached.
fn split_fused_rows(
    spans: &[Span],
    builds: Vec<LineBuild>,
    fallback: f32,
    width: f32,
) -> Vec<LineBuild> {
    if builds.is_empty() {
        return builds;
    }
    let sorted: Vec<Vec<(usize, BBox)>> = builds
        .iter()
        .map(|build| sorted_members(spans, build))
        .collect();
    let gaps: Vec<Option<(usize, f32, f32)>> = sorted
        .iter()
        .map(|members| widest_gap(members.as_slice()))
        .collect();
    let space = typical_space(&sorted, &builds, fallback);
    let min_gap = GUTTER_SPACES * space;
    let mut left = f32::INFINITY;
    let mut right = f32::NEG_INFINITY;
    for build in &builds {
        left = left.min(build.bbox.x0);
        right = right.max(build.bbox.x1);
    }
    let middles = [0.5 * width, left.midpoint(right)];
    let slack = MIDLINE_SLACK * width;

    let mut cuts: Vec<Option<usize>> = vec![None; builds.len()];
    for (k, build) in builds.iter().enumerate() {
        let Some((pos, gap_left, gap_right)) = gaps[k] else {
            continue;
        };
        let straddles = middles
            .iter()
            .any(|m| (gap_left - slack..=gap_right + slack).contains(m));
        if gap_right - gap_left <= min_gap || !straddles {
            continue;
        }
        let members = &sorted[k];
        let (head, tail) = members.split_at(pos);
        if word_count(spans, head, fallback) < GUTTER_WORDS
            || word_count(spans, tail, fallback) < GUTTER_WORDS
        {
            continue;
        }
        let spanning = build.bbox.x1 - build.bbox.x0 > SPANNING_PAGE * width;
        if spanning {
            let centre = gap_left.midpoint(gap_right);
            let Some((band_left, band_right)) = gutter_band(&builds, &gaps, k, centre, min_gap)
            else {
                continue;
            };
            if gap_left > band_left + GUTTER_COVER || gap_right < band_right - GUTTER_COVER {
                continue;
            }
        }
        cuts[k] = Some(pos);
    }
    if cuts.iter().all(Option::is_none) {
        return builds;
    }

    let mut out: Vec<LineBuild> = Vec::with_capacity(builds.len() + 1);
    for ((build, members), cut) in builds.into_iter().zip(sorted).zip(cuts) {
        let Some(pos) = cut else {
            out.push(build);
            continue;
        };
        let mut head = members;
        let tail = head.split_off(pos);
        out.push(build_of(spans, build.baseline, head, fallback));
        out.push(build_of(spans, build.baseline, tail, fallback));
    }
    out
}

/// Lines grouped from a page's spans: the horizontal lines and the
/// vertical lines outside the margin band sorted top-to-bottom, the
/// vertical lines in the margin band (top-to-bottom) and the number of
/// accent-only spans that sit over no glyph span.
#[derive(Default)]
struct Grouped {
    lines: Vec<Line>,
    margin: Vec<Line>,
    unattached: usize,
    accent_skipped: bool,
    vertical_limited: bool,
    horizontal_limit: Option<line_index::Limit>,
}

/// Group spans into lines by shared baseline and horizontal proximity, with
/// no column ordering. Blank spans and spans without a finite box are
/// skipped. Every returned line has a box, `column == 0`, its span indices
/// in visual order and its text joined as described in `finish_line`. An
/// accent-only span joins the line of the glyph it sits over (see the
/// module documentation); over no glyph it is an ordinary span. A span
/// ending in an accent glyph has that accent composed onto the letter of
/// the overlapping span under it, if there is one. Vertical text never
/// joins a horizontal line and a line fusing two columns is split at the
/// gutter (see the module documentation); the page width is taken as the
/// right edge of the text plus its left margin. The lines are sorted
/// top-to-bottom, except that vertical lines in the page margin come last.
pub fn group_lines(spans: &[Span]) -> Vec<Line> {
    let grouped = group_spans(spans, None);
    let mut lines = grouped.lines;
    lines.extend(grouped.margin);
    lines
}

/// [`group_lines`] on a page `page_width` wide (estimated from the spans
/// when `None` or not a positive number), with the vertical margin lines
/// kept apart.
fn group_spans(spans: &[Span], page_width: Option<f32>) -> Grouped {
    group_spans_with_horizontal_budget(spans, page_width, MAX_HORIZONTAL_WORK)
}

fn group_spans_with_horizontal_budget(
    spans: &[Span],
    page_width: Option<f32>,
    horizontal_work: usize,
) -> Grouped {
    let all = positioned(spans);
    if all.is_empty() {
        return Grouped::default();
    }
    let fallback = typical_size(spans, &all);
    let width = positive(page_width).unwrap_or_else(|| {
        let mut left = f32::INFINITY;
        let mut right = f32::NEG_INFINITY;
        for (_, b) in &all {
            left = left.min(b.x0);
            right = right.max(b.x1);
        }
        right + left.max(0.0)
    });
    let (vertical, mut candidates) = split_vertical(spans, all, fallback, width);
    let mut inner: Vec<Line> = Vec::new();
    let mut margin: Vec<Line> = Vec::new();
    let (vertical, vertical_limited) = vertical_lines(spans, &vertical);
    for line in vertical {
        if line.bbox.is_some_and(|b| in_margin(b, width)) {
            margin.push(line);
        } else {
            inner.push(line);
        }
    }
    margin.sort_by(line_top_first);
    // The body text sets the typical size, not a large margin stamp.
    let fallback = if candidates.is_empty() {
        fallback
    } else {
        typical_size(spans, &candidates)
    };

    let largest = candidates
        .iter()
        .map(|(i, _)| span_size(&spans[*i], fallback))
        .fold(fallback, f32::max);
    candidates.sort_by(baseline_first);

    // Each accent can inspect all candidate members while finding its base
    // and all characters of that base while inserting its mark. Bound that
    // attacker-controlled product before doing either repeated scan. When
    // over budget, group accent glyphs like ordinary text so no content is
    // lost and the rest of reading-order recovery remains available.
    let character_count = candidates.iter().fold(0usize, |total, (i, _)| {
        total.saturating_add(spans[*i].text.chars().count())
    });
    let accent_count = candidates.iter().fold(0usize, |total, (i, _)| {
        let text = spans[*i].text.trim();
        let accent_only = !text.is_empty() && text.chars().all(|ch| combining_accent(ch).is_some());
        let trailing = text
            .chars()
            .next_back()
            .is_some_and(|ch| !is_combining(ch) && combining_accent(ch).is_some())
            && !accent_only;
        total.saturating_add(usize::from(accent_only || trailing))
    });
    let accent_work = accent_count.saturating_mul(candidates.len().saturating_add(character_count));
    let compose_accents = accent_work <= MAX_ACCENT_WORK;

    let mut builds: Vec<LineBuild> = Vec::new();
    let mut line_index = LineIndex::new(horizontal_work);
    let mut accents: Vec<(usize, BBox, String)> = Vec::new();
    // Spans ending in an accent glyph: span index, the byte offset of the
    // tail and its marks.
    let mut tails: Vec<(usize, usize, String)> = Vec::new();
    for (i, bbox) in &candidates {
        let text = &spans[*i].text;
        // Accent-only spans use the later glyph-attachment pass. If its
        // budget is exhausted, preserve each accent separately instead of
        // merging it into an ordinary line without its intended base glyph.
        if let Some(marks) = accent_marks(text) {
            accents.push((*i, *bbox, marks));
            continue;
        }
        let size = span_size(&spans[*i], fallback);
        if let Some(k) = line_index.find(&builds, *bbox, size, largest) {
            let line = &mut builds[k];
            line.bbox = union(line.bbox, *bbox);
            line.size = line.size.max(size);
            line.spans.push((*i, *bbox));
            line_index.update(k, line);
        } else {
            builds.push(LineBuild {
                baseline: bbox.y0,
                size,
                bbox: *bbox,
                spans: vec![(*i, *bbox)],
                accents: Vec::new(),
            });
            line_index.update(builds.len() - 1, builds.last().expect("just appended"));
        }
        if compose_accents && let Some((cut, marks)) = trailing_accent(text) {
            tails.push((*i, cut, marks));
        }
    }
    let mut builds = split_fused_rows(spans, builds, fallback, width);

    // The letter under a trailing accent was kerned back under it, so its
    // span overlaps the tail of the accent's span: no slack, and the
    // accent's own span is never the base. Without such a span the text
    // stays as shown.
    for (i, cut, marks) in tails {
        let Some(k) = builds
            .iter()
            .position(|line| line.spans.iter().any(|(j, _)| *j == i))
        else {
            continue;
        };
        let line = &mut builds[k];
        let Some(host) = line.spans.iter().find_map(|(j, b)| (*j == i).then_some(*b)) else {
            continue;
        };
        let bbox = tail_box(&spans[i].text, cut, host);
        if let Some(m) = base_member(&line.spans, centre_x(bbox), 0.0, Some(i)) {
            line.accents.push(Accent {
                index: i,
                bbox,
                base: line.spans[m].0,
                marks,
                cut: Some(cut),
            });
        }
    }

    // An accent glyph sits above its letter, so baseline order reaches it
    // before the letter's line exists; place accents once all lines do.
    // `builds[..glyph_lines]` keeps the descending baseline order the
    // windowed searches rely on; lines added below it are accents alone.
    let glyph_lines = builds.len();
    let mut unattached: usize = 0;
    for (i, bbox, marks) in accents {
        let size = span_size(&spans[i], fallback);
        if !compose_accents {
            builds.push(LineBuild {
                baseline: bbox.y0,
                size,
                bbox,
                spans: vec![(i, bbox)],
                accents: Vec::new(),
            });
            continue;
        }
        let below = is_below_mark(&marks);
        if let Some((k, base)) = find_base_line(&builds[..glyph_lines], bbox, size, largest, below)
        {
            let line = &mut builds[k];
            line.bbox = union(line.bbox, bbox);
            line.accents.push(Accent {
                index: i,
                bbox,
                base,
                marks,
                cut: None,
            });
        } else if let Some(k) = find_same_baseline(&builds[..glyph_lines], bbox, size, largest) {
            let line = &mut builds[k];
            line.bbox = union(line.bbox, bbox);
            line.size = line.size.max(size);
            line.spans.push((i, bbox));
        } else {
            unattached += 1;
            builds.push(LineBuild {
                baseline: bbox.y0,
                size,
                bbox,
                spans: vec![(i, bbox)],
                accents: Vec::new(),
            });
        }
    }

    let mut lines: Vec<Line> = builds
        .into_iter()
        .map(|build| finish_line(spans, build, fallback))
        .collect();
    lines.extend(inner);
    lines.sort_by(line_top_first);
    Grouped {
        lines,
        margin,
        unattached,
        accent_skipped: !compose_accents,
        vertical_limited,
        horizontal_limit: line_index.limit,
    }
}

/// Position in `idx` (already sorted top-to-bottom) at which the widest
/// horizontal whitespace band wider than `min_gap` starts, among the
/// positions `allowed` accepts, if any.
fn widest_row_gap(
    boxes: &[BBox],
    idx: &[usize],
    min_gap: f32,
    allowed: impl Fn(usize) -> bool,
) -> Option<usize> {
    let mut bottom = boxes[idx[0]].y0;
    let mut best: Option<(usize, f32)> = None;
    for (pos, &i) in idx.iter().enumerate().skip(1) {
        let gap = bottom - boxes[i].y1;
        if gap > min_gap && allowed(pos) && best.is_none_or(|(_, g)| gap > g) {
            best = Some((pos, gap));
        }
        bottom = bottom.min(boxes[i].y0);
    }
    best.map(|(pos, _)| pos)
}

/// Position in `by_top` (sorted top-to-bottom) at which the widest
/// horizontal whitespace band wider than `min_gap` starts, if any.
fn row_cut(boxes: &[BBox], by_top: &[usize], min_gap: f32) -> Option<usize> {
    widest_row_gap(boxes, by_top, min_gap, |_| true)
}

/// Number of margin lines at the top and at the bottom of `idx` (already
/// sorted top-to-bottom). A margin line is either spanning (wider than
/// `SPANNING_WIDTH` of the region's width: a title, running header or
/// footer) or narrow and centred (narrower than `GUTTER_WIDTH` of the width
/// with its centre within `GUTTER_OFFSET` of the width from the region's
/// midpoint: a page number or short footer in the gutter).
fn margin_runs(boxes: &[BBox], idx: &[usize]) -> (usize, usize) {
    let mut left = f32::INFINITY;
    let mut right = f32::NEG_INFINITY;
    for &i in idx {
        left = left.min(boxes[i].x0);
        right = right.max(boxes[i].x1);
    }
    let width = right - left;
    let middle = left.midpoint(right);
    let is_margin = |i: usize| {
        let b = boxes[i];
        let w = b.x1 - b.x0;
        let centred = (centre_x(b) - middle).abs() <= GUTTER_OFFSET * width;
        w > SPANNING_WIDTH * width || (w < GUTTER_WIDTH * width && centred)
    };
    let top_run = idx.iter().take_while(|&&i| is_margin(i)).count();
    let bottom_run = idx.iter().rev().take_while(|&&i| is_margin(i)).count();
    (top_run, bottom_run)
}

/// Horizontal extent (`x0` left, `x1` right) of the boxes in `group`.
fn x_extent(boxes: &[BBox], group: &[usize]) -> (f32, f32) {
    let mut left = f32::INFINITY;
    let mut right = f32::NEG_INFINITY;
    for &i in group {
        left = left.min(boxes[i].x0);
        right = right.max(boxes[i].x1);
    }
    (left, right)
}

/// Positions in `by_top` (sorted top-to-bottom) of the first and of the
/// last row gap wider than `min_gap` when the lines above the first (below
/// the last) make a page header (footer) band: at most `BAND_LINES` lines,
/// each narrower than `BAND_WIDTH` of the region's width, with at least
/// `COEXIST_LINES` lines of `BRIDGE_COLUMN` of the width on the other side
/// of the gap. `0` and `by_top.len()` stand for no band.
fn furniture_bands(
    boxes: &[BBox],
    by_top: &[usize],
    min_gap: f32,
    split_x: Option<f32>,
) -> (usize, usize) {
    let n = by_top.len();
    let Some(&top_line) = by_top.first() else {
        return (0, n);
    };
    let (left, right) = x_extent(boxes, by_top);
    let width = right - left;
    let mut first: Option<usize> = None;
    let mut last: Option<usize> = None;
    let mut bottom = boxes[top_line].y0;
    for (pos, &i) in by_top.iter().enumerate().skip(1) {
        if bottom - boxes[i].y1 > min_gap {
            if first.is_none() {
                first = Some(pos);
            }
            last = Some(pos);
        }
        bottom = bottom.min(boxes[i].y0);
    }
    let short = |i: &usize| boxes[*i].x1 - boxes[*i].x0 < BAND_WIDTH * width;
    let prose = |group: &[usize]| {
        let wide = group
            .iter()
            .filter(|&&i| boxes[i].x1 - boxes[i].x0 >= BRIDGE_COLUMN * width)
            .count();
        wide >= COEXIST_LINES
    };
    let head = first
        .filter(|&p| p <= BAND_LINES && by_top[..p].iter().all(short) && prose(&by_top[p..]))
        .filter(|&p| {
            !split_x.is_some_and(|x| {
                column_heading_band(boxes, &by_top[..p], &by_top[p..], x, min_gap / ROW_GAP)
            })
        })
        .unwrap_or(0);
    let foot = last
        .filter(|&p| n - p <= BAND_LINES && by_top[p..].iter().all(short) && prose(&by_top[..p]))
        .unwrap_or(n);
    (head, foot)
}

/// Two short headings can open the respective columns at the same height.
/// They belong with their nearby, left-aligned prose, not in a shared page
/// header (`Ethical Considerations` beside `References`, arXiv:2502.00857).
/// Require this association on both sides of a proven column split. A
/// right-aligned folio, spanning title or widely separated running head does
/// not qualify. Text is deliberately not used to identify heading names.
fn column_heading_band(
    boxes: &[BBox],
    head: &[usize],
    body: &[usize],
    split_x: f32,
    line_height: f32,
) -> bool {
    let side = |b: BBox| usize::from(b.x0 >= split_x);
    let mut left = [f32::INFINITY; 2];
    let mut top = [f32::NEG_INFINITY; 2];
    for &i in body {
        let b = boxes[i];
        let column = side(b);
        left[column] = left[column].min(b.x0);
        top[column] = top[column].max(b.y1);
    }
    let mut seen = [false; 2];
    for &i in head {
        let b = boxes[i];
        let column = side(b);
        let gap = b.y0 - top[column];
        if (b.x0 - left[column]).abs() > COLUMN_HEADING_ALIGN
            || gap < 0.0
            || gap > COLUMN_HEADING_GAP * line_height
        {
            return false;
        }
        seen[column] = true;
    }
    seen == [true, true]
}

/// Like [`row_cut`], but only a cut whose upper part or lower part consists
/// of margin lines alone (see [`margin_runs`]) or is a page header or
/// footer band (see [`furniture_bands`]) qualifies: it splits a title,
/// running header, footer or page number off the top or bottom of the
/// region and nothing else.
fn spanning_row_cut(
    boxes: &[BBox],
    by_top: &[usize],
    min_gap: f32,
    split_x: Option<f32>,
) -> Option<usize> {
    let (top_run, bottom_run) = margin_runs(boxes, by_top);
    let (head, foot) = furniture_bands(boxes, by_top, min_gap, split_x);
    let top_end = top_run.max(head);
    let bottom_start = (by_top.len() - bottom_run).min(foot);
    widest_row_gap(boxes, by_top, min_gap, |pos| {
        pos <= top_end || pos >= bottom_start
    })
}

/// Position in `by_top` (sorted top-to-bottom) at which a region bridged
/// across its gutter is cut into bands, if any; `by_left` holds the same
/// indices sorted left-to-right. The candidates are the lines that run past
/// the region's horizontal midpoint by more than `BRIDGE_REACH` of its
/// width on both sides. Without them the region must split into two
/// columns (see [`split_columns`]) with at least
/// `COEXIST_LINES` lines of `BRIDGE_COLUMN` of the width on each side, and
/// the lines that run past that gutter by `BRIDGE_REACH` of the width on
/// both sides bridge it. The cut is the widest horizontal gap, wider than
/// `bridge_gap` (a small overlap is allowed), between a bridging line and a
/// line that is not bridging, where that line has a line of the other
/// column beside it: a band of columns meets a bridging line with a row of
/// both columns, while the short last line of a full-width paragraph above
/// two columns stands alone and stays with its paragraph.
fn bridge_row_cut(
    boxes: &[BBox],
    by_top: &[usize],
    by_left: &[usize],
    params: &CutParams,
) -> Option<usize> {
    let mut left = f32::INFINITY;
    let mut right = f32::NEG_INFINITY;
    for &i in by_top {
        left = left.min(boxes[i].x0);
        right = right.max(boxes[i].x1);
    }
    let width = right - left;
    if width.is_nan() || width <= 0.0 {
        return None;
    }
    let reach = BRIDGE_REACH * width;
    let middle = left.midpoint(right);
    let rest: Vec<usize> = by_left
        .iter()
        .copied()
        .filter(|&i| boxes[i].x0 > middle - reach || boxes[i].x1 < middle + reach)
        .collect();
    if rest.len() == by_left.len() || rest.len() < 2 * COEXIST_LINES {
        return None;
    }
    let split = split_columns(boxes, &rest, params.column_gap)?;
    if !split.coexist {
        return None;
    }
    let (left_side, right_side) = rest.split_at(split.at);
    let wide = |side: &[usize]| {
        side.iter()
            .filter(|&&i| boxes[i].x1 - boxes[i].x0 >= BRIDGE_COLUMN * width)
            .count()
    };
    if wide(left_side) < COEXIST_LINES || wide(right_side) < COEXIST_LINES {
        return None;
    }
    // Lines overhanging the gutter do not move its left edge.
    let gutter_left = split.left_edge;
    // `right_side` is sorted left-to-right: its first box starts the gutter's right edge.
    let gutter_right = boxes[right_side[0]].x0;
    let bridges: Vec<bool> = by_top
        .iter()
        .map(|&i| boxes[i].x0 <= gutter_left - reach && boxes[i].x1 >= gutter_right + reach)
        .collect();
    if !bridges.contains(&true) {
        return None;
    }
    let right_of = |i: usize| boxes[i].x0 >= gutter_right;
    let paired = |x: usize| {
        rest.iter().any(|&j| {
            right_of(j) != right_of(x) && boxes[j].y0 < boxes[x].y1 && boxes[j].y1 > boxes[x].y0
        })
    };
    widest_row_gap(boxes, by_top, params.bridge_gap, |pos| {
        // The line on the side of the cut that is not bridging.
        let near = if bridges[pos] {
            by_top[pos - 1]
        } else {
            by_top[pos]
        };
        bridges[pos] != bridges[pos - 1] && paired(near)
    })
}

/// Whether a column cut exists once the margin lines at the top and bottom
/// of the region (see [`margin_runs`]) are left out; at least one must be
/// left out and at least two lines must remain. `by_top` and `by_left` hold
/// the same indices sorted top-to-bottom and left-to-right; `marks` is all
/// `false` scratch space with one slot per box, left all `false`.
fn masked_column_cut(
    boxes: &[BBox],
    by_top: &[usize],
    by_left: &[usize],
    min_gap: f32,
    marks: &mut [bool],
) -> bool {
    let (top_run, bottom_run) = margin_runs(boxes, by_top);
    if top_run + bottom_run == 0 || top_run + bottom_run + 2 > by_top.len() {
        return false;
    }
    let kept = &by_top[top_run..by_top.len() - bottom_run];
    for &i in kept {
        marks[i] = true;
    }
    let inner: Vec<usize> = by_left.iter().copied().filter(|&i| marks[i]).collect();
    for &i in kept {
        marks[i] = false;
    }
    split_columns(boxes, &inner, min_gap).is_some_and(|split| split.coexist)
}

/// Vertical extent (`y0` low, `y1` high) of the boxes in `group`.
fn y_extent(boxes: &[BBox], group: &[usize]) -> (f32, f32) {
    let mut low = f32::INFINITY;
    let mut high = f32::NEG_INFINITY;
    for &i in group {
        low = low.min(boxes[i].y0);
        high = high.max(boxes[i].y1);
    }
    (low, high)
}

/// Whether the two sides of a column cut at `at` in `idx` (sorted
/// left-to-right by [`column_cut`]) stand side by side as columns: their
/// vertical extents overlap by at least `COEXIST_OVERLAP` of the shorter
/// side's extent, and each side has at least `COEXIST_LINES` lines whose
/// vertical centre lies inside the overlapping band. Horizontally disjoint
/// blocks stacked one above the other (a right-aligned heading over
/// left-aligned prose) are not columns.
fn columns_coexist(boxes: &[BBox], idx: &[usize], at: usize) -> bool {
    if at == 0 || at >= idx.len() {
        return false;
    }
    let (left, right) = idx.split_at(at);
    let (left_low, left_high) = y_extent(boxes, left);
    let (right_low, right_high) = y_extent(boxes, right);
    let low = left_low.max(right_low);
    let high = left_high.min(right_high);
    let overlap = high - low;
    let shorter = (left_high - left_low).min(right_high - right_low);
    if overlap <= 0.0 || overlap < COEXIST_OVERLAP * shorter {
        return false;
    }
    let inside = |group: &[usize]| {
        group
            .iter()
            .filter(|&&i| {
                let centre = boxes[i].y0.midpoint(boxes[i].y1);
                (low..=high).contains(&centre)
            })
            .count()
    };
    inside(left) >= COEXIST_LINES && inside(right) >= COEXIST_LINES
}

/// Position in `by_left` (sorted left-to-right) at which the widest
/// vertical whitespace band wider than `min_gap` starts, if any.
fn column_cut(boxes: &[BBox], by_left: &[usize], min_gap: f32) -> Option<usize> {
    let &first = by_left.first()?;
    let mut right = boxes[first].x1;
    let mut best: Option<(usize, f32)> = None;
    for (pos, &i) in by_left.iter().enumerate().skip(1) {
        let gap = boxes[i].x0 - right;
        if gap > min_gap && best.is_none_or(|(_, g)| gap > g) {
            best = Some((pos, gap));
        }
        right = right.max(boxes[i].x1);
    }
    best.map(|(pos, _)| pos)
}

/// Like [`column_cut`], but up to `OVERHANG_LINES` lines of the left side
/// (one, plus one per `OVERHANG_SHARE` lines there) may run into or across
/// the gutter: an overfull line or a protruding hyphen. Each such line must
/// have its centre left of the gutter and reach past the gutter's right
/// edge by at most the gutter's width; a line centred on the gutter (a
/// gutter page number, a centred equation) or running far into the right
/// side (a caption, a fused row) is never tolerated. The gutter runs from
/// the right edge of the other lines of the left side to the left edge of
/// the right side and must be wider than `min_gap`. Returns the position in
/// `by_left` (sorted left-to-right) at which the right side starts and the
/// gutter's left edge, for the widest such gutter.
fn overhang_column_cut(boxes: &[BBox], by_left: &[usize], min_gap: f32) -> Option<(usize, f32)> {
    // The boxes seen so far with the largest right edges, largest first.
    let mut widest: Vec<usize> = Vec::with_capacity(OVERHANG_LINES + 2);
    let mut best: Option<(usize, f32, f32)> = None;
    for (pos, &i) in by_left.iter().enumerate() {
        let allowed = (pos / OVERHANG_SHARE + 1).min(OVERHANG_LINES);
        if let Some(&core) = widest.get(allowed) {
            let edge = boxes[core].x1;
            let start = boxes[i].x0;
            let gap = start - edge;
            let tolerated = widest[..allowed].iter().all(|&j| {
                let b = boxes[j];
                b.x1 <= edge || (centre_x(b) < edge && b.x1 - start <= gap)
            });
            if gap > min_gap && tolerated && best.is_none_or(|(_, g, _)| gap > g) {
                best = Some((pos, gap, edge));
            }
        }
        let x1 = boxes[i].x1;
        let slot = widest.partition_point(|&j| boxes[j].x1 >= x1);
        if slot <= OVERHANG_LINES {
            widest.insert(slot, i);
            widest.truncate(OVERHANG_LINES + 1);
        }
    }
    best.map(|(pos, _, edge)| (pos, edge))
}

/// A column cut of a region: the position in the left-to-right order at
/// which the right side starts, the gutter's left edge (the right edge of
/// the left side's lines that do not overhang the gutter) and whether the
/// two sides stand side by side (see [`columns_coexist`]).
#[derive(Clone, Copy)]
struct ColumnSplit {
    at: usize,
    left_edge: f32,
    coexist: bool,
}

/// The column cut of the region `by_left` (sorted left-to-right): the cut
/// of [`column_cut`] when its two sides stand side by side (see
/// [`columns_coexist`]), else the cut of [`overhang_column_cut`] when its
/// sides do, else the cut of [`column_cut`] (if any) marked as not
/// coexisting.
fn split_columns(boxes: &[BBox], by_left: &[usize], min_gap: f32) -> Option<ColumnSplit> {
    let strict = column_cut(boxes, by_left, min_gap).map(|at| ColumnSplit {
        at,
        left_edge: x_extent(boxes, &by_left[..at]).1,
        coexist: columns_coexist(boxes, by_left, at),
    });
    if strict.is_some_and(|split| split.coexist) {
        return strict;
    }
    if let Some((at, left_edge)) = overhang_column_cut(boxes, by_left, min_gap)
        && columns_coexist(boxes, by_left, at)
    {
        return Some(ColumnSplit {
            at,
            left_edge,
            coexist: true,
        });
    }
    strict
}

/// Whether `ch` ends a sentence or a clause for [`flow`].
fn is_terminal(ch: char) -> bool {
    matches!(ch, '.' | '!' | '?' | ':' | ';' | ')' | ']')
}

/// How strongly the text of line `prev` reads on into line `next`: +2 when
/// `prev` ends in a hyphen and `next` starts lowercase, +2 when `prev` has
/// no terminal punctuation (see [`is_terminal`]) and `next` starts
/// lowercase, +1 when `prev` has no terminal punctuation and `next` starts
/// uppercase, -1 when `prev` ends in `.` and `next` starts lowercase, else
/// 0 (also when either line is blank or `next` does not start with a
/// letter).
fn flow(prev: &str, next: &str) -> i32 {
    let Some(last) = prev.trim_end().chars().next_back() else {
        return 0;
    };
    let Some(first) = next.trim_start().chars().next() else {
        return 0;
    };
    if !first.is_alphabetic() {
        return 0;
    }
    let lower = first.is_lowercase();
    if is_terminal(last) {
        if last == '.' && lower { -1 } else { 0 }
    } else if lower {
        // A trailing hyphen is not terminal, so a hyphen before a
        // lowercase letter scores +2 here.
        2
    } else {
        i32::from(first.is_uppercase())
    }
}

/// Whether the row gap at `at` in `idx` (sorted top-to-bottom), which runs
/// across a region that a column gap at `split_x` also runs through, is to
/// be cut before the columns. The region falls into four blocks: left top,
/// right top, left bottom and right bottom. Columns first reads left top,
/// left bottom, right top; rows first reads left top, right top, then left
/// bottom, right bottom. The row gap is taken first when the text of the
/// lines at the block boundaries flows (see [`flow`]) better that way: a
/// balanced column band followed by a new two-column band (an appendix
/// ending above a bibliography) reads by rows, paragraph gaps that merely
/// line up across the columns read by columns. Ties keep columns first.
fn rows_read_first(boxes: &[BBox], texts: &[&str], idx: &[usize], at: usize, split_x: f32) -> bool {
    let (top, bottom) = idx.split_at(at);
    let is_left = |i: &&usize| boxes[**i].x0 < split_x;
    let is_right = |i: &&usize| boxes[**i].x0 >= split_x;
    let (
        Some(&left_top_end),
        Some(&right_top_start),
        Some(&left_bottom_start),
        Some(&left_bottom_end),
        Some(&right_bottom_start),
    ) = (
        top.iter().rev().find(is_left),
        top.iter().find(is_right),
        bottom.iter().find(is_left),
        bottom.iter().rev().find(is_left),
        bottom.iter().find(is_right),
    )
    else {
        return false;
    };
    let text = |i: usize| texts.get(i).copied().unwrap_or("");
    let by_columns = flow(text(left_top_end), text(left_bottom_start))
        + flow(text(left_bottom_end), text(right_top_start));
    let by_rows = flow(text(left_top_end), text(right_top_start))
        + flow(text(left_bottom_end), text(right_bottom_start));
    by_rows > by_columns
}

/// Fixed inputs of one XY-cut run: the line boxes, the text of the line of
/// each box, the thresholds, all-`false` scratch marks with one slot per
/// box, and the leaf blocks found so far.
struct XyCut<'a> {
    boxes: &'a [BBox],
    texts: &'a [&'a str],
    params: &'a CutParams,
    marks: Vec<bool>,
    out: Vec<Vec<usize>>,
}

impl XyCut<'_> {
    /// The members of `other` (a superset of `cut`) that are in `cut` and
    /// those that are not, each kept in the order of `other`.
    fn partition(&mut self, cut: &[usize], other: &[usize]) -> (Vec<usize>, Vec<usize>) {
        for &i in cut {
            self.marks[i] = true;
        }
        let (inside, outside): (Vec<usize>, Vec<usize>) =
            other.iter().copied().partition(|&i| self.marks[i]);
        for &i in cut {
            self.marks[i] = false;
        }
        (inside, outside)
    }

    /// Recursive XY-cut over the lines in `by_top` (sorted top-to-bottom)
    /// and `by_left` (the same indices sorted left-to-right). When a
    /// column gap runs through the whole region (a few lines overhanging
    /// the gutter aside, see [`split_columns`]) and the two sides stand
    /// side by side (see [`columns_coexist`]), a row gap that splits margin
    /// lines (see [`margin_runs`]) or a page header or footer band (see
    /// [`furniture_bands`]) off its top or bottom is taken before it;
    /// otherwise the widest row gap across the region is taken first
    /// only when the text reads on better by rows than by columns (see
    /// [`rows_read_first`]), so paragraph gaps that happen to line up
    /// across columns do not cut the columns into bands. When such a
    /// column gap appears only once those margin lines are left out, the
    /// row gap that splits them off is taken first. Otherwise a region
    /// whose gutter is bridged is cut into bands at the bridging lines (see
    /// [`bridge_row_cut`]). Otherwise split on the widest row gap, else on
    /// the widest column gap, else emit the indices
    /// as one leaf block sorted top-to-bottom. Both lists are sorted once
    /// at the root and split with stable partitions, which keeps the order
    /// a stable re-sort of each region would give: the two sort keys are
    /// the same pair (`x0`, `y1`), so tied boxes stay in index order.
    fn run(&mut self, mut by_top: Vec<usize>, mut by_left: Vec<usize>, depth: u32) {
        if by_top.len() > 1 && depth < MAX_DEPTH {
            let boxes = self.boxes;
            let params = self.params;
            let split = split_columns(boxes, &by_left, params.column_gap);
            let has_column = split.is_some_and(|s| s.coexist);
            let cut = split.map(|s| s.at);
            let row = if has_column {
                // The right side starts at the box at the cut.
                let split_x = cut.map(|at| boxes[by_left[at]].x0);
                spanning_row_cut(boxes, &by_top, params.row_gap, split_x).or_else(|| {
                    let x = split_x?;
                    let at = row_cut(boxes, &by_top, params.row_gap)?;
                    rows_read_first(boxes, self.texts, &by_top, at, x).then_some(at)
                })
            } else {
                let masked =
                    masked_column_cut(boxes, &by_top, &by_left, params.column_gap, &mut self.marks);
                let margin = if masked {
                    spanning_row_cut(boxes, &by_top, params.row_gap, None)
                } else {
                    None
                };
                margin
                    .or_else(|| bridge_row_cut(boxes, &by_top, &by_left, params))
                    .or_else(|| row_cut(boxes, &by_top, params.row_gap))
            };
            if let Some(at) = row {
                let lower_top = by_top.split_off(at);
                let (upper_left, lower_left) = self.partition(&by_top, &by_left);
                self.run(by_top, upper_left, depth + 1);
                self.run(lower_top, lower_left, depth + 1);
                return;
            }
            if let Some(at) = cut {
                let right_left = by_left.split_off(at);
                let (left_top, right_top) = self.partition(&by_left, &by_top);
                self.run(left_top, by_left, depth + 1);
                self.run(right_top, right_left, depth + 1);
                return;
            }
        }
        self.out.push(by_top);
    }
}

/// Order lines for reading with a recursive XY-cut over their boxes and set
/// `column` to the index of the leaf block each line ends up in. Rows split
/// on vertical whitespace wider than `ROW_GAP` median line heights and
/// columns on horizontal whitespace wider than `COLUMN_GAP` median
/// character widths (floored at 0.5 % of `page_width` against degenerate
/// character widths). Lines without a finite box, and lines beyond
/// `MAX_LINES`, keep their order and form one extra block at the end. A
/// column split through a whole region (up to `OVERHANG_LINES` lines of
/// the left column overhanging the gutter) wins over any row split except
/// one that separates spanning lines or a header or footer band at its top
/// or bottom, or one after which the line texts read on better by rows
/// than by columns. A region whose
/// gutter is bridged by lines running across it is cut into bands at those
/// lines first, whatever the whitespace around them.
pub fn order_lines(lines: Vec<Line>, page_width: f32) -> Vec<Line> {
    let mut placed: Vec<(Line, BBox)> = Vec::new();
    let mut loose: Vec<Line> = Vec::new();
    for line in lines {
        let boxed = line.bbox.filter(|b| is_finite_box(*b));
        if let Some(b) = boxed {
            placed.push((line, normalized(b)));
        } else {
            loose.push(line);
        }
    }
    placed.sort_by(|a, b| top_first(&a.1, &b.1));
    if placed.len() > MAX_LINES {
        let extra = placed.split_off(MAX_LINES);
        loose.extend(extra.into_iter().map(|(line, _)| line));
    }

    let mut heights: Vec<f32> = placed.iter().map(|(_, b)| b.y1 - b.y0).collect();
    let line_height = positive(median(&mut heights)).unwrap_or(FALLBACK_SIZE);
    let mut widths: Vec<f32> = placed
        .iter()
        .map(|(line, b)| (b.x1 - b.x0) / line.text.chars().count().max(1) as f32)
        .collect();
    let default_char = line_height / 2.0;
    let char_width = positive(median(&mut widths)).unwrap_or(default_char);
    let floor = 0.005 * page_width.max(0.0);
    let params = CutParams {
        row_gap: ROW_GAP * line_height,
        column_gap: (COLUMN_GAP * char_width).max(floor),
        bridge_gap: -BRIDGE_OVERLAP * line_height,
    };

    let boxes: Vec<BBox> = placed.iter().map(|(_, b)| *b).collect();
    let texts: Vec<&str> = placed.iter().map(|(line, _)| line.text.as_str()).collect();
    let mut blocks: Vec<Vec<usize>> = Vec::new();
    if !boxes.is_empty() {
        let mut by_top: Vec<usize> = (0..boxes.len()).collect();
        by_top.sort_by(|a, b| top_first(&boxes[*a], &boxes[*b]));
        let mut by_left: Vec<usize> = by_top.clone();
        by_left.sort_by(|a, b| left_first(&boxes[*a], &boxes[*b]));
        let mut cutter = XyCut {
            boxes: &boxes,
            texts: &texts,
            params: &params,
            marks: vec![false; boxes.len()],
            out: Vec::new(),
        };
        cutter.run(by_top, by_left, 0);
        blocks = cutter.out;
    }

    let mut slots: Vec<Option<Line>> = placed.into_iter().map(|(l, _)| Some(l)).collect();
    let mut ordered: Vec<Line> = Vec::with_capacity(slots.len() + loose.len());
    for (col, block) in blocks.iter().enumerate() {
        let column = u32::try_from(col).unwrap_or(u32::MAX);
        for &i in block {
            if let Some(mut line) = slots[i].take() {
                line.column = column;
                ordered.push(line);
            }
        }
    }
    let loose_column = u32::try_from(blocks.len()).unwrap_or(u32::MAX);
    for mut line in loose {
        line.column = loose_column;
        ordered.push(line);
    }
    ordered
}

/// Separator between two consecutive ordered lines: a paragraph break
/// between blocks or across a vertical gap wider than `PARAGRAPH_GAP`
/// median line heights, else a line break.
fn separator(prev: &Line, cur: &Line, line_height: f32) -> &'static str {
    if prev.column != cur.column {
        return "\n\n";
    }
    let (Some(p), Some(c)) = (prev.bbox, cur.bbox) else {
        return "\n";
    };
    if p.y0 - c.y1 > PARAGRAPH_GAP * line_height {
        "\n\n"
    } else {
        "\n"
    }
}

/// A turn of the page frame under `/Rotate`: the clockwise angle (90, 180
/// or 270) the page is displayed at and the page's unturned size.
#[derive(Clone, Copy)]
struct Turn {
    degrees: i32,
    width: f32,
    height: f32,
}

impl Turn {
    /// The turn of `page`; `None` unless its rotation is 90, 180 or 270
    /// (modulo 360).
    fn of(page: &PageText) -> Option<Self> {
        let degrees = page.rotation.rem_euclid(360);
        let turn = Self {
            degrees,
            width: page.width,
            height: page.height,
        };
        matches!(degrees, 90 | 180 | 270).then_some(turn)
    }

    /// Width of the turned frame (the page's height after a quarter turn).
    fn frame_width(self) -> f32 {
        if self.degrees == 180 {
            self.width
        } else {
            self.height
        }
    }

    /// The user-space point `(x, y)` in the turned frame, or with `back`
    /// the turned-frame point `(x, y)` in user space.
    fn point(self, x: f32, y: f32, back: bool) -> (f32, f32) {
        match (self.degrees, back) {
            (90, false) => (y, self.width - x),
            (90, true) => (self.width - y, x),
            (270, false) => (self.height - y, x),
            (270, true) => (y, self.height - x),
            (180, _) => (self.width - x, self.height - y),
            _ => (x, y),
        }
    }

    /// The box `b` in the turned frame, or back in user space with `back`.
    fn bbox(self, b: BBox, back: bool) -> BBox {
        let first = self.point(b.x0, b.y0, back);
        let second = self.point(b.x1, b.y1, back);
        BBox {
            x0: first.0.min(second.0),
            y0: first.1.min(second.1),
            x1: first.0.max(second.0),
            y1: first.1.max(second.1),
        }
    }

    /// Copies of `spans`, in the same order, with every finite box turned;
    /// a box that is not finite is dropped.
    fn spans(self, spans: &[Span]) -> Vec<Span> {
        spans
            .iter()
            .map(|span| {
                let mut turned = span.clone();
                turned.bbox = span
                    .bbox
                    .filter(|b| is_finite_box(*b))
                    .map(|b| self.bbox(b, false));
                turned
            })
            .collect()
    }
}

/// Add a page warning unless the same text is already present.
fn push_warning(page: &mut PageText, warning: String) {
    if !page.warnings.contains(&warning) {
        page.warnings.push(warning);
    }
}

/// Fill `page.lines` and `page.text` from `page.spans`. Idempotent: lines
/// and text are rebuilt from scratch and warnings are never duplicated.
/// On a page with `/Rotate` 90, 180 or 270 the spans are grouped and
/// ordered in the frame the page is displayed in (for 90, `(x, y)` turns to
/// `(y, width - x)` in a frame `height` wide), so lines follow the rotated
/// baselines; the spans keep their boxes, the line boxes are turned back
/// into user space, and the warning `page N rotated R: ordered in the
/// turned frame` is added. Any other non-zero rotation is only noted as a
/// warning and the coordinates are used as supplied. Non-blank spans without geometry are appended
/// at the end, one line each in content-stream order, as their own block.
/// An accent-only span that sits over no glyph is left as its own line and
/// noted with the warning `unattached accent glyph at page N: M span(s)`.
/// If accent composition would exceed its per-page work limit, accent
/// glyphs remain verbatim and a warning records that composition was skipped.
/// Vertical text in the page margin (the rotated `arXiv` stamp) is placed
/// after the ordered lines, before the spans without geometry, as a block
/// of its own, and noted with the warning
/// `vertical margin text at page N: M line(s) placed last`.
pub fn order_page(page: &mut PageText) {
    page.lines.clear();
    page.text.clear();
    let turn = Turn::of(page);
    if let Some(t) = turn {
        let number = page.page;
        let degrees = t.degrees;
        let msg = format!("page {number} rotated {degrees}: ordered in the turned frame");
        push_warning(page, msg);
    } else if page.rotation != 0 {
        let rotation = page.rotation;
        let msg = format!("page rotation {rotation}: coordinates used unrotated");
        push_warning(page, msg);
    }

    let turned: Option<Vec<Span>> = turn.map(|t| t.spans(&page.spans));
    let width = turn.map_or(page.width, Turn::frame_width);
    let Grouped {
        lines: grouped,
        margin,
        unattached,
        accent_skipped,
        vertical_limited,
        horizontal_limit,
    } = group_spans(
        turned.as_deref().unwrap_or(page.spans.as_slice()),
        Some(width),
    );
    if unattached > 0 {
        let number = page.page;
        let msg = format!("unattached accent glyph at page {number}: {unattached} span(s)");
        push_warning(page, msg);
    }
    if vertical_limited {
        push_warning(
            page,
            "resource_limit: vertical grouping budget exhausted; remaining spans kept separate"
                .to_string(),
        );
    }
    if let Some(limit) = horizontal_limit {
        push_warning(page, limit.warning());
    }
    if accent_skipped {
        let number = page.page;
        let msg = format!(
            "resource_limit: accent composition skipped at page {number}: work limit exceeded"
        );
        push_warning(page, msg);
    }
    if grouped.len() > MAX_LINES {
        let n = grouped.len();
        let msg = format!(
            "resource_limit: too many lines: {n} > {MAX_LINES}; the rest is appended unordered"
        );
        push_warning(page, msg);
    }
    if !margin.is_empty() {
        let number = page.page;
        let n = margin.len();
        let msg = format!("vertical margin text at page {number}: {n} line(s) placed last");
        push_warning(page, msg);
    }
    let ordered = order_lines(grouped, width);
    let mut heights: Vec<f32> = ordered
        .iter()
        .filter_map(|l| l.bbox)
        .map(|b| b.y1 - b.y0)
        .collect();
    let line_height = positive(median(&mut heights)).unwrap_or(FALLBACK_SIZE);

    let mut text = String::new();
    for (k, line) in ordered.iter().enumerate() {
        if k > 0 {
            text.push_str(separator(&ordered[k - 1], line, line_height));
        }
        text.push_str(&line.text);
    }

    let mut loose: Vec<(u32, usize)> = Vec::new();
    for (i, span) in page.spans.iter().enumerate() {
        let has_box = span.bbox.is_some_and(is_finite_box);
        if !has_box && !span.text.trim().is_empty() {
            loose.push((span.seq, i));
        }
    }
    loose.sort_unstable();

    let mut lines = ordered;
    let margin_column = lines.last().map_or(0, |l| l.column.saturating_add(1));
    for (k, mut line) in margin.into_iter().enumerate() {
        if !text.is_empty() {
            text.push_str(if k == 0 { "\n\n" } else { "\n" });
        }
        text.push_str(&line.text);
        line.column = margin_column;
        lines.push(line);
    }
    if !loose.is_empty() {
        let next_column = lines.last().map_or(0, |l| l.column.saturating_add(1));
        for (k, (_, i)) in loose.iter().enumerate() {
            let line_text = page.spans[*i].text.trim().to_string();
            if !text.is_empty() {
                text.push_str(if k == 0 { "\n\n" } else { "\n" });
            }
            text.push_str(&line_text);
            lines.push(Line {
                text: line_text,
                bbox: None,
                column: next_column,
                spans: vec![u32::try_from(*i).unwrap_or(u32::MAX)],
                role: crate::schema::default_line_role(),
            });
        }
        let n = loose.len();
        let msg = format!("spans without geometry: {n}");
        push_warning(page, msg);
    }
    if let Some(t) = turn {
        for line in &mut lines {
            line.bbox = line.bbox.map(|b| t.bbox(b, true));
        }
    }
    page.lines = lines;
    page.text = text;
}

/// Fill `page.lines` and `page.text` for a backend that already emits spans
/// in reading order (`Extractor::provides_reading_order`): one line per
/// non-blank span in `seq` order (ties keep their stored order), column 0,
/// the span's box, text trimmed of surrounding whitespace; `page.text` is the
/// line texts joined by `\n`. No geometry is consulted. Idempotent.
pub fn lines_in_backend_order(page: &mut PageText) {
    let mut order: Vec<(u32, usize)> = page
        .spans
        .iter()
        .enumerate()
        .filter(|(_, span)| !span.text.trim().is_empty())
        .map(|(i, span)| (span.seq, i))
        .collect();
    order.sort_unstable();
    let mut lines: Vec<Line> = Vec::with_capacity(order.len());
    for (_, i) in order {
        let span = &page.spans[i];
        lines.push(Line {
            text: span.text.trim().to_string(),
            bbox: span.bbox,
            column: 0,
            spans: vec![u32::try_from(i).unwrap_or(u32::MAX)],
            role: crate::schema::default_line_role(),
        });
    }
    let texts: Vec<&str> = lines.iter().map(|line| line.text.as_str()).collect();
    page.text = texts.join("\n");
    page.lines = lines;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    fn span(text: &str, x0: f32, y0: f32, x1: f32, y1: f32, seq: u32) -> Span {
        Span {
            text: text.to_string(),
            bbox: Some(BBox { x0, y0, x1, y1 }),
            font: None,
            size: Some(10.0),
            seq,
        }
    }

    fn sized(text: &str, x0: f32, y0: f32, x1: f32, y1: f32, size: f32, seq: u32) -> Span {
        Span {
            text: text.to_string(),
            bbox: Some(BBox { x0, y0, x1, y1 }),
            font: None,
            size: Some(size),
            seq,
        }
    }

    fn loose_span(text: &str, seq: u32) -> Span {
        Span {
            text: text.to_string(),
            bbox: None,
            font: None,
            size: None,
            seq,
        }
    }

    fn page_with(spans: Vec<Span>) -> PageText {
        let mut page = PageText::new(1, 612.0, 792.0, 0);
        page.spans = spans;
        page
    }

    #[test]
    fn horizontal_budget_preserves_every_remaining_span() {
        let spans: Vec<_> = (0..12)
            .map(|i| {
                let x = i as f32 * 9.0;
                span("a", x, 100.0, x + 8.0, 110.0, i)
            })
            .collect();
        let complete = group_spans(&spans, Some(612.0));
        assert!(complete.horizontal_limit.is_none());
        assert_eq!(complete.lines.len(), 1);
        let limited = group_spans_with_horizontal_budget(&spans, Some(612.0), 0);
        assert!(limited.horizontal_limit.is_some());
        let mut kept: Vec<_> = limited
            .lines
            .iter()
            .flat_map(|line| &line.spans)
            .copied()
            .collect();
        kept.sort_unstable();
        assert_eq!(kept, (0..12).collect::<Vec<_>>());
        assert_eq!(
            limited
                .lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<String>(),
            "a".repeat(12)
        );
    }

    #[test]
    fn horizontal_index_limit_is_visible_on_the_page_without_dropping_text() {
        let count = MAX_LINES + 1;
        let mut page = PageText::new(1, count as f32 * 40.0 + 100.0, 792.0, 0);
        page.spans = (0..count)
            .map(|i| {
                let x = i as f32 * 40.0;
                span("a", x, 100.0, x + 10.0, 110.0, i as u32)
            })
            .collect();
        order_page(&mut page);
        assert_eq!(page.lines.len(), count);
        assert_eq!(page.extraction_status(), crate::schema::Status::Partial);
        assert_eq!(page.text.chars().filter(|&ch| ch == 'a').count(), count);
        assert_eq!(
            page.warnings
                .iter()
                .filter(|w| w.starts_with("resource_limit: horizontal grouping"))
                .count(),
            1
        );
        let mut kept: Vec<_> = page
            .lines
            .iter()
            .flat_map(|line| &line.spans)
            .copied()
            .collect();
        kept.sort_unstable();
        assert_eq!(kept, (0..count as u32).collect::<Vec<_>>());
    }

    fn texts(page: &PageText) -> Vec<&str> {
        page.lines.iter().map(|l| l.text.as_str()).collect()
    }

    /// `rows` lines per column, 18 pt apart from `top` downwards; the right
    /// column is emitted first in the content stream.
    fn two_columns(rows: u32, top: f32, seq: &mut u32) -> Vec<Span> {
        let mut spans = Vec::new();
        for k in 0..rows {
            let y0 = top - 18.0 * k as f32;
            let text = format!("right row {k} of the two column body text goes here");
            spans.push(span(&text, 320.0, y0, 560.0, y0 + 10.0, *seq));
            *seq += 1;
        }
        for k in 0..rows {
            let y0 = top - 18.0 * k as f32;
            let text = format!("left row {k} of the two column body text goes here");
            spans.push(span(&text, 50.0, y0, 290.0, y0 + 10.0, *seq));
            *seq += 1;
        }
        spans
    }

    #[test]
    fn median_of_odd_even_and_empty() {
        assert!(approx(median(&mut [3.0, 1.0, 2.0]).unwrap(), 2.0));
        assert!(approx(median(&mut [4.0, 1.0, 3.0, 2.0]).unwrap(), 2.5));
        let mut empty: [f32; 0] = [];
        assert!(median(&mut empty).is_none());
    }

    #[test]
    fn single_column_order_and_paragraph_break() {
        let mut page = page_with(vec![
            span("Third line", 50.0, 650.0, 300.0, 660.0, 2),
            span("First line", 50.0, 700.0, 300.0, 710.0, 0),
            span("Second line", 50.0, 688.0, 300.0, 698.0, 1),
        ]);
        order_page(&mut page);
        assert_eq!(texts(&page), ["First line", "Second line", "Third line"]);
        assert_eq!(page.text, "First line\nSecond line\n\nThird line");
        assert_eq!(page.lines[0].column, page.lines[1].column);
        assert_ne!(page.lines[1].column, page.lines[2].column);
        let bbox = page.lines[0].bbox.unwrap();
        assert!(approx(bbox.x0, 50.0) && approx(bbox.y1, 710.0));
        assert!(page.warnings.is_empty());
    }

    #[test]
    fn title_then_left_column_then_right_column() {
        let mut seq = 0;
        let mut spans = two_columns(11, 700.0, &mut seq);
        let mut title = span("Title Across Columns", 150.0, 750.0, 450.0, 762.0, seq);
        title.size = Some(12.0);
        spans.push(title);
        let mut page = page_with(spans);
        order_page(&mut page);

        let lines = texts(&page);
        assert_eq!(lines.len(), 23);
        assert_eq!(lines[0], "Title Across Columns");
        for (k, line) in lines[1..12].iter().enumerate() {
            assert!(line.starts_with(&format!("left row {k} ")), "{line}");
        }
        for (k, line) in lines[12..].iter().enumerate() {
            assert!(line.starts_with(&format!("right row {k} ")), "{line}");
        }
        let cols: Vec<u32> = page.lines.iter().map(|l| l.column).collect();
        assert_ne!(cols[1], cols[12]);
        assert!(cols[1..12].iter().all(|c| *c == cols[1]));
        assert!(cols[12..].iter().all(|c| *c == cols[12]));
        assert!(page.text.starts_with("Title Across Columns\n\nleft row 0 "));
        assert!(page.text.contains("goes here\n\nright row 0 of"));
        assert!(!page.text.contains("goes here\n\nleft row"));
    }

    #[test]
    fn spans_out_of_stream_order_join_left_to_right() {
        let mut page = page_with(vec![
            span("world", 60.0, 700.0, 85.0, 710.0, 0),
            span("Hello", 30.0, 700.0, 55.0, 710.0, 1),
            span(",", 55.0, 700.0, 58.0, 710.0, 2),
        ]);
        order_page(&mut page);
        assert_eq!(page.lines.len(), 1);
        assert_eq!(page.lines[0].text, "Hello, world");
        assert_eq!(page.lines[0].spans, vec![1, 2, 0]);
        assert_eq!(page.text, "Hello, world");
    }

    #[test]
    fn existing_boundary_space_is_not_doubled() {
        let lines = group_lines(&[
            span("Hello ", 30.0, 700.0, 58.0, 710.0, 0),
            span("world", 60.0, 700.0, 85.0, 710.0, 1),
        ]);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "Hello world");
    }

    #[test]
    fn order_page_is_idempotent() {
        let mut seq = 0;
        let mut spans = two_columns(4, 700.0, &mut seq);
        spans.push(loose_span("no geometry", seq));
        let mut page = page_with(spans);
        page.rotation = 90;
        order_page(&mut page);
        let first_text = page.text.clone();
        let first_lines = page.lines.clone();
        let first_warnings = page.warnings.clone();
        order_page(&mut page);
        assert_eq!(page.text, first_text);
        assert_eq!(page.lines, first_lines);
        assert_eq!(page.warnings, first_warnings);
        assert_eq!(page.warnings.len(), 2);
        assert_eq!(
            page.warnings[0],
            "page 1 rotated 90: ordered in the turned frame"
        );
    }

    #[test]
    fn spans_without_bbox_go_last_with_warning() {
        let mut page = page_with(vec![
            loose_span("Loose B", 9),
            span("Body 1", 50.0, 700.0, 300.0, 710.0, 0),
            loose_span("   ", 4),
            span("Body 2", 50.0, 688.0, 300.0, 698.0, 1),
            loose_span("Loose A", 3),
        ]);
        order_page(&mut page);
        assert_eq!(texts(&page), ["Body 1", "Body 2", "Loose A", "Loose B"]);
        assert_eq!(page.text, "Body 1\nBody 2\n\nLoose A\nLoose B");
        assert!(page.lines[2].bbox.is_none());
        assert_eq!(page.lines[2].spans, vec![4]);
        assert_eq!(page.lines[3].spans, vec![0]);
        assert_ne!(page.lines[1].column, page.lines[2].column);
        assert_eq!(page.lines[2].column, page.lines[3].column);
        assert_eq!(page.warnings, ["spans without geometry: 2"]);
    }

    #[test]
    fn header_columns_footer() {
        let mut seq = 0;
        let mut spans = two_columns(3, 700.0, &mut seq);
        spans.push(span(
            "page footer with the running title and the page number",
            50.0,
            60.0,
            560.0,
            70.0,
            seq,
        ));
        spans.push(span(
            "Section heading that spans the full page width",
            50.0,
            750.0,
            560.0,
            760.0,
            seq + 1,
        ));
        let mut page = page_with(spans);
        order_page(&mut page);

        let lines = texts(&page);
        assert_eq!(lines.len(), 8);
        assert!(lines[0].starts_with("Section heading"));
        assert!(lines[1].starts_with("left row 0 "));
        assert!(lines[2].starts_with("left row 1 "));
        assert!(lines[3].starts_with("left row 2 "));
        assert!(lines[4].starts_with("right row 0 "));
        assert!(lines[5].starts_with("right row 1 "));
        assert!(lines[6].starts_with("right row 2 "));
        assert!(lines[7].starts_with("page footer"));
        let tail = "goes here\n\npage footer with the running title and the page number";
        assert!(page.text.ends_with(tail));
    }

    #[test]
    fn aligned_paragraph_gaps_do_not_cut_columns_into_bands() {
        let mut spans = Vec::new();
        let mut seq = 0;
        for k in 0..8u16 {
            let gap = if k > 3 { 20.0 } else { 0.0 };
            let y0 = 700.0 - 18.0 * f32::from(k) - gap;
            let right = format!("right row {k} of the two column body text goes here");
            spans.push(span(&right, 320.0, y0, 560.0, y0 + 10.0, seq));
            seq += 1;
            if k == 5 {
                spans.push(sized("Heading 5", 50.0, y0, 130.0, y0 + 12.0, 12.0, seq));
            } else {
                let left = format!("left row {k} of the two column body text goes here");
                spans.push(span(&left, 50.0, y0, 290.0, y0 + 10.0, seq));
            }
            seq += 1;
        }
        let mut page = page_with(spans);
        order_page(&mut page);

        let lines = texts(&page);
        assert_eq!(lines.len(), 16);
        for (k, line) in lines[..8].iter().enumerate() {
            if k == 5 {
                assert_eq!(*line, "Heading 5");
            } else {
                assert!(line.starts_with(&format!("left row {k} ")), "{line}");
            }
        }
        for (k, line) in lines[8..].iter().enumerate() {
            assert!(line.starts_with(&format!("right row {k} ")), "{line}");
        }
        let paragraph = "row 3 of the two column body text goes here\n\nleft row 4 ";
        assert!(page.text.contains(paragraph));
        assert!(page.text.contains("goes here\n\nright row 0 of"));
    }

    #[test]
    fn balanced_column_band_above_a_new_two_column_band_is_read_by_rows() {
        // An appendix set in two balanced columns, then, below a gap across
        // both, a headingless bibliography whose first entry runs on from
        // the bottom of the left column into the top of the right one.
        let mut spans = Vec::new();
        let mut seq = 0;
        for k in 0..6u16 {
            let y0 = 700.0 - 18.0 * f32::from(k);
            let right = format!("right appendix row {k} of the derivation continues with");
            spans.push(span(&right, 320.0, y0, 560.0, y0 + 10.0, seq));
            let left = format!("left appendix row {k} of the derivation continues with");
            spans.push(span(&left, 50.0, y0, 290.0, y0 + 10.0, seq + 1));
            seq += 2;
        }
        // The top band ends at y0 = 610; a gap of 1.5 line heights follows.
        let bottom = [
            (
                "[1] U. Seifert, Stochastic thermodynamics, fluctuation theorems",
                "physics 75, 126001 (2012).",
            ),
            (
                "and molecular machines, Reports on progress in",
                "[2] N. Shiraishi, An Introduction to Stochastic Thermodynamics:",
            ),
        ];
        for (k, (left, right)) in bottom.iter().enumerate() {
            let y0 = 585.0 - 18.0 * k as f32;
            spans.push(span(right, 320.0, y0, 560.0, y0 + 10.0, seq));
            spans.push(span(left, 50.0, y0, 290.0, y0 + 10.0, seq + 1));
            seq += 2;
        }
        let mut page = page_with(spans);
        order_page(&mut page);

        let lines = texts(&page);
        assert_eq!(lines.len(), 16);
        for (k, line) in lines[..6].iter().enumerate() {
            assert!(
                line.starts_with(&format!("left appendix row {k} ")),
                "{line}"
            );
        }
        for (k, line) in lines[6..12].iter().enumerate() {
            assert!(
                line.starts_with(&format!("right appendix row {k} ")),
                "{line}"
            );
        }
        assert_eq!(lines[12], bottom[0].0);
        assert_eq!(lines[13], bottom[1].0);
        assert_eq!(lines[14], bottom[0].1);
        assert_eq!(lines[15], bottom[1].1);
    }

    #[test]
    fn aligned_gap_is_kept_inside_columns_when_the_text_flows_down_them() {
        // A paragraph gap that lines up across both columns, where the left
        // column's last line runs on into the top of the right column.
        let left = [
            "Left column opens the first paragraph and",
            "carries it on over a second line until",
            "the first paragraph ends here.",
            "Second paragraph starts here with a claim",
            "that the text keeps on going across",
            "and the argument addresses risks beyond",
        ];
        let right = [
            "those generally associated with models.",
            "The right column continues the text with",
            "a closing sentence of its first paragraph.",
            "Another paragraph opens on the right and",
            "runs over its second line until it",
            "ends at the bottom of the right column.",
        ];
        let mut spans = Vec::new();
        let mut seq = 0;
        for k in 0..6u16 {
            let gap = if k > 2 { 20.0 } else { 0.0 };
            let y0 = 700.0 - 18.0 * f32::from(k) - gap;
            let i = usize::from(k);
            spans.push(span(right[i], 320.0, y0, 560.0, y0 + 10.0, seq));
            spans.push(span(left[i], 50.0, y0, 290.0, y0 + 10.0, seq + 1));
            seq += 2;
        }
        let mut page = page_with(spans);
        order_page(&mut page);

        let lines = texts(&page);
        assert_eq!(lines[..6], left);
        assert_eq!(lines[6..], right);
    }

    #[test]
    fn flow_scores_line_continuity() {
        assert_eq!(flow("stochastic thermo-", "dynamics of"), 2);
        assert_eq!(flow("reports on progress in", "physics 75"), 2);
        assert_eq!(flow("risks beyond", "those generally"), 2);
        assert_eq!(flow("as shown by", "Seifert"), 1);
        assert_eq!(flow("the bound holds.", "the next"), -1);
        assert_eq!(flow("the bound holds.", "The next"), 0);
        assert_eq!(flow("(2012).", "[2] N. Shiraishi"), 0);
        assert_eq!(flow("progress in", "[1] U. Seifert"), 0);
        assert_eq!(flow("", "text"), 0);
        assert_eq!(flow("text", "   "), 0);
        assert_eq!(flow("see Eq. (3)", "and"), 0);
    }

    #[test]
    fn running_head_and_gutter_page_number_frame_the_columns() {
        let mut spans = Vec::new();
        let mut seq = 0;
        spans.push(span(
            "Running head of the paper that spans the full text width",
            50.0,
            750.0,
            560.0,
            760.0,
            seq,
        ));
        seq += 1;
        for k in 0..6u16 {
            let gap = if k > 2 { 20.0 } else { 0.0 };
            let y0 = 700.0 - 18.0 * f32::from(k) - gap;
            let right = format!("right row {k} of the two column body text goes here");
            spans.push(span(&right, 320.0, y0, 560.0, y0 + 10.0, seq));
            let left = format!("left row {k} of the two column body text goes here");
            spans.push(span(&left, 50.0, y0, 290.0, y0 + 10.0, seq + 1));
            seq += 2;
        }
        spans.push(span("8", 300.0, 60.0, 312.0, 70.0, seq));
        let mut page = page_with(spans);
        order_page(&mut page);

        let lines = texts(&page);
        assert_eq!(lines.len(), 14);
        assert!(lines[0].starts_with("Running head"));
        for (k, line) in lines[1..7].iter().enumerate() {
            assert!(line.starts_with(&format!("left row {k} ")), "{line}");
        }
        for (k, line) in lines[7..13].iter().enumerate() {
            assert!(line.starts_with(&format!("right row {k} ")), "{line}");
        }
        assert_eq!(lines[13], "8");
        assert!(page.text.ends_with("goes here\n\n8"));
    }

    #[test]
    fn right_aligned_heading_above_left_prose_is_read_first() {
        let mut spans = Vec::new();
        for k in 0..6u16 {
            let y0 = 600.0 - 18.0 * f32::from(k);
            let text = format!("prose row {k} of the single column body text");
            spans.push(span(&text, 50.0, y0, 300.0, y0 + 10.0, u32::from(k)));
        }
        spans.push(span("Right heading", 400.0, 700.0, 560.0, 710.0, 6));
        let mut page = page_with(spans);
        order_page(&mut page);

        let lines = texts(&page);
        assert_eq!(lines.len(), 7);
        assert_eq!(lines[0], "Right heading");
        for (k, line) in lines[1..].iter().enumerate() {
            assert!(line.starts_with(&format!("prose row {k} ")), "{line}");
        }
    }

    #[test]
    fn columns_coexist_needs_vertical_overlap_and_two_lines_each() {
        let b = |x0: f32, y0: f32, x1: f32| BBox {
            x0,
            y0,
            x1,
            y1: y0 + 10.0,
        };
        // Side by side: two lines per side at the same heights.
        let side = [
            b(50.0, 700.0, 290.0),
            b(50.0, 682.0, 290.0),
            b(320.0, 700.0, 560.0),
            b(320.0, 682.0, 560.0),
        ];
        assert!(columns_coexist(&side, &[0, 1, 2, 3], 2));
        // Stacked: the right block sits wholly above the left one.
        let stacked = [
            b(50.0, 600.0, 290.0),
            b(50.0, 582.0, 290.0),
            b(400.0, 700.0, 560.0),
            b(400.0, 682.0, 560.0),
        ];
        assert!(!columns_coexist(&stacked, &[0, 1, 2, 3], 2));
        // Overlapping, but the right side has a single line.
        let single = [
            b(50.0, 700.0, 290.0),
            b(50.0, 682.0, 290.0),
            b(320.0, 691.0, 560.0),
        ];
        assert!(!columns_coexist(&single, &[0, 1, 2], 2));
        assert!(!columns_coexist(&side, &[0, 1, 2, 3], 0));
    }

    #[test]
    fn empty_page_produces_nothing() {
        let mut page = page_with(Vec::new());
        order_page(&mut page);
        assert!(page.lines.is_empty());
        assert!(page.text.is_empty());
        assert!(page.warnings.is_empty());
        assert!(order_lines(Vec::new(), 612.0).is_empty());
    }

    #[test]
    fn accent_glyph_composes_onto_the_letter_under_it() {
        // OT1: "Verdu" then the acute set 0.04 pt up, centred over the u.
        let mut page = page_with(vec![
            span("Verdu", 100.0, 700.0, 126.0, 710.0, 0),
            span("\u{B4}", 121.5, 700.04, 126.5, 710.04, 1),
        ]);
        order_page(&mut page);
        assert_eq!(texts(&page), ["Verd\u{FA}"]);
        assert_eq!(page.text, "Verd\u{FA}");
        assert_eq!(page.lines[0].spans, vec![0, 1]);
        assert!(page.warnings.is_empty());

        // A dieresis raised over a capital, and a caron as lopdf decodes it.
        let lines = group_lines(&[
            span("Ungor", 100.0, 700.0, 130.0, 710.0, 0),
            span("\u{A8}", 100.5, 702.5, 105.5, 712.5, 1),
            span("Sarka", 140.0, 700.0, 170.0, 710.0, 2),
            span("\u{2C7}", 140.2, 702.5, 145.2, 712.5, 3),
        ]);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "\u{DC}ngor \u{160}arka");
        assert_eq!(lines[0].spans, vec![0, 1, 2, 3]);
    }

    #[test]
    fn accent_shown_before_its_letter_still_attaches() {
        // pdfTeX order: the accent is shown first, then the letter starts
        // the next string; the accent's centre is over that letter.
        let mut page = page_with(vec![
            span("\u{B4}", 121.5, 700.04, 126.5, 710.04, 0),
            span("Verd", 100.0, 700.0, 121.0, 710.0, 1),
            span("u,", 121.5, 700.0, 131.0, 710.0, 2),
        ]);
        order_page(&mut page);
        assert_eq!(texts(&page), ["Verd\u{FA},"]);
        assert_eq!(page.lines[0].spans, vec![1, 2, 0]);
        assert!(page.warnings.is_empty());

        // The same with the accent over the last letter of the earlier span.
        let lines = group_lines(&[
            span("\u{B4}", 121.5, 700.04, 126.5, 710.04, 0),
            span("Verdu", 100.0, 700.0, 126.0, 710.0, 1),
        ]);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "Verd\u{FA}");
        assert_eq!(lines[0].spans, vec![1, 0]);
    }

    #[test]
    fn reference_label_stays_first_before_accented_author() {
        let mut page = page_with(vec![
            span("\u{B4}", 96.5, 700.04, 101.5, 710.04, 0),
            span("[13]", 50.0, 700.0, 68.0, 710.0, 1),
            span("Verd", 75.0, 700.0, 96.0, 710.0, 2),
            span("u,", 96.5, 700.0, 106.0, 710.0, 3),
            span("J. Smith", 109.0, 700.0, 155.0, 710.0, 4),
            span("[14] Smith, A.", 50.0, 688.0, 130.0, 698.0, 5),
        ]);
        order_page(&mut page);
        assert_eq!(
            texts(&page),
            ["[13] Verd\u{FA}, J. Smith", "[14] Smith, A."]
        );
        assert_eq!(page.text, "[13] Verd\u{FA}, J. Smith\n[14] Smith, A.");
        assert_eq!(page.lines[0].spans, vec![1, 2, 3, 0, 4]);
        assert!(page.warnings.is_empty());

        let first = page.clone();
        order_page(&mut page);
        assert_eq!(page, first);
    }

    #[test]
    fn accent_over_no_glyph_stays_separate_with_warning() {
        let mut page = page_with(vec![
            span("Body text", 50.0, 700.0, 100.0, 710.0, 0),
            span("\u{A8}", 300.0, 700.04, 305.0, 710.04, 1),
            span("\u{B4}", 60.0, 730.0, 65.0, 740.0, 2),
        ]);
        order_page(&mut page);
        assert_eq!(page.lines.len(), 3);
        assert!(texts(&page).contains(&"Body text"));
        assert!(texts(&page).contains(&"\u{A8}"));
        assert!(texts(&page).contains(&"\u{B4}"));
        assert_eq!(
            page.warnings,
            ["unattached accent glyph at page 1: 2 span(s)"]
        );

        let first = page.clone();
        order_page(&mut page);
        assert_eq!(page, first);
    }

    #[test]
    fn excessive_accent_work_keeps_glyphs_verbatim() {
        let mut spans = vec![span(&"a".repeat(1_000), 100.0, 700.0, 200.0, 710.0, 0)];
        for seq in 1..=1_000 {
            spans.push(span("^", 149.0, 700.04, 151.0, 710.04, seq));
        }
        let mut page = page_with(spans);

        order_page(&mut page);

        assert_eq!(page.text.matches('^').count(), 1_000);
        assert_eq!(page.text.matches('\u{302}').count(), 0);
        assert_eq!(
            page.warnings,
            ["resource_limit: accent composition skipped at page 1: work limit exceeded"]
        );
    }

    #[test]
    fn excessive_accent_work_keeps_separated_accent_spans_linear() {
        let spans: Vec<_> = (0..1_000)
            .map(|seq| {
                let x = seq as f32 * 100.0;
                span("^", x, 700.0, x + 2.0, 710.0, seq)
            })
            .collect();
        let mut page = page_with(spans);

        order_page(&mut page);

        assert_eq!(page.lines.len(), 1_000);
        assert!(page.lines.iter().all(|line| line.text == "^"));
        assert_eq!(
            page.warnings,
            ["resource_limit: accent composition skipped at page 1: work limit exceeded"]
        );
    }

    #[test]
    fn ascii_tilde_on_the_baseline_over_no_glyph_is_an_ordinary_span() {
        let mut page = page_with(vec![
            span("a", 100.0, 700.0, 105.0, 710.0, 0),
            span("~", 107.0, 700.0, 112.0, 710.0, 1),
            span("b", 114.0, 700.0, 119.0, 710.0, 2),
        ]);
        order_page(&mut page);
        assert_eq!(texts(&page), ["a ~ b"]);
        assert_eq!(page.lines[0].spans, vec![0, 1, 2]);
        assert!(page.warnings.is_empty());
    }

    #[test]
    fn accent_helpers() {
        assert_eq!(accent_marks("\u{B4}"), Some("\u{301}".to_string()));
        assert_eq!(accent_marks(" ~ "), Some("\u{303}".to_string()));
        assert_eq!(accent_marks("\u{308}"), Some("\u{308}".to_string()));
        assert_eq!(
            accent_marks("\u{2DC}\u{B8}"),
            Some("\u{303}\u{327}".to_string())
        );
        assert_eq!(accent_marks("a"), None);
        assert_eq!(accent_marks("~a"), None);
        assert_eq!(accent_marks("  "), None);

        let b = BBox {
            x0: 100.0,
            y0: 0.0,
            x1: 130.0,
            y1: 10.0,
        };
        assert_eq!(base_char_slot("Ungor", b, 103.0), 0);
        assert_eq!(base_char_slot("Ungor", b, 127.0), 4);
        assert_eq!(base_char_slot("Ungor", b, 500.0), 4);
        assert_eq!(base_char_slot("u", b, 115.0), 0);

        let mut piece = "ab cd".to_string();
        insert_marks(&mut piece, 2, "\u{301}");
        assert_eq!(piece, "ab\u{301} cd");
        let mut piece = "e\u{308}x".to_string();
        insert_marks(&mut piece, 0, "\u{304}");
        assert_eq!(piece, "e\u{308}\u{304}x");
        let mut piece = String::new();
        insert_marks(&mut piece, 3, "\u{301}");
        assert_eq!(piece, "\u{301}");

        let members = [
            (0, span("Verd", 100.0, 700.0, 121.0, 710.0, 0).bbox.unwrap()),
            (1, span("u,", 121.5, 700.0, 131.0, 710.0, 1).bbox.unwrap()),
        ];
        assert_eq!(base_member(&members, 124.0, 1.0, None), Some(1));
        assert_eq!(base_member(&members, 121.3, 1.0, None), Some(1));
        assert_eq!(base_member(&members, 110.0, 1.0, None), Some(0));
        assert_eq!(base_member(&members, 140.0, 1.0, None), None);
        assert_eq!(base_member(&members, 124.0, 1.0, Some(1)), None);
        assert_eq!(base_member(&members, 121.3, 1.0, Some(1)), Some(0));

        assert_eq!(trailing_accent("H\u{A8}"), Some((1, "\u{308}".to_string())));
        assert_eq!(
            trailing_accent("(R\u{A8}"),
            Some((2, "\u{308}".to_string()))
        );
        assert_eq!(
            trailing_accent("Gonz\u{B4}"),
            Some((4, "\u{301}".to_string()))
        );
        assert_eq!(trailing_accent("\u{B4}"), None);
        assert_eq!(trailing_accent("  \u{B4}"), None);
        assert_eq!(trailing_accent("older"), None);
        assert_eq!(trailing_accent("q\u{303}"), None);
        assert_eq!(trailing_accent("\u{B4}a"), None);

        let tail = tail_box("H\u{A8}", 1, b);
        assert!(approx(tail.x0, 115.0) && approx(tail.x1, 130.0));
        let tail = tail_box("Gonz\u{B4}", 4, b);
        assert!(approx(tail.x0, 124.0) && approx(tail.x1, 130.0));
        assert_eq!(tail_box("\u{B4}", 0, b), b);
    }

    #[test]
    fn accent_closing_a_string_composes_onto_the_letter_kerned_back_under_it() {
        // arXiv:2603.21379, cmr10 (OT1) at 9.9626 pt, one TJ array:
        // `[(.)-474(Th)28(us,)-346(the)-343(H\177)500(older)-343(inequalit)...]`.
        // The dieresis (OT1 code 127, 0.5 em) closes the string that holds
        // the H (0.75 em): pen 239.1146 -> 251.5679 on baseline 143.645. The
        // kern of 500 moves the pen 4.9813 pt back, so `older` starts at
        // 246.5866, its `o` (0.5 em) exactly under the accent. Same font,
        // same size, no Td, no rise; boxes as the backend sets them
        // (baseline - 0.2 size .. baseline + 0.8 size).
        let size = 9.9626;
        let (y0, y1) = (141.6525, 151.6151);
        let mut page = page_with(vec![
            sized("H\u{A8}", 239.1146, y0, 251.5679, y1, size, 0),
            sized("older", 246.5866, y0, 268.2005, y1, size, 1),
            sized("inequalit", 271.6176, y0, 309.2533, y1, size, 2),
        ]);
        order_page(&mut page);
        assert_eq!(texts(&page), ["H\u{F6}lder inequalit"]);
        assert_eq!(page.text, "H\u{F6}lder inequalit");
        assert_eq!(page.lines[0].spans, vec![0, 1, 2]);
        assert!(page.warnings.is_empty());

        // arXiv:2401.15719, cmr10 at 10.9091 pt: `(\050R\177)500(ollin)`;
        // widths 0.3889 + 0.7361 + 0.5 em, then `ollin` kerned back 0.5 em.
        let lines = group_lines(&[
            sized("(R\u{A8}", 100.0, 700.0, 117.727, 710.0, 10.9091, 0),
            sized("ollin", 112.273, 700.0, 132.88, 710.0, 10.9091, 1),
            sized("2018)", 137.2, 700.0, 160.0, 710.0, 10.9091, 2),
        ]);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "(R\u{F6}llin 2018)");
        assert_eq!(lines[0].spans, vec![0, 1, 2]);

        // arXiv:2602.02748: a font whose acute glyph is a minus sign. The
        // next string starts where the accent ends, nothing is under it,
        // so the text is kept as shown.
        let lines = group_lines(&[
            sized("d\u{B4}", 100.0, 700.0, 110.0, 710.0, 10.0, 0),
            sized("1q", 110.0, 700.0, 120.0, 710.0, 10.0, 1),
        ]);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "d\u{B4}1q");
        assert_eq!(lines[0].spans, vec![0, 1]);
    }

    #[test]
    fn backend_order_follows_seq_not_geometry() {
        // Geometry says "bottom" comes last, `seq` says it comes first.
        let mut page = page_with(vec![
            span("top", 50.0, 700.0, 100.0, 710.0, 2),
            span("  ", 50.0, 650.0, 100.0, 660.0, 1),
            span(" bottom ", 50.0, 100.0, 100.0, 110.0, 0),
            loose_span("loose", 3),
        ]);
        lines_in_backend_order(&mut page);
        assert_eq!(texts(&page), vec!["bottom", "top", "loose"]);
        assert_eq!(page.text, "bottom\ntop\nloose");
        assert!(page.lines.iter().all(|line| line.column == 0));
        assert_eq!(page.lines[0].spans, vec![2]);
        assert_eq!(page.lines[1].spans, vec![0]);
        assert_eq!(page.lines[2].spans, vec![3]);
        assert_eq!(page.lines[0].bbox, page.spans[2].bbox);
        assert_eq!(page.lines[2].bbox, None);
        assert!(page.warnings.is_empty());

        let first = page.clone();
        lines_in_backend_order(&mut page);
        assert_eq!(page, first);
    }
    #[test]
    fn cedilla_below_the_baseline_composes() {
        // "Das" then a cedilla glyph under the s, sitting 0.4 sizes below
        // the baseline (OT1 puts it well under the letter).
        let spans = vec![
            span("Das", 100.0, 700.0, 118.0, 710.0, 0),
            span("\u{00B8}", 113.0, 696.0, 117.0, 699.0, 1),
        ];
        let lines = group_lines(&spans);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0].text, "Da\u{015F}");
    }

    /// `rows` rows of two columns whose gutter (300 to 309.5) is narrower
    /// than the line reach, so the spans of a row share one line unless the
    /// gutter rule splits it; the right column is emitted first.
    fn fused_columns(rows: u16, seq: &mut u32) -> Vec<Span> {
        let mut spans = Vec::new();
        for k in 0..rows {
            let y0 = 700.0 - 18.0 * f32::from(k);
            let right =
                format!("right row {k} of the fused two column body text runs on until it ends");
            spans.push(span(&right, 309.5, y0, 560.0, y0 + 10.0, *seq));
            let left =
                format!("left row {k} of the fused two column body text runs on until it ends");
            spans.push(span(&left, 50.0, y0, 300.0, y0 + 10.0, *seq + 1));
            *seq += 2;
        }
        spans
    }

    #[test]
    fn two_columns_on_shared_baselines_stay_two_lines_per_row() {
        let mut seq = 0;
        let mut page = page_with(fused_columns(6, &mut seq));
        order_page(&mut page);

        let lines = texts(&page);
        assert_eq!(lines.len(), 12, "{lines:?}");
        for (k, line) in lines[..6].iter().enumerate() {
            assert!(line.starts_with(&format!("left row {k} ")), "{line}");
            assert!(!line.contains("right row"), "{line}");
        }
        for (k, line) in lines[6..].iter().enumerate() {
            assert!(line.starts_with(&format!("right row {k} ")), "{line}");
        }
        assert_ne!(page.lines[0].column, page.lines[6].column);
        assert!(page.warnings.is_empty());
    }

    #[test]
    fn full_width_title_with_a_wide_word_space_still_joins() {
        // The title's widest word gap (302 to 309.5, 7.5 pt) straddles the
        // midline and is wider than 2.5 spaces, but does not cover the
        // gutter (300 to 309.5) the body rows leave open.
        let mut seq = 0;
        let mut spans = fused_columns(6, &mut seq);
        spans.push(sized(
            "A Study of Fused Rows",
            60.0,
            750.0,
            302.0,
            762.0,
            12.0,
            seq,
        ));
        spans.push(sized(
            "in Two Column Layouts",
            309.5,
            750.0,
            552.0,
            762.0,
            12.0,
            seq + 1,
        ));
        let mut page = page_with(spans);
        order_page(&mut page);

        let lines = texts(&page);
        assert_eq!(lines.len(), 13, "{lines:?}");
        assert_eq!(lines[0], "A Study of Fused Rows in Two Column Layouts");
        assert_eq!(page.lines[0].spans, vec![12, 13]);
        for (k, line) in lines[1..7].iter().enumerate() {
            assert!(line.starts_with(&format!("left row {k} ")), "{line}");
        }
        for (k, line) in lines[7..].iter().enumerate() {
            assert!(line.starts_with(&format!("right row {k} ")), "{line}");
        }
    }

    #[test]
    fn gutter_rule_needs_three_words_on_each_side() {
        // Two words each side of an 8 pt gap across the midline stay one
        // line; three words each side are split there.
        let lines = group_lines(&[
            span("Figure one", 200.0, 700.0, 300.0, 710.0, 0),
            span("left half", 308.0, 700.0, 380.0, 710.0, 1),
        ]);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0].text, "Figure one left half");

        let lines = group_lines(&[
            span("Figure one two", 200.0, 700.0, 300.0, 710.0, 0),
            span("left half three", 308.0, 700.0, 380.0, 710.0, 1),
        ]);
        let texts: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, ["Figure one two", "left half three"]);
    }

    #[test]
    fn rotated_arxiv_stamp_is_kept_out_of_the_body_and_placed_last() {
        // arXiv:2604.03540, page 1: `q 0 1 -1 0 45.92 219.36 cm` then one
        // 20 pt `Tj` of the stamp, so one tall, narrow span in the left
        // margin whose foot (219.36) is level with the baseline of left
        // column row 10 (220), which starts 6 pt to its right.
        let stamp = "arXiv:2604.03540v4  [cs.RO]  29 Aug 2026";
        let mut spans = vec![sized(stamp, 21.92, 219.36, 41.92, 560.0, 20.0, 0)];
        for k in 0..12u16 {
            let y0 = 400.0 - 18.0 * f32::from(k);
            let seq = 1 + 2 * u32::from(k);
            let left = format!("left row {k} of the two column body text goes here");
            spans.push(span(&left, 48.0, y0, 300.0, y0 + 10.0, seq));
            let right = format!("right row {k} of the two column body text goes here");
            spans.push(span(&right, 312.0, y0, 564.0, y0 + 10.0, seq + 1));
        }
        let mut page = page_with(spans);
        order_page(&mut page);

        let lines = texts(&page);
        assert_eq!(lines.len(), 25, "{lines:?}");
        for (k, line) in lines[..12].iter().enumerate() {
            assert!(line.starts_with(&format!("left row {k} ")), "{line}");
            assert!(line.ends_with("goes here"), "{line}");
        }
        for (k, line) in lines[12..24].iter().enumerate() {
            assert!(line.starts_with(&format!("right row {k} ")), "{line}");
        }
        assert_eq!(lines[24], stamp);
        assert_eq!(page.lines[24].spans, vec![0]);
        assert!(page.lines[24].column > page.lines[23].column);
        assert!(page.text.ends_with(&format!("goes here\n\n{stamp}")));
        assert_eq!(
            page.warnings,
            ["vertical margin text at page 1: 1 line(s) placed last"]
        );
        assert!(page.lines[..24].iter().all(|l| !l.spans.contains(&0)));

        let first = page.clone();
        order_page(&mut page);
        assert_eq!(page, first);
    }

    #[test]
    fn stamp_of_stacked_rotated_glyphs_does_not_join_body_lines() {
        // One rotated glyph per span, running upwards from y 200: each box
        // is one em (20 pt) wide and one advance (10 pt) tall. Spaces are
        // blank spans, which leave a gap.
        let stamp = "arXiv:2507.14211v1 [cs.NI] 15 Jul 2025";
        let mut spans = Vec::new();
        let mut seq = 0;
        for (n, ch) in stamp.chars().enumerate() {
            let y0 = 200.0 + 10.0 * n as f32;
            let text = ch.to_string();
            spans.push(sized(&text, 22.0, y0, 42.0, y0 + 10.0, 20.0, seq));
            seq += 1;
        }
        for k in 0..9u16 {
            let y0 = 200.0 + 14.0 * f32::from(k);
            let text = format!("body line {k} of the single column page");
            spans.push(span(&text, 48.0, y0, 560.0, y0 + 10.0, seq));
            seq += 1;
        }
        let mut page = page_with(spans);
        order_page(&mut page);

        let lines = texts(&page);
        assert_eq!(lines.len(), 10, "{lines:?}");
        for (k, line) in lines[..9].iter().enumerate() {
            let expected = format!("body line {} of the single column page", 8 - k);
            assert_eq!(*line, expected);
        }
        assert_eq!(lines[9], stamp);
        assert_eq!(
            page.warnings,
            ["vertical margin text at page 1: 1 line(s) placed last"]
        );

        // `group_lines` lists the margin line last as well.
        let grouped = group_lines(&page.spans);
        assert_eq!(grouped.len(), 10);
        assert_eq!(grouped[9].text, stamp);
    }

    #[test]
    fn line_numbers_and_zero_advance_spans_are_not_vertical_text() {
        // Line numbers in the margin are taller than wide (upright glyphs),
        // and a zero-advance span is only one size tall.
        let mut spans = Vec::new();
        let mut seq = 0;
        for k in 1..=9u16 {
            let y0 = 700.0 - 14.0 * f32::from(k);
            spans.push(span(&k.to_string(), 20.0, y0, 25.0, y0 + 10.0, seq));
            seq += 1;
        }
        for k in 1..=9u16 {
            let y0 = 700.0 - 14.0 * f32::from(k);
            let text = format!("numbered line {k} of the body");
            spans.push(span(&text, 48.0, y0, 400.0, y0 + 10.0, seq));
            seq += 1;
        }
        spans.push(span("hidden text layer", 300.0, 500.0, 300.0, 510.0, seq));
        let mut page = page_with(spans);
        order_page(&mut page);

        let lines = texts(&page);
        assert_eq!(lines.len(), 19, "{lines:?}");
        for k in 1..=9u16 {
            assert!(lines.contains(&k.to_string().as_str()), "{lines:?}");
            let body = format!("numbered line {k} of the body");
            assert!(lines.contains(&body.as_str()), "{lines:?}");
        }
        assert!(lines.contains(&"hidden text layer"));
        assert!(page.warnings.is_empty(), "{:?}", page.warnings);
    }

    #[test]
    fn vertical_budget_warning_reaches_the_page_once() {
        let mut page = PageText::new(1, 612.0, 130_000.0, 0);
        page.spans = (0..600)
            .map(|i| {
                let y = i as f32 * 200.0;
                span("vertical", 100.0, y, 110.0, y + 100.0, i)
            })
            .collect();
        order_page(&mut page);
        order_page(&mut page);
        let warnings: Vec<_> = page
            .warnings
            .iter()
            .filter(|w| w.starts_with("resource_limit: vertical grouping"))
            .collect();
        assert_eq!(warnings.len(), 1);
        assert_eq!(page.lines.len(), 600, "all spans survive exhaustion");
    }

    #[test]
    fn vertical_grouping_stops_searching_when_its_budget_is_exhausted() {
        let spans = vec![
            span("first", 10.0, 0.0, 20.0, 50.0, 0),
            span("second", 10.0, 200.0, 20.0, 250.0, 1),
            span("third", 10.0, 400.0, 20.0, 450.0, 2),
            span("near first", 10.0, 55.0, 20.0, 105.0, 3),
        ];
        let vertical: Vec<(usize, BBox)> = spans
            .iter()
            .enumerate()
            .map(|(i, span)| (i, span.bbox.unwrap()))
            .collect();

        let (unlimited, limited) = vertical_lines_with_budget(&spans, &vertical, usize::MAX);
        assert!(!limited);
        assert_eq!(unlimited.len(), 3);
        assert_eq!(unlimited[0].text, "first near first");

        let (bounded, limited) = vertical_lines_with_budget(&spans, &vertical, 2);
        assert!(limited);
        assert_eq!(bounded.len(), 4);
        assert_eq!(
            bounded
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            ["first", "second", "third", "near first"]
        );
    }

    #[test]
    fn vertical_span_and_stack_helpers() {
        let b = |x0: f32, y0: f32, x1: f32, y1: f32| BBox { x0, y0, x1, y1 };
        let tall = span("arXiv", 20.0, 200.0, 40.0, 400.0, 0);
        assert!(is_tall_text(&tall, tall.bbox.unwrap(), 10.0));
        let one = span("l", 20.0, 200.0, 22.0, 400.0, 0);
        assert!(!is_tall_text(&one, one.bbox.unwrap(), 10.0));
        let flat = span("text", 20.0, 200.0, 20.0, 210.0, 0);
        assert!(!is_tall_text(&flat, flat.bbox.unwrap(), 10.0));

        assert!(in_margin(b(22.0, 0.0, 42.0, 10.0), 612.0));
        assert!(in_margin(b(580.0, 0.0, 600.0, 10.0), 612.0));
        assert!(!in_margin(b(48.0, 0.0, 300.0, 10.0), 612.0));

        let members = [
            (0, b(50.0, 0.0, 100.0, 10.0)),
            (1, b(102.0, 0.0, 150.0, 10.0)),
            (2, b(160.0, 0.0, 200.0, 10.0)),
        ];
        let (pos, left, right) = widest_gap(&members).unwrap();
        assert_eq!(pos, 2);
        assert!(approx(left, 150.0) && approx(right, 160.0));
        assert!(widest_gap(&members[..1]).is_none());
    }

    /// One row of two columns of prose at `y0` (right column emitted first).
    fn prose_row(k: u16, y0: f32, seq: &mut u32, spans: &mut Vec<Span>) {
        let right = format!("right row {k} of the two column body text goes here");
        spans.push(span(&right, 320.0, y0, 560.0, y0 + 10.0, *seq));
        let left = format!("left row {k} of the two column body text goes here");
        spans.push(span(&left, 50.0, y0, 290.0, y0 + 10.0, *seq + 1));
        *seq += 2;
    }

    #[test]
    fn caption_bridging_the_gutter_cuts_the_columns_into_bands() {
        // arXiv:2508.19485-like: rows 18 pt apart with 10 pt boxes leave
        // 8 pt of whitespace, less than a row gap (one 10 pt line height),
        // also around the caption set across both columns on row 4, so
        // only the bridge rule can cut the page into bands.
        let caption = "Figure 1: A caption set across both columns of the page body";
        let mut spans = Vec::new();
        let mut seq = 0;
        for k in 0..9u16 {
            let y0 = 700.0 - 18.0 * f32::from(k);
            if k == 4 {
                spans.push(span(caption, 50.0, y0, 560.0, y0 + 10.0, seq));
                seq += 1;
            } else {
                prose_row(k, y0, &mut seq, &mut spans);
            }
        }
        let mut page = page_with(spans);
        order_page(&mut page);

        let mut expected: Vec<String> = Vec::new();
        for band in [0..4u16, 5..9u16] {
            for side in ["left", "right"] {
                for k in band.clone() {
                    expected.push(format!(
                        "{side} row {k} of the two column body text goes here"
                    ));
                }
            }
            if band.start == 0 {
                expected.push(caption.to_string());
            }
        }
        assert_eq!(texts(&page), expected);
        assert!(
            page.text
                .contains("row 3 of the two column body text goes here\n\nright row 0 ")
        );
        assert!(page.text.contains("goes here\n\nFigure 1: A caption"));
        assert!(page.text.contains("page body\n\nleft row 5 "));
        let cols: Vec<u32> = page.lines.iter().map(|l| l.column).collect();
        assert_eq!(cols, [0, 0, 0, 0, 1, 1, 1, 1, 2, 3, 3, 3, 3, 4, 4, 4, 4]);
    }

    #[test]
    fn bridging_line_at_the_bottom_is_cut_off_without_a_row_gap() {
        // A row of the two columns fused into one line at the bottom of the
        // page, 8 pt below the last rows: narrower than a row gap, so the
        // spanning-row rule does not cut it off and it blocked the columns.
        let fused = "fused bottom row of the left column running on into the right one";
        let mut spans = Vec::new();
        let mut seq = 0;
        for k in 0..6u16 {
            prose_row(k, 700.0 - 18.0 * f32::from(k), &mut seq, &mut spans);
        }
        spans.push(span(fused, 50.0, 592.0, 560.0, 602.0, seq));
        let mut page = page_with(spans);
        order_page(&mut page);

        let lines = texts(&page);
        assert_eq!(lines.len(), 13, "{lines:?}");
        for (k, line) in lines[..6].iter().enumerate() {
            assert!(line.starts_with(&format!("left row {k} ")), "{line}");
        }
        for (k, line) in lines[6..12].iter().enumerate() {
            assert!(line.starts_with(&format!("right row {k} ")), "{line}");
        }
        assert_eq!(lines[12], fused);
    }

    #[test]
    fn bridge_needs_two_columns_of_prose_beside_the_bridging_lines() {
        let b = |x0: f32, y0: f32, x1: f32| BBox {
            x0,
            y0,
            x1,
            y1: y0 + 10.0,
        };
        let params = CutParams {
            row_gap: 10.0,
            column_gap: 8.0,
            bridge_gap: -2.5,
        };
        // Full-width prose with short numbered lines at the left margin and
        // one at the right: no column of prose on either side.
        let single = [
            b(50.0, 700.0, 560.0),
            b(20.0, 682.0, 30.0),
            b(20.0, 664.0, 30.0),
            b(500.0, 682.0, 560.0),
            b(500.0, 664.0, 560.0),
            b(50.0, 646.0, 560.0),
        ];
        let by_top: Vec<usize> = (0..single.len()).collect();
        let mut by_left = by_top.clone();
        by_left.sort_by(|x, y| left_first(&single[*x], &single[*y]));
        assert_eq!(bridge_row_cut(&single, &by_top, &by_left, &params), None);
        // Two columns of prose under a caption: cut below the caption.
        let columns = [
            b(50.0, 700.0, 560.0),
            b(50.0, 682.0, 290.0),
            b(320.0, 682.0, 560.0),
            b(50.0, 664.0, 290.0),
            b(320.0, 664.0, 560.0),
        ];
        let by_top: Vec<usize> = (0..columns.len()).collect();
        let mut by_left = by_top.clone();
        by_left.sort_by(|x, y| left_first(&columns[*x], &columns[*y]));
        assert_eq!(
            bridge_row_cut(&columns, &by_top, &by_left, &params),
            Some(1)
        );
    }

    #[test]
    fn short_last_line_of_a_full_width_paragraph_stays_with_it() {
        // The paragraph's full-width lines bridge the columns below, but its
        // short last line, 8 pt under them, has no line of the right column
        // beside it: no bridge cut there, the row gap under it is taken.
        let mut spans = Vec::new();
        let mut seq = 0;
        for k in 0..3u16 {
            let y0 = 760.0 - 18.0 * f32::from(k);
            let text = format!("full width paragraph line {k} above the two columns of the page");
            spans.push(span(&text, 50.0, y0, 560.0, y0 + 10.0, seq));
            seq += 1;
        }
        spans.push(span("short last line.", 50.0, 706.0, 200.0, 716.0, seq));
        seq += 1;
        for k in 0..4u16 {
            prose_row(k, 670.0 - 18.0 * f32::from(k), &mut seq, &mut spans);
        }
        let mut page = page_with(spans);
        order_page(&mut page);

        let lines = texts(&page);
        assert_eq!(lines.len(), 12, "{lines:?}");
        for (k, line) in lines[..3].iter().enumerate() {
            assert!(
                line.starts_with(&format!("full width paragraph line {k} ")),
                "{line}"
            );
        }
        assert_eq!(lines[3], "short last line.");
        for (k, line) in lines[4..8].iter().enumerate() {
            assert!(line.starts_with(&format!("left row {k} ")), "{line}");
        }
        for (k, line) in lines[8..].iter().enumerate() {
            assert!(line.starts_with(&format!("right row {k} ")), "{line}");
        }
        assert!(
            page.text
                .contains("of the page\nshort last line.\n\nleft row 0 ")
        );
    }

    /// Two columns of eight rows 18 pt apart, the right column's baselines
    /// 9 pt below the left one's (no row gap, no shared baseline); left row
    /// 2 ends at `overhang` instead of 290.
    fn staggered_columns(overhang: f32) -> PageText {
        let mut spans = Vec::new();
        let mut seq = 0;
        for k in 0..8u16 {
            let y0 = 700.0 - 18.0 * f32::from(k);
            let right = format!("right row {k} of the two column body text goes here");
            spans.push(span(&right, 320.0, y0 - 9.0, 560.0, y0 + 1.0, seq));
            let left = format!("left row {k} of the two column body text goes here");
            let x1 = if k == 2 { overhang } else { 290.0 };
            spans.push(span(&left, 50.0, y0, x1, y0 + 10.0, seq + 1));
            seq += 2;
        }
        page_with(spans)
    }

    #[test]
    fn left_line_overhanging_the_gutter_does_not_interleave_the_columns() {
        // arXiv:2508.19485, page 3, and arXiv:2305.13843, page 24: an
        // overfull left line leaves 8 pt of the 30 pt gutter (less than the
        // column gap of two character widths, about 9.9 pt), or runs 6 pt
        // into the right column. With no clean column cut and no row gap,
        // the page was one block read row by row across both columns.
        for overhang in [312.0, 326.0] {
            let mut page = staggered_columns(overhang);
            order_page(&mut page);

            let lines = texts(&page);
            assert_eq!(lines.len(), 16, "{overhang}: {lines:?}");
            for (k, line) in lines[..8].iter().enumerate() {
                assert!(line.starts_with(&format!("left row {k} ")), "{line}");
            }
            for (k, line) in lines[8..].iter().enumerate() {
                assert!(line.starts_with(&format!("right row {k} ")), "{line}");
            }
            assert!(page.text.contains("goes here\n\nright row 0 of"));
            assert_ne!(page.lines[7].column, page.lines[8].column);
        }
    }

    #[test]
    fn overhang_cut_tolerates_only_left_lines_running_into_the_gutter() {
        let b = |x0: f32, y0: f32, x1: f32| BBox {
            x0,
            y0,
            x1,
            y1: y0 + 10.0,
        };
        let by_left_of = |boxes: &[BBox]| {
            let mut by_left: Vec<usize> = (0..boxes.len()).collect();
            by_left.sort_by(|x, y| left_first(&boxes[*x], &boxes[*y]));
            by_left
        };
        // Four staggered rows per column; left row 1 runs 6 pt past the
        // right column's left edge.
        let mut boxes = Vec::new();
        for k in 0..4u16 {
            let y0 = 700.0 - 18.0 * f32::from(k);
            boxes.push(b(50.0, y0, if k == 1 { 326.0 } else { 290.0 }));
            boxes.push(b(320.0, y0 - 9.0, 560.0));
        }
        let by_left = by_left_of(&boxes);
        assert_eq!(column_cut(&boxes, &by_left, 9.9), None);
        let (at, edge) = overhang_column_cut(&boxes, &by_left, 9.9).unwrap();
        assert_eq!(at, 4);
        assert!(approx(edge, 290.0));
        let split = split_columns(&boxes, &by_left, 9.9).unwrap();
        assert_eq!(split.at, 4);
        assert!(split.coexist && approx(split.left_edge, 290.0));
        // A line centred on the gutter (an equation set across it) is not
        // an overhang of the left column.
        boxes[2] = b(240.0, 682.0, 370.0);
        let by_left = by_left_of(&boxes);
        assert_eq!(overhang_column_cut(&boxes, &by_left, 9.9), None);
        // Nor is a line reaching further into the right column than the
        // gutter is wide.
        boxes[2] = b(50.0, 682.0, 356.0);
        let by_left = by_left_of(&boxes);
        assert_eq!(overhang_column_cut(&boxes, &by_left, 9.9), None);
        assert!(split_columns(&boxes, &by_left, 9.9).is_none());
    }

    #[test]
    fn overhanging_line_beside_a_bridging_caption_still_cuts_bands() {
        // The caption test above with short row texts (a column gap of
        // about 17 pt) and left row 1 ending at 309: 11 pt short of the
        // right column, too far to join its line and too narrow a gap for
        // a clean column cut, so the bridge rule found no columns.
        let caption = "Figure 1: A caption set across both columns of the page body";
        let mut spans = Vec::new();
        let mut seq = 0;
        for k in 0..9u16 {
            let y0 = 700.0 - 18.0 * f32::from(k);
            if k == 4 {
                spans.push(span(caption, 50.0, y0, 560.0, y0 + 10.0, seq));
                seq += 1;
            } else {
                let right = format!("right row {k} of the body text");
                spans.push(span(&right, 320.0, y0, 560.0, y0 + 10.0, seq));
                let left = format!("left row {k} of the body text");
                let x1 = if k == 1 { 309.0 } else { 290.0 };
                spans.push(span(&left, 50.0, y0, x1, y0 + 10.0, seq + 1));
                seq += 2;
            }
        }
        let mut page = page_with(spans);
        order_page(&mut page);

        let mut expected: Vec<String> = Vec::new();
        for band in [0..4u16, 5..9u16] {
            for side in ["left", "right"] {
                for k in band.clone() {
                    expected.push(format!("{side} row {k} of the body text"));
                }
            }
            if band.start == 0 {
                expected.push(caption.to_string());
            }
        }
        assert_eq!(texts(&page), expected);
        let cols: Vec<u32> = page.lines.iter().map(|l| l.column).collect();
        assert_eq!(cols, [0, 0, 0, 0, 1, 1, 1, 1, 2, 3, 3, 3, 3, 4, 4, 4, 4]);
    }

    #[test]
    fn page_header_band_is_cut_off_before_the_columns_are_weighed() {
        // arXiv:2508.19485, page 10: a short running head over the left
        // column and the page number over the right one; the left column
        // opens with two figure captions (the figures carry no text), the
        // right one has a table caption level with the second. The page
        // number opened the right column, so the text flow at the gap
        // under the first caption favoured rows, and the top of the right
        // column was read before the prose of the left one.
        let mut placed: Vec<(String, f32, f32, f32)> = vec![
            ("Short running head".to_string(), 50.0, 760.0, 200.0),
            ("7".to_string(), 550.0, 760.0, 560.0),
            (
                "Fig. 6: Value distributions of each prompt on".to_string(),
                50.0,
                600.0,
                290.0,
            ),
            ("the first dataset.".to_string(), 50.0, 588.0, 120.0),
            (
                "Fig. 7: Value distributions of each prompt on".to_string(),
                50.0,
                540.0,
                290.0,
            ),
            ("the second dataset.".to_string(), 50.0, 528.0, 125.0),
        ];
        for k in 0..6u16 {
            let y0 = 504.0 - 18.0 * f32::from(k);
            let text = format!("left row {k} of the two column body text goes here");
            placed.push((text, 50.0, y0, 290.0));
        }
        for k in 0..8u16 {
            let y0 = 736.0 - 18.0 * f32::from(k);
            let text = format!("right row {k} of the two column body text goes here");
            placed.push((text, 320.0, y0, 560.0));
        }
        placed.push((
            "Table 4: Contribution of each prompt for the".to_string(),
            320.0,
            540.0,
            560.0,
        ));
        for k in 8..14u16 {
            let y0 = 522.0 - 18.0 * f32::from(k - 8);
            let text = format!("right row {k} of the two column body text goes here");
            placed.push((text, 320.0, y0, 560.0));
        }
        let expected: Vec<String> = placed.iter().map(|(text, ..)| text.clone()).collect();
        // The content stream shows the right column first.
        placed.reverse();
        let spans: Vec<Span> = placed
            .iter()
            .zip(0u32..)
            .map(|((text, x0, y0, x1), seq)| span(text, *x0, *y0, *x1, *y0 + 10.0, seq))
            .collect();
        let mut page = page_with(spans);
        order_page(&mut page);

        assert_eq!(texts(&page), expected);
        assert!(page.text.starts_with("Short running head\n\n7\n\nFig. 6: "));
        assert!(page.text.contains("goes here\n\nright row 0 of"));
    }

    #[test]
    fn distant_running_heads_aligned_with_both_columns_remain_a_header_band() {
        let mut spans = vec![
            span("Left running head", 50.0, 760.0, 180.0, 770.0, 0),
            span("Right running head", 320.0, 760.0, 450.0, 770.0, 1),
        ];
        for row in 0..4 {
            let y = 700.0 - 18.0 * row as f32;
            for (side, x) in [("left", 50.0), ("right", 320.0)] {
                spans.push(span(
                    &format!("{side} column body row {row} continues here"),
                    x,
                    y,
                    x + 240.0,
                    y + 10.0,
                    spans.len() as u32,
                ));
            }
        }
        let mut page = page_with(spans);
        order_page(&mut page);
        let ordered = texts(&page);
        assert_eq!(&ordered[..2], ["Left running head", "Right running head"]);
        assert!(
            ordered[2..6]
                .iter()
                .all(|line| line.starts_with("left column"))
        );
        assert!(
            ordered[6..]
                .iter()
                .all(|line| line.starts_with("right column"))
        );
    }

    #[test]
    fn rotated_page_groups_lines_along_the_turned_baselines() {
        // `/Rotate 90`: text runs upwards in user space. Two lines at x 100
        // to 110 and 118 to 128 (the first reads on top once turned), each
        // of three pieces whose y ranges match the other line's, so
        // grouping by user-space baseline would slice across them.
        let pieces = [("Info", "DSC"), ("cell", "="), ("one", "0.91")];
        let mut spans = Vec::new();
        for (k, (first, second)) in pieces.iter().enumerate() {
            let y0 = 100.0 + 22.0 * k as f32;
            let seq = 2 * u32::try_from(k).unwrap();
            spans.push(span(second, 118.0, y0, 128.0, y0 + 18.0, seq));
            spans.push(span(first, 100.0, y0, 110.0, y0 + 18.0, seq + 1));
        }
        let mut page = PageText::new(3, 612.0, 792.0, 90);
        page.spans = spans;
        order_page(&mut page);

        assert_eq!(texts(&page), ["Info cell one", "DSC = 0.91"]);
        assert_eq!(page.text, "Info cell one\nDSC = 0.91");
        assert_eq!(
            page.warnings,
            ["page 3 rotated 90: ordered in the turned frame"]
        );
        let first = page.lines[0].bbox.unwrap();
        assert!(
            approx(first.x0, 100.0) && approx(first.x1, 110.0),
            "{first:?}"
        );
        assert!(
            approx(first.y0, 100.0) && approx(first.y1, 162.0),
            "{first:?}"
        );
        let kept = BBox {
            x0: 100.0,
            y0: 100.0,
            x1: 110.0,
            y1: 118.0,
        };
        assert_eq!(page.spans[1].bbox, Some(kept));
        let copy = page.clone();
        order_page(&mut page);
        assert_eq!(page, copy);
    }

    #[test]
    fn turn_maps_boxes_into_the_displayed_frame_and_back() {
        let b = BBox {
            x0: 100.0,
            y0: 200.0,
            x1: 110.0,
            y1: 260.0,
        };
        for degrees in [90, 180, 270, -90] {
            let page = PageText::new(1, 612.0, 792.0, degrees);
            let t = Turn::of(&page).unwrap();
            let back = t.bbox(t.bbox(b, false), true);
            let same = approx(back.x0, b.x0)
                && approx(back.y0, b.y0)
                && approx(back.x1, b.x1)
                && approx(back.y1, b.y1);
            assert!(same, "{degrees}: {back:?}");
        }
        let quarter = Turn::of(&PageText::new(1, 612.0, 792.0, 90)).unwrap();
        let turned = quarter.bbox(b, false);
        assert!(approx(turned.x0, 200.0) && approx(turned.x1, 260.0));
        assert!(approx(turned.y0, 502.0) && approx(turned.y1, 512.0));
        assert!(approx(quarter.frame_width(), 792.0));
        let counter = Turn::of(&PageText::new(1, 612.0, 792.0, 270)).unwrap();
        let turned = counter.bbox(b, false);
        assert!(approx(turned.x0, 532.0) && approx(turned.x1, 592.0));
        assert!(approx(turned.y0, 100.0) && approx(turned.y1, 110.0));
        assert!(Turn::of(&PageText::new(1, 612.0, 792.0, 0)).is_none());
        assert!(Turn::of(&PageText::new(1, 612.0, 792.0, 45)).is_none());
    }
}
