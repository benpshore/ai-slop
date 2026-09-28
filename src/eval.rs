//! Evaluation harness: scores an [`ExtractionResult`] against the ground truth
//! recovered from a paper's `LaTeX` source (see `crate::latex_refs`).
//!
//! Measured per paper: exact reference-count match, per-entry recall and
//! precision (greedy one-to-one matching by DOI, `arXiv` id, title, then
//! first-author surname plus year), DOI/year/title field accuracy over the
//! matched pairs, in-text marker resolution, and a word-alignment diagnostic
//! of the body text order. These are diagnostics on real papers, not the
//! human-checked acceptance protocol.

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use crate::latex_refs::{GroundTruth, TruthReference};
use crate::schema::{ExtractionResult, ReferenceEntry};

/// Product target: warm service time per 20-page chunk, in milliseconds.
pub const TARGET_MS_PER_CHUNK: f64 = 30.0;

/// Maximum tokens per side considered by [`word_alignment`]; longer inputs are
/// sampled evenly down to this many tokens so the quadratic DP stays bounded.
pub const MAX_ALIGN_TOKENS: usize = 12_000;

/// Minimum Jaccard similarity of title words for a fuzzy title match.
const TITLE_JACCARD_MIN: f32 = 0.8;

/// Unmatched truth keys listed per paper in the markdown report.
const UNMATCHED_KEYS_SHOWN: usize = 10;

/// How one truth reference was (or was not) paired with an extracted entry.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RefMatch {
    /// `TruthReference::key` (the `\bibitem` / `.bib` key).
    pub truth_key: String,
    /// `ReferenceEntry::index` of the paired entry, if any.
    pub extracted_index: Option<u32>,
    /// `"doi"`, `"arxiv"`, `"title"`, `"author-year"` or `"none"`.
    pub method: String,
    /// 1.0 for exact DOI/`arXiv`/title matches, the Jaccard value for fuzzy
    /// title matches, 0.75 for author-year matches, 0.0 when unmatched.
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
    pub doi_truth: u32,
    pub doi_correct: u32,
    pub year_truth: u32,
    pub year_correct: u32,
    pub title_truth: u32,
    pub title_correct: u32,
    pub truth_cite_commands: u32,
    pub extracted_markers: u32,
    /// Markers with at least one resolved target.
    pub resolved_markers: u32,
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
    pub year_accuracy: f32,
    pub title_accuracy: f32,
    pub marker_resolution_rate: f32,
    pub mean_body_alignment: Option<f32>,
    pub p50_ms_per_chunk: f64,
    pub p95_ms_per_chunk: f64,
    pub target_ms_per_chunk: f64,
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

/// Lower-case DOI without resolver prefixes or trailing punctuation.
fn normalize_doi(s: &str) -> String {
    let lower = s.trim().to_lowercase();
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
    rest.trim_end_matches(['.', ',', ';']).to_string()
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
}

/// Greedy one-to-one pairing of truth references with extracted entries, in
/// priority order: equal DOI (case-insensitive), equal `arXiv` id (version
/// ignored), equal normalized title or title-word Jaccard >= 0.8, then equal
/// first-author surname (lower-case, ASCII-folded) plus year. Each extracted
/// entry is used at most once. One [`RefMatch`] per truth reference, in order.
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

    for (truth_ref, m) in truth.references.iter().zip(&matches) {
        doi_truth += u32::from(truth_ref.doi.is_some());
        year_truth += u32::from(truth_ref.year.is_some());
        title_truth += u32::from(truth_ref.title.is_some());
        let Some(idx) = m.extracted_index else {
            unmatched_truth_keys.push(truth_ref.key.clone());
            continue;
        };
        matched_refs += 1;
        matched_indices.insert(idx);
        if let Some(ext) = result.references.iter().find(|entry| entry.index == idx) {
            doi_correct += u32::from(doi_equal(truth_ref.doi.as_ref(), ext.doi.as_ref()));
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
        truth_cite_commands: truth.citations.cite_commands,
        extracted_markers,
        resolved_markers,
        marker_targets,
        body_alignment,
        ms_total,
        ms_per_chunk,
        chunks,
        warnings: warnings as u32,
        matches,
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
        truth_cite_commands: 0,
        extracted_markers: 0,
        resolved_markers: 0,
        marker_targets: 0,
        body_alignment: None,
        ms_total: 0.0,
        ms_per_chunk: 0.0,
        chunks: 0,
        warnings: 0,
        matches: Vec::new(),
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
/// percentiles are nearest-rank over `ms_per_chunk`.
pub fn summarize(papers: &[PaperEval]) -> Summary {
    let ok: Vec<&PaperEval> = papers.iter().filter(|p| !is_failed(p)).collect();
    let failed = papers.len() - ok.len();

    let sum = |f: fn(&PaperEval) -> u32| -> u64 { ok.iter().map(|p| u64::from(f(p))).sum() };
    let exact = ok.iter().filter(|p| p.ref_count_exact).count() as u64;
    let matched = sum(|p| p.matched_refs);

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

    Summary {
        papers: papers.len() as u32,
        failed: failed as u32,
        ref_count_exact_rate: ratio(exact, ok.len() as u64),
        ref_recall: ratio(matched, sum(|p| p.truth_refs)),
        ref_precision: ratio(matched, sum(|p| p.extracted_refs)),
        doi_accuracy: ratio(sum(|p| p.doi_correct), sum(|p| p.doi_truth)),
        year_accuracy: ratio(sum(|p| p.year_correct), sum(|p| p.year_truth)),
        title_accuracy: ratio(sum(|p| p.title_correct), sum(|p| p.title_truth)),
        marker_resolution_rate: ratio(sum(|p| p.resolved_markers), sum(|p| p.extracted_markers)),
        mean_body_alignment,
        p50_ms_per_chunk: percentile(&ms, 50.0),
        p95_ms_per_chunk: percentile(&ms, 95.0),
        target_ms_per_chunk: TARGET_MS_PER_CHUNK,
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

/// Formats an optional alignment score.
fn align_cell(alignment: Option<f32>) -> String {
    alignment.map_or_else(|| "n/a".to_string(), |a| format!("{a:.3}"))
}

/// Renders the report as `GitHub`-flavoured markdown: a summary table, a
/// per-paper table, then the unmatched truth keys (at most ten per paper).
pub fn render_markdown(report: &CorpusReport) -> String {
    let s = &report.summary;
    let mut out = String::new();
    out.push_str("# Evaluation report\n\n");
    let _ = write!(out, "- Backend: `{}`\n", cell(&report.backend));
    let _ = write!(out, "- Host: `{}`\n", cell(&report.host));
    let _ = write!(out, "- Generated (unix): {}\n\n", report.generated_unix);

    out.push_str("## Summary\n\n");
    out.push_str("| Metric | Value |\n");
    out.push_str("| --- | --- |\n");
    let _ = write!(out, "| Papers | {} |\n", s.papers);
    let _ = write!(out, "| Failed | {} |\n", s.failed);
    let _ = write!(
        out,
        "| Reference count exact | {} |\n",
        pct(s.ref_count_exact_rate)
    );
    let _ = write!(out, "| Reference recall | {} |\n", pct(s.ref_recall));
    let _ = write!(out, "| Reference precision | {} |\n", pct(s.ref_precision));
    let _ = write!(out, "| DOI accuracy | {} |\n", pct(s.doi_accuracy));
    let _ = write!(out, "| Year accuracy | {} |\n", pct(s.year_accuracy));
    let _ = write!(out, "| Title accuracy | {} |\n", pct(s.title_accuracy));
    let _ = write!(
        out,
        "| Marker resolution | {} |\n",
        pct(s.marker_resolution_rate)
    );
    let _ = write!(
        out,
        "| Mean body alignment | {} |\n",
        align_cell(s.mean_body_alignment)
    );
    let _ = write!(out, "| p50 ms per chunk | {:.1} |\n", s.p50_ms_per_chunk);
    let _ = write!(out, "| p95 ms per chunk | {:.1} |\n", s.p95_ms_per_chunk);
    let _ = write!(
        out,
        "| Target ms per chunk | {:.1} |\n\n",
        s.target_ms_per_chunk
    );

    out.push_str("## Papers\n\n");
    out.push_str(
        "| id | status | pages | refs truth/extracted/matched | count exact | doi c/t | \
         year c/t | markers resolved/extracted | align | ms/chunk | warnings |\n",
    );
    out.push_str("| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |\n");
    for p in &report.papers {
        let exact = if p.ref_count_exact { "✓" } else { "✗" };
        let _ = write!(
            out,
            "| {} | {} | {} | {}/{}/{} | {} | {}/{} | {}/{} | {}/{} | {} | {:.1} | {} |\n",
            cell(&p.id),
            cell(&p.status),
            p.pages,
            p.truth_refs,
            p.extracted_refs,
            p.matched_refs,
            exact,
            p.doi_correct,
            p.doi_truth,
            p.year_correct,
            p.year_truth,
            p.resolved_markers,
            p.extracted_markers,
            align_cell(p.body_alignment),
            p.ms_per_chunk,
            p.warnings,
        );
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::latex_refs::{TruthCitations, TruthSource};
    use crate::schema::{
        BackendIdentity, ChunkResult, CitationMarker, ContentHash, Document, Metadata, PageText,
        SCHEMA_VERSION, StageTimings, Status,
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
        assert_eq!(eval.doi_truth, 1);
        assert_eq!(eval.doi_correct, 1);
        assert_eq!(eval.year_truth, 3);
        assert_eq!(eval.year_correct, 1);
        assert_eq!(eval.title_truth, 3);
        assert_eq!(eval.title_correct, 1);
        assert_eq!(eval.truth_cite_commands, 5);
        assert_eq!(eval.extracted_markers, 2);
        assert_eq!(eval.resolved_markers, 1);
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
        p1.body_alignment = Some(0.8);
        let mut p2 = paper_with("p2", 10.0);
        p2.truth_refs = 10;
        p2.extracted_refs = 12;
        p2.matched_refs = 10;
        p2.ref_count_exact = false;
        p2.body_alignment = Some(0.6);
        let mut p3 = paper_with("p3", 30.0);
        p3.truth_refs = 5;
        p3.extracted_refs = 5;
        p3.matched_refs = 5;
        let p4 = paper_with("p4", 20.0);
        let p5 = paper_with("p5", 40.0);
        let failed = failed_paper("p6", "boom");

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
        assert!(close(s.marker_resolution_rate, 0.5));
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
        assert!(
            md.contains("| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |\n")
        );
        assert!(md.contains("| arxiv:2108.04588 | complete | 0 | 31/31/30 | ✓ |"));
        assert!(md.contains("| 0.912 | 12.5 | 0 |"));
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
}
