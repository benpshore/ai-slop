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
    size: Option<f32>,
    /// Line sits in the top or bottom margin band (or has no bbox).
    edge: bool,
    text: String,
}

/// Largest gap in points between an entry start and its continuation lines
/// that still counts as "the same indent".
const INDENT_TOLERANCE: f32 = 1.0;
/// Longest run of numbers accepted from a `[a–b]` range marker.
const MAX_RANGE_SPAN: u32 = 50;

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
            r"(?i)^\s*(?:(?:\d+|[A-Z]|[IVX]+)[.:]?\s+)?(?:appendix|appendices|supplementary|supplemental|supporting information|acknowledg\w*|author biograph\w*|biograph\w*)\b",
        )
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

fn doi_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"10\.\d{4,9}/[^\s"<>]+"#).expect("valid regex"))
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

fn surname_first_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^\p{Lu}[\p{L}'’\-]+(?:\s+\p{Lu}[\p{L}'’\-]+)?,\s*\p{Lu}").expect("valid regex")
    })
}

fn vancouver_start_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\p{Lu}[\p{L}'’\-]+\s+\p{Lu}{1,3}\b[,.]").expect("valid regex"))
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
            lines.push(SectionLine {
                page: page.page,
                column: line.column,
                x0: line.bbox.map(|b| b.x0),
                size: line_size(page, line),
                edge,
                text: line.text.trim().to_string(),
            });
        }
    }
    // Texts that repeat on several pages near the page edge are running
    // headers or footers.
    let mut pages_per_text: BTreeMap<&str, Vec<u32>> = BTreeMap::new();
    for line in &lines {
        let seen = pages_per_text.entry(line.text.as_str()).or_default();
        if !seen.contains(&line.page) {
            seen.push(line.page);
        }
    }
    let repeated: Vec<String> = pages_per_text
        .iter()
        .filter(|(_, seen)| seen.len() >= 2)
        .map(|(text, _)| (*text).to_string())
        .collect();
    lines.retain(|line| {
        !(line.text.is_empty()
            || page_number_re().is_match(&line.text)
            || (line.edge && repeated.iter().any(|t| t == &line.text)))
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

fn detect_style(lines: &[SectionLine]) -> Style {
    let Some(first) = lines.first() else {
        return Style::AuthorYear;
    };
    if bracket_label_re().is_match(&first.text) {
        Style::Bracket
    } else if dot_label_re().is_match(&first.text) {
        Style::Dot
    } else if paren_label_re().is_match(&first.text) {
        Style::Paren
    } else {
        Style::AuthorYear
    }
}

fn median_size(lines: &[SectionLine]) -> Option<f32> {
    let mut sizes: Vec<f32> = lines.iter().filter_map(|l| l.size).collect();
    if sizes.is_empty() {
        return None;
    }
    sizes.sort_by(f32::total_cmp);
    Some(sizes[sizes.len() / 2])
}

/// A heading that ends the reference list: `Appendix`, `Supplementary`, ...
/// or, when sizes are known, a short line set clearly larger than the body.
fn is_end_heading(line: &SectionLine, style: Style, median: Option<f32>) -> bool {
    let short = line.text.chars().count() <= 80;
    if short && end_heading_re().is_match(&line.text) {
        return true;
    }
    if numbered_label(style, &line.text).is_some() {
        return false;
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
    if style == Style::AuthorYear {
        segment_author_year(&lines, median)
    } else {
        segment_numbered(&lines, style, median)
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

fn append_continuation(entries: &mut [ReferenceEntry], text: &str) {
    if let Some(last) = entries.last_mut() {
        if !last.raw.is_empty() {
            last.raw.push(' ');
        }
        last.raw.push_str(text);
    }
}

fn segment_numbered(
    lines: &[SectionLine],
    style: Style,
    median: Option<f32>,
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
        append_continuation(&mut entries, &line.text);
    }
    entries
}

/// Hanging-indent evidence for line `i`: `Some(true)` when it is outdented
/// relative to a neighbour in the same column and page, `Some(false)` when it
/// is indented relative to the previous line, `None` when the layout says
/// nothing.
fn indent_says_start(lines: &[SectionLine], i: usize) -> Option<bool> {
    let line = &lines[i];
    let x0 = line.x0?;
    let same_block = |other: &SectionLine| other.page == line.page && other.column == line.column;
    let prev = i
        .checked_sub(1)
        .map(|p| &lines[p])
        .filter(|p| same_block(p));
    let next = lines.get(i + 1).filter(|n| same_block(n));
    if let Some(px) = prev.and_then(|p| p.x0) {
        if x0 < px - INDENT_TOLERANCE {
            return Some(true);
        }
        if x0 > px + INDENT_TOLERANCE {
            return Some(false);
        }
    }
    if let Some(nx) = next.and_then(|n| n.x0) {
        if x0 < nx - INDENT_TOLERANCE {
            return Some(true);
        }
        if x0 > nx + INDENT_TOLERANCE {
            return Some(false);
        }
    }
    None
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

fn segment_author_year(lines: &[SectionLine], median: Option<f32>) -> Vec<ReferenceEntry> {
    let mut entries: Vec<ReferenceEntry> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if is_end_heading(line, Style::AuthorYear, median) {
            break;
        }
        let starts = if entries.is_empty() {
            true
        } else {
            indent_says_start(lines, i).unwrap_or_else(|| {
                author_start_re().is_match(&line.text)
                    && entries.last().is_some_and(|e| ends_like_entry(&e.raw))
            })
        };
        if starts {
            push_entry(&mut entries, None, &line.text, line.page);
        } else {
            append_continuation(&mut entries, &line.text);
        }
    }
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

/// First DOI with its byte range in `text`.
fn find_doi(text: &str) -> Option<(Range<usize>, String)> {
    let found = doi_re().find(text)?;
    let trimmed = trim_trailing_punct(found.as_str());
    if trimmed.len() < 8 {
        return None;
    }
    Some((
        found.start()..found.start() + trimmed.len(),
        trimmed.to_string(),
    ))
}

/// First arXiv identifier (after `arXiv:` or `abs/`) with its byte range.
fn find_arxiv(text: &str) -> Option<(Range<usize>, String)> {
    let caps = arxiv_re().captures(text)?;
    let whole = caps.get(0)?;
    let id = caps.get(1)?;
    Some((whole.range(), id.as_str().to_string()))
}

/// First URL with its byte range.
fn find_url(text: &str) -> Option<(Range<usize>, String)> {
    let found = url_re().find(text)?;
    let trimmed = trim_trailing_punct(found.as_str());
    Some((
        found.start()..found.start() + trimmed.len(),
        trimmed.to_string(),
    ))
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
fn find_quoted(text: &str) -> Option<(Range<usize>, String)> {
    let open = text.find(['“', '"', '„', '‘'])?;
    let open_len = text[open..].chars().next().map_or(1, char::len_utf8);
    let inner_start = open + open_len;
    let close_rel = text[inner_start..].find(['”', '"', '“', '’'])?;
    let close = inner_start + close_rel;
    let close_len = text[close..].chars().next().map_or(1, char::len_utf8);
    let inner = text[inner_start..close].trim();
    let inner = inner.trim_end_matches([',', '.', ';']);
    let inner = inner.trim();
    if inner.is_empty() {
        return None;
    }
    Some((open..close + close_len, inner.to_string()))
}

/// Alphabetic run that ends right before byte `end`.
fn word_before(text: &str, end: usize) -> &str {
    let head = &text[..end];
    let start = head
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_alphabetic())
        .last()
        .map_or(end, |(i, _)| i);
    &head[start..]
}

/// Is the period at byte `dot` the end of an initial or a name suffix
/// (`A.`, `Jr.`, `St.`, `al.`) rather than a sentence end?
fn period_is_abbreviation(text: &str, dot: usize) -> bool {
    let word = word_before(text, dot);
    word.chars().count() == 1
        || matches!(
            word.to_ascii_lowercase().as_str(),
            "jr" | "sr" | "st" | "al" | "eds" | "ed"
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
        if surname_first && !continues_author_list(body, pos + 1) {
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

/// End (byte offset, exclusive) of a title that starts at byte 0 of `text`.
fn title_end(text: &str) -> usize {
    let mut search = 0usize;
    while let Some(rel) = text[search..].find(['.', '?', '!']) {
        let pos = search + rel;
        let followed = text[pos + 1..].starts_with(' ');
        if followed && (text.as_bytes()[pos] != b'.' || !period_is_abbreviation(text, pos)) {
            return pos;
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
    let mut quoted_title: Option<(Range<usize>, String)> = None;
    if let Some((range, _)) = &year
        && quoted.as_ref().is_none_or(|(q, _)| range.start < q.start)
        && is_author_only(&masked[..range.start])
    {
        authors_end = Some(range.start);
        // Skip a year suffix (`2020a`) and the punctuation closing the year.
        let mut after = &masked[range.end..];
        if after.starts_with(|c: char| c.is_ascii_lowercase()) {
            after = &after[1..];
        }
        after = after.trim_start_matches([')', '.', ',', ':', ' ']);
        title_start = masked.len() - after.len();
    } else if let Some((range, text)) = &quoted
        && year.as_ref().is_none_or(|(y, _)| y.start > range.start)
        && is_author_only(&masked[..range.start])
    {
        authors_end = Some(range.start);
        quoted_title = Some((range.clone(), text.clone()));
        title_start = range.end;
    } else if let Some(end) = author_terminator(&masked) {
        authors_end = Some(end);
        title_start = end;
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
        let title_text = &body[title_start..];
        let stop = title_end(title_text);
        let title = title_text[..stop].trim().trim_end_matches(',').trim();
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
}
