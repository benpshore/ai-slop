//! Reference and citation ground truth taken from an `arXiv` e-print's `LaTeX`
//! source: the typeset bibliography (`.bbl`, one `\bibitem` per printed entry,
//! or a `biblatex` `\entry`), the database (`.bib`) filtered to the keys the
//! paper cites, and the `\cite` commands themselves. Also a rough "detex" of
//! the body text for word-alignment diagnostics.
//!
//! Everything here is best-effort text processing of author-written sources.
//! A field stays `None` unless the source contains it; nothing is invented.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use unicode_normalization::UnicodeNormalization;

use crate::corpus::LatexFiles;

/// Deepest `\input` nesting that is still inlined.
const MAX_INPUT_DEPTH: u32 = 5;
/// Maximum number of input directives expanded for one document.
const MAX_INPUTS: usize = 10_000;
/// Maximum size of a document after input expansion.
const MAX_EXPANDED_BYTES: usize = 16 * 1024 * 1024;
/// Upper bound on zero-argument macros expanded in the body text.
const MAX_MACROS: usize = 200;
/// Maximum number of distinct documents accepted from an arXiv manifest.
const MAX_TOPLEVEL_DOCUMENTS: usize = 16;
/// Maximum source text retained for all manifest documents together.
const MAX_TOPLEVEL_SOURCE_BYTES: usize = 32 * 1024 * 1024;
/// Maximum accumulated text retained in generated ground truth.
const MAX_GROUND_TRUTH_BYTES: usize = 64 * 1024 * 1024;
/// `.bib` fields searched for an `arXiv` identifier when `eprint` is absent.
const ARXIV_FALLBACK_FIELDS: &[&str] = &["journal", "eid", "note", "url", "pages", "volume"];

/// Citation commands whose brace argument is a comma-separated key list.
const CITE_COMMANDS: &[&str] = &[
    "cite",
    "citep",
    "citet",
    "citealp",
    "citealt",
    "citeauthor",
    "citeyear",
    "citeyearpar",
    "citenum",
    "citetitle",
    "citedate",
    "citeurl",
    "fullcite",
    "footfullcite",
    "parencite",
    "textcite",
    "autocite",
    "footcite",
    "footcitetext",
    "smartcite",
    "supercite",
    "cites",
    "parencites",
    "textcites",
    "autocites",
    "footcites",
    "shortcite",
    "shortciteA",
    "citeA",
    "citeNP",
    "Cite",
    "Citep",
    "Citet",
    "Citealp",
    "Citealt",
    "Citeauthor",
    "Parencite",
    "Textcite",
    "Autocite",
    "Smartcite",
    "Parencites",
    "Textcites",
    "Autocites",
    "Smartcites",
    "Footcites",
    "smartcites",
    "supercites",
    "footcitetexts",
    "fullcites",
    "footfullcites",
];

/// Commands that start with `cite` but are not citations (styles, hooks and
/// fonts of `natbib`, `cite` and `biblatex`); never counted.
const NOT_CITE_COMMANDS: &[&str] = &[
    "citestyle",
    "citetext",
    "citeindextrue",
    "citeindexfalse",
    "citeindextype",
    "citeform",
    "citeleft",
    "citeright",
    "citemid",
    "citepunct",
    "citedash",
    "citenamefont",
    "citenumfont",
    "citesetup",
    "citereset",
    "citeresetfalse",
    "citeresettrue",
    "citetrackerfalse",
    "citetrackertrue",
];

/// Cite commands that print only an author, year, title, date or URL, never
/// a marker that links to an entry.
const AUTHOR_YEAR_ONLY_COMMANDS: &[&str] = &[
    "citeauthor",
    "citefullauthor",
    "citeyear",
    "citeyearpar",
    "citetitle",
    "citedate",
    "citeurl",
    "citename",
];

/// Where a truth reference came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TruthSource {
    /// A typeset bibliography (`.bbl`, or `thebibliography` inline in the `.tex`).
    Bbl,
    /// A `.bib` database entry.
    Bib,
}

/// One bibliography entry as the author's source describes it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TruthReference {
    /// Citation key (`\bibitem{key}` or `@type{key,`).
    pub key: String,
    /// Printed label from `\bibitem[label]`, detexed; `None` for `.bib` entries.
    pub label: Option<String>,
    /// Detexed entry text (`.bbl`) or `authors. title. venue year` (`.bib`).
    pub text: String,
    /// Author names in `First Last` form, best effort.
    pub authors: Vec<String>,
    pub title: Option<String>,
    pub year: Option<u16>,
    pub doi: Option<String>,
    pub arxiv_id: Option<String>,
    pub source: TruthSource,
}

/// What the `.tex` cites.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TruthCitations {
    /// Number of `\cite`-family commands (not counting `\nocite`).
    pub cite_commands: u32,
    /// Keys in document order, duplicates kept.
    pub cited_keys: Vec<String>,
    /// Keys from `\nocite{...}` and `multibib`'s `\nocite<name>{...}`
    /// (excluding `*`).
    pub nocite_keys: Vec<String>,
    /// `\nocite{*}` (or a `multibib` `\nocite<name>{*}`) was present.
    pub nocite_all: bool,
    /// Commands among `cite_commands` that print only an author, year,
    /// title, date or URL (`\citeauthor`, `\citeyear`, ...); their keys are
    /// still in `cited_keys`.
    #[serde(default)]
    pub cite_only_author_year: u32,
}

/// Paper-level metadata as the author's source states it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TruthPaper {
    /// Detexed `\title{...}` (or `\icmltitle{...}`) without footnotes.
    pub title: Option<String>,
    /// Person names from the author commands, in source order, deduplicated.
    pub authors: Vec<String>,
    /// DOI from `\doi{..}` / `\acmDOI{..}` or a `doi:` / `doi.org/` on the title page.
    pub doi: Option<String>,
    /// `arXiv` identifier from an explicit `\arxiv{..}` / `\arxivid{..}` command.
    pub arxiv_id: Option<String>,
}

/// Ground truth for one paper.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroundTruth {
    pub references: Vec<TruthReference>,
    pub citations: TruthCitations,
    /// `bbl`, `bbl+bib`, `bib-cited` or `bib-all`; with several toplevel
    /// documents, their distinct methods joined by `,`.
    pub method: String,
    /// Detexed body for alignment diagnostics; may be empty.
    pub body_text: String,
    /// Title, authors and identifiers from the title page commands.
    #[serde(default)]
    pub paper: TruthPaper,
}

/// Why no ground truth could be built.
#[derive(Debug, Error)]
pub enum TruthError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("no .bbl, inline thebibliography or .bib bibliography found")]
    NoBibliography,
    #[error("no .tex file contains \\begin{{document}}")]
    NoMainTex,
    #[error("cyclic LaTeX input: {0}")]
    CyclicInput(PathBuf),
    #[error("LaTeX input expansion exceeded its resource limit")]
    InputLimit,
    #[error("LaTeX source exceeds ground-truth resource limits")]
    ResourceLimit,
}

// ---------------------------------------------------------------------------
// Regexes
// ---------------------------------------------------------------------------

fn command_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\\([A-Za-z]+)\*?").expect("valid regex"))
}

fn bibitem_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\\bibitem\b").expect("valid regex"))
}

fn newblock_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\\newblock\b").expect("valid regex"))
}

fn acm_title_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\\(?:showarticletitle|bibinfo\{title\})\s*\{").expect("valid regex")
    })
}

/// A `19xx`/`20xx` digit run; digit boundaries are checked by the caller.
fn year_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?:19|20)[0-9]{2}").expect("valid regex"))
}

/// An ISO-style date (`2020-05-01`, `2020-05`, `2021/03/15`, `2021/03`);
/// group 1 is the year.
fn iso_date_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?:^|[^0-9])((?:19|20)[0-9]{2})(?:-(?:0[1-9]|1[0-2])(?:-[0-3][0-9])?|/(?:0[1-9]|1[0-2])(?:/[0-3][0-9])?)(?:[^0-9]|$)",
        )
        .expect("valid regex")
    })
}

/// Opening of an italic group: `{\em`, `{\it`, `{\itshape`, `\emph{`, `\textit{`.
fn italic_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\{\\(?:em|it|itshape)\b|\\(?:emph|textit)\s*\{").expect("valid regex")
    })
}

/// URLs, DOIs and `arXiv` identifiers, whose digits must not be read as years.
fn year_mask_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)(?:https?://|doi:|doi\.org/)\S+|10\.\d{4,9}/\S+|\d{4}\.\d{4,5}(?:v\d+)?")
            .expect("valid regex")
    })
}

fn doi_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"10\.\d{4,9}/[^\s"<>{}]+"#).expect("valid regex"))
}

fn doi_url_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(?i)doi\.org/(10\.\d{4,9}/[^\s"<>{}]+)"#).expect("valid regex"))
}

fn arxiv_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)arxiv(?:\.org/abs/|\.org/pdf/|\s*:\s*|\.)(\d{4}\.\d{4,5}(?:v\d+)?|[a-z\-]+(?:\.[a-z]{2})?/\d{7})",
        )
        .expect("valid regex")
    })
}

fn eprint_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\\eprint\s*\{\s*(?:arxiv:)?(\d{4}\.\d{4,5}(?:v\d+)?)")
            .expect("valid regex")
    })
}

fn arxiv_id_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)^(?:arxiv:)?(\d{4}\.\d{4,5}(?:v\d+)?|[a-z\-]+(?:\.[a-z]{2})?/\d{7})$")
            .expect("valid regex")
    })
}

/// `and` between two author names at brace depth zero (anchored at the slice start).
fn and_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)^\s+and\s+").expect("valid regex"))
}

fn and_sep_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i),?\s+and\s+").expect("valid regex"))
}

fn initials_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\p{Lu}\.?(?:[\s\-]*\p{Lu}\.?){0,3}$").expect("valid regex"))
}

fn trailing_year_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\s*\((?:19|20)\d{2}[a-z]?\)\.?\s*$").expect("valid regex"))
}

/// `Authors (2023b) Title.` as the first `\newblock` segment (INFORMS,
/// `spbasic`): group 1 is the author list, group 2 the title.
fn author_year_title_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^(.+?)\s*\((?:19|20)\d{2}[a-z]?\)\s*[.,:]?\s+(\S.*)$").expect("valid regex")
    })
}

/// `, volume 375 of Mathematics and Its Applications` (or a bare `, volume
/// 48`) closing a book title: the series belongs to the entry text, not the
/// title.
fn series_suffix_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i),\s+(?:volume|vol\.)\s+\d+(?:\s+of\s+.+)?$").expect("valid regex")
    })
}

/// A year that `natbib` prints at the end of a title block (`…
/// reinforcement learning, 2025.`, `…, 2024{\natexlab{a}}`): the date of
/// the entry, not part of its title.
fn title_year_suffix_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r",\s*(?:19|20)\d{2}[a-z]?$").expect("valid regex"))
}

/// A bracketed descriptor closing a title (`… collaboration [analysis
/// code]`): it names the medium, it is not part of the title.
fn descriptor_suffix_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\s+\[[\p{L} ]+\]$").expect("valid regex"))
}

/// A title block that is only a URL (`\newblock \URLprefix \url{https://…}`
/// in `elsarticle-harv` misc entries): no title is printed.
fn url_only_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)^(?:url:?\s*)?(?:https?://|www\.)\S*$").expect("valid regex")
    })
}

/// Matches `\input{f}`, `\include{f}` and `\subfile{f}` (group 1), plus the
/// brace-free plain-`TeX` form `\input f` that only `\input` accepts, where
/// `f` runs until whitespace, `{`, `}` or `\` (group 2).
fn input_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\\(?:input|include|subfile)\s*\{([^}]*)\}|\\input\s+([^\s{}\\]+)")
            .expect("valid regex")
    })
}

fn begin_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\\begin\s*\{([A-Za-z*]+)\}").expect("valid regex"))
}

fn heading_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\\(?:(?:sub){0,2}section|chapter|paragraph|subparagraph)\*?")
            .expect("valid regex")
    })
}

fn par_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\\par\b").expect("valid regex"))
}

fn blank_line_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\n[ \t\r]*\n\s*").expect("valid regex"))
}

/// Zero-argument macro definitions: `\newcommand{\name}{body}`, `\def\name{body}`.
fn newcommand_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"\\(?:newcommand|renewcommand|providecommand|DeclareRobustCommand|def)\*?\s*\{?\s*\\([A-Za-z]+)\s*\}?\s*(?:\[0\])?\s*\{",
        )
        .expect("valid regex")
    })
}

fn entry_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\\entry\{").expect("valid regex"))
}

fn field_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\\field\{([A-Za-z]+)\}\{").expect("valid regex"))
}

fn verb_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?s)\\verb\{([A-Za-z]+)\}\s*\\verb (.*?)\s*\\endverb").expect("valid regex")
    })
}

fn name_part_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(family|given)=\{").expect("valid regex"))
}

// ---------------------------------------------------------------------------
// Low-level scanning helpers (all indices are byte offsets)
// ---------------------------------------------------------------------------

/// Byte index of the delimiter closing the group opened at `open` (`{`, `[` or
/// `(`), honouring backslash escapes and treating nested `{...}` groups as
/// opaque. `None` when the group is unbalanced or `open` is not a delimiter.
fn matching_close(s: &str, open: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    let open_byte = *bytes.get(open)?;
    let close_byte = match open_byte {
        b'{' => b'}',
        b'[' => b']',
        b'(' => b')',
        _ => return None,
    };
    let mut depth = 0usize;
    let mut i = open;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\\' {
            i += 2;
            continue;
        }
        if b == open_byte {
            depth += 1;
        } else if b == close_byte {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        } else if b == b'{' {
            i = matching_close(s, i)?;
        }
        i += 1;
    }
    None
}

/// The char starting at byte `i`, or `None` at the end or off a boundary.
fn char_at(s: &str, i: usize) -> Option<char> {
    s.get(i..)?.chars().next()
}

/// Index of the first non-whitespace char at or after `i`.
fn skip_ws(s: &str, mut i: usize) -> usize {
    while let Some(c) = char_at(s, i) {
        if !c.is_whitespace() {
            break;
        }
        i += c.len_utf8();
    }
    i
}

/// Whitespace after a control word, as `TeX` discards it: blanks and at most
/// one line end, but never a blank line (that is a paragraph break).
fn skip_control_space(s: &str, mut i: usize) -> usize {
    while matches!(char_at(s, i), Some(' ' | '\t' | '\r')) {
        i += 1;
    }
    if char_at(s, i) == Some('\n') {
        let mut after = i + 1;
        while matches!(char_at(s, after), Some(' ' | '\t' | '\r')) {
            after += 1;
        }
        if char_at(s, after) != Some('\n') {
            return after;
        }
    }
    i
}

/// If a brace group starts at byte `i`: its inner range and the index after `}`.
fn brace_group(s: &str, i: usize) -> Option<(Range<usize>, usize)> {
    if s.as_bytes().get(i) != Some(&b'{') {
        return None;
    }
    let close = matching_close(s, i)?;
    Some((i + 1..close, close + 1))
}

/// Remove `%` comments (not `\%`) through the end of the line and the next
/// line's indentation, keeping a following blank line as a paragraph break.
fn strip_comments(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while let Some(c) = char_at(s, i) {
        if c == '\\' {
            out.push(c);
            i += 1;
            if let Some(escaped) = char_at(s, i) {
                out.push(escaped);
                i += escaped.len_utf8();
            }
        } else if c == '%' {
            i = s[i..].find('\n').map_or(s.len(), |off| i + off + 1);
            while matches!(char_at(s, i), Some(' ' | '\t' | '\r')) {
                i += 1;
            }
            if char_at(s, i) == Some('\n') {
                out.push('\n');
            }
        } else {
            out.push(c);
            i += c.len_utf8();
        }
    }
    out
}

fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<&str>>().join(" ")
}

// ---------------------------------------------------------------------------
// Detex
// ---------------------------------------------------------------------------

/// Plain text for a `LaTeX` fragment.
///
/// Comments are removed; `\newblock`, `~`, `\,` and `\ ` become spaces;
/// formatting commands (`\emph`, `\textit`, `\textbf`, `\textsc`, `\texttt`,
/// `\mbox`, `\text`, `\url`, `\natexlab`, `{\em x}`) yield their argument;
/// `\href{url}{text}` yields `text`; quotes and dashes become their Unicode
/// forms; escaped symbols (`\&`, `\%`, `\_`, `\$`, `\{`, `\}`) become the
/// symbol; accents (`\'e`, `\c{c}`, `\v{s}`, ...) and special letters (`\ss`,
/// `\o`, `\ae`, `\l`, `\i`, ...) become the composed character; `$...$` keeps
/// its contents, with Greek letters and common symbols (`\tau`, `\pm`) as
/// Unicode and operator names (`\log`) as their name; labels, references,
/// spacing and citation commands are dropped with their arguments; any other
/// `\command` disappears and its brace arguments are kept as text.
/// Whitespace is collapsed and trimmed.
pub fn latex_to_text(s: &str) -> String {
    let clean = strip_comments(s);
    let mut out = String::with_capacity(clean.len());
    detex(&clean, &mut out);
    collapse_whitespace(&out)
}

fn detex(s: &str, out: &mut String) {
    let mut i = 0;
    while let Some(c) = char_at(s, i) {
        match c {
            '\\' => i = detex_command(s, i, out),
            '{' | '}' | '$' => i += 1,
            '~' => {
                out.push(' ');
                i += 1;
            }
            '`' => {
                if s[i..].starts_with("``") {
                    out.push('"');
                    i += 2;
                } else {
                    out.push('\'');
                    i += 1;
                }
            }
            '\'' => {
                if s[i..].starts_with("''") {
                    out.push('"');
                    i += 2;
                } else {
                    out.push('\'');
                    i += 1;
                }
            }
            '-' => {
                if s[i..].starts_with("---") {
                    out.push('\u{2014}');
                    i += 3;
                } else if s[i..].starts_with("--") {
                    out.push('\u{2013}');
                    i += 2;
                } else {
                    out.push('-');
                    i += 1;
                }
            }
            _ => {
                out.push(c);
                i += c.len_utf8();
            }
        }
    }
}

/// Handle the control sequence starting at byte `i` (a backslash); returns the
/// index to continue from.
fn detex_command(s: &str, i: usize, out: &mut String) -> usize {
    let Some(next) = char_at(s, i + 1) else {
        return i + 1;
    };
    if next.is_ascii_alphabetic() {
        let name_start = i + 1;
        let mut name_end = name_start;
        while char_at(s, name_end).is_some_and(|c| c.is_ascii_alphabetic()) {
            name_end += 1;
        }
        let name = &s[name_start..name_end];
        let mut rest = name_end;
        if char_at(s, rest) == Some('*') {
            rest += 1;
        }
        return letter_command(s, name, skip_control_space(s, rest), out);
    }
    let after = i + 1 + next.len_utf8();
    match next {
        '&' | '%' | '_' | '$' | '{' | '}' | '#' => {
            out.push(next);
            after
        }
        ',' | ' ' | '\n' | '\t' | '\r' | ';' => {
            out.push(' ');
            after
        }
        '\\' => {
            // Line break, optionally `\\*` or `\\[skip]`.
            out.push(' ');
            let mut k = after;
            if char_at(s, k) == Some('*') {
                k += 1;
            }
            if s.as_bytes().get(k) == Some(&b'[') {
                k = matching_close(s, k).map_or(k, |close| close + 1);
            }
            k
        }
        '\'' | '`' | '^' | '"' | '~' | '=' | '.' => accent(s, next, after, out),
        _ => after,
    }
}

/// Combining mark for an accent command (`\'`, `\c`, ...), if `mark` is one.
fn combining_mark(mark: char) -> Option<char> {
    Some(match mark {
        '\'' => '\u{301}',
        '`' => '\u{300}',
        '^' => '\u{302}',
        '"' => '\u{308}',
        '~' => '\u{303}',
        '=' => '\u{304}',
        '.' => '\u{307}',
        'c' => '\u{327}',
        'v' => '\u{30C}',
        'H' => '\u{30B}',
        'k' => '\u{328}',
        'u' => '\u{306}',
        'r' => '\u{30A}',
        'd' => '\u{323}',
        'b' => '\u{331}',
        _ => return None,
    })
}

/// Letter commands that stand for one special character.
fn special_letter(name: &str) -> Option<&'static str> {
    Some(match name {
        "ss" => "ß",
        "o" => "ø",
        "O" => "Ø",
        "ae" => "æ",
        "AE" => "Æ",
        "oe" => "œ",
        "OE" => "Œ",
        "aa" => "å",
        "AA" => "Å",
        "l" => "ł",
        "L" => "Ł",
        "i" => "ı",
        "j" => "ȷ",
        "dh" => "ð",
        "DH" => "Ð",
        "th" => "þ",
        "TH" => "Þ",
        "ldots" | "dots" | "textellipsis" => "…",
        "textendash" => "–",
        "textemdash" => "—",
        "textquotedblleft" | "textquotedblright" => "\"",
        "textquoteleft" | "textquoteright" => "'",
        "textbackslash" => "\\",
        "textasciitilde" => "~",
        "LaTeX" => "LaTeX",
        "TeX" => "TeX",
        "BibTeX" => "BibTeX",
        _ => return None,
    })
}

/// Math letters and symbols as the typeset PDF shows them (Greek letters,
/// `\pm`, `\times`, relations), so a title keeps `τ` in `$\tau$-bench`.
fn math_symbol(name: &str) -> Option<&'static str> {
    Some(match name {
        "alpha" => "α",
        "beta" => "β",
        "gamma" => "γ",
        "delta" => "δ",
        "epsilon" => "ϵ",
        "varepsilon" => "ε",
        "zeta" => "ζ",
        "eta" => "η",
        "theta" => "θ",
        "vartheta" => "ϑ",
        "iota" => "ι",
        "kappa" => "κ",
        "lambda" => "λ",
        "mu" => "μ",
        "nu" => "ν",
        "xi" => "ξ",
        "pi" => "π",
        "varpi" => "ϖ",
        "rho" => "ρ",
        "varrho" => "ϱ",
        "sigma" => "σ",
        "varsigma" => "ς",
        "tau" => "τ",
        "upsilon" => "υ",
        "phi" => "ϕ",
        "varphi" => "φ",
        "chi" => "χ",
        "psi" => "ψ",
        "omega" => "ω",
        "Gamma" => "Γ",
        "Delta" => "Δ",
        "Theta" => "Θ",
        "Lambda" => "Λ",
        "Xi" => "Ξ",
        "Pi" => "Π",
        "Sigma" => "Σ",
        "Upsilon" => "Υ",
        "Phi" => "Φ",
        "Psi" => "Ψ",
        "Omega" => "Ω",
        "pm" => "±",
        "mp" => "∓",
        "times" => "×",
        "cdot" => "·",
        "infty" => "∞",
        "le" | "leq" => "≤",
        "ge" | "geq" => "≥",
        "ne" | "neq" => "≠",
        "approx" => "≈",
        "sim" => "∼",
        "to" | "rightarrow" => "→",
        "ell" => "ℓ",
        "partial" => "∂",
        "nabla" => "∇",
        _ => return None,
    })
}

/// Math operator names that print as their own name (`\log n` → `log n`).
fn is_operator_name(name: &str) -> bool {
    matches!(
        name,
        "log"
            | "ln"
            | "exp"
            | "sin"
            | "cos"
            | "tan"
            | "max"
            | "min"
            | "sup"
            | "inf"
            | "lim"
            | "det"
            | "arg"
            | "deg"
            | "dim"
            | "ker"
            | "Pr"
    )
}

/// The mark of a one-letter accent command (`\c`, `\v`, `\H`, ...).
fn accent_letter(name: &str) -> Option<char> {
    let mut chars = name.chars();
    let mark = chars.next()?;
    if chars.next().is_some() || !mark.is_ascii_alphabetic() {
        return None;
    }
    combining_mark(mark).is_some().then_some(mark)
}

/// Commands dropped together with one brace argument (and optional `[...]`).
fn drops_one_argument(name: &str) -> bool {
    matches!(
        name,
        "label"
            | "ref"
            | "eqref"
            | "pageref"
            | "autoref"
            | "cref"
            | "Cref"
            | "nameref"
            | "vspace"
            | "hspace"
            | "vskip"
            | "hskip"
            | "includegraphics"
            | "begin"
            | "end"
            | "bibliographystyle"
            | "bibliography"
            | "addbibresource"
            | "nocite"
            | "index"
            | "glossary"
            | "documentclass"
            | "usepackage"
            | "input"
            | "include"
            | "pagestyle"
            | "thispagestyle"
            | "color"
            | "textcolor"
            | "colorbox"
            | "bibitem"
            | "bibfield"
            | "bibinfo"
    ) || CITE_COMMANDS.contains(&name)
}

/// Commands dropped together with two brace arguments (definitions).
fn drops_two_arguments(name: &str) -> bool {
    matches!(
        name,
        "newcommand"
            | "renewcommand"
            | "providecommand"
            | "DeclareRobustCommand"
            | "setlength"
            | "addtolength"
            | "setcounter"
            | "addtocounter"
            | "newenvironment"
            | "renewenvironment"
            | "DeclareMathOperator"
    )
}

/// A letter control word `name`; `after` is the index past the name (and the
/// whitespace `TeX` would swallow).
fn letter_command(s: &str, name: &str, after: usize, out: &mut String) -> usize {
    if let Some(text) = special_letter(name).or_else(|| math_symbol(name)) {
        out.push_str(text);
        return after;
    }
    if is_operator_name(name) {
        // `$O(n\log n)$` prints `O(n log n)`.
        if out.chars().next_back().is_some_and(char::is_alphanumeric) {
            out.push(' ');
        }
        out.push_str(name);
        if char_at(s, after).is_some_and(|c| c.is_alphanumeric() || c == '\\') {
            out.push(' ');
        }
        return after;
    }
    if let Some(mark) = accent_letter(name) {
        return accent(s, mark, after, out);
    }
    if drops_one_argument(name) {
        return drop_args(s, after, 1);
    }
    if drops_two_arguments(name) {
        return drop_args(s, after, 2);
    }
    match name {
        "newblock" | "newline" | "linebreak" | "par" | "quad" | "qquad" | "hfill" | "hfil"
        | "noindent" | "indent" | "centering" | "smallskip" | "medskip" | "bigskip" | "vfill"
        | "break" | "and" => {
            out.push(' ');
            after
        }
        // `\href{url}{text}`: drop the URL, the text group is stripped of braces below.
        "href" => {
            let Some((_, past_url)) = brace_group(s, after) else {
                return after;
            };
            skip_ws(s, past_url)
        }
        // `\penalty0`, `\penalty-100`.
        "penalty" => {
            let mut k = after;
            if char_at(s, k) == Some('-') {
                k += 1;
            }
            while char_at(s, k).is_some_and(|c| c.is_ascii_digit()) {
                k += 1;
            }
            k
        }
        // Anything else: the command disappears and a directly following
        // optional argument with it; brace arguments are kept as text.
        _ => {
            if s.as_bytes().get(after) == Some(&b'[') {
                return matching_close(s, after).map_or(after, |close| close + 1);
            }
            after
        }
    }
}

/// Skip `groups` brace arguments from `from`, each possibly preceded by
/// `[...]` optional arguments or `*` and followed by one `[...]`.
fn drop_args(s: &str, from: usize, groups: usize) -> usize {
    let mut i = from;
    for _ in 0..groups {
        i = skip_ws(s, i);
        while s.as_bytes().get(i) == Some(&b'[') {
            let Some(close) = matching_close(s, i) else {
                return i;
            };
            i = skip_ws(s, close + 1);
        }
        if char_at(s, i) == Some('*') {
            i = skip_ws(s, i + 1);
        }
        let Some((_, past_group)) = brace_group(s, i) else {
            return i;
        };
        i = past_group;
        if s.as_bytes().get(i) == Some(&b'[') {
            i = matching_close(s, i).map_or(i, |close| close + 1);
        }
    }
    i
}

/// Apply the accent `mark` to the argument starting at `from`: a brace group,
/// a control word (`\i`, `\j`) or a single character.
fn accent(s: &str, mark: char, from: usize, out: &mut String) -> usize {
    let Some(combining) = combining_mark(mark) else {
        return from;
    };
    let k = skip_ws(s, from);
    let (base, rest, end) = if let Some((inner, past_group)) = brace_group(s, k) {
        let mut inner_text = String::new();
        detex(&s[inner], &mut inner_text);
        let mut chars = inner_text.chars();
        let first = chars.next();
        (first, chars.as_str().to_owned(), past_group)
    } else if s.as_bytes().get(k) == Some(&b'\\') {
        let mut name_end = k + 1;
        while char_at(s, name_end).is_some_and(|c| c.is_ascii_alphabetic()) {
            name_end += 1;
        }
        let base = match &s[k + 1..name_end] {
            "i" => Some('i'),
            "j" => Some('j'),
            _ => return from,
        };
        (base, String::new(), name_end)
    } else {
        let base = char_at(s, k);
        (base, String::new(), k + base.map_or(0, char::len_utf8))
    };
    let Some(base) = base else { return end };
    let base = match base {
        'ı' => 'i',
        'ȷ' => 'j',
        other => other,
    };
    let composed: String = format!("{base}{combining}").chars().nfc().collect();
    out.push_str(&composed);
    out.push_str(&rest);
    end
}

// ---------------------------------------------------------------------------
// Field extraction shared by the parsers
// ---------------------------------------------------------------------------

/// Whether `text` ends with `word` and the character before it (if any) is
/// not alphanumeric.
fn ends_with_word(text: &str, word: &str) -> bool {
    text.strip_suffix(word)
        .is_some_and(|head| !head.chars().next_back().is_some_and(char::is_alphanumeric))
}

/// Whether the text before a year candidate makes it a page or volume
/// number: the end of a range (`1877–1901`), after `:` (`17:1923`), or after
/// `pp.`, `p.`, `pages`, `page`, `pg.`.
fn page_like_before(before: &str) -> bool {
    let trimmed = before.trim_end();
    if trimmed.ends_with(['-', '\u{2013}', '\u{2014}', ':']) {
        return true;
    }
    let lower = trimmed.to_lowercase();
    ["pp.", "pp", "p.", "pages", "page", "pg."]
        .iter()
        .any(|word| ends_with_word(&lower, word))
}

/// Whether the text after a year candidate makes it a page or volume number:
/// the start of a range (`1907–1921`) or a `volume:pages` pair (`1952:1–12`).
fn page_like_after(after: &str) -> bool {
    let rest = after.trim_start();
    let Some(first) = rest.chars().next() else {
        return false;
    };
    if !matches!(first, '-' | '\u{2013}' | '\u{2014}' | ':') {
        return false;
    }
    rest[first.len_utf8()..]
        .trim_start()
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_digit())
}

/// A year written as `(2020)` or `(2020a)`.
fn in_parentheses(before: &str, after: &str) -> bool {
    if !before.ends_with('(') {
        return false;
    }
    let mut chars = after.chars();
    match chars.next() {
        Some(')') => true,
        Some(c) if c.is_ascii_lowercase() => chars.next() == Some(')'),
        _ => false,
    }
}

/// A year written as a comma field that closes the entry or a clause:
/// `, 2020.`, `, 2020a,`, `, 2021` at the end.
fn after_comma_field(before: &str, after: &str) -> bool {
    if !before.trim_end().ends_with(',') {
        return false;
    }
    let rest = after
        .strip_prefix(|c: char| c.is_ascii_lowercase())
        .unwrap_or(after)
        .trim_start();
    rest.is_empty() || rest.starts_with(['.', ',', ';'])
}

/// The publication year of an entry's text, ignoring digits inside URLs,
/// DOIs and `arXiv` ids.
///
/// Candidates are standalone `19xx`/`20xx` numbers. Page and volume numbers
/// (range ends, `volume:pages`, after `pp.`/`pages`) are skipped. The first
/// parenthesised candidate wins, else the last comma field (`, 2021.`), else
/// the first remaining candidate. When every candidate looks like a page
/// number, only an ISO-style date (`2020-05-01`, `2021/03`) gives the year;
/// otherwise there is none (`pp. 1907–1921` is not a year).
fn first_year(text: &str) -> Option<u16> {
    let unescaped = unescape_underscores(text);
    let masked = year_mask_re().replace_all(&unescaped, " ");
    let masked: &str = &masked;
    let mut first_plain: Option<&str> = None;
    let mut first_paren: Option<&str> = None;
    let mut last_comma: Option<&str> = None;
    for m in year_re().find_iter(masked) {
        let before = &masked[..m.start()];
        let after = &masked[m.end()..];
        if before
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_digit())
            || after.chars().next().is_some_and(|c| c.is_ascii_digit())
        {
            continue;
        }
        let year = m.as_str();
        if page_like_before(before) || page_like_after(after) {
            continue;
        }
        if first_paren.is_none() && in_parentheses(before, after) {
            first_paren = Some(year);
        }
        if after_comma_field(before, after) {
            last_comma = Some(year);
        }
        if first_plain.is_none() {
            first_plain = Some(year);
        }
    }
    first_paren
        .or(last_comma)
        .or(first_plain)
        .or_else(|| {
            iso_date_re()
                .captures(masked)
                .and_then(|caps| caps.get(1))
                .map(|m| &masked[m.range()])
        })
        .and_then(|year| year.parse::<u16>().ok())
}

/// Detex a DOI candidate and trim trailing punctuation.
fn tidy_doi(raw: &str) -> Option<String> {
    let doi = latex_to_text(raw);
    let doi = doi.trim_end_matches(['.', ',', ';', ':', ')', ']']);
    if doi.is_empty() {
        None
    } else {
        Some(doi.to_owned())
    }
}

/// `{\_}` and `\_` as a plain `_`, so an escaped underscore inside a DOI
/// (`10.1002/{\_}x`) neither ends the DOI match nor leaves braces in it.
fn unescape_underscores(raw: &str) -> String {
    raw.replace("{\\_}", "_").replace("\\_", "_")
}

/// First DOI (`10.xxxx/...`) in raw or detexed text.
fn find_doi(raw: &str) -> Option<String> {
    let unescaped = unescape_underscores(raw);
    doi_re().find(&unescaped).and_then(|m| tidy_doi(m.as_str()))
}

/// DOI from a `doi.org` URL only.
fn doi_from_url(url: &str) -> Option<String> {
    let unescaped = unescape_underscores(url);
    doi_url_re()
        .captures(&unescaped)
        .and_then(|caps| caps.get(1))
        .and_then(|m| tidy_doi(m.as_str()))
}

/// First `arXiv` identifier written as `arXiv:id`, `arxiv.org/abs/id`,
/// `arXiv.id` (as in `DataCite` DOIs) or `\eprint{id}`.
fn find_arxiv(raw: &str) -> Option<String> {
    arxiv_re()
        .captures(raw)
        .or_else(|| eprint_re().captures(raw))
        .and_then(|caps| caps.get(1))
        .map(|m| m.as_str().to_owned())
}

/// An `eprint` field value that is exactly an `arXiv` identifier.
fn arxiv_from_eprint(value: &str) -> Option<String> {
    arxiv_id_re()
        .captures(value.trim())
        .and_then(|caps| caps.get(1))
        .map(|m| m.as_str().to_owned())
}

/// `authors. title. venue year`, skipping missing parts.
fn compose_text(
    authors: &[String],
    title: Option<&str>,
    venue: Option<&str>,
    year: Option<u16>,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !authors.is_empty() {
        parts.push(authors.join(", "));
    }
    if let Some(title_text) = title {
        parts.push(title_text.to_owned());
    }
    let tail: Vec<String> = venue
        .map(str::to_owned)
        .into_iter()
        .chain(year.map(|y| format!("{y}")))
        .collect();
    if !tail.is_empty() {
        parts.push(tail.join(" "));
    }
    parts.join(". ")
}

// ---------------------------------------------------------------------------
// .bbl
// ---------------------------------------------------------------------------

/// Parse a typeset bibliography.
///
/// `\bibitem`-based files (`plain`, `natbib`, ACM styles): each item is
/// split on `\bibitem`, the optional `[label]` (nested braces and `\protect`
/// allowed) and `{key}` are read, `text` is the detexed remainder, `year` is
/// the publication year outside URLs and identifiers, `doi` comes from
/// `\doi{..}`, `doi:`, `https://doi.org/` or a bare `10.xxxx/..`, `arxiv_id`
/// from `arXiv:..`, `/abs/` or `\eprint{..}`. When the item has `\newblock`s
/// the first segment gives the authors and the second the title (ACM's
/// `\showarticletitle{..}` is preferred when present). Without `\newblock`,
/// an INFORMS item `Authors (year) Title. \emph{Venue} ..` is split around
/// the year (see `author_year_inline`); otherwise (`IEEEtran`, `siam`) the
/// title is the first quoted span (` ``..'' `,
/// `"..."`, `“..”`) or italic group (`{\em ..}`, `\emph{..}`,
/// `\textit{..}`), whichever opens first, and the authors are the text
/// before it; with neither, both stay empty. The year skips page and volume
/// numbers (see `first_year`). Files without `\bibitem` are tried as
/// `biblatex` `\entry` blocks.
pub fn parse_bbl(text: &str) -> Vec<TruthReference> {
    let clean = strip_comments(text);
    if bibitem_re().is_match(&clean) {
        parse_bibitems(&clean)
    } else {
        parse_biblatex(&clean)
    }
}

fn parse_bibitems(clean: &str) -> Vec<TruthReference> {
    let body = clean
        .find("\\end{thebibliography}")
        .map_or(clean, |end| &clean[..end]);
    let item_starts: Vec<usize> = bibitem_re().find_iter(body).map(|m| m.start()).collect();
    let mut refs = Vec::with_capacity(item_starts.len());
    for (n, &begin) in item_starts.iter().enumerate() {
        let stop = item_starts.get(n + 1).copied().unwrap_or(body.len());
        if let Some(entry) = parse_bibitem(&body[begin..stop]) {
            refs.push(entry);
        }
    }
    refs
}

/// One `\bibitem[label]{key} ...` item.
fn parse_bibitem(item: &str) -> Option<TruthReference> {
    let mut i = skip_ws(item, "\\bibitem".len());
    let mut label = None;
    if item.as_bytes().get(i) == Some(&b'[') {
        let close = matching_close(item, i)?;
        let label_text = latex_to_text(&item[i + 1..close]);
        if !label_text.is_empty() {
            label = Some(label_text);
        }
        i = skip_ws(item, close + 1);
    }
    let (key_range, past_key) = brace_group(item, i)?;
    let key = item[key_range].trim().to_owned();
    if key.is_empty() {
        return None;
    }
    let rest = &item[past_key..];
    let text = latex_to_text(rest);
    let segments: Vec<&str> = newblock_re().split(rest).collect();
    let (authors, title) = if segments.len() >= 2 {
        let first = latex_to_text(segments[0]);
        if let Some((names, title)) = author_year_title(&first)
            && !acm_title_re().is_match(rest)
        {
            (split_bbl_authors(names), Some(title))
        } else {
            (split_bbl_authors(&first), bbl_title(rest, segments[1]))
        }
    } else if let Some((inline_authors, inline_title)) =
        author_year_inline(rest).or_else(|| inline_title(rest))
    {
        (inline_authors, Some(inline_title))
    } else {
        (Vec::new(), None)
    };
    let year = first_year(&text);
    let doi = find_doi(rest);
    let arxiv_id = find_arxiv(rest);
    Some(TruthReference {
        key,
        label,
        text,
        authors,
        title,
        year,
        doi,
        arxiv_id,
        source: TruthSource::Bbl,
    })
}

/// Title of a `\bibitem`: an explicit ACM title macro anywhere in the item,
/// else the detexed second `\newblock` segment without its final period, a
/// closing series (`, volume 48`), year (`, 2025`, `, 2024a`) or bracketed
/// descriptor (`[analysis code]`). A segment that is only a URL is no title.
fn bbl_title(rest: &str, segment: &str) -> Option<String> {
    if let Some(m) = acm_title_re().find(rest) {
        let open = m.end() - 1;
        if let Some(close) = matching_close(rest, open) {
            let title = latex_to_text(&rest[open + 1..close]);
            if !title.is_empty() {
                return Some(title);
            }
        }
    }
    let full = latex_to_text(segment);
    let title = full.strip_suffix('.').unwrap_or(&full).trim();
    if url_only_re().is_match(title) {
        return None;
    }
    let title = without_suffix(title, series_suffix_re());
    let title = without_suffix(title, title_year_suffix_re());
    let title = without_suffix(title, descriptor_suffix_re());
    if title.is_empty() {
        None
    } else {
        Some(title.to_owned())
    }
}

/// Authors and title of a first `\newblock` segment that carries both
/// around a parenthesised year (`Gui G, Toubia O (2023) The challenge of
/// using llms. \newblock arXiv preprint`); the later segments are the
/// venue. `None` when nothing that reads as a title follows the year.
fn author_year_title(first: &str) -> Option<(&str, String)> {
    let caps = author_year_title_re().captures(first)?;
    let names = caps.get(1)?.as_str().trim();
    let tail = caps.get(2)?.as_str().trim();
    // An `aea`-like quoted title (`“Title,”`) loses its quotes and comma.
    let mut title = tail.trim_end_matches(['.', ',', ' ']);
    if let Some(inner) = title
        .strip_prefix(['“', '"'])
        .and_then(|t| t.strip_suffix(['”', '"']))
    {
        title = inner.trim_end_matches([',', '.', ' ']).trim();
    }
    if names.is_empty() || title.split_whitespace().count() < 2 {
        return None;
    }
    Some((names, title.to_owned()))
}

/// `text` without the part `re` matches at its end, unless that is all of it.
fn without_suffix<'a>(text: &'a str, re: &Regex) -> &'a str {
    re.find(text)
        .filter(|m| m.start() > 0)
        .map_or(text, |m| text[..m.start()].trim_end())
}

/// Lower-case abbreviations whose period does not end a sentence.
const NON_FINAL_ABBREVIATIONS: [&str; 10] = [
    "e.g", "i.e", "vs", "dr", "st", "no", "vol", "fig", "al", "cf",
];

/// Whether the period right after `before` (the text up to it) belongs to an
/// abbreviation or an initial rather than ending a sentence: the word before
/// it is a single letter (`J.`, the `S` of `U.S.`), already contains a
/// period (`U.S`, `e.g`), or is in [`NON_FINAL_ABBREVIATIONS`].
fn abbreviation_period(before: &str) -> bool {
    let word = before
        .rsplit(char::is_whitespace)
        .next()
        .unwrap_or("")
        .trim_start_matches(['(', '[', '{', '"', '\u{201C}']);
    let mut chars = word.chars();
    let single_letter = chars.next().is_some_and(char::is_alphabetic) && chars.next().is_none();
    single_letter
        || word.contains('.')
        || NON_FINAL_ABBREVIATIONS.contains(&word.to_lowercase().as_str())
}

/// `text` up to its first sentence end: a `.`, `?` or `!` followed by
/// whitespace and then an uppercase letter or `[`. A closing `?` or `!` stays,
/// a period is dropped. A period inside a token (no whitespace after it) or
/// after an abbreviation or initial (see [`abbreviation_period`]: `U.S.`,
/// `Dr.`, `e.g.`) does not end the sentence. The whole `text` when there is
/// none.
fn first_sentence(text: &str) -> &str {
    for (at, c) in text.char_indices() {
        if !matches!(c, '.' | '?' | '!') {
            continue;
        }
        if c == '.' && abbreviation_period(&text[..at]) {
            continue;
        }
        let after = &text[at + c.len_utf8()..];
        let next_word = after.trim_start();
        if next_word.len() == after.len() {
            continue;
        }
        if next_word
            .chars()
            .next()
            .is_some_and(|n| n.is_uppercase() || n == '[')
        {
            let end = if c == '.' { at } else { at + c.len_utf8() };
            return &text[..end];
        }
    }
    text
}

/// Authors and title of a `\bibitem` without `\newblock` that reads
/// `Authors (year) Title. \emph{Venue} vol(no):pages.` (INFORMS, 2602.16061):
/// the detexed text before the first italic group is split around the
/// parenthesised year (see [`author_year_title`]) and the title ends at its
/// first sentence end (`… homo silicus? Technical report, …`). `None` when the
/// author part has a digit or a quote (a volume, or an `IEEEtran` quoted
/// title, precedes the year), when fewer than two words follow the year, or
/// when the text after the year starts with `In`; an italic group right after
/// the year (a book title) therefore falls through to [`inline_title`].
fn author_year_inline(rest: &str) -> Option<(Vec<String>, String)> {
    let head = italic_span(rest).map_or(rest, |(start, _)| &rest[..start]);
    let first = latex_to_text(head);
    let (names, tail) = author_year_title(&first)?;
    if names
        .chars()
        .any(|c| c.is_ascii_digit() || matches!(c, '"' | '\u{201C}' | '\u{201D}'))
    {
        return None;
    }
    let title = first_sentence(&tail).trim_end_matches(['.', ',', ' ']);
    let words = title
        .split_whitespace()
        .filter(|word| word.chars().filter(|c| c.is_alphabetic()).count() >= 2)
        .count();
    if words < 2 || title.to_lowercase().starts_with("in ") {
        return None;
    }
    Some((split_bbl_authors(names), title.to_owned()))
}

/// Byte index of the first `pat` in `s` at or after `from` that is not
/// preceded by a backslash.
fn find_unescaped(s: &str, from: usize, pat: &str) -> Option<usize> {
    s[from..]
        .match_indices(pat)
        .map(|(off, _)| from + off)
        .find(|&at| at == 0 || s.as_bytes()[at - 1] != b'\\')
}

/// First quoted span in a raw item: ` ``…'' `, `“…”` or `"…"`. Characters
/// after a backslash (accents such as `\"`) are skipped. Returns the byte
/// index of the opening quote and the inner byte range; `None` when there is
/// no opening quote or it is never closed.
fn quoted_span(rest: &str) -> Option<(usize, Range<usize>)> {
    let mut i = 0;
    while let Some(c) = char_at(rest, i) {
        let (open_len, closer) = match c {
            '\\' => {
                i += 1;
                if let Some(escaped) = char_at(rest, i) {
                    i += escaped.len_utf8();
                }
                continue;
            }
            '`' if rest[i..].starts_with("``") => (2, "''"),
            '\u{201C}' => ('\u{201C}'.len_utf8(), "\u{201D}"),
            '"' => (1, "\""),
            _ => {
                i += c.len_utf8();
                continue;
            }
        };
        let inner_start = i + open_len;
        let close = find_unescaped(rest, inner_start, closer)?;
        return Some((i, inner_start..close));
    }
    None
}

/// First italic group (`{\em ..}`, `{\it ..}`, `\emph{..}`, `\textit{..}`)
/// in a raw item whose text is not empty and not just `et al.`. Returns the
/// byte index where the group's markup starts and the inner byte range.
fn italic_span(rest: &str) -> Option<(usize, Range<usize>)> {
    for m in italic_re().find_iter(rest) {
        let braced_command = rest[m.start()..].starts_with('{');
        let open = if braced_command {
            m.start()
        } else {
            m.end() - 1
        };
        let Some(close) = matching_close(rest, open) else {
            continue;
        };
        let inner_start = if braced_command { m.end() } else { open + 1 };
        if inner_start > close {
            continue;
        }
        let text = latex_to_text(&rest[inner_start..close]);
        if text.is_empty() || text.to_lowercase().starts_with("et al") {
            continue;
        }
        return Some((m.start(), inner_start..close));
    }
    None
}

/// Authors from the detexed text before an inline title: trailing
/// punctuation and a final `et al.` are dropped, then [`split_bbl_authors`].
fn inline_authors(prefix: &str) -> Vec<String> {
    let mut text = prefix.trim().trim_end_matches([',', ';', ':']).trim_end();
    for suffix in ["et al.", "et al"] {
        if let Some(head) = text.strip_suffix(suffix) {
            text = head.trim_end().trim_end_matches(',').trim_end();
            break;
        }
    }
    split_bbl_authors(text)
}

/// Authors and title of a `\bibitem` without `\newblock` (`IEEEtran`,
/// `siam`, `plain`-like output): the first quoted span or italic group,
/// whichever opens first, is the title, and the text before it the author
/// list. A quote inside an italic title (`{\em ``Direct search'' solution
/// ..}`) therefore stays part of it. An italic group right after the word
/// `in` is a book or proceedings title, not the entry's title, so the item
/// then gets neither. `None` when nothing qualifies.
fn inline_title(rest: &str) -> Option<(Vec<String>, String)> {
    let quoted = quoted_span(rest);
    let italic = italic_span(rest);
    let (start, inner, is_italic) = match (quoted, italic) {
        (Some((q_start, q_inner)), Some((i_start, i_inner))) => {
            if i_start < q_start {
                (i_start, i_inner, true)
            } else {
                (q_start, q_inner, false)
            }
        }
        (Some((q_start, q_inner)), None) => (q_start, q_inner, false),
        (None, Some((i_start, i_inner))) => (i_start, i_inner, true),
        (None, None) => return None,
    };
    let prefix = latex_to_text(&rest[..start]);
    if is_italic && ends_with_word(&prefix.to_lowercase(), "in") {
        return None;
    }
    let full = latex_to_text(&rest[inner]);
    let title = full
        .trim_end_matches([',', '.', ';', ':', ' '])
        .trim_start();
    if title.is_empty() {
        return None;
    }
    Some((inline_authors(&prefix), title.to_owned()))
}

/// Split a detexed author segment (`A B, C D, and E F.` or `B, A., D, C., and
/// F, E. (2020).`) into `First Last` names.
fn split_bbl_authors(text: &str) -> Vec<String> {
    let without_year = trailing_year_re().replace(text, "");
    let joined = and_sep_re().replace_all(without_year.trim(), ", ");
    let mut authors: Vec<String> = Vec::new();
    for raw_token in joined.split(',') {
        // A final period after a surname is punctuation; after an initial it stays.
        let trimmed = raw_token.trim();
        let token = trimmed
            .strip_suffix('.')
            .filter(|stem| stem.chars().last().is_some_and(char::is_lowercase))
            .unwrap_or(trimmed);
        if token.is_empty()
            || is_year_token(token)
            || token.eq_ignore_ascii_case("others")
            || token.eq_ignore_ascii_case("et al")
            || token.eq_ignore_ascii_case("et al.")
        {
            continue;
        }
        // `Surname, A. B.` style: initials attach to the previous surname.
        if initials_re().is_match(token) && !authors.is_empty() {
            let last = authors.len() - 1;
            let merged = format!("{token} {}", authors[last]);
            authors[last] = merged;
        } else {
            authors.push(token.to_owned());
        }
    }
    authors
}

/// A year left in an author list (`Henderson, M., 2022.` in `elsarticle-harv`):
/// `2022`, `2022.` or `2022a`.
fn is_year_token(token: &str) -> bool {
    let stem = token.strip_suffix('.').unwrap_or(token);
    let digits = stem
        .strip_suffix(|c: char| c.is_ascii_lowercase())
        .unwrap_or(stem);
    digits.len() == 4
        && digits.bytes().all(|b| b.is_ascii_digit())
        && (digits.starts_with("19") || digits.starts_with("20"))
}

/// `biblatex` `.bbl`: `\entry{key}{type}{options} ... \endentry` blocks.
fn parse_biblatex(clean: &str) -> Vec<TruthReference> {
    let marks: Vec<(usize, usize)> = entry_re()
        .find_iter(clean)
        .map(|m| (m.start(), m.end()))
        .collect();
    let mut refs = Vec::new();
    for (n, &(begin, brace_end)) in marks.iter().enumerate() {
        let stop = marks.get(n + 1).map_or(clean.len(), |&(next, _)| next);
        let block = &clean[begin..stop];
        let open = brace_end - 1 - begin;
        let Some(close) = matching_close(block, open) else {
            continue;
        };
        let key = block[open + 1..close].trim();
        if key.is_empty() {
            continue;
        }
        refs.push(biblatex_entry(key, &block[close + 1..]));
    }
    refs
}

fn biblatex_entry(key: &str, body: &str) -> TruthReference {
    let mut fields: BTreeMap<String, String> = BTreeMap::new();
    for caps in field_re().captures_iter(body) {
        let Some(whole) = caps.get(0) else { continue };
        let open = whole.end() - 1;
        let Some(close) = matching_close(body, open) else {
            continue;
        };
        fields
            .entry(caps[1].to_ascii_lowercase())
            .or_insert_with(|| body[open + 1..close].to_owned());
    }
    for caps in verb_re().captures_iter(body) {
        fields
            .entry(caps[1].to_ascii_lowercase())
            .or_insert_with(|| caps[2].trim().to_owned());
    }
    let get = |wanted: &str| fields.get(wanted).map(String::as_str);
    let authors = biblatex_names(body);
    let title = get("title")
        .map(latex_to_text)
        .filter(|value| !value.is_empty());
    let year = get("year")
        .or_else(|| get("labelyear"))
        .or_else(|| get("date"))
        .and_then(first_year);
    let doi = get("doi").and_then(find_doi);
    let prefix_ok =
        get("eprinttype").is_none_or(|value| value.trim().eq_ignore_ascii_case("arxiv"));
    let arxiv_id = get("eprint")
        .filter(|_| prefix_ok)
        .and_then(arxiv_from_eprint)
        .or_else(|| find_arxiv(body));
    let venue = get("journaltitle")
        .or_else(|| get("booktitle"))
        .map(latex_to_text)
        .filter(|value| !value.is_empty());
    let text = compose_text(&authors, title.as_deref(), venue.as_deref(), year);
    TruthReference {
        key: key.to_owned(),
        label: None,
        text,
        authors,
        title,
        year,
        doi,
        arxiv_id,
        source: TruthSource::Bbl,
    }
}

/// Authors of a `biblatex` entry from `\name{author}{n}{}{{...family={X},given={Y}...}}`.
fn biblatex_names(body: &str) -> Vec<String> {
    let marker = "\\name{author}";
    let Some(pos) = body.find(marker) else {
        return Vec::new();
    };
    let mut i = pos + marker.len();
    let mut names_range: Option<Range<usize>> = None;
    for _ in 0..3 {
        i = skip_ws(body, i);
        let Some((inner, past_group)) = brace_group(body, i) else {
            return Vec::new();
        };
        names_range = Some(inner);
        i = past_group;
    }
    let Some(range) = names_range else {
        return Vec::new();
    };
    let names = &body[range];
    let mut authors: Vec<String> = Vec::new();
    let mut family: Option<String> = None;
    let mut given: Option<String> = None;
    for caps in name_part_re().captures_iter(names) {
        let Some(whole) = caps.get(0) else { continue };
        let open = whole.end() - 1;
        let Some(close) = matching_close(names, open) else {
            continue;
        };
        let value = latex_to_text(&names[open + 1..close]);
        if &caps[1] == "family" {
            if let Some(previous) = family.take() {
                authors.push(join_name(given.take().as_deref(), &previous));
            }
            family = Some(value);
        } else {
            given = Some(value);
        }
    }
    if let Some(previous) = family {
        authors.push(join_name(given.as_deref(), &previous));
    }
    authors
}

fn join_name(given: Option<&str>, family: &str) -> String {
    given
        .filter(|first| !first.is_empty())
        .map_or_else(|| family.to_owned(), |first| format!("{first} {family}"))
}

// ---------------------------------------------------------------------------
// .bib
// ---------------------------------------------------------------------------

/// Parse a `BibTeX` database.
///
/// Entries are `@type{key, field = value, ...}` (or with parentheses); values
/// may be brace groups, quoted strings, numbers or macros (month names are
/// expanded) joined with `#`. `@string`, `@preamble` and `@comment` are
/// skipped and anything outside an entry is ignored. Field names are
/// case-insensitive. `title` is detexed; `author` is split on `and` at brace
/// depth zero and each name normalised to `First Last`; `doi` comes from the
/// `doi` field or a `doi.org` URL; `arxiv_id` from `eprint` (unless
/// `archivePrefix` names another archive) or any field mentioning `arXiv:id`.
/// `text` is `authors. title. journal-or-booktitle year`.
pub fn parse_bib(text: &str) -> Vec<TruthReference> {
    let mut refs = Vec::new();
    let mut cursor = 0;
    while let Some(off) = text[cursor..].find('@') {
        let at = cursor + off;
        let mut name_end = at + 1;
        while char_at(text, name_end).is_some_and(|c| c.is_ascii_alphabetic()) {
            name_end += 1;
        }
        let kind = text[at + 1..name_end].to_ascii_lowercase();
        let open = skip_ws(text, name_end);
        let is_open = matches!(text.as_bytes().get(open), Some(b'{' | b'('));
        if kind.is_empty() || !is_open {
            cursor = at + 1;
            continue;
        }
        let Some(close) = matching_close(text, open) else {
            cursor = open + 1;
            continue;
        };
        cursor = close + 1;
        if matches!(kind.as_str(), "string" | "preamble" | "comment") {
            continue;
        }
        if let Some(entry) = bib_entry(&text[open + 1..close]) {
            refs.push(entry);
        }
    }
    refs
}

/// The inside of one `@type{ ... }` group.
fn bib_entry(body: &str) -> Option<TruthReference> {
    let comma = body.find(',');
    let key = comma.map_or(body, |pos| &body[..pos]).trim();
    let mut cursor = comma.map_or(body.len(), |pos| pos + 1);
    if key.is_empty() {
        return None;
    }
    let mut fields: BTreeMap<String, String> = BTreeMap::new();
    loop {
        cursor = skip_ws(body, cursor);
        let Some(eq_off) = body[cursor..].find('=') else {
            break;
        };
        let name = body[cursor..cursor + eq_off].trim().to_ascii_lowercase();
        if name.is_empty() || name.contains([',', '{', '}', '"']) {
            break;
        }
        let (value, past_value) = bib_value(body, cursor + eq_off + 1);
        fields.entry(name).or_insert(value);
        cursor = skip_ws(body, past_value);
        if body.as_bytes().get(cursor) == Some(&b',') {
            cursor += 1;
        } else {
            break;
        }
    }
    let get = |wanted: &str| fields.get(wanted).map(String::as_str);
    let authors = get("author").map_or_else(Vec::new, split_bib_authors);
    let title = get("title")
        .map(latex_to_text)
        .filter(|value| !value.is_empty());
    let year = get("year").or_else(|| get("date")).and_then(first_year);
    let doi = get("doi")
        .and_then(find_doi)
        .or_else(|| get("url").and_then(doi_from_url));
    let prefix_ok = get("archiveprefix")
        .or_else(|| get("eprinttype"))
        .is_none_or(|value| value.trim().eq_ignore_ascii_case("arxiv"));
    let arxiv_id = get("eprint")
        .filter(|_| prefix_ok)
        .and_then(arxiv_from_eprint)
        .or_else(|| {
            ARXIV_FALLBACK_FIELDS
                .iter()
                .find_map(|other| get(other).and_then(find_arxiv))
        });
    let venue = get("journal")
        .or_else(|| get("booktitle"))
        .map(latex_to_text)
        .filter(|value| !value.is_empty());
    let text = compose_text(&authors, title.as_deref(), venue.as_deref(), year);
    Some(TruthReference {
        key: key.to_owned(),
        label: None,
        text,
        authors,
        title,
        year,
        doi,
        arxiv_id,
        source: TruthSource::Bib,
    })
}

/// A field value starting at `from`: `{...}`, `"..."`, digits or a macro
/// name, possibly joined with `#`. Returns the raw value and the index after it.
fn bib_value(body: &str, from: usize) -> (String, usize) {
    let bytes = body.as_bytes();
    let mut value = String::new();
    let mut i = from;
    loop {
        i = skip_ws(body, i);
        match bytes.get(i) {
            Some(b'{') => {
                let close = matching_close(body, i).unwrap_or(body.len());
                value.push_str(&body[i + 1..close]);
                i = close + 1;
            }
            Some(b'"') => {
                let end = quoted_end(body, i + 1);
                value.push_str(&body[i + 1..end]);
                i = end + 1;
            }
            Some(lead) if lead.is_ascii_digit() => {
                let mut j = i;
                while bytes.get(j).is_some_and(u8::is_ascii_digit) {
                    j += 1;
                }
                value.push_str(&body[i..j]);
                i = j;
            }
            Some(lead) if lead.is_ascii_alphabetic() => {
                let mut j = i;
                while bytes.get(j).is_some_and(|&b| is_bib_macro_byte(b)) {
                    j += 1;
                }
                value.push_str(expand_bib_macro(&body[i..j]));
                i = j;
            }
            _ => break,
        }
        let past = skip_ws(body, i);
        if bytes.get(past) == Some(&b'#') {
            i = past + 1;
        } else {
            i = past;
            break;
        }
    }
    (value, i)
}

/// A byte that may appear in a `BibTeX` macro name.
fn is_bib_macro_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b':' | b'.')
}

/// Index of the `"` closing a quoted value that started at `from`; braces
/// inside the string protect quotes.
fn quoted_end(body: &str, from: usize) -> usize {
    let bytes = body.as_bytes();
    let mut depth = 0usize;
    let mut i = from;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 1,
            b'{' => depth += 1,
            b'}' => depth = depth.saturating_sub(1),
            b'"' if depth == 0 => return i,
            _ => {}
        }
        i += 1;
    }
    bytes.len()
}

/// The standard month macros; any other macro stands for its own name.
fn expand_bib_macro(name: &str) -> &str {
    match name.to_ascii_lowercase().as_str() {
        "jan" => "January",
        "feb" => "February",
        "mar" => "March",
        "apr" => "April",
        "may" => "May",
        "jun" => "June",
        "jul" => "July",
        "aug" => "August",
        "sep" => "September",
        "oct" => "October",
        "nov" => "November",
        "dec" => "December",
        _ => name,
    }
}

/// Split a raw `author` field on `and` at brace depth zero.
fn split_bib_authors(raw: &str) -> Vec<String> {
    let bytes = raw.as_bytes();
    let mut pieces: Vec<&str> = Vec::new();
    let mut depth = 0usize;
    let mut begin = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 1,
            b'{' => depth += 1,
            b'}' => depth = depth.saturating_sub(1),
            current if depth == 0 && current.is_ascii_whitespace() => {
                if let Some(m) = and_re().find(&raw[i..]) {
                    pieces.push(&raw[begin..i]);
                    i += m.end();
                    begin = i;
                    continue;
                }
            }
            _ => {}
        }
        i += 1;
    }
    pieces.push(&raw[begin..]);
    pieces.into_iter().filter_map(normalize_author).collect()
}

/// Detex one name and turn `Last, First` (or `Last, Jr., First`) into
/// `First Last`; `others` is dropped.
fn normalize_author(piece: &str) -> Option<String> {
    let text = latex_to_text(piece);
    if text.is_empty() || text.eq_ignore_ascii_case("others") {
        return None;
    }
    if !text.contains(',') {
        return Some(text);
    }
    let mut parts: Vec<&str> = text.split(',').map(str::trim).collect();
    let given = parts.pop().unwrap_or("");
    let family = parts.join(" ");
    Some(collapse_whitespace(&format!("{given} {family}")))
}

// ---------------------------------------------------------------------------
// \cite commands
// ---------------------------------------------------------------------------

/// One citation command found in comment-free `TeX`.
struct CiteCommand {
    /// Byte range of the whole command including its arguments.
    span: Range<usize>,
    /// It was `\nocite` or a `multibib` `\nocite<name>`.
    nocite: bool,
    /// It prints only an author, year, title, date or URL.
    author_year_only: bool,
    keys: Vec<String>,
}

/// Whether `\name` is a citation command: a known one, or any other name
/// starting with `cite` / `Cite` (the `multibib` commands `\citeapp`,
/// `\citemain`, `\citeA`, ...) that is not a style or font hook.
fn is_cite_command(name: &str) -> bool {
    if CITE_COMMANDS.contains(&name) {
        return true;
    }
    let lower = name.to_ascii_lowercase();
    lower.starts_with("cite") && !NOT_CITE_COMMANDS.contains(&lower.as_str())
}

/// Whether `\name` prints no linkable marker (`\citeauthor`, `\citeyear`, ...).
fn is_author_year_only(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    AUTHOR_YEAR_ONLY_COMMANDS.contains(&lower.as_str())
}

/// A plausible citation key: no whitespace, backslash or braces.
fn is_plain_key(key: &str) -> bool {
    !key.contains(|c: char| c.is_whitespace() || matches!(c, '\\' | '{' | '}'))
}

/// Whether `\name` is `\nocite` or a `multibib` variant (`\nociteapp`, ...).
fn is_nocite_command(name: &str) -> bool {
    name.starts_with("nocite")
}

/// All `\cite`-family and `\nocite` commands with their key lists.
fn scan_cites(clean: &str) -> Vec<CiteCommand> {
    let mut found = Vec::new();
    for caps in command_re().captures_iter(clean) {
        let Some(whole) = caps.get(0) else { continue };
        let name = &caps[1];
        let nocite = is_nocite_command(name);
        if !nocite && !is_cite_command(name) {
            continue;
        }
        let known = name == "nocite" || CITE_COMMANDS.contains(&name);
        let mut i = skip_ws(clean, whole.end());
        for _ in 0..2 {
            if clean.as_bytes().get(i) != Some(&b'[') {
                break;
            }
            let Some(close) = matching_close(clean, i) else {
                break;
            };
            i = skip_ws(clean, close + 1);
        }
        let Some((inner, past_group)) = brace_group(clean, i) else {
            continue;
        };
        let raw_keys: Vec<&str> = clean[inner]
            .split(',')
            .map(str::trim)
            .filter(|k| !k.is_empty())
            .collect();
        if !known && !raw_keys.iter().all(|k| is_plain_key(k)) {
            continue;
        }
        let keys: Vec<String> = raw_keys
            .into_iter()
            .filter(|k| !k.contains('#'))
            .map(str::to_owned)
            .collect();
        if keys.is_empty() {
            continue;
        }
        found.push(CiteCommand {
            span: whole.start()..past_group,
            nocite,
            author_year_only: !nocite && is_author_year_only(name),
            keys,
        });
    }
    found
}

/// Citation commands in a `.tex` source.
///
/// Recognises `\cite`, `\citep`, `\citet`, `\citealp`, `\citealt`,
/// `\citeauthor`, `\citeyear`, `\citeyearpar`, `\citenum`, `\parencite`,
/// `\textcite`, `\autocite`, `\footcite` and friends, and any other command
/// named `cite...` (`multibib`'s `\citeapp`, `\citeA`, ...) whose keys look
/// like keys, with `*` and up to two `[...]` arguments before `{keys}`;
/// `\nocite{keys}` and `\nocite{*}`, and `multibib`'s `\nocite<name>{keys}`
/// and `\nocite<name>{*}` (`\nociteapp{*}`). Commented text (`%`, `comment`
/// environments, `\iffalse ... \fi`) is ignored, and so are
/// commands with no key (macro definitions such as `\cite{#1}`). Keys are
/// trimmed and split on `,`. Only the first key group of multi-group
/// commands (`\cites{a}{b}`) is read. Author- or year-only commands are
/// counted in `cite_commands` and also in `cite_only_author_year`.
pub fn parse_cites(tex: &str) -> TruthCitations {
    let clean = remove_disabled(&strip_comments(tex));
    let mut cites = TruthCitations::default();
    for cmd in scan_cites(&clean) {
        if cmd.nocite {
            if cmd.keys.iter().any(|k| k == "*") {
                cites.nocite_all = true;
            }
            let listed = cmd.keys.into_iter().filter(|k| k != "*");
            cites.nocite_keys.extend(listed);
        } else {
            cites.cite_commands += 1;
            if cmd.author_year_only {
                cites.cite_only_author_year += 1;
            }
            cites.cited_keys.extend(cmd.keys);
        }
    }
    cites
}

// ---------------------------------------------------------------------------
// Body text
// ---------------------------------------------------------------------------

/// Detexed body of a document for alignment diagnostics.
///
/// Takes the text between `\begin{document}` and `\end{document}`, drops
/// the zero-argument macro definitions made there and expands every
/// zero-argument `\newcommand` macro first (so an alias such as
/// `\newcommand{\be}{\begin{equation}}` is removed like `equation`), then
/// removes the front matter (see [`remove_front_matter`]; the abstract is
/// kept), citation commands, floats, display environments, verbatim and
/// code listings and author biographies (see [`is_dropped_env`]), the
/// listing environments the document defines itself (see
/// [`verbatim_env_names`]), footnotes and box, listing and
/// colour settings (see [`BODY_DROPPED_COMMANDS`]), `key=value` options of
/// any environment (see [`remove_environment_options`]) and all math
/// (`$...$`, `$$...$$`, `\(...\)`, `\[...\]`, see [`remove_math`]), turns
/// `\section{X}` (and chapter, subsection, paragraph) into a paragraph of
/// its own, then detexes each blank-line-separated paragraph. Paragraphs
/// are joined with `"\n\n"`.
pub fn body_text(main_tex: &str) -> String {
    let clean = strip_comments(main_tex);
    let macros = collect_macros(&clean);
    let listing_envs = verbatim_env_names(&clean);
    let body = remove_definitions(document_body(&clean));
    let body = expand_macros(&expand_macros(&body, &macros), &macros);
    let body = remove_front_matter(&body);
    let body = remove_cites(&body);
    let body = remove_environments(&body, &listing_envs);
    let body = remove_commands(&body, BODY_DROPPED_COMMANDS);
    let body = remove_environment_options(&body);
    let body = remove_math(&body);
    let body = replace_headings(&body);
    let body = par_re().replace_all(&body, "\n\n");
    let mut paragraphs: Vec<String> = Vec::new();
    for para in blank_line_re().split(&body) {
        let text = latex_to_text(para);
        if !text.is_empty() {
            paragraphs.push(text);
        }
    }
    paragraphs.join("\n\n")
}

fn document_body(clean: &str) -> &str {
    let begin_tag = "\\begin{document}";
    let start = clean.find(begin_tag).map_or(0, |pos| pos + begin_tag.len());
    let stop = clean[start..]
        .find("\\end{document}")
        .map_or(clean.len(), |pos| start + pos);
    &clean[start..stop]
}

/// Front-matter commands removed from the document body with their
/// arguments, as `(name, brace arguments)`: title, author and affiliation
/// blocks (plain, `elsarticle`, `acmart`, ICML, IEEE, LNCS), notes,
/// emails, identifiers, keywords, dates, running heads and `\maketitle`. Any `[...]` optional
/// arguments before the first brace argument go with them.
const FRONT_MATTER_COMMANDS: &[(&str, usize)] = &[
    ("title", 1),
    ("subtitle", 1),
    ("shorttitle", 1),
    ("titlenote", 1),
    ("subtitlenote", 1),
    ("author", 1),
    ("affiliation", 1),
    ("affiliations", 1),
    ("affil", 1),
    ("address", 1),
    ("institute", 1),
    ("institution", 1),
    ("email", 1),
    ("ead", 1),
    ("thanks", 1),
    ("authornote", 1),
    ("authornotemark", 0),
    ("tnotetext", 1),
    ("tnoteref", 1),
    ("cortext", 1),
    ("corref", 1),
    ("fntext", 1),
    ("fnref", 1),
    ("icmltitle", 1),
    ("icmltitlerunning", 1),
    ("icmlauthor", 2),
    ("icmlaffiliation", 2),
    ("icmlcorrespondingauthor", 2),
    ("icmlkeywords", 1),
    ("icmlsetsymbol", 2),
    ("printAffiliationsAndNotice", 1),
    ("keywords", 1),
    ("IEEEauthorblockN", 1),
    ("IEEEauthorblockA", 1),
    ("IEEEpeerreviewmaketitle", 0),
    ("orcid", 1),
    ("orcidlink", 1),
    ("date", 1),
    ("ccsdesc", 1),
    ("maketitle", 0),
    ("titlerunning", 1),
    ("authorrunning", 1),
    ("markboth", 2),
];

/// Front-matter environments removed whole: keyword lists, author lists,
/// ACM classification XML, highlights and teaser figures.
fn is_front_matter_env(name: &str) -> bool {
    matches!(
        name.trim_end_matches('*'),
        "keyword"
            | "keywords"
            | "IEEEkeywords"
            | "icmlauthorlist"
            | "CCSXML"
            | "highlights"
            | "graphicalabstract"
            | "teaserfigure"
    )
}

/// `\begin{abstract}` or `\abstract`: where the abstract starts.
fn abstract_start_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\\(?:begin\s*\{abstract\}|abstract\b)").expect("valid regex"))
}

/// Index after the spaces and the one line end that follow `end`, when the
/// text between the previous line end and `start` is blank (the removed
/// text filled its lines); `end` otherwise. Removing a whole line this way
/// leaves no blank line behind to split a paragraph.
fn past_removed_line(body: &str, start: usize, end: usize) -> usize {
    let line_start = body[..start].rfind('\n').map_or(0, |pos| pos + 1);
    if !body[line_start..start].trim().is_empty() {
        return end;
    }
    let mut i = end;
    while matches!(char_at(body, i), Some(' ' | '\t' | '\r')) {
        i += 1;
    }
    if char_at(body, i) == Some('\n') {
        i + 1
    } else {
        end
    }
}

/// The document body without its front matter; the abstract stays.
///
/// Environments: [`is_front_matter_env`] ones are removed whole; a
/// `frontmatter` or `titlepage` environment loses everything before its
/// abstract (see [`abstract_start_re`]), or all of it when it has none.
/// Then every [`FRONT_MATTER_COMMANDS`] command is removed with its
/// arguments. A removal that fills its lines also takes the line end.
fn remove_front_matter(body: &str) -> String {
    let body = remove_front_environments(body);
    remove_front_commands(&body)
}

fn remove_front_environments(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut last = 0;
    for caps in begin_re().captures_iter(body) {
        let Some(whole) = caps.get(0) else { continue };
        if whole.start() < last {
            continue;
        }
        let name = &caps[1];
        let bare = name.trim_end_matches('*');
        if is_front_matter_env(bare) {
            let end = environment_end(body, name, whole.end());
            out.push_str(&body[last..whole.start()]);
            last = past_removed_line(body, whole.start(), end);
        } else if bare == "frontmatter" || bare == "titlepage" {
            let end = environment_end(body, name, whole.end());
            out.push_str(&body[last..whole.start()]);
            last = abstract_start_re()
                .find(&body[whole.end()..end])
                .map_or(end, |m| whole.end() + m.start());
        }
    }
    out.push_str(&body[last..]);
    out
}

fn remove_front_commands(body: &str) -> String {
    remove_commands(body, FRONT_MATTER_COMMANDS)
}

/// Commands removed from the document body with their arguments, as
/// `(name, brace arguments)`, besides the front matter: footnotes (the
/// extracted side tags footnote lines and leaves them out) and the
/// settings of boxes, listings, `TikZ`, colours, lists and theorems, whose
/// `key=value` arguments would otherwise survive as text.
const BODY_DROPPED_COMMANDS: &[(&str, usize)] = &[
    ("footnote", 1),
    ("footnotetext", 1),
    ("newtcolorbox", 2),
    ("renewtcolorbox", 2),
    ("newtcblisting", 2),
    ("renewtcblisting", 2),
    ("DeclareTColorBox", 3),
    ("tcbset", 1),
    ("tcbuselibrary", 1),
    ("newmdenv", 1),
    ("mdfsetup", 1),
    ("mdfdefinestyle", 2),
    ("surroundwithmdframed", 1),
    ("lstset", 1),
    ("lstdefinestyle", 2),
    ("lstdefinelanguage", 2),
    ("tikzset", 1),
    ("usetikzlibrary", 1),
    ("pgfplotsset", 1),
    ("definecolor", 3),
    ("colorlet", 2),
    ("hypersetup", 1),
    ("captionsetup", 1),
    ("setlist", 1),
    ("newtheorem", 2),
    ("theoremstyle", 1),
];

/// `body` without every `\name` of `commands` and its arguments: `[...]`
/// optional arguments before each of its brace arguments go with it. A
/// command whose brace arguments are missing loses only those found. A
/// removal that fills its lines also takes the line end.
fn remove_commands(body: &str, commands: &[(&str, usize)]) -> String {
    let mut out = String::with_capacity(body.len());
    let mut last = 0;
    for caps in command_re().captures_iter(body) {
        let Some(whole) = caps.get(0) else { continue };
        if whole.start() < last || body[..whole.start()].ends_with('\\') {
            continue;
        }
        let name = &caps[1];
        let Some(&(_, args)) = commands.iter().find(|entry| entry.0 == name) else {
            continue;
        };
        let mut i = whole.end();
        if args > 0 {
            i = skip_optional(body, i);
            for _ in 0..args {
                let Some((_, past_group)) =
                    brace_group(body, skip_ws(body, skip_optional(body, i)))
                else {
                    break;
                };
                i = past_group;
            }
        }
        out.push_str(&body[last..whole.start()]);
        last = past_removed_line(body, whole.start(), i);
    }
    out.push_str(&body[last..]);
    out
}

fn remove_cites(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut last = 0;
    for cmd in scan_cites(body) {
        if cmd.span.start < last {
            continue;
        }
        out.push_str(&body[last..cmd.span.start]);
        last = cmd.span.end;
    }
    out.push_str(&body[last..]);
    out
}

/// Environments whose contents never appear as running text. Verbatim and
/// code listings (`verbatim`, fancyvrb `Verbatim`, `lstlisting`, `minted`,
/// `alltt`, tcolorbox `tcblisting`, ...) are among them: the extracted
/// side tags their monospace lines `code` and leaves them out. Prose boxes
/// (`tcolorbox`, `mdframed`) are kept.
fn is_dropped_env(name: &str) -> bool {
    matches!(
        name.trim_end_matches('*'),
        "figure"
            | "table"
            | "tabular"
            | "tabularx"
            | "longtable"
            | "algorithm"
            | "algorithmic"
            | "algorithm2e"
            | "equation"
            | "align"
            | "alignat"
            | "flalign"
            | "gather"
            | "multline"
            | "eqnarray"
            | "displaymath"
            | "lstlisting"
            | "verbatim"
            | "Verbatim"
            | "BVerbatim"
            | "LVerbatim"
            | "minted"
            | "alltt"
            | "spverbatim"
            | "listing"
            | "code"
            | "python"
            | "pycode"
            | "sourcecode"
            | "tcblisting"
            | "tcbverbatim"
            | "tikzpicture"
            | "wrapfigure"
            | "wraptable"
            | "sidewaystable"
            | "sidewaysfigure"
            | "subfigure"
            | "thebibliography"
            | "filecontents"
            | "comment"
            | "IEEEbiography"
            | "IEEEbiographynophoto"
            | "biography"
    )
}

/// `\newtcblisting`, `\DeclareTCBListing`, `\DefineVerbatimEnvironment`
/// or `\lstnewenvironment` (and their `renew`/`New` forms), with any
/// `[...]` options before the brace argument; group 1 is the name of the
/// listing environment it defines.
fn verbatim_env_def_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(concat!(
            r"\\(?:newtcblisting|renewtcblisting|DeclareTCBListing|NewTCBListing|RenewTCBListing",
            r"|DefineVerbatimEnvironment|lstnewenvironment)",
            r"\s*(?:\[[^\]]*\]\s*)?\{\s*([A-Za-z]+\*?)\s*\}",
        ))
        .expect("valid regex")
    })
}

/// Names (without a trailing `*`) of the environments `clean` defines as
/// verbatim or code listings (see [`verbatim_env_def_re`]). `body_text`
/// drops them like the listings of [`is_dropped_env`]. Environments made
/// with `\newenvironment` or `\newtcolorbox` are not among them.
fn verbatim_env_names(clean: &str) -> BTreeSet<String> {
    verbatim_env_def_re()
        .captures_iter(clean)
        .map(|caps| caps[1].trim_end_matches('*').to_string())
        .collect()
}

/// `body` without every environment that [`is_dropped_env`] names or whose
/// name (without a trailing `*`) is in `listing_envs`.
fn remove_environments(body: &str, listing_envs: &BTreeSet<String>) -> String {
    let mut out = String::with_capacity(body.len());
    let mut last = 0;
    for caps in begin_re().captures_iter(body) {
        let Some(whole) = caps.get(0) else { continue };
        let name = &caps[1];
        if whole.start() < last
            || !(is_dropped_env(name) || listing_envs.contains(name.trim_end_matches('*')))
        {
            continue;
        }
        out.push_str(&body[last..whole.start()]);
        last = environment_end(body, &caps[1], whole.end());
    }
    out.push_str(&body[last..]);
    out
}

/// Index after the `\end{name}` matching an environment opened before `from`,
/// counting nested environments of the same name.
fn environment_end(body: &str, name: &str, from: usize) -> usize {
    let begin_tag = format!("\\begin{{{name}}}");
    let end_tag = format!("\\end{{{name}}}");
    let mut depth = 1usize;
    let mut i = from;
    while depth > 0 {
        let next_begin = body[i..].find(&begin_tag);
        let Some(next_end) = body[i..].find(&end_tag) else {
            return body.len();
        };
        if let Some(off) = next_begin.filter(|&off| off < next_end) {
            depth += 1;
            i += off + begin_tag.len();
        } else {
            depth -= 1;
            i += next_end + end_tag.len();
        }
    }
    i
}

/// Conditional-looking commands (`etoolbox`, `ifthen`) that take brace
/// arguments and are never closed by `\fi`.
const ARGUMENT_CONDITIONALS: &[&str] = &[
    "ifthenelse",
    "ifdef",
    "ifndef",
    "ifcsdef",
    "ifundef",
    "ifcsundef",
    "ifdefmacro",
    "ifcsmacro",
    "ifdefparam",
    "ifcsparam",
    "ifdefprefix",
    "ifcsprefix",
    "ifdefprotected",
    "ifcsprotected",
    "ifdefltxprotect",
    "ifcsltxprotect",
    "ifdefempty",
    "ifcsempty",
    "ifdefvoid",
    "ifcsvoid",
    "ifdefequal",
    "ifcsequal",
    "ifdefstring",
    "ifcsstring",
    "ifdefstrequal",
    "ifcsstrequal",
    "ifdefcounter",
    "ifcscounter",
    "ifltxcounter",
    "ifdeflength",
    "ifcslength",
    "ifdefdimen",
    "ifcsdimen",
    "ifstrequal",
    "ifstrempty",
    "ifblank",
    "ifnumcomp",
    "ifnumequal",
    "ifnumgreater",
    "ifnumless",
    "ifnumodd",
    "ifdimcomp",
    "ifdimequal",
    "ifdimgreater",
    "ifdimless",
    "ifbool",
    "ifboolexpr",
    "ifboolexpe",
    "iftoggle",
    "ifinlist",
    "ifinlistcs",
    "ifrmnum",
    "ifstrcmp",
];

/// The control word starting at the backslash at `at`: its name (letters
/// and `@`; empty for a control symbol such as `\\` or `\%`) and the index
/// just past it.
fn control_word(s: &str, at: usize) -> (&str, usize) {
    let start = at + 1;
    let mut end = start;
    while let Some(c) = char_at(s, end) {
        if c.is_ascii_alphabetic() || c == '@' {
            end += 1;
        } else {
            break;
        }
    }
    if end == start {
        let past = char_at(s, start).map_or(start, |c| start + c.len_utf8());
        return ("", past);
    }
    (&s[start..end], end)
}

/// Whether the control word `name` opens a `TeX` conditional closed by `\fi`.
fn opens_conditional(name: &str) -> bool {
    name.starts_with("if") && !ARGUMENT_CONDITIONALS.contains(&name)
}

/// Index just past the `\fi` (or a depth-one `\else`, whose branch is live)
/// that ends the false branch of an `\iffalse` ending at `from`. Nested
/// conditionals are counted; `\newif\ifname` declares rather than opens one.
/// `None` when the branch is never closed.
fn false_branch_end(s: &str, from: usize) -> Option<usize> {
    let mut depth = 1usize;
    let mut i = from;
    let mut after_newif = false;
    while let Some(off) = s[i..].find('\\') {
        let (name, end) = control_word(s, i + off);
        i = end;
        if name == "fi" {
            depth -= 1;
            if depth == 0 {
                return Some(end);
            }
        } else if name == "else" && depth == 1 {
            return Some(end);
        } else if !after_newif && opens_conditional(name) {
            depth += 1;
        }
        after_newif = name == "newif";
    }
    None
}

/// Remove `\iffalse ... \fi` blocks (up to a depth-one `\else`, whose
/// branch is kept). An `\iffalse` that is never closed is left alone.
fn remove_iffalse(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut copied = 0;
    let mut i = 0;
    let mut after_newif = false;
    while let Some(off) = s[i..].find('\\') {
        let at = i + off;
        let (name, end) = control_word(s, at);
        i = end;
        if name == "iffalse"
            && !after_newif
            && let Some(resume) = false_branch_end(s, end)
        {
            out.push_str(&s[copied..at]);
            copied = resume;
            i = resume;
        }
        after_newif = name == "newif";
    }
    out.push_str(&s[copied..]);
    out
}

/// Remove `\begin{comment} ... \end{comment}` environments (the `comment`
/// and `verbatim` packages); an unclosed one runs to the end of the text.
fn remove_comment_environments(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last = 0;
    for caps in begin_re().captures_iter(s) {
        let Some(whole) = caps.get(0) else { continue };
        if whole.start() < last || &caps[1] != "comment" {
            continue;
        }
        out.push_str(&s[last..whole.start()]);
        last = environment_end(s, "comment", whole.end());
    }
    out.push_str(&s[last..]);
    out
}

/// Remove text `TeX` never typesets from comment-free source: `comment`
/// environments, then `\iffalse ... \fi` blocks.
fn remove_disabled(clean: &str) -> String {
    remove_iffalse(&remove_comment_environments(clean))
}

/// Position of `\` + `symbol` at or after `from` whose backslash is not itself escaped.
fn find_control(body: &str, from: usize, symbol: u8) -> Option<usize> {
    let bytes = body.as_bytes();
    let mut i = from;
    while i + 1 < bytes.len() {
        if bytes[i] == b'\\' {
            if bytes[i + 1] == symbol {
                return Some(i);
            }
            i += 2;
        } else {
            i += 1;
        }
    }
    None
}

/// Index just past the `$` (or, for `display`, the `$$`) that closes math
/// opened before `from`, skipping control symbols such as `\$`. Inline math
/// never runs past a blank line. `None` when it is not closed.
fn math_close(body: &str, from: usize, display: bool) -> Option<usize> {
    let bytes = body.as_bytes();
    let mut i = from;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'$' if !display => return Some(i + 1),
            b'$' if bytes.get(i + 1) == Some(&b'$') => return Some(i + 2),
            b'\n'
                if !display
                    && body[i + 1..]
                        .trim_start_matches([' ', '\t', '\r'])
                        .starts_with('\n') =>
            {
                return None;
            }
            _ => i += 1,
        }
    }
    None
}

/// Remove all math: `$$ ... $$`, `\[ ... \]`, `\( ... \)` and inline
/// `$ ... $`, each replaced by one space. Control symbols (`\$`, `\\`) are
/// skipped, so `\\[2pt]` is not display math. A `$` or `$$` that is never
/// closed is dropped alone; an unclosed `\[` or `\(` runs to the end.
fn remove_math(body: &str) -> String {
    let bytes = body.as_bytes();
    let mut out = String::with_capacity(body.len());
    let mut copied = 0;
    let mut i = 0;
    while i < bytes.len() {
        let end = match (bytes[i], bytes.get(i + 1)) {
            (b'\\', Some(b'[')) => {
                find_control(body, i + 2, b']').map_or(body.len(), |close| close + 2)
            }
            (b'\\', Some(b'(')) => {
                find_control(body, i + 2, b')').map_or(body.len(), |close| close + 2)
            }
            (b'\\', _) => {
                i += 2;
                continue;
            }
            (b'$', Some(b'$')) => math_close(body, i + 2, true).unwrap_or(i + 2),
            (b'$', _) => math_close(body, i + 1, false).unwrap_or(i + 1),
            _ => {
                i += 1;
                continue;
            }
        };
        out.push_str(&body[copied..i]);
        out.push(' ');
        copied = end;
        i = end;
    }
    out.push_str(&body[copied..]);
    out
}

/// Remove the zero-argument macro definitions ([`newcommand_re`]) made in
/// the document body, so that expanding the macros does not rewrite the
/// definitions themselves.
fn remove_definitions(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut last = 0;
    for whole in newcommand_re().find_iter(body) {
        if whole.start() < last {
            continue;
        }
        let Some(close) = matching_close(body, whole.end() - 1) else {
            continue;
        };
        out.push_str(&body[last..whole.start()]);
        last = past_removed_line(body, whole.start(), close + 1);
    }
    out.push_str(&body[last..]);
    out
}

/// Remove a `[...]` option list holding a `key=value` pair (`[colback=...]`,
/// `[leftmargin=*]`) after `\begin{name}`, also when whitespace or a line
/// end comes first. A `[...]` without `=` (a theorem's name) is kept.
fn remove_environment_options(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut last = 0;
    for whole in begin_re().find_iter(body) {
        if whole.start() < last {
            continue;
        }
        let open = skip_ws(body, whole.end());
        if body.as_bytes().get(open) != Some(&b'[') {
            continue;
        }
        let Some(close) = matching_close(body, open) else {
            continue;
        };
        if !body[open..close].contains('=') {
            continue;
        }
        out.push_str(&body[last..whole.end()]);
        last = close + 1;
    }
    out.push_str(&body[last..]);
    out
}

/// `\section[short]{Title}` and relatives become their own paragraph.
fn replace_headings(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut last = 0;
    for m in heading_re().find_iter(body) {
        if m.start() < last {
            continue;
        }
        let mut i = skip_ws(body, m.end());
        if body.as_bytes().get(i) == Some(&b'[') {
            let Some(close) = matching_close(body, i) else {
                continue;
            };
            i = skip_ws(body, close + 1);
        }
        let Some((inner, past_group)) = brace_group(body, i) else {
            continue;
        };
        out.push_str(&body[last..m.start()]);
        out.push_str("\n\n");
        out.push_str(&body[inner]);
        out.push_str("\n\n");
        last = past_group;
    }
    out.push_str(&body[last..]);
    out
}

/// Zero-argument macro definitions anywhere in the comment-free source.
fn collect_macros(clean: &str) -> BTreeMap<String, String> {
    let mut macros = BTreeMap::new();
    for caps in newcommand_re().captures_iter(clean) {
        if macros.len() >= MAX_MACROS {
            break;
        }
        let Some(whole) = caps.get(0) else { continue };
        let open = whole.end() - 1;
        let Some(close) = matching_close(clean, open) else {
            continue;
        };
        let expansion = &clean[open + 1..close];
        if expansion.contains('#') {
            continue;
        }
        macros.insert(caps[1].to_owned(), expansion.to_owned());
    }
    macros
}

/// Replace every `\name` that has a collected definition (one level).
fn expand_macros(body: &str, macros: &BTreeMap<String, String>) -> String {
    if macros.is_empty() {
        return body.to_owned();
    }
    let mut out = String::with_capacity(body.len());
    let mut last = 0;
    for caps in command_re().captures_iter(body) {
        let Some(whole) = caps.get(0) else { continue };
        let Some(expansion) = macros.get(&caps[1]) else {
            continue;
        };
        out.push_str(&body[last..whole.start()]);
        out.push_str(expansion);
        last = whole.end();
    }
    out.push_str(&body[last..]);
    out
}

// ---------------------------------------------------------------------------
// Paper metadata: title, authors, DOI
// ---------------------------------------------------------------------------

/// A command rewrite: `(name, brace arguments consumed, argument kept)`.
type Rewrite = (&'static str, usize, Option<usize>);

/// Commands removed from a title before detexing (footnotes, spacing,
/// graphics); `\texorpdfstring` keeps its `TeX` argument and `\raisebox`
/// its content.
const TITLE_REWRITES: &[Rewrite] = &[
    ("thanks", 1, None),
    ("footnote", 1, None),
    ("footnotemark", 0, None),
    ("tnoteref", 1, None),
    ("texorpdfstring", 2, Some(0)),
    ("raisebox", 2, Some(1)),
    ("vspace", 1, None),
    ("hspace", 1, None),
    ("orcidlink", 1, None),
    ("includegraphics", 1, None),
];

/// Commands removed from an author block: footnotes, affiliations, emails,
/// identifiers and superscript markers.
const AUTHOR_REWRITES: &[Rewrite] = &[
    ("thanks", 1, None),
    ("footnote", 1, None),
    ("footnotemark", 0, None),
    ("affiliation", 1, None),
    ("affil", 1, None),
    ("institution", 1, None),
    ("institute", 1, None),
    ("inst", 1, None),
    ("address", 1, None),
    ("textsuperscript", 1, None),
    ("email", 1, None),
    ("ead", 1, None),
    ("texttt", 1, None),
    ("url", 1, None),
    ("href", 2, None),
    ("orcid", 1, None),
    ("orcidID", 1, None),
    ("orcidlink", 1, None),
    ("IEEEauthorblockA", 1, None),
    ("IEEEauthorrefmark", 1, None),
    ("IEEEmembership", 1, None),
    ("authornote", 1, None),
    ("authornotemark", 0, None),
    ("corref", 1, None),
    ("cortext", 1, None),
    ("fnref", 1, None),
    ("fntext", 1, None),
    ("tnoteref", 1, None),
    ("icmlaffiliation", 2, None),
];

/// Lower-case surname particles allowed inside a person name.
const NAME_PARTICLES: &[&str] = &[
    "van", "von", "der", "den", "de", "del", "della", "di", "da", "dos", "du", "le", "la", "bin",
    "al", "ter", "y",
];

/// Words that mark an affiliation, place or note rather than a person.
const NON_NAME_WORDS: &[&str] = &[
    "university",
    "universität",
    "université",
    "universidad",
    "universiteit",
    "institute",
    "institut",
    "department",
    "dept",
    "school",
    "college",
    "laboratory",
    "laboratories",
    "lab",
    "labs",
    "research",
    "center",
    "centre",
    "faculty",
    "inc",
    "ltd",
    "corporation",
    "corp",
    "company",
    "group",
    "academy",
    "hospital",
    "foundation",
    "national",
    "science",
    "sciences",
    "engineering",
    "technology",
    "program",
    "team",
    "google",
    "microsoft",
    "meta",
    "amazon",
    "deepmind",
    "openai",
    "nvidia",
    "street",
    "road",
    "avenue",
    "campus",
    "equal",
    "contribution",
    "corresponding",
    "author",
    "authors",
    "anonymous",
];

/// `{2em}`, `{0.85\textwidth}`, `{-1.7\height}`: a brace group holding only a length.
fn dimension_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"\{\s*-?(?:\d*\.?\d+\s*(?:em|ex|pt|cm|mm|in|bp)|(?:\d*\.?\d+\s*)?\\(?:textwidth|linewidth|columnwidth|textheight|height|width|baselineskip))\s*\}",
        )
        .expect("valid regex")
    })
}

/// `\and`, `\AND`, `\And`: a boundary between authors.
fn author_and_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\\(?:and|AND|And)\b").expect("valid regex"))
}

/// `\\`, `\\*`, `\\[2pt]`, `\newline`, `\par`: a line break inside an author block.
fn author_line_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\\\\\*?(?:\s*\[[^\]]*\])?|\\(?:newline|linebreak|par)\b").expect("valid regex")
    })
}

/// Horizontal gaps that separate names on one line.
fn author_gap_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\\(?:qquad|quad|hfill|enspace)\b|\\hspace\*?\s*\{[^}]*\}")
            .expect("valid regex")
    })
}

/// A DOI after `doi:` or in a `doi.org/` URL.
fn doi_label_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?i)(?:\bdoi\s*:\s*|doi\.org/)(10\.\d{4,9}/[^\s"<>{}]+)"#)
            .expect("valid regex")
    })
}

/// Index after any `[...]` optional arguments starting at or after `i`.
fn skip_optional(s: &str, mut i: usize) -> usize {
    loop {
        let j = skip_ws(s, i);
        if s.as_bytes().get(j) != Some(&b'[') {
            return i;
        }
        let Some(close) = matching_close(s, j) else {
            return i;
        };
        i = close + 1;
    }
}

/// Inner range of brace argument `index` (0-based, after any optional
/// arguments) of every `\name` command in `s`.
fn command_args(s: &str, name: &str, index: usize) -> Vec<Range<usize>> {
    let mut found = Vec::new();
    for caps in command_re().captures_iter(s) {
        let Some(whole) = caps.get(0) else { continue };
        if &caps[1] != name {
            continue;
        }
        let mut i = skip_optional(s, whole.end());
        for n in 0..=index {
            let Some((inner, past_group)) = brace_group(s, skip_ws(s, i)) else {
                break;
            };
            if n == index {
                found.push(inner);
            }
            i = past_group;
        }
    }
    found
}

/// Remove every `\name` with its optional arguments and `args` brace groups;
/// when `keep` is `Some(k)` the contents of group `k` stay, in braces.
fn rewrite_command(s: &str, name: &str, args: usize, keep: Option<usize>) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last = 0;
    for caps in command_re().captures_iter(s) {
        let Some(whole) = caps.get(0) else { continue };
        if whole.start() < last || &caps[1] != name {
            continue;
        }
        let mut i = skip_optional(s, whole.end());
        let mut retained: Option<Range<usize>> = None;
        for n in 0..args {
            let Some((inner, past_group)) = brace_group(s, skip_ws(s, i)) else {
                break;
            };
            if keep == Some(n) {
                retained = Some(inner);
            }
            i = past_group;
        }
        out.push_str(&s[last..whole.start()]);
        if let Some(range) = retained {
            out.push('{');
            out.push_str(&s[range]);
            out.push('}');
        }
        out.push(' ');
        last = i;
    }
    out.push_str(&s[last..]);
    out
}

fn apply_rewrites(s: &str, rewrites: &[Rewrite]) -> String {
    let mut text = s.to_owned();
    for &(name, args, keep) in rewrites {
        text = rewrite_command(&text, name, args, keep);
    }
    text
}

/// Remove `$...$` inline math (affiliation markers such as `$^{1,2}$`).
fn strip_inline_math(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_math = false;
    let mut i = 0;
    while let Some(c) = char_at(s, i) {
        let escaped = if c == '\\' { char_at(s, i + 1) } else { None };
        if let Some(escaped) = escaped {
            if !in_math {
                out.push(c);
                out.push(escaped);
            }
            i += 1 + escaped.len_utf8();
            continue;
        }
        if c == '$' {
            in_math = !in_math;
            out.push(' ');
        } else if !in_math {
            out.push(c);
        }
        i += c.len_utf8();
    }
    out
}

/// Comment-free source up to the first sectioning command, bibliography,
/// `\appendix` or `\end{document}` after `\begin{document}` (the whole
/// source when there is none of these), and the part
/// of it before `\begin{abstract}` (where a DOI line may be printed).
fn front_matter(clean: &str) -> (&str, &str) {
    let begin_tag = "\\begin{document}";
    let start = clean.find(begin_tag).map_or(0, |pos| pos + begin_tag.len());
    let mut stop = heading_re()
        .find_at(clean, start)
        .map_or(clean.len(), |m| m.start());
    for tag in [
        "\\begin{thebibliography}",
        "\\bibliography{",
        "\\printbibliography",
        "\\appendix",
        "\\end{document}",
    ] {
        if let Some(pos) = clean[start..].find(tag) {
            stop = stop.min(start + pos);
        }
    }
    let front = &clean[..stop];
    let before_abstract = front
        .find("\\begin{abstract}")
        .map_or(front, |pos| &front[..pos]);
    (front, before_abstract)
}

/// Detexed title from `\title{...}`, else `\icmltitle{...}`.
fn paper_title(front: &str, macros: &BTreeMap<String, String>) -> Option<String> {
    for name in ["title", "icmltitle"] {
        for range in command_args(front, name, 0) {
            let raw = expand_macros(&expand_macros(&front[range], macros), macros);
            let rewritten = apply_rewrites(&raw, TITLE_REWRITES);
            let without_lengths = dimension_re().replace_all(&rewritten, " ");
            let title = latex_to_text(&without_lengths);
            if !title.is_empty() {
                return Some(title);
            }
        }
    }
    None
}

/// A person name made of 2–4 capitalised words (initials and hyphens
/// allowed, lower-case particles such as `van der` inside); `None` for
/// affiliations, places, emails and anything with digits.
fn person_name(piece: &str) -> Option<String> {
    let trimmed = piece.trim_matches(|c: char| !c.is_alphabetic());
    if trimmed.contains(['@', '(', ')', '/', ':', '[', ']']) || trimmed.contains(char::is_numeric) {
        return None;
    }
    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    if !(2..=6).contains(&tokens.len()) {
        return None;
    }
    let mut capitalised = 0_usize;
    for (k, token) in tokens.iter().enumerate() {
        let lower = token.to_lowercase();
        let bare = lower.trim_matches(|c: char| !c.is_alphabetic());
        if NON_NAME_WORDS.contains(&bare) {
            return None;
        }
        let letters = token.chars().filter(|c| c.is_alphabetic()).count();
        let starts_upper = token.chars().next().is_some_and(char::is_uppercase);
        if starts_upper {
            let name_chars = token
                .chars()
                .all(|c| c.is_alphabetic() || matches!(c, '.' | '-' | '\'' | '\u{2019}'));
            let acronym = letters >= 3 && !token.chars().any(char::is_lowercase);
            if !name_chars || acronym {
                return None;
            }
            capitalised += 1;
        } else if k == 0 || k + 1 == tokens.len() || !NAME_PARTICLES.contains(&bare) {
            return None;
        }
    }
    if (2..=4).contains(&capitalised) {
        Some(tokens.join(" "))
    } else {
        None
    }
}

/// Comma, semicolon, ampersand and `and` separated pieces of one detexed line
/// that contain at least one letter.
fn line_pieces(line: &str) -> Vec<String> {
    let joined = and_sep_re().replace_all(line, ",");
    joined
        .split([',', ';', '&', '\u{b7}', '\u{2022}'])
        .map(str::trim)
        .filter(|piece| piece.chars().any(char::is_alphabetic))
        .map(str::to_owned)
        .collect()
}

/// Names in one author (one `\and`-separated piece): the first line with at
/// least one name-looking piece gives its names, and following lines are
/// added only while every piece on them looks like a name, so affiliation,
/// address and email lines after the names are skipped.
fn piece_names(piece: &str, names: &mut Vec<String>) {
    let mut started = false;
    for raw_line in piece.split('\n') {
        let pieces = line_pieces(&latex_to_text(raw_line));
        if pieces.is_empty() {
            continue;
        }
        let found: Vec<String> = pieces
            .iter()
            .map(String::as_str)
            .filter_map(person_name)
            .collect();
        if !started {
            if !found.is_empty() {
                started = true;
                names.extend(found);
            }
        } else if found.len() == pieces.len() {
            names.extend(found);
        } else {
            break;
        }
    }
}

/// Person names in one author block (`\author{...}`, `\name{...}` or
/// `\icmlauthor{...}` argument).
fn block_names(raw: &str, macros: &BTreeMap<String, String>, names: &mut Vec<String>) {
    let expanded = expand_macros(&expand_macros(raw, macros), macros);
    let one_line = expanded.replace(['\n', '\r'], " ");
    let split_ieee = one_line.replace("\\IEEEauthorblockN", "\\and\\IEEEauthorblockN");
    let rewritten = apply_rewrites(&split_ieee, AUTHOR_REWRITES);
    let text = strip_inline_math(&rewritten);
    let text = author_gap_re().replace_all(&text, ",");
    let text = author_line_re().replace_all(&text, "\n");
    let text = author_and_re().replace_all(&text, "\u{1e}");
    for piece in text.split('\u{1e}') {
        piece_names(piece, names);
    }
}

/// Author names from `\icmlauthor{..}{..}` when present, else every
/// `\author[..]{..}` (`article`, `revtex`, `acmart`, `elsarticle`,
/// `IEEEtran` blocks), else `\name{..}`; deduplicated in source order.
fn paper_authors(front: &str, macros: &BTreeMap<String, String>) -> Vec<String> {
    let mut blocks = command_args(front, "icmlauthor", 0);
    if blocks.is_empty() {
        blocks = command_args(front, "author", 0);
    }
    if blocks.is_empty() {
        blocks = command_args(front, "name", 0);
    }
    let mut names: Vec<String> = Vec::new();
    for range in blocks {
        block_names(&front[range], macros, &mut names);
    }
    let mut seen: BTreeSet<String> = BTreeSet::new();
    names.retain(|name| seen.insert(name.clone()));
    names
}

/// DOI from `\doi{..}`, `\acmDOI{..}` or `\DOI{..}`, else a `doi:` or
/// `doi.org/` DOI, all before the abstract (so cited DOIs never count).
fn paper_doi(before_abstract: &str) -> Option<String> {
    for name in ["doi", "acmDOI", "DOI"] {
        for range in command_args(before_abstract, name, 0) {
            if let Some(doi) = find_doi(&before_abstract[range]) {
                return Some(doi);
            }
        }
    }
    doi_label_re()
        .captures(before_abstract)
        .and_then(|caps| caps.get(1))
        .and_then(|m| tidy_doi(m.as_str()))
}

/// `arXiv` id from `\arxiv{..}`, `\arxivid{..}` or `\arXiv{..}` before the abstract.
fn paper_arxiv(front: &str) -> Option<String> {
    for name in ["arxiv", "arxivid", "arXiv"] {
        for range in command_args(front, name, 0) {
            if let Some(id) = arxiv_from_eprint(&front[range]) {
                return Some(id);
            }
        }
    }
    None
}

/// Title, authors and identifiers of a paper from its merged `.tex` source.
///
/// Only the front matter is read: everything before the first sectioning
/// command, bibliography, `\appendix` or `\end{document}` after
/// `\begin{document}`. Zero-argument macros are expanded.
/// The title is the first `\title[..]{..}` (else `\icmltitle{..}`) with
/// `\thanks`, `\footnote`, spacing, graphics and length arguments removed,
/// then detexed. Authors come from `\icmlauthor{name}{..}`, else every
/// `\author[..]{..}` (split on `\and`, `\AND` and `\IEEEauthorblockN`),
/// else `\name{..}`; affiliation, email, ORCID, footnote and superscript
/// commands and inline math are removed, each author is cut at line breaks
/// (`\\`) and pieces are split on `,`, `;`, `&` and `and`; only pieces that
/// look like person names (2–4 capitalised words) are kept. The DOI comes
/// from `\doi{..}`/`\acmDOI{..}` or a `doi:`/`doi.org/` DOI, and the
/// `arXiv` id only from an explicit `\arxiv{..}` command, both searched
/// before the abstract so a cited work's identifier is never taken.
/// Fields stay empty when the source does not state them.
pub fn paper_truth(main_tex_merged: &str) -> TruthPaper {
    let clean = strip_comments(main_tex_merged);
    let macros = collect_macros(&clean);
    let (front, before_abstract) = front_matter(&clean);
    TruthPaper {
        title: paper_title(front, &macros),
        authors: paper_authors(front, &macros),
        doi: paper_doi(before_abstract),
        arxiv_id: paper_arxiv(before_abstract),
    }
}

// ---------------------------------------------------------------------------
// \input resolution and ground truth
// ---------------------------------------------------------------------------

/// Inline `\input{f}`, `\include{f}` and `\subfile{f}` relative to `root`
/// (`.tex` is appended unless `f` already has that extension), recursively
/// while `depth <= 5`. `\input` also accepts the brace-free plain-`TeX` form
/// (`\input f`, `f` running until whitespace, `{`, `}` or `\`); `\include`
/// and `\subfile` always require braces. Missing files are left as they
/// are. Comments, `comment` environments and `\iffalse ... \fi` blocks are
/// removed from every file before its own inputs are expanded, so nothing
/// inside them is inlined. Cyclic or excessively large expansions produce an
/// empty string; ground-truth generation reports those conditions as errors.
pub fn resolve_inputs(root: &Path, main_tex: &str, depth: u32) -> String {
    resolve_inputs_checked(root, main_tex, depth, None).unwrap_or_default()
}

struct InputState {
    active: HashSet<PathBuf>,
    inputs: usize,
    out: String,
}

fn resolve_inputs_checked(
    root: &Path,
    main_tex: &str,
    depth: u32,
    main_path: Option<&Path>,
) -> Result<String, TruthError> {
    let mut state = InputState {
        active: HashSet::new(),
        inputs: 0,
        out: String::with_capacity(main_tex.len().min(MAX_EXPANDED_BYTES)),
    };
    if let Some(path) = main_path.and_then(|path| path.canonicalize().ok()) {
        state.active.insert(path);
    }
    expand_inputs(root, main_tex, depth, &mut state)?;
    Ok(state.out)
}

fn append_expanded(state: &mut InputState, text: &str) -> Result<(), TruthError> {
    if state
        .out
        .len()
        .checked_add(text.len())
        .is_none_or(|len| len > MAX_EXPANDED_BYTES)
    {
        return Err(TruthError::InputLimit);
    }
    state.out.push_str(text);
    Ok(())
}

fn expand_inputs(
    root: &Path,
    main_tex: &str,
    depth: u32,
    state: &mut InputState,
) -> Result<(), TruthError> {
    let clean = remove_disabled(&strip_comments(main_tex));
    if depth > MAX_INPUT_DEPTH {
        return append_expanded(state, &clean);
    }
    let mut last = 0;
    for caps in input_re().captures_iter(&clean) {
        let Some(whole) = caps.get(0) else { continue };
        state.inputs += 1;
        if state.inputs > MAX_INPUTS {
            return Err(TruthError::InputLimit);
        }
        let raw = caps.get(1).or(caps.get(2)).map_or("", |m| m.as_str());
        let name = raw.trim().trim_matches('"');
        let Some((path, content)) = read_input(root, name) else {
            continue;
        };
        let canonical = path.canonicalize().unwrap_or(path);
        if !state.active.insert(canonical.clone()) {
            return Err(TruthError::CyclicInput(canonical));
        }
        append_expanded(state, &clean[last..whole.start()])?;
        append_expanded(state, "\n")?;
        let result = expand_inputs(root, &content, depth + 1, state);
        state.active.remove(&canonical);
        result?;
        append_expanded(state, "\n")?;
        last = whole.end();
    }
    append_expanded(state, &clean[last..])
}

/// Resolves an `\input`/`\include`/`\subfile` argument to file bytes under
/// `root`. When `name` already has a `.tex` extension it is used as written
/// (no second `.tex` is appended); otherwise `<name>.tex` is tried first by
/// plain string concatenation (so a dotted basename like `3.1_method` is
/// never mistaken for an extension), then `name` as written, then
/// `Path::with_extension("tex")` as a last resort.
fn read_input(root: &Path, name: &str) -> Option<(PathBuf, String)> {
    if name.is_empty() || Path::new(name).is_absolute() || name.contains("..") {
        return None;
    }
    let has_tex_extension = Path::new(name)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("tex"));
    let candidates: Vec<PathBuf> = if has_tex_extension {
        vec![root.join(name)]
    } else {
        vec![
            root.join(format!("{name}.tex")),
            root.join(name),
            root.join(name).with_extension("tex"),
        ]
    };
    candidates.into_iter().find_map(|path| {
        fs::read(&path)
            .ok()
            .map(|bytes| (path, String::from_utf8_lossy(&bytes).into_owned()))
    })
}

fn read_lossy(path: &Path) -> Result<String, TruthError> {
    Ok(String::from_utf8_lossy(&fs::read(path)?).into_owned())
}

/// Read a source only after its on-disk size has been checked, so rejecting
/// an oversized source does not first allocate the oversized buffer.
fn read_lossy_bounded(path: &Path, budget: usize) -> Result<String, TruthError> {
    if usize::try_from(fs::metadata(path)?.len()).map_or(true, |len| len > budget) {
        return Err(TruthError::ResourceLimit);
    }
    let text = read_lossy(path)?;
    if text.len() > budget {
        return Err(TruthError::ResourceLimit);
    }
    Ok(text)
}

/// Keep the first entry per key (keys compare case-insensitively, as `BibTeX` does).
fn dedupe_keys(entries: Vec<TruthReference>) -> Vec<TruthReference> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    entries
        .into_iter()
        .filter(|entry| seen.insert(entry.key.to_ascii_lowercase()))
        .collect()
}

/// Every entry of the `.bib` files.
fn read_bib_entries(files: &LatexFiles) -> Result<Vec<TruthReference>, TruthError> {
    let mut entries = Vec::new();
    for path in &files.bib {
        entries.extend(parse_bib(&read_lossy(path)?));
    }
    Ok(entries)
}

/// Lower-cased cited and `\nocite`d keys.
fn wanted_keys(citations: &TruthCitations) -> BTreeSet<String> {
    citations
        .cited_keys
        .iter()
        .chain(&citations.nocite_keys)
        .map(|k| k.to_ascii_lowercase())
        .collect()
}

/// Whether a `.bbl` set misses a whole bibliography: the source declares
/// extra bibliographies with `multibib`'s `\newcites`, or the `.bbl` keys
/// cover fewer than half of the distinct cited keys. Keys missing from a
/// complete `.bbl` are otherwise left out, because the shipped `.bbl` is what
/// the PDF was built from (such a key prints as `[?]` with no entry).
fn bbl_is_partial(merged: &str, wanted: &BTreeSet<String>, bbl: &[TruthReference]) -> bool {
    if wanted.is_empty() {
        return false;
    }
    let have: BTreeSet<String> = bbl.iter().map(|r| r.key.to_ascii_lowercase()).collect();
    let covered = wanted.iter().filter(|k| have.contains(*k)).count();
    merged.contains("\\newcites") || covered * 2 < wanted.len()
}

/// Whether a `.bbl` set of a `\nocite{*}` source misses a whole
/// bibliography. With `\nocite{*}` the wanted keys say nothing (every `.bib`
/// entry is printed), so the test is the source declaring extra
/// bibliographies with `multibib`'s `\newcites`, or the `.bbl` keys covering
/// fewer than half of the distinct `.bib` keys.
fn bbl_is_partial_for_all(merged: &str, bib: &[TruthReference], bbl: &[TruthReference]) -> bool {
    let keys: BTreeSet<String> = bib.iter().map(|r| r.key.to_ascii_lowercase()).collect();
    if keys.is_empty() {
        return false;
    }
    let have: BTreeSet<String> = bbl.iter().map(|r| r.key.to_ascii_lowercase()).collect();
    let covered = keys.iter().filter(|k| have.contains(*k)).count();
    merged.contains("\\newcites") || covered * 2 < keys.len()
}

/// `.bib` entries that no `.bbl` entry has, restricted to `wanted` keys when
/// given (`None` keeps every such entry, for `\nocite{*}`).
fn missing_from_bbl(
    entries: Vec<TruthReference>,
    wanted: Option<&BTreeSet<String>>,
    bbl: &[TruthReference],
) -> Vec<TruthReference> {
    let have: BTreeSet<String> = bbl.iter().map(|r| r.key.to_ascii_lowercase()).collect();
    let extra: Vec<TruthReference> = entries
        .into_iter()
        .filter(|entry| {
            let key = entry.key.to_ascii_lowercase();
            wanted.is_none_or(|keys| keys.contains(&key)) && !have.contains(&key)
        })
        .collect();
    dedupe_keys(extra)
}

/// `.bib` entries to append to a `.bbl` set that misses a whole
/// bibliography: every `.bib` entry no `.bbl` has when `\nocite{*}` is
/// present (see `bbl_is_partial_for_all`), else the wanted ones (see
/// `bbl_is_partial`). Empty when the `.bbl` set looks complete.
fn bib_extras(
    files: &LatexFiles,
    merged: &str,
    citations: &TruthCitations,
    wanted: &BTreeSet<String>,
    bbl: &[TruthReference],
) -> Result<Vec<TruthReference>, TruthError> {
    if !citations.nocite_all && !bbl_is_partial(merged, wanted, bbl) {
        return Ok(Vec::new());
    }
    let entries = read_bib_entries(files)?;
    let extra = if citations.nocite_all && bbl_is_partial_for_all(merged, &entries, bbl) {
        missing_from_bbl(entries, None, bbl)
    } else if bbl_is_partial(merged, wanted, bbl) {
        missing_from_bbl(entries, Some(wanted), bbl)
    } else {
        Vec::new()
    };
    Ok(extra)
}

/// `arXiv`'s processing manifest at the root of a source tree.
const README_JSON: &str = "00README.json";

const BEGIN_DOCUMENT: &str = "\\begin{document}";

/// The `toplevel` sources that `00README.json` at `root` declares, in file
/// order, as distinct paths among the discovered `.tex` files. Empty when
/// the manifest is absent or unreadable; unsafe and excess names are skipped.
fn readme_toplevels(files: &LatexFiles) -> Vec<PathBuf> {
    let Ok(bytes) = fs::read(files.root.join(README_JSON)) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return Vec::new();
    };
    let Some(sources) = value.get("sources").and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };
    let discovered: BTreeSet<PathBuf> = files.tex.iter().cloned().collect();
    let mut seen = BTreeSet::new();
    let mut paths = Vec::new();
    for name in sources
        .iter()
        .filter(|source| {
            source.get("usage").and_then(serde_json::Value::as_str) == Some("toplevel")
        })
        .filter_map(|source| source.get("filename").and_then(serde_json::Value::as_str))
    {
        let mut relative = PathBuf::new();
        for component in Path::new(name).components() {
            match component {
                Component::Normal(part) => relative.push(part),
                Component::CurDir => {}
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    relative.clear();
                    break;
                }
            }
        }
        if relative.as_os_str().is_empty() {
            continue;
        }
        let path = files.root.join(relative);
        if discovered.contains(&path) && seen.insert(path.clone()) {
            paths.push(path);
            if paths.len() == MAX_TOPLEVEL_DOCUMENTS {
                break;
            }
        }
    }
    paths
}

/// The documents the PDF is built from, each as (path, source text): every
/// `00README.json` `toplevel` file that contains `\begin{document}` (`arXiv`
/// typesets them one after another into one PDF), else the first `.tex`
/// that contains `\begin{document}`.
fn main_documents(files: &LatexFiles) -> Result<Vec<(PathBuf, String)>, TruthError> {
    let mut docs: Vec<(PathBuf, String)> = Vec::new();
    let mut source_bytes = 0usize;
    for path in readme_toplevels(files) {
        let text = match read_lossy_bounded(&path, MAX_TOPLEVEL_SOURCE_BYTES - source_bytes) {
            Ok(text) => text,
            Err(TruthError::ResourceLimit) => return Err(TruthError::ResourceLimit),
            Err(_) => continue,
        };
        source_bytes = source_bytes
            .checked_add(text.len())
            .filter(|size| *size <= MAX_TOPLEVEL_SOURCE_BYTES)
            .ok_or(TruthError::ResourceLimit)?;
        if strip_comments(&text).contains(BEGIN_DOCUMENT) {
            docs.push((path, text));
        }
    }
    if !docs.is_empty() {
        return Ok(docs);
    }
    for path in &files.tex {
        let text = read_lossy_bounded(path, MAX_TOPLEVEL_SOURCE_BYTES)?;
        if strip_comments(&text).contains(BEGIN_DOCUMENT) {
            return Ok(vec![(path.clone(), text)]);
        }
    }
    Err(TruthError::NoMainTex)
}

fn ground_truth_bytes(truth: &GroundTruth) -> usize {
    let reference_bytes: usize = truth
        .references
        .iter()
        .map(|reference| {
            reference.key.len()
                + reference.label.as_ref().map_or(0, String::len)
                + reference.text.len()
                + reference.authors.iter().map(String::len).sum::<usize>()
                + reference.title.as_ref().map_or(0, String::len)
                + reference.doi.as_ref().map_or(0, String::len)
                + reference.arxiv_id.as_ref().map_or(0, String::len)
        })
        .sum();
    reference_bytes
        + truth.paper.title.as_ref().map_or(0, String::len)
        + truth.paper.authors.iter().map(String::len).sum::<usize>()
        + truth.paper.doi.as_ref().map_or(0, String::len)
        + truth.paper.arxiv_id.as_ref().map_or(0, String::len)
        + truth.body_text.len()
        + truth
            .citations
            .cited_keys
            .iter()
            .map(String::len)
            .sum::<usize>()
        + truth
            .citations
            .nocite_keys
            .iter()
            .map(String::len)
            .sum::<usize>()
}

/// `bibunits` `.bbl` files (`bu1.bbl`, `bu2.bbl`, ..., `bu10.bbl`) in unit
/// number order.
fn bibunit_bbls(bbl: &[PathBuf]) -> Vec<PathBuf> {
    let mut units: Vec<(u32, PathBuf)> = bbl
        .iter()
        .filter_map(|path| {
            let stem = path.file_stem()?.to_str()?;
            let digits = stem.strip_prefix("bu")?;
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let number = digits.parse::<u32>().ok()?;
            Some((number, path.clone()))
        })
        .collect();
    units.sort();
    units.into_iter().map(|(_, path)| path).collect()
}

/// The `.bbl` files one document typesets. With `bibunits` (`\putbib` in
/// the merged source) and unit files present, the unit files `bu<n>.bbl` in
/// unit order: any other `.bbl` is a stale whole-document one that is never
/// printed. When several toplevel documents share the tree, only the `.bbl`
/// named after the document. Otherwise every `.bbl`.
fn document_bbls(
    files: &LatexFiles,
    merged: &str,
    main_path: &Path,
    several: bool,
) -> Vec<PathBuf> {
    if merged.contains("\\putbib") {
        let units = bibunit_bbls(&files.bbl);
        if !units.is_empty() {
            return units;
        }
    }
    if several {
        let stem = main_path.file_stem();
        return files
            .bbl
            .iter()
            .filter(|path| path.file_stem() == stem)
            .cloned()
            .collect();
    }
    files.bbl.clone()
}

/// Ground truth for one toplevel document (see [`ground_truth`]); `several`
/// says whether other toplevel documents share the source tree.
fn document_truth(
    files: &LatexFiles,
    main_path: &Path,
    main_text: &str,
    several: bool,
    budget: usize,
) -> Result<GroundTruth, TruthError> {
    // Preflight every bibliography input that can contribute to this
    // document. Parsing copies fields into multiple owned strings, so this
    // check must happen before reading and parsing an oversized file.
    let mut input_bytes = main_text.len();
    for path in document_bbls(files, main_text, main_path, several)
        .iter()
        .chain(&files.bib)
    {
        let len = usize::try_from(fs::metadata(path)?.len()).unwrap_or(usize::MAX);
        input_bytes = input_bytes
            .checked_add(len)
            .filter(|size| *size <= budget)
            .ok_or(TruthError::ResourceLimit)?;
    }
    let root: &Path = main_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(files.root.as_path());
    let merged = resolve_inputs_checked(root, main_text, 0, Some(main_path))?;
    let citations = parse_cites(&merged);
    let body = body_text(&merged);
    let paper = paper_truth(&merged);
    let wanted = wanted_keys(&citations);

    let mut references = Vec::new();
    for path in document_bbls(files, &merged, main_path, several) {
        references.extend(parse_bbl(&read_lossy(&path)?));
    }
    let inline = if references.is_empty() {
        merged.find("\\begin{thebibliography}")
    } else {
        None
    };
    if let Some(start) = inline {
        references = parse_bbl(&merged[start..]);
    }
    let method = if references.is_empty() {
        let entries = read_bib_entries(files)?;
        if entries.is_empty() {
            return Err(TruthError::NoBibliography);
        }
        if citations.nocite_all {
            references = dedupe_keys(entries);
            "bib-all"
        } else {
            let cited_entries: Vec<TruthReference> = entries
                .into_iter()
                .filter(|entry| wanted.contains(&entry.key.to_ascii_lowercase()))
                .collect();
            references = dedupe_keys(cited_entries);
            "bib-cited"
        }
    } else if files.bib.is_empty() {
        "bbl"
    } else {
        let extra = bib_extras(files, &merged, &citations, &wanted, &references)?;
        if extra.is_empty() {
            "bbl"
        } else {
            references.extend(extra);
            "bbl+bib"
        }
    };
    let truth = GroundTruth {
        references,
        citations,
        method: method.to_owned(),
        body_text: body,
        paper,
    };
    if ground_truth_bytes(&truth) > budget {
        return Err(TruthError::ResourceLimit);
    }
    Ok(truth)
}

/// Append a later toplevel document's truth to `first`. References and
/// citations are concatenated (each document prints its own list, so a work
/// cited by both appears twice), body texts are joined by a blank line, the
/// paper metadata stays the first document's, and a method that differs
/// from those so far is appended after a `,`.
fn append_document(mut first: GroundTruth, next: GroundTruth) -> GroundTruth {
    first.references.extend(next.references);
    let cites = &mut first.citations;
    cites.cite_commands += next.citations.cite_commands;
    cites.cite_only_author_year += next.citations.cite_only_author_year;
    cites.cited_keys.extend(next.citations.cited_keys);
    cites.nocite_keys.extend(next.citations.nocite_keys);
    cites.nocite_all |= next.citations.nocite_all;
    if !next.body_text.is_empty() {
        if !first.body_text.is_empty() {
            first.body_text.push_str("\n\n");
        }
        first.body_text.push_str(&next.body_text);
    }
    if !first.method.split(',').any(|method| method == next.method) {
        first.method.push(',');
        first.method.push_str(&next.method);
    }
    first
}

/// Ground truth for one paper's source tree.
///
/// The main files are the `toplevel` sources of `00README.json` that contain
/// `\begin{document}` (`arXiv` typesets several of them, such as a paper and
/// its supporting information, into one PDF), else the first `.tex`
/// containing `\begin{document}` ([`TruthError::NoMainTex`] otherwise).
/// Each main file is handled on its own and the results are concatenated
/// in manifest order (see `append_document`); the paper metadata comes
/// from the first. `\input`s are resolved relative to the main file's
/// directory, after `%` comments, `comment` environments and
/// `\iffalse ... \fi` blocks are removed from each file. Citations and body
/// text come from the merged source. The bibliography is, in order of
/// preference: the `.bbl` files (method `bbl`; see `document_bbls` for
/// `bibunits` and several main files), a `thebibliography` environment
/// inline in the source (also `bbl`), or the `.bib` files filtered to cited
/// and `\nocite`d keys (`bib-cited`), or every `.bib` entry when
/// `\nocite{*}` is present (`bib-all`). When the `.bbl` files or the inline
/// list miss a whole bibliography (`multibib` shipping only `app.bbl`, or an
/// appendix `thebibliography` next to a `\bibliography{..}` with no `.bbl`;
/// see `bbl_is_partial`), the `.bib` entries of cited keys they lack are
/// appended after them (method `bbl+bib`); with `\nocite{*}` that is every
/// `.bib` entry they lack (see `bbl_is_partial_for_all`).
/// [`TruthError::NoBibliography`] when none of these yields an entry (with
/// several main files, when none of them does).
pub fn ground_truth(files: &LatexFiles) -> Result<GroundTruth, TruthError> {
    let docs = main_documents(files)?;
    let several = docs.len() > 1;
    let mut combined: Option<GroundTruth> = None;
    for (path, text) in &docs {
        let accumulated = combined.as_ref().map_or(0, ground_truth_bytes);
        let remaining = MAX_GROUND_TRUTH_BYTES
            .checked_sub(accumulated)
            .ok_or(TruthError::ResourceLimit)?;
        let truth = match document_truth(files, path, text, several, remaining) {
            Ok(truth) => truth,
            Err(TruthError::NoBibliography) if several => continue,
            Err(err) => return Err(err),
        };
        if accumulated
            .checked_add(ground_truth_bytes(&truth))
            .is_none_or(|size| size > MAX_GROUND_TRUTH_BYTES)
        {
            return Err(TruthError::ResourceLimit);
        }
        combined = Some(match combined {
            None => truth,
            Some(so_far) => append_document(so_far, truth),
        });
    }
    combined.ok_or(TruthError::NoBibliography)
}

/// Compile every regex this module uses, so the first document does not pay
/// for it inside its stage timings. Repeated calls are cheap.
pub fn warm_up() {
    let accessors: &[fn() -> &'static Regex] = &[
        command_re,
        bibitem_re,
        newblock_re,
        acm_title_re,
        year_re,
        iso_date_re,
        italic_re,
        year_mask_re,
        doi_re,
        doi_url_re,
        arxiv_re,
        eprint_re,
        arxiv_id_re,
        and_re,
        and_sep_re,
        initials_re,
        trailing_year_re,
        author_year_title_re,
        series_suffix_re,
        input_re,
        begin_re,
        verbatim_env_def_re,
        heading_re,
        par_re,
        blank_line_re,
        newcommand_re,
        entry_re,
        field_re,
        verb_re,
        name_part_re,
        abstract_start_re,
        dimension_re,
        author_and_re,
        author_line_re,
        author_gap_re,
        doi_label_re,
    ];
    for accessor in accessors {
        accessor();
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::*;

    const BBL: &str = r#"\begin{thebibliography}{10}

\bibitem[{Aamand et~al.(2021)Aamand, Abrahamsen, and {Rasmussen}}]{aamand2021classifying}
Anders Aamand, Mikkel Abrahamsen, and Peter Michael~Reichstein Rasmussen.
\newblock Classifying convex bodies by their contact and intersection graphs.
\newblock In {\em 37th International Symposium on Computational Geometry (SoCG
  2021)}, pages 3:1--3:16, 2021.
\newblock \href {https://doi.org/10.4230/LIPIcs.SoCG.2021.3}
  {\path{doi:10.4230/LIPIcs.SoCG.2021.3}}.

\bibitem[\protect\citeauthoryear{Alber and Fiala}{2004}]{alber2004geometric}
Jochen Alber and Ji\v{r}\'{\i} Fiala.
\newblock Geometric separation and exact solutions for the parameterized
  independent set problem on disk graphs.
\newblock {\em Journal of Algorithms}, 52(2):134--151, 2004.

\bibitem{fekete1923verteilung}
Mih\'aly Fekete.
\newblock \"Uber die {V}erteilung der {W}urzeln bei gewissen algebraischen
  {G}leichungen mit ganzzahligen {K}oeffizienten.
\newblock {\em Mathematische Zeitschrift}, 17:228--249, 1923.
\newblock arXiv:2101.00001.

\end{thebibliography}
"#;

    const BIB: &str = r#"@string{acl = "Association for Computational Linguistics"}
@comment{ignored @article{nope, title={x}} }
@Article{DBLP:journals/ftir/MaviJJ24,
  author       = {Vaibhav Mavi and
                  Anubhav Jangra and
                  Adam Jatowt},
  title        = {Multi-hop Question Answering},
  journal      = {Found. Trends Inf. Retr.},
  year         = {2024},
  doi          = {10.1561/1500000102},
}

@inproceedings{karpukhin-etal-2020-dense,
    title = "Dense Passage Retrieval for {Open-Domain} Question Answering",
    author = "Karpukhin, Vladimir  and
      Oguz, Barlas",
    booktitle = "Proceedings of " # "EMNLP",
    month = nov,
    year = "2020",
    url = "https://doi.org/10.18653/v1/2020.emnlp-main.550",
    pages = "6769--6781"
}

@ARTICLE{2024arXiv240608394W,
       author = {{Wu}, Jiannan and {Zhong}, Muyan},
        title = "{VisionLLM v2: An End-to-End Generalist {LLM}}",
      journal = {arXiv e-prints},
         year = 2024,
        month = jun,
archivePrefix = {arXiv},
       eprint = {2406.08394},
 primaryClass = {cs.CV},
}

@misc{noyear,
  title = {Untitled draft},
  author = {Doe, John and others},
  journal = {arXiv preprint arXiv:1704.05021}
}
"#;

    const AUTHOR_YEAR_BBL: &str = r"\bibitem[Aamand et~al., 2021]{k}
Aamand, A., Abrahamsen, M., and Rasmussen, P.~M. (2021).
\newblock Title here.
\newblock Venue.
";

    const BIBLATEX_BBL: &str = r"\entry{doe2020}{article}{}
  \name{author}{2}{}{%
    {{hash=1}{%
       family={Doe},
       familyi={D\bibinitperiod},
       given={Jane},
       giveni={J\bibinitperiod}}}%
    {{hash=2}{%
       family={Roe},
       given={Richard}}}%
  }
  \field{journaltitle}{Nature}
  \field{title}{A {Study}}
  \field{year}{2020}
  \verb{doi}
  \verb 10.1000/xyz
  \endverb
  \field{eprinttype}{arXiv}
  \verb{eprint}
  \verb 2001.00002
  \endverb
\endentry
\entry{second}{book}{}
  \field{title}{Second}
\endentry
";

    const CITES_TEX: &str = r"Intro~\citep[see][p. 3]{a,b} and \citet{c}.
% \cite{commented}
\nocite{*}\nocite{d, e}
\citestyle{x}\citeauthor*{f}
\cite
{g}";

    const BODY_TEX: &str = r"\documentclass{article}
\newcommand{\method}{FooNet}
\begin{document}
\maketitle
\section{Introduction}\label{sec:intro}
We present \method{} here~\cite{a}.

\begin{figure}[t]
\includegraphics{x.png}
\caption{Dropped caption}
\end{figure}

Second paragraph with $x^2$.
\[ E = mc^2 \]
\bibliography{refs}
\end{document}
Trailing";

    const MAIN_TEX: &str = r"\documentclass{article}
\begin{document}
\section{Intro}
See \cite{Alpha} and \cite{gamma}.
\bibliography{refs}
\end{document}
";

    const TYPESET_TEX: &str = r"\documentclass{article}
\begin{document}
See \cite{aamand2021classifying,alber2004geometric} and \cite{alpha}.
\bibliography{refs}
\end{document}
";

    const MULTIBIB_TEX: &str = r"\documentclass{llncs}
\usepackage{multibib}
\newcites{app}{References for the Appendices}
\newcommand{\citeboth}[1]{\cite{#1}}
\begin{document}
Main text \cite{alpha, gamma} and \cite{Alpha}.
\bibliographystyle{splncs04}
\bibliography{refs}
\appendix
Appendix \citeapp{aamand2021classifying} and \citeapp[p.~3]{alber2004geometric}.
As \citeauthor{gamma} showed in \citeyear{gamma}.
\citestyle{plain}\citeindextrue
\bibliographystyleapp{splncs04}
\bibliographyapp{refs}
\end{document}
";

    const REFS_BIB: &str = r"@article{alpha, title={A}, year={2001}}
@article{beta, title={B}, year={2002}}
@article{gamma, title={G}, year={2003}}
";

    const NOCITE_MULTIBIB_TEX: &str = r"\documentclass{article}
\usepackage{multibib}
\newcites{app}{References for the Appendices}
\begin{document}
Main text.
\nocite{*}
\bibliographystyle{plain}
\bibliography{refs}
\appendix
Appendix text.
\nociteapp{*}
\bibliographystyleapp{plain}
\bibliographyapp{app}
\end{document}
";

    const APP_BBL: &str = r"\begin{thebibliography}{2}
\bibitem{app1} Ann Appendix.
\newblock First appendix paper.
\newblock Venue, 2011.
\bibitem{app2} Bob Appendix.
\newblock Second appendix paper.
\newblock Venue, 2012.
\end{thebibliography}
";

    const MAIN_BBL: &str = r"\begin{thebibliography}{4}
\bibitem{m1} Author One.
\newblock Main one.
\newblock Venue, 2001.
\bibitem{m2} Author Two.
\newblock Main two.
\newblock Venue, 2002.
\bibitem{m3} Author Three.
\newblock Main three.
\newblock Venue, 2003.
\bibitem{m4} Author Four.
\newblock Main four.
\newblock Venue, 2004.
\end{thebibliography}
";

    const MAIN_REFS_BIB: &str = r"@article{m1, title={Main one}, year={2001}}
@article{m2, title={Main two}, year={2002}}
@article{m3, title={Main three}, year={2003}}
@article{m4, title={Main four}, year={2004}}
";

    const NOCITE_TEX: &str = r"\begin{document}
\nocite{*}
\end{document}
";

    const DUP_BIB: &str = r"@article{a, title={A}}
@article{A, title={dup}}
@article{b, title={B}}
";

    const INLINE_TEX: &str = r"\begin{document}
Text \cite{k1}.
\begin{thebibliography}{9}
\bibitem{k1} Some Author.
\newblock A title.
\newblock Venue, 1999.
\end{thebibliography}
\end{document}
";

    fn files(dir: &Path, tex: &[&str], bbl: &[&str], bib: &[&str]) -> LatexFiles {
        let paths = |names: &[&str]| names.iter().map(|n| dir.join(n)).collect::<Vec<_>>();
        LatexFiles {
            root: dir.to_path_buf(),
            tex: paths(tex),
            bbl: paths(bbl),
            bib: paths(bib),
        }
    }

    #[test]
    fn matching_close_handles_nesting_and_escapes() {
        assert_eq!(matching_close(r"{a{b}\}c}d", 0), Some(8));
        assert_eq!(matching_close("[x{]}y]", 0), Some(6));
        assert_eq!(matching_close("(a{b)}c)", 0), Some(7));
        assert_eq!(matching_close("{unbalanced", 0), None);
        assert_eq!(matching_close("abc", 0), None);
        assert_eq!(matching_close("{}", 5), None);
    }

    #[test]
    fn latex_to_text_table() {
        let cases: [(&str, &str); 21] = [
            (r#"\'e \`a \^o \"u \~n"#, "é à ô ü ñ"),
            (
                r"\c{c} \v{s} \H{o} \k{a} \={e} \.{z} \u{g} \r{a}",
                "ç š ő ą ē ż ğ å",
            ),
            (r#"\'{e} \"{\i} \v{c} \'\i"#, "é ï č í"),
            (
                r"Wei\ss enfels Bj\o rn \O{} \ae{} \AE{} \aa{} \AA{} \l{} \L{}",
                "Weißenfels Bjørn Ø æ Æ å Å ł Ł",
            ),
            (
                r"\emph{Deep} \textit{learning} \textbf{b} \textsc{c} \texttt{d} \mbox{e} \text{f}",
                "Deep learning b c d e f",
            ),
            (r"\url{https://x.org/a_b}", "https://x.org/a_b"),
            (
                r"\href{https://doi.org/10.1/x}{\path{doi:10.1/x}}",
                "doi:10.1/x",
            ),
            (
                r"{\em Journal of Algorithms}, 52(2):134--151, 2004.",
                "Journal of Algorithms, 52(2):134–151, 2004.",
            ),
            ("``quoted'' --- yes", "\"quoted\" — yes"),
            (r"A \& B 100\% a\_b \$5 \{x\}", "A & B 100% a_b $5 {x}"),
            (r"the {$\log n$} barrier", "the log n barrier"),
            (r"$O(n\log n)$ and $\log(n)$", "O(n log n) and log(n)"),
            (r"$\tau$-bench, Gym-$\mu$RTS", "τ-bench, Gym-μRTS"),
            (
                r"CuO$_{6\pm\delta}$ for $\chi$-separation",
                "CuO_6±δ for χ-separation",
            ),
            (r"$x^2 + y$", "x^2 + y"),
            (
                r"Über die {V}erteilung der {W}urzeln",
                "Über die Verteilung der Wurzeln",
            ),
            ("line % comment\nnext", "line next"),
            (r"a\,b\ c~d", "a b c d"),
            (r"Smith \newblock Title", "Smith Title"),
            (r"\natexlab{a} \protect\label{x}b", "a b"),
            (r"  spaced   out\\ text  ", "spaced out text"),
        ];
        for (input, expected) in cases {
            assert_eq!(latex_to_text(input), expected, "input: {input}");
        }
    }

    #[test]
    fn parse_bbl_natbib_snippet() {
        let refs = parse_bbl(BBL);
        assert_eq!(refs.len(), 3);
        let keys: Vec<&str> = refs.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(
            keys.join(","),
            "aamand2021classifying,alber2004geometric,fekete1923verteilung"
        );
        assert_eq!(
            refs[0].label.as_deref(),
            Some("Aamand et al.(2021)Aamand, Abrahamsen, and Rasmussen")
        );
        assert_eq!(refs[1].label.as_deref(), Some("Alber and Fiala2004"));
        assert_eq!(refs[2].label, None);
        let years: Vec<Option<u16>> = refs.iter().map(|r| r.year).collect();
        assert_eq!(years, [Some(2021), Some(2004), Some(1923)]);
        assert_eq!(refs[0].doi.as_deref(), Some("10.4230/LIPIcs.SoCG.2021.3"));
        assert_eq!(refs[1].doi, None);
        assert_eq!(
            refs[0].title.as_deref(),
            Some("Classifying convex bodies by their contact and intersection graphs")
        );
        let umlaut_title = refs[2].title.as_deref().unwrap();
        assert!(umlaut_title.starts_with("Über die Verteilung der Wurzeln"));
        assert!(umlaut_title.ends_with("mit ganzzahligen Koeffizienten"));
        assert_eq!(
            refs[0].authors.join("; "),
            "Anders Aamand; Mikkel Abrahamsen; Peter Michael Reichstein Rasmussen"
        );
        assert_eq!(refs[1].authors, ["Jochen Alber", "Jiří Fiala"]);
        assert_eq!(refs[2].arxiv_id.as_deref(), Some("2101.00001"));
        assert!(
            refs[1]
                .text
                .starts_with("Jochen Alber and Jiří Fiala. Geometric")
        );
        assert!(refs[0].text.ends_with("LIPIcs.SoCG.2021.3."));
        assert_eq!(refs[0].source, TruthSource::Bbl);
    }

    #[test]
    fn parse_bbl_author_year_initials() {
        let refs = parse_bbl(AUTHOR_YEAR_BBL);
        assert_eq!(refs.len(), 1);
        assert_eq!(
            refs[0].authors,
            ["A. Aamand", "M. Abrahamsen", "P. M. Rasmussen"]
        );
        assert_eq!(refs[0].title.as_deref(), Some("Title here"));
        assert_eq!(refs[0].label.as_deref(), Some("Aamand et al., 2021"));
    }

    /// `spbasic`-like output: authors, year and title share the first
    /// `\newblock` segment (entries from 2602.16061 with `\newblock`s added;
    /// the real file has none, see `INFORMS_PLAIN_BBL`).
    const INFORMS_BBL: &str = r"\begin{thebibliography}{2}
\bibitem[{Gui and Toubia(2023)}]{gui2023challenge}
Gui G, Toubia O (2023) The challenge of using llms to simulate human behavior:
  A causal inference perspective. \newblock \emph{arXiv preprint
  arXiv:2312.15524} .

\bibitem[{Abrevaya and Donald(2017)}]{abrevaya2017gmm}
Abrevaya J, Donald SG (2017) A gmm approach for dealing with missing data on
  regressors. \newblock \emph{Review of Economics and Statistics}
  99(4):657--662.
\end{thebibliography}
";

    #[test]
    fn parse_bbl_informs_title_in_first_block() {
        let refs = parse_bbl(INFORMS_BBL);
        assert_eq!(refs.len(), 2);
        assert_eq!(
            refs[0].title.as_deref(),
            Some(
                "The challenge of using llms to simulate human behavior: A causal inference \
                 perspective"
            )
        );
        assert_eq!(refs[0].authors, ["Gui G", "Toubia O"]);
        assert_eq!(refs[0].year, Some(2023));
        assert_eq!(refs[0].arxiv_id.as_deref(), Some("2312.15524"));
        assert_eq!(
            refs[1].title.as_deref(),
            Some("A gmm approach for dealing with missing data on regressors")
        );
        assert_eq!(refs[1].authors, ["Abrevaya J", "Donald SG"]);
    }

    /// INFORMS output as 2602.16061 ships it (verbatim): no `\newblock`, one
    /// block `Authors (year) Title. \emph{Venue} vol(no):pages.` per item.
    const INFORMS_PLAIN_BBL: &str = r"\begin{thebibliography}{54}
\providecommand{\natexlab}[1]{#1}
\providecommand{\url}[1]{\texttt{#1}}
\providecommand{\urlprefix}{URL }

\bibitem[{Abrevaya \protect\BIBand{} Donald(2017)}]{abrevaya2017gmm}
Abrevaya J, Donald SG (2017) A gmm approach for dealing with missing data on
  regressors. \emph{Review of Economics and Statistics} 99(4):657--662.

\bibitem[{Angelopoulos et~al.(2023{\natexlab{b}})Angelopoulos, Duchi,
  \protect\BIBand{} Zrnic}]{angelopoulos2023ppi++}
Angelopoulos AN, Duchi JC, Zrnic T (2023{\natexlab{b}}) Ppi++: Efficient
  prediction-powered inference. \emph{arXiv preprint arXiv:2311.01453} .

\bibitem[{Dell \protect\BIBand{} Rambachan(2026)}]{dell2026measurement}
Dell M, Rambachan A (2026) The measurement revolution? credible measurement and
  inference in the age of ai .

\bibitem[{Horton(2023)}]{horton2023large}
Horton JJ (2023) Large language models as simulated economic agents: What can
  we learn from homo silicus? Technical report, National Bureau of Economic
  Research.

\bibitem[{Litvinchev \protect\BIBand{}
  Tsurkov(2013)}]{litvinchev2013aggregation}
Litvinchev I, Tsurkov V (2013) \emph{Aggregation in large-scale optimization},
  volume~83 (Springer Science \& Business Media).

\bibitem[{Manski(2003)}]{manski2003partial}
Manski CF (2003) \emph{Partial identification of probability distributions}
  (Springer).
\end{thebibliography}
";

    #[test]
    fn parse_bbl_informs_without_newblock() {
        let refs = parse_bbl(INFORMS_PLAIN_BBL);
        assert_eq!(refs.len(), 6);
        // The title ends before the italic venue, which is not the title.
        assert_eq!(
            refs[0].title.as_deref(),
            Some("A gmm approach for dealing with missing data on regressors")
        );
        assert_eq!(refs[0].authors, ["Abrevaya J", "Donald SG"]);
        assert_eq!(refs[0].year, Some(2017));
        assert_eq!(
            refs[1].title.as_deref(),
            Some("Ppi++: Efficient prediction-powered inference")
        );
        assert_eq!(refs[1].authors, ["Angelopoulos AN", "Duchi JC", "Zrnic T"]);
        assert_eq!(refs[1].year, Some(2023));
        assert_eq!(refs[1].arxiv_id.as_deref(), Some("2311.01453"));
        // No venue: a lowercase word after `?` continues the title.
        assert_eq!(
            refs[2].title.as_deref(),
            Some("The measurement revolution? credible measurement and inference in the age of ai")
        );
        assert_eq!(refs[2].authors, ["Dell M", "Rambachan A"]);
        // No venue: the title ends at `? Technical report`, keeping the `?`.
        assert_eq!(
            refs[3].title.as_deref(),
            Some(
                "Large language models as simulated economic agents: What can we learn from \
                 homo silicus?"
            )
        );
        assert_eq!(refs[3].authors, ["Horton JJ"]);
        // An italic book title right after the year is the title.
        assert_eq!(
            refs[4].title.as_deref(),
            Some("Aggregation in large-scale optimization")
        );
        assert_eq!(refs[4].authors, ["Litvinchev I", "Tsurkov V"]);
        assert_eq!(refs[4].year, Some(2013));
        assert_eq!(
            refs[5].title.as_deref(),
            Some("Partial identification of probability distributions")
        );
        assert_eq!(refs[5].authors, ["Manski CF"]);
    }

    /// An `IEEEtran` online entry with the year after the author (2507.14211,
    /// verbatim) and an `IEEEtran` article whose `(2020)` follows the volume.
    const IEEE_YEAR_BBL: &str = r"\begin{thebibliography}{10}
\bibitem{gemv2}
\BIBentryALTinterwordspacing
M.~Boban. (2014) {GEMV2: Geometry Based Efficient Propagation Model for V2V
  Communication}. [Online]. Available: \url{http://vehicle2x.net/}
\BIBentrySTDinterwordspacing

\bibitem{volume}
A.~Author, Some title words, Journal 5 (2020) pages one to ten.
\end{thebibliography}
";

    #[test]
    fn parse_bbl_author_year_inline_guards() {
        let refs = parse_bbl(IEEE_YEAR_BBL);
        assert_eq!(refs.len(), 2);
        assert_eq!(
            refs[0].title.as_deref(),
            Some("GEMV2: Geometry Based Efficient Propagation Model for V2V Communication")
        );
        assert_eq!(refs[0].authors, ["M. Boban"]);
        assert_eq!(refs[0].year, Some(2014));
        // A digit before the year (a volume) is not an author list.
        assert_eq!(refs[1].title, None);
        assert!(refs[1].authors.is_empty());
    }

    #[test]
    fn author_year_inline_keeps_abbreviation_periods() {
        let bbl = r"\begin{thebibliography}{1}
\bibitem[{Abrevaya(2017)}]{abrevaya2017}
Abrevaya J (2017) A study of U.S. Policy after Dr. Smith. \emph{Journal}
  12(3):45--67.
\end{thebibliography}
";
        let refs = parse_bbl(bbl);
        assert_eq!(refs.len(), 1);
        assert_eq!(
            refs[0].title.as_deref(),
            Some("A study of U.S. Policy after Dr. Smith")
        );
        assert_eq!(refs[0].authors, ["Abrevaya J"]);
        assert_eq!(refs[0].year, Some(2017));
        // The sentence still ends at a real period, not at an abbreviation.
        assert_eq!(
            first_sentence("Trade e.g. Steel vs. Iron. Technical report"),
            "Trade e.g. Steel vs. Iron"
        );
        assert_eq!(
            first_sentence("Results in Fig. Two and No. Three. Working paper"),
            "Results in Fig. Two and No. Three"
        );
        assert_eq!(first_sentence("Web 2.0 Tools. Report"), "Web 2.0 Tools");
        assert_eq!(first_sentence("Plain title. Next"), "Plain title");
    }

    /// Title blocks with a trailing year or descriptor, or only a URL
    /// (2503.00030, 2511.13979 and 2410.17124, verbatim apart from the
    /// shortened author lists).
    const TITLE_EXTRAS_BBL: &str = r"\begin{thebibliography}{4}
\bibitem[DeepSeek-AI et~al.(2025)]{deepseekai2025deepseekr1}
DeepSeek-AI, Guo, D., Yang, D., and Zhang, Z.
\newblock Deepseek-r1: Incentivizing reasoning capability in llms via reinforcement learning, 2025.
\newblock URL \url{https://arxiv.org/abs/2501.12948}.

\bibitem[Zhang et~al.(2024{\natexlab{a}})Zhang, Yu, Peng, Song, Tian, Huo, Jiang, Mi, and Yu]{zhang2024iterativenashpolicyoptimization}
Zhang, Y., Yu, D., Peng, B., Song, L., Tian, Y., Huo, M., Jiang, N., Mi, H., and Yu, D.
\newblock Iterative nash policy optimization: Aligning llms with general preferences via no-regret learning, 2024{\natexlab{a}}.
\newblock URL \url{https://arxiv.org/abs/2407.00617}.

\bibitem[Ju and Aral(2026{\natexlab{a}})]{ju2026code}
H.~Ju and S.~Aral.
\newblock Personality pairing improves human-ai collaboration [analysis code].
\newblock GitHub, 2026{\natexlab{a}}.
\newblock URL \url{https://github.com/harangju/personality-pairing}.

%Type = Misc
\bibitem[{Henderson(2022)}]{RSNA}
\bibinfo{author}{Henderson, M.}, \bibinfo{year}{2022}.
\newblock \URLprefix \url{https://www.rsna.org/news/2022/may/global-radiologist-shortage}.
\end{thebibliography}
";

    #[test]
    fn parse_bbl_title_drops_year_descriptor_and_url() {
        let refs = parse_bbl(TITLE_EXTRAS_BBL);
        assert_eq!(refs.len(), 4);
        assert_eq!(
            refs[0].title.as_deref(),
            Some(
                "Deepseek-r1: Incentivizing reasoning capability in llms via reinforcement learning"
            )
        );
        assert_eq!(refs[0].year, Some(2025));
        assert_eq!(
            refs[1].title.as_deref(),
            Some(
                "Iterative nash policy optimization: Aligning llms with general preferences via \
                 no-regret learning"
            )
        );
        assert_eq!(refs[1].year, Some(2024));
        assert_eq!(
            refs[2].title.as_deref(),
            Some("Personality pairing improves human-ai collaboration")
        );
        // A URL is not a title.
        assert_eq!(refs[3].title, None);
        assert_eq!(refs[3].authors, ["M. Henderson"]);
        assert_eq!(refs[3].year, Some(2022));
    }

    /// `apalike`-style books whose title block carries the series (entries
    /// from 2603.05575): the series stays in the text, not the title.
    const SERIES_BBL: &str = r"\begin{thebibliography}{2}
\bibitem[Engl et~al.(1996)Engl, Hanke, and Neubauer]{engl1996regularization}
Engl, H.~W., Hanke, M., and Neubauer, A. (1996).
\newblock {\em Regularization of Inverse Problems}, volume 375 of {\em
  Mathematics and Its Applications}.
\newblock Kluwer Academic Publishers, Dordrecht.

\bibitem[Wainwright(2019)]{wainwright2019high}
Wainwright, M.~J. (2019).
\newblock {\em High-Dimensional Statistics: A Non-Asymptotic Viewpoint},
  volume~48.
\newblock Cambridge University Press.
\end{thebibliography}
";

    #[test]
    fn parse_bbl_series_is_not_part_of_the_title() {
        let refs = parse_bbl(SERIES_BBL);
        assert_eq!(refs.len(), 2);
        assert_eq!(
            refs[0].title.as_deref(),
            Some("Regularization of Inverse Problems")
        );
        assert!(
            refs[0]
                .text
                .contains("volume 375 of Mathematics and Its Applications"),
            "{}",
            refs[0].text
        );
        assert_eq!(refs[0].authors.len(), 3);
        assert_eq!(
            refs[1].title.as_deref(),
            Some("High-Dimensional Statistics: A Non-Asymptotic Viewpoint")
        );
        assert_eq!(refs[1].year, Some(2019));
    }

    /// `IEEEtran` output without `\newblock` (entries from 2608.28714).
    const IEEE_BBL: &str = r"\begin{thebibliography}{10}
\bibitem[Zaitsev et~al.(2015)Zaitsev, Maclaren, and Herbst]{zaitsev2015motion}
M.~Zaitsev, J.~Maclaren, and M.~Herbst, ``Motion artifacts in {MRI}: A
  review,'' \emph{NMR in Biomedicine}, vol.~28, no.~7, pp. 911--935, 2015.

\bibitem[Chen et~al.(2025)]{chen2025mri}
G.~Chen, H.~Xie, and C.~Liu, ``{MRI} motion correction through disentangled
  {CycleGAN} based on multi-mask k-space subsampling,'' \emph{IEEE Transactions
  on Medical Imaging}, vol.~44, pp. 1907--1921, 2025.

\bibitem[Page et~al.(2021)]{prisma2020}
M.~J. Page, J.~E. McKenzie, and C.~D. Mulrow \emph{et~al.}, ``The {PRISMA}
  2020 statement: An updated guideline for reporting systematic reviews,''
  \emph{BMJ}, vol. 372, p. n71, 2021.

\bibitem[Barrett and Myers(2004)]{barrett2004foundations}
H.~H. Barrett and K.~J. Myers, \emph{Foundations of Image Science}.\hskip 1em
  plus 0.5em minus 0.4em\relax Hoboken, NJ: Wiley, 2004.
\end{thebibliography}
";

    /// `siamplain` output without `\newblock` (entries from 2504.09409 and
    /// 2603.21379; the raw markup is reconstructed from the detexed truth).
    const SIAM_BBL: &str = r"\begin{thebibliography}{10}
\bibitem{BG22}
{\sc K.~Balasubramanian and S.~Ghadimi}, {\em Zeroth-order nonconvex stochastic
  optimization: Handling constraints, high-dimensionality and saddle-points},
  Found. Comput. Math., 22 (2022), pp.~35--76.

\bibitem{beck2017first}
{\sc A.~Beck}, {\em First-order methods in optimization}, SIAM, 2017.

\bibitem{hooke1961direct}
{\sc R.~Hooke and T.~A. Jeeves}, {\em ``{D}irect search'' solution of numerical
  and statistical problems}, J. ACM, 8 (1961), pp.~212--229.

\bibitem{Bou2017}
{\sc C.~Boutsidis and D.~P. Woodruff}, {\em Optimal {CUR} Matrix
  Decompositions}, SIAM Journal on Computing, 46 (2017), pp.~543--589,
  \url{https://doi.org/10.1137/140977898}.

\bibitem{Coherence2}
{\sc Y.~Chen and Y.~Chi}, {\em Spectral Compressed Sensing via Structured
  Matrix Completion}, in Proceedings of the 30th International Conference on
  Machine Learning - Volume 28, JMLR.org, 2013.

\bibitem{bookonly}
{\sc A.~Author}, in {\em Proceedings of Something}, 2019.
\end{thebibliography}
";

    #[test]
    fn parse_bbl_ieee_without_newblock() {
        let refs = parse_bbl(IEEE_BBL);
        assert_eq!(refs.len(), 4);
        assert_eq!(
            refs[0].title.as_deref(),
            Some("Motion artifacts in MRI: A review")
        );
        assert_eq!(refs[0].authors, ["M. Zaitsev", "J. Maclaren", "M. Herbst"]);
        assert_eq!(refs[0].year, Some(2015));
        assert_eq!(
            refs[1].title.as_deref(),
            Some(
                "MRI motion correction through disentangled CycleGAN based on multi-mask \
                 k-space subsampling"
            )
        );
        assert_eq!(refs[1].authors, ["G. Chen", "H. Xie", "C. Liu"]);
        // `pp. 1907--1921` is a page range, not the year.
        assert_eq!(refs[1].year, Some(2025));
        assert_eq!(
            refs[2].title.as_deref(),
            Some(
                "The PRISMA 2020 statement: An updated guideline for reporting systematic \
                 reviews"
            )
        );
        assert_eq!(
            refs[2].authors,
            ["M. J. Page", "J. E. McKenzie", "C. D. Mulrow"]
        );
        // The `2020` in the title is not the year.
        assert_eq!(refs[2].year, Some(2021));
        // No quotes: the italic book title.
        assert_eq!(
            refs[3].title.as_deref(),
            Some("Foundations of Image Science")
        );
        assert_eq!(refs[3].authors, ["H. H. Barrett", "K. J. Myers"]);
        assert_eq!(refs[3].year, Some(2004));
    }

    #[test]
    fn parse_bbl_siam_without_newblock() {
        let refs = parse_bbl(SIAM_BBL);
        assert_eq!(refs.len(), 6);
        assert_eq!(
            refs[0].title.as_deref(),
            Some(
                "Zeroth-order nonconvex stochastic optimization: Handling constraints, \
                 high-dimensionality and saddle-points"
            )
        );
        assert_eq!(refs[0].authors, ["K. Balasubramanian", "S. Ghadimi"]);
        assert_eq!(refs[0].year, Some(2022));
        assert_eq!(
            refs[1].title.as_deref(),
            Some("First-order methods in optimization")
        );
        assert_eq!(refs[1].authors, ["A. Beck"]);
        assert_eq!(refs[1].year, Some(2017));
        // A quote inside the italic title belongs to the title.
        assert_eq!(
            refs[2].title.as_deref(),
            Some("\"Direct search\" solution of numerical and statistical problems")
        );
        assert_eq!(refs[2].authors, ["R. Hooke", "T. A. Jeeves"]);
        assert_eq!(refs[2].year, Some(1961));
        assert_eq!(
            refs[3].title.as_deref(),
            Some("Optimal CUR Matrix Decompositions")
        );
        assert_eq!(refs[3].authors, ["C. Boutsidis", "D. P. Woodruff"]);
        assert_eq!(refs[3].year, Some(2017));
        assert_eq!(refs[3].doi.as_deref(), Some("10.1137/140977898"));
        assert_eq!(
            refs[4].title.as_deref(),
            Some("Spectral Compressed Sensing via Structured Matrix Completion")
        );
        assert_eq!(refs[4].authors, ["Y. Chen", "Y. Chi"]);
        assert_eq!(refs[4].year, Some(2013));
        // An italic group after `in` is a proceedings title: nothing is guessed.
        assert_eq!(refs[5].title, None);
        assert!(refs[5].authors.is_empty());
        assert_eq!(refs[5].year, Some(2019));
    }

    #[test]
    fn first_year_skips_page_and_volume_numbers() {
        let cases: [(&str, Option<u16>); 17] = [
            (
                "T. B. Brown, and D. Amodei. Language models are few-shot learners. In H. \
                 Larochelle et al., editors, Advances in Neural Information Processing \
                 Systems, volume 33, pages 1877–1901. Curran Associates, Inc., 2020.",
                Some(2020),
            ),
            (
                "G. Chen, and C. Liu, \"MRI motion correction,\" IEEE Transactions on \
                 Medical Imaging, vol. 44, pp. 1907–1921, 2025.",
                Some(2025),
            ),
            (
                "M. J. Page et al., \"The PRISMA 2020 statement: An updated guideline,\" \
                 BMJ, vol. 372, p. n71, 2021.",
                Some(2021),
            ),
            (
                "J. Lee, \"Lesion-aware post-training,\" in Medical Image Computing and \
                 Computer Assisted Intervention – MICCAI 2025.1em plus 0.5em minus \
                 0.4emSpringer, 2026.",
                Some(2026),
            ),
            ("X. Y. Z. RFC, 1952:1–12, 1996.", Some(1996)),
            ("IEEE Access, vol. 8, pp. 2087– 2098, 2024.", Some(2024)),
            (
                "Y. Zhang, Q. Yang, An overview, National Science Review 5 (2018) 30–43.",
                Some(2018),
            ),
            (
                "Smith, J. (2020a). Title. In Proc. 2019 Workshop.",
                Some(2020),
            ),
            ("2020-05-01", Some(2020)),
            ("2020-05", Some(2020)),
            ("2021/03/15", Some(2021)),
            ("date = {2021/03}", Some(2021)),
            ("pp. 1907–1921", None),
            ("Journal of Tests, 12:1907–1921.", None),
            ("1998", Some(1998)),
            (
                "Report 19201, see https://example.org/2019/x and arXiv:2001.01234.",
                None,
            ),
            ("No year here.", None),
        ];
        for (text, expected) in cases {
            assert_eq!(first_year(text), expected, "text: {text}");
        }
    }

    #[test]
    fn parse_bbl_biblatex_entries() {
        let refs = parse_bbl(BIBLATEX_BBL);
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].key, "doe2020");
        assert_eq!(refs[0].authors, ["Jane Doe", "Richard Roe"]);
        assert_eq!(refs[0].title.as_deref(), Some("A Study"));
        assert_eq!(refs[0].year, Some(2020));
        assert_eq!(refs[0].doi.as_deref(), Some("10.1000/xyz"));
        assert_eq!(refs[0].arxiv_id.as_deref(), Some("2001.00002"));
        assert_eq!(refs[0].text, "Jane Doe, Richard Roe. A Study. Nature 2020");
        assert_eq!(refs[1].key, "second");
        assert_eq!(refs[1].source, TruthSource::Bbl);
    }

    #[test]
    fn parse_bib_snippet() {
        let refs = parse_bib(BIB);
        assert_eq!(refs.len(), 4);
        let keys: Vec<&str> = refs.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(
            keys.join(","),
            "DBLP:journals/ftir/MaviJJ24,karpukhin-etal-2020-dense,2024arXiv240608394W,noyear"
        );
        assert_eq!(
            refs[0].authors,
            ["Vaibhav Mavi", "Anubhav Jangra", "Adam Jatowt"]
        );
        assert_eq!(
            refs[0].title.as_deref(),
            Some("Multi-hop Question Answering")
        );
        assert_eq!(refs[0].year, Some(2024));
        assert_eq!(refs[0].doi.as_deref(), Some("10.1561/1500000102"));
        assert_eq!(
            refs[0].text,
            "Vaibhav Mavi, Anubhav Jangra, Adam Jatowt. Multi-hop Question Answering. Found. Trends Inf. Retr. 2024"
        );

        assert_eq!(refs[1].authors, ["Vladimir Karpukhin", "Barlas Oguz"]);
        assert_eq!(refs[1].year, Some(2020));
        assert_eq!(
            refs[1].doi.as_deref(),
            Some("10.18653/v1/2020.emnlp-main.550")
        );
        assert_eq!(
            refs[1].title.as_deref(),
            Some("Dense Passage Retrieval for Open-Domain Question Answering")
        );
        assert!(refs[1].text.ends_with("Proceedings of EMNLP 2020"));

        assert_eq!(
            refs[2].title.as_deref(),
            Some("VisionLLM v2: An End-to-End Generalist LLM")
        );
        assert_eq!(refs[2].arxiv_id.as_deref(), Some("2406.08394"));
        assert_eq!(refs[2].authors, ["Jiannan Wu", "Muyan Zhong"]);
        assert_eq!(refs[2].year, Some(2024));

        assert_eq!(refs[3].year, None);
        assert_eq!(refs[3].authors, ["John Doe"]);
        assert_eq!(refs[3].arxiv_id.as_deref(), Some("1704.05021"));
        assert_eq!(refs[3].label, None);
        assert_eq!(refs[3].source, TruthSource::Bib);
    }

    #[test]
    fn parse_bib_parenthesised_entry_and_other_archive() {
        let bib = "@article(paren, title={P}, eprint={2001.00001}, archivePrefix={bioRxiv})";
        let refs = parse_bib(bib);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].key, "paren");
        assert_eq!(refs[0].title.as_deref(), Some("P"));
        assert_eq!(refs[0].arxiv_id, None);
    }

    #[test]
    fn parse_cites_variants() {
        let cites = parse_cites(CITES_TEX);
        assert_eq!(cites.cite_commands, 4);
        assert_eq!(cites.cite_only_author_year, 1);
        assert_eq!(cites.cited_keys, ["a", "b", "c", "f", "g"]);
        assert!(cites.nocite_all);
        assert_eq!(cites.nocite_keys, ["d", "e"]);
    }

    #[test]
    fn body_text_drops_figures_and_keeps_sections() {
        let out = body_text(BODY_TEX);
        assert!(
            out.starts_with("Introduction\n\nWe present FooNet here"),
            "{out}"
        );
        // Inline math is removed with its contents.
        assert!(out.contains("Second paragraph with ."), "{out}");
        assert!(!out.contains("x^2"), "{out}");
        assert!(!out.contains("caption"), "{out}");
        assert!(!out.contains("mc^2"), "{out}");
        assert!(!out.contains("Trailing"), "{out}");
        assert!(!out.contains("refs"), "{out}");
        assert!(!out.contains("sec:intro"), "{out}");
    }

    const ELSARTICLE_TEX: &str = r"\documentclass{elsarticle}
\begin{document}
\begin{frontmatter}
\title{A Survey Title}
\author[1]{Weiqiang Jin\corref{cor1}}
\ead{jin@example.edu}
\cortext[cor1]{Corresponding authors: Jin.}
\affiliation[1]{organization={School of Engineering, Example University},
            addressline={Harbour}, city={Xian}, postcode={710049}, country={China}}
\address{Old Style Address}
\tnotetext[t1]{Title note text.}
\fntext[fn1]{Footnote text.}
\begin{abstract}
With the rapid development of agents.
\end{abstract}
\begin{keyword}
multiagent \sep cooperation
\end{keyword}
\end{frontmatter}
\section{Introduction}
Body text here.
\end{document}
";

    /// `elsarticle` commands outside a `frontmatter` environment.
    const ELSARTICLE_LOOSE_TEX: &str = r"\begin{document}
\title{Loose Title}
\author[a]{Jane Roe\fnref{f1}}
\affiliation[a]{organization={Loose Institute}, city={Paris}, country={France}}
\ead[url]{www.example.org}
\fntext[f1]{Loose footnote.}
\cortext[c1]{Corresponding author: Roe.}
\tnotetext[t1]{Loose title note.}
\address{Loose Street 1, Lyon}
\markboth{Roe: Loose Running Head}{Journal Running Head}
\titlerunning{Loose Short Title}
\authorrunning{J. Roe}
\begin{abstract}
Loose abstract words.
\end{abstract}
\section{Method}
Loose body words.
\end{document}
";

    const ICML_TEX: &str = r"\documentclass{article}
\begin{document}
\twocolumn[
\icmltitle{Scaling Widgets}
\icmlsetsymbol{equal}{*}
\begin{icmlauthorlist}
\icmlauthor{Ada Lovelace}{equal,uni}
\icmlauthor{Alan Turing}{uni}
\end{icmlauthorlist}
\icmlaffiliation{uni}{Department of Computing, Example University, London, UK}
\icmlcorrespondingauthor{Ada Lovelace}{ada@example.org}
\icmlkeywords{Machine Learning, ICML}
\vskip 0.3in
]
\printAffiliationsAndNotice{\icmlEqualContribution}
\begin{abstract}
Widgets scale well.
\end{abstract}
\section{Introduction}
We scale widgets.
\end{document}
";

    const ACM_TEX: &str = r"\documentclass{acmart}
\begin{document}
\title{DesignAsCode: Bridging Editability}
\thanks{This arXiv version extends the paper.}
\author{Ziyuan Liu}
\authornote{Work done during an internship.}
\orcid{0000-0001-2345-6789}
\affiliation{\institution{Peking University}\city{Beijing}\country{China}}
\email{liu@example.edu}
\begin{abstract}
Graphic design generation demands balance.
\end{abstract}
\begin{CCSXML}
<ccs2012><concept_desc>Computing methodologies</concept_desc></ccs2012>
\end{CCSXML}
\ccsdesc[500]{Computing methodologies~Computer vision}
\keywords{graphic layout, posters}
\date{\today}
\maketitle
\section{Introduction}
Designers edit layers.
\end{document}
";

    const IEEE_TEX: &str = r"\documentclass{IEEEtran}
\begin{document}
\title{Fast Radios}
\author{\IEEEauthorblockN{Grace Hopper}
\IEEEauthorblockA{Navy Lab, Arlington, USA \\ grace@example.mil}}
\maketitle
\begin{abstract}
Radios are fast.
\end{abstract}
\begin{IEEEkeywords}
radio, speed
\end{IEEEkeywords}
\section{Introduction}
We build radios.
\end{document}
";

    const TITLEPAGE_TEX: &str = r"\begin{document}
\begin{titlepage}
\centering
{\Large Raw Title Words}\\
Raw Author Name
\begin{abstract}
Titlepage abstract words.
\end{abstract}
\end{titlepage}
\section{Start}
Titlepage body words.
\end{document}
";

    const TITLEPAGE_NO_ABSTRACT_TEX: &str = r"\begin{document}
\begin{titlepage}
No Abstract Title
\end{titlepage}
Plain body words.
\end{document}
";

    fn assert_absent(out: &str, noise: &[&str]) {
        for word in noise {
            assert!(!out.contains(word), "{word:?} in {out}");
        }
    }

    #[test]
    fn body_text_drops_elsarticle_front_matter() {
        let out = body_text(ELSARTICLE_TEX);
        assert_eq!(
            out,
            "With the rapid development of agents.\n\nIntroduction\n\nBody text here."
        );
        let loose = body_text(ELSARTICLE_LOOSE_TEX);
        assert!(loose.contains("Loose abstract words."), "{loose}");
        assert!(loose.contains("Method"), "{loose}");
        assert!(loose.contains("Loose body words."), "{loose}");
        assert_absent(
            &loose,
            &[
                "Loose Title",
                "Jane",
                "organization",
                "Institute",
                "Paris",
                "example.org",
                "footnote",
                "Corresponding",
                "title note",
                "Lyon",
                "Running Head",
                "Short Title",
                "J. Roe",
            ],
        );
    }

    #[test]
    fn body_text_keeps_only_the_abstract_of_a_titlepage() {
        let out = body_text(TITLEPAGE_TEX);
        assert!(out.starts_with("Titlepage abstract words."), "{out}");
        assert!(out.contains("Titlepage body words."), "{out}");
        assert_absent(&out, &["Raw Title", "Raw Author"]);
        let bare = body_text(TITLEPAGE_NO_ABSTRACT_TEX);
        assert_eq!(bare, "Plain body words.");
    }

    #[test]
    fn body_text_drops_icml_front_matter() {
        let out = body_text(ICML_TEX);
        assert!(out.starts_with("Widgets scale well."), "{out}");
        assert!(out.contains("Introduction"), "{out}");
        assert!(out.contains("We scale widgets."), "{out}");
        assert_absent(
            &out,
            &[
                "Scaling Widgets",
                "Ada",
                "Turing",
                "Example University",
                "example.org",
                "Machine Learning",
                "0.3in",
                "[",
                "]",
            ],
        );
    }

    #[test]
    fn body_text_drops_acm_front_matter() {
        let out = body_text(ACM_TEX);
        assert!(
            out.starts_with("Graphic design generation demands balance."),
            "{out}"
        );
        assert!(out.contains("Introduction"), "{out}");
        assert!(out.contains("Designers edit layers."), "{out}");
        assert_absent(
            &out,
            &[
                "DesignAsCode",
                "arXiv version",
                "Ziyuan",
                "internship",
                "0000-0001",
                "Peking",
                "Beijing",
                "example.edu",
                "ccs2012",
                "Computing methodologies",
                "posters",
                "today",
            ],
        );
    }

    #[test]
    fn body_text_drops_ieee_front_matter() {
        let out = body_text(IEEE_TEX);
        assert!(out.starts_with("Radios are fast."), "{out}");
        assert!(out.contains("We build radios."), "{out}");
        assert_absent(
            &out,
            &[
                "Fast Radios",
                "Grace",
                "Navy",
                "example.mil",
                "radio, speed",
            ],
        );
    }

    const MATH_TEX: &str = r"\documentclass{article}
\newcommand{\be}{\begin{equation}}
\newcommand{\ee}{\end{equation}}
\def\bea{\begin{eqnarray}}
\def\eea{\end{eqnarray}}
\begin{document}
\newcommand{\myword}{Widget}
\def\other{Gadget}
Before the display.
\be\label{eq:one}
K_s = 108 \alpha
\ee
After the \myword{} and \other{} display with $x_i = 2$ inline, $$y^2$$ shown, \(z_k\) and \[w + 1\] math.
\bea a_1 &=& b_1 \eea
Price is \$5 and \$6 total.
A line break\\[2pt]
stays text.
Open $ dollar

Next paragraph.
\end{document}
";

    #[test]
    fn body_text_expands_equation_aliases_and_removes_all_math() {
        let out = body_text(MATH_TEX);
        for kept in [
            "Before the display.",
            "After the Widget and Gadget display with inline, shown, and math.",
            "Price is $5 and $6 total.",
            "A line break stays text.",
            "Open dollar",
            "Next paragraph.",
        ] {
            assert!(out.contains(kept), "{kept:?} missing from {out}");
        }
        assert_absent(
            &out,
            &[
                "K_s",
                "108",
                "alpha",
                "eq:one",
                "x_i",
                "y^2",
                "z_k",
                "w + 1",
                "a_1",
                "b_1",
                "&",
                "newcommand",
                "Widget}",
                "equation",
                "eqnarray",
            ],
        );
    }

    const FOOTNOTE_TEX: &str = r"\begin{document}
We study cats.\footnote{See the {appendix} for $n$ dogs.} Then more.\footnotemark[2]\footnotetext[2]{Hidden text.} End.
\footnotesize Small words.
\end{document}
";

    #[test]
    fn body_text_drops_footnotes() {
        assert_eq!(
            body_text(FOOTNOTE_TEX),
            "We study cats. Then more. End. Small words."
        );
    }

    const BOX_NOISE_TEX: &str = r"\begin{document}
\newtcolorbox{findingbox}{enhanced, breakable, colback = blue!10, colframe = blue!10!black}
\newtcblisting{motivationbox}[1][]{listing only, colback=codebg, boxrule=0.5pt}
\lstset{basicstyle=\ttfamily, breaklines=true}
\tikzset{promptstyle/.style={breakable, colback=gray!10}}
\definecolor{codebg}{rgb}{0.95,0.95,0.95}
\colorlet{shade}{gray!20}
\begin{tcolorbox}
[colback=white, title={Key finding}]
Boxed finding words.
\end{tcolorbox}
\begin{mdframed} [linecolor=black]
Framed words.
\end{mdframed}
\begin{theorem}
Theorem words.
\end{theorem}
Body words.
\begin{IEEEbiography}[{\includegraphics{a.png}}]{Grace Hopper}
received her degree in 1934.
\end{IEEEbiography}
\begin{IEEEbiographynophoto}{Alan Turing}
was born in London.
\end{IEEEbiographynophoto}
\begin{biography}
Plain bio words.
\end{biography}
\end{document}
";

    #[test]
    fn body_text_drops_box_settings_options_and_biographies() {
        let out = body_text(BOX_NOISE_TEX);
        assert_eq!(
            out,
            "Boxed finding words. Framed words. Theorem words. Body words."
        );
        assert_absent(
            &out,
            &[
                "colback",
                "colframe",
                "enhanced",
                "basicstyle",
                "style",
                "rgb",
                "0.95",
                "gray",
                "codebg",
                "Key finding",
                "linecolor",
                "Grace",
                "1934",
                "Turing",
                "London",
                "bio words",
            ],
        );
    }

    const LISTING_TEX: &str = r#"\documentclass{article}
\usepackage{fancyvrb}
\DefineVerbatimEnvironment{prompt}{Verbatim}{}
\newtcblisting[auto counter]{codebox}{listing only}
\lstnewenvironment{pylisting}[1][]{\lstset{language=Python}}{}
\newenvironment{promptbox}{\begin{center}}{\end{center}}
\newtcolorbox{notebox}{colback=white}
\begin{document}
Intro words.
\begin{prompt}
You are a helpful assistant. Answer the {question} below.
\end{prompt}
\begin{codebox}
print("hidden code")
\end{codebox}
\begin{pylisting}[caption=x]
def hidden(): pass
\end{pylisting}
\begin{promptbox}
Kept prompt box words.
\end{promptbox}
\begin{notebox}
Kept note words.
\end{notebox}
\begin{tcolorbox}
Box prose words.
\begin{Verbatim}[fontsize=\small]
Hidden verbatim line.
\end{Verbatim}
\end{tcolorbox}
\begin{alltt}
hidden alltt
\end{alltt}
\begin{minted}{python}
hidden minted
\end{minted}
\begin{lstlisting}
hidden lstlisting
\end{lstlisting}
Closing words.
\end{document}
"#;

    #[test]
    fn verbatim_env_names_reads_listing_definitions_only() {
        let names = verbatim_env_names(&strip_comments(LISTING_TEX));
        let expected = BTreeSet::from(["codebox", "prompt", "pylisting"].map(String::from));
        assert_eq!(names, expected);
    }

    #[test]
    fn body_text_drops_verbatim_and_defined_listing_environments() {
        let out = body_text(LISTING_TEX);
        let flat: Vec<&str> = out.split_whitespace().collect();
        assert_eq!(
            flat.join(" "),
            "Intro words. Kept prompt box words. Kept note words. Box prose words. Closing words."
        );
        assert_absent(
            &out,
            &[
                "assistant",
                "question",
                "hidden",
                "print",
                "Verbatim",
                "fontsize",
                "caption",
            ],
        );
    }

    #[test]
    fn listing_environments_are_dropped_and_prose_boxes_kept() {
        for name in [
            "verbatim",
            "Verbatim",
            "Verbatim*",
            "BVerbatim",
            "LVerbatim",
            "lstlisting",
            "minted",
            "alltt",
            "spverbatim",
            "listing",
            "code",
            "python",
            "pycode",
            "sourcecode",
            "tcblisting",
            "tcbverbatim",
        ] {
            assert!(is_dropped_env(name), "{name:?}");
        }
        for name in ["tcolorbox", "mdframed", "promptbox", "theorem"] {
            assert!(!is_dropped_env(name), "{name:?}");
        }
    }

    #[test]
    fn remove_math_handles_escapes_and_unclosed_delimiters() {
        assert_eq!(remove_math(r"a $x$ b"), "a   b");
        assert_eq!(remove_math(r"cost \$3 and \$4"), r"cost \$3 and \$4");
        assert_eq!(remove_math(r"a $$x$$ b"), "a   b");
        assert_eq!(remove_math(r"a \(x\) b \[y\] c"), "a   b   c");
        assert_eq!(remove_math(r"line\\[2pt] next"), r"line\\[2pt] next");
        assert_eq!(remove_math("open $ x\n\nnext $y$"), "open   x\n\nnext  ");
        assert_eq!(remove_math(r"$\$$ kept"), "  kept");
    }

    #[test]
    fn resolve_inputs_inlines_relative_files() {
        let dir = tempfile::tempdir().unwrap();
        let sections = dir.path().join("sections");
        fs::create_dir_all(&sections).unwrap();
        fs::write(sections.join("intro.tex"), "Hello \\input{deeper}").unwrap();
        fs::write(dir.path().join("deeper.tex"), "World").unwrap();
        let merged = resolve_inputs(
            dir.path(),
            "A \\input{sections/intro} B \\include{missing} C % \\input{deeper}\n",
            0,
        );
        assert!(merged.contains("Hello"), "{merged}");
        assert!(merged.contains("World"), "{merged}");
        assert!(merged.contains("\\include{missing}"), "{merged}");
        assert_eq!(merged.matches("World").count(), 1);
    }

    #[test]
    fn resolve_inputs_handles_dotted_basename() {
        // `Path::with_extension` would turn `3.1_method` into `3.tex`; the
        // `.tex` suffix must be appended by string concatenation instead.
        let dir = tempfile::tempdir().unwrap();
        let sections = dir.path().join("sections");
        fs::create_dir_all(&sections).unwrap();
        fs::write(sections.join("3.1_method.tex"), "Method body").unwrap();
        let merged = resolve_inputs(dir.path(), "A \\input{sections/3.1_method} B", 0);
        assert!(merged.contains("Method body"), "{merged}");
    }

    #[test]
    fn resolve_inputs_accepts_explicit_tex_suffix() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("intro.tex"), "Intro body").unwrap();
        let merged = resolve_inputs(dir.path(), "A \\input{intro.tex} B", 0);
        assert!(merged.contains("Intro body"), "{merged}");
    }

    #[test]
    fn resolve_inputs_accepts_brace_free_input() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("intro.tex"), "Bare intro body").unwrap();
        let merged = resolve_inputs(dir.path(), "A \\input intro END", 0);
        assert!(merged.contains("Bare intro body"), "{merged}");
        assert!(merged.contains("END"), "{merged}");
    }

    #[test]
    fn resolve_inputs_rejects_cycles() {
        let dir = tempfile::tempdir().unwrap();
        let main = "\\begin{document}\\input{main}\\end{document}";
        let path = dir.path().join("main.tex");
        fs::write(&path, main).unwrap();

        let error = resolve_inputs_checked(dir.path(), main, 0, Some(&path)).unwrap_err();
        assert!(matches!(error, TruthError::CyclicInput(_)));
    }

    #[test]
    fn resolve_inputs_limits_include_operations() {
        let input = "\\input{missing}\n".repeat(MAX_INPUTS + 1);
        let error = resolve_inputs_checked(Path::new("."), &input, 0, None).unwrap_err();
        assert!(matches!(error, TruthError::InputLimit));
    }

    #[test]
    fn ground_truth_prefers_bbl_and_filters_bib() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("main.tex"), MAIN_TEX).unwrap();
        fs::write(dir.path().join("macros.tex"), "\\newcommand{\\x}{y}").unwrap();
        fs::write(dir.path().join("refs.bib"), REFS_BIB).unwrap();

        let cited_files = files(dir.path(), &["macros.tex", "main.tex"], &[], &["refs.bib"]);
        let cited = ground_truth(&cited_files).unwrap();
        assert_eq!(cited.method, "bib-cited");
        let keys: Vec<&str> = cited.references.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(keys, ["alpha", "gamma"]);
        assert_eq!(cited.citations.cite_commands, 2);
        assert!(
            cited.body_text.starts_with("Intro\n\nSee and ."),
            "{}",
            cited.body_text
        );

        fs::write(dir.path().join("main.tex"), TYPESET_TEX).unwrap();
        fs::write(dir.path().join("main.bbl"), BBL).unwrap();
        let typeset_files = files(dir.path(), &["main.tex"], &["main.bbl"], &["refs.bib"]);
        let typeset = ground_truth(&typeset_files).unwrap();
        assert_eq!(typeset.method, "bbl");
        assert_eq!(typeset.references.len(), 3);
        assert_eq!(typeset.references[0].source, TruthSource::Bbl);
    }

    #[test]
    fn ground_truth_nocite_all_and_errors() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("main.tex"), NOCITE_TEX).unwrap();
        fs::write(dir.path().join("refs.bib"), DUP_BIB).unwrap();
        let all = ground_truth(&files(dir.path(), &["main.tex"], &[], &["refs.bib"])).unwrap();
        assert_eq!(all.method, "bib-all");
        assert_eq!(all.references.len(), 2);

        let no_bib = ground_truth(&files(dir.path(), &["main.tex"], &[], &[]));
        assert!(matches!(no_bib, Err(TruthError::NoBibliography)));

        fs::write(dir.path().join("preamble.tex"), "\\usepackage{x}").unwrap();
        let no_main = ground_truth(&files(dir.path(), &["preamble.tex"], &[], &["refs.bib"]));
        assert!(matches!(no_main, Err(TruthError::NoMainTex)));
    }

    #[test]
    fn ground_truth_merges_bib_for_multibib_with_partial_bbl() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("main.tex"), MULTIBIB_TEX).unwrap();
        fs::write(dir.path().join("app.bbl"), BBL).unwrap();
        fs::write(dir.path().join("refs.bib"), REFS_BIB).unwrap();
        let truth = ground_truth(&files(
            dir.path(),
            &["main.tex"],
            &["app.bbl"],
            &["refs.bib"],
        ))
        .unwrap();
        assert_eq!(truth.method, "bbl+bib");
        let keys: Vec<&str> = truth.references.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "aamand2021classifying",
                "alber2004geometric",
                "fekete1923verteilung",
                "alpha",
                "gamma"
            ]
        );
        assert_eq!(truth.references[0].source, TruthSource::Bbl);
        assert_eq!(truth.references[3].source, TruthSource::Bib);
        assert_eq!(truth.references[4].year, Some(2003));
        assert_eq!(truth.citations.cite_commands, 6);
        assert_eq!(truth.citations.cite_only_author_year, 2);
        assert_eq!(
            truth.citations.cited_keys,
            [
                "alpha",
                "gamma",
                "Alpha",
                "aamand2021classifying",
                "alber2004geometric",
                "gamma",
                "gamma"
            ]
        );
        assert!(
            truth.body_text.contains("Appendix and ."),
            "{}",
            truth.body_text
        );

        // Without the `.bib` there is nothing to merge.
        let bbl_only = ground_truth(&files(dir.path(), &["main.tex"], &["app.bbl"], &[])).unwrap();
        assert_eq!(bbl_only.method, "bbl");
        assert_eq!(bbl_only.references.len(), 3);
    }

    #[test]
    fn ground_truth_partial_bbl_without_newcites_merges_below_half_coverage() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("main.tex"), MAIN_TEX).unwrap();
        fs::write(dir.path().join("main.bbl"), BBL).unwrap();
        fs::write(dir.path().join("refs.bib"), REFS_BIB).unwrap();
        let truth = ground_truth(&files(
            dir.path(),
            &["main.tex"],
            &["main.bbl"],
            &["refs.bib"],
        ))
        .unwrap();
        assert_eq!(truth.method, "bbl+bib");
        assert_eq!(truth.references.len(), 5);
    }

    #[test]
    fn ground_truth_nocite_all_multibib_merges_every_missing_bib_entry() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("main.tex"), NOCITE_MULTIBIB_TEX).unwrap();
        fs::write(dir.path().join("app.bbl"), APP_BBL).unwrap();
        fs::write(dir.path().join("refs.bib"), MAIN_REFS_BIB).unwrap();

        let cites = parse_cites(NOCITE_MULTIBIB_TEX);
        assert!(cites.nocite_all);
        assert!(cites.nocite_keys.is_empty());
        assert!(cites.cited_keys.is_empty());
        assert_eq!(cites.cite_commands, 0);

        let partial = ground_truth(&files(
            dir.path(),
            &["main.tex"],
            &["app.bbl"],
            &["refs.bib"],
        ))
        .unwrap();
        assert_eq!(partial.method, "bbl+bib");
        let keys: Vec<&str> = partial.references.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(keys, ["app1", "app2", "m1", "m2", "m3", "m4"]);
        assert_eq!(partial.references[1].source, TruthSource::Bbl);
        assert_eq!(partial.references[2].source, TruthSource::Bib);
        assert!(!partial.body_text.contains('*'), "{}", partial.body_text);

        // Every `.bbl` shipped: nothing is merged.
        fs::write(dir.path().join("main.bbl"), MAIN_BBL).unwrap();
        let complete = ground_truth(&files(
            dir.path(),
            &["main.tex"],
            &["app.bbl", "main.bbl"],
            &["refs.bib"],
        ))
        .unwrap();
        assert_eq!(complete.method, "bbl");
        assert_eq!(complete.references.len(), 6);
        assert!(
            complete
                .references
                .iter()
                .all(|r| r.source == TruthSource::Bbl)
        );
    }

    #[test]
    fn parse_cites_generic_cite_commands() {
        let tex = r"\newcommand{\citemine}[1]{\citep{#1}}
\def\citeapp#1{x}
A \citeapp{k1, k2} B \citemain*[see]{k3} C \citeA{k4} D \Parencites{k5}{k6}
E \citeps{k7} F \citeyearpar{k8} G \Citeauthor{k9} H \citetext{free text}
I \citestyle{acl} J \citeweird{not a key} K \citenum{k10}";
        let cites = parse_cites(tex);
        assert_eq!(
            cites.cited_keys,
            ["k1", "k2", "k3", "k4", "k5", "k7", "k8", "k9", "k10"]
        );
        assert_eq!(cites.cite_commands, 8);
        assert_eq!(cites.cite_only_author_year, 2);
    }

    #[test]
    fn escaped_underscore_in_doi_is_unescaped() {
        let bib = r"@article{wiley,
  title = {Escaped},
  year = {2019},
  doi = {10.1002/{\_}sim.8123},
}
@article{url, title={U}, url={https://doi.org/10.1002/ab\_cd.2020}, year={2020}}";
        let refs = parse_bib(bib);
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].doi.as_deref(), Some("10.1002/_sim.8123"));
        assert_eq!(refs[0].year, Some(2019));
        assert_eq!(refs[1].doi.as_deref(), Some("10.1002/ab_cd.2020"));
        assert_eq!(refs[1].year, Some(2020));
        assert_eq!(
            find_doi(r"\doi{10.1002/{\_}x.2019}").as_deref(),
            Some("10.1002/_x.2019")
        );
        assert_eq!(first_year(r"10.1002/{\_}2019.12, 2018"), Some(2018));
    }

    #[test]
    fn ground_truth_uses_inline_thebibliography() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("main.tex"), INLINE_TEX).unwrap();
        let truth = ground_truth(&files(dir.path(), &["main.tex"], &[], &[])).unwrap();
        assert_eq!(truth.method, "bbl");
        assert_eq!(truth.references.len(), 1);
        assert_eq!(truth.references[0].key, "k1");
        assert_eq!(truth.references[0].year, Some(1999));
        assert_eq!(truth.paper, TruthPaper::default());
        assert!(
            !truth.body_text.contains("Some Author"),
            "{}",
            truth.body_text
        );
    }

    const ARTICLE_TEX: &str = r"\documentclass{article}
\newcommand{\sys}{FooNet}
\title{\sys: Fast Things\thanks{Accepted at X. Published version doi:10.1234/abcd.5678.}}
\date{}
\author{Mikkel Abrahamsen\thanks{University of Copenhagen, Denmark.} \and
Bartosz Walczak\thanks{Jagiellonian University, Krak\'ow, Poland.}}
\begin{document}
\maketitle
\begin{abstract}
We study things, see doi:10.9999/not.this.one.
\end{abstract}
\section{Introduction}
\author{Not An Author}
\end{document}
";

    const IEEE_FRONT_TEX: &str = r"\documentclass[conference]{IEEEtran}
\begin{document}
\title{Robust Sensing with\\ Sparse Arrays}
\author{\IEEEauthorblockN{Alice M. Smith\IEEEauthorrefmark{1}, Bob Jones\IEEEauthorrefmark{2}}
\IEEEauthorblockA{\IEEEauthorrefmark{1}\textit{Dept. of Electrical Engineering},
Stanford University, CA, USA\\ alice@stanford.edu}
\and
\IEEEauthorblockN{Carlos de la Cruz}
\IEEEauthorblockA{\textit{Google Research}\\ Mountain View, USA}}
\maketitle
\begin{abstract}
Abstract text.
\end{abstract}
\section{Introduction}
";

    const ACM_FRONT_TEX: &str = r"\documentclass[sigconf]{acmart}
\acmDOI{10.1145/3580305.3599999}
\begin{document}
\title{H-FedSN: Personalized Sparse Networks for Hierarchical Federated Learning}
\author{Jiechao Gao}
\authornote{Both authors contributed equally.}
\affiliation{%
  \institution{Stanford University}
  \city{Stanford}
  \country{USA}}
\email{jiechao@stanford.edu}
\author{Yuangang Li\textsuperscript{*}}
\authornotemark[1]
\affiliation{\institution{University of California, Irvine}\country{USA}}
\email{yuanganl@uci.edu}
\author{Jie Wang}
\orcid{0000-0002-1825-0097}
\affiliation{\institution{Stanford University}}
\begin{abstract}
Abstract text.
\end{abstract}
\maketitle
\section{Introduction}
";

    const ELSARTICLE_FRONT_TEX: &str = r"\documentclass[preprint,12pt]{elsarticle}
\begin{document}
\begin{frontmatter}
\title{Graph Neural Networks for Traffic Forecasting\tnoteref{t1}}
\tnotetext[t1]{This work was funded by the Agency.}
\author[inst1]{Jane Q. Doe\corref{cor1}}
\ead{jane.doe@example.org}
\cortext[cor1]{Corresponding author}
\author[inst1,inst2]{Richard van der Berg}
\author[inst2]{Li Wei\fnref{fn1}}
\fntext[fn1]{Now at Tsinghua University.}
\affiliation[inst1]{organization={Delft University of Technology}, city={Delft}}
\address[inst2]{Tsinghua University, Beijing, China}
\begin{abstract}
Abstract text.
\end{abstract}
\end{frontmatter}
\section{Introduction}
";

    const ICML_FRONT_TEX: &str = r"\documentclass{article}
\usepackage{icml2024}
\icmltitlerunning{Scaling Sparse Autoencoders}
\begin{document}
\twocolumn[
\icmltitle{Scaling Sparse Autoencoders to Many Features}
\begin{icmlauthorlist}
\icmlauthor{Leo Gao}{oai}
\icmlauthor{Tom Dupr\'e la Tour}{oai}
\icmlauthor{Henk Tillman}{oai}
\end{icmlauthorlist}
\icmlaffiliation{oai}{OpenAI, San Francisco, USA}
\icmlcorrespondingauthor{Leo Gao}{lg@openai.com}
\vskip 0.3in
]
\printAffiliationsAndNotice{}
\begin{abstract}
Abstract text.
\end{abstract}
\section{Introduction}
";

    const INLINE_AFFIL_TEX: &str = r"\documentclass{article}
\newcommand{\JMorcid}{\orcidlink{0000-0003-4850-9239}}
\title{
    \begin{minipage}{0.85\textwidth}
        \centering
        \raisebox{-1.7\height}{\shortstack{HintEval: A Toolkit for Hint Generation \\ and Evaluation}}
    \end{minipage}
}
\author{Jamshid Mozafari\thanks{\, Corresponding Author.}\JMorcid, Bhawna Piryani$^{1}$, Adam Jatowt \\
  University of Innsbruck, Innsbruck, Austria \\
  \texttt{\{jamshid.mozafari, adam.jatowt\}@uibk.ac.at} \\
}
\begin{document}
\maketitle
\end{document}
";

    #[test]
    fn paper_truth_article_with_and() {
        let paper = paper_truth(ARTICLE_TEX);
        assert_eq!(paper.title.as_deref(), Some("FooNet: Fast Things"));
        assert_eq!(paper.authors, ["Mikkel Abrahamsen", "Bartosz Walczak"]);
        assert_eq!(paper.doi.as_deref(), Some("10.1234/abcd.5678"));
        assert_eq!(paper.arxiv_id, None);
    }

    #[test]
    fn paper_truth_ieeetran_author_blocks() {
        let paper = paper_truth(IEEE_FRONT_TEX);
        assert_eq!(
            paper.title.as_deref(),
            Some("Robust Sensing with Sparse Arrays")
        );
        assert_eq!(
            paper.authors,
            ["Alice M. Smith", "Bob Jones", "Carlos de la Cruz"]
        );
        assert_eq!(paper.doi, None);
    }

    #[test]
    fn paper_truth_acm_author_and_affiliation() {
        let paper = paper_truth(ACM_FRONT_TEX);
        assert_eq!(
            paper.title.as_deref(),
            Some("H-FedSN: Personalized Sparse Networks for Hierarchical Federated Learning")
        );
        assert_eq!(paper.authors, ["Jiechao Gao", "Yuangang Li", "Jie Wang"]);
        assert_eq!(paper.doi.as_deref(), Some("10.1145/3580305.3599999"));
    }

    #[test]
    fn paper_truth_elsarticle_frontmatter() {
        let paper = paper_truth(ELSARTICLE_FRONT_TEX);
        assert_eq!(
            paper.title.as_deref(),
            Some("Graph Neural Networks for Traffic Forecasting")
        );
        assert_eq!(
            paper.authors,
            ["Jane Q. Doe", "Richard van der Berg", "Li Wei"]
        );
        assert_eq!(paper.doi, None);
    }

    #[test]
    fn paper_truth_icml_author_list() {
        let paper = paper_truth(ICML_FRONT_TEX);
        assert_eq!(
            paper.title.as_deref(),
            Some("Scaling Sparse Autoencoders to Many Features")
        );
        assert_eq!(
            paper.authors,
            ["Leo Gao", "Tom Dupré la Tour", "Henk Tillman"]
        );
        assert_eq!(paper.doi, None);
        assert_eq!(paper.arxiv_id, None);
    }

    #[test]
    fn paper_truth_inline_affiliation_lines_and_layout_title() {
        let paper = paper_truth(INLINE_AFFIL_TEX);
        assert_eq!(
            paper.title.as_deref(),
            Some("HintEval: A Toolkit for Hint Generation and Evaluation")
        );
        assert_eq!(
            paper.authors,
            ["Jamshid Mozafari", "Bhawna Piryani", "Adam Jatowt"]
        );
        assert_eq!(paper_truth("no title here"), TruthPaper::default());
    }

    #[test]
    fn paper_truth_ignores_cited_dois_in_sectionless_papers() {
        let tex = r"\documentclass{revtex4-2}
\begin{document}
\title{A Short Letter}
\author{Ada Lovelace}
\affiliation{Analytical Society}
\maketitle
Body text \cite{k}.
\begin{thebibliography}{1}
\bibitem{k} C. Babbage, \doi{10.1000/cited.work}; arXiv:\arxiv{2101.00001}.
\end{thebibliography}
\end{document}
";
        let paper = paper_truth(tex);
        assert_eq!(paper.title.as_deref(), Some("A Short Letter"));
        assert_eq!(paper.authors, ["Ada Lovelace"]);
        assert_eq!(paper.doi, None);
        assert_eq!(paper.arxiv_id, None);
    }

    /// The `\title` forms of the three papers whose `/Info` title differs
    /// from the printed one (arXiv:2505.16990, 2305.13843, 2608.28714): the
    /// truth is the printed title.
    #[test]
    fn paper_truth_titles_with_line_breaks_and_optional_arguments() {
        let acl = r"\documentclass[11pt]{article}
\title{Dimple: Discrete Diffusion Parallel Generation for \\Large Multimodal Modal}
\author{Runpeng Yu \and Xinyin Ma}
\begin{document}
\maketitle
\end{document}
";
        assert_eq!(
            paper_truth(acl).title.as_deref(),
            Some("Dimple: Discrete Diffusion Parallel Generation for Large Multimodal Modal")
        );
        let cas = r"\documentclass[a4paper,fleqn]{cas-dc}
\shorttitle{Advances and Challenges of Multi-task Learning Method in Recommender Systems: A Survey}
\title[mode = title]{Advances and Challenges of Multi-task Learning Method in Recommender Systems: A Survey}
\begin{document}
\maketitle
\end{document}
";
        assert_eq!(
            paper_truth(cas).title.as_deref(),
            Some(
                "Advances and Challenges of Multi-task Learning Method in Recommender Systems: A Survey"
            )
        );
        let ieee = r"\documentclass[journal]{IEEEtran}
\begin{document}
\title{Evaluating the Safety of Deep Learning-Based Brain MRI Reconstruction:\\ A Systematic Review of Current Evaluation Practices}
\author{Dat~Tat~Mai,
        and~James~Jin~Kang%
\thanks{Dat Tat Mai is with RMIT University Vietnam.}}
\maketitle
\end{document}
";
        assert_eq!(
            paper_truth(ieee).title.as_deref(),
            Some(
                "Evaluating the Safety of Deep Learning-Based Brain MRI Reconstruction: \
                 A Systematic Review of Current Evaluation Practices"
            )
        );
    }

    #[test]
    fn person_name_heuristic() {
        assert_eq!(person_name("Jane Q. Doe").as_deref(), Some("Jane Q. Doe"));
        assert_eq!(person_name(" Gao* ").as_deref(), None);
        assert_eq!(
            person_name("Richard van der Berg").as_deref(),
            Some("Richard van der Berg")
        );
        assert_eq!(person_name("Stanford University"), None);
        assert_eq!(person_name("alice@stanford.edu"), None);
        assert_eq!(person_name("Firstname1 Lastname1"), None);
        assert_eq!(person_name("MIT CSAIL"), None);
        assert_eq!(person_name("van Gogh"), None);
    }

    use std::fmt::Write as _;

    /// A `thebibliography` list with one minimal entry per key.
    fn bbl_with(keys: &[&str]) -> String {
        let mut out = String::from("\\begin{thebibliography}{9}\n");
        for key in keys {
            let _ = write!(
                out,
                "\\bibitem{{{key}}} Some Author.\n\\newblock Title {key}.\n\\newblock Venue, 2001.\n"
            );
        }
        out.push_str("\\end{thebibliography}\n");
        out
    }

    fn truth_keys(truth: &GroundTruth) -> Vec<&str> {
        truth.references.iter().map(|r| r.key.as_str()).collect()
    }

    #[test]
    fn remove_disabled_drops_comment_environments_and_iffalse_blocks() {
        let tex = r"A \begin{comment} \cite{c1} \end{comment} B
\iffalse \newif\ifdraft \ifx\a\b \cite{c2} \fi \fill \cite{c3} \fi C
\iffalse \cite{c4} \else D \cite{k1} \fi E
\iftrue F \fi \ifthenelse{\boolean{x}}{G}{H} \figurename{} I
\iffalse never closed \cite{k2}";
        let out = remove_disabled(tex);
        for gone in ["c1", "c2", "c3", "c4"] {
            assert!(!out.contains(gone), "{gone}: {out}");
        }
        for kept in [
            "A ",
            " B",
            " C",
            "D \\cite{k1}",
            "E",
            "\\iftrue F",
            "\\ifthenelse",
            "I",
        ] {
            assert!(out.contains(kept), "{kept}: {out}");
        }
        assert!(out.contains("never closed \\cite{k2}"), "{out}");
        assert_eq!(parse_cites(tex).cited_keys, ["k1", "k2"]);
    }

    #[test]
    fn ground_truth_ignores_cites_in_comment_environments_of_input_files() {
        let dir = tempfile::tempdir().unwrap();
        let sections = dir.path().join("sections");
        fs::create_dir_all(&sections).unwrap();
        fs::write(
            dir.path().join("main.tex"),
            r"\documentclass{acmart}
\begin{document}
See \cite{alpha}.
\input{sections/related_work}
\iffalse
Old text \cite{beta}.
\fi
\bibliography{refs}
\end{document}
",
        )
        .unwrap();
        fs::write(
            sections.join("related_work.tex"),
            r"Addressing these gaps is essential \citep{gamma}.

\begin{comment}
    \subsection{Extra related work}
Task automation is not a new field \citep{beta, NEURIPS2024_0b82662b}.
\input{sections/hidden}
\end{comment}
",
        )
        .unwrap();
        fs::write(sections.join("hidden.tex"), r"\cite{beta}").unwrap();
        fs::write(dir.path().join("refs.bib"), REFS_BIB).unwrap();
        let tex = [
            "main.tex",
            "sections/hidden.tex",
            "sections/related_work.tex",
        ];
        let truth = ground_truth(&files(dir.path(), &tex, &[], &["refs.bib"])).unwrap();
        assert_eq!(truth.citations.cited_keys, ["alpha", "gamma"]);
        assert_eq!(truth.citations.cite_commands, 2);
        assert_eq!(truth.method, "bib-cited");
        assert_eq!(truth_keys(&truth), ["alpha", "gamma"]);
        assert!(
            !truth.body_text.contains("Task automation"),
            "{}",
            truth.body_text
        );
    }

    #[test]
    fn ground_truth_bibunits_uses_unit_bbls_in_order_and_ignores_stale_bbl() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("main.tex"),
            r"\documentclass{sn-jnl}
\usepackage{bibunits}
\defaultbibliography{references}
\begin{document}
\begin{bibunit}
Intro \citep{u1a,shared}.
\putbib
\end{bibunit}
\begin{bibunit}
Methods \citep{u2a,shared} and \citep{u10a}.
\putbib
\end{bibunit}
\end{document}
",
        )
        .unwrap();
        fs::write(dir.path().join("bu1.bbl"), bbl_with(&["u1a", "shared"])).unwrap();
        fs::write(dir.path().join("bu2.bbl"), bbl_with(&["u2a", "shared"])).unwrap();
        fs::write(dir.path().join("bu10.bbl"), bbl_with(&["u10a"])).unwrap();
        fs::write(dir.path().join("main.bbl"), bbl_with(&["stale1", "stale2"])).unwrap();
        let bbl = ["bu1.bbl", "bu10.bbl", "bu2.bbl", "main.bbl"];
        let truth = ground_truth(&files(dir.path(), &["main.tex"], &bbl, &[])).unwrap();
        assert_eq!(truth.method, "bbl");
        assert_eq!(
            truth_keys(&truth),
            ["u1a", "shared", "u2a", "shared", "u10a"]
        );

        // Without `\putbib` every `.bbl` is still read.
        fs::write(dir.path().join("plain.tex"), MAIN_TEX).unwrap();
        let plain = ground_truth(&files(dir.path(), &["plain.tex"], &bbl, &[])).unwrap();
        assert_eq!(plain.references.len(), 7);
    }

    #[test]
    fn ground_truth_merges_bib_when_inline_list_covers_under_half() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("main.tex"),
            r"\documentclass{informs4}
\begin{document}
Advice \citep{alpha,beta} and \citep{gamma}.
\bibliographystyle{informs2014}
\bibliography{refs}

\include{appendix}

\end{document}
",
        )
        .unwrap();
        fs::write(
            dir.path().join("appendix.tex"),
            r"Hardness follows from \citet{johnson1979computers}.

\begin{thebibliography}{9}
\bibitem[{Johnson \protect\BIBand{} Garey(1979)}]{johnson1979computers}
Johnson DS, Garey MR (1979) \emph{Computers and {I}ntractability: A {G}uide to
  the {T}heory of {N}P-{C}ompleteness} (WH Freeman).
\end{thebibliography}
",
        )
        .unwrap();
        fs::write(dir.path().join("refs.bib"), REFS_BIB).unwrap();
        let tex = ["appendix.tex", "main.tex"];
        let truth = ground_truth(&files(dir.path(), &tex, &[], &["refs.bib"])).unwrap();
        assert_eq!(truth.method, "bbl+bib");
        assert_eq!(
            truth_keys(&truth),
            ["johnson1979computers", "alpha", "beta", "gamma"]
        );
        assert_eq!(truth.references[0].source, TruthSource::Bbl);
        assert_eq!(truth.references[0].year, Some(1979));
        assert_eq!(truth.references[1].source, TruthSource::Bib);
    }

    #[test]
    fn ground_truth_concatenates_readme_toplevel_documents() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("00README.json"),
            r#"{
   "sources" : [
      { "usage" : "toplevel", "filename" : "paper.tex" },
      { "usage" : "toplevel", "filename" : "./paper.tex" },
      { "usage" : "ignore", "filename" : "refs.bib" },
      { "usage" : "toplevel", "filename" : "unlisted.tex" },
      { "usage" : "toplevel", "filename" : "a_si.tex" }
   ],
   "spec_version" : 1
}"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("paper.tex"),
            r"\documentclass{article}
\title{Main Paper Title}
\begin{document}
\maketitle
See \cite{alpha} and \cite{gamma}.
\bibliography{refs}
\end{document}
",
        )
        .unwrap();
        fs::write(
            dir.path().join("a_si.tex"),
            r"\documentclass{article}
\title{Supporting Information}
\begin{document}
\maketitle
Data from \cite{gamma} and \cite{beta, gamma}.
\bibliography{refs}
\end{document}
",
        )
        .unwrap();
        fs::write(dir.path().join("refs.bib"), REFS_BIB).unwrap();
        fs::write(
            dir.path().join("unlisted.tex"),
            r"\begin{document}\cite{alpha}\bibliography{refs}\end{document}",
        )
        .unwrap();
        let tree = files(dir.path(), &["a_si.tex", "paper.tex"], &[], &["refs.bib"]);
        let truth = ground_truth(&tree).unwrap();
        assert_eq!(truth.method, "bib-cited");
        assert_eq!(truth_keys(&truth), ["alpha", "gamma", "beta", "gamma"]);
        assert_eq!(
            truth.citations.cited_keys,
            ["alpha", "gamma", "gamma", "beta", "gamma"]
        );
        assert_eq!(truth.citations.cite_commands, 4);
        assert_eq!(truth.paper.title.as_deref(), Some("Main Paper Title"));
        assert!(truth.body_text.contains("Data from"), "{}", truth.body_text);

        // Without the manifest the first `.tex` with `\begin{document}` wins.
        fs::remove_file(dir.path().join("00README.json")).unwrap();
        let single = ground_truth(&tree).unwrap();
        assert_eq!(truth_keys(&single), ["beta", "gamma"]);
        assert_eq!(
            single.paper.title.as_deref(),
            Some("Supporting Information")
        );
    }

    #[test]
    fn ground_truth_size_includes_paper_metadata() {
        let truth = GroundTruth {
            references: Vec::new(),
            citations: TruthCitations::default(),
            method: String::new(),
            body_text: "body".to_owned(),
            paper: TruthPaper {
                title: Some("title".to_owned()),
                authors: vec!["Ada Lovelace".to_owned(), "Grace Hopper".to_owned()],
                doi: Some("10.1/example".to_owned()),
                arxiv_id: Some("2601.00001".to_owned()),
            },
        };
        assert_eq!(
            ground_truth_bytes(&truth),
            "bodytitleAda LovelaceGrace Hopper10.1/example2601.00001".len()
        );
    }

    #[test]
    fn ground_truth_rejects_oversized_bibliography_before_reading_it() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("main.tex"),
            r"\begin{document}\cite{x}\bibliography{refs}\end{document}",
        )
        .unwrap();
        let bbl_path = dir.path().join("main.bbl");
        let bbl = fs::File::create(&bbl_path).unwrap();
        bbl.set_len((MAX_GROUND_TRUTH_BYTES + 1) as u64).unwrap();

        let result = ground_truth(&files(dir.path(), &["main.tex"], &["main.bbl"], &[]));
        assert!(matches!(result, Err(TruthError::ResourceLimit)));
    }

    #[test]
    fn ground_truth_rejects_oversized_fallback_main_before_reading_it() {
        let dir = tempfile::tempdir().unwrap();
        let main_path = dir.path().join("main.tex");
        let main = fs::File::create(&main_path).unwrap();
        main.set_len((MAX_TOPLEVEL_SOURCE_BYTES + 1) as u64)
            .unwrap();

        let result = ground_truth(&files(dir.path(), &["main.tex"], &[], &[]));
        assert!(matches!(result, Err(TruthError::ResourceLimit)));
    }
}
