//! Merge records describing the same paper, reported by several sources.
//!
//! Two records are the same paper when they share a DOI, an `arXiv` id, or a
//! normalised title together with the same known year (records without a year
//! never match on title alone). Matching is transitive (a
//! record with both a DOI and an `arXiv` id links a DOI-only record to an
//! `arXiv`-only one). Within a group the record from the highest-precedence
//! source wins each field; empty fields are filled from the others.
//! Precedence: `crossref` > `openalex` > `semantic_scholar` > `pmc` / `europepmc` > other.

use std::collections::HashMap;
use std::collections::hash_map::Entry;

use tpe_common::{PaperRecord, normalize_arxiv_id, normalize_doi};

/// Precedence rank of a source name (lower wins).
pub fn source_rank(source: &str) -> u8 {
    match source {
        "crossref" => 0,
        "openalex" => 1,
        "semantic_scholar" | "s2" => 2,
        "pmc" | "europepmc" => 3,
        _ => 4,
    }
}

/// Lower-case, keep letters and digits, collapse everything else to single spaces.
pub fn normalize_title(title: &str) -> String {
    let mapped: String = title
        .chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_lowercase().next().unwrap_or(c)
            } else {
                ' '
            }
        })
        .collect();
    mapped.split_whitespace().collect::<Vec<&str>>().join(" ")
}

fn match_keys(rec: &PaperRecord) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    if let Some(doi) = rec.doi.as_deref().and_then(normalize_doi) {
        keys.push(format!("doi:{doi}"));
    }
    if let Some(id) = rec.arxiv_id.as_deref().and_then(normalize_arxiv_id) {
        keys.push(format!("arxiv:{id}"));
    }
    // Title keys need a concrete year: two year-less records with the same title
    // (editorials, "Correction", "Reply to ...") are not evidence of one paper.
    if let Some(year) = rec.year {
        let title = normalize_title(&rec.title);
        if !title.is_empty() {
            keys.push(format!("title:{title}|{year}"));
        }
    }
    keys
}

fn find_root(parent: &mut [usize], mut i: usize) -> usize {
    while parent[i] != i {
        parent[i] = parent[parent[i]];
        i = parent[i];
    }
    i
}

fn union(parent: &mut [usize], a: usize, b: usize) {
    let ra = find_root(parent, a);
    let rb = find_root(parent, b);
    if ra != rb {
        // Keep the earliest index as the root so output order is first appearance.
        let (lo, hi) = if ra < rb { (ra, rb) } else { (rb, ra) };
        parent[hi] = lo;
    }
}

fn fill<T>(target: &mut Option<T>, other: Option<T>) {
    if target.is_none() {
        *target = other;
    }
}

fn merge_group(mut members: Vec<PaperRecord>) -> PaperRecord {
    members.sort_by_key(|r| source_rank(&r.source));
    let mut iter = members.into_iter();
    let mut best = iter.next().unwrap_or_default();
    for other in iter {
        if best.title.trim().is_empty() {
            best.title = other.title;
        }
        if best.authors.is_empty() {
            best.authors = other.authors;
        }
        fill(&mut best.year, other.year);
        fill(&mut best.venue, other.venue);
        fill(&mut best.doi, normalized_doi(other.doi.as_deref()));
        fill(
            &mut best.arxiv_id,
            normalized_arxiv(other.arxiv_id.as_deref()),
        );
        fill(&mut best.pmid, other.pmid);
        fill(&mut best.pmcid, other.pmcid);
        fill(&mut best.url, other.url);
        fill(&mut best.abstract_text, other.abstract_text);
    }
    best.doi = normalized_doi(best.doi.as_deref());
    best.arxiv_id = normalized_arxiv(best.arxiv_id.as_deref());
    best
}

/// The normalised DOI, or the raw value when it cannot be normalised.
fn normalized_doi(raw: Option<&str>) -> Option<String> {
    raw.map(|d| normalize_doi(d).unwrap_or_else(|| d.to_string()))
}

/// The normalised `arXiv` id, or the raw value when it cannot be normalised.
fn normalized_arxiv(raw: Option<&str>) -> Option<String> {
    raw.map(|a| normalize_arxiv_id(a).unwrap_or_else(|| a.to_string()))
}

/// Merge records that describe the same paper. Output keeps the order in
/// which each paper first appears; `source` and `source_id` are the winner's.
pub fn merge_records(records: Vec<PaperRecord>) -> Vec<PaperRecord> {
    let mut parent: Vec<usize> = (0..records.len()).collect();
    let mut first_with_key: HashMap<String, usize> = HashMap::new();
    for (i, rec) in records.iter().enumerate() {
        for key in match_keys(rec) {
            match first_with_key.entry(key) {
                Entry::Occupied(e) => union(&mut parent, i, *e.get()),
                Entry::Vacant(e) => {
                    e.insert(i);
                }
            }
        }
    }
    let mut groups: Vec<(usize, Vec<PaperRecord>)> = Vec::new();
    for (i, rec) in records.into_iter().enumerate() {
        let root = find_root(&mut parent, i);
        match groups.iter_mut().find(|(r, _)| *r == root) {
            Some((_, members)) => members.push(rec),
            None => groups.push((root, vec![rec])),
        }
    }
    groups.sort_by_key(|(root, _)| *root);
    groups
        .into_iter()
        .map(|(_, members)| merge_group(members))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(source: &str, title: &str) -> PaperRecord {
        PaperRecord {
            title: title.to_string(),
            source: source.to_string(),
            ..PaperRecord::default()
        }
    }

    #[test]
    fn crossref_wins_even_when_listed_last() {
        let mut oa = rec("openalex", "Deep Learning (OpenAlex title)");
        oa.doi = Some("10.1038/nature14539".to_string());
        oa.venue = Some("Nature (OpenAlex)".to_string());
        oa.abstract_text = Some("Abstract from OpenAlex.".to_string());
        oa.pmid = Some("26017442".to_string());
        let mut cr = rec("crossref", "Deep learning");
        cr.doi = Some("10.1038/NATURE14539".to_string());
        cr.venue = Some("Nature".to_string());
        cr.year = Some(2015);
        let merged = merge_records(vec![oa, cr]);
        assert_eq!(merged.len(), 1);
        let m = &merged[0];
        assert_eq!(m.source, "crossref");
        assert_eq!(m.title, "Deep learning");
        assert_eq!(m.venue.as_deref(), Some("Nature"));
        assert_eq!(m.year, Some(2015));
        assert_eq!(m.abstract_text.as_deref(), Some("Abstract from OpenAlex."));
        assert_eq!(m.pmid.as_deref(), Some("26017442"));
    }

    #[test]
    fn transitive_bridge_merges_three() {
        let mut a = rec("semantic_scholar", "Attention is all you need");
        a.doi = Some("10.48550/arxiv.1706.03762".to_string());
        a.arxiv_id = Some("1706.03762".to_string());
        let mut b = rec("pmc", "Totally different title text");
        b.arxiv_id = Some("arXiv:1706.03762v5".to_string());
        b.pmcid = Some("PMC1".to_string());
        let mut c = rec("openalex", "Attention Is All You Need!");
        c.doi = Some("https://doi.org/10.48550/ARXIV.1706.03762".to_string());
        // Order: b (arXiv only) first, then c (DOI only), then a (bridge).
        let merged = merge_records(vec![b, c, a]);
        assert_eq!(merged.len(), 1);
        let m = &merged[0];
        assert_eq!(m.source, "openalex");
        assert_eq!(m.pmcid.as_deref(), Some("PMC1"));
        assert_eq!(m.arxiv_id.as_deref(), Some("1706.03762"));
        assert_eq!(m.doi.as_deref(), Some("10.48550/arxiv.1706.03762"));
    }

    #[test]
    fn title_and_year_match_but_year_must_agree() {
        let mut a = rec("pmc", "The State of OA: an analysis");
        a.year = Some(2018);
        let mut b = rec("semantic_scholar", "the state of oa  an analysis");
        b.year = Some(2018);
        b.authors = vec!["H Piwowar".to_string()];
        let mut c = rec("crossref", "The state of OA: an analysis");
        c.year = Some(2019);
        let merged = merge_records(vec![a, b, c]);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].source, "semantic_scholar");
        assert_eq!(merged[0].authors, vec!["H Piwowar"]);
        assert_eq!(merged[1].source, "crossref");
    }

    #[test]
    fn same_title_without_years_stays_apart() {
        let a = rec("crossref", "Editorial");
        let b = rec("openalex", "editorial");
        let merged = merge_records(vec![a, b]);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].source, "crossref");
        assert_eq!(merged[1].source, "openalex");

        // One year known, the other missing: still not the same paper by title.
        let mut c = rec("crossref", "Editorial");
        c.year = Some(2020);
        let d = rec("openalex", "Editorial");
        assert_eq!(merge_records(vec![c, d]).len(), 2);
    }

    #[test]
    fn distinct_papers_stay_apart_in_order() {
        let merged = merge_records(vec![rec("crossref", "B paper"), rec("openalex", "A paper")]);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].title, "B paper");
        assert_eq!(merged[1].title, "A paper");
        assert!(merge_records(Vec::new()).is_empty());
    }

    #[test]
    fn normalizes_titles() {
        assert_eq!(
            normalize_title("  Deep—Learning: A Review! "),
            "deep learning a review"
        );
        assert_eq!(source_rank("crossref"), 0);
        assert_eq!(source_rank("europepmc"), source_rank("pmc"));
    }
}
