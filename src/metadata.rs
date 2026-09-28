//! Paper-level metadata: title, authors, DOI, arXiv id, year, venue, abstract
//! and keywords, taken from the PDF `/Info` dictionary and from the evidence
//! on page 1. Nothing is guessed: a field stays `None` unless a concrete
//! source supports it, and every field that is set records its provenance.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use regex::Regex;
use unicode_normalization::UnicodeNormalization;

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
/// Leading words the printed title must share with a differing `/Info` title
/// before the printed one replaces it.
const MIN_SHARED_LEADING_WORDS: usize = 3;
/// Fraction of the page height at the top and at the bottom that counts as
/// the running header / footer band.
const HEADER_FOOTER_BAND: f32 = 0.1;
/// Longest piece that a line wrap may split off a DOI and still be joined.
const MAX_DOI_WRAP_TOKEN: usize = 64;
/// Trailing punctuation that is never part of a DOI.
const DOI_TRAILING: [char; 8] = ['.', ',', ';', ')', ']', ':', '}', '\''];

/// Start of a DOI, tolerating the single spaces a wrap leaves inside the
/// prefix (`10. 1145/`, `10.1145 /`).
fn doi_start_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\b10\. ?\d{4,9} ?/").expect("valid regex"))
}

/// A DOI that runs to the end of a line (its suffix possibly empty).
fn doi_at_line_end_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"\b10\. ?\d{4,9} ?/[^\s"<>]*$"#).expect("valid regex"))
}

/// A bare year token (`2020`, `2020a`), never the rest of a wrapped DOI.
fn year_token_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(?:19|20)\d{2}[a-z]?$").expect("valid regex"))
}

/// `, Member, IEEE` / `, Senior Member, IEEE` / `, Fellow, IEEE` after a name.
fn ieee_membership_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i),?\s*\b(?:(?:student|senior|life|graduate\s+student)\s+)?(?:member|fellow)\s*,?\s*ieee\b",
        )
        .expect("valid regex")
    })
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
    RE.get_or_init(|| Regex::new(r"[\d¹²³⁴⁵⁶⁷⁸⁹⁰⁺*∗⋆★☆†‡§¶‖#✉]+").expect("valid regex"))
}

fn info_split_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\s*(?:,|;|&|·|\band\b)\s*").expect("valid regex"))
}

fn name_group_split_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\s*(?:;|&|\band\b)\s*").expect("valid regex"))
}

/// Extract metadata from the `/Info` dictionary and the first page.
///
/// `info` keys are the dictionary keys without the leading `/`. Page-1
/// evidence is used when the corresponding `info` entry is missing or generic
/// (for example `untitled` or `Microsoft Word - draft.docx`). A non-generic
/// `/Info` title is replaced by the largest-font title printed on page 1 when
/// the two differ but clearly name the same paper: the `/Info` title can be
/// stale (an earlier version's title) or drop the printed subtitle.
/// Every field that is set gets an entry in `provenance`.
pub fn extract_metadata(info: &BTreeMap<String, String>, pages: &[PageText]) -> Metadata {
    let mut meta = Metadata {
        info: info.clone(),
        ..Metadata::default()
    };
    let first_page: Option<&PageText> = pages.iter().find(|page| page.page == 1);

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
    if let Some(info_title) = meta.title.clone()
        && let Some(page) = first_page
        && let Some((indices, printed)) = title_block(page)
        && printed_title_supersedes(page, &info_title, &printed)
    {
        set_title(&mut meta, &printed, "first_page:largest-font");
        title_lines = indices;
    }
    if meta.title.is_none()
        && let Some(page) = first_page
        && let Some((indices, text)) = title_block(page)
    {
        set_title(&mut meta, &text, "first_page:largest-font");
        title_lines = indices;
    }

    // Authors.
    if let Some(author) = info
        .get("Author")
        .map(String::as_str)
        .map(str::trim)
        .filter(|a| !author_is_generic(a))
    {
        let names = split_info_authors(author);
        if !names.is_empty() {
            meta.authors = names.into_iter().map(named_author).collect();
            meta.provenance
                .insert("authors".to_string(), "info:Author".to_string());
        }
    }
    if meta.authors.is_empty()
        && let Some(page) = first_page
        && let Some((start, source)) = author_start(page, meta.title.as_deref(), &title_lines)
    {
        let names = page1_authors(page, start);
        if !names.is_empty() {
            meta.authors = names.into_iter().map(named_author).collect();
            meta.provenance
                .insert("authors".to_string(), source.to_string());
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
        && let Some(page) = first_page
        && let Some((doi, source)) = page1_doi(page)
    {
        meta.doi = Some(doi);
        meta.provenance
            .insert("doi".to_string(), source.to_string());
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
        && let Some(page) = first_page
        && let Some(id) = first_in_lines(page, find_arxiv_id)
    {
        meta.arxiv_id = Some(id);
        meta.provenance
            .insert("arxiv_id".to_string(), "first_page:arxiv".to_string());
    }
    // arXiv's own DOI for the preprint (`10.48550/arXiv.<id>`) also names it,
    // whether or not that DOI became `doi` above: fill `arxiv_id` from it
    // when page 1 carries one and nothing else already has.
    if meta.arxiv_id.is_none()
        && let Some(page) = first_page
        && let Some(id) = page1_arxiv_doi_id(page)
    {
        meta.arxiv_id = Some(id);
        meta.provenance
            .insert("arxiv_id".to_string(), "doi".to_string());
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
            .into_iter()
            .chain(info.get("Title").map(String::as_str))
            .any(|t| t.trim().eq_ignore_ascii_case(subject));
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
        && let Some(page) = first_page
        && let Some(list) = page1_keywords(page)
    {
        meta.keywords = list;
        meta.provenance
            .insert("keywords".to_string(), "first_page:keywords".to_string());
    }

    // Abstract.
    if let Some(page) = first_page
        && let Some(text) = page1_abstract(page)
    {
        meta.abstract_text = Some(text);
        meta.provenance.insert(
            "abstract_text".to_string(),
            "first_page:abstract".to_string(),
        );
    }

    // Year: arXiv id, DOI, Info dates, page 1.
    if let Some(year) = meta.arxiv_id.as_deref().and_then(year_from_arxiv_id) {
        set_year(&mut meta, year, "arxiv_id");
    } else if let Some(year) = meta.doi.as_deref().and_then(year_from_doi) {
        set_year(&mut meta, year, "doi");
    } else if let Some((key, year)) = info_year(info) {
        set_year(&mut meta, year, &format!("info:{key}"));
    } else if let Some(page) = first_page
        && let Some(year) = page1_year(page)
    {
        set_year(&mut meta, year, "first_page:year");
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

/// Split a printed author line on commas, semicolons, `&`, the middle dot
/// that Springer prints between authors, and `and`.
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

/// Author names from an `/Info Author` string.
///
/// The string is split on `and`, `&` and `;` first. A part with exactly one
/// comma is handled by `comma_pair_candidates` so that `Lovelace, Ada` stays
/// one person while `Ada Lovelace, Charles Babbage` still splits. Every
/// candidate must pass `is_acceptable_info_name`; login names, product
/// names, organisations and addresses are dropped.
fn split_info_authors(text: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for part in name_group_split_re().split(text) {
        let collapsed = collapse_whitespace(part);
        let group: &str = collapsed.as_str();
        let candidates: Vec<String> = match group.split_once(',') {
            Some((left, right)) if !right.contains(',') => {
                comma_pair_candidates(left.trim(), right.trim())
            }
            Some(_) => split_author_names(group),
            None => vec![group.to_string()],
        };
        for candidate in candidates {
            if !candidate.is_empty() && is_acceptable_info_name(&candidate) {
                names.push(candidate);
            }
        }
    }
    names
}

/// Candidates from an `/Info` part that contains exactly one comma.
///
/// `Ada Lovelace, Charles Babbage` is a list of two people. When the part
/// before the comma is not itself a full name it is read as `Last, First`:
/// `Lovelace, Ada` and `Smith, J. R.` become `Ada Lovelace` and
/// `J. R. Smith`, while a longer given-name part (`Lovelace, Ada King`) and a
/// suffix (`John Smith, Jr.`) are kept as printed.
fn comma_pair_candidates(left: &str, right: &str) -> Vec<String> {
    if left.is_empty() || right.is_empty() {
        return vec![format!("{left}{right}")];
    }
    if is_name_suffix(right) {
        return vec![format!("{left}, {right}")];
    }
    if looks_like_person_name(left) {
        return vec![left.to_string(), right.to_string()];
    }
    if is_given_name_or_initials(right) {
        return vec![format!("{right} {left}")];
    }
    vec![format!("{left}, {right}")]
}

/// True for the part after the comma in `Last, First` when it is a single
/// given name (`Ada`) or only initials (`J. R.`, `J.-P.`).
fn is_given_name_or_initials(text: &str) -> bool {
    let tokens: Vec<&str> = text.split_whitespace().collect();
    match tokens.as_slice() {
        [] => false,
        [single] => is_capitalised_word(single),
        _ => tokens.len() <= 3 && tokens.iter().all(|t| is_initials(t)),
    }
}

/// A token starting with an upper-case letter and made of letters, dots,
/// hyphens and apostrophes.
fn is_capitalised_word(token: &str) -> bool {
    let mut chars = token.chars();
    chars.next().is_some_and(char::is_uppercase)
        && chars.all(|c| c.is_alphabetic() || matches!(c, '\'' | '’' | '-' | '.'))
}

/// `A.`, `A.B.` or `J.-P.`: one to three upper-case letters with dots or hyphens.
fn is_initials(token: &str) -> bool {
    let letters = token.chars().filter(|c| c.is_alphabetic()).count();
    let allowed = token
        .chars()
        .all(|c| c.is_uppercase() || matches!(c, '.' | '-'));
    (1..=3).contains(&letters) && allowed
}

/// True for an `/Info`-derived candidate that names a person rather than a
/// login, product, organisation or address.
fn is_acceptable_info_name(candidate: &str) -> bool {
    looks_like_person_name(candidate)
        && !affiliation_re().is_match(candidate)
        && !has_generic_name_token(candidate)
}

/// True when a word of `name` is a login, product or placeholder word such as
/// `User` or `Office`.
fn has_generic_name_token(name: &str) -> bool {
    name.split_whitespace().any(|token| {
        let lower = token
            .trim_matches(|c: char| !c.is_alphanumeric())
            .to_lowercase();
        matches!(
            lower.as_str(),
            "user"
                | "users"
                | "admin"
                | "administrator"
                | "office"
                | "microsoft"
                | "windows"
                | "account"
                | "guest"
                | "owner"
                | "unknown"
                | "author"
                | "default"
                | "anonymous"
                | "pc"
                | "computer"
                | "laptop"
                | "desktop"
                | "adobe"
                | "acrobat"
                | "latex"
                | "pdftex"
                | "libreoffice"
                | "openoffice"
        )
    })
}

fn split_keywords(text: &str) -> Vec<String> {
    text.split([',', ';', '·', '•'])
        .map(|k| collapse_whitespace(k.trim_matches(|c: char| c == '.' || c.is_whitespace())))
        .filter(|k| !k.is_empty())
        .collect()
}

/// Byte length of the identifier token at the start of `text`: up to the
/// first whitespace, quote or angle bracket.
fn id_token_len(text: &str) -> usize {
    text.find(|c: char| c.is_whitespace() || matches!(c, '"' | '<' | '>'))
        .unwrap_or(text.len())
}

/// Can `token` be the rest of a DOI that a line wrap split off
/// (`3330701`, `BF01504345.`)? It must carry a digit and at least three
/// characters; a bare year or anything starting a URL or parenthesis cannot.
fn doi_wrap_token(token: &str) -> bool {
    let core = token.trim_end_matches(['.', ',', ';', ')']);
    let chars = core.chars().count();
    if !(3..=MAX_DOI_WRAP_TOKEN).contains(&chars) || core.starts_with('(') {
        return false;
    }
    core.chars().any(|c| c.is_ascii_digit())
        && !year_token_re().is_match(core)
        && !core.to_ascii_lowercase().starts_with("http")
}

/// First DOI in `text`, with trailing punctuation removed.
///
/// Accepts the bare form and any prefix before it (`doi:10.`, `DOI: 10.`,
/// `https://doi.org/10.`). Single spaces that a line wrap left inside the
/// DOI are closed up, as `citations::find_doi` does: `10. 1145/…`,
/// `10.1145/ 3292500`, `10.1145/3292500. 3330701`. A piece is joined when
/// the DOI so far ends in `/ . - _` or the piece is a run of at least three
/// digits, and the piece passes `doi_wrap_token` (so an ISBN or a year after
/// the DOI is never glued on).
fn find_doi(text: &str) -> Option<String> {
    let found = doi_start_re().find(text)?;
    let start = found.start();
    let mut end = found.end() + id_token_len(&text[found.end()..]);
    while let Some(rest) = text[end..].strip_prefix(' ') {
        let token_len = id_token_len(rest);
        if token_len == 0 {
            break;
        }
        let token = &rest[..token_len];
        let consumed = &text[start..end];
        // A sentence-final `.` followed by an ISBN / price line
        // (`978-1-7281-1234-5/20/$31.00`) is not a wrap.
        if consumed.ends_with([',', ';']) || (consumed.ends_with('.') && token.contains(['$', '/']))
        {
            break;
        }
        // Only a DOI cut right after a separator continues on the next token;
        // a complete DOI followed by a page or article number stays as it is.
        let after_separator = consumed.ends_with(['/', '.', '-', '_']);
        if !after_separator || !doi_wrap_token(token) {
            break;
        }
        end += 1 + token_len;
    }
    let joined: String = text[start..end]
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let trimmed = joined.trim_end_matches(DOI_TRAILING);
    let suffix_ok = trimmed
        .split_once('/')
        .is_some_and(|(_, suffix)| suffix.chars().any(char::is_alphanumeric));
    if trimmed.len() < 8 || !suffix_ok {
        return None;
    }
    Some(trimmed.to_string())
}

/// DOI printed on line `index` of `page`, joined with the start of the next
/// line when the DOI runs to the end of the line and is cut after `10.NNNN/`
/// or after a `/ . - _` separator (a DOI split across a line break in a
/// footer or header).
fn line_doi(page: &PageText, index: usize) -> Option<String> {
    let text = page.lines.get(index)?.text.trim();
    let single = find_doi(text);
    let cut = doi_at_line_end_re()
        .find(text)
        .is_some_and(|m| m.as_str().ends_with(['/', '.', '-', '_']));
    if cut && let Some(next) = page.lines.get(index + 1) {
        let combined = format!("{text} {}", next.text.trim());
        if let Some(joined) = find_doi(&combined)
            && single.as_ref().is_none_or(|s| joined.len() > s.len())
        {
            return Some(joined);
        }
    }
    single
}

/// True when `line` lies in the top or bottom [`HEADER_FOOTER_BAND`] of the
/// page (PDF coordinates, y grows upwards). False without a bbox or height.
fn in_header_footer(page: &PageText, line: &Line) -> bool {
    if page.height <= 0.0 {
        return false;
    }
    let top = page.height * (1.0 - HEADER_FOOTER_BAND);
    let bottom = page.height * HEADER_FOOTER_BAND;
    line.bbox.is_some_and(|b| b.y0 >= top || b.y1 <= bottom)
}

/// Prefix of arXiv's own DOI for a preprint (`10.48550/arXiv.<id>`, the DOI
/// arXiv assigns to every submission), matched case-insensitively.
const ARXIV_DOI_PREFIX: &str = "10.48550/arxiv.";

/// The arXiv id embedded in `doi` when it is arXiv's own DOI for the
/// preprint (`10.48550/arXiv.<id>`, any case); `None` for a publisher's DOI.
fn arxiv_id_from_doi(doi: &str) -> Option<&str> {
    let prefix = doi.get(..ARXIV_DOI_PREFIX.len())?;
    if prefix.eq_ignore_ascii_case(ARXIV_DOI_PREFIX) {
        Some(&doi[ARXIV_DOI_PREFIX.len()..])
    } else {
        None
    }
}

/// True when `doi` is arXiv's own DOI for the preprint rather than a
/// publisher's DOI for a copy of it.
fn is_arxiv_doi(doi: &str) -> bool {
    arxiv_id_from_doi(doi).is_some()
}

/// The paper's own DOI on page 1 with its provenance.
///
/// A DOI in the running header or footer band (the paper's own DOI in ACM,
/// IEEE and Springer layouts) wins with `first_page:doi-header-footer`; then
/// a DOI on a line that labels it (`DOI`, `doi.org`); then the first DOI in
/// reading order, both with `first_page:doi`. arXiv's own DOI for the
/// preprint (`10.48550/arXiv.<id>`) never wins by the header/footer or
/// label rules: a paper that is also published elsewhere (ACM, IEEE,
/// Springer...) prints that publisher's DOI on the same page, and it always
/// describes the copy being read while the arXiv DOI merely names the
/// preprint. When no other DOI has that evidence, the arXiv DOI outranks a
/// bare DOI in running text (usually a citation in the abstract), which is
/// used only when page 1 carries no arXiv DOI at all.
fn page1_doi(page: &PageText) -> Option<(String, &'static str)> {
    let found: Vec<(usize, String)> = (0..page.lines.len())
        .filter_map(|i| line_doi(page, i).map(|doi| (i, doi)))
        .collect();
    let labelled = |i: usize| page.lines[i].text.to_lowercase().contains("doi");
    // Publisher furniture: a short line (no running sentence) in the margin
    // band. A cited DOI inside a paragraph that merely reaches the band must
    // not outrank the paper's labelled DOI.
    let furniture = |i: usize| {
        in_header_footer(page, &page.lines[i])
            && page.lines[i].text.split_whitespace().count() <= 12
    };
    let candidates: Vec<(usize, String)> = found
        .iter()
        .filter(|(_, doi)| !is_arxiv_doi(doi))
        .cloned()
        .collect();
    if let Some((_, doi)) = candidates
        .iter()
        .find(|(i, _)| furniture(*i) && labelled(*i))
    {
        return Some((doi.clone(), "first_page:doi-header-footer"));
    }
    if let Some((_, doi)) = candidates.iter().find(|(i, _)| labelled(*i)) {
        return Some((doi.clone(), "first_page:doi"));
    }
    if let Some((_, doi)) = candidates.iter().find(|(i, _)| furniture(*i)) {
        return Some((doi.clone(), "first_page:doi-header-footer"));
    }
    // No positive evidence for a publisher DOI: arXiv's own DOI names this
    // very paper, while a bare DOI in running text is usually a citation.
    if let Some((_, doi)) = found.iter().find(|(_, doi)| is_arxiv_doi(doi)) {
        return Some((doi.clone(), "first_page:doi"));
    }
    candidates
        .into_iter()
        .next()
        .map(|(_, doi)| (doi, "first_page:doi"))
}

/// The arXiv id inside the first `10.48550/arXiv.<id>` DOI printed anywhere
/// on page 1, whether or not that DOI became [`page1_doi`]'s result.
fn page1_arxiv_doi_id(page: &PageText) -> Option<String> {
    (0..page.lines.len())
        .filter_map(|i| line_doi(page, i))
        .find_map(|doi| arxiv_id_from_doi(&doi).map(str::to_string))
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

/// True for a line holding vertically set text, such as the rotated
/// `arXiv:<id> [cs.XX]` stamp in the left margin: one of its spans has a box
/// more than twice as tall as it is wide and taller than 1.5 times its font
/// size (a horizontal span's box is about one font size tall). Checked per
/// span so that a stamp grouped with a horizontal line is still recognised.
fn is_rotated_line(page: &PageText, line: &Line) -> bool {
    line.spans.iter().any(|idx| {
        page.spans.get(*idx as usize).is_some_and(|span| {
            span.bbox.is_some_and(|bbox| {
                let width = bbox.x1 - bbox.x0;
                let height = bbox.y1 - bbox.y0;
                height > 2.0 * width && height > 1.5 * span.size.unwrap_or(0.0)
            })
        })
    })
}

/// The group of consecutive largest-font lines near the top of the page.
///
/// Vertically set lines (the `arXiv` margin stamp) are never part of it.
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
        if line.bbox.is_some_and(|b| b.y1 < top_limit) || is_rotated_line(page, line) {
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
    while let Some(line) = page.lines.get(next) {
        if is_rotated_line(page, line) {
            next += 1;
            continue;
        }
        let large = line_size(page, line).is_some_and(|size| size >= threshold);
        if !large || !line.text.trim().chars().any(char::is_alphabetic) {
            break;
        }
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

/// Index of the first page-1 line after the title, with the provenance to
/// record for authors found there.
///
/// `title_lines` are the page-1 title lines when the title itself came from
/// page 1. Otherwise the (`/Info`) `title` is located on the page as a run of
/// consecutive lines whose normalised text concatenates to the normalised
/// title, falling back to the largest-font block, so that the author lines
/// below the printed title can still be read.
fn author_start(
    page: &PageText,
    title: Option<&str>,
    title_lines: &[usize],
) -> Option<(usize, &'static str)> {
    if let Some(&last) = title_lines.last() {
        return Some((last + 1, "first_page:authors"));
    }
    let lines = title
        .and_then(|t| matching_title_lines(page, t))
        .or_else(|| title_block(page).map(|(indices, _)| indices))?;
    let last = *lines.last()?;
    Some((last + 1, "page1:below-title"))
}

/// Indices of consecutive page lines whose normalised text concatenates to
/// the normalised `title` (a title printed on one line or wrapped over
/// several). Lines that normalise to nothing are skipped inside a run.
fn matching_title_lines(page: &PageText, title: &str) -> Option<Vec<usize>> {
    let target = normalize_for_match(title);
    if target.is_empty() {
        return None;
    }
    let pieces: Vec<String> = page
        .lines
        .iter()
        .map(|line| normalize_for_match(&line.text))
        .collect();
    for start in 0..pieces.len() {
        let mut joined = String::new();
        let mut indices: Vec<usize> = Vec::new();
        for (i, piece) in pieces.iter().enumerate().skip(start) {
            if piece.is_empty() {
                if indices.is_empty() {
                    break;
                }
                continue;
            }
            joined.push_str(piece);
            indices.push(i);
            if joined == target {
                return Some(indices);
            }
            if !target.starts_with(joined.as_str()) {
                break;
            }
        }
    }
    None
}

/// [`normalize_for_match`] after NFKC folding (ligatures, full-width forms).
fn fold_for_match(text: &str) -> String {
    let compat: String = text.nfkc().collect();
    normalize_for_match(&compat)
}

/// Lower-case alphanumeric words of `text` after NFKC folding, in order.
fn match_words(text: &str) -> Vec<String> {
    let compat: String = text.nfkc().collect();
    compat
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Whether the title `printed` in the largest font on page 1 should replace
/// the (non-generic) `/Info` title `info_title`.
///
/// Compared on lower-case letters and digits only, so case, spacing,
/// punctuation, hyphenated wraps and footnote symbols never count as a
/// difference:
/// - equal, or `printed` a strict prefix of `info_title` (a truncated block):
///   keep `/Info`;
/// - `info_title` a strict prefix of `printed`: replace only when the text of
///   `printed` before one of its `:` equals `info_title`, i.e. the `/Info`
///   title dropped the printed subtitle (a trailing footnote digit or an
///   author line swallowed by the block is not a subtitle);
/// - otherwise replace when both start with the same
///   [`MIN_SHARED_LEADING_WORDS`] words, neither is more than twice as long
///   as the other in words, and `info_title` itself is not printed on the
///   page (a stale `/Info` title from an earlier version of the paper).
fn printed_title_supersedes(page: &PageText, info_title: &str, printed: &str) -> bool {
    let info_key = fold_for_match(info_title);
    let printed_key = fold_for_match(printed);
    if info_key.is_empty() || printed_key.is_empty() || info_key == printed_key {
        return false;
    }
    if info_key.starts_with(printed_key.as_str()) {
        return false;
    }
    if printed_key.starts_with(info_key.as_str()) {
        return printed
            .match_indices(':')
            .any(|(pos, _)| fold_for_match(&printed[..pos]) == info_key);
    }
    let info_words = match_words(info_title);
    let printed_words = match_words(printed);
    let shared = info_words
        .iter()
        .zip(printed_words.iter())
        .take_while(|(a, b)| a == b)
        .count();
    if shared < MIN_SHARED_LEADING_WORDS {
        return false;
    }
    if printed_words.len() > 2 * info_words.len() || info_words.len() > 2 * printed_words.len() {
        return false;
    }
    matching_title_lines(page, info_title).is_none()
}

/// Lower-case alphanumeric characters only, for comparing printed text with
/// an `/Info` value regardless of spacing, case and punctuation.
fn normalize_for_match(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
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
        let stripped = ieee_membership_re().replace_all(&stripped, "");
        let stripped = marker_chars_re().replace_all(&stripped, "");
        if last_segment_is_country(&stripped) {
            continue;
        }
        let candidates: Vec<String> = split_author_names(&stripped)
            .into_iter()
            .filter(|c| !is_marker_letters(c))
            .collect();
        if candidates.is_empty() || !candidates.iter().all(|c| is_page1_person_name(c)) {
            continue;
        }
        names.extend(candidates);
    }
    names
}

/// A page-1 author candidate: a person name that is not a front-matter
/// label (`ARTICLE INFO`, `Corresponding Author`) and not a country.
fn is_page1_person_name(candidate: &str) -> bool {
    looks_like_person_name(candidate)
        && !has_front_matter_token(candidate)
        && !is_country(candidate)
}

/// One or two lower-case letters left over from affiliation markers printed
/// in the body font (`Jane Doe1,a,b` gives the pieces `a` and `b`).
fn is_marker_letters(piece: &str) -> bool {
    let count = piece.chars().count();
    (1..=2).contains(&count) && piece.chars().all(|c| c.is_ascii_lowercase())
}

/// True when a word of `candidate` is a front-matter label word rather than
/// part of a name (`ARTICLE INFO`, `Received`, `Corresponding Author`).
fn has_front_matter_token(candidate: &str) -> bool {
    candidate.split_whitespace().any(|token| {
        let lower = token
            .trim_matches(|c: char| !c.is_alphanumeric())
            .to_lowercase();
        matches!(
            lower.as_str(),
            "article"
                | "info"
                | "information"
                | "highlights"
                | "abstract"
                | "received"
                | "revised"
                | "accepted"
                | "published"
                | "corresponding"
                | "correspondence"
                | "author"
                | "authors"
                | "preprint"
                | "copyright"
                | "keywords"
                | "submitted"
                | "journal"
                | "proceedings"
                | "conference"
                | "volume"
                | "contents"
                | "homepage"
                | "equal"
                | "contribution"
                | "email"
                | "e-mail"
                | "orcid"
                | "affiliation"
                | "affiliations"
        )
    })
}

/// True for a country name (`China`, `The Netherlands`, `United States`),
/// compared case-insensitively after trimming punctuation. Used on whole
/// comma-separated segments only, never on single name tokens, so names such
/// as `Michael I. Jordan` stay.
fn is_country(segment: &str) -> bool {
    let lower = collapse_whitespace(
        segment
            .trim_matches(|c: char| !c.is_alphanumeric())
            .to_lowercase()
            .as_str(),
    );
    let name = lower.strip_prefix("the ").unwrap_or(&lower);
    matches!(
        name,
        "usa"
            | "u.s.a"
            | "us"
            | "uk"
            | "u.k"
            | "united states"
            | "united states of america"
            | "united kingdom"
            | "united arab emirates"
            | "netherlands"
            | "new zealand"
            | "south korea"
            | "republic of korea"
            | "korea"
            | "hong kong"
            | "saudi arabia"
            | "south africa"
            | "czech republic"
            | "china"
            | "p.r. china"
            | "pr china"
            | "japan"
            | "germany"
            | "france"
            | "italy"
            | "spain"
            | "portugal"
            | "canada"
            | "australia"
            | "india"
            | "singapore"
            | "switzerland"
            | "austria"
            | "belgium"
            | "denmark"
            | "finland"
            | "norway"
            | "sweden"
            | "poland"
            | "greece"
            | "ireland"
            | "russia"
            | "taiwan"
            | "turkey"
            | "mexico"
            | "brazil"
            | "argentina"
            | "chile"
            | "egypt"
            | "iran"
            | "pakistan"
            | "vietnam"
            | "thailand"
            | "malaysia"
            | "indonesia"
    )
}

/// True when the last comma-separated segment of an author-block line is a
/// country: the line is an address (`Delft, The Netherlands`), not names.
fn last_segment_is_country(text: &str) -> bool {
    text.rsplit(',')
        .map(str::trim)
        .find(|segment| !segment.is_empty())
        .is_some_and(is_country)
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
        assert_eq!(meta.provenance["title"], "first_page:largest-font");
        assert_eq!(
            author_names(&meta),
            vec!["Ashish Vaswani", "Noam Shazeer", "Niki Parmar"]
        );
        assert_eq!(meta.provenance["authors"], "first_page:authors");
        assert_eq!(meta.doi.as_deref(), Some("10.1000/xyz123"));
        assert_eq!(meta.provenance["doi"], "first_page:doi");
        let expected_abstract = "The dominant sequence transduction models are based on complex \
                                 recurrent networks. We propose a new architecture.";
        assert_eq!(meta.abstract_text.as_deref(), Some(expected_abstract));
        assert_eq!(meta.provenance["abstract_text"], "first_page:abstract");
        assert_eq!(meta.year, Some(2017));
        assert_eq!(meta.provenance["year"], "first_page:year");
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
        assert_eq!(meta.provenance["arxiv_id"], "first_page:arxiv");
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
        assert_eq!(meta.provenance["keywords"], "first_page:keywords");
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

    #[test]
    fn info_title_without_author_reads_authors_below_page_one_title() {
        let info = info_from(&[("Title", "Attention Is All You Need")]);
        let page = page_from(&[
            ("Attention Is", 18.0),
            ("All You Need", 18.0),
            ("Ashish Vaswani1, Noam Shazeer2 and Niki Parmar1", 11.0),
            ("1Google Brain 2Google Research", 9.0),
            ("Abstract", 10.0),
            ("We propose a new architecture.", 10.0),
        ]);
        let meta = extract_metadata(&info, &[page]);
        assert_eq!(meta.title.as_deref(), Some("Attention Is All You Need"));
        assert_eq!(meta.provenance["title"], "info:Title");
        assert_eq!(
            author_names(&meta),
            vec!["Ashish Vaswani", "Noam Shazeer", "Niki Parmar"]
        );
        assert_eq!(meta.provenance["authors"], "page1:below-title");
    }

    #[test]
    fn info_title_absent_from_page_falls_back_to_largest_font_block() {
        let info = info_from(&[("Title", "Final Camera Ready Version")]);
        let page = page_from(&[
            ("Attention Is All You Need", 18.0),
            ("Jane Doe and John Smith", 11.0),
            ("Abstract", 10.0),
            ("Text.", 10.0),
        ]);
        let meta = extract_metadata(&info, &[page]);
        assert_eq!(meta.title.as_deref(), Some("Final Camera Ready Version"));
        assert_eq!(meta.provenance["title"], "info:Title");
        assert_eq!(author_names(&meta), vec!["Jane Doe", "John Smith"]);
        assert_eq!(meta.provenance["authors"], "page1:below-title");
    }

    /// Put the rotated `arXiv` margin stamp (20 pt, a tall narrow box) first
    /// in reading order, as the column split of the reading order does.
    fn with_arxiv_stamp(mut page: PageText, stamp: &str) -> PageText {
        let bbox = BBox {
            x0: 12.0,
            y0: 220.0,
            x1: 32.0,
            y1: 570.0,
        };
        let index = page.spans.len() as u32;
        page.spans.push(Span {
            text: stamp.to_string(),
            bbox: Some(bbox),
            font: None,
            size: Some(20.0),
            seq: index,
        });
        page.lines.insert(
            0,
            Line {
                text: stamp.to_string(),
                bbox: Some(bbox),
                column: 0,
                spans: vec![index],
            },
        );
        page.text = format!("{stamp}\n{}", page.text);
        page
    }

    #[test]
    fn rotated_arxiv_stamp_is_never_the_title() {
        let page = with_arxiv_stamp(
            page_from(&[
                ("Kernel Widgets for Everyone", 14.0),
                ("Jane Doe", 11.0),
                ("Abstract", 10.0),
                ("Body text one.", 10.0),
                ("Body text two.", 10.0),
            ]),
            "arXiv:2501.00001v1 [cs.LG] 1 Jan 2025",
        );
        assert!(is_rotated_line(&page, &page.lines[0]));
        assert!(!is_rotated_line(&page, &page.lines[1]));
        // The stamp span grouped with a wide horizontal line still marks it.
        let stamp_index = page.lines[0].spans[0];
        let mut merged = page.lines[4].clone();
        merged.spans.insert(0, stamp_index);
        if let (Some(a), Some(b)) = (merged.bbox, page.lines[0].bbox) {
            merged.bbox = Some(BBox {
                x0: a.x0.min(b.x0),
                y0: a.y0.min(b.y0),
                x1: a.x1.max(b.x1),
                y1: a.y1.max(b.y1),
            });
        }
        assert!(is_rotated_line(&page, &merged));
        let meta = extract_metadata(&BTreeMap::new(), &[page]);
        assert_eq!(meta.title.as_deref(), Some("Kernel Widgets for Everyone"));
        assert_eq!(meta.provenance["title"], "first_page:largest-font");
    }

    /// arXiv:2505.16990: `/Info` carries the title of an earlier version; page
    /// 1 prints the `\title` of the current one.
    #[test]
    fn stale_info_title_is_replaced_by_printed_title() {
        let stale =
            "Dimple: Discrete Diffusion Multimodal Large Language Model with Parallel Decoding";
        let info = info_from(&[("Title", stale), ("Subject", stale)]);
        let page = with_arxiv_stamp(
            page_from(&[
                ("Dimple: Discrete Diffusion Parallel Generation for", 14.3),
                ("Large Multimodal Modal", 14.3),
                ("Runpeng Yu Xinyin Ma Xinchao Wang*", 12.0),
                ("National University of Singapore", 12.0),
                ("Abstract", 12.0),
                ("This paper introduces Dimple and Dimple+,", 10.0),
                ("two discrete diffusion multimodal large lan-", 10.0),
                ("guage models (dMLLMs). Dimple is derived", 10.0),
            ]),
            "arXiv:2505.16990v3 [cs.CV] 30 Aug 2026",
        );
        let meta = extract_metadata(&info, &[page]);
        assert_eq!(
            meta.title.as_deref(),
            Some("Dimple: Discrete Diffusion Parallel Generation for Large Multimodal Modal")
        );
        assert_eq!(meta.provenance["title"], "first_page:largest-font");
        // A Subject that repeats the (stale) `/Info` title is not a venue.
        assert_eq!(meta.venue, None);
    }

    /// arXiv:2305.13843: `/Info` says "Recommender System", page 1 prints
    /// "Recommender Systems".
    #[test]
    fn info_title_differing_in_one_word_is_replaced_by_printed_title() {
        let info = info_from(&[(
            "Title",
            "Advances and Challenges of Multi-task Learning Method in Recommender System: A Survey",
        )]);
        let page = page_from(&[
            (
                "Advances and Challenges of Multi-task Learning Method in",
                17.0,
            ),
            ("Recommender Systems: A Survey", 17.0),
            (
                "Mingzhu Zhang a , Ruiping Yin a,\u{2217} , Zhen Yang a and Yipeng Wang a",
                11.0,
            ),
            (
                "a Beijing University of Technology, Beijing, 100124, China",
                8.0,
            ),
            ("ABSTRACT", 10.0),
            (
                "Multi-task learning (MTL) has been widely applied to modern",
                10.0,
            ),
            (
                "RSs, enabling the simultaneous optimization of diverse",
                10.0,
            ),
        ]);
        let meta = extract_metadata(&info, &[page]);
        assert_eq!(
            meta.title.as_deref(),
            Some(
                "Advances and Challenges of Multi-task Learning Method in Recommender Systems: A Survey"
            )
        );
        assert_eq!(meta.provenance["title"], "first_page:largest-font");
    }

    /// arXiv:2608.28714: `/Info` drops the subtitle printed after the `:`.
    #[test]
    fn info_title_without_printed_subtitle_is_replaced() {
        let info = info_from(&[(
            "Title",
            "Evaluating the Safety of Deep Learning-Based Brain MRI Reconstruction",
        )]);
        let page = page_from(&[
            ("1", 8.0),
            ("Evaluating the Safety of Deep Learning-Based", 24.0),
            ("Brain MRI Reconstruction:", 24.0),
            ("A Systematic Review of Current Evaluation", 24.0),
            ("Practices", 24.0),
            (
                "Dat Tat Mai, Thai Viet Pham, Thu Nguyen Thi Dang, and James Jin Kang",
                11.0,
            ),
            (
                "Abstract\u{2014}Objective: Deep learning accelerates brain MRI",
                9.0,
            ),
            (
                "four- to tenfold, but learned models can erase a lesion or invent",
                9.0,
            ),
            (
                "tissue, and pixel-averaged scores such as PSNR and SSIM miss",
                9.0,
            ),
        ]);
        let meta = extract_metadata(&info, &[page]);
        assert_eq!(
            meta.title.as_deref(),
            Some(
                "Evaluating the Safety of Deep Learning-Based Brain MRI Reconstruction: \
                 A Systematic Review of Current Evaluation Practices"
            )
        );
        assert_eq!(meta.provenance["title"], "first_page:largest-font");
    }

    #[test]
    fn printed_title_supersedes_only_on_clear_evidence() {
        let page = page_from(&[("Body", 10.0)]);
        let keep = |info: &str, printed: &str| !printed_title_supersedes(&page, info, printed);
        // Same words up to case, punctuation, footnote symbols and wraps.
        assert!(keep(
            "A Practical Mode-parallel Implementation of the (H-)Tucker Decomposition via Randomization",
            "A PRACTICAL MODE-PARALLEL IMPLEMENTATION OF THE (H-)TUCKER DECOMPOSITION VIA RANDOMIZATION"
        ));
        assert!(keep(
            "Personality pairing improves human-AI collaboration",
            "Personality pairing improves human\u{2013}AI collaboration \u{2217}"
        ));
        assert!(keep("Robust Reconstruction", "Robust Recon- struction"));
        // A footnote digit or a swallowed author line is not a subtitle.
        assert!(keep(
            "Attention Is All You Need",
            "Attention Is All You Need1"
        ));
        assert!(keep(
            "Attention Is All You Need",
            "Attention Is All You Need Ashish Vaswani"
        ));
        // A truncated block never replaces the full `/Info` title.
        assert!(keep("Attention Is All You Need", "Attention Is"));
        // Unrelated or barely related printed text.
        assert!(keep(
            "Final Camera Ready Version",
            "Attention Is All You Need"
        ));
        assert!(keep(
            "Deep Widgets for Graphs",
            "Deep Widgets: A Survey of Everything"
        ));
        // An `/Info` title with its own `:` still gains a printed subtitle.
        assert!(!keep(
            "HintEval: A Python Toolkit",
            "HintEval: A Python Toolkit: Hint Generation and Evaluation"
        ));
    }

    #[test]
    fn info_title_printed_on_the_page_is_kept_over_a_different_block() {
        let info = info_from(&[("Title", "Kernel Widgets for Graph Learning")]);
        let page = page_from(&[
            ("Kernel Widgets for Everyone and More", 18.0),
            ("Kernel Widgets for Graph Learning", 12.0),
            ("Jane Doe", 10.0),
            ("Body.", 10.0),
        ]);
        let meta = extract_metadata(&info, &[page]);
        assert_eq!(
            meta.title.as_deref(),
            Some("Kernel Widgets for Graph Learning")
        );
        assert_eq!(meta.provenance["title"], "info:Title");
    }

    #[test]
    fn info_title_is_located_across_wrapped_lines() {
        let page = page_from(&[
            ("Journal of Testing 12(3)", 8.0),
            ("Attention Is", 18.0),
            ("All You Need", 18.0),
            ("Jane Doe", 11.0),
        ]);
        assert_eq!(
            matching_title_lines(&page, "Attention is all you need!"),
            Some(vec![1, 2])
        );
        assert_eq!(matching_title_lines(&page, "Something Else"), None);
        assert_eq!(matching_title_lines(&page, "..."), None);
    }

    #[test]
    fn info_author_surname_first_is_one_person() {
        let info = info_from(&[("Author", "Lovelace, Ada")]);
        let meta = extract_metadata(&info, &[]);
        assert_eq!(author_names(&meta), vec!["Ada Lovelace"]);
        assert_eq!(meta.provenance["authors"], "info:Author");
    }

    #[test]
    fn info_author_list_of_full_names_splits() {
        let author = "Ada Lovelace, Charles Babbage and Grace Hopper";
        let info = info_from(&[("Author", author)]);
        let meta = extract_metadata(&info, &[]);
        assert_eq!(
            author_names(&meta),
            vec!["Ada Lovelace", "Charles Babbage", "Grace Hopper"]
        );
        assert_eq!(meta.provenance["authors"], "info:Author");
    }

    #[test]
    fn generic_info_author_yields_no_authors() {
        let info = info_from(&[("Author", "Microsoft Office User")]);
        let meta = extract_metadata(&info, &[]);
        assert!(meta.authors.is_empty());
        assert!(!meta.provenance.contains_key("authors"));
    }

    #[test]
    fn info_author_splitting_cases() {
        assert_eq!(split_info_authors("Smith, J. R."), vec!["J. R. Smith"]);
        assert_eq!(split_info_authors("Ann Lee, Jr."), vec!["Ann Lee, Jr."]);
        assert_eq!(split_info_authors("de Vries, Ada"), vec!["Ada de Vries"]);
        assert_eq!(
            split_info_authors("Jane Doe; jane.doe@example.com; et al."),
            vec!["Jane Doe"]
        );
        assert_eq!(
            split_info_authors("Jane Doe & John Smith"),
            vec!["Jane Doe", "John Smith"]
        );
        assert!(split_info_authors("Microsoft Office User").is_empty());
        assert!(split_info_authors("MIT Media Lab").is_empty());
    }

    /// Build page 1 from `(text, font size, baseline y)` triples in reading
    /// order, one span per line, on a 612 x 792 page (y grows upwards).
    fn page_at(lines: &[(&str, f32, f32)]) -> PageText {
        let mut page = PageText::new(1, 612.0, 792.0, 0);
        let mut parts: Vec<&str> = Vec::new();
        for (i, (text, size, y)) in lines.iter().enumerate() {
            let width = text.chars().count() as f32 * size * 0.5;
            let bbox = BBox {
                x0: 72.0,
                y0: *y,
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
        }
        page.text = parts.join("\n");
        page
    }

    #[test]
    fn find_doi_accepts_prefixes_and_closes_wraps() {
        let cases: [(&str, Option<&str>); 13] = [
            (
                "DOI: 10.1109/X.2020.00123. 978-1-7281-1234-5/20/$31.00",
                Some("10.1109/X.2020.00123"),
            ),
            (
                "DOI: 10.1145/3292500.3330701",
                Some("10.1145/3292500.3330701"),
            ),
            (
                "doi:10.1145/3292500.3330701.",
                Some("10.1145/3292500.3330701"),
            ),
            (
                "https://doi.org/10.1007/s10994-021-05946-3",
                Some("10.1007/s10994-021-05946-3"),
            ),
            ("10. 1145/3292500", Some("10.1145/3292500")),
            (
                "https://doi.org/10.1145/ 3292500.3330701",
                Some("10.1145/3292500.3330701"),
            ),
            (
                "doi: 10.1145/3292500. 3330701",
                Some("10.1145/3292500.3330701"),
            ),
            (
                "DOI 10.1109/5.771073 978-1-7281-1234-5",
                Some("10.1109/5.771073"),
            ),
            ("doi:10.1000/abc. 2020 was a year", Some("10.1000/abc")),
            ("doi:10.1000/abc. The next sentence", Some("10.1000/abc")),
            ("https://doi.org/10.1000/", None),
            ("version 10.2 of the tool", None),
            ("no identifier", None),
        ];
        for (text, expected) in cases {
            assert_eq!(find_doi(text).as_deref(), expected, "{text}");
        }
    }

    #[test]
    fn acm_footer_doi_wins_over_cited_doi_in_body() {
        let page = page_at(&[
            ("Learning Widgets at Scale", 17.0, 700.0),
            ("Jane Doe", 11.0, 670.0),
            ("University of Somewhere", 9.0, 658.0),
            ("Los Angeles, United States", 9.0, 646.0),
            ("jane.doe@somewhere.edu", 9.0, 634.0),
            ("John Smith", 11.0, 610.0),
            ("Example Research Institute", 9.0, 598.0),
            ("ABSTRACT", 9.0, 570.0),
            (
                "We build on prior work (doi:10.5555/3294771.3294864) and",
                9.0,
                558.0,
            ),
            ("show that widgets scale.", 9.0, 546.0),
            ("KDD '19, August 4-8, 2019, Anchorage, AK, USA", 7.0, 70.0),
            ("© 2019 Association for Computing Machinery.", 7.0, 60.0),
            ("ACM ISBN 978-1-4503-6201-6/19/08...$15.00", 7.0, 50.0),
            ("https://doi.org/10.1145/3292500.3330701", 7.0, 40.0),
        ]);
        let meta = extract_metadata(&BTreeMap::new(), &[page]);
        assert_eq!(meta.doi.as_deref(), Some("10.1145/3292500.3330701"));
        assert_eq!(meta.provenance["doi"], "first_page:doi-header-footer");
        assert_eq!(meta.title.as_deref(), Some("Learning Widgets at Scale"));
        assert_eq!(author_names(&meta), vec!["Jane Doe", "John Smith"]);
        assert_eq!(meta.provenance["authors"], "first_page:authors");
    }

    /// arXiv:2410.19245 and similar: an ACM camera-ready hosted on arXiv
    /// prints both the ACM footer DOI and arXiv's own DOI for the preprint
    /// (`10.48550/arXiv.<id>`), on top of the usual arXiv margin stamp. The
    /// publisher's DOI must win; arXiv's own DOI never outranks it.
    #[test]
    fn acm_footer_doi_wins_over_arxiv_own_doi() {
        let page = page_at(&[
            ("arXiv:2410.19245v2 [cs.CL] 3 Nov 2024", 8.0, 775.0),
            ("Sparse Widgets at Scale", 17.0, 700.0),
            ("Jane Doe", 11.0, 670.0),
            ("University of Somewhere", 9.0, 658.0),
            ("ABSTRACT", 9.0, 640.0),
            ("We propose sparse widgets and show gains.", 9.0, 628.0),
            ("KDD '24, August 2024, Anchorage, AK, USA", 7.0, 70.0),
            ("© 2024 Copyright held by owner/author.", 7.0, 60.0),
            ("DOI: 10.48550/arXiv.2410.19245", 7.0, 50.0),
            ("https://doi.org/10.1145/3744916.3773221", 7.0, 40.0),
        ]);
        let meta = extract_metadata(&BTreeMap::new(), &[page]);
        assert_eq!(meta.doi.as_deref(), Some("10.1145/3744916.3773221"));
        assert_eq!(meta.provenance["doi"], "first_page:doi-header-footer");
        // The bare arXiv stamp is untouched by this rule and already supplies
        // arxiv_id, version suffix and all.
        assert_eq!(meta.arxiv_id.as_deref(), Some("2410.19245v2"));
        assert_eq!(meta.provenance["arxiv_id"], "first_page:arxiv");
    }

    /// A bare DOI cited in the abstract carries no evidence that it is the
    /// paper's own; arXiv's own DOI in the footer names this paper and wins.
    #[test]
    fn arxiv_own_doi_wins_over_a_doi_cited_in_the_abstract() {
        let page = page_at(&[
            ("Sparse Widgets at Scale", 17.0, 700.0),
            ("Jane Doe", 11.0, 670.0),
            ("ABSTRACT", 9.0, 640.0),
            (
                "Building on the widget corpus 10.1000/xyz123 we propose sparse widgets and show gains.",
                9.0,
                628.0,
            ),
            ("https://doi.org/10.48550/arXiv.2410.19245", 7.0, 50.0),
        ]);
        let meta = extract_metadata(&BTreeMap::new(), &[page]);
        assert_eq!(meta.doi.as_deref(), Some("10.48550/arXiv.2410.19245"));
        assert_eq!(meta.provenance["doi"], "first_page:doi");
    }

    /// When arXiv's own DOI is the only one printed on page 1 (no publisher
    /// copy), it is kept as `doi`, and its id part fills `arxiv_id` since
    /// nothing else on the page does: here the DOI wraps across the footer's
    /// two lines, so the plain arXiv-stamp scan (`first_page:arxiv`) never
    /// sees a complete id and the fallback from `doi` is what fills it.
    #[test]
    fn arxiv_own_doi_is_kept_and_fills_arxiv_id_when_it_is_the_only_doi() {
        let page = page_at(&[
            ("Sparse Widgets at Scale", 17.0, 700.0),
            ("Jane Doe", 11.0, 670.0),
            ("ABSTRACT", 9.0, 640.0),
            ("We propose sparse widgets and show gains.", 9.0, 628.0),
            ("DOI: 10.48550/arXiv.", 7.0, 50.0),
            ("2410.19245", 7.0, 40.0),
        ]);
        let meta = extract_metadata(&BTreeMap::new(), &[page]);
        assert_eq!(meta.doi.as_deref(), Some("10.48550/arXiv.2410.19245"));
        assert_eq!(meta.provenance["doi"], "first_page:doi");
        assert_eq!(meta.arxiv_id.as_deref(), Some("2410.19245"));
        assert_eq!(meta.provenance["arxiv_id"], "doi");
    }

    #[test]
    fn arxiv_doi_prefix_matched_case_insensitively() {
        assert_eq!(
            arxiv_id_from_doi("10.48550/arXiv.2410.19245"),
            Some("2410.19245")
        );
        assert_eq!(
            arxiv_id_from_doi("10.48550/ARXIV.2410.19245"),
            Some("2410.19245")
        );
        assert_eq!(
            arxiv_id_from_doi("10.48550/arxiv.2410.19245"),
            Some("2410.19245")
        );
        assert!(is_arxiv_doi("10.48550/arXiv.2410.19245"));
        assert_eq!(arxiv_id_from_doi("10.1145/3744916.3773221"), None);
        assert!(!is_arxiv_doi("10.1145/3744916.3773221"));
        // Shorter than the prefix: no panic, no match.
        assert_eq!(arxiv_id_from_doi("10.48550/ar"), None);
    }

    #[test]
    fn ieee_header_doi_split_across_lines_and_member_suffixes() {
        let page = page_at(&[
            ("IEEE ACCESS, VOL. 8, 2020", 8.0, 760.0),
            ("Digital Object Identifier 10.1109/ACCESS.2020.", 8.0, 740.0),
            ("2991234", 8.0, 730.0),
            ("Robust Widget Estimation", 22.0, 660.0),
            (
                "John Smith, Member, IEEE, and Jane Doe, Senior Member, IEEE",
                11.0,
                630.0,
            ),
            ("Abstract—We estimate widgets robustly.", 9.0, 600.0),
            ("Index Terms—widgets, estimation", 9.0, 580.0),
        ]);
        let meta = extract_metadata(&BTreeMap::new(), &[page]);
        assert_eq!(meta.doi.as_deref(), Some("10.1109/ACCESS.2020.2991234"));
        assert_eq!(meta.provenance["doi"], "first_page:doi-header-footer");
        assert_eq!(meta.year, Some(2020));
        assert_eq!(meta.provenance["year"], "doi");
        assert_eq!(meta.title.as_deref(), Some("Robust Widget Estimation"));
        assert_eq!(author_names(&meta), vec!["John Smith", "Jane Doe"]);
    }

    #[test]
    fn springer_doi_line_is_preferred_over_bare_doi_and_rejoined() {
        let page = page_at(&[
            ("Machine Learning (2021) 110:1-30", 8.0, 690.0),
            ("Kernel Widgets", 18.0, 650.0),
            ("Ada Lovelace1 · Charles Babbage2", 11.0, 620.0),
            ("Received: 3 March 2020 / Accepted: 1 June 2021", 8.0, 600.0),
            (
                "Abstract We compare with 10.5555/12345.678 and others.",
                9.0,
                580.0,
            ),
            ("https://doi.org/10.1007/s10994-021-", 8.0, 300.0),
            ("05946-3", 8.0, 290.0),
        ]);
        let meta = extract_metadata(&BTreeMap::new(), &[page]);
        assert_eq!(meta.doi.as_deref(), Some("10.1007/s10994-021-05946-3"));
        assert_eq!(meta.provenance["doi"], "first_page:doi");
        assert_eq!(meta.title.as_deref(), Some("Kernel Widgets"));
        assert_eq!(author_names(&meta), vec!["Ada Lovelace", "Charles Babbage"]);
        assert_eq!(meta.provenance["authors"], "first_page:authors");
    }

    #[test]
    fn line_doi_does_not_join_words_or_years_from_the_next_line() {
        let page = page_at(&[
            ("https://doi.org/10.1000/xyz123.", 8.0, 40.0),
            ("Permission to make digital copies", 8.0, 30.0),
            ("doi:10.2000/abc.", 8.0, 20.0),
            ("2020", 8.0, 10.0),
        ]);
        assert_eq!(line_doi(&page, 0).as_deref(), Some("10.1000/xyz123"));
        assert_eq!(line_doi(&page, 2).as_deref(), Some("10.2000/abc"));
        assert_eq!(line_doi(&page, 1), None);
    }

    #[test]
    fn header_footer_band_needs_a_bbox_and_height() {
        let page = page_at(&[
            ("top", 8.0, 740.0),
            ("middle", 8.0, 400.0),
            ("bottom", 8.0, 50.0),
        ]);
        assert!(in_header_footer(&page, &page.lines[0]));
        assert!(!in_header_footer(&page, &page.lines[1]));
        assert!(in_header_footer(&page, &page.lines[2]));
        let mut no_box = page.lines[0].clone();
        no_box.bbox = None;
        assert!(!in_header_footer(&page, &no_box));
        let flat = PageText::new(1, 612.0, 0.0, 0);
        assert!(!in_header_footer(&flat, &page.lines[0]));
    }

    #[test]
    fn author_block_with_interleaved_affiliations_keeps_only_people() {
        let page = page_at(&[
            ("Deep Graph Kernel Point Processes", 17.0, 720.0),
            ("Zheng Dong1,2, Matthew Repasky1", 11.0, 690.0),
            (
                "1H. Milton Stewart School of Industrial and Systems Engineering,",
                9.0,
                678.0,
            ),
            ("Georgia Institute of Technology", 9.0, 666.0),
            ("and", 11.0, 654.0),
            ("Xiuyuan Cheng3", 11.0, 642.0),
            ("3Department of Mathematics, Duke University", 9.0, 630.0),
            ("Los Angeles, United States", 9.0, 618.0),
            ("Hong Kong, China", 9.0, 606.0),
            ("ARTICLE INFO", 9.0, 594.0),
            ("Ruiping Yin∗, Zhen Yang⋆", 11.0, 582.0),
            ("Jane Doe1,a,b", 11.0, 570.0),
            ("Equal Contribution", 9.0, 558.0),
            ("September 1, 2026", 9.0, 546.0),
            ("{zdong, mrepasky}@gatech.edu", 9.0, 534.0),
            ("Abstract", 11.0, 510.0),
            ("Point process models are widely used.", 10.0, 498.0),
        ]);
        let meta = extract_metadata(&BTreeMap::new(), &[page]);
        assert_eq!(
            author_names(&meta),
            vec![
                "Zheng Dong",
                "Matthew Repasky",
                "Xiuyuan Cheng",
                "Ruiping Yin",
                "Zhen Yang",
                "Jane Doe"
            ]
        );
        assert_eq!(meta.provenance["authors"], "first_page:authors");
    }

    #[test]
    fn affiliation_and_label_rules() {
        assert!(is_country("China"));
        assert!(is_country("The Netherlands."));
        assert!(is_country("  United States of America "));
        assert!(!is_country("Michael I. Jordan"));
        assert!(!is_country("Georgia Tech"));
        assert!(last_segment_is_country("Delft, The Netherlands"));
        assert!(last_segment_is_country("Pasadena, CA 91125, USA"));
        assert!(!last_segment_is_country("Jane Doe, John Smith"));
        assert!(!last_segment_is_country("Jane Doe and Michael Jordan"));
        assert!(has_front_matter_token("ARTICLE INFO"));
        assert!(has_front_matter_token("Corresponding Author"));
        assert!(!has_front_matter_token("DAVID E. J. VAN WIJK"));
        assert!(is_page1_person_name("DAVID E. J. VAN WIJK"));
        assert!(!is_page1_person_name("ARTICLE INFO"));
        assert!(!is_page1_person_name("United Kingdom"));
        assert!(!is_page1_person_name("Plato"));
        assert!(is_marker_letters("a"));
        assert!(is_marker_letters("ab"));
        assert!(!is_marker_letters("Al"));
        assert!(!is_marker_letters("abc"));
        assert_eq!(
            ieee_membership_re().replace_all(
                "John Smith, Student Member, IEEE, and Ann Lee, Fellow, IEEE",
                ""
            ),
            "John Smith, and Ann Lee"
        );
    }
}
