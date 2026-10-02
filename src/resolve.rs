//! Resolve reference entries, and the paper itself, to DOI records that
//! are verified against what is printed.
//!
//! A DOI is an exact string: one wrong character resolves to nothing or,
//! worse, to another work. So the DOI is taken from the most reliable
//! place first, and every record is checked against the printed entry
//! before it is accepted:
//!
//! 1. a `doi.org` link annotation placed on the entry ([`attach_links`]),
//!    an exact string from the PDF's annotation dictionary;
//! 2. the DOI printed in the entry text;
//! 3. a bibliographic query on the entry text.
//!
//! A record is accepted only when its first author agrees with the printed
//! first author (or, when no author was parsed, its title agrees with the
//! printed title), and its year is within one of the printed year. Anything
//! else stays unresolved and the entry keeps only its printed fields.

use std::collections::{BTreeMap, HashSet};
use std::sync::OnceLock;
use std::time::Duration;

use regex::Regex;
use tpe_biblio::util::with_query;
use tpe_biblio::{BiblioError, Client, PaperRecord, crossref, normalize_doi, pmc};
use unicode_normalization::UnicodeNormalization;

use crate::schema::{Attempt, Metadata, PageText, ReferenceEntry, Resolved};

/// Crossref host, for the polite-pool rate limit.
const CROSSREF_HOST: &str = "api.crossref.org";
/// Interval between Crossref requests. The anonymous pool answers HTTP 429
/// well below its nominal limit for bibliographic queries; the polite pool
/// (a `mailto`) is faster and steadier.
const CROSSREF_INTERVAL: Duration = Duration::from_millis(200);
/// Retries after HTTP 429 or a transport failure, with backoff
/// `RETRY_BASE`, doubled each time.
const RETRIES: u32 = 4;
/// First backoff after a failed request.
const RETRY_BASE: Duration = Duration::from_millis(1500);
/// Rows asked from a bibliographic query.
const QUERY_ROWS: u32 = 5;
/// Longest entry text sent as a query.
const QUERY_CHARS: usize = 300;
/// Least title agreement for an accepted record when no author was parsed.
const TITLE_MIN: f32 = 0.7;
/// Least title agreement for the paper's own record found by query.
const PAPER_TITLE_MIN: f32 = 0.85;

/// A DOI in running text: `10.<registrant>/<suffix>`, without trailing
/// sentence punctuation.
fn doi_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)\b10\.\d{4,9}/[^\s\x22\x27]+").expect("valid regex"))
}

/// The DOI in `text` (a URI or a printed string), lower-cased, without a
/// resolver prefix or trailing punctuation; `None` when there is none.
#[must_use]
pub fn doi_in(text: &str) -> Option<String> {
    let found = doi_re().find(text)?;
    let mut doi = found.as_str();
    // SICI identifiers contain balanced parentheses and angle brackets. Only
    // remove citation delimiters with no opening partner inside the DOI.
    loop {
        let before = doi;
        doi = doi.trim_end_matches(['.', ',', ';', ':']);
        for (open, close) in [('(', ')'), ('[', ']'), ('{', '}'), ('<', '>')] {
            while doi.ends_with(close) && doi.matches(close).count() > doi.matches(open).count() {
                doi = &doi[..doi.len() - close.len_utf8()];
            }
        }
        if doi == before {
            break;
        }
    }
    normalize_doi(doi)
}

/// Attach `doi.org` link annotations to the entries they sit on. An entry
/// owns the links whose centre lies at or below its first line and in its
/// column (`x` between the entry's left edge and the page's midline plus a
/// margin), closer than any later entry. When one entry carries several
/// distinct DOIs the topmost is kept and the rest are ignored.
pub fn attach_links(entries: &mut [ReferenceEntry], pages: &[PageText]) {
    for page in pages {
        let dois: Vec<(f32, f32, String)> = page
            .links
            .iter()
            .filter_map(|link| {
                let bbox = link.bbox?;
                let doi = doi_in(&link.uri)?;
                Some((f32::midpoint(bbox.y0, bbox.y1), bbox.x0, doi))
            })
            .collect();
        if dois.is_empty() {
            continue;
        }
        let reach = page.width * 0.6;
        for (cy, cx, doi) in dois {
            // The lowest entry that starts at or above the link, in its column.
            let mut best: Option<(usize, f32)> = None;
            for (k, entry) in entries.iter().enumerate() {
                if entry.page != page.page {
                    continue;
                }
                let Some(anchor) = entry.anchor else {
                    continue;
                };
                let top = anchor.y1;
                if top < cy - 0.5 * (anchor.y1 - anchor.y0) {
                    continue;
                }
                if cx < anchor.x0 - 12.0 || cx > anchor.x0 + reach {
                    continue;
                }
                if best.is_none_or(|(_, y)| anchor.y0 < y) {
                    best = Some((k, anchor.y0));
                }
            }
            if let Some((k, _)) = best
                && entries[k].doi_link.is_none()
            {
                entries[k].doi_link = Some(doi);
            }
        }
    }
}

/// Letters and digits of `text`, lower-cased, diacritics removed. Stroked
/// and ligature letters that NFKD leaves alone are mapped by hand, since
/// registries often store their ASCII forms (`Obtułowicz` as `Obtulowicz`).
fn folded(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.nfkd() {
        if !(c.is_alphanumeric() || c.is_whitespace()) {
            continue;
        }
        match c {
            'ł' | 'Ł' => out.push('l'),
            'đ' | 'Đ' | 'ð' | 'Ð' => out.push('d'),
            'ø' | 'Ø' => out.push('o'),
            'ı' => out.push('i'),
            'ß' => out.push_str("ss"),
            'æ' | 'Æ' => out.push_str("ae"),
            'œ' | 'Œ' => out.push_str("oe"),
            'þ' | 'Þ' => out.push_str("th"),
            other => out.extend(other.to_lowercase()),
        }
    }
    out
}

/// Levenshtein similarity in `0..=1` over chars.
fn similarity(a: &str, b: &str) -> f32 {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    let distance = prev[b.len()];
    let longest = a.len().max(b.len());
    1.0 - (distance as f32) / (longest as f32)
}

/// The family name of a record author (`Given Family`): its last word, or
/// the last two when the second-last is a lower-case particle.
fn family_of(name: &str) -> String {
    let words: Vec<&str> = name.split_whitespace().collect();
    match words.as_slice() {
        [] => String::new(),
        [.., particle, last]
            if particle.chars().next().is_some_and(char::is_lowercase) && particle.len() <= 4 =>
        {
            format!("{particle} {last}")
        }
        [.., last] => (*last).to_string(),
    }
}

/// Agreement between the record's first author and the printed first
/// author: 1 when the record's family name is a word of the printed name,
/// otherwise the best similarity of the family name to any printed word.
#[cfg(test)]
fn author_agreement(printed: &str, record: &str) -> f32 {
    let family = folded(&family_of(record));
    let printed = folded(printed);
    if family.is_empty() || printed.is_empty() {
        return 0.0;
    }
    if printed.split_whitespace().any(|w| w == family) || printed.contains(&family) {
        return 1.0;
    }
    printed
        .split_whitespace()
        .map(|w| similarity(w, &family))
        .fold(0.0, f32::max)
}

/// Agreement between two titles after folding.
fn title_agreement(a: &str, b: &str) -> f32 {
    let a = folded(a);
    let b = folded(b);
    let a = a.split_whitespace().collect::<Vec<_>>().join(" ");
    let b = b.split_whitespace().collect::<Vec<_>>().join(" ");
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    similarity(&a, &b)
}

/// Least share of a record title's words that must appear in the entry.
const TITLE_OVERLAP_MIN: f32 = 0.6;
/// Least similarity between a record family name and an entry word.
const FAMILY_WORD_MIN: f32 = 0.8;

/// Words of `folded` text that are at least `min` chars long, deduplicated.
fn words(text: &str, min: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for w in text.split_whitespace() {
        if w.chars().count() >= min && !out.iter().any(|o| o == w) {
            out.push(w.to_string());
        }
    }
    out
}

/// Does the record's first-author family name appear in the printed entry?
/// A word match, a match with the spaces removed (a floating accent glyph
/// splits `Rühland` into `Ru Èhland`), or a close word.
fn family_in_entry(family: &str, raw_folded: &str) -> bool {
    let family = folded(family);
    if family.is_empty() {
        return false;
    }
    if raw_folded.split_whitespace().any(|w| w == family) {
        return true;
    }
    let squashed: String = raw_folded.chars().filter(|c| !c.is_whitespace()).collect();
    let family_squashed: String = family.chars().filter(|c| !c.is_whitespace()).collect();
    // The first author opens the entry: `RowJR` for `Row JR`.
    if squashed.starts_with(&family_squashed) {
        return true;
    }
    if family_squashed.chars().count() >= 4 && squashed.contains(&family_squashed) {
        return true;
    }
    // A floating accent glyph splits a name (`Ru Èhland`, `Arau Âjo`): compare
    // one- and two-word windows of the entry's opening with the spaces removed.
    let opening: Vec<&str> = raw_folded.split_whitespace().take(8).collect();
    for (k, w) in opening.iter().enumerate() {
        if w.chars().count() >= 4 && similarity(w, &family_squashed) >= FAMILY_WORD_MIN {
            return true;
        }
        if let Some(next) = opening.get(k + 1) {
            let pair = format!("{w}{next}");
            if pair.chars().count() >= 4 && similarity(&pair, &family_squashed) >= FAMILY_WORD_MIN {
                return true;
            }
        }
    }
    false
}

/// Share of the record title's words (4+ chars) that the entry contains,
/// or `None` when the title has fewer than two such words.
fn title_overlap(title: &str, raw_folded: &str) -> Option<f32> {
    let needles = words(&folded(title), 4);
    if needles.len() < 2 {
        return None;
    }
    let hay = words(raw_folded, 1);
    let hits = needles
        .iter()
        .filter(|n| hay.iter().any(|h| h == *n))
        .count();
    Some(hits as f32 / needles.len() as f32)
}

/// Does the record year, or the year before or after it, appear in the entry?
fn year_in_entry(year: u16, raw_folded: &str) -> bool {
    let candidates = [year.saturating_sub(1), year, year.saturating_add(1)];
    raw_folded
        .split_whitespace()
        .any(|w| candidates.iter().any(|y| w == y.to_string()))
}

/// Verify `record` against the printed `entry`: the score, or why not
/// (which check failed, with both values). The checks read the raw entry
/// text, not the parsed fields, so a parser slip cannot reject a correct
/// record: the record's first-author family name must appear in the
/// entry, its year (within one) must appear, and, when the record title
/// has words to check, most of them must appear.
fn verify(record: &PaperRecord, entry: &ReferenceEntry) -> Result<f32, String> {
    let raw = folded(&entry.raw);
    let mut score_parts: Vec<f32> = Vec::new();
    if let Some(year) = record.year {
        if !year_in_entry(year, &raw) {
            return Err(format!(
                "year: record {year} not in entry (printed {})",
                entry.year.map_or("none".to_string(), |y| y.to_string())
            ));
        }
        score_parts.push(1.0);
    }
    let title = title_overlap(&record.title, &raw);
    if let Some(first) = record.authors.first() {
        let family = family_of(first);
        if !family_in_entry(&family, &raw) {
            return Err(format!(
                "first author: record {first:?} ({family}) not in entry (printed {})",
                entry
                    .authors
                    .first()
                    .map_or("none".to_string(), |a| format!("{a:?}"))
            ));
        }
        score_parts.push(1.0);
        if let Some(overlap) = title {
            if overlap < TITLE_OVERLAP_MIN {
                return Err(format!(
                    "title: record {:?} shares {:.0}% of its words with the entry",
                    record.title,
                    overlap * 100.0
                ));
            }
            score_parts.push(overlap);
        }
    } else {
        let Some(overlap) = title else {
            return Err("nothing to compare: record has no author and no usable title".to_string());
        };
        if overlap < TITLE_MIN {
            return Err(format!(
                "title: record {:?} shares {:.0}% of its words with the entry (no record author)",
                record.title,
                overlap * 100.0
            ));
        }
        score_parts.push(overlap);
    }
    if score_parts.is_empty() {
        return Err("nothing to compare: record has no year, author or title".to_string());
    }
    Ok(score_parts.iter().sum::<f32>() / score_parts.len() as f32)
}

/// The accepted record as stored on the entry.
fn resolved_from(record: &PaperRecord, doi: &str, method: &str, score: f32) -> Resolved {
    Resolved {
        doi: (!doi.is_empty()).then(|| doi.to_string()),
        pmid: record.pmid.clone(),
        pmcid: record.pmcid.clone(),
        title: (!record.title.is_empty()).then(|| record.title.clone()),
        authors: record.authors.clone(),
        year: record.year,
        venue: record.venue.clone(),
        source: record.source.clone(),
        method: method.to_string(),
        score,
    }
}

/// How a batch of entries resolved.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct Outcome {
    pub entries: usize,
    pub resolved: usize,
    /// A record was found but disagreed with the printed entry.
    pub rejected: usize,
    pub unresolved: usize,
    /// Entries with at least one failed registry request, including enrichment.
    /// An error can coexist with a successful resolution through another source.
    pub errors: usize,
    pub by_method: BTreeMap<String, usize>,
}

/// Crossref `/works?query.bibliographic=…`: the query field meant for whole
/// citation strings (author, title, venue and year weighed together), unlike
/// the plain `query`.
fn bibliographic_search(
    client: &Client,
    text: &str,
    rows: u32,
) -> Result<Vec<tpe_biblio::Found>, BiblioError> {
    let n = rows.to_string();
    let mut pairs: Vec<(&str, &str)> = vec![("query.bibliographic", text), ("rows", n.as_str())];
    if let Some(m) = client.mailto() {
        pairs.push(("mailto", m));
    }
    let url = with_query(&format!("{}/works", crossref::BASE), &pairs);
    crossref::parse_crossref_found(&client.get_text(&url, &[])?)
}

/// Does a meaningful word of the record's venue (3+ chars, `RNA`, `Lancet`)
/// appear in the entry? Function words such as "and" in a title are not
/// journal evidence. Venue names help distinguish articles from preprints or
/// posters when title, authors and year all agree.
fn venue_in_entry(venue: &str, raw_folded: &str) -> bool {
    let needles = words(&folded(venue), 3);
    if needles.is_empty() {
        return false;
    }
    let hay = words(raw_folded, 1);
    needles.iter().any(|n| {
        !matches!(
            n.as_str(),
            "and" | "the" | "for" | "with" | "from" | "into" | "via"
        ) && hay.iter().any(|h| h == n)
    })
}

/// Minimum winning margin between distinct query candidates. Search order is
/// not identity evidence; unresolved ambiguity is preferable to a false edge.
const QUERY_MARGIN: f32 = 0.05;

fn select_query_record(
    entry: &mut ReferenceEntry,
    records: impl IntoIterator<Item = PaperRecord>,
) -> Option<Resolved> {
    let raw = folded(&entry.raw);
    let mut candidates: Vec<(f32, Resolved)> = Vec::new();
    for record in records {
        let Some(doi) = record.doi.as_deref().and_then(normalize_doi) else {
            continue;
        };
        // Author/year alone cannot establish a search result's identity (e.g.
        // two works by Smith in 2020). Exact-ID lookups have separate evidence.
        let checked = if title_overlap(&record.title, &raw).is_none() {
            Err("title: insufficient title evidence for a bibliographic query".to_string())
        } else {
            verify(&record, entry)
        };
        match checked {
            Ok(score) => {
                let venue_bonus = if record
                    .venue
                    .as_deref()
                    .is_some_and(|v| venue_in_entry(v, &raw))
                {
                    0.5
                } else {
                    0.0
                };
                let total = score + venue_bonus;
                entry.attempts.push(Attempt {
                    method: "query".to_string(),
                    doi: Some(doi.clone()),
                    outcome: "candidate".to_string(),
                    detail: Some(format!("metadata agrees, ranking score {total:.2}")),
                });
                candidates.push((total, resolved_from(&record, &doi, "query", score)));
            }
            Err(detail) => entry.attempts.push(Attempt {
                method: "query".to_string(),
                doi: Some(doi),
                outcome: "mismatch".to_string(),
                detail: Some(detail),
            }),
        }
    }
    candidates.sort_by(|a, b| b.0.total_cmp(&a.0));
    let (score, best) = candidates.first()?;
    if candidates
        .iter()
        .any(|(other, record)| record.doi != best.doi && score - other < QUERY_MARGIN)
    {
        entry.attempts.push(Attempt {
            method: "query".to_string(),
            doi: None,
            outcome: "ambiguous".to_string(),
            detail: Some("distinct DOIs have indistinguishable bibliographic evidence".to_string()),
        });
        return None;
    }
    let best = best.clone();
    entry.attempts.push(Attempt {
        method: "query".to_string(),
        doi: best.doi.clone(),
        outcome: "verified".to_string(),
        detail: None,
    });
    Some(best)
}

/// Prefer the parser's wrap repair, except when raw text extends that same DOI
/// (the parser can trim balanced suffix punctuation).
fn printed_doi(entry: &ReferenceEntry) -> Option<String> {
    let parsed = entry.doi.as_deref().and_then(doi_in);
    let raw = doi_in(&entry.raw);
    match (parsed, raw) {
        (Some(parsed), Some(raw)) if raw.starts_with(&parsed) => Some(raw),
        (Some(parsed), _) => Some(parsed),
        (None, raw) => raw,
    }
}

fn exact_paper_record(title: &str, doi: &str, record: &PaperRecord) -> Option<Resolved> {
    if record.doi.as_deref().and_then(normalize_doi) != normalize_doi(doi) {
        return None;
    }
    let score = if title.is_empty() {
        1.0
    } else {
        title_agreement(title, &record.title)
    };
    (score >= PAPER_TITLE_MIN).then(|| resolved_from(record, doi, "metadata", score))
}

fn select_paper_record(
    title: &str,
    records: impl IntoIterator<Item = PaperRecord>,
) -> Option<Resolved> {
    let mut candidates: Vec<(f32, Resolved)> = records
        .into_iter()
        .filter_map(|record| {
            let doi = record.doi.as_deref().and_then(normalize_doi)?;
            let score = title_agreement(title, &record.title);
            (score >= PAPER_TITLE_MIN)
                .then(|| (score, resolved_from(&record, &doi, "query", score)))
        })
        .collect();
    candidates.sort_by(|a, b| b.0.total_cmp(&a.0));
    let (score, best) = candidates.first()?;
    if candidates
        .iter()
        .any(|(other, record)| record.doi != best.doi && score - other < QUERY_MARGIN)
    {
        return None;
    }
    Some(best.clone())
}

/// Count unresolved metadata disagreements, excluding candidates withheld only
/// because identity evidence is ambiguous. Inspect only this resolution pass.
fn count_rejection(attempts: &[Attempt], outcome: &mut Outcome) {
    if attempts.iter().any(|a| a.outcome == "mismatch")
        && !attempts.iter().any(|a| a.outcome == "ambiguous")
    {
        outcome.rejected += 1;
    }
}

/// Run `request` again after a rate limit or transport failure, backing
/// off `RETRY_BASE`, `2 x RETRY_BASE`, ... up to `RETRIES` times.
fn with_retry<T>(mut request: impl FnMut() -> Result<T, BiblioError>) -> Result<T, BiblioError> {
    let mut wait = RETRY_BASE;
    let mut attempt = 0;
    loop {
        match request() {
            Err(err @ (BiblioError::RateLimited | BiblioError::Transport(_)))
                if attempt < RETRIES =>
            {
                let _ = err;
                std::thread::sleep(wait);
                wait *= 2;
                attempt += 1;
            }
            other => return other,
        }
    }
}

/// Explicit identifiers in the reference text. Bare numbers are not PMIDs.
fn biomedical_ids(text: &str) -> Vec<pmc::Identifier> {
    static PMID: OnceLock<Regex> = OnceLock::new();
    static PMCID: OnceLock<Regex> = OnceLock::new();
    let pubmed_pattern = PMID.get_or_init(|| Regex::new(
        r"(?i)\b(?:PMID\s*:\s*|pubmed\.ncbi\.nlm\.nih\.gov/|ncbi\.nlm\.nih\.gov/pubmed/)([1-9]\d{0,11})\b"
    ).expect("valid regex"));
    let central_pattern =
        PMCID.get_or_init(|| Regex::new(r"(?i)\bPMC([1-9]\d{0,11})\b").expect("valid regex"));
    let mut ids = Vec::new();
    let mut seen = HashSet::new();
    for found in pubmed_pattern.captures_iter(text) {
        let id = pmc::Identifier::Pmid(found[1].to_string());
        if seen.insert(id.clone()) {
            ids.push(id);
        }
    }
    for found in central_pattern.captures_iter(text) {
        let id = pmc::Identifier::Pmcid(format!("PMC{}", &found[1]));
        if seen.insert(id.clone()) {
            ids.push(id);
        }
    }
    ids
}

/// Accept one exact registry identity only if its metadata agrees with the
/// printed reference. Distinct matching biomedical records remain ambiguous.
fn select_biomedical_record(
    entry: &mut ReferenceEntry,
    id: &pmc::Identifier,
    records: impl IntoIterator<Item = PaperRecord>,
) -> Option<Resolved> {
    let mut accepted: Vec<Resolved> = Vec::new();
    for record in records {
        if !id.matches(&record) {
            entry.attempts.push(Attempt {
                method: "europepmc".to_string(),
                doi: record.doi.clone(),
                outcome: "mismatch".to_string(),
                detail: Some(
                    "lookup returned a different or missing biomedical identifier".to_string(),
                ),
            });
            continue;
        }
        if biomedical_ids(&entry.raw)
            .iter()
            .any(|printed| !printed.matches(&record))
        {
            entry.attempts.push(Attempt {
                method: "europepmc".to_string(),
                doi: record.doi.clone(),
                outcome: "mismatch".to_string(),
                detail: Some("printed PMID and PMCID do not identify the same record".to_string()),
            });
            continue;
        }
        match verify(&record, entry) {
            Ok(score) => {
                let resolved = resolved_from(
                    &record,
                    record.doi.as_deref().unwrap_or(""),
                    "europepmc",
                    score,
                );
                if !accepted.iter().any(|r| {
                    r.doi == resolved.doi && r.pmid == resolved.pmid && r.pmcid == resolved.pmcid
                }) {
                    accepted.push(resolved);
                }
            }
            Err(detail) => entry.attempts.push(Attempt {
                method: "europepmc".to_string(),
                doi: record.doi.clone(),
                outcome: "mismatch".to_string(),
                detail: Some(detail),
            }),
        }
    }
    let outcome = match accepted.len() {
        0 => "not_found",
        1 => "verified",
        _ => "ambiguous",
    };
    entry.attempts.push(Attempt {
        method: "europepmc".to_string(),
        doi: accepted.first().and_then(|r| r.doi.clone()),
        outcome: outcome.to_string(),
        detail: Some(format!("exact lookup {id:?}")),
    });
    if accepted.len() == 1 {
        accepted.pop()
    } else {
        None
    }
}

/// Explicit biomedical IDs are authoritative. If none can verify all printed
/// IDs, leave the entry unresolved rather than accepting a DOI/query fallback.
/// Returns whether explicit IDs were present (and therefore handled).
fn resolve_explicit_biomedical(
    entry: &mut ReferenceEntry,
    mut lookup: impl FnMut(&mut ReferenceEntry, &pmc::Identifier) -> Option<Resolved>,
) -> bool {
    let ids = biomedical_ids(&entry.raw);
    if ids.is_empty() {
        return false;
    }
    for id in ids {
        if let Some(resolved) = lookup(entry, &id) {
            entry.resolved = Some(resolved);
            break;
        }
    }
    true
}

/// A Crossref resolver with the polite-pool rate limit.
pub struct Resolver {
    client: Client,
}

impl Resolver {
    /// A resolver identifying as `tpe` with `mailto` for Crossref's polite pool.
    #[must_use]
    pub fn new(mailto: Option<&str>) -> Self {
        let mut client = Client::new(concat!("tpe/", env!("CARGO_PKG_VERSION")))
            .with_host_interval(CROSSREF_HOST, CROSSREF_INTERVAL);
        if let Some(mailto) = mailto {
            client = client.with_mailto(mailto);
        }
        Self { client }
    }

    fn try_biomedical(&self, entry: &mut ReferenceEntry, id: &pmc::Identifier) -> Option<Resolved> {
        match with_retry(|| pmc::fetch_identifier(&self.client, id)) {
            Ok(found) => select_biomedical_record(entry, id, found.into_iter().map(|f| f.record)),
            Err(err) => {
                entry.attempts.push(Attempt {
                    method: "europepmc".to_string(),
                    doi: None,
                    outcome: "error".to_string(),
                    detail: Some(format!("{id:?}: {err}")),
                });
                None
            }
        }
    }

    /// Enrich an already verified DOI with exact biomedical cross-identifiers.
    /// Failure leaves the accepted Crossref record intact and logs the attempt.
    fn enrich_identifiers(&self, entry: &mut ReferenceEntry) {
        let Some(resolved) = &entry.resolved else {
            return;
        };
        if resolved.pmid.is_some() || resolved.pmcid.is_some() {
            return;
        }
        let Some(doi) = resolved.doi.clone() else {
            return;
        };
        if let Some(extra) = self.try_biomedical(entry, &pmc::Identifier::Doi(doi))
            && let Some(resolved) = &mut entry.resolved
        {
            resolved.pmid = extra.pmid;
            resolved.pmcid = extra.pmcid;
        }
    }

    /// Fetch and verify one DOI for `entry`, logging the attempt.
    fn try_doi(
        &self,
        entry: &mut ReferenceEntry,
        doi: &str,
        method: &str,
    ) -> Result<Option<Resolved>, BiblioError> {
        let mut attempt = Attempt {
            method: method.to_string(),
            doi: Some(doi.to_string()),
            outcome: String::new(),
            detail: None,
        };
        let found = match with_retry(|| crossref::fetch_by_doi(&self.client, doi)) {
            Ok(found) => found,
            Err(err) => {
                attempt.outcome = "error".to_string();
                attempt.detail = Some(err.to_string());
                entry.attempts.push(attempt);
                return Err(err);
            }
        };
        let Some(found) = found else {
            attempt.outcome = "not_found".to_string();
            entry.attempts.push(attempt);
            return Ok(None);
        };
        let record = found.record;
        if record.doi.as_deref().and_then(normalize_doi).as_deref() != Some(doi) {
            attempt.outcome = "mismatch".to_string();
            attempt.detail = Some("lookup returned a different or missing DOI".to_string());
            entry.attempts.push(attempt);
            return Ok(None);
        }
        let outcome = match verify(&record, entry) {
            Ok(score) => {
                attempt.outcome = "verified".to_string();
                Some(resolved_from(&record, doi, method, score))
            }
            Err(detail) => {
                attempt.outcome = "mismatch".to_string();
                attempt.detail = Some(detail);
                None
            }
        };
        entry.attempts.push(attempt);
        Ok(outcome)
    }

    /// Resolve one entry in place; returns the method that succeeded.
    fn resolve_entry(
        &self,
        entry: &mut ReferenceEntry,
    ) -> Result<Option<&'static str>, BiblioError> {
        if resolve_explicit_biomedical(entry, |entry, id| self.try_biomedical(entry, id)) {
            return Ok(entry.resolved.as_ref().map(|_| "europepmc"));
        }
        let mut candidates: Vec<(String, &'static str)> = Vec::new();
        if let Some(doi) = entry.doi_link.as_deref().and_then(doi_in) {
            candidates.push((doi, "link"));
        }
        if let Some(doi) = printed_doi(entry)
            && !candidates.iter().any(|(d, _)| *d == doi)
        {
            candidates.push((doi, "printed"));
        }
        for (doi, method) in &candidates {
            // Keep logged errors; another registry may still resolve it.
            if let Ok(Some(resolved)) = self.try_doi(entry, doi, method) {
                entry.resolved = Some(resolved);
                return Ok(Some(method));
            }
        }
        for (doi, _) in &candidates {
            if let Some(resolved) = self.try_biomedical(entry, &pmc::Identifier::Doi(doi.clone())) {
                entry.resolved = Some(resolved);
                return Ok(Some("europepmc"));
            }
        }
        let query: String = entry.raw.chars().take(QUERY_CHARS).collect();
        let found = match with_retry(|| bibliographic_search(&self.client, &query, QUERY_ROWS)) {
            Ok(found) => found,
            Err(err) => {
                entry.attempts.push(Attempt {
                    method: "query".to_string(),
                    doi: None,
                    outcome: "error".to_string(),
                    detail: Some(err.to_string()),
                });
                return Err(err);
            }
        };
        if found.is_empty() {
            entry.attempts.push(Attempt {
                method: "query".to_string(),
                doi: None,
                outcome: "not_found".to_string(),
                detail: None,
            });
        }
        if let Some(resolved) = select_query_record(entry, found.into_iter().map(|f| f.record)) {
            entry.resolved = Some(resolved);
            return Ok(Some("query"));
        }
        Ok(None)
    }

    /// Resolve every entry that has no record yet. Errors are counted, not
    /// returned: a failed request leaves that entry unresolved.
    pub fn resolve_entries(&self, entries: &mut [ReferenceEntry]) -> Outcome {
        let mut outcome = Outcome {
            entries: entries.len(),
            ..Outcome::default()
        };
        for entry in entries.iter_mut() {
            let first_attempt = entry.attempts.len();
            if entry.resolved.is_some() {
                self.enrich_identifiers(entry);
                outcome.resolved += 1;
            } else {
                match self.resolve_entry(entry) {
                    Ok(Some(method)) => {
                        self.enrich_identifiers(entry);
                        outcome.resolved += 1;
                        *outcome.by_method.entry(method.to_string()).or_insert(0) += 1;
                    }
                    Ok(None) => outcome.unresolved += 1,
                    Err(_) => {
                        outcome.unresolved += 1;
                    }
                }
            }
            if entry.resolved.is_none() {
                count_rejection(&entry.attempts[first_attempt..], &mut outcome);
            }
            if entry.attempts[first_attempt..]
                .iter()
                .any(|a| a.outcome == "error")
            {
                outcome.errors += 1;
            }
        }
        outcome
    }

    /// The paper's own record: its metadata DOI when it verifies against
    /// the metadata title, otherwise a query on the title verified by title
    /// agreement of at least [`PAPER_TITLE_MIN`].
    #[must_use]
    pub fn resolve_paper(&self, meta: &Metadata) -> Option<Resolved> {
        let title = meta.title.as_deref().unwrap_or("");
        if let Some(doi) = meta.doi.as_deref().and_then(doi_in)
            && let Ok(Some(found)) = with_retry(|| crossref::fetch_by_doi(&self.client, &doi))
            && let Some(resolved) = exact_paper_record(title, &doi, &found.record)
        {
            return Some(resolved);
        }
        if title.len() < 12 {
            return None;
        }
        let found = with_retry(|| bibliographic_search(&self.client, title, QUERY_ROWS)).ok()?;
        select_paper_record(title, found.into_iter().map(|f| f.record))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{BBox, Link};

    #[test]
    fn doi_strings_are_cut_at_sentence_punctuation_and_lower_cased() {
        assert_eq!(
            doi_in("https://doi.org/10.1000/ABC.123."),
            Some("10.1000/abc.123".to_string())
        );
        assert_eq!(
            doi_in("doi: 10.1016/j.cell.2020.01.001)"),
            Some("10.1016/j.cell.2020.01.001".to_string())
        );
        assert_eq!(doi_in("no doi here"), None);
    }

    #[test]
    fn doi_lookup_keeps_balanced_biomedical_suffixes() {
        for doi in [
            "10.1016/S0140-6736(20)30183-5",
            "10.1002/(SICI)1097-0258(19980815)17:15<1741::AID-SIM868>3.0.CO;2-8",
            "10.1000/example(abc)",
        ] {
            assert_eq!(
                doi_in(&format!("doi: {doi}).")),
                Some(doi.to_ascii_lowercase())
            );
        }
        assert_eq!(
            doi_in("[doi:10.1000/example]."),
            Some("10.1000/example".to_string())
        );
    }

    fn query_record(doi: &str) -> PaperRecord {
        PaperRecord {
            doi: Some(doi.to_string()),
            title: "Molecular mechanisms of inflammation".to_string(),
            authors: vec!["Jane Smith".to_string()],
            year: Some(2020),
            source: "crossref".to_string(),
            ..PaperRecord::default()
        }
    }

    fn query_entry() -> ReferenceEntry {
        ReferenceEntry {
            raw: "Smith J. Molecular mechanisms of inflammation. Immunology 2020; 1:2-3."
                .to_string(),
            ..ReferenceEntry::default()
        }
    }

    #[test]
    fn query_ambiguity_is_not_decided_by_response_order() {
        let a = query_record("10.1000/a");
        let b = query_record("10.1000/b");
        for records in [[a.clone(), b.clone()], [b, a]] {
            let mut entry = query_entry();
            assert!(select_query_record(&mut entry, records).is_none());
            assert_eq!(entry.attempts.last().unwrap().outcome, "ambiguous");
            assert!(!entry.attempts.iter().any(|a| a.outcome == "verified"));
        }
    }

    #[test]
    fn query_needs_title_evidence_and_deduplicates_identical_dois() {
        let mut incomplete = query_record("10.1000/missing-title");
        incomplete.title.clear();
        let mut entry = query_entry();
        assert!(select_query_record(&mut entry, [incomplete]).is_none());
        let exact = query_record("10.1000/a");
        let selected = select_query_record(&mut entry, [exact.clone(), exact]).unwrap();
        assert_eq!(selected.doi.as_deref(), Some("10.1000/a"));
        assert_eq!(entry.attempts.last().unwrap().outcome, "verified");
    }

    #[test]
    fn query_uses_venue_evidence_to_distinguish_versions() {
        for venue in ["Immunology", "RNA", "The Lancet"] {
            let preprint = query_record("10.1000/preprint");
            let mut article = query_record("10.1000/article");
            article.venue = Some(venue.to_string());
            let mut entry = query_entry();
            entry.raw = entry.raw.replace("Immunology", venue);
            let selected = select_query_record(&mut entry, [preprint, article]).unwrap();
            assert_eq!(selected.doi.as_deref(), Some("10.1000/article"));
        }
    }

    #[test]
    fn venue_function_word_does_not_outrank_a_better_title_match() {
        // PMC11033918 reference 9, independently labeled by its JATS DOI.
        // The live registry returned a related conference abstract whose venue
        // shares only "and" with the citation. That is not journal evidence.
        let article = PaperRecord {
            doi: Some("10.1200/op.20.00266".to_string()),
            title: "Restricted mouth opening in head and neck cancer: etiology, prevention, and treatment".to_string(),
            authors: vec!["W. Abboud".to_string()],
            year: Some(2020),
            venue: Some("JCO Oncology Practice".to_string()),
            ..PaperRecord::default()
        };
        let abstract_record = PaperRecord {
            doi: Some("10.1016/j.ijom.2019.03.511".to_string()),
            title: "Reduced mouth opening in head and neck cancer patients".to_string(),
            authors: vec!["W. Abboud".to_string()],
            year: Some(2019),
            venue: Some("International Journal of Oral and Maxillofacial Surgery".to_string()),
            ..PaperRecord::default()
        };
        for records in [
            [article.clone(), abstract_record.clone()],
            [abstract_record, article],
        ] {
            let mut entry = ReferenceEntry {
                raw: "Abboud. Restricted mouth opening in head and neck cancer: etiology, prevention, and treatment. 2020.".to_string(),
                ..ReferenceEntry::default()
            };
            let selected = select_query_record(&mut entry, records).unwrap();
            assert_eq!(selected.doi.as_deref(), Some("10.1200/op.20.00266"));
        }
    }

    #[test]
    fn venue_function_word_does_not_remove_query_ambiguity() {
        let mut a = query_record("10.1000/a");
        a.title = "Molecular mechanisms for inflammation".to_string();
        let mut b = a.clone();
        b.doi = Some("10.1000/b".to_string());
        b.venue = Some("Journal for Biomedical Research".to_string());
        for records in [[a.clone(), b.clone()], [b, a]] {
            let mut entry = ReferenceEntry {
                raw: "Smith J. Molecular mechanisms for inflammation. 2020.".to_string(),
                ..ReferenceEntry::default()
            };
            assert!(select_query_record(&mut entry, records).is_none());
            assert_eq!(entry.attempts.last().unwrap().outcome, "ambiguous");
        }
    }

    #[test]
    #[ignore = "requires live Crossref access"]
    fn live_crossref_exact_identifier() {
        let mut entries = [ReferenceEntry {
            raw: "Piwowar H, Priem J, Larivière V, et al. The state of OA: a large-scale analysis of the prevalence and impact of Open Access articles. PeerJ 2018. doi:10.7717/peerj.4375".to_string(),
            ..ReferenceEntry::default()
        }];
        let outcome = Resolver::new(None).resolve_entries(&mut entries);
        assert_eq!(outcome.resolved, 1, "{:?}", entries[0].attempts);
        assert_eq!(
            entries[0].resolved.as_ref().unwrap().doi.as_deref(),
            Some("10.7717/peerj.4375")
        );
        assert_eq!(entries[0].resolved.as_ref().unwrap().method, "printed");
    }

    #[test]
    fn printed_doi_preserves_repairs_and_raw_extensions() {
        let mut entry = ReferenceEntry {
            raw: "doi: 10.1145/364399 1.3648400".to_string(),
            doi: Some("10.1145/3643991.3648400".to_string()),
            ..ReferenceEntry::default()
        };
        assert_eq!(
            printed_doi(&entry).as_deref(),
            Some("10.1145/3643991.3648400")
        );
        entry.raw = "doi: 10.1000/example(abc)".to_string();
        entry.doi = Some("10.1000/example(abc".to_string());
        assert_eq!(printed_doi(&entry).as_deref(), Some("10.1000/example(abc)"));
        entry.doi = None;
        assert_eq!(printed_doi(&entry), doi_in(&entry.raw));
    }

    #[test]
    fn paper_exact_lookup_requires_normalized_requested_doi() {
        let mut record = query_record("10.1000/other");
        for title in ["", record.title.as_str()] {
            assert!(exact_paper_record(title, "10.1000/requested", &record).is_none());
        }
        record.doi = None;
        assert!(exact_paper_record("", "10.1000/requested", &record).is_none());
        record.doi = Some("https://doi.org/10.1000/REQUESTED".to_string());
        assert!(exact_paper_record(&record.title, "10.1000/requested", &record).is_some());
    }

    #[test]
    fn paper_query_checks_near_ties_in_both_orders() {
        let a = query_record("10.1000/a");
        let mut b = query_record("10.1000/b");
        b.title.push('s');
        for records in [[a.clone(), b.clone()], [b.clone(), a.clone()]] {
            assert!(select_paper_record(&a.title, records).is_none());
        }
        b.doi = Some("https://doi.org/10.1000/A".to_string());
        assert!(select_paper_record(&a.title, [a.clone(), b.clone()]).is_some());
        b.doi = Some("10.1000/b".to_string());
        b.title = "Unrelated research findings".to_string();
        assert_eq!(
            select_paper_record(&a.title, [b, a.clone()])
                .unwrap()
                .doi
                .as_deref(),
            Some("10.1000/a")
        );
    }

    #[test]
    fn ambiguity_is_unresolved_without_metadata_rejection() {
        let mut entry = query_entry();
        let mut wrong = query_record("10.1000/wrong");
        wrong.title = "Unrelated research findings".to_string();
        assert!(
            select_query_record(
                &mut entry,
                [
                    wrong.clone(),
                    query_record("10.1000/a"),
                    query_record("10.1000/b")
                ]
            )
            .is_none()
        );
        let mut outcome = Outcome::default();
        count_rejection(&entry.attempts, &mut outcome);
        assert_eq!(outcome.rejected, 0);
        entry.attempts.clear();
        assert!(select_query_record(&mut entry, [wrong]).is_none());
        count_rejection(&entry.attempts, &mut outcome);
        assert_eq!(outcome.rejected, 1);
    }

    #[test]
    fn explicit_biomedical_ids_are_typed_and_deduplicated() {
        assert_eq!(
            biomedical_ids(
                "PMID: 123456 PMID:123456 https://pubmed.ncbi.nlm.nih.gov/123456/ PMCID:PMC7654321"
            ),
            vec![
                pmc::Identifier::Pmid("123456".to_string()),
                pmc::Identifier::Pmcid("PMC7654321".to_string())
            ]
        );
        assert!(biomedical_ids("2020; 123456:7654321").is_empty());
    }

    fn biomedical_record() -> PaperRecord {
        PaperRecord {
            pmid: Some("123456".to_string()),
            pmcid: Some("PMC7654321".to_string()),
            doi: None,
            source: "europepmc".to_string(),
            ..query_record("unused")
        }
    }

    #[test]
    fn pubmed_record_without_doi_resolves_without_inventing_one() {
        let mut entry = query_entry();
        entry.raw.push_str(" PMID:123456");
        let resolved = select_biomedical_record(
            &mut entry,
            &pmc::Identifier::Pmid("123456".to_string()),
            [biomedical_record()],
        )
        .unwrap();
        assert_eq!(resolved.doi, None);
        assert_eq!(resolved.pmid.as_deref(), Some("123456"));
        assert_eq!(resolved.pmcid.as_deref(), Some("PMC7654321"));
        assert_eq!(resolved.source, "europepmc");
        let json = serde_json::to_value(&resolved).unwrap();
        assert!(json["doi"].is_null());
        assert_eq!(serde_json::from_value::<Resolved>(json).unwrap(), resolved);
    }

    #[test]
    fn biomedical_lookup_rejects_wrong_ids_metadata_and_conflicts() {
        let id = pmc::Identifier::Pmid("123456".to_string());
        let mut wrong = biomedical_record();
        wrong.pmid = Some("999999".to_string());
        assert!(select_biomedical_record(&mut query_entry(), &id, [wrong]).is_none());
        let mut wrong = biomedical_record();
        wrong.title = "An unrelated clinical investigation".to_string();
        assert!(select_biomedical_record(&mut query_entry(), &id, [wrong]).is_none());
        let mut entry = query_entry();
        entry.raw.push_str(" PMID:123456 PMCID:PMC111111");
        assert!(select_biomedical_record(&mut entry, &id, [biomedical_record()]).is_none());
        assert!(entry.attempts.iter().any(|a| a.outcome == "mismatch"));
    }

    #[test]
    fn biomedical_lookup_does_not_pick_distinct_records_by_order() {
        let mut second = biomedical_record();
        second.pmcid = Some("PMC111111".to_string());
        let mut entry = query_entry();
        assert!(
            select_biomedical_record(
                &mut entry,
                &pmc::Identifier::Pmid("123456".to_string()),
                [biomedical_record(), second]
            )
            .is_none()
        );
        assert_eq!(entry.attempts.last().unwrap().outcome, "ambiguous");
    }

    #[test]
    fn explicit_ids_block_fallback_for_conflicts_missing_records_and_bad_metadata() {
        for suffix in [
            "PMID:999999",
            "PMCID:PMC999999",
            "PMID:123456 PMCID:PMC111111",
            "PMID:123456 PMID:999999",
        ] {
            let mut entry = query_entry();
            entry.raw.push(' ');
            entry.raw.push_str(suffix);
            entry.raw.push_str(" doi:10.1000/a");
            assert!(resolve_explicit_biomedical(&mut entry, |entry, id| {
                select_biomedical_record(entry, id, [biomedical_record()])
            }));
            assert!(entry.resolved.is_none());
            let mut outcome = Outcome::default();
            count_rejection(&entry.attempts, &mut outcome);
            assert_eq!(outcome.rejected, 1);
        }
        let mut entry = query_entry();
        entry.raw.push_str(" PMID:123456 doi:10.1000/a");
        assert!(resolve_explicit_biomedical(&mut entry, |entry, id| {
            select_biomedical_record(entry, id, [])
        }));
        assert!(entry.resolved.is_none());
        let mut outcome = Outcome::default();
        count_rejection(&entry.attempts, &mut outcome);
        assert_eq!(outcome.rejected, 0);
        let mut wrong = biomedical_record();
        wrong.title = "An unrelated clinical investigation".to_string();
        assert!(resolve_explicit_biomedical(&mut entry, |entry, id| {
            select_biomedical_record(entry, id, [wrong.clone()])
        }));
        count_rejection(&entry.attempts, &mut outcome);
        assert_eq!(outcome.rejected, 1);
        assert!(entry.resolved.is_none());
    }

    #[test]
    fn explicit_ids_accept_only_verified_joint_identity() {
        let mut entry = query_entry();
        assert!(!resolve_explicit_biomedical(&mut entry, |_, _| panic!(
            "no explicit IDs"
        )));
        entry.raw.push_str(" PMID:123456 PMCID:PMC7654321");
        assert!(resolve_explicit_biomedical(&mut entry, |entry, id| {
            select_biomedical_record(entry, id, [biomedical_record()])
        }));
        assert!(entry.resolved.is_some());
    }

    #[test]
    fn europepmc_mismatch_counts_even_after_not_found_or_transport_error() {
        for id in [
            pmc::Identifier::Pmid("123456".to_string()),
            pmc::Identifier::Doi("10.1000/a".to_string()),
        ] {
            let mut entry = query_entry();
            let mut wrong = biomedical_record();
            wrong.doi = Some("10.1000/a".to_string());
            wrong.title = "An unrelated clinical investigation".to_string();
            assert!(select_biomedical_record(&mut entry, &id, [wrong]).is_none());
            assert_eq!(entry.attempts.last().unwrap().outcome, "not_found");
            entry.attempts.push(Attempt {
                method: "query".to_string(),
                doi: None,
                outcome: "error".to_string(),
                detail: None,
            });
            let mut outcome = Outcome::default();
            count_rejection(&entry.attempts, &mut outcome);
            assert_eq!(outcome.rejected, 1);
            // A previous pass's mismatch must not contaminate a retry.
            count_rejection(&entry.attempts[entry.attempts.len()..], &mut outcome);
            assert_eq!(outcome.rejected, 1);
        }
    }

    #[test]
    fn old_resolved_json_remains_readable() {
        let old = r#"{"doi":"10.1000/a","title":"A title","authors":[],"year":2020,"venue":null,"source":"crossref","method":"printed","score":1.0}"#;
        let record: Resolved = serde_json::from_str(old).unwrap();
        assert_eq!(record.doi.as_deref(), Some("10.1000/a"));
        assert_eq!(record.pmid, None);
        assert_eq!(record.pmcid, None);
    }

    #[test]
    fn failed_registry_requests_are_visible_in_summary() {
        let resolver = Resolver {
            client: Client::new("test").with_offline(true),
        };
        let mut entries = [query_entry()];
        entries[0].raw.push_str(" PMID:123456 doi:10.1000/a");
        let outcome = resolver.resolve_entries(&mut entries);
        assert_eq!(outcome.errors, 1);
        assert_eq!(outcome.unresolved, 1);
        assert!(entries[0].attempts.iter().any(|a| a.method == "europepmc"));
        assert!(
            !entries[0]
                .attempts
                .iter()
                .any(|a| a.method == "printed" || a.method == "query")
        );
        assert_eq!(outcome.rejected, 0);
    }

    #[test]
    #[ignore = "requires live Europe PMC and Crossref access"]
    fn live_biomedical_exact_identifiers() {
        let raw = "Piwowar H, Priem J, Larivière V, et al. The state of OA: a large-scale analysis of the prevalence and impact of Open Access articles. PeerJ 2018.";
        for suffix in [
            "PMID:29456894",
            "PMCID:PMC5815332",
            "doi:10.7717/peerj.4375",
        ] {
            let mut entries = [ReferenceEntry {
                raw: format!("{raw} {suffix}"),
                ..ReferenceEntry::default()
            }];
            let outcome = Resolver::new(None).resolve_entries(&mut entries);
            assert_eq!(outcome.resolved, 1, "{:?}", entries[0].attempts);
            let record = entries[0].resolved.as_ref().unwrap();
            assert_eq!(record.doi.as_deref(), Some("10.7717/peerj.4375"));
            assert_eq!(record.pmid.as_deref(), Some("29456894"));
            assert_eq!(record.pmcid.as_deref(), Some("PMC5815332"));
        }
    }

    #[test]
    fn family_names_keep_particles() {
        assert_eq!(family_of("Adrian J. van der Kogel"), "der Kogel");
        assert_eq!(family_of("Jane Smith"), "Smith");
        assert!(author_agreement("van der Kogel, A.J.", "Adrian J. van der Kogel") >= 0.99);
        assert!(author_agreement("Obtułowicz K", "Krystyna Obtulowicz") >= 0.99);
        assert!(author_agreement("Smith, J.", "Jane Jones") < 0.8);
    }

    #[test]
    fn links_attach_to_the_entry_they_sit_on() {
        let mut page = PageText::new(3, 600.0, 800.0, 0);
        page.links.push(Link {
            bbox: Some(BBox {
                x0: 60.0,
                y0: 690.0,
                x1: 300.0,
                y1: 700.0,
            }),
            uri: "https://doi.org/10.1000/first".to_string(),
        });
        page.links.push(Link {
            bbox: Some(BBox {
                x0: 60.0,
                y0: 640.0,
                x1: 300.0,
                y1: 650.0,
            }),
            uri: "https://doi.org/10.1000/second".to_string(),
        });
        let entry = |index: u32, y0: f32| ReferenceEntry {
            index,
            page: 3,
            anchor: Some(BBox {
                x0: 50.0,
                y0,
                x1: 50.0,
                y1: y0 + 10.0,
            }),
            ..ReferenceEntry::default()
        };
        let mut entries = vec![entry(1, 710.0), entry(2, 660.0)];
        attach_links(&mut entries, &[page]);
        assert_eq!(entries[0].doi_link.as_deref(), Some("10.1000/first"));
        assert_eq!(entries[1].doi_link.as_deref(), Some("10.1000/second"));
    }

    #[test]
    fn verification_needs_author_and_year_agreement() {
        let record = PaperRecord {
            title: "A study of things".to_string(),
            authors: vec!["Jane Smith".to_string(), "Bob Jones".to_string()],
            year: Some(2020),
            venue: None,
            doi: Some("10.1000/x".to_string()),
            arxiv_id: None,
            pmid: None,
            pmcid: None,
            url: None,
            abstract_text: None,
            source: "crossref".to_string(),
            source_id: None,
        };
        let mut entry = ReferenceEntry {
            raw: "Smith, J., Jones, B. (2021). A study of things. J. Stuff 3, 1-9.".to_string(),
            ..ReferenceEntry::default()
        };
        assert!(verify(&record, &entry).is_ok());
        entry.raw = "Smith, J., Jones, B. (2015). A study of things. J. Stuff 3, 1-9.".to_string();
        assert!(verify(&record, &entry).unwrap_err().starts_with("year"));
        entry.raw = "Brown, T. (2020). A study of things. J. Stuff 3, 1-9.".to_string();
        assert!(
            verify(&record, &entry)
                .unwrap_err()
                .starts_with("first author")
        );
        entry.raw = "Smith, J. (2020). Something else entirely. J. Stuff 3, 1-9.".to_string();
        assert!(verify(&record, &entry).unwrap_err().starts_with("title"));
        entry.raw = "Ru Èhland K, Smith J. (2020). A study of things.".to_string();
        let record2 = PaperRecord {
            authors: vec!["K. M. Rühland".to_string()],
            ..record.clone()
        };
        assert!(verify(&record2, &entry).is_ok());
        entry.raw = "RowJR, Smith J. (2020). A study of things.".to_string();
        let record3 = PaperRecord {
            authors: vec!["Jeffrey R. Row".to_string()],
            ..record
        };
        assert!(verify(&record3, &entry).is_ok());
    }
}
