//! Provider-neutral HTTP transport and endpoint policy.
//!
//! Endpoint validation and credential binding deliberately happen before a
//! request builder is created. Redirects are disabled, so those invariants
//! cannot be invalidated by `Location` headers.

use std::fmt;
use std::io::Read;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::time::Duration;

use thiserror::Error;
use ureq::unversioned::resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{DefaultConnector, NextTimeout};
use url::{Host, Url};

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
pub const TOTAL_TIMEOUT: Duration = Duration::from_secs(120);
pub const MAX_REQUEST_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_RESPONSE_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_ERROR_BYTES: u64 = 8 * 1024;

#[derive(Clone, Copy)]
struct Timeouts {
    connect: Duration,
    request: Duration,
    total: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderPolicy {
    OpenAi,
    Anthropic,
    Gemini,
    Ollama,
}

impl ProviderPolicy {
    fn fixed_origin(self) -> Option<&'static str> {
        match self {
            Self::OpenAi => Some("https://api.openai.com"),
            Self::Anthropic => Some("https://api.anthropic.com"),
            Self::Gemini => Some("https://generativelanguage.googleapis.com"),
            Self::Ollama => None,
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PolicyError {
    #[error("invalid provider endpoint")]
    InvalidEndpoint,
    #[error("endpoint scheme is not permitted")]
    Scheme,
    #[error("endpoint contains forbidden URL components")]
    UrlComponents,
    #[error("endpoint origin is not permitted for this provider")]
    Origin,
    #[error("endpoint host could not be resolved unambiguously")]
    Resolution,
    #[error("endpoint resolves to a non-public destination")]
    NonPublic,
    #[error("request exceeds the configured size limit")]
    RequestTooLarge,
    #[error("credential is not valid for this provider endpoint")]
    CredentialMismatch,
}

/// A structurally validated endpoint. Debug output intentionally omits its URL.
#[derive(Clone)]
pub struct ValidatedEndpoint {
    provider: ProviderPolicy,
    url: Url,
    origin: String,
    /// Ollama addresses are retained so sending cannot perform a second DNS lookup.
    resolved_addresses: Option<Vec<SocketAddr>>,
}

impl fmt::Debug for ValidatedEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ValidatedEndpoint")
            .field("provider", &self.provider)
            .field("url", &"[REDACTED]")
            .field("origin", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

impl ValidatedEndpoint {
    pub fn parse(provider: ProviderPolicy, input: &str) -> Result<Self, PolicyError> {
        Self::parse_with(provider, input, resolve)
    }

    fn parse_with<F>(
        provider: ProviderPolicy,
        input: &str,
        resolver: F,
    ) -> Result<Self, PolicyError>
    where
        F: FnOnce(&str, u16) -> Result<Vec<IpAddr>, PolicyError>,
    {
        let url = Url::parse(input).map_err(|_| PolicyError::InvalidEndpoint)?;
        if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
            return Err(PolicyError::UrlComponents);
        }
        let host = url.host().ok_or(PolicyError::InvalidEndpoint)?;
        let origin = url.origin().ascii_serialization();

        let resolved_addresses = if let Some(fixed) = provider.fixed_origin() {
            if url.scheme() != "https" || origin != fixed {
                return Err(if url.scheme() == "https" {
                    PolicyError::Origin
                } else {
                    PolicyError::Scheme
                });
            }
            None
        } else {
            let port = url
                .port_or_known_default()
                .ok_or(PolicyError::InvalidEndpoint)?;
            let ips = match host {
                Host::Ipv4(ip) => vec![IpAddr::V4(ip)],
                Host::Ipv6(ip) => vec![IpAddr::V6(ip)],
                Host::Domain(name) => resolver(name, port)?,
            };
            if ips.is_empty() {
                return Err(PolicyError::Resolution);
            }
            if url.scheme() == "http" {
                if !ips.iter().all(IpAddr::is_loopback) {
                    return Err(PolicyError::NonPublic);
                }
            } else if url.scheme() != "https" {
                return Err(PolicyError::Scheme);
            } else if ips.iter().any(|ip| !is_public(*ip)) {
                return Err(PolicyError::NonPublic);
            }
            // ureq's resolver has a fixed capacity of 16 addresses. Resolve only
            // once, then retain exactly the validated destinations for sending.
            let mut addresses: Vec<_> = ips
                .into_iter()
                .map(|ip| SocketAddr::new(ip, port))
                .collect();
            addresses.sort_unstable();
            addresses.dedup();
            addresses.truncate(16);
            Some(addresses)
        };
        Ok(Self {
            provider,
            url,
            origin,
            resolved_addresses,
        })
    }
}

#[derive(Debug)]
enum EndpointResolver {
    Default(DefaultResolver),
    Pinned(Vec<SocketAddr>),
}

impl Resolver for EndpointResolver {
    fn resolve(
        &self,
        uri: &ureq::http::Uri,
        config: &ureq::config::Config,
        timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, ureq::Error> {
        match self {
            Self::Default(resolver) => resolver.resolve(uri, config, timeout),
            Self::Pinned(addresses) => {
                let mut resolved = self.empty();
                for address in addresses {
                    resolved.push(*address);
                }
                Ok(resolved)
            }
        }
    }
}

fn resolve(host: &str, port: u16) -> Result<Vec<IpAddr>, PolicyError> {
    (host, port)
        .to_socket_addrs()
        .map(|addrs| addrs.map(|a| a.ip()).collect())
        .map_err(|_| PolicyError::Resolution)
}

fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_multicast())
        }
        IpAddr::V6(ip) => {
            let first = ip.segments()[0];
            !(ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                || first & 0xfe00 == 0xfc00
                || first & 0xffc0 == 0xfe80)
        }
    }
}

/// Secret material bound to both a provider and the endpoint origin.
pub struct BoundCredential {
    provider: ProviderPolicy,
    origin: String,
    value: String,
}

impl BoundCredential {
    pub fn new(endpoint: &ValidatedEndpoint, value: &str) -> Self {
        Self {
            provider: endpoint.provider,
            origin: endpoint.origin.clone(),
            value: value.to_owned(),
        }
    }
}

impl fmt::Debug for BoundCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("BoundCredential([REDACTED])")
    }
}

fn auth_headers(
    endpoint: &ValidatedEndpoint,
    credential: &BoundCredential,
) -> Result<Vec<(&'static str, String)>, PolicyError> {
    if credential.provider != endpoint.provider || credential.origin != endpoint.origin {
        return Err(PolicyError::CredentialMismatch);
    }
    Ok(match endpoint.provider {
        ProviderPolicy::OpenAi => vec![("authorization", format!("Bearer {}", credential.value))],
        ProviderPolicy::Anthropic => vec![
            ("x-api-key", credential.value.clone()),
            ("anthropic-version", "2023-06-01".into()),
        ],
        // Gemini supports this header; never put the key in a query string.
        ProviderPolicy::Gemini => vec![("x-goog-api-key", credential.value.clone())],
        ProviderPolicy::Ollama => Vec::new(),
    })
}

fn read_bounded(reader: impl Read, limit: u64) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    reader
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "transport response failed".to_owned())?;
    if bytes.len() as u64 > limit {
        return Err("transport response exceeded size limit".to_owned());
    }
    Ok(bytes)
}

/// Send JSON under the centralized limits. Errors contain no URL, headers, or body.
pub fn post_json(
    endpoint: &ValidatedEndpoint,
    credential: &BoundCredential,
    body: &str,
) -> Result<(u16, String), String> {
    post_json_with_timeouts(
        endpoint,
        credential,
        body,
        Timeouts {
            connect: CONNECT_TIMEOUT,
            request: REQUEST_TIMEOUT,
            total: TOTAL_TIMEOUT,
        },
    )
}

fn post_json_with_timeouts(
    endpoint: &ValidatedEndpoint,
    credential: &BoundCredential,
    body: &str,
    timeouts: Timeouts,
) -> Result<(u16, String), String> {
    if body.len() > MAX_REQUEST_BYTES {
        return Err(PolicyError::RequestTooLarge.to_string());
    }
    let headers = auth_headers(endpoint, credential).map_err(|e| e.to_string())?;
    let config_builder = ureq::Agent::config_builder()
        .timeout_connect(Some(timeouts.connect))
        .timeout_per_call(Some(timeouts.request))
        .timeout_global(Some(timeouts.total))
        .max_redirects(0)
        .http_status_as_error(false);
    let (config, resolver) = match &endpoint.resolved_addresses {
        Some(addresses) => (
            // Environment proxies would receive plaintext Ollama prompts and
            // resolve the hostname themselves, bypassing destination validation.
            config_builder.proxy(None).build(),
            EndpointResolver::Pinned(addresses.clone()),
        ),
        None => (
            config_builder.build(),
            EndpointResolver::Default(DefaultResolver::default()),
        ),
    };
    let agent = ureq::Agent::with_parts(config, DefaultConnector::default(), resolver);
    let mut request = agent
        .post(endpoint.url.as_str())
        .header("content-type", "application/json");
    for (name, value) in headers {
        request = request.header(name, value);
    }
    let mut response = request
        .send(body)
        .map_err(|_| "transport request failed".to_owned())?;
    let status = response.status().as_u16();
    let limit = if (200..300).contains(&status) {
        MAX_RESPONSE_BYTES
    } else {
        MAX_ERROR_BYTES
    };
    let bytes = read_bounded(response.body_mut().as_reader(), limit)?;
    let text =
        String::from_utf8(bytes).map_err(|_| "transport response was not UTF-8".to_owned())?;
    Ok((status, text))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolved(ips: &[IpAddr]) -> impl FnOnce(&str, u16) -> Result<Vec<IpAddr>, PolicyError> + '_ {
        move |_, _| Ok(ips.to_vec())
    }

    #[test]
    fn fixed_cloud_origins_only() {
        assert!(
            ValidatedEndpoint::parse(ProviderPolicy::OpenAi, "https://api.openai.com/v1/chat")
                .is_ok()
        );
        assert_eq!(
            ValidatedEndpoint::parse(ProviderPolicy::OpenAi, "https://evil.test/v1").unwrap_err(),
            PolicyError::Origin
        );
        assert_eq!(
            ValidatedEndpoint::parse(
                ProviderPolicy::Gemini,
                "https://generativelanguage.googleapis.com/v1/models#key"
            )
            .unwrap_err(),
            PolicyError::UrlComponents
        );
    }

    #[test]
    fn ollama_http_requires_unambiguous_loopback_resolution() {
        for ip in ["127.0.0.1", "::1"] {
            let ip = ip.parse().unwrap();
            assert!(
                ValidatedEndpoint::parse_with(
                    ProviderPolicy::Ollama,
                    "http://localhost:11434/api/chat",
                    resolved(&[ip])
                )
                .is_ok()
            );
        }
        let mixed = ["127.0.0.1".parse().unwrap(), "10.0.0.1".parse().unwrap()];
        assert_eq!(
            ValidatedEndpoint::parse_with(
                ProviderPolicy::Ollama,
                "http://localhost:11434",
                resolved(&mixed)
            )
            .unwrap_err(),
            PolicyError::NonPublic
        );

        let endpoint = ValidatedEndpoint::parse_with(
            ProviderPolicy::Ollama,
            "http://model.test:11434/api/chat",
            resolved(&["127.0.0.1".parse().unwrap()]),
        )
        .unwrap();
        assert_eq!(
            endpoint.resolved_addresses,
            Some(vec!["127.0.0.1:11434".parse().unwrap()])
        );
    }

    #[test]
    fn pinned_resolver_returns_only_validated_addresses() {
        let pinned: SocketAddr = "127.0.0.1:11434".parse().unwrap();
        let resolver = EndpointResolver::Pinned(vec![pinned]);
        let uri = "http://attacker-controlled.test:11434/api/chat"
            .parse()
            .unwrap();
        let config = ureq::config::Config::default();
        let addresses = resolver
            .resolve(
                &uri,
                &config,
                NextTimeout {
                    after: ureq::unversioned::transport::time::Duration::NotHappening,
                    reason: ureq::Timeout::Connect,
                },
            )
            .unwrap();
        assert_eq!(addresses.iter().copied().collect::<Vec<_>>(), vec![pinned]);
    }

    #[test]
    fn private_and_link_local_custom_destinations_are_rejected() {
        for ip in [
            "10.1.2.3",
            "192.168.1.1",
            "169.254.1.1",
            "fc00::1",
            "fe80::1",
        ] {
            let ip = ip.parse().unwrap();
            assert_eq!(
                ValidatedEndpoint::parse_with(
                    ProviderPolicy::Ollama,
                    "https://model.test/api",
                    resolved(&[ip])
                )
                .unwrap_err(),
                PolicyError::NonPublic
            );
        }
    }

    #[test]
    fn credentials_cannot_cross_provider_or_origin() {
        let openai =
            ValidatedEndpoint::parse(ProviderPolicy::OpenAi, "https://api.openai.com/v1").unwrap();
        let anthropic =
            ValidatedEndpoint::parse(ProviderPolicy::Anthropic, "https://api.anthropic.com/v1")
                .unwrap();
        let key = BoundCredential::new(&openai, "super-secret");
        assert_eq!(
            auth_headers(&anthropic, &key).unwrap_err(),
            PolicyError::CredentialMismatch
        );
        assert!(!format!("{openai:?} {key:?}").contains("super-secret"));
    }

    #[test]
    fn request_limit_is_enforced_before_io() {
        let endpoint =
            ValidatedEndpoint::parse(ProviderPolicy::OpenAi, "https://api.openai.com/v1").unwrap();
        let key = BoundCredential::new(&endpoint, "secret");
        assert!(
            post_json(&endpoint, &key, &"x".repeat(MAX_REQUEST_BYTES + 1))
                .unwrap_err()
                .contains("size limit")
        );
    }

    #[test]
    fn hostile_cross_origin_redirect_fails_policy_and_credential_binding() {
        let source =
            ValidatedEndpoint::parse(ProviderPolicy::OpenAi, "https://api.openai.com/v1/chat")
                .unwrap();
        let key = BoundCredential::new(&source, "secret");
        assert_eq!(
            ValidatedEndpoint::parse(ProviderPolicy::OpenAi, "https://evil.test/steal")
                .unwrap_err(),
            PolicyError::Origin
        );
        let other = ValidatedEndpoint::parse(
            ProviderPolicy::Anthropic,
            "https://api.anthropic.com/v1/messages",
        )
        .unwrap();
        assert_eq!(
            auth_headers(&other, &key).unwrap_err(),
            PolicyError::CredentialMismatch
        );
    }

    #[test]
    fn response_and_timeout_limits_are_explicit() {
        let oversized = vec![0_u8; usize::try_from(MAX_ERROR_BYTES).unwrap() + 1];
        assert_eq!(
            read_bounded(oversized.as_slice(), MAX_ERROR_BYTES).unwrap_err(),
            "transport response exceeded size limit"
        );
        let connect = std::hint::black_box(CONNECT_TIMEOUT);
        let request = std::hint::black_box(REQUEST_TIMEOUT);
        let total = std::hint::black_box(TOTAL_TIMEOUT);
        assert!(connect < total && request <= total);
    }
}
