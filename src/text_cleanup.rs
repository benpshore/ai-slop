//! Document-level text cleanup, run after reading order and before metadata
//! and citations: running headers, footers and page numbers, the rotated
//! `arXiv` margin stamp, sub/superscript fragment lines and line-end
//! hyphenation.
//!
//! The pass edits `PageText::lines` and `PageText::text` only. Spans are
//! evidence and are never changed or removed:
//! - a removed line (furniture or stamp) stays in `lines`, moved after the
//!   body lines, and only leaves `text`; the page gets the warning
//!   `furniture removed: N`;
//! - a script line merged into its base line hands its span indices to
//!   that line; a detached superscript citation number (`5`, `5–7`,
//!   `10,11`) or a subscript digit is written with Unicode super- or
//!   subscript characters (`literature.⁵`, `Initiative⁵⁻⁷`, `NH₃`);
//! - a hyphen join moves the first word of the next line onto the line that
//!   ends with the hyphen, in both line texts and page text.
//!
//! A page whose `text` is not its line texts joined by whitespace (so the
//! separators cannot be recovered) is left untouched. Lines after the last
//! one found in `text` are treated as furniture removed by an earlier run,
//! which keeps the pass idempotent.
//!
//! The pass also sets `Line::role` (it only tags; `text` keeps every line
//! that is not furniture): removed lines become `furniture`, table-of-contents
//! lines with dot leaders `toc`, lines opening with `Figure N:`-style labels
//! (or `Fig. 3 Overview`-style ones, a number and a capitalised word, when
//! they do not continue the paragraph above) `caption`, and on page 1 the
//! lines before the abstract `front` (the standalone `Abstract` line itself
//! `heading`), stopping at the first run of prose lines and skipping long
//! lines (or sentence lines of 12 words with a verb-like word) that are
//! neither affiliations nor lists of names (on page 2 as well when page 1
//! is a title page). On those pages licence and copyright blocks, `ACM
//! Reference Format:` and contact blocks, `Keywords` / `Index Terms` /
//! `CCS Concepts` blocks and lettered affiliation lines are `front` too.
//! Author biographies after the references (a name, then `received`, an
//! IEEE membership grade or a confirmed `is a`) are `biography`; lines under
//! a short footnote rule at a column foot, and a small-font run carried over
//! from a page that ended inside a footnote, are `footnote`. Only lines
//! still tagged `body` are retagged, except that `furniture` wins over any
//! tag.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::OnceLock;

use regex::Regex;

use crate::schema::{BBox, Line, PageText};

/// Share of the page height at the top and at the bottom that holds
/// running headers, footers and page numbers.
const EDGE_BAND: f32 = 0.08;
/// Wider edge band for running heads set further from the edge (LNCS-style
/// classes put them 11-14 % down). A line in this band but outside
/// `EDGE_BAND` needs the stronger repetition evidence of [`strongly_repeated`].
const EDGE_BAND_WIDE: f32 = 0.15;
/// Pages of one parity (recto or verso) a running head must repeat on.
const PARITY_MIN_PAGES: usize = 3;
/// Share of the document's pages, as `numerator / denominator` (40 %), a
/// running head must repeat on regardless of parity, and never fewer than
/// `PARITY_MIN_PAGES` pages.
const SHARE_NUMERATOR: usize = 2;
const SHARE_DENOMINATOR: usize = 5;
/// The abstract must start within this many non-furniture lines of page 1
/// for the front-matter rule to use it.
const FRONT_MAX_LINES: usize = 60;
/// Dot-leader runs a table-of-contents line has at least.
const TOC_MIN_LEADERS: usize = 4;
/// Fewest words in each line of the prose run that ends page-1 front
/// matter even without an `Abstract` line.
const FRONT_RUN_WORDS: usize = 10;
/// Consecutive prose lines of at least `FRONT_RUN_WORDS` words that end the
/// front matter.
const FRONT_RUN_LINES: usize = 2;
/// A page-1 line after the abstract (or on a page without one) with at
/// least this many words is never front matter unless it carries an
/// affiliation signal or reads like a list of names.
const FRONT_LONG_WORDS: usize = 14;
/// A page-1 line with at least this many words and verb evidence (see
/// [`is_long_body_line`]) is never front matter unless it carries an
/// affiliation signal or reads like a list of names.
const FRONT_SENTENCE_WORDS: usize = 12;
/// Fewest words in each line of the shorter prose run that also ends the
/// front matter (an abstract set in a narrow column).
const FRONT_SHORT_RUN_WORDS: usize = 6;
/// Consecutive prose lines of at least `FRONT_SHORT_RUN_WORDS` words that
/// end the front matter.
const FRONT_SHORT_RUN_LINES: usize = 3;
/// Substrings that mark an affiliation or contact line in the front matter.
const AFFILIATION_SIGNALS: [&str; 6] = [
    "@",
    "University",
    "Institute",
    "Department",
    "Laboratory",
    "Corresponding",
];
/// Fewest words in a line directly above a bare caption start (`Figure 3
/// The ...`) for that line to read as a paragraph the start continues.
const CAPTION_PARAGRAPH_WORDS: usize = 6;
/// Largest gap, in heights of the lower line, between a line and the bare
/// caption start below it for the two to belong to one paragraph.
const CAPTION_PARAGRAPH_GAP: f32 = 0.6;
const ROLE_BODY: &str = "body";
const ROLE_FURNITURE: &str = "furniture";
const ROLE_TOC: &str = "toc";
const ROLE_CAPTION: &str = "caption";
const ROLE_FRONT: &str = "front";
const ROLE_HEADING: &str = "heading";
const ROLE_BIOGRAPHY: &str = "biography";
const ROLE_FOOTNOTE: &str = "footnote";
/// Most lines of one front-matter block (licence, `ACM Reference Format:`,
/// contact information) tagged after its opening line.
const FRONT_BLOCK_MAX_LINES: usize = 12;
/// Most lines of a `Keywords` / `Index Terms` / `CCS Concepts` block,
/// its label line included.
const KEYWORDS_MAX_LINES: usize = 6;
/// Most words on a page 1 that is a title page (a highlights or cover page)
/// before the paper's own front matter on page 2.
const TITLE_PAGE_MAX_WORDS: usize = 300;
/// Words, after a lettered affiliation's letter, searched for an
/// institution word (see [`AFFILIATION_WORDS`]).
const AFFILIATION_WORD_REACH: usize = 3;
/// Fewest commas in a plain-letter affiliation line (`a Department of X,
/// University Y, City`).
const AFFILIATION_MIN_COMMAS: usize = 2;
/// Institution words that open a lettered affiliation.
const AFFILIATION_WORDS: [&str; 12] = [
    "Department",
    "School",
    "Faculty",
    "University",
    "Institute",
    "College",
    "Laboratory",
    "Center",
    "Centre",
    "Division",
    "Hospital",
    "Academy",
];
/// Most lines tagged `biography` from one biography start (or one
/// continuation paragraph).
const BIOGRAPHY_MAX_LINES: usize = 40;
/// Most name tokens before a biography cue.
const BIOGRAPHY_MAX_NAME_TOKENS: usize = 5;
/// Fewest name tokens before a biography cue.
const BIOGRAPHY_MIN_NAME_TOKENS: usize = 2;
/// Pages at the end of the document searched for biographies even before
/// (or without) a references heading.
const BIOGRAPHY_TAIL_PAGES: usize = 2;
/// Lowercase particles allowed inside a name (`Ludwig van Beethoven`).
const NAME_PARTICLES: [&str; 13] = [
    "de", "van", "von", "der", "den", "da", "del", "di", "la", "le", "du", "dos", "y",
];
/// Capitalised words that never open a person's name.
const NOT_NAMES: [&str; 24] = [
    "The",
    "This",
    "That",
    "These",
    "Those",
    "Our",
    "We",
    "It",
    "Its",
    "In",
    "On",
    "For",
    "A",
    "An",
    "Each",
    "Table",
    "Figure",
    "Algorithm",
    "Section",
    "Appendix",
    "Lemma",
    "Theorem",
    "Proof",
    "Model",
];
/// Cues after a name that start a biography on their own.
const BIOGRAPHY_STRONG_CUES: [&str; 2] = ["received", "was born"];
/// Cues after a name that start a biography when the line or the next one
/// also holds a [`BIOGRAPHY_CONFIRMATIONS`] word.
const BIOGRAPHY_WEAK_CUES: [&str; 5] = ["is a ", "is an ", "is the ", "is currently", "is with"];
/// Words that confirm a weak biography cue.
const BIOGRAPHY_CONFIRMATIONS: [&str; 14] = [
    "received",
    "degree",
    "Ph.D",
    "PhD",
    "M.Sc",
    "MSc",
    "B.Sc",
    "BSc",
    "rofessor",
    "esearch",
    "University",
    "Institute",
    "Foundation",
    "Laboratory",
];
/// Openers of a biography's later paragraph (right after a biography).
const BIOGRAPHY_PARAGRAPH_OPENERS: [&str; 7] = [
    "His research interests",
    "Her research interests",
    "He received",
    "She received",
    "He is ",
    "She is ",
    "Dr. ",
];
/// Largest font size, as a share of the page's body size, of a footnote
/// line (as in `regions`).
const FOOTNOTE_SIZE_RATIO: f32 = 0.92;
/// Share of the page height, from the bottom, where footnotes sit.
const FOOTNOTE_ZONE: f32 = 0.35;
/// Most lines in a footnote run continued from the previous page.
const FOOTNOTE_MAX_LINES: usize = 10;
/// Most lines below a footnote rule.
const RULED_FOOTNOTE_MAX_LINES: usize = 15;
/// Fewest words in a line that measures a page's body font size.
const BODY_SIZE_MIN_WORDS: usize = 6;
/// Fewest such lines needed to measure it.
const BODY_SIZE_MIN_LINES: usize = 3;
/// A figure box lower than this, in points, and at least
/// [`RULE_MIN_WIDTH`] wide is a horizontal rule.
const RULE_HEIGHT: f32 = 3.0;
/// Narrowest rule, in points.
const RULE_MIN_WIDTH: f32 = 30.0;
/// Widest footnote rule, as a share of the page width (a table rule set
/// across the text block is wider).
const RULE_MAX_PAGE_SHARE: f32 = 0.6;
/// Largest distance, in points, from a footnote rule down to the top of
/// the first footnote line.
const RULE_REACH: f32 = 24.0;
/// How far, in points, a footnote line may start left of its rule.
const RULE_X_BEFORE: f32 = 3.0;
/// How far, in points, a footnote line may start right of its rule's left
/// end (the indented first line of a note).
const RULE_X_INDENT: f32 = 20.0;
/// How far, in points, a footnote line's top may reach above its rule.
const RULE_OVERLAP: f32 = 1.0;
/// A repeated edge line counts as furniture only up to this multiple of the
/// document's median span size (a display title is not a running head).
const HEADER_SIZE_SLACK: f32 = 1.1;
/// A script line is at most this share of its base line's font size.
const SCRIPT_RATIO: f32 = 0.75;
/// Minimum vertical overlap with the base line, as a share of the script
/// line's own height.
const SCRIPT_OVERLAP: f32 = 0.5;
/// Longest script fragment, in characters and in words.
const SCRIPT_MAX_CHARS: usize = 16;
const SCRIPT_MAX_WORDS: usize = 3;
/// Maximum cumulative amount of existing line data copied while attaching
/// script fragments on one page.  A hostile page can otherwise make every
/// fragment target the same growing line and turn the pass quadratic.
const SCRIPT_MERGE_WORK_LIMIT: usize = 1_000_000;
/// Longest superscript or subscript fragment (`10, 11, 12`), in characters.
const SUPERSCRIPT_MAX_CHARS: usize = 12;
/// A detached superscript or subscript fragment (rule 3a) is at most this
/// share of its base line's font size: RSC sets 7 pt citation numbers on
/// 9 pt text (0.78), and 8 on 10 pt is the loosest common pair.
const SUPERSCRIPT_RATIO: f32 = 0.8;
/// Rounding slack on `SUPERSCRIPT_RATIO` for the superscript rule: sizes
/// taken from scaled text matrices come out as 5.98 on 7.97 for a 6 on 8 pt
/// pair.
const RATIO_SLACK: f32 = 0.005;
/// Distance from a span box's bottom up to its baseline, as a share of its
/// font size (the backends' descent estimate).
const DESCENT_SHARE: f32 = 0.2;
/// Window for a superscript's box bottom above the base line's baseline, in
/// base font sizes. A superscript sits so far above the base line's
/// x-height that its box barely overlaps the base line's box, and reading
/// order gives it a line of its own.
const RAISED_LOW: f32 = -0.2;
const RAISED_HIGH: f32 = 0.9;
/// Lowest box bottom of a subscript, in base font sizes from the baseline
/// (a subscript's bottom lies below `RAISED_LOW`).
const LOWERED_LOW: f32 = -0.7;
/// Typical box-bottom offsets of a superscript and a subscript, used to
/// pick between two candidate base lines (a subscript of one line also lies
/// in the superscript window of the line below).
const RAISED_IDEAL: f32 = 0.3;
const LOWERED_IDEAL: f32 = -0.4;
/// How far outside the base line's box a superscript may sit, in base font
/// sizes.
const SUPERSCRIPT_REACH: f32 = 0.5;
/// How far a superscript lying wholly beyond the base line's right edge
/// may sit from it, in base font sizes: a tolerance for a citation number
/// set after the final punctuation of a line, where nothing to its right
/// competes for it.
const SUPERSCRIPT_REACH_AFTER: f32 = 1.0;
/// Most baseline-index entries inspected for one detached superscript. This
/// bounds cleanup work for hostile pages with thousands of tiny or non-body
/// lines packed into the same baseline window.
const SUPERSCRIPT_SCAN_LIMIT: usize = 256;
/// Unicode superscript and subscript digits, indexed by value.
const SUPERSCRIPT_DIGITS: [char; 10] = [
    '\u{2070}', '\u{00B9}', '\u{00B2}', '\u{00B3}', '\u{2074}', '\u{2075}', '\u{2076}', '\u{2077}',
    '\u{2078}', '\u{2079}',
];
const SUBSCRIPT_DIGITS: [char; 10] = [
    '\u{2080}', '\u{2081}', '\u{2082}', '\u{2083}', '\u{2084}', '\u{2085}', '\u{2086}', '\u{2087}',
    '\u{2088}', '\u{2089}',
];
/// Superscript minus, written for `-`, `–` and `−` in a raised range.
const SUPERSCRIPT_MINUS: char = '\u{207B}';
/// A span is vertical text when its box is this many times taller than wide.
const VERTICAL_RATIO: f32 = 3.0;
/// Shortest line the vertical-text stamp rule applies to (a lone `l` or `1`
/// has a tall, narrow box too).
const VERTICAL_MIN_CHARS: usize = 10;
/// Hyphen characters that can end a line.
const HYPHENS: [char; 3] = ['-', '\u{2010}', '\u{00AD}'];
/// Opening punctuation allowed before a hyphenated word.
const OPENERS: [char; 7] = ['(', '[', '{', '"', '\'', '\u{201C}', '\u{2018}'];
/// Shortest word half that counts as attested on its own when deciding to
/// keep a line-end hyphen between two words (`cost-` + `effective`,
/// `web-` + `based`); shorter halves (`in-` + `formation`) never keep it by
/// this rule.
const MIN_ATTESTED_HALF: usize = 3;
/// Short halves that commonly form one word with the other half: word
/// endings (`with-` + `out`, `learn-` + `ing`) and word beginnings (`con-` +
/// `tent`, `out-` + `put`). On either side of the hyphen they never count
/// as attested for the keep rule.
const AMBIGUOUS_HALVES: &[&str] = &[
    "out", "ing", "ers", "est", "ion", "ity", "ful", "ess", "ant", "ent", "ure", "age", "ive",
    "ous", "ise", "ize", "ism", "ist", "ate", "ify", "ary", "ory", "ial", "ual", "pre", "pro",
    "con", "com", "dis", "mis", "sub", "non", "per", "for", "ver", "sur", "int",
];
/// Left halves that form real compounds (`self-supervised`,
/// `cross-domain`): a line-end hyphen after one is kept unless the joined
/// word is attested.
const COMPOUND_PREFIXES: &[&str] = &["self", "cross", "well", "high", "low", "long", "short"];
/// Bound prefixes normally written solid (`preserving`, `nonlinear`,
/// `multimodal`): a line-end hyphen after one is dropped when the right
/// half is lowercase and an attested word or at least
/// [`PREFIX_JOIN_MIN_RIGHT`] letters long.
const JOIN_PREFIXES: &[&str] = &[
    "pre", "re", "non", "un", "sub", "multi", "semi", "inter", "intra", "over", "under", "micro",
    "nano", "co", "de", "dis", "mis", "anti", "auto", "bio", "counter", "hyper", "meta", "post",
    "pseudo", "super", "trans", "ultra", "extra", "infra", "pro",
];
/// Shortest unattested right half that still joins after a
/// [`JOIN_PREFIXES`] entry (`pre-` + `serving`).
const PREFIX_JOIN_MIN_RIGHT: usize = 5;
/// Capitalised left halves (`Multi-`, `Cross-`, `Dual-`), compared lower
/// cased, whose hyphen before a lowercase attested word is kept unless the
/// joined word dominates (see [`hyphen_policy_counted`]). Any other
/// capitalised word (`Every-` + `body`) is left to the later rules.
const CAPITALISED_COMPOUND_PREFIXES: &[&str] = &[
    "multi", "cross", "self", "semi", "non", "pre", "post", "co", "sub", "inter", "intra", "meta",
    "anti", "bi", "tri", "dual", "single", "low", "high", "long", "short", "real", "open", "two",
    "three", "zero", "few", "one", "fine", "coarse", "end", "full", "half", "well", "ill", "state",
    "large", "small", "deep", "wide",
];
/// The joined word dominates the hyphenated pair when it occurs at least
/// this many times as often.
const JOINED_DOMINANCE: usize = 2;
/// Fewest letters in a joined all-capital word (`EFFI-` + `CIENT`) for its
/// line-end hyphen to be dropped without the joined word being attested.
const CAPS_JOIN_MIN_LETTERS: usize = 6;
/// Longest right half that is never a typeset word break (`TeX` leaves at
/// least three letters after a break), so `most-` + `dl` stays a compound.
const MAX_UNBREAKABLE_RIGHT: usize = 2;

/// What the cleanup pass changed, summed over the document.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CleanupReport {
    /// Edge lines whose digit-normalised text repeats on two or more pages.
    pub running_lines: usize,
    /// Lines in the wider edge band (outside the 8 % band) whose
    /// digit-normalised text repeats on three or more pages of one parity or
    /// on at least 40 % (and three or more) of the pages.
    pub running_lines_wide: usize,
    /// Edge lines that are only a page number.
    pub page_numbers: usize,
    /// `arXiv` margin stamps on page 1.
    pub stamps: usize,
    /// Sub/superscript lines merged into their base line as they read
    /// (the general rule; not counting `superscripts_merged`).
    pub scripts_merged: usize,
    /// Detached superscript numbers (`5`, `5–7`, `10,11`), single-letter or
    /// asterisk marks and subscript digits merged into their base line in
    /// Unicode super- or subscript form.
    pub superscripts_merged: usize,
    /// Line-end hyphens removed by joining the word halves.
    pub hyphens_joined: usize,
    /// Line-end hyphens kept as compounds.
    pub hyphens_kept: usize,
    /// Pages left untouched because `text` does not match `lines`.
    pub pages_skipped: usize,
    /// Lines newly tagged `furniture` (removed from `text`).
    pub role_furniture: usize,
    /// Lines newly tagged `toc`.
    pub role_toc: usize,
    /// Lines newly tagged `caption`.
    pub role_caption: usize,
    /// Lines newly tagged `front`: page-1 front matter, and licence,
    /// keyword, reference-format and lettered-affiliation blocks (on page 2
    /// too when page 1 is a title page).
    pub role_front: usize,
    /// Lines newly tagged `heading` (the standalone `Abstract` line).
    pub role_heading: usize,
    /// Lines newly tagged `biography` (author biographies after the
    /// references).
    pub role_biography: usize,
    /// Lines newly tagged `footnote` (notes under a footnote rule, and
    /// notes carried over from the previous page).
    pub role_footnote: usize,
}

/// Role of a line during the pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Body,
    Furniture,
    Merged,
}

/// Per-page working state.
struct PageWork {
    eligible: bool,
    /// Separator in `text` before each line found there.
    seps: Vec<String>,
    state: Vec<State>,
    changed: bool,
    /// Lines newly removed from `text` as furniture on this page.
    removed: usize,
}

impl PageWork {
    fn is_body(&self, index: usize) -> bool {
        self.state.get(index) == Some(&State::Body)
    }

    fn mark(&mut self, index: usize, state: State) {
        if let Some(slot) = self.state.get_mut(index) {
            *slot = state;
            self.changed = true;
        }
    }
}

/// Outcome of looking at one line-end hyphen.
enum Decision {
    NotApplicable,
    Keep,
    /// New texts of the first and the second line.
    Join(String, String),
}

/// Lower-cased words and hyphenated pairs seen in the document's body lines,
/// with the number of times each occurs. Word halves at a line-end hyphen
/// (the last word before it and the first word of the next body line) are
/// not recorded as words, so a split `cost-` / `effective` does not attest
/// its own halves.
struct Vocabulary {
    words: HashMap<String, usize>,
    compounds: HashMap<String, usize>,
}

impl Vocabulary {
    /// Occurrences of the lower-cased word or hyphenated pair `piece`.
    fn count(&self, piece: &str) -> usize {
        self.words.get(piece).copied().unwrap_or(0)
            + self.compounds.get(piece).copied().unwrap_or(0)
    }
}

fn page_number_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)^(?:(?:page|p\.)\s*\d{1,4}(?:\s*(?:of|/)\s*\d{1,4})?|[-–—]?\s*\d{1,4}\s*[-–—]?|\d{1,4}\s*/\s*\d{1,4})$",
        )
        .expect("valid regex")
    })
}

fn roman_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^x{0,3}(?:ix|iv|v?i{0,3})$").expect("valid regex"))
}

fn stamp_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^arXiv:\d{4}\.\d{4,5}v\d+\s+\[[A-Za-z.\-]+\]\s+\d{1,2}\s+[A-Z][a-z]{2}\s+\d{4}$",
        )
        .expect("valid regex")
    })
}

fn abstract_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)^a\s?b\s?s\s?t\s?r\s?a\s?c\s?t\b").expect("valid regex"))
}

fn abstract_heading_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)^a\s?b\s?s\s?t\s?r\s?a\s?c\s?t\s*[.:\x{2014}\x{2013}-]?$")
            .expect("valid regex")
    })
}

fn introduction_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)^(?:(?:\d+|[ivx]+)\.?\s*)?introduction\s*$").expect("valid regex")
    })
}

fn caption_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^(?:Figure|FIGURE|Fig\.|FIG\.|Table|TABLE|Algorithm|ALGORITHM|Listing|LISTING)\s*(?:[A-Z]?\d+(?:\.\d+)*|[IVXL]+)\s*[.:|]",
        )
        .expect("valid regex")
    })
}

/// A caption start without punctuation after the number, followed by a
/// capitalised word: `Fig. 3 Overview of`, `Table 2 Results on`.
fn caption_bare_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^(?:Figure|FIGURE|Fig\.|FIG\.|Table|TABLE)\s*(?:[A-Z]?\d+(?:\.\d+)*|[IVXL]+)\s+\p{Lu}\p{Ll}",
        )
        .expect("valid regex")
    })
}

/// A line that opens a licence or copyright block of the front matter.
fn licence_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^(?:Permission to make digital or hard copies|This work is licensed under|\x{00A9}\s?(?:19|20)\d{2}|Copyright\s+(?:\x{00A9}\s?)?(?:19|20)\d{2}|ACM ISBN|https?://doi\.org/10\.1145/|Publication rights licensed to)",
        )
        .expect("valid regex")
    })
}

/// The `ACM Reference Format:` label.
fn reference_format_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^ACM Reference [Ff]ormat\b").expect("valid regex"))
}

/// The `Authors' Contact Information:` / `Authors' addresses:` label.
fn contact_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)^authors?['\x{2019}]?\s+(?:contact\s+information|addresses?)\s*:")
            .expect("valid regex")
    })
}

/// A `Keywords`, `Index Terms`, `CCS Concepts` or `Additional Key Words
/// and Phrases` label, alone or followed by `:`, `.`, a dash or a bullet.
fn keywords_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)^(?:key\s?words|index\s+terms|ccs\s+concepts|additional\s+key\s+words(?:\s+and\s+phrases)?)\s*(?:$|[:.\x{2014}\x{2013}\x{2022}-])",
        )
        .expect("valid regex")
    })
}

/// A short numbered heading (`1 Introduction`, `II. RELATED WORK`).
fn numbered_heading_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^(?:\d{1,2}(?:\.\d{1,2})*|[IVX]{1,5})\.?\s+\p{Lu}\S*(?:\s+\S+){0,7}$")
            .expect("valid regex")
    })
}

/// A superscript affiliation letter after a comma and before a capital
/// (`USA,ᵇ Entalpic`).
fn affiliation_marker_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"[,;]\s?[\x{02B0}-\x{02B8}\x{1D2C}-\x{1D61}\x{1D9C}-\x{1DBF}\x{2071}\x{207F}]\s?\p{Lu}",
        )
        .expect("valid regex")
    })
}

/// A references heading (`REFERENCES`, `7 References`, `Bibliography`).
fn references_heading_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)^(?:(?:\d+|[ivx]+)\.?\s*)?(?:r\s?eferences|b\s?ibliography|works\s+cited|literature\s+cited)\s*$",
        )
        .expect("valid regex")
    })
}

/// A biographies heading (`BIOGRAPHIES`, `About the Authors`).
fn biographies_heading_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)^(?:authors?['\x{2019}]?\s+)?(?:biographies|biography|about\s+the\s+authors?)\s*$",
        )
        .expect("valid regex")
    })
}

/// An IEEE membership grade in parentheses (`(Member, IEEE)`, `(Senior
/// Member, IEEE)`, `(Life Fellow, IEEE)`).
fn membership_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^\((?:[A-Za-z]+\s+){0,3}(?:Member|Fellow)\b[^()]{0,24}\)")
            .expect("valid regex")
    })
}

fn norm(b: BBox) -> BBox {
    BBox {
        x0: b.x0.min(b.x1),
        y0: b.y0.min(b.y1),
        x1: b.x0.max(b.x1),
        y1: b.y0.max(b.y1),
    }
}

fn union(a: BBox, b: BBox) -> BBox {
    BBox {
        x0: a.x0.min(b.x0),
        y0: a.y0.min(b.y0),
        x1: a.x1.max(b.x1),
        y1: a.y1.max(b.y1),
    }
}

/// Largest font size among the line's non-blank spans.
fn line_size(page: &PageText, line: &Line) -> Option<f32> {
    let mut best: Option<f32> = None;
    for idx in &line.spans {
        let Some(span) = page.spans.get(*idx as usize) else {
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

/// Median font size of all non-blank spans in the document.
fn median_span_size(pages: &[PageText]) -> Option<f32> {
    let mut sizes: Vec<f32> = pages
        .iter()
        .flat_map(|page| page.spans.iter())
        .filter(|span| !span.text.trim().is_empty())
        .filter_map(|span| span.size)
        .filter(|s| s.is_finite() && *s > 0.0)
        .collect();
    if sizes.is_empty() {
        return None;
    }
    sizes.sort_unstable_by(f32::total_cmp);
    Some(sizes[sizes.len() / 2])
}

/// True when the line's box centre lies in the top or bottom `band` share
/// of an unrotated page.
fn in_edge_band(page: &PageText, line: &Line, band: f32) -> bool {
    if page.rotation != 0 || !page.height.is_finite() || page.height <= 0.0 {
        return false;
    }
    let Some(b) = line.bbox.map(norm) else {
        return false;
    };
    let centre = b.y0.midpoint(b.y1);
    centre >= page.height * (1.0 - band) || centre <= page.height * band
}

/// Rule 1 evidence for a line in the wide edge band: its key repeats on
/// `PARITY_MIN_PAGES` pages of one parity (running heads alternate between
/// recto and verso), or on at least 40 % of the document's `total` pages and
/// never fewer than `PARITY_MIN_PAGES`.
fn strongly_repeated(pages_seen: &BTreeSet<u32>, total: usize) -> bool {
    let odd = pages_seen.iter().filter(|n| **n % 2 == 1).count();
    let even = pages_seen.len() - odd;
    let share = pages_seen.len() >= PARITY_MIN_PAGES
        && pages_seen.len() * SHARE_DENOMINATOR >= total * SHARE_NUMERATOR;
    odd >= PARITY_MIN_PAGES || even >= PARITY_MIN_PAGES || share
}

/// Text with every run of ASCII digits replaced by `#` and whitespace
/// collapsed, so `Journal 12` and `Journal 13` compare equal.
fn digit_key(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (w, word) in text.split_whitespace().enumerate() {
        if w > 0 {
            out.push(' ');
        }
        let mut in_digits = false;
        for c in word.chars() {
            if c.is_ascii_digit() {
                if !in_digits {
                    out.push('#');
                }
                in_digits = true;
            } else {
                out.push(c);
                in_digits = false;
            }
        }
    }
    out
}

/// A bare page number: `12`, `- 12 -`, `Page 12 of 30`, `12 / 30`, or a
/// lower-case roman numeral up to `xxxix`.
fn is_page_number(text: &str) -> bool {
    let text = text.trim();
    page_number_re().is_match(text) || (!text.is_empty() && roman_re().is_match(text))
}

/// The `arXiv` margin stamp: its exact text, or a line of at least
/// `VERTICAL_MIN_CHARS` characters whose spans are all vertical.
fn is_stamp(page: &PageText, line: &Line) -> bool {
    let text = line.text.trim();
    if stamp_re().is_match(text) {
        return true;
    }
    if text.chars().count() < VERTICAL_MIN_CHARS {
        return false;
    }
    let mut seen = false;
    for idx in &line.spans {
        let Some(span) = page.spans.get(*idx as usize) else {
            return false;
        };
        if span.text.trim().is_empty() {
            continue;
        }
        let Some(b) = span.bbox.map(norm) else {
            return false;
        };
        if b.y1 - b.y0 <= VERTICAL_RATIO * (b.x1 - b.x0) {
            return false;
        }
        seen = true;
    }
    seen
}

/// Separator before each line in `page.text` and the number of lines found
/// there, in order. `None` when `text` holds anything else.
fn separators(page: &PageText) -> Option<(Vec<String>, usize)> {
    let text = page.text.as_str();
    let mut seps: Vec<String> = Vec::with_capacity(page.lines.len());
    let mut cursor: usize = 0;
    for line in &page.lines {
        let rest = text.get(cursor..)?;
        let body = rest.trim_start();
        if !body.starts_with(line.text.as_str()) {
            break;
        }
        let gap = rest.len() - body.len();
        seps.push(rest[..gap].to_string());
        cursor += gap + line.text.len();
    }
    if !text.get(cursor..)?.trim().is_empty() {
        return None;
    }
    let found = seps.len();
    Some((seps, found))
}

fn prepare(page: &PageText) -> PageWork {
    match separators(page) {
        Some((seps, found)) => {
            let mut state = vec![State::Body; found];
            state.resize(page.lines.len(), State::Furniture);
            PageWork {
                eligible: true,
                seps,
                state,
                changed: false,
                removed: 0,
            }
        }
        None => PageWork {
            eligible: false,
            seps: Vec::new(),
            state: vec![State::Body; page.lines.len()],
            changed: false,
            removed: 0,
        },
    }
}

/// Rule 4: the `arXiv` stamp on page 1 leaves the text.
fn mark_stamps(pages: &[PageText], work: &mut [PageWork], report: &mut CleanupReport) {
    for (page, w) in pages.iter().zip(work.iter_mut()) {
        if page.page != 1 || !w.eligible {
            continue;
        }
        for (k, line) in page.lines.iter().enumerate() {
            if w.is_body(k) && is_stamp(page, line) {
                w.mark(k, State::Furniture);
                w.removed += 1;
                report.stamps += 1;
            }
        }
    }
}

/// Rule 1: page numbers and running headers/footers in the edge bands. A
/// line in the 8 % band goes when its digit-normalised text repeats on two
/// or more pages; a line further in, up to 15 %, only when the repetition is
/// strong (see [`strongly_repeated`]).
fn mark_furniture(pages: &[PageText], work: &mut [PageWork], report: &mut CleanupReport) {
    let body_size = median_span_size(pages);
    let mut seen: BTreeMap<String, BTreeSet<u32>> = BTreeMap::new();
    // (page index, line index, key, inside the narrow band)
    let mut candidates: Vec<(usize, usize, String, bool)> = Vec::new();
    let mut numbers: Vec<(usize, usize)> = Vec::new();
    for (p, (page, w)) in pages.iter().zip(work.iter()).enumerate() {
        if !w.eligible {
            continue;
        }
        for (k, line) in page.lines.iter().enumerate() {
            if !w.is_body(k) || !in_edge_band(page, line, EDGE_BAND_WIDE) {
                continue;
            }
            let narrow = in_edge_band(page, line, EDGE_BAND);
            if narrow && is_page_number(&line.text) {
                numbers.push((p, k));
                continue;
            }
            let key = digit_key(&line.text);
            if !key.chars().any(char::is_alphabetic) {
                continue;
            }
            let small = match (line_size(page, line), body_size) {
                (Some(size), Some(body)) => size <= HEADER_SIZE_SLACK * body,
                _ => true,
            };
            if small {
                seen.entry(key.clone()).or_default().insert(page.page);
                candidates.push((p, k, key, narrow));
            }
        }
    }
    let total = pages.len();
    for (p, k, key, narrow) in candidates {
        let Some(pages_seen) = seen.get(&key) else {
            continue;
        };
        let repeated = if narrow {
            pages_seen.len() >= 2
        } else {
            strongly_repeated(pages_seen, total)
        };
        if repeated && let Some(w) = work.get_mut(p) {
            w.mark(k, State::Furniture);
            w.removed += 1;
            if narrow {
                report.running_lines += 1;
            } else {
                report.running_lines_wide += 1;
            }
        }
    }
    for (p, k) in numbers {
        let alone = pages
            .get(p)
            .zip(work.get(p))
            .is_some_and(|(page, w)| alone_on_row(page, w, k));
        if alone && let Some(w) = work.get_mut(p) {
            w.mark(k, State::Furniture);
            w.removed += 1;
            report.page_numbers += 1;
        }
    }
}

/// True when no body line other than `index` shares its printed row (a
/// page number sits alone; `2026` at the end of an OCR'd title does not).
/// Lines already marked as running headers do not count.
fn alone_on_row(page: &PageText, w: &PageWork, index: usize) -> bool {
    let Some(own) = page.lines.get(index).and_then(|line| line.bbox).map(norm) else {
        return false;
    };
    let own_height = own.y1 - own.y0;
    page.lines.iter().enumerate().all(|(j, other)| {
        if j == index || !w.is_body(j) {
            return true;
        }
        let Some(b) = other.bbox.map(norm) else {
            return true;
        };
        let overlap = own.y1.min(b.y1) - own.y0.max(b.y0);
        overlap < 0.5 * own_height.min(b.y1 - b.y0)
    })
}

/// Rule 3: the body line a script fragment at `index` belongs to, if any.
fn script_target(
    page: &PageText,
    geom: &PageGeom,
    index: usize,
    candidates: [Option<usize>; 2],
) -> Option<usize> {
    let line = page.lines.get(index)?;
    let text = line.text.trim();
    if text.is_empty()
        || text.chars().count() > SCRIPT_MAX_CHARS
        || text.split_whitespace().count() > SCRIPT_MAX_WORDS
    {
        return None;
    }
    let bbox = norm(line.bbox?);
    let size = geom.lines.get(index)?.size?;
    let height = (bbox.y1 - bbox.y0).max(f32::EPSILON);
    let mut best: Option<(usize, f32)> = None;
    for cand in candidates.into_iter().flatten() {
        let Some(other) = page.lines.get(cand) else {
            continue;
        };
        if other.column != line.column {
            continue;
        }
        let other_size = geom.lines.get(cand).and_then(|g| g.size);
        let (Some(ob), Some(other_size)) = (other.bbox.map(norm), other_size) else {
            continue;
        };
        if size > SCRIPT_RATIO * other_size {
            continue;
        }
        let overlap = bbox.y1.min(ob.y1) - bbox.y0.max(ob.y0);
        if overlap < SCRIPT_OVERLAP * height {
            continue;
        }
        if bbox.x1 < ob.x0 - other_size || bbox.x0 > ob.x1 + other_size {
            continue;
        }
        let share = overlap / height;
        if best.is_none_or(|(_, s)| share > s) {
            best = Some((cand, share));
        }
    }
    best.map(|(cand, _)| cand)
}

/// Byte offset in `line.text` and slot in `line.spans` where content that
/// starts at `x0` goes: before the first span starting at or right of
/// `x0`, else at the end. Spans whose text cannot be located (composed
/// accents) are skipped.
fn insertion_point(page: &PageText, line: &Line, x0: f32) -> (usize, usize) {
    let mut cursor: usize = 0;
    for (slot, idx) in line.spans.iter().enumerate() {
        let Some(span) = page.spans.get(*idx as usize) else {
            continue;
        };
        let piece = span.text.trim();
        if piece.is_empty() {
            continue;
        }
        let Some((start, len)) = locate(&line.text, cursor, piece) else {
            continue;
        };
        if span.bbox.is_some_and(|b| norm(b).x0 >= x0) {
            return (start, slot);
        }
        cursor = start + len;
    }
    (line.text.len(), line.spans.len())
}

/// Byte offset at or after `cursor` where `piece` shows in `text`, and the
/// byte length it shows with: as itself or, for a merged fragment, in its
/// super- or subscript form, whichever comes first.
fn locate(text: &str, cursor: usize, piece: &str) -> Option<(usize, usize)> {
    let rest = text.get(cursor..)?;
    let mut best: Option<(usize, usize)> = rest.find(piece).map(|at| (at, piece.len()));
    for raised in [true, false] {
        let Some(form) = script_form(piece, raised) else {
            continue;
        };
        let found = rest
            .find(form.as_str())
            .filter(|at| best.is_none_or(|(b, _)| *at < b));
        if let Some(at) = found {
            best = Some((at, form.len()));
        }
    }
    best.map(|(at, len)| (cursor + at, len))
}

/// `text` written as a superscript (`raised`) or subscript fragment, when
/// it is one: at most `SUPERSCRIPT_MAX_CHARS` characters of digits with
/// optional `,`, `-`, `–`, `−` and spaces (raised only; a subscript is
/// digits alone), or, raised, a single letter or asterisk. Digits become
/// Unicode super- or subscript digits, dashes the superscript minus, spaces
/// are dropped; a lowercase letter becomes its Unicode superscript letter (none for `q`), an asterisk stays as it is. `None` for anything
/// else, words of two or more letters included.
/// The Unicode superscript form of a lowercase Latin letter, when one
/// exists (there is no superscript `q`).
fn superscript_letter(letter: char) -> Option<char> {
    let mapped = match letter {
        'a' => '\u{1D43}',
        'b' => '\u{1D47}',
        'c' => '\u{1D9C}',
        'd' => '\u{1D48}',
        'e' => '\u{1D49}',
        'f' => '\u{1DA0}',
        'g' => '\u{1D4D}',
        'h' => '\u{02B0}',
        'i' => '\u{2071}',
        'j' => '\u{02B2}',
        'k' => '\u{1D4F}',
        'l' => '\u{02E1}',
        'm' => '\u{1D50}',
        'n' => '\u{207F}',
        'o' => '\u{1D52}',
        'p' => '\u{1D56}',
        'r' => '\u{02B3}',
        's' => '\u{02E2}',
        't' => '\u{1D57}',
        'u' => '\u{1D58}',
        'v' => '\u{1D5B}',
        'w' => '\u{02B7}',
        'x' => '\u{02E3}',
        'y' => '\u{02B8}',
        'z' => '\u{1DBB}',
        _ => return None,
    };
    Some(mapped)
}

fn script_form(text: &str, raised: bool) -> Option<String> {
    let text = text.trim();
    if text.is_empty() || text.chars().count() > SUPERSCRIPT_MAX_CHARS {
        return None;
    }
    let mut chars = text.chars();
    if let (Some(only), None) = (chars.next(), chars.next())
        && raised
    {
        if matches!(only, '*' | '\u{2217}') {
            return Some(only.to_string());
        }
        if only.is_alphabetic() {
            // Only letters with a real superscript form are merged; a raised
            // `q` or capital is left to the general script rule so `xⁿ`
            // never collapses into `xn`.
            return superscript_letter(only).map(|c| c.to_string());
        }
    }
    let mut out = String::with_capacity(3 * text.len());
    let mut digits: usize = 0;
    for ch in text.chars() {
        if ch.is_whitespace() && raised {
            continue;
        }
        let mapped = match ch {
            '0'..='9' => {
                digits += 1;
                let index = usize::try_from(ch.to_digit(10)?).ok()?;
                let table = if raised {
                    &SUPERSCRIPT_DIGITS
                } else {
                    &SUBSCRIPT_DIGITS
                };
                table.get(index).copied()?
            }
            '-' | '\u{2013}' | '\u{2212}' if raised => SUPERSCRIPT_MINUS,
            ',' if raised => ',',
            _ => return None,
        };
        out.push(mapped);
    }
    (digits > 0).then_some(out)
}

/// True for a character `script_form` writes for a digit or a dash.
fn is_script_char(ch: char) -> bool {
    ch == SUPERSCRIPT_MINUS || SUPERSCRIPT_DIGITS.contains(&ch) || SUBSCRIPT_DIGITS.contains(&ch)
}

/// Baseline of the line and its dominant font size, from the first
/// non-blank span of that size (the line box grows with merged scripts).
fn baseline_of(page: &PageText, line: &Line) -> Option<(f32, f32)> {
    let dominant = line_size(page, line)?;
    line.spans.iter().find_map(|idx| {
        let span = page.spans.get(*idx as usize)?;
        if span.text.trim().is_empty() {
            return None;
        }
        let size = span
            .size
            .filter(|s| s.is_finite() && *s >= 0.9 * dominant)?;
        let bbox = norm(span.bbox?);
        Some((bbox.y0 + DESCENT_SHARE * size, dominant))
    })
}

/// Geometry of one line for rules 3 and 3a: its normalised box, its
/// dominant size ([`line_size`]) and its baseline with that size
/// ([`baseline_of`]).
#[derive(Clone, Copy)]
struct LineGeom {
    bbox: Option<BBox>,
    size: Option<f32>,
    base: Option<(f32, f32)>,
}

impl LineGeom {
    fn of(page: &PageText, line: &Line) -> Self {
        Self {
            bbox: line.bbox.map(norm),
            size: line_size(page, line),
            base: baseline_of(page, line),
        }
    }

    /// Baseline and size bits of a line that can be a base line (it has a
    /// box and a finite baseline), for telling whether the index changes.
    fn index_key(&self) -> Option<(u32, u32)> {
        self.bbox?;
        let (baseline, size) = self.base?;
        baseline
            .is_finite()
            .then_some((baseline.to_bits(), size.to_bits()))
    }
}

/// Per-page cache for rules 3 and 3a, computed once per page and refreshed
/// for the target line of each merge: the geometry of every line, the
/// lines that can be a base line as `(baseline, index)` sorted by
/// baseline, and the largest dominant size among them.
struct PageGeom {
    lines: Vec<LineGeom>,
    by_baseline: Vec<(f32, usize)>,
    max_size: f32,
}

impl PageGeom {
    fn new(page: &PageText) -> Self {
        let lines: Vec<LineGeom> = page
            .lines
            .iter()
            .map(|line| LineGeom::of(page, line))
            .collect();
        let mut geom = Self {
            lines,
            by_baseline: Vec::new(),
            max_size: 0.0,
        };
        geom.index();
        geom
    }

    fn index(&mut self) {
        self.by_baseline.clear();
        self.max_size = 0.0;
        for (j, g) in self.lines.iter().enumerate() {
            if g.index_key().is_some()
                && let Some((baseline, size)) = g.base
            {
                self.by_baseline.push((baseline, j));
                self.max_size = self.max_size.max(size);
            }
        }
        self.by_baseline.sort_by(|a, b| a.0.total_cmp(&b.0));
    }

    /// Recompute line `index` after a merge changed it; the baseline index
    /// is rebuilt only when its baseline or size changed.
    fn refresh(&mut self, page: &PageText, index: usize) {
        let (Some(slot), Some(line)) = (self.lines.get_mut(index), page.lines.get(index)) else {
            return;
        };
        let before = slot.index_key();
        *slot = LineGeom::of(page, line);
        if slot.index_key() != before {
            self.index();
        }
    }

    /// Body-line candidates among the first [`SUPERSCRIPT_SCAN_LIMIT`] index
    /// entries with a baseline in `low..=high`, returned in line-index order
    /// through `out`. Counting every entry inspected (including non-body lines)
    /// keeps the work bounded even when the window contains much furniture,
    /// and the entries inspected are charged to the page's `work_left`; when
    /// that budget cannot cover them nothing is collected and `false` is
    /// returned.
    fn window(
        &self,
        low: f32,
        high: f32,
        index: usize,
        w: &PageWork,
        out: &mut Vec<usize>,
        work_left: &mut usize,
    ) -> bool {
        out.clear();
        let start = self.by_baseline.partition_point(|(b, _)| *b < low);
        let end =
            start + self.by_baseline[start..].partition_point(|(baseline, _)| *baseline <= high);
        let inspected = (end - start).min(SUPERSCRIPT_SCAN_LIMIT);
        if inspected > *work_left {
            *work_left = 0;
            return false;
        }
        *work_left -= inspected;
        for &(_, j) in &self.by_baseline[start..start + inspected] {
            if j != index && w.is_body(j) {
                out.push(j);
            }
        }
        out.sort_unstable();
        true
    }
}

/// Rule 3a: the body line a detached superscript or subscript fragment at
/// `index` belongs to, with the fragment's rendered form. Every body line of
/// the page is a candidate, not only the neighbours in reading order:
/// fragments printed on one row follow each other (`8`, `9`, `10,11`), and
/// when two columns interleave the base line can be further away. The base
/// line has a font size of at least `1 / SUPERSCRIPT_RATIO` times the
/// fragment's, reaches the fragment horizontally within
/// `SUPERSCRIPT_REACH` (`SUPERSCRIPT_REACH_AFTER` when the fragment lies
/// wholly beyond its right edge), and has its baseline at a box-bottom
/// offset in the raised window (superscript) or, for digits only, just
/// below it (subscript). The candidate closest to the typical offset wins (the
/// lowest line index on a tie). Only lines whose baseline lies within the
/// offset range at the page's largest size are looked at (`geom`, with
/// `window` as scratch space); every other line fails the offset test.
fn superscript_target(
    page: &PageText,
    w: &PageWork,
    geom: &PageGeom,
    index: usize,
    window: &mut Vec<usize>,
    work_left: &mut usize,
) -> Option<(usize, String)> {
    let line = page.lines.get(index)?;
    let raised_form = script_form(&line.text, true);
    let lowered_form = script_form(&line.text, false);
    if raised_form.is_none() && lowered_form.is_none() {
        return None;
    }
    let own = geom.lines.get(index)?;
    let bbox = own.bbox?;
    let size = own.size?;
    if !bbox.y0.is_finite() {
        return None;
    }
    // A candidate passes only with `LOWERED_LOW <= (y0 - baseline) / size
    // <= RAISED_HIGH` and `size <= max_size`; the slack covers rounding.
    let reach = RAISED_HIGH.abs().max(LOWERED_LOW.abs()) * geom.max_size;
    let slack = 1e-3 * (bbox.y0.abs() + reach) + 1e-3;
    if !geom.window(
        bbox.y0 - reach - slack,
        bbox.y0 + reach + slack,
        index,
        w,
        window,
        work_left,
    ) {
        return None;
    }
    let mut best: Option<(usize, f32, bool)> = None;
    for &j in &*window {
        let Some(other) = geom.lines.get(j) else {
            continue;
        };
        let (Some(ob), Some((baseline, other_size))) = (other.bbox, other.base) else {
            continue;
        };
        if size > (SUPERSCRIPT_RATIO + RATIO_SLACK) * other_size {
            continue;
        }
        let gap = (ob.x0 - bbox.x1).max(bbox.x0 - ob.x1).max(0.0);
        let horizontal_reach = if bbox.x0 >= ob.x1 {
            SUPERSCRIPT_REACH_AFTER
        } else {
            SUPERSCRIPT_REACH
        };
        if gap > horizontal_reach * other_size {
            continue;
        }
        let offset = (bbox.y0 - baseline) / other_size;
        let (raised, miss) =
            if raised_form.is_some() && (RAISED_LOW..=RAISED_HIGH).contains(&offset) {
                (true, (offset - RAISED_IDEAL).abs())
            } else if lowered_form.is_some() && (LOWERED_LOW..RAISED_LOW).contains(&offset) {
                (false, (offset - LOWERED_IDEAL).abs())
            } else {
                continue;
            };
        let score = miss + gap / other_size;
        if best.is_none_or(|(_, s, _)| score < s) {
            best = Some((j, score, raised));
        }
    }
    let (target, _, raised) = best?;
    let form = if raised { raised_form } else { lowered_form };
    form.map(|f| (target, f))
}

/// Insert the rendered fragment `form` of line `script` into line `target`
/// at its horizontal position, attached to the word before it with no
/// space (`literature.⁵`, `Initiative⁶,`), or to the word after it when it
/// opens the line (`⁵Prein`). The fragment's span indices go into the
/// target line's spans at the same place.
fn merge_superscript(page: &mut PageText, script: usize, target: usize, form: &str) {
    let Some(line) = page.lines.get(script) else {
        return;
    };
    let spans = line.spans.clone();
    let bbox = line.bbox;
    let x0 = bbox.map_or(f32::INFINITY, |b| norm(b).x0);
    let Some(base) = page.lines.get(target) else {
        return;
    };
    let (byte, slot) = insertion_point(page, base, x0);
    let Some(base) = page.lines.get_mut(target) else {
        return;
    };
    let Some(head) = base.text.get(..byte) else {
        return;
    };
    let tail = base.text.get(byte..).unwrap_or("");
    let head = head.trim_end();
    let mut text = String::with_capacity(base.text.len() + form.len() + 1);
    text.push_str(head);
    text.push_str(form);
    if head.is_empty() {
        text.push_str(tail.trim_start());
    } else {
        let word_follows = tail
            .chars()
            .next()
            .is_some_and(|c| c.is_alphanumeric() && !is_script_char(c));
        if word_follows {
            text.push(' ');
        }
        text.push_str(tail);
    }
    base.text = text;
    let slot = slot.min(base.spans.len());
    let mut merged: Vec<u32> = base.spans[..slot].to_vec();
    merged.extend(spans);
    merged.extend_from_slice(&base.spans[slot..]);
    base.spans = merged;
    base.bbox = match (base.bbox, bbox) {
        (Some(a), Some(b)) => Some(union(norm(a), norm(b))),
        (a, b) => a.or(b),
    };
}

/// Insert the script line `script` into line `target` at its horizontal
/// position. A space separates it from a letter or digit on either side,
/// so `G` + raised `hom` + `(A)` reads `G hom(A)`.
fn merge_script(page: &mut PageText, script: usize, target: usize) {
    let Some(line) = page.lines.get(script) else {
        return;
    };
    let text = line.text.trim().to_string();
    let spans = line.spans.clone();
    let bbox = line.bbox;
    let x0 = bbox.map_or(f32::INFINITY, |b| norm(b).x0);
    let Some(base) = page.lines.get(target) else {
        return;
    };
    let (byte, slot) = insertion_point(page, base, x0);
    let Some(base) = page.lines.get_mut(target) else {
        return;
    };
    let before = base.text.get(..byte).and_then(|s| s.chars().next_back());
    let after = base.text.get(byte..).and_then(|s| s.chars().next());
    let mut piece = String::new();
    if before.is_some_and(char::is_alphanumeric) {
        piece.push(' ');
    }
    piece.push_str(&text);
    if after.is_some_and(char::is_alphanumeric) {
        piece.push(' ');
    }
    base.text.insert_str(byte, &piece);
    let slot = slot.min(base.spans.len());
    let mut merged: Vec<u32> = base.spans[..slot].to_vec();
    merged.extend(spans);
    merged.extend_from_slice(&base.spans[slot..]);
    base.spans = merged;
    base.bbox = match (base.bbox, bbox) {
        (Some(a), Some(b)) => Some(union(norm(a), norm(b))),
        (a, b) => a.or(b),
    };
}

/// Rules 3a and 3 over one page; returns the number of lines merged by
/// the general script rule and by the superscript rule.
fn merge_scripts(page: &mut PageText, w: &mut PageWork) -> (usize, usize) {
    let mut merged: usize = 0;
    let mut superscripts: usize = 0;
    let mut geom = PageGeom::new(page);
    let mut window: Vec<usize> = Vec::new();
    // Forward neighbours never change while walking front to back. Cache
    // them once instead of rescanning an ever-growing run of merged lines.
    let mut next_body = vec![None; page.lines.len()];
    let mut next = None;
    for k in (0..page.lines.len()).rev() {
        next_body[k] = next;
        if w.is_body(k) {
            next = Some(k);
        }
    }
    let mut previous_body = None;
    let mut work_left = SCRIPT_MERGE_WORK_LIMIT;
    for (k, following_body) in next_body.iter().copied().enumerate() {
        if !w.is_body(k) {
            continue;
        }
        let superscript = if work_left == 0 {
            None
        } else {
            superscript_target(page, w, &geom, k, &mut window, &mut work_left)
        };
        let general = superscript
            .is_none()
            .then(|| script_target(page, &geom, k, [following_body, previous_body]))
            .flatten();
        let target = superscript.as_ref().map(|(target, _)| *target).or(general);
        let cost = target.map_or(0, |target| {
            page.lines
                .get(target)
                .map_or(0, |line| line.text.len().saturating_add(line.spans.len()))
        });
        if target.is_some() && cost > work_left {
            work_left = 0;
            previous_body = Some(k);
            continue;
        }
        work_left -= cost;
        if let Some((target, form)) = superscript {
            merge_superscript(page, k, target, &form);
            geom.refresh(page, target);
            w.mark(k, State::Merged);
            superscripts += 1;
        } else if let Some(target) = general {
            merge_script(page, k, target);
            geom.refresh(page, target);
            w.mark(k, State::Merged);
            merged += 1;
        } else {
            previous_body = Some(k);
        }
    }
    (merged, superscripts)
}

/// Append `word` lower-cased to `buf`, as `str::to_lowercase` does (ASCII
/// in place, anything else through `to_lowercase`).
fn push_lowercase(buf: &mut String, word: &str) {
    if word.is_ascii() {
        let start = buf.len();
        buf.push_str(word);
        if let Some(tail) = buf.get_mut(start..) {
            tail.make_ascii_lowercase();
        }
    } else {
        buf.push_str(&word.to_lowercase());
    }
}

/// Count one occurrence of `key` in `counts` (one allocation per new entry
/// only).
fn count_one(counts: &mut HashMap<String, usize>, key: &str) {
    if let Some(slot) = counts.get_mut(key) {
        *slot += 1;
    } else {
        counts.insert(key.to_owned(), 1);
    }
}

/// Words and hyphenated word pairs of every body line, lower-cased and
/// counted, in reading order (see [`Vocabulary`] for the halves left out).
fn vocabulary(pages: &[PageText], work: &[PageWork]) -> Vocabulary {
    let mut vocab = Vocabulary {
        words: HashMap::new(),
        compounds: HashMap::new(),
    };
    let alphabetic = |piece: &str| !piece.is_empty() && piece.chars().all(char::is_alphabetic);
    let mut buf = String::new();
    let mut after_hyphen = false;
    for (page, w) in pages.iter().zip(work) {
        if !w.eligible {
            after_hyphen = false;
            continue;
        }
        for (k, line) in page.lines.iter().enumerate() {
            if !w.is_body(k) {
                continue;
            }
            let ends_hyphen = strip_final_hyphen(&line.text).is_some();
            let mut tokens = line.text.split_whitespace().peekable();
            let mut first_token = true;
            while let Some(token) = tokens.next() {
                let last_token = tokens.peek().is_none();
                let core = token.trim_matches(|c: char| !c.is_alphanumeric());
                let mut parts = core.split(HYPHENS).peekable();
                let mut first_part = true;
                let mut prev_part: Option<&str> = None;
                while let Some(part) = parts.next() {
                    let last_part = parts.peek().is_none();
                    let mut words = part
                        .split(|c: char| !c.is_alphabetic())
                        .filter(|word| !word.is_empty())
                        .peekable();
                    let mut first_word = true;
                    while let Some(word) = words.next() {
                        let last_word = words.peek().is_none();
                        let split_head = after_hyphen && first_token && first_part && first_word;
                        let split_tail = ends_hyphen && last_token && last_part && last_word;
                        if !split_head && !split_tail {
                            buf.clear();
                            push_lowercase(&mut buf, word);
                            count_one(&mut vocab.words, &buf);
                        }
                        first_word = false;
                    }
                    if let Some(prev) = prev_part
                        && alphabetic(prev)
                        && alphabetic(part)
                    {
                        buf.clear();
                        push_lowercase(&mut buf, prev);
                        buf.push('-');
                        push_lowercase(&mut buf, part);
                        count_one(&mut vocab.compounds, &buf);
                    }
                    prev_part = Some(part);
                    first_part = false;
                }
                first_token = false;
            }
            after_hyphen = ends_hyphen;
        }
    }
    vocab
}

/// `text` without its final hyphen, when it ends with one.
fn strip_final_hyphen(text: &str) -> Option<&str> {
    let text = text.trim_end();
    let last = text.chars().next_back()?;
    if HYPHENS.contains(&last) {
        text.get(..text.len() - last.len_utf8())
    } else {
        None
    }
}

/// Outcome of [`hyphen_policy`] for one line-end hyphen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HyphenPolicy {
    /// A word broken for justification: `opti-` + `mization` → `optimization`.
    Join,
    /// A real compound: `cost-` + `effective` → `cost-effective`.
    Keep,
}

/// Whether a right half of three letters has no vowel (`cnn`), so it reads
/// as an acronym rather than a word ending (`ing`, `ves`).
fn vowelless(right: &str) -> bool {
    !right
        .chars()
        .any(|c| matches!(c.to_ascii_lowercase(), 'a' | 'e' | 'i' | 'o' | 'u' | 'y'))
}

/// Whether the printed halves themselves mark a real compound: the right
/// half starts with a capital or has a digit, the left half is one letter
/// (`k-space`, `x-ray`) or an all-capital acronym (`MRI-guided`), or the
/// right half is too short to be a typeset break (`most-dl`, `state-of`) or
/// is a three-letter acronym after a short left half (`deep-cnn`).
fn printed_compound(left: &str, right: &str) -> bool {
    let left_len = left.chars().count();
    let right_len = right.chars().count();
    right.chars().next().is_some_and(char::is_uppercase)
        || right.chars().any(char::is_numeric)
        || left.chars().any(char::is_numeric)
        || left_len == 1
        || (left_len >= 2 && left.chars().all(char::is_uppercase))
        || right_len <= MAX_UNBREAKABLE_RIGHT
        || (left_len <= 5 && right_len == 3 && vowelless(right))
}

/// Whether `left` is a capitalised compound prefix (`Multi`, `Cross`,
/// `Dual`: a capital, then lowercase letters, and a
/// `CAPITALISED_COMPOUND_PREFIXES` entry when lower cased) and `right` is
/// all lowercase (`agent`): in a title or at a sentence start such a pair
/// is far more often a compound than a broken word.
fn capitalised_prefix(left: &str, right: &str) -> bool {
    CAPITALISED_COMPOUND_PREFIXES.contains(&left.to_lowercase().as_str())
        && left.chars().next().is_some_and(char::is_uppercase)
        && left.chars().skip(1).all(char::is_lowercase)
        && !right.is_empty()
        && right.chars().all(char::is_lowercase)
}

/// Whether both halves are all-capital words (`EFFI-` + `CIENT`, `TIME-` +
/// `DIAL`) with a right half long enough to be a typeset break and at least
/// `CAPS_JOIN_MIN_LETTERS` letters together.
fn all_caps_break(left: &str, right: &str) -> bool {
    let caps = |half: &str| !half.is_empty() && half.chars().all(char::is_uppercase);
    let left_len = left.chars().count();
    let right_len = right.chars().count();
    caps(left)
        && caps(right)
        && left_len >= 2
        && right_len > MAX_UNBREAKABLE_RIGHT
        && left_len + right_len >= CAPS_JOIN_MIN_LETTERS
}

/// Decide a line-end hyphen between `left`, the word before the hyphen, and
/// `right`, the word that starts the next line, both as printed.
/// `attested(piece)` tells whether the lower-cased word (`optimization`) or
/// hyphenated pair (`noise-regularized`) occurs elsewhere in the document
/// as a whole word. The rules are those of [`hyphen_policy_counted`], with
/// every attested piece counted once.
pub fn hyphen_policy(left: &str, right: &str, attested: &dyn Fn(&str) -> bool) -> HyphenPolicy {
    hyphen_policy_counted(left, right, &|piece: &str| usize::from(attested(piece)))
}

/// Decide a line-end hyphen between `left` and `right` (see
/// [`hyphen_policy`]) with `count(piece)`, the number of times the
/// lower-cased word or hyphenated pair occurs elsewhere in the document.
/// The first matching rule wins:
/// 1. the hyphenated pair is attested at least as often as the joined word
///    → keep (`noise-regularized` seen);
/// 2. a capitalised `CAPITALISED_COMPOUND_PREFIXES` entry before a
///    lowercase right half that is an attested word of at least
///    `MIN_ATTESTED_HALF` letters, not an `AMBIGUOUS_HALVES` entry → keep
///    (`Multi-` + `agent`), unless the joined word is attested and occurs
///    at least `JOINED_DOMINANCE` times as often as the pair; any other
///    capitalised left half (`Every-` + `body`) goes on to the later rules;
/// 3. the joined word is attested → join (`with-` + `out`, `without` seen);
/// 4. the left half is a `COMPOUND_PREFIXES` entry (`self-`) → keep;
/// 5. the left half is a `JOIN_PREFIXES` entry and the right half is all
///    lowercase and an attested word or at least `PREFIX_JOIN_MIN_RIGHT`
///    letters → join (`pre-` + `serving`);
/// 6. both halves are all capitals (see `all_caps_break`) and not both
///    attested words of at least `MIN_ATTESTED_HALF` letters → join
///    (`EFFI-` + `CIENT`, `TIME-` + `DIAL`; `LARGE-` + `SCALE` falls through
///    when `large` and `scale` occur);
/// 7. the printed halves mark a compound (see `printed_compound`) → keep;
/// 8. both halves are attested words of at least `MIN_ATTESTED_HALF`
///    letters, neither an `AMBIGUOUS_HALVES` entry → keep (`cost-` +
///    `effective`, `web-` + `based`, `Dual-` + `domain`);
/// 9. otherwise join (`algo-` + `rithm`).
pub fn hyphen_policy_counted(
    left: &str,
    right: &str,
    count: &dyn Fn(&str) -> usize,
) -> HyphenPolicy {
    let lower_left = left.to_lowercase();
    let lower_right = right.to_lowercase();
    let joined_count = count(&format!("{lower_left}{lower_right}"));
    let pair_count = count(&format!("{lower_left}-{lower_right}"));
    if pair_count > 0 && pair_count >= joined_count {
        return HyphenPolicy::Keep;
    }
    let word = |half: &str| {
        half.chars().count() >= MIN_ATTESTED_HALF
            && !AMBIGUOUS_HALVES.contains(&half)
            && count(half) > 0
    };
    let joined_dominates =
        joined_count > 0 && joined_count >= JOINED_DOMINANCE.saturating_mul(pair_count);
    if capitalised_prefix(left, right) && word(&lower_right) && !joined_dominates {
        return HyphenPolicy::Keep;
    }
    if joined_count > 0 {
        return HyphenPolicy::Join;
    }
    if COMPOUND_PREFIXES.contains(&lower_left.as_str()) {
        return HyphenPolicy::Keep;
    }
    let lowercase_word = !right.is_empty() && right.chars().all(char::is_lowercase);
    if JOIN_PREFIXES.contains(&lower_left.as_str())
        && lowercase_word
        && (count(&lower_right) > 0 || right.chars().count() >= PREFIX_JOIN_MIN_RIGHT)
    {
        return HyphenPolicy::Join;
    }
    let both_words = word(&lower_left) && word(&lower_right);
    if all_caps_break(left, right) && !both_words {
        return HyphenPolicy::Join;
    }
    if printed_compound(left, right) {
        return HyphenPolicy::Keep;
    }
    if both_words {
        HyphenPolicy::Keep
    } else {
        HyphenPolicy::Join
    }
}

/// Rule 2 for one pair of consecutive lines: a lowercase continuation after
/// an alphabetic word and a line-end hyphen, or an all-capital continuation
/// after an all-capital word (`EFFI-` + `CIENT`), is decided by
/// [`hyphen_policy_counted`] against the document vocabulary.
fn hyphen_decision(first: &str, second: &str, vocab: &Vocabulary) -> Decision {
    let Some(stem) = strip_final_hyphen(first) else {
        return Decision::NotApplicable;
    };
    let token = stem.rsplit(char::is_whitespace).next().unwrap_or(stem);
    let word = token.trim_start_matches(OPENERS);
    if word.is_empty() || !word.chars().all(char::is_alphabetic) {
        return Decision::NotApplicable;
    }
    let rest = second.trim_start();
    let Some(head) = rest.split_whitespace().next() else {
        return Decision::NotApplicable;
    };
    let first_char = head.chars().next();
    let caps_pair = word.chars().count() >= 2
        && word.chars().all(char::is_uppercase)
        && first_char.is_some_and(char::is_uppercase);
    if !first_char.is_some_and(char::is_lowercase) && !caps_pair {
        return Decision::NotApplicable;
    }
    let right: String = head.chars().take_while(|c| c.is_alphanumeric()).collect();
    if right.is_empty() || (caps_pair && !right.chars().all(char::is_uppercase)) {
        return Decision::NotApplicable;
    }
    let count = |piece: &str| vocab.count(piece);
    match hyphen_policy_counted(word, &right, &count) {
        HyphenPolicy::Join => {
            let tail = rest
                .get(head.len()..)
                .unwrap_or("")
                .trim_start()
                .to_string();
            Decision::Join(format!("{stem}{head}"), tail)
        }
        HyphenPolicy::Keep => Decision::Keep,
    }
}

fn line_at(pages: &[PageText], at: (usize, usize)) -> Option<&Line> {
    pages.get(at.0).and_then(|page| page.lines.get(at.1))
}

fn line_at_mut(pages: &mut [PageText], at: (usize, usize)) -> Option<&mut Line> {
    pages
        .get_mut(at.0)
        .and_then(|page| page.lines.get_mut(at.1))
}

/// Whether a separator in `text` is a paragraph break (two or more line
/// breaks).
fn is_paragraph_break(sep: &str) -> bool {
    sep.matches('\n').count() >= 2
}

/// Rule 2 decision for body line `second` following body line `first`:
/// same page and column with no paragraph break between them in `text`,
/// or the last and first body lines of consecutive pages.
fn plan_join(
    pages: &[PageText],
    work: &[PageWork],
    first: (usize, usize),
    second: (usize, usize),
    vocab: &Vocabulary,
) -> Decision {
    let (Some(a), Some(b)) = (line_at(pages, first), line_at(pages, second)) else {
        return Decision::NotApplicable;
    };
    let continues = if first.0 == second.0 {
        let paragraph_break = work.get(first.0).is_some_and(|w| {
            (first.1 + 1..=second.1)
                .filter_map(|k| w.seps.get(k))
                .any(|sep| is_paragraph_break(sep))
        });
        a.column == b.column && !paragraph_break
    } else {
        let earlier_number = pages.get(first.0).map(|page| page.page);
        let later_number = pages.get(second.0).map(|page| page.page);
        earlier_number
            .and_then(|n| n.checked_add(1))
            .is_some_and(|n| Some(n) == later_number)
    };
    if !continues {
        return Decision::NotApplicable;
    }
    hyphen_decision(&a.text, &b.text, vocab)
}

/// Rule 2 over the document, in reading order.
fn join_hyphens(
    pages: &mut [PageText],
    work: &mut [PageWork],
    vocab: &Vocabulary,
    report: &mut CleanupReport,
) {
    let mut prev: Option<(usize, usize)> = None;
    for page_idx in 0..pages.len() {
        let eligible = work.get(page_idx).is_some_and(|w| w.eligible);
        if !eligible {
            prev = None;
            continue;
        }
        let line_count = pages.get(page_idx).map_or(0, |page| page.lines.len());
        for line_idx in 0..line_count {
            let current = (page_idx, line_idx);
            if !work.get(page_idx).is_some_and(|w| w.is_body(line_idx)) {
                continue;
            }
            let mut consumed = false;
            if let Some(earlier) = prev {
                match plan_join(pages, work, earlier, current, vocab) {
                    Decision::NotApplicable => {}
                    Decision::Keep => report.hyphens_kept += 1,
                    Decision::Join(first_text, second_text) => {
                        report.hyphens_joined += 1;
                        consumed = second_text.is_empty();
                        if let Some(line) = line_at_mut(pages, earlier) {
                            line.text = first_text;
                        }
                        let mut moved: Vec<u32> = Vec::new();
                        if let Some(line) = line_at_mut(pages, current) {
                            line.text = second_text;
                            if consumed && earlier.0 == page_idx {
                                moved = std::mem::take(&mut line.spans);
                            }
                        }
                        if let Some(line) = line_at_mut(pages, earlier) {
                            line.spans.extend(moved);
                        }
                        if let Some(w) = work.get_mut(earlier.0) {
                            w.changed = true;
                        }
                        if let Some(w) = work.get_mut(page_idx) {
                            w.changed = true;
                            if consumed {
                                w.mark(line_idx, State::Merged);
                            }
                        }
                    }
                }
            }
            if !consumed {
                prev = Some(current);
            }
        }
    }
}

/// The separator with more line breaks (a paragraph break wins).
fn stronger<'a>(a: &'a str, b: &'a str) -> &'a str {
    if b.matches('\n').count() > a.matches('\n').count() {
        b
    } else {
        a
    }
}

/// Rebuild `lines` (body lines, then furniture) and `text` (body lines
/// only, with their original separators).
fn rebuild(page: &mut PageText, w: &PageWork) {
    let old = std::mem::take(&mut page.lines);
    let mut body: Vec<Line> = Vec::with_capacity(old.len());
    let mut furniture: Vec<Line> = Vec::new();
    let mut text = String::new();
    let mut pending: &str = "";
    for (k, line) in old.into_iter().enumerate() {
        let state = w.state.get(k).copied().unwrap_or(State::Furniture);
        let sep = w.seps.get(k).map_or("", String::as_str);
        match state {
            State::Body => {
                if !body.is_empty() {
                    text.push_str(stronger(pending, sep));
                }
                pending = "";
                text.push_str(&line.text);
                body.push(line);
            }
            State::Furniture => {
                pending = stronger(pending, sep);
                furniture.push(line);
            }
            State::Merged => {
                pending = stronger(pending, sep);
            }
        }
    }
    body.extend(furniture);
    page.lines = body;
    page.text = text;
}

/// Set `line.role` to `role` when the line is still `body` (or untagged), or
/// when `role` is `furniture`. True when the role changed.
fn tag(line: &mut Line, role: &str) -> bool {
    if line.role == role {
        return false;
    }
    let untagged = line.role.is_empty() || line.role == ROLE_BODY;
    if untagged || role == ROLE_FURNITURE {
        line.role = role.to_string();
        true
    } else {
        false
    }
}

/// Tag every line of `page` whose state is `Furniture` (before `rebuild`
/// moves them), counting the new tags.
fn tag_furniture(page: &mut PageText, w: &PageWork, report: &mut CleanupReport) {
    for (k, line) in page.lines.iter_mut().enumerate() {
        if w.state.get(k) == Some(&State::Furniture) && tag(line, ROLE_FURNITURE) {
            report.role_furniture += 1;
        }
    }
}

/// A table-of-contents entry: at least `TOC_MIN_LEADERS` dot-leader runs
/// (` .`, `..` or `…`) and a page number at the end.
fn is_toc(text: &str) -> bool {
    let text = text.trim();
    if !text.chars().next_back().is_some_and(|c| c.is_ascii_digit()) {
        return false;
    }
    let leaders =
        text.matches(" .").count() + text.matches("..").count() + text.matches('\u{2026}').count();
    leaders >= TOC_MIN_LEADERS
}

/// A caption's first line: `Figure`, `Fig.`, `Table`, `Algorithm` or
/// `Listing`, a number (`3`, `S1`, `A1`, `2.1`, `IV`) and then `.`, `:` or
/// `|`.
fn is_caption(text: &str) -> bool {
    caption_re().is_match(text.trim())
}

/// A bare caption start: `Figure`, `Fig.` or `Table`, a number and then a
/// capitalised word with no punctuation between (`Fig. 3 Overview of the`,
/// `Table 2 Results`). `Figure 3 shows` stays body.
fn is_bare_caption(text: &str) -> bool {
    caption_bare_re().is_match(text.trim())
}

/// Words in `text` and how many of them start with a lowercase letter.
fn lowercase_words(text: &str) -> (usize, usize) {
    let mut total: usize = 0;
    let mut lower: usize = 0;
    for token in text.split_whitespace() {
        total += 1;
        let starts_lower = token
            .chars()
            .find(|c| c.is_alphanumeric())
            .is_some_and(char::is_lowercase);
        if starts_lower {
            lower += 1;
        }
    }
    (total, lower)
}

/// The line at `index` directly continues the paragraph of the nearest
/// earlier non-furniture line: that line has at least
/// `CAPTION_PARAGRAPH_WORDS` words, does not end a sentence or a label, and
/// sits just above it with overlapping x ranges.
fn continues_paragraph(page: &PageText, index: usize) -> bool {
    let Some(line) = page.lines.get(index) else {
        return false;
    };
    let Some(prev) = page.lines[..index]
        .iter()
        .rev()
        .find(|l| l.role != ROLE_FURNITURE)
    else {
        return false;
    };
    let (words, _) = lowercase_words(&prev.text);
    if words < CAPTION_PARAGRAPH_WORDS || prev.text.trim_end().ends_with(['.', ':', '!', '?']) {
        return false;
    }
    let (Some(upper), Some(lower)) = (prev.bbox.map(norm), line.bbox.map(norm)) else {
        return false;
    };
    let height = lower.y1 - lower.y0;
    let gap = upper.y0 - lower.y1;
    let overlap = upper.x0 < lower.x1 && lower.x0 < upper.x1;
    overlap && height > 0.0 && gap >= -0.5 * height && gap <= CAPTION_PARAGRAPH_GAP * height
}

/// The line carries an affiliation or contact signal (see
/// `AFFILIATION_SIGNALS`).
fn has_affiliation_signal(text: &str) -> bool {
    AFFILIATION_SIGNALS.iter().any(|s| text.contains(s))
}

/// A line of the prose run that ends the front matter: at least
/// `FRONT_RUN_WORDS` words, at least half of them starting lowercase, and
/// no affiliation signal.
fn is_front_prose(text: &str) -> bool {
    let (total, lower) = lowercase_words(text);
    total >= FRONT_RUN_WORDS && lower * 2 >= total && !has_affiliation_signal(text)
}

/// A line of the shorter prose run that ends the front matter: at least
/// `FRONT_SHORT_RUN_WORDS` words, at least half of them starting lowercase,
/// and no affiliation signal.
fn is_short_front_prose(text: &str) -> bool {
    let (total, lower) = lowercase_words(text);
    total >= FRONT_SHORT_RUN_WORDS && lower * 2 >= total && !has_affiliation_signal(text)
}

/// Lowercase auxiliaries, modals and sentence cues that mark a line as
/// running prose rather than a title (see [`has_verb_cue`]).
const VERB_CUES: [&str; 15] = [
    "is",
    "are",
    "was",
    "were",
    "has",
    "have",
    "can",
    "will",
    "show",
    "shows",
    "propose",
    "present",
    "introduce",
    "we",
    "our",
];

/// The line holds a verb-like word: all letters, at least 5 of them,
/// starting lowercase and ending in `ed` or `ing`. A plural noun
/// (`models`, `systems`) is no verb evidence.
fn has_verb_ending(text: &str) -> bool {
    text.split_whitespace().any(|token| {
        let word = token.trim_matches(|c: char| !c.is_alphabetic());
        word.chars().count() >= 5
            && word.chars().all(char::is_alphabetic)
            && word.chars().next().is_some_and(char::is_lowercase)
            && (word.ends_with("ed") || word.ends_with("ing"))
    })
}

/// The line holds one of the lowercase [`VERB_CUES`] as a whole word.
fn has_verb_cue(text: &str) -> bool {
    text.split_whitespace().any(|token| {
        let word = token.trim_matches(|c: char| !c.is_alphabetic());
        VERB_CUES.contains(&word)
    })
}

/// The line ends a sentence and goes on: a word of at least 3 letters
/// followed by `.`, `?` or `!`, then a word that starts with an
/// upper-case letter (`here. We`). Initials (`J.`), `al.` and `e.g.` do
/// not count.
fn has_sentence_break(text: &str) -> bool {
    let tokens: Vec<&str> = text.split_whitespace().collect();
    tokens.windows(2).any(|pair| {
        let word = pair[0].trim_end_matches(['.', '?', '!']);
        word.len() < pair[0].len()
            && word.chars().count() >= 3
            && word.chars().all(char::is_alphabetic)
            && pair[1].chars().next().is_some_and(char::is_uppercase)
    })
}

/// A long page-1 line that is not front matter: no affiliation signal and
/// not a list of names (at least 30 % of its words start lowercase). Before
/// a recognised abstract line (`before_abstract`) it needs at least
/// `FRONT_SENTENCE_WORDS` words and a verb cue ([`has_verb_cue`]) or a
/// sentence break ([`has_sentence_break`]), so a long sentence-case title
/// stays front matter. Elsewhere it needs at least `FRONT_LONG_WORDS`
/// words, or at least `FRONT_SENTENCE_WORDS` with any of those or a
/// verb-like word ([`has_verb_ending`]).
fn is_long_body_line(text: &str, before_abstract: bool) -> bool {
    let (total, lower) = lowercase_words(text);
    if has_affiliation_signal(text) || lower * 10 < total * 3 {
        return false;
    }
    let sentence = total >= FRONT_SENTENCE_WORDS;
    let strong = has_verb_cue(text) || has_sentence_break(text);
    if before_abstract {
        return sentence && strong;
    }
    total >= FRONT_LONG_WORDS || (sentence && (strong || has_verb_ending(text)))
}

/// Page-1 front matter: the non-furniture lines before the abstract when it
/// starts within `FRONT_MAX_LINES` lines (a standalone `Abstract` line is
/// tagged `heading`), else those before an `Introduction` heading. Either
/// way the front matter also stops at the first run of `FRONT_RUN_LINES`
/// consecutive prose lines (see [`is_front_prose`]) or of
/// `FRONT_SHORT_RUN_LINES` shorter ones (see [`is_short_front_prose`]), an
/// unlabelled abstract or first paragraph, and a long line that is neither
/// an affiliation nor a list of names (see [`is_long_body_line`], stricter
/// before an abstract line) is never tagged.
fn tag_front(page: &mut PageText, report: &mut CleanupReport) {
    let order: Vec<usize> = page
        .lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.role != ROLE_FURNITURE)
        .map(|(k, _)| k)
        .collect();
    let text_of = |k: usize| page.lines.get(k).map_or("", |line| line.text.trim());
    let abstract_at = order
        .iter()
        .take(FRONT_MAX_LINES)
        .position(|k| abstract_re().is_match(text_of(*k)));
    let (end, heading) = if let Some(pos) = abstract_at {
        let standalone = order
            .get(pos)
            .copied()
            .filter(|k| abstract_heading_re().is_match(text_of(*k)));
        (pos, standalone)
    } else if let Some(pos) = order
        .iter()
        .position(|k| introduction_re().is_match(text_of(*k)))
    {
        (pos, None)
    } else {
        return;
    };
    let run_at = order
        .windows(FRONT_RUN_LINES)
        .position(|w| w.iter().all(|k| is_front_prose(text_of(*k))));
    let short_run_at = order
        .windows(FRONT_SHORT_RUN_LINES)
        .position(|w| w.iter().all(|k| is_short_front_prose(text_of(*k))));
    let end = run_at.map_or(end, |run| end.min(run));
    let end = short_run_at.map_or(end, |run| end.min(run));
    let before_abstract = abstract_at.is_some();
    let long: Vec<bool> = order
        .iter()
        .map(|k| is_long_body_line(text_of(*k), before_abstract))
        .collect();
    for (pos, k) in order.iter().enumerate().take(end) {
        if long[pos] {
            continue;
        }
        if let Some(line) = page.lines.get_mut(*k)
            && tag(line, ROLE_FRONT)
        {
            report.role_front += 1;
        }
    }
    if let Some(k) = heading
        && let Some(line) = page.lines.get_mut(k)
        && tag(line, ROLE_HEADING)
    {
        report.role_heading += 1;
    }
}

/// Text-based roles on the final lines: `toc`, `caption`, and on the page
/// that holds the paper's front matter (`front_page`) `front`/`heading`
/// (see [`tag_front`] and [`tag_front_blocks`]). A bare caption start (see
/// [`is_bare_caption`]) is a caption only when it does not continue the
/// paragraph above it (see [`continues_paragraph`]). Never changes `text`.
fn tag_roles(page: &mut PageText, front_page: bool, report: &mut CleanupReport) {
    let kinds: Vec<(bool, bool)> = page
        .lines
        .iter()
        .enumerate()
        .map(|(k, line)| {
            let text = line.text.as_str();
            let toc = is_toc(text);
            let bare = is_bare_caption(text) && !continues_paragraph(page, k);
            (toc, !toc && (is_caption(text) || bare))
        })
        .collect();
    for (line, (toc, caption)) in page.lines.iter_mut().zip(kinds) {
        if line.role == ROLE_FURNITURE {
            continue;
        }
        if toc {
            if tag(line, ROLE_TOC) {
                report.role_toc += 1;
            }
        } else if caption && tag(line, ROLE_CAPTION) {
            report.role_caption += 1;
        }
    }
    if front_page {
        tag_front(page, report);
        tag_front_blocks(page, report);
    }
}

/// Kind of a front-matter block that runs on past its opening line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FrontBlock {
    /// Licence, permission and copyright lines.
    Licence,
    /// `ACM Reference Format:` and the citation under it.
    ReferenceFormat,
    /// `Authors' Contact Information:` and the addresses under it.
    Contact,
    /// `Keywords`, `Index Terms`, `CCS Concepts` and their terms.
    Keywords,
}

impl FrontBlock {
    /// Most lines of the block, its opening line included.
    fn max_lines(self) -> usize {
        match self {
            Self::Keywords => KEYWORDS_MAX_LINES,
            Self::Licence | Self::ReferenceFormat | Self::Contact => FRONT_BLOCK_MAX_LINES,
        }
    }
}

/// The front-matter block `text` opens, if any.
fn front_block_start(text: &str) -> Option<FrontBlock> {
    if licence_re().is_match(text) {
        Some(FrontBlock::Licence)
    } else if reference_format_re().is_match(text) {
        Some(FrontBlock::ReferenceFormat)
    } else if contact_re().is_match(text) {
        Some(FrontBlock::Contact)
    } else if keywords_re().is_match(text) {
        Some(FrontBlock::Keywords)
    } else {
        None
    }
}

/// A superscript letter such as the `ᵃ` of an affiliation mark.
fn is_superscript_letter(c: char) -> bool {
    matches!(
        c,
        '\u{02B0}'..='\u{02B8}'
            | '\u{1D2C}'..='\u{1D61}'
            | '\u{1D9C}'..='\u{1DBF}'
            | '\u{2071}'
            | '\u{207F}'
    )
}

/// A lettered affiliation line: a superscript letter and a capitalised
/// word with a comma or an affiliation signal after it (`ᵃSchool of
/// Physics, ...`); a plain letter `a`-`h`, a space, an institution word
/// among the next [`AFFILIATION_WORD_REACH`] words and at least
/// [`AFFILIATION_MIN_COMMAS`] commas (`a Department of Physics, University
/// of X, Paris`); or a line with two or more superscript letters between a
/// comma and a capital (`USA,ᵇ Entalpic, Paris, France,ᶜ ...`).
fn is_lettered_affiliation(text: &str) -> bool {
    let text = text.trim();
    if affiliation_marker_re().find_iter(text).count() >= 2 {
        return true;
    }
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    let rest = chars.as_str();
    if is_superscript_letter(first) {
        let opens_capital = rest
            .trim_start()
            .chars()
            .next()
            .is_some_and(char::is_uppercase);
        return opens_capital && (rest.contains(',') || has_affiliation_signal(rest));
    }
    if !matches!(first, 'a'..='h') || !rest.starts_with(' ') {
        return false;
    }
    let institution = rest
        .split_whitespace()
        .take(AFFILIATION_WORD_REACH)
        .any(|word| AFFILIATION_WORDS.contains(&word.trim_end_matches(',')));
    institution && rest.matches(',').count() >= AFFILIATION_MIN_COMMAS
}

/// Whether a paragraph starts at each line of `page`: from the separators
/// in `text` when they can be recovered (a line not found there starts
/// one), else from a column change or a vertical gap taller than the line.
fn paragraph_starts(page: &PageText) -> Vec<bool> {
    if let Some((seps, found)) = separators(page) {
        return (0..page.lines.len())
            .map(|k| k == 0 || k >= found || seps.get(k).is_some_and(|sep| is_paragraph_break(sep)))
            .collect();
    }
    let mut starts: Vec<bool> = Vec::with_capacity(page.lines.len());
    for (k, line) in page.lines.iter().enumerate() {
        let Some(prev) = k.checked_sub(1).and_then(|j| page.lines.get(j)) else {
            starts.push(true);
            continue;
        };
        let gap = match (prev.bbox.map(norm), line.bbox.map(norm)) {
            (Some(upper), Some(lower)) => upper.y0 - lower.y1 > lower.y1 - lower.y0,
            _ => false,
        };
        starts.push(prev.column != line.column || gap);
    }
    starts
}

/// The line ends a sentence: its last character is `.`, `!` or `?`.
fn ends_sentence(text: &str) -> bool {
    text.trim_end().ends_with(['.', '!', '?'])
}

/// The first letter of the line is lowercase.
fn starts_lowercase(text: &str) -> bool {
    text.chars()
        .find(|c| c.is_alphabetic())
        .is_some_and(char::is_lowercase)
}

/// Front-matter blocks on the page that holds the paper's front matter:
/// licence and copyright blocks (see [`licence_re`]), `ACM Reference
/// Format:` and `Authors' Contact Information:` blocks, each through the
/// next paragraph break (at most [`FRONT_BLOCK_MAX_LINES`] lines);
/// `Keywords` / `Index Terms` / `CCS Concepts` blocks, which also stop after
/// a line ending with `.` (at most [`KEYWORDS_MAX_LINES`] lines); and
/// lettered affiliation lines (see [`is_lettered_affiliation`]) anywhere on
/// the page. A block never runs into an abstract line or a numbered
/// heading. Tagged `front`.
fn tag_front_blocks(page: &mut PageText, report: &mut CleanupReport) {
    let heads = paragraph_starts(page);
    let mut picked: Vec<usize> = Vec::new();
    let mut block: Option<(FrontBlock, usize)> = None;
    for (k, line) in page.lines.iter().enumerate() {
        if line.role == ROLE_FURNITURE {
            continue;
        }
        let text = line.text.trim();
        if let Some(kind) = front_block_start(text) {
            picked.push(k);
            let closed = kind == FrontBlock::Keywords && text.ends_with('.');
            block = if closed { None } else { Some((kind, 1)) };
            continue;
        }
        if is_lettered_affiliation(text) {
            picked.push(k);
            block = None;
            continue;
        }
        let Some((kind, count)) = block else {
            continue;
        };
        let stop = heads.get(k).copied().unwrap_or(true)
            || count >= kind.max_lines()
            || abstract_re().is_match(text)
            || introduction_re().is_match(text)
            || numbered_heading_re().is_match(text);
        if stop {
            block = None;
            continue;
        }
        picked.push(k);
        let closed = kind == FrontBlock::Keywords && text.ends_with('.');
        block = if closed {
            None
        } else {
            Some((kind, count + 1))
        };
    }
    for k in picked {
        if let Some(line) = page.lines.get_mut(k)
            && tag(line, ROLE_FRONT)
        {
            report.role_front += 1;
        }
    }
}

/// Page 1 is a title page (a highlights or cover page) and page 2 holds the
/// paper's front matter: page 1 has no abstract line and no introduction
/// heading and fewer than [`TITLE_PAGE_MAX_WORDS`] words, and page 2 has an
/// abstract line within its first [`FRONT_MAX_LINES`] lines.
fn is_title_page(first: &PageText, second: &PageText) -> bool {
    if first.page != 1 || second.page != 2 {
        return false;
    }
    let mut words: usize = 0;
    for line in first.lines.iter().filter(|l| l.role != ROLE_FURNITURE) {
        let text = line.text.trim();
        if abstract_re().is_match(text) || introduction_re().is_match(text) {
            return false;
        }
        words += text.split_whitespace().count();
    }
    words < TITLE_PAGE_MAX_WORDS
        && second
            .lines
            .iter()
            .filter(|l| l.role != ROLE_FURNITURE)
            .take(FRONT_MAX_LINES)
            .any(|l| abstract_re().is_match(l.text.trim()))
}

/// A word of a person's name: capitalised (`Samuel`, `COOGAN`, `W.`,
/// `Veres-Vitályos`), letters with combining accents, `-`, `.` and
/// apostrophes only, and not one of [`NOT_NAMES`]; after the first word a
/// lowercase particle (`van`, `de`) also counts.
fn is_name_token(token: &str, first: bool) -> bool {
    let word = token.trim_end_matches(',');
    if !first && NAME_PARTICLES.contains(&word) {
        return true;
    }
    let mut chars = word.chars();
    let Some(head) = chars.next() else {
        return false;
    };
    head.is_uppercase()
        && !NOT_NAMES.contains(&word)
        && chars.all(|c| {
            c.is_alphabetic()
                || matches!(c, '\u{0300}'..='\u{036F}' | '-' | '.' | '\'' | '\u{2019}')
        })
}

/// The words after a name open a biography: an IEEE membership grade
/// (`(Senior Member, IEEE)`), a strong cue ([`BIOGRAPHY_STRONG_CUES`]), or
/// a weak one ([`BIOGRAPHY_WEAK_CUES`]) confirmed by a
/// [`BIOGRAPHY_CONFIRMATIONS`] word in `rest`.
fn biography_cue(rest: &str) -> bool {
    let rest = rest.trim_start_matches([',', ' ']);
    if membership_re().is_match(rest) || BIOGRAPHY_STRONG_CUES.iter().any(|c| rest.starts_with(c)) {
        return true;
    }
    BIOGRAPHY_WEAK_CUES.iter().any(|c| rest.starts_with(c))
        && BIOGRAPHY_CONFIRMATIONS.iter().any(|w| rest.contains(w))
}

/// The line opens an author biography: [`BIOGRAPHY_MIN_NAME_TOKENS`] to
/// [`BIOGRAPHY_MAX_NAME_TOKENS`] name words (see [`is_name_token`]) and
/// then a biography cue (see [`biography_cue`]), read on into `following`
/// (the next line of the paragraph, or empty) when the cue wraps.
fn is_biography_start(text: &str, following: &str) -> bool {
    let tokens: Vec<&str> = text.split_whitespace().collect();
    let names = tokens
        .iter()
        .take(BIOGRAPHY_MAX_NAME_TOKENS)
        .enumerate()
        .take_while(|(n, token)| is_name_token(token, *n == 0))
        .count();
    (BIOGRAPHY_MIN_NAME_TOKENS..=names).rev().any(|m| {
        let rest = format!("{} {following}", tokens[m..].join(" "));
        biography_cue(&rest)
    })
}

/// The line opens a later paragraph of a biography (see
/// [`BIOGRAPHY_PARAGRAPH_OPENERS`]).
fn opens_biography_paragraph(text: &str) -> bool {
    BIOGRAPHY_PARAGRAPH_OPENERS
        .iter()
        .any(|opener| text.starts_with(opener))
}

/// Trimmed text of the line at `seq[pos]` (`(page index, line index,
/// paragraph start)`), or empty.
fn seq_text<'a>(pages: &'a [PageText], seq: &[(usize, usize, bool)], pos: usize) -> &'a str {
    seq.get(pos)
        .and_then(|&(p, k, _)| pages.get(p).and_then(|page| page.lines.get(k)))
        .map_or("", |line| line.text.trim())
}

/// Trimmed text of the line after `seq[pos]` when it continues the same
/// paragraph, else empty.
fn seq_following<'a>(pages: &'a [PageText], seq: &[(usize, usize, bool)], pos: usize) -> &'a str {
    match seq.get(pos + 1) {
        Some(&(_, _, false)) => seq_text(pages, seq, pos + 1),
        _ => "",
    }
}

/// Author biographies, as `(page index, line index)` pairs: in reading
/// order from the last references heading (or from the last
/// [`BIOGRAPHY_TAIL_PAGES`] pages, whichever starts first), each paragraph
/// that opens with a biography start (see [`is_biography_start`]) through
/// the next paragraph break or biography start, at most
/// [`BIOGRAPHY_MAX_LINES`] lines. A paragraph break does not end a
/// biography when the line before it does not end a sentence and the line
/// after it starts lowercase (a biography carried to the next column or
/// page), and a paragraph right after a biography that opens with
/// [`BIOGRAPHY_PARAGRAPH_OPENERS`] is one more. A biographies heading right
/// above a biography start is included.
fn collect_biographies(pages: &[PageText]) -> Vec<(usize, usize)> {
    let mut heading: Option<(usize, usize)> = None;
    for (p, page) in pages.iter().enumerate() {
        for (k, line) in page.lines.iter().enumerate() {
            if line.role != ROLE_FURNITURE && references_heading_re().is_match(line.text.trim()) {
                heading = Some((p, k + 1));
            }
        }
    }
    let tail = (pages.len().saturating_sub(BIOGRAPHY_TAIL_PAGES), 0);
    let from = heading.map_or(tail, |at| at.min(tail));
    let mut seq: Vec<(usize, usize, bool)> = Vec::new();
    for (p, page) in pages.iter().enumerate().skip(from.0) {
        let heads = paragraph_starts(page);
        let mut first = true;
        for (k, line) in page.lines.iter().enumerate() {
            if line.role == ROLE_FURNITURE || (p == from.0 && k < from.1) {
                continue;
            }
            seq.push((p, k, first || heads.get(k).copied().unwrap_or(true)));
            first = false;
        }
    }
    let mut picked: Vec<(usize, usize)> = Vec::new();
    let mut current: Option<usize> = None;
    let mut after_biography = false;
    let mut prev_unfinished = false;
    for (pos, &(p, k, start)) in seq.iter().enumerate() {
        let text = seq_text(pages, &seq, pos);
        let unfinished = !ends_sentence(text);
        if is_biography_start(text, seq_following(pages, &seq, pos)) {
            picked.push((p, k));
            current = Some(1);
            after_biography = true;
        } else if biographies_heading_re().is_match(text)
            && is_biography_start(
                seq_text(pages, &seq, pos + 1),
                seq_following(pages, &seq, pos + 1),
            )
        {
            picked.push((p, k));
            current = None;
        } else if let Some(count) = current.filter(|count| {
            *count < BIOGRAPHY_MAX_LINES && (!start || (prev_unfinished && starts_lowercase(text)))
        }) {
            picked.push((p, k));
            current = Some(count + 1);
        } else if start && after_biography && opens_biography_paragraph(text) {
            picked.push((p, k));
            current = Some(1);
        } else {
            current = None;
            if start {
                after_biography = false;
            }
        }
        prev_unfinished = unfinished;
    }
    picked
}

/// Tag author biographies `biography` (see [`collect_biographies`]).
fn tag_biographies(pages: &mut [PageText], report: &mut CleanupReport) {
    for (p, k) in collect_biographies(pages) {
        if let Some(line) = pages.get_mut(p).and_then(|page| page.lines.get_mut(k))
            && tag(line, ROLE_BIOGRAPHY)
        {
            report.role_biography += 1;
        }
    }
}

/// Median font size of the page's non-furniture lines of at least
/// [`BODY_SIZE_MIN_WORDS`] words; `None` with fewer than
/// [`BODY_SIZE_MIN_LINES`] such lines.
fn page_body_size(page: &PageText) -> Option<f32> {
    let mut sizes: Vec<f32> = page
        .lines
        .iter()
        .filter(|line| {
            line.role != ROLE_FURNITURE
                && line.text.split_whitespace().count() >= BODY_SIZE_MIN_WORDS
        })
        .filter_map(|line| line_size(page, line))
        .collect();
    if sizes.len() < BODY_SIZE_MIN_LINES {
        return None;
    }
    sizes.sort_unstable_by(f32::total_cmp);
    Some(sizes[sizes.len() / 2])
}

/// The line opens with a footnote marker: `*`, `∗`, `†`, `‡`, `§`, `¶`, a
/// superscript digit or letter, or 1 or 2 digits, a space and a letter.
fn starts_footnote_marker(text: &str) -> bool {
    let text = text.trim_start();
    let Some(first) = text.chars().next() else {
        return false;
    };
    if matches!(
        first,
        '*' | '\u{2217}' | '\u{2020}' | '\u{2021}' | '\u{00A7}' | '\u{00B6}'
    ) || SUPERSCRIPT_DIGITS.contains(&first)
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

/// The line sits in the footnote zone (see [`FOOTNOTE_ZONE`]) and is set
/// at most `limit` points.
fn is_small_low(page: &PageText, line: &Line, limit: f32) -> bool {
    let small = line_size(page, line).is_some_and(|s| s <= limit);
    let low = line
        .bbox
        .map(norm)
        .is_some_and(|b| b.y0.midpoint(b.y1) <= FOOTNOTE_ZONE * page.height);
    small && low
}

/// Horizontal rules in the page's footnote zone no wider than
/// [`RULE_MAX_PAGE_SHARE`] of the page: figure boxes lower than
/// [`RULE_HEIGHT`] and at least [`RULE_MIN_WIDTH`] wide.
fn footnote_rules(page: &PageText) -> Vec<BBox> {
    page.figures
        .iter()
        .filter_map(|figure| figure.bbox.map(norm))
        .filter(|b| {
            let width = b.x1 - b.x0;
            b.y1 - b.y0 < RULE_HEIGHT
                && width >= RULE_MIN_WIDTH
                && width <= RULE_MAX_PAGE_SHARE * page.width
                && b.y0.midpoint(b.y1) <= FOOTNOTE_ZONE * page.height
        })
        .collect()
}

/// Footnotes under a footnote rule: every non-furniture line below a rule
/// from [`footnote_rules`] that starts within its left end (from
/// [`RULE_X_BEFORE`] left of it to [`RULE_X_INDENT`] right of it). They
/// are tagged `footnote`, markers or not, when there is at least one and
/// at most [`RULED_FOOTNOTE_MAX_LINES`], the first starts within
/// [`RULE_REACH`] of the rule, and every one is a `body` (or `footnote`)
/// line set at most [`FOOTNOTE_SIZE_RATIO`] times the page's body size.
fn tag_ruled_footnotes(page: &mut PageText, report: &mut CleanupReport) {
    if !page.height.is_finite() || page.height <= 0.0 {
        return;
    }
    let Some(body) = page_body_size(page) else {
        return;
    };
    let limit = FOOTNOTE_SIZE_RATIO * body;
    let mut picked: Vec<usize> = Vec::new();
    for rule in footnote_rules(page) {
        let mut below: Vec<usize> = Vec::new();
        let mut top: f32 = f32::NEG_INFINITY;
        for (k, line) in page.lines.iter().enumerate() {
            if line.role == ROLE_FURNITURE {
                continue;
            }
            let Some(b) = line.bbox.map(norm) else {
                continue;
            };
            let aligned = b.x0 >= rule.x0 - RULE_X_BEFORE && b.x0 <= rule.x0 + RULE_X_INDENT;
            if aligned && b.y1 <= rule.y0 + RULE_OVERLAP {
                below.push(k);
                top = top.max(b.y1);
            }
        }
        let fits = !below.is_empty()
            && below.len() <= RULED_FOOTNOTE_MAX_LINES
            && top >= rule.y0 - RULE_REACH
            && below.iter().all(|&k| {
                let line = &page.lines[k];
                (line.role == ROLE_BODY || line.role == ROLE_FOOTNOTE)
                    && line_size(page, line).is_some_and(|s| s <= limit)
            });
        if fits {
            picked.extend(below);
        }
    }
    for k in picked {
        if let Some(line) = page.lines.get_mut(k)
            && tag(line, ROLE_FOOTNOTE)
        {
            report.role_footnote += 1;
        }
    }
}

/// The page ends inside a footnote: its last non-furniture line in reading
/// order is a small line in the footnote zone (see [`is_small_low`]) that
/// does not end a sentence, and it is tagged `footnote` or belongs to a
/// run of such lines in its column, at most [`FOOTNOTE_MAX_LINES`] long,
/// that opens with a footnote marker (see [`starts_footnote_marker`]).
fn ends_in_open_footnote(page: &PageText) -> bool {
    let Some(body) = page_body_size(page) else {
        return false;
    };
    let limit = FOOTNOTE_SIZE_RATIO * body;
    let mut lines = page
        .lines
        .iter()
        .rev()
        .filter(|line| line.role != ROLE_FURNITURE);
    let Some(last) = lines.next() else {
        return false;
    };
    if ends_sentence(&last.text) || !is_small_low(page, last, limit) {
        return false;
    }
    if last.role == ROLE_FOOTNOTE {
        return true;
    }
    for (run, line) in std::iter::once(last).chain(lines).enumerate() {
        let member = line.column == last.column
            && (line.role == ROLE_BODY || line.role == ROLE_FOOTNOTE)
            && is_small_low(page, line, limit);
        if !member || run >= FOOTNOTE_MAX_LINES {
            return false;
        }
        if line.role == ROLE_FOOTNOTE || starts_footnote_marker(&line.text) {
            return true;
        }
    }
    false
}

/// A footnote carried over from the previous page: when that page ends
/// inside a footnote (see [`ends_in_open_footnote`]), the bottom run of the
/// first column (in reading order) that has one: its last `body` lines set
/// at most [`FOOTNOTE_SIZE_RATIO`] times the body size in the footnote
/// zone, at most [`FOOTNOTE_MAX_LINES`] of them, below a larger line and
/// opening without a footnote marker. The whole run is tagged `footnote`
/// (so the marker notes below the carried text stay one run).
fn tag_footnote_continuations(pages: &mut [PageText], report: &mut CleanupReport) {
    for i in 1..pages.len() {
        let consecutive = pages[i - 1].page.checked_add(1) == Some(pages[i].page);
        if !consecutive || !ends_in_open_footnote(&pages[i - 1]) {
            continue;
        }
        let page = &pages[i];
        if !page.height.is_finite() || page.height <= 0.0 {
            continue;
        }
        let Some(body) = page_body_size(page) else {
            continue;
        };
        let limit = FOOTNOTE_SIZE_RATIO * body;
        let mut columns: Vec<u32> = Vec::new();
        for line in page.lines.iter().filter(|l| l.role != ROLE_FURNITURE) {
            if !columns.contains(&line.column) {
                columns.push(line.column);
            }
        }
        let mut picked: Vec<usize> = Vec::new();
        for column in columns {
            let members: Vec<usize> = page
                .lines
                .iter()
                .enumerate()
                .filter(|(_, l)| l.role != ROLE_FURNITURE && l.column == column)
                .map(|(k, _)| k)
                .collect();
            let mut run: Vec<usize> = Vec::new();
            let mut above_large = false;
            for &k in members.iter().rev() {
                let line = &page.lines[k];
                if line.role == ROLE_BODY && is_small_low(page, line, limit) {
                    run.push(k);
                    continue;
                }
                above_large = line_size(page, line).is_some_and(|s| s > limit);
                break;
            }
            if run.is_empty() {
                continue;
            }
            run.reverse();
            let carried = run
                .first()
                .is_some_and(|&k| !starts_footnote_marker(&page.lines[k].text));
            if above_large && carried && run.len() <= FOOTNOTE_MAX_LINES {
                picked = run;
            }
            break;
        }
        for k in picked {
            if let Some(line) = pages[i].lines.get_mut(k)
                && tag(line, ROLE_FOOTNOTE)
            {
                report.role_footnote += 1;
            }
        }
    }
}

/// Clean the ordered text of a whole document in place: rule 4 (`arXiv`
/// stamp on page 1), rule 1 (page numbers and running headers/footers in
/// the top or bottom 8 % of the page, or up to 15 % for strongly repeated
/// running heads), rule 3 (sub/superscript fragments
/// merged into their base line) and rule 2 (line-end hyphenation within a
/// column and across a page break), in that order; then tags line roles
/// (`furniture`, `toc`, `caption`, `front` and `heading` on page 1, or on
/// page 2 after a title page, `biography` after the references, and
/// `footnote` under a footnote rule or carried over from the previous page)
/// without changing `text`. Requires `lines` and `text` from `reading_order`; never
/// touches `spans`. See the module documentation for how removed lines are
/// kept.
pub fn clean_document(pages: &mut [PageText]) -> CleanupReport {
    let mut report = CleanupReport::default();
    let mut work: Vec<PageWork> = pages.iter().map(prepare).collect();
    for (page, w) in pages.iter().zip(&work) {
        if !w.eligible && !page.lines.is_empty() {
            report.pages_skipped += 1;
        }
    }
    mark_stamps(pages, &mut work, &mut report);
    mark_furniture(pages, &mut work, &mut report);
    for (page, w) in pages.iter_mut().zip(work.iter_mut()) {
        if w.eligible {
            let (merged, superscripts) = merge_scripts(page, w);
            report.scripts_merged += merged;
            report.superscripts_merged += superscripts;
        }
    }
    let vocab = vocabulary(pages, &work);
    join_hyphens(pages, &mut work, &vocab, &mut report);
    for (page, w) in pages.iter_mut().zip(&work) {
        if w.eligible {
            tag_furniture(page, w, &mut report);
        }
        if !w.changed {
            continue;
        }
        rebuild(page, w);
        if w.removed > 0 {
            let removed = w.removed;
            let msg = format!("furniture removed: {removed}");
            if !page.warnings.contains(&msg) {
                page.warnings.push(msg);
            }
        }
    }
    let title_page = pages.len() >= 2 && is_title_page(&pages[0], &pages[1]);
    for page in pages.iter_mut() {
        let front_page = page.page == 1 || (title_page && page.page == 2);
        tag_roles(page, front_page, &mut report);
    }
    tag_biographies(pages, &mut report);
    for page in pages.iter_mut() {
        tag_ruled_footnotes(page, &mut report);
    }
    tag_footnote_continuations(pages, &mut report);
    report
}

/// Compile every regex this module uses, so the first document does not pay
/// for it inside its stage timings. Repeated calls are cheap.
pub fn warm_up() {
    let accessors: &[fn() -> &'static Regex] = &[
        page_number_re,
        roman_re,
        stamp_re,
        abstract_re,
        abstract_heading_re,
        introduction_re,
        caption_re,
        caption_bare_re,
        licence_re,
        reference_format_re,
        contact_re,
        keywords_re,
        numbered_heading_re,
        affiliation_marker_re,
        references_heading_re,
        biographies_heading_re,
        membership_re,
    ];
    for accessor in accessors {
        accessor();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{Figure, Span};

    fn span_at(text: &str, x0: f32, y0: f32, size: f32, seq: u32) -> Span {
        let width = 0.5 * size * text.chars().count() as f32;
        Span {
            text: text.to_string(),
            bbox: Some(BBox {
                x0,
                y0,
                x1: x0 + width,
                y1: y0 + size,
            }),
            font: None,
            size: Some(size),
            seq,
        }
    }

    fn joined(lines: &[Line]) -> String {
        let texts: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
        texts.join("\n")
    }

    /// A US Letter page with one 10 pt span and one line per row
    /// `(text, x0, y0, column)`; `text` is the line texts joined by `\n`.
    fn page_of(number: u32, rows: &[(&str, f32, f32, u32)]) -> PageText {
        let mut page = PageText::new(number, 612.0, 792.0, 0);
        for (i, (text, x0, y0, column)) in rows.iter().enumerate() {
            let seq = u32::try_from(i).unwrap();
            let span = span_at(text, *x0, *y0, 10.0, seq);
            page.lines.push(Line {
                text: (*text).to_string(),
                bbox: span.bbox,
                column: *column,
                spans: vec![seq],
                role: ROLE_BODY.to_string(),
            });
            page.spans.push(span);
        }
        page.text = joined(&page.lines);
        page
    }

    fn texts(page: &PageText) -> Vec<&str> {
        page.lines.iter().map(|l| l.text.as_str()).collect()
    }

    fn furniture_pages() -> Vec<PageText> {
        (1..=3)
            .map(|n| {
                let header = format!("Journal of Testing, vol. {n}");
                let body = format!("Body line one of page {n}");
                let number = n.to_string();
                page_of(
                    n,
                    &[
                        (header.as_str(), 60.0, 760.0, 0),
                        (body.as_str(), 60.0, 600.0, 1),
                        ("Body line two", 60.0, 588.0, 1),
                        (number.as_str(), 300.0, 30.0, 2),
                    ],
                )
            })
            .collect()
    }

    #[test]
    fn page_number_forms() {
        assert!(is_page_number("12"));
        assert!(is_page_number("- 12 -"));
        assert!(is_page_number("– 7 –"));
        assert!(is_page_number("Page 12 of 30"));
        assert!(is_page_number("12 / 30"));
        assert!(is_page_number("iv"));
        assert!(is_page_number("xii"));
        assert!(!is_page_number("civil"));
        assert!(!is_page_number("ill"));
        assert!(!is_page_number("12 apples"));
        assert!(!is_page_number("Figure 3"));
        assert!(!is_page_number(""));
        assert_eq!(digit_key("Journal  12, vol. 345"), "Journal #, vol. #");
    }

    #[test]
    fn running_header_and_page_number_leave_the_text_only() {
        let mut pages = furniture_pages();
        let spans_before: Vec<Vec<Span>> = pages.iter().map(|p| p.spans.clone()).collect();
        let report = clean_document(&mut pages);
        assert_eq!(report.running_lines, 3);
        assert_eq!(report.page_numbers, 3);
        for (i, page) in pages.iter().enumerate() {
            let n = i + 1;
            assert_eq!(
                page.text,
                format!("Body line one of page {n}\nBody line two")
            );
            assert_eq!(page.lines.len(), 4, "furniture stays in lines");
            assert_eq!(page.lines[2].text, format!("Journal of Testing, vol. {n}"));
            assert_eq!(page.lines[3].text, n.to_string());
            assert_eq!(page.lines[0].role, "body");
            assert_eq!(page.lines[2].role, "furniture");
            assert_eq!(page.lines[3].role, "furniture");
            assert!(page.warnings.contains(&"furniture removed: 2".to_string()));
            assert_eq!(page.spans, spans_before[i], "spans are evidence");
        }
    }

    #[test]
    fn number_sharing_its_row_with_text_is_not_a_page_number() {
        let mut pages = vec![page_of(
            1,
            &[
                ("HELLO WORLD SCAN", 60.0, 760.0, 0),
                ("2026", 250.0, 760.0, 0),
                ("Body text", 60.0, 600.0, 0),
            ],
        )];
        let before = pages[0].text.clone();
        let report = clean_document(&mut pages);
        assert_eq!(report.page_numbers, 0);
        assert_eq!(pages[0].text, before);
    }

    #[test]
    fn cleanup_is_idempotent() {
        let mut pages = furniture_pages();
        clean_document(&mut pages);
        let once = pages.clone();
        let report = clean_document(&mut pages);
        assert_eq!(report, CleanupReport::default());
        assert_eq!(pages, once);
    }

    #[test]
    fn single_edge_line_stays() {
        let mut pages = vec![
            page_of(
                1,
                &[
                    ("A Title In The Top Band", 60.0, 760.0, 0),
                    ("Body text on page one", 60.0, 600.0, 1),
                ],
            ),
            page_of(2, &[("Body text on page two", 60.0, 600.0, 0)]),
        ];
        let before: Vec<String> = pages.iter().map(|p| p.text.clone()).collect();
        let report = clean_document(&mut pages);
        assert_eq!(report, CleanupReport::default());
        assert_eq!(pages[0].text, before[0]);
        assert_eq!(pages[1].text, before[1]);
        assert!(pages[0].warnings.is_empty());
    }

    #[test]
    fn hyphen_joins_when_the_word_is_attested() {
        let mut pages = vec![page_of(
            1,
            &[
                ("We provide a comprehen-", 60.0, 600.0, 0),
                ("sive review of the field.", 60.0, 588.0, 0),
                ("A comprehensive survey follows.", 60.0, 576.0, 0),
            ],
        )];
        let report = clean_document(&mut pages);
        assert_eq!(report.hyphens_joined, 1);
        assert_eq!(
            pages[0].text,
            "We provide a comprehensive\nreview of the field.\nA comprehensive survey follows."
        );
        assert_eq!(
            &texts(&pages[0])[..2],
            ["We provide a comprehensive", "review of the field."]
        );
    }

    #[test]
    fn hyphen_joins_without_evidence_unless_a_compound_prefix() {
        let mut pages = vec![page_of(
            1,
            &[
                ("the proces-", 60.0, 600.0, 0),
                ("sing step is fast, and a self-", 60.0, 588.0, 0),
                ("supervised model is used.", 60.0, 576.0, 0),
            ],
        )];
        let report = clean_document(&mut pages);
        assert_eq!(report.hyphens_joined, 1);
        assert_eq!(report.hyphens_kept, 1);
        assert_eq!(
            pages[0].text,
            "the processing\nstep is fast, and a self-\nsupervised model is used."
        );
    }

    #[test]
    fn attested_hyphenated_form_keeps_the_hyphen() {
        let mut pages = vec![page_of(
            1,
            &[
                ("we train the gas-", 60.0, 600.0, 0),
                ("leak detector on a gas-leak dataset.", 60.0, 588.0, 0),
            ],
        )];
        let before = pages[0].text.clone();
        let report = clean_document(&mut pages);
        assert_eq!(report.hyphens_kept, 1);
        assert_eq!(report.hyphens_joined, 0);
        assert_eq!(pages[0].text, before);
    }

    #[test]
    fn hyphen_stays_when_both_halves_are_words_elsewhere() {
        let mut pages = vec![page_of(
            1,
            &[
                ("a cost-", 60.0, 600.0, 0),
                ("effective method at low cost.", 60.0, 588.0, 0),
                ("It is effective in practice.", 60.0, 576.0, 0),
                ("the Dual-", 60.0, 564.0, 0),
                ("channel design uses a dual layout", 60.0, 552.0, 0),
                ("and one channel per link.", 60.0, 540.0, 0),
            ],
        )];
        let before = pages[0].text.clone();
        let report = clean_document(&mut pages);
        assert_eq!(report.hyphens_kept, 2);
        assert_eq!(report.hyphens_joined, 0);
        assert_eq!(pages[0].text, before);
        assert_eq!(texts(&pages[0])[0], "a cost-");
        assert_eq!(texts(&pages[0])[3], "the Dual-");
    }

    #[test]
    fn hyphen_joins_when_attested_or_when_halves_are_not_words() {
        let mut pages = vec![page_of(
            1,
            &[
                ("the opti-", 60.0, 600.0, 0),
                ("mization step runs first.", 60.0, 588.0, 0),
                ("Our optimization is fast.", 60.0, 576.0, 0),
                ("the algo-", 60.0, 564.0, 0),
                ("rithm ends.", 60.0, 552.0, 0),
            ],
        )];
        let report = clean_document(&mut pages);
        assert_eq!(report.hyphens_joined, 2);
        assert_eq!(report.hyphens_kept, 0);
        assert_eq!(
            pages[0].text,
            "the optimization\nstep runs first.\nOur optimization is fast.\n\
             the algorithm\nends."
        );
    }

    /// [`hyphen_policy`] against a fixed document vocabulary.
    fn policy(left: &str, right: &str, seen: &[&str]) -> HyphenPolicy {
        let vocab: BTreeSet<String> = seen.iter().map(|word| String::from(*word)).collect();
        hyphen_policy(left, right, &|piece: &str| vocab.contains(piece))
    }

    /// An attested joined word wins over every later keep rule.
    #[test]
    fn hyphen_policy_joins_an_attested_word_first() {
        use HyphenPolicy::Join;
        assert_eq!(policy("with", "out", &["with", "out", "without"]), Join);
        assert_eq!(policy("work", "flow", &["work", "flow", "workflow"]), Join);
        assert_eq!(policy("opti", "mization", &["optimization"]), Join);
        assert_eq!(policy("self", "supervised", &["selfsupervised"]), Join);
    }

    /// An attested hyphenated pair keeps the hyphen, also when the joined
    /// word is attested as often (this case joined before the pair rule
    /// moved ahead of the joined-word rule).
    #[test]
    fn hyphen_policy_keeps_an_attested_compound() {
        use HyphenPolicy::Keep;
        assert_eq!(policy("noise", "regularized", &["noise-regularized"]), Keep);
        assert_eq!(policy("pre", "serving", &["pre-serving"]), Keep);
        assert_eq!(
            policy(
                "noise",
                "regularized",
                &["noiseregularized", "noise-regularized"]
            ),
            Keep
        );
    }

    /// [`hyphen_policy_counted`] against fixed occurrence counts.
    fn counted(left: &str, right: &str, seen: &[(&str, usize)]) -> HyphenPolicy {
        let counts: BTreeMap<String, usize> = seen
            .iter()
            .map(|(piece, n)| (String::from(*piece), *n))
            .collect();
        hyphen_policy_counted(left, right, &|piece: &str| {
            counts.get(piece).copied().unwrap_or(0)
        })
    }

    /// The pair rule runs first whenever the pair occurs at least as often
    /// as the joined word; a more frequent joined word still joins.
    #[test]
    fn hyphen_policy_counts_pair_against_joined_word() {
        use HyphenPolicy::{Join, Keep};
        let tie = [("multi-agent", 1), ("multiagent", 1)];
        assert_eq!(counted("multi", "agent", &tie), Keep);
        let pair_more = [("multi-agent", 3), ("multiagent", 2)];
        assert_eq!(counted("multi", "agent", &pair_more), Keep);
        let joined_more = [("multi-agent", 1), ("multiagent", 2)];
        assert_eq!(counted("multi", "agent", &joined_more), Join);
        assert_eq!(counted("opti", "mization", &[("optimization", 1)]), Join);
    }

    /// A capitalised compound prefix before an attested lowercase word
    /// keeps its hyphen unless the joined word occurs at least twice as
    /// often as the pair; broken words whose right half is no word, and
    /// capitalised words that are no compound prefix, still join.
    #[test]
    fn hyphen_policy_keeps_capitalised_prefix_compounds() {
        use HyphenPolicy::{Join, Keep};
        // `multi` is a bound prefix: without the capital rule this joins.
        assert_eq!(counted("Multi", "agent", &[("agent", 3)]), Keep);
        assert_eq!(counted("multi", "agent", &[("agent", 3)]), Join);
        assert_eq!(counted("Cross", "domain", &[("domain", 1)]), Keep);
        assert_eq!(counted("Dual", "channel", &[("channel", 1)]), Keep);
        // `Super` is no compound prefix: the bound-prefix rule joins it.
        assert_eq!(counted("Super", "resolution", &[("resolution", 2)]), Join);
        // An ordinary capitalised word split at the line end joins, also
        // when its right half occurs elsewhere.
        assert_eq!(counted("Every", "body", &[("body", 2)]), Join);
        assert_eq!(counted("Over", "all", &[("all", 3)]), Join);
        // The joined word below twice the pair's count keeps the hyphen.
        let close = [("agent", 1), ("multi-agent", 2), ("multiagent", 3)];
        assert_eq!(counted("Multi", "agent", &close), Keep);
        // Twice as often, or seen with no pair at all, joins.
        let twice = [("agent", 1), ("multi-agent", 1), ("multiagent", 2)];
        assert_eq!(counted("Multi", "agent", &twice), Join);
        let only_joined = [("agent", 1), ("multiagent", 1)];
        assert_eq!(counted("Multi", "agent", &only_joined), Join);
        assert_eq!(
            counted("Self", "supervised", &[("selfsupervised", 1)]),
            Join
        );
        // The right half must be an attested, unambiguous word.
        assert_eq!(counted("Multi", "agent", &[]), Join);
        assert_eq!(counted("Addi", "tionally", &[]), Join);
        assert_eq!(counted("How", "ever", &[("ever", 1), ("however", 1)]), Join);
        assert_eq!(counted("Pro", "ing", &[("ing", 1)]), Join);
        // Other or all-capital left halves are not covered.
        assert_eq!(counted("Presence", "only", &[("only", 4)]), Join);
        assert_eq!(counted("MULTI", "agent", &[("agent", 1)]), Join);
    }

    /// All-capital halves join when the joined word is attested or has at
    /// least six letters, unless both halves are attested words.
    #[test]
    fn hyphen_policy_joins_all_capital_breaks() {
        use HyphenPolicy::{Join, Keep};
        assert_eq!(counted("EFFI", "CIENT", &[]), Join);
        assert_eq!(counted("TIME", "DIAL", &[("time", 4)]), Join);
        assert_eq!(counted("ABC", "DEF", &[("abcdef", 1)]), Join);
        // Short, attested as a pair, both halves words, or prefix: keep.
        assert_eq!(counted("ABC", "DE", &[]), Keep);
        assert_eq!(counted("RGB", "D", &[]), Keep);
        assert_eq!(counted("EFFI", "CIENT", &[("effi-cient", 1)]), Keep);
        let words = [("large", 1), ("scale", 2)];
        assert_eq!(counted("LARGE", "SCALE", &words), Keep);
        assert_eq!(counted("SELF", "SUPERVISED", &[]), Keep);
        // An all-capital left half before a lowercase or mixed word stays.
        assert_eq!(counted("MRI", "guided", &[]), Keep);
        assert_eq!(counted("MRI", "Guided", &[]), Keep);
    }

    /// The vocabulary counts words and hyphenated pairs, leaving out the
    /// halves of a line-end hyphen.
    #[test]
    fn vocabulary_counts_occurrences() {
        let pages = vec![page_of(
            1,
            &[
                (
                    "a multi-task model and a multi-task loss for data",
                    60.0,
                    600.0,
                    0,
                ),
                ("with multitask data and a pre-", 60.0, 588.0, 0),
                ("training step.", 60.0, 576.0, 0),
            ],
        )];
        let work: Vec<PageWork> = pages.iter().map(prepare).collect();
        let vocab = vocabulary(&pages, &work);
        assert_eq!(vocab.count("multi-task"), 2);
        assert_eq!(vocab.count("multitask"), 1);
        assert_eq!(vocab.count("data"), 2);
        assert_eq!(vocab.count("task"), 2);
        assert_eq!(vocab.count("pre"), 0);
        assert_eq!(vocab.count("training"), 0);
        assert_eq!(vocab.count("step"), 1);
    }

    /// Capitalised compounds, pairs attested as often as the joined word,
    /// all-capital breaks and a capitalised word that is no compound prefix
    /// (`Every-` + `body`, `body` seen) through the whole pass.
    #[test]
    fn capitalised_and_all_capital_hyphens_in_the_document_pass() {
        let mut pages = vec![page_of(
            1,
            &[
                ("the Multi-", 60.0, 600.0, 0),
                ("agent planner and one agent per task.", 60.0, 588.0, 0),
                ("OUR EFFI-", 60.0, 576.0, 0),
                ("CIENT MODELS", 60.0, 564.0, 0),
                ("a multi-", 60.0, 552.0, 0),
                (
                    "task model, a multi-task loss and multitask data.",
                    60.0,
                    540.0,
                    0,
                ),
                ("see the Proto-", 60.0, 528.0, 0),
                ("Indo text.", 60.0, 516.0, 0),
                ("so Every-", 60.0, 504.0, 0),
                ("body agrees on one body plan.", 60.0, 492.0, 0),
            ],
        )];
        let report = clean_document(&mut pages);
        assert_eq!(report.hyphens_joined, 2);
        assert_eq!(report.hyphens_kept, 2);
        assert_eq!(
            pages[0].text,
            "the Multi-\nagent planner and one agent per task.\n\
             OUR EFFICIENT\nMODELS\n\
             a multi-\ntask model, a multi-task loss and multitask data.\n\
             see the Proto-\nIndo text.\n\
             so Everybody\nagrees on one body plan."
        );
    }

    /// Rules 3 and 4: compound prefixes keep, bound prefixes join.
    #[test]
    fn hyphen_policy_prefix_lists() {
        use HyphenPolicy::{Join, Keep};
        assert_eq!(policy("self", "supervised", &[]), Keep);
        assert_eq!(policy("Cross", "domain", &[]), Keep);
        assert_eq!(policy("well", "known", &[]), Keep);
        assert_eq!(policy("pre", "serving", &[]), Join);
        assert_eq!(policy("pre", "serving", &["pre", "serving"]), Join);
        assert_eq!(policy("non", "linear", &[]), Join);
        assert_eq!(policy("multi", "modal", &[]), Join);
        assert_eq!(policy("re", "use", &["use"]), Join);
        // A capitalised right half is not a bound-prefix join.
        assert_eq!(policy("pre", "MRI", &["mri"]), Keep);
        // Too short and unattested: decided by the later rules.
        assert_eq!(policy("co", "rn", &[]), Keep);
    }

    /// Rule 5: capitals, digits, one-letter or acronym left halves and
    /// right halves too short for a typeset break keep the hyphen.
    #[test]
    fn hyphen_policy_keeps_printed_compounds() {
        use HyphenPolicy::{Join, Keep};
        assert_eq!(policy("most", "dl", &["most"]), Keep);
        assert_eq!(policy("MOST", "dl", &[]), Keep);
        assert_eq!(policy("deep", "ai", &[]), Keep);
        assert_eq!(policy("deep", "cnn", &[]), Keep);
        assert_eq!(policy("k", "space", &["space"]), Keep);
        assert_eq!(policy("MRI", "guided", &[]), Keep);
        assert_eq!(policy("resnet", "v2", &[]), Keep);
        assert_eq!(policy("state", "of", &[]), Keep);
        // Three-letter word endings are ordinary breaks.
        assert_eq!(policy("learn", "ing", &[]), Join);
        assert_eq!(policy("cur", "ves", &[]), Join);
    }

    /// Rule 6 and the fallback join.
    #[test]
    fn hyphen_policy_keeps_two_attested_words_and_joins_the_rest() {
        use HyphenPolicy::{Join, Keep};
        assert_eq!(policy("cost", "effective", &["cost", "effective"]), Keep);
        assert_eq!(policy("Dual", "domain", &["dual", "domain"]), Keep);
        assert_eq!(policy("Dual", "channel", &["dual", "channel"]), Keep);
        assert_eq!(
            policy("noise", "regularized", &["noise", "regularized"]),
            Keep
        );
        // A three-letter half counts when it is not an ambiguous one.
        assert_eq!(policy("web", "based", &["web", "based"]), Keep);
        // `out` is an ambiguous short half: `without` unseen still joins.
        assert_eq!(policy("with", "out", &["with", "out"]), Join);
        assert_eq!(policy("con", "tent", &["con", "tent"]), Join);
        assert_eq!(policy("out", "put", &["out", "put"]), Join);
        assert_eq!(policy("in", "formation", &["in", "formation"]), Join);
        assert_eq!(policy("cost", "effective", &["cost"]), Join);
        assert_eq!(policy("opti", "mization", &[]), Join);
        assert_eq!(policy("algo", "rithm", &[]), Join);
        assert_eq!(policy("noise", "regularized", &[]), Join);
    }

    /// The observed reference-title cases through the whole pass.
    #[test]
    fn hyphen_policy_in_the_document_pass() {
        let mut pages = vec![page_of(
            1,
            &[
                ("structure pre-", 60.0, 600.0, 0),
                ("serving reconstruction of scans.", 60.0, 588.0, 0),
                ("the with-", 60.0, 576.0, 0),
                (
                    "out step, with and without it, out of range.",
                    60.0,
                    564.0,
                    0,
                ),
                ("a noise-", 60.0, 552.0, 0),
                (
                    "regularized prior and a noise-regularized loss.",
                    60.0,
                    540.0,
                    0,
                ),
                ("the most-", 60.0, 528.0, 0),
                ("dl network.", 60.0, 516.0, 0),
            ],
        )];
        let report = clean_document(&mut pages);
        assert_eq!(report.hyphens_joined, 2);
        assert_eq!(report.hyphens_kept, 2);
        assert_eq!(
            pages[0].text,
            "structure preserving\nreconstruction of scans.\n\
             the without\nstep, with and without it, out of range.\n\
             a noise-\nregularized prior and a noise-regularized loss.\n\
             the most-\ndl network."
        );
    }

    #[test]
    fn hyphen_is_not_joined_across_a_paragraph_break() {
        let mut pages = vec![page_of(
            1,
            &[
                ("a list ends with comprehen-", 60.0, 600.0, 0),
                ("sive text starts a new paragraph.", 60.0, 580.0, 0),
            ],
        )];
        pages[0].text =
            "a list ends with comprehen-\n\nsive text starts a new paragraph.".to_string();
        let before = pages[0].text.clone();
        let report = clean_document(&mut pages);
        assert_eq!(report.hyphens_joined, 0);
        assert_eq!(report.hyphens_kept, 0);
        assert_eq!(pages[0].text, before);
    }

    #[test]
    fn hyphen_is_not_joined_across_columns_urls_or_capitals() {
        let mut pages = vec![page_of(
            1,
            &[
                ("a comprehen-", 60.0, 600.0, 0),
                (
                    "sive review, see https://example.org/data-",
                    320.0,
                    600.0,
                    1,
                ),
                ("set for the files of the Proto-", 320.0, 588.0, 1),
                ("Indo text.", 320.0, 576.0, 1),
            ],
        )];
        let before = pages[0].text.clone();
        let report = clean_document(&mut pages);
        assert_eq!(report.hyphens_joined, 0);
        assert_eq!(pages[0].text, before);
    }

    #[test]
    fn fully_consumed_line_hands_its_spans_over() {
        let mut pages = vec![page_of(
            1,
            &[
                ("a comprehen-", 60.0, 600.0, 0),
                ("sive", 60.0, 588.0, 0),
                ("next line here", 60.0, 576.0, 0),
            ],
        )];
        clean_document(&mut pages);
        assert_eq!(pages[0].text, "a comprehensive\nnext line here");
        assert_eq!(texts(&pages[0]), ["a comprehensive", "next line here"]);
        assert_eq!(pages[0].lines[0].spans, [0, 1]);
        assert_eq!(pages[0].spans.len(), 3);
    }

    #[test]
    fn hyphen_joins_across_a_page_break_past_the_page_number() {
        let mut pages = vec![
            page_of(
                1,
                &[
                    ("Body text with a comprehen-", 60.0, 400.0, 0),
                    ("1", 300.0, 30.0, 1),
                ],
            ),
            page_of(
                2,
                &[
                    ("sive review follows.", 60.0, 700.0, 0),
                    ("2", 300.0, 30.0, 1),
                ],
            ),
        ];
        let report = clean_document(&mut pages);
        assert_eq!(report.page_numbers, 2);
        assert_eq!(report.hyphens_joined, 1);
        assert_eq!(pages[0].text, "Body text with a comprehensive");
        assert_eq!(pages[1].text, "review follows.");
    }

    /// Base line `the class G (A) of graphs` with a raised `hom` of
    /// `script_size` between `G` and `(A)`, emitted as its own line above.
    fn script_page(script_size: f32) -> PageText {
        let mut page = PageText::new(1, 612.0, 792.0, 0);
        page.spans = vec![
            span_at("the class G", 50.0, 398.0, 10.0, 0),
            span_at("(A) of graphs", 116.0, 398.0, 10.0, 1),
            span_at("hom", 105.0, 402.0, script_size, 2),
        ];
        let base_box = union(page.spans[0].bbox.unwrap(), page.spans[1].bbox.unwrap());
        page.lines = vec![
            Line {
                text: "hom".to_string(),
                bbox: page.spans[2].bbox,
                column: 0,
                spans: vec![2],
                role: ROLE_BODY.to_string(),
            },
            Line {
                text: "the class G (A) of graphs".to_string(),
                bbox: Some(base_box),
                column: 0,
                spans: vec![0, 1],
                role: ROLE_BODY.to_string(),
            },
        ];
        page.text = joined(&page.lines);
        page
    }

    #[test]
    fn superscript_line_merges_into_its_base_line() {
        let mut pages = vec![script_page(7.0)];
        let report = clean_document(&mut pages);
        assert_eq!(report.scripts_merged, 1);
        assert_eq!(pages[0].text, "the class G hom(A) of graphs");
        assert_eq!(pages[0].lines.len(), 1);
        assert_eq!(pages[0].lines[0].spans, [0, 2, 1]);
        assert_eq!(pages[0].spans.len(), 3);
    }

    #[test]
    fn near_body_size_fragment_is_not_a_script() {
        let mut pages = vec![script_page(9.5)];
        let before = pages[0].clone();
        let report = clean_document(&mut pages);
        assert_eq!(report.scripts_merged, 0);
        assert_eq!(pages[0], before);
    }

    #[test]
    fn arxiv_stamp_leaves_page_one_text() {
        let stamp = "arXiv:2507.14211v1  [cs.NI]  15 Jul 2025";
        let mut first = page_of(
            1,
            &[
                ("Abstract text here.", 60.0, 600.0, 0),
                (stamp, 60.0, 500.0, 0),
                ("More body text.", 60.0, 588.0, 0),
            ],
        );
        // Make the stamp span vertical, as the rotated margin stamp is.
        let vertical = Some(BBox {
            x0: 18.0,
            y0: 200.0,
            x1: 30.0,
            y1: 600.0,
        });
        first.spans[1].bbox = vertical;
        first.lines[1].bbox = vertical;
        let second = page_of(2, &[(stamp, 60.0, 500.0, 0)]);
        let mut pages = vec![first, second];
        let report = clean_document(&mut pages);
        assert_eq!(report.stamps, 1);
        assert_eq!(pages[0].text, "Abstract text here.\nMore body text.");
        assert_eq!(pages[0].lines[2].text, stamp);
        assert!(
            pages[0]
                .warnings
                .contains(&"furniture removed: 1".to_string())
        );
        assert_eq!(pages[1].text, stamp, "only page 1 carries the stamp rule");
    }

    #[test]
    fn vertical_text_line_is_a_stamp_and_short_lines_are_not() {
        let mut page = page_of(
            1,
            &[
                ("Preprint under review", 18.0, 200.0, 0),
                ("l", 60.0, 400.0, 0),
            ],
        );
        for span in &mut page.spans {
            let b = span.bbox.unwrap();
            span.bbox = Some(BBox {
                x0: b.x0,
                y0: b.y0,
                x1: b.x0 + 2.0,
                y1: b.y0 + 300.0,
            });
        }
        assert!(is_stamp(&page, &page.lines[0]));
        assert!(!is_stamp(&page, &page.lines[1]));
        let horizontal = page_of(1, &[("arXiv:2507.14211 is our identifier", 60.0, 400.0, 0)]);
        assert!(!is_stamp(&horizontal, &horizontal.lines[0]));
    }

    #[test]
    fn page_whose_text_does_not_match_its_lines_is_skipped() {
        let mut page = page_of(1, &[("1", 300.0, 30.0, 0)]);
        page.text = "something else".to_string();
        let mut pages = vec![page];
        let report = clean_document(&mut pages);
        assert_eq!(report.pages_skipped, 1);
        assert_eq!(report.page_numbers, 0);
        assert_eq!(pages[0].text, "something else");
    }

    fn roles(page: &PageText) -> Vec<&str> {
        page.lines.iter().map(|l| l.role.as_str()).collect()
    }

    /// Six pages whose running heads sit at 690 pt (inside the 15 % band,
    /// outside the 8 % one): recto pages carry the title with the page number
    /// in it, verso pages the authors; `extra` adds a line at the same height
    /// on pages 2 and 4 only.
    fn recto_verso_pages(extra: bool) -> Vec<PageText> {
        (1..=6)
            .map(|n| {
                let head = if n % 2 == 1 {
                    format!("Individual Rationality in Constrained Hedonic Games {n}")
                } else {
                    "Ann Author and Bob Writer".to_string()
                };
                let body = format!("Body text of page {n}.");
                let mut rows: Vec<(&str, f32, f32, u32)> = vec![
                    (head.as_str(), 60.0, 690.0, 0),
                    (body.as_str(), 60.0, 600.0, 1),
                ];
                if extra && (n == 2 || n == 4) {
                    rows.push(("A short repeated label", 300.0, 690.0, 2));
                }
                page_of(n, &rows)
            })
            .collect()
    }

    #[test]
    fn alternating_running_heads_in_the_wide_band_are_furniture() {
        let mut pages = recto_verso_pages(true);
        let report = clean_document(&mut pages);
        assert_eq!(report.running_lines_wide, 6);
        assert_eq!(report.running_lines, 0);
        for (i, page) in pages.iter().enumerate() {
            let n = i + 1;
            if n == 2 || n == 4 {
                assert_eq!(
                    page.text,
                    format!("Body text of page {n}.\nA short repeated label"),
                    "a wide-band line on two pages stays"
                );
            } else {
                assert_eq!(page.text, format!("Body text of page {n}."));
            }
            assert_eq!(
                page.lines.last().map(|l| l.role.as_str()),
                Some("furniture")
            );
        }
        let once = pages.clone();
        assert_eq!(clean_document(&mut pages), CleanupReport::default());
        assert_eq!(pages, once);
    }

    #[test]
    fn strong_repetition_rule() {
        fn pages(list: &[u32]) -> BTreeSet<u32> {
            list.iter().copied().collect()
        }
        assert!(strongly_repeated(&pages(&[1, 3, 5]), 20));
        assert!(strongly_repeated(&pages(&[2, 4, 6]), 20));
        assert!(!strongly_repeated(&pages(&[1, 2, 3]), 20));
        assert!(strongly_repeated(&pages(&[1, 2, 3]), 7));
        assert!(!strongly_repeated(&pages(&[1, 2]), 2));
    }

    #[test]
    fn dot_leader_lines_are_toc() {
        let mut pages = vec![
            page_of(1, &[("Body text on page one", 60.0, 600.0, 0)]),
            page_of(
                2,
                &[
                    ("Contents", 60.0, 600.0, 0),
                    ("1 Introduction . . . . . . . . . 3", 60.0, 588.0, 0),
                    (
                        "1.1 Research Backgrounds of Multi-Agent Decision-Making . . . . . . . 4",
                        60.0,
                        576.0,
                        0,
                    ),
                    ("2 Methods ........ 12", 60.0, 564.0, 0),
                    ("Values of x . y . z are 5", 60.0, 552.0, 0),
                ],
            ),
        ];
        let before = pages[1].text.clone();
        let report = clean_document(&mut pages);
        assert_eq!(report.role_toc, 3);
        assert_eq!(roles(&pages[1]), ["body", "toc", "toc", "toc", "body"]);
        assert_eq!(pages[1].text, before, "tags never remove text");
    }

    #[test]
    fn page_one_lines_before_the_abstract_are_front_matter() {
        let mut pages = vec![page_of(
            1,
            &[
                ("A Study of Things", 60.0, 700.0, 0),
                ("Ann Author, Bob Writer", 60.0, 680.0, 0),
                ("University of Somewhere", 60.0, 668.0, 0),
                ("Abstract", 60.0, 640.0, 0),
                ("We study things.", 60.0, 628.0, 0),
                ("1 Introduction", 60.0, 600.0, 0),
                ("Things matter.", 60.0, 588.0, 0),
            ],
        )];
        let before = pages[0].text.clone();
        let report = clean_document(&mut pages);
        assert_eq!(report.role_front, 3);
        assert_eq!(report.role_heading, 1);
        assert_eq!(
            roles(&pages[0]),
            ["front", "front", "front", "heading", "body", "body", "body"]
        );
        assert_eq!(pages[0].text, before);

        let mut run_in = vec![page_of(
            1,
            &[
                ("Abstractive Summaries Revisited", 60.0, 700.0, 0),
                ("Ann Author", 60.0, 680.0, 0),
                ("Abstract\u{2014}We revisit summaries.", 60.0, 640.0, 0),
            ],
        )];
        clean_document(&mut run_in);
        assert_eq!(roles(&run_in[0]), ["front", "front", "body"]);
    }

    #[test]
    fn without_an_abstract_front_matter_ends_at_the_introduction() {
        let mut pages = vec![
            page_of(
                1,
                &[
                    ("A Study of Things", 60.0, 700.0, 0),
                    ("Ann Author", 60.0, 680.0, 0),
                    ("I. INTRODUCTION", 60.0, 640.0, 0),
                    ("Things matter.", 60.0, 628.0, 0),
                ],
            ),
            page_of(2, &[("Ann Author", 60.0, 600.0, 0)]),
        ];
        let before = pages[0].text.clone();
        let report = clean_document(&mut pages);
        assert_eq!(report.role_front, 2);
        assert_eq!(report.role_heading, 0);
        assert_eq!(roles(&pages[0]), ["front", "front", "body", "body"]);
        assert_eq!(roles(&pages[1]), ["body"], "only page 1 has front matter");
        assert_eq!(pages[0].text, before);
    }

    #[test]
    fn caption_labels_are_tagged() {
        let mut pages = vec![
            page_of(1, &[("Body text on page one", 60.0, 600.0, 0)]),
            page_of(
                2,
                &[
                    ("Figure 1: Overview of the system.", 60.0, 600.0, 0),
                    ("Fig. 2. Results on the test set.", 60.0, 588.0, 0),
                    ("Table S1 | Data sources.", 60.0, 576.0, 0),
                    ("Algorithm 3: Greedy search", 60.0, 564.0, 0),
                    ("TABLE IV. Error rates", 60.0, 552.0, 0),
                    ("Figure 3 shows the results.", 60.0, 540.0, 0),
                    ("Tables are listed below.", 60.0, 528.0, 0),
                ],
            ),
        ];
        let before = pages[1].text.clone();
        let report = clean_document(&mut pages);
        assert_eq!(report.role_caption, 5);
        assert_eq!(
            roles(&pages[1]),
            [
                "caption", "caption", "caption", "caption", "caption", "body", "body"
            ]
        );
        assert_eq!(pages[1].text, before);
    }

    #[test]
    fn bare_caption_starts_are_tagged_unless_they_continue_a_paragraph() {
        let mut pages = vec![
            page_of(1, &[("Body text on page one", 60.0, 600.0, 0)]),
            page_of(
                2,
                &[
                    (
                        "Fig. 3 Overview of the judge and extractor choice",
                        60.0,
                        700.0,
                        0,
                    ),
                    ("Table 2 Results on the benchmark", 60.0, 660.0, 0),
                    (
                        "Figure 1 The overall framework of the model",
                        60.0,
                        620.0,
                        0,
                    ),
                    ("Figure 3 shows the results.", 60.0, 580.0, 0),
                    ("TABLE IV Error Rates", 60.0, 540.0, 0),
                    (
                        "the accuracy of the two systems is compared in",
                        60.0,
                        500.0,
                        0,
                    ),
                    ("Table 5 The numbers there are averages.", 60.0, 488.0, 0),
                ],
            ),
        ];
        let before = pages[1].text.clone();
        let report = clean_document(&mut pages);
        assert_eq!(report.role_caption, 4);
        assert_eq!(
            roles(&pages[1]),
            [
                "caption", "caption", "caption", "body", "caption", "body", "body"
            ]
        );
        assert_eq!(pages[1].text, before);
        assert!(is_bare_caption("Fig. 3 Overview of the judge"));
        assert!(is_bare_caption("Table S1 Data sources"));
        assert!(!is_bare_caption("Table 2 shows the gains"));
        assert!(!is_bare_caption("Figure 3 GPT results"));
        assert!(!is_bare_caption("Algorithm 1 Greedy Search"));
    }

    #[test]
    fn a_prose_run_ends_front_matter_without_an_abstract_line() {
        let affiliation = "Department of Physics, University of Somewhere, 1 Main Street, \
                           Some City, Some Country, Earth";
        let names = "Carl Coauthor, Dana Doe, Eve Example, Finn Fourth, Gail Fifth, \
                     Hal Sixth, Ida Seventh";
        let mut pages = vec![page_of(
            1,
            &[
                ("A Study of Things", 60.0, 700.0, 0),
                ("Ann Author, Bob Writer", 60.0, 688.0, 0),
                (affiliation, 60.0, 676.0, 0),
                (names, 60.0, 664.0, 0),
                (
                    "we study the dynamics of chemical reaction networks with the goal of",
                    60.0,
                    640.0,
                    0,
                ),
                (
                    "deriving an upper bound on their rates, which is hard because",
                    60.0,
                    628.0,
                    0,
                ),
                ("1 Introduction", 60.0, 600.0, 0),
                ("Things matter.", 60.0, 588.0, 0),
            ],
        )];
        let before = pages[0].text.clone();
        let report = clean_document(&mut pages);
        assert_eq!(report.role_front, 4);
        assert_eq!(
            roles(&pages[0]),
            [
                "front", "front", "front", "front", "body", "body", "body", "body"
            ]
        );
        assert_eq!(pages[0].text, before);
    }

    #[test]
    fn a_long_line_without_an_affiliation_signal_is_not_front_matter() {
        let notice = "This work has been submitted to the IEEE for possible publication \
                      and may change without notice";
        let mut pages = vec![page_of(
            1,
            &[
                ("A Study of Things", 60.0, 700.0, 0),
                (notice, 60.0, 688.0, 0),
                ("Ann Author", 60.0, 676.0, 0),
                ("Abstract", 60.0, 640.0, 0),
                ("We study things.", 60.0, 628.0, 0),
            ],
        )];
        let report = clean_document(&mut pages);
        assert_eq!(report.role_front, 2);
        assert_eq!(
            roles(&pages[0]),
            ["front", "body", "front", "heading", "body"]
        );
        assert!(is_front_prose(
            "we study the dynamics of chemical reaction networks with the goal of"
        ));
        assert!(!is_front_prose(
            "Carl Coauthor, Dana Doe, Eve Example, Finn Fourth, Gail Fifth, Hal Sixth"
        ));
        assert!(!is_front_prose(
            "we thank the department of physics at the University of Somewhere for"
        ));
        assert!(!is_long_body_line(
            "Carl Coauthor, Dana Doe, Eve Example, Finn Fourth, Gail Fifth, Hal Sixth, Ida Seventh",
            false
        ));
    }

    #[test]
    fn a_twelve_word_sentence_line_is_not_front_matter() {
        let notice = "This paper was accepted at the main conference and presented there in person";
        let affiliation = "Department of Computer Science, University of Somewhere, housed in the \
                           old buildings";
        let mut pages = vec![page_of(
            1,
            &[
                ("A Study of Things", 60.0, 700.0, 0),
                (notice, 60.0, 688.0, 0),
                ("Ann Author", 60.0, 676.0, 0),
                (affiliation, 60.0, 664.0, 0),
                ("Abstract", 60.0, 640.0, 0),
                ("We study things.", 60.0, 628.0, 0),
            ],
        )];
        let report = clean_document(&mut pages);
        assert_eq!(report.role_front, 3);
        assert_eq!(
            roles(&pages[0]),
            ["front", "body", "front", "front", "heading", "body"]
        );
        assert!(has_verb_ending(notice));
        assert!(!has_verb_ending("A Study of Things and Their Uses"));
        assert!(!has_verb_ending("we saw it all"));
    }

    #[test]
    fn a_long_title_with_plural_nouns_before_the_abstract_is_front_matter() {
        let title =
            "Sparse graph models for robust control of large power systems and their networks";
        let sentence =
            "We show that sparse graph models improve the control of large power systems.";
        let mut pages = vec![page_of(
            1,
            &[
                (title, 60.0, 700.0, 0),
                ("Ann Author", 60.0, 676.0, 0),
                ("Abstract", 60.0, 640.0, 0),
                (sentence, 60.0, 628.0, 0),
            ],
        )];
        let report = clean_document(&mut pages);
        assert_eq!(report.role_front, 2);
        assert_eq!(roles(&pages[0]), ["front", "front", "heading", "body"]);
        assert_eq!(title.split_whitespace().count(), 13);
        assert_eq!(sentence.split_whitespace().count(), 13);
        assert!(!has_verb_ending(title));
        assert!(!has_verb_cue(title));
        assert!(!is_long_body_line(title, true));
        assert!(!is_long_body_line(title, false));
        assert!(is_long_body_line(sentence, false));
        assert!(has_verb_cue(sentence));
        // Before the abstract an `-ed`/`-ing` word alone is no evidence.
        let learned = "Learning sparse graph models for robust control of large distributed \
                       power systems";
        assert!(has_verb_ending(learned));
        assert!(!is_long_body_line(learned, true));
        assert!(is_long_body_line(learned, false));
        assert!(has_sentence_break(
            "the results hold here. We then prove it"
        ));
        assert!(!has_sentence_break(
            "J. Smith and A. Jones et al. Sparse models"
        ));
        assert!(!has_sentence_break("models for Fig. 3 of the paper"));
    }

    #[test]
    fn a_run_of_three_shorter_prose_lines_ends_front_matter() {
        let mut pages = vec![page_of(
            1,
            &[
                ("A Study of Things", 60.0, 700.0, 0),
                ("Ann Author", 60.0, 688.0, 0),
                ("University of Toronto & Vector Institute", 60.0, 676.0, 0),
                ("kernels and the attention layers with", 60.0, 652.0, 0),
                ("kernels which, when executed on the", 60.0, 640.0, 0),
                ("host processors, can be very slow", 60.0, 628.0, 0),
                ("1 Introduction", 60.0, 600.0, 0),
                ("Things matter.", 60.0, 588.0, 0),
            ],
        )];
        let report = clean_document(&mut pages);
        assert_eq!(report.role_front, 3);
        assert_eq!(
            roles(&pages[0]),
            [
                "front", "front", "front", "body", "body", "body", "body", "body"
            ]
        );
        assert!(is_short_front_prose(
            "kernels and the attention layers with"
        ));
        assert!(!is_short_front_prose(
            "University of Toronto & Vector Institute"
        ));
    }

    #[test]
    fn existing_tags_survive_and_furniture_wins() {
        let mut line = Line {
            role: "figure".to_string(),
            ..Line::default()
        };
        assert!(!tag(&mut line, ROLE_CAPTION));
        assert_eq!(line.role, "figure");
        assert!(tag(&mut line, ROLE_FURNITURE));
        assert_eq!(line.role, "furniture");
    }

    /// `page.text` rebuilt from its lines with a paragraph break before
    /// each line index in `breaks`.
    fn with_breaks(mut page: PageText, breaks: &[usize]) -> PageText {
        let mut text = String::new();
        for (k, line) in page.lines.iter().enumerate() {
            if k > 0 {
                text.push_str(if breaks.contains(&k) { "\n\n" } else { "\n" });
            }
            text.push_str(&line.text);
        }
        page.text = text;
        page
    }

    /// A US Letter page with one line per row `(text, x0, y0, size)`, all in
    /// column 0.
    fn sized_page(number: u32, rows: &[(&str, f32, f32, f32)]) -> PageText {
        let mut page = PageText::new(number, 612.0, 792.0, 0);
        for (i, (text, x0, y0, size)) in rows.iter().enumerate() {
            let seq = u32::try_from(i).unwrap();
            let span = span_at(text, *x0, *y0, *size, seq);
            page.lines.push(Line {
                text: (*text).to_string(),
                bbox: span.bbox,
                column: 0,
                spans: vec![seq],
                role: ROLE_BODY.to_string(),
            });
            page.spans.push(span);
        }
        page.text = joined(&page.lines);
        page
    }

    fn rule_figure(x0: f32, y0: f32, x1: f32) -> Figure {
        Figure {
            index: 0,
            bbox: Some(BBox {
                x0,
                y0,
                x1,
                y1: y0 + 0.4,
            }),
            kind: "rule".to_string(),
            mime: None,
            width_px: None,
            height_px: None,
            sha256: None,
            file: None,
            caption: None,
        }
    }

    #[test]
    fn biography_starts() {
        assert!(is_biography_start(
            "SAMUEL COOGAN (Senior Member, IEEE) received",
            "the B.S. degree"
        ));
        assert!(is_biography_start("JOEL W. BURDICK (Member, IEEE) the", ""));
        assert!(is_biography_start(
            "MANORANJAN MAJJI (Senior Member, IEEE),",
            "received the B.E. (Hons) in mechanical engineering"
        ));
        assert!(is_biography_start(
            "\u{00C1}lmos Veres-Vit\u{00E1}lyos received an MSc degree in",
            ""
        ));
        assert!(is_biography_start(
            "Gen\u{0131}\u{0301}s Castillo G\u{00F3}mez-Raya received a double BSc",
            ""
        ));
        assert!(is_biography_start(
            "Filip Lemic is a senior researcher at the i2Cat Foundation",
            ""
        ));
        assert!(is_biography_start(
            "Xavier Costa P\u{00E9}rez is an ICREA Research Professor",
            ""
        ));
        assert!(is_biography_start(
            "Jane Doe was born in Oslo, Norway, in 1980.",
            ""
        ));
        assert!(!is_biography_start(
            "The model is a simple baseline for tests.",
            ""
        ));
        assert!(!is_biography_start(
            "Kalman Filter is a recursive estimator.",
            ""
        ));
        assert!(!is_biography_start("Samuel received the award.", ""));
        assert!(!is_biography_start(
            "[1] A. Author, B. Writer, and C. Reader.",
            ""
        ));
    }

    #[test]
    fn author_biographies_after_the_references_are_tagged() {
        let page_one = page_of(
            1,
            &[
                ("Body text of the first page is here.", 60.0, 600.0, 0),
                (
                    "Ann Other received the Ph.D. degree in 2010.",
                    60.0,
                    588.0,
                    0,
                ),
            ],
        );
        let page_two = page_of(2, &[("More body text on the second page.", 60.0, 600.0, 0)]);
        let page_three = with_breaks(
            page_of(
                3,
                &[
                    ("REFERENCES", 60.0, 700.0, 0),
                    (
                        "[1] A. Author, \u{201C}A title,\u{201D} J. Tests, 2020.",
                        60.0,
                        688.0,
                        0,
                    ),
                    (
                        "SAMUEL COOGAN (Senior Member, IEEE) received",
                        60.0,
                        650.0,
                        0,
                    ),
                    ("the B.S. degree in electrical engineering", 60.0, 638.0, 0),
                    ("from the Georgia Institute of Technology.", 60.0, 626.0, 0),
                    ("His research interests include control.", 60.0, 600.0, 0),
                    ("Filip Lemic is a senior researcher at the", 60.0, 570.0, 0),
                    ("i2Cat Foundation.", 60.0, 558.0, 0),
                    ("The model is a simple baseline for tests.", 60.0, 530.0, 0),
                    (
                        "Bea Writer received the M.Sc. degree in 2012.",
                        60.0,
                        518.0,
                        0,
                    ),
                ],
            ),
            &[2, 5, 6, 8],
        );
        let mut pages = vec![page_one, page_two, page_three];
        let before: Vec<String> = pages.iter().map(|p| p.text.clone()).collect();
        let report = clean_document(&mut pages);
        assert_eq!(report.role_biography, 7);
        assert_eq!(
            roles(&pages[0]),
            ["body", "body"],
            "not after the references"
        );
        assert_eq!(
            roles(&pages[2]),
            [
                "body",
                "body",
                "biography",
                "biography",
                "biography",
                "biography",
                "biography",
                "biography",
                "body",
                "biography"
            ]
        );
        let after: Vec<String> = pages.iter().map(|p| p.text.clone()).collect();
        assert_eq!(after, before, "tags never remove text");
        let again = clean_document(&mut pages);
        assert_eq!(again.role_biography, 0);
        assert_eq!(roles(&pages[2])[9], "biography");
    }

    #[test]
    fn a_biography_heading_is_tagged_and_long_biographies_are_capped() {
        let mut rows: Vec<(String, f32, f32, u32)> = vec![
            ("BIOGRAPHIES".to_string(), 60.0, 700.0, 0),
            (
                "Jane Doe received the Ph.D. degree from the".to_string(),
                60.0,
                688.0,
                0,
            ),
        ];
        let extra = u16::try_from(BIOGRAPHY_MAX_LINES + 5).expect("small cap");
        for k in 0..extra {
            let y = 676.0 - 12.0 * f32::from(k);
            rows.push((
                format!("continued biography line number {k} of the text"),
                60.0,
                y,
                0,
            ));
        }
        let borrowed: Vec<(&str, f32, f32, u32)> = rows
            .iter()
            .map(|(text, x0, y0, column)| (text.as_str(), *x0, *y0, *column))
            .collect();
        let mut pages = vec![with_breaks(page_of(1, &borrowed), &[1])];
        let report = clean_document(&mut pages);
        assert_eq!(report.role_biography, 1 + BIOGRAPHY_MAX_LINES);
        let tagged = roles(&pages[0]);
        assert_eq!(tagged[0], "biography");
        assert!(
            tagged[1..=BIOGRAPHY_MAX_LINES]
                .iter()
                .all(|r| *r == "biography")
        );
        assert!(
            tagged[BIOGRAPHY_MAX_LINES + 1..]
                .iter()
                .all(|r| *r == "body")
        );
    }

    #[test]
    fn licence_keyword_and_affiliation_blocks_are_front_matter() {
        let page = with_breaks(
            page_of(
                1,
                &[
                    ("A Study of Things", 60.0, 700.0, 0),
                    ("Ann Author", 60.0, 680.0, 0),
                    ("Abstract", 60.0, 660.0, 0),
                    ("We study things in depth here.", 60.0, 648.0, 0),
                    ("Keywords", 60.0, 620.0, 0),
                    ("audio description, item response theory,", 60.0, 608.0, 0),
                    ("quality evaluation", 60.0, 596.0, 0),
                    ("ACM Reference Format:", 60.0, 584.0, 0),
                    (
                        "Ann Author. 2026. A Study of Things. In Proc.",
                        60.0,
                        572.0,
                        0,
                    ),
                    (
                        "Permission to make digital or hard copies of all",
                        60.0,
                        540.0,
                        0,
                    ),
                    ("or part of this work is granted.", 60.0, 528.0, 0),
                    (
                        "\u{00A9} 2026 Copyright held by the owner/author(s).",
                        60.0,
                        516.0,
                        0,
                    ),
                    (
                        "a Department of Physics, University of Somewhere, Paris, France",
                        60.0,
                        490.0,
                        0,
                    ),
                    ("1 Introduction", 60.0, 460.0, 0),
                    ("Things matter a great deal to everyone.", 60.0, 448.0, 0),
                ],
            ),
            &[4, 9, 12, 13],
        );
        let mut pages = vec![page];
        let before = pages[0].text.clone();
        let report = clean_document(&mut pages);
        assert_eq!(report.role_front, 11);
        assert_eq!(
            roles(&pages[0]),
            [
                "front", "front", "heading", "body", "front", "front", "front", "front", "front",
                "front", "front", "front", "front", "body", "body"
            ]
        );
        assert_eq!(pages[0].text, before);
        let again = clean_document(&mut pages);
        assert_eq!(again.role_front, 0);
    }

    #[test]
    fn keyword_blocks_stop_at_a_full_stop_and_need_a_label() {
        let page = with_breaks(
            page_of(
                1,
                &[
                    ("A Study of Things", 60.0, 700.0, 0),
                    ("Abstract", 60.0, 680.0, 0),
                    ("We study things in depth here.", 60.0, 668.0, 0),
                    ("Index Terms\u{2014}control, safety.", 60.0, 640.0, 0),
                    ("A body line that follows directly here.", 60.0, 628.0, 0),
                    (
                        "Keywords are extracted from the text by the model.",
                        60.0,
                        600.0,
                        0,
                    ),
                    (
                        "a new method, which is simple, fast, and robust.",
                        60.0,
                        588.0,
                        0,
                    ),
                ],
            ),
            &[3, 5],
        );
        let mut pages = vec![page];
        clean_document(&mut pages);
        assert_eq!(
            roles(&pages[0]),
            ["front", "heading", "body", "front", "body", "body", "body"]
        );
        assert!(is_lettered_affiliation(
            "\u{1D43}School of Information Engineering, Xi'an Jiaotong University"
        ));
        assert!(is_lettered_affiliation(
            "Tech, Atlanta, Georgia, USA,\u{2071} University of California,\u{02B2} Chalmers"
        ));
        assert!(!is_lettered_affiliation(
            "a Department of Energy grant funded this."
        ));
    }

    #[test]
    fn keywords_on_page_two_after_a_title_page_are_front_matter() {
        let title = page_of(
            1,
            &[
                ("Highlights", 60.0, 700.0, 0),
                ("\u{2022} A short highlight about things.", 60.0, 680.0, 0),
            ],
        );
        let second = with_breaks(
            page_of(
                2,
                &[
                    ("A Study of Things", 60.0, 700.0, 0),
                    ("Ann Author\u{1D43}", 60.0, 680.0, 0),
                    (
                        "\u{1D43}School of Physics, University of Somewhere, Paris",
                        60.0,
                        668.0,
                        0,
                    ),
                    ("Abstract", 60.0, 640.0, 0),
                    ("We study things.", 60.0, 628.0, 0),
                    ("Keywords:", 60.0, 600.0, 0),
                    ("things, stuff.", 60.0, 588.0, 0),
                    ("1 Introduction", 60.0, 560.0, 0),
                    ("Body text follows.", 60.0, 548.0, 0),
                ],
            ),
            &[5, 7],
        );
        let third = page_of(3, &[("Keywords:", 60.0, 600.0, 0)]);
        let mut pages = vec![title, second, third];
        clean_document(&mut pages);
        assert_eq!(roles(&pages[0]), ["body", "body"]);
        assert_eq!(
            roles(&pages[1]),
            [
                "front", "front", "front", "heading", "body", "front", "front", "body", "body"
            ]
        );
        assert_eq!(roles(&pages[2]), ["body"], "only the front-matter page");
    }

    /// Page `number` with three body lines at 10 pt and then `notes` at
    /// 8 pt near the foot of the page.
    fn page_with_notes(number: u32, notes: &[&str], note_size: f32) -> PageText {
        let first = format!("The first body line of page {number} is here.");
        let second = format!("The second body line of page {number} is here.");
        let third = format!("The third body line of page {number} ends it.");
        let mut rows: Vec<(&str, f32, f32, f32)> = vec![
            (first.as_str(), 60.0, 500.0, 10.0),
            (second.as_str(), 60.0, 488.0, 10.0),
            (third.as_str(), 60.0, 476.0, 10.0),
        ];
        for (k, note) in notes.iter().enumerate() {
            let y = 186.0 - 10.0 * f32::from(u16::try_from(k).unwrap());
            rows.push((*note, 60.0, y, note_size));
        }
        sized_page(number, &rows)
    }

    #[test]
    fn lines_under_a_footnote_rule_are_footnotes() {
        let notes = [
            "Work done while at the lab and elsewhere.",
            "See the project page.",
        ];
        let mut ruled = page_with_notes(2, &notes, 8.0);
        ruled.figures.push(rule_figure(60.0, 200.0, 160.0));
        let mut pages = vec![ruled];
        let report = clean_document(&mut pages);
        assert_eq!(report.role_footnote, 2);
        assert_eq!(
            roles(&pages[0]),
            ["body", "body", "body", "footnote", "footnote"]
        );
        let again = clean_document(&mut pages);
        assert_eq!(again.role_footnote, 0);

        let mut body_size = page_with_notes(2, &notes, 10.0);
        body_size.figures.push(rule_figure(60.0, 200.0, 160.0));
        let mut wide = page_with_notes(3, &notes, 8.0);
        wide.figures.push(rule_figure(60.0, 200.0, 560.0));
        let mut pages = vec![body_size, wide];
        let report = clean_document(&mut pages);
        assert_eq!(report.role_footnote, 0, "a table rule or a wide rule");
        assert_eq!(roles(&pages[0])[3], "body");
        assert_eq!(roles(&pages[1])[3], "body");
    }

    #[test]
    fn a_footnote_carried_to_the_next_page_is_tagged() {
        let open = page_with_notes(
            2,
            &[
                "1 A footnote that runs on to the next",
                "page of the paper and",
            ],
            8.0,
        );
        let carried = page_with_notes(
            3,
            &[
                "continues here without a marker.",
                "2 A second note on this page.",
            ],
            8.0,
        );
        let mut pages = vec![open, carried];
        let report = clean_document(&mut pages);
        assert_eq!(report.role_footnote, 2);
        assert_eq!(roles(&pages[0])[3..], ["body", "body"], "left to regions");
        assert_eq!(roles(&pages[1])[3..], ["footnote", "footnote"]);

        let closed = page_with_notes(
            2,
            &[
                "1 A footnote that runs on to the next",
                "page of the paper.",
            ],
            8.0,
        );
        let next = page_with_notes(
            3,
            &[
                "continues here without a marker.",
                "2 A second note on this page.",
            ],
            8.0,
        );
        let mut pages = vec![closed, next];
        let report = clean_document(&mut pages);
        assert_eq!(
            report.role_footnote, 0,
            "the previous page ended a sentence"
        );
        assert_eq!(roles(&pages[1])[3..], ["body", "body"]);
    }

    /// Page 1 with `spans` and one body line per entry of `lines` (span
    /// indices); a line's text is its span texts joined by spaces.
    fn page_with(spans: Vec<Span>, lines: &[&[u32]]) -> PageText {
        let mut page = PageText::new(1, 612.0, 792.0, 0);
        page.spans = spans;
        for members in lines {
            let texts: Vec<&str> = members
                .iter()
                .map(|i| page.spans[*i as usize].text.as_str())
                .collect();
            let text = texts.join(" ");
            let mut bbox: Option<BBox> = None;
            for i in *members {
                let b = page.spans[*i as usize].bbox.unwrap();
                bbox = Some(bbox.map_or(b, |a| union(a, b)));
            }
            page.lines.push(Line {
                text,
                bbox,
                column: 0,
                spans: members.to_vec(),
                role: ROLE_BODY.to_string(),
            });
        }
        page.text = joined(&page.lines);
        page
    }

    #[test]
    fn script_forms() {
        assert_eq!(script_form("5", true).as_deref(), Some("\u{2075}"));
        assert_eq!(
            script_form("5\u{2013}7", true).as_deref(),
            Some("\u{2075}\u{207B}\u{2077}")
        );
        assert_eq!(
            script_form("10, 11", true).as_deref(),
            Some("\u{00B9}\u{2070},\u{00B9}\u{00B9}")
        );
        assert_eq!(script_form("a", true).as_deref(), Some("\u{1D43}"));
        assert_eq!(script_form("n", true).as_deref(), Some("\u{207F}"));
        assert_eq!(script_form("q", true), None);
        assert_eq!(script_form("N", true), None);
        assert_eq!(script_form("*", true).as_deref(), Some("*"));
        assert_eq!(script_form("3", false).as_deref(), Some("\u{2083}"));
        assert_eq!(script_form("a", false), None);
        assert_eq!(script_form("ing", true), None);
        assert_eq!(script_form("ab", true), None);
        assert_eq!(script_form("-", true), None);
        assert_eq!(script_form("1234567890123", true), None);
        assert_eq!(script_form("1-2", false), None);
    }

    #[test]
    fn raised_number_attaches_to_the_word_before_it() {
        let spans = vec![
            span_at("the literature.", 50.0, 398.0, 10.0, 0),
            span_at("5", 125.5, 401.0, 6.0, 1),
        ];
        let mut pages = vec![page_with(spans, &[&[1], &[0]])];
        let report = clean_document(&mut pages);
        assert_eq!(report.superscripts_merged, 1);
        assert_eq!(report.scripts_merged, 0);
        assert_eq!(pages[0].text, "the literature.\u{2075}");
        assert_eq!(pages[0].lines.len(), 1);
        assert_eq!(pages[0].lines[0].spans, [0, 1]);
        assert_eq!(pages[0].spans.len(), 2);
        let once = pages.clone();
        let again = clean_document(&mut pages);
        assert_eq!(again.superscripts_merged, 0);
        assert_eq!(pages, once);
    }

    #[test]
    fn raised_range_becomes_superscript_digits_and_minus() {
        let spans = vec![
            span_at("the Initiative", 50.0, 398.0, 10.0, 0),
            span_at("5\u{2013}7", 120.5, 401.0, 6.0, 1),
        ];
        let mut pages = vec![page_with(spans, &[&[1], &[0]])];
        let report = clean_document(&mut pages);
        assert_eq!(report.superscripts_merged, 1);
        assert_eq!(pages[0].text, "the Initiative\u{2075}\u{207B}\u{2077}");
    }

    #[test]
    fn raised_number_before_a_comma_span_drops_the_space() {
        let spans = vec![
            span_at("the Initiative", 50.0, 398.0, 10.0, 0),
            span_at(",", 124.0, 398.0, 10.0, 1),
            span_at("6", 120.5, 401.0, 6.0, 2),
        ];
        let mut pages = vec![page_with(spans, &[&[2], &[0, 1]])];
        let report = clean_document(&mut pages);
        assert_eq!(report.superscripts_merged, 1);
        assert_eq!(pages[0].text, "the Initiative\u{2076},");
        assert_eq!(pages[0].lines[0].spans, [0, 2, 1]);
    }

    #[test]
    fn raised_number_at_line_start_attaches_to_the_next_word() {
        let spans = vec![
            span_at("Prior work by Smith et al.", 50.0, 412.0, 10.0, 0),
            span_at("38", 50.0, 401.0, 6.0, 1),
            span_at("Prein and co", 50.0, 398.0, 10.0, 2),
        ];
        let mut pages = vec![page_with(spans, &[&[0], &[1], &[2]])];
        let report = clean_document(&mut pages);
        assert_eq!(report.superscripts_merged, 1);
        assert_eq!(
            pages[0].text,
            "Prior work by Smith et al.\n\u{00B3}\u{2078}Prein and co"
        );
        assert_eq!(pages[0].lines[1].spans, [1, 2]);
    }

    #[test]
    fn fragments_on_one_row_merge_at_their_positions() {
        let spans = vec![
            span_at("papers from arXiv", 50.0, 398.0, 10.0, 0),
            span_at(", ChemRxiv", 140.0, 398.0, 10.0, 1),
            span_at(", and 1999 data", 195.0, 398.0, 10.0, 2),
            span_at("9", 135.5, 401.0, 6.0, 3),
            span_at("10, 11", 190.5, 401.0, 6.0, 4),
        ];
        let mut pages = vec![page_with(spans, &[&[3], &[4], &[0, 1, 2]])];
        let report = clean_document(&mut pages);
        assert_eq!(report.superscripts_merged, 2);
        assert_eq!(
            pages[0].text,
            "papers from arXiv\u{2079}, ChemRxiv\u{00B9}\u{2070},\u{00B9}\u{00B9}, and 1999 data"
        );
        assert_eq!(pages[0].lines[0].spans, [0, 3, 1, 4, 2]);
    }

    #[test]
    fn script_merging_has_a_per_page_work_budget() {
        let fragments = 2_000u32;
        let mut spans = Vec::with_capacity(fragments as usize + 1);
        spans.push(span_at("base", 50.0, 398.0, 10.0, 0));
        // Give the base the deliberately tall box used by the hostile case:
        // every following small line overlaps it and selects it as a target.
        spans[0].bbox = Some(BBox {
            x0: 50.0,
            y0: 390.0,
            x1: 100.0,
            y1: 420.0,
        });
        for seq in 1..=fragments {
            // A convertible digit forces superscript target discovery, so the
            // candidate-window scans themselves must consume the work budget.
            spans.push(span_at("1", 60.0, 400.0, 1.0, seq));
        }
        let lines: Vec<Vec<u32>> = (0..=fragments).map(|seq| vec![seq]).collect();
        let members: Vec<&[u32]> = lines.iter().map(Vec::as_slice).collect();
        let mut pages = vec![page_with(spans, &members)];

        let report = clean_document(&mut pages);

        let merged = report.scripts_merged + report.superscripts_merged;
        assert!(merged > 0);
        assert!(merged < fragments as usize);
        assert!(
            pages[0].lines.len() > 1,
            "over-budget fragments stay intact"
        );
    }

    #[test]
    fn raised_letters_and_body_size_numbers_stay() {
        // `ing` sits in the raised window but overlaps the base line too
        // little for the general script rule.
        for (text, size, y0) in [("ing", 6.0, 406.0), ("38", 10.0, 401.0)] {
            let spans = vec![
                span_at("the literature.", 50.0, 398.0, 10.0, 0),
                span_at(text, 125.5, y0, size, 1),
            ];
            let mut pages = vec![page_with(spans, &[&[1], &[0]])];
            let before = pages[0].clone();
            let report = clean_document(&mut pages);
            assert_eq!(report.superscripts_merged, 0, "{text}");
            assert_eq!(report.scripts_merged, 0, "{text}");
            assert_eq!(pages[0], before, "{text}");
        }
    }

    /// RSC-style geometry: a 7 pt citation number raised 3.5 pt over 10 pt
    /// text, 6 pt to the right of the line end (beyond the 5 pt reach that
    /// applies inside the line).
    #[test]
    fn raised_number_after_the_line_end_attaches_within_one_size() {
        // Base baseline 400; fragment baseline 403.5, box bottom 402.1.
        let spans = vec![
            span_at("the literature.", 50.0, 398.0, 10.0, 0),
            span_at("38", 131.0, 402.1, 7.0, 1),
        ];
        let page = page_with(spans, &[&[1], &[0]]);
        assert_matches_naive(&page);
        let mut pages = vec![page];
        let report = clean_document(&mut pages);
        assert_eq!(report.superscripts_merged, 1);
        assert_eq!(report.scripts_merged, 0);
        assert_eq!(pages[0].text, "the literature.\u{00B3}\u{2078}");
        assert_eq!(pages[0].lines.len(), 1);
        assert_eq!(pages[0].lines[0].spans, [0, 1]);
    }

    /// The printed geometry of `arXiv:2510.26824`: 7 pt numbers on 9 pt text
    /// (ratio 0.78), 1 pt after the word and before its comma.
    #[test]
    fn seven_point_number_on_nine_point_text_attaches() {
        // Base baseline 399.8; fragment box bottom 1.9 pt above it.
        let spans = vec![
            span_at("energy conversion", 50.0, 398.0, 9.0, 0),
            span_at(",", 131.5, 398.0, 9.0, 1),
            span_at("1", 127.5, 401.7, 7.0, 2),
        ];
        let page = page_with(spans, &[&[2], &[0, 1]]);
        assert_matches_naive(&page);
        let mut pages = vec![page];
        let report = clean_document(&mut pages);
        assert_eq!(report.superscripts_merged, 1);
        assert_eq!(pages[0].text, "energy conversion\u{00B9},");
        assert_eq!(pages[0].lines[0].spans, [0, 2, 1]);
    }

    /// The wider reach holds only beyond the right edge, and a fragment
    /// above `SUPERSCRIPT_RATIO` of the base size is no superscript.
    #[test]
    fn wider_reach_and_ratio_have_limits() {
        // 6 pt before the line start: outside the 5 pt reach.
        let before = vec![
            span_at("38", 47.0, 402.1, 7.0, 0),
            span_at("Prein and co", 60.0, 398.0, 10.0, 1),
        ];
        // 11 pt after the line end: outside the 10 pt reach.
        let far = vec![
            span_at("the literature.", 50.0, 398.0, 10.0, 0),
            span_at("38", 136.0, 402.1, 7.0, 1),
        ];
        // 7.5 pt on 9 pt (0.83), touching the word.
        let large = vec![
            span_at("energy conversion", 50.0, 398.0, 9.0, 0),
            span_at("1", 127.5, 401.7, 7.5, 1),
        ];
        for (spans, script) in [(before, 0), (far, 1), (large, 1)] {
            let lines: [&[u32]; 2] = [&[0], &[1]];
            let page = page_with(spans, &lines);
            assert_matches_naive(&page);
            let w = prepare(&page);
            let geom = PageGeom::new(&page);
            let mut window: Vec<usize> = Vec::new();
            let mut work_left = usize::MAX;
            assert_eq!(
                superscript_target(&page, &w, &geom, script, &mut window, &mut work_left,),
                None,
                "{script}"
            );
        }
    }

    #[test]
    fn lowered_digit_becomes_a_subscript_of_its_own_line() {
        // The `3` also lies in the raised window of the line below; the
        // offset closer to a typical subscript wins.
        let spans = vec![
            span_at("NH", 50.0, 398.0, 10.0, 0),
            span_at("3", 60.5, 396.0, 6.0, 1),
            span_at("and water", 50.0, 386.0, 10.0, 2),
        ];
        let mut pages = vec![page_with(spans, &[&[0], &[1], &[2]])];
        let report = clean_document(&mut pages);
        assert_eq!(report.superscripts_merged, 1);
        assert_eq!(pages[0].text, "NH\u{2083}\nand water");
        assert_eq!(pages[0].lines[0].spans, [0, 1]);
    }

    /// The scan over every line of the page that `superscript_target`
    /// replaced, kept as the oracle for its window search.
    fn naive_target(page: &PageText, w: &PageWork, index: usize) -> Option<(usize, String)> {
        let line = page.lines.get(index)?;
        let raised_form = script_form(&line.text, true);
        let lowered_form = script_form(&line.text, false);
        if raised_form.is_none() && lowered_form.is_none() {
            return None;
        }
        let bbox = norm(line.bbox?);
        let size = line_size(page, line)?;
        let mut best: Option<(usize, f32, bool)> = None;
        for (j, other) in page.lines.iter().enumerate() {
            if j == index || !w.is_body(j) {
                continue;
            }
            let (Some(ob), Some((baseline, other_size))) =
                (other.bbox.map(norm), baseline_of(page, other))
            else {
                continue;
            };
            if size > (SUPERSCRIPT_RATIO + RATIO_SLACK) * other_size {
                continue;
            }
            let gap = (ob.x0 - bbox.x1).max(bbox.x0 - ob.x1).max(0.0);
            let horizontal_reach = if bbox.x0 >= ob.x1 {
                SUPERSCRIPT_REACH_AFTER
            } else {
                SUPERSCRIPT_REACH
            };
            if gap > horizontal_reach * other_size {
                continue;
            }
            let offset = (bbox.y0 - baseline) / other_size;
            let (raised, miss) =
                if raised_form.is_some() && (RAISED_LOW..=RAISED_HIGH).contains(&offset) {
                    (true, (offset - RAISED_IDEAL).abs())
                } else if lowered_form.is_some() && (LOWERED_LOW..RAISED_LOW).contains(&offset) {
                    (false, (offset - LOWERED_IDEAL).abs())
                } else {
                    continue;
                };
            let score = miss + gap / other_size;
            if best.is_none_or(|(_, s, _)| score < s) {
                best = Some((j, score, raised));
            }
        }
        let (target, _, raised) = best?;
        let form = if raised { raised_form } else { lowered_form };
        form.map(|f| (target, f))
    }

    /// `superscript_target` agrees with [`naive_target`] on every line.
    fn assert_matches_naive(page: &PageText) {
        let w = prepare(page);
        assert!(w.eligible);
        let geom = PageGeom::new(page);
        let mut window: Vec<usize> = Vec::new();
        let mut work_left = usize::MAX;
        for k in 0..page.lines.len() {
            assert_eq!(
                superscript_target(page, &w, &geom, k, &mut window, &mut work_left),
                naive_target(page, &w, k),
                "line {k}"
            );
        }
    }

    #[test]
    fn window_search_matches_a_scan_of_every_line() {
        let spans = vec![
            // 0: best base is the 10 pt line; the 20 pt line also reaches it.
            span_at("5", 85.0, 401.0, 6.0, 0),
            // 1: reached only by a 20 pt line whose baseline lies outside
            // the window a 10 pt line would give.
            span_at("7", 300.0, 401.0, 6.0, 1),
            // 2: 10 pt lines just inside and just outside both edges.
            span_at("3", 450.0, 401.0, 6.0, 2),
            span_at("the literature.", 50.0, 398.0, 10.0, 3),
            span_at("Big", 50.0, 387.0, 20.0, 4),
            span_at("Big", 270.0, 387.0, 20.0, 5),
            // Offsets from fragment 2: 0.89, 0.91, -0.69 and -0.71.
            span_at("inside high", 400.0, 390.1, 10.0, 6),
            span_at("outside high", 400.0, 389.9, 10.0, 7),
            span_at("inside low", 400.0, 405.9, 10.0, 8),
            span_at("outside low", 400.0, 406.1, 10.0, 9),
            // A second column with its fragment first in reading order.
            span_at("12", 380.5, 501.0, 6.0, 10),
            span_at("in two studies", 320.0, 498.0, 10.0, 11),
        ];
        let page = page_with(
            spans,
            &[
                &[0],
                &[1],
                &[2],
                &[3],
                &[4],
                &[5],
                &[6],
                &[7],
                &[8],
                &[9],
                &[10],
                &[11],
            ],
        );
        assert_matches_naive(&page);
        let w = prepare(&page);
        let geom = PageGeom::new(&page);
        let mut window: Vec<usize> = Vec::new();
        let mut work_left = usize::MAX;
        let mut target =
            |k: usize| superscript_target(&page, &w, &geom, k, &mut window, &mut work_left);
        assert_eq!(target(0), Some((3, "\u{2075}".to_string())));
        assert_eq!(target(1), Some((5, "\u{2077}".to_string())));
        assert_eq!(target(2), Some((8, "\u{2083}".to_string())));
        assert_eq!(target(10), Some((11, "\u{00B9}\u{00B2}".to_string())));
    }

    #[test]
    fn superscript_search_bounds_a_crowded_baseline_window() {
        let spans: Vec<Span> = (0..(SUPERSCRIPT_SCAN_LIMIT + 100))
            .map(|i| span_at("a", i as f32, 401.0, 10.0, i as u32))
            .collect();
        let members: Vec<Vec<u32>> = (0..spans.len() as u32).map(|i| vec![i]).collect();
        let lines: Vec<&[u32]> = members.iter().map(Vec::as_slice).collect();
        let page = page_with(spans, &lines);
        let w = prepare(&page);
        let geom = PageGeom::new(&page);
        let mut window = Vec::new();
        let mut work_left = usize::MAX;

        assert_eq!(
            superscript_target(&page, &w, &geom, 0, &mut window, &mut work_left),
            None
        );
        assert_eq!(window.len(), SUPERSCRIPT_SCAN_LIMIT - 1);
        assert_eq!(work_left, usize::MAX - SUPERSCRIPT_SCAN_LIMIT);
    }

    #[test]
    fn superscript_search_counts_furniture_toward_scan_limit() {
        let spans: Vec<Span> = (0..(SUPERSCRIPT_SCAN_LIMIT + 100))
            .map(|i| span_at("a", i as f32, 401.0, 10.0, i as u32))
            .collect();
        let members: Vec<Vec<u32>> = (0..spans.len() as u32).map(|i| vec![i]).collect();
        let lines: Vec<&[u32]> = members.iter().map(Vec::as_slice).collect();
        let page = page_with(spans, &lines);
        let mut w = prepare(&page);
        for state in &mut w.state[..SUPERSCRIPT_SCAN_LIMIT] {
            *state = State::Furniture;
        }
        let geom = PageGeom::new(&page);
        let mut window = Vec::new();
        let mut work_left = usize::MAX;

        assert!(geom.window(
            390.0,
            410.0,
            page.lines.len() - 1,
            &w,
            &mut window,
            &mut work_left
        ));

        assert!(window.is_empty());
    }

    #[test]
    fn fragments_far_from_their_base_lines_in_reading_order_merge() {
        // Both fragments come first in reading order and their base lines
        // last, with a column of filler lines in between.
        let mut spans = vec![
            span_at("7", 125.5, 401.0, 6.0, 0),
            span_at("12", 385.5, 501.0, 6.0, 1),
        ];
        for k in 0..14u16 {
            let y = 680.0 - 20.0 * f32::from(k);
            let seq = u32::from(k) + 2;
            spans.push(span_at("filler text of the body", 50.0, y, 10.0, seq));
        }
        spans.push(span_at("the literature.", 50.0, 398.0, 10.0, 16));
        spans.push(span_at("in two studies", 320.0, 498.0, 10.0, 17));
        let members: Vec<Vec<u32>> = (0..18u32).map(|i| vec![i]).collect();
        let lines: Vec<&[u32]> = members.iter().map(Vec::as_slice).collect();
        let page = page_with(spans, &lines);
        assert_matches_naive(&page);
        let mut pages = vec![page];
        let report = clean_document(&mut pages);
        assert_eq!(report.superscripts_merged, 2);
        assert_eq!(report.scripts_merged, 0);
        assert_eq!(pages[0].lines.len(), 16);
        assert!(pages[0].text.contains("the literature.\u{2077}"));
        assert!(pages[0].text.contains("in two studies\u{00B9}\u{00B2}"));
        assert_eq!(pages[0].lines[14].spans, [16, 0]);
        assert_eq!(pages[0].lines[15].spans, [17, 1]);
    }
}
