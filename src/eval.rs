//! Evaluation harness: scores an [`ExtractionResult`] against the ground truth
//! recovered from a paper's `LaTeX` source (see `crate::latex_refs`).
//!
//! Measured per paper: exact reference-count match, per-entry recall and
//! precision (greedy one-to-one matching by DOI, `arXiv` id, title,
//! first-author surname plus year, then whole-entry text similarity),
//! DOI/year/title field accuracy over the
//! matched pairs only (so segmentation recall is not counted twice), in-text
//! marker resolution and marker recall against the source's `\cite`
//! commands, and a word-alignment diagnostic
//! of the body text order. These are diagnostics on real papers, not the
//! human-checked acceptance protocol.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use regex::Regex;
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use crate::citations::find_reference_section;
use crate::latex_refs::{GroundTruth, TruthPaper, TruthReference};
use crate::schema::{
    CitationMarker, ExtractionResult, Metadata, PageText, ReferenceEntry, StageTimings,
};

/// Product target: warm service time per 20-page chunk, in milliseconds.
pub const TARGET_MS_PER_CHUNK: f64 = 30.0;

/// Maximum tokens per side considered by [`word_alignment`]; longer inputs are
/// sampled evenly down to this many tokens so the quadratic DP stays bounded.
pub const MAX_ALIGN_TOKENS: usize = 12_000;

/// Minimum Jaccard similarity of title words for a fuzzy title match.
const TITLE_JACCARD_MIN: f32 = 0.8;

/// Minimum Jaccard similarity of whole-entry words for the last-resort text
/// match, as `(numerator, denominator)` = 0.6 so the test stays in integers.
const TEXT_JACCARD_MIN: (usize, usize) = (3, 5);

/// Minimum Jaccard similarity of title words for the paper's own title to
/// count as correct when the normalised titles differ.
const PAPER_TITLE_JACCARD_MIN: f32 = 0.9;

/// Unmatched truth keys listed per paper in the markdown report.
const UNMATCHED_KEYS_SHOWN: usize = 10;

/// Characters of an extracted or truth value quoted in a paper metadata
/// mismatch line.
const MISMATCH_VALUE_CHARS: usize = 100;

/// Trailing punctuation that is never part of a DOI (as in
/// `metadata::find_doi` and `citations::find_doi`).
const DOI_TRAILING: [char; 8] = ['.', ',', ';', ')', ']', ':', '}', '\''];

/// Byte cap on [`PaperDump::reference_section_text`] (60 kB).
pub const REFERENCE_TEXT_CAP: usize = 60_000;

/// Separator between pages in [`PaperDump::reference_section_text`].
pub const DUMP_PAGE_SEPARATOR: &str = "\n\u{c}\n";

/// How one truth reference was (or was not) paired with an extracted entry.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RefMatch {
    /// `TruthReference::key` (the `\bibitem` / `.bib` key).
    pub truth_key: String,
    /// `ReferenceEntry::index` of the paired entry, if any.
    pub extracted_index: Option<u32>,
    /// `"doi"`, `"arxiv"`, `"title"`, `"author-year"`, `"text"` or `"none"`.
    /// When duplicate truth entries swap partners (see [`match_references`])
    /// the method and score travel with the extracted entry.
    pub method: String,
    /// 1.0 for exact DOI/`arXiv`/title matches, the Jaccard value for fuzzy
    /// title and text matches, 0.75 for author-year matches, 0.0 when
    /// unmatched.
    pub score: f32,
}

/// Every measurement for one paper.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PaperEval {
    pub id: String,
    /// `ExtractionResult::status` as a string, or `failed:<error>`.
    pub status: String,
    /// Pages actually extracted.
    pub pages: u32,
    /// `GroundTruth::method`: `bbl`, `bib-cited` or `bib-all`.
    pub truth_method: String,
    pub truth_refs: u32,
    pub extracted_refs: u32,
    pub matched_refs: u32,
    pub ref_count_exact: bool,
    pub unmatched_truth_keys: Vec<String>,
    /// `ReferenceEntry::index` values that matched no truth reference.
    pub spurious_extracted: Vec<u32>,
    /// Matched truth references that carry a DOI (the DOI-accuracy denominator).
    pub doi_truth: u32,
    /// Matched pairs whose extracted DOI equals the truth DOI.
    pub doi_correct: u32,
    /// Matched truth references that carry a year.
    pub year_truth: u32,
    pub year_correct: u32,
    /// Matched truth references that carry a title.
    pub title_truth: u32,
    pub title_correct: u32,
    /// All truth references that carry a DOI, matched or not.
    #[serde(default)]
    pub doi_truth_total: u32,
    /// All truth references that carry a year, matched or not.
    #[serde(default)]
    pub year_truth_total: u32,
    /// All truth references that carry a title, matched or not.
    #[serde(default)]
    pub title_truth_total: u32,
    /// Matched truth references with a DOI that is printed in the PDF: the
    /// normalised DOI occurs in the concatenated page text (case-insensitive,
    /// whitespace ignored on both sides so line-wrapped DOIs count), or the
    /// extracted DOI is already correct (covers DOIs hyphenated across lines),
    /// so `doi_correct <= doi_printed` always holds.
    #[serde(default)]
    pub doi_printed: u32,
    /// `extracted_refs / truth_refs`; 0.0 when the truth has no references.
    /// Above 1 means over-segmentation, below 1 merged or missed entries.
    #[serde(default)]
    pub over_segmentation: f32,
    /// Per-stage timings copied from `ExtractionResult::timings`.
    #[serde(default)]
    pub timings: StageTimings,
    /// `\cite`-family commands counted in the `LaTeX` source.
    pub truth_cite_commands: u32,
    pub extracted_markers: u32,
    /// Markers with at least one resolved target.
    pub resolved_markers: u32,
    /// `resolved_markers / truth_cite_commands`; `None` when the source has
    /// no cite commands. Not clamped, so over-detection shows as > 1.
    #[serde(default)]
    pub marker_recall: Option<f32>,
    /// Sum of resolved targets over all markers.
    pub marker_targets: u32,
    /// [`word_alignment`] of the extracted page text against the detexed
    /// body; `None` when the truth has no body text.
    pub body_alignment: Option<f32>,
    /// Sum of all stage timings.
    pub ms_total: f64,
    pub ms_per_chunk: f64,
    pub chunks: u32,
    /// Document warnings plus page warnings.
    pub warnings: u32,
    pub matches: Vec<RefMatch>,
    /// Paper title (`metadata.title`) against the `LaTeX` title: equal after
    /// [`normalize_title`], or title-word Jaccard >= 0.9. `None` when the
    /// source states no title.
    #[serde(default)]
    pub paper_title_correct: Option<bool>,
    /// Person names in the source's author commands.
    #[serde(default)]
    pub authors_truth: u32,
    /// Entries of `metadata.authors`.
    #[serde(default)]
    pub authors_extracted: u32,
    /// Extracted authors paired one-to-one with truth authors by folded
    /// surname plus first initial.
    #[serde(default)]
    pub authors_correct: u32,
    /// `metadata.doi` equals the source's DOI after normalisation; `None`
    /// when the source states no DOI (or only a template placeholder DOI).
    #[serde(default)]
    pub paper_doi_correct: Option<bool>,
    /// One line per wrong paper title or DOI, `title extracted "…" vs truth
    /// "…"` / `doi extracted "…" vs truth "…"`, each value capped at 100
    /// characters; empty when both are right or unknown.
    #[serde(default)]
    pub paper_metadata_mismatches: Vec<String>,
}

/// Corpus-level rates; all rates are over non-failed papers.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Summary {
    pub papers: u32,
    pub failed: u32,
    pub ref_count_exact_rate: f32,
    pub ref_recall: f32,
    pub ref_precision: f32,
    pub doi_accuracy: f32,
    /// `doi_correct / doi_printed` summed over papers: DOI accuracy counting
    /// only DOIs that the PDF actually prints. 0.0 when none are printed.
    #[serde(default)]
    pub doi_accuracy_printed: f32,
    pub year_accuracy: f32,
    pub title_accuracy: f32,
    /// Precision-like: resolved markers over extracted markers.
    pub marker_resolution_rate: f32,
    /// Resolved markers over truth `\cite` commands, summed over the
    /// non-failed papers whose source has at least one cite command.
    #[serde(default)]
    pub marker_recall: f32,
    pub mean_body_alignment: Option<f32>,
    pub p50_ms_per_chunk: f64,
    pub p95_ms_per_chunk: f64,
    pub target_ms_per_chunk: f64,
    /// Mean `StageTimings::acquire_ms` per non-failed document.
    #[serde(default)]
    pub mean_acquire_ms: f64,
    /// Mean `StageTimings::parse_ms` per non-failed document.
    #[serde(default)]
    pub mean_parse_ms: f64,
    /// Mean `StageTimings::order_ms` per non-failed document.
    #[serde(default)]
    pub mean_order_ms: f64,
    /// Mean `StageTimings::metadata_ms` per non-failed document.
    #[serde(default)]
    pub mean_metadata_ms: f64,
    /// Mean `StageTimings::citations_ms` per non-failed document.
    #[serde(default)]
    pub mean_citations_ms: f64,
    /// Mean `StageTimings::write_ms` per non-failed document.
    #[serde(default)]
    pub mean_write_ms: f64,
    /// Papers whose extracted title is correct over papers whose source
    /// states a title.
    #[serde(default)]
    pub paper_title_accuracy: f32,
    /// Correct paper authors over truth paper authors.
    #[serde(default)]
    pub paper_author_recall: f32,
    /// Correct paper authors over extracted paper authors, counting only
    /// papers whose source names at least one author (extracted names cannot
    /// be judged against an empty truth list).
    #[serde(default)]
    pub paper_author_precision: f32,
    /// Papers whose extracted DOI is correct over papers whose source states
    /// a DOI.
    #[serde(default)]
    pub paper_doi_accuracy: f32,
}

/// One evaluation run over the corpus.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CorpusReport {
    pub generated_unix: i64,
    pub backend: String,
    pub host: String,
    pub papers: Vec<PaperEval>,
    pub summary: Summary,
}

/// Lower-cases `s` and keeps only Unicode letters and digits, with runs of
/// anything else collapsed to a single space. No compatibility decomposition.
pub fn normalize_title(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut separator_pending = false;
    for c in s.chars() {
        if c.is_alphanumeric() {
            if separator_pending && !out.is_empty() {
                out.push(' ');
            }
            separator_pending = false;
            out.extend(c.to_lowercase());
        } else {
            separator_pending = true;
        }
    }
    out
}

/// Lower-case alphanumeric words of `s`, in order.
fn words(s: &str) -> Vec<String> {
    normalize_title(s)
        .split(' ')
        .filter(|w| !w.is_empty())
        .map(String::from)
        .collect()
}

/// Maps each word to a small integer id shared across both sides.
fn intern_tokens<'a>(words: &'a [String], table: &mut HashMap<&'a str, u32>) -> Vec<u32> {
    words
        .iter()
        .map(|w| {
            let next = table.len() as u32;
            *table.entry(w.as_str()).or_insert(next)
        })
        .collect()
}

/// Keeps at most `cap` tokens, evenly spaced over the input when it is longer.
fn sample_evenly(tokens: Vec<u32>, cap: usize) -> Vec<u32> {
    if tokens.len() <= cap {
        return tokens;
    }
    (0..cap).map(|i| tokens[i * tokens.len() / cap]).collect()
}

/// Length of the longest common subsequence using two DP rows over the
/// shorter side (memory `O(min(n, m))`, time `O(n * m)`).
fn lcs_len(left: &[u32], right: &[u32]) -> usize {
    let (long, short) = if left.len() >= right.len() {
        (left, right)
    } else {
        (right, left)
    };
    if short.is_empty() {
        return 0;
    }
    let mut prev = vec![0_usize; short.len() + 1];
    let mut cur = vec![0_usize; short.len() + 1];
    for &token in long {
        for (j, &other) in short.iter().enumerate() {
            cur[j + 1] = if token == other {
                prev[j] + 1
            } else {
                cur[j].max(prev[j + 1])
            };
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[short.len()]
}

/// Order-sensitive similarity of two texts: `2 * lcs / (n + m)` over
/// lower-case alphanumeric word tokens, each side capped at
/// [`MAX_ALIGN_TOKENS`] by even sampling. Returns 1.0 when both sides have no
/// tokens and 0.0 when exactly one side has none.
pub fn word_alignment(a: &str, b: &str) -> f32 {
    let left_words = words(a);
    let right_words = words(b);
    if left_words.is_empty() && right_words.is_empty() {
        return 1.0;
    }
    if left_words.is_empty() || right_words.is_empty() {
        return 0.0;
    }
    let mut table: HashMap<&str, u32> = HashMap::new();
    let left = sample_evenly(intern_tokens(&left_words, &mut table), MAX_ALIGN_TOKENS);
    let right = sample_evenly(intern_tokens(&right_words, &mut table), MAX_ALIGN_TOKENS);
    let lcs = lcs_len(&left, &right);
    ((2 * lcs) as f64 / (left.len() + right.len()) as f64) as f32
}

/// `10.NNNN/`: where a DOI starts inside a longer string.
fn doi_prefix_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"10\.\d{4,9}/").expect("valid regex"))
}

/// Lower-case DOI without resolver prefixes or trailing punctuation.
///
/// Everything before the first `10.NNNN/` is dropped, so `doi:`, `DOI: `,
/// `https://doi.org/`, `https://www.doi.org/` and combinations such as
/// `doi: https://doi.org/` all reduce to the bare DOI; the same trailing
/// punctuation as `metadata::find_doi` is trimmed. Both sides of every DOI
/// comparison go through this function.
fn normalize_doi(s: &str) -> String {
    let lower = s.trim().to_lowercase();
    if let Some(found) = doi_prefix_re().find(&lower) {
        return lower[found.start()..]
            .trim()
            .trim_end_matches(DOI_TRAILING)
            .to_string();
    }
    let mut rest: &str = &lower;
    for prefix in [
        "https://doi.org/",
        "http://doi.org/",
        "https://dx.doi.org/",
        "http://dx.doi.org/",
        "doi.org/",
        "doi:",
        "doi ",
    ] {
        if let Some(stripped) = rest.strip_prefix(prefix) {
            rest = stripped.trim();
        }
    }
    rest.trim_end_matches(DOI_TRAILING).to_string()
}

/// True for the DOIs that `LaTeX` templates ship as placeholders
/// (`10.1145/nnnnnnn.nnnnnnn`, `10.1145/1122445.1122456`, `10.475/123_4`):
/// a source stating one of these states no DOI.
fn is_placeholder_doi(doi: &str) -> bool {
    let norm = normalize_doi(doi);
    if matches!(norm.as_str(), "10.1145/1122445.1122456" | "10.475/123_4") {
        return true;
    }
    let Some((_, suffix)) = norm.split_once('/') else {
        return false;
    };
    !suffix.is_empty()
        && suffix
            .chars()
            .all(|c| matches!(c, 'n' | 'x' | '.' | '-' | '_' | '#'))
}

/// `"value"` capped at [`MISMATCH_VALUE_CHARS`] characters (with `…` when
/// cut), or `none` when absent.
fn mismatch_value(value: Option<&str>) -> String {
    let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) else {
        return "none".to_string();
    };
    let mut capped: String = value.chars().take(MISMATCH_VALUE_CHARS).collect();
    if value.chars().count() > MISMATCH_VALUE_CHARS {
        capped.push('…');
    }
    format!("\"{capped}\"")
}

/// Mismatch lines for a wrong paper title and a wrong paper DOI.
fn paper_mismatches(
    meta: &Metadata,
    paper: &TruthPaper,
    title_correct: Option<bool>,
    doi_correct: Option<bool>,
) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    if title_correct == Some(false) {
        lines.push(format!(
            "title extracted {} vs truth {}",
            mismatch_value(meta.title.as_deref()),
            mismatch_value(paper.title.as_deref())
        ));
    }
    if doi_correct == Some(false) {
        lines.push(format!(
            "doi extracted {} vs truth {}",
            mismatch_value(meta.doi.as_deref()),
            mismatch_value(paper.doi.as_deref())
        ));
    }
    lines
}

/// `arXiv` id without a `vN` version suffix (`2101.00001v2` -> `2101.00001`).
fn strip_arxiv_version(id: &str) -> &str {
    if let Some(pos) = id.rfind('v') {
        let suffix = &id[pos + 1..];
        if pos > 0 && !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit()) {
            return &id[..pos];
        }
    }
    id
}

/// Lower-case `arXiv` id without `arXiv:`/URL prefixes or a version suffix.
fn normalize_arxiv(s: &str) -> String {
    let lower = s.trim().to_lowercase();
    let mut rest: &str = &lower;
    for prefix in [
        "https://arxiv.org/abs/",
        "http://arxiv.org/abs/",
        "arxiv.org/abs/",
        "arxiv:",
        "arxiv ",
    ] {
        if let Some(stripped) = rest.strip_prefix(prefix) {
            rest = stripped.trim();
        }
    }
    strip_arxiv_version(rest.trim_end_matches(['.', ',', ';'])).to_string()
}

/// Lower-case ASCII-only letters and digits of `s`: canonical decomposition
/// drops combining marks, and the common non-decomposable letters are mapped
/// by hand (`ß` -> `ss`, `ø` -> `o`, `ł` -> `l`, ...).
fn fold_ascii(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.nfd() {
        if !c.is_alphanumeric() {
            continue;
        }
        match c {
            'ß' => out.push_str("ss"),
            'ø' | 'Ø' => out.push('o'),
            'æ' | 'Æ' => out.push_str("ae"),
            'œ' | 'Œ' => out.push_str("oe"),
            'ł' | 'Ł' => out.push('l'),
            'đ' | 'Đ' | 'ð' | 'Ð' => out.push('d'),
            'þ' | 'Þ' => out.push_str("th"),
            'ı' => out.push('i'),
            _ => out.extend(c.to_lowercase()),
        }
    }
    out
}

/// Whether a name token is an initial (`J.`, `AB`) rather than a name word.
fn is_initial_token(token: &str, folded: &str) -> bool {
    let letters = folded.chars().count();
    letters <= 1
        || (letters <= 3
            && token
                .chars()
                .filter(|c| c.is_alphabetic())
                .all(char::is_uppercase))
}

/// Folded, lower-case surname of an author name in any of the common shapes:
/// `Smith, J.`, `J. A. Smith`, `Smith AB`, `van der Berg, J.`.
fn surname(name: &str) -> String {
    let base = name.split(',').next().unwrap_or(name).trim();
    let mut last_any = String::new();
    let mut last_word = String::new();
    for token in base.split_whitespace() {
        let folded = fold_ascii(token);
        if folded.is_empty() {
            continue;
        }
        if !is_initial_token(token, &folded) {
            last_word.clone_from(&folded);
        }
        last_any = folded;
    }
    if last_word.is_empty() {
        last_any
    } else {
        last_word
    }
}

/// `surname|year` key for author-year matching; `None` without both parts.
fn author_year_key(first_author: Option<&String>, year: Option<u16>) -> Option<String> {
    let name = first_author?;
    let year = year?;
    let sur = surname(name);
    if sur.is_empty() {
        return None;
    }
    Some(format!("{sur}|{year}"))
}

/// Non-empty normalized title, or `None`.
fn title_key(title: Option<&String>) -> Option<String> {
    let normalized = normalize_title(title?);
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

/// Jaccard similarity of two word sets; 0.0 when either is empty.
fn jaccard(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f32 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count();
    let union = a.union(b).count();
    (inter as f64 / union as f64) as f32
}

/// Intersection and union sizes of two word sets.
fn overlap(a: &BTreeSet<String>, b: &BTreeSet<String>) -> (usize, usize) {
    let inter = a.intersection(b).count();
    (inter, a.len() + b.len() - inter)
}

/// Joins words split by a line-break hyphen (`recon- struction` ->
/// `reconstruction`): a `-` between a letter and whitespace followed by a
/// letter is dropped together with the whitespace.
fn join_break_hyphens(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '-' && i > 0 && chars[i - 1].is_alphabetic() {
            let mut next = i + 1;
            while next < chars.len() && chars[next].is_whitespace() {
                next += 1;
            }
            if next > i + 1 && next < chars.len() && chars[next].is_alphabetic() {
                i = next;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

/// `s` without a leading `[n]` label.
fn strip_bracket_label(s: &str) -> &str {
    let trimmed = s.trim_start();
    trimmed
        .strip_prefix('[')
        .and_then(|inner| inner.find(']').map(|close| &inner[close + 1..]))
        .unwrap_or(trimmed)
}

/// Word set of a whole reference text for the last-resort match: NFKC
/// (ligatures such as `ﬁ`), line-break hyphens joined, a leading `[n]` label
/// dropped, then lower-case alphanumeric words.
fn text_tokens(s: &str) -> BTreeSet<String> {
    let compat: String = s.nfkc().collect();
    let joined = join_break_hyphens(&compat);
    words(strip_bracket_label(&joined)).into_iter().collect()
}

/// Mutable state of the greedy one-to-one matcher.
struct Matcher<'a> {
    extracted: &'a [ReferenceEntry],
    matches: Vec<RefMatch>,
    used: Vec<bool>,
}

impl Matcher<'_> {
    fn is_open(&self, truth_pos: usize) -> bool {
        self.matches[truth_pos].extracted_index.is_none()
    }

    fn assign(&mut self, truth_pos: usize, ext_pos: usize, method: &str, score: f32) {
        self.used[ext_pos] = true;
        let m = &mut self.matches[truth_pos];
        m.extracted_index = Some(self.extracted[ext_pos].index);
        m.method = method.to_string();
        m.score = score;
    }

    /// Pairs open truth entries with the first unused extracted entry whose
    /// key equals theirs.
    fn exact_pass(
        &mut self,
        truth_keys: &[Option<String>],
        ext_keys: &[Option<String>],
        method: &str,
        score: f32,
    ) {
        for (truth_pos, key) in truth_keys.iter().enumerate() {
            let Some(key) = key else {
                continue;
            };
            if !self.is_open(truth_pos) {
                continue;
            }
            let found = ext_keys.iter().enumerate().position(|(ext_pos, ext_key)| {
                !self.used[ext_pos] && ext_key.as_deref() == Some(key.as_str())
            });
            if let Some(ext_pos) = found {
                self.assign(truth_pos, ext_pos, method, score);
            }
        }
    }

    /// Pairs open truth entries with the unused extracted entry whose title
    /// words have the highest Jaccard similarity, when it reaches the minimum.
    fn fuzzy_title_pass(
        &mut self,
        truth_words: &[BTreeSet<String>],
        ext_words: &[BTreeSet<String>],
    ) {
        for (truth_pos, truth_set) in truth_words.iter().enumerate() {
            if truth_set.is_empty() || !self.is_open(truth_pos) {
                continue;
            }
            let mut best: Option<(usize, f32)> = None;
            for (ext_pos, ext_set) in ext_words.iter().enumerate() {
                if self.used[ext_pos] {
                    continue;
                }
                let sim = jaccard(truth_set, ext_set);
                if sim >= TITLE_JACCARD_MIN && best.is_none_or(|(_, b)| sim > b) {
                    best = Some((ext_pos, sim));
                }
            }
            if let Some((ext_pos, sim)) = best {
                self.assign(truth_pos, ext_pos, "title", sim);
            }
        }
    }

    /// Last resort: pairs each open truth entry, in order, with the unused
    /// extracted entry whose whole-text word set is most similar, when the
    /// Jaccard similarity reaches [`TEXT_JACCARD_MIN`]. On equal similarity the
    /// entry agreeing with the truth on more of first-author surname and year
    /// wins, then the earliest one.
    fn text_pass(
        &mut self,
        truth: &[TruthReference],
        truth_tokens: &[BTreeSet<String>],
        ext_tokens: &[BTreeSet<String>],
    ) {
        let (min_num, min_den) = TEXT_JACCARD_MIN;
        for (truth_pos, truth_set) in truth_tokens.iter().enumerate() {
            if truth_set.is_empty() || !self.is_open(truth_pos) {
                continue;
            }
            // (extracted position, intersection, union, surname/year agreements)
            let mut best: Option<(usize, usize, usize, u32)> = None;
            for (ext_pos, ext_set) in ext_tokens.iter().enumerate() {
                if self.used[ext_pos] || ext_set.is_empty() {
                    continue;
                }
                let (inter, union) = overlap(truth_set, ext_set);
                if inter * min_den < union * min_num {
                    continue;
                }
                let agrees = Self::agreement(&truth[truth_pos], &self.extracted[ext_pos]);
                let better = best.is_none_or(|(_, best_inter, best_union, best_agrees)| {
                    let candidate_score = inter * best_union;
                    let best_score = best_inter * union;
                    candidate_score > best_score
                        || (candidate_score == best_score && agrees > best_agrees)
                });
                if better {
                    best = Some((ext_pos, inter, union, agrees));
                }
            }
            if let Some((ext_pos, inter, union, _)) = best {
                let sim = (inter as f64 / union as f64) as f32;
                self.assign(truth_pos, ext_pos, "text", sim);
            }
        }
    }

    /// Position in `extracted` of the entry paired with `truth_pos`.
    fn ext_pos_of(&self, truth_pos: usize) -> Option<usize> {
        let index = self.matches[truth_pos].extracted_index?;
        self.extracted.iter().position(|entry| entry.index == index)
    }

    /// Year and first-author agreements of pairing `truth_ref` with `entry`.
    fn agreement(truth_ref: &TruthReference, entry: &ReferenceEntry) -> u32 {
        let year = u32::from(truth_ref.year.is_some() && truth_ref.year == entry.year);
        let author = match (truth_ref.authors.first(), entry.authors.first()) {
            (Some(t), Some(e)) => {
                let sur = surname(t);
                u32::from(!sur.is_empty() && sur == surname(e))
            }
            _ => 0,
        };
        year + author
    }

    /// Duplicate truth entries (see [`duplicate_groups`]) take their paired
    /// extracted entries in index order, so the earlier truth entry gets the
    /// earlier extracted entry, unless the current pairing agrees on year and
    /// first-author surname more often. Each extracted entry keeps the method
    /// and score it was matched with.
    fn order_duplicates(&mut self, truth: &[TruthReference], groups: &[Vec<usize>]) {
        for group in groups {
            let mut members: Vec<(usize, usize)> = Vec::new();
            for &truth_pos in group {
                if let Some(ext_pos) = self.ext_pos_of(truth_pos) {
                    members.push((truth_pos, ext_pos));
                }
            }
            if members.len() < 2 {
                continue;
            }
            let mut ordered: Vec<usize> = members.iter().map(|&(_, ext_pos)| ext_pos).collect();
            ordered.sort_by_key(|&ext_pos| self.extracted[ext_pos].index);
            let unchanged = members
                .iter()
                .zip(&ordered)
                .all(|(&(_, current), &wanted)| current == wanted);
            if unchanged {
                continue;
            }
            let current_score: u32 = members
                .iter()
                .map(|&(truth_pos, ext_pos)| {
                    Self::agreement(&truth[truth_pos], &self.extracted[ext_pos])
                })
                .sum();
            let ordered_score: u32 = members
                .iter()
                .zip(&ordered)
                .map(|(&(truth_pos, _), &ext_pos)| {
                    Self::agreement(&truth[truth_pos], &self.extracted[ext_pos])
                })
                .sum();
            if ordered_score < current_score {
                continue;
            }
            let carried: Vec<(usize, String, f32)> = members
                .iter()
                .map(|&(truth_pos, ext_pos)| {
                    let m = &self.matches[truth_pos];
                    (ext_pos, m.method.clone(), m.score)
                })
                .collect();
            for (&(truth_pos, _), &ext_pos) in members.iter().zip(&ordered) {
                let Some((_, method, score)) = carried.iter().find(|(pos, _, _)| *pos == ext_pos)
                else {
                    continue;
                };
                let m = &mut self.matches[truth_pos];
                m.extracted_index = Some(self.extracted[ext_pos].index);
                m.method.clone_from(method);
                m.score = *score;
            }
        }
    }
}

/// Groups (two or more positions, ascending) of truth entries that describe
/// the same work: equal normalised title, or equal normalised text when there
/// is no title, and no two different DOIs or `arXiv` ids among them.
fn duplicate_groups(
    truth: &[TruthReference],
    truth_doi: &[Option<String>],
    truth_arxiv: &[Option<String>],
) -> Vec<Vec<usize>> {
    let mut by_key: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (truth_pos, truth_ref) in truth.iter().enumerate() {
        let key = title_key(truth_ref.title.as_ref())
            .map(|title| format!("t|{title}"))
            .or_else(|| {
                let text = normalize_title(&truth_ref.text);
                (!text.is_empty()).then(|| format!("x|{text}"))
            });
        if let Some(key) = key {
            by_key.entry(key).or_default().push(truth_pos);
        }
    }
    by_key
        .into_values()
        .filter(|group| {
            let dois: BTreeSet<&str> = group
                .iter()
                .filter_map(|&pos| truth_doi[pos].as_deref())
                .collect();
            let arxiv_ids: BTreeSet<&str> = group
                .iter()
                .filter_map(|&pos| truth_arxiv[pos].as_deref())
                .collect();
            group.len() >= 2 && dois.len() <= 1 && arxiv_ids.len() <= 1
        })
        .collect()
}

/// Greedy one-to-one pairing of truth references with extracted entries, in
/// priority order: equal DOI (case-insensitive), equal `arXiv` id (version
/// ignored), equal normalized title or title-word Jaccard >= 0.8, equal
/// first-author surname (lower-case, ASCII-folded) plus year, then, as a last
/// resort, the most similar whole entry text (word Jaccard >= 0.6 of the
/// truth `text` and the extracted `raw`, method `"text"`). Each extracted
/// entry is used at most once. Finally, truth entries that are duplicates of
/// each other (same normalised title or text) take their partners in index
/// order unless that agrees worse on year and first author, so they are not
/// paired crosswise. One [`RefMatch`] per truth reference, in order.
pub fn match_references(truth: &[TruthReference], extracted: &[ReferenceEntry]) -> Vec<RefMatch> {
    let mut matcher = Matcher {
        extracted,
        matches: truth
            .iter()
            .map(|truth_ref| RefMatch {
                truth_key: truth_ref.key.clone(),
                extracted_index: None,
                method: "none".to_string(),
                score: 0.0,
            })
            .collect(),
        used: vec![false; extracted.len()],
    };

    let truth_doi: Vec<Option<String>> = truth
        .iter()
        .map(|truth_ref| {
            truth_ref
                .doi
                .as_deref()
                .map(normalize_doi)
                .filter(|doi| !doi.is_empty())
        })
        .collect();
    let ext_doi: Vec<Option<String>> = extracted
        .iter()
        .map(|entry| {
            entry
                .doi
                .as_deref()
                .map(normalize_doi)
                .filter(|doi| !doi.is_empty())
        })
        .collect();
    matcher.exact_pass(&truth_doi, &ext_doi, "doi", 1.0);

    let truth_arxiv: Vec<Option<String>> = truth
        .iter()
        .map(|truth_ref| {
            truth_ref
                .arxiv_id
                .as_deref()
                .map(normalize_arxiv)
                .filter(|id| !id.is_empty())
        })
        .collect();
    let ext_arxiv: Vec<Option<String>> = extracted
        .iter()
        .map(|entry| {
            entry
                .arxiv_id
                .as_deref()
                .map(normalize_arxiv)
                .filter(|id| !id.is_empty())
        })
        .collect();
    matcher.exact_pass(&truth_arxiv, &ext_arxiv, "arxiv", 1.0);

    let truth_title: Vec<Option<String>> = truth
        .iter()
        .map(|truth_ref| title_key(truth_ref.title.as_ref()))
        .collect();
    let ext_title: Vec<Option<String>> = extracted
        .iter()
        .map(|entry| title_key(entry.title.as_ref()))
        .collect();
    matcher.exact_pass(&truth_title, &ext_title, "title", 1.0);

    let truth_words: Vec<BTreeSet<String>> = truth_title
        .iter()
        .map(|title| {
            title
                .as_deref()
                .map(words)
                .unwrap_or_default()
                .into_iter()
                .collect()
        })
        .collect();
    let ext_words: Vec<BTreeSet<String>> = ext_title
        .iter()
        .map(|title| {
            title
                .as_deref()
                .map(words)
                .unwrap_or_default()
                .into_iter()
                .collect()
        })
        .collect();
    matcher.fuzzy_title_pass(&truth_words, &ext_words);

    let truth_ay: Vec<Option<String>> = truth
        .iter()
        .map(|truth_ref| author_year_key(truth_ref.authors.first(), truth_ref.year))
        .collect();
    let ext_ay: Vec<Option<String>> = extracted
        .iter()
        .map(|entry| author_year_key(entry.authors.first(), entry.year))
        .collect();
    matcher.exact_pass(&truth_ay, &ext_ay, "author-year", 0.75);

    let truth_tokens: Vec<BTreeSet<String>> = truth
        .iter()
        .enumerate()
        .map(|(truth_pos, truth_ref)| {
            if matcher.is_open(truth_pos) {
                text_tokens(&truth_ref.text)
            } else {
                BTreeSet::new()
            }
        })
        .collect();
    let ext_tokens: Vec<BTreeSet<String>> = extracted
        .iter()
        .enumerate()
        .map(|(ext_pos, entry)| {
            if matcher.used[ext_pos] {
                BTreeSet::new()
            } else {
                text_tokens(&entry.raw)
            }
        })
        .collect();
    matcher.text_pass(truth, &truth_tokens, &ext_tokens);

    let groups = duplicate_groups(truth, &truth_doi, &truth_arxiv);
    matcher.order_duplicates(truth, &groups);

    matcher.matches
}

/// Whether two optional DOIs are both present and equal after normalisation.
fn doi_equal(truth: Option<&String>, extracted: Option<&String>) -> bool {
    match (truth, extracted) {
        (Some(t), Some(e)) => {
            let t = normalize_doi(t);
            !t.is_empty() && t == normalize_doi(e)
        }
        _ => false,
    }
}

/// Whether two optional titles are both present and equal after normalisation.
fn title_equal(truth: Option<&String>, extracted: Option<&String>) -> bool {
    match (title_key(truth), title_key(extracted)) {
        (Some(t), Some(e)) => t == e,
        _ => false,
    }
}

/// Whether the extracted paper title matches the truth title: equal after
/// [`normalize_title`], or title-word Jaccard >= 0.9.
fn paper_title_matches(truth: &str, extracted: Option<&str>) -> bool {
    let Some(extracted) = extracted else {
        return false;
    };
    let truth_norm = normalize_title(truth);
    let ext_norm = normalize_title(extracted);
    if truth_norm.is_empty() || ext_norm.is_empty() {
        return false;
    }
    if truth_norm == ext_norm {
        return true;
    }
    let truth_words: BTreeSet<String> = words(truth).into_iter().collect();
    let ext_words: BTreeSet<String> = words(extracted).into_iter().collect();
    jaccard(&truth_words, &ext_words) >= PAPER_TITLE_JACCARD_MIN
}

/// ASCII-folded lower-case letters of `s` only (digits and marks dropped).
fn fold_letters(s: &str) -> String {
    fold_ascii(s)
        .chars()
        .filter(char::is_ascii_alphabetic)
        .collect()
}

/// `(surname, first initial)` of a person name for paper-author matching.
/// Tokens are folded to lower-case letters (so `Gao1` and `GAO` give `gao`);
/// the surname is the last token of at least two letters before a comma
/// (`Smith, J.`), else of the whole name; the initial is the first letter of
/// the first token after the comma, else of the first other token. `None`
/// without a surname.
fn person_key(name: &str) -> Option<(String, Option<char>)> {
    let tokens = |part: &str| -> Vec<String> {
        part.split_whitespace()
            .map(fold_letters)
            .filter(|token| !token.is_empty())
            .collect()
    };
    let (family, after_comma) = name.split_once(',').map_or_else(
        || (tokens(name), None),
        |(before, after)| (tokens(before), Some(tokens(after))),
    );
    let pos = family.iter().rposition(|token| token.len() >= 2)?;
    let given: Option<&String> = if let Some(rest) = &after_comma {
        rest.first()
    } else {
        family
            .iter()
            .enumerate()
            .find(|&(k, _)| k != pos)
            .map(|(_, token)| token)
    };
    let initial = given.and_then(|token| token.chars().next());
    Some((family[pos].clone(), initial))
}

/// Number of extracted names paired one-to-one with truth names by equal
/// [`person_key`].
fn author_matches(truth: &[String], extracted: &[String]) -> u32 {
    let ext_keys: Vec<Option<(String, Option<char>)>> = extracted
        .iter()
        .map(String::as_str)
        .map(person_key)
        .collect();
    let mut used = vec![false; ext_keys.len()];
    let mut correct = 0_u32;
    for key in truth.iter().map(String::as_str).filter_map(person_key) {
        let found =
            (0..ext_keys.len()).find(|&pos| !used[pos] && ext_keys[pos].as_ref() == Some(&key));
        if let Some(pos) = found {
            used[pos] = true;
            correct += 1;
        }
    }
    correct
}

/// Concatenated page text, lower-cased, with all whitespace removed.
fn squashed_page_text(pages: &[PageText]) -> String {
    pages
        .iter()
        .flat_map(|page| page.text.chars())
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Whether the normalised DOI, whitespace removed, occurs in `squashed`
/// (the output of [`squashed_page_text`]).
fn doi_is_printed(doi: &str, squashed: &str) -> bool {
    let needle: String = normalize_doi(doi)
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    !needle.is_empty() && squashed.contains(&needle)
}

/// Scores one extraction result against its ground truth.
pub fn evaluate(id: &str, result: &ExtractionResult, truth: &GroundTruth) -> PaperEval {
    let matches = match_references(&truth.references, &result.references);

    let mut matched_refs = 0_u32;
    let mut unmatched_truth_keys: Vec<String> = Vec::new();
    let mut matched_indices: BTreeSet<u32> = BTreeSet::new();
    let mut doi_truth = 0_u32;
    let mut doi_correct = 0_u32;
    let mut year_truth = 0_u32;
    let mut year_correct = 0_u32;
    let mut title_truth = 0_u32;
    let mut title_correct = 0_u32;
    let mut doi_truth_total = 0_u32;
    let mut year_truth_total = 0_u32;
    let mut title_truth_total = 0_u32;
    let mut doi_printed = 0_u32;
    let printed_text = squashed_page_text(&result.pages);

    for (truth_ref, m) in truth.references.iter().zip(&matches) {
        let has_doi = u32::from(truth_ref.doi.is_some());
        let has_year = u32::from(truth_ref.year.is_some());
        let has_title = u32::from(truth_ref.title.is_some());
        doi_truth_total += has_doi;
        year_truth_total += has_year;
        title_truth_total += has_title;
        let Some(idx) = m.extracted_index else {
            unmatched_truth_keys.push(truth_ref.key.clone());
            continue;
        };
        matched_refs += 1;
        matched_indices.insert(idx);
        doi_truth += has_doi;
        year_truth += has_year;
        title_truth += has_title;
        let ext = result.references.iter().find(|entry| entry.index == idx);
        let correct = ext.is_some_and(|e| doi_equal(truth_ref.doi.as_ref(), e.doi.as_ref()));
        if let Some(doi) = &truth_ref.doi {
            doi_printed += u32::from(correct || doi_is_printed(doi, &printed_text));
        }
        if let Some(ext) = ext {
            doi_correct += u32::from(correct);
            year_correct += u32::from(truth_ref.year.is_some() && truth_ref.year == ext.year);
            title_correct += u32::from(title_equal(truth_ref.title.as_ref(), ext.title.as_ref()));
        }
    }

    let spurious_extracted: Vec<u32> = result
        .references
        .iter()
        .map(|entry| entry.index)
        .filter(|idx| !matched_indices.contains(idx))
        .collect();

    let extracted_markers = result.citations.len() as u32;
    let resolved_markers = result
        .citations
        .iter()
        .filter(|marker| !marker.targets.is_empty())
        .count() as u32;
    let marker_targets = result
        .citations
        .iter()
        .map(|marker| marker.targets.len())
        .sum::<usize>() as u32;
    let truth_cite_commands = truth.citations.cite_commands;
    let marker_recall = if truth_cite_commands == 0 {
        None
    } else {
        Some(ratio(
            u64::from(resolved_markers),
            u64::from(truth_cite_commands),
        ))
    };

    let body_alignment = if truth.body_text.trim().is_empty() {
        None
    } else {
        let joined: Vec<&str> = result.pages.iter().map(|page| page.text.as_str()).collect();
        Some(word_alignment(&joined.join("\n"), &truth.body_text))
    };

    let timings = &result.timings;
    let ms_total = timings.acquire_ms
        + timings.parse_ms
        + timings.order_ms
        + timings.metadata_ms
        + timings.citations_ms
        + timings.write_ms;
    let chunks = result.chunks.len() as u32;
    let ms_per_chunk = ms_total / f64::from(chunks.max(1));
    let page_warnings: usize = result.pages.iter().map(|page| page.warnings.len()).sum();
    let warnings = result.warnings.len() + page_warnings;

    let truth_refs = truth.references.len() as u32;
    let extracted_refs = result.references.len() as u32;

    let meta = &result.metadata;
    let paper_title_correct = truth
        .paper
        .title
        .as_deref()
        .filter(|title| !normalize_title(title).is_empty())
        .map(|title| paper_title_matches(title, meta.title.as_deref()));
    let extracted_names: Vec<String> = meta.authors.iter().map(|a| a.name.clone()).collect();
    let authors_correct = author_matches(&truth.paper.authors, &extracted_names);
    let paper_doi_correct = truth
        .paper
        .doi
        .as_ref()
        .filter(|doi| !normalize_doi(doi).is_empty() && !is_placeholder_doi(doi))
        .map(|doi| doi_equal(Some(doi), meta.doi.as_ref()));
    let paper_metadata_mismatches =
        paper_mismatches(meta, &truth.paper, paper_title_correct, paper_doi_correct);

    PaperEval {
        id: id.to_string(),
        status: result.status.as_str().to_string(),
        pages: result.pages.len() as u32,
        truth_method: truth.method.clone(),
        truth_refs,
        extracted_refs,
        matched_refs,
        ref_count_exact: truth_refs == extracted_refs,
        unmatched_truth_keys,
        spurious_extracted,
        doi_truth,
        doi_correct,
        year_truth,
        year_correct,
        title_truth,
        title_correct,
        doi_truth_total,
        year_truth_total,
        title_truth_total,
        doi_printed,
        over_segmentation: ratio(u64::from(extracted_refs), u64::from(truth_refs)),
        timings: result.timings,
        truth_cite_commands,
        extracted_markers,
        resolved_markers,
        marker_recall,
        marker_targets,
        body_alignment,
        ms_total,
        ms_per_chunk,
        chunks,
        warnings: warnings as u32,
        matches,
        paper_title_correct,
        authors_truth: truth.paper.authors.len() as u32,
        authors_extracted: extracted_names.len() as u32,
        authors_correct,
        paper_doi_correct,
        paper_metadata_mismatches,
    }
}

/// A paper that could not be evaluated: status `failed:<error>`, all zeros.
pub fn failed_paper(id: &str, error: &str) -> PaperEval {
    PaperEval {
        id: id.to_string(),
        status: format!("failed:{error}"),
        pages: 0,
        truth_method: String::new(),
        truth_refs: 0,
        extracted_refs: 0,
        matched_refs: 0,
        ref_count_exact: false,
        unmatched_truth_keys: Vec::new(),
        spurious_extracted: Vec::new(),
        doi_truth: 0,
        doi_correct: 0,
        year_truth: 0,
        year_correct: 0,
        title_truth: 0,
        title_correct: 0,
        doi_truth_total: 0,
        year_truth_total: 0,
        title_truth_total: 0,
        doi_printed: 0,
        over_segmentation: 0.0,
        timings: StageTimings::default(),
        truth_cite_commands: 0,
        extracted_markers: 0,
        resolved_markers: 0,
        marker_recall: None,
        marker_targets: 0,
        body_alignment: None,
        ms_total: 0.0,
        ms_per_chunk: 0.0,
        chunks: 0,
        warnings: 0,
        matches: Vec::new(),
        paper_title_correct: None,
        authors_truth: 0,
        authors_extracted: 0,
        authors_correct: 0,
        paper_doi_correct: None,
        paper_metadata_mismatches: Vec::new(),
    }
}

/// Whether a [`PaperEval`] came from [`failed_paper`].
fn is_failed(paper: &PaperEval) -> bool {
    paper.status.starts_with("failed:")
}

/// `num / den` as `f32`, 0.0 when `den` is zero.
fn ratio(num: u64, den: u64) -> f32 {
    if den == 0 {
        0.0
    } else {
        (num as f64 / den as f64) as f32
    }
}

/// Nearest-rank percentile of an ascending-sorted slice; 0.0 when empty.
fn percentile(sorted: &[f64], pct: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = (pct / 100.0 * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

/// Corpus-level rates over the non-failed papers. Recall is total matched
/// over total truth references, precision total matched over total extracted;
/// field accuracies are over matched pairs only; marker recall is resolved
/// markers over truth cite commands for papers that have any; percentiles
/// are nearest-rank over `ms_per_chunk`.
pub fn summarize(papers: &[PaperEval]) -> Summary {
    let ok: Vec<&PaperEval> = papers.iter().filter(|p| !is_failed(p)).collect();
    let failed = papers.len() - ok.len();

    let sum = |f: fn(&PaperEval) -> u32| -> u64 { ok.iter().map(|p| u64::from(f(p))).sum() };
    let exact = ok.iter().filter(|p| p.ref_count_exact).count() as u64;
    let matched = sum(|p| p.matched_refs);
    let mut cited_resolved = 0_u64;
    let mut cited_commands = 0_u64;
    for p in ok.iter().filter(|p| p.truth_cite_commands > 0) {
        cited_resolved += u64::from(p.resolved_markers);
        cited_commands += u64::from(p.truth_cite_commands);
    }

    let alignments: Vec<f64> = ok
        .iter()
        .filter_map(|p| p.body_alignment)
        .map(f64::from)
        .collect();
    let mean_body_alignment = if alignments.is_empty() {
        None
    } else {
        Some((alignments.iter().sum::<f64>() / alignments.len() as f64) as f32)
    };

    let mut ms: Vec<f64> = ok.iter().map(|p| p.ms_per_chunk).collect();
    ms.sort_unstable_by(f64::total_cmp);
    let mean_stage = |f: fn(&StageTimings) -> f64| -> f64 {
        if ok.is_empty() {
            0.0
        } else {
            ok.iter().map(|p| f(&p.timings)).sum::<f64>() / ok.len() as f64
        }
    };

    let titled: Vec<bool> = ok.iter().filter_map(|p| p.paper_title_correct).collect();
    let titles_right = titled.iter().filter(|correct| **correct).count() as u64;
    let authors_right = sum(|p| p.authors_correct);
    let authors_judged: u64 = ok
        .iter()
        .filter(|p| p.authors_truth > 0)
        .map(|p| u64::from(p.authors_extracted))
        .sum();
    let doi_checked: Vec<bool> = ok.iter().filter_map(|p| p.paper_doi_correct).collect();
    let dois_right = doi_checked.iter().filter(|correct| **correct).count() as u64;

    Summary {
        papers: papers.len() as u32,
        failed: failed as u32,
        ref_count_exact_rate: ratio(exact, ok.len() as u64),
        ref_recall: ratio(matched, sum(|p| p.truth_refs)),
        ref_precision: ratio(matched, sum(|p| p.extracted_refs)),
        doi_accuracy: ratio(sum(|p| p.doi_correct), sum(|p| p.doi_truth)),
        doi_accuracy_printed: ratio(sum(|p| p.doi_correct), sum(|p| p.doi_printed)),
        year_accuracy: ratio(sum(|p| p.year_correct), sum(|p| p.year_truth)),
        title_accuracy: ratio(sum(|p| p.title_correct), sum(|p| p.title_truth)),
        marker_resolution_rate: ratio(sum(|p| p.resolved_markers), sum(|p| p.extracted_markers)),
        marker_recall: ratio(cited_resolved, cited_commands),
        mean_body_alignment,
        p50_ms_per_chunk: percentile(&ms, 50.0),
        p95_ms_per_chunk: percentile(&ms, 95.0),
        target_ms_per_chunk: TARGET_MS_PER_CHUNK,
        mean_acquire_ms: mean_stage(|t| t.acquire_ms),
        mean_parse_ms: mean_stage(|t| t.parse_ms),
        mean_order_ms: mean_stage(|t| t.order_ms),
        mean_metadata_ms: mean_stage(|t| t.metadata_ms),
        mean_citations_ms: mean_stage(|t| t.citations_ms),
        mean_write_ms: mean_stage(|t| t.write_ms),
        paper_title_accuracy: ratio(titles_right, titled.len() as u64),
        paper_author_recall: ratio(authors_right, sum(|p| p.authors_truth)),
        paper_author_precision: ratio(authors_right, authors_judged),
        paper_doi_accuracy: ratio(dois_right, doi_checked.len() as u64),
    }
}

/// Assembles the report, stamping the current Unix time.
pub fn build_report(backend: &str, host: &str, papers: Vec<PaperEval>) -> CorpusReport {
    let generated_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
    let summary = summarize(&papers);
    CorpusReport {
        generated_unix,
        backend: backend.to_string(),
        host: host.to_string(),
        papers,
        summary,
    }
}

/// Escapes a value for a markdown table cell.
fn cell(s: &str) -> String {
    s.replace('|', "\\|").replace(['\n', '\r'], " ")
}

/// Formats a rate as a percentage with one decimal.
fn pct(rate: f32) -> String {
    let value = rate * 100.0;
    format!("{value:.1}%")
}

/// Formats an optional marker recall as a percentage.
fn recall_cell(recall: Option<f32>) -> String {
    recall.map_or_else(|| "n/a".to_string(), pct)
}

/// `✓`, `✗` or `n/a` for an optional check.
fn check_cell(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "✓",
        Some(false) => "✗",
        None => "n/a",
    }
}

/// Formats an optional alignment score.
fn align_cell(alignment: Option<f32>) -> String {
    alignment.map_or_else(|| "n/a".to_string(), |a| format!("{a:.3}"))
}

/// Renders the report as `GitHub`-flavoured markdown: a summary table, a
/// per-paper table, the paper metadata mismatches (wrong title or DOI, one
/// line each), then the unmatched truth keys (at most ten per paper).
pub fn render_markdown(report: &CorpusReport) -> String {
    let s = &report.summary;
    let mut out = String::new();
    out.push_str("# Evaluation report\n\n");
    let _ = writeln!(out, "- Backend: `{}`", cell(&report.backend));
    let _ = writeln!(out, "- Host: `{}`", cell(&report.host));
    let _ = write!(out, "- Generated (unix): {}\n\n", report.generated_unix);

    out.push_str("## Summary\n\n");
    out.push_str("| Metric | Value |\n");
    out.push_str("| --- | --- |\n");
    let _ = writeln!(out, "| Papers | {} |", s.papers);
    let _ = writeln!(out, "| Failed | {} |", s.failed);
    let _ = writeln!(
        out,
        "| Reference count exact | {} |",
        pct(s.ref_count_exact_rate)
    );
    let _ = writeln!(out, "| Reference recall | {} |", pct(s.ref_recall));
    let _ = writeln!(out, "| Reference precision | {} |", pct(s.ref_precision));
    let _ = writeln!(out, "| DOI accuracy | {} |", pct(s.doi_accuracy));
    let _ = writeln!(
        out,
        "| DOI accuracy (of printed DOIs) | {} |",
        pct(s.doi_accuracy_printed)
    );
    let _ = writeln!(out, "| Year accuracy | {} |", pct(s.year_accuracy));
    let _ = writeln!(out, "| Title accuracy | {} |", pct(s.title_accuracy));
    let _ = writeln!(
        out,
        "| Paper title accuracy | {} |",
        pct(s.paper_title_accuracy)
    );
    let _ = writeln!(
        out,
        "| Paper author recall | {} |",
        pct(s.paper_author_recall)
    );
    let _ = writeln!(
        out,
        "| Paper author precision | {} |",
        pct(s.paper_author_precision)
    );
    let doi_checked = report
        .papers
        .iter()
        .filter(|p| !is_failed(p) && p.paper_doi_correct.is_some())
        .count();
    let doi_cell = if doi_checked == 0 {
        "n/a (no source states a DOI)".to_string()
    } else {
        pct(s.paper_doi_accuracy)
    };
    let _ = writeln!(
        out,
        "| Paper DOI accuracy (of papers whose source states a DOI) | {doi_cell} |"
    );
    let _ = writeln!(
        out,
        "| Marker resolution (precision-like, resolved/extracted) | {} |",
        pct(s.marker_resolution_rate)
    );
    let _ = writeln!(
        out,
        "| Marker recall (resolved/truth cite commands) | {} |",
        pct(s.marker_recall)
    );
    let _ = writeln!(
        out,
        "| Mean body alignment | {} |",
        align_cell(s.mean_body_alignment)
    );
    let _ = writeln!(out, "| p50 ms per chunk | {:.1} |", s.p50_ms_per_chunk);
    let _ = writeln!(out, "| p95 ms per chunk | {:.1} |", s.p95_ms_per_chunk);
    let _ = write!(
        out,
        "| Target ms per chunk | {:.1} |\n\n",
        s.target_ms_per_chunk
    );

    out.push_str("## Stage timings (mean ms per document)\n\n");
    out.push_str("| Stage | Mean ms |\n");
    out.push_str("| --- | --- |\n");
    let stages = [
        ("acquire", s.mean_acquire_ms),
        ("parse", s.mean_parse_ms),
        ("order", s.mean_order_ms),
        ("metadata", s.mean_metadata_ms),
        ("citations", s.mean_citations_ms),
        ("write", s.mean_write_ms),
    ];
    let mut stage_total = 0.0_f64;
    for (name, mean) in stages {
        stage_total += mean;
        let _ = writeln!(out, "| {name} | {mean:.1} |");
    }
    let _ = write!(out, "| total | {stage_total:.1} |\n\n");

    out.push_str("## Papers\n\n");
    out.push_str(
        "| id | status | pages | refs truth/extracted/matched | count exact | ext/truth | \
         doi c/t/printed | year c/t | markers resolved/extracted | truth cites | \
         marker recall | align | ms/chunk | warnings | title ✓/✗ | authors c/t | \
         paper doi ✓/✗/n/a |\n",
    );
    out.push_str(
        "| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | \
         --- | --- | --- |\n",
    );
    for p in &report.papers {
        let exact = if p.ref_count_exact { "✓" } else { "✗" };
        let _ = writeln!(
            out,
            "| {} | {} | {} | {}/{}/{} | {} | {:.2} | {}/{}/{} | {}/{} | {}/{} | {} | {} | {} | \
             {:.1} | {} | {} | {}/{} | {} |",
            cell(&p.id),
            cell(&p.status),
            p.pages,
            p.truth_refs,
            p.extracted_refs,
            p.matched_refs,
            exact,
            p.over_segmentation,
            p.doi_correct,
            p.doi_truth,
            p.doi_printed,
            p.year_correct,
            p.year_truth,
            p.resolved_markers,
            p.extracted_markers,
            p.truth_cite_commands,
            recall_cell(p.marker_recall),
            align_cell(p.body_alignment),
            p.ms_per_chunk,
            p.warnings,
            check_cell(p.paper_title_correct),
            p.authors_correct,
            p.authors_truth,
            check_cell(p.paper_doi_correct),
        );
    }

    out.push_str("\n## Paper metadata mismatches\n\n");
    let mut any_mismatch = false;
    for p in &report.papers {
        for line in &p.paper_metadata_mismatches {
            any_mismatch = true;
            let _ = writeln!(out, "- {}: {}", cell(&p.id), cell(line));
        }
    }
    if !any_mismatch {
        out.push_str("- none\n");
    }

    out.push_str("\n## Unmatched truth keys\n\n");
    let mut any = false;
    for p in &report.papers {
        if p.unmatched_truth_keys.is_empty() {
            continue;
        }
        any = true;
        let shown: Vec<String> = p
            .unmatched_truth_keys
            .iter()
            .take(UNMATCHED_KEYS_SHOWN)
            .map(|k| format!("`{}`", cell(k)))
            .collect();
        let more = p
            .unmatched_truth_keys
            .len()
            .saturating_sub(UNMATCHED_KEYS_SHOWN);
        let _ = write!(out, "- {}: {}", cell(&p.id), shown.join(", "));
        if more > 0 {
            let _ = write!(out, " (+{more} more)");
        }
        out.push('\n');
    }
    if !any {
        out.push_str("- none\n");
    }
    out
}

/// Everything needed to diagnose one paper's reference parsing offline.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PaperDump {
    pub id: String,
    /// `GroundTruth::method`: `bbl`, `bib-cited` or `bib-all`.
    pub truth_method: String,
    pub truth: Vec<TruthReference>,
    pub extracted: Vec<ReferenceEntry>,
    pub matches: Vec<RefMatch>,
    pub unmatched_truth_keys: Vec<String>,
    /// `ReferenceEntry::index` values that matched no truth reference.
    pub spurious_extracted: Vec<u32>,
    pub markers: Vec<CitationMarker>,
    /// Document-level warnings.
    pub warnings: Vec<String>,
    /// `(page, warning)` for every page warning.
    pub page_warnings: Vec<(u32, String)>,
    pub timings: StageTimings,
    /// Pages actually extracted.
    pub pages: u32,
    /// Page text from the detected reference heading line to the end of the
    /// document, pages joined by [`DUMP_PAGE_SEPARATOR`], capped at
    /// [`REFERENCE_TEXT_CAP`] bytes; empty when no heading was found.
    pub reference_section_text: String,
    /// The extracted paper metadata (`ExtractionResult::metadata`).
    #[serde(default)]
    pub metadata: Metadata,
    /// Title, authors and identifiers the `LaTeX` source states
    /// (`GroundTruth::paper`).
    #[serde(default)]
    pub paper_truth: TruthPaper,
}

/// Reference heading on a single text line, for pages without `lines`.
fn text_heading_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^\s*(\d+\.?\s*)?(References|Bibliography|Works Cited|REFERENCES)\s*$")
            .expect("valid regex")
    })
}

/// Byte offset of line `line_index` of `page.lines` inside `page.text`,
/// found by walking the line texts in order; `None` when it cannot be found.
fn line_byte_offset(page: &PageText, line_index: usize) -> Option<usize> {
    let mut cursor = 0_usize;
    for (i, line) in page.lines.iter().enumerate().take(line_index + 1) {
        let needle = line.text.trim();
        if needle.is_empty() {
            continue;
        }
        let rel = page.text.get(cursor..).and_then(|rest| rest.find(needle));
        match rel {
            Some(rel) => {
                let start = cursor + rel;
                if i == line_index {
                    return Some(start);
                }
                cursor = start + needle.len();
            }
            None if i == line_index => return None,
            None => {}
        }
    }
    None
}

/// `(position in pages, byte offset in its text)` of the last reference
/// heading: via `citations::find_reference_section` over `page.lines`, else
/// the last matching line of `page.text`.
fn reference_start(pages: &[PageText]) -> Option<(usize, usize)> {
    if let Some(section) = find_reference_section(pages) {
        let pos = pages.iter().position(|p| p.page == section.first_page)?;
        let offset = line_byte_offset(&pages[pos], section.first_line)
            .or_else(|| pages[pos].text.find(section.heading.as_str()))
            .unwrap_or(0);
        return Some((pos, offset));
    }
    let mut found: Option<(usize, usize)> = None;
    for (pos, page) in pages.iter().enumerate() {
        let mut offset = 0_usize;
        for line in page.text.split('\n') {
            if text_heading_re().is_match(line) {
                found = Some((pos, offset));
            }
            offset += line.len() + 1;
        }
    }
    found
}

/// Truncates `s` to at most `cap` bytes on a char boundary.
fn truncate_on_char_boundary(s: &mut String, cap: usize) {
    if s.len() <= cap {
        return;
    }
    let mut end = cap;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
}

/// Page text from the last reference heading to the end of the document,
/// capped at [`REFERENCE_TEXT_CAP`] bytes; empty when there is no heading.
pub fn reference_section_text(pages: &[PageText]) -> String {
    let Some((pos, offset)) = reference_start(pages) else {
        return String::new();
    };
    let mut out = String::new();
    for (i, page) in pages.iter().enumerate().skip(pos) {
        if i == pos {
            out.push_str(page.text.get(offset..).unwrap_or(""));
        } else {
            out.push_str(DUMP_PAGE_SEPARATOR);
            out.push_str(&page.text);
        }
        if out.len() > REFERENCE_TEXT_CAP {
            break;
        }
    }
    truncate_on_char_boundary(&mut out, REFERENCE_TEXT_CAP);
    out
}

/// Collects the truth, the extracted entries, the pairing from `eval`, the
/// markers, warnings, timings, reference-section text, and the extracted
/// and truth paper metadata for one paper.
pub fn dump_paper(
    id: &str,
    result: &ExtractionResult,
    truth: &GroundTruth,
    eval: &PaperEval,
) -> PaperDump {
    let page_warnings: Vec<(u32, String)> = result
        .pages
        .iter()
        .flat_map(|page| page.warnings.iter().map(|w| (page.page, w.clone())))
        .collect();
    PaperDump {
        id: id.to_string(),
        truth_method: truth.method.clone(),
        truth: truth.references.clone(),
        extracted: result.references.clone(),
        matches: eval.matches.clone(),
        unmatched_truth_keys: eval.unmatched_truth_keys.clone(),
        spurious_extracted: eval.spurious_extracted.clone(),
        markers: result.citations.clone(),
        warnings: result.warnings.clone(),
        page_warnings,
        timings: result.timings,
        pages: result.pages.len() as u32,
        reference_section_text: reference_section_text(&result.pages),
        metadata: result.metadata.clone(),
        paper_truth: truth.paper.clone(),
    }
}

/// File-name-safe form of a paper id: every character other than ASCII
/// letters, digits, `-`, `_` and `.` becomes `_` (`arxiv:2108.04588` ->
/// `arxiv_2108.04588`); an empty result becomes `paper`.
pub fn safe_file_stem(id: &str) -> String {
    let stem: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if stem.is_empty() || stem.chars().all(|c| c == '.') {
        "paper".to_string()
    } else {
        stem
    }
}

/// Writes `dump` as pretty JSON to `<dir>/<safe id>.json`, creating `dir`
/// when missing, and returns the path written.
pub fn write_dump(dir: &Path, dump: &PaperDump) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{}.json", safe_file_stem(&dump.id)));
    let mut json = serde_json::to_string_pretty(dump).map_err(std::io::Error::other)?;
    json.push('\n');
    std::fs::write(&path, json)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::latex_refs::{TruthCitations, TruthPaper, TruthSource};
    use crate::schema::{
        Author, BackendIdentity, ChunkResult, CitationMarker, ContentHash, Document, Line,
        Metadata, PageText, SCHEMA_VERSION, StageTimings, Status,
    };

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-5
    }

    fn truth_ref(key: &str) -> TruthReference {
        TruthReference {
            key: key.to_string(),
            label: None,
            text: String::new(),
            authors: Vec::new(),
            title: None,
            year: None,
            doi: None,
            arxiv_id: None,
            source: TruthSource::Bbl,
        }
    }

    fn extracted(index: u32) -> ReferenceEntry {
        ReferenceEntry {
            index,
            ..ReferenceEntry::default()
        }
    }

    fn page(number: u32, text: &str) -> PageText {
        let mut p = PageText::new(number, 612.0, 792.0, 0);
        p.text = text.to_string();
        p
    }

    fn sample_result(
        references: Vec<ReferenceEntry>,
        citations: Vec<CitationMarker>,
    ) -> ExtractionResult {
        ExtractionResult {
            schema_version: SCHEMA_VERSION,
            document: Document {
                hash: ContentHash("abc".to_string()),
                size: 1,
                pages: 2,
                sources: Vec::new(),
            },
            backend: BackendIdentity {
                name: "lopdf".to_string(),
                version: "0.45".to_string(),
                config_digest: String::new(),
            },
            status: Status::Complete,
            pages: vec![
                page(1, "The quick brown fox"),
                page(2, "jumps over the lazy dog."),
            ],
            chunks: vec![ChunkResult {
                chunk_index: 0,
                first_page: 1,
                last_page: 2,
                status: Status::Complete,
                text_sha256: String::new(),
                ms: 14.0,
            }],
            metadata: Metadata::default(),
            references,
            citations,
            warnings: vec!["one warning".to_string()],
            timings: StageTimings {
                acquire_ms: 1.0,
                parse_ms: 10.0,
                order_ms: 4.0,
                metadata_ms: 2.0,
                citations_ms: 3.0,
                write_ms: 0.0,
            },
        }
    }

    fn truth_with(references: Vec<TruthReference>, body_text: &str) -> GroundTruth {
        GroundTruth {
            references,
            citations: TruthCitations {
                cite_commands: 5,
                cited_keys: Vec::new(),
                nocite_keys: Vec::new(),
                nocite_all: false,
            },
            method: "bbl".to_string(),
            body_text: body_text.to_string(),
            paper: TruthPaper::default(),
        }
    }

    #[test]
    fn normalize_title_keeps_alphanumerics_and_single_spaces() {
        assert_eq!(
            normalize_title("  Deep   Learning: A Survey! "),
            "deep learning a survey"
        );
        assert_eq!(normalize_title("Élan-Vital (2nd ed.)"), "élan vital 2nd ed");
        assert_eq!(normalize_title("---"), "");
        assert_eq!(normalize_title(""), "");
    }

    #[test]
    fn word_alignment_identical_is_one() {
        let text = "The quick brown fox jumps over the lazy dog";
        assert!(close(word_alignment(text, text), 1.0));
        assert!(close(
            word_alignment("The Quick, brown fox!", "the quick brown fox"),
            1.0
        ));
    }

    #[test]
    fn word_alignment_disjoint_is_zero() {
        assert!(close(
            word_alignment("alpha beta gamma", "delta epsilon zeta"),
            0.0
        ));
        assert!(close(word_alignment("", "some words here"), 0.0));
        assert!(close(word_alignment("some words here", ""), 0.0));
    }

    #[test]
    fn word_alignment_empty_both_is_one() {
        assert!(close(word_alignment("", ""), 1.0));
        assert!(close(word_alignment("...", "!!!"), 1.0));
    }

    #[test]
    fn word_alignment_partial_overlap() {
        let a = word_alignment("the quick brown fox jumps", "the quick brown cat sleeps");
        assert!(close(a, 0.6), "got {a}");
        assert!((0.5..0.67).contains(&a), "got {a}");
    }

    #[test]
    fn lcs_len_two_row_dp() {
        assert_eq!(lcs_len(&[1, 2, 3, 4], &[2, 4]), 2);
        assert_eq!(lcs_len(&[1, 2, 3], &[3, 2, 1]), 1);
        assert_eq!(lcs_len(&[], &[1]), 0);
        assert_eq!(lcs_len(&[7, 8, 9], &[7, 8, 9]), 3);
    }

    #[test]
    fn sample_evenly_caps_length_and_keeps_order() {
        let tokens: Vec<u32> = (0..100).collect();
        let sampled = sample_evenly(tokens.clone(), 10);
        assert_eq!(sampled, vec![0, 10, 20, 30, 40, 50, 60, 70, 80, 90]);
        assert_eq!(sample_evenly(tokens.clone(), 100), tokens);
        assert_eq!(sample_evenly(vec![1, 2, 3], 5), vec![1, 2, 3]);
    }

    #[test]
    fn normalize_doi_is_symmetric_over_prefixes_case_and_punctuation() {
        let bare = "10.1145/3292500.3330701";
        for printed in [
            "10.1145/3292500.3330701",
            "DOI: 10.1145/3292500.3330701",
            "doi:10.1145/3292500.3330701.",
            "https://doi.org/10.1145/3292500.3330701",
            "https://www.doi.org/10.1145/3292500.3330701)",
            "http://dx.doi.org/10.1145/3292500.3330701;",
            "doi: https://doi.org/10.1145/3292500.3330701",
            "DOI 10.1145/3292500.3330701]",
        ] {
            assert_eq!(normalize_doi(printed), bare, "{printed}");
        }
        assert_eq!(
            normalize_doi("10.1109/TPAMI.2020.1234567"),
            "10.1109/tpami.2020.1234567"
        );
        let upper = "DOI:10.1109/TPAMI.2020.1234567".to_string();
        let lower = "https://doi.org/10.1109/tpami.2020.1234567".to_string();
        assert!(doi_equal(Some(&upper), Some(&lower)));
        assert!(doi_equal(Some(&lower), Some(&upper)));
        let other = "10.1109/TPAMI.2020.7654321".to_string();
        assert!(!doi_equal(Some(&upper), Some(&other)));
        assert!(!doi_equal(Some(&upper), None));
        // Without a `10.NNNN/` the old prefix stripping still applies.
        assert_eq!(normalize_doi("doi:ABC."), "abc");
    }

    #[test]
    fn placeholder_dois_are_recognised() {
        assert!(is_placeholder_doi("10.1145/nnnnnnn.nnnnnnn"));
        assert!(is_placeholder_doi(
            "https://doi.org/10.1145/XXXXXXX.XXXXXXX"
        ));
        assert!(is_placeholder_doi("10.1145/1122445.1122456"));
        assert!(is_placeholder_doi("10.475/123_4"));
        assert!(!is_placeholder_doi("10.1145/3292500.3330701"));
        assert!(!is_placeholder_doi("10.1038/nature14539"));
    }

    #[test]
    fn normalizers_strip_prefixes_and_versions() {
        assert_eq!(normalize_doi("https://doi.org/10.1000/ABC."), "10.1000/abc");
        assert_eq!(normalize_doi("doi:10.1000/xyz"), "10.1000/xyz");
        assert_eq!(normalize_arxiv("arXiv:2101.00001v2"), "2101.00001");
        assert_eq!(
            normalize_arxiv("https://arxiv.org/abs/hep-th/9901001v3"),
            "hep-th/9901001"
        );
        assert_eq!(normalize_arxiv("2101.00001"), "2101.00001");
    }

    #[test]
    fn surname_handles_common_shapes() {
        assert_eq!(surname("Smith, J."), "smith");
        assert_eq!(surname("J. A. Smith"), "smith");
        assert_eq!(surname("Smith AB"), "smith");
        assert_eq!(surname("van der Berg, J."), "berg");
        assert_eq!(surname("Müller"), "muller");
        assert_eq!(surname("Løvborg, Ø."), "lovborg");
        assert_eq!(surname("Straße"), "strasse");
        assert_eq!(surname(""), "");
    }

    #[test]
    fn match_references_by_each_method() {
        let mut ref_a = truth_ref("a");
        ref_a.doi = Some("10.1000/AAA".to_string());
        let mut ref_b = truth_ref("b");
        ref_b.arxiv_id = Some("2101.00001".to_string());
        let mut ref_c = truth_ref("c");
        ref_c.title = Some("A Study of Things".to_string());
        let mut ref_d = truth_ref("d");
        ref_d.authors = vec!["Müller, K.".to_string()];
        ref_d.year = Some(2020);
        let mut ref_f = truth_ref("f");
        ref_f.title = Some("Alpha beta gamma delta epsilon".to_string());
        let mut ref_g = truth_ref("g");
        ref_g.title = Some("Completely unrelated".to_string());

        let mut e1 = extracted(1);
        e1.doi = Some("https://doi.org/10.1000/aaa".to_string());
        let mut e2 = extracted(2);
        e2.arxiv_id = Some("arXiv:2101.00001v3".to_string());
        let mut e3 = extracted(3);
        e3.title = Some("A study of things.".to_string());
        let mut e4 = extracted(4);
        e4.authors = vec!["K. Muller".to_string()];
        e4.year = Some(2020);
        let mut e5 = extracted(5);
        e5.title = Some("Alpha beta gamma delta".to_string());

        let matches = match_references(
            &[ref_a, ref_b, ref_c, ref_d, ref_f, ref_g],
            &[e1, e2, e3, e4, e5],
        );
        assert_eq!(matches.len(), 6);
        assert_eq!(matches[0].extracted_index, Some(1));
        assert_eq!(matches[0].method, "doi");
        assert_eq!(matches[1].extracted_index, Some(2));
        assert_eq!(matches[1].method, "arxiv");
        assert_eq!(matches[2].extracted_index, Some(3));
        assert_eq!(matches[2].method, "title");
        assert!(close(matches[2].score, 1.0));
        assert_eq!(matches[3].extracted_index, Some(4));
        assert_eq!(matches[3].method, "author-year");
        assert!(close(matches[3].score, 0.75));
        assert_eq!(matches[4].extracted_index, Some(5));
        assert_eq!(matches[4].method, "title");
        assert!(close(matches[4].score, 0.8));
        assert_eq!(matches[5].extracted_index, None);
        assert_eq!(matches[5].method, "none");
        assert_eq!(matches[5].truth_key, "g");
    }

    #[test]
    fn match_references_is_one_to_one() {
        let mut ref_a = truth_ref("a");
        ref_a.authors = vec!["Smith, J.".to_string()];
        ref_a.year = Some(2019);
        let mut ref_b = truth_ref("b");
        ref_b.authors = vec!["Smith, K.".to_string()];
        ref_b.year = Some(2019);
        let mut e1 = extracted(1);
        e1.authors = vec!["J. Smith".to_string()];
        e1.year = Some(2019);

        let matches = match_references(&[ref_a, ref_b], &[e1]);
        assert_eq!(matches[0].extracted_index, Some(1));
        assert_eq!(matches[1].extracted_index, None);
        let used: Vec<u32> = matches.iter().filter_map(|m| m.extracted_index).collect();
        assert_eq!(used, vec![1]);
    }

    #[test]
    fn match_references_empty_inputs() {
        assert!(match_references(&[], &[]).is_empty());
        let matches = match_references(&[truth_ref("a")], &[]);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].method, "none");
        assert!(match_references(&[], &[extracted(1)]).is_empty());
    }

    #[test]
    fn match_references_text_pass_leaves_keyed_matches_alone() {
        // Every truth text is similar enough to every extracted raw for the
        // text pass (Jaccard 0.8); the keyed passes must still win.
        let shared = "Shared words that every entry has in common";
        let mut ref_a = truth_ref("a");
        ref_a.doi = Some("10.1000/AAA".to_string());
        let mut ref_b = truth_ref("b");
        ref_b.arxiv_id = Some("2101.00001".to_string());
        let mut ref_c = truth_ref("c");
        ref_c.title = Some("A Study of Things".to_string());
        let mut ref_d = truth_ref("d");
        ref_d.authors = vec!["Müller, K.".to_string()];
        ref_d.year = Some(2020);
        let mut ref_g = truth_ref("g");
        ref_g.title = Some("Completely unrelated".to_string());
        let mut truth = vec![ref_d, ref_c, ref_b, ref_a, ref_g];
        for truth_ref in &mut truth {
            truth_ref.text = format!("{shared} {}", truth_ref.key);
        }

        let mut e1 = extracted(1);
        e1.doi = Some("https://doi.org/10.1000/aaa".to_string());
        let mut e2 = extracted(2);
        e2.arxiv_id = Some("arXiv:2101.00001v3".to_string());
        let mut e3 = extracted(3);
        e3.title = Some("A study of things.".to_string());
        let mut e4 = extracted(4);
        e4.authors = vec!["K. Muller".to_string()];
        e4.year = Some(2020);
        let mut ext = vec![e1, e2, e3, e4];
        for entry in &mut ext {
            entry.raw = format!("[{}] {shared}", entry.index);
        }

        let matches = match_references(&truth, &ext);
        let got: Vec<(Option<u32>, &str)> = matches
            .iter()
            .map(|m| (m.extracted_index, m.method.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                (Some(4), "author-year"),
                (Some(3), "title"),
                (Some(2), "arxiv"),
                (Some(1), "doi"),
                (None, "none"),
            ]
        );
    }

    #[test]
    fn match_references_text_pass_recovers_untitled_truth() {
        // IEEE `.bbl` truth without `\newblock` used to have no title or
        // authors; only the entry text can pair it.
        let mut ref_a = truth_ref("zaitsev2015motion");
        ref_a.text = "M. Zaitsev, J. Maclaren, and M. Herbst, \"Motion artifacts in MRI: \
                      A review,\" NMR in Biomedicine, vol. 28, no. 7, pp. 911–935, 2015."
            .to_string();
        let mut ref_b = truth_ref("other");
        ref_b.text = "Q. Nobody, Nothing alike at all, 1999.".to_string();
        let mut e1 = extracted(1);
        e1.raw = "[1] K. P. Pruessmann, M. Weiger, and P. Boesiger, “SENSE: Sensitivity \
                  encoding for fast MRI,” Magnetic Resonance in Medicine, 1999."
            .to_string();
        let mut e2 = extracted(2);
        e2.raw = "[2] M. Zaitsev, J. Maclaren, and M. Herbst, “Motion artifacts in MRI: A \
                  review,” NMR in Biomed- icine, vol. 28, no. 7, pp. 911– 935, 2015."
            .to_string();

        let matches = match_references(&[ref_a, ref_b], &[e1, e2]);
        assert_eq!(matches[0].extracted_index, Some(2));
        assert_eq!(matches[0].method, "text");
        // Same words after the label, hyphen and range spacing are normalised.
        assert!(close(matches[0].score, 1.0));
        assert_eq!(matches[1].extracted_index, None);
        assert_eq!(matches[1].method, "none");
    }

    #[test]
    fn match_references_text_pass_threshold_and_ties() {
        let mut ref_a = truth_ref("a");
        ref_a.text = "alpha beta gamma delta epsilon".to_string();
        ref_a.authors = vec!["J. Smith".to_string()];
        ref_a.year = Some(2021);
        // 3 shared of 7 words: Jaccard 0.43, below the minimum.
        let mut ref_b = truth_ref("b");
        ref_b.text = "one two three four five".to_string();
        let mut e1 = extracted(1);
        e1.raw = "alpha beta gamma delta epsilon".to_string();
        e1.authors = vec!["K. Jones".to_string()];
        e1.year = Some(2020);
        let mut e2 = extracted(2);
        e2.raw = "alpha beta gamma delta epsilon".to_string();
        e2.authors = vec!["J. Smith".to_string()];
        e2.year = Some(2020);
        let mut e3 = extracted(3);
        e3.raw = "one two three six seven".to_string();

        let matches = match_references(&[ref_a, ref_b], &[e1, e2, e3]);
        // Equal similarity: the entry with the same first-author surname wins.
        assert_eq!(matches[0].extracted_index, Some(2));
        assert_eq!(matches[0].method, "text");
        assert_eq!(matches[1].extracted_index, None);
    }

    #[test]
    fn match_references_orders_duplicate_truth_entries() {
        // Two `.bib` entries for the same paper (2412.06210 `Khan2020Federated`
        // and `khan2021federated`) and the two printed entries: the DOI of
        // the first printed one is truncated, so the DOI pass pairs the first
        // truth entry with the second printed entry and the title pass pairs
        // them crosswise.
        let title = "Federated Learning for Internet of Things".to_string();
        let mut ref_a = truth_ref("Khan2020Federated");
        ref_a.title = Some(title.clone());
        ref_a.doi = Some("10.1109/COMST.2021.3090430".to_string());
        ref_a.authors = vec!["L. U. Khan".to_string()];
        ref_a.year = Some(2020);
        let mut ref_b = truth_ref("khan2021federated");
        ref_b.title = Some(title.clone());
        ref_b.doi = Some("10.1109/COMST.2021.3090430".to_string());
        ref_b.authors = vec!["Latif U. Khan".to_string()];
        ref_b.year = Some(2021);
        let mut e10 = extracted(10);
        e10.title = Some(title.clone());
        e10.doi = Some("10.1109/COMST".to_string());
        e10.authors = vec!["L. U. Khan".to_string()];
        e10.year = Some(2020);
        let mut e11 = extracted(11);
        e11.title = Some(title);
        e11.doi = Some("10.1109/COMST.2021.3090430".to_string());
        e11.authors = vec!["Latif U. Khan".to_string()];
        e11.year = Some(2021);

        let matches = match_references(&[ref_a, ref_b], &[e10, e11]);
        assert_eq!(matches[0].extracted_index, Some(10));
        assert_eq!(matches[0].method, "title");
        assert_eq!(matches[1].extracted_index, Some(11));
        assert_eq!(matches[1].method, "doi");
    }

    #[test]
    fn match_references_keeps_same_title_entries_with_different_dois() {
        let mut ref_a = truth_ref("a");
        ref_a.title = Some("Same Title".to_string());
        ref_a.doi = Some("10.1/a".to_string());
        let mut ref_b = truth_ref("b");
        ref_b.title = Some("Same Title".to_string());
        ref_b.doi = Some("10.1/b".to_string());
        let mut e1 = extracted(1);
        e1.title = Some("Same Title".to_string());
        e1.doi = Some("10.1/b".to_string());
        let mut e2 = extracted(2);
        e2.title = Some("Same Title".to_string());
        e2.doi = Some("10.1/a".to_string());

        let matches = match_references(&[ref_a, ref_b], &[e1, e2]);
        assert_eq!(matches[0].extracted_index, Some(2));
        assert_eq!(matches[1].extracted_index, Some(1));
        assert!(matches.iter().all(|m| m.method == "doi"));
    }

    #[test]
    fn evaluate_counts_matches_and_fields() {
        let mut ref_a = truth_ref("a");
        ref_a.doi = Some("10.1/a".to_string());
        ref_a.year = Some(2020);
        ref_a.title = Some("Alpha Beta Gamma".to_string());
        let mut ref_b = truth_ref("b");
        ref_b.arxiv_id = Some("2101.00001".to_string());
        ref_b.year = Some(2019);
        ref_b.title = Some("Delta".to_string());
        let mut ref_c = truth_ref("c");
        ref_c.authors = vec!["Nobody, X.".to_string()];
        ref_c.year = Some(2018);
        ref_c.title = Some("Unmatched Thing".to_string());

        let mut e1 = extracted(1);
        e1.doi = Some("10.1/A".to_string());
        e1.year = Some(2020);
        e1.title = Some("Alpha beta gamma!".to_string());
        let mut e2 = extracted(2);
        e2.arxiv_id = Some("2101.00001v2".to_string());
        e2.year = Some(2018);
        let mut e3 = extracted(3);
        e3.authors = vec!["Zed, Q.".to_string()];
        e3.year = Some(1999);
        e3.title = Some("Something Else".to_string());

        let citations = vec![
            CitationMarker {
                page: 1,
                offset: 4,
                text: "[1]".to_string(),
                targets: vec![1],
            },
            CitationMarker {
                page: 2,
                offset: 0,
                text: "[9]".to_string(),
                targets: Vec::new(),
            },
        ];
        let result = sample_result(vec![e1, e2, e3], citations);
        let truth = truth_with(
            vec![ref_a, ref_b, ref_c],
            "The quick brown fox jumps over the lazy dog",
        );

        let eval = evaluate("arxiv:0000.00000", &result, &truth);
        assert_eq!(eval.id, "arxiv:0000.00000");
        assert_eq!(eval.status, "complete");
        assert_eq!(eval.pages, 2);
        assert_eq!(eval.truth_method, "bbl");
        assert_eq!(eval.truth_refs, 3);
        assert_eq!(eval.extracted_refs, 3);
        assert_eq!(eval.matched_refs, 2);
        assert!(eval.ref_count_exact);
        assert_eq!(eval.unmatched_truth_keys, vec!["c".to_string()]);
        assert_eq!(eval.spurious_extracted, vec![3]);
        // Field denominators count matched truth refs only (a and b).
        assert_eq!(eval.doi_truth, 1);
        assert_eq!(eval.doi_correct, 1);
        assert_eq!(eval.year_truth, 2);
        assert_eq!(eval.year_correct, 1);
        assert_eq!(eval.title_truth, 2);
        assert_eq!(eval.title_correct, 1);
        // Totals count every truth ref carrying the field.
        assert_eq!(eval.doi_truth_total, 1);
        assert_eq!(eval.year_truth_total, 3);
        assert_eq!(eval.title_truth_total, 3);
        assert_eq!(eval.truth_cite_commands, 5);
        assert_eq!(eval.extracted_markers, 2);
        assert_eq!(eval.resolved_markers, 1);
        let marker_recall = eval.marker_recall.expect("truth has cite commands");
        assert!(close(marker_recall, 0.2), "got {marker_recall}");
        assert_eq!(eval.marker_targets, 1);
        let alignment = eval.body_alignment.expect("body text present");
        assert!(close(alignment, 1.0), "got {alignment}");
        assert!((eval.ms_total - 20.0).abs() < 1e-9);
        assert!((eval.ms_per_chunk - 20.0).abs() < 1e-9);
        assert_eq!(eval.chunks, 1);
        assert_eq!(eval.warnings, 1);
        assert_eq!(eval.matches.len(), 3);
        assert_eq!(eval.matches[0].method, "doi");
        assert_eq!(eval.matches[1].method, "arxiv");
        assert_eq!(eval.matches[2].method, "none");
    }

    #[test]
    fn evaluate_field_accuracy_ignores_unmatched_truth_refs() {
        let mut truth_refs: Vec<TruthReference> = Vec::new();
        for i in 0..10_u16 {
            let mut r = truth_ref(&format!("k{i}"));
            r.doi = Some(format!("10.1000/ref{i}"));
            r.year = Some(2000 + i);
            r.title = Some(format!("Distinct title number {i}"));
            truth_refs.push(r);
        }
        let mut e1 = extracted(1);
        e1.doi = Some("10.1000/REF3".to_string());
        e1.year = Some(2003);
        e1.title = Some("Distinct Title Number 3".to_string());
        let result = sample_result(vec![e1], Vec::new());
        let truth = truth_with(truth_refs, "");

        let eval = evaluate("p", &result, &truth);
        assert_eq!(eval.matched_refs, 1);
        assert_eq!(eval.doi_truth, 1);
        assert_eq!(eval.doi_correct, 1);
        assert_eq!(eval.year_truth, 1);
        assert_eq!(eval.year_correct, 1);
        assert_eq!(eval.title_truth, 1);
        assert_eq!(eval.title_correct, 1);
        assert_eq!(eval.doi_truth_total, 10);
        assert_eq!(eval.year_truth_total, 10);
        assert_eq!(eval.title_truth_total, 10);
        assert!(eval.marker_recall.is_some_and(|r| r.abs() < 1e-9));

        let s = summarize(&[eval]);
        assert!(close(s.doi_accuracy, 1.0), "{}", s.doi_accuracy);
        assert!(close(s.year_accuracy, 1.0), "{}", s.year_accuracy);
        assert!(close(s.title_accuracy, 1.0), "{}", s.title_accuracy);
        assert!(close(s.ref_recall, 0.1), "{}", s.ref_recall);
    }

    #[test]
    fn evaluate_without_cite_commands_has_no_marker_recall() {
        let result = sample_result(Vec::new(), Vec::new());
        let mut truth = truth_with(Vec::new(), "");
        truth.citations.cite_commands = 0;
        let eval = evaluate("x", &result, &truth);
        assert!(eval.marker_recall.is_none());
    }

    #[test]
    fn evaluate_without_body_text_has_no_alignment() {
        let result = sample_result(Vec::new(), Vec::new());
        let truth = truth_with(Vec::new(), "");
        let eval = evaluate("x", &result, &truth);
        assert!(eval.body_alignment.is_none());
        assert!(eval.ref_count_exact);
        assert_eq!(eval.matched_refs, 0);
    }

    #[test]
    fn failed_paper_is_zeroed() {
        let p = failed_paper("arxiv:1", "offline");
        assert_eq!(p.status, "failed:offline");
        assert_eq!(p.pages, 0);
        assert_eq!(p.truth_refs, 0);
        assert!(!p.ref_count_exact);
        assert!(p.body_alignment.is_none());
        assert!(p.matches.is_empty());
        assert!(is_failed(&p));
        assert!(p.marker_recall.is_none());
        assert_eq!(p.doi_truth_total, 0);
    }

    fn paper_with(id: &str, ms_per_chunk: f64) -> PaperEval {
        let mut p = failed_paper(id, "");
        p.status = "complete".to_string();
        p.ref_count_exact = true;
        p.ms_per_chunk = ms_per_chunk;
        p.ms_total = ms_per_chunk;
        p.chunks = 1;
        p
    }

    #[test]
    fn summarize_percentiles_and_rates() {
        let mut p1 = paper_with("p1", 50.0);
        p1.truth_refs = 10;
        p1.extracted_refs = 8;
        p1.matched_refs = 8;
        p1.ref_count_exact = false;
        p1.doi_truth = 4;
        p1.doi_correct = 2;
        p1.extracted_markers = 10;
        p1.resolved_markers = 5;
        p1.truth_cite_commands = 20;
        p1.body_alignment = Some(0.8);
        let mut p2 = paper_with("p2", 10.0);
        p2.truth_refs = 10;
        p2.extracted_refs = 12;
        p2.matched_refs = 10;
        p2.ref_count_exact = false;
        p2.body_alignment = Some(0.6);
        // No truth cite commands: excluded from marker recall entirely.
        p2.extracted_markers = 4;
        p2.resolved_markers = 4;
        let mut p3 = paper_with("p3", 30.0);
        p3.truth_refs = 5;
        p3.extracted_refs = 5;
        p3.matched_refs = 5;
        let p4 = paper_with("p4", 20.0);
        let p5 = paper_with("p5", 40.0);
        let mut failed = failed_paper("p6", "boom");
        failed.truth_cite_commands = 100;

        let s = summarize(&[p1, p2, p3, p4, p5, failed]);
        assert_eq!(s.papers, 6);
        assert_eq!(s.failed, 1);
        assert!(
            (s.p50_ms_per_chunk - 30.0).abs() < 1e-9,
            "p50 {}",
            s.p50_ms_per_chunk
        );
        assert!(
            (s.p95_ms_per_chunk - 50.0).abs() < 1e-9,
            "p95 {}",
            s.p95_ms_per_chunk
        );
        assert!((s.target_ms_per_chunk - 30.0).abs() < 1e-9);
        // p3, p4 and p5 have equal truth/extracted counts (5/5, 0/0, 0/0).
        assert!(
            close(s.ref_count_exact_rate, 3.0 / 5.0),
            "{}",
            s.ref_count_exact_rate
        );
        assert!(close(s.ref_recall, 23.0 / 25.0), "{}", s.ref_recall);
        assert!(close(s.ref_precision, 23.0 / 25.0), "{}", s.ref_precision);
        assert!(close(s.doi_accuracy, 0.5), "{}", s.doi_accuracy);
        assert!(close(s.year_accuracy, 0.0));
        assert!(
            close(s.marker_resolution_rate, 9.0 / 14.0),
            "{}",
            s.marker_resolution_rate
        );
        assert!(close(s.marker_recall, 0.25), "{}", s.marker_recall);
        let mean = s.mean_body_alignment.expect("two alignments");
        assert!(close(mean, 0.7), "{mean}");
    }

    #[test]
    fn summarize_empty_and_all_failed() {
        let s = summarize(&[]);
        assert_eq!(s.papers, 0);
        assert!(close(s.ref_recall, 0.0));
        assert!(s.mean_body_alignment.is_none());
        assert!(s.p50_ms_per_chunk.abs() < 1e-9);

        let s = summarize(&[failed_paper("a", "x"), failed_paper("b", "y")]);
        assert_eq!(s.papers, 2);
        assert_eq!(s.failed, 2);
        assert!(close(s.ref_count_exact_rate, 0.0));
    }

    #[test]
    fn render_markdown_has_header_and_rows() {
        let mut p1 = paper_with("arxiv:2108.04588", 12.5);
        p1.truth_refs = 31;
        p1.extracted_refs = 31;
        p1.matched_refs = 30;
        p1.ref_count_exact = true;
        p1.unmatched_truth_keys = (0..12).map(|i| format!("key{i}")).collect();
        p1.body_alignment = Some(0.912_3);
        p1.truth_cite_commands = 8;
        p1.extracted_markers = 6;
        p1.resolved_markers = 4;
        p1.marker_recall = Some(0.5);
        let p2 = failed_paper("arxiv:9999.99999", "offline | no cache\nsecond line");
        let report = build_report("lopdf", "ci-arm64", vec![p1, p2]);
        assert!(report.generated_unix > 0);
        assert_eq!(report.summary.papers, 2);

        let md = render_markdown(&report);
        assert!(md.starts_with("# Evaluation report\n"));
        assert!(md.contains("## Summary\n\n| Metric | Value |\n| --- | --- |\n"));
        assert!(
            md.contains("| id | status | pages | refs truth/extracted/matched | count exact |")
        );
        assert!(md.contains("| truth cites | marker recall | align |"));
        assert!(md.contains(
            "| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |\n"
        ));
        assert!(md.contains("| arxiv:2108.04588 | complete | 0 | 31/31/30 | ✓ | 0.00 | 0/0/0 |"));
        assert!(md.contains("| ext/truth | doi c/t/printed | year c/t |"));
        assert!(md.contains("| DOI accuracy (of printed DOIs) | 0.0% |"));
        assert!(md.contains("## Stage timings (mean ms per document)\n"));
        assert!(md.contains("| 4/6 | 8 | 50.0% | 0.912 | 12.5 | 0 |"));
        assert!(md.contains("| 0/0 | 0 | n/a | n/a | 0.0 | 0 |"));
        assert!(md.contains("| Marker resolution (precision-like, resolved/extracted) | 66.7% |"));
        assert!(md.contains("| Marker recall (resolved/truth cite commands) | 50.0% |"));
        assert!(md.contains("| arxiv:9999.99999 | failed:offline \\| no cache second line | 0 |"));
        assert!(md.contains("## Unmatched truth keys\n"));
        assert!(md.contains("- arxiv:2108.04588: `key0`, `key1`"));
        assert!(md.contains("`key9` (+2 more)\n"));
        assert!(!md.contains("`key10`"));
        assert!(md.contains("| Target ms per chunk | 30.0 |"));

        let rows = md.lines().filter(|l| l.starts_with("| arxiv:")).count();
        assert_eq!(rows, 2);
    }

    #[test]
    fn render_markdown_no_unmatched_keys() {
        let report = build_report("lopdf", "host", vec![paper_with("p", 1.0)]);
        let md = render_markdown(&report);
        assert!(md.ends_with("## Unmatched truth keys\n\n- none\n"));
    }

    #[test]
    fn report_round_trips_json() {
        let report = build_report(
            "lopdf",
            "host",
            vec![paper_with("p", 1.0), failed_paper("q", "e")],
        );
        let json = serde_json::to_string(&report).unwrap();
        let back: CorpusReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back, report);
    }

    fn lined_page(number: u32, lines: &[&str]) -> PageText {
        let mut p = PageText::new(number, 612.0, 792.0, 0);
        p.lines = lines
            .iter()
            .map(|text| Line {
                text: (*text).to_string(),
                bbox: None,
                column: 0,
                spans: Vec::new(),
            })
            .collect();
        p.text = lines.join("\n");
        p
    }

    #[test]
    fn dump_paper_collects_truth_extraction_and_reference_text() {
        let mut ref_a = truth_ref("a");
        ref_a.title = Some("Alpha Title".to_string());
        let ref_b = truth_ref("b");
        let mut e1 = extracted(1);
        e1.title = Some("Alpha title".to_string());
        let e2 = extracted(2);
        let markers = vec![CitationMarker {
            page: 1,
            offset: 0,
            text: "[1]".to_string(),
            targets: vec![1],
        }];
        let mut result = sample_result(vec![e1, e2], markers.clone());
        let mut page2 = lined_page(
            2,
            &[
                "Body text",
                "1 References",
                "[1] A. Smith. Alpha title. 2020.",
            ],
        );
        page2.warnings.push("odd glyph".to_string());
        result.pages = vec![
            lined_page(1, &["Intro", "References", "not the real section"]),
            page2,
            lined_page(3, &["[2] B. Jones. Other. 2021."]),
        ];
        let truth = truth_with(vec![ref_a, ref_b], "");
        let eval = evaluate("arxiv:1234.5678", &result, &truth);
        let dump = dump_paper("arxiv:1234.5678", &result, &truth, &eval);

        assert_eq!(dump.id, "arxiv:1234.5678");
        assert_eq!(dump.truth_method, "bbl");
        assert_eq!(dump.truth.len(), 2);
        assert_eq!(dump.extracted.len(), 2);
        assert_eq!(dump.matches, eval.matches);
        assert_eq!(dump.matches[0].extracted_index, Some(1));
        assert_eq!(dump.unmatched_truth_keys, vec!["b".to_string()]);
        assert_eq!(dump.spurious_extracted, vec![2]);
        assert_eq!(dump.markers, markers);
        assert_eq!(dump.warnings, vec!["one warning".to_string()]);
        assert_eq!(dump.page_warnings, vec![(2, "odd glyph".to_string())]);
        assert!((dump.timings.parse_ms - 10.0).abs() < 1e-9);
        assert_eq!(dump.pages, 3);
        // The last heading wins, the heading line is included and later
        // pages follow after the separator.
        assert_eq!(
            dump.reference_section_text,
            "1 References\n[1] A. Smith. Alpha title. 2020.\n\u{c}\n[2] B. Jones. Other. 2021."
        );
        assert_eq!(dump.metadata, result.metadata);
        assert_eq!(dump.paper_truth, truth.paper);
    }

    #[test]
    fn dump_carries_extracted_metadata_and_paper_truth() {
        let mut result = sample_result(Vec::new(), Vec::new());
        result.metadata.title = Some("Extracted Title".to_string());
        result.metadata.doi = Some("10.1000/abc".to_string());
        result
            .metadata
            .provenance
            .insert("title".to_string(), "first_page:largest-font".to_string());
        let mut truth = truth_with(Vec::new(), "");
        truth.paper = TruthPaper {
            title: Some("True Title".to_string()),
            authors: names(&["Ada Lovelace"]),
            doi: Some("10.1000/ABC".to_string()),
            arxiv_id: None,
        };
        let eval = evaluate("m", &result, &truth);
        let dump = dump_paper("m", &result, &truth, &eval);
        assert_eq!(dump.metadata.title.as_deref(), Some("Extracted Title"));
        assert_eq!(dump.metadata.provenance["title"], "first_page:largest-font");
        assert_eq!(dump.paper_truth.title.as_deref(), Some("True Title"));
        assert_eq!(dump.paper_truth.authors, names(&["Ada Lovelace"]));
        let json = serde_json::to_string(&dump).unwrap();
        let back: PaperDump = serde_json::from_str(&json).unwrap();
        assert_eq!(back, dump);

        // Dumps written before these fields existed still load.
        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let object = value.as_object_mut().unwrap();
        object.remove("metadata");
        object.remove("paper_truth");
        let old: PaperDump = serde_json::from_value(value).unwrap();
        assert_eq!(old.metadata, Metadata::default());
        assert_eq!(old.paper_truth, TruthPaper::default());
    }

    #[test]
    fn reference_section_text_falls_back_to_text_lines() {
        let pages = [
            page(1, "Body\nREFERENCES\n[1] X. Y. Title."),
            page(2, "[2] Z."),
        ];
        assert_eq!(
            reference_section_text(&pages),
            "REFERENCES\n[1] X. Y. Title.\n\u{c}\n[2] Z."
        );
        assert_eq!(reference_section_text(&[page(1, "No heading here")]), "");
        assert_eq!(reference_section_text(&[]), "");
    }

    #[test]
    fn reference_section_text_is_capped_on_char_boundary() {
        let long = "é".repeat(40_000);
        let pages = [lined_page(1, &["References", long.as_str()])];
        let text = reference_section_text(&pages);
        assert!(text.len() <= REFERENCE_TEXT_CAP, "{}", text.len());
        assert!(text.len() >= REFERENCE_TEXT_CAP - 3, "{}", text.len());
        assert!(text.starts_with("References\né"));
    }

    #[test]
    fn safe_file_stem_replaces_unsafe_characters() {
        assert_eq!(safe_file_stem("arxiv:2108.04588"), "arxiv_2108.04588");
        assert_eq!(safe_file_stem("a/b\\c d"), "a_b_c_d");
        assert_eq!(safe_file_stem(""), "paper");
        assert_eq!(safe_file_stem(".."), "paper");
    }

    #[test]
    fn write_dump_writes_pretty_json_that_round_trips() {
        let result = sample_result(vec![extracted(1)], Vec::new());
        let truth = truth_with(vec![truth_ref("a")], "");
        let eval = evaluate("arxiv:2108.04588", &result, &truth);
        let dump = dump_paper("arxiv:2108.04588", &result, &truth, &eval);
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("dumps").join("nested");
        let path = write_dump(&dir, &dump).unwrap();
        assert_eq!(path, dir.join("arxiv_2108.04588.json"));
        let json = std::fs::read_to_string(&path).unwrap();
        assert!(json.contains("\n  \"id\": \"arxiv:2108.04588\""));
        let back: PaperDump = serde_json::from_str(&json).unwrap();
        assert_eq!(back, dump);
    }

    #[test]
    fn evaluate_counts_printed_dois() {
        let mut ref_a = truth_ref("a");
        ref_a.doi = Some("10.1000/ABC.123".to_string());
        ref_a.title = Some("Title A".to_string());
        let mut ref_b = truth_ref("b");
        ref_b.doi = Some("10.2000/xyz".to_string());
        ref_b.title = Some("Title B".to_string());
        let mut ref_c = truth_ref("c");
        ref_c.doi = Some("10.3000/q".to_string());
        let mut ref_d = truth_ref("d");
        ref_d.doi = Some("10.4000/d".to_string());

        let mut e1 = extracted(1);
        e1.title = Some("Title A".to_string());
        let mut e2 = extracted(2);
        e2.title = Some("Title B".to_string());
        let mut e4 = extracted(4);
        e4.doi = Some("10.4000/D".to_string());
        let mut result = sample_result(vec![e1, e2, e4], Vec::new());
        // a is printed but wrapped across a line; c is printed but unmatched;
        // b is not printed; d is not printed but was extracted correctly.
        result.pages[0].text = "see DOI: 10.1000/abc.\n123 and 10.3000/q".to_string();
        let truth = truth_with(vec![ref_a, ref_b, ref_c, ref_d], "");

        let eval = evaluate("p", &result, &truth);
        assert_eq!(eval.matched_refs, 3);
        assert_eq!(eval.doi_truth, 3);
        assert_eq!(eval.doi_correct, 1);
        assert_eq!(eval.doi_printed, 2);

        let s = summarize(&[eval]);
        assert!(close(s.doi_accuracy, 1.0 / 3.0), "{}", s.doi_accuracy);
        assert!(
            close(s.doi_accuracy_printed, 0.5),
            "{}",
            s.doi_accuracy_printed
        );
        let md = render_markdown(&build_report("lopdf", "h", vec![paper_with("q", 1.0)]));
        assert!(md.contains("| DOI accuracy (of printed DOIs) | 0.0% |"));
    }

    #[test]
    fn summarize_means_stage_timings_over_non_failed_papers() {
        let mut p1 = paper_with("p1", 10.0);
        p1.timings = StageTimings {
            acquire_ms: 1.0,
            parse_ms: 10.0,
            order_ms: 2.0,
            metadata_ms: 4.0,
            citations_ms: 6.0,
            write_ms: 0.0,
        };
        let mut p2 = paper_with("p2", 20.0);
        p2.timings = StageTimings {
            acquire_ms: 3.0,
            parse_ms: 30.0,
            order_ms: 4.0,
            metadata_ms: 0.0,
            citations_ms: 2.0,
            write_ms: 1.0,
        };
        let mut failed = failed_paper("p3", "boom");
        failed.timings.parse_ms = 1000.0;

        let s = summarize(&[p1, p2, failed]);
        assert!((s.mean_acquire_ms - 2.0).abs() < 1e-9);
        assert!((s.mean_parse_ms - 20.0).abs() < 1e-9);
        assert!((s.mean_order_ms - 3.0).abs() < 1e-9);
        assert!((s.mean_metadata_ms - 2.0).abs() < 1e-9);
        assert!((s.mean_citations_ms - 4.0).abs() < 1e-9);
        assert!((s.mean_write_ms - 0.5).abs() < 1e-9);
        assert!(summarize(&[]).mean_parse_ms.abs() < 1e-9);

        let result = sample_result(Vec::new(), Vec::new());
        let eval = evaluate("x", &result, &truth_with(Vec::new(), ""));
        assert!((eval.timings.order_ms - 4.0).abs() < 1e-9);

        let report = build_report("lopdf", "h", vec![eval]);
        let md = render_markdown(&report);
        assert!(md.contains(
            "## Stage timings (mean ms per document)\n\n| Stage | Mean ms |\n| --- | --- |\n\
             | acquire | 1.0 |\n| parse | 10.0 |\n| order | 4.0 |\n| metadata | 2.0 |\n\
             | citations | 3.0 |\n| write | 0.0 |\n| total | 20.0 |\n\n## Papers\n"
        ));
    }

    #[test]
    fn evaluate_reports_over_segmentation() {
        let result = sample_result(vec![extracted(1), extracted(2), extracted(3)], Vec::new());
        let truth = truth_with(vec![truth_ref("a"), truth_ref("b")], "");
        let eval = evaluate("over", &result, &truth);
        assert!(
            close(eval.over_segmentation, 1.5),
            "{}",
            eval.over_segmentation
        );

        let none = evaluate("none", &result, &truth_with(Vec::new(), ""));
        assert!(close(none.over_segmentation, 0.0));

        let md = render_markdown(&build_report("lopdf", "h", vec![eval]));
        assert!(md.contains("| over | complete | 2 | 2/3/0 | ✗ | 1.50 | 0/0/0 |"));
    }

    #[test]
    fn render_markdown_paper_rows_match_header_columns() {
        let mut p1 = paper_with("p1", 3.0);
        p1.marker_recall = Some(0.5);
        p1.body_alignment = Some(0.5);
        let p2 = failed_paper("p2", "offline");
        let md = render_markdown(&build_report("lopdf", "h", vec![p1, p2]));
        let papers = md.split("## Papers\n").nth(1).expect("papers section");
        let table: Vec<&str> = papers
            .lines()
            .skip_while(|l| !l.starts_with('|'))
            .take_while(|l| l.starts_with('|'))
            .collect();
        assert_eq!(table.len(), 4, "{table:?}");
        let columns = table[0].matches('|').count();
        assert_eq!(columns, 18);
        for row in &table {
            assert_eq!(row.matches('|').count(), columns, "{row}");
        }
    }

    fn author(name: &str) -> Author {
        Author {
            name: name.to_string(),
            affiliation: None,
            orcid: None,
            email: None,
        }
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|n| (*n).to_string()).collect()
    }

    #[test]
    fn person_key_folds_markers_case_and_comma_form() {
        let gao = Some(("gao".to_string(), Some('l')));
        assert_eq!(person_key("Leo Gao"), gao);
        assert_eq!(person_key("Leo Gao1"), gao);
        assert_eq!(person_key("LEO GAO*"), gao);
        assert_eq!(person_key("Gao, Leo"), gao);
        assert_eq!(person_key("L. Gao"), gao);
        assert_eq!(
            person_key("JIE WANG"),
            Some(("wang".to_string(), Some('j')))
        );
        assert_eq!(
            person_key("Tom Dupré la Tour"),
            Some(("tour".to_string(), Some('t')))
        );
        assert_eq!(person_key("Plato"), Some(("plato".to_string(), None)));
        assert_eq!(person_key("  "), None);
        assert_eq!(person_key("1 2"), None);
    }

    #[test]
    fn author_matches_is_one_to_one() {
        let truth = names(&["Leo Gao", "Lisa Gao", "Jie Wang", "Henk Tillman"]);
        let extracted = names(&["L. Gao", "JIE WANG", "Someone Else"]);
        // One extracted "L. Gao" can pair with only one of the two L. Gaos.
        assert_eq!(author_matches(&truth, &extracted), 2);
        assert_eq!(author_matches(&truth, &[]), 0);
        assert_eq!(author_matches(&[], &extracted), 0);
    }

    #[test]
    fn paper_title_matches_exact_and_fuzzy() {
        let truth = "Scaling Sparse Autoencoders to Many Features";
        assert!(paper_title_matches(
            truth,
            Some("Scaling sparse autoencoders to many features.")
        ));
        assert!(!paper_title_matches(truth, None));
        assert!(!paper_title_matches(truth, Some("---")));
        // 10 of 11 distinct words shared: Jaccard 0.909.
        let long = "one two three four five six seven eight nine ten";
        assert!(paper_title_matches(
            long,
            Some(format!("{long} eleven").as_str())
        ));
        // 8 of 10: Jaccard 0.8, below the paper-title threshold.
        assert!(!paper_title_matches(
            long,
            Some("one two three four five six seven eight")
        ));
    }

    #[test]
    fn evaluate_scores_paper_metadata() {
        let mut result = sample_result(Vec::new(), Vec::new());
        result.metadata.title = Some("Scaling sparse autoencoders to many features.".to_string());
        result.metadata.authors = vec![
            author("Leo Gao1"),
            author("T. Dupre la Tour"),
            author("JIE WANG"),
            author("Someone Else"),
        ];
        result.metadata.doi = Some("https://doi.org/10.1145/1.2".to_string());
        let mut truth = truth_with(Vec::new(), "");
        truth.paper = TruthPaper {
            title: Some("Scaling Sparse Autoencoders to Many Features".to_string()),
            authors: names(&["Leo Gao", "Tom Dupré la Tour", "Henk Tillman", "Jie Wang"]),
            doi: Some("10.1145/1.2".to_string()),
            arxiv_id: None,
        };

        let eval = evaluate("p", &result, &truth);
        assert_eq!(eval.paper_title_correct, Some(true));
        assert_eq!(eval.authors_truth, 4);
        assert_eq!(eval.authors_extracted, 4);
        assert_eq!(eval.authors_correct, 3);
        assert_eq!(eval.paper_doi_correct, Some(true));

        result.metadata.title = Some("A different paper".to_string());
        result.metadata.doi = None;
        let wrong = evaluate("q", &result, &truth);
        assert_eq!(wrong.paper_title_correct, Some(false));
        assert_eq!(wrong.paper_doi_correct, Some(false));

        let unknown = evaluate("r", &result, &truth_with(Vec::new(), ""));
        assert_eq!(unknown.paper_title_correct, None);
        assert_eq!(unknown.paper_doi_correct, None);
        assert_eq!(unknown.authors_truth, 0);
        assert_eq!(unknown.authors_correct, 0);

        let failed = failed_paper("f", "x");
        assert_eq!(failed.paper_title_correct, None);
        assert_eq!(failed.authors_extracted, 0);

        let s = summarize(&[eval, wrong, unknown, failed]);
        assert!(
            close(s.paper_title_accuracy, 0.5),
            "{}",
            s.paper_title_accuracy
        );
        // 6 correct of 8 truth authors (the third paper has none). Precision
        // counts only papers whose source names authors: 6 of 8 extracted
        // (the third paper's 4 names cannot be judged).
        assert!(
            close(s.paper_author_recall, 0.75),
            "{}",
            s.paper_author_recall
        );
        assert!(
            close(s.paper_author_precision, 0.75),
            "{}",
            s.paper_author_precision
        );
    }

    #[test]
    fn render_markdown_shows_paper_metadata() {
        let mut p1 = paper_with("p1", 1.0);
        p1.paper_title_correct = Some(true);
        p1.authors_truth = 4;
        p1.authors_extracted = 5;
        p1.authors_correct = 3;
        let mut p2 = paper_with("p2", 1.0);
        p2.paper_title_correct = Some(false);
        p2.authors_truth = 2;
        p2.authors_extracted = 1;
        p2.authors_correct = 1;
        let p3 = paper_with("p3", 1.0);
        let md = render_markdown(&build_report("lopdf", "h", vec![p1, p2, p3]));
        assert!(md.contains("| warnings | title ✓/✗ | authors c/t | paper doi ✓/✗/n/a |\n"));
        assert!(md.contains("| Paper title accuracy | 50.0% |"));
        assert!(md.contains("| Paper author recall | 66.7% |"));
        assert!(md.contains("| Paper author precision | 66.7% |"));
        let row = |id: &str| -> String {
            md.lines()
                .find(|l| l.starts_with(&format!("| {id} |")))
                .unwrap_or_default()
                .to_string()
        };
        assert!(
            row("p1").ends_with("| 0 | ✓ | 3/4 | n/a |"),
            "{}",
            row("p1")
        );
        assert!(
            row("p2").ends_with("| 0 | ✗ | 1/2 | n/a |"),
            "{}",
            row("p2")
        );
        assert!(
            row("p3").ends_with("| 0 | n/a | 0/0 | n/a |"),
            "{}",
            row("p3")
        );
    }

    #[test]
    fn summarize_and_render_paper_doi_accuracy() {
        let mut p1 = paper_with("p1", 1.0);
        p1.paper_doi_correct = Some(true);
        let mut p2 = paper_with("p2", 1.0);
        p2.paper_doi_correct = Some(true);
        let mut p3 = paper_with("p3", 1.0);
        p3.paper_doi_correct = Some(false);
        // No DOI in the source: outside the denominator.
        let p4 = paper_with("p4", 1.0);
        // Failed papers are excluded even when their DOI check is set.
        let mut failed = failed_paper("p5", "boom");
        failed.paper_doi_correct = Some(false);
        let papers = vec![p1, p2, p3, p4, failed];

        let s = summarize(&papers);
        assert!(
            close(s.paper_doi_accuracy, 2.0 / 3.0),
            "{}",
            s.paper_doi_accuracy
        );
        assert!(close(summarize(&[]).paper_doi_accuracy, 0.0));

        let md = render_markdown(&build_report("lopdf", "h", papers));
        assert!(
            md.contains("| Paper DOI accuracy (of papers whose source states a DOI) | 66.7% |")
        );
        let row = |id: &str| -> String {
            md.lines()
                .find(|l| l.starts_with(&format!("| {id} |")))
                .unwrap_or_default()
                .to_string()
        };
        assert!(row("p1").ends_with("| ✓ |"), "{}", row("p1"));
        assert!(row("p3").ends_with("| ✗ |"), "{}", row("p3"));
        assert!(row("p4").ends_with("| n/a |"), "{}", row("p4"));
    }

    #[test]
    fn evaluate_paper_doi_matches_prefixed_upper_case_extraction() {
        let mut result = sample_result(Vec::new(), Vec::new());
        result.metadata.doi = Some("DOI:10.1145/ABC.123.".to_string());
        let mut truth = truth_with(Vec::new(), "");
        truth.paper.doi = Some("https://doi.org/10.1145/abc.123".to_string());
        let eval = evaluate("p", &result, &truth);
        assert_eq!(eval.paper_doi_correct, Some(true));
        assert!(eval.paper_metadata_mismatches.is_empty());

        // A template placeholder in the source is no DOI at all.
        truth.paper.doi = Some("10.1145/nnnnnnn.nnnnnnn".to_string());
        let placeholder = evaluate("q", &result, &truth);
        assert_eq!(placeholder.paper_doi_correct, None);
        assert!(placeholder.paper_metadata_mismatches.is_empty());
    }

    #[test]
    fn evaluate_lists_paper_metadata_mismatches() {
        let mut result = sample_result(Vec::new(), Vec::new());
        result.metadata.title = Some("Received August 2026".to_string());
        result.metadata.doi = Some("10.1000/wrong".to_string());
        let mut truth = truth_with(Vec::new(), "");
        truth.paper.title = Some("Uncertainty Estimators".to_string());
        truth.paper.doi = Some("10.1000/right".to_string());
        let eval = evaluate("arxiv:1", &result, &truth);
        assert_eq!(
            eval.paper_metadata_mismatches,
            vec![
                "title extracted \"Received August 2026\" vs truth \"Uncertainty Estimators\""
                    .to_string(),
                "doi extracted \"10.1000/wrong\" vs truth \"10.1000/right\"".to_string(),
            ]
        );

        result.metadata.title = None;
        result.metadata.doi = None;
        let missing = evaluate("arxiv:2", &result, &truth);
        assert_eq!(
            missing.paper_metadata_mismatches,
            vec![
                "title extracted none vs truth \"Uncertainty Estimators\"".to_string(),
                "doi extracted none vs truth \"10.1000/right\"".to_string(),
            ]
        );
    }

    #[test]
    fn mismatch_values_are_capped_at_100_chars() {
        let long = "é".repeat(150);
        let shown = mismatch_value(Some(long.as_str()));
        assert_eq!(shown.chars().count(), 100 + 3);
        assert!(shown.ends_with("é…\""));
        assert_eq!(mismatch_value(Some("  ")), "none");
        assert_eq!(mismatch_value(None), "none");
    }

    #[test]
    fn render_markdown_lists_paper_metadata_mismatches() {
        let mut p1 = paper_with("p1", 1.0);
        p1.paper_title_correct = Some(false);
        p1.paper_metadata_mismatches = vec!["title extracted \"A | B\" vs truth \"C\"".to_string()];
        let p2 = paper_with("p2", 1.0);
        let md = render_markdown(&build_report("lopdf", "h", vec![p1, p2]));
        assert!(md.contains(
            "\n## Paper metadata mismatches\n\n- p1: title extracted \"A \\| B\" vs truth \"C\"\n\n## Unmatched truth keys\n"
        ));
        let clean = render_markdown(&build_report("lopdf", "h", vec![paper_with("p", 1.0)]));
        assert!(clean.contains("## Paper metadata mismatches\n\n- none\n"));
    }

    #[test]
    fn render_markdown_paper_doi_accuracy_is_na_without_truth_dois() {
        let md = render_markdown(&build_report("lopdf", "h", vec![paper_with("p", 1.0)]));
        assert!(md.contains(
            "| Paper DOI accuracy (of papers whose source states a DOI) | n/a (no source states a DOI) |"
        ));
    }
}
