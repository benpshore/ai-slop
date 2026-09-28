//! Reference and citation ground truth taken from an `arXiv` e-print's `LaTeX`
//! source: the typeset bibliography (`.bbl`, one `\bibitem` per printed entry,
//! or a `biblatex` `\entry`), the database (`.bib`) filtered to the keys the
//! paper cites, and the `\cite` commands themselves. Also a rough "detex" of
//! the body text for word-alignment diagnostics.
//!
//! Everything here is best-effort text processing of author-written sources.
//! A field stays `None` unless the source contains it; nothing is invented.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use unicode_normalization::UnicodeNormalization;

use crate::corpus::LatexFiles;

/// Deepest `\input` nesting that is still inlined.
const MAX_INPUT_DEPTH: u32 = 5;
/// Upper bound on zero-argument macros expanded in the body text.
const MAX_MACROS: usize = 200;
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
    /// Keys from `\nocite{...}` (excluding `*`).
    pub nocite_keys: Vec<String>,
    /// `\nocite{*}` was present.
    pub nocite_all: bool,
}

/// Ground truth for one paper.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroundTruth {
    pub references: Vec<TruthReference>,
    pub citations: TruthCitations,
    /// `bbl`, `bib-cited` or `bib-all`.
    pub method: String,
    /// Detexed body for alignment diagnostics; may be empty.
    pub body_text: String,
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

fn year_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?:^|\D)((?:19|20)\d{2})(?:\D|$)").expect("valid regex"))
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

fn input_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\\(?:input|include|subfile)\s*\{([^}]*)\}").expect("valid regex")
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
/// its contents; labels, references, spacing and citation commands are
/// dropped with their arguments; any other `\command` disappears and its
/// brace arguments are kept as text. Whitespace is collapsed and trimmed.
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
    if let Some(text) = special_letter(name) {
        out.push_str(text);
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

/// First `19xx`/`20xx` in `text`, ignoring digits inside URLs, DOIs and `arXiv` ids.
fn first_year(text: &str) -> Option<u16> {
    let masked = year_mask_re().replace_all(text, " ");
    year_re()
        .captures(&masked)
        .and_then(|caps| caps.get(1))
        .and_then(|m| m.as_str().parse::<u16>().ok())
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

/// First DOI (`10.xxxx/...`) in raw or detexed text.
fn find_doi(raw: &str) -> Option<String> {
    doi_re().find(raw).and_then(|m| tidy_doi(m.as_str()))
}

/// DOI from a `doi.org` URL only.
fn doi_from_url(url: &str) -> Option<String> {
    doi_url_re()
        .captures(url)
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
/// the first `19xx`/`20xx` outside URLs and identifiers, `doi` comes from
/// `\doi{..}`, `doi:`, `https://doi.org/` or a bare `10.xxxx/..`, `arxiv_id`
/// from `arXiv:..`, `/abs/` or `\eprint{..}`. When the item has `\newblock`s
/// the first segment gives the authors and the second the title (ACM's
/// `\showarticletitle{..}` is preferred when present); otherwise both stay
/// empty. Files without `\bibitem` are tried as `biblatex` `\entry` blocks.
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
        (
            split_bbl_authors(&latex_to_text(segments[0])),
            bbl_title(rest, segments[1]),
        )
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
/// else the detexed second `\newblock` segment without its final period.
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
    if title.is_empty() {
        None
    } else {
        Some(title.to_owned())
    }
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
    /// It was `\nocite`.
    nocite: bool,
    keys: Vec<String>,
}

/// All `\cite`-family and `\nocite` commands with their key lists.
fn scan_cites(clean: &str) -> Vec<CiteCommand> {
    let mut found = Vec::new();
    for caps in command_re().captures_iter(clean) {
        let Some(whole) = caps.get(0) else { continue };
        let name = &caps[1];
        let nocite = name == "nocite";
        if !nocite && !CITE_COMMANDS.contains(&name) {
            continue;
        }
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
        let keys: Vec<String> = clean[inner]
            .split(',')
            .map(str::trim)
            .filter(|k| !k.is_empty() && !k.contains('#'))
            .map(str::to_owned)
            .collect();
        found.push(CiteCommand {
            span: whole.start()..past_group,
            nocite,
            keys,
        });
    }
    found
}

/// Citation commands in a `.tex` source.
///
/// Recognises `\cite`, `\citep`, `\citet`, `\citealp`, `\citealt`,
/// `\citeauthor`, `\citeyear`, `\citeyearpar`, `\citenum`, `\parencite`,
/// `\textcite`, `\autocite`, `\footcite` and friends, with `*` and up to two
/// `[...]` arguments before `{keys}`; `\nocite{keys}` and `\nocite{*}`.
/// Commented text is ignored. Keys are trimmed and split on `,`. Only the
/// first key group of multi-group commands (`\cites{a}{b}`) is read.
pub fn parse_cites(tex: &str) -> TruthCitations {
    let clean = strip_comments(tex);
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
/// Takes the text between `\begin{document}` and `\end{document}`, removes
/// citation commands, floats and display environments (figure, table,
/// tabular, algorithm, equation, align, listings, verbatim, ...) and `\[...\]`,
/// turns `\section{X}` (and chapter, subsection, paragraph) into a paragraph
/// of its own, expands zero-argument `\newcommand` macros, then detexes each
/// blank-line-separated paragraph. Paragraphs are joined with `"\n\n"`.
pub fn body_text(main_tex: &str) -> String {
    let clean = strip_comments(main_tex);
    let macros = collect_macros(&clean);
    let body = document_body(&clean);
    let body = remove_cites(body);
    let body = remove_environments(&body);
    let body = remove_display_math(&body);
    let body = replace_headings(&body);
    let body = par_re().replace_all(&body, "\n\n");
    let body = expand_macros(&expand_macros(&body, &macros), &macros);
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

/// Environments whose contents never appear as running text.
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
            | "minted"
            | "tikzpicture"
            | "wrapfigure"
            | "wraptable"
            | "sidewaystable"
            | "sidewaysfigure"
            | "subfigure"
            | "thebibliography"
            | "filecontents"
            | "comment"
    )
}

fn remove_environments(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut last = 0;
    for caps in begin_re().captures_iter(body) {
        let Some(whole) = caps.get(0) else { continue };
        if whole.start() < last || !is_dropped_env(&caps[1]) {
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

/// Remove `\[ ... \]` display math.
fn remove_display_math(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut i = 0;
    while let Some(open) = find_control(body, i, b'[') {
        out.push_str(&body[i..open]);
        i = find_control(body, open + 2, b']').map_or(body.len(), |close| close + 2);
    }
    out.push_str(&body[i..]);
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
// \input resolution and ground truth
// ---------------------------------------------------------------------------

/// Inline `\input{f}`, `\include{f}` and `\subfile{f}` relative to `root`
/// (`.tex` is added when the name has no extension), recursively while
/// `depth <= 5`. Missing files are left as they are. Comments are removed.
pub fn resolve_inputs(root: &Path, main_tex: &str, depth: u32) -> String {
    let clean = strip_comments(main_tex);
    if depth > MAX_INPUT_DEPTH {
        return clean;
    }
    let mut out = String::with_capacity(clean.len());
    let mut last = 0;
    for caps in input_re().captures_iter(&clean) {
        let Some(whole) = caps.get(0) else { continue };
        let name = caps[1].trim().trim_matches('"');
        let Some(content) = read_input(root, name) else {
            continue;
        };
        out.push_str(&clean[last..whole.start()]);
        out.push('\n');
        out.push_str(&resolve_inputs(root, &content, depth + 1));
        out.push('\n');
        last = whole.end();
    }
    out.push_str(&clean[last..]);
    out
}

fn read_input(root: &Path, name: &str) -> Option<String> {
    if name.is_empty() || Path::new(name).is_absolute() || name.contains("..") {
        return None;
    }
    let direct = root.join(name);
    let with_tex = direct.with_extension("tex");
    let candidates: [&Path; 2] = if Path::new(name).extension().is_some() {
        [&direct, &with_tex]
    } else {
        [&with_tex, &direct]
    };
    candidates
        .iter()
        .find_map(|path| fs::read(path).ok())
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

fn read_lossy(path: &Path) -> Result<String, TruthError> {
    Ok(String::from_utf8_lossy(&fs::read(path)?).into_owned())
}

/// Keep the first entry per key (keys compare case-insensitively, as `BibTeX` does).
fn dedupe_keys(entries: Vec<TruthReference>) -> Vec<TruthReference> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    entries
        .into_iter()
        .filter(|entry| seen.insert(entry.key.to_ascii_lowercase()))
        .collect()
}

/// Ground truth for one paper's source tree.
///
/// The main file is the first `.tex` containing `\begin{document}`
/// ([`TruthError::NoMainTex`] otherwise); `\input`s are resolved relative to
/// its directory. Citations and body text come from the merged source. The
/// bibliography is, in order of preference: all `.bbl` files (method `bbl`),
/// a `thebibliography` environment inline in the source (also `bbl`), or the
/// `.bib` files filtered to cited and `\nocite`d keys (`bib-cited`), or every
/// `.bib` entry when `\nocite{*}` is present (`bib-all`).
/// [`TruthError::NoBibliography`] when none of these yields an entry.
pub fn ground_truth(files: &LatexFiles) -> Result<GroundTruth, TruthError> {
    let mut main: Option<(PathBuf, String)> = None;
    for path in &files.tex {
        let text = read_lossy(path)?;
        if strip_comments(&text).contains("\\begin{document}") {
            main = Some((path.clone(), text));
            break;
        }
    }
    let Some((main_path, main_text)) = main else {
        return Err(TruthError::NoMainTex);
    };
    let root: &Path = main_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(files.root.as_path());
    let merged = resolve_inputs(root, &main_text, 0);
    let citations = parse_cites(&merged);
    let body = body_text(&merged);

    let mut references = Vec::new();
    for path in &files.bbl {
        references.extend(parse_bbl(&read_lossy(path)?));
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
        let mut entries = Vec::new();
        for path in &files.bib {
            entries.extend(parse_bib(&read_lossy(path)?));
        }
        if entries.is_empty() {
            return Err(TruthError::NoBibliography);
        }
        if citations.nocite_all {
            references = dedupe_keys(entries);
            "bib-all"
        } else {
            let wanted: BTreeSet<String> = citations
                .cited_keys
                .iter()
                .chain(&citations.nocite_keys)
                .map(|k| k.to_ascii_lowercase())
                .collect();
            let cited_entries: Vec<TruthReference> = entries
                .into_iter()
                .filter(|entry| wanted.contains(&entry.key.to_ascii_lowercase()))
                .collect();
            references = dedupe_keys(cited_entries);
            "bib-cited"
        }
    } else {
        "bbl"
    };
    Ok(GroundTruth {
        references,
        citations,
        method: method.to_owned(),
        body_text: body,
    })
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

    const REFS_BIB: &str = r"@article{alpha, title={A}, year={2001}}
@article{beta, title={B}, year={2002}}
@article{gamma, title={G}, year={2003}}
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
        let cases: [(&str, &str); 18] = [
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
            (r"the {$\log n$} barrier", "the n barrier"),
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
        assert!(out.contains("Second paragraph with x^2."), "{out}");
        assert!(!out.contains("caption"), "{out}");
        assert!(!out.contains("mc^2"), "{out}");
        assert!(!out.contains("Trailing"), "{out}");
        assert!(!out.contains("refs"), "{out}");
        assert!(!out.contains("sec:intro"), "{out}");
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
    fn ground_truth_uses_inline_thebibliography() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("main.tex"), INLINE_TEX).unwrap();
        let truth = ground_truth(&files(dir.path(), &["main.tex"], &[], &[])).unwrap();
        assert_eq!(truth.method, "bbl");
        assert_eq!(truth.references.len(), 1);
        assert_eq!(truth.references[0].key, "k1");
        assert_eq!(truth.references[0].year, Some(1999));
        assert!(
            !truth.body_text.contains("Some Author"),
            "{}",
            truth.body_text
        );
    }
}
