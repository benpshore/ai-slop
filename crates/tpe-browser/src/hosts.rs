//! Research mode: an allowlist of scholarly hosts, per-session additions, and
//! library proxy (`EZproxy`-style) handling so that a proxied publisher host is
//! judged by the publisher it stands for.

use crate::url::{NormalizedUrl, host_in_domain, percent_encode_component};

/// Domain suffixes treated as scholarly: identifier resolvers, indexes,
/// preprint servers, repositories, publishers and library discovery
/// platforms. A host matches when it equals an entry or is a subdomain of it.
pub const SCHOLARLY_HOSTS: &[&str] = &[
    // identifiers and indexes
    "doi.org",
    "hdl.handle.net",
    "orcid.org",
    "crossref.org",
    "openalex.org",
    "semanticscholar.org",
    "unpaywall.org",
    "scholar.google.com",
    "scholar.archive.org",
    "lens.org",
    "dimensions.ai",
    "scopus.com",
    "webofscience.com",
    "base-search.net",
    "core.ac.uk",
    "dblp.org",
    "inspirehep.net",
    "adsabs.harvard.edu",
    "ui.adsabs.harvard.edu",
    "philpapers.org",
    "econpapers.repec.org",
    "ideas.repec.org",
    "worldcat.org",
    // biomedical
    "ncbi.nlm.nih.gov",
    "nih.gov",
    "europepmc.org",
    "ebi.ac.uk",
    "cochranelibrary.com",
    "who.int",
    // preprints and repositories
    "arxiv.org",
    "biorxiv.org",
    "medrxiv.org",
    "chemrxiv.org",
    "psyarxiv.com",
    "osf.io",
    "ssrn.com",
    "zenodo.org",
    "hal.science",
    "researchsquare.com",
    "preprints.org",
    "figshare.com",
    "dryad.org",
    "datadryad.org",
    "openreview.net",
    "aclanthology.org",
    "proceedings.mlr.press",
    "neurips.cc",
    "papers.nips.cc",
    "openaccess.thecvf.com",
    "ijcai.org",
    "aaai.org",
    "jmlr.org",
    "eprint.iacr.org",
    "escholarship.org",
    "dspace.mit.edu",
    "eric.ed.gov",
    // publishers and platforms
    "springer.com",
    "springernature.com",
    "nature.com",
    "biomedcentral.com",
    "sciencedirect.com",
    "elsevier.com",
    "cell.com",
    "thelancet.com",
    "wiley.com",
    "tandfonline.com",
    "sagepub.com",
    "ieee.org",
    "acm.org",
    "jstor.org",
    "oup.com",
    "cambridge.org",
    "pnas.org",
    "science.org",
    "plos.org",
    "frontiersin.org",
    "mdpi.com",
    "bmj.com",
    "nejm.org",
    "jamanetwork.com",
    "karger.com",
    "degruyter.com",
    "emerald.com",
    "iop.org",
    "aps.org",
    "acs.org",
    "rsc.org",
    "aip.org",
    "worldscientific.com",
    "muse.jhu.edu",
    "hindawi.com",
    "peerj.com",
    "elifesciences.org",
    "royalsocietypublishing.org",
    "annualreviews.org",
    "psycnet.apa.org",
    "apa.org",
    "liebertpub.com",
    "thieme-connect.com",
    "ahajournals.org",
    "asm.org",
    "aacrjournals.org",
    "ashpublications.org",
    "journals.physiology.org",
    "informs.org",
    "siam.org",
    "ams.org",
    "projecteuclid.org",
    "mit.edu",
    "uchicago.edu",
    "brill.com",
    "ingentaconnect.com",
    "scielo.br",
    "scielo.org",
    "doaj.org",
    // library discovery and access platforms
    "ebscohost.com",
    "proquest.com",
    "exlibrisgroup.com",
    "ovid.com",
    "gale.com",
    "openathens.net",
    "zotero.org",
];

/// `true` when `host` is, or is a subdomain of, an entry in [`SCHOLARLY_HOSTS`].
pub fn is_scholarly_host(host: &str) -> bool {
    SCHOLARLY_HOSTS.iter().any(|d| host_in_domain(host, d))
}

/// Outcome of judging a host under a [`ResearchPolicy`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostDecision {
    /// The host may be loaded.
    Allow,
    /// Research mode refuses the host.
    Block,
    /// The host is the library proxy standing in for a scholarly `origin`
    /// (`www-nature-com.ezproxy.lib.edu` for `www.nature.com`).
    Proxied {
        /// The publisher host behind the proxy.
        origin: String,
    },
}

/// How a library proxy rewrites publisher hosts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProxyKind {
    /// `EZproxy` "proxy by hostname": `www.nature.com` becomes
    /// `www-nature-com.<proxy>` (dots to hyphens, wildcard certificate) or the
    /// older `www.nature.com.<proxy>`. Sessions start at `https://<proxy>/login?url=<target>`.
    EzproxyHostname,
    /// A redirector that only prefixes the target (`OpenAthens` style
    /// `https://go.openathens.net/redirector/<org>?url=<target>`): hosts are not rewritten.
    Redirector,
}

/// A library proxy configured for the session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LibraryProxy {
    /// Proxy host, for example `ezproxy.lib.example.edu` or `go.openathens.net`.
    pub host: String,
    /// Rewriting scheme.
    pub kind: ProxyKind,
    /// Login path with the query name for the target, for example
    /// `/login?url=` or `/redirector/example.edu?url=`.
    pub login_path: String,
}

impl LibraryProxy {
    /// An `EZproxy` instance at `host` with the standard `/login?url=` entry point.
    pub fn ezproxy(host: &str) -> Self {
        Self {
            host: host.trim_end_matches('.').to_ascii_lowercase(),
            kind: ProxyKind::EzproxyHostname,
            login_path: "/login?url=".to_string(),
        }
    }

    /// A redirector that prefixes targets (`login_path` ends with the query name and `=`).
    pub fn redirector(host: &str, login_path: &str) -> Self {
        Self {
            host: host.trim_end_matches('.').to_ascii_lowercase(),
            kind: ProxyKind::Redirector,
            login_path: login_path.to_string(),
        }
    }

    /// `true` for the proxy host itself and any host under it.
    pub fn is_proxy_host(&self, host: &str) -> bool {
        host_in_domain(host, &self.host)
    }

    /// The publisher host a proxied hostname stands for; `None` when `host` is
    /// not under this proxy or is the proxy itself. Hyphenated names are
    /// mapped back by replacing every hyphen with a dot, which is what
    /// `EZproxy` does in reverse; a publisher host containing a real hyphen is
    /// therefore ambiguous and documented as such.
    pub fn unproxy_host(&self, host: &str) -> Option<String> {
        if self.kind != ProxyKind::EzproxyHostname {
            return None;
        }
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        let inner = host.strip_suffix(self.host.as_str())?.strip_suffix('.')?;
        if inner.is_empty() {
            return None;
        }
        if inner.contains('.') {
            return Some(inner.to_string());
        }
        Some(inner.replace('-', "."))
    }

    /// The URL that starts a proxied session for `target`
    /// (`https://ezproxy.lib.edu/login?url=https%3A%2F%2Fwww.nature.com%2F...`).
    pub fn proxied_url(&self, target: &NormalizedUrl) -> String {
        format!(
            "https://{}{}{}",
            self.host,
            self.login_path,
            percent_encode_component(&target.to_string())
        )
    }
}

/// Which hosts a session may load.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResearchPolicy {
    /// When `false` every host is allowed (an ordinary browser).
    pub enabled: bool,
    /// Extra domain suffixes allowed in addition to [`SCHOLARLY_HOSTS`]
    /// (institutional SSO hosts, a departmental server ...).
    pub extra_allow: Vec<String>,
    /// Domain suffixes refused even though they would otherwise be allowed.
    pub deny: Vec<String>,
    /// The library proxy, if the institution has one.
    pub proxy: Option<LibraryProxy>,
}

impl Default for ResearchPolicy {
    fn default() -> Self {
        Self::research()
    }
}

impl ResearchPolicy {
    /// Research mode: scholarly hosts only.
    pub fn research() -> Self {
        Self {
            enabled: true,
            extra_allow: Vec::new(),
            deny: Vec::new(),
            proxy: None,
        }
    }

    /// Open mode: everything is allowed; the proxy is still used for `Proxied` decisions.
    pub fn open() -> Self {
        Self {
            enabled: false,
            ..Self::research()
        }
    }

    /// Also allow `domain` (and its subdomains).
    #[must_use]
    pub fn allow(mut self, domain: &str) -> Self {
        self.extra_allow.push(normalize_domain(domain));
        self
    }

    /// Refuse `domain` (and its subdomains) even in open mode.
    #[must_use]
    pub fn refuse(mut self, domain: &str) -> Self {
        self.deny.push(normalize_domain(domain));
        self
    }

    /// Route through and recognise `proxy`.
    #[must_use]
    pub fn with_proxy(mut self, proxy: LibraryProxy) -> Self {
        self.proxy = Some(proxy);
        self
    }

    /// Judge a host. Denied suffixes win, checked against the raw host and,
    /// for a host under the library proxy, against the unwrapped publisher
    /// origin too, so a denied origin stays blocked when reached through the
    /// proxy. A proxied host is then allowed only when its origin is (the
    /// proxy host being allowed never vouches for arbitrary origins); other
    /// hosts are judged by the allowlists (only when `enabled`).
    pub fn decide(&self, host: &str) -> HostDecision {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        if self.is_denied(&host) {
            return HostDecision::Block;
        }
        if let Some(proxy) = &self.proxy
            && proxy.is_proxy_host(&host)
        {
            return match proxy.unproxy_host(&host) {
                Some(origin) if self.is_denied(&origin) => HostDecision::Block,
                Some(origin) if self.host_allowed(&origin) => HostDecision::Proxied { origin },
                // The proxy's own login/menu pages are always needed.
                None => HostDecision::Allow,
                Some(_) => HostDecision::Block,
            };
        }
        if self.host_allowed(&host) {
            HostDecision::Allow
        } else {
            HostDecision::Block
        }
    }

    /// `true` when `host` is, or is under, a denied domain (no proxy unwrapping).
    pub fn is_denied(&self, host: &str) -> bool {
        self.deny.iter().any(|d| host_in_domain(host, d))
    }

    /// `true` when `host` is allowed as itself (no proxy unwrapping, deny list ignored).
    pub fn host_allowed(&self, host: &str) -> bool {
        !self.enabled
            || is_scholarly_host(host)
            || self.extra_allow.iter().any(|d| host_in_domain(host, d))
    }
}

fn normalize_domain(domain: &str) -> String {
    domain
        .trim()
        .trim_start_matches('.')
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_matches_suffixes_only() {
        assert!(is_scholarly_host("doi.org"));
        assert!(is_scholarly_host("www.nature.com"));
        assert!(is_scholarly_host("pubmed.ncbi.nlm.nih.gov"));
        assert!(is_scholarly_host("EXPORT.ARXIV.ORG"));
        assert!(is_scholarly_host("dl.acm.org"));
        assert!(!is_scholarly_host("nature.com.evil.example"));
        assert!(!is_scholarly_host("example.com"));
        assert!(!is_scholarly_host("google.com"));
        assert!(!is_scholarly_host("acm.organic-food.shop"));
    }

    #[test]
    fn allowlist_entries_are_lowercase_bare_domains() {
        for entry in SCHOLARLY_HOSTS {
            assert_eq!(*entry, entry.to_ascii_lowercase(), "{entry}");
            assert!(!entry.starts_with('.') && !entry.ends_with('.'), "{entry}");
            assert!(entry.contains('.'), "{entry}");
        }
    }

    #[test]
    fn research_policy_blocks_unknown_hosts_and_open_does_not() {
        let research = ResearchPolicy::research();
        assert_eq!(
            research.decide("www.sciencedirect.com"),
            HostDecision::Allow
        );
        assert_eq!(research.decide("news.example.com"), HostDecision::Block);
        let open = ResearchPolicy::open();
        assert_eq!(open.decide("news.example.com"), HostDecision::Allow);
        assert_eq!(ResearchPolicy::default(), research);
    }

    #[test]
    fn extra_allow_and_deny() {
        let policy = ResearchPolicy::research()
            .allow(".SSO.University.EDU.")
            .refuse("mdpi.com");
        assert_eq!(
            policy.decide("login.sso.university.edu"),
            HostDecision::Allow
        );
        assert_eq!(policy.decide("www.mdpi.com"), HostDecision::Block);
        let open = ResearchPolicy::open().refuse("tracker.example");
        assert_eq!(open.decide("a.tracker.example"), HostDecision::Block);
    }

    #[test]
    fn ezproxy_hosts_are_unwrapped() {
        let proxy = LibraryProxy::ezproxy("EZproxy.lib.example.edu");
        assert!(proxy.is_proxy_host("www-nature-com.ezproxy.lib.example.edu"));
        assert!(!proxy.is_proxy_host("www.nature.com"));
        assert_eq!(
            proxy.unproxy_host("www-nature-com.ezproxy.lib.example.edu"),
            Some("www.nature.com".to_string())
        );
        assert_eq!(
            proxy.unproxy_host("www.nature.com.ezproxy.lib.example.edu"),
            Some("www.nature.com".to_string())
        );
        assert_eq!(proxy.unproxy_host("ezproxy.lib.example.edu"), None);
        assert_eq!(proxy.unproxy_host("www.nature.com"), None);

        let policy = ResearchPolicy::research().with_proxy(proxy);
        assert_eq!(
            policy.decide("www-nature-com.ezproxy.lib.example.edu"),
            HostDecision::Proxied {
                origin: "www.nature.com".to_string()
            }
        );
        assert_eq!(
            policy.decide("ezproxy.lib.example.edu"),
            HostDecision::Allow
        );
        assert_eq!(
            policy.decide("www-example-com.ezproxy.lib.example.edu"),
            HostDecision::Block
        );
    }

    #[test]
    fn deny_applies_to_the_unwrapped_proxy_origin() {
        let proxy = LibraryProxy::ezproxy("ezproxy.lib.edu");
        let open = ResearchPolicy::open()
            .refuse("example.com")
            .with_proxy(proxy.clone());
        assert_eq!(
            open.decide("www-example-com.ezproxy.lib.edu"),
            HostDecision::Block
        );
        assert_eq!(
            open.decide("www.example.com.ezproxy.lib.edu"),
            HostDecision::Block
        );
        assert_eq!(open.decide("www.example.com"), HostDecision::Block);
        assert_eq!(
            open.decide("www-nature-com.ezproxy.lib.edu"),
            HostDecision::Proxied {
                origin: "www.nature.com".to_string()
            }
        );
        assert_eq!(open.decide("ezproxy.lib.edu"), HostDecision::Allow);

        // Research mode: an explicitly allowed origin is still refused when denied.
        let research = ResearchPolicy::research()
            .allow("example.com")
            .refuse("example.com")
            .with_proxy(proxy);
        assert_eq!(
            research.decide("www-example-com.ezproxy.lib.edu"),
            HostDecision::Block
        );
        assert!(research.is_denied("www.example.com"));
        assert!(!research.is_denied("www.nature.com"));
    }

    #[test]
    fn proxied_login_url_encodes_target() {
        let proxy = LibraryProxy::ezproxy("ezproxy.lib.example.edu");
        let target = NormalizedUrl::parse("https://www.nature.com/articles/x?y=1").unwrap();
        assert_eq!(
            proxy.proxied_url(&target),
            "https://ezproxy.lib.example.edu/login?url=https%3A%2F%2Fwww.nature.com%2Farticles%2Fx%3Fy%3D1"
        );
        let redirector =
            LibraryProxy::redirector("go.openathens.net", "/redirector/example.edu?url=");
        assert!(
            redirector
                .proxied_url(&target)
                .starts_with("https://go.openathens.net/redirector/example.edu?url=https%3A")
        );
        assert_eq!(redirector.unproxy_host("x.go.openathens.net"), None);
    }
}
