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

use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::time::Duration;

use regex::Regex;
use tpe_biblio::util::with_query;
use tpe_biblio::{BiblioError, Client, PaperRecord, crossref};
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
    RE.get_or_init(|| Regex::new(r"(?i)\b10\.\d{4,9}/[^\s\]\)>\x22\x27]+").expect("valid regex"))
}

/// The DOI in `text` (a URI or a printed string), lower-cased, without a
/// resolver prefix or trailing punctuation; `None` when there is none.
#[must_use]
pub fn doi_in(text: &str) -> Option<String> {
    let found = doi_re().find(text)?;
    let mut doi = found.as_str().to_ascii_lowercase();
    while doi.ends_with(['.', ',', ';', ':']) {
        doi.pop();
    }
    (doi.len() > 7).then_some(doi)
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
        doi: doi.to_string(),
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
    /// Requests that failed (network, rate limit); those entries are unresolved.
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

/// Does a word of the record's venue (3+ chars, `RNA`, `Lancet`) appear in
/// the entry? Journal names are how an article is told from its preprint or
/// poster when title, authors and year all agree.
fn venue_in_entry(venue: &str, raw_folded: &str) -> bool {
    let needles = words(&folded(venue), 3);
    if needles.is_empty() {
        return false;
    }
    let hay = words(raw_folded, 1);
    needles.iter().any(|n| hay.iter().any(|h| h == n))
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
        let doi = record.doi.clone().unwrap_or_else(|| doi.to_string());
        let outcome = match verify(&record, entry) {
            Ok(score) => {
                attempt.outcome = "verified".to_string();
                Some(resolved_from(&record, &doi, method, score))
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
        outcome: &mut Outcome,
    ) -> Result<Option<&'static str>, BiblioError> {
        let mut candidates: Vec<(String, &'static str)> = Vec::new();
        if let Some(doi) = entry.doi_link.as_deref().and_then(doi_in) {
            candidates.push((doi, "link"));
        }
        if let Some(doi) = entry.doi.as_deref().and_then(doi_in)
            && !candidates.iter().any(|(d, _)| *d == doi)
        {
            candidates.push((doi, "printed"));
        }
        let mut saw_record = false;
        for (doi, method) in &candidates {
            match self.try_doi(entry, doi, method)? {
                Some(resolved) => {
                    entry.resolved = Some(resolved);
                    return Ok(Some(method));
                }
                None => {
                    saw_record |= entry
                        .attempts
                        .last()
                        .is_some_and(|a| a.outcome == "mismatch");
                }
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
        // Several candidates can verify (a journal article and its preprint or
        // poster share title, authors and year): keep the best score, where a
        // venue named in the entry counts extra.
        let raw = folded(&entry.raw);
        let mut best: Option<(f32, Resolved)> = None;
        for candidate in found {
            let record = candidate.record;
            let Some(doi) = record.doi.clone() else {
                continue;
            };
            saw_record = true;
            match verify(&record, entry) {
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
                        outcome: "verified".to_string(),
                        detail: record
                            .venue
                            .as_ref()
                            .map(|v| format!("venue {v:?}, score {total:.2}")),
                    });
                    if best.as_ref().is_none_or(|(b, _)| total > *b) {
                        best = Some((total, resolved_from(&record, &doi, "query", score)));
                    }
                }
                Err(detail) => entry.attempts.push(Attempt {
                    method: "query".to_string(),
                    doi: Some(doi),
                    outcome: "mismatch".to_string(),
                    detail: Some(detail),
                }),
            }
        }
        if let Some((_, resolved)) = best {
            entry.resolved = Some(resolved);
            return Ok(Some("query"));
        }
        if saw_record {
            outcome.rejected += 1;
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
            if entry.resolved.is_some() {
                outcome.resolved += 1;
                continue;
            }
            match self.resolve_entry(entry, &mut outcome) {
                Ok(Some(method)) => {
                    outcome.resolved += 1;
                    *outcome.by_method.entry(method.to_string()).or_insert(0) += 1;
                }
                Ok(None) => outcome.unresolved += 1,
                Err(_) => {
                    outcome.errors += 1;
                    outcome.unresolved += 1;
                }
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
        {
            let record = found.record;
            let score = if title.is_empty() {
                1.0
            } else {
                title_agreement(title, &record.title)
            };
            if title.is_empty() || score >= PAPER_TITLE_MIN {
                let doi = record.doi.clone().unwrap_or(doi);
                return Some(resolved_from(&record, &doi, "metadata", score));
            }
        }
        if title.len() < 12 {
            return None;
        }
        let found = with_retry(|| bibliographic_search(&self.client, title, QUERY_ROWS)).ok()?;
        for candidate in found {
            let record = candidate.record;
            let Some(doi) = record.doi.clone() else {
                continue;
            };
            let score = title_agreement(title, &record.title);
            if score >= PAPER_TITLE_MIN {
                return Some(resolved_from(&record, &doi, "query", score));
            }
        }
        None
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
