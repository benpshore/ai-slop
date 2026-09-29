//! Evaluation harness: scores an [`ExtractionResult`] against the ground truth
//! recovered from a paper's `LaTeX` source (see `crate::latex_refs`).
//!
//! Measured per paper: exact reference-count match, per-entry recall and
//! precision (greedy one-to-one matching by DOI, `arXiv` id, title,
//! first-author surname plus year, then whole-entry text similarity),
//! DOI/year/title field accuracy over the
//! matched pairs only (so segmentation recall is not counted twice), in-text
//! marker resolution, marker precision and key recall (a marker target is
//! correct when its extracted entry is matched to a key the source's `\cite`
//! commands cite), and a word-alignment diagnostic
//! of the body text order. The alignment is reported twice: over body text
//! only (extracted text without the reference lists, with citation markers,
//! caption paragraphs and math-heavy lines removed, against the truth body
//! with math-heavy lines removed) and raw (all page text against the truth
//! body). These are diagnostics on real papers, not the
//! human-checked acceptance protocol.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use regex::Regex;
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use crate::citations::{find_reference_section, find_reference_sections, segment_entries};
use crate::latex_refs::{GroundTruth, TruthPaper, TruthReference};
use crate::schema::{
    CitationMarker, ExtractionResult, Metadata, PageText, ReferenceEntry, StageTimings,
};

/// Product target: warm service time per 20-page chunk, in milliseconds.
pub const TARGET_MS_PER_CHUNK: f64 = 30.0;

/// Safety cap on the tokens per side considered by [`word_alignment`], far
/// above any paper: a longer side is cut to its first this many tokens (with
/// a warning).
pub const MAX_ALIGN_TOKENS: usize = 100_000;

/// Memory budget of the bit-parallel LCS match masks, in `u64` words
/// (64 MB): see [`lcs_bounded`].
const LCS_MAX_WORDS: usize = 8_000_000;

/// Tokens per side kept when the match masks of the whole sides would
/// exceed [`LCS_MAX_WORDS`]: at most this many distinct tokens times
/// `ceil(22_000 / 64) = 344` words is 7.57 M, within the budget.
const LCS_FALLBACK_TOKENS: usize = 22_000;

/// Minimum Jaccard similarity of title words for a fuzzy title match.
const TITLE_JACCARD_MIN: f32 = 0.8;

/// Minimum Jaccard similarity of whole-entry words for the last-resort text
/// match, as `(numerator, denominator)` = 0.6 so the test stays in integers.
const TEXT_JACCARD_MIN: (usize, usize) = (3, 5);

/// Score of an `"author-venue"` match (a title-less extracted entry paired by
/// first author, year and volume/page or venue plus a second author).
const AUTHOR_VENUE_SCORE: f32 = 0.9;

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

/// Byte cap on [`PaperDump::body_text_extracted`] and
/// [`PaperDump::body_text_truth`] (200 kB each).
pub const BODY_TEXT_CAP: usize = 200_000;

/// Separator between pages in [`PaperDump::reference_section_text`] and
/// [`PaperDump::body_text_extracted`].
pub const DUMP_PAGE_SEPARATOR: &str = "\n\u{c}\n";

/// How one truth reference was (or was not) paired with an extracted entry.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RefMatch {
    /// `TruthReference::key` (the `\bibitem` / `.bib` key).
    pub truth_key: String,
    /// `ReferenceEntry::index` of the paired entry, if any.
    pub extracted_index: Option<u32>,
    /// `"doi"`, `"arxiv"`, `"title"`, `"author-venue"`, `"author-year"`,
    /// `"text"` or `"none"`.
    /// When duplicate truth entries swap partners (see [`match_references`])
    /// the method and score travel with the extracted entry.
    pub method: String,
    /// 1.0 for exact DOI/`arXiv`/title matches, the Jaccard value for fuzzy
    /// title and text matches, 0.9 for author-venue matches, 0.75 for
    /// author-year matches, 0.0 when unmatched.
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
    /// `GroundTruth::method`: `bbl`, `bbl+bib`, `bib-cited` or `bib-all`.
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
    /// Matched truth references with a title whose extracted entry has no
    /// title and whose raw text is a title-less journal style (RSC
    /// `…, Nature, 2015, 518, 179–186.`, see [`titleless_raw`]). Counted in
    /// `title_truth`, but left out of the summary's title-accuracy
    /// denominator.
    #[serde(default)]
    pub title_not_applicable: u32,
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
    /// Keys cited by those commands, duplicates kept (each `\cite`
    /// occurrence cites again): `TruthCitations::cited_keys.len()`.
    #[serde(default)]
    pub truth_cited_keys: u32,
    /// Cite commands that print only an author, year, title, date or URL
    /// (`\citeauthor`, `\citeyear`, ...), so no marker can be found for them.
    #[serde(default)]
    pub truth_author_year_only: u32,
    pub extracted_markers: u32,
    /// Markers with at least one resolved target.
    pub resolved_markers: u32,
    /// Sum over markers of `targets.len()`: the references the extracted
    /// markers cite (`[1, 2]` and `[1], [2]` both count 2).
    #[serde(default)]
    pub resolved_targets: u32,
    /// Diagnostic only, the occurrence-based count ratio
    /// `min(1, resolved_targets / truth_cited_keys)`; it does not check that a
    /// target is the right entry. `None` when the source cites no key.
    #[serde(default)]
    pub marker_recall: Option<f32>,
    /// Diagnostic only, the old per-marker count ratio
    /// `resolved_markers / truth_cite_commands`; `None` when the source has
    /// no cite commands. Not clamped: a command printed as `[1], [2]` counts
    /// twice, so it can exceed 1.
    #[serde(default)]
    pub marker_command_ratio: Option<f32>,
    /// Sum of resolved targets over all markers (same as `resolved_targets`).
    pub marker_targets: u32,
    /// Marker targets whose extracted entry is matched (see `matches`) to a
    /// truth key that is cited somewhere in the document.
    #[serde(default)]
    pub marker_targets_correct: u32,
    /// Distinct cited truth keys with at least one correct marker target.
    #[serde(default)]
    pub marker_keys_hit: u32,
    /// Distinct keys in `TruthCitations::cited_keys`.
    #[serde(default)]
    pub marker_keys_cited: u32,
    /// [`word_alignment`] of the extracted body text against the detexed
    /// body, both prepared by `alignment_texts`: the extracted side is the
    /// page text without the reference lists (text after a list, such as
    /// an appendix, is kept), with lines tagged with a non-body role,
    /// citation markers, caption paragraphs and math-heavy lines removed, the truth side has math-heavy lines removed; both
    /// sides are tokenized without math tokens (see `is_math_token`). `None`
    /// when the truth has no body text.
    pub body_alignment: Option<f32>,
    /// [`word_alignment`] of all extracted page text (pages joined by `\n`)
    /// against the unfiltered detexed body; the pre-loop-5 metric. `None`
    /// when the truth has no body text.
    #[serde(default)]
    pub body_alignment_raw: Option<f32>,
    /// Extracted-side word tokens in the `body_alignment` comparison (the
    /// whole side, up to the [`MAX_ALIGN_TOKENS`] safety cap).
    #[serde(default)]
    pub body_words_extracted: u32,
    /// Truth-side word tokens in the `body_alignment` comparison (the whole
    /// side, up to the [`MAX_ALIGN_TOKENS`] safety cap).
    #[serde(default)]
    pub body_words_truth: u32,
    /// Longest common subsequence of the two token sequences behind
    /// `body_alignment`: words matched in order.
    #[serde(default)]
    pub body_words_matched: u32,
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
    /// `title_correct` over `title_truth - title_not_applicable`, summed
    /// over papers: matched entries of a title-less style do not count.
    pub title_accuracy: f32,
    /// Summed `PaperEval::title_not_applicable`: matched entries left out of
    /// the title-accuracy denominator as a title-less style.
    #[serde(default)]
    pub title_not_applicable: u32,
    /// Precision-like: resolved markers over extracted markers.
    pub marker_resolution_rate: f32,
    /// Diagnostic only (`Marker count ratio`): resolved marker targets over
    /// truth cited-key occurrences, each paper's targets capped at its cited
    /// keys, summed over the non-failed papers whose source cites at least
    /// one key. It does not check that a target is the right entry.
    #[serde(default)]
    pub marker_recall: f32,
    /// `marker_targets_correct / resolved_targets`, summed over the
    /// non-failed papers whose source cites at least one key.
    #[serde(default)]
    pub marker_precision: f32,
    /// `marker_keys_hit / marker_keys_cited`, summed over the non-failed
    /// papers whose source cites at least one key.
    #[serde(default)]
    pub marker_key_recall: f32,
    /// Diagnostic only: resolved markers over truth `\cite` commands, summed
    /// over the non-failed papers whose source has at least one cite command
    /// (the old, uncapped marker count ratio).
    #[serde(default)]
    pub marker_command_ratio: f32,
    /// Mean `PaperEval::body_alignment` (body text, markers, captions and
    /// math removed) over papers that have one.
    pub mean_body_alignment: Option<f32>,
    /// Mean `PaperEval::body_alignment_raw` over papers that have one.
    #[serde(default)]
    pub mean_body_alignment_raw: Option<f32>,
    /// Summed `body_words_matched` over summed `body_words_truth`, over
    /// papers with a `body_alignment`; 0.0 when there are none.
    #[serde(default)]
    pub body_word_recall: f32,
    /// Summed `body_words_matched` over summed `body_words_extracted`, over
    /// papers with a `body_alignment`; 0.0 when there are none.
    #[serde(default)]
    pub body_word_precision: f32,
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

/// What [`lcs_bounded`] allocated.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct LcsStats {
    /// Match-mask rows built: one per distinct token of the shorter side
    /// that also occurs in the longer side. Read by the tests only.
    #[cfg_attr(not(test), allow(dead_code))]
    rows_built: usize,
}

/// Length of the longest common subsequence, exact, by the bit-parallel
/// algorithm of Allison and Dix (in Hyyrö's form): one bit per token of the
/// shorter side, one `u64` word per 64 of them, and a match bitset per
/// distinct shorter-side token that also occurs in the longer side (a
/// token absent from the longer side is never looked up, and a longer-side
/// token absent from the shorter side has an all-zero mask, which leaves
/// the row unchanged, so neither needs a row). Each token of the longer
/// side updates the row as `V' = (V + U) | (V - U)` with `U = V & M[token]`;
/// since `U` is a subset of `V`, `V - U` is `V & !U` and only the addition
/// carries across words. The result is the number of zero bits among the
/// `short.len()` low bits. Time `O(n * m / 64)`, memory
/// `O(common * m / 64)` words. `None`, before any mask is allocated, when
/// the masks would take more than `max_words` words.
fn lcs_bounded(left: &[u32], right: &[u32], max_words: usize) -> Option<(usize, LcsStats)> {
    let (long, short) = if left.len() >= right.len() {
        (left, right)
    } else {
        (right, left)
    };
    if short.is_empty() {
        return Some((0, LcsStats::default()));
    }
    let words_per_row = short.len().div_ceil(64);
    let max_id = short.iter().copied().max().unwrap_or(0) as usize;
    let mut in_long: Vec<bool> = vec![false; max_id + 1];
    for &token in long {
        if let Some(flag) = in_long.get_mut(token as usize) {
            *flag = true;
        }
    }
    // Row of each token id in `masks`; `usize::MAX` when the id has no row
    // (not in both sides: its tokens never change the row).
    let mut row_of: Vec<usize> = vec![usize::MAX; max_id + 1];
    let mut rows = 0_usize;
    for &token in short {
        let id = token as usize;
        if in_long[id] && row_of[id] == usize::MAX {
            row_of[id] = rows;
            rows += 1;
        }
    }
    if rows.saturating_mul(words_per_row) > max_words {
        return None;
    }
    let mut masks: Vec<u64> = vec![0; rows * words_per_row];
    for (bit, &token) in short.iter().enumerate() {
        let mask_row = row_of[token as usize];
        if mask_row != usize::MAX {
            masks[mask_row * words_per_row + bit / 64] |= 1_u64 << (bit % 64);
        }
    }
    let mut row: Vec<u64> = vec![u64::MAX; words_per_row];
    for &token in long {
        let Some(&mask_row) = row_of.get(token as usize) else {
            continue;
        };
        if mask_row == usize::MAX {
            continue;
        }
        let mask = &masks[mask_row * words_per_row..(mask_row + 1) * words_per_row];
        let mut carry = 0_u64;
        for (word, &matches) in row.iter_mut().zip(mask) {
            let hits = *word & matches;
            let (sum, overflow_hits) = word.overflowing_add(hits);
            let (sum, overflow_carry) = sum.overflowing_add(carry);
            carry = u64::from(overflow_hits | overflow_carry);
            *word = sum | (*word & !hits);
        }
    }
    let mut zeros = 0_usize;
    for (index, word) in row.iter().enumerate() {
        let used = short.len() - index * 64;
        let valid = if used >= 64 {
            u64::MAX
        } else {
            (1_u64 << used) - 1
        };
        zeros += (!word & valid).count_ones() as usize;
    }
    Some((zeros, LcsStats { rows_built: rows }))
}

/// Order-sensitive similarity of two texts: `2 * lcs / (n + m)` over
/// lower-case alphanumeric word tokens, with the exact longest common
/// subsequence ([`lcs_bounded`]); each side is cut to its first
/// [`MAX_ALIGN_TOKENS`] tokens only past that safety cap (and to its first
/// [`LCS_FALLBACK_TOKENS`] only when the match masks would exceed
/// [`LCS_MAX_WORDS`]). Returns 1.0 when
/// both sides have no tokens and 0.0 when exactly one side has none.
pub fn word_alignment(a: &str, b: &str) -> f32 {
    alignment_score(token_counts(a, b, words))
}

/// Word tokens and their matches behind [`word_alignment`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct AlignCounts {
    /// Tokens of the left text (at most [`MAX_ALIGN_TOKENS`]) that were aligned.
    left: usize,
    /// Tokens of the right text (at most [`MAX_ALIGN_TOKENS`]) that were aligned.
    right: usize,
    /// Longest common subsequence of the two token sequences.
    matched: usize,
}

/// Token counts and LCS length of `a` against `b` for the body alignment:
/// [`token_counts`] over [`alignment_words`], so math tokens on either side
/// are left out.
fn align_counts(a: &str, b: &str) -> AlignCounts {
    token_counts(a, b, alignment_words)
}

/// Token counts and LCS length of `a` against `b`, both split into tokens
/// by `tokenize`. A side longer than [`MAX_ALIGN_TOKENS`] is cut to its
/// first `MAX_ALIGN_TOKENS` tokens, with a warning on stderr; when the LCS
/// match masks of the two sides would exceed [`LCS_MAX_WORDS`], both sides
/// are cut to their first [`LCS_FALLBACK_TOKENS`] tokens, with a warning.
fn token_counts(a: &str, b: &str, tokenize: fn(&str) -> Vec<String>) -> AlignCounts {
    let mut left_words = tokenize(a);
    let mut right_words = tokenize(b);
    if left_words.len() > MAX_ALIGN_TOKENS || right_words.len() > MAX_ALIGN_TOKENS {
        eprintln!(
            "warning: word alignment input over {MAX_ALIGN_TOKENS} tokens ({} and {}); \
             each side cut to its first {MAX_ALIGN_TOKENS}",
            left_words.len(),
            right_words.len()
        );
        left_words.truncate(MAX_ALIGN_TOKENS);
        right_words.truncate(MAX_ALIGN_TOKENS);
    }
    if left_words.is_empty() || right_words.is_empty() {
        return AlignCounts {
            left: left_words.len(),
            right: right_words.len(),
            matched: 0,
        };
    }
    let mut table: HashMap<&str, u32> = HashMap::new();
    let mut left = intern_tokens(&left_words, &mut table);
    let mut right = intern_tokens(&right_words, &mut table);
    let matched = if let Some((matched, _)) = lcs_bounded(&left, &right, LCS_MAX_WORDS) {
        matched
    } else {
        eprintln!(
            "warning: word alignment masks over {LCS_MAX_WORDS} words ({} and {} tokens); \
             each side cut to its first {LCS_FALLBACK_TOKENS}",
            left.len(),
            right.len()
        );
        left.truncate(LCS_FALLBACK_TOKENS);
        right.truncate(LCS_FALLBACK_TOKENS);
        lcs_bounded(&left, &right, usize::MAX).map_or(0, |(matched, _)| matched)
    };
    AlignCounts {
        left: left.len(),
        right: right.len(),
        matched,
    }
}

/// `2 * matched / (left + right)`; 1.0 when both sides are empty, 0.0 when
/// exactly one is.
fn alignment_score(counts: AlignCounts) -> f32 {
    if counts.left == 0 && counts.right == 0 {
        return 1.0;
    }
    if counts.left == 0 || counts.right == 0 {
        return 0.0;
    }
    ((2 * counts.matched) as f64 / (counts.left + counts.right) as f64) as f32
}

/// Numeric citation groups (`[3]`, `[2, 5]`, `[4–6]`) and parenthetical
/// author-year groups (`(Smith, 2020)`, `(Smith et al., 2020; Lee and Kim,
/// 2019a)`).
fn citation_marker_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"\[\s*\d+(?:\s*[,;–—-]\s*\d+)*\s*\]|\([A-Z][^()\[\]]{0,200}?(?:19|20)\d{2}[a-z]?\)",
        )
        .expect("valid regex")
    })
}

/// A figure or table caption's first line: `Figure N`, `Fig. N` or
/// `Table N` (any case, `N` possibly dotted like `2.1`, optional letter)
/// followed either by `:`, `.` or `|` and then whitespace or the line end,
/// or (Springer/RSC style, `Fig. 3 Overview of ...`) by whitespace and a
/// capitalised word (an upper-case letter then a lower-case one). Prose
/// such as "Table 2 shows", "Table 1.5 lists" or "Figure 3 and 4 show" is
/// kept.
fn caption_start_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^\s*(?i:figure|fig\.|table)\s*\d+(?:\.\d+)*[a-z]?(?:\s*[:.|](?:\s|$)|\s+\p{Lu}\p{Ll})",
        )
        .expect("valid regex")
    })
}

/// Whether `c` is a math symbol that `char::is_alphabetic` would count as a
/// letter or that often stands in for math: Greek letters (U+0370–U+03FF),
/// Mathematical Alphanumeric Symbols (U+1D400–U+1D7FF) and common
/// operators and brackets.
fn is_math_char(c: char) -> bool {
    matches!(
        c,
        '\u{0370}'..='\u{03FF}'
            | '\u{1D400}'..='\u{1D7FF}'
            | '∈'
            | '∑'
            | '∏'
            | '∫'
            | '≤'
            | '≥'
            | '≠'
            | '≈'
            | '∀'
            | '∃'
            | '∇'
            | '∂'
            | '⊆'
            | '⊂'
            | '∪'
            | '∩'
            | '→'
            | '↦'
            | '‖'
            | '⟨'
            | '⟩'
    )
}

/// Whether at least half of the line's non-whitespace characters are not
/// letters (display math, table rows, bare page numbers); Greek letters,
/// math-alphanumeric symbols and operators ([`is_math_char`]) count as
/// math, not letters. Blank lines are not math-heavy.
fn is_math_heavy(line: &str) -> bool {
    let mut letters = 0_usize;
    let mut other = 0_usize;
    for c in line.chars().filter(|c| !c.is_whitespace()) {
        if !is_math_char(c) && c.is_alphabetic() {
            letters += 1;
        } else {
            other += 1;
        }
    }
    other > 0 && other >= letters
}

/// Whether `c` marks a token as math for [`is_math_token`]: an
/// [`is_math_char`] symbol, one of `±×÷√∞∝`, a superscript or subscript
/// (U+2070–U+209F, `¹²³`) or one of the `LaTeX` math characters `=`, `^`,
/// `_` and `\`.
fn is_math_token_char(c: char) -> bool {
    is_math_char(c)
        || ('\u{2070}'..='\u{209F}').contains(&c)
        || matches!(
            c,
            '\u{00B9}'
                | '\u{00B2}'
                | '\u{00B3}'
                | '±'
                | '×'
                | '÷'
                | '√'
                | '∞'
                | '∝'
                | '='
                | '^'
                | '_'
                | '\\'
        )
}

/// Letter suffixes that keep a digits-then-letters token (`2nd`, `1990s`,
/// `10km`, `3D`, `4K`, `7B`) a word: ordinals, decades, dimensions and
/// units. Compared lower-case.
const NUMBER_SUFFIXES: &[&str] = &[
    "st", "nd", "rd", "th", "s", "d", "k", "m", "b", "km", "cm", "mm", "kg", "kb", "mb", "gb",
    "tb", "ms", "hz", "khz", "mhz", "ghz", "px", "pt",
];

/// Hyphens that keep a digits-and-letters token a word (`COVID-19`) and
/// split a token holding math (`top-𝑘`) in [`alignment_words`].
const HYPHENS: [char; 3] = ['-', '\u{2010}', '\u{2011}'];

/// Whether `core` is ASCII digits followed by a [`NUMBER_SUFFIXES`] suffix.
fn is_ordinal_or_unit(core: &str) -> bool {
    let digits = core
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(core.len());
    digits > 0 && NUMBER_SUFFIXES.contains(&core[digits..].to_lowercase().as_str())
}

/// Whether a token is math rather than a word, on either side of the body
/// alignment: it holds an [`is_math_token_char`] character, or its
/// alphanumeric core (the token without leading and trailing punctuation)
/// is a single letter other than `a`, `A`, `i` or `I`, or mixes digits and
/// letters (`x0`, `vij2`) while the token has no hyphen and the core is not
/// an ordinal or unit ([`is_ordinal_or_unit`]).
fn is_math_token(token: &str) -> bool {
    if token.chars().any(is_math_token_char) {
        return true;
    }
    let core = token.trim_matches(|c: char| !c.is_alphanumeric());
    let mut chars = core.chars();
    if let (Some(only), None) = (chars.next(), chars.next()) {
        return only.is_alphabetic() && !matches!(only, 'a' | 'A' | 'i' | 'I');
    }
    core.chars().any(char::is_numeric)
        && core.chars().any(char::is_alphabetic)
        && !token.contains(HYPHENS)
        && !is_ordinal_or_unit(core)
}

/// The body-alignment tokens of `s`: its whitespace-separated tokens that
/// are not [`is_math_token`], split into [`words`], without the words that
/// are themselves math tokens (`f(x)` keeps neither `f` nor `x`). A token
/// holding an [`is_math_token_char`] is first split at its hyphens, so
/// `top-𝑘`, `𝑛-gram` and `ε-greedy` keep `top`, `gram` and `greedy` as the
/// detexed `top-$k$` does.
fn alignment_words(s: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for token in s.split_whitespace() {
        let pieces: Vec<&str> = if token.chars().any(is_math_token_char) {
            token.split(HYPHENS).collect()
        } else {
            vec![token]
        };
        for piece in pieces {
            if !is_math_token(piece) {
                out.extend(words(piece).into_iter().filter(|word| !is_math_token(word)));
            }
        }
    }
    out
}

/// `text` without its math-heavy lines (see [`is_math_heavy`]).
fn drop_math_lines(text: &str) -> String {
    text.split('\n')
        .filter(|line| !is_math_heavy(line))
        .collect::<Vec<&str>>()
        .join("\n")
}

/// Most lines a caption paragraph may run past its [`caption_start_re`]
/// line when no blank line or prose paragraph closes it first.
const CAPTION_MAX_EXTRA_LINES: usize = 8;

/// Prose lines that must follow a new-paragraph line inside a caption for
/// that line to end the caption (see [`caption_ends_before`]).
const CAPTION_END_PROSE_LINES: usize = 2;

/// Whether the line ends a sentence or a parenthetical (`.`, `!`, `?` or
/// `)` after trailing whitespace).
fn ends_sentence(line: &str) -> bool {
    line.trim_end().ends_with(['.', '!', '?', ')'])
}

/// Whether a line reads as prose after a caption: not blank, not a
/// [`caption_start_re`] line and not [`is_math_heavy`].
fn is_prose_line(line: &str) -> bool {
    !line.trim().is_empty() && !caption_start_re().is_match(line) && !is_math_heavy(line)
}

/// Whether `lines[k]`, a line inside a caption, starts the prose after it:
/// it starts a new paragraph (an upper-case start after a line that ends a
/// sentence, see [`ends_sentence`]) and the next
/// [`CAPTION_END_PROSE_LINES`] lines are all [`is_prose_line`]s.
fn caption_ends_before(lines: &[&str], k: usize) -> bool {
    k > 0
        && ends_sentence(lines[k - 1])
        && lines[k]
            .trim_start()
            .chars()
            .next()
            .is_some_and(char::is_uppercase)
        && lines
            .get(k + 1..=k + CAPTION_END_PROSE_LINES)
            .is_some_and(|next| next.iter().all(|line| is_prose_line(line)))
}

/// `text` without caption paragraphs and without math-heavy lines. A
/// caption paragraph is a [`caption_start_re`] line plus at most
/// `CAPTION_MAX_EXTRA_LINES` further lines; it ends early at a blank line or
/// before the first line that starts a prose paragraph (see
/// [`caption_ends_before`]), so prose that follows a caption without a
/// blank line is kept while a caption of several sentences is dropped
/// whole. A new paragraph followed by fewer prose lines (one short
/// paragraph glued to the caption) is dropped with it.
fn drop_caption_and_math_lines(text: &str) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut kept: Vec<&str> = Vec::new();
    // `Some(n)`: inside a caption that may drop `n` more lines.
    let mut caption_left: Option<usize> = None;
    for (k, &line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            caption_left = None;
            kept.push(line);
            continue;
        }
        if caption_start_re().is_match(line) {
            caption_left = Some(CAPTION_MAX_EXTRA_LINES);
            continue;
        }
        if let Some(left) = caption_left {
            if caption_ends_before(&lines, k) {
                caption_left = None;
            } else {
                caption_left = Some(left - 1).filter(|&n| n > 0);
                continue;
            }
        }
        if !is_math_heavy(line) {
            kept.push(line);
        }
    }
    kept.join("\n")
}

/// Char ranges `[start, end)` of `page.text` covered by the markers of this
/// page whose text is found at their recorded char offset, sorted by start.
fn marker_char_ranges(page: &PageText, markers: &[CitationMarker]) -> Vec<(usize, usize)> {
    let chars: Vec<char> = page.text.chars().collect();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for marker in markers.iter().filter(|m| m.page == page.page) {
        let start = marker.offset as usize;
        let len = marker.text.chars().count();
        let end = start.saturating_add(len);
        if len == 0 || end > chars.len() {
            continue;
        }
        if chars[start..end].iter().copied().eq(marker.text.chars()) {
            ranges.push((start, end));
        }
    }
    ranges.sort_unstable();
    ranges
}

/// Line roles whose text stays in the body-only text: `body`, `heading`,
/// and an empty role (treated as untagged).
fn is_body_role(role: &str) -> bool {
    matches!(role, "" | "body" | "heading")
}

/// A byte range `[start, end)` of `page.text` left out of the body-only
/// text: a line tagged with a non-body role plus the whitespace after it.
/// `paragraph` puts one `\n` in its place, so a paragraph break after the
/// dropped line survives when the separator before it was a single `\n`.
struct DroppedLine {
    start: usize,
    end: usize,
    paragraph: bool,
}

/// Byte offset of the first occurrence of `needle` at or after `from` in
/// `text` that is a whole line: only spaces or tabs between it and the
/// previous `\n` (or the text start) and between it and the next `\n` (or
/// the text end). `needle` must not be empty.
fn find_whole_line(text: &str, needle: &str, from: usize) -> Option<usize> {
    let step = needle.chars().next().map_or(1, char::len_utf8);
    let mut search = from;
    while let Some(rel) = text.get(search..).and_then(|rest| rest.find(needle)) {
        let start = search + rel;
        let end = start + needle.len();
        let line_start = text[..start].rfind('\n').map_or(0, |pos| pos + 1);
        let line_end = text[end..].find('\n').map_or(text.len(), |pos| end + pos);
        if text[line_start..start].trim().is_empty() && text[end..line_end].trim().is_empty() {
            return Some(start);
        }
        search = start + step;
    }
    None
}

/// The lines of `page.lines` with a non-body role (see [`is_body_role`]),
/// as byte ranges of `page.text` sorted by start. The line texts are found
/// in `page.text` in order, each as a whole line after the previous one
/// found; a line that is not found (furniture removed from the text, or
/// text that is not built from the lines) is skipped without moving the
/// search on. Empty when no line has a non-body role, so untagged pages and
/// pages without lines keep their whole text.
fn dropped_lines(page: &PageText) -> Vec<DroppedLine> {
    let mut dropped: Vec<DroppedLine> = Vec::new();
    if page.lines.iter().all(|line| is_body_role(&line.role)) {
        return dropped;
    }
    let text = page.text.as_str();
    let mut cursor = 0_usize;
    for line in &page.lines {
        let needle = line.text.trim();
        if needle.is_empty() {
            continue;
        }
        let Some(start) = find_whole_line(text, needle, cursor) else {
            continue;
        };
        let end = start + needle.len();
        cursor = end;
        if is_body_role(&line.role) {
            continue;
        }
        let next = text.len() - text[end..].trim_start().len();
        let before_len = text[..start].trim_end().len();
        let paragraph = before_len > 0
            && next < text.len()
            && text[end..next].matches('\n').count() >= 2
            && text[before_len..start].matches('\n').count() < 2;
        dropped.push(DroppedLine {
            start,
            end: next,
            paragraph,
        });
    }
    dropped
}

/// `page.text` without the byte ranges `skips` (reference lists; sorted,
/// not overlapping), without the lines tagged with a non-body role (see
/// [`dropped_lines`]) and with each verified citation marker replaced by one
/// space. Text kept after a skipped range starts after a blank line, so it
/// does not run into the text before the range.
fn page_body_text(page: &PageText, markers: &[CitationMarker], skips: &[(usize, usize)]) -> String {
    let ranges = marker_char_ranges(page, markers);
    let dropped = dropped_lines(page);
    let mut out = String::with_capacity(page.text.len());
    let mut next = 0_usize;
    let mut next_drop = 0_usize;
    let mut next_skip = 0_usize;
    let mut after_skip = false;
    for (char_index, (byte_index, c)) in page.text.char_indices().enumerate() {
        while next_skip < skips.len() && skips[next_skip].1 <= byte_index {
            next_skip += 1;
        }
        if skips
            .get(next_skip)
            .is_some_and(|&(start, _)| start <= byte_index)
        {
            after_skip = true;
            continue;
        }
        if after_skip {
            after_skip = false;
            if !out.is_empty() {
                out.push_str("\n\n");
            }
        }
        while next_drop < dropped.len() && dropped[next_drop].end <= byte_index {
            next_drop += 1;
        }
        if let Some(drop) = dropped.get(next_drop).filter(|d| d.start <= byte_index) {
            if drop.start == byte_index && drop.paragraph {
                out.push('\n');
            }
            continue;
        }
        while next < ranges.len() && ranges[next].1 <= char_index {
            next += 1;
        }
        let covered = ranges
            .get(next)
            .is_some_and(|&(start, end)| (start..end).contains(&char_index));
        if covered {
            if ranges[next].0 == char_index {
                out.push(' ');
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The two sides of the body-only alignment, `(extracted, truth)`.
///
/// Extracted: [`body_only_text`] with pages joined by blank lines. Truth:
/// `truth_body` with math-heavy lines dropped.
fn alignment_texts(
    pages: &[PageText],
    markers: &[CitationMarker],
    truth_body: &str,
) -> (String, String) {
    (
        body_only_text(pages, markers, "\n\n"),
        drop_math_lines(truth_body),
    )
}

/// The extracted body-only text.
///
/// First pass, per page: lines tagged with a role other than `body` or
/// `heading` (figure and table text, captions, algorithms, table of
/// contents, front matter, furniture) are left out, found in `page.text` by
/// [`dropped_lines`]; untagged pages keep their whole text. Second pass,
/// the heuristics for untagged backends: page texts without the reference
/// lists (each from its heading to its end, see [`reference_extents`]), so
/// appendices and supplements printed after a list are kept, joined by
/// `separator`, with the `markers` found at their char offsets and any
/// remaining [`citation_marker_re`] match removed, then caption paragraphs
/// and math-heavy lines dropped. A page wholly inside a list is left out.
fn body_only_text(pages: &[PageText], markers: &[CitationMarker], separator: &str) -> String {
    let extents = reference_extents(pages);
    let mut parts: Vec<String> = Vec::new();
    for (pos, page) in pages.iter().enumerate() {
        let skips = page_skips(pos, page.text.len(), &extents);
        let whole = !page.text.is_empty()
            && skips
                .iter()
                .any(|&(start, end)| start == 0 && end >= page.text.len());
        if whole {
            continue;
        }
        parts.push(page_body_text(page, markers, &skips));
    }
    let joined = parts.join(separator);
    let unmarked = citation_marker_re().replace_all(&joined, " ");
    drop_caption_and_math_lines(&unmarked)
}

/// Where one reference list sits in the page texts: from `start` up to
/// `end` (exclusive), each a `(position in pages, byte offset in its text)`
/// pair; `end` is `(pages.len(), 0)` when the list runs to the document end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ReferenceExtent {
    start: (usize, usize),
    end: (usize, usize),
}

/// Byte ranges of the page at `pos` (with `len` bytes of text) that lie
/// inside one of the `extents`, in order.
fn page_skips(pos: usize, len: usize, extents: &[ReferenceExtent]) -> Vec<(usize, usize)> {
    let mut skips: Vec<(usize, usize)> = Vec::new();
    for extent in extents {
        if pos < extent.start.0 || pos > extent.end.0 {
            continue;
        }
        let start = if pos == extent.start.0 {
            extent.start.1.min(len)
        } else {
            0
        };
        let end = if pos == extent.end.0 {
            extent.end.1.min(len)
        } else {
            len
        };
        if start < end {
            skips.push((start, end));
        }
    }
    skips
}

/// Every reference list of the document as a [`ReferenceExtent`]. Starts
/// come from `citations::find_reference_sections` (each heading's line in
/// `page.text`), else from the text-line fallback of [`reference_start`].
/// A list ends as [`reference_end`] finds: at an `Appendix`,
/// `Supplementary`, `Acknowledgments` or caption line, or a short line set
/// clearly larger than the list, on the page of its last segmented entry;
/// else at the page after that one, at the next list's heading, or at the
/// document end, whichever comes first.
fn reference_extents(pages: &[PageText]) -> Vec<ReferenceExtent> {
    // `(start, position of the page of the last segmented entry)`.
    let mut starts: Vec<((usize, usize), Option<usize>)> = Vec::new();
    for section in find_reference_sections(pages) {
        let Some(pos) = pages.iter().position(|p| p.page == section.first_page) else {
            continue;
        };
        let offset = line_byte_offset(&pages[pos], section.first_line)
            .or_else(|| pages[pos].text.find(section.heading.as_str()))
            .unwrap_or(0);
        let last_entry = segment_entries(pages, &section)
            .last()
            .and_then(|entry| pages.iter().position(|p| p.page == entry.page));
        starts.push(((pos, offset), last_entry));
    }
    if starts.is_empty()
        && let Some(start) = reference_start(pages)
    {
        starts.push((start, None));
    }
    starts.sort_unstable();
    starts.dedup_by_key(|entry| entry.0);
    let doc_end = (pages.len(), 0_usize);
    let mut extents: Vec<ReferenceExtent> = Vec::new();
    for (k, &(start, last_entry)) in starts.iter().enumerate() {
        let limit = starts.get(k + 1).map_or(doc_end, |next| next.0);
        let end = reference_end(pages, start, limit, last_entry);
        extents.push(ReferenceExtent { start, end });
    }
    extents
}

/// Where the reference list that starts at `start` ends (see
/// [`reference_extents`]): the first [`is_reference_end`] line after
/// `start` that is on or after the page of the last segmented entry
/// (`last_entry`, a position in `pages`) and that no entry start, numbered
/// or author-year ([`is_entry_start`]), follows within
/// [`LIST_RESUME_LINES`] lines (a list
/// interrupted by a caption or table resumes, as in `citations`); else,
/// when the last entry wraps onto the following page(s), the end that
/// [`continuation_end`] finds there; else the start of the page after
/// `last_entry`, or `limit`, whichever comes first.
fn reference_end(
    pages: &[PageText],
    start: (usize, usize),
    limit: (usize, usize),
    last_entry: Option<usize>,
) -> (usize, usize) {
    let bound = last_entry
        .map(|entry| (entry + 1, 0_usize))
        .filter(|&resume| resume > start)
        .map_or(limit, |resume| resume.min(limit));
    let list_last_page = last_entry.unwrap_or(start.0).max(start.0);
    let median = list_median_size(pages, start, list_last_page.min(bound.0));
    let mut lines: Vec<(usize, TextLine<'_>)> = Vec::new();
    for (pos, page) in pages.iter().enumerate().take(bound.0 + 1).skip(start.0) {
        for line in page_line_starts(page) {
            let at = (pos, line.offset);
            if at > start && at < bound {
                lines.push((pos, line));
            }
        }
    }
    for (k, (pos, line)) in lines.iter().enumerate() {
        // `segment_entries` ends a list at the same kind of line, so a line
        // before the page of the last entry it found does not end the list.
        if last_entry.is_some_and(|entry| *pos < entry) {
            continue;
        }
        if is_reference_end(line.text, line.size, median) && !list_resumes(&lines[k + 1..]) {
            return (*pos, line.offset);
        }
    }
    if let Some(entry) = last_entry
        && bound == (entry + 1, 0)
        && bound < limit
    {
        let tail = lines
            .iter()
            .rev()
            .map(|(_, line)| line.text.trim())
            .find(|text| !text.is_empty());
        return continuation_end(pages, bound, limit, median, tail);
    }
    bound
}

/// Non-blank lines in a row that start no entry (see [`continuation_end`])
/// past the page of the last reference entry that may still count as the
/// list's continuation.
const CONTINUATION_MAX_LINES: usize = 12;

/// A line that reads like the rest of a reference entry: it starts with a
/// lower-case letter, or holds a year, a DOI, a URL, an `arXiv` id or a
/// `pp.`/`vol.` page or volume mark.
fn entry_tail_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^\s*\p{Ll}|\b(?:19|20)\d{2}[a-z]?\b|(?i:\bdoi\b|https?://|\barxiv\b|\bpp?\.\s*\d|\bvol\.)",
        )
        .expect("valid regex")
    })
}

/// A non-blank line of the pages after a reference list, as
/// [`continuation_end`] scans them.
struct ContinuationLine<'a> {
    pos: usize,
    line: TextLine<'a>,
    /// Whether a blank line separates it from the text before it on its
    /// page.
    paragraph_start: bool,
    /// Whether it starts a reference entry ([`is_entry_start`]).
    entry: bool,
    /// Whether it starts an entry or is a label of one: [`is_entry_start`]
    /// or [`continuation_label_re`].
    label: bool,
}

/// Where a reference list ends when its last entry started on the page
/// before `from` (the start of the following page) and may wrap onto it.
/// The following lines, up to `limit`, count as the list's continuation
/// only when the list's last line (`tail`) does not end with `.` or the
/// first following line reads like an entry's rest ([`entry_tail_re`]) or
/// starts an entry ([`is_entry_start`]: a numbered or bracketed label, or
/// an unlabeled author-year entry, so a list that goes on over the page
/// continues). The continuation then runs until the first
/// [`is_reference_end`] line (a heading, a caption or a line set clearly
/// larger than the list's `median`), the first line of a blank-separated
/// prose paragraph (two lines of at least 8 words each, neither starting
/// an entry nor a [`continuation_label_re`] label) or the line after
/// [`CONTINUATION_MAX_LINES`] non-blank lines in a row that start no entry
/// and are no label, or to `limit` when that comes first.
/// Returns `from` when nothing continues the list.
fn continuation_end(
    pages: &[PageText],
    from: (usize, usize),
    limit: (usize, usize),
    median: Option<f32>,
    tail: Option<&str>,
) -> (usize, usize) {
    let mut window: Vec<ContinuationLine<'_>> = Vec::new();
    // Non-blank lines in a row that start no entry; the scan stops one line
    // past the cap (the prose test looks one line ahead).
    let mut since_entry = 0_usize;
    'pages: for (pos, page) in pages.iter().enumerate().take(limit.0 + 1).skip(from.0) {
        for line in page_line_starts(page) {
            if (pos, line.offset) >= limit {
                break 'pages;
            }
            if line.text.trim().is_empty() {
                continue;
            }
            let paragraph_start = page.text[..line.offset]
                .trim_end_matches([' ', '\t'])
                .ends_with("\n\n");
            let entry = is_entry_start(line.text);
            let label = entry || continuation_label_re().is_match(line.text);
            since_entry = if label { 0 } else { since_entry + 1 };
            window.push(ContinuationLine {
                pos,
                line,
                paragraph_start,
                entry,
                label,
            });
            if since_entry > CONTINUATION_MAX_LINES + 1 {
                break 'pages;
            }
        }
    }
    let Some(first) = window.first() else {
        return from;
    };
    let unfinished = tail.is_some_and(|text| !text.ends_with('.'));
    if !unfinished && !first.entry && !entry_tail_re().is_match(first.line.text) {
        return from;
    }
    let is_prose =
        |item: &ContinuationLine<'_>| !item.label && item.line.text.split_whitespace().count() >= 8;
    let mut run = 0_usize;
    for (k, item) in window.iter().enumerate() {
        if is_reference_end(item.line.text, item.line.size, median) {
            return (item.pos, item.line.offset);
        }
        run = if item.label { 0 } else { run + 1 };
        if run > CONTINUATION_MAX_LINES {
            return (item.pos, item.line.offset);
        }
        let prose_paragraph = item.paragraph_start
            && is_prose(item)
            && window
                .get(k + 1)
                .is_some_and(|next| !next.paragraph_start && is_prose(next));
        if prose_paragraph {
            return (item.pos, item.line.offset);
        }
    }
    limit
}

/// Non-blank lines after a candidate list end within which an entry start
/// ([`is_entry_start`]) means the list resumes.
const LIST_RESUME_LINES: usize = 30;

/// A numbered reference entry's first line: `[12] ...` or `12. ...`.
fn entry_label_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*(?:\[\d+\]|\d+\.)\s+\S").expect("valid regex"))
}

/// An unlabeled author-year entry's first line: a surname (with optional
/// particles such as `van` or `de`, and up to one more capitalised part)
/// then a comma and initials (`Hu, W.`, `Smith, J.-P.`), and later on the
/// line either a parenthesised year (`(2021)`, `(2019a)`) or, when the
/// author list wraps, a line end after `,`, `&` or `and`.
fn author_year_entry_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^\s*(?:(?:van|von|de|der|den|del|della|di|da|du|le|la|dos|das)\s+)*\p{Lu}[\p{L}'’-]+(?:[ -]\p{Lu}[\p{L}'’-]+)?,\s*\p{Lu}\.(?:\s*-?\s*\p{Lu}\.)*(?:.*\((?:19|20)\d{2}[a-z]?\)|.*(?:,|&|\band)\s*$)",
        )
        .expect("valid regex")
    })
}

/// A reference label that only [`continuation_end`] counts as an entry
/// start: an RSC-style bare number before a capitalised word (`12 Smith`)
/// or a detached `[12]` alone on its line. Too loose for [`list_resumes`],
/// where a stray number would swallow an appendix.
fn continuation_label_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*(?:\d{1,4}\s+\p{Lu}|\[\d+\]\s*$)").expect("valid regex"))
}

/// Whether `text` starts a reference entry: a numbered or bracketed label
/// ([`entry_label_re`]) or an unlabeled author-year entry
/// ([`author_year_entry_re`]).
fn is_entry_start(text: &str) -> bool {
    entry_label_re().is_match(text) || author_year_entry_re().is_match(text)
}

/// Whether an entry ([`is_entry_start`]) starts one of the first
/// [`LIST_RESUME_LINES`] non-blank `lines`.
fn list_resumes(lines: &[(usize, TextLine<'_>)]) -> bool {
    lines
        .iter()
        .filter(|(_, line)| !line.text.trim().is_empty())
        .take(LIST_RESUME_LINES)
        .any(|(_, line)| is_entry_start(line.text))
}

/// A line of `page.text` as [`reference_end`] scans it.
struct TextLine<'a> {
    /// Byte offset of the line's first character in `page.text`.
    offset: usize,
    text: &'a str,
    /// Largest span font size of the line, when known.
    size: Option<f32>,
}

/// The lines of `page` with their byte offsets: the body-role lines of
/// `page.lines` found in `page.text` in order (as whole lines, see
/// [`find_whole_line`]; a line not found is skipped), or the `\n`-separated
/// lines of `page.text` when the page has no `lines`.
fn page_line_starts(page: &PageText) -> Vec<TextLine<'_>> {
    let mut out: Vec<TextLine<'_>> = Vec::new();
    let text = page.text.as_str();
    if page.lines.is_empty() {
        let mut offset = 0_usize;
        for line in text.split('\n') {
            out.push(TextLine {
                offset,
                text: line,
                size: None,
            });
            offset += line.len() + 1;
        }
        return out;
    }
    let mut cursor = 0_usize;
    for line in &page.lines {
        let needle = line.text.trim();
        if needle.is_empty() {
            continue;
        }
        let Some(found) = find_whole_line(text, needle, cursor) else {
            continue;
        };
        cursor = found + needle.len();
        if !is_body_role(&line.role) {
            continue;
        }
        let mut size: Option<f32> = None;
        for index in &line.spans {
            if let Some(span_size) = page.spans.get(*index as usize).and_then(|span| span.size) {
                size = Some(size.map_or(span_size, |best: f32| best.max(span_size)));
            }
        }
        out.push(TextLine {
            offset: found,
            text: &text[found..cursor],
            size,
        });
    }
    out
}

/// Median font size of the lines of a reference list: those after `start`
/// on its page and on the following pages up to position `last_page`.
fn list_median_size(pages: &[PageText], start: (usize, usize), last_page: usize) -> Option<f32> {
    let mut sizes: Vec<f32> = Vec::new();
    for (pos, page) in pages
        .iter()
        .enumerate()
        .take(last_page.saturating_add(1))
        .skip(start.0)
    {
        for line in page_line_starts(page) {
            if (pos, line.offset) > start
                && let Some(size) = line.size
            {
                sizes.push(size);
            }
        }
    }
    if sizes.is_empty() {
        return None;
    }
    sizes.sort_by(f32::total_cmp);
    Some(sizes[sizes.len() / 2])
}

/// A heading that ends a reference list: `Appendix`, `Appendices`,
/// `Supplementary`, `Supporting information`, `Acknowledgments` or an
/// author biography, optionally numbered (`A`, `7.`, `IV`), as
/// `citations` ends a list.
fn reference_end_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)^\s*[-–—\s]*(?:(?:\d+|[A-Z]|[IVX]+)[.:]?\s+)?(?:(?:technical|online)\s+)?(?:appendix|appendices|supplementary|supplemental|supporting information|acknowledg\w*|author biograph\w*|biograph\w*)\b",
        )
        .expect("valid regex")
    })
}

/// Whether a line after a reference heading starts the text that follows
/// the list: a short line (at most 80 characters) that is a
/// [`reference_end_re`] heading or a [`caption_start_re`] caption, or that
/// is set at least 1.15 times the list's `median` size, starts with an
/// upper-case letter or a digit and does not end like an entry (`.` or
/// `,`).
fn is_reference_end(text: &str, size: Option<f32>, median: Option<f32>) -> bool {
    let line = text.trim();
    if line.is_empty() || line.chars().count() > 80 {
        return false;
    }
    if reference_end_re().is_match(line) || caption_start_re().is_match(line) {
        return true;
    }
    let (Some(size), Some(typical)) = (size, median) else {
        return false;
    };
    size >= typical * 1.15
        && line
            .chars()
            .next()
            .is_some_and(|c| c.is_uppercase() || c.is_ascii_digit())
        && !line.ends_with(['.', ','])
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

/// Digit runs of a title for the version check, with their counts: NFKC
/// first (so `NH₃` gives `3`), leading zeros dropped (`04` is `4`), and
/// 19xx/20xx years left out (extracted titles often swallow the year).
fn title_digit_runs(title: &str) -> BTreeMap<String, u32> {
    let compat: String = title.nfkc().collect();
    let mut runs: BTreeMap<String, u32> = BTreeMap::new();
    for run in compat
        .split(|c: char| !c.is_ascii_digit())
        .filter(|run| !run.is_empty())
    {
        let is_year = run.len() == 4 && (run.starts_with("19") || run.starts_with("20"));
        if is_year {
            continue;
        }
        let trimmed = run.trim_start_matches('0');
        let key = if trimmed.is_empty() { "0" } else { trimmed };
        *runs.entry(key.to_string()).or_insert(0) += 1;
    }
    runs
}

/// Whether two titles name different versions of a work (`Gemini 3` vs
/// `Gemini 2`, `v1` vs `v2`): each side has a digit run the other lacks.
/// Digits on one side only (page numbers or a month swallowed into the
/// extracted title, a garbled superscript) are no conflict.
fn digits_conflict(a: &BTreeMap<String, u32>, b: &BTreeMap<String, u32>) -> bool {
    let has_extra = |x: &BTreeMap<String, u32>, y: &BTreeMap<String, u32>| {
        x.iter()
            .any(|(run, &count)| y.get(run).copied().unwrap_or(0) < count)
    };
    has_extra(a, b) && has_extra(b, a)
}

/// [`title_digit_runs`] of a non-empty title, `None` without one.
fn optional_title_digits(title: Option<&String>) -> Option<BTreeMap<String, u32>> {
    let title = title?;
    if normalize_title(title).is_empty() {
        None
    } else {
        Some(title_digit_runs(title))
    }
}

/// First run of ASCII digits in `s` (`179` of `179–186`), without leading
/// zeros.
fn first_digit_run(s: &str) -> Option<String> {
    let run = s
        .split(|c: char| !c.is_ascii_digit())
        .find(|run| !run.is_empty())?;
    let trimmed = run.trim_start_matches('0');
    let digits = if trimmed.is_empty() { "0" } else { trimmed };
    Some(digits.to_string())
}

/// Whether `needle` occurs as a contiguous run of words in `haystack`.
fn contains_words(haystack: &[String], needle: &[String]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

/// Number of `truth` surnames paired one-to-one with equal `extracted`
/// surnames.
fn surname_matches(truth: &[String], extracted: &[String]) -> usize {
    let mut used = vec![false; extracted.len()];
    let mut count = 0;
    for sur in truth {
        let found = (0..extracted.len()).find(|&pos| !used[pos] && extracted[pos] == *sur);
        if let Some(pos) = found {
            used[pos] = true;
            count += 1;
        }
    }
    count
}

/// Truth-side evidence for the author-venue pass.
struct AuthorVenueTruth {
    /// Year of the truth entry (the pass skips entries without one).
    year: u16,
    /// Surname of the first truth author (never empty).
    first_surname: String,
    /// Non-empty surnames of all truth authors, in order.
    surnames: Vec<String>,
    /// Words of the truth text (NFKC, line-break hyphens joined).
    text_words: Vec<String>,
}

impl AuthorVenueTruth {
    /// Evidence of `truth_ref`, `None` without a year or a first-author
    /// surname.
    fn from_truth(truth_ref: &TruthReference) -> Option<Self> {
        let year = truth_ref.year?;
        let first_surname = surname(truth_ref.authors.first()?);
        if first_surname.is_empty() {
            return None;
        }
        let surnames: Vec<String> = truth_ref
            .authors
            .iter()
            .map(|name| surname(name))
            .filter(|sur| !sur.is_empty())
            .collect();
        let compat: String = truth_ref.text.nfkc().collect();
        let text_words = words(&join_break_hyphens(&compat));
        Some(Self {
            year,
            first_surname,
            surnames,
            text_words,
        })
    }

    /// Whether the title-less `entry` is this truth entry: same first-author
    /// surname and year, and either its volume and first page both appear as
    /// words of the truth text, or its venue appears there as a run of words
    /// and at least two author surnames match.
    fn agrees(&self, entry: &ReferenceEntry) -> bool {
        if title_key(entry.title.as_ref()).is_some() || entry.year != Some(self.year) {
            return false;
        }
        let first_agrees = entry
            .authors
            .first()
            .is_some_and(|name| surname(name) == self.first_surname);
        if !first_agrees {
            return false;
        }
        let has_word = |word: &str| self.text_words.iter().any(|w| w == word);
        let volume = entry.volume.as_deref().map(words).unwrap_or_default();
        let first_page = entry.pages.as_deref().and_then(first_digit_run);
        let volume_page = !volume.is_empty()
            && volume.iter().all(|word| has_word(word))
            && first_page.is_some_and(|page| has_word(&page));
        if volume_page {
            return true;
        }
        let venue = entry.venue.as_deref().map(words).unwrap_or_default();
        if !contains_words(&self.text_words, &venue) {
            return false;
        }
        let ext_surnames: Vec<String> = entry
            .authors
            .iter()
            .map(|name| surname(name))
            .filter(|sur| !sur.is_empty())
            .collect();
        surname_matches(&self.surnames, &ext_surnames) >= 2
    }
}

/// Tie-break rank of pairing `truth_ref` with `entry` when several extracted
/// entries match equally well: (same year, same first-author surname), where
/// surnames compare folded, so accents and case do not matter. Higher wins,
/// year first.
fn tie_rank(truth_ref: &TruthReference, entry: &ReferenceEntry) -> (bool, bool) {
    let year = truth_ref.year.is_some() && truth_ref.year == entry.year;
    let author = match (truth_ref.authors.first(), entry.authors.first()) {
        (Some(t), Some(e)) => {
            let sur = surname(t);
            !sur.is_empty() && sur == surname(e)
        }
        _ => false,
    };
    (year, author)
}

/// Mutable state of the greedy one-to-one matcher.
struct Matcher<'a> {
    truth: &'a [TruthReference],
    extracted: &'a [ReferenceEntry],
    matches: Vec<RefMatch>,
    used: Vec<bool>,
    /// [`optional_title_digits`] of each truth title.
    truth_digits: Vec<Option<BTreeMap<String, u32>>>,
    /// [`optional_title_digits`] of each extracted title.
    ext_digits: Vec<Option<BTreeMap<String, u32>>>,
}

impl Matcher<'_> {
    fn is_open(&self, truth_pos: usize) -> bool {
        self.matches[truth_pos].extracted_index.is_none()
    }

    /// Whether the two titles, when both exist, do not name different
    /// versions of a work (see [`digits_conflict`]).
    fn digits_compatible(&self, truth_pos: usize, ext_pos: usize) -> bool {
        match (&self.truth_digits[truth_pos], &self.ext_digits[ext_pos]) {
            (Some(t), Some(e)) => !digits_conflict(t, e),
            _ => true,
        }
    }

    fn assign(&mut self, truth_pos: usize, ext_pos: usize, method: &str, score: f32) {
        self.used[ext_pos] = true;
        let m = &mut self.matches[truth_pos];
        m.extracted_index = Some(self.extracted[ext_pos].index);
        m.method = method.to_string();
        m.score = score;
    }

    /// Pairs open truth entries with the first unused extracted entry whose
    /// key equals theirs (and, with `check_digits`, whose title does not
    /// name another version, see [`Self::digits_compatible`]).
    fn exact_pass(
        &mut self,
        truth_keys: &[Option<String>],
        ext_keys: &[Option<String>],
        method: &str,
        score: f32,
        check_digits: bool,
    ) {
        for (truth_pos, key) in truth_keys.iter().enumerate() {
            let Some(key) = key else {
                continue;
            };
            if !self.is_open(truth_pos) {
                continue;
            }
            let found = ext_keys.iter().enumerate().position(|(ext_pos, ext_key)| {
                !self.used[ext_pos]
                    && ext_key.as_deref() == Some(key.as_str())
                    && (!check_digits || self.digits_compatible(truth_pos, ext_pos))
            });
            if let Some(ext_pos) = found {
                self.assign(truth_pos, ext_pos, method, score);
            }
        }
    }

    /// Pairs open truth entries with unused extracted entries of the same
    /// normalised title, method `"title"`, in two phases: first only pairs
    /// that also agree on the year, then the remaining title-only pairs, so a
    /// truth entry whose year matches is not starved by an earlier truth entry
    /// with the same title (`Orthogonal Polynomials`, Szegő 1939, vs Case's
    /// `Orthogonal polynomials. II.` 1975 cut to the same title). Among
    /// several candidates the one agreeing on year, then on first-author
    /// surname (see [`tie_rank`]), then the earliest one wins.
    fn exact_title_pass(&mut self, truth_keys: &[Option<String>], ext_keys: &[Option<String>]) {
        for year_phase in [true, false] {
            for (truth_pos, key) in truth_keys.iter().enumerate() {
                let Some(key) = key else {
                    continue;
                };
                if !self.is_open(truth_pos) {
                    continue;
                }
                let truth_ref = &self.truth[truth_pos];
                // (extracted position, tie rank)
                let mut best: Option<(usize, (bool, bool))> = None;
                for (ext_pos, ext_key) in ext_keys.iter().enumerate() {
                    if self.used[ext_pos] || ext_key.as_deref() != Some(key.as_str()) {
                        continue;
                    }
                    let rank = tie_rank(truth_ref, &self.extracted[ext_pos]);
                    if year_phase && !rank.0 {
                        continue;
                    }
                    if best.is_none_or(|(_, best_rank)| rank > best_rank) {
                        best = Some((ext_pos, rank));
                    }
                }
                if let Some((ext_pos, _)) = best {
                    self.assign(truth_pos, ext_pos, "title", 1.0);
                }
            }
        }
    }

    /// Pairs open truth entries with the unused extracted entry whose title
    /// words have the highest Jaccard similarity, when it reaches the minimum
    /// and the titles do not name different versions. On equal similarity the
    /// entry agreeing on year, then on first-author surname (see
    /// [`tie_rank`]), then the earliest one wins.
    fn fuzzy_title_pass(
        &mut self,
        truth_words: &[BTreeSet<String>],
        ext_words: &[BTreeSet<String>],
    ) {
        for (truth_pos, truth_set) in truth_words.iter().enumerate() {
            if truth_set.is_empty() || !self.is_open(truth_pos) {
                continue;
            }
            // (extracted position, similarity, tie rank)
            let mut best: Option<(usize, f32, (bool, bool))> = None;
            for (ext_pos, ext_set) in ext_words.iter().enumerate() {
                if self.used[ext_pos] || !self.digits_compatible(truth_pos, ext_pos) {
                    continue;
                }
                let sim = jaccard(truth_set, ext_set);
                if sim < TITLE_JACCARD_MIN {
                    continue;
                }
                let rank = tie_rank(&self.truth[truth_pos], &self.extracted[ext_pos]);
                let better =
                    best.is_none_or(|(_, best_sim, best_rank)| match sim.total_cmp(&best_sim) {
                        Ordering::Greater => true,
                        Ordering::Equal => rank > best_rank,
                        Ordering::Less => false,
                    });
                if better {
                    best = Some((ext_pos, sim, rank));
                }
            }
            if let Some((ext_pos, sim, _)) = best {
                self.assign(truth_pos, ext_pos, "title", sim);
            }
        }
    }

    /// Last resort: pairs each open truth entry, in order, with the unused
    /// extracted entry whose whole-text word set is most similar, when the
    /// Jaccard similarity reaches [`TEXT_JACCARD_MIN`] and, when both have a
    /// title, the titles do not name different versions. On equal similarity the
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
                if self.used[ext_pos]
                    || ext_set.is_empty()
                    || !self.digits_compatible(truth_pos, ext_pos)
                {
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

    /// Pairs open truth entries that have a year with the first unused
    /// extracted entry without a title (RSC-style entries print none) that
    /// [`AuthorVenueTruth::agrees`] with, method `"author-venue"`.
    fn author_venue_pass(&mut self, truth: &[TruthReference]) {
        for (truth_pos, truth_ref) in truth.iter().enumerate() {
            if !self.is_open(truth_pos) {
                continue;
            }
            let Some(evidence) = AuthorVenueTruth::from_truth(truth_ref) else {
                continue;
            };
            let found = (0..self.extracted.len())
                .find(|&ext_pos| !self.used[ext_pos] && evidence.agrees(&self.extracted[ext_pos]));
            if let Some(ext_pos) = found {
                self.assign(truth_pos, ext_pos, "author-venue", AUTHOR_VENUE_SCORE);
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
/// ignored), equal normalized title or title-word Jaccard >= 0.8, for
/// extracted entries without a title first-author surname and year plus
/// volume and first page or venue and a second surname (method
/// `"author-venue"`), equal first-author surname (lower-case, ASCII-folded)
/// plus year, then, as a last
/// resort, the most similar whole entry text (word Jaccard >= 0.6 of the
/// truth `text` and the extracted `raw`, method `"text"`). Each extracted
/// entry is used at most once. The fuzzy title, author-year and text passes
/// reject a pair whose titles both carry digit runs the other lacks
/// (`Gemini 3` vs `Gemini 2`; see [`digits_conflict`]). Among entries of
/// equal title (or equal title similarity), the one agreeing with the truth
/// on year, then on first-author surname, is preferred, and same-title pairs
/// that agree on the year are assigned before title-only ones. Finally, truth
/// entries that are duplicates of
/// each other (same normalised title or text) take their partners in index
/// order unless that agrees worse on year and first author, so they are not
/// paired crosswise. One [`RefMatch`] per truth reference, in order.
pub fn match_references(truth: &[TruthReference], extracted: &[ReferenceEntry]) -> Vec<RefMatch> {
    let mut matcher = Matcher {
        truth,
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
        truth_digits: truth
            .iter()
            .map(|truth_ref| optional_title_digits(truth_ref.title.as_ref()))
            .collect(),
        ext_digits: extracted
            .iter()
            .map(|entry| optional_title_digits(entry.title.as_ref()))
            .collect(),
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
    matcher.exact_pass(&truth_doi, &ext_doi, "doi", 1.0, false);

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
    matcher.exact_pass(&truth_arxiv, &ext_arxiv, "arxiv", 1.0, false);

    let truth_title: Vec<Option<String>> = truth
        .iter()
        .map(|truth_ref| title_key(truth_ref.title.as_ref()))
        .collect();
    let ext_title: Vec<Option<String>> = extracted
        .iter()
        .map(|entry| title_key(entry.title.as_ref()))
        .collect();
    matcher.exact_title_pass(&truth_title, &ext_title);

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
    matcher.author_venue_pass(truth);

    let truth_ay: Vec<Option<String>> = truth
        .iter()
        .map(|truth_ref| author_year_key(truth_ref.authors.first(), truth_ref.year))
        .collect();
    let ext_ay: Vec<Option<String>> = extracted
        .iter()
        .map(|entry| author_year_key(entry.authors.first(), entry.year))
        .collect();
    matcher.exact_pass(&truth_ay, &ext_ay, "author-year", 0.75, true);

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

/// A leading printed label (`[12]`, `(12)`, `12.`, `12)` or a bare `12`)
/// before a reference's text.
fn raw_label_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^\s*(?:\[\d{1,4}\]|\(\d{1,4}\)|\d{1,4}[.)]?)\s+").expect("valid regex")
    })
}

/// The tail of a title-less journal reference: `, <year>, <volume>,
/// <pages>.` (`, 2013, 42, 3127–3171.`, `, 2015, 518, 179–186.`), with an
/// optional `(issue)` after the volume.
fn titleless_tail_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r",\s*(?:19|20)\d{2}[a-z]?,\s*\d{1,5}(?:\s*\(\d{1,4}\))?,\s*\p{Lu}?\d{1,6}(?:\s*[–—-]\s*\p{Lu}?\d{1,6})?\.?$",
        )
        .expect("valid regex")
    })
}

/// One initials-first person name: `Q. Zhang`, `J.-P. Sauvage`,
/// `P. G. de Gennes`, `A. Smith-Jones`.
fn initials_first_name_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^(?:\p{Lu}\p{Ll}?\.\s*(?:-\s*\p{Lu}\p{Ll}?\.\s*)?)+(?:\p{Ll}+\s+)*\p{Lu}[\p{L}'’-]*(?:[\s-]\p{Lu}[\p{L}'’-]*)*$",
        )
        .expect("valid regex")
    })
}

/// Whether one comma-delimited part of a reference is a list of
/// initials-first names (`Q. Zhang`, `E. Uchaker and G. Cao`, `and G. Cao`,
/// `et al.`).
fn is_initials_first_names(part: &str) -> bool {
    let part = part.trim();
    let part = part.strip_prefix("and ").unwrap_or(part);
    let mut any = false;
    for name in part.split(" and ").map(str::trim) {
        let is_name = name == "et al." || initials_first_name_re().is_match(name);
        if name.is_empty() || !is_name {
            return false;
        }
        any = true;
    }
    any
}

/// Whether a reference's `raw` text is a title-less journal style (Royal
/// Society of Chemistry: `Q. Zhang, E. Uchaker and G. Cao, Chem. Soc. Rev.,
/// 2013, 42, 3127–3171.`): after an optional label, one or more
/// initials-first author parts, then a journal abbreviation of at most 80
/// characters that starts upper-case and has no lower-case word of four or
/// more letters (no sentence), then `, <year>, <volume>, <pages>.` at the
/// end, and no quotation marks anywhere. An ordinary entry whose title
/// parsing merely failed does not match.
fn titleless_raw(raw: &str) -> bool {
    let raw = raw.trim();
    let body = raw_label_re()
        .find(raw)
        .map_or(raw, |found| &raw[found.end()..])
        .trim();
    if body.contains(['"', '“', '”', '„', '‘']) {
        return false;
    }
    let Some(tail) = titleless_tail_re().find(body) else {
        return false;
    };
    let parts: Vec<&str> = body[..tail.start()].split(',').collect();
    let names = parts
        .iter()
        .take_while(|part| is_initials_first_names(part))
        .count();
    if names == 0 || names >= parts.len() {
        return false;
    }
    let journal = parts[names..].join(",");
    let journal = journal.trim();
    journal.chars().next().is_some_and(char::is_uppercase)
        && journal.chars().count() <= 80
        && !journal
            .split_whitespace()
            .any(|w| w.chars().count() >= 4 && w.starts_with(char::is_lowercase))
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

/// Correctness of the marker targets against the reference `matches`:
/// `(correct targets, distinct cited keys hit, distinct cited keys)`. A target
/// is correct when its extracted entry is matched to a truth key that occurs
/// in `cited_keys`; a key is hit when at least one correct target points at
/// its matched entry.
fn marker_correctness(
    markers: &[CitationMarker],
    matches: &[RefMatch],
    cited_keys: &[String],
) -> (u32, u32, u32) {
    let cited: BTreeSet<&str> = cited_keys.iter().map(String::as_str).collect();
    let mut key_of: HashMap<u32, &str> = HashMap::new();
    for m in matches {
        if let Some(idx) = m.extracted_index {
            key_of.insert(idx, m.truth_key.as_str());
        }
    }
    let mut correct = 0_u32;
    let mut hit: BTreeSet<&str> = BTreeSet::new();
    for target in markers.iter().flat_map(|marker| marker.targets.iter()) {
        if let Some(key) = key_of
            .get(target)
            .copied()
            .filter(|key| cited.contains(key))
        {
            correct += 1;
            hit.insert(key);
        }
    }
    (correct, hit.len() as u32, cited.len() as u32)
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
    let mut title_not_applicable = 0_u32;
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
            let titleless_style = ext.title.is_none() && titleless_raw(&ext.raw);
            title_not_applicable += u32::from(truth_ref.title.is_some() && titleless_style);
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
    let resolved_targets = marker_targets;
    let truth_cite_commands = truth.citations.cite_commands;
    let truth_cited_keys = truth.citations.cited_keys.len() as u32;
    let marker_recall = if truth_cited_keys == 0 {
        None
    } else {
        Some(ratio(u64::from(resolved_targets), u64::from(truth_cited_keys)).min(1.0))
    };
    let marker_command_ratio = if truth_cite_commands == 0 {
        None
    } else {
        Some(ratio(
            u64::from(resolved_markers),
            u64::from(truth_cite_commands),
        ))
    };
    let (marker_targets_correct, marker_keys_hit, marker_keys_cited) =
        marker_correctness(&result.citations, &matches, &truth.citations.cited_keys);

    let (body_alignment, body_alignment_raw, body_counts) = if truth.body_text.trim().is_empty() {
        (None, None, AlignCounts::default())
    } else {
        let joined: Vec<&str> = result.pages.iter().map(|page| page.text.as_str()).collect();
        let raw = word_alignment(&joined.join("\n"), &truth.body_text);
        let (extracted_body, truth_body) =
            alignment_texts(&result.pages, &result.citations, &truth.body_text);
        let counts = align_counts(&extracted_body, &truth_body);
        (Some(alignment_score(counts)), Some(raw), counts)
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
        title_not_applicable,
        doi_printed,
        over_segmentation: ratio(u64::from(extracted_refs), u64::from(truth_refs)),
        timings: result.timings,
        truth_cite_commands,
        truth_cited_keys,
        truth_author_year_only: truth.citations.cite_only_author_year,
        extracted_markers,
        resolved_markers,
        resolved_targets,
        marker_recall,
        marker_command_ratio,
        marker_targets,
        marker_targets_correct,
        marker_keys_hit,
        marker_keys_cited,
        body_alignment,
        body_alignment_raw,
        body_words_extracted: body_counts.left as u32,
        body_words_truth: body_counts.right as u32,
        body_words_matched: body_counts.matched as u32,
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
        title_not_applicable: 0,
        doi_printed: 0,
        over_segmentation: 0.0,
        timings: StageTimings::default(),
        truth_cite_commands: 0,
        truth_cited_keys: 0,
        truth_author_year_only: 0,
        extracted_markers: 0,
        resolved_markers: 0,
        resolved_targets: 0,
        marker_recall: None,
        marker_command_ratio: None,
        marker_targets: 0,
        marker_targets_correct: 0,
        marker_keys_hit: 0,
        marker_keys_cited: 0,
        body_alignment: None,
        body_alignment_raw: None,
        body_words_extracted: 0,
        body_words_truth: 0,
        body_words_matched: 0,
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
/// field accuracies are over matched pairs only; marker precision (correct
/// over resolved targets), marker key recall (keys hit over distinct cited
/// keys) and the diagnostic marker count ratio (resolved targets, capped per
/// paper at its cited-key occurrences, over those occurrences) are summed
/// over the papers that cite any key; percentiles
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
    let mut targets_capped = 0_u64;
    let mut cited_keys = 0_u64;
    let mut targets_resolved = 0_u64;
    let mut targets_correct = 0_u64;
    let mut keys_hit = 0_u64;
    let mut keys_cited = 0_u64;
    for p in ok.iter().filter(|p| p.truth_cited_keys > 0) {
        targets_capped += u64::from(p.resolved_targets.min(p.truth_cited_keys));
        cited_keys += u64::from(p.truth_cited_keys);
        targets_resolved += u64::from(p.resolved_targets);
        targets_correct += u64::from(p.marker_targets_correct);
        keys_hit += u64::from(p.marker_keys_hit);
        keys_cited += u64::from(p.marker_keys_cited);
    }

    let mean_of = |f: fn(&PaperEval) -> Option<f32>| -> Option<f32> {
        let values: Vec<f64> = ok.iter().copied().filter_map(f).map(f64::from).collect();
        if values.is_empty() {
            None
        } else {
            Some((values.iter().sum::<f64>() / values.len() as f64) as f32)
        }
    };
    let mean_body_alignment = mean_of(|p| p.body_alignment);
    let mean_body_alignment_raw = mean_of(|p| p.body_alignment_raw);
    let mut body_matched = 0_u64;
    let mut body_extracted = 0_u64;
    let mut body_truth = 0_u64;
    for p in ok.iter().filter(|p| p.body_alignment.is_some()) {
        body_matched += u64::from(p.body_words_matched);
        body_extracted += u64::from(p.body_words_extracted);
        body_truth += u64::from(p.body_words_truth);
    }

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
        title_accuracy: ratio(
            sum(|p| p.title_correct),
            sum(|p| p.title_truth).saturating_sub(sum(|p| p.title_not_applicable)),
        ),
        title_not_applicable: sum(|p| p.title_not_applicable) as u32,
        marker_resolution_rate: ratio(sum(|p| p.resolved_markers), sum(|p| p.extracted_markers)),
        marker_recall: ratio(targets_capped, cited_keys),
        marker_precision: ratio(targets_correct, targets_resolved),
        marker_key_recall: ratio(keys_hit, keys_cited),
        marker_command_ratio: ratio(cited_resolved, cited_commands),
        mean_body_alignment,
        mean_body_alignment_raw,
        body_word_recall: ratio(body_matched, body_truth),
        body_word_precision: ratio(body_matched, body_extracted),
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
        "| Title n/a (title-less style) | {} |",
        s.title_not_applicable
    );
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
        "| Marker precision (correct targets / resolved targets) | {} |",
        pct(s.marker_precision)
    );
    let _ = writeln!(
        out,
        "| Marker key recall (cited keys with a correct marker / cited keys) | {} |",
        pct(s.marker_key_recall)
    );
    let _ = writeln!(
        out,
        "| Marker count ratio (diagnostic, resolved targets/truth cited-key occurrences, \
         capped per paper) | {} |",
        pct(s.marker_recall)
    );
    let _ = writeln!(
        out,
        "| Body alignment (body text, markers/captions/math removed) | {} |",
        align_cell(s.mean_body_alignment)
    );
    let _ = writeln!(
        out,
        "| Body alignment raw (all page text vs truth body) | {} |",
        align_cell(s.mean_body_alignment_raw)
    );
    let _ = writeln!(
        out,
        "| Body word recall (matched/truth words) | {} |",
        pct(s.body_word_recall)
    );
    let _ = writeln!(
        out,
        "| Body word precision (matched/extracted words) | {} |",
        pct(s.body_word_precision)
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
         targets/cited keys | mk P/R | align | ms/chunk | warnings | title ✓/✗ | \
         authors c/t | paper doi ✓/✗/n/a | align raw | body words m/e/t |\n",
    );
    out.push_str(
        "| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | \
         --- | --- | --- | --- | --- | --- |\n",
    );
    for p in &report.papers {
        let exact = if p.ref_count_exact { "✓" } else { "✗" };
        let marker_pr = format!(
            "{}/{} {}/{}",
            p.marker_targets_correct, p.resolved_targets, p.marker_keys_hit, p.marker_keys_cited
        );
        let _ = writeln!(
            out,
            "| {} | {} | {} | {}/{}/{} | {} | {:.2} | {}/{}/{} | {}/{} | {}/{} | {} | {}/{} | {} | \
             {} | {:.1} | {} | {} | {}/{} | {} | {} | {}/{}/{} |",
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
            p.resolved_targets,
            p.truth_cited_keys,
            marker_pr,
            align_cell(p.body_alignment),
            p.ms_per_chunk,
            p.warnings,
            check_cell(p.paper_title_correct),
            p.authors_correct,
            p.authors_truth,
            check_cell(p.paper_doi_correct),
            align_cell(p.body_alignment_raw),
            p.body_words_matched,
            p.body_words_extracted,
            p.body_words_truth,
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
    /// `GroundTruth::method`: `bbl`, `bbl+bib`, `bib-cited` or `bib-all`.
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
    /// The extracted side of `body_alignment` (lines with a non-body role,
    /// the reference section, citation markers, captions and math-heavy
    /// lines removed), pages joined by [`DUMP_PAGE_SEPARATOR`], capped at
    /// [`BODY_TEXT_CAP`] bytes.
    #[serde(default)]
    pub body_text_extracted: String,
    /// The `LaTeX` body text (`GroundTruth::body_text`), capped at
    /// [`BODY_TEXT_CAP`] bytes.
    #[serde(default)]
    pub body_text_truth: String,
    /// Lines tagged with a role other than `body`, `heading` or empty, for
    /// diagnosing role tagging offline (see [`tagged_lines`]).
    #[serde(default)]
    pub tagged_lines: Vec<TaggedLine>,
    /// Count of every line role, including `body`, across `pages` (see
    /// [`role_counts`]).
    #[serde(default)]
    pub role_counts: BTreeMap<String, usize>,
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

/// The body-only text of [`body_only_text`] without recorded markers
/// (citation groups are still removed by pattern), pages joined by
/// [`DUMP_PAGE_SEPARATOR`], capped at [`BODY_TEXT_CAP`] bytes on a char
/// boundary.
pub fn body_text_extracted(pages: &[PageText]) -> String {
    body_text_with_markers(pages, &[])
}

/// [`body_text_extracted`] with the recorded `markers` also removed at
/// their char offsets: the extracted side of `body_alignment`, with the
/// dump's page separator.
fn body_text_with_markers(pages: &[PageText], markers: &[CitationMarker]) -> String {
    let mut out = body_only_text(pages, markers, DUMP_PAGE_SEPARATOR);
    truncate_on_char_boundary(&mut out, BODY_TEXT_CAP);
    out
}

/// Collects the truth, the extracted entries, the pairing from `eval`, the
/// markers, warnings, timings, reference-section text, the extracted and
/// truth paper metadata, the extracted and truth body texts, and the
/// tagged-line diagnostics ([`tagged_lines`], [`role_counts`]) for one
/// paper.
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
    let mut body_text_truth = truth.body_text.clone();
    truncate_on_char_boundary(&mut body_text_truth, BODY_TEXT_CAP);
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
        body_text_extracted: body_text_with_markers(&result.pages, &result.citations),
        body_text_truth,
        tagged_lines: tagged_lines(&result.pages),
        role_counts: role_counts(&result.pages),
    }
}

/// Max entries in [`PaperDump::tagged_lines`]; when there are more tagged
/// lines than this, only the first `TAGGED_LINES_CAP - 1` are kept and a
/// final marker entry takes the last slot (see [`tagged_lines`]).
pub const TAGGED_LINES_CAP: usize = 600;

/// Character cap on [`TaggedLine::text`] (a `char` count, so a truncated
/// multi-byte character is never split).
pub const TAGGED_LINE_TEXT_CHARS: usize = 160;

/// One line of [`PaperDump::tagged_lines`]: a line whose role was not
/// `body`, `heading` or empty.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaggedLine {
    pub page: u32,
    pub role: String,
    /// The line's text, truncated to [`TAGGED_LINE_TEXT_CHARS`] `char`s.
    pub text: String,
}

/// Truncates `s` to at most `max_chars` `char`s (never splits a multi-byte
/// `char`, unlike a byte-length cap).
fn truncate_chars(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

/// Every line of `pages` whose role is not `body`, `heading` or empty (see
/// [`is_body_role`]), in page order, text truncated with [`truncate_chars`]
/// to [`TAGGED_LINE_TEXT_CHARS`]. Capped at [`TAGGED_LINES_CAP`] entries:
/// when more lines are tagged, only the first `TAGGED_LINES_CAP - 1` are
/// kept and a final `{page: 0, role: "truncated", text: "<N more>"}` entry
/// (`N` the number of lines left out) takes the last slot.
pub fn tagged_lines(pages: &[PageText]) -> Vec<TaggedLine> {
    let mut tagged: Vec<(u32, &str, &str)> = Vec::new();
    for page in pages {
        for line in &page.lines {
            if !is_body_role(&line.role) {
                tagged.push((page.page, line.role.as_str(), line.text.as_str()));
            }
        }
    }
    let total = tagged.len();
    let keep = if total > TAGGED_LINES_CAP {
        TAGGED_LINES_CAP - 1
    } else {
        total
    };
    let mut out: Vec<TaggedLine> = Vec::with_capacity(keep + 1);
    for &(page, role, text) in &tagged[..keep] {
        out.push(TaggedLine {
            page,
            role: role.to_string(),
            text: truncate_chars(text, TAGGED_LINE_TEXT_CHARS),
        });
    }
    if total > keep {
        let more = total - keep;
        out.push(TaggedLine {
            page: 0,
            role: "truncated".to_string(),
            text: format!("<{more} more>"),
        });
    }
    out
}

/// Count of `line.role` for every line of `pages`, across every role
/// (including `body` and the empty role).
pub fn role_counts(pages: &[PageText]) -> BTreeMap<String, usize> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for page in pages {
        for line in &page.lines {
            *counts.entry(line.role.clone()).or_default() += 1;
        }
    }
    counts
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
                hash_ms: 0.0,
            },
        }
    }

    fn truth_with(references: Vec<TruthReference>, body_text: &str) -> GroundTruth {
        GroundTruth {
            references,
            citations: TruthCitations {
                cite_commands: 5,
                cited_keys: ["a", "b", "c", "a", "b"]
                    .iter()
                    .map(|k| (*k).to_string())
                    .collect(),
                nocite_keys: Vec::new(),
                nocite_all: false,
                cite_only_author_year: 1,
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

    /// The exact LCS length by [`lcs_bounded`] with no memory budget.
    fn lcs_len(left: &[u32], right: &[u32]) -> usize {
        lcs_bounded(left, right, usize::MAX).map_or(0, |(matched, _)| matched)
    }

    #[test]
    fn lcs_len_small_cases() {
        assert_eq!(lcs_len(&[1, 2, 3, 4], &[2, 4]), 2);
        assert_eq!(lcs_len(&[1, 2, 3], &[3, 2, 1]), 1);
        assert_eq!(lcs_len(&[], &[1]), 0);
        assert_eq!(lcs_len(&[7, 8, 9], &[7, 8, 9]), 3);
        assert_eq!(lcs_len(&[5, 6], &[7, 8]), 0);
    }

    /// Reference LCS: the two-row dynamic programme, `O(n * m)`.
    fn dp_lcs_len(left: &[u32], right: &[u32]) -> usize {
        let mut prev = vec![0_usize; right.len() + 1];
        let mut cur = vec![0_usize; right.len() + 1];
        for &token in left {
            for (j, &other) in right.iter().enumerate() {
                cur[j + 1] = if token == other {
                    prev[j] + 1
                } else {
                    cur[j].max(prev[j + 1])
                };
            }
            std::mem::swap(&mut prev, &mut cur);
        }
        prev[right.len()]
    }

    /// `len` pseudo-random tokens below `alphabet` from a fixed LCG seed.
    fn lcg_tokens(seed: u64, len: usize, alphabet: u64) -> Vec<u32> {
        let mut state = seed;
        let mut out: Vec<u32> = Vec::with_capacity(len);
        for _ in 0..len {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            out.push(((state >> 33) % alphabet) as u32);
        }
        out
    }

    #[test]
    fn lcs_len_bit_parallel_matches_dp() {
        // A full carry chain across four words.
        let same = vec![3_u32; 200];
        assert_eq!(lcs_len(&same, &same), 200);
        assert_eq!(lcs_len(&same, &same[..130]), 130);
        let mut seed = 1_u64;
        for len in [1_usize, 2, 63, 64, 65, 127, 128, 129, 200] {
            for other in [1_usize, 31, 64, 65, 129, 190] {
                for alphabet in [2_u64, 3, 4, 50] {
                    seed += 1;
                    let left = lcg_tokens(seed, len, alphabet);
                    let right = lcg_tokens(seed * 7 + 3, other, alphabet);
                    let expected = dp_lcs_len(&left, &right);
                    assert_eq!(lcs_len(&left, &right), expected, "{len} {other} {alphabet}");
                    assert_eq!(lcs_len(&right, &left), expected, "{other} {len} {alphabet}");
                }
            }
        }
    }

    #[test]
    fn lcs_builds_mask_rows_only_for_tokens_in_both_sides() {
        // The right side is 5,000 distinct tokens; the left side shares
        // only 100 of them (every 50th position) among 700 others.
        let right: Vec<u32> = (0..5_000).collect();
        let left: Vec<u32> = (0..5_000_u32)
            .map(|i| if i % 50 == 0 { i } else { 10_000 + i % 700 })
            .collect();
        let expected = dp_lcs_len(&left, &right);
        assert_eq!(expected, 100);
        for (a, b) in [(&left, &right), (&right, &left)] {
            let (matched, stats) = lcs_bounded(a, b, LCS_MAX_WORDS).unwrap();
            assert_eq!(matched, expected);
            assert_eq!(stats, LcsStats { rows_built: 100 });
        }
        // 100 rows of 79 words exceed a 7,899-word budget: nothing built.
        assert!(lcs_bounded(&left, &right, 7_899).is_none());
        assert!(lcs_bounded(&left, &right, 7_900).is_some());
        // The fallback length always fits the budget.
        let fallback = std::hint::black_box(LCS_FALLBACK_TOKENS);
        assert!(fallback * fallback.div_ceil(64) <= LCS_MAX_WORDS);
    }

    #[test]
    fn align_counts_is_exact_above_the_old_sampling_cap() {
        // 13,000 words against the same words with one extra word inserted
        // after every 100th: every original word stays matched in order.
        let original: Vec<String> = (0..13_000).map(|i| format!("w{}", i % 997)).collect();
        let mut edited: Vec<String> = Vec::new();
        for (i, word) in original.iter().enumerate() {
            edited.push(word.clone());
            if i % 100 == 99 {
                edited.push("inserted".to_string());
            }
        }
        // `w123`-style tokens mix digits and letters, so they go through
        // the plain tokenizer (`align_counts` would drop them as math).
        let counts = token_counts(&original.join(" "), &edited.join(" "), words);
        assert_eq!(
            counts,
            AlignCounts {
                left: 13_000,
                right: 13_130,
                matched: 13_000,
            }
        );
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

    fn titled(key: &str, title: &str, author: &str, year: u16) -> TruthReference {
        let mut r = truth_ref(key);
        r.title = Some(title.to_string());
        r.authors = vec![author.to_string()];
        r.year = Some(year);
        r
    }

    fn extracted_titled(index: u32, title: &str, author: &str, year: u16) -> ReferenceEntry {
        let mut e = extracted(index);
        e.title = Some(title.to_string());
        e.authors = vec![author.to_string()];
        e.year = Some(year);
        e
    }

    #[test]
    fn match_references_exact_title_prefers_same_year() {
        // arxiv 2510.00443: Szegő's `Orthogonal Polynomials` (1939) took
        // Case's `Orthogonal polynomials. II.` (1975), whose title was cut to
        // `Orthogonal polynomials`, because it came first.
        let szego = titled(
            "szego1939orthogonal",
            "Orthogonal Polynomials",
            "G. Szegő",
            1939,
        );
        let case = titled(
            "case1975orthogonal",
            "Orthogonal polynomials",
            "K. M. Case",
            1975,
        );
        let e7 = extracted_titled(7, "Orthogonal polynomials", "K. M. Case", 1975);
        let e8 = extracted_titled(8, "Orthogonal polynomials", "G. Szego", 1939);

        let matches = match_references(&[szego, case], &[e7, e8]);
        assert_eq!(matches[0].extracted_index, Some(8));
        assert_eq!(matches[0].method, "title");
        assert_eq!(matches[1].extracted_index, Some(7));
        assert_eq!(matches[1].method, "title");
    }

    #[test]
    fn match_references_exact_title_single_candidate_ignores_year() {
        let truth = titled("a", "A Study of Things", "J. Smith", 2019);
        let e3 = extracted_titled(3, "A study of things", "J. Doe", 2021);

        let matches = match_references(&[truth], &[e3]);
        assert_eq!(matches[0].extracted_index, Some(3));
        assert_eq!(matches[0].method, "title");
        assert!(close(matches[0].score, 1.0));
    }

    #[test]
    fn match_references_title_year_tie_broken_by_first_author() {
        let truth = titled("muller", "Neural Networks", "Müller, K.", 2020);
        let e1 = extracted_titled(1, "Neural networks", "J. Smith", 2020);
        let e2 = extracted_titled(2, "Neural networks", "K. Muller", 2020);

        let matches = match_references(&[truth], &[e1, e2]);
        assert_eq!(matches[0].extracted_index, Some(2));
        assert_eq!(matches[0].method, "title");

        // The same tie in the fuzzy title pass (Jaccard 5/6 on both).
        let fuzzy = titled(
            "muller",
            "Deep neural networks for vision",
            "Müller, K.",
            2020,
        );
        let f1 = extracted_titled(
            1,
            "Deep neural networks for speech vision",
            "J. Smith",
            2020,
        );
        let f2 = extracted_titled(
            2,
            "Deep neural networks for audio vision",
            "K. Muller",
            2020,
        );
        let matches = match_references(&[fuzzy], &[f1, f2]);
        assert_eq!(matches[0].extracted_index, Some(2));
        assert_eq!(matches[0].method, "title");
        assert!(close(matches[0].score, 5.0 / 6.0));
    }

    #[test]
    fn title_digit_runs_conflict_only_on_both_sides() {
        // Different model versions, the month swallowed on the extracted side.
        assert!(digits_conflict(
            &title_digit_runs("Gemini 3 Flash Model Card"),
            &title_digit_runs("Gemini 2 flash model card, 04 2025"),
        ));
        assert!(digits_conflict(
            &title_digit_runs("Model card v1"),
            &title_digit_runs("Model card v2")
        ));
        // Extra digits on one side only: month, year, pages, lost superscript.
        assert!(!digits_conflict(
            &title_digit_runs("Gemini 3 Flash Model Card"),
            &title_digit_runs("Gemini 3 flash model card, 12 2025"),
        ));
        assert!(!digits_conflict(
            &title_digit_runs("Qwen3.5: Towards Native Multimodal Agents"),
            &title_digit_runs("Qwen3.5: Towards native multimodal agents, 2 2026"),
        ));
        assert!(!digits_conflict(
            &title_digit_runs("F^3Net: fusion, feedback and focus"),
            &title_digit_runs("F net: fusion, feedback and focus"),
        ));
        // Years are ignored; subscripts are NFKC-folded; leading zeros dropped.
        assert!(!digits_conflict(
            &title_digit_runs("Proceedings of the 25th Conference"),
            &title_digit_runs("Ngwe D (2024) Using gpt for market research"),
        ));
        assert!(!digits_conflict(
            &title_digit_runs("NH₃ decomposition"),
            &title_digit_runs("NH3 decomposition")
        ));
        assert!(!digits_conflict(
            &title_digit_runs("Part 04"),
            &title_digit_runs("Part 4")
        ));
    }

    #[test]
    fn match_references_rejects_other_version_titles() {
        // arxiv 2510.26824: "Gemini 3 Flash Model Card" (2025) was paired by
        // author-year with the first "Google DeepMind" 2025 entry, the
        // Gemini 2 card; it must take the Gemini 3 card instead.
        let mut gemini = truth_ref("Gemini3FlashModelCard2025");
        gemini.title = Some("Gemini 3 Flash Model Card".to_string());
        gemini.authors = vec!["Google DeepMind".to_string()];
        gemini.year = Some(2025);
        gemini.text = "Google DeepMind. Gemini 3 Flash Model Card. 2025".to_string();
        // Fuzzy title (Jaccard 10/12) with another version number.
        let mut llama = truth_ref("llama");
        llama.title =
            Some("Llama 2 open foundation and fine tuned chat models for everyone".to_string());
        // Text pass (Jaccard 4/6) with another version number.
        let mut card = truth_ref("card");
        card.title = Some("Model card v1".to_string());
        card.text = "Acme. Model card v1. 2024.".to_string();

        let mut e3 = extracted(3);
        e3.raw = "[3] Google DeepMind. Gemini 2 flash model card, 04 2025. Published April 2025."
            .to_string();
        e3.title = Some("Gemini 2 flash model card, 04 2025".to_string());
        e3.authors = vec!["Google DeepMind".to_string()];
        e3.year = Some(2025);
        let mut e7 = extracted(7);
        e7.title =
            Some("Llama 3 open foundation and fine tuned chat models for everyone".to_string());
        let mut e8 = extracted(8);
        e8.title = Some("Model card v2".to_string());
        e8.raw = "[8] Acme. Model card v2. 2024.".to_string();
        let mut e44 = extracted(44);
        e44.raw = "[44] Google DeepMind. Gemini 3 flash model card, 12 2025. Published December \
                   2025."
            .to_string();
        e44.title = Some("Gemini 3 flash model card, 12 2025".to_string());
        e44.authors = vec!["Google DeepMind".to_string()];
        e44.year = Some(2025);

        let matches = match_references(&[gemini, llama, card], &[e3, e7, e8, e44]);
        assert_eq!(matches[0].extracted_index, Some(44));
        assert_eq!(matches[0].method, "author-year");
        assert_eq!(matches[1].extracted_index, None);
        assert_eq!(matches[1].method, "none");
        assert_eq!(matches[2].extracted_index, None);
        assert_eq!(matches[2].method, "none");
    }

    #[test]
    fn match_references_pairs_title_less_entries_by_author_and_venue() {
        // RSC style prints no title: `B. Keimer, S. A. Kivelson, M. R. Norman,
        // S. Uchida and J. Zaanen, Nature, 2015, 518, 179–186.` An earlier
        // entry by the same first author and year must not take the pair.
        let mut keimer = truth_ref("keimer2015");
        keimer.title = Some(
            "From Quantum Matter to High-Temperature Superconductivity in Copper Oxides"
                .to_string(),
        );
        keimer.authors = vec![
            "B. Keimer".to_string(),
            "S. A. Kivelson".to_string(),
            "M. R. Norman".to_string(),
            "S. Uchida".to_string(),
            "J. Zaanen".to_string(),
        ];
        keimer.year = Some(2015);
        keimer.text = "B. Keimer, S. A. Kivelson, M. R. Norman, S. Uchida, J. Zaanen. From \
                       Quantum Matter to High-Temperature Superconductivity in Copper Oxides. \
                       Nature 2015"
            .to_string();
        // `.bbl` truth carries volume and pages in its text; the extracted
        // venue abbreviation does not appear there.
        let mut zaitsev = truth_ref("zaitsev2015");
        zaitsev.title = Some("Motion artifacts in MRI: A review".to_string());
        zaitsev.authors = vec!["M. Zaitsev".to_string()];
        zaitsev.year = Some(2015);
        zaitsev.text = "M. Zaitsev, J. Maclaren, and M. Herbst, \"Motion artifacts in MRI: A \
                        review,\" NMR in Biomedicine, vol. 28, no. 7, pp. 911–935, 2015."
            .to_string();

        let mut e1 = extracted(1);
        e1.authors = vec!["B. Keimer".to_string(), "A. Other".to_string()];
        e1.venue = Some("Science".to_string());
        e1.year = Some(2015);
        e1.volume = Some("347".to_string());
        e1.pages = Some("12–15".to_string());
        let mut e2 = extracted(2);
        e2.authors = vec!["M. Zaitsev".to_string()];
        e2.venue = Some("Phys. Rev. B".to_string());
        e2.year = Some(2015);
        e2.volume = Some("91".to_string());
        e2.pages = Some("1–9".to_string());
        let mut e3 = extracted(3);
        e3.authors = vec![
            "B. Keimer".to_string(),
            "S. A. Kivelson".to_string(),
            "M. R. Norman".to_string(),
            "S. Uchida".to_string(),
            "J. Zaanen".to_string(),
        ];
        e3.venue = Some("Nature".to_string());
        e3.year = Some(2015);
        e3.volume = Some("518".to_string());
        e3.pages = Some("179–186".to_string());
        let mut e4 = extracted(4);
        e4.authors = vec!["M. Zaitsev".to_string()];
        e4.venue = Some("NMR Biomed.".to_string());
        e4.year = Some(2015);
        e4.volume = Some("28".to_string());
        e4.pages = Some("911–935".to_string());

        let matches = match_references(&[keimer, zaitsev], &[e1, e2, e3, e4]);
        // Venue plus five surnames.
        assert_eq!(matches[0].extracted_index, Some(3));
        assert_eq!(matches[0].method, "author-venue");
        assert!(close(matches[0].score, AUTHOR_VENUE_SCORE));
        // Volume and first page.
        assert_eq!(matches[1].extracted_index, Some(4));
        assert_eq!(matches[1].method, "author-venue");
    }

    #[test]
    fn match_references_author_venue_needs_truth_year_and_no_extracted_title() {
        let mut no_year = truth_ref("keimer");
        no_year.authors = vec!["B. Keimer".to_string(), "S. A. Kivelson".to_string()];
        no_year.text = "B. Keimer, S. A. Kivelson. Copper oxides. Nature".to_string();
        let mut titled_truth = truth_ref("lee");
        titled_truth.authors = vec!["P. A. Lee".to_string(), "N. Nagaosa".to_string()];
        titled_truth.year = Some(2006);
        titled_truth.text = "P. A. Lee, N. Nagaosa. Doping a Mott insulator. Rev. Mod. Phys. \
                             2006, 78, 17"
            .to_string();

        let mut e1 = extracted(1);
        e1.authors = vec!["B. Keimer".to_string(), "S. A. Kivelson".to_string()];
        e1.venue = Some("Nature".to_string());
        e1.year = Some(2015);
        // Venue, volume and page agree, but a titled extracted entry is left
        // to the title and author-year passes.
        let mut e2 = extracted(2);
        e2.title = Some("Something else entirely".to_string());
        e2.authors = vec!["P. A. Lee".to_string(), "N. Nagaosa".to_string()];
        e2.venue = Some("Rev. Mod. Phys.".to_string());
        e2.year = Some(2006);
        e2.volume = Some("78".to_string());
        e2.pages = Some("17–85".to_string());

        let matches = match_references(&[no_year, titled_truth], &[e1, e2]);
        assert_eq!(matches[0].extracted_index, None);
        assert_eq!(matches[0].method, "none");
        assert_eq!(matches[1].extracted_index, Some(2));
        assert_eq!(matches[1].method, "author-year");
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
        assert_eq!(eval.truth_cited_keys, 5);
        assert_eq!(eval.truth_author_year_only, 1);
        assert_eq!(eval.resolved_targets, 1);
        let command_ratio = eval.marker_command_ratio.expect("cite commands");
        assert!(close(command_ratio, 0.2), "got {command_ratio}");
        // `[1]` resolves to e1, matched to the cited key `a`.
        assert_eq!(eval.marker_targets_correct, 1);
        assert_eq!(eval.marker_keys_hit, 1);
        assert_eq!(eval.marker_keys_cited, 3);
        let alignment = eval.body_alignment.expect("body text present");
        assert!(close(alignment, 1.0), "got {alignment}");
        let raw = eval.body_alignment_raw.expect("body text present");
        assert!(close(raw, 1.0), "got {raw}");
        // The stale `[1]` marker at offset 4 does not match the page text there
        // and removes nothing.
        assert_eq!(eval.body_words_extracted, 9);
        assert_eq!(eval.body_words_truth, 9);
        assert_eq!(eval.body_words_matched, 9);
        assert!((eval.ms_total - 20.0).abs() < 1e-9);
        assert!((eval.ms_per_chunk - 20.0).abs() < 1e-9);
        assert_eq!(eval.chunks, 1);
        assert_eq!(eval.warnings, 1);
        assert_eq!(eval.matches.len(), 3);
        assert_eq!(eval.matches[0].method, "doi");
        assert_eq!(eval.matches[1].method, "arxiv");
        assert_eq!(eval.matches[2].method, "none");
    }

    /// A matched entry without a title whose raw text is a title-less style
    /// (RSC `…, Nature, 2015, 518, 179–186.`) leaves the title-accuracy
    /// denominator; one whose raw text is not still counts as a miss.
    #[test]
    fn title_accuracy_skips_titleless_styles() {
        let mut truth_refs: Vec<TruthReference> = Vec::new();
        for i in 0..3_u16 {
            let mut r = truth_ref(&format!("k{i}"));
            r.doi = Some(format!("10.1000/ref{i}"));
            r.year = Some(2000 + i);
            r.title = Some(format!("Distinct title number {i}"));
            truth_refs.push(r);
        }
        let mut e1 = extracted(1);
        e1.doi = Some("10.1000/ref0".to_string());
        e1.year = Some(2000);
        e1.title = Some("Distinct title number 0".to_string());
        let mut e2 = extracted(2);
        e2.doi = Some("10.1000/ref1".to_string());
        e2.year = Some(2001);
        e2.venue = Some("Nature".to_string());
        e2.raw = "J. Smith and K. Lee, Nature, 2001, 518, 179–186.".to_string();
        let mut e3 = extracted(3);
        e3.doi = Some("10.1000/ref2".to_string());
        e3.year = Some(2002);
        let result = sample_result(vec![e1, e2, e3], Vec::new());
        let truth = truth_with(truth_refs, "");

        let eval = evaluate("p", &result, &truth);
        assert_eq!(eval.matched_refs, 3);
        assert_eq!(eval.title_truth, 3);
        assert_eq!(eval.title_correct, 1);
        assert_eq!(eval.title_not_applicable, 1);

        let s = summarize(std::slice::from_ref(&eval));
        assert_eq!(s.title_not_applicable, 1);
        assert!(close(s.title_accuracy, 0.5), "{}", s.title_accuracy);
        let md = render_markdown(&build_report("lopdf", "h", vec![eval]));
        assert!(md.contains("| Title accuracy | 50.0% |\n| Title n/a (title-less style) | 1 |\n"));
    }

    /// Title-less style is judged from the raw text, not from a missing
    /// title: an RSC entry without a title is n/a, while an ordinary entry
    /// whose (over-long) title was left unset still counts as a miss even
    /// though its venue and year were parsed.
    #[test]
    fn title_not_applicable_needs_titleless_raw_text() {
        let long_title = "A very long title ".repeat(30);
        let mut truth_refs: Vec<TruthReference> = Vec::new();
        for i in 0..2_u16 {
            let mut r = truth_ref(&format!("k{i}"));
            r.doi = Some(format!("10.1000/ref{i}"));
            r.year = Some(2013 + i);
            r.title = Some(format!("Distinct title number {i}"));
            truth_refs.push(r);
        }
        let mut rsc = extracted(1);
        rsc.doi = Some("10.1000/ref0".to_string());
        rsc.year = Some(2013);
        rsc.venue = Some("Chem. Soc. Rev.".to_string());
        rsc.raw =
            "1 Q. Zhang, E. Uchaker and G. Cao, Chem. Soc. Rev., 2013, 42, 3127–3171.".to_string();
        let mut failed = extracted(2);
        failed.doi = Some("10.1000/ref1".to_string());
        failed.year = Some(2014);
        failed.venue = Some("Journal".to_string());
        failed.raw = format!("A. Author. {}. Journal 12, 1–2 (2014).", long_title.trim());
        let result = sample_result(vec![rsc, failed], Vec::new());
        let truth = truth_with(truth_refs, "");

        let eval = evaluate("p", &result, &truth);
        assert_eq!(eval.matched_refs, 2);
        assert_eq!(eval.title_truth, 2);
        assert_eq!(eval.title_correct, 0);
        assert_eq!(eval.title_not_applicable, 1);
        let s = summarize(std::slice::from_ref(&eval));
        assert!(close(s.title_accuracy, 0.0), "{}", s.title_accuracy);
    }

    #[test]
    fn titleless_raw_accepts_only_rsc_style() {
        assert!(titleless_raw(
            "Q. Zhang, E. Uchaker and G. Cao, Chem. Soc. Rev., 2013, 42, 3127–3171."
        ));
        assert!(titleless_raw(
            "[3] J.-P. Sauvage, Nature, 2015, 518, 179–186."
        ));
        assert!(titleless_raw(
            "A. B. Smith and P. G. de Gennes, Angew. Chem., Int. Ed., 2010, 49, 1–5."
        ));
        assert!(titleless_raw(
            "12. K. Lee, M. Park, et al., J. Am. Chem. Soc., 2019, 141(3), 100."
        ));
        // Ordinary styles whose title parsing failed.
        assert!(!titleless_raw(
            "A. Author. A very long title about things. Journal 12, 1–2 (2020)."
        ));
        assert!(!titleless_raw(
            "A. Author, \u{201c}A quoted title,\u{201d} Nature, 2015, 518, 179–186."
        ));
        assert!(!titleless_raw(
            "A. Author, A study of the effects of things, Nature, 2015, 518, 179–186."
        ));
        assert!(!titleless_raw("Smith, J., Nature, 2015, 518, 179–186."));
        assert!(!titleless_raw("Q. Zhang, 2013, 42, 3127–3171."));
        assert!(!titleless_raw(""));
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
        truth.citations.cited_keys.clear();
        let eval = evaluate("x", &result, &truth);
        assert!(eval.marker_recall.is_none());
        assert!(eval.marker_command_ratio.is_none());
        assert_eq!(eval.truth_cited_keys, 0);
    }

    #[test]
    fn marker_recall_counts_cited_references_not_marker_groups() {
        let marker = |text: &str, targets: Vec<u32>| CitationMarker {
            page: 1,
            offset: 0,
            text: text.to_string(),
            targets,
        };
        // `\cite{a,b}` printed as `[1], [2]` (two markers) plus `\cite{c,d,e}`
        // printed as `[3-5]` (one marker): five cited keys, five targets.
        let split = vec![
            marker("[1]", vec![1]),
            marker("[2]", vec![2]),
            marker("[3-5]", vec![3, 4, 5]),
        ];
        let mut truth = truth_with(Vec::new(), "");
        truth.citations.cite_commands = 2;
        truth.citations.cited_keys = ["a", "b", "c", "d", "e"]
            .iter()
            .map(|k| (*k).to_string())
            .collect();
        let eval = evaluate("x", &sample_result(Vec::new(), split), &truth);
        assert_eq!(eval.resolved_targets, 5);
        assert_eq!(eval.truth_cited_keys, 5);
        let recall = eval.marker_recall.expect("keys cited");
        assert!(close(recall, 1.0), "got {recall}");
        // The old per-marker ratio overcounts the split command: 3 / 2.
        let old = eval.marker_command_ratio.expect("commands");
        assert!(close(old, 1.5), "got {old}");
        // No extracted references are matched, so no target is correct.
        assert_eq!(eval.marker_targets_correct, 0);
        assert_eq!(eval.marker_keys_hit, 0);
        assert_eq!(eval.marker_keys_cited, 5);

        // More targets than cited keys (false markers) is capped at 1.
        let noisy: Vec<CitationMarker> = (1..=8).map(|i| marker("[1]", vec![i])).collect();
        let over = evaluate("y", &sample_result(Vec::new(), noisy), &truth);
        assert_eq!(over.resolved_targets, 8);
        let capped = over.marker_recall.expect("keys cited");
        assert!(close(capped, 1.0), "got {capped}");

        // Unresolved markers add nothing.
        let unresolved = vec![marker("[1]", vec![1]), marker("[9]", Vec::new())];
        let partial = evaluate("z", &sample_result(Vec::new(), unresolved), &truth);
        let low = partial.marker_recall.expect("keys cited");
        assert!(close(low, 0.2), "got {low}");

        // The corpus figure caps each paper before summing: (5 + 1) / (5 + 5).
        let s = summarize(&[over, partial]);
        assert!(close(s.marker_recall, 0.6), "{}", s.marker_recall);
        assert!(
            close(s.marker_command_ratio, 9.0 / 4.0),
            "{}",
            s.marker_command_ratio
        );
        assert!(close(s.marker_precision, 0.0), "{}", s.marker_precision);
        assert!(close(s.marker_key_recall, 0.0), "{}", s.marker_key_recall);
    }

    /// Truth `a` (cited) and `z` (not cited); e1 matches `z` and e2 matches
    /// `a` by DOI; one `[n]` marker with the given targets.
    fn wrong_or_right_marker(targets: Vec<u32>) -> PaperEval {
        let mut ref_a = truth_ref("a");
        ref_a.doi = Some("10.1/a".to_string());
        let mut ref_z = truth_ref("z");
        ref_z.doi = Some("10.1/z".to_string());
        let mut e1 = extracted(1);
        e1.doi = Some("10.1/z".to_string());
        let mut e2 = extracted(2);
        e2.doi = Some("10.1/a".to_string());
        let marker = CitationMarker {
            page: 1,
            offset: 0,
            text: "[n]".to_string(),
            targets,
        };
        let mut truth = truth_with(vec![ref_a, ref_z], "");
        truth.citations.cite_commands = 1;
        truth.citations.cited_keys = vec!["a".to_string()];
        let eval = evaluate("x", &sample_result(vec![e1, e2], vec![marker]), &truth);
        assert_eq!(eval.matches[0].extracted_index, Some(2));
        assert_eq!(eval.matches[1].extracted_index, Some(1));
        eval
    }

    #[test]
    fn marker_resolving_to_the_wrong_entry_is_not_correct() {
        // `\cite{a}` printed as `[1]`, but entry 1 is `z` (never cited).
        let eval = wrong_or_right_marker(vec![1]);
        assert_eq!(eval.resolved_targets, 1);
        assert_eq!(eval.marker_targets_correct, 0);
        assert_eq!(eval.marker_keys_hit, 0);
        assert_eq!(eval.marker_keys_cited, 1);
        // The occurrence-based diagnostic still reports 100%.
        let count_ratio = eval.marker_recall.expect("keys cited");
        assert!(close(count_ratio, 1.0), "got {count_ratio}");
        let s = summarize(&[eval]);
        assert!(close(s.marker_precision, 0.0), "{}", s.marker_precision);
        assert!(close(s.marker_key_recall, 0.0), "{}", s.marker_key_recall);
        assert!(close(s.marker_recall, 1.0), "{}", s.marker_recall);
    }

    #[test]
    fn marker_resolving_to_the_right_entry_is_correct() {
        let eval = wrong_or_right_marker(vec![2]);
        assert_eq!(eval.resolved_targets, 1);
        assert_eq!(eval.marker_targets_correct, 1);
        assert_eq!(eval.marker_keys_hit, 1);
        assert_eq!(eval.marker_keys_cited, 1);
        let s = summarize(&[eval]);
        assert!(close(s.marker_precision, 1.0), "{}", s.marker_precision);
        assert!(close(s.marker_key_recall, 1.0), "{}", s.marker_key_recall);
        // A second target at the same key counts once towards key recall;
        // an unmatched index is a resolved but wrong target.
        let eval = wrong_or_right_marker(vec![2, 2, 7]);
        assert_eq!(eval.resolved_targets, 3);
        assert_eq!(eval.marker_targets_correct, 2);
        assert_eq!(eval.marker_keys_hit, 1);
        let s = summarize(&[eval]);
        assert!(
            close(s.marker_precision, 2.0 / 3.0),
            "{}",
            s.marker_precision
        );
        assert!(close(s.marker_key_recall, 1.0), "{}", s.marker_key_recall);
    }

    #[test]
    fn evaluate_without_body_text_has_no_alignment() {
        let result = sample_result(Vec::new(), Vec::new());
        let truth = truth_with(Vec::new(), "");
        let eval = evaluate("x", &result, &truth);
        assert!(eval.body_alignment.is_none());
        assert!(eval.body_alignment_raw.is_none());
        assert_eq!(eval.body_words_truth, 0);
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
        p1.truth_cited_keys = 40;
        p1.resolved_targets = 10;
        p1.marker_targets_correct = 6;
        p1.marker_keys_hit = 12;
        p1.marker_keys_cited = 20;
        p1.body_alignment = Some(0.8);
        p1.body_alignment_raw = Some(0.5);
        p1.body_words_extracted = 100;
        p1.body_words_truth = 80;
        p1.body_words_matched = 60;
        let mut p2 = paper_with("p2", 10.0);
        p2.truth_refs = 10;
        p2.extracted_refs = 12;
        p2.matched_refs = 10;
        p2.ref_count_exact = false;
        p2.body_alignment = Some(0.6);
        p2.body_alignment_raw = Some(0.3);
        p2.body_words_extracted = 100;
        p2.body_words_truth = 120;
        p2.body_words_matched = 90;
        // No truth cite commands: excluded from the marker metrics entirely.
        p2.extracted_markers = 4;
        p2.resolved_markers = 4;
        p2.resolved_targets = 4;
        let mut p3 = paper_with("p3", 30.0);
        p3.truth_refs = 5;
        p3.extracted_refs = 5;
        p3.matched_refs = 5;
        let p4 = paper_with("p4", 20.0);
        let p5 = paper_with("p5", 40.0);
        let mut failed = failed_paper("p6", "boom");
        failed.truth_cite_commands = 100;
        failed.truth_cited_keys = 100;
        failed.resolved_targets = 100;
        failed.marker_targets_correct = 100;
        failed.marker_keys_hit = 100;
        failed.marker_keys_cited = 100;
        failed.body_words_truth = 1000;

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
        // Only p1 cites keys (p2's four targets are left out).
        assert!(close(s.marker_precision, 0.6), "{}", s.marker_precision);
        assert!(close(s.marker_key_recall, 0.6), "{}", s.marker_key_recall);
        assert!(
            close(s.marker_command_ratio, 0.25),
            "{}",
            s.marker_command_ratio
        );
        let mean = s.mean_body_alignment.expect("two alignments");
        assert!(close(mean, 0.7), "{mean}");
        let mean_raw = s.mean_body_alignment_raw.expect("two raw alignments");
        assert!(close(mean_raw, 0.4), "{mean_raw}");
        assert!(
            close(s.body_word_recall, 150.0 / 200.0),
            "{}",
            s.body_word_recall
        );
        assert!(
            close(s.body_word_precision, 150.0 / 200.0),
            "{}",
            s.body_word_precision
        );
    }

    #[test]
    fn summarize_empty_and_all_failed() {
        let s = summarize(&[]);
        assert_eq!(s.papers, 0);
        assert!(close(s.ref_recall, 0.0));
        assert!(s.mean_body_alignment.is_none());
        assert!(s.mean_body_alignment_raw.is_none());
        assert!(close(s.body_word_recall, 0.0));
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
        p1.truth_cited_keys = 10;
        p1.resolved_targets = 5;
        p1.marker_recall = Some(0.5);
        p1.marker_targets_correct = 3;
        p1.marker_keys_hit = 2;
        p1.marker_keys_cited = 4;
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
        assert!(md.contains("| truth cites | targets/cited keys | mk P/R | align |"));
        assert!(md.contains(
            "| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |\n"
        ));
        assert!(md.contains("| arxiv:2108.04588 | complete | 0 | 31/31/30 | ✓ | 0.00 | 0/0/0 |"));
        assert!(md.contains("| ext/truth | doi c/t/printed | year c/t |"));
        assert!(md.contains("| DOI accuracy (of printed DOIs) | 0.0% |"));
        assert!(md.contains("## Stage timings (mean ms per document)\n"));
        assert!(md.contains("| 4/6 | 8 | 5/10 | 3/5 2/4 | 0.912 | 12.5 | 0 |"));
        assert!(md.contains("| 0/0 | 0 | 0/0 | 0/0 0/0 | n/a | 0.0 | 0 |"));
        assert!(md.contains("| Marker resolution (precision-like, resolved/extracted) | 66.7% |"));
        assert!(md.contains("| Marker precision (correct targets / resolved targets) | 60.0% |"));
        assert!(md.contains(
            "| Marker key recall (cited keys with a correct marker / cited keys) | 50.0% |"
        ));
        assert!(md.contains(
            "| Marker count ratio (diagnostic, resolved targets/truth cited-key occurrences, \
             capped per paper) | 50.0% |"
        ));
        assert!(!md.contains("| Marker recall"));
        assert!(!md.contains("marker recall"));
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

    #[test]
    fn report_without_marker_correctness_fields_still_loads() {
        let report = build_report("lopdf", "host", vec![paper_with("p", 1.0)]);
        let mut value = serde_json::to_value(&report).unwrap();
        let paper = value["papers"][0].as_object_mut().unwrap();
        for key in [
            "marker_targets_correct",
            "marker_keys_hit",
            "marker_keys_cited",
        ] {
            assert!(paper.remove(key).is_some(), "{key}");
        }
        let summary = value["summary"].as_object_mut().unwrap();
        for key in ["marker_precision", "marker_key_recall"] {
            assert!(summary.remove(key).is_some(), "{key}");
        }
        let old: CorpusReport = serde_json::from_value(value).unwrap();
        assert_eq!(old, report);
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
                role: "body".to_string(),
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
        let truth = truth_with(vec![ref_a, ref_b], "Intro text.");
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
        assert_eq!(
            dump.body_text_extracted,
            "Intro\nReferences\nnot the real section\n\u{c}\nBody text\n"
        );
        assert_eq!(dump.body_text_truth, "Intro text.");
        // All lines built by `lined_page` are role "body": nothing tagged,
        // and role_counts has just the one key (3 + 3 + 1 lines).
        assert_eq!(dump.tagged_lines, Vec::new());
        assert_eq!(dump.role_counts.len(), 1);
        assert_eq!(dump.role_counts["body"], 7);
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
        object.remove("body_text_extracted");
        object.remove("body_text_truth");
        object.remove("tagged_lines");
        object.remove("role_counts");
        let old: PaperDump = serde_json::from_value(value).unwrap();
        assert_eq!(old.metadata, Metadata::default());
        assert_eq!(old.paper_truth, TruthPaper::default());
        assert_eq!(old.body_text_extracted, "");
        assert_eq!(old.body_text_truth, "");
        assert_eq!(old.tagged_lines, Vec::new());
        assert_eq!(old.role_counts, BTreeMap::new());
    }

    #[test]
    fn dump_paper_collects_tagged_lines_and_role_counts_across_pages() {
        let roles = ["heading", "body", "figure", "table", "body", "toc", "front"];
        let page1_lines: Vec<(&str, &str)> = ROLED_LINES.iter().copied().zip(roles).collect();
        let page1 = roled_page(1, ROLED_TEXT, &page1_lines);

        let long = "\u{e9}".repeat(200);
        let page2_lines = [(long.as_str(), "figure"), ("Untagged line", "")];
        let page2 = roled_page(2, "Untagged line", &page2_lines);

        let mut result = sample_result(Vec::new(), Vec::new());
        result.pages = vec![page1, page2];
        let truth = truth_with(Vec::new(), "");
        let eval = evaluate("roles", &result, &truth);
        let dump = dump_paper("roles", &result, &truth, &eval);

        let truncated_long = "\u{e9}".repeat(TAGGED_LINE_TEXT_CHARS);
        assert_eq!(
            dump.tagged_lines,
            vec![
                TaggedLine {
                    page: 1,
                    role: "figure".to_string(),
                    text: "Epoch 0 Epoch 50".to_string(),
                },
                TaggedLine {
                    page: 1,
                    role: "table".to_string(),
                    text: "Method Score".to_string(),
                },
                TaggedLine {
                    page: 1,
                    role: "toc".to_string(),
                    text: "Contents . . . 3".to_string(),
                },
                TaggedLine {
                    page: 1,
                    role: "front".to_string(),
                    text: "A Title Line".to_string(),
                },
                TaggedLine {
                    page: 2,
                    role: "figure".to_string(),
                    text: truncated_long,
                },
            ]
        );
        assert_eq!(
            dump.tagged_lines[4].text.chars().count(),
            TAGGED_LINE_TEXT_CHARS
        );

        assert_eq!(dump.role_counts.len(), 7);
        assert_eq!(dump.role_counts["heading"], 1);
        assert_eq!(dump.role_counts["body"], 2);
        assert_eq!(dump.role_counts["figure"], 2);
        assert_eq!(dump.role_counts["table"], 1);
        assert_eq!(dump.role_counts["toc"], 1);
        assert_eq!(dump.role_counts["front"], 1);
        assert_eq!(dump.role_counts[""], 1);

        let json = serde_json::to_string(&dump).unwrap();
        let back: PaperDump = serde_json::from_str(&json).unwrap();
        assert_eq!(back, dump);
    }

    #[test]
    fn tagged_lines_capped_with_truncated_marker() {
        let truth = truth_with(Vec::new(), "");

        let exact_texts: Vec<String> = (0..TAGGED_LINES_CAP).map(|i| format!("line {i}")).collect();
        let exact_lines: Vec<(&str, &str)> =
            exact_texts.iter().map(|t| (t.as_str(), "figure")).collect();
        let mut exact_result = sample_result(Vec::new(), Vec::new());
        exact_result.pages = vec![roled_page(1, "irrelevant", &exact_lines)];
        let eval = evaluate("cap-exact", &exact_result, &truth);
        let dump = dump_paper("cap-exact", &exact_result, &truth, &eval);
        assert_eq!(dump.tagged_lines.len(), TAGGED_LINES_CAP);
        assert_eq!(dump.tagged_lines.last().unwrap().role, "figure");

        let over_texts: Vec<String> = (0..(TAGGED_LINES_CAP + 5))
            .map(|i| format!("line {i}"))
            .collect();
        let over_lines: Vec<(&str, &str)> =
            over_texts.iter().map(|t| (t.as_str(), "figure")).collect();
        let mut over_result = sample_result(Vec::new(), Vec::new());
        over_result.pages = vec![roled_page(1, "irrelevant", &over_lines)];
        let eval = evaluate("cap-over", &over_result, &truth);
        let dump = dump_paper("cap-over", &over_result, &truth, &eval);
        assert_eq!(dump.tagged_lines.len(), TAGGED_LINES_CAP);
        assert_eq!(
            dump.tagged_lines[TAGGED_LINES_CAP - 2].text,
            format!("line {}", TAGGED_LINES_CAP - 2)
        );
        let marker = dump.tagged_lines.last().unwrap();
        assert_eq!(marker.page, 0);
        assert_eq!(marker.role, "truncated");
        assert_eq!(marker.text, "<6 more>");
    }

    #[test]
    fn dump_body_texts_are_capped_on_char_boundary() {
        let long = "\u{e9}".repeat(120_000);
        let mut result = sample_result(Vec::new(), Vec::new());
        result.pages = vec![page(1, &long), page(2, "tail")];
        let truth = truth_with(Vec::new(), &long);
        let eval = evaluate("m", &result, &truth);
        let dump = dump_paper("m", &result, &truth, &eval);
        for text in [&dump.body_text_extracted, &dump.body_text_truth] {
            assert!(text.len() <= BODY_TEXT_CAP, "{}", text.len());
            assert!(text.len() >= BODY_TEXT_CAP - 1, "{}", text.len());
            assert!(text.chars().all(|c| c == '\u{e9}'));
        }
        assert_eq!(body_text_extracted(&[]), "");
        assert_eq!(
            body_text_extracted(&[page(1, "a"), page(2, "b")]),
            "a\n\u{c}\nb"
        );
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
            hash_ms: 0.0,
        };
        let mut p2 = paper_with("p2", 20.0);
        p2.timings = StageTimings {
            acquire_ms: 3.0,
            parse_ms: 30.0,
            order_ms: 4.0,
            metadata_ms: 0.0,
            citations_ms: 2.0,
            write_ms: 1.0,
            hash_ms: 0.0,
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
        assert_eq!(columns, 21);
        for row in &table {
            assert_eq!(row.matches('|').count(), columns, "{row}");
        }
    }

    fn body_result(pages: Vec<PageText>, citations: Vec<CitationMarker>) -> ExtractionResult {
        let mut result = sample_result(Vec::new(), citations);
        result.pages = pages;
        result
    }

    #[test]
    fn body_alignment_ignores_reference_section_and_marker() {
        let pages = vec![
            lined_page(1, &["Intro text [3] more."]),
            lined_page(
                2,
                &[
                    "Closing words here.",
                    "References",
                    "[3] A. Author. Title. 2020.",
                ],
            ),
        ];
        let marker = CitationMarker {
            page: 1,
            offset: 11,
            text: "[3]".to_string(),
            targets: vec![3],
        };
        let result = body_result(pages, vec![marker]);
        let truth = truth_with(Vec::new(), "Intro text more. Closing words here.");
        let eval = evaluate("refs", &result, &truth);
        let alignment = eval.body_alignment.expect("body text present");
        assert!(close(alignment, 1.0), "got {alignment}");
        assert_eq!(eval.body_words_extracted, 6);
        assert_eq!(eval.body_words_truth, 6);
        assert_eq!(eval.body_words_matched, 6);
        // Raw keeps the marker and the bibliography: 13 extracted words
        // (intro text 3 more closing words here references 3 a author title
        // 2020) against 6 truth words, 6 matched.
        let raw = eval.body_alignment_raw.expect("body text present");
        assert!(close(raw, 12.0 / 19.0), "got {raw}");
    }

    #[test]
    fn body_alignment_removes_unrecorded_marker_groups_by_pattern() {
        let result = body_result(
            vec![page(
                1,
                "Prior work [4, 5] and (Smith et al., 2020; Lee and Kim, 2019a) agree [7–9].",
            )],
            Vec::new(),
        );
        let (extracted, _) = alignment_texts(&result.pages, &result.citations, "");
        assert_eq!(words(&extracted), vec!["prior", "work", "and", "agree"]);
    }

    #[test]
    fn body_alignment_without_reference_heading_uses_all_text() {
        let pages = vec![page(1, "First page words."), page(2, "Second page words.")];
        let (extracted, _) = alignment_texts(&pages, &[], "");
        assert_eq!(
            words(&extracted),
            vec!["first", "page", "words", "second", "page", "words"]
        );
    }

    #[test]
    fn body_alignment_drops_caption_paragraphs() {
        let result = body_result(
            vec![page(
                1,
                "Body one.\n\nFigure 2: A caption here\nsecond caption line\n\n\
                 Table 1. Results of the run\n\nTable 2 shows the body two.",
            )],
            Vec::new(),
        );
        let truth = truth_with(Vec::new(), "Body one. Table 2 shows the body two.");
        let eval = evaluate("captions", &result, &truth);
        let alignment = eval.body_alignment.expect("body text present");
        assert!(close(alignment, 1.0), "got {alignment}");
        assert_eq!(eval.body_words_extracted, 8);
    }

    #[test]
    fn caption_without_blank_line_does_not_swallow_following_prose() {
        let text = "Body one.\n\
                    Figure 3: Accuracy against model size for\n\
                    all three datasets.\n\
                    The prose resumes here.\n\
                    More prose follows.\n\
                    And a third line.";
        assert_eq!(
            drop_caption_and_math_lines(text),
            "Body one.\nThe prose resumes here.\nMore prose follows.\nAnd a third line."
        );
        let one_line = "Table 2: Results.\nProse after the table.\nIt goes on.\nAnd on.";
        assert_eq!(
            drop_caption_and_math_lines(one_line),
            "Prose after the table.\nIt goes on.\nAnd on."
        );
        let paren = "Figure 1: Results (left) and (right)\nProse here.\nMore.\nEnd.";
        assert_eq!(
            drop_caption_and_math_lines(paren),
            "Prose here.\nMore.\nEnd."
        );
        // A new paragraph with fewer than two prose lines after it is still
        // caption (a caption of several sentences).
        let glued = "Figure 1: Results (left) and (right)\nProse here.";
        assert_eq!(drop_caption_and_math_lines(glued), "");
        // Without a boundary at most 8 lines follow the start line.
        let long = "Figure 1: a\nb\nc\nd\ne\nf\ng\nh\ni\nkept line\nkept too";
        assert_eq!(drop_caption_and_math_lines(long), "kept line\nkept too");
    }

    #[test]
    fn caption_of_several_sentences_is_dropped_whole() {
        let text = "Figure 4: Accuracy per model size.\n\
                    Shaded bands show the spread over five seeds.\n\
                    Dashed lines mark the baseline.\n\
                    \n\
                    Body resumes here.";
        assert_eq!(drop_caption_and_math_lines(text), "\nBody resumes here.");
        // A new paragraph followed by two prose lines ends the caption.
        let text = "Figure 4: Accuracy per model size.\n\
                    We now turn to the second experiment.\n\
                    It uses the same data.\n\
                    Results follow below.";
        assert_eq!(
            drop_caption_and_math_lines(text),
            "We now turn to the second experiment.\nIt uses the same data.\nResults follow below."
        );
        // A math-heavy line is not prose, so it does not end the caption.
        let text = "Figure 4: Accuracy per model size.\n\
                    Shaded bands show the spread.\n\
                    x = 2 + 3\n\
                    last caption words";
        assert_eq!(drop_caption_and_math_lines(text), "");
    }

    #[test]
    fn body_alignment_drops_math_heavy_lines_on_both_sides() {
        let result = body_result(
            vec![page(
                1,
                "Prose words here\nx = 2 + 3 * (y - 1)\nmore prose\n12",
            )],
            Vec::new(),
        );
        let truth = truth_with(Vec::new(), "Prose words here\na_1 = 42 + 7\nmore prose");
        let eval = evaluate("math", &result, &truth);
        let alignment = eval.body_alignment.expect("body text present");
        assert!(close(alignment, 1.0), "got {alignment}");
        assert_eq!(eval.body_words_extracted, 5);
        assert_eq!(eval.body_words_truth, 5);
        assert_eq!(eval.body_words_matched, 5);
        assert!(is_math_heavy("x = 2 + 3 * (y - 1)"));
        assert!(is_math_heavy("12"));
        assert!(!is_math_heavy("In 2019, 45 of the 1234 runs failed"));
        assert!(!is_math_heavy(""));
    }

    #[test]
    fn math_tokens_are_recognised() {
        for math in [
            "𝑥",
            "μ(vij)",
            "(v_ij)",
            "x^2",
            "a=b",
            "\\alpha",
            "10⁻²",
            "x²",
            "∀x",
            "±1",
            "x",
            "(x,",
            "f",
            "x0",
            "v1",
            "l2",
            "GPT4",
            "(2020a)",
            "θ̂(x0),",
        ] {
            assert!(is_math_token(math), "{math:?}");
        }
        for word in [
            "a",
            "A",
            "I",
            "i",
            "(a)",
            "word",
            "Word,",
            "2nd",
            "21st",
            "10km",
            "3D",
            "2D",
            "4K",
            "1990s",
            "7B",
            "2020",
            "3",
            "COVID-19",
            "GPT-4o",
            "x0-dependent",
            "e.g.,",
        ] {
            assert!(!is_math_token(word), "{word:?}");
        }
    }

    #[test]
    fn alignment_words_drop_math_tokens_on_both_sides() {
        assert_eq!(
            alignment_words("Let 𝑋𝑖 = (Age𝑖, Sex𝑖) and f(x) be the 2nd map in 3D x0-dependent"),
            vec![
                "let",
                "and",
                "be",
                "the",
                "2nd",
                "map",
                "in",
                "3d",
                "dependent"
            ]
        );
        assert_eq!(
            alignment_words("Let X_i = (Age_i, Sex_i) and f(x) be the 2nd map in 3D x0-dependent"),
            vec![
                "let",
                "and",
                "be",
                "the",
                "2nd",
                "map",
                "in",
                "3d",
                "dependent"
            ]
        );
        // The plain tokenizer keeps them.
        assert_eq!(words("f(x) x0"), vec!["f", "x", "x0"]);
        // Hyphenated math compounds keep their word part on both sides.
        assert_eq!(
            alignment_words("top-𝑘 𝑛-gram ε-greedy"),
            vec!["top", "gram", "greedy"]
        );
        assert_eq!(
            alignment_words("top- -gram -greedy"),
            vec!["top", "gram", "greedy"]
        );
    }

    #[test]
    fn body_alignment_strips_math_tokens_but_raw_keeps_them() {
        let result = body_result(
            vec![page(1, "We bound μ(vij) by 𝜃 for each x in the set")],
            Vec::new(),
        );
        let truth = truth_with(Vec::new(), "We bound (v_ij) by for each in the set");
        let eval = evaluate("math-tokens", &result, &truth);
        let alignment = eval.body_alignment.expect("body text present");
        assert!(close(alignment, 1.0), "got {alignment}");
        // we bound by for each in the set
        assert_eq!(eval.body_words_extracted, 8);
        assert_eq!(eval.body_words_truth, 8);
        assert_eq!(eval.body_words_matched, 8);
        // Raw: 12 extracted tokens (we bound μ vij by 𝜃 for each x in the
        // set) against 10 truth tokens (we bound v ij by for each in the
        // set), 8 matched.
        let raw = eval.body_alignment_raw.expect("body text present");
        assert!(close(raw, 16.0 / 22.0), "got {raw}");
    }

    #[test]
    fn math_symbols_count_as_math_not_letters() {
        assert!(is_math_heavy("𝑄𝑛 (𝑋) 𝜏"));
        assert!(is_math_heavy("αβγ δε"));
        assert!(is_math_heavy("∀x ∈ X"));
        assert!(!is_math_heavy("The parameter α controls the rate"));
        assert!(is_math_char('∑'));
        assert!(is_math_char('𝐼'));
        assert!(!is_math_char('a'));
    }

    #[test]
    fn caption_start_accepts_number_then_capitalised_word() {
        for caption in [
            "Fig. 3 Overview of the judge and extractor choice",
            "Figure 3 The overall framework of CoFiRec",
            "Table 2 Results on the test split",
            "Figure 2: A caption here",
            "Table 1. Results of the run",
        ] {
            assert!(caption_start_re().is_match(caption), "{caption}");
        }
        for prose in [
            "Table 2 shows the body two.",
            "Table 1.5 lists the runs.",
            "Figure 3 and 4 show the trend.",
            "Table 3 GPT-4 outperforms the rest.",
        ] {
            assert!(!caption_start_re().is_match(prose), "{prose}");
        }
    }

    #[test]
    fn body_keeps_appendix_after_reference_list() {
        let pages = vec![
            lined_page(1, &["Intro words here."]),
            lined_page(
                2,
                &[
                    "Closing words.",
                    "References",
                    "[1] A. Author. First title. 2020.",
                    "[2] B. Author. Second title. 2021.",
                ],
            ),
            lined_page(3, &["Appendix A", "Appendix proof text."]),
        ];
        assert_eq!(
            words(&body_text_extracted(&pages)),
            vec![
                "intro", "words", "here", "closing", "words", "appendix", "a", "appendix", "proof",
                "text"
            ]
        );
    }

    #[test]
    fn body_resumes_at_appendix_heading_on_the_reference_page() {
        let pages = vec![lined_page(
            1,
            &[
                "Closing words.",
                "References",
                "[1] A. Author. First title. 2020.",
                "[2] B. Author. Second title. 2021.",
                "Appendix A",
                "Proof text here.",
            ],
        )];
        let body = body_text_extracted(&pages);
        assert_eq!(
            words(&body),
            vec!["closing", "words", "appendix", "a", "proof", "text", "here"]
        );
        // The kept text on either side of the cut does not run together.
        assert!(body.contains("Closing words.\n"), "{body:?}");
        assert!(
            body.contains("\n\nAppendix A\nProof text here."),
            "{body:?}"
        );
    }

    #[test]
    fn body_cuts_every_reference_list_and_keeps_text_between() {
        let pages = vec![
            lined_page(
                1,
                &[
                    "Body text.",
                    "References",
                    "[1] A. Author. First title. 2020.",
                    "[2] B. Author. Second title. 2021.",
                ],
            ),
            lined_page(2, &["Appendix A", "More appendix text."]),
            lined_page(
                3,
                &[
                    "Supplementary References",
                    "[3] C. Author. Third title. 2019.",
                    "[4] D. Author. Fourth title. 2018.",
                ],
            ),
        ];
        assert_eq!(
            words(&body_text_extracted(&pages)),
            vec!["body", "text", "appendix", "a", "more", "appendix", "text"]
        );
    }

    #[test]
    fn body_resumes_on_the_page_after_the_last_reference_entry() {
        let pages = vec![
            lined_page(
                1,
                &[
                    "Body text.",
                    "References",
                    "[1] A. Author. First title. 2020.",
                    "[2] B. Author. Second title. 2021.",
                    "[3] C. Author. Third title. 2019.",
                ],
            ),
            lined_page(2, &["Proof details continue here."]),
        ];
        assert_eq!(
            words(&body_text_extracted(&pages)),
            vec!["body", "text", "proof", "details", "continue", "here"]
        );
    }

    #[test]
    fn body_excludes_last_entry_wrapped_onto_the_next_page() {
        let pages = vec![
            lined_page(
                1,
                &[
                    "Body text.",
                    "References",
                    "[1] A. Author. First title. 2020.",
                    "[2] B. Author. A second title that runs",
                ],
            ),
            lined_page(
                2,
                &[
                    "over the page break. In Proc. Conf.,",
                    "pages 1-10, 2021.",
                    "Appendix A",
                    "Proof text of the appendix here.",
                ],
            ),
        ];
        let body = body_text_extracted(&pages);
        assert_eq!(
            words(&body),
            vec![
                "body", "text", "appendix", "a", "proof", "text", "of", "the", "appendix", "here"
            ]
        );
        assert!(!body.contains("2021"), "{body:?}");
    }

    #[test]
    fn wrapped_last_entry_ends_at_a_prose_paragraph_or_the_line_cap() {
        let head = lined_page(
            1,
            &[
                "Body text.",
                "References",
                "[1] A. Author. A title that runs",
            ],
        );
        // A blank-separated paragraph of two long lines ends the entry.
        let prose = page(
            2,
            "over the page break. In Proc. Conf., 2021.\n\n\
             We now prove the main theorem of this paper in full detail.\n\
             The argument follows the standard route through the lemma above.",
        );
        assert_eq!(
            words(&body_text_extracted(&[head.clone(), prose])),
            words(
                "Body text. We now prove the main theorem of this paper in full detail. \
                 The argument follows the standard route through the lemma above."
            )
        );
        // Without any boundary, at most 12 lines continue the entry.
        let lines: Vec<String> = (1..=15).map(|i| format!("line {i}")).collect();
        let tail = page(2, &lines.join("\n"));
        assert_eq!(
            words(&body_text_extracted(&[head, tail])),
            words("Body text. line 13 line 14 line 15")
        );
    }

    #[test]
    fn unlabeled_author_year_entries_continue_the_list_over_the_page() {
        let head = lined_page(
            1,
            &[
                "Body text.",
                "References",
                "Adams, B. (2019). A first title about regression models in practice.",
                "Baker, C., & Cole, D. (2020). A second title about sparse estimation",
            ],
        );
        // Page 2 is a second page of references: 14 blank-separated
        // unlabeled author-year entries of at least 8 words each (the prose
        // rule and the 12-line cap would each have ended the list here),
        // then the supplement.
        let mut text = String::from("methods. Journal of Statistics, 12, 1-20.");
        for i in 0..14 {
            text.push_str(&format!(
                "\n\nHu, W., Pan, T., Kong, D. & Shen, W. (2021). Nonparametric matrix \
                 response regression number {i}\nwith application to brain imaging data \
                 analysis. Annals of Statistics, 49, 1-30."
            ));
        }
        text.push_str(
            "\n\nSupplementary material\n\nA. Additional simulations\n\
             The supplement text is kept.",
        );
        let pages = [head, page(2, &text)];
        // The segmented list ends on page 1, so the end on page 2 is
        // `continuation_end`'s.
        let sections = find_reference_sections(&pages);
        let last = segment_entries(&pages, &sections[0]).last().map(|e| e.page);
        assert_eq!(last, Some(1));
        let body = body_text_extracted(&pages);
        assert_eq!(
            words(&body),
            words(
                "Body text. Supplementary material A. Additional simulations \
                 The supplement text is kept."
            )
        );
        assert!(is_entry_start(
            "Hu, W., Pan, T., Kong, D. & Shen, W. (2021). Title"
        ));
        assert!(is_entry_start("Smith, J., & Lee, K. (2019a). Title here."));
        assert!(is_entry_start(
            "van der Vaart, A. W. (1998). Asymptotic statistics."
        ));
        assert!(is_entry_start("Van Cooten, B., Morand, V.,"));
        assert!(is_entry_start("[12] A. Author. Title."));
        assert!(is_entry_start("12. A. Author. Title."));
        assert!(!is_entry_start("Smith, J. argued in 2020 that this holds."));
        assert!(!is_entry_start(
            "We now prove the main theorem in full detail."
        ));
    }

    #[test]
    fn author_year_entry_after_a_caption_resumes_the_list() {
        let pages = vec![lined_page(
            1,
            &[
                "Body words.",
                "References",
                "Adams, B. (2019). First title. Journal, 1, 2-3.",
                "Table 3: Results of the run",
                "",
                "Baker, C., & Cole, D. (2020). Second title. Journal, 4, 5-6.",
            ],
        )];
        assert_eq!(words(&body_text_extracted(&pages)), vec!["body", "words"]);
    }

    #[test]
    fn caption_inside_reference_list_does_not_end_it() {
        let pages = vec![lined_page(
            1,
            &[
                "Body words.",
                "References",
                "[1] A. Author. First title. 2020.",
                "Table 3: Results of the run",
                "[2] B. Author. Second title. 2021.",
            ],
        )];
        assert_eq!(words(&body_text_extracted(&pages)), vec!["body", "words"]);
    }

    #[test]
    fn body_keeps_appendix_after_text_only_reference_heading() {
        let pages = vec![page(
            1,
            "Body text.\nReferences\n[1] X. Author. Title. 2020.\nAppendix A\nMore text.",
        )];
        assert_eq!(
            words(&body_text_extracted(&pages)),
            vec!["body", "text", "appendix", "a", "more", "text"]
        );
    }

    /// A page whose `text` is given and whose lines are `(text, role)`.
    fn roled_page(number: u32, text: &str, lines: &[(&str, &str)]) -> PageText {
        let mut p = page(number, text);
        p.lines = lines
            .iter()
            .map(|(line, role)| Line {
                text: (*line).to_string(),
                role: (*role).to_string(),
                ..Line::default()
            })
            .collect();
        p
    }

    const ROLED_TEXT: &str = "1 Introduction\nWe study text.\nEpoch 0 Epoch 50\n\n\
                              Method Score\nThe prose resumes.\nContents . . . 3\nA Title Line";

    const ROLED_LINES: [&str; 7] = [
        "1 Introduction",
        "We study text.",
        "Epoch 0 Epoch 50",
        "Method Score",
        "The prose resumes.",
        "Contents . . . 3",
        "A Title Line",
    ];

    #[test]
    fn body_text_skips_non_body_roles_and_keeps_body_and_headings() {
        let roles = ["heading", "body", "figure", "table", "body", "toc", "front"];
        let lines: Vec<(&str, &str)> = ROLED_LINES.iter().copied().zip(roles).collect();
        let mut tagged = roled_page(1, ROLED_TEXT, &lines);
        tagged.lines.push(Line {
            text: "3".to_string(),
            role: "furniture".to_string(),
            ..Line::default()
        });
        assert_eq!(
            body_text_extracted(&[tagged]),
            "1 Introduction\nWe study text.\n\nThe prose resumes.\n"
        );
    }

    #[test]
    fn body_text_without_roles_or_lines_keeps_the_old_text() {
        let untagged: Vec<(&str, &str)> = ROLED_LINES.iter().map(|l| (*l, "body")).collect();
        assert_eq!(
            body_text_extracted(&[roled_page(1, ROLED_TEXT, &untagged)]),
            ROLED_TEXT
        );
        let empty_role: Vec<(&str, &str)> = ROLED_LINES.iter().map(|l| (*l, "")).collect();
        assert_eq!(
            body_text_extracted(&[roled_page(1, ROLED_TEXT, &empty_role)]),
            ROLED_TEXT
        );
        assert_eq!(body_text_extracted(&[page(1, ROLED_TEXT)]), ROLED_TEXT);
    }

    #[test]
    fn dropped_line_keeps_paragraph_separators() {
        let lines = [("A line", "body"), ("FIG", "figure"), ("B line", "body")];
        let para_after = roled_page(1, "A line\nFIG\n\nB line", &lines);
        assert_eq!(body_text_extracted(&[para_after]), "A line\n\nB line");
        let para_before = roled_page(1, "A line\n\nFIG\nB line", &lines);
        assert_eq!(body_text_extracted(&[para_before]), "A line\n\nB line");
        let no_para = roled_page(1, "A line\nFIG\nB line", &lines);
        assert_eq!(body_text_extracted(&[no_para]), "A line\nB line");
        let caption = [("Body.", "body"), ("A plain caption", "caption")];
        let captioned = roled_page(1, "Body.\nA plain caption", &caption);
        assert_eq!(body_text_extracted(&[captioned]), "Body.\n");
    }

    #[test]
    fn short_tagged_line_only_matches_a_whole_line() {
        // The figure label "7" is not in the text as a line of its own; it
        // must not match the "7" inside the prose that follows.
        let lines = [("7", "figure"), ("Ran 7 epochs.", "body")];
        let p = roled_page(1, "Ran 7 epochs.", &lines);
        assert_eq!(body_text_extracted(&[p]), "Ran 7 epochs.");
        let lines = [("Ran 50 epochs.", "body"), ("50", "figure")];
        let p = roled_page(1, "Ran 50 epochs.\n50", &lines);
        assert_eq!(body_text_extracted(&[p]), "Ran 50 epochs.\n");
    }

    #[test]
    fn recorded_marker_after_a_dropped_line_is_still_removed() {
        let lines = [
            ("Epoch 0 Epoch 50", "figure"),
            ("See Smith (2020) here.", "body"),
        ];
        let p = roled_page(1, "Epoch 0 Epoch 50\nSee Smith (2020) here.", &lines);
        let marker = CitationMarker {
            page: 1,
            offset: 21,
            text: "Smith (2020)".to_string(),
            targets: vec![1],
        };
        let (extracted, _) = alignment_texts(&[p], &[marker], "");
        assert_eq!(words(&extracted), vec!["see", "here"]);
    }

    #[test]
    fn body_alignment_excludes_figure_lines_but_raw_keeps_them() {
        let lines = [("Real prose words.", "body"), ("Plot label axis", "figure")];
        let p = roled_page(1, "Real prose words.\nPlot label axis", &lines);
        let result = body_result(vec![p], Vec::new());
        let truth = truth_with(Vec::new(), "Real prose words.");
        let eval = evaluate("roles", &result, &truth);
        let alignment = eval.body_alignment.expect("body text present");
        assert!(close(alignment, 1.0), "got {alignment}");
        assert_eq!(eval.body_words_extracted, 3);
        let raw = eval.body_alignment_raw.expect("body text present");
        assert!(close(raw, 6.0 / 9.0), "got {raw}");
    }

    #[test]
    fn body_alignment_raw_matches_plain_word_alignment() {
        let result = body_result(
            vec![
                page(1, "The quick brown [2] fox"),
                page(2, "References\n[2] B. Writer. 2020."),
            ],
            Vec::new(),
        );
        let truth = truth_with(Vec::new(), "The quick brown fox");
        let eval = evaluate("raw", &result, &truth);
        let raw = eval.body_alignment_raw.expect("body text present");
        let expected = word_alignment(
            "The quick brown [2] fox\nReferences\n[2] B. Writer. 2020.",
            "The quick brown fox",
        );
        assert!(close(raw, expected), "{raw} vs {expected}");
        // 10 extracted tokens, 4 truth tokens, 4 matched: 8 / 14.
        assert!(close(raw, 8.0 / 14.0), "got {raw}");
        let alignment = eval.body_alignment.expect("body text present");
        assert!(close(alignment, 1.0), "got {alignment}");
    }

    #[test]
    fn align_counts_match_word_alignment() {
        let counts = align_counts("the quick brown fox jumps", "the quick brown cat sleeps");
        assert_eq!(
            counts,
            AlignCounts {
                left: 5,
                right: 5,
                matched: 3,
            }
        );
        assert!(close(alignment_score(counts), 0.6));
        assert!(close(alignment_score(AlignCounts::default()), 1.0));
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
        assert!(md.contains(
            "| warnings | title ✓/✗ | authors c/t | paper doi ✓/✗/n/a | align raw | body words m/e/t |\n"
        ));
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
            row("p1").ends_with("| 0 | ✓ | 3/4 | n/a | n/a | 0/0/0 |"),
            "{}",
            row("p1")
        );
        assert!(
            row("p2").ends_with("| 0 | ✗ | 1/2 | n/a | n/a | 0/0/0 |"),
            "{}",
            row("p2")
        );
        assert!(
            row("p3").ends_with("| 0 | n/a | 0/0 | n/a | n/a | 0/0/0 |"),
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
        assert!(row("p1").ends_with("| ✓ | n/a | 0/0/0 |"), "{}", row("p1"));
        assert!(row("p3").ends_with("| ✗ | n/a | 0/0/0 |"), "{}", row("p3"));
        assert!(
            row("p4").ends_with("| n/a | n/a | 0/0/0 |"),
            "{}",
            row("p4")
        );
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
