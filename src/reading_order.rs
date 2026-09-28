//! Reading order recovery: positioned spans -> lines -> ordered page text.
//!
//! The backend emits one [`Span`] per string operator in content-stream
//! order, which on multi-column pages is rarely the reading order. This
//! module groups spans into lines by baseline, orders the lines with a
//! recursive XY-cut over their boxes (rows first, then columns) and joins
//! them into `PageText::text`. Coordinates are PDF user space (origin
//! bottom-left, `y` grows upwards) and are used unrotated. No text repair
//! of any kind is performed.
//!
//! Fonts without precomposed accented letters (`pdfTeX` with the OT1
//! encoding, say) set an accent as a glyph of its own, placed slightly
//! above the letter it belongs to and often shown before that letter. Such
//! an accent span is not a line of its own: it is attached to the line
//! whose glyph it sits over, listed right after that glyph's span, and its
//! spacing accent is composed onto the letter under it as the combining
//! mark it stands for (`Verdu` + acute over the `u` reads `Verdú`). That
//! is a rendering of what the page shows, not a repair of it.

use std::cmp::Ordering;

use unicode_normalization::UnicodeNormalization;

use crate::schema::{BBox, Line, PageText, Span};

/// Maximum XY-cut recursion depth.
const MAX_DEPTH: u32 = 64;
/// Maximum number of lines laid out on one page; the rest is appended as is.
const MAX_LINES: usize = 20_000;
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

/// Thresholds of one XY-cut run, in points.
struct CutParams {
    row_gap: f32,
    column_gap: f32,
}

/// An accent glyph attached to a line: its span index and box, the span
/// index of the glyph it sits over, and the combining marks it stands for.
struct Accent {
    index: usize,
    bbox: BBox,
    base: usize,
    marks: String,
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

/// Horizontal centre of a box.
fn centre_x(b: BBox) -> f32 {
    b.x0.midpoint(b.x1)
}

/// Position in `members` of the glyph span an accent centred at `cx` sits
/// over: the span whose x range, widened by `slack`, contains `cx`; the
/// nearer range when two do.
fn base_member(members: &[(usize, BBox)], cx: f32, slack: f32) -> Option<usize> {
    let mut best: Option<(usize, f32)> = None;
    for (m, (_, b)) in members.iter().enumerate() {
        if !(b.x0 - slack..=b.x1 + slack).contains(&cx) {
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
        let Some(m) = base_member(&line.spans, cx, ACCENT_SLACK * reference) else {
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
/// are composed onto the character under it, after which the line text is
/// put in NFC; a line without accents keeps its text exactly as joined.
fn finish_line(spans: &[Span], build: LineBuild, fallback: f32) -> Line {
    let mut members = build.spans;
    members.sort_by(|a, b| {
        let by_x = a.1.x0.total_cmp(&b.1.x0);
        by_x.then(spans[a.0].seq.cmp(&spans[b.0].seq))
    });
    let mut pieces: Vec<String> = members
        .iter()
        .map(|(i, _)| spans[*i].text.clone())
        .collect();
    let mut attached: Vec<Vec<usize>> = vec![Vec::new(); members.len()];
    let mut accents = build.accents;
    accents.sort_by(|a, b| {
        let by_x = centre_x(a.bbox).total_cmp(&centre_x(b.bbox));
        by_x.then(spans[a.index].seq.cmp(&spans[b.index].seq))
    });
    let composed = !accents.is_empty();
    for accent in accents {
        let last = members.len().saturating_sub(1);
        let m = members
            .iter()
            .position(|(i, _)| *i == accent.base)
            .unwrap_or(last);
        let slot = base_char_slot(&pieces[m], members[m].1, centre_x(accent.bbox));
        insert_marks(&mut pieces[m], slot, &accent.marks);
        attached[m].push(accent.index);
    }

    let mut joined = String::new();
    let mut order: Vec<usize> = Vec::with_capacity(members.len());
    let mut prev_x1: Option<f32> = None;
    for (m, (i, bbox)) in members.iter().enumerate() {
        let span = &spans[*i];
        let piece = pieces[m].as_str();
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
    let text: String = if composed {
        joined.nfc().collect()
    } else {
        joined
    };
    Line {
        text: text.trim().to_string(),
        bbox: Some(build.bbox),
        column: 0,
        spans: order
            .iter()
            .map(|i| u32::try_from(*i).unwrap_or(u32::MAX))
            .collect(),
    }
}

/// Group spans into lines by shared baseline and horizontal proximity, with
/// no column ordering. Blank spans and spans without a finite box are
/// skipped. Every returned line has a box, `column == 0`, its span indices
/// in visual order and its text joined as described in `finish_line`. An
/// accent-only span joins the line of the glyph it sits over (see the
/// module documentation); over no glyph it is an ordinary span. The lines
/// are sorted top-to-bottom.
pub fn group_lines(spans: &[Span]) -> Vec<Line> {
    group_lines_counted(spans).0
}

/// [`group_lines`] together with the number of accent-only spans that sit
/// over no glyph span and so form lines of their own.
fn group_lines_counted(spans: &[Span]) -> (Vec<Line>, usize) {
    let mut candidates = positioned(spans);
    if candidates.is_empty() {
        return (Vec::new(), 0);
    }
    let fallback = typical_size(spans, &candidates);
    let largest = candidates
        .iter()
        .map(|(i, _)| span_size(&spans[*i], fallback))
        .fold(fallback, f32::max);
    candidates.sort_by(baseline_first);

    let mut builds: Vec<LineBuild> = Vec::new();
    let mut accents: Vec<(usize, BBox, String)> = Vec::new();
    for (i, bbox) in &candidates {
        if let Some(marks) = accent_marks(&spans[*i].text) {
            accents.push((*i, *bbox, marks));
            continue;
        }
        let size = span_size(&spans[*i], fallback);
        if let Some(k) = find_line(&builds, *bbox, size, largest) {
            let line = &mut builds[k];
            line.bbox = union(line.bbox, *bbox);
            line.size = line.size.max(size);
            line.spans.push((*i, *bbox));
        } else {
            builds.push(LineBuild {
                baseline: bbox.y0,
                size,
                bbox: *bbox,
                spans: vec![(*i, *bbox)],
                accents: Vec::new(),
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
    lines.sort_by(line_top_first);
    (lines, unattached)
}

/// Position in `idx` (sorted top-to-bottom here) at which the widest
/// horizontal whitespace band wider than `min_gap` starts, if any.
fn row_cut(boxes: &[BBox], idx: &mut [usize], min_gap: f32) -> Option<usize> {
    idx.sort_by(|a, b| top_first(&boxes[*a], &boxes[*b]));
    let mut bottom = boxes[idx[0]].y0;
    let mut best: Option<(usize, f32)> = None;
    for (pos, &i) in idx.iter().enumerate().skip(1) {
        let gap = bottom - boxes[i].y1;
        if gap > min_gap && best.is_none_or(|(_, g)| gap > g) {
            best = Some((pos, gap));
        }
        bottom = bottom.min(boxes[i].y0);
    }
    best.map(|(pos, _)| pos)
}

/// Position in `idx` (sorted left-to-right here) at which the widest
/// vertical whitespace band wider than `min_gap` starts, if any.
fn column_cut(boxes: &[BBox], idx: &mut [usize], min_gap: f32) -> Option<usize> {
    idx.sort_by(|a, b| left_first(&boxes[*a], &boxes[*b]));
    let mut right = boxes[idx[0]].x1;
    let mut best: Option<(usize, f32)> = None;
    for (pos, &i) in idx.iter().enumerate().skip(1) {
        let gap = boxes[i].x0 - right;
        if gap > min_gap && best.is_none_or(|(_, g)| gap > g) {
            best = Some((pos, gap));
        }
        right = right.max(boxes[i].x1);
    }
    best.map(|(pos, _)| pos)
}

/// Recursive XY-cut: split on the widest row gap, else on the widest column
/// gap, else emit the indices as one leaf block sorted top-to-bottom.
fn xy_cut(
    boxes: &[BBox],
    mut idx: Vec<usize>,
    depth: u32,
    params: &CutParams,
    out: &mut Vec<Vec<usize>>,
) {
    if idx.len() > 1 && depth < MAX_DEPTH {
        if let Some(at) = row_cut(boxes, &mut idx, params.row_gap) {
            let lower = idx.split_off(at);
            xy_cut(boxes, idx, depth + 1, params, out);
            xy_cut(boxes, lower, depth + 1, params, out);
            return;
        }
        if let Some(at) = column_cut(boxes, &mut idx, params.column_gap) {
            let right = idx.split_off(at);
            xy_cut(boxes, idx, depth + 1, params, out);
            xy_cut(boxes, right, depth + 1, params, out);
            return;
        }
    }
    idx.sort_by(|a, b| top_first(&boxes[*a], &boxes[*b]));
    out.push(idx);
}

/// Order lines for reading with a recursive XY-cut over their boxes and set
/// `column` to the index of the leaf block each line ends up in. Rows split
/// on vertical whitespace wider than `ROW_GAP` median line heights, then
/// columns on horizontal whitespace wider than `COLUMN_GAP` median
/// character widths (floored at 0.5 % of `page_width` against degenerate
/// character widths). Lines without a finite box, and lines beyond
/// `MAX_LINES`, keep their order and form one extra block at the end.
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
    };

    let boxes: Vec<BBox> = placed.iter().map(|(_, b)| *b).collect();
    let mut blocks: Vec<Vec<usize>> = Vec::new();
    if !boxes.is_empty() {
        let all: Vec<usize> = (0..boxes.len()).collect();
        xy_cut(&boxes, all, 0, &params, &mut blocks);
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

/// Add a page warning unless the same text is already present.
fn push_warning(page: &mut PageText, warning: String) {
    if !page.warnings.contains(&warning) {
        page.warnings.push(warning);
    }
}

/// Fill `page.lines` and `page.text` from `page.spans`. Idempotent: lines
/// and text are rebuilt from scratch and warnings are never duplicated.
/// Coordinates are used as supplied (unrotated); a non-zero rotation is
/// only noted as a warning. Non-blank spans without geometry are appended
/// at the end, one line each in content-stream order, as their own block.
/// An accent-only span that sits over no glyph is left as its own line and
/// noted with the warning `unattached accent glyph at page N: M span(s)`.
pub fn order_page(page: &mut PageText) {
    page.lines.clear();
    page.text.clear();
    if page.rotation != 0 {
        let rotation = page.rotation;
        let msg = format!("page rotation {rotation}: coordinates used unrotated");
        push_warning(page, msg);
    }

    let (grouped, unattached) = group_lines_counted(&page.spans);
    if unattached > 0 {
        let number = page.page;
        let msg = format!("unattached accent glyph at page {number}: {unattached} span(s)");
        push_warning(page, msg);
    }
    if grouped.len() > MAX_LINES {
        let n = grouped.len();
        let msg = format!("too many lines: {n} > {MAX_LINES}; the rest is appended unordered");
        push_warning(page, msg);
    }
    let ordered = order_lines(grouped, page.width);
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
            });
        }
        let n = loose.len();
        let msg = format!("spans without geometry: {n}");
        push_warning(page, msg);
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
        assert!(page.warnings[0].starts_with("page rotation 90"));
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
        assert_eq!(base_member(&members, 124.0, 1.0), Some(1));
        assert_eq!(base_member(&members, 121.3, 1.0), Some(1));
        assert_eq!(base_member(&members, 110.0, 1.0), Some(0));
        assert_eq!(base_member(&members, 140.0, 1.0), None);
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
}
