//! The checks every request passes before it reaches an endpoint, in this
//! order: `Host` (DNS rebinding), `Origin` and `Sec-Fetch-Site` (other web
//! pages), then the bearer token (everything but `GET /v1/health`).

use http::header::{AUTHORIZATION, HOST, ORIGIN};
use http::{HeaderMap, HeaderName, Uri};

use crate::error::ApiError;
use crate::token::Token;

/// `Sec-Fetch-Site`, sent by current browsers on every request they make.
const SEC_FETCH_SITE: HeaderName = HeaderName::from_static("sec-fetch-site");

pub struct Guard {
    /// `127.0.0.1:<port>`, `[::1]:<port>`, `localhost:<port>`.
    hosts: [String; 3],
    /// The same three as `http://` origins, then any extra ones.
    origins: Vec<String>,
    token: Token,
}

/// The one value of header `name`; `Err` when there are several.
fn single<'a>(headers: &'a HeaderMap, name: &HeaderName) -> Result<Option<&'a [u8]>, ()> {
    let mut values = headers.get_all(name).iter();
    match (values.next(), values.next()) {
        (None, _) => Ok(None),
        (Some(value), None) => Ok(Some(value.as_bytes())),
        (Some(_), Some(_)) => Err(()),
    }
}

impl Guard {
    /// `extra_origins` must already be normalised
    /// (`config::normalize_origin`).
    pub fn new(port: u16, extra_origins: &[String], token: Token) -> Self {
        let hosts = [
            format!("127.0.0.1:{port}"),
            format!("[::1]:{port}"),
            format!("localhost:{port}"),
        ];
        let mut origins: Vec<String> = hosts.iter().map(|h| format!("http://{h}")).collect();
        origins.extend(extra_origins.iter().cloned());
        Self {
            hosts,
            origins,
            token,
        }
    }

    /// DNS rebinding: a page on `evil.example` that re-resolves its own name
    /// to 127.0.0.1 still sends `Host: evil.example`, so only the server's
    /// own names pass. The request target must be in origin form (`/v1/…`),
    /// since an absolute-form target (`http://evil.example/v1/…`) overrides
    /// `Host` (RFC 9112 §3.2.2).
    pub fn check_host(&self, uri: &Uri, headers: &HeaderMap) -> Result<(), ApiError> {
        if uri.scheme().is_some() || uri.authority().is_some() {
            return Err(ApiError::bad_host());
        }
        match single(headers, &HOST) {
            Ok(Some(host))
                if self
                    .hosts
                    .iter()
                    .any(|h| h.as_bytes().eq_ignore_ascii_case(host)) =>
            {
                Ok(())
            }
            _ => Err(ApiError::bad_host()),
        }
    }

    /// Other web pages: a browser names the page's origin in `Origin` on
    /// every cross-origin request and on same-origin non-GET ones; only the
    /// server's own origin (and configured extras) pass, and `null` (sandboxed
    /// frames, `file:` pages, redirects) never does. A request with no
    /// `Origin` comes from a non-browser client or is a same-origin GET.
    /// `Sec-Fetch-Site` covers requests where a browser sends no `Origin`
    /// (a no-CORS `<img>` or `<script>` load, a top-level navigation from a
    /// link): `cross-site` and `same-site` (another port on localhost) are
    /// refused.
    pub fn check_origin(&self, headers: &HeaderMap) -> Result<(), ApiError> {
        match single(headers, &ORIGIN) {
            Ok(None) => {}
            Ok(Some(origin))
                if self
                    .origins
                    .iter()
                    .any(|o| o.as_bytes().eq_ignore_ascii_case(origin)) => {}
            _ => return Err(ApiError::bad_origin()),
        }
        match single(headers, &SEC_FETCH_SITE) {
            Ok(None) => Ok(()),
            Ok(Some(site)) if site == b"same-origin" || site == b"none" => Ok(()),
            _ => Err(ApiError::bad_origin()),
        }
    }

    /// CSRF: the token travels only in `Authorization`, which a browser never
    /// adds by itself (no cookies, no query string), so a page cannot make a
    /// request with the user's credentials attached.
    pub fn check_auth(&self, headers: &HeaderMap) -> Result<(), ApiError> {
        let Ok(Some(value)) = single(headers, &AUTHORIZATION) else {
            return Err(ApiError::unauthorized());
        };
        let presented = match value.split_at_checked(7) {
            Some((scheme, rest)) if scheme.eq_ignore_ascii_case(b"bearer ") => rest,
            _ => return Err(ApiError::unauthorized()),
        };
        if self.token.matches(presented) {
            Ok(())
        } else {
            Err(ApiError::unauthorized())
        }
    }
}
