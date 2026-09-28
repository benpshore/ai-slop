//! Paper-level metadata: title, authors, DOI, arXiv id, year, venue, abstract
//! and keywords, taken from the PDF `/Info` dictionary and from the evidence
//! on page 1. Nothing is guessed: a field stays `None` unless a concrete
//! source supports it, and every field that is set records its provenance.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use regex::Regex;

use crate::schema::{Author, Line, Metadata, PageText};

/// Tolerance in points when grouping lines of "the same" font size.
const SIZE_TOLERANCE: f32 = 0.5;
/// Spans smaller than this fraction of the line's dominant size are treated as
/// superscript affiliation markers.
const SUPERSCRIPT_RATIO: f32 = 0.8;
/// Maximum number of lines inspected between the title block and the abstract.
const MAX_AUTHOR_LINES: usize = 30;
/// Maximum number of lines collected for the abstract.
const MAX_ABSTRACT_LINES: usize = 80;

fn doi_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"10\.\d{4,9}/[^\s"<>]+"#).expect("valid regex"))
}

fn arxiv_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)arxiv[\s:.]*(\d{4}\.\d{4,5}(?:v\d+)?|[a-z\-]+(?:\.[a-z]{2})?/\d{7})")
            .expect("valid regex")
    })
}

fn year_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?:^|\D)((?:19|20)\d{2})(?:\D|$)").expect("valid regex"))
}

fn abstract_heading_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)^\s*abstract\b\s*[.:—–\-]?\s*(.*)$").expect("valid regex"))
}

fn keywords_heading_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)^\s*(?:keywords|key words|index terms)\b\s*[:—–\-.]?\s*(.*)$")
            .expect("valid regex")
    })
}

fn section_heading_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)^\s*(?:(?:\d+|i)[.:]?\s+)?(?:introduction|keywords|key words|index terms|ccs concepts|background|motivation)\b",
        )
        .expect("valid regex")
    })
}

fn numbered_heading_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*1\.?\s+\p{Lu}").expect("valid regex"))
}

fn affiliation_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)\b(?:universit\w*|institut\w*|department|dept|school|laborator\w*|college|faculty|center|centre|research|labs?|inc|ltd|gmbh|corporation|company|hospital|academy|foundation|group|division|street|avenue|road|google|microsoft|deepmind|openai|nvidia|amazon|facebook|ibm|intel|usa|uk)\b|@",
        )
        .expect("valid regex")
    })
}

fn marker_chars_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[\d¹²³⁴⁵⁶⁷⁸⁹⁰⁺*†‡§¶‖#]+").expect("valid regex"))
}

fn info_split_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\s*(?:,|;|&|\band\b)\s*").expect("valid regex"))
}

/// Extract metadata from the `/Info` dictionary and the first page.
///
/// `info` keys are the dictionary keys without the leading `/`. Page-1
/// evidence is used when the corresponding `info` entry is missing or generic
/// (for example `untitled` or `Microsoft Word - draft.docx`). Every field that
/// is set gets an entry in `provenance`.
pub fn extract_metadata(info: &BTreeMap<String, String>, pages: &[PageText]) -> Metadata {
    let mut meta = Metadata {
        info: info.clone(),
        ..Metadata::default()
    };
    let page1: Option<&PageText> = pages.first();

    // Title.
    if let Some(title) = info
        .get("Title")
        .map(String::as_str)
        .map(str::trim)
        .filter(|t| !title_is_generic(t))
    {
        set_title(&mut meta, title, "info:Title");
    }
    let mut title_lines: Vec<usize> = Vec::new();
    if meta.title.is_none()
        && let Some(page) = page1
        && let Some((indices, text)) = title_block(page)
    {
        set_title(&mut meta, &text, "page1:largest-font");
        title_lines = indices;
    }

    // Authors.
    if let Some(author) = info
        .get("Author")
        .map(String::as_str)
        .map(str::trim)
        .filter(|a| !author_is_generic(a))
    {
        let names = split_author_names(author);
        if !names.is_empty() {
            meta.authors = names.into_iter().map(named_author).collect();
            meta.provenance
                .insert("authors".to_string(), "info:Author".to_string());
        }
    }
    if meta.authors.is_empty()
        && let Some(page) = page1
        && let Some(&last_title_line) = title_lines.last()
    {
        let names = page1_authors(page, last_title_line + 1);
        if !names.is_empty() {
            meta.authors = names.into_iter().map(named_author).collect();
            meta.provenance
                .insert("authors".to_string(), "page1:authors".to_string());
        }
    }

    // DOI: any Info value first, then page 1.
    for (key, value) in info {
        if let Some(doi) = find_doi(value) {
            meta.doi = Some(doi);
            meta.provenance
                .insert("doi".to_string(), format!("info:{key}"));
            break;
        }
    }
    if meta.doi.is_none()
        && let Some(page) = page1
        && let Some(doi) = first_in_lines(page, find_doi)
    {
        meta.doi = Some(doi);
        meta.provenance
            .insert("doi".to_string(), "page1:doi".to_string());
    }

    // arXiv id: Info values first, then page 1.
    for (key, value) in info {
        if let Some(id) = find_arxiv_id(value) {
            meta.arxiv_id = Some(id);
            meta.provenance
                .insert("arxiv_id".to_string(), format!("info:{key}"));
            break;
        }
    }
    if meta.arxiv_id.is_none()
        && let Some(page) = page1
        && let Some(id) = first_in_lines(page, find_arxiv_id)
    {
        meta.arxiv_id = Some(id);
        meta.provenance
            .insert("arxiv_id".to_string(), "page1:arxiv".to_string());
    }

    // Venue from Subject when it is not merely a copy of the title.
    if let Some(subject) = info
        .get("Subject")
        .map(String::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let same_as_title = meta
            .title
            .as_deref()
            .is_some_and(|t| t.eq_ignore_ascii_case(subject));
        if !same_as_title && find_doi(subject).is_none() && find_arxiv_id(subject).is_none() {
            meta.venue = Some(subject.to_string());
            meta.provenance
                .insert("venue".to_string(), "info:Subject".to_string());
        }
    }

    // Keywords.
    if let Some(keywords) = info.get("Keywords") {
        let list = split_keywords(keywords);
        if !list.is_empty() {
            meta.keywords = list;
            meta.provenance
                .insert("keywords".to_string(), "info:Keywords".to_string());
        }
    }
    if meta.keywords.is_empty()
        && let Some(page) = page1
        && let Some(list) = page1_keywords(page)
    {
        meta.keywords = list;
        meta.provenance
            .insert("keywords".to_string(), "page1:keywords".to_string());
    }

    // Abstract.
    if let Some(page) = page1
        && let Some(text) = page1_abstract(page)
    {
        meta.abstract_text = Some(text);
        meta.provenance
            .insert("abstract_text".to_string(), "page1:abstract".to_string());
    }

    // Year: arXiv id, DOI, Info dates, page 1.
    if let Some(year) = meta.arxiv_id.as_deref().and_then(year_from_arxiv_id) {
        set_year(&mut meta, year, "arxiv_id");
    } else if let Some(year) = meta.doi.as_deref().and_then(year_from_doi) {
        set_year(&mut meta, year, "doi");
    } else if let Some((key, year)) = info_year(info) {
        set_year(&mut meta, year, &format!("info:{key}"));
    } else if let Some(page) = page1
        && let Some(year) = page1_year(page)
    {
        set_year(&mut meta, year, "page1:year");
    }

    meta
}

/// First value that `find` extracts from a line of `page`, in reading order.
fn first_in_lines(page: &PageText, find: fn(&str) -> Option<String>) -> Option<String> {
    for line in &page.lines {
        if let Some(found) = find(&line.text) {
            return Some(found);
        }
    }
    None
}

/// Year from the first `/Info` date entry that parses.
fn info_year(info: &BTreeMap<String, String>) -> Option<(&'static str, u16)> {
    for key in ["CreationDate", "ModDate"] {
        if let Some(year) = info.get(key).and_then(|v| year_from_pdf_date(v.as_str())) {
            return Some((key, year));
        }
    }
    None
}

fn set_title(meta: &mut Metadata, title: &str, source: &str) {
    let cleaned = collapse_whitespace(title);
    if cleaned.is_empty() {
        return;
    }
    meta.title = Some(cleaned);
    meta.provenance
        .insert("title".to_string(), source.to_string());
}

fn set_year(meta: &mut Metadata, year: u16, source: &str) {
    meta.year = Some(year);
    meta.provenance
        .insert("year".to_string(), source.to_string());
}

fn named_author(name: String) -> Author {
    Author {
        name,
        ..Author::default()
    }
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<&str>>().join(" ")
}

/// True when an `/Info` title carries no information about the paper.
fn title_is_generic(title: &str) -> bool {
    let lower = title.trim().to_lowercase();
    if matches!(
        lower.as_str(),
        "" | "untitled" | "title" | "untitled document"
    ) {
        return true;
    }
    if lower.starts_with("microsoft word") || lower.starts_with("microsoft powerpoint") {
        return true;
    }
    if !lower.chars().any(char::is_alphabetic) {
        return true;
    }
    let extensions = [
        ".docx", ".doc", ".tex", ".dvi", ".pdf", ".ps", ".odt", ".rtf", ".txt", ".indd", ".qxd",
    ];
    if extensions.iter().any(|ext| lower.ends_with(*ext)) {
        return true;
    }
    // A single token containing a dot or underscore is almost always a file name.
    !lower.contains(' ') && (lower.contains('.') || lower.contains('_'))
}

/// True when an `/Info` author string is a login name or placeholder.
fn author_is_generic(author: &str) -> bool {
    let lower = author.trim().to_lowercase();
    lower.is_empty()
        || matches!(
            lower.as_str(),
            "unknown" | "user" | "admin" | "administrator" | "author" | "owner" | "guest"
        )
        || !lower.chars().any(char::is_alphabetic)
}

/// Split an `/Info` author string on commas, semicolons, `&` and `and`.
fn split_author_names(text: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for part in info_split_re().split(text) {
        let cleaned = collapse_whitespace(part);
        if cleaned.is_empty() {
            continue;
        }
        if is_name_suffix(&cleaned)
            && let Some(last) = names.last_mut()
        {
            last.push_str(", ");
            last.push_str(&cleaned);
            continue;
        }
        names.push(cleaned);
    }
    names
}

fn split_keywords(text: &str) -> Vec<String> {
    text.split([',', ';', '·', '•'])
        .map(|k| collapse_whitespace(k.trim_matches(|c: char| c == '.' || c.is_whitespace())))
        .filter(|k| !k.is_empty())
        .collect()
}

/// First DOI in `text`, with trailing punctuation removed.
fn find_doi(text: &str) -> Option<String> {
    let found = doi_re().find(text)?;
    let trimmed = found
        .as_str()
        .trim_end_matches(|c| matches!(c, '.' | ',' | ';' | ')' | ']' | ':' | '}'));
    if trimmed.len() < 8 {
        return None;
    }
    Some(trimmed.to_string())
}

/// First arXiv identifier (new or old style) that follows an `arXiv` marker.
fn find_arxiv_id(text: &str) -> Option<String> {
    arxiv_re()
        .captures(text)
        .and_then(|caps| caps.get(1))
        .map(|m| m.as_str().to_string())
}

fn year_from_arxiv_id(id: &str) -> Option<u16> {
    let digits: String = if let Some((_, tail)) = id.rsplit_once('/') {
        tail.chars().take(2).collect()
    } else {
        id.chars().take(2).collect()
    };
    if digits.len() != 2 {
        return None;
    }
    let yy: u16 = digits.parse().ok()?;
    // Old-style ids run from 1991; new-style ids start in 2007.
    Some(if id.contains('/') && yy >= 90 {
        1900 + yy
    } else {
        2000 + yy
    })
}

fn year_from_doi(doi: &str) -> Option<u16> {
    if doi.to_lowercase().contains("arxiv") {
        return None;
    }
    first_year(doi)
}

/// Year from a PDF date string such as `D:20200315120000Z`.
fn year_from_pdf_date(date: &str) -> Option<u16> {
    let digits: String = date
        .trim()
        .trim_start_matches("D:")
        .chars()
        .take(4)
        .collect();
    if digits.len() != 4 || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let year: u16 = digits.parse().ok()?;
    (1900..=2099).contains(&year).then_some(year)
}

/// First `19xx`/`20xx` year in `text` that is not part of a longer number.
fn first_year(text: &str) -> Option<u16> {
    year_re()
        .captures(text)
        .and_then(|caps| caps.get(1))
        .and_then(|m| m.as_str().parse::<u16>().ok())
}

/// A year printed on page 1, preferring dated lines (copyright, received, ...).
fn page1_year(page: &PageText) -> Option<u16> {
    let dated = [
        "©",
        "copyright",
        "published",
        "accepted",
        "received",
        "preprint",
        "proceedings",
        "conference",
        "journal",
        "january",
        "february",
        "march",
        "april",
        "may ",
        "june",
        "july",
        "august",
        "september",
        "october",
        "november",
        "december",
    ];
    let preferred = page.lines.iter().find_map(|line| {
        let lower = line.text.to_lowercase();
        if dated.iter().any(|needle| lower.contains(*needle)) {
            first_year(&line.text)
        } else {
            None
        }
    });
    preferred.or_else(|| page.lines.iter().find_map(|line| first_year(&line.text)))
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

/// Median font size over all sized lines of the page.
fn median_line_size(page: &PageText) -> Option<f32> {
    let mut sizes: Vec<f32> = page
        .lines
        .iter()
        .filter_map(|line| line_size(page, line))
        .collect();
    if sizes.is_empty() {
        return None;
    }
    sizes.sort_by(f32::total_cmp);
    Some(sizes[sizes.len() / 2])
}

/// The group of consecutive largest-font lines near the top of the page.
///
/// Returns the indices of the lines and their joined text. `None` when there
/// is no font-size evidence or when the largest size is not clearly larger
/// than the page's typical size (no title stands out).
fn title_block(page: &PageText) -> Option<(Vec<usize>, String)> {
    let top_limit = page.height * 0.4;
    let mut sized: Vec<(usize, f32)> = Vec::new();
    for (i, line) in page.lines.iter().enumerate() {
        let text = line.text.trim();
        if text.chars().count() < 3 || !text.chars().any(char::is_alphabetic) {
            continue;
        }
        if line.bbox.is_some_and(|b| b.y1 < top_limit) {
            continue;
        }
        if let Some(size) = line_size(page, line) {
            sized.push((i, size));
        }
    }
    if sized.is_empty() {
        return None;
    }
    let max_size = sized.iter().map(|(_, s)| *s).fold(f32::MIN, f32::max);
    let median = median_line_size(page)?;
    if max_size < median + SIZE_TOLERANCE {
        return None;
    }
    let threshold = max_size - SIZE_TOLERANCE;
    let first = sized.iter().find(|(_, s)| *s >= threshold)?.0;
    let mut indices: Vec<usize> = vec![first];
    let mut next = first + 1;
    while let Some(line) = page.lines.get(next)
        && let Some(size) = line_size(page, line)
        && size >= threshold
        && line.text.trim().chars().any(char::is_alphabetic)
    {
        indices.push(next);
        next += 1;
    }
    let text = indices
        .iter()
        .map(|&i| page.lines[i].text.trim())
        .collect::<Vec<&str>>()
        .join(" ");
    let text = collapse_whitespace(&text);
    if text.is_empty() {
        return None;
    }
    Some((indices, text))
}

/// Author names from the lines between the title block and the abstract.
fn page1_authors(page: &PageText, start: usize) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for line in page.lines.iter().skip(start).take(MAX_AUTHOR_LINES) {
        let text = line.text.trim();
        if text.is_empty() {
            continue;
        }
        if abstract_heading_re().is_match(text)
            || section_heading_re().is_match(text)
            || keywords_heading_re().is_match(text)
        {
            break;
        }
        if text.chars().count() > 300 || affiliation_re().is_match(text) {
            continue;
        }
        let stripped = strip_superscripts(page, line);
        let stripped = marker_chars_re().replace_all(&stripped, "");
        let candidates = split_author_names(&stripped);
        if candidates.is_empty() || !candidates.iter().all(|c| looks_like_person_name(c)) {
            continue;
        }
        names.extend(candidates);
    }
    names
}

/// Line text with spans much smaller than the line's dominant size removed
/// (superscript affiliation markers). Falls back to the plain text when the
/// spans cannot be located in it.
fn strip_superscripts(page: &PageText, line: &Line) -> String {
    let Some(dominant) = line_size(page, line) else {
        return line.text.clone();
    };
    let mut out = String::new();
    let mut cursor: usize = 0;
    for idx in &line.spans {
        let Some(span) = page.spans.get(*idx as usize) else {
            continue;
        };
        let piece = span.text.as_str();
        if piece.is_empty() {
            continue;
        }
        let Some(rel) = line.text.get(cursor..).and_then(|rest| rest.find(piece)) else {
            return line.text.clone();
        };
        let start = cursor + rel;
        let end = start + piece.len();
        out.push_str(&line.text[cursor..start]);
        let small = span.size.is_some_and(|s| s < dominant * SUPERSCRIPT_RATIO);
        if !small {
            out.push_str(piece);
        }
        cursor = end;
    }
    out.push_str(&line.text[cursor..]);
    out
}

fn is_name_suffix(token: &str) -> bool {
    matches!(
        token.trim_end_matches('.').to_ascii_lowercase().as_str(),
        "jr" | "sr" | "ii" | "iii" | "iv" | "phd" | "md"
    )
}

fn is_name_particle(token: &str) -> bool {
    matches!(
        token,
        "van"
            | "von"
            | "de"
            | "der"
            | "den"
            | "del"
            | "della"
            | "di"
            | "da"
            | "do"
            | "dos"
            | "das"
            | "la"
            | "le"
            | "du"
            | "bin"
            | "ibn"
            | "al"
            | "el"
            | "ten"
            | "ter"
            | "y"
            | "e"
    )
}

/// Conservative test for a `First [Middle] Last` person name.
fn looks_like_person_name(candidate: &str) -> bool {
    let tokens: Vec<&str> = candidate.split_whitespace().collect();
    if tokens.len() < 2 || tokens.len() > 6 {
        return false;
    }
    let mut capitalised = 0usize;
    for token in &tokens {
        let token = token.trim_matches(|c: char| c == ',' || c == '(' || c == ')');
        if token.is_empty() || token.chars().count() > 25 {
            return false;
        }
        if is_name_particle(token) || is_name_suffix(token) {
            continue;
        }
        let mut chars = token.chars();
        let Some(first) = chars.next() else {
            return false;
        };
        if !first.is_uppercase() {
            return false;
        }
        if !chars.all(|c| c.is_alphabetic() || matches!(c, '\'' | '’' | '-' | '.')) {
            return false;
        }
        capitalised += 1;
    }
    capitalised >= 2
}

/// Keywords printed on page 1 after a `Keywords:` / `Index Terms—` label.
fn page1_keywords(page: &PageText) -> Option<Vec<String>> {
    let (pos, rest) = page.lines.iter().enumerate().find_map(|(i, line)| {
        keywords_heading_re()
            .captures(line.text.trim())
            .and_then(|caps| caps.get(1))
            .map(|m| (i, m.as_str().to_string()))
    })?;
    let mut text = rest;
    // A keyword list may wrap onto the next line or two.
    for line in page.lines.iter().skip(pos + 1).take(2) {
        let candidate = line.text.trim();
        if candidate.is_empty()
            || section_heading_re().is_match(candidate)
            || abstract_heading_re().is_match(candidate)
            || !text.trim_end().ends_with([',', ';', '·', '•'])
        {
            break;
        }
        text.push(' ');
        text.push_str(candidate);
    }
    let list = split_keywords(&text);
    (!list.is_empty()).then_some(list)
}

/// Abstract text: the lines after a line starting with `Abstract` up to the
/// next heading (`1 Introduction`, `Keywords`, ...), joined with spaces.
fn page1_abstract(page: &PageText) -> Option<String> {
    let (pos, inline) = page.lines.iter().enumerate().find_map(|(i, line)| {
        abstract_heading_re()
            .captures(line.text.trim())
            .and_then(|caps| caps.get(1))
            .map(|m| (i, m.as_str().trim().to_string()))
    })?;
    let mut parts: Vec<String> = Vec::new();
    if !inline.is_empty() {
        parts.push(inline);
    }
    for line in page.lines.iter().skip(pos + 1).take(MAX_ABSTRACT_LINES) {
        let text = line.text.trim();
        if text.is_empty() {
            continue;
        }
        if section_heading_re().is_match(text)
            || numbered_heading_re().is_match(text)
            || keywords_heading_re().is_match(text)
        {
            break;
        }
        parts.push(text.to_string());
    }
    let joined = collapse_whitespace(&parts.join(" "));
    (!joined.is_empty()).then_some(joined)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{BBox, Span};

    /// Build page 1 from `(text, font size)` pairs laid out top to bottom, one
    /// span per line.
    fn page_from(lines: &[(&str, f32)]) -> PageText {
        let mut page = PageText::new(1, 612.0, 792.0, 0);
        let mut y = 760.0_f32;
        let mut parts: Vec<&str> = Vec::new();
        for (i, (text, size)) in lines.iter().enumerate() {
            let width = text.chars().count() as f32 * size * 0.5;
            let bbox = BBox {
                x0: 72.0,
                y0: y,
                x1: 72.0 + width,
                y1: y + size,
            };
            page.spans.push(Span {
                text: (*text).to_string(),
                bbox: Some(bbox),
                font: None,
                size: Some(*size),
                seq: i as u32,
            });
            page.lines.push(Line {
                text: (*text).to_string(),
                bbox: Some(bbox),
                column: 0,
                spans: vec![i as u32],
            });
            parts.push(*text);
            y -= size * 1.4;
        }
        page.text = parts.join("\n");
        page
    }

    fn info_from(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    fn author_names(meta: &Metadata) -> Vec<&str> {
        meta.authors.iter().map(|a| a.name.as_str()).collect()
    }

    #[test]
    fn empty_input_gives_default() {
        let meta = extract_metadata(&BTreeMap::new(), &[]);
        assert_eq!(meta, Metadata::default());
    }

    #[test]
    fn info_dict_fields_carry_provenance() {
        let info = info_from(&[
            ("Title", "Deep Learning for Widgets"),
            ("Author", "Jane Doe, John Smith and Kim Lee"),
            ("Subject", "Journal of Testing"),
            ("Keywords", "widgets; deep learning"),
            ("CreationDate", "D:20200315120000Z"),
            ("Producer", "pdfTeX"),
        ]);
        let meta = extract_metadata(&info, &[]);
        assert_eq!(meta.title.as_deref(), Some("Deep Learning for Widgets"));
        assert_eq!(
            author_names(&meta),
            vec!["Jane Doe", "John Smith", "Kim Lee"]
        );
        assert_eq!(meta.venue.as_deref(), Some("Journal of Testing"));
        assert_eq!(meta.keywords, vec!["widgets", "deep learning"]);
        assert_eq!(meta.year, Some(2020));
        assert_eq!(meta.doi, None);
        assert_eq!(meta.abstract_text, None);
        assert_eq!(meta.info, info);
        assert_eq!(meta.provenance["title"], "info:Title");
        assert_eq!(meta.provenance["authors"], "info:Author");
        assert_eq!(meta.provenance["venue"], "info:Subject");
        assert_eq!(meta.provenance["keywords"], "info:Keywords");
        assert_eq!(meta.provenance["year"], "info:CreationDate");
    }

    #[test]
    fn generic_info_title_falls_back_to_page_one() {
        let info = info_from(&[("Title", "Microsoft Word - draft.docx"), ("Author", "")]);
        let page = page_from(&[
            ("Attention Is", 18.0),
            ("All You Need", 18.0),
            ("Ashish Vaswani1, Noam Shazeer2 and Niki Parmar1", 11.0),
            ("1Google Brain 2Google Research", 9.0),
            ("Abstract", 10.0),
            (
                "The dominant sequence transduction models are based on",
                10.0,
            ),
            (
                "complex recurrent networks. We propose a new architecture.",
                10.0,
            ),
            ("1 Introduction", 12.0),
            ("Recurrent neural networks have been established as", 10.0),
            ("© 2017 Copyright held by the owner/author(s).", 8.0),
            ("https://doi.org/10.1000/xyz123.", 8.0),
        ]);
        let meta = extract_metadata(&info, &[page]);
        assert_eq!(meta.title.as_deref(), Some("Attention Is All You Need"));
        assert_eq!(meta.provenance["title"], "page1:largest-font");
        assert_eq!(
            author_names(&meta),
            vec!["Ashish Vaswani", "Noam Shazeer", "Niki Parmar"]
        );
        assert_eq!(meta.provenance["authors"], "page1:authors");
        assert_eq!(meta.doi.as_deref(), Some("10.1000/xyz123"));
        assert_eq!(meta.provenance["doi"], "page1:doi");
        let expected_abstract = "The dominant sequence transduction models are based on complex \
                                 recurrent networks. We propose a new architecture.";
        assert_eq!(meta.abstract_text.as_deref(), Some(expected_abstract));
        assert_eq!(meta.provenance["abstract_text"], "page1:abstract");
        assert_eq!(meta.year, Some(2017));
        assert_eq!(meta.provenance["year"], "page1:year");
        assert_eq!(meta.venue, None);
        assert_eq!(meta.arxiv_id, None);
        assert_eq!(meta.info["Title"], "Microsoft Word - draft.docx");
    }

    #[test]
    fn arxiv_id_from_page_one_gives_year() {
        let page = page_from(&[
            ("arXiv:2001.01234v2 [cs.CL] 5 Jan 2020", 9.0),
            ("A Study of Things", 16.0),
            ("Jane Doe and John Smith", 11.0),
            ("Abstract. We study things.", 10.0),
            ("1 Introduction", 12.0),
        ]);
        let meta = extract_metadata(&BTreeMap::new(), &[page]);
        assert_eq!(meta.arxiv_id.as_deref(), Some("2001.01234v2"));
        assert_eq!(meta.provenance["arxiv_id"], "page1:arxiv");
        assert_eq!(meta.year, Some(2020));
        assert_eq!(meta.provenance["year"], "arxiv_id");
        assert_eq!(meta.title.as_deref(), Some("A Study of Things"));
        assert_eq!(author_names(&meta), vec!["Jane Doe", "John Smith"]);
        assert_eq!(meta.abstract_text.as_deref(), Some("We study things."));
    }

    #[test]
    fn old_style_arxiv_ids() {
        assert_eq!(
            find_arxiv_id("arXiv:hep-th/9901001").as_deref(),
            Some("hep-th/9901001")
        );
        assert_eq!(year_from_arxiv_id("hep-th/9901001"), Some(1999));
        assert_eq!(year_from_arxiv_id("math.AG/0701001"), Some(2007));
        assert_eq!(find_arxiv_id("no identifier here 2001.01234"), None);
    }

    #[test]
    fn doi_trimming_and_year() {
        assert_eq!(
            find_doi("doi:10.1109/TPAMI.2020.1234567).").as_deref(),
            Some("10.1109/TPAMI.2020.1234567")
        );
        assert_eq!(year_from_doi("10.1109/TPAMI.2020.1234567"), Some(2020));
        assert_eq!(year_from_doi("10.1038/s41586-020-2649-2"), None);
        assert_eq!(find_doi("nothing"), None);
    }

    #[test]
    fn superscript_spans_are_stripped_from_author_lines() {
        let mut page = page_from(&[
            ("A Title Here", 18.0),
            ("placeholder", 11.0),
            ("Abstract", 10.0),
            ("Text.", 10.0),
        ]);
        // Replace the placeholder line by a multi-span line with superscripts.
        let pieces: [(&str, f32); 4] = [
            ("John Smith", 11.0),
            ("a,b", 7.0),
            (", Jane Doe", 11.0),
            ("c", 7.0),
        ];
        let base = page.spans.len() as u32;
        let mut x = 72.0_f32;
        let mut indices: Vec<u32> = Vec::new();
        for (i, (text, size)) in pieces.iter().enumerate() {
            let width = text.chars().count() as f32 * size * 0.5;
            page.spans.push(Span {
                text: (*text).to_string(),
                bbox: Some(BBox {
                    x0: x,
                    y0: 700.0,
                    x1: x + width,
                    y1: 700.0 + size,
                }),
                font: None,
                size: Some(*size),
                seq: base + i as u32,
            });
            indices.push(base + i as u32);
            x += width;
        }
        page.lines[1].text = "John Smitha,b, Jane Doec".to_string();
        page.lines[1].spans = indices;
        let meta = extract_metadata(&BTreeMap::new(), &[page]);
        assert_eq!(author_names(&meta), vec!["John Smith", "Jane Doe"]);
    }

    #[test]
    fn uniform_font_page_has_no_title() {
        let page = page_from(&[
            ("Some running text on a page", 10.0),
            ("Jane Doe and John Smith", 10.0),
            ("More text follows here", 10.0),
        ]);
        let meta = extract_metadata(&BTreeMap::new(), &[page]);
        assert_eq!(meta.title, None);
        assert!(meta.authors.is_empty());
        assert!(meta.provenance.is_empty());
    }

    #[test]
    fn keywords_from_page_one() {
        let page = page_from(&[
            ("A Title", 18.0),
            ("Abstract", 10.0),
            ("Short.", 10.0),
            ("Keywords: graphs, networks; learning", 10.0),
            ("1 Introduction", 12.0),
        ]);
        let meta = extract_metadata(&BTreeMap::new(), &[page]);
        assert_eq!(meta.keywords, vec!["graphs", "networks", "learning"]);
        assert_eq!(meta.provenance["keywords"], "page1:keywords");
        assert_eq!(meta.abstract_text.as_deref(), Some("Short."));
    }

    #[test]
    fn generic_titles_and_authors() {
        assert!(title_is_generic("untitled"));
        assert!(title_is_generic("Microsoft Word - draft.docx"));
        assert!(title_is_generic("paper_final.tex"));
        assert!(title_is_generic(""));
        assert!(!title_is_generic("On the Origin of Species"));
        assert!(author_is_generic("Administrator"));
        assert!(!author_is_generic("Charles Darwin"));
        assert_eq!(
            split_author_names("Smith, Jr., John & Jane Doe"),
            vec!["Smith, Jr.", "John", "Jane Doe"]
        );
    }
}
