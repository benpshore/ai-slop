//! Parsing of the response headers the Zotero API uses for paging and
//! versioning (`Link`, `Total-Results`, `Last-Modified-Version`, `Backoff`).

/// One entry of an HTTP `Link` header: the target URL and its `rel` values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkEntry {
    /// Target URL (the text between `<` and `>`).
    pub url: String,
    /// Lower-cased relation types, e.g. `next`, `last`, `alternate`.
    pub rels: Vec<String>,
}

/// Parse a `Link` header value as the Zotero API sends it:
/// `<url>; rel="next", <url>; rel="last"`. Commas inside `<...>` (for
/// example in `itemKey=A,B`) stay part of the URL.
pub fn parse_link_header(value: &str) -> Vec<LinkEntry> {
    let mut out = Vec::new();
    let mut rest = value;
    while let Some(open) = rest.find('<') {
        let after_open = &rest[open + 1..];
        let Some(close) = after_open.find('>') else {
            break;
        };
        let url = after_open[..close].trim().to_string();
        let tail = &after_open[close + 1..];
        let params_end = tail.find('<').unwrap_or(tail.len());
        out.push(LinkEntry {
            url,
            rels: parse_rels(&tail[..params_end]),
        });
        rest = &tail[params_end..];
    }
    out
}

/// Extract the `rel` values from the parameter part of one link entry.
fn parse_rels(params: &str) -> Vec<String> {
    let mut rels = Vec::new();
    for param in params.split(';') {
        let param = param.trim().trim_end_matches(',').trim();
        let Some((name, value)) = param.split_once('=') else {
            continue;
        };
        if !name.trim().eq_ignore_ascii_case("rel") {
            continue;
        }
        for rel in value.trim().trim_matches('"').split_whitespace() {
            rels.push(rel.to_ascii_lowercase());
        }
    }
    rels
}

/// The URL of the `rel="next"` link, if the header has one.
pub fn link_next(value: &str) -> Option<String> {
    parse_link_header(value)
        .into_iter()
        .find(|entry| entry.rels.iter().any(|rel| rel == "next"))
        .map(|entry| entry.url)
}

/// Parse a non-negative integer header such as `Total-Results`,
/// `Last-Modified-Version`, `Backoff` or `Retry-After`.
pub fn parse_u64(value: Option<&str>) -> Option<u64> {
    value.and_then(|v| v.trim().parse::<u64>().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    // The example from the saved "Link Header" section of the Web API basics page.
    const DOC_EXAMPLE: &str = "<https://api.zotero.org/users/12345/items?limit=30&start=30>; rel=\"next\",\n <https://api.zotero.org/users/12345/items?limit=30&start=5040>; rel=\"last\",\n <https://www.zotero.org/users/12345/items>; rel=\"alternate\"";

    #[test]
    fn link_header_from_docs_parses_all_entries() {
        let links = parse_link_header(DOC_EXAMPLE);
        assert_eq!(links.len(), 3);
        assert_eq!(links[1].rels, vec!["last".to_string()]);
        assert_eq!(links[2].url, "https://www.zotero.org/users/12345/items");
    }

    #[test]
    fn link_next_is_found() {
        assert_eq!(
            link_next(DOC_EXAMPLE).as_deref(),
            Some("https://api.zotero.org/users/12345/items?limit=30&start=30")
        );
    }

    #[test]
    fn last_page_has_no_next() {
        let value = "<https://api.zotero.org/users/1/items?start=0>; rel=\"first\", \
                     <https://api.zotero.org/users/1/items?start=25>; rel=\"prev\"";
        assert_eq!(link_next(value), None);
        assert_eq!(link_next(""), None);
    }

    #[test]
    fn commas_inside_url_and_unquoted_rel() {
        let value =
            "<https://api.zotero.org/users/1/items?itemKey=AAAA2222,BBBB3333&start=2>; rel=next";
        assert_eq!(
            link_next(value).as_deref(),
            Some("https://api.zotero.org/users/1/items?itemKey=AAAA2222,BBBB3333&start=2")
        );
    }

    #[test]
    fn numeric_headers() {
        assert_eq!(parse_u64(Some(" 5040 ")), Some(5040));
        assert_eq!(parse_u64(Some("abc")), None);
        assert_eq!(parse_u64(None), None);
    }
}
