//! [`BrowserSession`]: the decision layer between the Chromium shell and the
//! engine. The shell asks it what to do with a navigation, a response and a
//! loaded page; it answers with data (block, load, hand off a DOI / arXiv id /
//! PDF) and keeps the cookie jar and a history. It never fetches anything.
//!
//! A [`Navigation::Handoff`] never blocks the shell: it means "give this to
//! the engine" (resolve the DOI through `tpe-biblio`, fetch the arXiv PDF,
//! download the PDF into `tpe extract`). Whether the shell also shows the
//! page is the app's choice; for PDFs it should not, since Chromium's viewer
//! would otherwise swallow the bytes the engine wants.

use std::collections::HashSet;

use crate::BrowserError;
use crate::cookies::{Cookie, CookieJar};
use crate::doi::{
    MAX_IDENTIFIERS, MAX_SCAN_BYTES, arxiv_id_in_url, arxiv_ids_in_text, declared_dois_in_html,
    dois_in_html, dois_in_url, is_doi_resolver,
};
use crate::hosts::{HostDecision, ResearchPolicy};
use crate::html::strip_tags;
use crate::pdf::{
    LinkClassification, PdfLink, PdfVerdict, ResponseHints, classify_pdf_response,
    classify_pdf_url, pdf_links_in_html,
};
use crate::url::NormalizedUrl;

/// Something the engine should take over.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Intercept {
    /// A DOI addressed through a resolver host.
    Doi(String),
    /// An arXiv identifier addressed through `arxiv.org`.
    Arxiv(String),
    /// A PDF to download and extract.
    Pdf {
        /// Normalised URL of the PDF.
        url: String,
        /// How sure the classifier is.
        verdict: PdfVerdict,
        /// Suggested filename when one was found.
        filename: Option<String>,
    },
}

/// What the shell should do with a navigation request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Navigation {
    /// Research mode refuses the host; show a "not a scholarly host" notice.
    Blocked {
        /// Normalised URL.
        url: String,
        /// The refused host.
        host: String,
    },
    /// Load the page normally.
    Load {
        /// Normalised URL to load.
        url: String,
        /// DOIs visible in the URL itself (publisher landing pages).
        dois: Vec<String>,
        /// The publisher host behind a library proxy, when the URL is proxied.
        proxied_origin: Option<String>,
    },
    /// Hand an identifier or file to the engine.
    Handoff {
        /// Normalised URL that triggered the handoff.
        url: String,
        /// What to hand over.
        intercept: Intercept,
    },
}

/// What a loaded page tells us about itself.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PageFacts {
    /// Normalised page URL.
    pub url: String,
    /// The page's own DOI: the first declared `<meta>` DOI, else the first DOI in the URL.
    pub primary_doi: Option<String>,
    /// Every DOI declared, linked or mentioned, declared ones first.
    pub dois: Vec<String>,
    /// arXiv identifiers from the URL and from `arXiv:` mentions in the text.
    pub arxiv_ids: Vec<String>,
    /// PDF candidates declared or linked on the page.
    pub pdf_links: Vec<PdfLink>,
}

/// A browsing session: policy, cookies, history and what was handed to the engine.
#[derive(Clone, Debug)]
pub struct BrowserSession {
    policy: ResearchPolicy,
    jar: CookieJar,
    history: Vec<NormalizedUrl>,
    handoffs: Vec<Intercept>,
    pages: Vec<PageFacts>,
}

impl BrowserSession {
    /// A session under `policy` with an empty jar.
    pub fn new(policy: ResearchPolicy) -> Self {
        Self {
            policy,
            jar: CookieJar::new(),
            history: Vec::new(),
            handoffs: Vec::new(),
            pages: Vec::new(),
        }
    }

    /// Research mode (scholarly hosts only).
    pub fn research() -> Self {
        Self::new(ResearchPolicy::research())
    }

    /// Open mode (every host).
    pub fn open() -> Self {
        Self::new(ResearchPolicy::open())
    }

    /// The host policy.
    pub fn policy(&self) -> &ResearchPolicy {
        &self.policy
    }

    /// Mutable access to the host policy (toggle research mode, add hosts).
    pub fn policy_mut(&mut self) -> &mut ResearchPolicy {
        &mut self.policy
    }

    /// The cookie jar.
    pub fn jar(&self) -> &CookieJar {
        &self.jar
    }

    /// Mutable access to the cookie jar (import/export, pruning).
    pub fn jar_mut(&mut self) -> &mut CookieJar {
        &mut self.jar
    }

    /// Every URL that was loaded or handed off, oldest first.
    pub fn history(&self) -> &[NormalizedUrl] {
        &self.history
    }

    /// Everything handed to the engine, oldest first.
    pub fn handoffs(&self) -> &[Intercept] {
        &self.handoffs
    }

    /// Facts gathered from inspected pages, oldest first.
    pub fn pages(&self) -> &[PageFacts] {
        &self.pages
    }

    /// Decide a navigation. Order: host policy, DOI resolver, arXiv, PDF
    /// classification, then a plain load carrying any DOIs seen in the URL.
    pub fn navigate(&mut self, raw_url: &str) -> Result<Navigation, BrowserError> {
        let url = NormalizedUrl::parse(raw_url)?;
        let text = url.to_string();
        let proxied_origin = match self.policy.decide(&url.host) {
            HostDecision::Allow => None,
            HostDecision::Block => {
                return Ok(Navigation::Blocked {
                    url: text,
                    host: url.host,
                });
            }
            HostDecision::Proxied { origin } => Some(origin),
        };
        let judged_host = proxied_origin.as_deref().unwrap_or(url.host.as_str());
        // Detection runs against the unwrapped origin so a proxied arXiv or
        // publisher URL (arxiv-org.ezproxy.example.edu/abs/…) is recognised.
        let judged_url = NormalizedUrl {
            host: judged_host.to_string(),
            ..url.clone()
        };
        let intercept = if is_doi_resolver(judged_host) {
            dois_in_url(&judged_url)
                .into_iter()
                .next()
                .map(Intercept::Doi)
        } else if let Some(id) = arxiv_id_in_url(&judged_url) {
            Some(Intercept::Arxiv(id))
        } else {
            pdf_intercept(&text, &classify_pdf_url(&judged_url))
        };
        self.history.push(url.clone());
        if let Some(intercept) = intercept {
            self.handoffs.push(intercept.clone());
            return Ok(Navigation::Handoff {
                url: text,
                intercept,
            });
        }
        Ok(Navigation::Load {
            url: text,
            dois: dois_in_url(&url),
            proxied_origin,
        })
    }

    /// Judge a response by its headers; a PDF (`Likely` or better) is handed off.
    pub fn on_response(
        &mut self,
        raw_url: &str,
        hints: &ResponseHints<'_>,
    ) -> Result<Option<Intercept>, BrowserError> {
        let url = NormalizedUrl::parse(raw_url)?;
        let intercept = pdf_intercept(&url.to_string(), &classify_pdf_response(&url, hints));
        if let Some(intercept) = &intercept {
            self.handoffs.push(intercept.clone());
        }
        Ok(intercept)
    }

    /// Store a `Set-Cookie` header received from `raw_url`; `Ok(false)` when
    /// the header was rejected (bad name, or a `Domain` the host may not set:
    /// a foreign domain or a public suffix). Without a `Path` attribute the
    /// cookie is scoped to the default path of `raw_url` (RFC 6265 5.1.4).
    pub fn on_set_cookie(
        &mut self,
        raw_url: &str,
        header: &str,
        now_unix: i64,
    ) -> Result<bool, BrowserError> {
        let url = NormalizedUrl::parse(raw_url)?;
        let Some(cookie) = Cookie::parse_set_cookie(header, &url, now_unix) else {
            return Ok(false);
        };
        self.jar.insert(cookie);
        Ok(true)
    }

    /// The `Cookie:` header to send with a request to `raw_url`, if any.
    pub fn cookie_header(
        &self,
        raw_url: &str,
        now_unix: i64,
    ) -> Result<Option<String>, BrowserError> {
        let url = NormalizedUrl::parse(raw_url)?;
        Ok(self.jar.header_for(&url, now_unix))
    }

    /// Gather identifiers and PDF links from a loaded page and remember them.
    pub fn inspect_html(&mut self, raw_url: &str, html: &str) -> Result<PageFacts, BrowserError> {
        let url = NormalizedUrl::parse(raw_url)?;
        if html.len() > MAX_SCAN_BYTES {
            return Err(BrowserError::PageTooLarge {
                actual: html.len(),
                maximum: MAX_SCAN_BYTES,
            });
        }
        let facts = inspect_page(&url, html);
        self.pages.push(facts.clone());
        Ok(facts)
    }
}

/// Identifiers and PDF links of a page at `url`.
pub fn inspect_page(url: &NormalizedUrl, html: &str) -> PageFacts {
    let html = bounded_html(html);
    let declared = declared_dois_in_html(html);
    let url_dois = dois_in_url(url);
    let primary_doi = declared
        .first()
        .cloned()
        .or_else(|| url_dois.first().cloned());
    let mut dois = dois_in_html(html);
    for doi in url_dois {
        if dois.contains(&doi) {
            continue;
        }
        // The page's own identifiers outrank the last mentioned one.
        if dois.len() == MAX_IDENTIFIERS {
            dois.pop();
        }
        dois.push(doi);
    }
    let mut arxiv_ids: Vec<String> = arxiv_id_in_url(url).into_iter().collect();
    let mut seen_arxiv: HashSet<String> = arxiv_ids.iter().cloned().collect();
    for id in arxiv_ids_in_text(&strip_tags(html)) {
        if arxiv_ids.len() == MAX_IDENTIFIERS {
            break;
        }
        if seen_arxiv.insert(id.clone()) {
            arxiv_ids.push(id);
        }
    }
    PageFacts {
        url: url.to_string(),
        primary_doi,
        dois,
        arxiv_ids,
        pdf_links: pdf_links_in_html(html, url),
    }
}

fn bounded_html(html: &str) -> &str {
    if html.len() <= MAX_SCAN_BYTES {
        return html;
    }
    let mut end = MAX_SCAN_BYTES;
    while !html.is_char_boundary(end) {
        end -= 1;
    }
    &html[..end]
}

fn pdf_intercept(url: &str, cls: &LinkClassification) -> Option<Intercept> {
    if cls.verdict < PdfVerdict::Likely {
        return None;
    }
    Some(Intercept::Pdf {
        url: url.to_string(),
        verdict: cls.verdict,
        filename: cls.filename.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hosts::LibraryProxy;

    const NOW: i64 = 1_700_000_000;

    #[test]
    fn research_mode_blocks_and_open_mode_loads() {
        let mut research = BrowserSession::research();
        assert_eq!(
            research.navigate("https://news.example.com/story").unwrap(),
            Navigation::Blocked {
                url: "https://news.example.com/story".to_string(),
                host: "news.example.com".to_string(),
            }
        );
        assert!(research.history().is_empty());

        let mut open = BrowserSession::open();
        assert_eq!(
            open.navigate("https://news.example.com/story").unwrap(),
            Navigation::Load {
                url: "https://news.example.com/story".to_string(),
                dois: Vec::new(),
                proxied_origin: None,
            }
        );
        assert_eq!(open.history().len(), 1);
    }

    #[test]
    fn doi_resolver_hands_off_the_doi() {
        let mut s = BrowserSession::research();
        let nav = s
            .navigate("https://doi.org/10.1038/S41586-020-2649-2?utm_source=x")
            .unwrap();
        assert_eq!(
            nav,
            Navigation::Handoff {
                url: "https://doi.org/10.1038/S41586-020-2649-2".to_string(),
                intercept: Intercept::Doi("10.1038/s41586-020-2649-2".to_string()),
            }
        );
        assert_eq!(s.handoffs().len(), 1);
        assert_eq!(s.history().len(), 1);
        // A resolver URL without a DOI is just a load.
        assert!(matches!(
            s.navigate("https://doi.org/").unwrap(),
            Navigation::Load { .. }
        ));
    }

    #[test]
    fn arxiv_and_pdf_urls_hand_off() {
        let mut s = BrowserSession::research();
        assert_eq!(
            s.navigate("https://arxiv.org/abs/2502.00857v2").unwrap(),
            Navigation::Handoff {
                url: "https://arxiv.org/abs/2502.00857v2".to_string(),
                intercept: Intercept::Arxiv("2502.00857".to_string()),
            }
        );
        let pdf = s
            .navigate("https://link.springer.com/content/pdf/10.1007/s00134-020-06022-5.pdf")
            .unwrap();
        match pdf {
            Navigation::Handoff {
                intercept:
                    Intercept::Pdf {
                        verdict, filename, ..
                    },
                ..
            } => {
                assert_eq!(verdict, PdfVerdict::Likely);
                assert_eq!(filename, Some("s00134-020-06022-5.pdf".to_string()));
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(s.handoffs().len(), 2);
    }

    #[test]
    fn landing_pages_load_with_their_dois() {
        let mut s = BrowserSession::research();
        assert_eq!(
            s.navigate("https://onlinelibrary.wiley.com/doi/full/10.1002/anie.201900001")
                .unwrap(),
            Navigation::Load {
                url: "https://onlinelibrary.wiley.com/doi/full/10.1002/anie.201900001".to_string(),
                dois: vec!["10.1002/anie.201900001".to_string()],
                proxied_origin: None,
            }
        );
    }

    #[test]
    fn proxied_hosts_are_judged_by_their_origin() {
        let policy =
            ResearchPolicy::research().with_proxy(LibraryProxy::ezproxy("ezproxy.lib.edu"));
        let mut s = BrowserSession::new(policy);
        assert_eq!(
            s.navigate("https://www-nature-com.ezproxy.lib.edu/articles/x")
                .unwrap(),
            Navigation::Load {
                url: "https://www-nature-com.ezproxy.lib.edu/articles/x".to_string(),
                dois: Vec::new(),
                proxied_origin: Some("www.nature.com".to_string()),
            }
        );
        assert!(matches!(
            s.navigate("https://doi-org.ezproxy.lib.edu/10.1000/x")
                .unwrap(),
            Navigation::Handoff {
                intercept: Intercept::Doi(_),
                ..
            }
        ));
        assert_eq!(
            s.navigate("https://arxiv-org.ezproxy.lib.edu/abs/2502.00857")
                .unwrap(),
            Navigation::Handoff {
                url: "https://arxiv-org.ezproxy.lib.edu/abs/2502.00857".to_string(),
                intercept: Intercept::Arxiv("2502.00857".to_string()),
            },
            "arXiv detection uses the unwrapped proxy origin"
        );
        assert!(matches!(
            s.navigate("https://www-example-com.ezproxy.lib.edu/")
                .unwrap(),
            Navigation::Blocked { .. }
        ));
        s.policy_mut().enabled = false;
        assert!(matches!(
            s.navigate("https://www-example-com.ezproxy.lib.edu/")
                .unwrap(),
            Navigation::Load { .. }
        ));
    }

    #[test]
    fn responses_hand_off_pdfs_but_not_login_walls() {
        let mut s = BrowserSession::research();
        let pdf = s
            .on_response(
                "https://www.nature.com/articles/x.pdf",
                &ResponseHints {
                    content_type: Some("application/pdf"),
                    content_disposition: Some("attachment; filename=\"x.pdf\""),
                },
            )
            .unwrap();
        assert_eq!(
            pdf,
            Some(Intercept::Pdf {
                url: "https://www.nature.com/articles/x.pdf".to_string(),
                verdict: PdfVerdict::Confirmed,
                filename: Some("x.pdf".to_string()),
            })
        );
        let wall = s
            .on_response(
                "https://www.nature.com/articles/x.pdf",
                &ResponseHints {
                    content_type: Some("text/html; charset=utf-8"),
                    content_disposition: None,
                },
            )
            .unwrap();
        assert_eq!(wall, None);
        assert_eq!(s.handoffs().len(), 1);
    }

    #[test]
    fn cookies_flow_through_the_session() {
        let mut s = BrowserSession::research();
        assert!(
            s.on_set_cookie(
                "https://login.ezproxy.lib.edu/login",
                "ezproxy=tok; Domain=.ezproxy.lib.edu; Path=/; Secure; HttpOnly",
                NOW
            )
            .unwrap()
        );
        assert!(
            !s.on_set_cookie(
                "https://login.ezproxy.lib.edu/login",
                "bad=1; Domain=other.org",
                NOW
            )
            .unwrap()
        );
        assert_eq!(
            s.cookie_header("https://www-nature-com.ezproxy.lib.edu/articles/x", NOW)
                .unwrap(),
            Some("ezproxy=tok".to_string())
        );
        assert_eq!(
            s.cookie_header("http://www-nature-com.ezproxy.lib.edu/articles/x", NOW)
                .unwrap(),
            None
        );
        assert_eq!(
            s.cookie_header("https://www.nature.com/", NOW).unwrap(),
            None
        );
        assert_eq!(s.jar().len(), 1);
        s.jar_mut().remove_expired(NOW);
        assert_eq!(s.jar().len(), 1);
    }

    #[test]
    fn set_cookie_path_defaults_from_the_request_url() {
        let mut s = BrowserSession::open();
        assert!(
            s.on_set_cookie("https://www.ebi.ac.uk/account/login", "sid=1", NOW)
                .unwrap()
        );
        assert_eq!(s.jar().all()[0].path, "/account");
        assert_eq!(
            s.cookie_header("https://www.ebi.ac.uk/account/settings", NOW)
                .unwrap(),
            Some("sid=1".to_string())
        );
        assert_eq!(
            s.cookie_header("https://www.ebi.ac.uk/other", NOW).unwrap(),
            None
        );
        assert!(
            !s.on_set_cookie("https://www.ebi.ac.uk/", "wide=1; Domain=ac.uk", NOW)
                .unwrap()
        );
        assert_eq!(s.jar().len(), 1);
    }

    #[test]
    fn denied_origins_stay_blocked_behind_the_proxy() {
        let policy = ResearchPolicy::open()
            .refuse("example.com")
            .with_proxy(LibraryProxy::ezproxy("ezproxy.lib.edu"));
        let mut s = BrowserSession::new(policy);
        assert_eq!(
            s.navigate("https://www-example-com.ezproxy.lib.edu/paper")
                .unwrap(),
            Navigation::Blocked {
                url: "https://www-example-com.ezproxy.lib.edu/paper".to_string(),
                host: "www-example-com.ezproxy.lib.edu".to_string(),
            }
        );
        assert!(matches!(
            s.navigate("https://www.example.com.ezproxy.lib.edu/")
                .unwrap(),
            Navigation::Blocked { .. }
        ));
        assert!(matches!(
            s.navigate("https://www-nature-com.ezproxy.lib.edu/articles/x")
                .unwrap(),
            Navigation::Load {
                proxied_origin: Some(_),
                ..
            }
        ));
        assert_eq!(s.history().len(), 1);
    }

    #[test]
    fn inspecting_a_page_collects_facts() {
        let mut s = BrowserSession::research();
        let html = r#"<html><head>
            <meta name="citation_doi" content="10.1038/s41586-020-2649-2">
            <meta name="citation_pdf_url" content="/articles/s41586-020-2649-2.pdf">
            </head><body>
            <p>Preprint: arXiv:2006.10256. See also 10.1000/other.</p>
            </body></html>"#;
        let facts = s
            .inspect_html("https://www.nature.com/articles/s41586-020-2649-2", html)
            .unwrap();
        assert_eq!(
            facts.url,
            "https://www.nature.com/articles/s41586-020-2649-2"
        );
        assert_eq!(
            facts.primary_doi,
            Some("10.1038/s41586-020-2649-2".to_string())
        );
        assert_eq!(facts.dois, ["10.1038/s41586-020-2649-2", "10.1000/other"]);
        assert_eq!(facts.arxiv_ids, ["2006.10256"]);
        assert_eq!(facts.pdf_links.len(), 1);
        assert_eq!(
            facts.pdf_links[0].url,
            "https://www.nature.com/articles/s41586-020-2649-2.pdf"
        );
        assert_eq!(s.pages().len(), 1);

        let url = NormalizedUrl::parse("https://arxiv.org/abs/2502.00857").unwrap();
        let bare = inspect_page(&url, "<p>nothing</p>");
        assert_eq!(bare.primary_doi, None);
        assert_eq!(bare.arxiv_ids, ["2502.00857"]);
        assert!(bare.pdf_links.is_empty());

        let url = NormalizedUrl::parse("https://x.org/doi/abs/10.1000/from-url").unwrap();
        let from_url = inspect_page(&url, "<p>nothing</p>");
        assert_eq!(from_url.primary_doi, Some("10.1000/from-url".to_string()));
        assert_eq!(from_url.dois, ["10.1000/from-url"]);
    }

    #[test]
    fn invalid_urls_are_errors_not_panics() {
        let mut s = BrowserSession::research();
        assert!(s.navigate("javascript:alert(1)").is_err());
        assert!(s.navigate("").is_err());
        assert!(s.cookie_header("nope", NOW).is_err());
        assert!(s.inspect_html("nope", "<p></p>").is_err());
        assert!(s.history().is_empty());
    }

    #[test]
    fn capped_identifier_lists_keep_the_url_identifiers() {
        use std::fmt::Write as _;
        let mut html = String::new();
        for i in 0..MAX_IDENTIFIERS + 5 {
            write!(html, "<p>10.1000/m{i} arXiv:2401.{i:05}</p>").unwrap();
        }
        let url = NormalizedUrl::parse("https://doi.org/10.9999/from-url").unwrap();
        let facts = inspect_page(&url, &html);
        assert_eq!(facts.dois.len(), MAX_IDENTIFIERS);
        assert!(facts.dois.iter().any(|doi| doi == "10.9999/from-url"));
        assert_eq!(facts.primary_doi.as_deref(), Some("10.9999/from-url"));

        let url = NormalizedUrl::parse("https://arxiv.org/abs/2312.99999").unwrap();
        let facts = inspect_page(&url, &html);
        assert_eq!(facts.arxiv_ids.len(), MAX_IDENTIFIERS);
        assert_eq!(facts.arxiv_ids[0], "2312.99999");
    }

    #[test]
    fn page_inspection_rejects_oversized_html() {
        let mut s = BrowserSession::open();
        let html = "x".repeat(MAX_SCAN_BYTES + 1);
        assert!(matches!(
            s.inspect_html("https://example.com", &html),
            Err(BrowserError::PageTooLarge { .. })
        ));
        assert!(s.pages().is_empty());
    }
}
