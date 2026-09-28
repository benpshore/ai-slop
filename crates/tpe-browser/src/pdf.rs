//! Classify links and responses as PDF or not, from the URL alone and from
//! response hints (`Content-Type`, `Content-Disposition`). The verdict is a
//! confidence level; only the engine's `%PDF-` magic check
//! ([`looks_like_pdf_bytes`]) is final.

use crate::html::{meta_content, tags};
use crate::url::{NormalizedUrl, host_in_domain, percent_decode};

/// Media types that mean "this is a PDF".
pub const PDF_MIME_TYPES: &[&str] = &[
    "application/pdf",
    "application/x-pdf",
    "application/acrobat",
    "applications/vnd.pdf",
    "text/pdf",
    "text/x-pdf",
];

/// Media types that say nothing about the content; the filename or URL decides.
pub const OPAQUE_MIME_TYPES: &[&str] = &[
    "application/octet-stream",
    "binary/octet-stream",
    "application/force-download",
    "application/download",
    "application/x-download",
];

/// Path segments publishers use for PDF delivery.
const PDF_SEGMENTS: &[&str] = &[
    "pdf",
    "pdfdirect",
    "pdfft",
    "fullpdf",
    "downloadpdf",
    "pdf-download",
    "getpdf",
    "epdf",
];

/// Path segments that mark HTML landing pages.
const LANDING_SEGMENTS: &[&str] = &[
    "abs",
    "abstract",
    "full",
    "fulltext",
    "landing",
    "meta",
    "citedby",
    "references",
    "figures",
    "metrics",
];

/// How sure we are that a link or response is a PDF.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum PdfVerdict {
    /// Nothing suggests a PDF, or the response says it is HTML.
    No,
    /// Weak signals only (a `download` parameter, a viewer frame).
    Possible,
    /// Strong URL or filename signals; the body has not been seen.
    Likely,
    /// The response declared a PDF media type (or an opaque type plus a `.pdf` filename).
    Confirmed,
}

impl PdfVerdict {
    /// Short label for logs and the UI.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::No => "no",
            Self::Possible => "possible",
            Self::Likely => "likely",
            Self::Confirmed => "confirmed",
        }
    }
}

/// Verdict plus the evidence behind it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkClassification {
    /// Confidence level.
    pub verdict: PdfVerdict,
    /// Filename from `Content-Disposition` or the last path segment, when it ends in `.pdf`.
    pub filename: Option<String>,
    /// Human-readable reasons, in the order they were found.
    pub reasons: Vec<&'static str>,
}

/// Response headers relevant to classification.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResponseHints<'a> {
    /// Raw `Content-Type` header value.
    pub content_type: Option<&'a str>,
    /// Raw `Content-Disposition` header value.
    pub content_disposition: Option<&'a str>,
}

/// A parsed `Content-Disposition` header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContentDisposition {
    /// `attachment`, `inline` or whatever the server said, lower-cased.
    pub kind: String,
    /// Filename (RFC 5987 `filename*` preferred over `filename`), path components removed.
    pub filename: Option<String>,
}

/// Where a PDF link on a page came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PdfLinkSource {
    /// `<meta name="citation_pdf_url">` (Google Scholar convention).
    CitationMeta,
    /// `<link rel="alternate" type="application/pdf">`.
    AlternateLink,
    /// An `<a href>` whose target classifies as a PDF.
    Anchor,
}

/// A PDF candidate found on a page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PdfLink {
    /// Absolute, normalised URL.
    pub url: String,
    /// Where it was found.
    pub source: PdfLinkSource,
    /// URL-only verdict (no response seen yet).
    pub verdict: PdfVerdict,
}

/// Classify from the URL alone.
pub fn classify_pdf_url(url: &NormalizedUrl) -> LinkClassification {
    let mut cls = LinkClassification {
        verdict: PdfVerdict::No,
        filename: None,
        reasons: Vec::new(),
    };
    let segments = url.path_segments();
    let ext = url.extension();
    if ext.as_deref() == Some("pdf") {
        cls.verdict = PdfVerdict::Likely;
        cls.reasons.push("path ends in .pdf");
        cls.filename = segments.last().map(|s| percent_decode(s));
    }
    if segments
        .iter()
        .any(|s| PDF_SEGMENTS.iter().any(|p| s.eq_ignore_ascii_case(p)))
    {
        cls.verdict = cls.verdict.max(PdfVerdict::Likely);
        cls.reasons.push("pdf path segment");
    }
    for key in ["format", "type", "mimetype", "mime"] {
        if url.query_param(key).is_some_and(|v| {
            v.eq_ignore_ascii_case("pdf") || v.eq_ignore_ascii_case("application/pdf")
        }) {
            cls.verdict = cls.verdict.max(PdfVerdict::Likely);
            cls.reasons.push("query asks for pdf");
            break;
        }
    }
    if url
        .query_param("download")
        .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
    {
        cls.verdict = cls.verdict.max(PdfVerdict::Possible);
        cls.reasons.push("download parameter");
    }
    if host_in_domain(&url.host, "ieee.org") && url.path.contains("/stamp/stamp.jsp") {
        cls.verdict = cls.verdict.max(PdfVerdict::Possible);
        cls.reasons.push("IEEE stamp viewer frame");
    }
    if cls.verdict == PdfVerdict::No
        && segments
            .iter()
            .any(|s| LANDING_SEGMENTS.iter().any(|p| s.eq_ignore_ascii_case(p)))
    {
        cls.reasons.push("landing page segment");
    }
    cls
}

/// Classify from the URL and the response headers. A `text/html` response
/// always wins (login wall, cookie interstitial or landing page), a PDF media
/// type always confirms, an opaque media type defers to the filename and URL.
pub fn classify_pdf_response(url: &NormalizedUrl, hints: &ResponseHints<'_>) -> LinkClassification {
    let mut cls = classify_pdf_url(url);
    let mime = hints.content_type.map(mime_type);
    let mut opaque = false;
    match mime.as_deref() {
        Some(m) if PDF_MIME_TYPES.contains(&m) => {
            cls.verdict = PdfVerdict::Confirmed;
            cls.reasons.push("content-type is pdf");
        }
        Some(m) if OPAQUE_MIME_TYPES.contains(&m) => {
            opaque = true;
            cls.reasons.push("opaque content-type");
        }
        Some(m) if m == "text/html" || m == "application/xhtml+xml" => {
            cls.verdict = PdfVerdict::No;
            cls.reasons
                .push("content-type is html: login wall, interstitial or landing page");
            return cls;
        }
        Some(_) => {
            cls.verdict = PdfVerdict::No;
            cls.reasons.push("non-pdf content-type");
            return cls;
        }
        None => {}
    }
    if let Some(header) = hints.content_disposition
        && let Some(name) = parse_content_disposition(header).filename
    {
        if has_extension(&name, "pdf") {
            cls.reasons.push("content-disposition filename is .pdf");
            let promoted = if opaque {
                PdfVerdict::Confirmed
            } else {
                PdfVerdict::Likely
            };
            cls.verdict = cls.verdict.max(promoted);
        }
        cls.filename = Some(name);
    }
    if opaque && cls.verdict == PdfVerdict::Likely {
        cls.verdict = PdfVerdict::Confirmed;
        cls.reasons.push("opaque content-type with pdf url");
    }
    cls
}

/// Lower-case media type without parameters (`Application/PDF; charset=x` → `application/pdf`).
pub fn mime_type(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

/// Parse a `Content-Disposition` header. Parameters are split on `;`, so a
/// quoted filename containing `;` is truncated (rare, and harmless here).
pub fn parse_content_disposition(header: &str) -> ContentDisposition {
    let mut parts = header.split(';');
    let kind = parts.next().unwrap_or("").trim().to_ascii_lowercase();
    let mut filename: Option<String> = None;
    let mut filename_star: Option<String> = None;
    for part in parts {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        if key == "filename*" {
            filename_star = decode_ext_value(value);
        } else if key == "filename" {
            filename = Some(unquote(value));
        }
    }
    ContentDisposition {
        kind,
        filename: filename_star
            .or(filename)
            .map(|f| sanitize_filename(&f))
            .filter(|f| !f.is_empty()),
    }
}

/// `true` when the first bytes of a body are a PDF header. The PDF spec lets
/// up to 1024 bytes of junk precede `%PDF-`, so the whole head is searched.
pub fn looks_like_pdf_bytes(head: &[u8]) -> bool {
    head.windows(5).take(1024).any(|w| w == b"%PDF-")
}

/// PDF candidates declared or linked on a page, deduplicated by URL, in the
/// order: `citation_pdf_url` meta tags, `<link type="application/pdf">`,
/// then anchors that classify as at least `Possible`.
pub fn pdf_links_in_html(html: &str, base: &NormalizedUrl) -> Vec<PdfLink> {
    let mut out: Vec<PdfLink> = Vec::new();
    let mut push = |target: &str, source: PdfLinkSource, floor: PdfVerdict| {
        if let Ok(url) = base.resolve(target) {
            let verdict = classify_pdf_url(&url).verdict.max(floor);
            let text = url.to_string();
            if verdict > PdfVerdict::No && !out.iter().any(|l| l.url == text) {
                out.push(PdfLink {
                    url: text,
                    source,
                    verdict,
                });
            }
        }
    };
    for content in meta_content(html, "citation_pdf_url") {
        push(&content, PdfLinkSource::CitationMeta, PdfVerdict::Likely);
    }
    let all = tags(html);
    for tag in all.iter().filter(|t| t.name == "link") {
        let is_pdf = tag
            .attr("type")
            .is_some_and(|t| mime_type(t) == "application/pdf");
        if let Some(href) = tag.attr("href")
            && is_pdf
        {
            push(href, PdfLinkSource::AlternateLink, PdfVerdict::Likely);
        }
    }
    for tag in all.iter().filter(|t| t.name == "a") {
        let Some(href) = tag.attr("href") else {
            continue;
        };
        let typed_pdf = tag
            .attr("type")
            .is_some_and(|t| mime_type(t) == "application/pdf");
        let floor = if typed_pdf {
            PdfVerdict::Likely
        } else {
            PdfVerdict::No
        };
        push(href, PdfLinkSource::Anchor, floor);
    }
    out
}

/// Case-insensitive extension test without `str::ends_with` on a literal.
fn has_extension(name: &str, ext: &str) -> bool {
    name.rsplit_once('.')
        .is_some_and(|(_, e)| e.eq_ignore_ascii_case(ext))
}

fn unquote(value: &str) -> String {
    let inner = value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or(value);
    inner.replace("\\\"", "\"")
}

/// RFC 5987 `charset'language'percent-encoded`.
fn decode_ext_value(value: &str) -> Option<String> {
    let mut parts = value.splitn(3, '\'');
    let charset = parts.next()?.trim().to_ascii_lowercase();
    parts.next()?;
    let encoded = parts.next()?;
    if charset != "utf-8" && charset != "iso-8859-1" {
        return None;
    }
    Some(percent_decode(encoded))
}

fn sanitize_filename(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name).trim();
    base.chars().filter(|c| !c.is_control()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> NormalizedUrl {
        NormalizedUrl::parse(s).unwrap()
    }

    fn verdict(s: &str) -> PdfVerdict {
        classify_pdf_url(&url(s)).verdict
    }

    #[test]
    fn url_only_verdicts() {
        assert_eq!(
            verdict("https://arxiv.org/pdf/2502.00857.pdf"),
            PdfVerdict::Likely
        );
        assert_eq!(
            verdict("https://arxiv.org/pdf/2502.00857"),
            PdfVerdict::Likely
        );
        assert_eq!(
            verdict("https://onlinelibrary.wiley.com/doi/pdfdirect/10.1002/x"),
            PdfVerdict::Likely
        );
        assert_eq!(
            verdict(
                "https://www.sciencedirect.com/science/article/pii/S000/pdfft?md5=a&pid=1-s2.0-main.pdf"
            ),
            PdfVerdict::Likely
        );
        assert_eq!(
            verdict("https://x.org/download?format=PDF"),
            PdfVerdict::Likely
        );
        assert_eq!(
            verdict("https://x.org/paper?download=true"),
            PdfVerdict::Possible
        );
        assert_eq!(
            verdict("https://ieeexplore.ieee.org/stamp/stamp.jsp?tp=&arnumber=1"),
            PdfVerdict::Possible
        );
        assert_eq!(verdict("https://arxiv.org/abs/2502.00857"), PdfVerdict::No);
        assert_eq!(
            verdict("https://onlinelibrary.wiley.com/doi/full/10.1002/x"),
            PdfVerdict::No
        );
        assert_eq!(verdict("https://www.nature.com/articles/x"), PdfVerdict::No);
    }

    #[test]
    fn url_only_reasons_and_filename() {
        let cls = classify_pdf_url(&url("https://x.org/content/pdf/My%20Paper.PDF"));
        assert_eq!(cls.verdict, PdfVerdict::Likely);
        assert_eq!(cls.filename, Some("My Paper.PDF".to_string()));
        assert_eq!(cls.reasons, ["path ends in .pdf", "pdf path segment"]);
        let landing = classify_pdf_url(&url("https://x.org/doi/abs/10.1000/x"));
        assert_eq!(landing.reasons, ["landing page segment"]);
        assert_eq!(landing.filename, None);
    }

    #[test]
    fn response_media_type_confirms_or_refutes() {
        let u = url("https://x.org/content/pdf/paper.pdf");
        let pdf = classify_pdf_response(
            &u,
            &ResponseHints {
                content_type: Some("Application/PDF; charset=binary"),
                content_disposition: None,
            },
        );
        assert_eq!(pdf.verdict, PdfVerdict::Confirmed);

        let html = classify_pdf_response(
            &u,
            &ResponseHints {
                content_type: Some("text/html; charset=utf-8"),
                content_disposition: Some("attachment; filename=\"paper.pdf\""),
            },
        );
        assert_eq!(html.verdict, PdfVerdict::No);
        assert!(html.reasons.iter().any(|r| r.contains("login wall")));

        let other = classify_pdf_response(
            &u,
            &ResponseHints {
                content_type: Some("image/png"),
                content_disposition: None,
            },
        );
        assert_eq!(other.verdict, PdfVerdict::No);

        let none = classify_pdf_response(&u, &ResponseHints::default());
        assert_eq!(none.verdict, PdfVerdict::Likely);
    }

    #[test]
    fn opaque_media_type_defers_to_filename_and_url() {
        let landing = url("https://x.org/download/12345");
        let with_name = classify_pdf_response(
            &landing,
            &ResponseHints {
                content_type: Some("application/octet-stream"),
                content_disposition: Some("attachment; filename=\"12345.pdf\""),
            },
        );
        assert_eq!(with_name.verdict, PdfVerdict::Confirmed);
        assert_eq!(with_name.filename, Some("12345.pdf".to_string()));

        let without_name = classify_pdf_response(
            &landing,
            &ResponseHints {
                content_type: Some("application/octet-stream"),
                content_disposition: None,
            },
        );
        assert_eq!(without_name.verdict, PdfVerdict::No);

        let pdf_url = classify_pdf_response(
            &url("https://x.org/content/pdf/paper.pdf"),
            &ResponseHints {
                content_type: Some("binary/octet-stream"),
                content_disposition: None,
            },
        );
        assert_eq!(pdf_url.verdict, PdfVerdict::Confirmed);

        let zip_name = classify_pdf_response(
            &landing,
            &ResponseHints {
                content_type: Some("application/octet-stream"),
                content_disposition: Some("attachment; filename=data.zip"),
            },
        );
        assert_eq!(zip_name.verdict, PdfVerdict::No);
        assert_eq!(zip_name.filename, Some("data.zip".to_string()));
    }

    #[test]
    fn disposition_alone_is_only_likely() {
        let cls = classify_pdf_response(
            &url("https://x.org/download/12345"),
            &ResponseHints {
                content_type: None,
                content_disposition: Some("inline; filename*=UTF-8''My%20Paper.pdf"),
            },
        );
        assert_eq!(cls.verdict, PdfVerdict::Likely);
        assert_eq!(cls.filename, Some("My Paper.pdf".to_string()));
    }

    #[test]
    fn content_disposition_parsing() {
        let cd = parse_content_disposition("Attachment; filename=\"a \\\"b\\\".pdf\"");
        assert_eq!(cd.kind, "attachment");
        assert_eq!(cd.filename, Some("a \"b\".pdf".to_string()));

        let star = parse_content_disposition(
            "attachment; filename=\"fallback.pdf\"; filename*=utf-8'en'%E2%82%AC%20rates.pdf",
        );
        assert_eq!(star.filename, Some("€ rates.pdf".to_string()));

        let bad_charset =
            parse_content_disposition("attachment; filename*=gb2312''x.pdf; filename=y.pdf");
        assert_eq!(bad_charset.filename, Some("y.pdf".to_string()));

        let traversal = parse_content_disposition("attachment; filename=\"../../etc/passwd\"");
        assert_eq!(traversal.filename, Some("passwd".to_string()));

        let windows = parse_content_disposition("attachment; filename=C:\\Users\\me\\paper.pdf");
        assert_eq!(windows.filename, Some("paper.pdf".to_string()));

        let inline = parse_content_disposition("inline");
        assert_eq!(inline.kind, "inline");
        assert_eq!(inline.filename, None);

        let empty = parse_content_disposition("attachment; filename=\"\"");
        assert_eq!(empty.filename, None);
    }

    #[test]
    fn mime_type_normalises() {
        assert_eq!(
            mime_type("Application/PDF; charset=binary"),
            "application/pdf"
        );
        assert_eq!(mime_type(" text/html "), "text/html");
        assert_eq!(mime_type(""), "");
    }

    #[test]
    fn pdf_magic() {
        assert!(looks_like_pdf_bytes(b"%PDF-1.7\n%\xE2\xE3\xCF\xD3"));
        assert!(looks_like_pdf_bytes(b"\xEF\xBB\xBF\r\n%PDF-1.4"));
        assert!(!looks_like_pdf_bytes(b"<!DOCTYPE html><html>"));
        assert!(!looks_like_pdf_bytes(b"%PDF"));
        assert!(!looks_like_pdf_bytes(b""));
        let mut junk = vec![b' '; 1100];
        junk.extend_from_slice(b"%PDF-1.0");
        assert!(!looks_like_pdf_bytes(&junk));
    }

    #[test]
    fn pdf_links_on_a_page() {
        let base = url("https://www.nature.com/articles/s41586-020-2649-2");
        let html = r#"<html><head>
            <meta name="citation_pdf_url" content="https://www.nature.com/articles/s41586-020-2649-2.pdf">
            <link rel="alternate" type="application/pdf" href="/articles/s41586-020-2649-2.pdf">
            <link rel="stylesheet" href="/style.css">
            </head><body>
            <a href="/articles/s41586-020-2649-2.pdf">Download PDF</a>
            <a href="/articles/s41586-020-2649-2/metrics">Metrics</a>
            <a href="supplement.bin" type="application/pdf">Supplement</a>
            <a href="javascript:void(0)">x</a>
            </body></html>"#;
        let links = pdf_links_in_html(html, &base);
        assert_eq!(links.len(), 2);
        assert_eq!(
            links[0].url,
            "https://www.nature.com/articles/s41586-020-2649-2.pdf"
        );
        assert_eq!(links[0].source, PdfLinkSource::CitationMeta);
        assert_eq!(links[0].verdict, PdfVerdict::Likely);
        assert_eq!(
            links[1].url,
            "https://www.nature.com/articles/supplement.bin"
        );
        assert_eq!(links[1].source, PdfLinkSource::Anchor);
        assert!(pdf_links_in_html("<p>no links</p>", &base).is_empty());
    }

    #[test]
    fn verdict_order_and_labels() {
        assert!(PdfVerdict::No < PdfVerdict::Possible);
        assert!(PdfVerdict::Possible < PdfVerdict::Likely);
        assert!(PdfVerdict::Likely < PdfVerdict::Confirmed);
        assert_eq!(PdfVerdict::Confirmed.as_str(), "confirmed");
    }
}
