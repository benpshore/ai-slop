//! A small, embedded public-suffix check for cookie `Domain` attributes.
//!
//! This is a **curated subset**, not the full Mozilla Public Suffix List
//! (<https://publicsuffix.org/list/>). The rule is:
//!
//! - every single-label name (`com`, `uk`, `io` ...) is a public suffix;
//! - every entry of [`MULTI_LABEL_SUFFIXES`] is a public suffix;
//! - the public suffix of a domain is the longest of those that equals the
//!   domain or is a label-aligned suffix of it (falling back to the last label).
//!
//! A domain is *registrable* (may carry cookies for its subtree) only when it
//! has at least one label more than its public suffix: `ebi.ac.uk` is, `ac.uk`
//! and `uk` are not.
//!
//! To extend the list, add the lower-case suffix without leading or trailing
//! dots to [`MULTI_LABEL_SUFFIXES`] (the `list_entries_are_lowercase_multi_label`
//! test enforces the shape). Wildcard (`*.ck`) and exception (`!www.ck`) rules
//! of the full list are not supported; a host under such a suffix is treated by
//! the single-label fallback, which is more permissive than the real list.

use crate::url::host_in_domain;

/// Multi-label public suffixes commonly met in scholarly browsing: academic,
/// commercial and government second-level domains of countries with a
/// registry hierarchy, plus shared-hosting platforms where every subdomain
/// belongs to a different owner.
pub const MULTI_LABEL_SUFFIXES: &[&str] = &[
    // United Kingdom
    "ac.uk",
    "co.uk",
    "org.uk",
    "gov.uk",
    "ltd.uk",
    "me.uk",
    "net.uk",
    "nhs.uk",
    "plc.uk",
    "sch.uk",
    // Japan
    "ac.jp",
    "co.jp",
    "go.jp",
    "or.jp",
    "ne.jp",
    // Australia
    "com.au",
    "edu.au",
    "org.au",
    "gov.au",
    "net.au",
    "asn.au",
    // Brazil
    "com.br",
    "edu.br",
    "gov.br",
    "org.br",
    "net.br",
    // New Zealand
    "co.nz",
    "ac.nz",
    "govt.nz",
    "org.nz",
    "net.nz",
    // India
    "co.in",
    "ac.in",
    "edu.in",
    "gov.in",
    "res.in",
    "org.in",
    "net.in",
    // China
    "edu.cn",
    "com.cn",
    "gov.cn",
    "org.cn",
    "net.cn",
    "ac.cn",
    // South Africa
    "ac.za",
    "co.za",
    "gov.za",
    "org.za",
    // South Korea
    "ac.kr",
    "co.kr",
    "go.kr",
    "or.kr",
    // Other academic and commercial second levels
    "ac.at",
    "co.at",
    "ac.be",
    "ac.il",
    "co.il",
    "ac.ir",
    "ac.id",
    "co.id",
    "ac.th",
    "co.th",
    "edu.sg",
    "com.sg",
    "edu.hk",
    "com.hk",
    "edu.tw",
    "com.tw",
    "edu.mx",
    "com.mx",
    "edu.ar",
    "com.ar",
    "edu.tr",
    "com.tr",
    "edu.pl",
    "com.pl",
    "edu.my",
    "com.my",
    "edu.pk",
    "com.pk",
    "edu.ng",
    "com.ng",
    "edu.eg",
    "com.eg",
    "ac.ke",
    "co.ke",
    // Shared hosting platforms
    "github.io",
    "gitlab.io",
    "herokuapp.com",
    "cloudfront.net",
    "s3.amazonaws.com",
    "appspot.com",
    "blogspot.com",
    "netlify.app",
    "vercel.app",
    "pages.dev",
    "workers.dev",
    "azurewebsites.net",
    "firebaseapp.com",
    "web.app",
    "readthedocs.io",
    "wordpress.com",
];

/// The public suffix of `domain` (lower-cased, surrounding dots removed):
/// the longest [`MULTI_LABEL_SUFFIXES`] entry covering it, else its last label.
pub fn public_suffix(domain: &str) -> String {
    let domain = normalize(domain);
    let mut best: Option<&str> = None;
    for &entry in MULTI_LABEL_SUFFIXES {
        if host_in_domain(&domain, entry) && best.is_none_or(|b| entry.len() > b.len()) {
            best = Some(entry);
        }
    }
    if let Some(entry) = best {
        return entry.to_string();
    }
    domain
        .rsplit('.')
        .next()
        .map_or_else(String::new, str::to_string)
}

/// `true` when `domain` is itself a public suffix (`uk`, `ac.uk`, `github.io`).
pub fn is_public_suffix(domain: &str) -> bool {
    let domain = normalize(domain);
    !domain.is_empty() && public_suffix(&domain) == domain
}

/// `true` when `domain` has at least one label more than its public suffix,
/// so it may scope cookies (`ebi.ac.uk`, `www.example.com`).
pub fn is_registrable_or_below(domain: &str) -> bool {
    let domain = normalize(domain);
    !domain.is_empty() && !domain.split('.').any(str::is_empty) && !is_public_suffix(&domain)
}

fn normalize(domain: &str) -> String {
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
    fn list_entries_are_lowercase_multi_label() {
        for entry in MULTI_LABEL_SUFFIXES {
            assert_eq!(*entry, entry.to_ascii_lowercase(), "{entry}");
            assert!(!entry.starts_with('.') && !entry.ends_with('.'), "{entry}");
            assert!(entry.contains('.'), "{entry}");
        }
    }

    #[test]
    fn suffix_lookup_prefers_the_longest_entry() {
        assert_eq!(public_suffix("www.ebi.ac.uk"), "ac.uk");
        assert_eq!(public_suffix("bucket.s3.amazonaws.com"), "s3.amazonaws.com");
        assert_eq!(public_suffix("www.example.com"), "com");
        assert_eq!(public_suffix("User.GitHub.IO."), "github.io");
        assert_eq!(public_suffix("uk"), "uk");
        // Label-aligned only: `mac.uk` is not under `ac.uk`.
        assert_eq!(public_suffix("mac.uk"), "uk");
    }

    #[test]
    fn public_and_registrable_domains() {
        for suffix in [
            "uk",
            "ac.uk",
            "com",
            "github.io",
            "s3.amazonaws.com",
            ".co.jp.",
        ] {
            assert!(is_public_suffix(suffix), "{suffix}");
            assert!(!is_registrable_or_below(suffix), "{suffix}");
        }
        for domain in [
            "ebi.ac.uk",
            "www.ebi.ac.uk",
            "example.com",
            "user.github.io",
        ] {
            assert!(!is_public_suffix(domain), "{domain}");
            assert!(is_registrable_or_below(domain), "{domain}");
        }
        assert!(!is_public_suffix(""));
        assert!(!is_registrable_or_below(""));
        assert!(!is_registrable_or_below("a..example.com"));
    }
}
