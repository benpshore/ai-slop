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

use crate::schema::{BBox, CitationMarker, Line, PageText, ReferenceEntry};
use crate::text_cleanup::{HyphenPolicy, hyphen_policy};

/// Where a reference list starts. A document may hold several lists
/// (`References` and `References for the Appendices`, say); see
/// [`find_reference_sections`].
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
    /// `[12]` printed on a line of its own, apart from the entry it labels
    /// (the label column of an IEEE list that the layout pass detached).
    /// The entries segment like author-year ones and take the printed
    /// numbers as labels.
    Detached,
    /// `12 Q. Zhang, ...`: a bare number and a space (RSC `Notes and
    /// references`), numbered consecutively from 1.
    Bare,
}

/// One line of the reference section with the layout evidence needed for
/// segmentation.
#[derive(Clone, Debug)]
struct SectionLine {
    page: u32,
    /// Index of the (first) fragment in the page's `lines`.
    line: usize,
    column: u32,
    x0: Option<f32>,
    y0: Option<f32>,
    size: Option<f32>,
    text: String,
}

impl SectionLine {
    /// Is `other` a fragment of the same printed row (same page and column,
    /// baselines within 0.4 × the font size)? `size` is the row's font size
    /// so far: the largest of its fragments, as the merged row used to carry.
    fn same_row_as(&self, other: &Self, size: Option<f32>) -> bool {
        if self.page != other.page || self.column != other.column {
            return false;
        }
        let (Some(a), Some(b)) = (self.y0, other.y0) else {
            return false;
        };
        let size = size.or(other.size).unwrap_or(10.0);
        (a - b).abs() <= 0.4 * size
    }
}

/// The larger of two optional font sizes.
fn max_size(a: Option<f32>, b: Option<f32>) -> Option<f32> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

// A genuine printed row has few fragments. These bounds prevent hostile PDF
// geometry from turning an arbitrary number of lines into one allocation.
const MAX_ROW_FRAGMENTS: usize = 1_024;
const MAX_ROW_TEXT_BYTES: usize = 1024 * 1024;

/// Sort the collected fragments once and concatenate them in one allocation.
/// This avoids repeatedly copying the accumulated row when lower-x fragments
/// arrive late in reading order.
fn finish_row(mut fragments: Vec<SectionLine>) -> SectionLine {
    debug_assert!(!fragments.is_empty());
    if fragments.len() == 1 {
        // A lone fragment (possibly one larger than the row budget) is moved
        // out as it is, never copied.
        return fragments.pop().expect("one fragment");
    }
    // The row keeps the position of the fragment that arrived first, the
    // lowest x, the largest size and the lowest line index.
    let (page, column, y0) = (fragments[0].page, fragments[0].column, fragments[0].y0);
    let line = fragments
        .iter()
        .map(|fragment| fragment.line)
        .min()
        .unwrap_or(0);
    let x0 = fragments
        .iter()
        .filter_map(|fragment| fragment.x0)
        .reduce(f32::min);
    let size = fragments
        .iter()
        .filter_map(|fragment| fragment.size)
        .reduce(f32::max);

    fragments.sort_by(|a, b| match (a.x0, b.x0) {
        (Some(a), Some(b)) => a.total_cmp(&b),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });
    let capacity = fragments
        .iter()
        .map(|fragment| fragment.text.len())
        .sum::<usize>()
        + fragments.len().saturating_sub(1);
    let mut text = String::with_capacity(capacity);
    for fragment in fragments {
        if !text.is_empty() && !fragment.text.is_empty() {
            text.push(' ');
        }
        text.push_str(&fragment.text);
    }
    SectionLine {
        page,
        line,
        column,
        x0,
        y0,
        size,
        text,
    }
}

/// `text` with every run of up to three digits replaced by one `#`, so that
/// `Page 30 of 35`, `Page 31 of 35` and `Page 9 of 35` compare equal. Longer
/// runs (years, arXiv ids, DOIs) stay as printed: the `arXiv:` line that
/// ends a full column is not a repeated footer just because other pages end
/// with `arXiv:` lines too.
fn digit_key(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut digits = String::new();
    for c in text.chars() {
        if c.is_ascii_digit() {
            digits.push(c);
            continue;
        }
        flush_digit_run(&mut out, &mut digits);
        out.push(c);
    }
    flush_digit_run(&mut out, &mut digits);
    out
}

/// Append the pending digit run of [`digit_key`] to `out` and clear it.
fn flush_digit_run(out: &mut String, digits: &mut String) {
    if digits.is_empty() {
        return;
    }
    if digits.len() <= 3 {
        out.push('#');
    } else {
        out.push_str(digits);
    }
    digits.clear();
}

/// A floating accent: `´`, `¨`, `¸`, `¯`, a spacing modifier letter
/// (`ˆ`, `˜`, `˘`, `˙`, `˚`, `˝`, `ˇ`) or a combining mark. OT1 fonts set the
/// accent of `Verdú` or `Güngör` as a glyph of its own, which the layout pass
/// leaves as a separate line on the row's baseline.
fn is_accent_mark(c: char) -> bool {
    matches!(
        c,
        '\u{A8}'
            | '\u{AF}'
            | '\u{B4}'
            | '\u{B8}'
            | '`'
            | '^'
            | '~'
            | '\u{2B0}'..='\u{2FF}'
            | '\u{300}'..='\u{36F}'
    )
}

/// Does `text` consist of floating accents only? Such a line carries no
/// text and would otherwise be joined in front of the row it sits on,
/// hiding the `[n]` label there.
fn is_accent_only(text: &str) -> bool {
    let mut marks = false;
    for c in text.chars() {
        if is_accent_mark(c) {
            marks = true;
        } else if !c.is_whitespace() {
            return false;
        }
    }
    marks
}

/// Largest gap in points between an entry start and its continuation lines
/// that still counts as "the same indent".
const INDENT_TOLERANCE: f32 = 1.0;
/// Widest hanging indent (points) between an entry start and its
/// continuation lines.
const MAX_HANGING_INDENT: f32 = 40.0;
/// Smallest share of the section's lines that must sit at a hanging indent
/// before the layout is trusted over the text patterns.
const MIN_INDENTED_SHARE: f32 = 0.25;
/// Longest run of numbers accepted from a `[a–b]` range marker.
const MAX_RANGE_SPAN: u32 = 50;
/// Longest token accepted as the continuation of a DOI or URL broken by a
/// line wrap.
const MAX_WRAP_TOKEN: usize = 64;
/// Share of the page height, from the top, in which a separated top row is
/// a running header (LNCS sets its running heads about 11.5% down).
const HEADER_BAND: f32 = 0.15;
/// Share of the page height, at the top and at the bottom, in which any
/// line may be a running header, footer or folio.
const MARGIN_BAND: f32 = 0.08;
/// Longest line (chars) that counts as a running header or footer.
const MAX_FURNITURE_CHARS: usize = 80;
/// Number of content lines after a `References` heading within which a
/// reference entry must start for the heading to open a list.
const HEADING_LOOKAHEAD: usize = 3;

/// First halves that keep their hyphen when [`hyphen_policy`] would join an
/// unattested word (`spatio- temporal` → `spatio-temporal`). Bound prefixes
/// that are normally written solid (`pre`, `non`, `multi`) are left to the
/// policy.
const COMPOUND_PREFIXES: &[&str] = &[
    "bi", "cross", "e", "fine", "high", "long", "low", "of", "one", "quasi", "real", "self",
    "short", "spatio", "the", "three", "two", "well", "zero",
];
/// Second halves that keep their hyphen when [`hyphen_policy`] would join an
/// unattested word (`privacy- preserving`).
const COMPOUND_HEADS: &[&str] = &[
    "agnostic",
    "augmented",
    "aware",
    "based",
    "box",
    "dimensional",
    "driven",
    "efficient",
    "end",
    "form",
    "free",
    "generated",
    "grained",
    "intensive",
    "language",
    "level",
    "like",
    "linear",
    "machine",
    "order",
    "oriented",
    "preserving",
    "scale",
    "shot",
    "specific",
    "task",
    "time",
    "to",
    "wise",
    "world",
];

/// A reference-list heading: `References`, `7. References`, `A Bibliography`,
/// `Supplementary References`, `References for the Appendices`,
/// `References and Notes`, and the RSC `Notes and references` (either
/// order in any case).
fn heading_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^\s*(?:(?:\d+|[IVX]+)\.?\s*|[A-Z]\.?\s+)?(?:(?:Supplementary|Supplemental|Additional|Appendix|Further|Extended|Online|SUPPLEMENTARY|SUPPLEMENTAL|ADDITIONAL|APPENDIX)\s+)?(?:(?i:notes\s+and\s+references|references\s+and\s+notes)|References|REFERENCES|Reference List|Bibliography|BIBLIOGRAPHY|Works Cited|WORKS CITED|Literature Cited|LITERATURE CITED)(?:\s+(?:for|of|to|and|in|FOR|OF|TO|AND|IN)\s+[\p{L}\s’'\-]{1,40})?\s*:?\s*$",
        )
        .expect("valid regex")
    })
}

fn end_heading_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)^\s*[-–—\s]*(?:(?:\d+|[A-Z]|[IVX]+)[.:]?\s+)?(?:(?:technical|online)\s+)?(?:appendix|appendices|supplementary|supplemental|supporting information|acknowledg\w*|author biograph\w*|biograph\w*)\b",
        )
        .expect("valid regex")
    })
}

/// An appendix or supplement title: `Appendices for …`, `Supplementary
/// Material for …`, `Appendix to …`.
fn appendix_title_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^\s*(?:Appendix|Appendices|APPENDIX|APPENDICES|(?:Supplementary|Supplemental|SUPPLEMENTARY)\s+(?:Materials?|MATERIALS?))\s+(?:for|to|of|FOR|TO|OF)\s",
        )
        .expect("valid regex")
    })
}

/// `Table 5: ...` / `Figure 2.` captions that follow a reference list.
fn caption_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^\s*(?:Table|Figure|Fig\.|TABLE|FIGURE)\s+\d+").expect("valid regex")
    })
}

/// A table row: three or more purely numeric cells.
fn numeric_row_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^\s*(?:[-+±]?\d+(?:[.,]\d+)?%?\s+){2,}[-+±]?\d+(?:[.,]\d+)?%?\s*$")
            .expect("valid regex")
    })
}

/// First line of an author biography: a name of at least two capitalised
/// words followed by a biography verb phrase (`received the`, `is currently`,
/// `is a Professor`, `was born`).
fn biography_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^\s*\p{Lu}[\p{L}.'’\-]*(?:\s+\p{Lu}[\p{L}.'’\-]*){1,6}\s+(?:\([^)]{1,40}\)\s+)?(?:received (?:the|his|her|a|an)\b|is currently\b|was born\b|is with\b|obtained (?:the|his|her|a)\b|holds (?:a|an|the)\b|earned (?:the|his|her|a|an)\b|is (?:a|an) (?:Postdoctoral|Professor|Ph\.?D|Research|Senior|Principal|Lecturer|Assistant|Associate|Full|Distinguished|Staff|student|graduate|member|Member|faculty|postdoc|Postdoc)\b)",
        )
        .expect("valid regex")
    })
}

/// A line that may open an author-year entry: capital letter, opening quote
/// or bracket, a lowercase surname particle, or an elided particle before a
/// capitalised surname (`d’Haultfoeuille`, `l’Hôpital`).
fn entry_start_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"^(?:\p{Lu}|[“"„‘\[(]|(?:van|von|de|der|den|del|di|da|la|le|du)\s|[dDlL][’']\p{Lu})"#,
        )
        .expect("valid regex")
    })
}

fn bracket_label_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*\[(\d+)\]\s*").expect("valid regex"))
}

/// A bare-number label (RSC): `12 Q. Zhang, ...`. Group 1 is the number;
/// an uppercase letter must follow.
fn bare_number_label_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*(\d{1,4})\s+\p{Lu}").expect("valid regex"))
}

/// A bare number followed by any text (`8 arXiV, https://...`). Group 1 is
/// the number.
fn bare_number_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*(\d{1,4})\s+\S").expect("valid regex"))
}

/// Printed number of a line opening with a bare number ([`bare_number_re`]).
fn bare_number(text: &str) -> Option<u32> {
    let caps = bare_number_re().captures(text)?;
    caps.get(1)?.as_str().parse::<u32>().ok()
}

/// The first entry of an RSC list: `1 Q. Zhang, ...`.
fn rsc_first_entry_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*1\s+\p{Lu}\.").expect("valid regex"))
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
/// `van der Maaten, L.`, `d’Haultfoeuille X`.
fn author_start_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^\s*(?:(?:van|von|de|der|den|del|di|da|la|le|du)\s+)*(?:[dDlL][’']\s?)?\p{Lu}[\p{L}'’\-]*(?:\s+\p{Lu}[\p{L}'’\-]*)?(?:,\s*\p{Lu}(?:\.|\p{L}+)|\s+\p{Lu}{1,3}\b[,.]?)",
        )
        .expect("valid regex")
    })
}

/// A lowercase handle opening an entry in ACM style, followed by the year
/// sentence and a title: `nostalgebraist. 2020. Interpreting GPT`. Group 1
/// is the handle.
fn handle_start_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^\s*(\p{Ll}[\p{L}\d_\-]{2,})\.\s+\(?(?:19|20)\d{2}[a-z]?\)?[.:]\s+\S")
            .expect("valid regex")
    })
}

/// Springer LNCS / `spmpsci` author list closed by a colon:
/// `Surname, I., Other, J.K.: Title` (`et al.` may close it). Surnames may
/// carry a particle or a second word; initials may be hyphenated with a
/// lowercase second part (`C.-i.`).
fn lncs_authors_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^\s*(?:(?:(?:van|von|de|der|den|del|di|da|la|le|du)\s+)*\p{Lu}[\p{L}'’\-]+(?:\s\p{Lu}[\p{L}'’\-]+)*,\s?\p{Lu}\.(?:\s?-?\p{L}\.)*,\s)*(?:(?:(?:van|von|de|der|den|del|di|da|la|le|du)\s+)*\p{Lu}[\p{L}'’\-]+(?:\s\p{Lu}[\p{L}'’\-]+)*,\s?\p{Lu}\.(?:\s?-?\p{L}\.)*|et al\.):\s",
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

/// Start of a DOI, tolerating the single space a line wrap leaves after
/// `10.` or before `/`.
fn doi_start_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\b10\.\s?\d{4,9}\s?/").expect("valid regex"))
}

/// A bare year token (`2020`, `2020a`).
fn year_token_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(?:19|20)\d{2}[a-z]?$").expect("valid regex"))
}

/// A year that opens the text after the author list (`2019. Title` in ACM
/// style, `(2019). Title` without an author-only prefix).
fn leading_year_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*\(?(?:19|20)\d{2}[a-z]?\)?[.,:]?\s+").expect("valid regex"))
}

/// `A.`, `A.B.`, `J.-M.`, `C.-i.` (a hyphenated initial whose second part
/// is lowercase, as in `C.-i. Wang`) or a broken `P.-` before a line-wrapped
/// `Y.`.
fn initial_token_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^\p{Lu}\p{M}*\.(?:-?\p{Lu}\p{M}*\.|-\p{Ll}\.)*-?$").expect("valid regex")
    })
}

/// A capitalised word: `Smith`, `O'Brien`, `Ahmadi-Asl`, `IEEE`, `Martı́` (a
/// letter with a combining accent as extracted).
fn cap_word_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\p{Lu}[\p{L}\p{M}'’\-]*$").expect("valid regex"))
}

/// Up to three capitals: Vancouver initials (`AB`) or a short acronym.
fn caps_block_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\p{Lu}{1,3}$").expect("valid regex"))
}

/// ` and ` / ` & ` between two names inside one comma-delimited part.
fn and_split_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\s+(?:and|&)\s+").expect("valid regex"))
}

/// Trailing `et al.` / `and others` of a name part.
fn et_al_tail_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\s+(?:et\s+al\.?|and\s+others)$").expect("valid regex"))
}

/// `et al.` opening the part that follows the last comma of the author list.
fn et_al_lead_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(?i:et\s+al\.?)\s+").expect("valid regex"))
}

/// A year at the start of a comma-delimited part.
fn year_lead_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\(?(?:19|20)\d{2}").expect("valid regex"))
}

/// Words that open the venue part after a comma-delimited title.
fn venue_lead_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^(?i:in\b|pp?\.|pages?\b|vol\b|volume\b|arxiv|http|doi\b|eds?\b|edited\b|editors?\b|tech\b|technical\b|phd\b|master|chapter\b|ch\.|no\.|preprint|proc\b|proceedings\b|submitted\b|to appear|available|url\b|accessed|retrieved|ser\.|series\b|version\b|v\d)",
        )
        .expect("valid regex")
    })
}

/// A comma inside an unquoted title that is followed by venue words
/// (`Title, volume 6.` / `Title, pp. 3–9` / `Title, in Proceedings`).
fn comma_venue_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r",\s+(?:(?i:vol(?:ume)?\b|pp?\.|pages?\b|in:|eds?\.|edited\b|editors?\b|no\.|chapter\b|ch\.|tech\.|technical\b|version\b|arxiv)|in\s+\p{Lu})",
        )
        .expect("valid regex")
    })
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

/// Volume (and issue) before a blanked year: `35 (    ) 61–70`,
/// `20 (2) (    ) 130–141`, `22 (    ), pp.`.
fn vol_before_year_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"\b(\d+)\s*(?:\((\d+(?:[–\-]\d+)?)\)\s*)?\(\s+[a-z]?\)(?:\s*[,:]?\s*(\d+)\s*[–\-—]\s*(\d+))?",
        )
        .expect("valid regex")
    })
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

/// A block of initials (`A.`, `A. B.`, `AB`, `J.-M.`, `C.-i.`).
fn initials_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^\p{Lu}\.?(?:[\s\-]*\p{Lu}\.?|-\p{Ll}\.)*$").expect("valid regex")
    })
}

/// `Smith, A.` / `Smith, John,` / `Lee, J. and`: a surname-first author list.
/// `Hideo Bannai, Mitsuru Funakoshi` (two full names) is not one.
fn surname_first_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^\p{Lu}[\p{L}'’\-]+(?:\s+\p{Lu}[\p{L}'’\-]+)?,\s*(?:\p{Lu}\.|\p{Lu}[\p{L}'’\-]+\s*(?:,|\band\b|&|\(|$))",
        )
        .expect("valid regex")
    })
}

/// `Smith AB, Jones C.` (Vancouver initials without periods). `Brent N.
/// Clark` is not Vancouver: a single initial with a period is a middle name.
fn vancouver_start_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^\p{Lu}[\p{L}'’\-]+\s+(?:\p{Lu}{1,3},|\p{Lu}{2,3}\.\s)").expect("valid regex")
    })
}

/// Locator words that may open the note after the numbers of a bracket
/// group (`[22, Theorem 4]`, `[5, Sec. 3.1]`, `[18, Kapitel VIII, § 6]`,
/// `[31, Théorème 1]`), in English, German, French, Italian and Spanish,
/// matched without regard to case.
const MARKER_LOCATORS: &str = r"(?:(?i:Theorem|Thm\.|Th\.|Lemma|Lem\.|Corollary|Cor\.|Proposition|Prop\.|Definition|Def\.|Remark|Rem\.|Section|Sect?\.|Secs\.|Chapter|Chap\.|Chs?\.|Equation|Eqs?\.|Example|Ex\.|Appendix|App\.|Table|Tab\.|Figure|Figs?\.|pp?\.|pages?|Part|Kapitel|Kap\.|Satz|Abschnitt|Abschn\.|Seite|S\.|Anhang|Bemerkung|Beispiel|Folgerung|Korollar|Tabelle|Abb\.|Théorème|Théor\.|Lemme|Corollaire|Chapitre|Remarque|Exemple|Annexe|Partie|Teorema|Lema|Corolario|Corollario|Capitolo|Capítulo|Cap\.|Sez\.)|§)";

/// `[1]`, `[2, 3]`, `[4–6]`, `[22, Theorem 4]`: group 1 holds the numbers,
/// the optional note after the last number (`, Theorem 4`, `, p. 12`,
/// `, Sec. 3.1`, `, Kapitel VIII, § 6, II`; see [`MARKER_LOCATORS`]) is part
/// of the marker text only.
fn numeric_marker_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        let locators = MARKER_LOCATORS;
        Regex::new(&format!(
            r"\[(\s*\d+\s*(?:[–\-—]\s*\d+\s*)?(?:[,;]\s*\d+\s*(?:[–\-—]\s*\d+\s*)?)*)(?:,\s*{locators}[^\]\[]{{0,40}})?\]"
        ))
        .expect("valid regex")
    })
}

fn numeric_item_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(\d+)\s*(?:[–\-—]\s*(\d+))?").expect("valid regex"))
}

/// Lower-case surname particles that may open or sit inside a marker name
/// (`de Moura`, `van den Oord`).
const MARKER_PARTICLES: &str = "(?:de|van|von|der|den|del|della|di|da|du|le|la|dos|das|ten|ter)";

/// Regex source of a first-author name in a citation marker: optional
/// initials (`J. Wu`), then one or more capitalised tokens, each possibly
/// after particles (`Tchetgen Tchetgen`, `de Moura`, `Van Roy`, `Alibaba
/// Cloud Qwen Team`, `Nomic AI`). With `capture`, the name without the
/// initials is capture group 1 of the source.
fn marker_name_source(capture: bool) -> String {
    let token = r"\p{Lu}[\p{L}'’\-]+";
    let particles = MARKER_PARTICLES;
    let surname = format!(r"(?:{particles}\s+)*{token}(?:\s+(?:{particles}\s+)*{token})*");
    let initials = r"(?:\p{Lu}\.\s?)*";
    if capture {
        format!("{initials}({surname})")
    } else {
        format!("{initials}{surname}")
    }
}

/// Lower-case words that may close the last co-author of a marker when it
/// is a group (`Hanu and Unitary team, 2020`).
const MARKER_GROUP_WORDS: &str =
    "(?:team|group|lab|consortium|collaboration|contributors|developers|community|project)";

/// Regex source (no capture groups) of what may follow the first author
/// before the year: co-authors (`and Kim`, `& Kim`, `, Paulson, and
/// Wenzel`, `and van Roy`, `and Unitary team`) or `et al.` (with or
/// without the period).
fn marker_coauthors_source() -> String {
    let author = marker_name_source(false);
    let group = MARKER_GROUP_WORDS;
    format!(
        r"(?:(?:\s*,\s*{author})*\s*,?\s+(?:and|&)\s+{author}(?:\s+{group}(?-u:\b))?|\s+et\s+al(?-u:\b)\.?)?"
    )
}

/// Regex source (no capture groups) of the years of one author: a year
/// with an optional letter, letter lists (`2023a,b`, `2025c,b,a`) and
/// further years of the same author (`2022, 2023a`).
const MARKER_YEARS: &str = r"(?:19|20)\d{2}[a-h]?(?-u:\b)(?:\s*,\s*[a-h](?-u:\b))*(?:\s*,\s*(?:19|20)\d{2}[a-h]?(?-u:\b)(?:\s*,\s*[a-h](?-u:\b))*)*";

/// Narrative marker `Name (2020)`, `Smith et al. (2020a)`, `Duan et al
/// (2020)`, `Politis and Romano (1994, Theorem 3.1)`, `Doan (2021, 2022)`.
/// Group 1 is the first author, group 2 the years.
fn narrative_marker_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        let name = marker_name_source(true);
        let coauthors = marker_coauthors_source();
        let years = MARKER_YEARS;
        Regex::new(&format!(
            r"{name}{coauthors}\s+\(({years})(?:\s*[,;][^()]{{0,40}})?\)"
        ))
        .expect("valid regex")
    })
}

/// One author-year clause anywhere inside a parenthetical: `Rubin 1976`,
/// `Robins et al. 1994`, `Nipkow, Paulson, and Wenzel 2002`, `Banerjee et
/// al. 2023a,b`, `Qwen Team 2024, 2025`. Group 1 is the first author,
/// group 2 the years ([`MARKER_YEARS`]).
fn clause_scan_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        let name = marker_name_source(true);
        let coauthors = marker_coauthors_source();
        let years = MARKER_YEARS;
        Regex::new(&format!(r"{name}{coauthors}\s*,?\s*({years})")).expect("valid regex")
    })
}

/// An `et al.` citation without parentheses (a table cell `Duan et al
/// 2020`, a narrative `As Alexander et al. 2015 put it`, a tail whose `(`
/// was separated by the layout `Barrault et al., 2023)`). Group 1 is the
/// first author, group 2 the year, group 3 the letter.
fn bare_et_al_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        let name = marker_name_source(true);
        Regex::new(&format!(
            r"{name}\s+et\s+al(?-u:\b)\.?\s*,?\s*((?:19|20)\d{{2}})([a-h]?)(?-u:\b)"
        ))
        .expect("valid regex")
    })
}

/// One item of a [`MARKER_YEARS`] run: a year and its letter (groups 1
/// and 2) or a further letter of the previous year (group 3).
fn year_item_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"((?:19|20)\d{2})([a-h]?)|(?-u:\b)([a-h])(?-u:\b)").expect("valid regex")
    })
}

/// A run of superscript digits attached to a word (`literature.⁵`,
/// `Initiative⁵⁻⁷`, `data⁸,⁹`): a superscript citation.
fn superscript_marker_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[⁰¹²³⁴⁵⁶⁷⁸⁹]+(?:[,⁻–][⁰¹²³⁴⁵⁶⁷⁸⁹]+)*").expect("valid regex"))
}

/// A line holding nothing but a superscript citation that text cleanup
/// left detached from its word: a Unicode superscript run (`⁵`, `⁵⁻⁷`,
/// `¹⁰,¹¹`), which text cleanup produces only for raised fragments. Plain
/// digits on a line of their own (`5`, `10`) are figure axis ticks or
/// labels as often as citations, so they never count.
fn detached_fragment_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[⁰¹²³⁴⁵⁶⁷⁸⁹]+(?:[,⁻–][⁰¹²³⁴⁵⁶⁷⁸⁹]+)*$").expect("valid regex"))
}

/// A `;`-separated clause of a parenthetical that is only years
/// ([`MARKER_YEARS`]): `2024` in `(Abe et al., 2023; 2024)`.
fn bare_years_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        let years = MARKER_YEARS;
        Regex::new(&format!(r"^\s*(?:{years})\s*$")).expect("valid regex")
    })
}

/// The head of a page that closes a narrative citation begun on the page
/// before: `(2017), …` after `van Bevern et al.`.
fn year_paren_head_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\((?:19|20)\d{2}[a-h]?(?-u:\b)").expect("valid regex"))
}

fn parenthetical_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\(([^()]*?(?:19|20)\d{2}[a-z]?[^()]*)\)").expect("valid regex"))
}

/// The strict form of one `;`-separated clause of a parenthetical: a
/// single-token name (or `Name and Name`, `Name et al.`) and a year at the
/// start of the clause. A parenthetical none of whose clauses resolves is
/// still a marker when a clause has this form.
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

/// A bare `[n]` label with nothing after it (the label column of an IEEE
/// list that the layout pass emitted apart from its entries). Group 1 is
/// the number.
fn bare_label_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*\[(\d+)\]\s*$").expect("valid regex"))
}

/// Printed number of a bare `[n]` line ([`bare_label_re`]).
fn bare_label_number(text: &str) -> Option<u32> {
    let caps = bare_label_re().captures(text)?;
    caps.get(1)?.as_str().parse::<u32>().ok()
}

/// Shortest run of ascending bare `[n]` lines that numbers a list.
const DETACHED_RUN: usize = 3;

/// Do at least [`DETACHED_RUN`] bare `[n]` lines with ascending numbers
/// follow one another in `lines` (other lines may sit between them)? Such
/// a run is the label column of a numbered list that the layout pass
/// emitted apart from its entries.
fn detached_run(lines: &[SectionLine]) -> bool {
    let mut run = 0usize;
    let mut previous: Option<u32> = None;
    for line in lines {
        let Some(number) = bare_label_number(&line.text) else {
            continue;
        };
        run = if previous.is_some_and(|p| number > p) {
            run + 1
        } else {
            1
        };
        if run >= DETACHED_RUN {
            return true;
        }
        previous = Some(number);
    }
    false
}

/// Could `text` be the first line of a reference entry: a numbered label
/// (`1 Q. Zhang` for RSC), a surname-first or initials-first author list,
/// or reference evidence (a year, DOI, arXiv id or URL)?
fn opens_entry(text: &str) -> bool {
    bracket_label_re().is_match(text)
        || dot_label_re().is_match(text)
        || paren_label_re().is_match(text)
        || rsc_first_entry_re().is_match(text)
        || author_start_re().is_match(text)
        || initials_start_re().is_match(text)
        || has_reference_evidence(text)
}

/// Does a reference entry start within the [`HEADING_LOOKAHEAD`] content
/// lines after the heading at (`pos`, `first_line`)? Empty, page-number and
/// accent-only lines are skipped; the next candidate heading (`stop`, as
/// page position and line index) ends the search. A `References` line in a
/// table of contents has no entry after it.
fn list_follows(
    pages: &[PageText],
    pos: usize,
    first_line: usize,
    stop: Option<(usize, usize)>,
) -> bool {
    let mut seen = 0usize;
    for (p, page) in pages.iter().enumerate().skip(pos) {
        let skip = if p == pos { first_line + 1 } else { 0 };
        for (i, line) in page.lines.iter().enumerate().skip(skip) {
            if stop.is_some_and(|s| (p, i) >= s) {
                return false;
            }
            let text = line.text.trim();
            if text.is_empty() || is_accent_only(text) || page_number_re().is_match(text) {
                continue;
            }
            if opens_entry(text) {
                return true;
            }
            seen += 1;
            if seen >= HEADING_LOOKAHEAD {
                return false;
            }
        }
    }
    false
}

/// Number of content lines after a `[1]` line within which `[2]` and `[3]`
/// must follow for the line to open a heading-less list.
const HEADINGLESS_LOOKAHEAD: usize = 12;

/// A list without a heading (`REVTeX` sets `[1] ...` right after the last
/// section): the last `[1]` line that `[2]` and `[3]` follow, in order,
/// within [`HEADINGLESS_LOOKAHEAD`] content lines. The section's
/// `first_line` is the `[1]` line itself and its `heading` is empty.
fn headingless_section(pages: &[PageText]) -> Option<ReferenceSection> {
    let mut found: Option<ReferenceSection> = None;
    for (pos, page) in pages.iter().enumerate() {
        for (i, line) in page.lines.iter().enumerate() {
            let Some(caps) = bracket_label_re_first().captures(&line.text) else {
                continue;
            };
            if caps.get(1).map(|m| m.as_str()) != Some("1") {
                continue;
            }
            if numbered_run_follows(pages, pos, i) {
                found = Some(ReferenceSection {
                    first_page: page.page,
                    first_line: i,
                    heading: String::new(),
                });
            }
        }
    }
    found
}

/// `[n]` at the start of a line followed by text.
fn bracket_label_re_first() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*\[(\d+)\]\s+\S").expect("valid regex"))
}

/// Do `[2]` and then `[3]` lines follow the `[1]` line at (`pos`, `line`)
/// within [`HEADINGLESS_LOOKAHEAD`] content lines?
fn numbered_run_follows(pages: &[PageText], pos: usize, first_line: usize) -> bool {
    let mut expected: u32 = 2;
    let mut seen = 0usize;
    for (p, page) in pages.iter().enumerate().skip(pos) {
        let skip = if p == pos { first_line + 1 } else { 0 };
        for line in page.lines.iter().skip(skip) {
            let text = line.text.trim();
            if text.is_empty() || is_accent_only(text) || page_number_re().is_match(text) {
                continue;
            }
            let number = bracket_label_re()
                .captures(text)
                .and_then(|caps| caps.get(1))
                .and_then(|m| m.as_str().parse::<u32>().ok());
            if number == Some(expected) {
                expected += 1;
                if expected > 3 {
                    return true;
                }
            }
            seen += 1;
            if seen >= HEADINGLESS_LOOKAHEAD {
                return false;
            }
        }
    }
    false
}

/// Every reference-list heading in document order: lines matching
/// [`heading_re`] (`References`, `Bibliography`, `Supplementary References`,
/// `References for the Appendices`, `Notes and references`, ...) that a
/// reference entry follows
/// within a few lines. When no heading qualifies, the last heading line is
/// taken as printed; without any heading, a `[1] ... [2] ... [3]` run opens
/// a heading-less list (see [`headingless_section`]).
pub fn find_reference_sections(pages: &[PageText]) -> Vec<ReferenceSection> {
    let mut candidates: Vec<(usize, ReferenceSection)> = Vec::new();
    for (pos, page) in pages.iter().enumerate() {
        for (i, line) in page.lines.iter().enumerate() {
            if heading_re().is_match(&line.text) {
                candidates.push((
                    pos,
                    ReferenceSection {
                        first_page: page.page,
                        first_line: i,
                        heading: line.text.trim().to_string(),
                    },
                ));
            }
        }
    }
    let mut sections: Vec<ReferenceSection> = Vec::new();
    for (k, (pos, section)) in candidates.iter().enumerate() {
        let stop = candidates.get(k + 1).map(|(p, next)| (*p, next.first_line));
        if list_follows(pages, *pos, section.first_line, stop) {
            sections.push(section.clone());
        }
    }
    if sections.is_empty()
        && let Some((_, last)) = candidates.last()
    {
        sections.push(last.clone());
    }
    if sections.is_empty()
        && let Some(section) = headingless_section(pages)
    {
        sections.push(section);
    }
    sections
}

/// The main reference list: the first heading of [`find_reference_sections`]
/// (a `References` line in a table of contents is not one, since no entry
/// follows it).
pub fn find_reference_section(pages: &[PageText]) -> Option<ReferenceSection> {
    find_reference_sections(pages).into_iter().next()
}

/// `(page number, line index)` of the heading that follows `section`, if
/// any: where this list must stop.
fn following_section(pages: &[PageText], section: &ReferenceSection) -> Option<(u32, usize)> {
    let own = (section.first_page, section.first_line);
    find_reference_sections(pages)
        .iter()
        .map(|s| (s.first_page, s.first_line))
        .find(|&pos| pos > own)
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

/// Is the box in the top or bottom [`MARGIN_BAND`] of a page `height` tall?
fn in_margin(b: BBox, height: f32) -> bool {
    b.y1 > height * (1.0 - MARGIN_BAND) || b.y0 < height * MARGIN_BAND
}

/// Per line of `page`: does it sit where running headers, footers and
/// folios go? Any line in the margin bands counts (as does a line without
/// a bbox), and so does the top-most row of the page when it lies in the
/// top [`HEADER_BAND`] and is separated from the row below it by at least
/// its own height (LNCS and IEEE journal running heads sit below the 8%
/// band but clear of the body).
fn furniture_flags(page: &PageText) -> Vec<bool> {
    let height = page.height;
    let mut flags: Vec<bool> = page
        .lines
        .iter()
        .map(|line| line.bbox.is_none_or(|b| in_margin(b, height)))
        .collect();
    let Some(top) = page
        .lines
        .iter()
        .filter_map(|l| l.bbox)
        .map(|b| b.y1)
        .max_by(f32::total_cmp)
    else {
        return flags;
    };
    if top <= height * (1.0 - HEADER_BAND) {
        return flags;
    }
    let on_top_row = |b: BBox| b.y1 >= top - 0.5 * (b.y1 - b.y0);
    let below = page
        .lines
        .iter()
        .filter_map(|l| l.bbox)
        .filter(|&b| !on_top_row(b))
        .map(|b| b.y1)
        .max_by(f32::total_cmp);
    for (flag, line) in flags.iter_mut().zip(&page.lines) {
        if let Some(b) = line.bbox
            && on_top_row(b)
        {
            let gap = below.map_or(f32::INFINITY, |next| b.y0 - next);
            if gap >= b.y1 - b.y0 {
                *flag = true;
            }
        }
    }
    flags
}

/// Digit-normalised texts ([`digit_key`]) of the running headers and
/// footers of the document: short furniture-position lines
/// ([`furniture_flags`]) that repeat on at least two pages (`Page 30 of
/// 35` and `Page 31 of 35` repeat; two `arXiv:` lines do not).
fn repeated_furniture(pages: &[PageText]) -> Vec<String> {
    let mut pages_per_text: BTreeMap<String, Vec<u32>> = BTreeMap::new();
    for page in pages {
        let flags = furniture_flags(page);
        for (line, flag) in page.lines.iter().zip(flags) {
            let text = line.text.trim();
            if !flag || text.is_empty() || text.chars().count() > MAX_FURNITURE_CHARS {
                continue;
            }
            let seen = pages_per_text.entry(digit_key(text)).or_default();
            if !seen.contains(&page.page) {
                seen.push(page.page);
            }
        }
    }
    pages_per_text
        .into_iter()
        .filter(|(_, seen)| seen.len() >= 2)
        .map(|(text, _)| text)
        .collect()
}

/// Does `text` end inside a DOI or URL that the next line may continue
/// (`https://doi.org/10.1016/j.jmp.2013.05.` before a `005` line)?
fn identifier_open(text: &str) -> bool {
    let Some(last) = text.split_whitespace().next_back() else {
        return false;
    };
    (doi_start_re().is_match(last) || last.contains("http") || last.starts_with("www."))
        && last.ends_with(['.', '/', '-', '_'])
}

/// Lines of the reference list in reading order, from the line after the
/// heading (or from the `[1]` line of a heading-less list) to `stop` (the
/// next heading, as page number and line index) or the end of the
/// document, with page furniture removed: running headers and footers
/// ([`repeated_furniture`]) and page numbers, except a numeric line that
/// continues a DOI or URL of the line before it and does not sit in a
/// margin band.
/// Collect section lines with document-wide furniture already computed.
fn section_lines_with_furniture(
    pages: &[PageText],
    section: &ReferenceSection,
    stop: Option<(u32, usize)>,
    repeated: &[String],
) -> Vec<SectionLine> {
    let mut lines: Vec<SectionLine> = Vec::new();
    let mut row = Vec::<SectionLine>::new();
    let mut row_text_bytes = 0usize;
    // The largest font size among the row's fragments, which decides the
    // baseline tolerance for the next fragment (as the merged row used to).
    let mut row_size: Option<f32> = None;
    'pages: for page in pages {
        if page.page < section.first_page {
            continue;
        }
        let skip = if page.page != section.first_page {
            0
        } else if section.heading.is_empty() {
            section.first_line
        } else {
            section.first_line + 1
        };
        let flags = furniture_flags(page);
        for (i, line) in page.lines.iter().enumerate().skip(skip) {
            if stop.is_some_and(|s| (page.page, i) >= s) {
                break 'pages;
            }
            let text = line.text.trim();
            if text.is_empty() || is_accent_only(text) {
                continue;
            }
            // A bare `[n]` label at the top of two pages (`[15]` and `[37]`
            // of a detached label column) is not a running header.
            let furniture = flags.get(i).copied().unwrap_or(true);
            if furniture && repeated.contains(&digit_key(text)) && !bare_label_re().is_match(text) {
                continue;
            }
            if page_number_re().is_match(text) {
                let margin = line.bbox.is_some_and(|b| in_margin(b, page.height));
                let continues = !margin
                    && row
                        .iter()
                        .max_by(|a, b| match (a.x0, b.x0) {
                            (Some(a), Some(b)) => a.total_cmp(&b),
                            (Some(_), None) => std::cmp::Ordering::Less,
                            (None, Some(_)) => std::cmp::Ordering::Greater,
                            (None, None) => std::cmp::Ordering::Equal,
                        })
                        .or_else(|| lines.last())
                        .is_some_and(|prev| identifier_open(&prev.text));
                if !continues {
                    continue;
                }
            }
            let fragment = SectionLine {
                page: page.page,
                line: i,
                column: line.column,
                x0: line.bbox.map(|b| b.x0),
                y0: line.bbox.map(|b| b.y0),
                size: line_size(page, line),
                text: text.to_string(),
            };
            // Justified columns leave gaps wider than the layout pass joins,
            // so one printed row can arrive as several lines: re-join them.
            let same_row = row
                .first()
                .is_some_and(|first| first.same_row_as(&fragment, row_size));
            let added_bytes = fragment.text.len() + usize::from(!row.is_empty());
            let within_budget = row.len() < MAX_ROW_FRAGMENTS
                && row_text_bytes.saturating_add(added_bytes) <= MAX_ROW_TEXT_BYTES;
            if !same_row || !within_budget {
                if !row.is_empty() {
                    lines.push(finish_row(std::mem::take(&mut row)));
                }
                row_text_bytes = 0;
                row_size = None;
            }
            row_text_bytes += fragment.text.len() + usize::from(!row.is_empty());
            row_size = max_size(row_size, fragment.size);
            row.push(fragment);
        }
    }
    if !row.is_empty() {
        lines.push(finish_row(row));
    }
    reattach_numbered_labels(lines)
}

/// A label-only column can precede all of its entry text in reading order.
/// Reattach punctuated labels to the nearest text on their printed row before
/// detecting list style or end headings. Bare integers remain untouched.
fn reattach_numbered_labels(mut lines: Vec<SectionLine>) -> Vec<SectionLine> {
    let standalone = |text: &str| {
        text.strip_suffix(['.', ')']).is_some_and(|number| {
            !number.is_empty() && number.len() <= 4 && number.bytes().all(|b| b.is_ascii_digit())
        })
    };
    let mut rows: BTreeMap<u32, Vec<(f32, usize)>> = BTreeMap::new();
    for (i, line) in lines.iter().enumerate() {
        if !standalone(&line.text)
            && let Some(y) = line.y0.filter(|y| y.is_finite())
        {
            rows.entry(line.page).or_default().push((y, i));
        }
    }
    for page_rows in rows.values_mut() {
        page_rows.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
    }
    let mut used = vec![false; lines.len()];
    let mut removed = vec![false; lines.len()];
    for i in 0..lines.len() {
        let label = &lines[i];
        if !standalone(&label.text) {
            continue;
        }
        let (Some(x), Some(y), Some(page_rows)) = (label.x0, label.y0, rows.get(&label.page))
        else {
            continue;
        };
        let tolerance = 0.4 * label.size.unwrap_or(10.0);
        if !x.is_finite() || !y.is_finite() || !tolerance.is_finite() || tolerance <= 0.0 {
            continue;
        }
        let start = page_rows.partition_point(|&(baseline, _)| baseline < y - tolerance);
        let end = page_rows.partition_point(|&(baseline, _)| baseline <= y + tolerance);
        // Bound work even when hostile geometry puts every line on one row.
        if end - start > MAX_ROW_FRAGMENTS {
            continue;
        }
        let target = page_rows[start..end]
            .iter()
            .filter_map(|&(_, k)| {
                let gap = lines[k].x0? - x;
                (!used[k] && gap > 0.0 && gap <= LABEL_TEXT_GAP).then_some((gap, k))
            })
            .min_by(|a, b| a.0.total_cmp(&b.0));
        if let Some((_, k)) = target {
            lines[k].text = format!("{} {}", lines[i].text, lines[k].text);
            lines[k].x0 = Some(x);
            used[k] = true;
            removed[i] = true;
        }
    }
    lines
        .into_iter()
        .enumerate()
        .filter_map(|(i, line)| (!removed[i]).then_some(line))
        .collect()
}

/// Printed number and label of a numbered entry start, per style. A bare
/// number (`12 Q. Zhang`) labels as `12` and must be followed by an
/// uppercase letter here (see [`entry_label`] for the lowercase case).
fn numbered_label(style: Style, text: &str) -> Option<(u32, String)> {
    let re = match style {
        Style::Bracket => bracket_label_re(),
        Style::Dot => dot_label_re(),
        Style::Paren => paren_label_re(),
        Style::Bare => bare_number_label_re(),
        Style::AuthorYear | Style::Detached => return None,
    };
    let caps = re.captures(text)?;
    let number: u32 = caps.get(1)?.as_str().parse().ok()?;
    let label = match style {
        Style::Bracket => format!("[{number}]"),
        Style::Dot => format!("{number}."),
        Style::Paren => format!("{number})"),
        Style::Bare => number.to_string(),
        Style::AuthorYear | Style::Detached => return None,
    };
    Some((number, label))
}

/// Number and label of the entry that `text` starts in a numbered list
/// whose next number is `expected` (`None` before the first entry): a label
/// from `expected` to `expected + 2`. A bare number followed by a lowercase
/// word (`8 arXiV, ...`) starts an entry only when it is exactly the
/// expected one.
fn entry_label(style: Style, text: &str, expected: Option<u32>) -> Option<(u32, String)> {
    if let Some((number, label)) = numbered_label(style, text) {
        return expected
            .is_none_or(|e| (e..=e + 2).contains(&number))
            .then_some((number, label));
    }
    if style == Style::Bare
        && let Some(e) = expected
        && bare_number(text) == Some(e)
    {
        return Some((e, e.to_string()));
    }
    None
}

/// Number of content lines after the `1` line of a bare-number list within
/// which `2` and `3` must follow.
const BARE_RUN_LOOKAHEAD: usize = 40;

/// Does a bare-number list start here: a `1 Q. Zhang` line among the first
/// three lines, then `2` and `3` lines in order within
/// [`BARE_RUN_LOOKAHEAD`] lines?
fn bare_run(lines: &[SectionLine]) -> bool {
    let Some(first) = lines
        .iter()
        .take(3)
        .position(|line| numbered_label(Style::Bare, &line.text).is_some_and(|(n, _)| n == 1))
    else {
        return false;
    };
    let mut expected: u32 = 2;
    for line in lines.iter().skip(first + 1).take(BARE_RUN_LOOKAHEAD) {
        if bare_number(&line.text) == Some(expected) {
            expected += 1;
            if expected > 3 {
                return true;
            }
        }
    }
    false
}

/// Numbering style from the first three lines (the first line may be a
/// stray fragment or a column artefact). Bare `[n]` lines are labels the
/// layout pass detached from their entries: they are no evidence of the
/// bracket style, but a run of them ([`detached_run`]) numbers the list
/// ([`Style::Detached`]). A bare-number run `1`, `2`, `3`
/// ([`bare_run`]) is an RSC list ([`Style::Bare`]).
fn detect_style(lines: &[SectionLine]) -> Style {
    let candidates = lines
        .iter()
        .filter(|line| !bare_label_re().is_match(&line.text))
        .take(3);
    for line in candidates {
        if bracket_label_re().is_match(&line.text) {
            return Style::Bracket;
        }
        if dot_label_re().is_match(&line.text) {
            return Style::Dot;
        }
        if paren_label_re().is_match(&line.text) {
            return Style::Paren;
        }
    }
    if bare_run(lines) {
        return Style::Bare;
    }
    if detached_run(lines) {
        return Style::Detached;
    }
    Style::AuthorYear
}

/// Drop the lines of a numbered list that belong to another column: on a
/// page whose labels start at one or more x levels, a line that starts
/// left of every label level (by more than [`INDENT_TOLERANCE`]) is body
/// text or a caption that the layout pass interleaved with the list (a
/// figure caption and a `5 Conclusion` paragraph set left of an LNCS list
/// in the right column). Pages without a label keep every line.
fn drop_foreign_column_lines(lines: Vec<SectionLine>, style: Style) -> Vec<SectionLine> {
    let mut label_levels: BTreeMap<u32, Vec<f32>> = BTreeMap::new();
    for line in &lines {
        if let Some(x0) = line.x0
            && numbered_label(style, &line.text).is_some()
        {
            label_levels.entry(line.page).or_default().push(x0);
        }
    }
    lines
        .into_iter()
        .filter(|line| {
            let (Some(x0), Some(levels)) = (line.x0, label_levels.get(&line.page)) else {
                return true;
            };
            levels.iter().any(|&level| x0 >= level - INDENT_TOLERANCE)
        })
        .collect()
}

/// The lines of one reference list cut at its end, with the numbering
/// style, the text used for hyphenation decisions and where the list ends.
struct ListBody {
    lines: Vec<SectionLine>,
    style: Style,
    /// Every section line joined by newlines (before the cut), lower-cased,
    /// for [`hyphen_break`].
    context: String,
    /// `(page number, line index)` of the line that ends the list (an
    /// appendix heading, a caption, a biography, ...); `None` when the list
    /// runs to `stop` or to the end of the document.
    end: Option<(u32, usize)>,
    /// Printed numbers of the bare `[n]` lines of a [`Style::Detached`]
    /// list, in reading order and before the cut, preceded by the labels
    /// set above the heading when they number the entries before the
    /// first label in the list; empty for other styles.
    labels: Vec<u32>,
    /// The bare `[n]` lines of a [`Style::Detached`] list with their
    /// positions: those above the heading on its page, then those of the
    /// list before the cut; empty for other styles.
    label_rows: Vec<LabelRow>,
}

/// A bare `[n]` label line of a detached label column.
#[derive(Clone, Copy, Debug)]
struct LabelRow {
    /// `(page number, line index)` of the label line.
    at: (u32, usize),
    x0: Option<f32>,
    y0: Option<f32>,
    size: Option<f32>,
    number: u32,
}

/// The bare `[n]` lines above the heading line on the heading's page (an
/// IEEE list whose first labels the layout pass set before `REFERENCES`,
/// arXiv:2509.12458), in reading order. None for a heading-less list.
fn labels_above_heading(pages: &[PageText], section: &ReferenceSection) -> Vec<LabelRow> {
    if section.heading.is_empty() {
        return Vec::new();
    }
    let Some(page) = pages.iter().find(|p| p.page == section.first_page) else {
        return Vec::new();
    };
    page.lines
        .iter()
        .enumerate()
        .take(section.first_line)
        .filter_map(|(i, line)| {
            let number = bare_label_number(&line.text)?;
            Some(LabelRow {
                at: (page.page, i),
                x0: line.bbox.map(|b| b.x0),
                y0: line.bbox.map(|b| b.y0),
                size: line_size(page, line),
                number,
            })
        })
        .collect()
}

/// Collect, clean and cut the lines of the list that starts at `section`
/// and stops before `stop`.
#[cfg(test)]
fn list_body(
    pages: &[PageText],
    section: &ReferenceSection,
    stop: Option<(u32, usize)>,
) -> ListBody {
    let repeated = repeated_furniture(pages);
    list_body_with_furniture(pages, section, stop, &repeated)
}

fn list_body_with_furniture(
    pages: &[PageText],
    section: &ReferenceSection,
    stop: Option<(u32, usize)>,
    repeated: &[String],
) -> ListBody {
    let mut lines = section_lines_with_furniture(pages, section, stop, repeated);
    let style = detect_style(&lines);
    // Labels the layout pass detached from their entries carry no text;
    // a detached list keeps their numbers (with their positions) to label
    // the entries with.
    let mut detached: Vec<LabelRow> = Vec::new();
    match style {
        Style::AuthorYear => {
            lines.retain(|line| !bare_label_re().is_match(&line.text));
        }
        Style::Detached => {
            for line in &lines {
                if let Some(number) = bare_label_number(&line.text) {
                    detached.push(LabelRow {
                        at: (line.page, line.line),
                        x0: line.x0,
                        y0: line.y0,
                        size: line.size,
                        number,
                    });
                }
            }
            lines.retain(|line| !bare_label_re().is_match(&line.text));
        }
        Style::Bracket | Style::Dot | Style::Paren | Style::Bare => {
            lines = drop_foreign_column_lines(lines, style);
        }
    }
    let median = median_size(&lines);
    let context: String = lines
        .iter()
        .map(|l| l.text.as_str())
        .collect::<Vec<&str>>()
        .join("\n")
        .to_lowercase();
    // The list ends at the first end heading. Everything after it (an
    // appendix, tables, biographies) is set at the entry-start x and must
    // not feed the indent-level statistics of the list itself. A numbered
    // list interrupted by a biography, caption or table resumes at its
    // next expected label (see [`resume_index`]).
    let mut kept: Vec<SectionLine> = Vec::with_capacity(lines.len());
    let mut end: Option<(u32, usize)> = None;
    let mut k = 0usize;
    while k < lines.len() {
        if !is_end_heading(&lines[k], style, median) {
            kept.push(lines[k].clone());
            k += 1;
            continue;
        }
        let expected = next_expected_label(&kept, style);
        if let Some(next) = resume_index(&lines, k, style, expected) {
            k = next;
        } else {
            end = Some((lines[k].page, lines[k].line));
            break;
        }
    }
    let lines = kept;
    detached.retain(|row| end.is_none_or(|e| row.at < e));
    let mut labels: Vec<u32> = detached.iter().map(|row| row.number).collect();
    let mut label_rows: Vec<LabelRow> = Vec::new();
    if style == Style::Detached {
        let above = labels_above_heading(pages, section);
        let above_numbers: Vec<u32> = above.iter().map(|row| row.number).collect();
        if let Some(&first) = labels.first()
            && first > 1
        {
            let mut missing: Vec<u32> = (1..first).collect();
            if above_numbers.ends_with(&missing) {
                missing.extend_from_slice(&labels);
                labels = missing;
            }
        }
        label_rows = above;
        label_rows.extend(detached);
    }
    ListBody {
        lines,
        style,
        context,
        end,
        labels,
        label_rows,
    }
}

/// The number the next entry of the numbered list `lines` would carry (as
/// [`segment_numbered`] reads the labels), or `None` before any label.
fn next_expected_label(lines: &[SectionLine], style: Style) -> Option<u32> {
    let mut expected: Option<u32> = None;
    for line in lines {
        if let Some((number, _)) = entry_label(style, &line.text, expected) {
            expected = Some(number + 1);
        }
    }
    expected
}

/// Where a `[n]` / `n.` / `n)` list cut at `lines[cut]` resumes: the index
/// of the next line labelled `expected`, when the cut is not a section
/// heading (`Appendix`, `Acknowledgments`, ...) but an interruption (an
/// author biography, a caption, a table) and no heading and no label below
/// `expected` (an appendix numbering its own items) comes before it
/// (arXiv:2508.19485 sets biographies between entries 48 and 49). `None`
/// ends the list at the cut.
fn resume_index(
    lines: &[SectionLine],
    cut: usize,
    style: Style,
    expected: Option<u32>,
) -> Option<usize> {
    if matches!(style, Style::AuthorYear | Style::Detached) {
        return resume_author_year(lines, cut);
    }
    if !matches!(style, Style::Bracket | Style::Dot | Style::Paren) {
        return None;
    }
    let expected = expected?;
    // An appendix is a new section: its numbered items never continue the
    // list. A back-matter heading at the cut (`Conflict of interest`,
    // `Supplementary material`, `Funding`) is how a two-column page reads:
    // the band above the list comes after the first column of entries, and
    // the list resumes at its next label, provided that entry carries
    // reference evidence (a year, a venue) within its first lines. A smaller
    // label first (a table or appendix numbering its own items) ends it.
    if appendix_heading_re().is_match(&lines.get(cut)?.text) {
        return None;
    }
    for (k, line) in lines.iter().enumerate().skip(cut + 1).take(RESUME_REACH) {
        if let Some((number, _)) = numbered_label(style, &line.text) {
            if number == expected {
                let head: String = lines[k..lines.len().min(k + 6)]
                    .iter()
                    .map(|l| l.text.as_str())
                    .collect::<Vec<_>>()
                    .join(" ");
                return has_reference_evidence(&head).then_some(k);
            }
            if number < expected {
                return None;
            }
        }
    }
    None
}

/// `Appendix A`, `Appendices`, `Supplementary Appendix`: a section heading
/// after which a numbered list never resumes.
fn appendix_heading_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)^\s*(?:(?:\d+|[A-Z]|[IVX]+)[.:]?\s+)?(?:supplementary\s+|online\s+)?appendi(?:x|ces)\b")
            .expect("valid regex")
    })
}

/// Fewest consecutive author-year entry starts that mark a resumed list.
const RESUME_RUN: usize = 2;
/// Lines searched after an interrupting heading for the resumed list.
const RESUME_REACH: usize = 60;

/// Where an author-year list cut at `lines[cut]` resumes. In a two-column
/// page the band above the list (author contributions, funding,
/// acknowledgments) is read after the first column of entries, so the list
/// is "ended" by a heading and then continues. The list resumes at the
/// first run of [`RESUME_RUN`] consecutive lines that open like entries
/// ([`is_author_year_start`]) within [`RESUME_REACH`] lines; `None` when
/// no such run follows.
fn resume_author_year(lines: &[SectionLine], cut: usize) -> Option<usize> {
    let mut run_start: Option<usize> = None;
    let mut run = 0usize;
    for (k, line) in lines.iter().enumerate().skip(cut + 1).take(RESUME_REACH) {
        if is_author_year_start(&line.text) {
            if run == 0 {
                run_start = Some(k);
            }
            run += 1;
            if run >= RESUME_RUN {
                return run_start;
            }
        } else {
            run = 0;
            run_start = None;
        }
    }
    None
}

/// Largest distance from the left edge of a detached `[n]` label to the
/// left edge of the entry text it labels.
const LABEL_TEXT_GAP: f32 = 40.0;

/// The unused label row in `rows` that labels `line`: on the same page and
/// printed row (baselines within 0.4 × the font size), left of the line by
/// at most [`LABEL_TEXT_GAP`]; the nearest one when several qualify (the
/// label columns of two text columns share their rows).
fn label_for_line(line: &SectionLine, rows: &[LabelRow], used: &[bool]) -> Option<usize> {
    let (Some(x0), Some(y0)) = (line.x0, line.y0) else {
        return None;
    };
    let mut best: Option<(usize, f32)> = None;
    for (k, row) in rows.iter().enumerate() {
        let (Some(rx), Some(ry)) = (row.x0, row.y0) else {
            continue;
        };
        if used.get(k).copied().unwrap_or(true) || row.at.0 != line.page {
            continue;
        }
        let size = line.size.or(row.size).unwrap_or(10.0);
        let gap = x0 - rx;
        if (ry - y0).abs() > 0.4 * size || gap <= 0.0 || gap > LABEL_TEXT_GAP {
            continue;
        }
        if best.is_none_or(|(_, g)| gap < g) {
            best = Some((k, gap));
        }
    }
    best.map(|(k, _)| k)
}

/// Segment a detached list by its label column: a line printed on the row
/// of a `[n]` label ([`label_for_line`]) starts the entry labelled `[n]`;
/// every other line continues the entry before it. The labels are read,
/// never counted: a list whose first labels sit above the heading or at a
/// page top keeps its printed numbers. `None` (segment by the text
/// instead) without at least [`DETACHED_RUN`] positioned labels or when
/// fewer than half of them label a line.
fn segment_by_label_rows(
    lines: &[SectionLine],
    rows: &[LabelRow],
    context: &str,
) -> Option<Vec<ReferenceEntry>> {
    let placed = rows
        .iter()
        .filter(|row| row.x0.is_some() && row.y0.is_some())
        .count();
    if placed < DETACHED_RUN {
        return None;
    }
    let mut used: Vec<bool> = vec![false; rows.len()];
    let mut entries: Vec<ReferenceEntry> = Vec::new();
    let mut matched = 0usize;
    for line in lines {
        if let Some(k) = label_for_line(line, rows, &used) {
            used[k] = true;
            matched += 1;
            let number = rows[k].number;
            push_entry(&mut entries, Some(format!("[{number}]")), line);
        } else if entries.is_empty() {
            push_entry(&mut entries, None, line);
        } else {
            append_continuation(&mut entries, &line.text, context);
        }
    }
    (matched * 2 >= placed).then_some(entries)
}

/// Label the entries of a list whose `[n]` labels arrived on lines of
/// their own: the k-th printed label goes to the k-th entry in reading
/// order; entries beyond the last label continue the sequence.
fn assign_detached_labels(entries: &mut [ReferenceEntry], labels: &[u32]) {
    let mut next: u32 = 1;
    for (k, entry) in entries.iter_mut().enumerate() {
        let number = labels.get(k).copied().unwrap_or(next);
        entry.label = Some(format!("[{number}]"));
        next = number.saturating_add(1);
    }
}

/// Segment the list that starts at `section` and stops before `stop`.
fn segment_list(
    pages: &[PageText],
    section: &ReferenceSection,
    stop: Option<(u32, usize)>,
) -> Vec<ReferenceEntry> {
    let repeated = repeated_furniture(pages);
    segment_list_with_furniture(pages, section, stop, &repeated)
}

fn segment_list_with_furniture(
    pages: &[PageText],
    section: &ReferenceSection,
    stop: Option<(u32, usize)>,
    repeated: &[String],
) -> Vec<ReferenceEntry> {
    let body = list_body_with_furniture(pages, section, stop, repeated);
    match body.style {
        Style::AuthorYear => segment_author_year(&body.lines, &body.context),
        Style::Detached => {
            if let Some(entries) =
                segment_by_label_rows(&body.lines, &body.label_rows, &body.context)
            {
                return entries;
            }
            let mut entries = segment_author_year(&body.lines, &body.context);
            assign_detached_labels(&mut entries, &body.labels);
            entries
        }
        Style::Bracket | Style::Dot | Style::Paren | Style::Bare => {
            segment_numbered(&body.lines, body.style, &body.context)
        }
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

/// A heading or block that ends the reference list: `Appendix`,
/// `Supplementary`, a table caption or a row of numbers, the first line of
/// an author biography, or, when sizes are known, a short line set clearly
/// larger than the body.
fn is_end_heading(line: &SectionLine, style: Style, median: Option<f32>) -> bool {
    let short = line.text.chars().count() <= 80;
    if short && (end_heading_re().is_match(&line.text) || caption_re().is_match(&line.text)) {
        return true;
    }
    // `Appendices for “EquiReg: Equivariance Regularized Diffusion for
    // Inverse Problems”`: a long appendix title, with no year and no final
    // period, before the appendix prose.
    if line.text.chars().count() <= 160
        && appendix_title_re().is_match(&line.text)
        && !evidence_year_re().is_match(&line.text)
        && !line.text.trim_end().ends_with(['.', ','])
    {
        return true;
    }
    if numbered_label(style, &line.text).is_some() {
        return false;
    }
    if numeric_row_re().is_match(&line.text) || biography_re().is_match(&line.text) {
        return true;
    }
    let (Some(size), Some(typical)) = (line.size, median) else {
        return false;
    };
    short && size >= typical * 1.15 && line.text.chars().next().is_some_and(char::is_uppercase)
}

/// Split the reference section into entries. `raw`, `label`, `index` and
/// `page` are filled; call [`parse_entry`] for the parsed fields.
pub fn segment_entries(pages: &[PageText], section: &ReferenceSection) -> Vec<ReferenceEntry> {
    let stop = following_section(pages, section);
    segment_list(pages, section, stop)
}

/// The first line's left edge, baseline and size as an anchor box, for
/// attaching link annotations to the entry.
fn anchor_of(line: &SectionLine) -> Option<BBox> {
    let (Some(x0), Some(y0)) = (line.x0, line.y0) else {
        return None;
    };
    let size = line.size.unwrap_or(10.0);
    Some(BBox {
        x0,
        y0,
        x1: x0,
        y1: y0 + size,
    })
}

fn push_entry(entries: &mut Vec<ReferenceEntry>, label: Option<String>, line: &SectionLine) {
    let index = u32::try_from(entries.len() + 1).unwrap_or(u32::MAX);
    entries.push(ReferenceEntry {
        index,
        label,
        raw: line.text.clone(),
        page: line.page,
        anchor: anchor_of(line),
        ..ReferenceEntry::default()
    });
}

/// Alphanumeric run that ends `text` (the first half of a word broken at a
/// line end).
fn word_tail(text: &str) -> &str {
    let start = text
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_alphanumeric())
        .last()
        .map_or(text.len(), |(i, _)| i);
    &text[start..]
}

/// Alphanumeric run that starts `text` (the second half of a broken word).
fn word_head(text: &str) -> &str {
    let end = text
        .char_indices()
        .find(|(_, c)| !c.is_alphanumeric())
        .map_or(text.len(), |(i, _)| i);
    &text[..end]
}

/// How a hyphen at the end of a line is resolved when the next line is
/// joined on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HyphenJoin {
    /// A compound: `multi-` + `task` → `multi-task`.
    Keep,
    /// A word broken for justification: `recon-` + `struction` → `reconstruction`.
    Drop,
    /// Not a word break (`x -` + `y`): join with a space as usual.
    Separate,
}

/// Does `piece` (lower case) occur in `context` (the lower-cased section
/// text) as a whole word or hyphenated pair? The halves of a word broken at
/// a line end (`noise-` + newline + `regularized`) do not count: an
/// occurrence right before or after `-` and a newline is not evidence.
fn attested_in(context: &str, piece: &str) -> bool {
    if piece.is_empty() {
        return false;
    }
    context.match_indices(piece).any(|(start, _)| {
        let before = &context[..start];
        let after = &context[start + piece.len()..];
        let open = before
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric());
        let close = after.chars().next().is_none_or(|c| !c.is_alphanumeric());
        open && close && !before.ends_with("-\n") && !after.starts_with("-\n")
    })
}

/// Resolve the hyphen that ends `previous` against the line `next`.
///
/// A hyphen inside a URL or DOI and one before a continuation that does not
/// start in lowercase are kept. Otherwise [`hyphen_policy`] decides, with
/// `context` (the lower-cased section text) as the evidence of which words
/// and hyphenated pairs occur whole ([`attested_in`]); a join of a word that
/// does not occur is overridden to keep the hyphen after a
/// [`COMPOUND_PREFIXES`] entry or before a [`COMPOUND_HEADS`] entry.
fn hyphen_break(previous: &str, next: &str, context: &str) -> HyphenJoin {
    let Some(head_text) = previous.strip_suffix('-') else {
        return HyphenJoin::Separate;
    };
    let first = word_tail(head_text);
    let second = word_head(next);
    if first.is_empty() {
        return HyphenJoin::Separate;
    }
    // A hyphen inside a URL or DOI is part of the identifier
    // (`https://doi.org/10.1214/14-` + `sts504`).
    let last_token = head_text.split_whitespace().next_back().unwrap_or("");
    if last_token.contains("http")
        || last_token.starts_with("www.")
        || doi_start_re().is_match(last_token)
    {
        return HyphenJoin::Keep;
    }
    // `EFFI-` + `CIENT`: a word set in capitals and broken at the line end.
    if let Some(join) = capitals_break(first, second, context) {
        return join;
    }
    if !next.starts_with(|c: char| c.is_lowercase()) {
        return HyphenJoin::Keep;
    }
    let attested = |piece: &str| attested_in(context, piece);
    match hyphen_policy(first, second, &attested) {
        HyphenPolicy::Keep => HyphenJoin::Keep,
        HyphenPolicy::Join => {
            let first_lower = first.to_lowercase();
            let second_lower = second.to_lowercase();
            let joined_seen = attested(&format!("{first_lower}{second_lower}"));
            if !joined_seen
                && (COMPOUND_PREFIXES.contains(&first_lower.as_str())
                    || COMPOUND_HEADS.contains(&second_lower.as_str())
                    || capitalised_prefix(first, second))
            {
                HyphenJoin::Keep
            } else {
                HyphenJoin::Drop
            }
        }
    }
}

/// First halves of title-case compounds (lower case) that keep their
/// hyphen before an unattested lowercase word ([`capitalised_prefix`]).
const CAPITALISED_COMPOUND_PREFIXES: &[&str] = &[
    "multi", "cross", "self", "semi", "non", "pre", "post", "co", "sub", "inter", "intra", "meta",
    "anti", "bi", "tri", "dual", "single", "low", "high", "long", "short", "real", "open", "two",
    "three", "zero", "few", "one", "fine", "coarse", "end", "full", "half", "well", "ill", "state",
];

/// Is `first` the capitalised first half of a title-case compound
/// (`Multi-` + `turn`, `Dual-` + `channel`): a [`CAPITALISED_COMPOUND_PREFIXES`]
/// entry set as a capital then lowercase, before a lowercase word of four
/// or more letters? Any other capitalised half (`Every-` + `body`, `Gen-` +
/// `erative`) and shorter endings (`Learn-` + `ing`) are word breaks; a
/// hyphenated pair attested in the section already keeps its hyphen in
/// [`hyphen_policy`].
fn capitalised_prefix(first: &str, second: &str) -> bool {
    let mut chars = first.chars();
    let capital = chars.next().is_some_and(char::is_uppercase);
    capital
        && chars.all(char::is_lowercase)
        && CAPITALISED_COMPOUND_PREFIXES.contains(&first.to_lowercase().as_str())
        && second.chars().count() >= 4
        && second.chars().all(char::is_lowercase)
}

/// Resolve a hyphen between two halves set in capitals (`EFFI-` +
/// `CIENT`, `TIME-` + `DIAL`, `SIG-` + `COMM`). The hyphen stays when the
/// hyphenated pair is attested, a half is a compound prefix or head
/// ([`COMPOUND_PREFIXES`], [`COMPOUND_HEADS`]) or the second half is an
/// attested word (`MULTI-` + `AGENT`); it is dropped when the joined word
/// is attested or has six or more letters with a vowel in each half (so
/// the acronyms of `ICL-` + `GNSS` stay apart). `None` when a half is not
/// all capitals, or when neither rule applies.
fn capitals_break(first: &str, second: &str, context: &str) -> Option<HyphenJoin> {
    let capitals = |half: &str| {
        half.chars().count() >= 2 && half.chars().all(|c| c.is_alphabetic() && c.is_uppercase())
    };
    if !capitals(first) || !capitals(second) {
        return None;
    }
    let first_lower = first.to_lowercase();
    let second_lower = second.to_lowercase();
    let attested = |piece: &str| attested_in(context, piece);
    if attested(&format!("{first_lower}-{second_lower}"))
        || COMPOUND_PREFIXES.contains(&first_lower.as_str())
        || COMPOUND_HEADS.contains(&second_lower.as_str())
        || (second_lower.chars().count() >= 3 && attested(&second_lower))
    {
        return Some(HyphenJoin::Keep);
    }
    let joined = format!("{first_lower}{second_lower}");
    let vowel = |half: &str| half.contains(['a', 'e', 'i', 'o', 'u', 'y']);
    let long = joined.chars().count() >= 6 && vowel(&first_lower) && vowel(&second_lower);
    (attested(&joined) || long).then_some(HyphenJoin::Drop)
}

/// Append a continuation line to the last entry, resolving a word broken by
/// a hyphen at the line end (see [`hyphen_break`]).
fn append_continuation(entries: &mut [ReferenceEntry], text: &str, context: &str) {
    let Some(last) = entries.last_mut() else {
        return;
    };
    if last.raw.is_empty() {
        last.raw.push_str(text);
        return;
    }
    let join = if text.is_empty() {
        HyphenJoin::Separate
    } else {
        hyphen_break(&last.raw, text, context)
    };
    match join {
        HyphenJoin::Keep => {}
        HyphenJoin::Drop => {
            last.raw.pop();
        }
        HyphenJoin::Separate => last.raw.push(' '),
    }
    last.raw.push_str(text);
}

/// Split the numbered list `lines` (already cut at the end heading) into
/// entries: every `[n]` / `n.` / `n)` / bare `n` label in sequence starts
/// one (see [`entry_label`]).
fn segment_numbered(lines: &[SectionLine], style: Style, context: &str) -> Vec<ReferenceEntry> {
    let mut entries: Vec<ReferenceEntry> = Vec::new();
    let mut expected: Option<u32> = None;
    for line in lines {
        if let Some((number, label)) = entry_label(style, &line.text, expected) {
            push_entry(&mut entries, Some(label), line);
            expected = Some(number + 1);
            continue;
        }
        // A list that restarts at 1 is a second (supplementary) list.
        if let Some((1, _)) = numbered_label(style, &line.text)
            && expected.is_some_and(|e| e > 3)
        {
            break;
        }
        append_continuation(&mut entries, &line.text, context);
    }
    entries
}

/// One cluster of line starts at (nearly) the same x position.
struct XLevel {
    x: f32,
    last: f32,
    count: usize,
}

/// Hanging-indent evidence for every line, from the x positions of the
/// whole section rather than the neighbouring line (which may be a row
/// fragment sitting anywhere in the column).
///
/// Line starts are clustered by x; a cluster is an entry-start level when it
/// is populated and no populated cluster lies within [`MAX_HANGING_INDENT`]
/// to its left; the populated clusters within that distance to the right of
/// a start level are the continuation levels. `Some(true)` marks lines at a
/// start level, `Some(false)` every other line with a position, `None` all
/// lines when the section shows no hanging indent at all.
fn layout_starts(lines: &[SectionLine]) -> Vec<Option<bool>> {
    let mut order: Vec<(f32, usize)> = lines
        .iter()
        .enumerate()
        .filter_map(|(i, l)| l.x0.map(|x| (x, i)))
        .collect();
    order.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut cluster_of: Vec<Option<usize>> = vec![None; lines.len()];
    let mut clusters: Vec<XLevel> = Vec::new();
    for &(x, i) in &order {
        let near = clusters
            .last()
            .is_some_and(|level| x - level.last <= INDENT_TOLERANCE);
        if near && let Some(level) = clusters.last_mut() {
            level.last = x;
            level.count += 1;
        } else {
            clusters.push(XLevel {
                x,
                last: x,
                count: 1,
            });
        }
        cluster_of[i] = Some(clusters.len() - 1);
    }
    let threshold = (order.len() / 20).max(2);
    let populated = |level: &XLevel| level.count >= threshold;
    let is_start: Vec<bool> = clusters
        .iter()
        .map(|level| {
            populated(level)
                && !clusters.iter().any(|other| {
                    populated(other)
                        && level.x - other.x > 0.0
                        && level.x - other.x <= MAX_HANGING_INDENT
                })
        })
        .collect();
    let indented: usize = clusters
        .iter()
        .enumerate()
        .filter(|&(c, level)| {
            !is_start[c]
                && populated(level)
                && clusters.iter().enumerate().any(|(other, start)| {
                    is_start[other]
                        && level.x - start.x > 0.0
                        && level.x - start.x <= MAX_HANGING_INDENT
                })
        })
        .map(|(_, level)| level.count)
        .sum();
    if order.is_empty() || (indented as f32) < MIN_INDENTED_SHARE * (order.len() as f32) {
        return vec![None; lines.len()];
    }
    cluster_of
        .iter()
        .map(|cluster| cluster.map(|c| is_start[c]))
        .collect()
}

fn ends_like_entry(text: &str) -> bool {
    text.trim_end()
        .chars()
        .next_back()
        .is_some_and(|c| matches!(c, '.' | ')' | ']' | '}') || c.is_ascii_digit())
}

/// Does `text` end like a whole entry, not like a wrapped author list: as
/// [`ends_like_entry`], but a final period must close a word, not an
/// initial or abbreviation (`... S. H. H.` is a wrapped list).
fn ends_like_whole_entry(text: &str) -> bool {
    let trimmed = text.trim_end();
    if !ends_like_entry(trimmed) {
        return false;
    }
    trimmed
        .strip_suffix('.')
        .is_none_or(|head| !period_is_abbreviation(trimmed, head.len()))
}

/// Start of an initials-first author list: `M. Mozaffari,`, `D. Floreano
/// and`, `S. A. H. Mohsan,`, `D. Giordan et al.`, `J.-M. Doe &`.
fn initials_start_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^\s*(?:\p{Lu}\.(?:-\p{L}\.)*\s?)+(?:(?:van|von|de|der|den|del|di|da|la|le|du)\s+)*\p{Lu}[\p{L}'’\-]+\s*(?:,|\band\b|&|\bet\s+al\b|$)",
        )
        .expect("valid regex")
    })
}

fn author_year_label(raw: &str) -> Option<String> {
    let surname: String =
        if let Some(found) = surname_re().captures(raw).and_then(|caps| caps.get(1)) {
            found
                .as_str()
                .split_whitespace()
                .collect::<Vec<&str>>()
                .join(" ")
        } else {
            // A lowercase handle (`gwern. 2020.`) labels as `gwern2020`.
            handle_start_re()
                .captures(raw)?
                .get(1)?
                .as_str()
                .to_string()
        };
    let year = year_paren_re()
        .captures(raw)
        .or_else(|| year_bare_re().captures(raw))
        .and_then(|caps| caps.get(1))
        .map_or_else(String::new, |m| m.as_str().to_string());
    Some(format!("{surname}{year}"))
}

/// Any year from 1500 to 2099, bare or parenthesised (older works such as
/// `(1843)` or `1687` count as reference evidence too).
fn evidence_year_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?:^|[^\d–\-—])((?:1[5-9]|20)\d{2})[a-z]?(?:[^\d–\-—]|$)")
            .expect("valid regex")
    })
}

/// The date marker of an undated or unpublished entry, in parentheses or
/// after a comma: `(n.d.)`, `(no date)`, `(forthcoming)`, `, in press`,
/// `(under review)`, `(to appear)`.
fn undated_marker_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)(?:\(|,)\s*(?:n\.\s?d\b\.?|(?:no date|forthcoming|in press|under review|to appear)\b)",
        )
        .expect("valid regex")
    })
}

/// Does `raw` carry the evidence every reference has: a year (1500–2099),
/// an undated marker, a DOI, an `arXiv` id or a URL?
fn has_reference_evidence(raw: &str) -> bool {
    evidence_year_re().is_match(raw)
        || undated_marker_re().is_match(raw)
        || doi_start_re().is_match(raw)
        || arxiv_re().is_match(raw)
        || url_re().is_match(raw)
}

/// Longest prefix of an entry searched for the year or date marker that
/// follows its first author.
const AUTHOR_YEAR_START_CHARS: usize = 160;

/// Does `raw` open like an author-year entry: `Surname, F.` or
/// `Surname AB` followed (within its first line's length) by a year or an
/// undated marker? Such an entry is never folded or dropped.
fn is_author_year_start(raw: &str) -> bool {
    let head: String = raw.chars().take(AUTHOR_YEAR_START_CHARS).collect();
    let Some(author) = author_start_re().find(&head) else {
        return false;
    };
    let rest = &head[author.end()..];
    evidence_year_re().is_match(rest) || undated_marker_re().is_match(rest)
}

/// Fold entries without any reference evidence into the entry before them
/// (a false start such as `Series A, containing papers ...`), and drop the
/// run of such entries at the end of the list (table rows, appendix text).
/// A list where no entry has evidence is left alone.
fn apply_evidence_guard(entries: Vec<ReferenceEntry>, context: &str) -> Vec<ReferenceEntry> {
    let evidence: Vec<bool> = entries
        .iter()
        .map(|e| has_reference_evidence(&e.raw) || is_author_year_start(&e.raw))
        .collect();
    let Some(last_ok) = evidence.iter().rposition(|&ok| ok) else {
        return entries;
    };
    let mut out: Vec<ReferenceEntry> = Vec::new();
    for (entry, ok) in entries.into_iter().zip(evidence).take(last_ok + 1) {
        if ok || out.is_empty() {
            let index = u32::try_from(out.len() + 1).unwrap_or(u32::MAX);
            out.push(ReferenceEntry { index, ..entry });
        } else {
            append_continuation(&mut out, &entry.raw, context);
        }
    }
    out
}

/// A surname-first author list closed by a parenthesised year:
/// `Berlinet, A. & Thomas-Agnan, C. (2003)`, `Chiu, T. Y. M., Leonard, T. &
/// Tsui, K.-W. (1996)`, `Omi, T., Aihara, K., et al. (2019)`.
fn author_year_signature_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^\s*(?:(?:van|von|de|der|den|del|di|da|la|le|du)\s+)*\p{Lu}[\p{L}'’\-]+(?:\s\p{Lu}[\p{L}'’\-]+)*,\s?\p{Lu}\.(?:\s?-?\p{L}\.)*(?:(?:,\s|,?\s&\s|,?\sand\s)(?:(?:van|von|de|der|den|del|di|da|la|le|du)\s+)*\p{Lu}[\p{L}'’\-]+(?:\s\p{Lu}[\p{L}'’\-]+)*,\s?\p{Lu}\.(?:\s?-?\p{L}\.)*)*(?:,?\s(?:&\s)?et\s+al\.)?,?\s\((?:1[5-9]|20)\d{2}[a-z]?\)",
        )
        .expect("valid regex")
    })
}

/// Two surname-first names in a comma-separated list, the second followed
/// by a comma, `&`, `and` or the year: `Elyahu, Y., Hekselman, I., ...`
/// (an author list too long for its year to fit on the first line).
fn author_list_signature_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^\s*(?:(?:van|von|de|der|den|del|di|da|la|le|du)\s+)*\p{Lu}[\p{L}'’\-]+(?:\s\p{Lu}[\p{L}'’\-]+)*,\s?\p{Lu}\.(?:\s?-?\p{L}\.)*,\s(?:(?:&|and)\s)?(?:(?:van|von|de|der|den|del|di|da|la|le|du)\s+)*\p{Lu}[\p{L}'’\-]+(?:\s\p{Lu}[\p{L}'’\-]+)*,\s?\p{Lu}\.(?:\s?-?\p{L}\.)*(?:,|\s&|\sand\b|\s\()",
        )
        .expect("valid regex")
    })
}

/// An organisation or name list closed by a period and followed by the
/// year sentence (ACL / AAAI): `Alibaba Cloud Qwen Team. 2025a.`,
/// `Anthropic. 2025b. Claude 4 Sonnet`, `(Maxwell-Jia), M. J. 2024.`,
/// `xAI (Elon Musk’s AI Company). 2025.`.
fn organisation_start_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^\s*(?:\p{Lu}|\p{Ll}+\p{Lu}|\(\p{Lu})[^.]{1,80}\.(?:\s\p{Lu}\.)*\s(?:19|20)\d{2}[a-z]?\.(?:\s|$)",
        )
        .expect("valid regex")
    })
}

/// An organisation opening a semicolon-separated author list (AAAI):
/// `DeepSeek-AI; Liu, A.; Feng, B.;`.
fn organisation_list_start_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^\s*\p{Lu}[\p{L}\d\-]*(?:\s\p{Lu}[\p{L}\d\-]*){0,3};\s(?:(?:van|von|de|der|den|del|di|da|la|le|du)\s+)*\p{Lu}[\p{L}'’\-]+,\s?\p{Lu}\.(?:\s?-?\p{Lu}\.)*;",
        )
        .expect("valid regex")
    })
}

/// Does `text` end a sentence: a final period (after any trailing
/// replacement characters, `�`) that closes a word rather than an initial or
/// abbreviation?
fn ends_sentence(text: &str) -> bool {
    let trimmed = text.trim_end_matches(|c: char| c.is_whitespace() || c == '\u{FFFD}');
    trimmed
        .strip_suffix('.')
        .is_some_and(|head| !period_is_abbreviation(trimmed, head.len()))
}

/// Split the author-year list `lines` (already cut at the end heading) into
/// entries, from the section's hanging-indent levels when it has them and
/// from the name pattern otherwise. A line that opens with a surname-first
/// author list and its year ([`author_year_signature_re`],
/// [`author_list_signature_re`]) starts an entry after a sentence end
/// whatever the layout says (double-spaced lists, pages whose margins
/// differ).
fn segment_author_year(lines: &[SectionLine], context: &str) -> Vec<ReferenceEntry> {
    let layout = layout_starts(lines);
    let mut entries: Vec<ReferenceEntry> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let can_start = entry_start_re().is_match(&line.text);
        // A lowercase handle (`nostalgebraist. 2020. Title`) opens an entry
        // when the entry before it is complete.
        let handle = handle_start_re().is_match(&line.text)
            && entries
                .last()
                .is_some_and(|e| e.raw.trim_end().ends_with('.'));
        let signature = author_year_signature_re().is_match(&line.text)
            || author_list_signature_re().is_match(&line.text);
        let sentence_done = entries.last().is_some_and(|e| ends_sentence(&e.raw));
        // `DeepSeek-AI; Liu, A.; …` opens an entry even after a line that
        // lost its final period (`Achieves 72.6`), unless the line before
        // leaves an author list open.
        let organisation_list = organisation_list_start_re().is_match(&line.text)
            && entries.last().is_some_and(|e| {
                let raw = e.raw.trim_end();
                !raw.ends_with([';', ',', '&']) && !raw.ends_with(" and")
            });
        let starts = entries.is_empty()
            || (signature && sentence_done)
            || organisation_list
            || match layout[i] {
                Some(true) => can_start || handle,
                Some(false) => false,
                None => {
                    handle
                        || (can_start
                            && author_start_re().is_match(&line.text)
                            && entries.last().is_some_and(|e| ends_like_entry(&e.raw)))
                        || ((initials_start_re().is_match(&line.text)
                            || organisation_start_re().is_match(&line.text))
                            && entries
                                .last()
                                .is_some_and(|e| ends_like_whole_entry(&e.raw)))
                }
            };
        if starts {
            push_entry(&mut entries, None, line);
        } else {
            append_continuation(&mut entries, &line.text, context);
        }
    }
    let mut entries = apply_evidence_guard(entries, context);
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

/// Can `token` be the rest of a DOI or URL that a line wrap split off
/// (`6040.`, `BF01504345.`, `forum?id=abc`)? A bare year, a word, or
/// anything that starts a new URL or parenthesis cannot.
fn wrapped_id_token(token: &str, allow_no_digit: bool) -> bool {
    let core = token.trim_end_matches(['.', ',', ';', ')']);
    if core.is_empty() || core.chars().count() > MAX_WRAP_TOKEN || core.starts_with('(') {
        return false;
    }
    let has_digit = core.chars().any(|c| c.is_ascii_digit());
    let url_shaped = allow_no_digit && core.contains(['/', '.', '=', '_', '-']);
    if !has_digit && !url_shaped {
        return false;
    }
    !year_token_re().is_match(core) && !core.to_ascii_lowercase().starts_with("http")
}

/// A page number printed after a DOI's closing period by hyperref's
/// `backref` (`039. 4`, `3639. 2, 3, 8`): digits only, no leading zero,
/// fewer than four digits. A wrapped piece of the DOI itself (`005`,
/// `00045`, `112670`, `2023.2`) is not one.
fn is_back_reference(token: &str) -> bool {
    let core = token.trim_end_matches(['.', ',', ';', ')']);
    !core.is_empty()
        && core.chars().all(|c| c.is_ascii_digit())
        && !core.starts_with('0')
        && core.chars().count() < 4
}

/// End of the identifier that starts at `start` and reaches `end` so far,
/// extended across the single spaces that line wraps leave inside it. A
/// piece is joined when the identifier so far ends in a separator, when the
/// piece starts with a digit, or when `lenient` (a `doi.org/` URL) — and the
/// piece itself looks like identifier text ([`wrapped_id_token`], which
/// accepts digit-free pieces only when `url_shaped` is allowed).
fn extend_across_wraps(
    text: &str,
    start: usize,
    end: usize,
    lenient: bool,
    url_shaped: bool,
) -> usize {
    let mut end = end;
    loop {
        end = text[end..]
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '<' | '>'))
            .map_or(text.len(), |rel| end + rel);
        let Some(rest) = text[end..].strip_prefix(' ') else {
            break;
        };
        let Some(token) = rest.split_whitespace().next() else {
            break;
        };
        if rest.starts_with(' ') {
            break;
        }
        let consumed = &text[start..end];
        if consumed.ends_with([',', ';']) {
            break;
        }
        // `doi: 10.1016/j.jcp.2017.08.039. 4`: a back-reference page list
        // after the DOI's closing period, not a wrapped piece of it.
        if consumed.ends_with('.') && is_back_reference(token) {
            break;
        }
        let joinable = lenient
            || consumed.ends_with(['/', '.', '-', '_', '(', ')', ':', '=', '&', '?'])
            || token.starts_with(|c: char| c.is_ascii_digit());
        if !joinable || !wrapped_id_token(token, url_shaped) {
            break;
        }
        end += 1;
    }
    end
}

/// First DOI with its byte range in `text`. Line wraps inside the DOI
/// (`10.1007/ BF01504345`, `10. 1145/3292500`, `364399 1.3648400` after
/// `doi.org/`) are closed up.
fn find_doi(text: &str) -> Option<(Range<usize>, String)> {
    let found = doi_start_re().find(text)?;
    let start = found.start();
    let lenient = text[..start].to_ascii_lowercase().ends_with("doi.org/");
    let end = extend_across_wraps(text, start, found.end(), lenient, false);
    let joined: String = text[start..end]
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let trimmed = trim_trailing_punct(&joined);
    let suffix_ok = trimmed
        .rsplit('/')
        .next()
        .is_some_and(|s| s.chars().any(char::is_alphanumeric));
    if trimmed.len() < 8 || !suffix_ok {
        return None;
    }
    let tail = joined.len() - trimmed.len();
    Some((start..end - tail, trimmed.to_string()))
}

/// First arXiv identifier (after `arXiv:` or `abs/`) with its byte range.
fn find_arxiv(text: &str) -> Option<(Range<usize>, String)> {
    let caps = arxiv_re().captures(text)?;
    let whole = caps.get(0)?;
    let id = caps.get(1)?;
    Some((whole.range(), id.as_str().to_string()))
}

/// First URL with its byte range, closed up across line wraps
/// (`https://openreview.net/ forum?id=abc`).
fn find_url(text: &str) -> Option<(Range<usize>, String)> {
    let found = url_re().find(text)?;
    let start = found.start();
    let end = extend_across_wraps(text, start, found.end(), false, true);
    let joined: String = text[start..end]
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let trimmed = trim_trailing_punct(&joined);
    let tail = joined.len() - trimmed.len();
    Some((start..end - tail, trimmed.to_string()))
}

/// Year in `text`: a parenthesised `(2020)` first, else the first bare
/// `19xx`/`20xx` not glued to a page range. Returns the byte range of the
/// four digits and the value.
fn find_year(text: &str) -> Option<(Range<usize>, u16)> {
    let digits = if let Some(caps) = year_paren_re().captures(text) {
        caps.get(1)?
    } else {
        // A year with a dashed suffix letter (`1988–a`) counts where it
        // stands; the bare-year pattern skips years glued to a dash.
        let bare = year_bare_re().captures(text).and_then(|caps| caps.get(1));
        let dashed = year_dash_suffix_re()
            .captures(text)
            .and_then(|caps| caps.get(1));
        [bare, dashed]
            .into_iter()
            .flatten()
            .min_by_key(regex::Match::start)?
    };
    let year: u16 = digits.as_str().parse().ok()?;
    Some((digits.range(), year))
}

/// A year with a dashed suffix letter: `1988–a`, `2020-b` (NCBI entries of
/// one author and year). Group 1 is the year.
fn year_dash_suffix_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?:^|[^\d–\-—])((?:19|20)\d{2})[–\-][a-z](?:[^\p{L}\d]|$)")
            .expect("valid regex")
    })
}

/// Byte range and content of the first quoted title `“...”` or `"..."`.
/// The closing quote must match the opening one, so an apostrophe inside
/// `“developers’ conversations”` does not end the title. A short quotation
/// that runs straight on into lowercase text (`"collaborating" with ai`) is
/// part of an unquoted title, not a quoted title.
fn find_quoted(text: &str) -> Option<(Range<usize>, String)> {
    let open = text.find(['“', '"', '„', '‘'])?;
    let open_char = text[open..].chars().next()?;
    let closers: &[char] = match open_char {
        '“' => &['”', '"'],
        '"' => &['"', '”'],
        '„' => &['“', '”', '"'],
        _ => &['’'],
    };
    let inner_start = open + open_char.len_utf8();
    let close_rel = text[inner_start..].find(closers)?;
    let close = inner_start + close_rel;
    let close_len = text[close..].chars().next().map_or(1, char::len_utf8);
    let raw_inner = text[inner_start..close].trim();
    let inner = raw_inner.trim_end_matches([',', '.', ';']).trim();
    if inner.is_empty() {
        return None;
    }
    let unpunctuated = raw_inner == raw_inner.trim_end_matches([',', '.', ';', '?', '!']);
    let after = text[close + close_len..].trim_start();
    if unpunctuated && after.starts_with(|c: char| c.is_lowercase()) {
        return None;
    }
    // A quoted phrase that a subtitle follows after `:` or a dash opens an
    // unquoted title: `“It’s Kind of Context Dependent”: Understanding …`,
    // `"what’s happening"- a human-centered …`.
    if unpunctuated && quoted_phrase_subtitle(&text[close + close_len..]) {
        return None;
    }
    Some((open..close + close_len, inner.to_string()))
}

/// Does `after`, the text right after a closing quote, go on with a
/// subtitle: `:` or a dash, a space, then a word that does not open a
/// venue (`”: In Proceedings` does not)?
fn quoted_phrase_subtitle(after: &str) -> bool {
    let Some(rest) = after.strip_prefix([':', '-', '–', '—']) else {
        return false;
    };
    let words = rest.trim_start();
    words.len() < rest.len()
        && words.starts_with(char::is_alphabetic)
        && !venue_lead_re().is_match(words)
}

/// Alphanumeric run that ends right before byte `end`.
fn word_before(text: &str, end: usize) -> &str {
    let head = &text[..end];
    let start = head
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_alphanumeric())
        .last()
        .map_or(end, |(i, _)| i);
    &head[start..]
}

/// Is the period at byte `dot` the end of an initial or an abbreviation
/// (`A.`, `Jr.`, `St.`, `al.`, `vs.`, `pp.`) rather than a sentence end? `4o.` and
/// `4.3.` are sentence ends: a lone digit is not an initial. Neither is the
/// `t` of `Shouldn’t.`: a letter after an apostrophe belongs to its word.
fn period_is_abbreviation(text: &str, dot: usize) -> bool {
    let word = word_before(text, dot);
    if text[..dot - word.len()].ends_with(['’', '\'']) {
        return false;
    }
    let mut chars = word.chars();
    let single_letter =
        matches!((chars.next(), chars.next()), (Some(c), None) if c.is_alphabetic());
    single_letter
        || matches!(
            word.to_ascii_lowercase().as_str(),
            "jr" | "sr"
                | "st"
                | "al"
                | "eds"
                | "ed"
                | "vs"
                | "pp"
                | "vol"
                | "no"
                | "ch"
                | "cf"
                | "fig"
        )
}

/// Does `segment`, the text after an initial's period in what should be an
/// author list, read as a title instead? A colon, a lowercase word of four
/// or more letters (`Slayer: Spike layer error reassignment in time`, `On
/// general minimax theorems`) or five or more capitalised words without a
/// comma (`Structured State Space Model Dynamics and ...`) do; names,
/// surname particles of any length (`della`, [`is_surname_particle`]),
/// `and` and `et al` do not.
fn reads_as_title(segment: &str) -> bool {
    let text = segment.trim();
    if text.contains(':') {
        return true;
    }
    let words: Vec<&str> = text.split_whitespace().collect();
    let capitalised = words
        .iter()
        .filter(|word| {
            let core = word.trim_matches(|c: char| !c.is_alphanumeric());
            core.chars().count() >= 2
                && core.starts_with(char::is_uppercase)
                && !initials_re().is_match(core)
        })
        .count();
    if capitalised >= 5 && !text.contains(',') {
        return true;
    }
    words.iter().any(|word| {
        let core = word.trim_matches(|c: char| !c.is_alphanumeric());
        core.chars().count() >= 4
            && core.chars().all(char::is_alphabetic)
            && core.starts_with(char::is_lowercase)
            && core != "others"
            && !is_particle(core)
            && !is_surname_particle(core)
    })
}

/// A lowercase surname particle of any length (`della Porta`, `van der
/// Berg`, `bin Salman`): part of a name, never a title word.
fn is_surname_particle(token: &str) -> bool {
    matches!(
        token,
        "della"
            | "delle"
            | "dalla"
            | "degli"
            | "van"
            | "von"
            | "der"
            | "den"
            | "de"
            | "la"
            | "los"
            | "las"
            | "du"
            | "des"
            | "ter"
            | "ten"
            | "da"
            | "di"
            | "do"
            | "dos"
            | "das"
            | "del"
            | "dela"
            | "le"
            | "lo"
            | "bin"
            | "ibn"
            | "al"
            | "el"
            | "af"
            | "av"
            | "zu"
            | "zur"
            | "y"
            | "e"
    )
}

/// True when `segment` reads as an author list only: every `. ` inside it
/// closes an initial or abbreviation, and no text after an initial reads as
/// a title ([`reads_as_title`]): in `Orchard, G. Slayer: Spike layer error
/// reassignment in time (2018)` the year follows the title, not the
/// authors. The period of a dotted acronym (`U.S. Food and Drug
/// Administration`) is not an initial's.
fn is_author_only(segment: &str) -> bool {
    let trimmed = segment.trim_end();
    let trimmed = trimmed.trim_end_matches(['.', ',', '(', ' ']);
    if trimmed.is_empty() || !trimmed.chars().next().is_some_and(char::is_uppercase) {
        return false;
    }
    let mut search = 0usize;
    while let Some(rel) = trimmed[search..].find(". ") {
        let dot = search + rel;
        if !period_is_abbreviation(trimmed, dot) {
            return false;
        }
        let acronym = trimmed[..dot - word_before(trimmed, dot).len()].ends_with('.');
        let next = trimmed[dot + 2..]
            .find(". ")
            .map_or(trimmed.len(), |found| dot + 2 + found);
        if !acronym && reads_as_title(&trimmed[dot + 2..next]) {
            return false;
        }
        search = dot + 2;
    }
    // Long lists (fifty authors in IEEE style) stay author lists.
    trimmed.chars().count() <= 1200
}

/// Is `text` a surname-first author list ([`surname_first_re`])? A list
/// whose first author has a full given name and whose second is
/// initials-first (`Monika Mudgel, V. P. S. Awana, R. Lal`) is not, though
/// it opens like `Surname Surname, I.`.
fn surname_first_list(text: &str) -> bool {
    surname_first_re().is_match(text) && !full_name_then_initials(text)
}

/// Does `text` open with a full name of two or more capitalised words and go
/// on with an initials-first name (`Monika Mudgel, V. P. S. Awana, …`)?
fn full_name_then_initials(text: &str) -> bool {
    let mut parts = text.splitn(3, ", ");
    let (Some(first), Some(second)) = (parts.next(), parts.next()) else {
        return false;
    };
    let names: Vec<&str> = first.split_whitespace().collect();
    let tokens: Vec<&str> = second.split_whitespace().collect();
    names.len() >= 2
        && names.iter().all(|t| cap_word_re().is_match(t))
        && tokens.len() >= 2
        && initial_token_re().is_match(tokens[0])
        && tokens.last().is_some_and(|t| {
            let word = t.trim_end_matches([',', '.']);
            cap_word_re().is_match(word) && word.chars().skip(1).any(char::is_lowercase)
        })
}

/// Byte offset just past the terminator (`. `, `? `, `! `) that ends the
/// author list when it is followed by the title, honouring initials.
fn author_terminator(body: &str) -> Option<usize> {
    let vancouver = vancouver_start_re().is_match(body);
    let surname_first = surname_first_list(body);
    let mut search = 0usize;
    loop {
        let rel = body[search..].find(['.', '?', '!'])?;
        let pos = search + rel;
        search = pos + 1;
        if !body[pos + 1..].starts_with(' ') {
            continue;
        }
        // `Md. Abdul Aziz`: an abbreviated given name inside the list.
        let abbreviation = body.as_bytes()[pos] == b'.'
            && (period_is_abbreviation(body, pos)
                || matches!(word_before(body, pos), "Md" | "Mohd" | "Muhd"));
        if !abbreviation || vancouver {
            return Some(pos + 1);
        }
        // `et al.` ends the list unless more names follow; in surname-first
        // style an initial does too (`Smith, A. Title`).
        let et_al = word_before(body, pos).eq_ignore_ascii_case("al");
        if (et_al || surname_first) && !continues_author_list(body, pos + 1) {
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
    // A lowercase initial is an abbreviated particle (`Casas, D. d. l.,`).
    let mut chars = word.chars();
    if matches!(
        (chars.next(), chars.next(), chars.next()),
        (Some(c), Some('.'), None) if c.is_lowercase()
    ) {
        return true;
    }
    // A capital without a period before a lowercase word opens the title
    // (`Kroer, C. A unified approach`), not another initial.
    if !word.contains('.') && after_word.starts_with(char::is_lowercase) {
        return false;
    }
    // A capitalised run of four or more letters without periods opens the
    // title (`Weiss, G. WISDM Smartphone and …`): no block of initials.
    if !word.contains('.') && word.chars().count() >= 4 {
        return false;
    }
    is_initials(word)
}

/// A part marker after a title sentence: ` II.`, ` IV.`, ` Part 2.`,
/// ` Part II.` (the period closes the marker).
fn part_marker_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^\s(?:Part\s+(?:[IVX]{1,4}|\d{1,2})|[IVX]{1,4})\.").expect("valid regex")
    })
}

/// End (byte offset, exclusive) of a title that starts at byte 0 of `text`:
/// the first `. ` that does not close an abbreviation, or a `? ` / `! `
/// after which the title does not go on ([`question_ends_title`]). A part
/// marker after the period stays in the title (`Orthogonal polynomials.
/// II. J. Math. Phys.` ends after `II`), and so does a `?` / `!` that venue
/// words follow (`Resolved? arXiv preprint` ends after the `?`).
fn title_end(text: &str) -> usize {
    let mut search = 0usize;
    while let Some(rel) = text[search..].find(['.', '?', '!']) {
        let pos = search + rel;
        if text[pos + 1..].starts_with(' ') {
            let period = text.as_bytes()[pos] == b'.';
            if !period && venue_lead_re().is_match(text[pos + 2..].trim_start()) {
                return pos + 1;
            }
            let terminal = if period {
                title_period_ends(text, pos)
            } else {
                question_ends_title(&text[pos + 2..])
            };
            if terminal {
                if period && let Some(marker) = part_marker_re().find(&text[pos + 1..]) {
                    return pos + marker.end();
                }
                // Sentence-separating periods are omitted, but question and
                // exclamation marks are part of the printed title.
                return pos + usize::from(!period);
            }
        }
        search = pos + 1;
    }
    text.len()
}

/// Does the period at byte `dot` end a title? A period that closes an
/// initial or abbreviation does not ([`period_is_abbreviation`], and `Dr.`
/// or `Prof.`), unless the single capital before it is a unit after a
/// number (`above 40 K. Supercond. Sci.`) or a word before a journal name
/// (`models in R. Journal of Open Source Software`). A period before a
/// lowercase clause of the title does not end it either ([`title_goes_on`]).
fn title_period_ends(text: &str, dot: usize) -> bool {
    let word = word_before(text, dot);
    if period_is_abbreviation(text, dot) {
        return capital_ends_title(text, dot, word);
    }
    if matches!(
        word.to_ascii_lowercase().as_str(),
        "dr" | "mr" | "mrs" | "ms" | "prof"
    ) {
        return false;
    }
    !title_goes_on(&text[dot + 1..])
}

/// Is the single capital `word` before the period at byte `dot` a word of
/// the title rather than an initial: a unit after a number (`40 K.`) or a
/// one-letter name after a lowercase word and before a journal name (`in
/// R. Journal`)?
fn capital_ends_title(text: &str, dot: usize, word: &str) -> bool {
    let mut chars = word.chars();
    let capital = matches!((chars.next(), chars.next()), (Some(c), None) if c.is_uppercase());
    if !capital {
        return false;
    }
    let Some(head) = text[..dot - word.len()].strip_suffix(' ') else {
        return false;
    };
    let previous = head.rsplit(' ').next().unwrap_or("");
    let unit = !previous.is_empty() && previous.chars().all(|c| c.is_ascii_digit());
    let named = previous.chars().count() >= 2
        && previous.chars().all(char::is_lowercase)
        && journal_name_re().is_match(&text[dot + 1..]);
    unit || named
}

/// A journal name right after a period: ` Journal`, ` Proceedings`,
/// ` Transactions`, ` Advances`, ` IEEE`, ` ACM`.
fn journal_name_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^\s(?:Journal|Proceedings|Transactions|Advances|IEEE|ACM)\b")
            .expect("valid regex")
    })
}

/// A lowercase Roman numeral with its period (`iii. `), a part marker
/// inside a lowercased title.
fn lower_roman_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[ivx]{1,4}\.\s").expect("valid regex"))
}

/// A note that follows a title in lowercase (`unpublished manuscript`,
/// `under review`, `in press`, `and`, `et al`): not part of the title.
fn title_note_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^(?:unpublished|manuscript|mimeo|working\s+paper|thesis|dissertation|report|lecture|slides|blog|software|dataset|online|forthcoming|under\s+review|in\s+press|and|et\s+al)\b",
        )
        .expect("valid regex")
    })
}

/// Venue words in any case (`ieee transactions on …`).
fn lower_venue_word_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)\b(?:journal|proceedings|transactions|conference|letters|review|annals|advances|workshop|symposium|press|ieee|acm)\b",
        )
        .expect("valid regex")
    })
}

/// Does the title go on after a sentence period, `after` being the text that
/// follows the period? A lowercase Roman numeral (`Graph minors. iii.
/// planar`) or a lowercase clause of two or more words without digits
/// (`attribution. from black magic to theory?`, `by the way.. . the closing
/// moments`) does. A venue lead (`arXiv`, `in`, `doi`), a note
/// ([`title_note_re`]), a camel-case journal (`bioRxiv`, `eLife`), `npj`, a
/// clause with venue words (`ieee transactions on …`) and a one-word or
/// numbered clause (`nature, 529`, `eneuro 12`) do not.
fn title_goes_on(after: &str) -> bool {
    // An identifier masked to spaces follows the period (`Suite.
    // https://www.ibm.com/products/ maximo Accessed: …`): the title ends
    // there, whatever text trails the identifier.
    if after.starts_with("  ") {
        return false;
    }
    let rest = after.trim_start_matches(['.', ' ']);
    if lower_roman_re().is_match(rest) {
        return true;
    }
    let Some(first) = rest.split_whitespace().next() else {
        return false;
    };
    if !first.starts_with(char::is_lowercase)
        || first.chars().skip(1).any(char::is_uppercase)
        || first.starts_with("npj")
        || venue_lead_re().is_match(rest)
        || title_note_re().is_match(rest)
    {
        return false;
    }
    let clause = rest
        .find([',', '.', ';', ':', '?', '!', '('])
        .map_or(rest, |end| &rest[..end]);
    clause.split_whitespace().count() >= 2
        && !clause.chars().any(|c| c.is_ascii_digit())
        && !lower_venue_word_re().is_match(clause)
}

/// Byte offset of the first `. `, `? ` or `! ` in `text` that ends a
/// sentence; a period that closes an initial or abbreviation does not.
fn sentence_end(text: &str) -> Option<usize> {
    let mut search = 0usize;
    while let Some(rel) = text[search..].find(['.', '?', '!']) {
        let pos = search + rel;
        if text[pos + 1..].starts_with(' ')
            && (text.as_bytes()[pos] != b'.' || !period_is_abbreviation(text, pos))
        {
            return Some(pos);
        }
        search = pos + 1;
    }
    None
}

/// Venue words that may follow a subtitle after a `? ` / `! ` title part.
fn subtitle_venue_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^(?i:in\b|proceedings\b|proc\.|arxiv|preprint\b)").expect("valid regex")
    })
}

/// A volume, issue or page range: `43(4)`, `106, 3`, `709–722`.
fn volume_digits_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\d\s*[,(:]|\d[–\-]\d").expect("valid regex"))
}

/// Venue words inside a clause after a `? ` / `! ` (`Journal of Artificial
/// Intelligence`): the clause names the venue, not a subtitle.
fn clause_venue_word_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"\b(?:Journal|Proceedings|Conference|Transactions|Review|Letters|Annals|Advances|Workshop|Symposium|Press|(?i:arxiv|preprint|vol|pp))\b",
        )
        .expect("valid regex")
    })
}

/// Does `clause` read as a venue name rather than a subtitle? It holds a
/// venue word ([`clause_venue_word_re`]) and does not open with an article
/// (`A Systematic Review` is a subtitle).
fn clause_names_venue(clause: &str) -> bool {
    let first = clause.split_whitespace().next().unwrap_or("");
    !matches!(first, "A" | "An") && clause_venue_word_re().is_match(clause)
}

/// Does a title end at a `? ` or `! ` that `after` follows (venue words
/// right after it are handled by [`title_end`])? It ends before a journal
/// with its volume (`preferences? Marketing Science 43(4):709–722.`) and
/// before a short or venue-like sentence. It goes on
/// before a lowercase word (`negotiate? negotiationarena platform`) and
/// before a subtitle of three or more words that is followed by a venue
/// (`Search? Investigating Large Language Models as Re-Ranking Agents. In
/// Proceedings`) or by a masked identifier (`Tests? An Empirical Study.
/// arXiv:2602.00409`). A masked identifier alone is not enough: a clause
/// with venue words (`Work? Journal of Artificial Intelligence. https://…`)
/// or followed by a volume is the venue, and the title ends.
fn question_ends_title(after: &str) -> bool {
    if after.trim_start().starts_with(char::is_lowercase) {
        return false;
    }
    let Some(end) = sentence_end(after) else {
        return true;
    };
    let subtitle = &after[..end];
    if subtitle.split_whitespace().count() < 3
        || venue_lead_re().is_match(subtitle.trim_start())
        || volume_digits_re().is_match(subtitle)
    {
        return true;
    }
    let following = &after[end + 1..];
    if subtitle_venue_re().is_match(following.trim_start()) {
        return false;
    }
    // An identifier masked to spaces (`Study. arXiv:2602.00409`) follows.
    let masked_identifier = following.starts_with("  ");
    if !masked_identifier || subtitle.chars().any(|c| c.is_ascii_digit()) {
        return true;
    }
    // A volume after the identifier (`… (2021), 12(3).`) marks the clause
    // as the venue; a bare year (`arXiv:2602.00409 (2026).`) does not.
    let tail = following.trim_start();
    let tail = &tail[..sentence_end(tail).unwrap_or(tail.len())];
    clause_names_venue(subtitle) || volume_digits_re().is_match(tail)
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
    let surname_first = surname_first_list(cleaned);
    let mut names: Vec<String> = Vec::new();
    for part in author_sep_re().split(cleaned) {
        let part = strip_editor_tag(part.trim().trim_matches(',').trim()).trim();
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
        // Surname-first names without a separator (`Rossi, M. della Porta,
        // A.`): the leading initials close the previous name and the rest
        // opens the next one.
        if surname_first
            && names.last().is_some_and(|last| !last.contains(','))
            && let Some((initials, rest)) = split_leading_initials(part)
        {
            if let Some(last) = names.last_mut() {
                last.push_str(", ");
                last.push_str(initials);
            }
            names.push(rest.to_string());
            continue;
        }
        if looks_like_name(part) {
            names.push(part.to_string());
        }
    }
    names
}

/// `M. della Porta` → (`M.`, `della Porta`): a leading block of initials
/// with periods, then a surname that starts with a capital or a surname
/// particle. `None` when either piece is missing.
fn split_leading_initials(part: &str) -> Option<(&str, &str)> {
    let mut end = 0usize;
    let mut rest = part;
    loop {
        let trimmed = rest.trim_start();
        let token = trimmed.split_whitespace().next()?;
        if !token.ends_with('.') || !is_initials(token) {
            break;
        }
        end = part.len() - trimmed.len() + token.len();
        rest = &trimmed[token.len()..];
    }
    let rest = rest.trim();
    let first = rest.split_whitespace().next()?;
    let surname = first.starts_with(char::is_uppercase) || is_surname_particle(first);
    if end == 0 || !surname || is_initials(rest) || !looks_like_name(rest) {
        return None;
    }
    Some((part[..end].trim(), rest))
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
        || matches!(lower.as_str(), "p" | "pp" | "vol" | "no" | "in")
    {
        return None;
    }
    Some(collapsed)
}

/// Venue from the text that follows the title.
fn parse_venue(rest: &str) -> Option<String> {
    let rest = rest.trim_start_matches(|c: char| c == ',' || c == '.' || c.is_whitespace());
    // A book in a series: `volume 375 of Mathematics and Its Applications.
    // Kluwer` names the series as the venue; a bare `volume 48.` is skipped.
    if let Some(caps) = series_volume_re().captures(rest)
        && let Some(series) = caps.get(1)
    {
        return clean_venue(series.as_str());
    }
    let rest = bare_volume_lead_re()
        .find(rest)
        .map_or(rest, |lead| &rest[lead.end()..]);
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
    // Elsevier / SIAM: `35 (1992) 61–70`, `20 (2) (1963) 130–141`,
    // `22 (2022), pp. 35–76` — the year digits are already blanked. The end
    // of a page range before the year (`pp. 41–49 (2025)`) is not a volume.
    if volume.is_none()
        && let Some(caps) = vol_before_year_re().captures(rest)
        && let Some(vol) = caps.get(1)
        && !rest[..vol.start()].trim_end().ends_with(['–', '-', '—'])
    {
        volume = caps.get(1).map(|m| m.as_str().to_string());
        if issue.is_none() {
            issue = caps.get(2).map(|m| m.as_str().to_string());
        }
        if pages.is_none()
            && let (Some(a), Some(b)) = (caps.get(3), caps.get(4))
        {
            pages = Some(dash_range(a.as_str(), Some(b.as_str())));
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

/// Every year in `text` (parenthesised or bare) with the byte range of its
/// digits, in order of position.
fn all_years(text: &str) -> Vec<(Range<usize>, u16)> {
    let mut years: Vec<(Range<usize>, u16)> = Vec::new();
    for caps in year_paren_re()
        .captures_iter(text)
        .chain(year_bare_re().captures_iter(text))
        .chain(year_dash_suffix_re().captures_iter(text))
    {
        if let Some(digits) = caps.get(1)
            && let Ok(value) = digits.as_str().parse::<u16>()
        {
            years.push((digits.range(), value));
        }
    }
    years.sort_by_key(|(range, _)| range.start);
    years.dedup_by_key(|(range, _)| range.start);
    years
}

fn is_particle(token: &str) -> bool {
    matches!(
        token,
        "van" | "von" | "de" | "der" | "den" | "del" | "di" | "da" | "la" | "le" | "du" | "y"
    )
}

/// A single lowercase letter with a period: an abbreviated particle (`v.`
/// for `van`, `d.` for `de`) or an initial whose capital was lost.
fn is_lowercase_initial(token: &str) -> bool {
    let mut chars = token.chars();
    matches!(
        (chars.next(), chars.next(), chars.next()),
        (Some(c), Some('.'), None) if c.is_lowercase()
    )
}

/// Loose test for an author name in an IEEE list that ends at a quoted
/// title: at most six tokens, no digits, every token capitalised, an
/// initial, a particle or an elided particle (`d'Aspremont`), and at least
/// one initial (`A.`, `v.`, `J.-M.`).
fn loose_name_part(part: &str) -> bool {
    let mut text = part.trim();
    if let Some(rest) = text
        .strip_prefix("and ")
        .or_else(|| text.strip_prefix("& "))
    {
        text = rest.trim_start();
    }
    let tokens: Vec<&str> = text.split_whitespace().collect();
    if tokens.is_empty() || tokens.len() > 6 || text.chars().any(|c| c.is_ascii_digit()) {
        return false;
    }
    let mut has_initial = false;
    for token in &tokens {
        let initial = token.ends_with('.') && token.chars().count() <= 5;
        let first = token.chars().next().unwrap_or(' ');
        let second = token.chars().nth(1).unwrap_or(' ');
        let elided = first.is_lowercase() && matches!(second, '\'' | '’');
        if !(initial || first.is_uppercase() || is_particle(token) || elided) {
            return false;
        }
        has_initial |= initial;
    }
    has_initial
}

/// `x- y` (a word broken at a line end) closed up, for classifying a
/// fragment; the text itself is left as printed.
fn close_word_breaks(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find("- ") {
        let before_is_letter = rest[..pos]
            .chars()
            .next_back()
            .is_some_and(char::is_alphabetic);
        let after_is_lower = rest[pos + 2..].starts_with(|c: char| c.is_lowercase());
        out.push_str(&rest[..pos]);
        if !(before_is_letter && after_is_lower) {
            out.push_str("- ");
        }
        rest = &rest[pos + 2..];
    }
    out.push_str(rest);
    out
}

/// Does a comma-delimited part read as one or two author names in an
/// initials-first style: `A. B. Smith`, `and C. Jones`, `Smith AB`,
/// `J. Doe and K. Roe`, `et al.`? `Nonlinear Systems` and `Cambridge
/// University Press` do not (no initials, not Vancouver).
fn is_name_part(part: &str) -> bool {
    let mut text = part.trim();
    if let Some(rest) = text
        .strip_prefix("and ")
        .or_else(|| text.strip_prefix("& "))
    {
        text = rest.trim_start();
    }
    if text.is_empty() {
        return false;
    }
    if is_et_al(text) || is_name_suffix(text) {
        return true;
    }
    if let Some(tail) = et_al_tail_re().find(text) {
        text = text[..tail.start()].trim_end();
    }
    let subs: Vec<&str> = and_split_re().split(text).collect();
    if subs.len() > 1 {
        return subs.len() <= 2 && subs.iter().all(|sub| is_name_part(sub));
    }
    let closed = close_word_breaks(text);
    let tokens: Vec<&str> = closed.split_whitespace().collect();
    if tokens.is_empty() || tokens.len() > 6 {
        return false;
    }
    let mut has_initial = false;
    let mut names = 0usize;
    for (position, token) in tokens.iter().enumerate() {
        // A lowercase initial opens a name whose accented capital was lost
        // (`c. Öztürk`); later it is an abbreviated particle (`A. v. Niekerk`,
        // `O. v. d. Heide`).
        let lowercase_initial = is_lowercase_initial(token);
        if initial_token_re().is_match(token) || (lowercase_initial && position == 0) {
            has_initial = true;
        } else if lowercase_initial || is_particle(token) || cap_word_re().is_match(token) {
            names += 1;
        } else {
            return false;
        }
    }
    if names == 0 {
        return false;
    }
    let vancouver = tokens.len() == 2
        && caps_block_re().is_match(tokens[1])
        && !caps_block_re().is_match(tokens[0]);
    has_initial || vancouver
}

/// Does the first comma-delimited part open an initials-first author list
/// (`A. Smith`, `A. B. Smith and C. Jones`, `Smith AB`)?
fn initials_first(part: &str) -> bool {
    let tokens: Vec<&str> = part.split_whitespace().collect();
    match tokens.as_slice() {
        [first, ..] if initial_token_re().is_match(first) => true,
        [surname, initials] => {
            cap_word_re().is_match(surname)
                && caps_block_re().is_match(initials)
                && !caps_block_re().is_match(surname)
        }
        _ => false,
    }
}

/// Does a comma-delimited part that follows the title open the venue
/// (`Communications of the ACM 35`, `in: Proceedings`, `pp. 3–9`, `2020`)?
/// A part starting in lowercase is title text unless it is a venue word.
fn venue_like(part: &str) -> bool {
    let text = part.trim_start();
    let Some(first) = text.chars().next() else {
        return true;
    };
    first.is_uppercase()
        || first.is_ascii_digit()
        || matches!(first, '“' | '"' | '„' | '‘' | '(' | '[')
        || venue_lead_re().is_match(text)
}

/// Byte ranges of the `, `-delimited parts of `text`.
fn comma_parts(text: &str) -> Vec<Range<usize>> {
    let mut parts: Vec<Range<usize>> = Vec::new();
    let mut start = 0usize;
    for (pos, _) in text.match_indices(", ") {
        parts.push(start..pos);
        start = pos + 2;
    }
    parts.push(start..text.len());
    parts
}

/// An editor tag after a name (`D. Kazakov (Eds.)`).
fn editor_tag_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\s*\((?i:eds?|editors?)\.?\)\s*$").expect("valid regex"))
}

/// `part` without a trailing editor tag ([`editor_tag_re`]).
fn strip_editor_tag(part: &str) -> &str {
    editor_tag_re()
        .find(part)
        .map_or(part, |tag| &part[..tag.start()])
}

/// Is `part` a one-word name (`Mausam`, `Vanhoucke`) inside an
/// initials-first author list: a capitalised word that is not an acronym,
/// followed by another name, by `and`, or by a title with a colon (`…, Q.
/// Vuong, Vanhoucke, Rt-2: Vision-language-action models …`)?
fn mononym_in_list(part: &str, next: &str) -> bool {
    let name = part.trim();
    if name.contains(char::is_whitespace)
        || name.chars().count() < 3
        || !cap_word_re().is_match(name)
        || !name.chars().skip(1).any(char::is_lowercase)
    {
        return false;
    }
    let next = next.trim_start();
    let lower = next.to_lowercase();
    lower.starts_with("and ")
        || lower.starts_with("& ")
        || is_name_part(next)
        || next[..title_end(next)].contains(':')
}

/// Does the list open with a one-word name (`Bhumika, D. Das, MARRS: …`):
/// a capitalised word, then an initials-first name with a surname, then a
/// part that is not a block of initials (so a surname-first `Lee, J. van
/// Berg, K.` does not)?
fn mononym_opens_list(masked: &str, parts: &[Range<usize>]) -> bool {
    if parts.len() < 3 {
        return false;
    }
    let name = masked[parts[0].clone()].trim();
    let second = &masked[parts[1].clone()];
    let third = masked[parts[2].clone()].trim();
    !name.contains(char::is_whitespace)
        && name.chars().count() >= 3
        && cap_word_re().is_match(name)
        && name.chars().skip(1).any(char::is_lowercase)
        && initials_first(second)
        && is_name_part(second)
        && !initials_re().is_match(third.trim_end_matches(['.', ',']))
}

/// A Vancouver name with a multi-word surname (`Van Roy B`): two to four
/// capitalised words, the last a block of up to three capitals.
fn vancouver_name(text: &str) -> bool {
    let tokens: Vec<&str> = text.split_whitespace().collect();
    let Some((initials, surname)) = tokens.split_last() else {
        return false;
    };
    (2..=4).contains(&tokens.len())
        && caps_block_re().is_match(initials)
        && surname
            .iter()
            .all(|t| cap_word_re().is_match(t) && !caps_block_re().is_match(t))
}

/// May a title that is so far one capitalised word, or whose subtitle after
/// a colon is one word (`Automated`, `You Only Look Once: Unified`), go on
/// over the comma part `part`? Only when the part reads as more title: it
/// opens with a capital and holds no venue lead, venue word, publisher word
/// or digit.
fn title_takes_part(title: &str, part: &str) -> bool {
    let title = title.trim();
    let last = title.rsplit(':').next().unwrap_or(title).trim();
    let one_word = !last.contains(char::is_whitespace) && cap_word_re().is_match(last);
    let clause = part.trim_start();
    let clause = clause[..title_end(clause)].trim_end();
    one_word
        && clause.starts_with(char::is_uppercase)
        && !venue_lead_re().is_match(clause)
        && !clause_venue_word_re().is_match(clause)
        && !publisher_word_re().is_match(clause)
        && !clause.chars().any(|c| c.is_ascii_digit())
}

/// Lower-case words that may join the words of an organisation name
/// (`National Center for Biotechnology Information`).
fn is_name_connector(token: &str) -> bool {
    matches!(
        token,
        "of" | "for" | "and" | "the" | "on" | "de" | "du" | "des" | "für" | "und"
    )
}

/// Does `text` read as an organisation or a one-word name standing for the
/// authors (`OpenAI`, `Qwen Team`, `3GPP`, `DeepSeek-AI`, `Chemical
/// Abstracts Service (CAS)`): one to six words that open with a capital, a
/// digit or `(` (or join them: `for`, `of`), no sentence period, and no
/// initials-first or Vancouver name?
fn organisation_name(text: &str) -> bool {
    let tokens: Vec<&str> = text.split_whitespace().collect();
    let Some(first) = tokens.first() else {
        return false;
    };
    (1..=6).contains(&tokens.len())
        && text.chars().count() <= 60
        && !text.ends_with('.')
        && !text.contains(". ")
        && first.starts_with(|c: char| c.is_uppercase() || c.is_ascii_digit())
        && !initial_token_re().is_match(first)
        && !initials_first(text)
        && !is_name_part(text)
        && tokens.iter().all(|t| {
            t.starts_with(|c: char| c.is_uppercase() || c.is_ascii_digit() || c == '(')
                || is_name_connector(t)
        })
}

/// An organisation or one-word author, a comma, then an unquoted title
/// (`Anthropic, System Card: Claude Sonnet 4.6, 2026, https://…`, `OpenAI,
/// Gpt-4 technical report, arXiv preprint …`, `Anonymous, A generalist
/// hanabi agent, in: Submitted to …`, `Elsevier, Reaxys, Database, 2025,
/// https://…`). The title runs over lowercase parts and needs positive
/// evidence after it: a bare year, a masked identifier, a venue lead, or a
/// one-word descriptor before a year (`Database, 2025`). A title that is a
/// bare name (`Hideo Bannai, Mitsuru Funakoshi, …`) or a list that goes on
/// with `and` is not one.
fn organisation_comma(masked: &str) -> Option<CommaSplit> {
    let parts = comma_parts(masked);
    if parts.len() < 3 || !organisation_name(masked[parts[0].clone()].trim()) {
        return None;
    }
    let range = parts[1].clone();
    let part = &masked[range.clone()];
    let title = part.trim();
    let tokens: Vec<&str> = title.split_whitespace().collect();
    let first = tokens.first()?;
    let bare_name =
        (2..=3).contains(&tokens.len()) && tokens.iter().all(|t| cap_word_re().is_match(t));
    if !first.starts_with(char::is_uppercase)
        || initial_token_re().is_match(first)
        || title.ends_with('.')
        || sentence_end(title).is_some()
        || bare_name
    {
        return None;
    }
    let title_start = range.start + (part.len() - part.trim_start().len());
    let is_year = |text: &str| year_token_re().is_match(text.trim_matches(['(', ')', '.', ' ']));
    let mut end = range.end;
    for (k, later) in parts.iter().enumerate().skip(2) {
        let text = masked[later.clone()].trim();
        if text.is_empty() || is_year(text) || venue_lead_re().is_match(text) {
            break;
        }
        if text.starts_with(|c: char| c.is_uppercase() || c.is_ascii_digit()) {
            let descriptor = !text.contains(char::is_whitespace)
                && cap_word_re().is_match(text.trim_end_matches('.'));
            let dated = parts
                .get(k + 1)
                .is_some_and(|next| is_year(&masked[next.clone()]));
            if descriptor && dated {
                break;
            }
            return None;
        }
        if text.starts_with("and ") || text.starts_with("& ") {
            return None;
        }
        end = later.end;
        if k + 1 == parts.len() {
            return None;
        }
    }
    Some(CommaSplit {
        authors_end: range.start,
        title_start,
        title_end: end,
    })
}

/// A journal with its volume, issue and parenthesised year opening a comma
/// part (Elsevier): `Physica A: Statistical Mechanics and its Applications
/// 467 (2017)`, `Ecological Economics 69 (4) (2010)`.
fn elsevier_journal_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^\s*\p{Lu}[^,]*?\s\d+\s*(?:\(\d+(?:[–\-]\d+)?\)\s*)?\((?:19|20)\d{2}\)")
            .expect("valid regex")
    })
}

/// Entries without authors whose first comma part is the title: Elsevier
/// (`Kinetic models of collective decision-making …, Physica A: … 467
/// (2017) 201–217.`: a part that reads as a title, then a journal with its
/// volume and parenthesised year) and web resources (`ChemRxiv,
/// https://chemrxiv.org/, Accessed: …`: a name, then a masked URL; a bare
/// two- or three-word name such as `Semantic Scholar` is taken for an
/// author and left alone). Returns the end of the title.
fn authorless_title(masked: &str) -> Option<usize> {
    let parts = comma_parts(masked);
    let (Some(head), Some(next)) = (parts.first(), parts.get(1)) else {
        return None;
    };
    let title = masked[head.clone()].trim();
    let first = title.split_whitespace().next()?;
    if !first.starts_with(char::is_alphabetic)
        || initial_token_re().is_match(first)
        || initials_first(title)
        || is_name_part(title)
        || sentence_end(title).is_some()
    {
        return None;
    }
    let after = &masked[next.clone()];
    let journal = title.starts_with(char::is_uppercase)
        && reads_as_title(title)
        && elsevier_journal_re().is_match(after);
    // A bare personal name before a URL (`John Smith, https://…`) is an
    // author, not a title.
    let words: Vec<&str> = title.split_whitespace().collect();
    let bare_name =
        (2..=3).contains(&words.len()) && words.iter().all(|w| cap_word_re().is_match(w));
    let web = title.chars().count() <= 100
        && !bare_name
        && (after.trim_matches(['.', ' ']).is_empty() || after.starts_with(' '));
    (journal || web).then_some(head.end)
}

/// Where a comma-delimited entry splits.
struct CommaSplit {
    /// End of the author list (the comma before the title).
    authors_end: usize,
    /// First byte of the title.
    title_start: usize,
    /// End of the title (exclusive).
    title_end: usize,
}

/// Elsevier, SIAM and IEEE-book entries put an unquoted title after the
/// comma that closes an initials-first author list:
/// `D. Goldberg, D. Nichols, Using collaborative filtering ..., Communications
/// of the ACM 35 (1992) 61–70.` The authors are the leading name-like parts;
/// the title runs to the first sentence end or the comma before a venue-like
/// part. `None` when the entry is not of this shape (a quoted title, a
/// sentence period after the authors, a year right after them).
fn comma_style(masked: &str) -> Option<CommaSplit> {
    let parts = comma_parts(masked);
    if parts.len() < 2 {
        return None;
    }
    let first = &masked[parts[0].clone()];
    let opens =
        (initials_first(first) && is_name_part(first)) || mononym_opens_list(masked, &parts);
    if !opens || title_end(first) < first.len() {
        return None;
    }
    let mut k = 1usize;
    while k < parts.len() {
        // `D. Kazakov (Eds.)`: an editor tag after a name.
        let part = strip_editor_tag(&masked[parts[k].clone()]);
        let next = parts.get(k + 1).map_or("", |r| &masked[r.clone()]);
        let named = is_name_part(part) || mononym_in_list(part, next);
        if title_end(part) < part.len() || !named {
            break;
        }
        k += 1;
    }
    if k >= parts.len() {
        return None;
    }
    let range = parts[k].clone();
    // IEEE: when a quoted title follows a comma and everything between the
    // last recognised name and the quote still reads as names (`A. v.
    // Niekerk`, `M. d'Aspremont`), the title starts at the quote.
    if let Some((quote, _)) = find_quoted(masked)
        && quote.start > range.start
        && masked[..quote.start].trim_end().ends_with(',')
    {
        let lead = masked[range.start..quote.start]
            .trim_end()
            .trim_end_matches(',');
        if comma_parts(lead)
            .into_iter()
            .all(|r| loose_name_part(&lead[r]))
        {
            return None;
        }
    }
    let part = &masked[range.clone()];
    let stripped = part.trim_start();
    let first_char = stripped.chars().next()?;
    // A quoted title opens the part (IEEE); a quoted phrase that the title
    // runs on from (`“Direct search” solution of …`) does not. The closing
    // quote may lie past later commas (`“Time-efficient, high-resolution
    // …,” Magnetic resonance in medicine`), so look for it in the rest of
    // the entry, not in this comma part alone.
    let part_start = range.start + (part.len() - stripped.len());
    if matches!(first_char, '“' | '"' | '„' | '‘')
        && find_quoted(&masked[part_start..]).is_some_and(|(quote, _)| quote.start == 0)
    {
        return None;
    }
    let lower = stripped.to_lowercase();
    if lower.starts_with("and ") || lower.starts_with("& ") || year_lead_re().is_match(stripped) {
        return None;
    }
    // INFORMS / Springer author-year: the last author stands before the
    // parenthesised year (`Kempe D, Kleinberg R (2008) Title`), so the part
    // is still an author, not a title.
    if let Some(paren) = year_paren_re().find(stripped) {
        let prefix = stripped[..paren.start()].trim();
        if !prefix.is_empty() && (is_name_part(prefix) || vancouver_name(prefix)) {
            return None;
        }
    }
    // A name followed by a period (`Jones C. Title`, `A. B. Smith. Title`)
    // is the last author of a period-delimited entry, not a title.
    let stop = title_end(part);
    let mut search = 0usize;
    while let Some(rel) = part[search..].find(". ") {
        let pos = search + rel;
        if pos > stop {
            break;
        }
        let prefix = part[..pos].trim();
        if is_name_part(prefix) && !is_et_al(prefix) {
            return None;
        }
        search = pos + 2;
    }
    if !first_char.is_uppercase() && venue_lead_re().is_match(stripped) {
        return None;
    }
    // A bare name (capitalised words only) followed by another author part
    // is a full name, not a title: `M. Nori, Sangseok Yun, and Il Kim.`
    let tokens: Vec<&str> = stripped.split_whitespace().collect();
    if (1..=3).contains(&tokens.len()) && tokens.iter().all(|t| cap_word_re().is_match(t)) {
        let next = parts
            .get(k + 1)
            .map_or("", |r| masked[r.clone()].trim_start());
        let next_lower = next.to_lowercase();
        if next_lower.starts_with("and ")
            || next_lower.starts_with("& ")
            || is_name_part(&next[..title_end(next)])
        {
            return None;
        }
    }
    // `et al.` opening the part belongs to the authors.
    let mut title_start = part_start;
    if let Some(lead) = et_al_lead_re().find(stripped) {
        title_start += lead.end();
    }
    let mut end = title_start + title_end(&masked[title_start..]);
    if end <= title_start {
        return None;
    }
    for later in parts.iter().skip(k + 1) {
        let comma = later.start.saturating_sub(2);
        if comma >= end {
            break;
        }
        // `You Only Look Once: Unified, Real-Time Object Detection`,
        // `Automated, LLM enabled extraction …`: a one-word title or subtitle
        // goes on over a capitalised part that is no venue.
        let text = &masked[later.clone()];
        if venue_like(text) && !title_takes_part(&masked[title_start..comma], text) {
            end = comma;
            break;
        }
    }
    Some(CommaSplit {
        authors_end: range.start,
        title_start,
        title_end: end,
    })
}

/// `title` without a trailing comma and without the quotes that enclose the
/// whole of it (`“Title”.` in biblatex); quotes inside stay.
fn strip_wrapping_quotes(title: &str) -> &str {
    let trimmed = title.trim().trim_end_matches(',').trim();
    let opens = trimmed.starts_with(['“', '"', '„', '‘']);
    let closes = trimmed.ends_with(['”', '"', '’']);
    if opens && closes && trimmed.chars().count() > 2 {
        return trimmed
            .trim_start_matches(['“', '"', '„', '‘'])
            .trim_end_matches(['”', '"', '’'])
            .trim();
    }
    trimmed
}

/// `title` without a trailing bracketed descriptor: APA `[Doctoral
/// dissertation, University of Oxford]`, `[Pyro Tutorial]`, `[Online]`. The
/// descriptor must contain a lowercase letter (`[MASK]` is a token in a
/// title) and leave some title before it.
fn strip_bracket_descriptor(title: &str) -> &str {
    let trimmed = title.trim_end();
    if let Some(head) = trimmed.strip_suffix(']')
        && let Some(open) = head.rfind('[')
        && open > 0
        && head.len() - open <= 80
        && head[open..].chars().any(char::is_lowercase)
    {
        return head[..open].trim_end();
    }
    trimmed
}

/// Byte offset of the colon that closes a Springer LNCS author list
/// (`Surname, I., Other, J.: Title`), if the body opens with one.
fn lncs_authors_end(text: &str) -> Option<usize> {
    let found = lncs_authors_re()
        .find(text)
        .or_else(|| lncs_surnames_re().find(text))?;
    text[..found.end()].rfind(':')
}

/// An LNCS author list of surnames only, closed by a colon before the
/// title: `Galun, Sharon, Basri, Brandt: Texture segmentation …`.
fn lncs_surnames_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^\s*\p{Lu}[\p{L}'’\-]+(?:,\s\p{Lu}[\p{L}'’\-]+){1,9}:\s+\p{Lu}")
            .expect("valid regex")
    })
}

/// Royal Society of Chemistry style has no title: `Q. Zhang, E. Uchaker
/// and G. Cao, Chem. Soc. Rev., 2013, 42, 3127–3171.` After an
/// initials-first author list comes the journal, then a bare year and a
/// volume as their own comma-delimited parts. Returns the byte range of the
/// journal part (the author list ends where it starts) and the volume.
///
/// The journal may be abbreviated like a name (`AIChE J.`) or open in
/// camel case (`iScience`); the volume may be a range (`207-208`) or empty
/// (`10,.`). A thesis note (`D. M. Lowe, PhD thesis, …`) and a conference
/// or workshop named after the authors and followed by the year alone
/// (`ICLR 2026 Workshop on …, 2026.`, `2024 IEEE/CVF Conference on …, 2024,
/// pp. 4818–4829.`) mark a title-less entry too; the volume is then `None`.
fn titleless_journal(masked: &str) -> Option<(Range<usize>, Option<String>)> {
    let parts = comma_parts(masked);
    let first = &masked[parts.first()?.clone()];
    if !initials_first(first) || !is_name_part(first) {
        return None;
    }
    let mut k = 1usize;
    while k < parts.len()
        && is_name_part(&masked[parts[k].clone()])
        && rsc_volume(masked, &parts, k).is_none()
    {
        k += 1;
    }
    let journal = parts.get(k)?.clone();
    let text = masked[journal.clone()].trim();
    if thesis_re().is_match(text) {
        return Some((journal, None));
    }
    if text.contains(['“', '"', '„', '‘']) || text.chars().count() > 80 {
        return None;
    }
    if let Some(volume) = rsc_volume(masked, &parts, k) {
        let mut chars = text.chars();
        let opens = match (chars.next(), chars.next()) {
            (Some(a), Some(b)) => a.is_uppercase() || (a.is_lowercase() && b.is_uppercase()),
            (Some(a), None) => a.is_uppercase(),
            _ => false,
        };
        let prose = text.split_whitespace().any(|w| {
            w.chars().count() >= 4
                && w.starts_with(char::is_lowercase)
                && !w.chars().skip(1).any(char::is_uppercase)
        });
        return (opens && !prose).then_some((journal, Some(volume)));
    }
    let year = masked[parts.get(k + 1)?.clone()]
        .trim()
        .trim_end_matches('.');
    let after_year = parts.get(k + 2).map_or("", |r| masked[r.clone()].trim());
    let lead = text.split_whitespace().next().unwrap_or("");
    let acronym_lead = year_token_re().is_match(lead)
        || (lead.chars().count() >= 2
            && lead.chars().all(|c| c.is_uppercase() || c.is_ascii_digit()));
    let venue_only = year_token_re().is_match(year)
        && (after_year.is_empty() || pages_labelled_re().is_match(after_year))
        && acronym_lead
        && clause_names_venue(text);
    venue_only.then_some((journal, None))
}

/// The volume of an RSC entry whose journal is part `k`: the next part is a
/// bare year and the one after it a volume (`42`, `207-208`, or `10,.` with
/// the pages left out).
fn rsc_volume(masked: &str, parts: &[Range<usize>], k: usize) -> Option<String> {
    let year = masked[parts.get(k + 1)?.clone()].trim();
    let volume = masked[parts.get(k + 2)?.clone()]
        .trim()
        .trim_end_matches(['.', ',']);
    if !year_token_re().is_match(year) {
        return None;
    }
    let digits = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    let range = volume
        .split_once(['-', '–'])
        .is_some_and(|(a, b)| digits(a) && digits(b));
    (digits(volume) || range).then(|| volume.to_string())
}

/// A thesis note standing where the title would (`PhD thesis`, `Master's
/// thesis`, `Doctoral dissertation`).
fn thesis_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)^(?:ph\.?\s?d\.?|doctoral|master(?:'s|’s)?|m\.?\s?sc\.?|bachelor(?:'s|’s)?|diploma)\s+(?:thesis|dissertation)\b",
        )
        .expect("valid regex")
    })
}

/// An open publisher group before a year: `(Springer, ` in `(Springer,
/// 2009)`.
fn publisher_group_open_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\([^()]{2,60},\s*$").expect("valid regex"))
}

/// The no-date token that stands where the year would (`[n. d.]` in ACM,
/// `(n.d.)` in APA, a bare `n.d.`), with the punctuation and space after it.
fn no_date_lead_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*[\[(]?n\.\s?d\.[\])]?[.,:]?\s+").expect("valid regex"))
}

/// A date closing an unquoted title: `Mistral small 3.1, 2025`, `model
/// card, 04 2025`, `, March 2005.`. Group 1 is the year.
fn trailing_year_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r",\s*(?:\d{1,2}\s+|\p{Lu}\p{Ll}{2,8}\.?\s+)?((?:19|20)\d{2})[a-z]?\.?$")
            .expect("valid regex")
    })
}

/// A parenthesised year closing an unquoted title (`unsrt`-like
/// `Authors, Title (2026). arXiv:...`). Group 1 is the year.
fn trailing_paren_year_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\s*\(((?:19|20)\d{2})[a-z]?\)\.?$").expect("valid regex"))
}

/// `arXiv preprint` after an unquoted title (`... work? arXiv preprint
/// arXiv:2507.11891`): the title ends before it.
fn arxiv_preprint_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)[,.]?\s+arxiv\s+preprint\b").expect("valid regex"))
}

/// A ditto dash standing for the previous entry's authors (`——, “Title,”`),
/// with the punctuation and space after it.
fn ditto_authors_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(?:[—–]+|-{2,}|_{2,})\s*[,.:]?\s+").expect("valid regex"))
}

/// A regulation or standard opening an entry without authors: `Regulation
/// (EU) 2017/745 on medical devices`, `Directive (EU) 2016/680`, `ISO/IEC
/// 27001:2013 ...`. The whole leading clause is the title.
fn legal_title_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^(?:(?:Council |Commission )?(?:Regulation|Directive|Decision)\s+\((?:EU|EC|EEC|Euratom)\)\s+(?:No\.?\s+)?\d+/\d+|(?:ISO/IEC|ISO|IEC)\s+\d+(?:[-:]\d+)*\s+\p{Lu})",
        )
        .expect("valid regex")
    })
}

/// A leading `volume N of Series` (group 1 is the series) in the text after
/// a book title.
fn series_volume_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^(?i:vol(?:ume)?\.?)\s*\d+\s+of\s+([^,;]+?)(?:\.\s|[,;]|\.?$)")
            .expect("valid regex")
    })
}

/// A leading `volume N.` sentence without a series (`volume 48. Cambridge
/// University Press`).
fn bare_volume_lead_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(?i:vol(?:ume)?\.?)\s*\d+\.\s+").expect("valid regex"))
}

/// `title` without a trailing `, 2025` or ` (2025)` that repeats the entry's
/// year (the year sentence of `Title, 2025. Model card` absorbed into the
/// title).
fn strip_trailing_year(title: &str, year: Option<u16>) -> &str {
    let Some(year) = year else {
        return title;
    };
    for re in [trailing_year_re(), trailing_paren_year_re()] {
        if let Some(caps) = re.captures(title)
            && let (Some(whole), Some(digits)) = (caps.get(0), caps.get(1))
            && digits.as_str().parse::<u16>().ok() == Some(year)
        {
            let head = title[..whole.start()].trim_end();
            if !head.is_empty() {
                return head;
            }
        }
    }
    title
}

/// A group closing a book title: `(Springer, 2019)`, `(MIT press)`, or its
/// open half when the title was cut at the comma before the year
/// (`(Springer`). Group 1 is the content.
fn trailing_group_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\s*\(([^()]{2,80})\)?\.?$").expect("valid regex"))
}

/// A year closing a publisher group: `, 2019` in `(Springer, 2019)`.
fn group_year_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r",\s*(?:19|20)\d{2}[a-z]?$").expect("valid regex"))
}

/// A word of a publisher name (`Springer`, `MIT press`, `Athena
/// Scientific`, `Courier Corporation`).
fn publisher_word_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"\b(?:Springer|Elsevier|Wiley|Press|press|Publishers?|Publishing|Verlag|Freeman|Scientific|Media|Corporation|ACM|IEEE|MIT|Kluwer|Birkhäuser|CRC|Routledge|Pearson|McGraw-Hill|Addison-Wesley|Dover|Academic|Athena|Courier|SIAM|Interscience|InterScience)\b",
        )
        .expect("valid regex")
    })
}

/// Volume and pages, a page range or a labelled page range left at the end
/// of a title when the journal is not printed (Nature-style lists):
/// `Temporal Coding 153, 26–46.`, `Spacings , 1–12`, `segmentation pp.
/// 216–235`.
fn locator_tail_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?:\s*,\s*(?:pp?\.\s*)?\d+\s*[–\-—]\s*\d+|\s+pp?\.\s*\d+(?:\s*[–\-—]\s*\d+)?|\s+\d+,\s*\d+(?:\s*[–\-—]\s*\d+)?)\.?$",
        )
        .expect("valid regex")
    })
}

/// A name after the last comma of a title (`…, Elsevier`). Group 1 is the
/// name.
fn publisher_tail_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r",\s*(\p{Lu}[^,]{1,60}?)\.?$").expect("valid regex"))
}

/// An editor tag opening a title (`(eds) Investigating …`) after an editor
/// list that the author list absorbed.
fn editor_lead_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\((?i:eds?|editors?)\.?\)\s+").expect("valid regex"))
}

/// A label that stands where a title would, before a URL (`URL:`,
/// `Available at`, `[Online]`).
fn url_label_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)^(?:url|available(?:\s+(?:at|from|online))?|online|\[online\]|link|retrieved\s+from)\s*:?\.?$",
        )
        .expect("valid regex")
    })
}

/// Does `title` name something: a letter, and not a bare URL label
/// ([`url_label_re`])? A page number left after a URL (`3`) does not.
fn names_something(title: &str) -> bool {
    title.chars().any(char::is_alphabetic) && !url_label_re().is_match(title)
}

/// Does `inner`, the content of a group closing a title, name a publisher:
/// a capitalised name without digits or commas, followed by a year or
/// holding a publisher word ([`publisher_word_re`])?
fn is_publisher_group(inner: &str) -> bool {
    let name = group_year_re()
        .find(inner)
        .map_or(inner, |year| &inner[..year.start()]);
    let dated = name.len() < inner.len();
    name.trim_start().starts_with(char::is_uppercase)
        && !name.contains(',')
        && !name.chars().any(|c| c.is_ascii_digit())
        && (dated || publisher_word_re().is_match(name))
}

/// `title` without the publisher, volume or pages that a list without a
/// printed venue leaves at its end: a publisher group (`(Springer, 2019)`,
/// `(Athena Scientific)`, the open `(Springer` of a title cut at the comma
/// before the year), a volume and page tail (`153, 26–46`, `, 1–12`, `pp.
/// 216–235`; a range of years such as `, 1950–2000` stays) or a publisher
/// after a comma (`, Elsevier`). A group without a publisher word or year
/// (`(Release 15)`, `(IMEx)`) stays.
fn strip_trailing_locator(title: &str) -> &str {
    let title = title.trim_end();
    if let Some(caps) = trailing_group_re().captures(title)
        && let (Some(whole), Some(inner)) = (caps.get(0), caps.get(1))
        && whole.start() > 0
        && is_publisher_group(inner.as_str())
    {
        return title[..whole.start()].trim_end();
    }
    if let Some(tail) = locator_tail_re().find(title)
        && tail.start() > 0
        && !tail
            .as_str()
            .split(|c: char| !c.is_ascii_digit())
            .any(|digits| year_token_re().is_match(digits))
    {
        return title[..tail.start()].trim_end();
    }
    if let Some(caps) = publisher_tail_re().captures(title)
        && let (Some(whole), Some(name)) = (caps.get(0), caps.get(1))
        && whole.start() > 0
        && name.as_str().split_whitespace().count() <= 5
        && !name.as_str().chars().any(|c| c.is_ascii_digit())
        && publisher_word_re().is_match(name.as_str())
    {
        return title[..whole.start()].trim_end();
    }
    title
}

/// A bare printed number before an initials-first author list (RSC
/// `1 Q. Zhang, ...`) left in `raw` when no numbered label was stripped.
fn bare_number_lead_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\d{1,4}\s+\p{Lu}\.").expect("valid regex"))
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
    if bare_number_lead_re().is_match(raw) {
        return raw
            .trim_start_matches(|c: char| c.is_ascii_digit())
            .trim_start();
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
    let mut title_limit: Option<usize> = None;
    let mut quoted_title: Option<(Range<usize>, String)> = None;
    let mut titleless_volume: Option<String> = None;
    let mut titleless = false;
    if let Some(ditto) = ditto_authors_re().find(&masked) {
        // `——, “Title,” Venue`: the previous entry's authors, not repeated.
        authors_end = Some(0);
        title_start = ditto.end();
        if let Some((q, text)) = &quoted
            && q.start == title_start
        {
            quoted_title = Some((q.clone(), text.clone()));
        } else if let Some(lead) = leading_year_re().find(&masked[title_start..]) {
            title_start += lead.end();
        }
    } else if legal_title_re().is_match(&masked) {
        // `Regulation (EU) 2017/745 on medical devices. Official Journal`:
        // no authors; the leading clause is the title.
        authors_end = Some(0);
    } else if let Some(colon) = lncs_authors_end(&masked) {
        // Springer LNCS: `Surname, I., Other, J.: Title. In: Venue (Year)`.
        authors_end = Some(colon);
        let after = masked[colon + 1..].trim_start();
        title_start = masked.len() - after.len();
    } else if let Some((journal, volume)) = titleless_journal(&masked) {
        // RSC: `A. Author and B. Author, Journal, Year, Volume, Pages.`
        authors_end = Some(journal.start);
        title_start = journal.start;
        titleless = true;
        titleless_volume = volume;
    } else if let Some(split) = comma_style(&masked).or_else(|| organisation_comma(&masked)) {
        authors_end = Some(split.authors_end);
        title_start = split.title_start;
        title_limit = Some(split.title_end);
    } else if let Some(end) = authorless_title(&masked) {
        // No authors: `Kinetic models of …, Physica A … 467 (2017) 201–217.`
        authors_end = Some(0);
        title_limit = Some(end);
    } else {
        // The first year (in position) preceded by an author list only:
        // `Smith, A. (2020). Title`, `Aji and Heafield. 2017. Title`.
        for (range, _) in all_years(&masked) {
            if quoted.as_ref().is_some_and(|(q, _)| range.start > q.start) {
                break;
            }
            // `Delay Differential Equations (Springer, 2009)`: the year of a
            // publisher group closes the title, not the author list.
            if publisher_group_open_re().is_match(&masked[..range.start]) {
                continue;
            }
            if !is_author_only(&masked[..range.start]) {
                continue;
            }
            authors_end = Some(range.start);
            // Skip a year suffix (`2020a`, `1988–a`) and the punctuation
            // closing the year.
            let mut after = &masked[range.end..];
            if after.starts_with(|c: char| c.is_ascii_lowercase()) {
                after = &after[1..];
            } else if let Some(rest) = after.strip_prefix(['–', '-'])
                && rest.starts_with(|c: char| c.is_ascii_lowercase())
            {
                after = &rest[1..];
            }
            after = after.trim_start_matches([')', '.', ',', ':', ' ']);
            title_start = masked.len() - after.len();
            if after.starts_with(['“', '"', '„', '‘'])
                && let Some((q, text)) = find_quoted(after)
            {
                quoted_title = Some((title_start + q.start..title_start + q.end, text));
            }
            break;
        }
        if authors_end.is_none()
            && let Some((range, text)) = &quoted
            && (is_author_only(&masked[..range.start])
                || organisation_name(masked[..range.start].trim_end().trim_end_matches(',')))
        {
            authors_end = Some(range.start);
            quoted_title = Some((range.clone(), text.clone()));
            title_start = range.end;
        }
        if authors_end.is_none()
            && let Some(end) = author_terminator(&masked)
        {
            authors_end = Some(end);
            title_start = end;
            // ACM style puts the year as its own sentence: `Authors. 2019. Title.`
            if let Some(lead) = leading_year_re().find(&masked[end..]) {
                title_start = end + lead.end();
            }
        }
    }

    // An undated entry puts `[n. d.]` where the year would be; the title
    // follows it and any later date (`Accessed: ...`) is not the year.
    if quoted_title.is_none()
        && !titleless
        && let Some(lead) = no_date_lead_re().find(&masked[title_start..])
    {
        title_start += lead.end();
        entry.year = None;
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
    } else if let Some(caps) = handle_start_re().captures(&body)
        && let Some(handle) = caps.get(1)
    {
        entry.authors = vec![handle.as_str().to_string()];
    }

    let rest_start: usize = if titleless {
        // The journal part opens the rest; there is no title to report.
        title_start
    } else if let Some((range, text)) = quoted_title {
        entry.title = Some(text);
        range.end
    } else {
        let title_masked = &masked[title_start..];
        let mut stop = title_end(title_masked);
        if let Some(limit) = title_limit {
            stop = stop.min(limit.saturating_sub(title_start));
        } else if let Some(venue) = comma_venue_re().find(title_masked) {
            stop = stop.min(venue.start());
        }
        if let Some(preprint) = arxiv_preprint_re().find(title_masked)
            && preprint.start() > 0
        {
            stop = stop.min(preprint.start());
        }
        // A URL, DOI or arXiv id is never part of the title
        // (`A. Rohatgi, WebPlotDigitizer, https://automeris.io.`).
        if let Some(id_start) = masked_ranges
            .iter()
            .map(|range| range.start)
            .filter(|&start| start >= title_start)
            .min()
        {
            stop = stop.min(id_start - title_start);
        }
        let title = strip_wrapping_quotes(body[title_start..title_start + stop].trim());
        let title = strip_bracket_descriptor(title);
        let title = strip_trailing_year(title, entry.year);
        let title = strip_trailing_locator(title);
        let title = strip_trailing_year(title, entry.year);
        let title = editor_lead_re()
            .find(title)
            .map_or(title, |lead| &title[lead.end()..]);
        if !title.is_empty() && title.chars().count() <= 500 && names_something(title) {
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
    entry.volume = volume.or(titleless_volume);
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

/// The numeric namespace of one reference list: its printed numbers and
/// where the list ends, so that a marker past that end (in an appendix)
/// resolves in the next list first.
struct NumberSpace {
    /// Page of the list's first numbered entry, used to pair the namespace
    /// with its [`ListExtent`].
    first_page: u32,
    /// `(page number, line index)` of the line that ends the list; `None`
    /// when it runs to the end of the document or its extent is unknown.
    end: Option<(u32, usize)>,
    /// Printed number -> entry index (the first entry wins when a number
    /// repeats within one list).
    by_number: BTreeMap<u32, u32>,
}

/// Lookup tables for resolving markers to `ReferenceEntry::index`.
struct RefIndex {
    /// The list is numbered (`[n]`, `n.`, `n)`); markers are numeric.
    numbered: bool,
    /// One numeric namespace per list in document order: a list whose
    /// numbers restart (`References for the Appendices` starting again at
    /// `[1]`) opens a new one; a list that continues the numbering shares
    /// the namespace of the list before it.
    spaces: Vec<NumberSpace>,
    /// Largest printed number of any list; a marker citing more is not a
    /// citation.
    max_number: u32,
    /// (first-author surname, lower case; year; entry index).
    by_author_year: Vec<(String, u16, u32)>,
    /// Entry index -> the shape of its parsed author list, for entries
    /// with at least one parsed author.
    shapes: BTreeMap<u32, AuthorShape>,
}

/// The shape of an entry's author list, which tells apart entries with the
/// same first-author surname and year (`Xu 2025` alone, `Xu & Li 2025`,
/// `Xu et al. 2025`).
struct AuthorShape {
    /// Number of parsed authors.
    count: usize,
    /// The list ends in `et al.`, so it has more authors than were parsed.
    et_al: bool,
    /// Lower-case tokens of the second author's name.
    second: Vec<String>,
}

/// What a marker says about the authors after the first one, from the text
/// between the first author and the year ([`marker_coauthors`]).
enum Coauthors {
    /// No co-author: `Xu (2025)`.
    Single,
    /// `et al.`: three or more authors.
    EtAl,
    /// Named co-authors (`& Li`, `and van Roy`, `, Paulson, and Wenzel`):
    /// the lower-case surname token of each.
    Named(Vec<String>),
}

/// [`Coauthors`] of a marker from the text `between` its first author and
/// its year (`& Li (`, ` and Liu, `, ` et al., `, `, `). A co-author's
/// surname token is its last capitalised token (`van Roy` gives `roy`),
/// else its first token.
fn marker_coauthors(between: &str) -> Coauthors {
    let spaced = between.replace([',', '&', '(', ')'], " , ");
    let words: Vec<&str> = spaced.split_whitespace().collect();
    let et_al = words
        .windows(2)
        .any(|pair| pair[0] == "et" && pair[1].trim_end_matches('.') == "al");
    if et_al {
        return Coauthors::EtAl;
    }
    let mut names: Vec<String> = Vec::new();
    let mut current: Option<String> = None;
    for word in words {
        if word == "," || word == "and" {
            if let Some(name) = current.take() {
                names.push(name);
            }
            continue;
        }
        let token = word.trim_matches('.').to_lowercase();
        if token.is_empty() {
            continue;
        }
        if word.starts_with(char::is_uppercase) || current.is_none() {
            current = Some(token);
        }
    }
    if let Some(name) = current {
        names.push(name);
    }
    if names.is_empty() {
        Coauthors::Single
    } else {
        Coauthors::Named(names)
    }
}

/// Does the printed author name end in `et al.` (`Lawrence Cayton et
/// al.`)?
fn ends_with_et_al(name: &str) -> bool {
    without_et_al(name).len() < name.trim_end().len()
}

/// `name` without a trailing `et al.`, `et al`, `et alii` or `and others`
/// (`Koichi Miyasawa et al.` gives `Koichi Miyasawa`).
fn without_et_al(name: &str) -> &str {
    let trimmed = name.trim_end();
    for tail in ["et al.", "et al", "et alii", "and others"] {
        if let Some(head) = trimmed.strip_suffix(tail)
            && (head.is_empty() || head.ends_with([' ', ',']))
        {
            return head.trim_end().trim_end_matches(',').trim_end();
        }
    }
    trimmed
}

/// The shape of `entry`'s author list, `None` when no author was parsed.
/// `et al.` counts when it ends an author name or appears in the raw text
/// before the year.
fn author_shape(entry: &ReferenceEntry) -> Option<AuthorShape> {
    if entry.authors.is_empty() {
        return None;
    }
    let before_year = entry
        .year
        .and_then(|year| entry.raw.find(&year.to_string()))
        .map_or(entry.raw.as_str(), |at| &entry.raw[..at]);
    let et_al =
        entry.authors.iter().any(|name| ends_with_et_al(name)) || before_year.contains(" et al");
    let second: Vec<String> = entry.authors.get(1).map_or_else(Vec::new, |name| {
        without_et_al(name)
            .split([' ', ','])
            .map(|token| token.trim_matches('.').to_lowercase())
            .filter(|token| !token.is_empty())
            .collect()
    });
    Some(AuthorShape {
        count: entry.authors.len(),
        et_al,
        second,
    })
}

/// Printed number of a numbered entry's label (`[12]`, `12.`, `12)`).
fn printed_number(entry: &ReferenceEntry) -> Option<u32> {
    let label = entry.label.as_deref()?;
    let caps = numbered_label_re().captures(label)?;
    caps.get(1)?.as_str().parse::<u32>().ok()
}

/// The namespace a marker at byte `offset` of a page resolves in first,
/// given [`RefIndex::space_ends`] for that page: the list after the last
/// list whose end the marker has passed (a marker in the appendix after
/// the main bibliography cites the appendix list), else the first list.
fn home_space(ends: &[Option<usize>], offset: usize) -> usize {
    let passed = ends
        .iter()
        .take_while(|end| matches!(**end, Some(e) if offset >= e))
        .count();
    passed.min(ends.len().saturating_sub(1))
}

/// Surname of a printed author name: the part before a comma, else the last
/// token, else the first token when the last one is a block of initials. A
/// trailing `et al.` is no part of the name (`Lawrence Cayton et al.` gives
/// `cayton`).
fn author_surname(name: &str) -> String {
    let name = without_et_al(name);
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

/// Is `token` a block of initials (`EJ`, `J.`, `A.C.`)?
fn is_initials_block(token: &str) -> bool {
    token.chars().count() <= 3 && token.chars().all(|c| c.is_uppercase() || c == '.')
}

/// Whole-name key of a printed first-author name without a comma: all its
/// tokens, lower case, minus leading initials (`E. J.`) and trailing
/// initials blocks (`EJ`): `Tchetgen Tchetgen EJ` gives `tchetgen
/// tchetgen`, `Omar El Malki` gives `omar el malki`, `Alibaba Cloud Qwen
/// Team` gives `alibaba cloud qwen team`. `None` for a single token or a
/// `Surname, Given` name, whose surname ([`author_surname`]) is already
/// the whole part before the comma. A trailing `et al.` is dropped first.
fn author_full_key(name: &str) -> Option<String> {
    let name = without_et_al(name);
    if name.contains(',') {
        return None;
    }
    let tokens: Vec<&str> = name
        .split_whitespace()
        .filter(|t| !is_name_suffix(t))
        .collect();
    let start = tokens
        .iter()
        .position(|t| !t.contains('.'))
        .unwrap_or(tokens.len());
    let mut end = tokens.len();
    while end > start && is_initials_block(tokens[end - 1]) {
        end -= 1;
    }
    let kept = &tokens[start..end];
    (kept.len() >= 2).then(|| kept.join(" ").trim_matches('.').to_lowercase())
}

/// Second first-author key of a name without a comma that ends in a block
/// of two or three capitals without periods, lower case: such a block is
/// usually initials (`Rubin DB`) but may be the surname (`Suhas BN`, cited
/// as `BN et al.`), so both are indexed. `None` for other names.
fn trailing_caps_key(name: &str) -> Option<String> {
    let name = without_et_al(name);
    if name.contains(',') {
        return None;
    }
    let tokens: Vec<&str> = name
        .split_whitespace()
        .filter(|t| !is_name_suffix(t))
        .collect();
    let last = tokens.last()?;
    let caps = (2..=3).contains(&last.chars().count()) && last.chars().all(char::is_uppercase);
    (tokens.len() >= 2 && caps).then(|| last.to_lowercase())
}

/// Entry indices among `candidates` that the letter `suffix` (`2020b`)
/// picks: the n-th of several same-year entries, else all of them.
fn pick_by_suffix(mut candidates: Vec<u32>, suffix: &str) -> Vec<u32> {
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

/// Byte offset in `text` of its whitespace-separated token number `k`
/// (`text.len()` when it has fewer tokens).
fn token_offset(text: &str, k: usize) -> usize {
    let mut count = 0usize;
    let mut in_token = false;
    for (i, c) in text.char_indices() {
        if c.is_whitespace() {
            in_token = false;
        } else if !in_token {
            if count == k {
                return i;
            }
            count += 1;
            in_token = true;
        }
    }
    text.len()
}

impl RefIndex {
    /// Tables over `refs`; `extents` (the lists in document order, see
    /// [`list_extents`]) give every numeric namespace its end.
    fn build(refs: &[ReferenceEntry], extents: &[ListExtent]) -> Self {
        let mut spaces: Vec<NumberSpace> = Vec::new();
        let mut last_number: Option<u32> = None;
        let mut by_author_year: Vec<(String, u16, u32)> = Vec::new();
        let mut shapes: BTreeMap<u32, AuthorShape> = BTreeMap::new();
        for entry in refs {
            if let Some(shape) = author_shape(entry) {
                shapes.insert(entry.index, shape);
            }
            if let Some(number) = printed_number(entry) {
                let restart = last_number.is_some_and(|previous| number <= previous);
                if spaces.is_empty() || restart {
                    spaces.push(NumberSpace {
                        first_page: entry.page,
                        end: None,
                        by_number: BTreeMap::new(),
                    });
                }
                if let Some(space) = spaces.last_mut() {
                    space.by_number.entry(number).or_insert(entry.index);
                }
                last_number = Some(number);
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
            let full = entry
                .authors
                .first()
                .and_then(|name| author_full_key(name.as_str()));
            let caps = entry
                .authors
                .first()
                .and_then(|name| trailing_caps_key(name.as_str()));
            if let Some(year) = entry.year {
                let mut keys: Vec<String> = Vec::with_capacity(3);
                for key in [surname, full, caps].into_iter().flatten() {
                    if !key.is_empty() && !keys.contains(&key) {
                        keys.push(key);
                    }
                }
                for key in keys {
                    by_author_year.push((key, year, entry.index));
                }
            }
        }
        // Pair every namespace with the list it was segmented from: the
        // last list that starts on or before the namespace's first page,
        // never one already taken by an earlier namespace.
        let mut taken: Option<usize> = None;
        for space in &mut spaces {
            let by_page = extents
                .iter()
                .rposition(|extent| extent.start.0 <= space.first_page)
                .unwrap_or(0);
            let extent_index = taken.map_or(by_page, |t| by_page.max(t + 1));
            space.end = extents.get(extent_index).and_then(|extent| extent.end);
            taken = Some(extent_index);
        }
        let max_number = spaces
            .iter()
            .filter_map(|space| space.by_number.keys().next_back().copied())
            .max()
            .unwrap_or(0);
        Self {
            numbered: !spaces.is_empty(),
            spaces,
            max_number,
            by_author_year,
            shapes,
        }
    }

    /// Per numeric namespace: the byte offset of `page.text` from which a
    /// marker lies past that list's end (`Some(0)` when the list ended on
    /// an earlier page), or `None` when the list has not ended by this
    /// page. See [`home_space`].
    fn space_ends(&self, page: &PageText) -> Vec<Option<usize>> {
        self.spaces
            .iter()
            .map(|space| match space.end {
                Some((end_page, _)) if page.page > end_page => Some(0),
                Some((end_page, end_line)) if page.page == end_page => {
                    Some(heading_byte_offset(page, end_line))
                }
                _ => None,
            })
            .collect()
    }

    /// Entry indices of the printed `numbers`, in order, without repeats.
    /// Each number is looked up in the namespace `home` first and then in
    /// the other lists in document order.
    fn targets_for(&self, numbers: &[u32], home: usize) -> Vec<u32> {
        let mut targets: Vec<u32> = Vec::new();
        for number in numbers {
            let found = self
                .spaces
                .get(home)
                .and_then(|space| space.by_number.get(number))
                .or_else(|| {
                    self.spaces
                        .iter()
                        .find_map(|space| space.by_number.get(number))
                });
            if let Some(&idx) = found
                && !targets.contains(&idx)
            {
                targets.push(idx);
            }
        }
        targets
    }

    /// Entries of `year` whose first-author key equals `needle` (lower
    /// case) or, with `tail`, ends with it as a whole word (`de moura` for
    /// `moura`).
    fn lookup(&self, needle: &str, year: u16, tail: bool) -> Vec<u32> {
        let ending = format!(" {needle}");
        self.by_author_year
            .iter()
            .filter(|(key, y, _)| {
                *y == year && (*key == needle || (tail && key.ends_with(&ending)))
            })
            .map(|(_, _, idx)| *idx)
            .collect()
    }

    /// `candidates` whose author list fits the marker's `coauthors`: for
    /// `Xu & Li` the entries whose second author is `Li`, else those with
    /// exactly two authors; for `et al.` those with three or more; for a
    /// lone name those with one author (arXiv:2506.23487 `Xu & Li (2025)`
    /// and `Xu (2025)` beside a thesis by Xu alone). All of them when none
    /// fits or when there is just one candidate.
    fn narrow_by_coauthors(&self, mut candidates: Vec<u32>, coauthors: &Coauthors) -> Vec<u32> {
        candidates.sort_unstable();
        candidates.dedup();
        if candidates.len() < 2 {
            return candidates;
        }
        let kept: Vec<u32> = match coauthors {
            Coauthors::Single => self.fitting(&candidates, &|shape: &AuthorShape| {
                shape.count == 1 && !shape.et_al
            }),
            Coauthors::EtAl => self.fitting(&candidates, &|shape: &AuthorShape| {
                shape.count >= 3 || shape.et_al
            }),
            Coauthors::Named(names) => {
                let by_second = names.first().map_or_else(Vec::new, |second| {
                    self.fitting(&candidates, &|shape: &AuthorShape| {
                        shape.second.contains(second)
                    })
                });
                if by_second.is_empty() {
                    let expected = names.len() + 1;
                    self.fitting(&candidates, &|shape: &AuthorShape| {
                        shape.count == expected && !shape.et_al
                    })
                } else {
                    by_second
                }
            }
        };
        if kept.is_empty() { candidates } else { kept }
    }

    /// The `candidates` whose author list passes `test`; an entry without
    /// a parsed author list never does.
    fn fitting(&self, candidates: &[u32], test: &dyn Fn(&AuthorShape) -> bool) -> Vec<u32> {
        candidates
            .iter()
            .copied()
            .filter(|idx| self.shapes.get(idx).is_some_and(test))
            .collect()
    }

    /// Entries whose first author surname and year match. A suffix letter
    /// (`2020b`) picks the n-th of several same-year entries.
    fn resolve_author_year(&self, surname: &str, year: u16, suffix: &str) -> Vec<u32> {
        pick_by_suffix(self.lookup(&surname.to_lowercase(), year, true), suffix)
    }

    /// Entries cited by the marker name `name` (the first author as
    /// printed, one or more tokens: `Smith`, `Tchetgen Tchetgen`, `Alibaba
    /// Cloud Qwen Team`, `de Moura`) and `year`/`suffix`, with the number
    /// of leading tokens of `name` that are not part of the name (`As` in
    /// `As Alexander et al. 2015`).
    ///
    /// The whole name and then ever shorter endings of it are looked up
    /// exactly against the first-author keys (the surname and the whole
    /// name of every entry), then the same again allowing a key that ends
    /// with the name (`omar el malki` for `El Malki`), and last the first
    /// token alone. Several matches are narrowed by the marker's
    /// `coauthors` ([`RefIndex::narrow_by_coauthors`]) before the suffix
    /// letter picks among them.
    fn resolve_name(
        &self,
        name: &str,
        year: u16,
        suffix: &str,
        coauthors: &Coauthors,
    ) -> (Vec<u32>, usize) {
        let tokens: Vec<&str> = name.split_whitespace().collect();
        for tail in [false, true] {
            for dropped in 0..tokens.len() {
                let needle = tokens[dropped..].join(" ").to_lowercase();
                let found = self.lookup(&needle, year, tail);
                if !found.is_empty() {
                    let narrowed = self.narrow_by_coauthors(found, coauthors);
                    return (pick_by_suffix(narrowed, suffix), dropped);
                }
            }
        }
        if tokens.len() > 1
            && let Some(first) = tokens.first()
        {
            let found = self.resolve_author_year(first, year, suffix);
            if !found.is_empty() {
                return (found, 0);
            }
        }
        (Vec::new(), 0)
    }

    /// Entries cited by the first author `name` with every year and letter
    /// of the run `years` ([`MARKER_YEARS`]: `2023a,b`, `2022, 2023a`,
    /// `2025c,b,a, 2024`), in order without repeats, and the leading
    /// tokens of `name` dropped by the first resolved year (see
    /// [`RefIndex::resolve_name`]).
    fn resolve_years(&self, name: &str, years: &str, coauthors: &Coauthors) -> (Vec<u32>, usize) {
        let mut targets: Vec<u32> = Vec::new();
        let mut dropped: Option<usize> = None;
        let mut current: Option<u16> = None;
        for caps in year_item_re().captures_iter(years) {
            let (year, suffix) = if let Some(y) = caps.get(1) {
                let Ok(value) = y.as_str().parse::<u16>() else {
                    continue;
                };
                current = Some(value);
                (value, caps.get(2).map_or("", |m| m.as_str()))
            } else if let (Some(value), Some(letter)) = (current, caps.get(3)) {
                (value, letter.as_str())
            } else {
                continue;
            };
            let (found, skip) = self.resolve_name(name, year, suffix, coauthors);
            if !found.is_empty() && dropped.is_none() {
                dropped = Some(skip);
            }
            for idx in found {
                if !targets.contains(&idx) {
                    targets.push(idx);
                }
            }
        }
        (targets, dropped.unwrap_or(0))
    }
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

/// A marker found on a page: its byte range in `PageText::text`, its text,
/// the resolved entry indices and, for a numeric marker, the printed
/// numbers it cites and the namespace ([`home_space`]) they resolve in
/// first.
struct Found {
    range: Range<usize>,
    text: String,
    targets: Vec<u32>,
    numbers: Vec<u32>,
    space: usize,
}

/// Largest first number of a bracket group glued to a short all-caps token
/// that still reads as a symbol index (`W[1]`, `FPT[1]`, `NP[3]`).
const MAX_SYMBOL_INDEX: u32 = 3;

/// Is the bracket group at `start..end` part of a symbol rather than a
/// citation: `W[1]-hard`, `x[2]`, `FPT[1]`, `NP[3]` (a single letter right
/// before `[`, a token of up to three capitals right before `[` whose first
/// number is at most [`MAX_SYMBOL_INDEX`], or `-` and a letter right after
/// `]`)? `PEPNet[43]` and `BERT[12]` are citations glued to a word, and so
/// are `TCM[183]` and `Multi-ACG[103]` (arXiv:2305.13843): a caps token
/// after a hyphenated word or before a larger number is a name.
fn glued_to_word(text: &str, start: usize, end: usize) -> bool {
    let before = word_before(text, start);
    if before.is_empty() {
        return false;
    }
    let mut chars = before.chars();
    let single = chars.next().is_some_and(char::is_alphabetic) && chars.next().is_none();
    let compound = text[..start - before.len()]
        .strip_suffix('-')
        .is_some_and(|head| head.ends_with(char::is_alphabetic));
    let first_number: u32 = text[start..end]
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .unwrap_or(0);
    let short_caps = before.chars().count() <= 3
        && before.chars().all(char::is_uppercase)
        && !compound
        && first_number <= MAX_SYMBOL_INDEX;
    let dashed = text[end..]
        .strip_prefix('-')
        .is_some_and(|rest| rest.starts_with(char::is_alphabetic));
    single || short_caps || dashed
}

/// The numbers cited by the items of a numeric group (`1, 3–5`), or
/// `None` when any item is `0`, a backwards or overlong range, or above
/// `max_number`, or when no item is a number.
fn cited_numbers(inner: &str, max_number: u32) -> Option<Vec<u32>> {
    let mut numbers: Vec<u32> = Vec::new();
    for item in inner.split([',', ';']) {
        let Some(item_caps) = numeric_item_re().captures(item) else {
            continue;
        };
        let Some(lo) = item_caps
            .get(1)
            .and_then(|m| m.as_str().parse::<u32>().ok())
        else {
            continue;
        };
        let hi = item_caps
            .get(2)
            .and_then(|m| m.as_str().parse::<u32>().ok())
            .unwrap_or(lo);
        if lo == 0 || hi < lo || hi - lo > MAX_RANGE_SPAN || hi > max_number {
            return None;
        }
        numbers.extend(lo..=hi);
    }
    (!numbers.is_empty()).then_some(numbers)
}

/// Is the bracket group at `start..end` a line of its own next to another
/// bare `[n]` line (blank lines between them do not count)? That is the
/// label column of a reference list that the layout pass emitted apart
/// from its entries (arXiv:2509.12458 sets `[1]` ... `[14]` above the
/// `REFERENCES` heading), not a citation.
fn in_label_column(text: &str, start: usize, end: usize) -> bool {
    let line_start = text[..start].rfind('\n').map_or(0, |i| i + 1);
    let line_end = text[end..].find('\n').map_or(text.len(), |i| end + i);
    if !text[line_start..start].trim().is_empty() || !text[end..line_end].trim().is_empty() {
        return false;
    }
    let previous = text[..line_start]
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty());
    let next = text[line_end..]
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty());
    previous.is_some_and(|l| bare_label_re().is_match(l))
        || next.is_some_and(|l| bare_label_re().is_match(l))
}

/// Numeric markers in `text[window]`, with byte ranges into `text`. A
/// group is rejected when any item is `0` or above the largest printed
/// number (`[0, 1]` is an interval), when it is glued to a symbol
/// ([`glued_to_word`]), or when it sits in a detached label column
/// ([`in_label_column`]). A note after the numbers (`[22, Theorem 4]`) is
/// kept in the marker text but cites nothing. `ends` is
/// [`RefIndex::space_ends`] for the page: it picks the namespace each
/// marker resolves in first.
fn numeric_markers(
    text: &str,
    window: &Range<usize>,
    index: &RefIndex,
    ends: &[Option<usize>],
) -> Vec<Found> {
    let mut out: Vec<Found> = Vec::new();
    for caps in numeric_marker_re().captures_iter(&text[window.clone()]) {
        let (Some(whole), Some(inner)) = (caps.get(0), caps.get(1)) else {
            continue;
        };
        let start = window.start + whole.start();
        let end = window.start + whole.end();
        if glued_to_word(text, start, end) || in_label_column(text, start, end) {
            continue;
        }
        let Some(numbers) = cited_numbers(inner.as_str(), index.max_number) else {
            continue;
        };
        let space = home_space(ends, start);
        let targets = index.targets_for(&numbers, space);
        if targets.is_empty() {
            continue;
        }
        out.push(Found {
            range: start..end,
            text: whole.as_str().to_string(),
            targets,
            numbers,
            space,
        });
    }
    out
}

/// ASCII form of a superscript digit or range sign (`⁵` to `5`, `⁻` and
/// `–` to `-`); other characters are kept.
fn superscript_ascii(c: char) -> char {
    match c {
        '⁰' => '0',
        '¹' => '1',
        '²' => '2',
        '³' => '3',
        '⁴' => '4',
        '⁵' => '5',
        '⁶' => '6',
        '⁷' => '7',
        '⁸' => '8',
        '⁹' => '9',
        '⁻' | '–' | '−' => '-',
        other => other,
    }
}

/// Words that follow an affiliation or footnote mark at a line start
/// (`¹Department of Chemistry`), where a superscript is no citation.
const AFFILIATION_WORDS: &[&str] = &[
    "Department",
    "Dept",
    "University",
    "Institute",
    "School",
    "Faculty",
    "Laboratory",
    "College",
    "Center",
    "Centre",
    "Division",
    "Corresponding",
    "Email",
    "E",
    "Present",
    "These",
    "Electronic",
];

/// Is the superscript run at `start..end` attached like a citation? At a
/// line start the run attaches to the word that follows it (`³⁸Prein`),
/// unless that word opens an affiliation or footnote
/// ([`AFFILIATION_WORDS`]), or it closes the line above when punctuation
/// follows it ([`closes_line_above`]: `Semantic Scholar⏎¹⁰,¹¹, which`);
/// elsewhere it follows sentence punctuation or a closing bracket or
/// quote (`literature.⁵`) or a word of at least three letters
/// (`Initiative⁵⁻⁷`, `data⁸,⁹`) and is not followed by a letter or digit.
/// A power after a digit or a short symbol or unit (`10⁵`, `R²`, `km²`) is
/// not a citation.
fn superscript_attached(text: &str, start: usize, end: usize) -> bool {
    let next = text[end..].chars().next();
    let previous = text[..start].chars().next_back();
    let Some(previous) = previous.filter(|&c| c != '\n') else {
        if closes_line_above(text, start, end) {
            return true;
        }
        let word: String = text[end..]
            .chars()
            .take_while(|c| c.is_alphabetic())
            .collect();
        return !word.is_empty() && !AFFILIATION_WORDS.contains(&word.as_str());
    };
    if next.is_some_and(char::is_alphanumeric) {
        return false;
    }
    if matches!(
        previous,
        '.' | ',' | ';' | ':' | ')' | ']' | '’' | '”' | '"' | '\''
    ) {
        return true;
    }
    previous.is_alphabetic()
        && text[..start]
            .chars()
            .rev()
            .take_while(|c| c.is_alphabetic())
            .count()
            >= 3
}

/// Does the superscript run at `start..end`, which opens a line, close the
/// line above (arXiv:2510.26824 `…and Semantic Scholar⏎¹⁰,¹¹, which is
/// filtered`)? The line above ends with a letter, and the run is followed
/// by `.`, or by `,`, `;` or `:` and then a lower-case word or the line
/// end (`¹, ²Department` is an affiliation list).
fn closes_line_above(text: &str, start: usize, end: usize) -> bool {
    let above = text[..start]
        .trim_end()
        .chars()
        .next_back()
        .is_some_and(char::is_alphabetic);
    let mut after = text[end..].chars();
    let punctuation = after.next();
    let following = after.find(|&c| c != ' ');
    above
        && match punctuation {
            Some('.') => true,
            Some(',' | ';' | ':') => following.is_none_or(|c| c == '\n' || c.is_lowercase()),
            _ => false,
        }
}

/// Most detached superscript lines that follow one body line (two columns
/// interleaved give five in a row in arXiv:2510.26824).
const MAX_DETACHED_RUN: usize = 8;

/// Does `line` read as prose: two or more words of at least two letters?
fn is_prose_line(line: &str) -> bool {
    line.split_whitespace()
        .filter(|word| word.chars().filter(|c| c.is_alphabetic()).count() >= 2)
        .count()
        >= 2
}

/// Superscript citations on lines of their own in `text[window]`
/// (arXiv:2510.26824, RSC): a run of lines holding only a superscript
/// fragment ([`detached_fragment_re`]) right after a prose line, which
/// text cleanup could not attach to its word. Each fragment is a marker at
/// its own line; plain digits (`5`, `10,11`) never count (axis ticks
/// between prose lines look the same). The run is
/// dropped when a blank line separates it from the prose line above, when
/// any fragment does not cite the list (`0`, a number above it), when it
/// is longer than [`MAX_DETACHED_RUN`], or when neither a prose line nor
/// the page end follows it (axis ticks, a label column).
fn detached_superscript_markers(
    text: &str,
    window: &Range<usize>,
    index: &RefIndex,
    ends: &[Option<usize>],
) -> Vec<Found> {
    let mut out: Vec<Found> = Vec::new();
    let mut run: Vec<Found> = Vec::new();
    let mut run_ok = true;
    let mut after_prose = false;
    let mut line_start = window.start;
    for line in text[window.clone()].split('\n') {
        let start = line_start;
        line_start += line.len() + 1;
        let trimmed = line.trim();
        if detached_fragment_re().is_match(trimmed) {
            if !after_prose {
                continue;
            }
            let ascii: String = trimmed
                .chars()
                .filter(|c| !c.is_whitespace())
                .map(superscript_ascii)
                .collect();
            let numbers = cited_numbers(&ascii, index.max_number);
            let at = start + (line.len() - line.trim_start().len());
            let space = home_space(ends, at);
            let found = numbers.and_then(|numbers| {
                let targets = index.targets_for(&numbers, space);
                (!targets.is_empty()).then(|| Found {
                    range: at..at + trimmed.len(),
                    text: trimmed.to_string(),
                    targets,
                    numbers,
                    space,
                })
            });
            match found {
                Some(f) if run.len() < MAX_DETACHED_RUN => run.push(f),
                _ => run_ok = false,
            }
            continue;
        }
        if trimmed.is_empty() && !run.is_empty() {
            continue;
        }
        let prose = is_prose_line(trimmed);
        if prose && run_ok {
            out.append(&mut run);
        }
        run.clear();
        run_ok = true;
        after_prose = prose;
    }
    if run_ok && window.end == text.len() {
        out.append(&mut run);
    }
    out
}

/// Superscript numeric markers in `text[window]` (`literature.⁵`,
/// `Initiative⁵⁻⁷`, `data⁸,⁹`), resolved like bracket groups in the same
/// numeric namespaces. Only called for a numbered list, so a footnote
/// mark in an author-year document is never a citation.
fn superscript_markers(
    text: &str,
    window: &Range<usize>,
    index: &RefIndex,
    ends: &[Option<usize>],
) -> Vec<Found> {
    let mut out: Vec<Found> = Vec::new();
    for found in superscript_marker_re().find_iter(&text[window.clone()]) {
        let start = window.start + found.start();
        let end = window.start + found.end();
        if !superscript_attached(text, start, end) {
            continue;
        }
        let ascii: String = found.as_str().chars().map(superscript_ascii).collect();
        let Some(numbers) = cited_numbers(&ascii, index.max_number) else {
            continue;
        };
        let space = home_space(ends, start);
        let targets = index.targets_for(&numbers, space);
        if targets.is_empty() {
            continue;
        }
        out.push(Found {
            range: start..end,
            text: found.as_str().to_string(),
            targets,
            numbers,
            space,
        });
    }
    out
}

/// Numbers cited by two numeric groups joined by `gap`: `[17], [18]` cites
/// both lists, `[3]–[5]` (IEEE `cite` package) the closed range from the
/// last number of `a` to the single number of `b`, after the numbers of
/// `a` (`[6]–[10], [16]` then `–[19]` cites 6 to 10 and 16 to 19,
/// arXiv:2604.03540). `None` when the groups are separate markers.
fn adjacent_numbers(gap: &str, a: &Found, b: &Found) -> Option<Vec<u32>> {
    let trimmed = gap.trim();
    if trimmed == "," {
        let mut numbers = a.numbers.clone();
        for &n in &b.numbers {
            if !numbers.contains(&n) {
                numbers.push(n);
            }
        }
        return Some(numbers);
    }
    if matches!(trimmed, "–" | "—" | "-")
        && let (Some(&lo), &[hi]) = (a.numbers.last(), b.numbers.as_slice())
        && hi > lo
        && hi - lo <= MAX_RANGE_SPAN
    {
        let mut numbers = a.numbers.clone();
        for n in lo + 1..=hi {
            if !numbers.contains(&n) {
                numbers.push(n);
            }
        }
        return Some(numbers);
    }
    None
}

/// Merge adjacent numeric groups printed one bracket per number
/// (`[17], [18]`, `[3]–[5]`) into one marker whose targets are the union.
/// `found` must be sorted by position.
fn merge_adjacent(text: &str, found: Vec<Found>, index: &RefIndex) -> Vec<Found> {
    let mut merged: Vec<Found> = Vec::with_capacity(found.len());
    for next in found {
        if let Some(last) = merged.last_mut()
            && last.range.end <= next.range.start
            && let Some(numbers) =
                adjacent_numbers(&text[last.range.end..next.range.start], last, &next)
        {
            let targets = index.targets_for(&numbers, last.space);
            if !targets.is_empty() {
                last.range.end = next.range.end;
                last.text = text[last.range.clone()].to_string();
                last.targets = targets;
                last.numbers = numbers;
                continue;
            }
        }
        merged.push(next);
    }
    merged
}

/// Byte offset where a marker whose name group `name` starts at
/// `name_start` begins when `dropped` leading tokens of the name are not
/// part of it (see [`RefIndex::resolve_name`]); `whole_start` (the start
/// of the match, initials included) when none are dropped.
fn marker_start(whole_start: usize, name_start: usize, name: &str, dropped: usize) -> usize {
    if dropped == 0 {
        whole_start
    } else {
        name_start + token_offset(name, dropped)
    }
}

/// Entries cited by the inside of a parenthetical, in order without
/// repeats: every author-year clause ([`clause_scan_re`]) of every
/// `;`-separated part, each narrowed by its co-authors, and a part that is
/// only years ([`bare_years_re`]) with the author of the clause before it
/// (`Abe et al., 2023; 2024`, arXiv:2503.00030).
fn parenthetical_targets(inner: &str, index: &RefIndex) -> Vec<u32> {
    let mut targets: Vec<u32> = Vec::new();
    let mut previous: Option<(&str, Coauthors)> = None;
    for part in inner.split(';') {
        let mut resolved: Vec<u32> = Vec::new();
        let mut matched = false;
        for caps in clause_scan_re().captures_iter(part) {
            let (Some(name), Some(years)) = (caps.get(1), caps.get(2)) else {
                continue;
            };
            let coauthors = marker_coauthors(&part[name.end()..years.start()]);
            resolved.extend(
                index
                    .resolve_years(name.as_str(), years.as_str(), &coauthors)
                    .0,
            );
            previous = Some((name.as_str(), coauthors));
            matched = true;
        }
        if !matched
            && bare_years_re().is_match(part)
            && let Some((name, coauthors)) = &previous
        {
            resolved.extend(index.resolve_years(name, part.trim(), coauthors).0);
        }
        for idx in resolved {
            if !targets.contains(&idx) {
                targets.push(idx);
            }
        }
    }
    targets
}

/// Author-year markers in `text[window]`, with byte ranges into `text`:
/// narrative `Name (2020)` forms (with locators and year lists,
/// `Politis and Romano (1994, Theorem 3.1)`), parentheticals of one or
/// more clauses separated by `;` or by a comma after a year (`(Rubin 1976,
/// Robins et al. 1994, Qin et al. 2008)`), and bare `Name et al. 2020`
/// citations that resolve to an entry. A parenthetical is one marker whose
/// targets are the union over its clauses; it is kept without targets
/// only when a clause has the strict `Name, 2020` form ([`clause_re`]).
fn author_year_markers(text: &str, window: &Range<usize>, index: &RefIndex) -> Vec<Found> {
    let slice = &text[window.clone()];
    let mut out: Vec<Found> = Vec::new();
    for caps in narrative_marker_re().captures_iter(slice) {
        let (Some(whole), Some(name), Some(years)) = (caps.get(0), caps.get(1), caps.get(2)) else {
            continue;
        };
        let coauthors = marker_coauthors(&slice[name.end()..years.start()]);
        let (targets, dropped) = index.resolve_years(name.as_str(), years.as_str(), &coauthors);
        if targets.is_empty() {
            continue;
        }
        let begin = marker_start(whole.start(), name.start(), name.as_str(), dropped);
        out.push(Found {
            range: window.start + begin..window.start + whole.end(),
            text: slice[begin..whole.end()].to_string(),
            targets,
            numbers: Vec::new(),
            space: 0,
        });
    }
    for found in parenthetical_re().find_iter(slice) {
        let start = window.start + found.start();
        let end = window.start + found.end();
        let overlaps = out
            .iter()
            .any(|f| f.range.start < end && start < f.range.end);
        if overlaps {
            continue;
        }
        let inner = &slice[found.start() + 1..found.end() - 1];
        let targets = parenthetical_targets(inner, index);
        let strict = inner.split(';').any(|clause| clause_re().is_match(clause));
        if targets.is_empty() && !strict {
            continue;
        }
        out.push(Found {
            range: start..end,
            text: found.as_str().to_string(),
            targets,
            numbers: Vec::new(),
            space: 0,
        });
    }
    for caps in bare_et_al_re().captures_iter(slice) {
        let (Some(whole), Some(name), Some(year)) = (caps.get(0), caps.get(1), caps.get(2)) else {
            continue;
        };
        let Ok(year_value) = year.as_str().parse::<u16>() else {
            continue;
        };
        let suffix = caps.get(3).map_or("", |m| m.as_str());
        let (targets, dropped) =
            index.resolve_name(name.as_str(), year_value, suffix, &Coauthors::EtAl);
        if targets.is_empty() {
            continue;
        }
        let begin = marker_start(whole.start(), name.start(), name.as_str(), dropped);
        let start = window.start + begin;
        let end = window.start + whole.end();
        let overlaps = out
            .iter()
            .any(|f| f.range.start < end && start < f.range.end);
        if overlaps {
            continue;
        }
        out.push(Found {
            range: start..end,
            text: slice[begin..whole.end()].to_string(),
            targets,
            numbers: Vec::new(),
            space: 0,
        });
    }
    out
}

/// Where a reference list sits in the document: from its heading line
/// (`start`, as page number and line index) to the line that ends it
/// (`end`), or to the end of the document when `end` is `None`.
struct ListExtent {
    start: (u32, usize),
    end: Option<(u32, usize)>,
}

/// The extent of every list in `sections` (see [`ListExtent`]).
fn list_extents(
    pages: &[PageText],
    sections: &[ReferenceSection],
    repeated: &[String],
) -> Vec<ListExtent> {
    sections
        .iter()
        .enumerate()
        .map(|(k, section)| {
            let stop = sections
                .get(k + 1)
                .map(|next| (next.first_page, next.first_line));
            let body = list_body_with_furniture(pages, section, stop, repeated);
            ListExtent {
                start: (section.first_page, section.first_line),
                end: body.end.or(stop),
            }
        })
        .collect()
}

/// `windows` with the bytes `cut_start..cut_end` removed.
fn subtract_range(windows: &[Range<usize>], cut_start: usize, cut_end: usize) -> Vec<Range<usize>> {
    let mut out: Vec<Range<usize>> = Vec::with_capacity(windows.len() + 1);
    for window in windows {
        if window.end <= cut_start || window.start >= cut_end {
            out.push(window.clone());
            continue;
        }
        if window.start < cut_start {
            out.push(window.start..cut_start);
        }
        if cut_end < window.end {
            out.push(cut_end..window.end);
        }
    }
    out
}

/// Byte ranges of `page.text` outside every reference list: the whole page
/// when no list touches it, the part above the heading, the part after the
/// line that ends a list (an appendix after the bibliography is scanned).
fn page_scan_windows(page: &PageText, extents: &[ListExtent]) -> Vec<Range<usize>> {
    let mut windows: Vec<Range<usize>> = Vec::with_capacity(2);
    windows.push(0..page.text.len());
    for extent in extents {
        let (start_page, start_line) = extent.start;
        if page.page < start_page {
            continue;
        }
        let cut_start = if page.page == start_page {
            heading_byte_offset(page, start_line)
        } else {
            0
        };
        let cut_end = match extent.end {
            Some((end_page, _)) if page.page > end_page => 0,
            Some((end_page, end_line)) if page.page == end_page => {
                heading_byte_offset(page, end_line)
            }
            _ => page.text.len(),
        };
        if cut_start < cut_end {
            windows = subtract_range(&windows, cut_start, cut_end);
        }
    }
    windows
}

/// Longest tail of a page (and head of the next page), in bytes, over
/// which a bracket group left open at a page end is carried.
const MAX_CARRY: usize = 400;

/// A `(` or `[` group left open at the end of `page`: its byte offset in
/// `page.text` and the character that closes it, when the last scan window
/// reaches the end of the page, the opener lies within [`MAX_CARRY`] bytes
/// of it and nothing after the opener closes a group.
fn open_group_at_end(page: &PageText, windows: &[Range<usize>]) -> Option<(usize, char)> {
    let last = windows.last()?;
    if last.end != page.text.len() {
        return None;
    }
    let slice = &page.text[last.clone()];
    let pos = slice.rfind(['(', '['])?;
    let rest = &slice[pos..];
    if rest.len() > MAX_CARRY || rest.contains([')', ']']) {
        return None;
    }
    let close = if rest.starts_with('(') { ')' } else { ']' };
    Some((last.start + pos, close))
}

/// Byte length of the head of `next` that closes a group carried over
/// from the page before: up to and including the first `close` within
/// [`MAX_CARRY`] bytes, when the page's first scan window starts at its top
/// and no other group opens before the `close`.
fn carried_head(next: &PageText, windows: &[Range<usize>], close: char) -> Option<usize> {
    let first = windows.first()?;
    if first.start != 0 {
        return None;
    }
    let at = next.text[first.clone()].find(close)?;
    if at > MAX_CARRY || next.text[..at].contains(['(', '[']) {
        return None;
    }
    Some(at + close.len_utf8())
}

/// A narrative citation split by a page break (arXiv:2602.02748 `van
/// Bevern et al.` / `(2017), …`): the byte offset in `page.text` of the
/// page's last non-blank line and the byte length of the head of `next` up
/// to and including the `)` of its leading `(year`, when the last scan
/// window reaches the page end, the next page's first window starts at its
/// top and both pieces are within [`MAX_CARRY`] bytes.
fn narrative_carry(
    page: &PageText,
    windows: &[Range<usize>],
    next: &PageText,
    next_windows: &[Range<usize>],
) -> Option<(usize, usize)> {
    let last = windows.last()?;
    let first = next_windows.first()?;
    if last.end != page.text.len() || first.start != 0 {
        return None;
    }
    let head = &next.text[first.clone()];
    let lead = head.len() - head.trim_start().len();
    if !year_paren_head_re().is_match(&head[lead..]) {
        return None;
    }
    let head_len = lead + head[lead..].find(')')? + 1;
    let tail = page.text[last.clone()].trim_end();
    let open = last.start + tail.rfind('\n').map_or(0, |i| i + 1);
    let fits = head_len <= MAX_CARRY && page.text.len() - open <= MAX_CARRY;
    fits.then_some((open, head_len))
}

/// Every marker in the `windows` of `text`, sorted by position; adjacent
/// numeric groups are merged ([`merge_adjacent`]). A numbered list gets
/// bracket groups and, on a page where no bracket group cites anything,
/// superscript runs ([`superscript_markers`]) and superscript lines of
/// their own ([`detached_superscript_markers`]); an author-year list gets
/// [`author_year_markers`].
fn scan_windows(
    text: &str,
    windows: &[Range<usize>],
    index: &RefIndex,
    ends: &[Option<usize>],
) -> Vec<Found> {
    let mut found: Vec<Found> = Vec::new();
    if index.numbered {
        let mut numeric: Vec<Found> = Vec::new();
        for window in windows {
            numeric.extend(numeric_markers(text, window, index, ends));
        }
        if numeric.is_empty() {
            for window in windows {
                found.extend(superscript_markers(text, window, index, ends));
                found.extend(detached_superscript_markers(text, window, index, ends));
            }
        }
        found.extend(numeric);
    } else {
        for window in windows {
            found.extend(author_year_markers(text, window, index));
        }
    }
    found.sort_by_key(|f| f.range.start);
    if index.numbered {
        merge_adjacent(text, found, index)
    } else {
        found
    }
}

/// In-text citation markers on every page outside the reference lists,
/// resolved against `refs`.
///
/// Numeric lists get `[1]`, `[2, 3]`, `[4–6]` and `[22, Theorem 4]`
/// markers, and superscript runs (`literature.⁵`, `Initiative⁵⁻⁷`,
/// `data⁸,⁹`) on pages without bracket markers; adjacent groups `[17],
/// [18]` and `[3]–[5]` become one marker. A group that cites `0` or a
/// number above the list, that is glued to a symbol (`W[1]-hard`, `x[2]`)
/// or that belongs to a detached label column is not a marker.
/// Author-year lists get `(Smith, 2020)`, `(Smith et al., 2020; Lee and
/// Kim, 2019)`, `(Rubin 1976, Robins et al. 1994)`, `Smith (2020)`,
/// `Politis and Romano (1994, Theorem 3.1)` and bare `Duan et al 2020`,
/// with multi-token, particle and organisation names, year lists and
/// letter lists. A group left open at a page end (`(Gui and` / `Toubia
/// 2023)`, `[6, 7,` / `41]`) is read across the page break and reported on
/// the page where it starts. Pages before the first list, the part of a
/// list's first page above its heading and the pages after a list's end
/// (an appendix) are searched. Numbered lists that restart at `[1]`
/// (`References` and `References for the Appendices`) keep separate
/// numberings: a marker before the first list's end resolves in the first
/// list, a marker after it (in the appendix that the later list serves)
/// in the later list first, each falling back to the other lists. `offset`
/// is a char offset into `PageText::text`.
pub fn find_citation_markers(pages: &[PageText], refs: &[ReferenceEntry]) -> Vec<CitationMarker> {
    if refs.is_empty() {
        return Vec::new();
    }
    let sections = find_reference_sections(pages);
    markers_in_sections(pages, refs, &sections)
}

/// [`find_citation_markers`] with the reference lists already found by
/// [`find_reference_sections`].
fn markers_in_sections(
    pages: &[PageText],
    refs: &[ReferenceEntry],
    sections: &[ReferenceSection],
) -> Vec<CitationMarker> {
    if refs.is_empty() {
        return Vec::new();
    }
    let repeated = repeated_furniture(pages);
    markers_in_sections_with_furniture(pages, refs, sections, &repeated)
}

fn markers_in_sections_with_furniture(
    pages: &[PageText],
    refs: &[ReferenceEntry],
    sections: &[ReferenceSection],
    repeated: &[String],
) -> Vec<CitationMarker> {
    if refs.is_empty() {
        return Vec::new();
    }
    let extents = list_extents(pages, sections, repeated);
    let index = RefIndex::build(refs, &extents);
    let page_windows: Vec<Vec<Range<usize>>> = pages
        .iter()
        .map(|page| page_scan_windows(page, &extents))
        .collect();
    let mut markers: Vec<CitationMarker> = Vec::new();
    // Bytes at the top of the current page already read as the end of a
    // group carried over from the page before.
    let mut carried_cut: Option<usize> = None;
    for (pos, page) in pages.iter().enumerate() {
        let ends = index.space_ends(page);
        let mut windows = page_windows[pos].clone();
        if let Some(cut) = carried_cut.take() {
            windows = subtract_range(&windows, 0, cut);
        }
        let mut found = scan_windows(&page.text, &windows, &index, &ends);
        let carry = match (pages.get(pos + 1), page_windows.get(pos + 1)) {
            (Some(next), Some(next_windows)) => open_group_at_end(page, &windows)
                .and_then(|(open, close)| {
                    carried_head(next, next_windows, close).map(|head| (open, head))
                })
                .or_else(|| {
                    if index.numbered {
                        None
                    } else {
                        narrative_carry(page, &windows, next, next_windows)
                    }
                }),
            _ => None,
        };
        if let Some((open, head)) = carry
            && let Some(next) = pages.get(pos + 1)
        {
            let tail = &page.text[open..];
            let head_text = &next.text[..head];
            let combined = format!("{tail}\n{head_text}");
            let shifted: Vec<Option<usize>> = ends
                .iter()
                .map(|end| end.map(|e| e.saturating_sub(open)))
                .collect();
            let whole: Range<usize> = 0..combined.len();
            let carried: Vec<Found> =
                scan_windows(&combined, std::slice::from_ref(&whole), &index, &shifted)
                    .into_iter()
                    .filter(|f| f.range.start < tail.len())
                    .collect();
            if !carried.is_empty() {
                found.retain(|f| f.range.start < open);
                for mut f in carried {
                    f.range = open + f.range.start..page.text.len();
                    found.push(f);
                }
                carried_cut = Some(head);
            }
        }
        let mut byte_cursor = 0usize;
        let mut char_cursor = 0usize;
        for f in found {
            if f.range.start < byte_cursor {
                continue;
            }
            char_cursor += page.text[byte_cursor..f.range.start].chars().count();
            byte_cursor = f.range.start;
            markers.push(CitationMarker {
                page: page.page,
                offset: u32::try_from(char_cursor).unwrap_or(u32::MAX),
                text: f.text,
                targets: f.targets,
            });
        }
    }
    markers
}

/// Convenience: find every reference list, segment and parse the entries
/// of each in document order (indices continue across lists), then find
/// the markers against the union. No reference list gives two empty
/// vectors.
pub fn extract_citations(pages: &[PageText]) -> (Vec<ReferenceEntry>, Vec<CitationMarker>) {
    let sections = find_reference_sections(pages);
    if sections.is_empty() {
        // Nothing to segment or resolve: skip the document-wide furniture
        // scan every list would otherwise share.
        return (Vec::new(), Vec::new());
    }
    let repeated = repeated_furniture(pages);
    let mut refs: Vec<ReferenceEntry> = Vec::new();
    for (k, section) in sections.iter().enumerate() {
        let stop = sections
            .get(k + 1)
            .map(|next| (next.first_page, next.first_line));
        for mut entry in segment_list_with_furniture(pages, section, stop, &repeated) {
            entry.index = u32::try_from(refs.len() + 1).unwrap_or(u32::MAX);
            parse_entry(&mut entry);
            refs.push(entry);
        }
    }
    let markers = markers_in_sections_with_furniture(pages, &refs, &sections, &repeated);
    (refs, markers)
}

/// Compile every regex this module uses, so the first document does not pay
/// for it inside its stage timings. Repeated calls are cheap.
pub fn warm_up() {
    let accessors: &[fn() -> &'static Regex] = &[
        heading_re,
        end_heading_re,
        caption_re,
        numeric_row_re,
        biography_re,
        entry_start_re,
        bracket_label_re,
        bare_number_label_re,
        bare_number_re,
        rsc_first_entry_re,
        dot_label_re,
        paren_label_re,
        page_number_re,
        author_start_re,
        handle_start_re,
        lncs_authors_re,
        surname_re,
        year_paren_re,
        year_bare_re,
        doi_start_re,
        year_token_re,
        leading_year_re,
        initial_token_re,
        cap_word_re,
        caps_block_re,
        and_split_re,
        et_al_tail_re,
        et_al_lead_re,
        year_lead_re,
        venue_lead_re,
        comma_venue_re,
        arxiv_re,
        url_re,
        pages_labelled_re,
        vol_issue_pages_re,
        vol_colon_pages_re,
        vol_comma_pages_re,
        vol_labelled_re,
        issue_labelled_re,
        vol_issue_re,
        vol_before_year_re,
        dash_range_re,
        in_venue_re,
        journal_venue_re,
        publisher_re,
        author_sep_re,
        initials_re,
        surname_first_re,
        vancouver_start_re,
        numeric_marker_re,
        numeric_item_re,
        narrative_marker_re,
        clause_scan_re,
        bare_et_al_re,
        year_item_re,
        superscript_marker_re,
        detached_fragment_re,
        bare_years_re,
        year_paren_head_re,
        parenthetical_re,
        clause_re,
        numbered_label_re,
        bare_label_re,
        bracket_label_re_first,
        initials_start_re,
        evidence_year_re,
        undated_marker_re,
        author_year_signature_re,
        author_list_signature_re,
        organisation_start_re,
        no_date_lead_re,
        trailing_year_re,
        trailing_paren_year_re,
        arxiv_preprint_re,
        ditto_authors_re,
        legal_title_re,
        series_volume_re,
        bare_volume_lead_re,
        bare_number_lead_re,
    ];
    for accessor in accessors {
        accessor();
    }
}

#[cfg(test)]
mod loop12_parse_tests {
    //! Loop 12 parser and segmentation fixes (`docs/analysis/refs-loop12-2026-09-28.md`,
    //! ranks 3, 4, 6, 7, 8, 9, 11, 13 and 16), on raw entries from the dev dumps.

    use super::*;

    /// `raw` parsed with the label the segmenter gave it.
    fn parse_raw(raw: &str, label: &str) -> ReferenceEntry {
        let mut entry = ReferenceEntry {
            index: 1,
            label: Some(label.to_string()),
            raw: raw.to_string(),
            page: 1,
            ..ReferenceEntry::default()
        };
        parse_entry(&mut entry);
        entry
    }

    /// The entries of a `References` section whose lines are `texts`,
    /// without layout evidence.
    fn entries_of(texts: &[&str]) -> Vec<ReferenceEntry> {
        let all: Vec<&str> = std::iter::once("References")
            .chain(texts.iter().copied())
            .collect();
        let mut page = PageText::new(1, 612.0, 792.0, 0);
        page.text = all.join("\n");
        page.lines = all
            .iter()
            .map(|text| Line {
                text: (*text).to_string(),
                bbox: None,
                column: 0,
                spans: Vec::new(),
                role: crate::schema::default_line_role(),
            })
            .collect();
        extract_citations(&[page]).0
    }

    /// Rank 4: a publisher group, a volume and page tail or a publisher
    /// after a comma is not part of the title.
    #[test]
    fn publisher_volume_and_venue_tails_leave_the_title() {
        let sutton = parse_raw(
            "Sutton RS, Barto AG (2018) Reinforcement learning: An introduction (MIT press).",
            "Sutton2018",
        );
        assert_eq!(
            sutton.title.as_deref(),
            Some("Reinforcement learning: An introduction")
        );
        assert_eq!(sutton.authors, vec!["Sutton RS", "Barto AG"]);
        assert_eq!(sutton.year, Some(2018));

        let feinberg = parse_raw(
            "[33] M. Feinberg, Foundations of chemical reaction network theory (Springer, 2019).",
            "[33]",
        );
        assert_eq!(
            feinberg.title.as_deref(),
            Some("Foundations of chemical reaction network theory")
        );

        let maass = parse_raw(
            "[9] Maass, W. & Schmitt, M. On the Complexity of Learning for Spiking Neurons with \
             Temporal Coding 153, 26–46.",
            "[9]",
        );
        assert_eq!(
            maass.title.as_deref(),
            Some("On the Complexity of Learning for Spiking Neurons with Temporal Coding")
        );

        let kothari = parse_raw(
            "[17] Kothari, S., Oh, H., 1993. Neural networks for pattern recognition, Elsevier. \
             volume 37 of Advances in Computers, pp. 119– 166. URL: \
             https://www.sciencedirect.com/science/article/pii/S0065245808604040, \
             doi:https://doi.org/10.1016/ S0065-2458(08)60404-0.",
            "[17]",
        );
        assert_eq!(
            kothari.title.as_deref(),
            Some("Neural networks for pattern recognition")
        );
        assert_eq!(kothari.year, Some(1993));

        let burkner = parse_raw(
            "[14] Paul-Christian Bürkner. 2019. thurstonianIRT: Thurstonian IRT models in R. \
             Journal of Open Source Software 4, 42 (2019), 1662.",
            "[14]",
        );
        assert_eq!(
            burkner.title.as_deref(),
            Some("thurstonianIRT: Thurstonian IRT models in R")
        );

        // Groups and ranges that belong to the title stay.
        assert_eq!(
            strip_trailing_locator("Global trends, 1950–2000"),
            "Global trends, 1950–2000"
        );
        assert_eq!(
            strip_trailing_locator("Service requirements (Release 15)"),
            "Service requirements (Release 15)"
        );
        assert_eq!(
            strip_trailing_locator("Definitions of immersive media experience (IMEx)"),
            "Definitions of immersive media experience (IMEx)"
        );
    }

    /// Ranks 6 and 13: organisation and one-word authors, entries without
    /// authors, a quoted phrase opening an unquoted title, surname-only LNCS
    /// lists and an acronym opening the title.
    #[test]
    fn titles_after_organisations_and_without_authors() {
        let openai = parse_raw(
            "[173] OpenAI, Gpt-4 technical report, arXiv preprint arXiv:2303.08774 (2023). URL \
             https://arxiv.org/abs/2303.08774",
            "[173]",
        );
        assert_eq!(openai.title.as_deref(), Some("Gpt-4 technical report"));
        assert_eq!(openai.authors, vec!["OpenAI"]);

        let kinetic = parse_raw(
            "[148] Kinetic models of collective decision-making in the presence of equality \
             bias, Physica A: Statistical Mechanics and its Applications 467 (2017) 201–217. \
             doi:https://doi.org/10.1016/j.physa.2016.10.003.",
            "[148]",
        );
        assert_eq!(
            kinetic.title.as_deref(),
            Some("Kinetic models of collective decision-making in the presence of equality bias")
        );
        assert!(kinetic.authors.is_empty());
        assert_eq!(kinetic.year, Some(2017));

        let arxiv = parse_raw("8 arXiV, https://arxiv.org/, Accessed: 2025-08-11.", "8");
        assert_eq!(arxiv.title.as_deref(), Some("arXiV"));
        assert!(arxiv.authors.is_empty());
        assert_eq!(arxiv.year, None);

        let deepseek = parse_raw(
            "50 DeepSeek-AI, DeepSeek-V3.2: Pushing the Frontier of Open Large Language Models, \
             2025, https://arxiv.org/abs/25 12.02556.",
            "50",
        );
        assert_eq!(
            deepseek.title.as_deref(),
            Some("DeepSeek-V3.2: Pushing the Frontier of Open Large Language Models")
        );
        assert_eq!(deepseek.authors, vec!["DeepSeek-AI"]);
        assert_eq!(deepseek.year, Some(2025));

        let reaxys = parse_raw(
            "13 Elsevier, Reaxys, Database, 2025, https://www.elsevier.c om/products/reaxys.",
            "13",
        );
        assert_eq!(reaxys.title.as_deref(), Some("Reaxys"));

        let mausam = parse_raw(
            "[40] Y. Nandwani, A. Pathak, Mausam, and P. Singla, A primal dual formulation for \
             deep learning with constraints, in NeurIPS, 2019.",
            "[40]",
        );
        assert_eq!(
            mausam.title.as_deref(),
            Some("A primal dual formulation for deep learning with constraints")
        );
        assert_eq!(
            mausam.authors,
            vec!["Y. Nandwani", "A. Pathak", "Mausam", "P. Singla"]
        );

        let gpp = parse_raw(
            "[47] 3GPP, “Service requirements for enhanced V2X scenarios (Release 15),” TS \
             22.186, September 2018.",
            "[47]",
        );
        assert_eq!(
            gpp.title.as_deref(),
            Some("Service requirements for enhanced V2X scenarios (Release 15)")
        );

        let hooke = parse_raw(
            "[23] R. Hooke and T. A. Jeeves, “Direct search” solution of numerical and \
             statistical problems, J. ACM, 8 (1961), pp. 212–229.",
            "[23]",
        );
        assert_eq!(
            hooke.title.as_deref(),
            Some("“Direct search” solution of numerical and statistical problems")
        );

        let galun = parse_raw(
            "7. Galun, Sharon, Basri, Brandt: Texture segmentation by multiscale aggregation of \
             filter responses and shape elements. In: Proceedings Ninth IEEE international \
             conference on computer vision, pp. 716–723. IEEE (2003)",
            "7.",
        );
        assert_eq!(
            galun.title.as_deref(),
            Some(
                "Texture segmentation by multiscale aggregation of filter responses and shape \
                 elements"
            )
        );

        let weiss = parse_raw(
            "[3] Weiss, G. WISDM Smartphone and Smartwatch Activity and Biometrics Dataset . UCI \
             Machine Learning Repository (2019). DOI: https://doi.org/10.24432/C5HK59.",
            "[3]",
        );
        assert_eq!(
            weiss.title.as_deref(),
            Some("WISDM Smartphone and Smartwatch Activity and Biometrics Dataset")
        );
    }

    /// Rank 7: a title goes on after an inner comma when a one-word title or
    /// subtitle meets a part that is no venue, after a period before a
    /// lowercase clause, and after a quoted phrase that a subtitle follows.
    #[test]
    fn titles_run_past_inner_commas_periods_and_quotes() {
        let redmon = parse_raw(
            "[66] J. Redmon, S. Divvala, R. Girshick, A. Farhadi, You Only Look Once: Unified, \
             Real-Time Object Detection, in: 2016 IEEE Conference on Computer Vision and Pattern \
             Recognition (CVPR), 2016, pp. 779–788. doi: 10.1109/CVPR.2016.91.",
            "[66]",
        );
        assert_eq!(
            redmon.title.as_deref(),
            Some("You Only Look Once: Unified, Real-Time Object Detection")
        );

        let sparsity = parse_raw(
            "[18] J. Neetil, P. O. de Mendez, Sparsity: Graphs, Structures, and Algorithms, \
             Springer Publishing Company, Incorporated, 2012.",
            "[18]",
        );
        assert_eq!(
            sparsity.title.as_deref(),
            Some("Sparsity: Graphs, Structures, and Algorithms")
        );

        let minors = parse_raw(
            "[12] N. Robertson, P. Seymour, Graph minors. iii. planar tree-width, Journal of \
             Combinatorial Theory, Series B 36 (1) (1984) 49 – 64.",
            "[12]",
        );
        assert_eq!(
            minors.title.as_deref(),
            Some("Graph minors. iii. planar tree-width")
        );

        let kestemont = parse_raw(
            "Mike Kestemont. 2014. Function words in authorship attribution. from black magic to \
             theory? In Proceedings of the 3rd Workshop on Computational Linguistics for \
             Literature (CLFL).",
            "Mike2014",
        );
        assert_eq!(
            kestemont.title.as_deref(),
            Some("Function words in authorship attribution. from black magic to theory?")
        );

        let white = parse_raw(
            "Jocelyn White, Wendy Levinson, and Debra Roter. 1994. Oh, by the way.. . the \
             closing moments of the medical visit. Journal of General Internal Medicine, \
             9(1):24–28.",
            "Jocelyn1994",
        );
        assert_eq!(
            white.title.as_deref(),
            Some("Oh, by the way.. . the closing moments of the medical visit")
        );

        let jiang = parse_raw(
            "[24] Lucy Jiang, Crescentia Jung, Mahika Phutane, Abigale Stangl, and Shiri \
             Azenkot. 2024. “It’s Kind of Context Dependent”: Understanding Blind and Low Vision \
             People’s Video Accessibility Preferences Across Viewing Scenarios. In Proceedings of \
             the 2024 CHI Conference on Human Factors in Computing Systems. doi:10.1145/ \
             3613904.3642238",
            "[24]",
        );
        assert_eq!(
            jiang.title.as_deref(),
            Some(
                "“It’s Kind of Context Dependent”: Understanding Blind and Low Vision People’s \
                 Video Accessibility Preferences Across Viewing Scenarios"
            )
        );

        assert_eq!(
            find_quoted("“It’s Kind of Context Dependent”: Understanding Blind"),
            None
        );
        assert_eq!(
            find_quoted("“Attention is all you need,” in Proc.").map(|(_, text)| text),
            Some("Attention is all you need".to_string())
        );
        let minors_title = "Graph minors. iii. planar tree-width";
        assert_eq!(title_end(minors_title), minors_title.len());
        assert_eq!(
            title_end("Deep widgets. arXiv preprint"),
            "Deep widgets".len()
        );
        assert_eq!(title_end("Mastering go. nature, 529"), "Mastering go".len());
    }

    /// Rank 8: entries the evaluation could not pair get sane fields.
    #[test]
    fn entries_the_evaluation_could_not_pair() {
        let eds = parse_raw(
            "[297] E. Alonso, D. Kudenko, D. Kazakov (Eds.), Adaptive agents and multi-agent \
             systems: adaptation and multiagent learning, Springer-Verlag, Berlin, Heidelberg, \
             2003.",
            "[297]",
        );
        assert_eq!(
            eds.title.as_deref(),
            Some("Adaptive agents and multi-agent systems: adaptation and multiagent learning")
        );
        assert_eq!(eds.authors, vec!["E. Alonso", "D. Kudenko", "D. Kazakov"]);

        let ncbi = parse_raw(
            "[29] National Center for Biotechnology Information (NCBI), 1988–a. Ncbi genome \
             database. URL: https://www.ncbi.nlm.nih.gov/ datasets/genome/. [cited 2025 Jan 16].",
            "[29]",
        );
        assert_eq!(ncbi.year, Some(1988));
        assert_eq!(ncbi.title.as_deref(), Some("Ncbi genome database"));
        assert_eq!(
            ncbi.authors,
            vec!["National Center for Biotechnology Information (NCBI)"]
        );

        let rohatgi = parse_raw(
            "55 A. Rohatgi, WebPlotDigitizer, https://automeris.io.",
            "55",
        );
        assert_eq!(rohatgi.title.as_deref(), Some("WebPlotDigitizer"));
        assert_eq!(rohatgi.url.as_deref(), Some("https://automeris.io"));

        let park = parse_raw(
            "[85] M.-J. Park, H. S. Cho, T.-S. Yun, S. Byeon, Y. J. Koo, S. Yoon, D. U. Lee, S. \
             Choi, J. Park, J. Lee, K. Cho, J. Moon, B.-K. Yoon, Y.-J. Park, S.-m. Oh, C. K. \
             Lee, T.-K. Kim, S.-H. Lee, H.-W. Kim, Y. Ju, S.-K. Lim, S. G. Baek, K. Y. Lee, S. H. \
             Lee, W. S. We, S. Kim, Y. Choi, S.-H. Lee, S. M. Yang, G. Lee, I.-K. Kim, Y. Jeon, \
             J.-H. Park, J. C. Yun, C. Park, S.-Y. Kim, S. Kim, D.-Y. Lee, S.-H. Oh, T. Hwang, J. \
             Shin, Y. Lee, H. Kim, J. Lee, Y. Hur, S. Lee, J. Jang, J. Chun, and J. Cho, “A \
             192-Gb 12-High 896-GB/s HBM3 DRAM with a TSV Auto-Calibration Scheme and \
             Machine-Learning-Based Layout Optimization,” in ISSCC, 2022.",
            "[85]",
        );
        assert_eq!(
            park.title.as_deref(),
            Some(
                "A 192-Gb 12-High 896-GB/s HBM3 DRAM with a TSV Auto-Calibration Scheme and \
                 Machine-Learning-Based Layout Optimization"
            )
        );
        assert_eq!(park.authors.len(), 49);
        assert_eq!(park.authors[0], "M.-J. Park");
        assert_eq!(park.authors[48], "J. Cho");
        assert_eq!(park.year, Some(2022));

        let oneformer = parse_raw("[1] https://github.com/SHI- Labs/OneFormer, 2023. 3", "[1]");
        assert_eq!(oneformer.title, None);
        assert_eq!(
            oneformer.url.as_deref(),
            Some("https://github.com/SHI-Labs/OneFormer")
        );
        assert_eq!(oneformer.year, Some(2023));
    }

    /// Rank 9: RSC entries print no title; the parser does not invent one
    /// from an abbreviated journal, a volume, a thesis note or a workshop.
    #[test]
    fn rsc_entries_without_a_printed_title() {
        let aiche = parse_raw(
            "68 Y. Yi, L. Wang, Y. Guo, S. Sun and H. Guo, AIChE J., 2019, 65, 691–701.",
            "68",
        );
        assert_eq!(aiche.title, None);
        assert_eq!(aiche.volume.as_deref(), Some("65"));
        assert_eq!(aiche.year, Some(2019));

        let chem = parse_raw(
            "56 X. Duan, G. Qian, X. Zhou, D. Chen and W. Yuan, Chem. Eng. J., 2012, 207-208, \
             103–108.",
            "56",
        );
        assert_eq!(chem.title, None);

        let int = parse_raw(
            "76 D. Varisli and T. Rona, Int. J. Chem. React. Eng., 2012, 10,.",
            "76",
        );
        assert_eq!(int.title, None);

        let iscience = parse_raw(
            "85 I. Lucentini, I. Serrano, X. Garcia, A. GarzÃşn ManjÃşn, X. Hu, J. Arbiol, L. \
             Pascua-SolÃl’, J. Prat, E. E. Villalobos-Portillo, C. Marini, C. Escudero and J. \
             Llorca, iScience, 2024, 27, 110028.",
            "85",
        );
        assert_eq!(iscience.title, None);
        assert_eq!(iscience.volume.as_deref(), Some("27"));

        let thesis = parse_raw(
            "16 D. M. Lowe, PhD thesis, University of Cambridge, 2012.",
            "16",
        );
        assert_eq!(thesis.title, None);

        let workshop = parse_raw(
            "40 E. Pan, T. Prein, J. Nam, X. Du, S. Yang, P. Cai, J. Rupp, R. Gomez-Bombarelli \
             and E. Olivetti, ICLR 2026 Workshop on AI for Accelerated Materials Design \
             (AI4MAT), 2026.",
            "40",
        );
        assert_eq!(workshop.title, None);
        assert_eq!(workshop.year, Some(2026));
    }

    /// Rank 13: author lists that ended early (a publisher-group year, a
    /// two-word Vancouver surname, a full first name before initials-first
    /// names, `Md.`).
    #[test]
    fn author_lists_that_ended_early() {
        let gilsinn = parse_raw(
            "[21] Gilsinn, D. E., Kalmár-Nagy, T. & Balachandran, B. Delay Differential \
             Equations (Springer, 2009).",
            "[21]",
        );
        assert_eq!(
            gilsinn.title.as_deref(),
            Some("Delay Differential Equations")
        );
        assert_eq!(gilsinn.year, Some(2009));

        let van_roy = parse_raw(
            "Tsitsiklis J, Van Roy B (1996) Analysis of temporal-difference learning with \
             function approximation. Advances in neural information processing systems 9.",
            "Tsitsiklis1996",
        );
        assert_eq!(
            van_roy.title.as_deref(),
            Some("Analysis of temporal-difference learning with function approximation")
        );
        assert_eq!(van_roy.authors, vec!["Tsitsiklis J", "Van Roy B"]);

        let mudgel = parse_raw(
            "[77] Monika Mudgel, V. P. S. Awana, R. Lal, H. Kishan, L. S. Sharath Chandra, V. \
             Ganesan, A. V. Narlikar, and G. L. Bhalla. Anomalous thermoelectric power of the Mg \
             1−𝑥Al𝑥B2 system with 𝑥 = 0.0–1.0. J. Phys.: Condens. Matter, 20(9):095205, \
             February 2008.",
            "[77]",
        );
        assert_eq!(
            mudgel.title.as_deref(),
            Some("Anomalous thermoelectric power of the Mg 1−𝑥Al𝑥B2 system with 𝑥 = 0.0–1.0")
        );
        assert_eq!(mudgel.authors.len(), 8);
        assert_eq!(mudgel.authors[4], "L. S. Sharath Chandra");

        let usman = parse_raw(
            "[57] Mohammad Usman, Ahsan Ali, Abdesslem Jedidi, Afnan Ajeebi, Mohammad Mozahar \
             Hossain, Khalifa M. Yau, Huda Alghamdi, Md. Abdul Aziz, and M. Nasiruzzaman Shaikh. \
             Rare earth metal promoters (La, Ce, Nd, Sm) on nickel-supported Al2O3 catalysts for \
             ammonia decomposition. Fuel, 396:135272, 2025.",
            "[57]",
        );
        assert_eq!(
            usman.title.as_deref(),
            Some(
                "Rare earth metal promoters (La, Ce, Nd, Sm) on nickel-supported Al2O3 catalysts \
                 for ammonia decomposition"
            )
        );
        assert_eq!(usman.authors.len(), 9);
        assert_eq!(usman.authors[7], "Md. Abdul Aziz");
    }

    /// Rank 16: an elided-particle surname and an organisation-led
    /// semicolon list open entries; an appendix title ends the list.
    #[test]
    fn entry_starts_and_list_ends() {
        let refs = entries_of(&[
            "Dellarocas C, Wood CA (2008) The sound of silence in online feedback: Estimating \
             trading risks in the",
            "presence of reporting bias. Management science 54(3):460–476.",
            "d’Haultfoeuille X (2010) A new instrumental method for dealing with endogenous \
             selection. Journal of",
            "Econometrics 154(1):1–15.",
            "Fang Z, Santos A (2019) Inference on directionally differentiable functions. The \
             Review of Economic Studies",
            "86(1):377–412.",
        ]);
        assert_eq!(refs.len(), 3);
        assert!(refs[1].raw.starts_with("d’Haultfoeuille X (2010)"));
        assert!(refs[1].raw.ends_with("154(1):1–15."));

        let refs = entries_of(&[
            "DeepSeek-AI. 2025. DeepSeek-R1-Distill-Qwen-32B: A 32",
            "B model distilled from DeepSeek-R1 with state-of-the-art",
            "reasoning performance.",
            "Model card on Hugging Face /",
            "DeepSeek Platform. Achieves 72.6",
            "DeepSeek-AI; Liu, A.; Feng, B.; Xue, B.; Wang, B.;",
            "Zhao, C.; et al. 2024.",
            "DeepSeek-V3 Technical Report.",
        ]);
        assert_eq!(refs.len(), 2);
        assert!(refs[0].raw.ends_with("Achieves 72.6"));
        assert!(refs[1].raw.starts_with("DeepSeek-AI; Liu, A.;"));

        let refs = entries_of(&[
            "Yuanzhi Zhu, Kai Zhang, Jingyun Liang, Jiezhang Cao, Bihan Wen, Radu Timofte, and \
             Luc Van Gool.",
            "Denoising diffusion models for plug-and-play image restoration. In Proceedings of \
             the IEEE/CVF Conference",
            "on Computer Vision and Pattern Recognition, pp. 1219–1229, 2023.",
            "Rayhan Zirvi, Bahareh Tolooshams, and Anima Anandkumar. Diffusion state-guided \
             projected gradient for",
            "inverse problems. In The Thirteenth International Conference on Learning \
             Representations, 2025. URL",
            "https://openreview.net/forum?id=kRBQwlkFSP.",
            "Appendices for “EquiReg: Equivariance Regularized Diffusion for Inverse Problems”",
            "These supplementary materials contain the following:",
            "• Section A includes additional experiments on text-to-image guidance. We \
             regularize DreamSampler (Kim",
            "et al., 2024) with EquiReg for an improved performance (see Figures 7 to 11).",
        ]);
        assert_eq!(refs.len(), 2);
        assert!(refs[1].raw.ends_with("forum?id=kRBQwlkFSP."));
        assert!(!refs[1].raw.contains("Appendices"));
    }

    /// Ranks 3 and 11 (`hyphen_break` part): a title-case compound keeps
    /// its hyphen, a word set in capitals is joined, acronyms stay apart.
    #[test]
    fn capitals_and_title_case_compounds_at_line_ends() {
        assert_eq!(
            hyphen_break(
                "DECOUPLING REASONING FROM OBSERVATIONS FOR EFFI-",
                "CIENT AUGMENTED LANGUAGE MODELS.",
                ""
            ),
            HyphenJoin::Drop
        );
        assert_eq!(
            hyphen_break(
                "Manaal Faruqui. 2021. TIME-",
                "DIAL: Temporal commonsense",
                ""
            ),
            HyphenJoin::Drop
        );
        assert_eq!(
            hyphen_break("Localization and GNSS (ICL-", "GNSS), IEEE, 2015", ""),
            HyphenJoin::Keep
        );
        assert_eq!(
            hyphen_break("SYSTEMS OF MULTI-", "AGENT SYSTEMS", "agent systems"),
            HyphenJoin::Keep
        );
        assert_eq!(
            hyphen_break("Challenges in Multi-", "turn dialogue", ""),
            HyphenJoin::Keep
        );
        assert_eq!(
            hyphen_break("Deep Gen-", "erative models", ""),
            HyphenJoin::Drop
        );
        assert_eq!(hyphen_break("Deep Learn-", "ing", ""), HyphenJoin::Drop);
        assert_eq!(
            hyphen_break("Dual-", "channel attention", ""),
            HyphenJoin::Keep
        );
        // A capitalised half outside the compound prefixes is a word break
        // unless the hyphenated pair is attested.
        assert_eq!(hyphen_break("Every-", "body counts", ""), HyphenJoin::Drop);
        assert_eq!(
            hyphen_break("Every-", "body counts", "every-body"),
            HyphenJoin::Keep
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            role: crate::schema::default_line_role(),
        }
    }

    /// Lines without layout evidence.
    fn bare_line(text: &str) -> Line {
        Line {
            text: text.to_string(),
            bbox: None,
            column: 0,
            spans: Vec::new(),
            role: crate::schema::default_line_role(),
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

    /// A `References` line in a table of contents has no entry after it and
    /// is not a list heading.
    #[test]
    fn table_of_contents_heading_is_skipped() {
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
        let index = RefIndex::build(&refs, &[]);
        assert_eq!(index.resolve_author_year("Smith", 2020, "b"), vec![2]);
        assert_eq!(index.resolve_author_year("Smith", 2020, ""), vec![1, 2]);
        assert!(index.resolve_author_year("Jones", 2020, "").is_empty());
    }

    // ---- Real-data tests: verbatim snippets from the evaluation corpus. ----

    /// Segment and parse `texts` as one column page under a `References`
    /// heading, without layout evidence for the entry starts.
    fn refs_from_lines(texts: &[&str]) -> Vec<ReferenceEntry> {
        let mut lines: Vec<Line> = vec![bare_line("References")];
        lines.extend(texts.iter().map(|t| bare_line(t)));
        let page = page_of(1, lines);
        let (refs, _) = extract_citations(&[page]);
        refs
    }

    fn doi_of(raw: &str) -> Option<String> {
        find_doi(raw).map(|(_, doi)| doi)
    }

    /// Elsevier style (arXiv:2305.13843): initials-first authors, then the
    /// unquoted title after a comma, then the journal with `35 (1992) 61–70`
    /// or `in: Proceedings ..., volume 34, 2020, pp. 8936–8943`.
    #[test]
    fn elsevier_comma_delimited_titles() {
        let refs = refs_from_lines(&[
            "[1] D. Goldberg, D. Nichols, B. M. Oki, D. Terry, Using collaborative",
            "ﬁltering to weave an information tapestry, Communications of the",
            "ACM 35 (1992) 61–70.",
            "[2] T. Sun, Y. Shao, X. Li, P. Liu, H. Yan, X. Qiu, X. Huang, Learning",
            "sparse sharing architectures for multiple tasks, in: Proceedings of",
            "the AAAI conference on artiﬁcial intelligence, volume 34, 2020, pp.",
            "8936–8943.",
        ]);
        assert_eq!(refs.len(), 2);
        assert_eq!(
            refs[0].title.as_deref(),
            Some("Using collaborative ﬁltering to weave an information tapestry")
        );
        assert_eq!(
            refs[0].authors,
            vec!["D. Goldberg", "D. Nichols", "B. M. Oki", "D. Terry"]
        );
        assert_eq!(refs[0].venue.as_deref(), Some("Communications of the ACM"));
        assert_eq!(refs[0].volume.as_deref(), Some("35"));
        assert_eq!(refs[0].pages.as_deref(), Some("61–70"));
        assert_eq!(refs[0].year, Some(1992));
        assert_eq!(
            refs[1].title.as_deref(),
            Some("Learning sparse sharing architectures for multiple tasks")
        );
        assert_eq!(refs[1].authors.len(), 7);
        assert_eq!(refs[1].authors[6], "X. Huang");
        assert_eq!(
            refs[1].venue.as_deref(),
            Some("Proceedings of the AAAI conference on artiﬁcial intelligence")
        );
        assert_eq!(refs[1].volume.as_deref(), Some("34"));
        assert_eq!(refs[1].pages.as_deref(), Some("8936–8943"));
        assert_eq!(refs[1].year, Some(2020));
    }

    /// SIAM style (arXiv:2603.21379): title after the author comma, a comma
    /// inside a title, `Journal, 46 (2020)`, and the DOI as a trailing URL.
    /// `De-` + `composition` is closed up because `Decomposition` occurs
    /// elsewhere in the section; `Univer-` + `sity` because nothing says
    /// otherwise.
    #[test]
    fn siam_comma_delimited_titles_with_dois() {
        let refs = refs_from_lines(&[
            "[1] S. Ahmadi-Asl, S. Abukhovich, M. G. Asante-Mensah, A. Cichocki, A. H. Phan,",
            "T. Tanaka, and I. Oseledets, Randomized Algorithms for Computation of Tucker De-",
            "composition and Higher Order SVD (HOSVD), IEEE Access, 9 (2021), pp. 28684–28706,",
            "https://doi.org/10.1109/ACCESS.2021.3058103.",
            "[2] G. Ballard, A. Klinvex, and T. G. Kolda, TuckerMPI: A Parallel C++/MPI Software",
            "Package for Large-scale Data Compression via the Tucker Tensor Decomposition, ACM",
            "Transactions on Mathematical Software, 46 (2020), https://doi.org/10.1145/3378445.",
            "[3] G. Ballard and T. G. Kolda, Tensor Decompositions for Data Science, Cambridge Univer-",
            "sity Press, 2025, https://doi.org/10.1017/9781009471664.",
            "[4] C. Boutsidis and D. P. Woodruff, Optimal CUR Matrix Decompositions, SIAM Journal on",
            "Computing, 46 (2017), pp. 543–589, https://doi.org/10.1137/140977898.",
        ]);
        assert_eq!(refs.len(), 4);
        assert_eq!(
            refs[0].title.as_deref(),
            Some(
                "Randomized Algorithms for Computation of Tucker Decomposition and Higher \
                 Order SVD (HOSVD)"
            )
        );
        assert_eq!(refs[0].authors.len(), 7);
        assert_eq!(refs[0].authors[0], "S. Ahmadi-Asl");
        assert_eq!(refs[0].authors[6], "I. Oseledets");
        assert_eq!(refs[0].doi.as_deref(), Some("10.1109/ACCESS.2021.3058103"));
        assert_eq!(refs[0].venue.as_deref(), Some("IEEE Access"));
        assert_eq!(refs[0].volume.as_deref(), Some("9"));
        assert_eq!(refs[0].pages.as_deref(), Some("28684–28706"));
        assert_eq!(refs[0].year, Some(2021));
        assert_eq!(
            refs[1].title.as_deref(),
            Some(
                "TuckerMPI: A Parallel C++/MPI Software Package for Large-scale Data \
                 Compression via the Tucker Tensor Decomposition"
            )
        );
        assert_eq!(refs[1].doi.as_deref(), Some("10.1145/3378445"));
        assert_eq!(
            refs[1].venue.as_deref(),
            Some("ACM Transactions on Mathematical Software")
        );
        assert_eq!(refs[1].volume.as_deref(), Some("46"));
        assert_eq!(
            refs[2].title.as_deref(),
            Some("Tensor Decompositions for Data Science")
        );
        assert_eq!(refs[2].authors, vec!["G. Ballard", "T. G. Kolda"]);
        assert_eq!(refs[2].venue.as_deref(), Some("Cambridge University Press"));
        assert_eq!(refs[2].doi.as_deref(), Some("10.1017/9781009471664"));
        assert_eq!(refs[2].year, Some(2025));
        assert_eq!(
            refs[3].title.as_deref(),
            Some("Optimal CUR Matrix Decompositions")
        );
        assert_eq!(refs[3].venue.as_deref(), Some("SIAM Journal on Computing"));
        assert_eq!(refs[3].pages.as_deref(), Some("543–589"));
        assert_eq!(refs[3].doi.as_deref(), Some("10.1137/140977898"));
    }

    /// SIAM style with `in VENUE, year` and `Journal, 22 (2022), pp.`
    /// (arXiv:2504.09409).
    #[test]
    fn siam_titles_before_in_venue() {
        let refs = refs_from_lines(&[
            "[1] A. Alacaoglu and S. J. Wright, Complexity of single loop algorithms for nonlinear programming",
            "with stochastic objective and constraints, in AISTATS, 2024.",
            "[2] K. Balasubramanian and S. Ghadimi, Zeroth-order (non)-convex stochastic optimization via con-",
            "ditional gradient and gradient updates, in NeurIPS, 2018.",
            "[3] K. Balasubramanian and S. Ghadimi, Zeroth-order nonconvex stochastic optimization: Handling",
            "constraints, high-dimensionality and saddle-points, Found. Comput. Math., 22 (2022), pp. 35–76.",
            "[4] A. Beck, First-order methods in optimization, SIAM, 2017.",
        ]);
        assert_eq!(refs.len(), 4);
        assert_eq!(
            refs[0].title.as_deref(),
            Some(
                "Complexity of single loop algorithms for nonlinear programming with \
                 stochastic objective and constraints"
            )
        );
        assert_eq!(refs[0].authors, vec!["A. Alacaoglu", "S. J. Wright"]);
        assert_eq!(refs[0].venue.as_deref(), Some("AISTATS"));
        assert_eq!(refs[0].year, Some(2024));
        assert_eq!(
            refs[1].title.as_deref(),
            Some(
                "Zeroth-order (non)-convex stochastic optimization via conditional gradient \
                 and gradient updates"
            )
        );
        assert_eq!(refs[1].venue.as_deref(), Some("NeurIPS"));
        assert_eq!(
            refs[2].title.as_deref(),
            Some(
                "Zeroth-order nonconvex stochastic optimization: Handling constraints, \
                 high-dimensionality and saddle-points"
            )
        );
        assert_eq!(refs[2].venue.as_deref(), Some("Found. Comput. Math."));
        assert_eq!(refs[2].volume.as_deref(), Some("22"));
        assert_eq!(refs[2].pages.as_deref(), Some("35–76"));
        assert_eq!(refs[2].year, Some(2022));
        assert_eq!(
            refs[3].title.as_deref(),
            Some("First-order methods in optimization")
        );
        assert_eq!(refs[3].authors, vec!["A. Beck"]);
        assert_eq!(refs[3].venue.as_deref(), Some("SIAM"));
        assert_eq!(refs[3].year, Some(2017));
    }

    /// ACM style (arXiv:2412.06210): `Authors. 2019. Title. Venue (2019)`;
    /// the year sentence after the authors is not the title, and the
    /// parenthesised year at the end does not win over it.
    #[test]
    fn acm_year_sentence_between_authors_and_title() {
        let refs = refs_from_lines(&[
            "[1] Alham Fikri Aji and Kenneth Heaﬁeld. 2017. Sparse communication for dis-",
            "tributed gradient descent. arXiv preprint arXiv:1704.05021 (2017).",
            "[2] Leonidas G Anthopoulos. 2015. Understanding the smart city domain: A literature",
            "review. Transforming city governments for successful smart cities (2015), 9–21.",
        ]);
        assert_eq!(refs.len(), 2);
        assert_eq!(
            refs[0].title.as_deref(),
            Some("Sparse communication for distributed gradient descent")
        );
        assert_eq!(refs[0].authors, vec!["Alham Fikri Aji", "Kenneth Heaﬁeld"]);
        assert_eq!(refs[0].year, Some(2017));
        assert_eq!(refs[0].arxiv_id.as_deref(), Some("1704.05021"));
        assert_eq!(
            refs[1].title.as_deref(),
            Some("Understanding the smart city domain: A literature review")
        );
        assert_eq!(refs[1].authors, vec!["Leonidas G Anthopoulos"]);
        assert_eq!(refs[1].year, Some(2015));
        assert_eq!(refs[1].pages.as_deref(), Some("9–21"));

        let entry = parsed(
            "[40] Qiang Yang, Yang Liu, Tianjian Chen, and Yongxin Tong. 2019. Federated \
             machine learning: Concept and applications. ACM Transactions on Intelligent \
             Systems and Technology (TIST) 10, 2 (2019), 1–19.",
            Some("[40]"),
        );
        assert_eq!(
            entry.title.as_deref(),
            Some("Federated machine learning: Concept and applications")
        );
        assert_eq!(
            entry.authors,
            vec!["Qiang Yang", "Yang Liu", "Tianjian Chen", "Yongxin Tong"]
        );
        assert_eq!(entry.year, Some(2019));
        assert_eq!(
            entry.venue.as_deref(),
            Some("ACM Transactions on Intelligent Systems and Technology")
        );
    }

    /// DOIs broken by line wraps (arXiv:2108.04588, 2305.13843, 2509.10402,
    /// 2501.17300): after `/`, after `10.`, after `.`, mid-number inside a
    /// `doi.org` URL, after `)`; a sentence or a year after the DOI is not
    /// glued on.
    #[test]
    fn doi_closed_up_across_line_wraps() {
        assert_eq!(
            doi_of(
                "[7] Sergio Cabello and Miha Jejčič. Reﬁning the hierarchies of classes of \
                 geometric intersection graphs. Electronic Journal of Combinatorics, \
                 24(1):P1.33, 19 pp., 2017. doi:10.37236/ 6040."
            )
            .as_deref(),
            Some("10.37236/6040")
        );
        assert_eq!(
            doi_of("Mathematische Zeitschrift, 17:228–249, 1923. doi:10.1007/ BF01504345.")
                .as_deref(),
            Some("10.1007/BF01504345")
        );
        assert_eq!(
            doi_of("Discovery & Data Mining, 2019, pp. 1123–1131. doi: 10. 1145/3292500.3330861.")
                .as_deref(),
            Some("10.1145/3292500.3330861")
        );
        assert_eq!(
            doi_of("Journal of Combinatorial Theory, Series B, 103(1):114–143, 2013. doi:10.1016/j.jctb. 2012.09.004.")
                .as_deref(),
            Some("10.1016/j.jctb.2012.09.004")
        );
        assert_eq!(
            doi_of(
                "2024, p. 227–230. [Online]. Available: https://doi.org/10.1145/364399 1.3648400"
            )
            .as_deref(),
            Some("10.1145/3643991.3648400")
        );
        assert_eq!(
            doi_of(
                "Discrete Mathematics, 262(1–3):221–227, 2003. doi:10.1016/S0012-365X(02) 00501-0."
            )
            .as_deref(),
            Some("10.1016/S0012-365X(02)00501-0")
        );
        assert_eq!(
            doi_of("Oxford University Press. doi: 10.1093/acprof:oso/9780199591565. 001.0001.")
                .as_deref(),
            Some("10.1093/acprof:oso/9780199591565.001.0001")
        );
        assert_eq!(
            doi_of("doi:10.1000/abc. Accessed 12 March 2021.").as_deref(),
            Some("10.1000/abc")
        );
        assert_eq!(
            doi_of("doi:10.1000/xyz. 2020.").as_deref(),
            Some("10.1000/xyz")
        );
        assert_eq!(doi_of("see 10.1234/ and nothing"), None);

        // The masked range covers the whole wrapped DOI, so no piece of it
        // is read as a volume or page number.
        let entry = parsed(
            "[15] Mihály Fekete. Über die Verteilung der Wurzeln bei gewissen algebraischen \
             Gleichungen mit ganzzahligen Koeﬃzienten. Mathematische Zeitschrift, \
             17:228–249, 1923. doi:10.1007/ BF01504345.",
            Some("[15]"),
        );
        assert_eq!(entry.doi.as_deref(), Some("10.1007/BF01504345"));
        assert_eq!(entry.volume.as_deref(), Some("17"));
        assert_eq!(entry.pages.as_deref(), Some("228–249"));
        assert_eq!(entry.year, Some(1923));
        assert_eq!(entry.venue.as_deref(), Some("Mathematische Zeitschrift"));
    }

    /// `Brent N. Clark` is a middle initial, not Vancouver `Smith AB`
    /// (arXiv:2108.04588 [13]).
    #[test]
    fn middle_initial_is_not_vancouver() {
        let entry = parsed(
            "[13] Brent N. Clark, Charles J. Colbourn, and David S. Johnson. Unit disk graphs. \
             Discrete Mathematics, 86(1–3):165–177, 1990. doi:10.1016/0012-365X(90)90358-O.",
            Some("[13]"),
        );
        assert_eq!(
            entry.authors,
            vec!["Brent N. Clark", "Charles J. Colbourn", "David S. Johnson"]
        );
        assert_eq!(entry.title.as_deref(), Some("Unit disk graphs"));
        assert_eq!(entry.venue.as_deref(), Some("Discrete Mathematics"));
        assert_eq!(entry.volume.as_deref(), Some("86"));
        assert_eq!(entry.issue.as_deref(), Some("1–3"));
        assert_eq!(entry.pages.as_deref(), Some("165–177"));
        assert_eq!(entry.year, Some(1990));
        assert_eq!(entry.doi.as_deref(), Some("10.1016/0012-365X(90)90358-O"));
    }

    /// An apostrophe inside `“...”` does not close the quoted title, and the
    /// `doi.org` URL after `[Online]. Available:` is read (arXiv:2509.10402).
    #[test]
    fn quoted_title_keeps_inner_apostrophe() {
        let refs = refs_from_lines(&[
            "[57] H. Hao, K. A. Hasan, H. Qin, M. Macedo, Y. Tian, S. H. H.",
            "Ding, and A. E. Hassan, “An empirical study on developers’ shared",
            "conversations with chatgpt in github pull requests and issues,”",
            "Empirical Softw. Engg., vol. 29, no. 6, Sep. 2024. [Online]. Available:",
            "https://doi.org/10.1007/s10664-024-10540-x",
        ]);
        assert_eq!(refs.len(), 1);
        assert_eq!(
            refs[0].title.as_deref(),
            Some(
                "An empirical study on developers’ shared conversations with chatgpt in \
                 github pull requests and issues"
            )
        );
        assert_eq!(refs[0].authors.len(), 7);
        assert_eq!(refs[0].authors[5], "S. H. H. Ding");
        assert_eq!(refs[0].venue.as_deref(), Some("Empirical Softw. Engg."));
        assert_eq!(refs[0].volume.as_deref(), Some("29"));
        assert_eq!(refs[0].issue.as_deref(), Some("6"));
        assert_eq!(refs[0].year, Some(2024));
        assert_eq!(refs[0].doi.as_deref(), Some("10.1007/s10664-024-10540-x"));
        assert_eq!(
            refs[0].url.as_deref(),
            Some("https://doi.org/10.1007/s10664-024-10540-x")
        );
    }

    /// IEEE books (arXiv:2503.15734): `H. Khalil, Nonlinear Systems. Publisher`
    /// and `E. D. Sontag, Contractive Systems with Inputs, pp. 217–228.`; a
    /// quoted title broken as `sta-` + `bilization` is closed up.
    #[test]
    fn ieee_book_titles_before_publisher() {
        let refs = refs_from_lines(&[
            "[2] M. Jankovic, “Robust control barrier functions for constrained sta-",
            "bilization of nonlinear systems,” Automatica, vol. 96, pp. 359–367,",
            "2018.",
        ]);
        assert_eq!(refs.len(), 1);
        assert_eq!(
            refs[0].title.as_deref(),
            Some(
                "Robust control barrier functions for constrained stabilization of nonlinear systems"
            )
        );
        assert_eq!(refs[0].venue.as_deref(), Some("Automatica"));
        assert_eq!(refs[0].volume.as_deref(), Some("96"));
        assert_eq!(refs[0].pages.as_deref(), Some("359–367"));
        assert_eq!(refs[0].year, Some(2018));

        let khalil = parsed(
            "[21] H. Khalil, Nonlinear Systems. Pearson Education, Prentice Hall, 2 ed., 2002.",
            Some("[21]"),
        );
        assert_eq!(khalil.title.as_deref(), Some("Nonlinear Systems"));
        assert_eq!(khalil.authors, vec!["H. Khalil"]);
        assert_eq!(khalil.year, Some(2002));

        let sontag = parsed(
            "[24] E. D. Sontag, Contractive Systems with Inputs, pp. 217–228. Berlin, \
             Heidelberg: Springer Berlin Heidelberg, 2010.",
            Some("[24]"),
        );
        assert_eq!(
            sontag.title.as_deref(),
            Some("Contractive Systems with Inputs")
        );
        assert_eq!(sontag.authors, vec!["E. D. Sontag"]);
        assert_eq!(sontag.pages.as_deref(), Some("217–228"));
        assert_eq!(sontag.year, Some(2010));
    }

    /// `et al.` closes the author list when the title follows it directly,
    /// and a quotation inside an unquoted title is not the title
    /// (arXiv:2511.13979, alpha style without hanging indent evidence).
    #[test]
    fn et_al_and_inline_quotation_before_unquoted_title() {
        let refs = refs_from_lines(&[
            "K. M. Collins, I. Sucholutsky, U. Bhatt, K. Chandra, L. Wong, M. Lee, C. E. Zhang, T. Zhi-Xuan, M. Ho,",
            "V. Mansinghka, et al. Building machines that learn and think with people. Nature Human Behaviour, 8",
            "(10):1851–1863, 2024.",
        ]);
        assert_eq!(refs.len(), 1);
        assert_eq!(
            refs[0].title.as_deref(),
            Some("Building machines that learn and think with people")
        );
        assert_eq!(refs[0].authors.len(), 10);
        assert_eq!(refs[0].authors[0], "K. M. Collins");
        assert_eq!(refs[0].authors[9], "V. Mansinghka");
        assert_eq!(refs[0].venue.as_deref(), Some("Nature Human Behaviour"));
        assert_eq!(refs[0].volume.as_deref(), Some("8"));
        assert_eq!(refs[0].issue.as_deref(), Some("10"));
        assert_eq!(refs[0].pages.as_deref(), Some("1851–1863"));
        assert_eq!(refs[0].year, Some(2024));

        let entry = parsed(
            r#"C. Anthony, B. A. Bechky, and A.-L. Fayard. "collaborating" with ai: Taking a system view to explore the future of work. Organization Science, 34(5):1672–1694, 2023. doi: 10.1287/orsc.2022.1651."#,
            None,
        );
        assert_eq!(
            entry.title.as_deref(),
            Some(r#""collaborating" with ai: Taking a system view to explore the future of work"#)
        );
        assert_eq!(
            entry.authors,
            vec!["C. Anthony", "B. A. Bechky", "A.-L. Fayard"]
        );
        assert_eq!(entry.venue.as_deref(), Some("Organization Science"));
        assert_eq!(entry.volume.as_deref(), Some("34"));
        assert_eq!(entry.issue.as_deref(), Some("5"));
        assert_eq!(entry.pages.as_deref(), Some("1672–1694"));
        assert_eq!(entry.year, Some(2023));
        assert_eq!(entry.doi.as_deref(), Some("10.1287/orsc.2022.1651"));
    }

    /// A URL broken after `/` is closed up (arXiv:2511.13979).
    #[test]
    fn url_closed_up_across_line_wrap() {
        let refs = refs_from_lines(&[
            "OpenAI. Customizing your ChatGPT personality. https://help.openai.com/en/articles/",
            "11899719-customizing-your-chatgpt-personality, 2025a. Accessed: 2025-08-29.",
        ]);
        assert_eq!(refs.len(), 1);
        assert_eq!(
            refs[0].url.as_deref(),
            Some(
                "https://help.openai.com/en/articles/11899719-customizing-your-chatgpt-personality"
            )
        );
        assert_eq!(
            refs[0].title.as_deref(),
            Some("Customizing your ChatGPT personality")
        );
        assert_eq!(refs[0].authors, vec!["OpenAI"]);
        assert_eq!(refs[0].year, Some(2025));
    }

    /// Justified ACL columns arrive as row fragments (`and Jingren Zhou.
    /// 2023.` | `Qwen-vl: A versatile` on one baseline) and the neighbouring
    /// line can no longer say whether a line is outdented; the section-wide
    /// x levels can (arXiv:2505.16990). Hyphenated line ends are resolved
    /// when the lines are joined.
    #[test]
    fn row_fragments_and_section_wide_indent_levels() {
        let rows: Vec<(&str, f32, Option<&str>)> = vec![
            (
                "Jinze Bai, Shuai Bai, Shusheng Yang, Shijie Wang,",
                72.0,
                None,
            ),
            ("Sinan Tan, Peng Wang, Junyang Lin, Chang Zhou,", 86.0, None),
            (
                "and Jingren Zhou. 2023.",
                86.0,
                Some("Qwen-vl: A versatile"),
            ),
            (
                "vision-language model for understanding, localiza-",
                86.0,
                None,
            ),
            (
                "tion, text reading, and beyond.",
                86.0,
                Some("arXiv preprint"),
            ),
            ("arXiv:2308.12966.", 86.0, None),
            (
                "Shuai Bai, Keqin Chen, Xuejing Liu, Jialin Wang, Wen-",
                72.0,
                None,
            ),
            ("bin Ge, Sibo Song, Kai Dang, Peng Wang, Shi-", 86.0, None),
            ("jie Wang, Jun Tang, Humen Zhong, Yuanzhi Zhu,", 86.0, None),
            (
                "Mingkun Yang, Zhaohai Li, Jianqiang Wan, Pengfei",
                86.0,
                None,
            ),
            (
                "Wang, Wei Ding, Zheren Fu, Yiheng Xu, and 8 oth-",
                86.0,
                None,
            ),
            (
                "ers. 2025. Qwen2.5-vl technical report. Preprint,",
                86.0,
                None,
            ),
            ("arXiv:2502.13923.", 86.0, None),
            (
                "Zalán Borsos, Matt Shariﬁ, Damien Vincent, Eugene",
                72.0,
                None,
            ),
            (
                "Kharitonov, Neil Zeghidour, and Marco Tagliasacchi.",
                86.0,
                None,
            ),
            (
                "2023. Soundstorm: Efﬁcient parallel audio genera-",
                86.0,
                None,
            ),
            ("tion. Preprint, arXiv:2305.09636.", 86.0, None),
        ];
        let mut lines: Vec<Line> = vec![line_at("References", 0, 72.0, 754.0)];
        for (i, (text, x0, fragment)) in rows.iter().enumerate() {
            let y = 740.0 - 14.0 * i as f32;
            lines.push(line_at(text, 0, *x0, y));
            if let Some(fragment) = fragment {
                lines.push(line_at(fragment, 0, 220.0, y));
            }
        }
        let page = page_of(9, lines);
        let (refs, _) = extract_citations(&[page]);

        assert_eq!(refs.len(), 3);
        assert_eq!(
            refs[0].raw,
            "Jinze Bai, Shuai Bai, Shusheng Yang, Shijie Wang, Sinan Tan, Peng Wang, \
             Junyang Lin, Chang Zhou, and Jingren Zhou. 2023. Qwen-vl: A versatile \
             vision-language model for understanding, localization, text reading, and \
             beyond. arXiv preprint arXiv:2308.12966."
        );
        assert_eq!(
            refs[0].title.as_deref(),
            Some(
                "Qwen-vl: A versatile vision-language model for understanding, localization, \
                 text reading, and beyond"
            )
        );
        assert_eq!(refs[0].year, Some(2023));
        assert_eq!(refs[0].arxiv_id.as_deref(), Some("2308.12966"));
        assert_eq!(refs[0].label.as_deref(), Some("Jinze2023"));
        assert!(
            refs[1]
                .raw
                .contains("Wenbin Ge, Sibo Song, Kai Dang, Peng Wang, Shijie Wang")
        );
        assert!(refs[1].raw.contains("and 8 others. 2025."));
        assert_eq!(
            refs[1].title.as_deref(),
            Some("Qwen2.5-vl technical report")
        );
        assert_eq!(refs[1].arxiv_id.as_deref(), Some("2502.13923"));
        assert_eq!(
            refs[2].title.as_deref(),
            Some("Soundstorm: Efﬁcient parallel audio generation")
        );
        assert_eq!(refs[2].year, Some(2023));
        assert_eq!(refs[2].arxiv_id.as_deref(), Some("2305.09636"));
    }

    /// Line-end hyphens: a compound that occurs unbroken elsewhere keeps its
    /// hyphen, a known compound prefix keeps it, a word that occurs joined
    /// elsewhere drops it, an uppercase continuation keeps it, a dash that
    /// is not a word break stays separate.
    #[test]
    fn hyphen_breaks_are_resolved_at_line_joins() {
        let context = "multi-task learning\nreconstruction of images";
        assert_eq!(
            hyphen_break("Learning multi-", "task models", context),
            HyphenJoin::Keep
        );
        assert_eq!(
            hyphen_break("MRI recon-", "struction", context),
            HyphenJoin::Drop
        );
        assert_eq!(
            hyphen_break("a self-", "supervised model", ""),
            HyphenJoin::Keep
        );
        assert_eq!(hyphen_break("privacy-", "preserving", ""), HyphenJoin::Keep);
        assert_eq!(hyphen_break("Ming-", "Hsuan Yang", ""), HyphenJoin::Keep);
        assert_eq!(hyphen_break("pp. 911-", "935", ""), HyphenJoin::Keep);
        assert_eq!(hyphen_break("sta-", "bilization", ""), HyphenJoin::Drop);
        assert_eq!(
            hyphen_break("Attention -", "is all", ""),
            HyphenJoin::Separate
        );
    }

    /// `Series A, containing papers ...` looks like `Surname A,` and follows
    /// a period, but carries no year, DOI, arXiv id or URL: it is a
    /// continuation of the previous entry (arXiv:2306.11313).
    #[test]
    fn false_start_without_evidence_folds_into_previous_entry() {
        let refs = refs_from_lines(&[
            "Mercer, J. (1909). Xvi. functions of positive and negative type, and their connection the",
            "theory of integral equations. Philosophical Transactions of the Royal Society of London.",
            "Series A, containing papers of a mathematical or physical character, 209(441-458):415–446.",
            "Moradi, M. M. and Mateu, J. (2020). First-and second-order characteristics of spatio-",
            "temporal point processes on linear networks. Journal of Computational and Graphical Statistics, 29(3):432–443.",
        ]);
        assert_eq!(refs.len(), 2);
        assert!(refs[0].raw.ends_with("209(441-458):415–446."));
        assert_eq!(refs[0].label.as_deref(), Some("Mercer1909"));
        assert!(refs[1].raw.contains("spatio-temporal point processes"));
        assert_eq!(refs[1].label.as_deref(), Some("Moradi2020"));
        assert_eq!(refs[1].volume.as_deref(), Some("29"));
        assert_eq!(refs[1].pages.as_deref(), Some("432–443"));
    }

    /// An undated final entry (`(n.d.)`) carries no year, DOI or URL but is
    /// a real reference: it is kept, not dropped as trailing furniture.
    #[test]
    fn undated_final_entry_is_kept() {
        let refs = refs_from_lines(&[
            "Adams, R. (2019). A first title. Journal of Tests, 4(2), 10–20.",
            "Brown, K. (in press). A second title. Journal of Tests.",
            "Smith, J. (n.d.). Title of an undated report. Example Institute.",
        ]);
        assert_eq!(refs.len(), 3);
        assert!(refs[1].raw.starts_with("Brown, K. (in press)."));
        assert!(refs[2].raw.starts_with("Smith, J. (n.d.)."));
        assert_eq!(refs[2].index, 3);
    }

    /// A pre-1900 entry (`(1843)`) in the middle of the list is its own
    /// entry, not a continuation of the one before it.
    #[test]
    fn pre_1900_entry_stays_separate() {
        let refs = refs_from_lines(&[
            "Babbage, C. (1999). A modern edition. Journal of Tests, 4(2), 10–20.",
            "Lovelace, A. (1843). Notes on the analytical engine. Scientific Memoirs, 3, 666–731.",
            "Turing, A. M. (1950). Computing machinery and intelligence. Mind, 59(236), 433–460.",
        ]);
        assert_eq!(refs.len(), 3);
        assert!(refs[0].raw.ends_with("10–20."));
        assert!(refs[1].raw.starts_with("Lovelace, A. (1843)."));
        assert!(refs[1].raw.ends_with("666–731."));
        assert!(refs[2].raw.starts_with("Turing, A. M. (1950)."));
    }

    #[test]
    fn undated_and_old_entries_are_evidence() {
        assert!(has_reference_evidence("Smith, J. (n.d.). Title."));
        assert!(has_reference_evidence("Smith, J. (No date). Title."));
        assert!(has_reference_evidence("Smith, J. (forthcoming). Title."));
        assert!(has_reference_evidence("Smith, J., in press. Title."));
        assert!(has_reference_evidence("Smith, J. (under review). Title."));
        assert!(has_reference_evidence("Smith, J. (to appear). Title."));
        assert!(has_reference_evidence("Newton, I. 1687. Principia."));
        assert!(!has_reference_evidence(
            "Series A, containing papers of a mathematical or physical character, 209(441-458):415–446."
        ));
        assert!(is_author_year_start("Smith, J. (n.d.). Title."));
        assert!(!is_author_year_start(
            "Series A, containing papers of a mathematical"
        ));
    }

    /// Table cells after the last reference sit at the entry-start x level
    /// but carry no reference evidence: the trailing run is dropped
    /// (arXiv:2502.00857).
    #[test]
    fn trailing_table_cells_are_dropped() {
        let texts: Vec<(&str, f32)> = vec![
            ("Asahi Ushio, Fernando Alva-Manchego, and Jose", 72.0),
            ("Camacho-Collados. 2023. A practical toolkit for", 86.0),
            ("multilingual question and answer generation. In Pro-", 86.0),
            ("ceedings of the 61st Annual Meeting of the Associa-", 86.0),
            ("tion for Computational Linguistics (Volume 3: Sys-", 86.0),
            ("tem Demonstrations), pages 86–94, Toronto, Canada.", 86.0),
            ("Association for Computational Linguistics.", 86.0),
            ("Preferred", 72.0),
            ("Cost", 72.0),
            ("Execution", 72.0),
            ("Metric", 72.0),
            ("Method", 72.0),
            ("Accuracy", 72.0),
            ("Device", 72.0),
            ("Effectiveness", 72.0),
        ];
        let mut lines: Vec<Line> = vec![line_at("References", 0, 72.0, 754.0)];
        for (i, (text, x0)) in texts.iter().enumerate() {
            lines.push(line_at(text, 0, *x0, 740.0 - 14.0 * i as f32));
        }
        let page = page_of(10, lines);
        let (refs, _) = extract_citations(&[page]);
        assert_eq!(refs.len(), 1);
        assert_eq!(
            refs[0].raw,
            "Asahi Ushio, Fernando Alva-Manchego, and Jose Camacho-Collados. 2023. A \
             practical toolkit for multilingual question and answer generation. In \
             Proceedings of the 61st Annual Meeting of the Association for Computational \
             Linguistics (Volume 3: System Demonstrations), pages 86–94, Toronto, Canada. \
             Association for Computational Linguistics."
        );
        assert_eq!(
            refs[0].title.as_deref(),
            Some("A practical toolkit for multilingual question and answer generation")
        );
        assert_eq!(refs[0].pages.as_deref(), Some("86–94"));
    }

    /// Section-end markers seen after real reference lists: `- Supplementary
    /// Material -` (arXiv:2505.16990), an IEEE author biography
    /// (arXiv:2503.15734), a table caption and a row of numbers. Ordinary
    /// entries and titles containing `is a` are not markers.
    #[test]
    fn section_end_markers() {
        let line = |text: &str| SectionLine {
            page: 1,
            line: 0,
            column: 0,
            x0: Some(72.0),
            y0: Some(700.0),
            size: Some(10.0),
            text: text.to_string(),
        };
        let ends = |text: &str| is_end_heading(&line(text), Style::Bracket, Some(10.0));
        assert!(ends("- Supplementary Material -"));
        assert!(ends(
            "DAVID E. J. VAN WIJK is a Postdoctoral Scholar in the Department"
        ));
        assert!(ends("Jane Doe received the B.S. degree in 2001."));
        assert!(ends(
            "Table 5: Qualitative comparison of hint generation methods."
        ));
        assert!(ends("0.85 0.91 0.77"));
        assert!(ends("Appendix A"));
        assert!(ends("Technical Appendix"));
        assert!(!ends(
            "[3] A. Author, Deep Learning is a Robust Method, Venue, 2020."
        ));
        assert!(!ends(
            "Deep Learning is a Robust Method. In Proceedings of Things, 2020."
        ));
        assert!(!ends("pp. 3615–3620, 2023."));
    }

    /// `Page 30 of 35` / `Page 31 of 35` footers differ only in their digits
    /// and are page furniture (arXiv:2305.13843).
    #[test]
    fn page_footers_with_changing_numbers_are_furniture() {
        let mut first = column_page(
            30,
            &[
                "References",
                "[1] D. Goldberg, D. Nichols, B. M. Oki, D. Terry, Using collaborative",
                "ﬁltering to weave an information tapestry, Communications of the",
                "ACM 35 (1992) 61–70.",
            ],
        );
        first.lines.push(line_at("Page 30 of 35", 0, 250.0, 30.0));
        let mut second = column_page(
            31,
            &[
                "[2] H. Guo, R. Tang, Y. Ye, Z. Li, X. He, Deepfm: a factorization-",
                "machine based neural network for ctr prediction, arXiv preprint",
                "arXiv:1703.04247 (2017).",
            ],
        );
        second.lines.push(line_at("Page 31 of 35", 0, 250.0, 30.0));
        let (refs, _) = extract_citations(&[first, second]);
        assert_eq!(refs.len(), 2);
        assert!(refs[0].raw.ends_with("ACM 35 (1992) 61–70."));
        assert!(refs[1].raw.ends_with("arXiv:1703.04247 (2017)."));
        assert!(refs.iter().all(|r| !r.raw.contains("Page 3")));
        assert!(refs[1].raw.contains("factorization-machine"));
        assert_eq!(refs[1].arxiv_id.as_deref(), Some("1703.04247"));
        assert_eq!(refs[1].year, Some(2017));
        assert_eq!(refs[1].page, 31);
    }

    /// biblatex style (arXiv:2501.17300): `Authors (2005). “Title”. In:` — the
    /// quoted title after the year is read without its quotes; a title
    /// ending in `?` before the closing quote is complete.
    #[test]
    fn quoted_title_after_year_and_question_mark_titles() {
        let entry = parsed(
            "Pujol, J. M., J. Delgado, R. Sangüesa, and A. Flache (2005). “The role of \
             clustering on the emergence of eﬀicient social conventions”. In: Proceedings \
             of the 19th international joint conference on Artificial intelligence, pp. \
             965–970.",
            None,
        );
        assert_eq!(
            entry.title.as_deref(),
            Some("The role of clustering on the emergence of eﬀicient social conventions")
        );
        assert_eq!(entry.year, Some(2005));
        assert_eq!(entry.pages.as_deref(), Some("965–970"));
        assert_eq!(entry.authors[0], "Pujol, J. M.");

        let mary = parsed(
            "[8] P. Mary, J.-M. Gorce, A. Unsal, and H. V. Poor, “Finite blocklength \
             information theory: What is the practical impact on wireless communications?” \
             in 2016 IEEE Globecom Workshops (GC Wkshps), 2016, pp. 1–6.",
            Some("[8]"),
        );
        assert_eq!(
            mary.title.as_deref(),
            Some(
                "Finite blocklength information theory: What is the practical impact on \
                 wireless communications?"
            )
        );
        assert_eq!(mary.year, Some(2016));
        assert_eq!(mary.pages.as_deref(), Some("1–6"));

        let bianchi = parsed(
            "Federico Bianchi, Patrick John Chia, Mert Yuksekgonul, Jacopo Tagliabue, Dan \
             Jurafsky, and James Zou. 2024. How well can llms negotiate? negotiationarena \
             platform and analysis. arXiv preprint arXiv:2402.05863.",
            None,
        );
        assert_eq!(
            bianchi.title.as_deref(),
            Some("How well can llms negotiate? negotiationarena platform and analysis")
        );
        assert_eq!(bianchi.arxiv_id.as_deref(), Some("2402.05863"));
    }

    /// OT1 fonts set the accent of `Verdú` or `Güngör` as a glyph of its own
    /// on the row's baseline; the layout pass leaves it as a separate line
    /// that sorts before the row, so it used to be joined in front of the
    /// `[n]` label and hide it (arXiv:2507.08599 `[2]`, arXiv:2608.28714
    /// `[13]` with four accents).
    #[test]
    fn floating_accents_do_not_hide_numbered_labels() {
        type Row<'a> = (&'a str, f32, Vec<(&'a str, f32)>);
        let rows: Vec<Row<'_>> = vec![
            (
                "[1] Y. Polyanskiy, H. V. Poor, and S. Verdu, “Channel coding rate in the finite",
                72.0,
                vec![],
            ),
            (
                "blocklength regime,” IEEE Transactions on Information Theory, vol. 56,",
                86.0,
                vec![],
            ),
            ("no. 5, pp. 2307–2359, 2010.", 86.0, vec![]),
            (
                "[2] S. Verdu, “Non-asymptotic achievability bounds in multiuser information",
                72.0,
                vec![("´", 118.0)],
            ),
            (
                "theory,” in 2012 50th Annual Allerton Conference on Communication,",
                86.0,
                vec![],
            ),
            (
                "Control, and Computing (Allerton), 2012, pp. 1–8.",
                86.0,
                vec![],
            ),
            (
                "[3] A. Gungor, S. U. Dar, C. Ozturk, Y. Korkmaz, H. A. Bedel, G. Elmas, M. Ozbey,",
                72.0,
                vec![("¨", 100.0), ("¨", 160.0), ("¨", 330.0), ("¨", 400.0)],
            ),
            (
                "and T. Cukur, “Adaptive diffusion priors for accelerated MRI reconstruction,”",
                86.0,
                vec![("¸", 100.0)],
            ),
            (
                "Medical image analysis, 2023, pMID: 37384951.",
                86.0,
                vec![],
            ),
        ];
        let mut lines: Vec<Line> = vec![line_at("References", 0, 72.0, 754.0)];
        for (i, (text, x0, accents)) in rows.iter().enumerate() {
            let y = 740.0 - 14.0 * i as f32;
            for (mark, x) in accents {
                lines.push(line_at(mark, 0, *x, y + 0.04));
            }
            lines.push(line_at(text, 0, *x0, y));
        }
        let page = page_of(6, lines);
        let (refs, _) = extract_citations(&[page]);

        assert_eq!(refs.len(), 3);
        let labels: Vec<&str> = refs.iter().filter_map(|r| r.label.as_deref()).collect();
        assert_eq!(labels, vec!["[1]", "[2]", "[3]"]);
        assert!(refs[0].raw.ends_with("no. 5, pp. 2307–2359, 2010."));
        assert!(refs[1].raw.starts_with("[2] S. Verdu, “Non-asymptotic"));
        assert!(refs[1].raw.ends_with("(Allerton), 2012, pp. 1–8."));
        assert_eq!(
            refs[1].title.as_deref(),
            Some("Non-asymptotic achievability bounds in multiuser information theory")
        );
        assert_eq!(refs[1].year, Some(2012));
        assert!(refs[2].raw.starts_with("[3] A. Gungor, S. U. Dar"));
        assert!(refs[2].raw.contains("M. Ozbey, and T. Cukur, “Adaptive"));
        assert_eq!(
            refs[2].title.as_deref(),
            Some("Adaptive diffusion priors for accelerated MRI reconstruction")
        );
        assert_eq!(refs[2].year, Some(2023));
        assert!(refs.iter().all(|r| !r.raw.contains(['´', '¨', '¸'])));
    }

    /// ACL style (arXiv:2505.16990): the hanging-indent levels are read from
    /// the list itself. The supplementary material after it is set at the
    /// entry-start x, and counting its lines used to bury the indented
    /// share, discard the layout and fall back to the name pattern, which
    /// does not accept `Mozhgan Nasr Azadani,`, `Fnu Mohbat and` or
    /// `Shitong Xu. 2022.`.
    #[test]
    fn indent_levels_ignore_the_appendix_after_the_list() {
        let rows: Vec<(&str, f32, Option<&str>)> = vec![
            (
                "Jacob Austin, Daniel D. Johnson, Jonathan Ho, Daniel",
                72.0,
                None,
            ),
            (
                "Tarlow, and Rianne van den Berg. 2023. Structured",
                86.0,
                None,
            ),
            (
                "denoising diffusion models in discrete state-spaces.",
                86.0,
                None,
            ),
            ("Preprint, arXiv:2107.03006.", 86.0, None),
            (
                "Mozhgan Nasr Azadani, James Riddell, Sean Sedwards,",
                72.0,
                None,
            ),
            (
                "and Krzysztof Czarnecki. 2025. Leo: Boosting mix-",
                86.0,
                None,
            ),
            (
                "ture of vision encoders for multimodal large language",
                86.0,
                None,
            ),
            ("models. Preprint, arXiv:2501.06986.", 86.0, None),
            (
                "Fnu Mohbat and Mohammed J. Zaki. 2024. Llava-chef:",
                72.0,
                None,
            ),
            (
                "A multi-modal generative model for food recipes.",
                86.0,
                None,
            ),
            ("Preprint, arXiv:2408.16889.", 86.0, None),
            (
                "Shitong Xu. 2022.",
                72.0,
                Some("Clip-diffusion-lm: Apply dif-"),
            ),
            ("fusion model on image captioning.", 86.0, Some("Preprint,")),
            ("arXiv:2210.04559.", 86.0, None),
            ("- Supplementary Material -", 72.0, None),
        ];
        let mut lines: Vec<Line> = vec![line_at("References", 0, 72.0, 754.0)];
        for (i, (text, x0, fragment)) in rows.iter().enumerate() {
            let y = 740.0 - 14.0 * i as f32;
            lines.push(line_at(text, 0, *x0, y));
            if let Some(fragment) = fragment {
                lines.push(line_at(fragment, 0, 220.0, y));
            }
        }
        // Thirty lines of appendix prose at the entry-start x: two thirds
        // of the section's lines, all of them unindented.
        let appendix: Vec<String> = (1..=30)
            .map(|k| format!("Appendix sentence {k} about the training data and the setup."))
            .collect();
        for (k, text) in appendix.iter().enumerate() {
            let y = 740.0 - 14.0 * (rows.len() + k) as f32;
            lines.push(line_at(text, 0, 72.0, y));
        }
        let page = page_of(11, lines);
        let (refs, _) = extract_citations(&[page]);

        assert_eq!(refs.len(), 4);
        assert_eq!(refs[0].label.as_deref(), Some("Jacob2023"));
        assert_eq!(refs[0].arxiv_id.as_deref(), Some("2107.03006"));
        assert_eq!(
            refs[1].raw,
            "Mozhgan Nasr Azadani, James Riddell, Sean Sedwards, and Krzysztof Czarnecki. \
             2025. Leo: Boosting mixture of vision encoders for multimodal large language \
             models. Preprint, arXiv:2501.06986."
        );
        assert_eq!(refs[1].label.as_deref(), Some("Mozhgan2025"));
        assert_eq!(
            refs[1].title.as_deref(),
            Some("Leo: Boosting mixture of vision encoders for multimodal large language models")
        );
        assert!(
            refs[2]
                .raw
                .starts_with("Fnu Mohbat and Mohammed J. Zaki. 2024. Llava-chef:")
        );
        assert_eq!(refs[2].arxiv_id.as_deref(), Some("2408.16889"));
        assert_eq!(
            refs[3].raw,
            "Shitong Xu. 2022. Clip-diffusion-lm: Apply diffusion model on image captioning. \
             Preprint, arXiv:2210.04559."
        );
        assert_eq!(refs[3].year, Some(2022));
        assert!(refs.iter().all(|r| !r.raw.contains("Appendix sentence")));
    }

    /// The last line of a full column sits in the bottom margin band. An
    /// `arXiv:` line there is not a running footer even though the next
    /// page ends with an `arXiv:` line too: only short digit runs are
    /// wildcarded, so `Page 9 of 11` and `Page 10 of 11` still repeat while
    /// two arXiv ids do not (arXiv:2505.16990, `Muse`).
    #[test]
    fn arxiv_lines_at_the_page_edge_are_not_furniture() {
        let mut first = column_page(
            9,
            &[
                "References",
                "Huiwen Chang, Han Zhang, Jarred Barber, and Dilip Krishnan. 2023. Muse: Text-to-image",
                "generation via masked generative transformers. Preprint,",
            ],
        );
        first
            .lines
            .push(line_at("arXiv:2301.00704.", 0, 72.0, 40.0));
        first.lines.push(line_at("Page 9 of 11", 0, 250.0, 20.0));
        let mut second = column_page(
            10,
            &[
                "Huiwen Chang, Han Zhang, Lu Jiang, Ce Liu, and William T. Freeman. 2022. Maskgit:",
                "Masked generative image transformer. Preprint,",
            ],
        );
        second
            .lines
            .push(line_at("arXiv:2202.04200.", 0, 72.0, 40.0));
        second.lines.push(line_at("Page 10 of 11", 0, 250.0, 20.0));
        let (refs, _) = extract_citations(&[first, second]);

        assert_eq!(refs.len(), 2);
        assert!(
            refs[0]
                .raw
                .ends_with("transformers. Preprint, arXiv:2301.00704.")
        );
        assert_eq!(refs[0].arxiv_id.as_deref(), Some("2301.00704"));
        assert_eq!(refs[0].label.as_deref(), Some("Huiwen2023"));
        assert!(refs[1].raw.starts_with("Huiwen Chang, Han Zhang, Lu Jiang"));
        assert!(
            refs[1]
                .raw
                .ends_with("transformer. Preprint, arXiv:2202.04200.")
        );
        assert_eq!(refs[1].arxiv_id.as_deref(), Some("2202.04200"));
        assert_eq!(refs[1].page, 10);
        assert!(refs.iter().all(|r| !r.raw.contains("Page ")));
        assert_eq!(digit_key("Page 9 of 11"), digit_key("Page 10 of 11"));
        assert_ne!(
            digit_key("arXiv:2301.00704."),
            digit_key("arXiv:2202.04200.")
        );
    }

    /// biblatex (arXiv:2501.17300): a DOI set in a second font can arrive
    /// before the text to its left on the same printed row. The row is
    /// joined in x order, so the DOI follows `doi:` and the entry does not
    /// end in `doi:`.
    #[test]
    fn row_fragments_join_in_x_order() {
        let lines = vec![
            line_at("References", 0, 72.0, 754.0),
            line_at(
                "Centola, D. and A. Baronchelli (2015). “The spontaneous emergence of conventions: An",
                0,
                72.0,
                740.0,
            ),
            line_at(
                "experimental study of cultural evolution”. In: Proceedings of the National Academy of",
                0,
                89.93,
                726.0,
            ),
            line_at("10.1073/pnas.1418838112.", 0, 300.0, 712.2),
            line_at("Sciences 112.7, pp. 1989–1994. doi:", 0, 89.93, 712.0),
            line_at(
                "Hawkins, R. X. and R. L. Goldstone (2016). “The Formation of Social Conventions in",
                0,
                72.0,
                698.0,
            ),
            line_at(
                "Real-Time Environments”. In: PLOS ONE 11.3. Ed. by C. T. Bauch, e0151670. doi:",
                0,
                89.93,
                684.0,
            ),
            line_at("10.1371/journal.pone.0151670.", 0, 89.93, 670.0),
        ];
        let page = page_of(21, lines);
        let (refs, _) = extract_citations(&[page]);

        assert_eq!(refs.len(), 2);
        assert!(refs[0].raw.ends_with(
            "Proceedings of the National Academy of Sciences 112.7, pp. 1989–1994. doi: \
             10.1073/pnas.1418838112."
        ));
        assert_eq!(refs[0].doi.as_deref(), Some("10.1073/pnas.1418838112"));
        assert_eq!(refs[0].label.as_deref(), Some("Centola2015"));
        assert!(
            refs[1]
                .raw
                .starts_with("Hawkins, R. X. and R. L. Goldstone (2016).")
        );
        assert_eq!(refs[1].doi.as_deref(), Some("10.1371/journal.pone.0151670"));
        assert_eq!(refs[1].label.as_deref(), Some("Hawkins2016"));
    }

    #[test]
    fn numbered_label_columns_continue_across_pages() {
        for suffix in ['.', ')'] {
            let first = page_of(
                1,
                vec![
                    line_at("References", 0, 40.0, 700.0),
                    line_at(
                        &format!("1{suffix} Adams, A. First study. Nature 2020."),
                        0,
                        40.0,
                        680.0,
                    ),
                    line_at(
                        &format!("2{suffix} Baker, B. Second study. Science 2021."),
                        0,
                        40.0,
                        650.0,
                    ),
                    line_at(
                        &format!("3{suffix} Clark, C. Third study. Cell 2022."),
                        0,
                        40.0,
                        620.0,
                    ),
                ],
            );
            // Reading order puts the whole narrow label column first, then
            // the entry-text column. Continuation lines have no label.
            let second = page_of(
                2,
                vec![
                    line_at(&format!("4{suffix}"), 0, 40.0, 700.0),
                    line_at(&format!("5{suffix}"), 0, 40.0, 670.0),
                    line_at(&format!("6{suffix}"), 0, 40.0, 640.0),
                    line_at("Davis, D. Fourth study.", 1, 60.0, 700.0),
                    line_at("Nature 2023, 12, 10–20.", 1, 60.0, 690.0),
                    line_at("Evans, E. Fifth study. Cell 2024.", 1, 60.0, 670.0),
                    line_at("Ford, F. Sixth study. Science 2025.", 1, 60.0, 640.0),
                    line_at("Appendix A", 1, 60.0, 600.0),
                    line_at("Extra material.", 1, 60.0, 580.0),
                ],
            );
            let pages = [first, second];
            let section = find_reference_section(&pages).expect("numbered list");
            let refs = segment_entries(&pages, &section);
            assert_eq!(refs.len(), 6);
            for (i, entry) in refs.iter().enumerate() {
                assert_eq!(entry.label, Some(format!("{}{suffix}", i + 1)));
                assert!(!entry.raw.contains("Appendix"));
            }
            assert!(refs[3].raw.contains("Fourth study. Nature 2023"));
            assert_eq!(refs[3].page, 2);
            assert!((refs[3].anchor.unwrap().x0 - 40.0).abs() < f32::EPSILON);
        }
    }

    #[test]
    fn detached_numbered_labels_need_a_nearby_row_on_the_same_page() {
        let line = |text: &str, page: u32, x: f32, y: f32| SectionLine {
            text: text.to_string(),
            page,
            line: 0,
            column: 0,
            x0: Some(x),
            y0: Some(y),
            size: Some(10.0),
        };
        let original = vec![
            line("1.", 1, 40.0, 700.0),
            line("Too far right", 1, 100.0, 700.0),
            line("Wrong baseline", 1, 60.0, 680.0),
            line("Different page", 2, 60.0, 700.0),
            line("2020", 2, 40.0, 600.0),
            line("Bare year stays separate", 2, 60.0, 600.0),
        ];
        let result = reattach_numbered_labels(original.clone());
        assert_eq!(
            result.iter().map(|l| &l.text).collect::<Vec<_>>(),
            original.iter().map(|l| &l.text).collect::<Vec<_>>()
        );

        let mut crowded = vec![line("1.", 1, 40.0, 700.0)];
        crowded.extend((0..=MAX_ROW_FRAGMENTS).map(|_| line("crowded", 1, 60.0, 700.0)));
        let result = reattach_numbered_labels(crowded);
        assert_eq!(result.len(), MAX_ROW_FRAGMENTS + 2);
        assert_eq!(result[0].text, "1.");
    }

    #[test]
    fn row_tolerance_follows_the_largest_fragment_size() {
        let fragment = |x0: f32, y0: f32, size: f32| SectionLine {
            page: 1,
            line: 0,
            column: 0,
            x0: Some(x0),
            y0: Some(y0),
            size: Some(size),
            text: "t".to_string(),
        };
        let first = fragment(0.0, 100.0, 5.0);
        let third = fragment(40.0, 106.0, 5.0);
        // Under the first fragment's 5 pt the third is 6 pt away and split;
        // once a 20 pt fragment joined the row it is within 0.4 × 20 pt.
        assert!(!first.same_row_as(&third, first.size));
        assert!(first.same_row_as(&third, max_size(first.size, Some(20.0))));

        let lone = vec![fragment(0.0, 100.0, 5.0)];
        assert_eq!(finish_row(lone).text, "t");
        let merged = finish_row(vec![fragment(50.0, 100.0, 5.0), fragment(0.0, 100.0, 20.0)]);
        assert_eq!(merged.text, "t t");
        assert_eq!(merged.x0, Some(0.0));
        assert_eq!(merged.size, Some(20.0));
    }

    #[test]
    fn row_fragment_budget_splits_hostile_geometry() {
        let mut input = vec![line_at("References", 0, 72.0, 754.0)];
        for i in 0..=MAX_ROW_FRAGMENTS {
            input.push(line_at(
                &format!("F{i:04}"),
                0,
                (MAX_ROW_FRAGMENTS - i) as f32,
                740.0,
            ));
        }
        let page = page_of(1, input);
        let section = ReferenceSection {
            first_page: 1,
            first_line: 0,
            heading: "References".to_string(),
        };

        let pages = [page];
        let repeated = repeated_furniture(&pages);
        let rows = section_lines_with_furniture(&pages, &section, None, &repeated);

        assert_eq!(rows.len(), 2);
        assert!(rows[0].text.starts_with("F1023 F1022"));
        assert!(rows[0].text.ends_with("F0000"));
        assert_eq!(rows[1].text, "F1024");
    }

    /// Loop 6 segmentation fixes, each built from the text of an arXiv
    /// paper whose list the evaluation found mis-segmented.
    mod loop6_segmentation_tests {
        use super::*;

        /// RSC and the other orders and cases of the notes heading.
        #[test]
        fn notes_and_references_headings() {
            for text in [
                "Notes and references",
                "Notes and References",
                "NOTES AND REFERENCES",
                "References and notes",
                "References and Notes",
            ] {
                assert!(heading_re().is_match(text), "{text}");
            }
            assert!(!heading_re().is_match("Notes and references are below."));
        }

        /// arXiv:2510.26824: the main paper's RSC list (`Notes and
        /// references`, entries `1 Q. Zhang, ...` without brackets or
        /// period) and the supplement's `[n]` list (`References and Notes`)
        /// are both segmented; `8 arXiV, ...` starts its entry although a
        /// lowercase word follows the number.
        #[test]
        fn rsc_bare_number_list_and_a_bracket_list() {
            let first = page_of(
                1,
                [
                    "for finetuning our image segmentation model.",
                    "Notes and references",
                    "1 Q. Zhang, E. Uchaker, S. L. Candelaria and G. Cao, Chem. Soc.",
                    "Rev., 2013, 42, 3127–3171.",
                    "2 C. Liu, F. Li, L.-P. Ma and H.-M. Cheng, Adv. Mater., 2010, 22,",
                    "E28–E62.",
                    "3 C. Vogt and B. M. Weckhuysen, Nat. Rev. Chem., 2022, 6, 89–",
                    "111.",
                    "4 K. T. Butler, D. W. Davies, H. Cartwright, O. Isayev and",
                    "A. Walsh, Nature, 2018, 559, 547–555.",
                    "5 J. J. de Pablo, B. Jones, C. L. Kovacs, V. Ozolins and A. P.",
                    "Ramirez, Curr. Opin. Solid State Mater. Sci., 2014, 18, 99–117.",
                    "6 J. Hachmann,",
                    "R. Olivares-Amaya,",
                    "S. Atahan-Evrenk,",
                    "C. Amador-Bedolla, R. S. SÃąnchez-Carrera, A. Gold-Parker,",
                    "L. Vogt, A. M. Brockway and A. Aspuru-Guzik, J. Phys. Chem.",
                    "Lett., 2011, 2, 2241–2251.",
                    "7 A. Jain, S. P. Ong, G. Hautier, W. Chen, W. D. Richards,",
                    "S. Dacek, S. Cholia, D. Gunter, D. Skinner, G. Ceder and K. a.",
                    "Persson, APL Mater., 2013, 1, 011002.",
                    "8 arXiV, https://arxiv.org/, Accessed: 2025-08-11.",
                    "9 ChemRxiv, https://chemrxiv.org/, Accessed: 2025-08-11.",
                ]
                .into_iter()
                .map(bare_line)
                .collect(),
            );
            let second = page_of(
                2,
                [
                    "References and Notes",
                    "[1] Mistral AI. Mistral small 3.1, 2025. Model card: https://huggingface.co/mistralai/",
                    "Mistral-Small-3.1-24B-Instruct-2503.",
                    "[2] Heegyu Kim, Taeyang Jeon, Seungtaek Choi, Ji Hoon Hong, Dong Won Jeon, Ga-Yeon Baek,",
                    "Gyeong-Won Kwak, Dong-Hee Lee, Jisu Bae, Chihoon Lee, Yunseo Kim, Seon-Jin Choi,",
                    "Jin-Seong Park, Sung Beom Cho, and Hyunsouk Cho. Towards fully-automated materials",
                    "discovery via large-scale synthesis dataset and expert-level llm-as-a-judge, 2025.",
                    "[3] Google DeepMind. Gemini 2 flash model card, 04 2025. Published April 2025.",
                ]
                .into_iter()
                .map(bare_line)
                .collect(),
            );
            let pages = vec![first, second];
            let sections = find_reference_sections(&pages);
            let headings: Vec<&str> = sections.iter().map(|s| s.heading.as_str()).collect();
            assert_eq!(
                headings,
                vec!["Notes and references", "References and Notes"]
            );

            let stop = Some((2, 0));
            let body = list_body(&pages, &sections[0], stop);
            assert_eq!(body.style, Style::Bare);

            let (refs, _) = extract_citations(&pages);
            let labels: Vec<&str> = refs.iter().filter_map(|r| r.label.as_deref()).collect();
            assert_eq!(
                labels,
                vec![
                    "1", "2", "3", "4", "5", "6", "7", "8", "9", "[1]", "[2]", "[3]"
                ]
            );
            assert_eq!(
                refs[0].raw,
                "1 Q. Zhang, E. Uchaker, S. L. Candelaria and G. Cao, Chem. Soc. Rev., 2013, \
                 42, 3127–3171."
            );
            assert!(
                refs[2]
                    .raw
                    .starts_with("3 C. Vogt and B. M. Weckhuysen, Nat. Rev. Chem.")
            );
            assert!(
                refs[5]
                    .raw
                    .starts_with("6 J. Hachmann, R. Olivares-Amaya, S. Atahan-Evrenk,")
            );
            assert!(refs[5].raw.ends_with("Lett., 2011, 2, 2241–2251."));
            assert_eq!(
                refs[7].raw,
                "8 arXiV, https://arxiv.org/, Accessed: 2025-08-11."
            );
            assert!(refs[9].raw.starts_with("[1] Mistral AI."));
            let indices: Vec<u32> = refs.iter().map(|r| r.index).collect();
            assert_eq!(indices, (1..=12).collect::<Vec<u32>>());
        }

        /// A bare-number run must start at `1` and continue with `2` and `3`:
        /// a list whose lines merely start with numbers is not RSC.
        #[test]
        fn bare_number_style_needs_a_run_from_one() {
            let section = |texts: &[&str]| -> Vec<SectionLine> {
                texts
                    .iter()
                    .enumerate()
                    .map(|(i, t)| SectionLine {
                        page: 1,
                        line: i,
                        column: 0,
                        x0: None,
                        y0: None,
                        size: None,
                        text: (*t).to_string(),
                    })
                    .collect()
            };
            let rsc = section(&[
                "1 Q. Zhang, E. Uchaker, S. L. Candelaria and G. Cao, Chem. Soc.",
                "Rev., 2013, 42, 3127–3171.",
                "2 C. Liu, F. Li, L.-P. Ma and H.-M. Cheng, Adv. Mater., 2010, 22,",
                "3 C. Vogt and B. M. Weckhuysen, Nat. Rev. Chem., 2022, 6, 89–",
            ]);
            assert_eq!(detect_style(&rsc), Style::Bare);
            let no_run = section(&[
                "Smith, J. (2020). A title. Journal 3, 1–2.",
                "2 Kinds of Things. Publisher, 2019.",
                "Wang, X. (2021). Another title. Journal 4, 5–6.",
            ]);
            assert_eq!(detect_style(&no_run), Style::AuthorYear);
            // A lowercase word after the number starts only the expected entry.
            let text = "8 arXiV, https://arxiv.org/, Accessed: 2025-08-11.";
            assert_eq!(
                entry_label(Style::Bare, text, Some(8)),
                Some((8, "8".to_string()))
            );
            assert_eq!(entry_label(Style::Bare, text, Some(7)), None);
            assert_eq!(entry_label(Style::Bare, "111.", Some(4)), None);
        }

        /// arXiv:2506.23487: a double-spaced author-year list whose first
        /// page is set at another x than the second page, so that the
        /// section's indent levels call every first-page line a
        /// continuation. `Surname, I. ... (year)` lines (and an author
        /// list too long for its year, `Elyahu, Y., Hekselman, I., ...`)
        /// start entries after a sentence end regardless; a stray `�` line
        /// does not hide the sentence end before `Dudley`.
        #[test]
        fn author_year_signature_overrides_indent_levels() {
            // Page 1 is set at x = 90 (the stray `\u{FFFD}` at x = 300); page 2
            // starts its entries at x = 72 and indents continuations to 84.
            let first_texts = [
                "Berlinet, A. & Thomas-Agnan, C. (2003). Reproducing Kernel Hilbert Spaces in Probability and Statistics. Springer",
                "US.",
                "Bhatia, R. & Elsner, L. (1994). The hoffman–wielandt inequality in infinite dimensions. Proceedings of the Indian",
                "Academy of Sciences (Mathematical Sciences) 104, 483–494.",
                "Chiu, T. Y. M., Leonard, T. & Tsui, K.-W. (1996). The Matrix-Logarithmic Covariance Model. Journal of the",
                "American Statistical Association 91, 198–210.",
                "Dryden, I. L., Koloydenko, A. & Zhou, D. (2009). Non-Euclidean statistics for covariance matrices, with applications",
                "to diffusion tensor imaging. The Annals of Applied Statistics 3, 1102–1123.",
                "\u{FFFD}",
                "Dudley, R. M. & Norvaisa, R. (2011). Concrete Functional Calculus. New York, NY: Springer.",
                "Elyahu, Y., Hekselman, I., Eizenberg-Magar, I., Berner, O., Strominger, I., Schiller, M., Mittal, K., Ne-",
                "mirovsky, A., Eremenko, E., Vital, A., Simonovsky, E., Chalifa-Caspi, V., Friedman, N., Yeger-Lotem, E.",
                "& Monsonego, A. (2019). Aging promotes reorganization of the CD4 T cell landscape toward extreme regulatory",
                "and effector phenotypes. Science Advances 5, eaaw8330.",
                "Fillard, P., Arsigny, V., Pennec, X., Hayashi, K. M., Thompson, P. M. & Ayache, N. (2007). Measuring brain",
                "variability by extrapolating sparse tensor fields measured on sulcal lines. Neuroimage 34, 639–650.",
                "Friston, K. J. (2011). Functional and effective connectivity: a review. Brain Connectivity 1, 13–36.",
                "Hoff, P. D. & Niu, X. (2012). A covariance regression model. Statistica Sinica , 729–753.",
                "Hsing, T. & Eubank, R. (2015). Theoretical foundations of functional data analysis, with an introduction to linear",
                "operators, vol. 997. John Wiley & Sons.",
            ];
            let mut lines: Vec<Line> = vec![line_at("References", 0, 72.0, 714.0)];
            for (i, text) in first_texts.iter().enumerate() {
                let x0 = if text.starts_with('\u{FFFD}') {
                    300.0
                } else {
                    90.0
                };
                lines.push(line_at(text, 0, x0, 700.0 - 14.0 * i as f32));
            }
            let first = page_of(1, lines);
            let second_texts = [
                "Hu, W., Pan, T., Kong, D. & Shen, W. (2021). Nonparametric matrix response regression with application to brain",
                "imaging data analysis. Biometrics 77, 1227–1240.",
                "Kong, D., An, B., Zhang, J. & Zhu, H. (2020). L2rm: Low-rank linear regression models for high-dimensional",
                "matrix responses. Journal of the American Statistical Association 115, 403–424.",
                "Kroshnin, A., Spokoiny, V. & Suvorikova, A. (2021). Statistical inference for Bures–Wasserstein barycenters. The",
                "Annals of Applied Probability 31, 1264–1298.",
                "Kuchibhotla, A. K. & Chakrabortty, A. (2022). Moving beyond sub-Gaussianity in high-dimensional statistics:",
                "Applications in covariance estimation and linear regression. Information and Inference: A Journal of the IMA 11,",
                "1389–1456.",
                "Lax, P. (2002). Functional Analysis. Pure and Applied Mathematics: A Wiley Series of Texts, Monographs and",
                "Tracts. Wiley-Interscience.",
                "Lee, A. J. (1990). U-Statistics: Theory and Practice. Statistics: A Series of Textbooks and Monographs. Boca Raton,",
                "FL: CRC Press / Routledge. Reprinted or later editions also available.",
                "Markus, A. S. (1964). The eigen- and singular values of the sum and product of linear operators. Russian Mathematical",
                "Surveys 19, 91–120.",
            ];
            let second = page_of(
                2,
                second_texts
                    .iter()
                    .enumerate()
                    .map(|(i, text)| {
                        let starts = text.contains(" (");
                        let x0 = if starts { 72.0 } else { 84.0 };
                        line_at(text, 0, x0, 700.0 - 14.0 * i as f32)
                    })
                    .collect(),
            );
            let pages = vec![first, second];
            let section = find_reference_section(&pages).expect("a list");
            let body = list_body(&pages, &section, None);
            let layout = layout_starts(&body.lines);
            // The indent levels alone call every first-page line a continuation.
            assert!(layout[..20].iter().all(|&start| start == Some(false)));

            let (refs, _) = extract_citations(&pages);
            let labels: Vec<&str> = refs.iter().filter_map(|r| r.label.as_deref()).collect();
            assert_eq!(
                labels,
                vec![
                    "Berlinet2003",
                    "Bhatia1994",
                    "Chiu1996",
                    "Dryden2009",
                    "Dudley2011",
                    "Elyahu2019",
                    "Fillard2007",
                    "Friston2011",
                    "Hoff2012",
                    "Hsing2015",
                    "Hu2021",
                    "Kong2020",
                    "Kroshnin2021",
                    "Kuchibhotla2022",
                    "Lax2002",
                    "Lee1990",
                    "Markus1964",
                ]
            );
            assert_eq!(
                refs[0].raw,
                "Berlinet, A. & Thomas-Agnan, C. (2003). Reproducing Kernel Hilbert Spaces in \
                 Probability and Statistics. Springer US."
            );
            assert!(
                refs[5]
                    .raw
                    .contains("& Monsonego, A. (2019). Aging promotes")
            );
            assert!(refs[5].raw.ends_with("Science Advances 5, eaaw8330."));
            assert!(refs[9].raw.ends_with("vol. 997. John Wiley & Sons."));
        }

        /// The sentence-end test behind the signature: a word's period
        /// counts (after trailing `�`), an initial's does not.
        #[test]
        fn sentence_ends_for_the_signature() {
            assert!(ends_sentence("Springer US."));
            assert!(ends_sentence("1102–1123. \u{FFFD}"));
            assert!(!ends_sentence("Yeger-Lotem, E."));
            assert!(!ends_sentence("Mittal, K., Ne-"));
            assert!(author_list_signature_re().is_match(
                "Elyahu, Y., Hekselman, I., Eizenberg-Magar, I., Berner, O., Strominger, I."
            ));
            assert!(!author_list_signature_re().is_match("Series A, containing papers of a"));
            assert!(!author_year_signature_re().is_match("& Monsonego, A. (2019). Aging"));
            assert!(
                author_year_signature_re()
                    .is_match("Chiu, T. Y. M., Leonard, T. & Tsui, K.-W. (1996). The Matrix")
            );
        }

        /// arXiv:2508.02208 (AAAI): organisation authors followed by the
        /// year sentence (`Alibaba Cloud Qwen Team. 2025a.`, `Anthropic.
        /// 2025b.`, `(Maxwell-Jia), M. J. 2024.`, `xAI (...). 2025.`) start
        /// entries after a complete entry; without layout evidence they
        /// had been merged into the entry before them.
        #[test]
        fn organisation_authors_with_year_sentences() {
            let refs = refs_from_lines(&[
                "AI, M. 2025. Kimi-K2: A Trillion-Parameter Open-Source",
                "Agentic Language Model. GitHub repository and model",
                "card. Released July 2025; utilizes a mixture-of-experts ar-",
                "chitecture with 32B active parameters per forward pass; op-",
                "timized for agentic tasks and tool integration.",
                "Alibaba Cloud Qwen Team. 2025a.",
                "Qwen3-30B-A3B:",
                "A 30B MoE Model with Hybrid Reasoning Modes and",
                "Long-Context Support. Qwen3 Technical Report, Model",
                "Card (Apache 2.0, Hugging Face). Supports enable think-",
                "ing mode (complex reasoning) or fast mode interchange-",
                "ably; 30.5B total vs. 3.3B active params; context up to 131K.",
                "Alibaba Cloud Qwen Team. 2025b. QwQ-32B: A Com-",
                "pact 32B-Parameter Reasoning Model with Reinforcement",
                "Learning and 131K-Token Context Support. Alibaba Cloud",
                "Blog, Qwen Technical Blog.",
                "Released March 5, 2025;",
                "achieves performance comparable to DeepSeek-R1 and",
                "OpenAI’s o1-mini on reasoning benchmarks.",
                "Anthropic. 2025a. Claude 4 Opus. Accessed: 2025-06-01.",
                "Anthropic. 2025b.",
                "Claude 4 Sonnet: A Cost-Effective",
                "Hybrid-Reasoning Model Optimized for Coding and Agen-",
                "tic Workflows. Public release / Model card on Anthropic",
                "Website and Shared via API Platforms.",
                "Released May",
                "22 2025 alongside Claude 4 Opus as a midsize hybrid-",
                "reasoning model.",
                "Cai, K.; and Singh, J. 2025. Google clinches milestone gold",
                "at global math competition, while OpenAI also claims win.",
                "Reuters. Accessed: 2025-07-25.",
                "IMO-Board, T. 2025. International Mathematical Olympiad.",
                "Accessed: 2025-06-01.",
                "(Maxwell-Jia), M. J. 2024. AIME 2024 Dataset. Hugging",
                "Face Dataset. Available at https://huggingface.co/datasets/",
                "Maxwell-Jia/AIME 2024.",
                "xAI (Elon Musk’s AI Company). 2025.",
                "Grok 4: A",
                "Reasoning-Capable Multimodal Model with Native Tool",
                "Use and Real-Time Search Integration.",
            ]);
            let starts: Vec<String> = refs
                .iter()
                .map(|r| r.raw.chars().take(24).collect())
                .collect();
            assert_eq!(
                starts,
                vec![
                    "AI, M. 2025. Kimi-K2: A ",
                    "Alibaba Cloud Qwen Team.",
                    "Alibaba Cloud Qwen Team.",
                    "Anthropic. 2025a. Claude",
                    "Anthropic. 2025b. Claude",
                    "Cai, K.; and Singh, J. 2",
                    "IMO-Board, T. 2025. Inte",
                    "(Maxwell-Jia), M. J. 202",
                    "xAI (Elon Musk’s AI Comp",
                ]
            );
            assert!(
                refs[1]
                    .raw
                    .starts_with("Alibaba Cloud Qwen Team. 2025a. Qwen3-30B-A3B:")
            );
            assert!(refs[1].raw.ends_with("context up to 131K."));
            assert!(
                refs[2]
                    .raw
                    .starts_with("Alibaba Cloud Qwen Team. 2025b. QwQ-32B")
            );
            assert!(refs[2].raw.ends_with("on reasoning benchmarks."));
            assert!(refs[4].raw.ends_with("reasoning model."));
            assert!(refs[5].raw.ends_with("Reuters. Accessed: 2025-07-25."));
            // A continuation line after a sentence end is no organisation start.
            assert!(!organisation_start_re().is_match("Reuters. Accessed: 2025-07-25."));
            assert!(!organisation_start_re().is_match("Released March 5, 2025;"));
        }

        /// arXiv:2508.19485 (Springer LNCS, two columns): author
        /// biographies interrupt the list after entry 48; the list resumes
        /// at `49.` and ends at the next biography that no label follows.
        #[test]
        fn numbered_list_resumes_after_a_biography() {
            let texts = [
                "References",
                "47. Ye, W., Zhao, J., Wang, S., Wang, Y., Zhang, D., Yuan, Z.: Dy-",
                "namic texture based smoke detection using surfacelet transform",
                "and hmt model. Fire Safety Journal 73, 91–101 (2015)",
                "48. Ying, S., Kunming, S., Jing, W., Feng, H., Yuze, G.: Optical",
                "gas detection: key technologies and applications review. Opto-",
                "Electronic Engineering 47(4), 190280–1 (2020)",
                "Xinlong Zhao received his Master’s",
                "degree in Computer Science from",
                "the University of British Columbia in",
                "2025. He is an IEEE Student Member.",
                "He is currently a Computer Vision Al-",
                "gorithm Engineer at Qihoo 360 in Bei-",
                "jing, China. His work focuses on de-",
                "His research interests include Vi-",
                "sion–Language Models, video inpaint-",
                "ing, and camouflaged video object de-",
                "academic research and cutting-edge in-",
                "dustrial applications.",
                "49. Yu, H., Wang, J., Wang, Z., Yang, J., Huang, K., Lu, G., Deng,",
                "F., Zhou, Y.: A lightweight network based on local-global feature",
                "fusion for real-time industrial invisible gas detection with infrared",
                "thermography. Applied Soft Computing 152, 111138 (2024)",
                "50. Yuan, J., Mao, W., Hu, C., Zheng, J., Zheng, D., Yang, Y.: Leak",
                "detection and localization techniques in oil and gas pipeline: A",
                "bibliometric and systematic review. Engineering Failure Analysis",
                "146, 107060 (2023)",
                "Qixiang Pang received his Ph.D.",
                "from Beijing University of Posts and",
                "Telecommunications and completed",
            ];
            let page = page_of(1, texts.iter().map(|t| bare_line(t)).collect());
            let pages = vec![page];
            let section = find_reference_section(&pages).expect("a list");
            let body = list_body(&pages, &section, None);
            assert_eq!(body.style, Style::Dot);
            assert_eq!(body.end, Some((1, 27)));

            let (refs, _) = extract_citations(&pages);
            let labels: Vec<&str> = refs.iter().filter_map(|r| r.label.as_deref()).collect();
            assert_eq!(labels, vec!["47.", "48.", "49.", "50."]);
            assert!(
                refs[1]
                    .raw
                    .ends_with("Opto-Electronic Engineering 47(4), 190280–1 (2020)")
            );
            assert!(refs[2].raw.starts_with("49. Yu, H., Wang, J."));
            assert!(refs[3].raw.ends_with("146, 107060 (2023)"));
            assert!(refs.iter().all(|r| !r.raw.contains("Xinlong")));
            assert!(refs.iter().all(|r| !r.raw.contains("Qixiang")));
        }

        /// A list cut at a section heading (`Appendix A`) does not resume,
        /// even when the next label follows; nor does one whose
        /// interruption numbers its own items from 1.
        #[test]
        fn numbered_list_does_not_resume_after_a_heading() {
            let refs = refs_from_lines(&[
                "[1] A. Author. First. Venue, 2020.",
                "[2] B. Author. Second. Venue, 2021.",
                "Appendix A",
                "[3] The appendix restates the bound.",
            ]);
            assert_eq!(refs.len(), 2);
            assert!(refs[1].raw.ends_with("Venue, 2021."));

            let refs = refs_from_lines(&[
                "1. A. Author. First. Venue, 2020.",
                "2. B. Author. Second. Venue, 2021.",
                "Table 1: Settings of the runs.",
                "1. Warm-up for ten epochs.",
                "3. Decay the rate by half.",
            ]);
            assert_eq!(refs.len(), 2);
            assert!(refs.iter().all(|r| !r.raw.contains("Decay")));
        }
    }

    /// Loop 9 title fixes (holdout failure taxonomy), each built from the
    /// raw text of a dev-split entry whose title did not match the truth.
    mod loop9b_title_tests {
        use super::*;

        /// `title` without the parenthesised year that some styles print
        /// after it (stripped by a separate change).
        fn without_paren_year(title: Option<&str>, year: &str) -> Option<String> {
            title.map(|t| t.trim_end_matches(year).trim_end().to_string())
        }

        /// Nature style (arXiv:2509.24852) and `Title. Year.` entries: the
        /// year follows the title, so the text before the year is not an
        /// author list only; the title after the last initial is read.
        #[test]
        fn titles_before_the_year_are_not_authors() {
            let slayer = parsed(
                "[10] Shrestha, S. B. & Orchard, G. Slayer: Spike layer error reassignment in \
                 time (2018). URL https://arxiv.org/abs/1810.08646. arXiv:1810.08646.",
                Some("[10]"),
            );
            assert_eq!(slayer.authors, vec!["Shrestha, S. B.", "Orchard, G."]);
            assert_eq!(
                without_paren_year(slayer.title.as_deref(), "(2018)").as_deref(),
                Some("Slayer: Spike layer error reassignment in time")
            );
            assert_eq!(slayer.year, Some(2018));
            assert_eq!(slayer.arxiv_id.as_deref(), Some("1810.08646"));

            let fabre = parsed(
                "[5] Fabre, M., Dudchenko, L. & Neftci, E. Structured State Space Model \
                 Dynamics and Parametrization for Spiking Neural Networks (2025). \
                 arXiv:2506.06374.",
                Some("[5]"),
            );
            assert_eq!(
                fabre.authors,
                vec!["Fabre, M.", "Dudchenko, L.", "Neftci, E."]
            );
            assert_eq!(
                without_paren_year(fabre.title.as_deref(), "(2025)").as_deref(),
                Some(
                    "Structured State Space Model Dynamics and Parametrization for Spiking \
                     Neural Networks"
                )
            );

            let glif = parsed(
                "[34] Yao, X., Li, F., Mo, Z. & Cheng, J. Glif: A unified gated leaky \
                 integrate-and-fire neuron for spiking neural networks , Vol. 35, 32160–32171 \
                 (2022).",
                Some("[34]"),
            );
            assert_eq!(
                glif.title.as_deref(),
                Some(
                    "Glif: A unified gated leaky integrate-and-fire neuron for spiking neural networks"
                )
            );
            assert_eq!(glif.authors.len(), 4);
            assert_eq!(glif.year, Some(2022));

            let sion = parsed("Sion, M. On general minimax theorems. 1958.", None);
            assert_eq!(sion.authors, vec!["Sion, M."]);
            assert_eq!(sion.title.as_deref(), Some("On general minimax theorems"));
            assert_eq!(sion.year, Some(1958));

            let cui = parsed(
                "Cui, G., Yuan, L., Ding, N., Yao, G., Zhu, W., Ni, Y., Xie, G., Liu, Z., and \
                 Sun, M. Ultrafeedback: Boosting language models with high-quality feedback. \
                 2023.",
                None,
            );
            assert_eq!(
                cui.title.as_deref(),
                Some("Ultrafeedback: Boosting language models with high-quality feedback")
            );
            assert_eq!(cui.authors.len(), 9);
            assert_eq!(cui.authors[8], "Sun, M.");
            assert_eq!(cui.year, Some(2023));
        }

        /// Author lists that stay author lists: a dotted acronym before an
        /// organisation name (arXiv:2608.28714 `[50]`), particles and `and
        /// others` before the year.
        #[test]
        fn organisation_and_particle_author_lists_stay_authors() {
            let fda = parsed(
                "[50] U.S. Food and Drug Administration, Health Canada, and Medicines and \
                 Healthcare products Regulatory Agency, “Good machine learning practice for \
                 medical device development: Guiding principles,” Joint guiding principles, \
                 October 2021, 2021.",
                Some("[50]"),
            );
            assert_eq!(
                fda.title.as_deref(),
                Some(
                    "Good machine learning practice for medical device development: Guiding principles"
                )
            );

            let entry = parsed(
                "Smith, J., van der Berg, K. and others (2019). A title. Venue.",
                None,
            );
            assert_eq!(entry.authors, vec!["Smith, J.", "van der Berg, K."]);
            assert_eq!(entry.title.as_deref(), Some("A title"));
            assert_eq!(entry.year, Some(2019));
        }

        /// Surname-first lists (arXiv:2503.00030, arXiv:2509.24852): a
        /// title that opens with the word `A` is not one more initial, and
        /// a lowercase initial (`Casas, D. d. l.`) is a particle, not the
        /// title.
        #[test]
        fn surname_first_lists_end_before_a_title_word() {
            let sokota = parsed(
                "Sokota, S., D’Orazio, R., Kolter, J. Z., Loizou, N., Lanctot, M., \
                 Mitliagkas, I., Brown, N., and Kroer, C. A unified approach to \
                 reinforcement learning, quantal response equilibria, and two-player \
                 zero-sum games. arXiv preprint arXiv:2206.05825, 2022.",
                None,
            );
            assert_eq!(
                sokota.title.as_deref(),
                Some(
                    "A unified approach to reinforcement learning, quantal response \
                     equilibria, and two-player zero-sum games"
                )
            );
            assert_eq!(sokota.authors.len(), 8);
            assert_eq!(sokota.authors[7], "Kroer, C.");

            let freund = parsed(
                "Freund, Y. and Schapire, R. E. A decision-theoretic generalization of \
                 on-line learning and an application to boosting. Journal of computer and \
                 system sciences, 55(1):119–139, 1997.",
                None,
            );
            assert_eq!(freund.authors, vec!["Freund, Y.", "Schapire, R. E."]);
            assert_eq!(
                freund.title.as_deref(),
                Some(
                    "A decision-theoretic generalization of on-line learning and an \
                     application to boosting"
                )
            );

            let murray = parsed(
                "[44] Murray, J. D. et al. A hierarchy of intrinsic timescales across \
                 primate cortex. Nature Neuroscience 17 , 1661–1663 (2014). Epub 2014 Nov 10.",
                Some("[44]"),
            );
            assert_eq!(
                murray.title.as_deref(),
                Some("A hierarchy of intrinsic timescales across primate cortex")
            );

            let jiang = parsed(
                "Jiang, A. Q., Sablayrolles, A., Mensch, A., Bamford, C., Chaplot, D. S., \
                 Casas, D. d. l., Bressand, F., Lengyel, G., Lample, G., Saulnier, L., et al. \
                 Mistral 7b. arXiv preprint arXiv:2310.06825, 2023a.",
                None,
            );
            assert_eq!(jiang.title.as_deref(), Some("Mistral 7b"));
            assert_eq!(jiang.arxiv_id.as_deref(), Some("2310.06825"));

            // An initial with its period still continues the list.
            let rossi = parsed("Rossi, R. A. and Lee, K. Deep widgets. Venue, 2020.", None);
            assert_eq!(rossi.authors, vec!["Rossi, R. A.", "Lee, K."]);
            assert_eq!(rossi.title.as_deref(), Some("Deep widgets"));
        }

        /// A `?` inside a title (arXiv:2603.03010, arXiv:2506.08311,
        /// arXiv:2601.12491): the subtitle after it belongs to the title when
        /// the venue or a masked identifier follows the subtitle; venue words
        /// or a journal with its volume right after the `?` end the title.
        #[test]
        fn question_mark_subtitles() {
            let search = parsed(
                "[38] Weiwei Sun, Lingyong Yan, Xinyu Ma, Shuaiqiang Wang, Pengjie Ren, \
                 Zhumin Chen, Dawei Yin, and Zhaochun Ren. 2023. Is ChatGPT Good at Search? \
                 Investigating Large Language Models as Re-Ranking Agents. In Proceedings of \
                 the 2023 Conference on Empirical Methods in Natural Language Processing, \
                 Houda Bouamor, Juan Pino, and Kalika Bali (Eds.). Association for \
                 Computational Linguistics, Singapore, 14918–14937. \
                 doi:10.18653/v1/2023.emnlp-main.923",
                Some("[38]"),
            );
            assert_eq!(
                search.title.as_deref(),
                Some(
                    "Is ChatGPT Good at Search? Investigating Large Language Models as \
                     Re-Ranking Agents"
                )
            );

            let mocked = parsed(
                "[16] Andre Hora and Romain Robbes. 2026. Are Coding Agents Generating \
                 Over-Mocked Tests? An Empirical Study. arXiv:2602.00409 [cs.SE] \
                 https://arxiv.org/abs/2602.00409",
                Some("[16]"),
            );
            assert_eq!(
                mocked.title.as_deref(),
                Some("Are Coding Agents Generating Over-Mocked Tests? An Empirical Study")
            );

            let better = parsed(
                "Galen Weld, Amy X. Zhang, and Tim Althoff. 2022. What Makes Online \
                 Communities ‘Better’? Measuring Values, Consensus, and Conflict across \
                 Thousands of Subreddits. Proceedings of the International AAAI Conference \
                 on Web and Social Media, 16:1121– 1132.",
                None,
            );
            assert_eq!(
                better.title.as_deref(),
                Some(
                    "What Makes Online Communities ‘Better’? Measuring Values, Consensus, \
                     and Conflict across Thousands of Subreddits"
                )
            );

            let tdd = parsed(
                "[2] Toufique Ahmed, Martin Hirzel, Rangeet Pan, Avraham Shinnar, and \
                 Saurabh Sinha. 2024. TDD-Bench Verified: Can LLMs Generate Tests for Issues \
                 Before They Get Resolved? arXiv preprint arXiv:2412.02883 (2024).",
                Some("[2]"),
            );
            assert_eq!(
                tdd.title.as_deref(),
                Some(
                    "TDD-Bench Verified: Can LLMs Generate Tests for Issues Before They Get \
                     Resolved?"
                )
            );

            let live = parsed(
                "[12] Carmen J. Branje and Deborah I. Fels. 2012. LiveDescribe: Can Amateur \
                 Describers Create High-Quality Audio Description? Journal of Visual \
                 Impairment & Blindness 106, 3 (2012), 154–165. \
                 doi:10.1177/0145482X1210600304",
                Some("[12]"),
            );
            assert_eq!(
                live.title.as_deref(),
                Some("LiveDescribe: Can Amateur Describers Create High-Quality Audio Description?")
            );

            let goli = parsed(
                "Goli A, Singh A (2024) Frontiers: Can large language models capture human \
                 preferences? Marketing Science 43(4):709–722.",
                None,
            );
            assert_eq!(
                goli.title.as_deref(),
                Some("Frontiers: Can large language models capture human preferences?")
            );
        }

        #[test]
        fn terminal_title_punctuation_survives_journal_boundary() {
            for mark in ['?', '!'] {
                let text = format!("Can widgets work{mark} Marketing Science 43(4):709–722.");
                let expected = format!("Can widgets work{mark}");
                assert_eq!(&text[..title_end(&text)], expected);
                let raw = format!("Goli A, Singh A (2024) {text}");
                let entry = parsed(&raw, None);
                assert_eq!(entry.title.as_deref(), Some(expected.as_str()));
                assert_eq!(entry.year, Some(2024));
                assert_eq!(entry.raw, raw);
            }
            let text = "Widgets work. Marketing Science 43(4):709–722.";
            assert_eq!(&text[..title_end(text)], "Widgets work");
        }

        /// A masked identifier after a clause is no evidence of a subtitle
        /// when the clause names a venue (`Journal of …`) or a year or
        /// volume follows the identifier; a subtitle followed by venue words
        /// or only a masked identifier still belongs to the title.
        #[test]
        fn question_mark_before_a_venue_clause() {
            let work = parsed(
                "[16] Andre Hora and Romain Robbes. 2026. Does It Work? Journal of \
                 Artificial Intelligence. https://example.org/papers/does-it-work",
                Some("[16]"),
            );
            assert_eq!(work.title.as_deref(), Some("Does It Work?"));
            assert_eq!(
                work.venue.as_deref(),
                Some("Journal of Artificial Intelligence")
            );

            let tdd = parsed(
                "[2] Toufique Ahmed, Martin Hirzel, Rangeet Pan, Avraham Shinnar, and \
                 Saurabh Sinha. 2024. TDD-Bench Verified: Can LLMs Generate Tests for Issues \
                 Before They Get Resolved? A Study of Coding Agents. arXiv preprint \
                 arXiv:2412.02883 (2024).",
                Some("[2]"),
            );
            assert_eq!(
                tdd.title.as_deref(),
                Some(
                    "TDD-Bench Verified: Can LLMs Generate Tests for Issues Before They Get \
                     Resolved? A Study of Coding Agents"
                )
            );

            // The clause and what follows the masked identifier decide.
            let masked = "Journal of Artificial Intelligence.                 ";
            assert!(question_ends_title(masked));
            assert!(question_ends_title(
                "Deep Widget Models.                  (2021), 12(3)."
            ));
            assert!(!question_ends_title(
                "A Systematic Review.                  [cs.SE]"
            ));
            assert!(!question_ends_title(
                "An Empirical Study.                  [cs.SE]"
            ));
            // A bare year after the identifier is no venue evidence.
            let dated = parsed(
                "[16] Andre Hora and Romain Robbes. 2026. Are Coding Agents Generating \
                 Over-Mocked Tests? An Empirical Study. arXiv:2602.00409 (2026).",
                Some("[16]"),
            );
            assert_eq!(
                dated.title.as_deref(),
                Some("Are Coding Agents Generating Over-Mocked Tests? An Empirical Study")
            );
        }

        /// A lowercase surname particle of four or more letters (`della`)
        /// in a surname-first list is a name part, not a title word.
        #[test]
        fn long_surname_particles_stay_in_the_author_list() {
            let rossi = parsed(
                "Rossi, M. della Porta, A. (2020). Deep Learning. Journal of Widgets, 3(1), \
                 1–10.",
                None,
            );
            assert_eq!(rossi.authors, vec!["Rossi, M.", "della Porta, A."]);
            assert_eq!(rossi.title.as_deref(), Some("Deep Learning"));
            assert_eq!(rossi.year, Some(2020));

            let joined = parsed(
                "Rossi, M. and della Porta, A. (2020). Deep Learning. Journal of Widgets.",
                None,
            );
            assert_eq!(joined.authors, vec!["Rossi, M.", "della Porta, A."]);
            assert_eq!(joined.title.as_deref(), Some("Deep Learning"));

            assert!(!reads_as_title("della Porta, A"));
            assert!(!reads_as_title("and van der Berg, K"));
            assert!(reads_as_title(
                "Slayer: Spike layer error reassignment in time"
            ));
            assert!(reads_as_title("On general minimax theorems"));
        }

        /// A part marker after the title sentence stays in the title, so
        /// `Orthogonal polynomials. II` (arXiv:2510.00443 `[7]`) is told
        /// apart from `Orthogonal Polynomials` (`[55]`).
        #[test]
        fn part_markers_stay_in_the_title() {
            let case = parsed(
                "[7] K. M. Case. Orthogonal polynomials. II. J. Math. Phys., 16:1435–1440, 1975.",
                Some("[7]"),
            );
            assert_eq!(case.title.as_deref(), Some("Orthogonal polynomials. II"));
            assert_eq!(case.authors, vec!["K. M. Case"]);
            assert_eq!(case.year, Some(1975));

            let szego = parsed(
                "[55] G. Szegő. Orthogonal Polynomials, volume 23. American Mathematical \
                 Society, 1939.",
                Some("[55]"),
            );
            assert_eq!(szego.title.as_deref(), Some("Orthogonal Polynomials"));

            assert_eq!(
                title_end("Lectures. Part 2. Springer"),
                "Lectures. Part 2".len()
            );
            assert_eq!(
                title_end("Graph minors. IV. Journal"),
                "Graph minors. IV".len()
            );
            // A title sentence followed by the journal keeps its end.
            assert_eq!(title_end("Deep widgets. J. Widgets"), "Deep widgets".len());
        }

        /// Line-end hyphens in a reference list are decided by
        /// [`hyphen_policy`] against the words of the whole section:
        /// `noise-` + `regularized` keeps the hyphen when both halves occur
        /// elsewhere, `pre-` + `serving` joins, and the broken halves
        /// themselves are no evidence.
        #[test]
        fn line_end_hyphens_follow_the_cleanup_policy() {
            let refs = refs_from_lines(&[
                "[1] A. Author. A noise-",
                "regularized prior for noise removal. In Proc. Venue, 2020.",
                "[2] B. Author. Structure pre-",
                "serving and regularized reconstruction. In Proc. Venue, 2021.",
            ]);
            assert_eq!(refs.len(), 2);
            assert!(refs[0].raw.contains("A noise-regularized prior"));
            assert!(refs[1].raw.contains("Structure preserving and"));

            assert_eq!(
                hyphen_break(
                    "a noise-",
                    "regularized prior",
                    "a noise-\nregularized prior"
                ),
                HyphenJoin::Drop
            );
            assert!(attested_in("privacy preserving", "preserving"));
            assert!(!attested_in("privacy preserving", "serving"));
            assert!(!attested_in("a noise-\nregularized", "noise"));
            assert!(attested_in("noise-regularized loss", "noise-regularized"));
        }
    }

    /// Heading forms that open a list, and lines that do not.
    #[test]
    fn heading_variants() {
        for text in [
            "References",
            "7. References",
            "A Bibliography",
            "REFERENCES",
            "Supplementary References",
            "References for the Appendices",
            "References and Notes",
            "References:",
        ] {
            assert!(heading_re().is_match(text), "{text}");
        }
        for text in [
            "The references are listed below.",
            "References [1] and [2] agree.",
        ] {
            assert!(!heading_re().is_match(text), "{text}");
        }
    }

    /// A paper with two lists (multibib: `References` on page 2 and
    /// `References for the Appendices` on page 3, numbered on from the
    /// first): both are segmented, indices continue, markers on any page
    /// resolve against the union, and the appendix between the lists is
    /// scanned for markers too.
    #[test]
    fn multiple_reference_lists_are_concatenated() {
        let body = column_page(1, &["Prior work [1] and [44] and [2, 3]."]);
        let first = column_page(
            2,
            &[
                "References",
                "[1] A. Author. First. Venue, 2020.",
                "[2] B. Author. Second. Venue, 2021.",
                "[3] C. Author. Third. Venue, 2022.",
                "Appendix A",
                "The appendix cites [2].",
            ],
        );
        let second = column_page(
            3,
            &[
                "References for the Appendices",
                "43. Kim, S., Park, J.: Counting trees in planar graphs. Discrete Mathematics 23(1), 11–24 (1989)",
                "44. Lee, H.: Fast matching. In: FOCS ’80. pp. 17–27 (1980)",
            ],
        );
        let pages = vec![body.clone(), first.clone(), second];
        let sections = find_reference_sections(&pages);
        let headings: Vec<&str> = sections.iter().map(|s| s.heading.as_str()).collect();
        assert_eq!(
            headings,
            vec!["References", "References for the Appendices"]
        );
        assert_eq!(
            find_reference_section(&pages).map(|s| s.first_page),
            Some(2)
        );

        let (refs, markers) = extract_citations(&pages);
        assert_eq!(refs.len(), 5);
        let labels: Vec<&str> = refs.iter().filter_map(|r| r.label.as_deref()).collect();
        assert_eq!(labels, vec!["[1]", "[2]", "[3]", "43.", "44."]);
        let indices: Vec<u32> = refs.iter().map(|r| r.index).collect();
        assert_eq!(indices, vec![1, 2, 3, 4, 5]);
        assert_eq!(refs[2].raw, "[3] C. Author. Third. Venue, 2022.");
        assert_eq!(refs[3].page, 3);
        assert_eq!(
            refs[3].title.as_deref(),
            Some("Counting trees in planar graphs")
        );
        assert_eq!(refs[3].authors, vec!["Kim, S.", "Park, J."]);
        assert_eq!(refs[4].year, Some(1980));
        assert_eq!(refs[4].pages.as_deref(), Some("17–27"));
        assert_eq!(refs[4].volume, None);

        let texts: Vec<(u32, &str)> = markers.iter().map(|m| (m.page, m.text.as_str())).collect();
        assert_eq!(
            texts,
            vec![(1, "[1]"), (1, "[44]"), (1, "[2, 3]"), (2, "[2]")]
        );
        assert_eq!(markers[0].targets, vec![1]);
        assert_eq!(markers[1].targets, vec![5]);
        assert_eq!(markers[2].targets, vec![2, 3]);
        assert_eq!(markers[3].targets, vec![2]);
        assert_marker_offsets(&body, &markers);
        assert_marker_offsets(&first, &markers);
    }

    /// Two numbered lists that both start at `[1]` (`References` and
    /// `References for the Appendices`) keep their own numbering: a marker
    /// before the first list's end cites the first list; a marker after
    /// that end (in the appendix the second list serves) cites the second
    /// list first and falls back to the first for a number the second does
    /// not print.
    #[test]
    fn restarted_numbering_resolves_by_position() {
        let body = column_page(1, &["Main text cites [1] and [2, 3]."]);
        let first = column_page(
            2,
            &[
                "References",
                "[1] A. Author. First. Venue, 2020.",
                "[2] B. Author. Second. Venue, 2021.",
                "[3] C. Author. Third. Venue, 2022.",
                "Appendix A",
                "The appendix cites [1] and [3].",
            ],
        );
        let appendix = column_page(3, &["Appendix B proves the bound of [2]."]);
        let second = column_page(
            4,
            &[
                "References for the Appendices",
                "[1] D. Author. Fourth. Venue, 2023.",
                "[2] E. Author. Fifth. Venue, 2024.",
            ],
        );
        let pages = vec![body.clone(), first.clone(), appendix.clone(), second];
        let (refs, markers) = extract_citations(&pages);
        assert_eq!(refs.len(), 5);
        let labels: Vec<&str> = refs.iter().filter_map(|r| r.label.as_deref()).collect();
        assert_eq!(labels, vec!["[1]", "[2]", "[3]", "[1]", "[2]"]);
        let indices: Vec<u32> = refs.iter().map(|r| r.index).collect();
        assert_eq!(indices, vec![1, 2, 3, 4, 5]);

        let sections = find_reference_sections(&pages);
        let repeated = repeated_furniture(&pages);
        let index = RefIndex::build(&refs, &list_extents(&pages, &sections, &repeated));
        assert!(index.numbered);
        assert_eq!(index.spaces.len(), 2);
        assert_eq!(index.spaces[0].end, Some((2, 4)));
        assert_eq!(index.max_number, 3);
        assert_eq!(index.targets_for(&[1, 3], 0), vec![1, 3]);
        assert_eq!(index.targets_for(&[1, 3], 1), vec![4, 3]);
        assert_eq!(home_space(&[None, None], 0), 0);
        assert_eq!(home_space(&[Some(10), None], 9), 0);
        assert_eq!(home_space(&[Some(10), None], 10), 1);
        assert_eq!(home_space(&[Some(0), None], 5), 1);

        let texts: Vec<(u32, &str)> = markers.iter().map(|m| (m.page, m.text.as_str())).collect();
        assert_eq!(
            texts,
            vec![
                (1, "[1]"),
                (1, "[2, 3]"),
                (2, "[1]"),
                (2, "[3]"),
                (3, "[2]")
            ]
        );
        let targets: Vec<Vec<u32>> = markers.iter().map(|m| m.targets.clone()).collect();
        assert_eq!(
            targets,
            vec![vec![1], vec![2, 3], vec![4], vec![3], vec![5]]
        );
        assert_marker_offsets(&body, &markers);
        assert_marker_offsets(&first, &markers);
        assert_marker_offsets(&appendix, &markers);
    }

    /// The label column of an IEEE list emitted apart from its entries
    /// (arXiv:2509.12458): bare `[1]`, `[2]`, `[3]` lines number the list,
    /// so the entries take the printed labels, the index is numbered and
    /// body markers resolve.
    #[test]
    fn detached_labels_number_the_list() {
        let body = column_page(1, &["Prior work [2] builds on [1]."]);
        let list = page_of(
            2,
            vec![
                bare_line("References"),
                bare_line("[1]"),
                bare_line("[2]"),
                bare_line("[3]"),
                bare_line("A. Author, “First title,” Journal One, vol. 1, pp. 1–2, 2020."),
                bare_line("B. Writer, “Second title,” Journal Two, vol. 2, pp. 3–4, 2021."),
                bare_line("C. Third, “Third title,” Journal Three, vol. 3, pp. 5–6, 2022."),
            ],
        );
        let (refs, markers) = extract_citations(&[body.clone(), list]);
        assert_eq!(refs.len(), 3);
        let labels: Vec<&str> = refs.iter().filter_map(|r| r.label.as_deref()).collect();
        assert_eq!(labels, vec!["[1]", "[2]", "[3]"]);
        assert!(refs.iter().all(|r| !r.raw.contains('[')));
        assert!(refs[1].raw.starts_with("B. Writer"));
        assert_eq!(refs[1].title.as_deref(), Some("Second title"));
        assert_eq!(refs[2].year, Some(2022));

        let index = RefIndex::build(&refs, &[]);
        assert!(index.numbered);
        assert_eq!(index.max_number, 3);

        let texts: Vec<(u32, &str)> = markers.iter().map(|m| (m.page, m.text.as_str())).collect();
        assert_eq!(texts, vec![(1, "[2]"), (1, "[1]")]);
        assert_eq!(markers[0].targets, vec![2]);
        assert_eq!(markers[1].targets, vec![1]);
        assert_marker_offsets(&body, &markers);

        // Two bare labels are not a run; the list stays author-year.
        let two = [
            bare_line("[4]"),
            bare_line("[5]"),
            bare_line("A. Author, “First title,” Journal One, vol. 1, pp. 1–2, 2020."),
        ];
        let lines: Vec<SectionLine> = two
            .iter()
            .enumerate()
            .map(|(i, l)| SectionLine {
                page: 1,
                line: i,
                column: 0,
                x0: None,
                y0: None,
                size: None,
                text: l.text.clone(),
            })
            .collect();
        assert!(!detached_run(&lines));
        assert_eq!(detect_style(&lines), Style::AuthorYear);
        assert_eq!(bare_label_number("[12]"), Some(12));
        assert_eq!(bare_label_number("[12] text"), None);

        // Entries beyond the printed labels continue the sequence.
        let mut entries = vec![
            parsed("A. Author, “First title,” 2020.", None),
            parsed("B. Writer, “Second title,” 2021.", None),
            parsed("C. Third, “Third title,” 2022.", None),
        ];
        assign_detached_labels(&mut entries, &[7, 8]);
        let labels: Vec<&str> = entries.iter().filter_map(|e| e.label.as_deref()).collect();
        assert_eq!(labels, vec!["[7]", "[8]", "[9]"]);
    }

    /// `REVTeX` sets the list right after the last appendix without a
    /// heading: a `[1]` line that `[2]` and `[3]` follow opens it.
    #[test]
    fn headingless_numbered_list_is_found() {
        let body = column_page(1, &["Text citing [1] and [2]."]);
        let list = column_page(
            2,
            &[
                "Appendix B: Upper bound system",
                "The bound follows from Eq. (B.1).",
                "[1] U. Author, A first title, Journal of Things 75, 126001 (2012).",
                "[2] N. Writer, A second title, Vol. 212 (Springer, 2023).",
                "[3] R. Third, A third title, Phys. Rev. Lett. 98, 080602 (2007).",
            ],
        );
        let sections = find_reference_sections(&[body.clone(), list.clone()]);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].first_page, 2);
        assert_eq!(sections[0].first_line, 2);
        assert_eq!(sections[0].heading, "");

        let (refs, markers) = extract_citations(&[body.clone(), list]);
        assert_eq!(refs.len(), 3);
        assert_eq!(
            refs[0].raw,
            "[1] U. Author, A first title, Journal of Things 75, 126001 (2012)."
        );
        assert_eq!(refs[2].year, Some(2007));
        let texts: Vec<&str> = markers.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, vec!["[1]", "[2]"]);
        assert!(markers.iter().all(|m| m.page == 1));
        assert_marker_offsets(&body, &markers);
    }

    /// arXiv:2309.10334 (`REVTeX`): the list starts at the foot of the last
    /// appendix page without a heading and its first entry wraps onto the
    /// next page. Pins the heading-less `[n]` path on correctly ordered page
    /// text (the loop-6 failure on this paper came from the reading order,
    /// which interleaved the appendix with the list; see `reading_order.rs`).
    #[test]
    fn headingless_revtex_list_wrapping_across_pages() {
        let last = column_page(
            10,
            &[
                "is the L 2 norm given by",
                "matrix: G = 1.",
                "[1] U. Seifert, Stochastic thermodynamics, fluctuation theorems",
                "and molecular machines, Reports on progress in",
                "physics 75, 126001 (2012).",
            ],
        );
        let next = column_page(
            11,
            &[
                "[2] N. Shiraishi, An Introduction to Stochastic Thermodynamics:",
                "From Basic to Advanced, Vol. 212 (Springer Nature,",
                "2023).",
                "[3] R. Kawai, J. M. R. Parrondo, and C. V. den Broeck, Dissipation:",
                "The phase-space perspective, Phys. Rev. Lett.",
                "98, 080602 (2007).",
                "[4] H. Miyahara and K. Aihara, Work relations with measurement",
                "and feedback control on nonuniform temperature",
                "systems, Phys. Rev. E 98, 042138 (2018).",
            ],
        );
        let pages = [last, next];
        let sections = find_reference_sections(&pages);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].first_page, 10);
        assert_eq!(sections[0].first_line, 2);
        assert_eq!(sections[0].heading, "");

        let (refs, _) = extract_citations(&pages);
        let labels: Vec<&str> = refs.iter().filter_map(|r| r.label.as_deref()).collect();
        assert_eq!(labels, vec!["[1]", "[2]", "[3]", "[4]"]);
        assert!(refs[0].raw.starts_with("[1] U. Seifert"), "{}", refs[0].raw);
        assert!(refs[0].raw.contains("126001 (2012)"), "{}", refs[0].raw);
        assert_eq!(refs[0].year, Some(2012));
        assert_eq!(refs[2].year, Some(2007));
        assert_eq!(refs[3].year, Some(2018));
    }

    /// hyperref `backref` prints the citing pages after the DOI: `039. 4`
    /// and `3639. 2, 3, 8` are not wrapped pieces of the DOI, while `005`,
    /// `00045` and `112670` still are (arXiv:2603.21379, 2108.04588 forms).
    /// A numeric line that continues a DOI is not a page number, and a
    /// hyphen inside a URL survives the line join.
    #[test]
    fn doi_back_references_are_not_joined() {
        assert_eq!(
            doi_of("doi: 10.1016/j.jcp.2017.08.039. 4").as_deref(),
            Some("10.1016/j.jcp.2017.08.039")
        );
        assert_eq!(
            doi_of("doi: 10.1002/cnm.3639. 2, 3, 8, 11, 18").as_deref(),
            Some("10.1002/cnm.3639")
        );
        assert_eq!(
            doi_of("doi: 10.1016/ j.finel.2010.01.007. 3").as_deref(),
            Some("10.1016/j.finel.2010.01.007")
        );
        assert_eq!(
            doi_of("https://doi.org/10.1016/j.jmp.2013.05. 005").as_deref(),
            Some("10.1016/j.jmp.2013.05.005")
        );
        assert_eq!(
            doi_of("https://doi.org/10.1109/SC.2018. 00045.").as_deref(),
            Some("10.1109/SC.2018.00045")
        );
        assert_eq!(
            doi_of("doi:10.1016/j.jbiomech.2025. 112670.").as_deref(),
            Some("10.1016/j.jbiomech.2025.112670")
        );
        assert!(is_back_reference("4"));
        assert!(is_back_reference("11,"));
        assert!(!is_back_reference("005"));
        assert!(!is_back_reference("112670"));
        assert!(!is_back_reference("2023.2"));
        assert_eq!(
            hyphen_break("https://doi.org/10.1214/14-", "sts504", ""),
            HyphenJoin::Keep
        );

        let page = column_page(
            4,
            &[
                "References",
                "Doe, J. (2013). A model of choice. Journal of Mathematical Psychology, 57(1), 1–2. https://doi.org/10.1016/j.jmp.2013.05.",
                "005",
                "Roe, K. (2014). Another model. Statistical Science, 29(1), 3–4. https://doi.org/10.1214/14-",
                "sts504",
            ],
        );
        let (refs, _) = extract_citations(&[page]);
        assert_eq!(refs.len(), 2);
        assert!(refs[0].raw.ends_with("j.jmp.2013.05. 005"));
        assert_eq!(refs[0].doi.as_deref(), Some("10.1016/j.jmp.2013.05.005"));
        assert_eq!(
            refs[0].url.as_deref(),
            Some("https://doi.org/10.1016/j.jmp.2013.05.005")
        );
        assert!(refs[1].raw.ends_with("10.1214/14-sts504"));
        assert_eq!(refs[1].doi.as_deref(), Some("10.1214/14-sts504"));
    }

    /// Springer LNCS / `spmpsci` (arXiv:2508.19485): `Surname, I., Other,
    /// J.K.: Title. Venue vol(issue), pages (year)`; the colon ends the
    /// author list and the title runs to the next sentence end. The end of
    /// a page range before the year is not a volume.
    #[test]
    fn springer_lncs_colon_author_lists() {
        let entry = parsed(
            "1. Badawi, D., Pan, H., Cetin, S.C., Enis Çetin, A.: Computationally efficient \
             spatio-temporal dynamic texture recognition for volatile organic compound (voc) \
             leakage detection in industrial plants. IEEE Journal of Selected Topics in Signal \
             Processing 14(4), 676–687 (2020). DOI 10.1109/JSTSP.2020.2976555",
            Some("1."),
        );
        assert_eq!(
            entry.authors,
            vec!["Badawi, D.", "Pan, H.", "Cetin, S.C.", "Enis Çetin, A."]
        );
        assert_eq!(
            entry.title.as_deref(),
            Some(
                "Computationally efficient spatio-temporal dynamic texture recognition for \
                 volatile organic compound (voc) leakage detection in industrial plants"
            )
        );
        assert_eq!(
            entry.venue.as_deref(),
            Some("IEEE Journal of Selected Topics in Signal Processing")
        );
        assert_eq!(entry.volume.as_deref(), Some("14"));
        assert_eq!(entry.issue.as_deref(), Some("4"));
        assert_eq!(entry.pages.as_deref(), Some("676–687"));
        assert_eq!(entry.year, Some(2020));
        assert_eq!(entry.doi.as_deref(), Some("10.1109/JSTSP.2020.2976555"));

        let entry = parsed(
            "2. Bekuzarov, M., Bermudez, A., Lee, J.Y., Li, H.: Xmem++: Production-level video \
             segmentation from few annotated frames. In: Proceedings of the IEEE/CVF \
             International Conference on Computer Vision (ICCV), pp. 635–644 (2023)",
            Some("2."),
        );
        assert_eq!(
            entry.authors,
            vec!["Bekuzarov, M.", "Bermudez, A.", "Lee, J.Y.", "Li, H."]
        );
        assert_eq!(
            entry.title.as_deref(),
            Some("Xmem++: Production-level video segmentation from few annotated frames")
        );
        assert_eq!(
            entry.venue.as_deref(),
            Some("Proceedings of the IEEE/CVF International Conference on Computer Vision")
        );
        assert_eq!(entry.pages.as_deref(), Some("635–644"));
        assert_eq!(entry.volume, None);
        assert_eq!(entry.year, Some(2023));

        assert_eq!(lncs_authors_end("Doe, J. K.: Title"), Some(10));
        assert_eq!(lncs_authors_end("Doe, J. K., et al.: Title"), Some(18));
        assert_eq!(lncs_authors_end("Doe, J. (2020). Title: subtitle"), None);
        assert_eq!(
            lncs_authors_end("D. Goldberg, D. Nichols, Title: subtitle"),
            None
        );
    }

    /// The layout pass can interleave the lines of a list set in the right
    /// column with a caption and a section of body text set in the left
    /// column (arXiv:2508.19485, page 13). Lines that start left of every
    /// label on the page belong to the other column.
    #[test]
    fn foreign_column_lines_are_dropped_from_numbered_lists() {
        let rows: Vec<(&str, f32)> = vec![
            ("References", 301.4),
            (
                "Fig. 11: Performance-Efficiency Diagram. Blue and red",
                42.1,
            ),
            (
                "1. Badawi, D., Pan, H., Cetin, S.C., Enis Çetin, A.: Computationally",
                301.4,
            ),
            (
                "points represent results on our two datasets, SimGas and",
                42.1,
            ),
            (
                "efficient spatio-temporal dynamic texture recognition for volatile",
                312.8,
            ),
            (
                "IGS-Few, while green points are the average accuracy across",
                42.1,
            ),
            (
                "organic compound (voc) leakage detection in industrial plants.",
                312.8,
            ),
            ("both datasets.", 42.1),
            (
                "IEEE Journal of Selected Topics in Signal Processing 14(4), 676–",
                312.8,
            ),
            ("687 (2020). DOI 10.1109/JSTSP.2020.2976555", 312.8),
            (
                "2. Bekuzarov, M., Bermudez, A., Lee, J.Y., Li, H.: Xmem++:",
                301.4,
            ),
            ("5 Conclusion", 42.1),
            (
                "Production-level video segmentation from few annotated frames.",
                312.8,
            ),
            (
                "In this paper, we presented JVLGS, a novel framework de-",
                42.1,
            ),
            (
                "In: Proceedings of the IEEE/CVF International Conference on",
                312.8,
            ),
            ("Computer Vision (ICCV), pp. 635–644 (2023)", 312.8),
        ];
        let lines: Vec<Line> = rows
            .iter()
            .enumerate()
            .map(|(i, (text, x0))| line_at(text, 0, *x0, 740.0 - 14.0 * i as f32))
            .collect();
        let page = page_of(13, lines);
        let (refs, _) = extract_citations(&[page]);

        assert_eq!(refs.len(), 2);
        assert!(refs[0].raw.starts_with("1. Badawi, D., Pan, H."));
        assert!(
            refs[0]
                .raw
                .contains("recognition for volatile organic compound (voc) leakage detection in industrial plants. IEEE Journal")
        );
        assert_eq!(refs[0].pages.as_deref(), Some("676–687"));
        assert_eq!(refs[0].doi.as_deref(), Some("10.1109/JSTSP.2020.2976555"));
        assert_eq!(
            refs[1].title.as_deref(),
            Some("Xmem++: Production-level video segmentation from few annotated frames")
        );
        assert_eq!(refs[1].year, Some(2023));
        assert!(refs.iter().all(|r| {
            !r.raw.contains("Conclusion")
                && !r.raw.contains("Fig. 11")
                && !r.raw.contains("SimGas")
                && !r.raw.contains("JVLGS")
        }));
    }

    /// IEEE lists whose `[n]` labels the layout pass emits as their own
    /// column (arXiv:2509.12458): the bare labels are dropped and the
    /// initials-first name pattern (`M. Mozaffari,`, `D. Giordan et al.,`)
    /// starts an entry after a complete one. `... H. H.` at a line end is a
    /// wrapped author list, not an entry end.
    #[test]
    fn detached_label_column_and_initials_first_starts() {
        let page = page_of(
            1,
            vec![
                bare_line("[1]"),
                bare_line("[2]"),
                bare_line("[3]"),
                bare_line("REFERENCES"),
                bare_line("M. Mozaffari, X. Lin, and S. Hayes, “Toward 6g with connected sky:"),
                bare_line("Uavs and beyond,” IEEE Communications Magazine, vol. 59, no. 12,"),
                bare_line("pp. 74–80, 2021."),
                bare_line("B. Rinner, C. Bettstetter, H. Hellwagner, and S. Weiss, “Multidrone"),
                bare_line("systems: More than the sum of the parts,” Computer, vol. 54, no. 5,"),
                bare_line("pp. 34–43, 2021."),
                bare_line("[4]"),
                bare_line("[5]"),
                bare_line("M. Gordan, Z. Ismail, K. Ghaedi, Z. Ibrahim, H. Hashim, H. H."),
                bare_line("Ghayeb, and M. Talebkhah, “A brief overview and future perspective"),
                bare_line("of unmanned aerial systems for in-service structural health monitor-"),
                bare_line("ing,” Engineering Advances, vol. 1, no. 1, pp. 9–15, 2021."),
                bare_line("D. Giordan et al., “The use of uavs for engineering geology applica-"),
                bare_line("tions,” Bulletin of Engineering Geology and the Environment, vol. 79,"),
                bare_line("pp. 3437–3481, 2020."),
            ],
        );
        let (refs, _) = extract_citations(&[page]);

        assert_eq!(refs.len(), 4);
        assert!(refs.iter().all(|r| !r.raw.contains('[')));
        assert_eq!(
            refs[0].title.as_deref(),
            Some("Toward 6g with connected sky: Uavs and beyond")
        );
        assert_eq!(refs[0].authors, vec!["M. Mozaffari", "X. Lin", "S. Hayes"]);
        assert_eq!(
            refs[0].venue.as_deref(),
            Some("IEEE Communications Magazine")
        );
        assert_eq!(refs[0].volume.as_deref(), Some("59"));
        assert_eq!(refs[0].issue.as_deref(), Some("12"));
        assert_eq!(refs[0].pages.as_deref(), Some("74–80"));
        assert!(refs[1].raw.starts_with("B. Rinner, C. Bettstetter"));
        assert!(refs[2].raw.starts_with("M. Gordan, Z. Ismail"));
        assert_eq!(refs[2].authors.len(), 7);
        assert_eq!(refs[2].authors[5], "H. H. Ghayeb");
        assert_eq!(
            refs[2].title.as_deref(),
            Some(
                "A brief overview and future perspective of unmanned aerial systems for \
                 in-service structural health monitoring"
            )
        );
        assert!(refs[3].raw.starts_with("D. Giordan et al., “The use"));
        assert_eq!(
            refs[3].title.as_deref(),
            Some("The use of uavs for engineering geology applications")
        );
        assert_eq!(refs[3].pages.as_deref(), Some("3437–3481"));
        assert_eq!(refs[3].year, Some(2020));

        assert!(ends_like_whole_entry("pp. 74–80, 2021."));
        assert!(ends_like_whole_entry("Venue (2020)"));
        assert!(!ends_like_whole_entry("Z. Ibrahim, H. Hashim, H. H."));
        assert!(!ends_like_whole_entry("H. Hashim, and"));
    }

    /// Hyphenated initials with a lowercase second part are initials
    /// (`X.-m. Wu` in Elsevier style, arXiv:2305.13843 [152]; `C.-i. Wang`
    /// in IEEE style); `shouldn’t.` is a sentence end, not an initial; an
    /// APA bracketed descriptor is not part of the title.
    #[test]
    fn lowercase_hyphenated_initials_apostrophes_and_descriptors() {
        let entry = parsed(
            "[152] M. Wang, Y. Lin, G. Lin, K. Yang, X.-m. Wu, M2GRL: A Multi-task Multi-view \
             Graph Representation Learning Framework for Web-scale Recommender Systems, in: \
             Proceedings of the 26th ACM SIGKDD International Conference on Knowledge Discovery \
             & Data Mining, 2020, pp. 2349–2358. doi: 10.1145/3394486.3403284.",
            Some("[152]"),
        );
        assert_eq!(
            entry.title.as_deref(),
            Some(
                "M2GRL: A Multi-task Multi-view Graph Representation Learning Framework for \
                 Web-scale Recommender Systems"
            )
        );
        assert_eq!(
            entry.authors,
            vec!["M. Wang", "Y. Lin", "G. Lin", "K. Yang", "X.-m. Wu"]
        );
        assert_eq!(
            entry.venue.as_deref(),
            Some(
                "Proceedings of the 26th ACM SIGKDD International Conference on Knowledge \
                 Discovery & Data Mining"
            )
        );
        assert_eq!(entry.pages.as_deref(), Some("2349–2358"));
        assert_eq!(entry.year, Some(2020));
        assert_eq!(entry.doi.as_deref(), Some("10.1145/3394486.3403284"));

        let entry = parsed(
            "[31] C.-i. Wang, J.-y. Hung, and Y.-H. Yang, “Tonet: Tone-octave network for \
             singing melody extraction from polyphonic music,” in ICASSP 2022, pp. 1–5.",
            Some("[31]"),
        );
        assert_eq!(
            entry.title.as_deref(),
            Some("Tonet: Tone-octave network for singing melody extraction from polyphonic music")
        );
        assert_eq!(
            entry.authors,
            vec!["C.-i. Wang", "J.-y. Hung", "Y.-H. Yang"]
        );
        assert_eq!(entry.pages.as_deref(), Some("1–5"));
        assert_eq!(entry.year, Some(2022));
        assert!(is_initials("C.-i."));
        assert!(is_initials("J.-M."));
        assert!(!is_initials("Smith"));
        assert!(!is_initials("Ab"));

        let entry = parsed(
            "A. Author and B. Writer. Graphs when they shouldn’t. Proceedings of the Conference \
             on Things, 2021.",
            None,
        );
        assert_eq!(entry.title.as_deref(), Some("Graphs when they shouldn’t"));
        assert_eq!(entry.authors, vec!["A. Author", "B. Writer"]);
        assert!(!period_is_abbreviation("they don't. Next", 10));
        assert!(period_is_abbreviation("by A. Smith", 4));

        let entry = parsed(
            "Doe, J. (2019). Learning to design [Doctoral dissertation, University of \
             Somewhere]. ProQuest Dissertations Publishing.",
            None,
        );
        assert_eq!(entry.title.as_deref(), Some("Learning to design"));
        assert_eq!(entry.authors, vec!["Doe, J."]);
        assert_eq!(entry.year, Some(2019));
        assert_eq!(
            strip_bracket_descriptor("Notes on memory [Pyro Tutorial]"),
            "Notes on memory"
        );
        assert_eq!(strip_bracket_descriptor("Beyond [MASK]"), "Beyond [MASK]");
        assert_eq!(
            strip_bracket_descriptor("[analysis code]"),
            "[analysis code]"
        );
    }

    /// A lowercase handle followed by the year sentence (`gwern. 2020.`)
    /// opens an entry when the entry before it is complete, with the layout
    /// saying entry start; it gets a `gwern2020` label and the handle as
    /// its author.
    #[test]
    fn lowercase_handle_starts_an_entry() {
        let rows: Vec<(&str, f32)> = vec![
            (
                "Smith, J. and Doe, A. (2021). A study of things. Journal of Stuff,",
                72.0,
            ),
            ("5(1), 107–135.", 86.0),
            ("gwern. 2020. The scaling hypothesis. Blog", 72.0),
            ("post. Retrieved 2024-01-01.", 86.0),
            ("Zhang, Q. (2022). Another study. Venue.", 72.0),
        ];
        let mut lines: Vec<Line> = vec![line_at("References", 0, 72.0, 754.0)];
        for (i, (text, x0)) in rows.iter().enumerate() {
            lines.push(line_at(text, 0, *x0, 740.0 - 14.0 * i as f32));
        }
        let page = page_of(7, lines);
        let (refs, _) = extract_citations(&[page]);

        assert_eq!(refs.len(), 3);
        assert_eq!(
            refs[1].raw,
            "gwern. 2020. The scaling hypothesis. Blog post. Retrieved 2024-01-01."
        );
        assert_eq!(refs[1].label.as_deref(), Some("gwern2020"));
        assert_eq!(refs[1].title.as_deref(), Some("The scaling hypothesis"));
        assert_eq!(refs[1].authors, vec!["gwern"]);
        assert_eq!(refs[1].year, Some(2020));
        assert!(refs[2].raw.starts_with("Zhang, Q. (2022)."));
        assert!(handle_start_re().is_match("nostalgebraist. 2020. Interpreting GPT"));
        assert!(!handle_start_re().is_match("et al. 2020. Title"));
        assert!(!handle_start_re().is_match("preprint, 2020."));
    }

    /// LNCS-style running heads sit about 11.5% down the page, outside the
    /// old 8% band, alternating between the authors (even pages) and the
    /// title (odd pages), with the folio on the same row. They repeat on
    /// two or more pages of the document and are dropped from the list.
    #[test]
    fn running_headers_in_the_top_band_are_furniture() {
        let header_page = |number: u32, header: &[(&str, f32)], body: &[&str]| {
            let mut lines: Vec<Line> = header
                .iter()
                .map(|(text, x0)| line_at(text, 0, *x0, 692.0))
                .collect();
            for (i, text) in body.iter().enumerate() {
                lines.push(line_at(text, 0, 72.0, 660.0 - 14.0 * i as f32));
            }
            page_of(number, lines)
        };
        let title = [("Balanced Partitions of Things", 200.0), ("15", 500.0)];
        let authors = [("16", 72.0), ("A. Author and B. Writer", 150.0)];
        let pages = vec![
            header_page(
                15,
                &title,
                &["Some body text about partitions.", "More body text."],
            ),
            header_page(16, &authors, &["The body continues here.", "And here."]),
            header_page(
                17,
                &title,
                &[
                    "References",
                    "[1] C. Person. First title. Venue, 2020.",
                    "[2] D. Person. Second title with a long",
                ],
            ),
            header_page(
                18,
                &authors,
                &["tail. Venue, 2021.", "[3] E. Person. Third. Venue, 2022."],
            ),
        ];
        let flags = furniture_flags(&pages[2]);
        assert_eq!(&flags[..2], &[true, true]);
        assert!(flags[2..].iter().all(|&f| !f));
        let repeated = repeated_furniture(&pages);
        assert!(repeated.contains(&"A. Author and B. Writer".to_string()));
        assert!(repeated.contains(&"Balanced Partitions of Things".to_string()));

        let (refs, _) = extract_citations(&pages);
        assert_eq!(refs.len(), 3);
        assert_eq!(refs[0].raw, "[1] C. Person. First title. Venue, 2020.");
        assert_eq!(
            refs[1].raw,
            "[2] D. Person. Second title with a long tail. Venue, 2021."
        );
        assert_eq!(refs[2].page, 18);
        assert!(refs.iter().all(|r| {
            !r.raw.contains("A. Author and B. Writer") && !r.raw.contains("Balanced Partitions")
        }));
    }

    /// Markers after the list (an appendix) are scanned; math intervals
    /// (`[0, 1]`), symbols (`W[1]-hard`, `x[2]`) and numbers above the list
    /// (`[9]`, `[17]`) are not markers; `[2, Theorem 4]` cites 2; adjacent
    /// groups `[1], [2]` and `[3]–[5]` become one marker each; a citation
    /// glued to a word (`BERT[3]`) still counts.
    #[test]
    fn markers_after_the_list_with_guards() {
        let body = column_page(
            1,
            &[
                "Prior work [1], [2] and [3]–[5] is W[1]-hard on x[2] over [0, 1]; see [2, Theorem 4], [9] and BERT[3].",
            ],
        );
        let refs_page = column_page(
            2,
            &[
                "References",
                "[1] A. Author. First. Venue, 2020.",
                "[2] B. Author. Second. Venue, 2021.",
                "[3] C. Author. Third. Venue, 2022.",
                "[4] D. Author. Fourth. Venue, 2023.",
                "[5] E. Author. Fifth. Venue, 2024.",
                "Appendix A",
                "The appendix cites [4] and [17].",
            ],
        );
        let (refs, markers) = extract_citations(&[body.clone(), refs_page.clone()]);
        assert_eq!(refs.len(), 5);
        let texts: Vec<(u32, &str)> = markers.iter().map(|m| (m.page, m.text.as_str())).collect();
        assert_eq!(
            texts,
            vec![
                (1, "[1], [2]"),
                (1, "[3]–[5]"),
                (1, "[2, Theorem 4]"),
                (1, "[3]"),
                (2, "[4]"),
            ]
        );
        let targets: Vec<Vec<u32>> = markers.iter().map(|m| m.targets.clone()).collect();
        assert_eq!(
            targets,
            vec![vec![1, 2], vec![3, 4, 5], vec![2], vec![3], vec![4]]
        );
        assert_marker_offsets(&body, &markers);
        assert_marker_offsets(&refs_page, &markers);

        assert!(glued_to_word("W[1]-hard", 1, 4));
        assert!(glued_to_word("FPT[1] ", 3, 6));
        assert!(!glued_to_word("PEPNet[43], MoME[44]", 6, 10));
        assert!(!glued_to_word("see [1]", 4, 7));
    }

    mod loop6_parse_tests {
        use super::parsed;

        /// INFORMS / Springer author-year (arxiv 2504.10389): every
        /// `Surname Initials` part before `(year)` is an author.
        #[test]
        fn informs_authors_run_to_the_parenthesised_year() {
            let entry = parsed(
                "Aminian MR, Manshadi V, Niazadeh R (2023) Markovian search with socially \
                 aware constraints. Available at SSRN 4347447 .",
                None,
            );
            assert_eq!(
                entry.authors,
                vec!["Aminian MR", "Manshadi V", "Niazadeh R"]
            );
            assert_eq!(
                entry.title.as_deref(),
                Some("Markovian search with socially aware constraints")
            );
            assert_eq!(entry.year, Some(2023));

            let entry = parsed(
                "Babaioff M, Immorlica N, Kempe D, Kleinberg R (2008) Online auctions and \
                 generalized secretary problems. ACM SIGecom Exchanges 7(2):1–11.",
                None,
            );
            assert_eq!(
                entry.authors,
                vec!["Babaioff M", "Immorlica N", "Kempe D", "Kleinberg R"]
            );
            assert_eq!(
                entry.title.as_deref(),
                Some("Online auctions and generalized secretary problems")
            );
            assert_eq!(entry.venue.as_deref(), Some("ACM SIGecom Exchanges"));
            assert_eq!(entry.volume.as_deref(), Some("7"));
            assert_eq!(entry.issue.as_deref(), Some("2"));
            assert_eq!(entry.pages.as_deref(), Some("1–11"));
            assert_eq!(entry.year, Some(2008));
        }

        /// ACM `[n. d.]` (arxiv 2506.03828): no year; the next sentence is
        /// the title and the access date is not the year.
        #[test]
        fn acm_no_date_token_is_not_the_title() {
            let entry = parsed(
                "[49] WikiALM. [n. d.]. Asset lifecycle management. \
                 https://en.wikipedia.org/wiki/ Enterprise_asset_management. \
                 Accessed: 2025-11-25.",
                Some("[49]"),
            );
            assert_eq!(entry.authors, vec!["WikiALM"]);
            assert_eq!(entry.title.as_deref(), Some("Asset lifecycle management"));
            assert_eq!(
                entry.url.as_deref(),
                Some("https://en.wikipedia.org/wiki/Enterprise_asset_management")
            );
            assert_eq!(entry.year, None);
            assert_eq!(entry.venue, None);

            let entry = parsed(
                "[19] IBM. [n. d.]. IBM Maximo Application Suite. \
                 https://www.ibm.com/products/ maximo Accessed: May 13, 2025.",
                Some("[19]"),
            );
            assert_eq!(entry.authors, vec!["IBM"]);
            assert_eq!(entry.title.as_deref(), Some("IBM Maximo Application Suite"));
            assert_eq!(entry.year, None);

            let entry = parsed(
                "[7] CodabenchTeam. [n.d.]. Codabench. https://www.codabench.org/. \
                 Accessed: 2026-02-06.",
                Some("[7]"),
            );
            assert_eq!(entry.authors, vec!["CodabenchTeam"]);
            assert_eq!(entry.title.as_deref(), Some("Codabench"));
            assert_eq!(entry.year, None);

            let entry = parsed(
                "Smith, J. (n.d.). Title of an undated report. Example Institute.",
                None,
            );
            assert_eq!(entry.authors, vec!["Smith, J."]);
            assert_eq!(entry.title.as_deref(), Some("Title of an undated report"));
            assert_eq!(entry.year, None);
        }

        /// RSC style (arxiv 2510.26824) has no title: authors, journal,
        /// year, volume, pages.
        #[test]
        fn rsc_entries_have_a_journal_and_no_title() {
            let entry = parsed(
                "1 Q. Zhang, E. Uchaker, S. L. Candelaria and G. Cao, Chem. Soc. Rev., 2013, \
                 42, 3127–3171.",
                Some("1"),
            );
            assert_eq!(
                entry.authors,
                vec!["Q. Zhang", "E. Uchaker", "S. L. Candelaria", "G. Cao"]
            );
            assert_eq!(entry.title, None);
            assert_eq!(entry.venue.as_deref(), Some("Chem. Soc. Rev."));
            assert_eq!(entry.year, Some(2013));
            assert_eq!(entry.volume.as_deref(), Some("42"));
            assert_eq!(entry.issue, None);
            assert_eq!(entry.pages.as_deref(), Some("3127–3171"));

            // Unlabelled (the list's bare numbers are not a segmentation style).
            let entry = parsed(
                "1 Q. Zhang, E. Uchaker, S. L. Candelaria and G. Cao, Chem. Soc. Rev., 2013, \
                 42, 3127–3171.",
                None,
            );
            assert_eq!(
                entry.authors,
                vec!["Q. Zhang", "E. Uchaker", "S. L. Candelaria", "G. Cao"]
            );
            assert_eq!(entry.title, None);
            assert_eq!(entry.venue.as_deref(), Some("Chem. Soc. Rev."));

            // Letter-prefixed pages: the volume comes from its own part.
            let entry = parsed(
                "C. Liu, F. Li, L.-P. Ma and H.-M. Cheng, Adv. Mater., 2010, 22, E28–E62.",
                None,
            );
            assert_eq!(
                entry.authors,
                vec!["C. Liu", "F. Li", "L.-P. Ma", "H.-M. Cheng"]
            );
            assert_eq!(entry.title, None);
            assert_eq!(entry.venue.as_deref(), Some("Adv. Mater."));
            assert_eq!(entry.year, Some(2010));
            assert_eq!(entry.volume.as_deref(), Some("22"));

            let entry = parsed(
                "B. Keimer, S. A. Kivelson, M. R. Norman, S. Uchida and J. Zaanen, Nature, \
                 2015, 518, 179–186.",
                None,
            );
            assert_eq!(
                entry.authors,
                vec![
                    "B. Keimer",
                    "S. A. Kivelson",
                    "M. R. Norman",
                    "S. Uchida",
                    "J. Zaanen"
                ]
            );
            assert_eq!(entry.title, None);
            assert_eq!(entry.venue.as_deref(), Some("Nature"));
            assert_eq!(entry.year, Some(2015));
            assert_eq!(entry.volume.as_deref(), Some("518"));
            assert_eq!(entry.pages.as_deref(), Some("179–186"));
        }

        /// The year sentence after a title (`Mistral small 3.1, 2025.`) is not
        /// part of the title.
        #[test]
        fn trailing_year_is_not_part_of_the_title() {
            let entry = parsed(
                "[1] Mistral AI. Mistral small 3.1, 2025. Model card: \
                 https://huggingface.co/mistralai/ Mistral-Small-3.1-24B-Instruct-2503.",
                Some("[1]"),
            );
            assert_eq!(entry.authors, vec!["Mistral AI"]);
            assert_eq!(entry.title.as_deref(), Some("Mistral small 3.1"));
            assert_eq!(entry.year, Some(2025));

            let entry = parsed("[4] Mistral AI. Mistral ocr, 2025.", Some("[4]"));
            assert_eq!(entry.title.as_deref(), Some("Mistral ocr"));
            assert_eq!(entry.year, Some(2025));
        }
    }

    /// Loop 8 marker recall (docs/analysis/markers-loop8-2026-09-28.md):
    /// comma-separated clauses, multi-token names, page-break groups,
    /// superscripts and detached label columns.
    mod loop8_marker_tests {
        use super::super::{
            RefIndex, author_full_key, author_year_markers, extract_citations,
            find_citation_markers, superscript_attached,
        };
        use super::{bare_line, line_at, page_of};
        use crate::schema::{PageText, ReferenceEntry};

        /// An entry with the author strings as the parser gives them.
        fn entry(index: u32, authors: &[&str], year: u16, raw: &str) -> ReferenceEntry {
            ReferenceEntry {
                index,
                raw: raw.to_string(),
                authors: authors.iter().map(|a| (*a).to_string()).collect(),
                year: Some(year),
                page: 9,
                ..ReferenceEntry::default()
            }
        }

        /// `[1]` ... `[n]` entries.
        fn numbered_refs(n: u32) -> Vec<ReferenceEntry> {
            (1..=n)
                .map(|k| ReferenceEntry {
                    index: k,
                    label: Some(format!("[{k}]")),
                    raw: format!("[{k}] A. Author. Title {k}. Venue, 2020."),
                    year: Some(2020),
                    page: 9,
                    ..ReferenceEntry::default()
                })
                .collect()
        }

        /// A page whose text is `text` (no lines needed for marker search).
        fn text_page(number: u32, text: &str) -> PageText {
            let mut page = PageText::new(number, 612.0, 792.0, 0);
            page.text = text.to_string();
            page
        }

        /// Author-year markers in `text` as (marker text, targets).
        fn found_in(text: &str, index: &RefIndex) -> Vec<(String, Vec<u32>)> {
            let mut found = author_year_markers(text, &(0..text.len()), index);
            found.sort_by_key(|f| f.range.start);
            found.into_iter().map(|f| (f.text, f.targets)).collect()
        }

        fn one(text: &str, targets: &[u32]) -> Vec<(String, Vec<u32>)> {
            vec![(text.to_string(), targets.to_vec())]
        }

        /// arXiv:2602.16061, 2504.10389, 2508.02208, 2401.15719: clauses
        /// split on a comma after a year; letter lists, year lists and
        /// locators resolve every year and letter.
        #[test]
        fn comma_clauses_year_lists_letter_lists_and_locators() {
            let refs = vec![
                entry(
                    1,
                    &["Rubin DB"],
                    1976,
                    "Rubin DB (1976) Inference and missing data.",
                ),
                entry(
                    2,
                    &["Robins JM", "Rotnitzky A"],
                    1994,
                    "Robins JM, Rotnitzky A, Zhao LP (1994) Estimation.",
                ),
                entry(
                    3,
                    &["Qin J", "Shao J"],
                    2008,
                    "Qin J, Shao J, Zhang B (2008) Efficient imputation.",
                ),
                entry(
                    4,
                    &["Banerjee S", "Gkatzelis V"],
                    2022,
                    "Banerjee S, Gkatzelis V, Gorokh A, Jin B (2022) Online Nash.",
                ),
                entry(
                    5,
                    &["Banerjee S", "Gkatzelis V"],
                    2023,
                    "Banerjee S, Gkatzelis V, Hossain S (2023a) Proportionally fair.",
                ),
                entry(
                    6,
                    &["Banerjee S", "Hssaine C"],
                    2023,
                    "Banerjee S, Hssaine C, Sinclair SR (2023b) Online fair allocation.",
                ),
                entry(
                    7,
                    &["Barman S", "Khan A"],
                    2022,
                    "Barman S, Khan A, Maiti A (2022) Universal and tight.",
                ),
                entry(
                    8,
                    &["Politis DN", "Romano JP"],
                    1994,
                    "Politis DN, Romano JP (1994) Large sample confidence regions.",
                ),
                entry(9, &["OpenAI"], 2024, "OpenAI. 2024. GPT-4o."),
                entry(10, &["OpenAI"], 2025, "OpenAI. 2025a. Introducing GPT-4.1."),
                entry(11, &["OpenAI"], 2025, "OpenAI. 2025b. Introducing o4-mini."),
                entry(12, &["OpenAI"], 2025, "OpenAI. 2025c. OpenAI o3."),
                entry(
                    13,
                    &["Doan TT"],
                    2021,
                    "Doan TT (2021) Finite-time analysis.",
                ),
                entry(
                    14,
                    &["Doan TT"],
                    2022,
                    "Doan TT (2022) Nonlinear two-time-scale.",
                ),
            ];
            let index = RefIndex::build(&refs, &[]);

            let text = "(Rubin 1976, Robins et al. 1994, Qin et al. 2008)";
            assert_eq!(found_in(text, &index), one(text, &[1, 2, 3]));
            // Layout debris between clauses does not stop the scan.
            let text = "(Rubin 1976, Robins et al. 1994,\n⊥⊥\nQin et al. 2008)";
            assert_eq!(found_in(text, &index), one(text, &[1, 2, 3]));
            // A letter list stays with its author.
            let text = "(Banerjee et al. 2023a,b, Barman et al. 2022)";
            assert_eq!(found_in(text, &index), one(text, &[5, 6, 7]));
            // A year list: every year of the author resolves.
            let text = "(Jin\nand Ma 2022, Banerjee et al. 2022, 2023a)";
            assert_eq!(found_in(text, &index), one(text, &[4, 5]));
            let text = "(Gupta\net al. 2019, Doan 2021, 2022)";
            assert_eq!(found_in(text, &index), one(text, &[13, 14]));
            // Letters in any order, then a further year.
            let text = "(OpenAI 2025c,b,a, 2024)";
            assert_eq!(found_in(text, &index), one(text, &[12, 11, 10, 9]));
            // A narrative citation with a locator.
            let text = "Politis and Romano (1994, Theorem 3.1)";
            assert_eq!(found_in(text, &index), one(text, &[8]));
            let text = "as in Doan (2021, 2022).";
            assert_eq!(found_in(text, &index), one("Doan (2021, 2022)", &[13, 14]));

            // A parenthetical without a resolvable clause is a marker only in
            // the strict `Name, 2020` form.
            assert!(found_in("(Received March 2020)", &index).is_empty());
            assert_eq!(found_in("(Smith, 2020)", &index), one("(Smith, 2020)", &[]));
        }

        /// arXiv:2508.02208, 2602.16061, 2401.15719, 2603.05575, 2603.12824,
        /// 2601.12491: multi-token, particle and organisation first authors
        /// match the whole first-author name, exact matches first.
        #[test]
        fn multi_token_particle_and_organisation_names() {
            let refs = vec![
                entry(
                    1,
                    &["Alibaba Cloud Qwen Team"],
                    2025,
                    "Alibaba Cloud Qwen Team. 2025a. Qwen3-30B-A3B.",
                ),
                entry(
                    2,
                    &["Alibaba Cloud Qwen Team"],
                    2025,
                    "Alibaba Cloud Qwen Team. 2025b. QwQ-32B.",
                ),
                entry(
                    3,
                    &["Qwen Team, A. C."],
                    2024,
                    "Qwen Team, A. C. 2024. Qwen2.5-72B-Instruct.",
                ),
                entry(
                    4,
                    &["Qwen Team, A. C."],
                    2025,
                    "Qwen Team, A. C. 2025. Qwen3-235B-A22B.",
                ),
                entry(5, &["Nomic AI"], 2024, "Nomic AI. 2024. multimodal-7b."),
                entry(
                    6,
                    &[],
                    2021,
                    "de Moura, L.; and Ullrich, S. 2021. The Lean 4 Theorem Prover.",
                ),
                entry(
                    7,
                    &["Van Roy B"],
                    2006,
                    "Van Roy B (2006) Performance loss bounds.",
                ),
                entry(
                    8,
                    &["De Vito, E.", "Rosasco, L."],
                    2005,
                    "De Vito, E., Rosasco, L., and Caponnetto, A. (2005). Model selection.",
                ),
                entry(
                    9,
                    &["Tchetgen Tchetgen EJ"],
                    2010,
                    "Tchetgen Tchetgen EJ (2010) Doubly robust estimation.",
                ),
                entry(
                    10,
                    &["Omar El Malki", "Manoel Horta Ribeiro"],
                    2023,
                    "Omar El Malki and Manoel Horta Ribeiro. 2023. Bonsai.",
                ),
                entry(
                    11,
                    &["Tsitsiklis J"],
                    1996,
                    "Tsitsiklis J, Van Roy B (1996) Analysis of temporal-difference learning.",
                ),
                entry(
                    12,
                    &["Caponnetto, A.", "De Vito, E."],
                    2007,
                    "Caponnetto, A. and De Vito, E. (2007). Optimal rates.",
                ),
                entry(
                    13,
                    &["Miao W"],
                    2016,
                    "Miao W, Tchetgen Tchetgen EJ (2016) On varieties.",
                ),
            ];
            let index = RefIndex::build(&refs, &[]);

            let text = "(Alibaba Cloud Qwen Team 2025a)";
            assert_eq!(found_in(text, &index), one(text, &[1]));
            // `qwen team` matches its own entry exactly, not the Alibaba
            // entries that end with it.
            let text = "(Qwen Team 2025)";
            assert_eq!(found_in(text, &index), one(text, &[4]));
            let text = "(Qwen Team\n2024, 2025; Alibaba Cloud Qwen Team 2025b)";
            assert_eq!(found_in(text, &index), one(text, &[3, 4, 2]));
            let text = "(Nomic AI 2024)";
            assert_eq!(found_in(text, &index), one(text, &[5]));
            let text = "(de Moura and Ullrich 2021; x)";
            assert_eq!(found_in(text, &index), one(text, &[6]));
            let text = "(De Vito et al. 2005)";
            assert_eq!(found_in(text, &index), one(text, &[8]));
            let text = "(El Malki et al., 2023)";
            assert_eq!(found_in(text, &index), one(text, &[10]));
            let text = "(Tsitsiklis and Van Roy\n1996)";
            assert_eq!(found_in(text, &index), one(text, &[11]));
            let text = "(Caponnetto and De Vito, 2007)";
            assert_eq!(found_in(text, &index), one(text, &[12]));
            let text = "(Miao and Tchetgen Tchetgen\n2016)";
            assert_eq!(found_in(text, &index), one(text, &[13]));
            let text = "Tchetgen Tchetgen (2010)";
            assert_eq!(found_in(text, &index), one(text, &[9]));
            // A capitalised word before the name is not part of the marker.
            assert_eq!(
                found_in("In Van Roy (2006) the bound", &index),
                one("Van Roy (2006)", &[7])
            );

            assert_eq!(
                author_full_key("Tchetgen Tchetgen EJ").as_deref(),
                Some("tchetgen tchetgen")
            );
            assert_eq!(
                author_full_key("Alibaba Cloud Qwen Team").as_deref(),
                Some("alibaba cloud qwen team")
            );
            assert_eq!(author_full_key("E. J. Van Roy").as_deref(), Some("van roy"));
            assert_eq!(author_full_key("Qwen Team, A. C."), None);
            assert_eq!(author_full_key("Rubin DB"), None);
        }

        /// arXiv:2508.02208: three or more authors before the year.
        #[test]
        fn three_or_more_authors() {
            let refs = vec![
                entry(
                    1,
                    &["Nipkow, T.", "Paulson, L. C.", "Wenzel, M."],
                    2002,
                    "Nipkow, T.; Paulson, L. C.; and Wenzel, M. 2002. Isabelle/HOL.",
                ),
                entry(
                    2,
                    &["Zheng, K.", "Han, J. M.", "Polu, S."],
                    2022,
                    "Zheng, K.; Han, J. M.; and Polu, S. 2022. MiniF2F.",
                ),
            ];
            let index = RefIndex::build(&refs, &[]);
            let text = "(Nipkow, Paulson, and Wenzel 2002)";
            assert_eq!(found_in(text, &index), one(text, &[1]));
            let text = "(Zheng, Han,\nand Polu 2022; Nipkow,\nPaulson, and Wenzel 2002)";
            assert_eq!(found_in(text, &index), one(text, &[2, 1]));
            let text = "Zheng, Han, and Polu (2022)";
            assert_eq!(found_in(text, &index), one(text, &[2]));
        }

        /// arXiv:2410.17124, 2501.17300, 2509.17930: `et al` without a
        /// period, and `et al.` citations without parentheses that resolve.
        #[test]
        fn et_al_without_period_and_bare_citations() {
            let refs = vec![
                entry(
                    1,
                    &["Duan, J."],
                    2020,
                    "Duan, J. et al. (2020). Primary study.",
                ),
                entry(2, &["Liu, X."], 2020, "Liu, X. et al. (2020). Deep study."),
                entry(
                    3,
                    &["Alexander, J."],
                    2015,
                    "Alexander, J. et al. (2015). Epistemic.",
                ),
                entry(
                    4,
                    &["Barrault, L."],
                    2023,
                    "Barrault, L. et al. (2023). SeamlessM4T.",
                ),
            ];
            let index = RefIndex::build(&refs, &[]);
            assert_eq!(
                found_in("Good\nDuan et al 2020\nGood", &index),
                one("Duan et al 2020", &[1])
            );
            assert_eq!(
                found_in("Liu et al (2020) show", &index),
                one("Liu et al (2020)", &[2])
            );
            assert_eq!(
                found_in("As Alexander et al. 2015 put it", &index),
                one("Alexander et al. 2015", &[3])
            );
            assert_eq!(
                found_in("text-to-\nBarrault et al., 2023). See", &index),
                one("Barrault et al., 2023", &[4])
            );
            assert!(found_in("Nobody et al 2020 is unknown", &index).is_empty());
        }

        /// arXiv:2602.16061, 2503.00030 (`(Gui and` / `Toubia 2023)`) and
        /// 2510.26824 (`[6, 7, …` / `41]`): a group left open at a page end
        /// is read across the break and reported where it starts.
        #[test]
        fn groups_open_at_a_page_end_continue_on_the_next_page() {
            let refs = vec![
                entry(
                    1,
                    &["Horton JJ"],
                    2023,
                    "Horton JJ (2023) Large language models.",
                ),
                entry(
                    2,
                    &["Goli A", "Singh A"],
                    2024,
                    "Goli A, Singh A (2024) Frontiers.",
                ),
                entry(
                    3,
                    &["Brand J", "Israeli A"],
                    2024,
                    "Brand J, Israeli A, Ngwe D (2024) Using LLMs.",
                ),
                entry(
                    4,
                    &["Gui G", "Toubia O"],
                    2023,
                    "Gui G, Toubia O (2023) The challenge.",
                ),
                entry(
                    5,
                    &["Li P", "Castelo N"],
                    2024,
                    "Li P, Castelo N (2024) Frontiers.",
                ),
            ];
            let pages = vec![
                text_page(1, "Prior work (Horton 2023, Goli and Singh 2024, Brand"),
                text_page(2, "et al. 2024) shows. Later (Gui and"),
                text_page(3, "Toubia 2023, Li et al. 2024) too."),
            ];
            let markers = find_citation_markers(&pages, &refs);
            let got: Vec<(u32, u32, &str, Vec<u32>)> = markers
                .iter()
                .map(|m| (m.page, m.offset, m.text.as_str(), m.targets.clone()))
                .collect();
            assert_eq!(
                got,
                vec![
                    (
                        1,
                        11,
                        "(Horton 2023, Goli and Singh 2024, Brand\net al. 2024)",
                        vec![1, 2, 3]
                    ),
                    (2, 26, "(Gui and\nToubia 2023, Li et al. 2024)", vec![4, 5]),
                ]
            );

            let refs = numbered_refs(10);
            let pages = vec![
                text_page(1, "as shown by [6, 7, 8,"),
                text_page(2, "9, 10] and [2]."),
            ];
            let markers = find_citation_markers(&pages, &refs);
            let got: Vec<(u32, u32, &str, Vec<u32>)> = markers
                .iter()
                .map(|m| (m.page, m.offset, m.text.as_str(), m.targets.clone()))
                .collect();
            assert_eq!(
                got,
                vec![
                    (1, 12, "[6, 7, 8,\n9, 10]", vec![6, 7, 8, 9, 10]),
                    (2, 11, "[2]", vec![2]),
                ]
            );
        }

        /// arXiv:2510.26824 (RSC): superscript runs attached to a word cite
        /// the numbered list; powers, units, affiliation marks and pages
        /// with bracket markers are left alone, and an author-year document
        /// has no superscript citations.
        #[test]
        fn superscript_runs_cite_a_numbered_list() {
            let mut refs = numbered_refs(11);
            for entry in &mut refs {
                let number = entry.index;
                entry.label = Some(format!("{number}."));
            }
            let body = text_page(
                1,
                "¹Department of Chemistry\nMaterials discovery underpins the literature.⁵ \
                 Initiatives such as the Materials Genome Initiative⁵⁻⁷ and\nChemRxiv¹⁰,¹¹, with \
                 data⁸,⁹ in km² and 10⁵ units.\n³Prein showed it.",
            );
            let bracketed = text_page(2, "The SI cites [2] beside word⁴.");
            let markers = find_citation_markers(&[body, bracketed], &refs);
            let got: Vec<(u32, &str, Vec<u32>)> = markers
                .iter()
                .map(|m| (m.page, m.text.as_str(), m.targets.clone()))
                .collect();
            assert_eq!(
                got,
                vec![
                    (1, "⁵", vec![5]),
                    (1, "⁵⁻⁷", vec![5, 6, 7]),
                    (1, "¹⁰,¹¹", vec![10, 11]),
                    (1, "⁸,⁹", vec![8, 9]),
                    (1, "³", vec![3]),
                    (2, "[2]", vec![2]),
                ]
            );

            assert!(superscript_attached("literature.⁵ x", 11, 14));
            assert!(!superscript_attached("R² value", 1, 3));

            let refs = vec![entry(1, &["Smith, A."], 2020, "Smith, A. (2020). Title.")];
            let page = text_page(1, "a footnote mark here¹ and Smith (2020).");
            let markers = find_citation_markers(&[page], &refs);
            let texts: Vec<&str> = markers.iter().map(|m| m.text.as_str()).collect();
            assert_eq!(texts, vec!["Smith (2020)"]);
        }

        /// arXiv:2509.12458 (IEEE): the label column is detached; `[1]` and
        /// `[2]` sit above `REFERENCES`, and the first label of each column
        /// block (`[3]`, `[5]`) is at a page top where it repeats like a
        /// running header. Labels come from the printed `[n]` on each entry's
        /// row, the entry after a page-top label (`P. De Petris`) is not
        /// merged into the one before, and the label lines are no markers.
        #[test]
        fn detached_labels_are_read_from_their_rows() {
            let body = page_of(
                1,
                vec![line_at(
                    "UAVs [1] enable inspection [3], [4] and mapping [6].",
                    0,
                    72.0,
                    400.0,
                )],
            );
            let list = page_of(
                2,
                vec![
                    line_at("[1]", 0, 50.0, 600.0),
                    line_at("[2]", 0, 50.0, 570.0),
                    line_at("REFERENCES", 1, 120.0, 620.0),
                    line_at(
                        "M. Mozaffari, X. Lin, and S. Hayes, “Toward 6g with connected sky:",
                        1,
                        66.0,
                        600.0,
                    ),
                    line_at(
                        "Uavs and beyond,” IEEE Communications Magazine, vol. 59, no. 12,",
                        1,
                        66.0,
                        590.0,
                    ),
                    line_at("pp. 74–80, 2021.", 1, 66.0, 580.0),
                    line_at(
                        "B. Rinner, C. Bettstetter, H. Hellwagner, and S. Weiss, “Multidrone",
                        1,
                        66.0,
                        570.0,
                    ),
                    line_at(
                        "systems: More than the sum of the parts,” Computer, vol. 54, no. 5,",
                        1,
                        66.0,
                        560.0,
                    ),
                    line_at("pp. 34–43, 2021.", 1, 66.0, 550.0),
                    line_at("[3]", 2, 310.0, 740.0),
                    line_at("[4]", 2, 310.0, 710.0),
                    line_at(
                        "P. De Petris, H. Nguyen, M. Dharmadhikari, et al., “Rmf-owl: A",
                        3,
                        326.0,
                        740.0,
                    ),
                    line_at(
                        "collision-tolerant flying robot for autonomous subterranean exploration,”",
                        3,
                        326.0,
                        730.0,
                    ),
                    line_at("in Proc. ICUAS, IEEE, 2022, pp. 536–543.", 3, 326.0, 720.0),
                    line_at(
                        "Z. Xu, L. Wu, M. Gerke, R. Wang, and H. Yang, “Skeletal camera",
                        3,
                        326.0,
                        710.0,
                    ),
                    line_at(
                        "network embedded structure-from-motion,” ISPRS, vol. 121, pp. 113–127, 2016.",
                        3,
                        326.0,
                        700.0,
                    ),
                ],
            );
            let next = page_of(
                3,
                vec![
                    line_at("[5]", 0, 50.0, 740.0),
                    line_at("[6]", 0, 50.0, 720.0),
                    line_at(
                        "T. Schenk, “Introduction to photogrammetry,” The Ohio State University,",
                        1,
                        66.0,
                        740.0,
                    ),
                    line_at("Columbus, Tech. Rep., 2005.", 1, 66.0, 730.0),
                    line_at(
                        "L. Kovanič, B. Topitzer, and M. Blištanová, “Review of photogrammetric",
                        1,
                        66.0,
                        720.0,
                    ),
                    line_at(
                        "and lidar applications of uav,” Applied Sciences, vol. 13, p. 6732, 2023.",
                        1,
                        66.0,
                        710.0,
                    ),
                ],
            );
            let (refs, markers) = extract_citations(&[body, list, next]);

            let labels: Vec<&str> = refs.iter().filter_map(|r| r.label.as_deref()).collect();
            assert_eq!(labels, vec!["[1]", "[2]", "[3]", "[4]", "[5]", "[6]"]);
            assert!(refs[0].raw.starts_with("M. Mozaffari"));
            assert_eq!(
                refs[1].raw,
                "B. Rinner, C. Bettstetter, H. Hellwagner, and S. Weiss, “Multidrone systems: \
                 More than the sum of the parts,” Computer, vol. 54, no. 5, pp. 34–43, 2021."
            );
            assert!(refs[2].raw.starts_with("P. De Petris"));
            assert!(refs[3].raw.starts_with("Z. Xu"));
            assert!(refs[4].raw.starts_with("T. Schenk"));
            assert!(refs[5].raw.starts_with("L. Kovanič"));

            let got: Vec<(u32, &str, Vec<u32>)> = markers
                .iter()
                .map(|m| (m.page, m.text.as_str(), m.targets.clone()))
                .collect();
            assert_eq!(
                got,
                vec![
                    (1, "[1]", vec![1]),
                    (1, "[3], [4]", vec![3, 4]),
                    (1, "[6]", vec![6]),
                ]
            );
        }

        /// Without positions, labels set above the heading still number the
        /// entries before the first label of the list.
        #[test]
        fn labels_above_the_heading_prefix_the_detached_labels() {
            let page = page_of(
                1,
                vec![
                    bare_line("[1]"),
                    bare_line("[2]"),
                    bare_line("References"),
                    bare_line("A. Author, “First title,” Journal One, vol. 1, pp. 1–2, 2020."),
                    bare_line("B. Writer, “Second title,” Journal Two, vol. 2, pp. 3–4, 2021."),
                    bare_line("[3]"),
                    bare_line("[4]"),
                    bare_line("[5]"),
                    bare_line("C. Third, “Third title,” Journal Three, vol. 3, pp. 5–6, 2022."),
                    bare_line("D. Fourth, “Fourth title,” Journal Four, vol. 4, pp. 7–8, 2023."),
                    bare_line("E. Fifth, “Fifth title,” Journal Five, vol. 5, pp. 9–10, 2024."),
                ],
            );
            let (refs, _) = extract_citations(&[page]);
            let labels: Vec<&str> = refs.iter().filter_map(|r| r.label.as_deref()).collect();
            assert_eq!(labels, vec!["[1]", "[2]", "[3]", "[4]", "[5]"]);
            assert!(refs[2].raw.starts_with("C. Third"));
        }
    }

    mod loop12_marker_tests {
        //! Loop 12 marker fixes (`docs/analysis/refs-loop12-2026-09-28.md`,
        //! ranks 1, 9b, 12, 15 and 17).

        use super::super::{
            RefIndex, author_full_key, author_surname, author_year_markers, closes_line_above,
            detached_superscript_markers, find_citation_markers, glued_to_word,
            superscript_attached, trailing_caps_key,
        };
        use super::assert_marker_offsets;
        use crate::schema::{PageText, ReferenceEntry};

        /// An author-year entry with the author strings as the parser gives
        /// them.
        fn entry(index: u32, authors: &[&str], year: u16, raw: &str) -> ReferenceEntry {
            ReferenceEntry {
                index,
                raw: raw.to_string(),
                authors: authors.iter().map(|a| (*a).to_string()).collect(),
                year: Some(year),
                page: 9,
                ..ReferenceEntry::default()
            }
        }

        /// Entries 1 to `n` labelled `label(k)`.
        fn labelled_refs(n: u32, label: fn(u32) -> String) -> Vec<ReferenceEntry> {
            (1..=n)
                .map(|k| ReferenceEntry {
                    index: k,
                    label: Some(label(k)),
                    raw: format!("{} A. Author. Title {k}. Venue, 2020.", label(k)),
                    year: Some(2020),
                    page: 9,
                    ..ReferenceEntry::default()
                })
                .collect()
        }

        fn bare_label(k: u32) -> String {
            k.to_string()
        }

        fn bracket_label(k: u32) -> String {
            format!("[{k}]")
        }

        /// A page whose text is `text` (no lines needed for marker search).
        fn text_page(number: u32, text: &str) -> PageText {
            let mut page = PageText::new(number, 612.0, 792.0, 0);
            page.text = text.to_string();
            page
        }

        /// Author-year markers in `text` as (marker text, targets).
        fn found_in(text: &str, index: &RefIndex) -> Vec<(String, Vec<u32>)> {
            let mut found = author_year_markers(text, &(0..text.len()), index);
            found.sort_by_key(|f| f.range.start);
            found.into_iter().map(|f| (f.text, f.targets)).collect()
        }

        /// Detached superscript markers in `text[..window_end]`.
        fn detached_in(text: &str, window_end: usize, index: &RefIndex) -> Vec<(String, Vec<u32>)> {
            detached_superscript_markers(text, &(0..window_end), index, &[])
                .into_iter()
                .map(|f| (f.text, f.targets))
                .collect()
        }

        fn one(text: &str, targets: &[u32]) -> Vec<(String, Vec<u32>)> {
            vec![(text.to_string(), targets.to_vec())]
        }

        /// arXiv:2510.26824 (RSC, bare labels `1`–`90`): Unicode superscript
        /// runs that text cleanup left on lines of their own between body
        /// lines (`⁵⁻⁷`) cite the list at their own line; plain digits on
        /// lines of their own (`1`, `2`) do not (they read the same as axis
        /// ticks); a Unicode run opening a line before `, which` closes the
        /// line above (`Semantic Scholar⏎¹⁰,¹¹, which`).
        #[test]
        fn superscript_lines_of_their_own_cite_a_numbered_list() {
            let refs = labelled_refs(12, bare_label);
            let body = text_page(
                1,
                "Materials discovery underpins advances in energy conversion ,\n1\n\
                 energy storage , catalysis , and many other technologies.\n2\n3\n4\n\
                 Associated challenges have driven decades of work\n⁵⁻⁷\n\
                 and arXiv , ChemRxiv , and Semantic Scholar\n¹⁰,¹¹, which is filtered by us.",
            );
            let markers = find_citation_markers(std::slice::from_ref(&body), &refs);
            let got: Vec<(&str, Vec<u32>)> = markers
                .iter()
                .map(|m| (m.text.as_str(), m.targets.clone()))
                .collect();
            assert_eq!(got, vec![("⁵⁻⁷", vec![5, 6, 7]), ("¹⁰,¹¹", vec![10, 11])]);
            assert_marker_offsets(&body, &markers);

            let text = "Semantic Scholar\n¹⁰,¹¹, which";
            let start = text.find('¹').expect("run");
            let end = start + "¹⁰,¹¹".len();
            assert!(closes_line_above(text, start, end));
            assert!(superscript_attached(text, start, end));
            let text = "Jane Doe\n¹, ²Department of Chemistry";
            let start = text.find('¹').expect("run");
            assert!(!closes_line_above(text, start, start + "¹".len()));
            assert!(!superscript_attached(text, start, start + "¹".len()));
            assert!(!superscript_attached(
                "¹Department of Chemistry",
                0,
                "¹".len()
            ));
            assert!(superscript_attached("x\n³Prein showed", 2, 2 + "³".len()));
        }

        /// Detached fragments are read only after a prose line and before a
        /// prose line or the page end; plain digits never count, whatever
        /// the labels (axis ticks between prose lines); a run with `0` or a
        /// blank line before it is no citation.
        #[test]
        fn detached_superscript_lines_need_prose_around_them() {
            let bare = RefIndex::build(&labelled_refs(90, bare_label), &[]);
            let bracketed = RefIndex::build(&labelled_refs(90, bracket_label), &[]);

            let text = "Some prose text is here\n3\nMore prose text is here\n⁴\nFinal prose line";
            assert_eq!(detached_in(text, text.len(), &bracketed), one("⁴", &[4]));
            assert_eq!(detached_in(text, text.len(), &bare), one("⁴", &[4]));
            let text = "Figure 2 shows the loss curves\n0\n20\n40\nEpoch count on the axis";
            assert!(detached_in(text, text.len(), &bare).is_empty());
            // Axis ticks between two prose lines are no citations.
            let text = "The loss falls over training\n5\n10\nEpoch count on the axis";
            assert!(detached_in(text, text.len(), &bare).is_empty());
            let text = "The figure caption goes here\n²\n⁴\nx";
            assert!(detached_in(text, text.len(), &bare).is_empty());
            let text = "Prose line is here now\n\n⁵\nProse again is here";
            assert!(detached_in(text, text.len(), &bare).is_empty());
            let text = "Two or more words\n5–7\n10,11";
            assert!(detached_in(text, text.len(), &bare).is_empty());
            let text = "Two or more words\n⁵⁻⁷\n¹⁰,¹¹";
            assert_eq!(
                detached_in(text, text.len(), &bare),
                vec![
                    ("⁵⁻⁷".to_string(), vec![5, 6, 7]),
                    ("¹⁰,¹¹".to_string(), vec![10, 11])
                ]
            );
            // A run cut off by the end of the scan window (a label column
            // above a heading) is not a citation; at the page end it is.
            let text = "Prose words are here\n⁵\nReferences";
            let cut = text.find("References").expect("heading");
            assert!(detached_in(text, cut, &bare).is_empty());
            let text = "Prose words are here\n⁵";
            assert_eq!(detached_in(text, text.len(), &bare), one("⁵", &[5]));
        }

        /// arXiv:2506.23487, 2509.04183, 2603.04445, 2306.11313: entries
        /// with the same first-author surname and year are told apart by
        /// the co-author (`Xu & Li`), by `et al.` (three or more authors)
        /// or by a lone name (one author); a marker that stays ambiguous
        /// keeps every candidate.
        #[test]
        fn coauthors_narrow_same_surname_same_year_entries() {
            let refs = vec![
                entry(1, &["Xu, Y."], 2025, "Xu, Y. (2025). A thesis."),
                entry(
                    2,
                    &["Xu, Y.", "Li, Z."],
                    2025,
                    "Xu, Y. and Li, Z. (2025). A paper.",
                ),
                entry(
                    3,
                    &["Chen, M.", "Liu, Q."],
                    2025,
                    "Chen, M. and Liu, Q. (2025). One.",
                ),
                entry(
                    4,
                    &["Chen, Q.", "Wang, A.", "Zhou, B."],
                    2025,
                    "Chen, Q., Wang, A., and Zhou, B. (2025). Two.",
                ),
                entry(
                    5,
                    &["Wu, A.", "Silwal, S."],
                    2025,
                    "Wu, A. and Silwal, S. (2025). Three.",
                ),
                entry(
                    6,
                    &["Wu, B.", "Kim, C."],
                    2025,
                    "Wu, B. and Kim, C. (2025). Four.",
                ),
                entry(
                    7,
                    &["Wu, C.", "Park, D."],
                    2025,
                    "Wu, C., Park, D., et al. (2025). Five.",
                ),
                entry(
                    8,
                    &["Zhu, A.", "Xie, B.", "Sun, C."],
                    2022,
                    "Zhu, A., Xie, B., and Sun, C. (2022). Six.",
                ),
                entry(
                    9,
                    &["Zhu, D.", "Ma, E.", "Qi, F."],
                    2022,
                    "Zhu, D., Ma, E., and Qi, F. (2022). Seven.",
                ),
            ];
            let index = RefIndex::build(&refs, &[]);
            assert_eq!(
                found_in("Xu & Li (2025)", &index),
                one("Xu & Li (2025)", &[2])
            );
            assert_eq!(found_in("Xu (2025)", &index), one("Xu (2025)", &[1]));
            let text = "(Chen and Liu, 2025)";
            assert_eq!(found_in(text, &index), one(text, &[3]));
            let text = "(Chen et al., 2025)";
            assert_eq!(found_in(text, &index), one(text, &[4]));
            let text = "(Wu & Silwal, 2025)";
            assert_eq!(found_in(text, &index), one(text, &[5]));
            // `et al.` in the printed list counts as three or more authors.
            let text = "(Wu et al., 2025)";
            assert_eq!(found_in(text, &index), one(text, &[7]));
            // Two authors, but no second author named `Kaur`: the two-author
            // entries stay.
            let text = "(Wu and Kaur, 2025)";
            assert_eq!(found_in(text, &index), one(text, &[5, 6]));
            // Still ambiguous: both three-author Zhu 2022 entries.
            let text = "(Zhu et al., 2022)";
            assert_eq!(found_in(text, &index), one(text, &[8, 9]));
        }

        /// arXiv:2505.22973, 2509.04183, 2602.02748: `X Y et al.` authors
        /// key on the surname (not `al`), `Suhas BN` is also keyed on `BN`,
        /// and a narrative `van Bevern et al.` / `(2017)` split by a page
        /// break is read across it although an unclosed `(see` precedes it.
        #[test]
        fn author_keys_for_et_al_caps_surnames_and_page_breaks() {
            assert_eq!(author_surname("Lawrence Cayton et al."), "cayton");
            assert_eq!(author_surname("Smith, J. et al."), "smith");
            assert_eq!(
                author_full_key("Koichi Miyasawa et al.").as_deref(),
                Some("koichi miyasawa")
            );
            assert_eq!(trailing_caps_key("Suhas BN").as_deref(), Some("bn"));
            assert_eq!(trailing_caps_key("Rubin DB").as_deref(), Some("db"));
            assert_eq!(trailing_caps_key("Smith, AB"), None);
            assert_eq!(trailing_caps_key("Nomic"), None);

            let refs = vec![
                entry(
                    1,
                    &["Lawrence Cayton et al."],
                    2005,
                    "Lawrence Cayton et al. Algorithms for manifold learning. Tech. Rep, 2005.",
                ),
                entry(
                    2,
                    &["Koichi Miyasawa et al."],
                    1961,
                    "Koichi Miyasawa et al. An empirical bayes estimator. Bull. Inst., 1961.",
                ),
                entry(
                    3,
                    &["Suhas BN", "Andrew M. Sherrill", "Rosa I. Arriaga"],
                    2025,
                    "Suhas BN, Andrew M. Sherrill, and Rosa I. Arriaga. 2025. Thousand voices.",
                ),
                entry(
                    4,
                    &[],
                    2017,
                    "van Bevern, R., Niedermeier, R., & Suchý, O. (2017). A parameterized view.",
                ),
            ];
            let index = RefIndex::build(&refs, &[]);
            let text = "(Cayton et al.,\n2005)";
            assert_eq!(found_in(text, &index), one(text, &[1]));
            let text = "(BN et al., 2025)";
            assert_eq!(found_in(text, &index), one(text, &[3]));
            assert_eq!(
                found_in(
                    "the posterior mean x0|t\nMiyasawa et al., 1961; Efron, 2011)",
                    &index
                ),
                one("Miyasawa et al., 1961", &[2])
            );

            let pages = vec![
                text_page(
                    1,
                    "attracted significant\ninterest from the scheduling community (see, e.g., \
                     van Bevern et al.",
                ),
                text_page(2, "(2017), and others). The central question"),
            ];
            let markers = find_citation_markers(&pages, &refs);
            let got: Vec<(u32, u32, &str, Vec<u32>)> = markers
                .iter()
                .map(|m| (m.page, m.offset, m.text.as_str(), m.targets.clone()))
                .collect();
            let offset = pages[0].text.find("van").expect("name");
            assert_eq!(
                got,
                vec![(
                    1,
                    u32::try_from(offset).expect("offset"),
                    "van Bevern et al.\n(2017)",
                    vec![4]
                )]
            );
        }

        /// arXiv:2604.03540 (`[6]–[10], [16]–[19]`), 2108.04588 (German and
        /// French locators) and 2305.13843 (`Multi-ACG[103]`, `TCM[183]`):
        /// a second IEEE range after a comma, locator notes in other
        /// languages and caps names glued to their bracket group; `NP[3]`
        /// and `x[2]` stay symbols.
        #[test]
        fn ieee_second_range_foreign_locators_and_glued_caps() {
            let refs = labelled_refs(200, bracket_label);
            let page = text_page(
                1,
                "Prior work [6]–[10], [16]–[19] and [3–5, 7–9]; see [18, Kapitel VIII, § 6, II] \
                 and [31, Théorème 1] or [2, Sec. 4], Multi-ACG[103], TCM[183], NP[3] and x[2].",
            );
            let markers = find_citation_markers(std::slice::from_ref(&page), &refs);
            let got: Vec<(&str, Vec<u32>)> = markers
                .iter()
                .map(|m| (m.text.as_str(), m.targets.clone()))
                .collect();
            assert_eq!(
                got,
                vec![
                    ("[6]–[10], [16]–[19]", vec![6, 7, 8, 9, 10, 16, 17, 18, 19]),
                    ("[3–5, 7–9]", vec![3, 4, 5, 7, 8, 9]),
                    ("[18, Kapitel VIII, § 6, II]", vec![18]),
                    ("[31, Théorème 1]", vec![31]),
                    ("[2, Sec. 4]", vec![2]),
                    ("[103]", vec![103]),
                    ("[183]", vec![183]),
                ]
            );
            assert_marker_offsets(&page, &markers);

            assert!(!glued_to_word("Multi-ACG[103]", 9, 14));
            assert!(!glued_to_word("GRU-MTL[1]", 7, 10));
            assert!(!glued_to_word("TCM[183]", 3, 8));
            assert!(glued_to_word("NP[3]", 2, 5));
            assert!(glued_to_word("FPT[1] ", 3, 6));
            assert!(glued_to_word("x[20]", 1, 5));
        }

        /// arXiv:2503.00030, 2505.22973 (`(Abe et al., 2023; 2024)`), 2601.12491
        /// (`Hanu and Unitary team, 2020`): a bare year after `;` inherits
        /// the author of the clause before it, and a co-author may be a
        /// particle name or a group.
        #[test]
        fn bare_year_clauses_and_lowercase_coauthors() {
            let refs = vec![
                entry(
                    1,
                    &["Abe, K.", "Ito, M.", "Sato, T."],
                    2023,
                    "Abe, K., Ito, M., and Sato, T. (2023). One.",
                ),
                entry(
                    2,
                    &["Abe, K.", "Ito, M.", "Sato, T."],
                    2024,
                    "Abe, K., Ito, M., and Sato, T. (2024). Two.",
                ),
                entry(3, &["Zhou, L."], 2022, "Zhou, L. (2022). Three."),
                entry(
                    4,
                    &["Kaur, P.", "Singh, R.", "Das, A."],
                    2022,
                    "Kaur, P., Singh, R., and Das, A. (2022). Four.",
                ),
                entry(
                    5,
                    &["Kaur, P.", "Singh, R.", "Das, A."],
                    2023,
                    "Kaur, P., Singh, R., and Das, A. (2023). Five.",
                ),
                entry(
                    6,
                    &["Xu, Y.", "Van Roy, B."],
                    2020,
                    "Xu, Y. and Van Roy, B. (2020). Six.",
                ),
                entry(
                    7,
                    &["Hanu, L.", "Unitary team"],
                    2020,
                    "Hanu, L. and Unitary team (2020). Detoxify.",
                ),
                entry(8, &["Smith, J."], 2020, "Smith, J. (2020). Eight."),
                entry(9, &["Smith, J."], 2021, "Smith, J. (2021). Nine."),
            ];
            let index = RefIndex::build(&refs, &[]);
            let text = "(Abe et al., 2023; 2024)";
            assert_eq!(found_in(text, &index), one(text, &[1, 2]));
            let text = "(Zhou, 2022; Kaur et al., 2022; 2023)";
            assert_eq!(found_in(text, &index), one(text, &[3, 4, 5]));
            let text = "(Smith 2020; 2021)";
            assert_eq!(found_in(text, &index), one(text, &[8, 9]));
            let text = "(Xu and van Roy 2020)";
            assert_eq!(found_in(text, &index), one(text, &[6]));
            let text = "Xu and van Roy (2020)";
            assert_eq!(found_in(text, &index), one(text, &[6]));
            let text = "(Hanu and Unitary team, 2020)";
            assert_eq!(found_in(text, &index), one(text, &[7]));
        }
    }

    mod loop9_title_tests {
        use super::parsed;

        /// IEEE author lists with abbreviated particles (`A. v. Niekerk`,
        /// `O. v. d. Heide`, `E. d. Weerdt`) run to the quoted title
        /// (arxiv 2608.28714).
        #[test]
        fn ieee_particles_stay_in_the_author_list() {
            let entry = parsed(
                "[44] S. Schauman, A. v. Niekerk, O. Norbeck, H. Rydén, E. Avventi, and S. \
                 Skare, “An exploration of motion-sampling interactions in 3D MRI for \
                 neuroimaging,” Magnetic resonance in medicine, 2026, pMID: 41204061.",
                Some("[44]"),
            );
            assert_eq!(
                entry.authors,
                vec![
                    "S. Schauman",
                    "A. v. Niekerk",
                    "O. Norbeck",
                    "H. Rydén",
                    "E. Avventi",
                    "S. Skare"
                ]
            );
            assert_eq!(
                entry.title.as_deref(),
                Some("An exploration of motion-sampling interactions in 3D MRI for neuroimaging")
            );
            assert_eq!(
                entry.venue.as_deref(),
                Some("Magnetic resonance in medicine")
            );
            assert_eq!(entry.year, Some(2026));

            let entry = parsed(
                "[70] J. E. Vranic, N. M. Cross, Y. Wang, D. S. Hippe, E. d. Weerdt, and M. \
                 Mossa-Basha, “Compressed sensing-sensitivity encoding (cs-SENSE) accelerated \
                 brain imaging: Reduced scan time without reduced image quality,” AJNR. \
                 American journal of neuroradiology, 2019, pMID: 30523142.",
                Some("[70]"),
            );
            assert_eq!(entry.authors.len(), 6);
            assert_eq!(entry.authors[4], "E. d. Weerdt");
            assert_eq!(
                entry.title.as_deref(),
                Some(
                    "Compressed sensing-sensitivity encoding (cs-SENSE) accelerated brain \
                     imaging: Reduced scan time without reduced image quality"
                )
            );

            let entry = parsed(
                "[192] H. Liu, E. Versteeg, M. Fuderer, O. v. d. Heide, M. B. Schilder, C. A. \
                 T. v. d. Berg, and A. Sbrizzi, “Time-efficient, high-resolution 3t \
                 whole-brain relaxometry using cartesian 3D MR spin tomography in time-domain \
                 (MR-stat) with cerebrospinal fluid suppression,” Magnetic resonance in \
                 medicine, 2025, pMID: 39607873.",
                Some("[192]"),
            );
            assert_eq!(entry.authors.len(), 7);
            assert_eq!(entry.authors[3], "O. v. d. Heide");
            assert_eq!(entry.authors[5], "C. A. T. v. d. Berg");
            assert!(
                entry
                    .title
                    .as_deref()
                    .is_some_and(|t| t.starts_with("Time-efficient, high-resolution 3t")),
                "{:?}",
                entry.title
            );
        }

        /// A lowercase initial whose accented capital was lost (`c. Öztürk`)
        /// and accents extracted as combining marks (`Martı́`, `Ramı́rez`) do
        /// not end the author list (arxiv 2608.28714).
        #[test]
        fn ieee_accented_names_stay_in_the_author_list() {
            let entry = parsed(
                "[13] A. Güngör, S. U. Dar, c. Öztürk, Y. Korkmaz, H. A. Bedel, G. Elmas, M. \
                 Ozbey, and T. Çukur, “Adaptive diffusion priors for accelerated MRI \
                 reconstruction,” Medical image analysis,",
                Some("[13]"),
            );
            assert_eq!(entry.authors.len(), 8);
            assert_eq!(entry.authors[2], "c. Öztürk");
            assert_eq!(entry.authors[7], "T. Çukur");
            assert_eq!(
                entry.title.as_deref(),
                Some("Adaptive diffusion priors for accelerated MRI reconstruction")
            );
            assert_eq!(entry.venue.as_deref(), Some("Medical image analysis"));

            let entry = parsed(
                "[199] T. Sanchez, V. Zalevskyi, A. Mihailov, G. Mart\u{131}\u{301} Juan, E. \
                 Eixarch, A. Jakab, V. Dunet, M. Koob, G. Auzias, and M. Bach Cuadra, \
                 “Automatic quality control in multi-centric fetal brain MRI super-resolution \
                 reconstruction,” Perinatal, Preterm and Paediatric Image Analysis, 2026.",
                Some("[199]"),
            );
            assert_eq!(entry.authors.len(), 10);
            assert_eq!(entry.authors[3], "G. Mart\u{131}\u{301} Juan");
            assert_eq!(
                entry.title.as_deref(),
                Some(
                    "Automatic quality control in multi-centric fetal brain MRI \
                     super-resolution reconstruction"
                )
            );

            let entry = parsed(
                "[296] S. Verclytte, G. Beaugrard, D. Nickel, V. Muñoz-Ram\u{131}\u{301}rez, L. \
                 Norberciak, M. Boissel, V. Chaton, and A. Kwiatkowski, “Deep \
                 learning–accelerated 3d FLAIR enables reliable MS lesion detection,” \
                 American Journal of Neuroradiology, vol. 47, pp. 1560–1568, 2026.",
                Some("[296]"),
            );
            assert_eq!(entry.authors.len(), 8);
            assert_eq!(
                entry.title.as_deref(),
                Some("Deep learning–accelerated 3d FLAIR enables reliable MS lesion detection")
            );
            assert_eq!(
                entry.venue.as_deref(),
                Some("American Journal of Neuroradiology")
            );
            assert_eq!(entry.volume.as_deref(), Some("47"));
            assert_eq!(entry.pages.as_deref(), Some("1560–1568"));
        }

        /// A name the strict name test does not know (`M. d'Aspremont`)
        /// still belongs to the authors when a quoted title follows.
        #[test]
        fn quoted_title_after_loose_names_starts_at_the_quote() {
            let entry = parsed(
                "[5] A. Smith, M. d'Aspremont, and B. Jones, “Sparse principal component \
                 analysis,” SIAM Review, vol. 49, no. 3, pp. 434–448, 2007.",
                Some("[5]"),
            );
            assert_eq!(
                entry.authors,
                vec!["A. Smith", "M. d'Aspremont", "B. Jones"]
            );
            assert_eq!(
                entry.title.as_deref(),
                Some("Sparse principal component analysis")
            );
            assert_eq!(entry.venue.as_deref(), Some("SIAM Review"));
        }

        /// `unsrt`-like `Authors, Title (year).` and a title followed by
        /// `arXiv preprint arXiv:...` (arxiv 2603.04447, 2602.16061,
        /// 2410.19245).
        #[test]
        fn trailing_year_and_arxiv_preprint_leave_the_title() {
            let entry = parsed(
                "[22] S. Qin, K. Xu, S. Liao, A paradox concerning the numerical simulation of \
                 Navier-Stokes turbulence (2026). arXiv:2510.11220. URL \
                 https://arxiv.org/abs/2510.11220",
                Some("[22]"),
            );
            assert_eq!(entry.authors, vec!["S. Qin", "K. Xu", "S. Liao"]);
            assert_eq!(
                entry.title.as_deref(),
                Some("A paradox concerning the numerical simulation of Navier-Stokes turbulence")
            );
            assert_eq!(entry.year, Some(2026));
            assert_eq!(entry.arxiv_id.as_deref(), Some("2510.11220"));

            let entry = parsed(
                "Li S, Wang C, Wang J (2025) Choosing the better bandit algorithm under data \
                 sharing: When do a/b experiments work? arXiv preprint arXiv:2507.11891 .",
                None,
            );
            assert_eq!(entry.authors, vec!["Li S", "Wang C", "Wang J"]);
            assert_eq!(
                entry.title.as_deref(),
                Some(
                    "Choosing the better bandit algorithm under data sharing: When do a/b \
                     experiments work?"
                )
            );
            assert_eq!(entry.arxiv_id.as_deref(), Some("2507.11891"));
            assert_eq!(entry.year, Some(2025));

            let entry = parsed(
                "[24] Raymond Li, Loubna Ben Allal, Yangtian Zi, Niklas Muennighoff, Denis \
                 Kocetkov, Chenghao Mou, Marc Marone, Christopher Akiki, Jia Li, Jenny Chim, et \
                 al. 2023. Starcoder: may the source be with you! arXiv preprint \
                 arXiv:2305.06161 (2023).",
                Some("[24]"),
            );
            assert_eq!(
                entry.title.as_deref(),
                Some("Starcoder: may the source be with you!")
            );
            assert_eq!(entry.arxiv_id.as_deref(), Some("2305.06161"));
        }

        /// A book title keeps its own words; `volume N of Series` names the
        /// series as the venue (arxiv 2603.05575).
        #[test]
        fn book_series_goes_to_the_venue() {
            let entry = parsed(
                "Engl, H. W., Hanke, M., and Neubauer, A. (1996). Regularization of Inverse \
                 Problems, volume 375 of Mathematics and Its Applications. Kluwer Academic \
                 Publishers, Dordrecht.",
                None,
            );
            assert_eq!(
                entry.title.as_deref(),
                Some("Regularization of Inverse Problems")
            );
            assert_eq!(
                entry.venue.as_deref(),
                Some("Mathematics and Its Applications")
            );
            assert_eq!(entry.volume.as_deref(), Some("375"));
            assert_eq!(entry.year, Some(1996));

            let entry = parsed(
                "Wainwright, M. J. (2019). High-Dimensional Statistics: A Non-Asymptotic \
                 Viewpoint, volume 48. Cambridge University Press.",
                None,
            );
            assert_eq!(
                entry.title.as_deref(),
                Some("High-Dimensional Statistics: A Non-Asymptotic Viewpoint")
            );
            assert_eq!(entry.venue.as_deref(), Some("Cambridge University Press"));
            assert_eq!(entry.volume.as_deref(), Some("48"));
        }

        /// A ditto dash stands for the previous authors; the quoted title
        /// follows it (arxiv 2608.28714, 2512.10223).
        #[test]
        fn ditto_dash_entry_keeps_its_quoted_title() {
            let entry = parsed(
                "[52] ——, “Regulation (EU) 2017/745 on medical devices (Medical Device \
                 Regulation),” Official Journal of the European Union, L 117, 5 May 2017, 2017.",
                Some("[52]"),
            );
            assert!(entry.authors.is_empty());
            assert_eq!(
                entry.title.as_deref(),
                Some("Regulation (EU) 2017/745 on medical devices (Medical Device Regulation)")
            );
            assert_eq!(
                entry.venue.as_deref(),
                Some("Official Journal of the European Union")
            );
            assert_eq!(entry.year, Some(2017));

            let entry = parsed(
                "[18] ——, “Segmented GRAND: Complexity reduction through sub-pattern \
                 combination,” IEEE Trans. Commun., vol. 73, no. 8, pp. 5607–5620, 2025.",
                Some("[18]"),
            );
            assert!(entry.authors.is_empty());
            assert_eq!(
                entry.title.as_deref(),
                Some("Segmented GRAND: Complexity reduction through sub-pattern combination")
            );
            assert_eq!(entry.volume.as_deref(), Some("73"));
        }

        /// A regulation without authors: the leading clause is the title
        /// (the unquoted form of the 2608.28714 entry).
        #[test]
        fn regulation_without_authors_titles_the_leading_clause() {
            let entry = parsed(
                "Regulation (EU) 2017/745 on medical devices (Medical Device Regulation). \
                 Official Journal of the European Union, L 117, 5 May 2017.",
                None,
            );
            assert!(entry.authors.is_empty());
            assert_eq!(
                entry.title.as_deref(),
                Some("Regulation (EU) 2017/745 on medical devices (Medical Device Regulation)")
            );
            assert_eq!(
                entry.venue.as_deref(),
                Some("Official Journal of the European Union")
            );
        }
    }
}
