//! Minimal, tolerant HTML scanning: opening tags with their attributes, the
//! visible text, and character-entity decoding. This is deliberately not a
//! parser; it is enough to read `<meta>` tags, `href` attributes and body text
//! from publisher landing pages.

/// An opening tag with its attributes. Names are lower-cased; attribute
/// values are entity-decoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tag {
    /// Lower-case element name (`meta`, `a`, `link`).
    pub name: String,
    /// Attributes in document order, names lower-cased.
    pub attrs: Vec<(String, String)>,
}

impl Tag {
    /// First value of attribute `name` (case-insensitive).
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Opening tags in document order. Comments, doctype/processing instructions,
/// closing tags and the bodies of `<script>` and `<style>` are skipped.
pub fn tags(html: &str) -> Vec<Tag> {
    let mut out = Vec::new();
    scan(html, &mut |event| {
        if let Event::Tag(tag) = event {
            out.push(tag);
        }
    });
    out
}

/// Visible text: tags removed, `<script>`/`<style>` bodies and comments
/// dropped, entities decoded, whitespace collapsed to single spaces.
pub fn strip_tags(html: &str) -> String {
    let mut pieces: Vec<String> = Vec::new();
    scan(html, &mut |event| {
        if let Event::Text(text) = event {
            pieces.push(decode_entities(text));
        }
    });
    let joined = pieces.join(" ");
    let mut out = String::with_capacity(joined.len());
    let mut last_space = true;
    for c in joined.chars() {
        if c.is_whitespace() {
            if !last_space {
                out.push(' ');
                last_space = true;
            }
        } else {
            out.push(c);
            last_space = false;
        }
    }
    out.truncate(out.trim_end().len());
    out
}

/// `content` values of `<meta>` tags whose `name` or `property` equals
/// `name` (case-insensitive), in document order.
pub fn meta_content(html: &str, name: &str) -> Vec<String> {
    tags(html)
        .into_iter()
        .filter(|tag| tag.name == "meta")
        .filter(|tag| {
            tag.attr("name")
                .or_else(|| tag.attr("property"))
                .is_some_and(|n| n.trim().eq_ignore_ascii_case(name))
        })
        .filter_map(|tag| tag.attr("content").map(|c| c.trim().to_string()))
        .filter(|c| !c.is_empty())
        .collect()
}

/// Values of attribute `attr` on every `<tag_name>` element, in document order.
pub fn attribute_values(html: &str, tag_name: &str, attr: &str) -> Vec<String> {
    tags(html)
        .into_iter()
        .filter(|tag| tag.name.eq_ignore_ascii_case(tag_name))
        .filter_map(|tag| tag.attr(attr).map(|v| v.trim().to_string()))
        .filter(|v| !v.is_empty())
        .collect()
}

/// Decode the named entities that matter for URLs and identifiers plus
/// numeric (`&#NNN;`, `&#xHH;`) references. Unknown entities are kept.
pub fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        let tail = &rest[pos..];
        match entity_at(tail) {
            Some((decoded, consumed)) => {
                out.push_str(&decoded);
                rest = &tail[consumed..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Decode the entity starting at `tail` (which begins with `&`); returns the
/// replacement and the byte length consumed.
fn entity_at(tail: &str) -> Option<(String, usize)> {
    let semi = tail.find(';')?;
    if semi < 2 || semi > 12 {
        return None;
    }
    let body = &tail[1..semi];
    let decoded = if let Some(hex) = body.strip_prefix("#x").or_else(|| body.strip_prefix("#X")) {
        let code = u32::from_str_radix(hex, 16).ok()?;
        char::from_u32(code)?.to_string()
    } else if let Some(dec) = body.strip_prefix('#') {
        let code: u32 = dec.parse().ok()?;
        char::from_u32(code)?.to_string()
    } else {
        match body {
            "amp" => "&",
            "lt" => "<",
            "gt" => ">",
            "quot" => "\"",
            "apos" => "'",
            "nbsp" => "\u{A0}",
            "ndash" => "\u{2013}",
            "mdash" => "\u{2014}",
            "hellip" => "\u{2026}",
            _ => return None,
        }
        .to_string()
    };
    Some((decoded, semi + 1))
}

enum Event<'a> {
    Tag(Tag),
    Text(&'a str),
}

/// Drive `sink` with tags and text runs. All delimiters are ASCII, so every
/// byte offset used to slice is a char boundary.
fn scan(html: &str, sink: &mut dyn FnMut(Event<'_>)) {
    let lower = html.to_ascii_lowercase();
    let bytes = html.as_bytes();
    let mut i = 0;
    let mut text_start = 0;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        let rest = &lower[i..];
        let (next, tag) = if rest.starts_with("<!--") {
            (rest.find("-->").map_or(bytes.len(), |e| i + e + 3), None)
        } else if rest.starts_with("<!") || rest.starts_with("<?") || rest.starts_with("</") {
            (rest.find('>').map_or(bytes.len(), |e| i + e + 1), None)
        } else if rest.as_bytes().get(1).is_some_and(u8::is_ascii_alphabetic) {
            let end = tag_end(rest).map_or(bytes.len(), |e| i + e);
            let tag = parse_tag(&html[i + 1..end]);
            let mut next = (end + 1).min(bytes.len());
            if tag.name == "script" || tag.name == "style" {
                let closer = format!("</{}", tag.name);
                next = lower[next..]
                    .find(&closer)
                    .map_or(bytes.len(), |e| next + e);
            }
            (next, Some(tag))
        } else {
            i += 1;
            continue;
        };
        if text_start < i {
            sink(Event::Text(&html[text_start..i]));
        }
        if let Some(tag) = tag {
            sink(Event::Tag(tag));
        }
        i = next;
        text_start = next;
    }
    if text_start < bytes.len() {
        sink(Event::Text(&html[text_start..]));
    }
}

/// Offset of the `>` closing the tag that starts at `s[0] == '<'`, honouring
/// quoted attribute values.
fn tag_end(s: &str) -> Option<usize> {
    let mut quote: Option<u8> = None;
    for (idx, &b) in s.as_bytes().iter().enumerate().skip(1) {
        match quote {
            Some(q) => {
                if b == q {
                    quote = None;
                }
            }
            None => match b {
                b'"' | b'\'' => quote = Some(b),
                b'>' => return Some(idx),
                _ => {}
            },
        }
    }
    None
}

/// Parse `name attr="v" attr2=v2 attr3` (the text between `<` and `>`).
fn parse_tag(body: &str) -> Tag {
    let body = body.trim_end_matches('/');
    let bytes = body.as_bytes();
    let name_end = bytes
        .iter()
        .position(|b| b.is_ascii_whitespace() || *b == b'/')
        .unwrap_or(bytes.len());
    let name = body[..name_end].to_ascii_lowercase();
    let mut attrs = Vec::new();
    let mut i = name_end;
    while i < bytes.len() {
        while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b'/') {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let key_start = i;
        while i < bytes.len()
            && !bytes[i].is_ascii_whitespace()
            && bytes[i] != b'='
            && bytes[i] != b'/'
        {
            i += 1;
        }
        let key = body[key_start..i].to_ascii_lowercase();
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let value = if bytes.get(i) == Some(&b'=') {
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            let (raw, next) = read_value(body, i);
            i = next;
            decode_entities(raw)
        } else {
            String::new()
        };
        if !key.is_empty() {
            attrs.push((key, value));
        }
    }
    Tag { name, attrs }
}

/// Read an attribute value starting at byte `i`; returns it and the offset
/// after it.
fn read_value(body: &str, i: usize) -> (&str, usize) {
    let bytes = body.as_bytes();
    match bytes.get(i) {
        Some(&q) if q == b'"' || q == b'\'' => {
            let start = i + 1;
            let end = bytes[start..]
                .iter()
                .position(|&b| b == q)
                .map_or(bytes.len(), |e| start + e);
            (&body[start..end], (end + 1).min(bytes.len()))
        }
        Some(_) => {
            let end = bytes[i..]
                .iter()
                .position(|b| b.is_ascii_whitespace())
                .map_or(bytes.len(), |e| i + e);
            (&body[i..end], end)
        }
        None => ("", i),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r#"<!DOCTYPE html><html><head>
<meta charset="utf-8">
<meta name="citation_doi" content="10.1038/s41586-020-2649-2">
<META NAME="citation_pdf_url" CONTENT='https://www.nature.com/articles/s41586-020-2649-2.pdf'>
<meta property="og:title" content="Array programming with &amp; NumPy">
<link rel="alternate" type="application/pdf" href="/articles/s41586-020-2649-2.pdf"/>
<script>var doi = "10.9999/should-not-appear"; </script>
<style>.x { content: "<b>" }</style>
</head><body><!-- 10.8888/comment -->
<h1>Array   programming</h1>
<p>DOI: <a href="https://doi.org/10.1038/s41586-020-2649-2" data-track>10.1038/s41586-020-2649-2</a>
and a &lt;tag&gt; &#38; &#x2F; &unknown; done</p>
<a href=/pdf/plain.pdf download>PDF</a>
</body></html>"#;

    #[test]
    fn tags_are_found_with_lowercase_names_and_attrs() {
        let all = tags(PAGE);
        let names: Vec<&str> = all.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "html", "head", "meta", "meta", "meta", "meta", "link", "script", "style", "body",
                "h1", "p", "a", "a"
            ]
        );
        let pdf_meta = &all[4];
        assert_eq!(pdf_meta.attr("NAME"), Some("citation_pdf_url"));
        assert_eq!(
            pdf_meta.attr("content"),
            Some("https://www.nature.com/articles/s41586-020-2649-2.pdf")
        );
    }

    #[test]
    fn unquoted_and_valueless_attributes() {
        let a = tags("<a href=/pdf/plain.pdf download>x</a>");
        assert_eq!(a[0].attr("href"), Some("/pdf/plain.pdf"));
        assert_eq!(a[0].attr("download"), Some(""));
        assert_eq!(a[0].attr("missing"), None);
        let img = tags("<img src='a.png'/>");
        assert_eq!(img[0].name, "img");
        assert_eq!(img[0].attr("src"), Some("a.png"));
        let quoted_gt = tags(r#"<a title="x > y" href="z">"#);
        assert_eq!(quoted_gt[0].attr("href"), Some("z"));
    }

    #[test]
    fn meta_content_matches_name_or_property_case_insensitively() {
        assert_eq!(
            meta_content(PAGE, "CITATION_DOI"),
            ["10.1038/s41586-020-2649-2"]
        );
        assert_eq!(
            meta_content(PAGE, "og:title"),
            ["Array programming with & NumPy"]
        );
        assert!(meta_content(PAGE, "citation_title").is_empty());
    }

    #[test]
    fn attribute_values_collects_hrefs() {
        assert_eq!(
            attribute_values(PAGE, "a", "href"),
            [
                "https://doi.org/10.1038/s41586-020-2649-2",
                "/pdf/plain.pdf"
            ]
        );
        assert_eq!(
            attribute_values(PAGE, "link", "href"),
            ["/articles/s41586-020-2649-2.pdf"]
        );
    }

    #[test]
    fn strip_tags_drops_script_style_comments_and_decodes() {
        let text = strip_tags(PAGE);
        assert!(text.contains("Array programming DOI: 10.1038/s41586-020-2649-2"));
        assert!(text.contains("and a <tag> & / &unknown; done PDF"));
        assert!(!text.contains("should-not-appear"));
        assert!(!text.contains("comment"));
        assert!(!text.contains("content:"));
        assert!(!text.ends_with(' '));
    }

    #[test]
    fn entities_decode() {
        assert_eq!(
            decode_entities("a &amp; b &lt;c&gt; &quot;d&quot; &#39;e&#39;"),
            "a & b <c> \"d\" 'e'"
        );
        assert_eq!(decode_entities("&#x41;&#66;&nbsp;"), "AB\u{A0}");
        assert_eq!(decode_entities("&bogus; & plain &"), "&bogus; & plain &");
        assert_eq!(decode_entities("no entities"), "no entities");
        assert_eq!(decode_entities("&#xZZ;"), "&#xZZ;");
    }

    #[test]
    fn unterminated_constructs_do_not_panic() {
        let open = tags("<a href='x");
        assert_eq!(open[0].name, "a");
        assert_eq!(open[0].attr("href"), Some("x"));
        assert_eq!(strip_tags("text <!-- never closed"), "text");
        assert_eq!(strip_tags("<script>never closed"), "");
        assert_eq!(strip_tags("a < b and c > d"), "a < b and c > d");
        assert_eq!(strip_tags("<"), "<");
    }
}
