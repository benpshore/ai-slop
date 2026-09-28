//! DOI and arXiv identifier detection in plain text, URLs and HTML.
//!
//! Detection is syntactic (`10.<4-9 digits>/<suffix>`); nothing here resolves
//! or validates an identifier against a registry. Results are lower-cased,
//! deduplicated and kept in order of first appearance. `tpe_common::normalize_doi`
//! is used as the validator of every candidate; the final trimming is done here
//! because `normalize_doi` also cuts a trailing `)` that belongs to DOIs such as
//! `10.1000/a(b)`, whereas this module only removes unbalanced brackets.
//! arXiv identifiers go through `tpe_common::normalize_arxiv_id`.

use tpe_common::{normalize_arxiv_id, normalize_doi};

use crate::html::{attribute_values, meta_content, strip_tags};
use crate::url::{NormalizedUrl, host_in_domain, percent_decode};

/// Hosts whose whole purpose is to resolve a DOI (or a handle) to a landing page.
pub const DOI_RESOLVERS: &[&str] = &["doi.org", "hdl.handle.net"];

/// `<meta>` names publishers use to declare the page's own DOI, most specific first.
pub const DOI_META_NAMES: &[&str] = &[
    "citation_doi",
    "dc.identifier.doi",
    "prism.doi",
    "bepress_citation_doi",
    "dc.identifier",
    "dcterms.identifier",
];

/// Path segments that follow a DOI in publisher URLs but are not part of it
/// (`/article/10.1007/s00134-020-06022-5/fulltext.html`).
const URL_TAIL_WORDS: &[&str] = &[
    "abstract",
    "abs",
    "full",
    "fulltext",
    "fulltext.html",
    "pdf",
    "epdf",
    "pdfdirect",
    "meta",
    "metrics",
    "references",
    "citedby",
    "figures",
    "tables",
    "supplementary",
    "supplemental",
    "download",
    "html",
];

/// `true` for `doi.org`, `dx.doi.org`, `hdl.handle.net` and their subdomains.
pub fn is_doi_resolver(host: &str) -> bool {
    DOI_RESOLVERS.iter().any(|d| host_in_domain(host, d))
}

/// Every DOI in free text, normalised, deduplicated, in order of appearance.
///
/// A candidate must be preceded by a non-identifier character (so `210.1234/x`
/// is not a DOI), ends at whitespace or one of `"<>`, and loses trailing
/// sentence punctuation and unbalanced closing brackets.
pub fn scan_dois(text: &str) -> Vec<String> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut found: Vec<String> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if let Some((next, candidate)) = doi_at(text, &chars, i) {
            if normalize_doi(candidate).is_some() {
                let doi = candidate.to_ascii_lowercase();
                if !found.contains(&doi) {
                    found.push(doi);
                }
            }
            i = next;
        } else {
            i += 1;
        }
    }
    found
}

/// DOIs embedded in a URL's path or query, percent-decoded first. Publisher
/// tail segments (`/fulltext.html`, `/pdf`) and a `.pdf` extension are cut
/// off, because DOIs never end that way while publisher URLs always do.
pub fn dois_in_url(url: &NormalizedUrl) -> Vec<String> {
    let mut text = percent_decode(&url.path);
    for (k, v) in &url.query {
        text.push(' ');
        text.push_str(&percent_decode(k));
        text.push(' ');
        text.push_str(&percent_decode(v));
    }
    let mut out: Vec<String> = Vec::new();
    for doi in scan_dois(&text) {
        let trimmed = trim_url_tail(&doi);
        if !out.contains(&trimmed) {
            out.push(trimmed);
        }
    }
    out
}

/// DOIs a page declares about itself in `<meta>` tags (see [`DOI_META_NAMES`]),
/// most trustworthy names first.
pub fn declared_dois_in_html(html: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for name in DOI_META_NAMES {
        for content in meta_content(html, name) {
            for doi in scan_dois(&content) {
                if !out.contains(&doi) {
                    out.push(doi);
                }
            }
        }
    }
    out
}

/// All DOIs on a page: declared ones first, then those in `<a href>` targets,
/// then those mentioned in the visible text.
pub fn dois_in_html(html: &str) -> Vec<String> {
    let mut out = declared_dois_in_html(html);
    let mut push_all = |dois: Vec<String>| {
        for doi in dois {
            if !out.contains(&doi) {
                out.push(doi);
            }
        }
    };
    for href in attribute_values(html, "a", "href") {
        if let Ok(url) = NormalizedUrl::parse(&href) {
            push_all(dois_in_url(&url));
        } else {
            push_all(scan_dois(&href));
        }
    }
    push_all(scan_dois(&strip_tags(html)));
    out
}

/// The arXiv identifier addressed by an `arxiv.org` URL (`/abs/`, `/pdf/`,
/// `/html/`, `/format/`, `/ps/`), without version suffix.
pub fn arxiv_id_in_url(url: &NormalizedUrl) -> Option<String> {
    if !host_in_domain(&url.host, "arxiv.org") {
        return None;
    }
    let segments = url.path_segments();
    let kind = *segments.first()?;
    if !matches!(kind, "abs" | "pdf" | "html" | "format" | "ps") {
        return None;
    }
    let rest: Vec<String> = segments
        .get(1..)?
        .iter()
        .map(|s| percent_decode(s))
        .collect();
    if rest.is_empty() {
        return None;
    }
    let joined = rest.join("/");
    let without_ext = strip_ci_suffix(&joined, ".pdf");
    normalize_arxiv_id(without_ext)
}

/// arXiv identifiers written as `arXiv:2502.00857v2` in text (case-insensitive
/// prefix), normalised and deduplicated. Bare identifiers are not collected
/// because a date such as `2024.12345` is indistinguishable from one.
pub fn arxiv_ids_in_text(text: &str) -> Vec<String> {
    let lower = text.to_ascii_lowercase();
    let mut out: Vec<String> = Vec::new();
    for (pos, _) in lower.match_indices("arxiv:") {
        let start = pos + "arxiv:".len();
        let tail = &text[start..];
        let end = tail
            .find(|c: char| {
                c.is_whitespace() || matches!(c, '"' | '<' | '>' | ')' | ']' | ',' | ';')
            })
            .unwrap_or(tail.len());
        let token = tail[..end].trim_end_matches('.');
        if let Some(id) = normalize_arxiv_id(token)
            && !out.contains(&id)
        {
            out.push(id);
        }
    }
    out
}

fn strip_ci_suffix<'a>(s: &'a str, suffix: &str) -> &'a str {
    let n = suffix.len();
    if s.len() >= n
        && s.is_char_boundary(s.len() - n)
        && s[s.len() - n..].eq_ignore_ascii_case(suffix)
    {
        &s[..s.len() - n]
    } else {
        s
    }
}

fn trim_url_tail(doi: &str) -> String {
    let Some((prefix, suffix)) = doi.split_once('/') else {
        return doi.to_string();
    };
    let mut parts: Vec<&str> = suffix.split('/').collect();
    while parts.len() > 1 {
        let Some(&last) = parts.last() else {
            break;
        };
        let is_tail = URL_TAIL_WORDS.contains(&last)
            || strip_ci_suffix(last, ".html").len() != last.len()
            || strip_ci_suffix(last, ".pdf").len() != last.len();
        if is_tail {
            parts.pop();
        } else {
            break;
        }
    }
    let joined = parts.join("/");
    let rebuilt = strip_ci_suffix(&joined, ".pdf");
    format!("{prefix}/{rebuilt}")
}

fn char_at(chars: &[(usize, char)], i: usize) -> Option<char> {
    chars.get(i).map(|(_, c)| *c)
}

/// When a DOI starts at `chars[i]`, return the index just past it and the
/// trimmed candidate text.
fn doi_at<'a>(text: &'a str, chars: &[(usize, char)], i: usize) -> Option<(usize, &'a str)> {
    if i > 0 && char_at(chars, i - 1).is_some_and(|c| c.is_alphanumeric() || c == '.') {
        return None;
    }
    if char_at(chars, i) != Some('1')
        || char_at(chars, i + 1) != Some('0')
        || char_at(chars, i + 2) != Some('.')
    {
        return None;
    }
    let mut j = i + 3;
    while char_at(chars, j).is_some_and(|c| c.is_ascii_digit()) {
        j += 1;
    }
    let digits = j - (i + 3);
    if !(4..=9).contains(&digits) || char_at(chars, j) != Some('/') {
        return None;
    }
    j += 1;
    let suffix_start = j;
    while char_at(chars, j).is_some_and(|c| !c.is_whitespace() && !matches!(c, '"' | '<' | '>')) {
        j += 1;
    }
    if j == suffix_start {
        return None;
    }
    let start = chars.get(i)?.0;
    let end = chars.get(j).map_or(text.len(), |(offset, _)| *offset);
    let candidate = trim_candidate(&text[start..end]);
    if candidate.split_once('/').is_none_or(|(_, s)| s.is_empty()) {
        return None;
    }
    Some((j, candidate))
}

fn trim_candidate(mut s: &str) -> &str {
    loop {
        let before = s.len();
        s = s.trim_end_matches(['.', ',', ';', ':', '\'', '"', '!', '?']);
        for (open, close) in [('(', ')'), ('[', ']'), ('{', '}')] {
            if s.ends_with(close) && count(s, open) < count(s, close) {
                s = &s[..s.len() - close.len_utf8()];
            }
        }
        if s.len() == before {
            return s;
        }
    }
}

fn count(s: &str, c: char) -> usize {
    s.chars().filter(|x| *x == c).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> NormalizedUrl {
        NormalizedUrl::parse(s).unwrap()
    }

    #[test]
    fn scan_finds_and_normalises() {
        let text = "See https://doi.org/10.1038/S41586-020-2649-2. Also doi:10.1000/xyz, and \
                    10.1016/S0140-6736(20)30183-5 (Lancet). Not 210.1234/x nor 10.12/short.";
        assert_eq!(
            scan_dois(text),
            [
                "10.1038/s41586-020-2649-2",
                "10.1000/xyz",
                "10.1016/s0140-6736(20)30183-5"
            ]
        );
    }

    #[test]
    fn scan_trims_unbalanced_brackets_and_quotes() {
        assert_eq!(scan_dois("(10.1000/abc)"), ["10.1000/abc"]);
        assert_eq!(scan_dois("[10.1000/abc]."), ["10.1000/abc"]);
        assert_eq!(scan_dois("\"10.1000/abc\""), ["10.1000/abc"]);
        assert_eq!(scan_dois("<a>10.1000/abc</a>"), ["10.1000/abc"]);
        assert_eq!(scan_dois("10.1000/a(b)"), ["10.1000/a(b)"]);
    }

    #[test]
    fn scan_rejects_non_dois_and_dedupes() {
        assert!(scan_dois("10.1000/").is_empty());
        assert!(scan_dois("10.100/abc").is_empty());
        assert!(scan_dois("10.1234567890/abc").is_empty());
        assert!(scan_dois("version 10.15 of 10.1000").is_empty());
        assert_eq!(scan_dois("10.1000/a 10.1000/A"), ["10.1000/a"]);
        assert!(scan_dois("").is_empty());
    }

    #[test]
    fn dois_in_resolver_and_publisher_urls() {
        assert_eq!(
            dois_in_url(&url("https://doi.org/10.1038/s41586-020-2649-2")),
            ["10.1038/s41586-020-2649-2"]
        );
        assert_eq!(
            dois_in_url(&url("https://dx.doi.org/10.1000%2Fabc%28d%29")),
            ["10.1000/abc(d)"]
        );
        assert_eq!(
            dois_in_url(&url(
                "https://link.springer.com/article/10.1007/s00134-020-06022-5/fulltext.html"
            )),
            ["10.1007/s00134-020-06022-5"]
        );
        assert_eq!(
            dois_in_url(&url(
                "https://link.springer.com/content/pdf/10.1007/s00134-020-06022-5.pdf"
            )),
            ["10.1007/s00134-020-06022-5"]
        );
        assert_eq!(
            dois_in_url(&url(
                "https://onlinelibrary.wiley.com/doi/pdf/10.1002/anie.201900001"
            )),
            ["10.1002/anie.201900001"]
        );
        assert_eq!(
            dois_in_url(&url(
                "https://www.tandfonline.com/doi/full/10.1080/01621459.2020.1000000"
            )),
            ["10.1080/01621459.2020.1000000"]
        );
        assert_eq!(
            dois_in_url(&url(
                "https://resolver.lib.edu/openurl?rft_id=info:doi/10.1000/q&x=1"
            )),
            ["10.1000/q"]
        );
        assert!(dois_in_url(&url("https://www.nature.com/articles/s41586-020-2649-2")).is_empty());
    }

    #[test]
    fn declared_and_mentioned_dois_in_html() {
        let html = r#"<html><head>
            <meta name="dc.identifier" content="doi:10.1000/declared-second">
            <meta name="citation_doi" content="10.1000/declared-first">
            </head><body>
            <a href="https://doi.org/10.1000/linked">ref</a>
            <p>See also 10.1000/mentioned and 10.1000/declared-first again.</p>
            </body></html>"#;
        assert_eq!(
            declared_dois_in_html(html),
            ["10.1000/declared-first", "10.1000/declared-second"]
        );
        assert_eq!(
            dois_in_html(html),
            [
                "10.1000/declared-first",
                "10.1000/declared-second",
                "10.1000/linked",
                "10.1000/mentioned"
            ]
        );
        assert!(dois_in_html("<p>nothing here</p>").is_empty());
    }

    #[test]
    fn arxiv_ids_in_urls() {
        assert_eq!(
            arxiv_id_in_url(&url("https://arxiv.org/abs/2502.00857v2")),
            Some("2502.00857".to_string())
        );
        assert_eq!(
            arxiv_id_in_url(&url("https://arxiv.org/pdf/2502.00857.pdf")),
            Some("2502.00857".to_string())
        );
        assert_eq!(
            arxiv_id_in_url(&url("http://export.arxiv.org/abs/hep-th/9901001")),
            Some("hep-th/9901001".to_string())
        );
        assert_eq!(
            arxiv_id_in_url(&url("https://arxiv.org/pdf/2502.00857")),
            Some("2502.00857".to_string())
        );
        assert_eq!(
            arxiv_id_in_url(&url("https://arxiv.org/list/cs.CL/recent")),
            None
        );
        assert_eq!(arxiv_id_in_url(&url("https://arxiv.org/abs/")), None);
        assert_eq!(
            arxiv_id_in_url(&url("https://example.org/abs/2502.00857")),
            None
        );
    }

    #[test]
    fn arxiv_ids_in_text_need_prefix() {
        assert_eq!(
            arxiv_ids_in_text("arXiv:2502.00857v2, ARXIV:hep-th/9901001. and 2502.00857"),
            ["2502.00857", "hep-th/9901001"]
        );
        assert!(arxiv_ids_in_text("arXiv: preprint").is_empty());
    }

    #[test]
    fn resolver_hosts() {
        assert!(is_doi_resolver("doi.org"));
        assert!(is_doi_resolver("dx.doi.org"));
        assert!(is_doi_resolver("hdl.handle.net"));
        assert!(!is_doi_resolver("doi.org.example.com"));
        assert!(!is_doi_resolver("nature.com"));
    }
}
