//! Zotero Web API v3 client (blocking, over `ureq`).
//!
//! Verified against the Zotero Web API documentation (basics and write
//! requests pages, last updated 2026-07-29):
//! * base URL `https://api.zotero.org`, library prefix `/users/<id>` or
//!   `/groups/<id>`;
//! * `Zotero-API-Version: 3` request header, key in `Zotero-API-Key`;
//! * multi-object reads return `Total-Results`, `Last-Modified-Version` and a
//!   `Link` header with `rel="next"`; `limit` is 1–100 (default 25);
//! * `POST <prefix>/items` takes up to 50 objects and an optional
//!   32-character `Zotero-Write-Token` for unversioned writes; the 200
//!   response has `successful` / `success`, `unchanged` and `failed` maps
//!   keyed by the index in the uploaded array;
//! * `PATCH <prefix>/items/<key>` with `If-Unmodified-Since-Version`
//!   returns 204, or 412 when the item changed;
//! * 403 = bad key or privileges, 412/428 = version or write-token problems,
//!   429 = rate limited (`Retry-After`), `Backoff` may appear on any response.

use std::collections::BTreeMap;
use std::collections::hash_map::RandomState;
use std::fmt;
use std::fmt::Write as _;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::error::ZError;
use crate::headers::{link_next, parse_u64};
use crate::item::{ZItem, ZItemPatch};

/// Default API base URL.
pub const DEFAULT_BASE: &str = "https://api.zotero.org";
/// API version requested with every call.
pub const API_VERSION: &str = "3";
/// Largest `limit` the Web API accepts for multi-object reads.
pub const MAX_LIMIT: u32 = 100;
/// Longest pause honoured for a `Backoff` header between pages, in seconds.
const MAX_BACKOFF_SECS: u64 = 60;
/// Most objects one write request may carry.
pub const MAX_WRITE_ITEMS: usize = 50;
/// `User-Agent` sent by this client.
pub const USER_AGENT: &str = "text-processing-engine-zotero/0.1";

/// Which library a client talks to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Library {
    /// A user library (numeric user id, not the username).
    User(u64),
    /// A group library (numeric group id).
    Group(u64),
}

impl Library {
    /// URL prefix: `/users/<id>` or `/groups/<id>`.
    pub fn prefix(self) -> String {
        match self {
            Self::User(id) => format!("/users/{id}"),
            Self::Group(id) => format!("/groups/{id}"),
        }
    }
}

/// A Zotero API key. `Debug` never prints the value.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiKey(String);

impl ApiKey {
    /// Wrap a key obtained from the credential store.
    pub fn new(value: &str) -> Self {
        Self(value.trim().to_string())
    }

    /// The raw key, for the request header only.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKey(***)")
    }
}

/// Parameters of an items read. `None` fields are not sent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ItemQuery {
    /// Quick search (`q`), titles and creators by default.
    pub q: Option<String>,
    /// `itemType` search syntax, e.g. `journalArticle || preprint` or `-attachment`.
    pub item_type: Option<String>,
    /// Only objects modified after this library version (`since`).
    pub since: Option<u64>,
    /// Page size (clamped to 1..=100).
    pub limit: Option<u32>,
    /// Index of the first result (`start`).
    pub start: Option<u32>,
    /// Sort field (`dateModified`, `title`, `date`, ...).
    pub sort: Option<String>,
    /// Read `/items/top` (no child notes or attachments) instead of `/items`.
    pub top: bool,
}

/// One page of a multi-object read.
#[derive(Clone, Debug, PartialEq)]
pub struct Page<T> {
    /// The objects on this page.
    pub items: Vec<T>,
    /// `Total-Results` header.
    pub total_results: Option<u64>,
    /// `Last-Modified-Version` header (library version).
    pub last_modified_version: Option<u64>,
    /// URL of the next page from the `Link` header.
    pub next: Option<String>,
    /// `Backoff` header in seconds, when the server asked clients to slow down.
    pub backoff_secs: Option<u64>,
}

/// A collection in the library.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ZCollection {
    /// Collection key.
    pub key: String,
    /// Collection version.
    pub version: u64,
    /// Display name.
    pub name: String,
    /// Parent collection key (`None` for top-level collections).
    pub parent: Option<String>,
}

/// One object the server refused in a write request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WriteFailure {
    /// Object key, when the server echoed one.
    pub key: Option<String>,
    /// Per-object HTTP-style code.
    pub code: u16,
    /// Server message.
    pub message: String,
}

/// Outcome of a multi-object write, keyed by index in the uploaded array.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WriteResult {
    /// Index -> key of objects created or modified.
    pub successful: BTreeMap<usize, String>,
    /// Index -> key of objects that were already identical.
    pub unchanged: BTreeMap<usize, String>,
    /// Index -> failure details.
    pub failed: BTreeMap<usize, WriteFailure>,
    /// Library version assigned to the successful objects.
    pub last_modified_version: Option<u64>,
}

/// A response reduced to what the parsers need; lets them be tested on
/// recorded data without a network.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RawResponse {
    /// HTTP status code.
    pub status: u16,
    /// Header names and values in received order.
    pub headers: Vec<(String, String)>,
    /// Body text (empty for 204).
    pub body: String,
}

impl RawResponse {
    /// First header with this name (case-insensitive).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Convert a `ureq` response, reading the whole body.
    fn from_http(response: ureq::http::Response<ureq::Body>) -> Result<Self, ZError> {
        let (parts, mut body) = response.into_parts();
        let headers: Vec<(String, String)> = parts
            .headers
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|v| (name.as_str().to_string(), v.to_string()))
            })
            .collect();
        let text = body.read_to_string()?;
        Ok(Self {
            status: parts.status.as_u16(),
            headers,
            body: text,
        })
    }
}

/// Map error statuses to [`ZError`]; 2xx and 304 pass through.
pub fn check_status(raw: RawResponse) -> Result<RawResponse, ZError> {
    match raw.status {
        200..=299 | 304 => Ok(raw),
        403 => Err(ZError::Forbidden),
        404 => Err(ZError::NotFound(truncate(&raw.body, 300))),
        412 => Err(ZError::PreconditionFailed),
        428 => Err(ZError::PreconditionRequired),
        429 => Err(ZError::RateLimited {
            retry_after_secs: parse_u64(raw.header("Retry-After")),
        }),
        code => Err(ZError::Status {
            code,
            message: truncate(&raw.body, 300),
        }),
    }
}

/// Parse a multi-object items response (body + paging headers).
pub fn parse_items_page(raw: &RawResponse) -> Result<Page<ZItem>, ZError> {
    let items = if raw.status == 304 {
        Vec::new()
    } else {
        ZItem::parse_many(&raw.body)?
    };
    Ok(page_from(raw, items))
}

/// Parse a multi-object collections response body.
pub fn parse_collections(body: &str) -> Result<Vec<ZCollection>, ZError> {
    let value: Value = serde_json::from_str(body)?;
    let array = value
        .as_array()
        .ok_or_else(|| ZError::Parse("expected a JSON array of collections".to_string()))?;
    let mut out = Vec::with_capacity(array.len());
    for entry in array {
        let data = entry
            .get("data")
            .and_then(Value::as_object)
            .ok_or_else(|| ZError::Parse("collection without a data object".to_string()))?;
        let key = entry
            .get("key")
            .and_then(Value::as_str)
            .or_else(|| data.get("key").and_then(Value::as_str))
            .ok_or_else(|| ZError::Parse("collection without a key".to_string()))?;
        out.push(ZCollection {
            key: key.to_string(),
            version: entry.get("version").and_then(Value::as_u64).unwrap_or(0),
            name: data
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            // `parentCollection` is a key, or `false` for top-level collections.
            parent: data
                .get("parentCollection")
                .and_then(Value::as_str)
                .map(str::to_string),
        });
    }
    Ok(out)
}

/// Parse the 200 response of a multi-object write. Accepts both
/// `success` (index -> key) and `successful` (index -> saved object).
pub fn parse_write_result(
    body: &str,
    last_modified_version: Option<u64>,
) -> Result<WriteResult, ZError> {
    let value: Value = serde_json::from_str(body)?;
    let object = value
        .as_object()
        .ok_or_else(|| ZError::Parse("write response is not a JSON object".to_string()))?;
    let mut result = WriteResult {
        last_modified_version,
        ..WriteResult::default()
    };
    if let Some(map) = object.get("success").and_then(Value::as_object) {
        for (index, entry) in map {
            if let (Ok(i), Some(key)) = (index.parse::<usize>(), entry.as_str()) {
                result.successful.insert(i, key.to_string());
            }
        }
    }
    if let Some(map) = object.get("successful").and_then(Value::as_object) {
        for (index, entry) in map {
            let key = entry
                .get("key")
                .and_then(Value::as_str)
                .or_else(|| entry.as_str());
            if let (Ok(i), Some(key)) = (index.parse::<usize>(), key) {
                result
                    .successful
                    .entry(i)
                    .or_insert_with(|| key.to_string());
            }
        }
    }
    if let Some(map) = object.get("unchanged").and_then(Value::as_object) {
        for (index, entry) in map {
            if let (Ok(i), Some(key)) = (index.parse::<usize>(), entry.as_str()) {
                result.unchanged.insert(i, key.to_string());
            }
        }
    }
    if let Some(map) = object.get("failed").and_then(Value::as_object) {
        for (index, entry) in map {
            let Ok(i) = index.parse::<usize>() else {
                continue;
            };
            let code = entry
                .get("code")
                .and_then(Value::as_u64)
                .and_then(|c| u16::try_from(c).ok())
                .unwrap_or(0);
            result.failed.insert(
                i,
                WriteFailure {
                    key: entry.get("key").and_then(Value::as_str).map(str::to_string),
                    code,
                    message: entry
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                },
            );
        }
    }
    Ok(result)
}

/// JSON body for `POST <prefix>/items`; refuses more than 50 objects.
pub fn write_items_body(items: &[ZItemPatch]) -> Result<String, ZError> {
    if items.len() > MAX_WRITE_ITEMS {
        return Err(ZError::TooMany { count: items.len() });
    }
    let array: Vec<Value> = items.iter().map(ZItemPatch::to_json).collect();
    Ok(serde_json::to_string(&Value::Array(array))?)
}

/// Check that an object key is 8 ASCII alphanumerics, so it is safe to put in
/// a URL path. (Server-generated keys use `[23456789ABCDEFGHIJKLMNPQRSTUVWXYZ]`.)
pub fn validate_key(key: &str) -> Result<(), ZError> {
    if key.len() == 8 && key.bytes().all(|b| b.is_ascii_alphanumeric()) {
        Ok(())
    } else {
        Err(ZError::InvalidKey(key.to_string()))
    }
}

/// Percent-encode a query-string value (RFC 3986 unreserved characters kept).
pub fn encode_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// A client-generated 32-character hexadecimal `Zotero-Write-Token`. It only
/// has to be unique per request (the server uses it to drop duplicate
/// submissions), so the standard library's randomly keyed hasher suffices.
pub fn new_write_token() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let state = RandomState::new();
    let mut first = state.build_hasher();
    first.write_u128(nanos);
    first.write_u64(count);
    let high = first.finish();
    let mut second = state.build_hasher();
    second.write_u64(high);
    second.write_u64(count ^ 0x9E37_79B9_7F4A_7C15);
    let low = second.finish();
    format!("{high:016x}{low:016x}")
}

fn page_from<T>(raw: &RawResponse, items: Vec<T>) -> Page<T> {
    Page {
        items,
        total_results: parse_u64(raw.header("Total-Results")),
        last_modified_version: parse_u64(raw.header("Last-Modified-Version")),
        next: raw.header("Link").and_then(link_next),
        backoff_secs: parse_u64(raw.header("Backoff")),
    }
}

fn truncate(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

fn build_url(base: &str, params: &[(&str, String)]) -> String {
    if params.is_empty() {
        return base.to_string();
    }
    let joined: Vec<String> = params
        .iter()
        .map(|(name, value)| format!("{name}={}", encode_component(value)))
        .collect();
    format!("{base}?{}", joined.join("&"))
}

/// HTTP methods that carry a JSON body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WriteMethod {
    Post,
    Patch,
}

/// Blocking Zotero Web API v3 client for one library.
pub struct ZoteroClient {
    base: String,
    library: Library,
    key: Option<ApiKey>,
    agent: ureq::Agent,
    offline: bool,
}

impl fmt::Debug for ZoteroClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ZoteroClient")
            .field("base", &self.base)
            .field("library", &self.library)
            .field("key", &self.key)
            .field("offline", &self.offline)
            .finish_non_exhaustive()
    }
}

impl ZoteroClient {
    /// A client for `library` at [`DEFAULT_BASE`]. Status codes are handled
    /// by this crate (the agent does not turn 4xx/5xx into errors); the
    /// global timeout is 60 s.
    pub fn new(library: Library, key: Option<ApiKey>) -> Self {
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(60)))
            .user_agent(USER_AGENT)
            .build();
        Self {
            base: DEFAULT_BASE.to_string(),
            library,
            key,
            agent: ureq::Agent::new_with_config(config),
            offline: false,
        }
    }

    /// Use another base URL (for example a test server); trailing `/` removed.
    #[must_use]
    pub fn with_base(mut self, base: &str) -> Self {
        self.base = base.trim_end_matches('/').to_string();
        self
    }

    /// When `true`, every request fails with [`ZError::Offline`] before any
    /// network access.
    #[must_use]
    pub fn offline(mut self, offline: bool) -> Self {
        self.offline = offline;
        self
    }

    /// The library this client reads and writes.
    pub fn library(&self) -> Library {
        self.library
    }

    /// Absolute URL of a path inside the library (`path` starts with `/`).
    pub fn library_url(&self, path: &str) -> String {
        let base = &self.base;
        let prefix = self.library.prefix();
        format!("{base}{prefix}{path}")
    }

    /// URL of an items read with the given query.
    pub fn items_url(&self, query: &ItemQuery) -> String {
        let path = if query.top { "/items/top" } else { "/items" };
        let mut params: Vec<(&str, String)> = Vec::new();
        if let Some(q) = &query.q {
            params.push(("q", q.clone()));
        }
        if let Some(item_type) = &query.item_type {
            params.push(("itemType", item_type.clone()));
        }
        if let Some(since) = query.since {
            params.push(("since", since.to_string()));
        }
        if let Some(limit) = query.limit {
            params.push(("limit", limit.clamp(1, MAX_LIMIT).to_string()));
        }
        if let Some(start) = query.start {
            params.push(("start", start.to_string()));
        }
        if let Some(sort) = &query.sort {
            params.push(("sort", sort.clone()));
        }
        build_url(&self.library_url(path), &params)
    }

    /// URL of an attachment's file (`/items/<key>/file`). Downloading it
    /// needs the same `Zotero-API-Key` header; the API redirects to storage.
    pub fn attachment_file_url(&self, key: &str) -> Result<String, ZError> {
        validate_key(key)?;
        Ok(self.library_url(&format!("/items/{key}/file")))
    }

    /// One page of items.
    pub fn items(&self, query: &ItemQuery) -> Result<Page<ZItem>, ZError> {
        let raw = self.get(&self.items_url(query))?;
        parse_items_page(&raw)
    }

    /// The page after `page`, following its `Link: rel="next"` URL; `None`
    /// on the last page. Links outside the API base are refused so the key
    /// is never sent elsewhere.
    pub fn next_page(&self, page: &Page<ZItem>) -> Result<Option<Page<ZItem>>, ZError> {
        let Some(next) = &page.next else {
            return Ok(None);
        };
        self.check_same_origin(next)?;
        let raw = self.get(next)?;
        parse_items_page(&raw).map(Some)
    }

    /// One item by key.
    pub fn item(&self, key: &str) -> Result<ZItem, ZError> {
        validate_key(key)?;
        let raw = self.get(&self.library_url(&format!("/items/{key}")))?;
        ZItem::parse_one(&raw.body)
    }

    /// Child notes and attachments of an item (all pages).
    pub fn children(&self, key: &str) -> Result<Vec<ZItem>, ZError> {
        validate_key(key)?;
        let url = build_url(
            &self.library_url(&format!("/items/{key}/children")),
            &[("limit", MAX_LIMIT.to_string())],
        );
        let mut out = Vec::new();
        let mut next = Some(url);
        while let Some(url) = next {
            let raw = self.get(&url)?;
            out.extend(ZItem::parse_many(&raw.body)?);
            next = raw.header("Link").and_then(link_next);
            if let Some(link) = &next {
                self.check_same_origin(link)?;
                Self::honor_backoff(&raw);
            }
        }
        Ok(out)
    }

    /// Sleep for the server-requested `Backoff` (seconds, capped at
    /// [`MAX_BACKOFF_SECS`]) before the next request of a multi-page read.
    fn honor_backoff(raw: &RawResponse) {
        if let Some(secs) = parse_u64(raw.header("Backoff")).filter(|s| *s > 0) {
            std::thread::sleep(std::time::Duration::from_secs(secs.min(MAX_BACKOFF_SECS)));
        }
    }

    /// All collections in the library (all pages).
    pub fn collections(&self) -> Result<Vec<ZCollection>, ZError> {
        let url = build_url(
            &self.library_url("/collections"),
            &[("limit", MAX_LIMIT.to_string())],
        );
        let mut out = Vec::new();
        let mut next = Some(url);
        while let Some(url) = next {
            let raw = self.get(&url)?;
            out.extend(parse_collections(&raw.body)?);
            next = raw.header("Link").and_then(link_next);
            if let Some(link) = &next {
                self.check_same_origin(link)?;
                Self::honor_backoff(&raw);
            }
        }
        Ok(out)
    }

    /// Create (or, with `key` + `version` properties, update) up to 50
    /// items in one unversioned request guarded by a fresh
    /// `Zotero-Write-Token`. An empty slice makes no request.
    pub fn write_items(&self, items: &[ZItemPatch]) -> Result<WriteResult, ZError> {
        let body = write_items_body(items)?;
        if items.is_empty() {
            return Ok(WriteResult::default());
        }
        let token = new_write_token();
        let raw = self.send_json(
            WriteMethod::Post,
            &self.library_url("/items"),
            body,
            &[("Zotero-Write-Token", token)],
        )?;
        parse_write_result(&raw.body, parse_u64(raw.header("Last-Modified-Version")))
    }

    /// Partially update one item (`PATCH`) if it is still at `version`.
    /// Returns the new `Last-Modified-Version` when the server sends one;
    /// a changed item yields [`ZError::PreconditionFailed`].
    pub fn update_item(
        &self,
        key: &str,
        version: u64,
        patch: &ZItemPatch,
    ) -> Result<Option<u64>, ZError> {
        validate_key(key)?;
        let body = serde_json::to_string(&patch.to_json())?;
        let raw = self.send_json(
            WriteMethod::Patch,
            &self.library_url(&format!("/items/{key}")),
            body,
            &[("If-Unmodified-Since-Version", version.to_string())],
        )?;
        Ok(parse_u64(raw.header("Last-Modified-Version")))
    }

    /// Add an HTML child note to `parent`.
    pub fn create_note(&self, parent: &str, html: &str) -> Result<WriteResult, ZError> {
        validate_key(parent)?;
        self.write_items(&[ZItemPatch::note(parent, html)])
    }

    /// Add a `linked_url` attachment (a link, no file upload) to `parent`.
    pub fn add_attachment_link(
        &self,
        parent: &str,
        url: &str,
        title: &str,
    ) -> Result<WriteResult, ZError> {
        validate_key(parent)?;
        self.write_items(&[ZItemPatch::linked_url_attachment(parent, url, title)])
    }

    fn check_same_origin(&self, url: &str) -> Result<(), ZError> {
        let base = &self.base;
        if url.starts_with(&format!("{base}/")) {
            Ok(())
        } else {
            Err(ZError::ForeignLink(url.to_string()))
        }
    }

    fn ensure_online(&self) -> Result<(), ZError> {
        if self.offline {
            Err(ZError::Offline)
        } else {
            Ok(())
        }
    }

    fn get(&self, url: &str) -> Result<RawResponse, ZError> {
        self.ensure_online()?;
        let mut request = self
            .agent
            .get(url)
            .header("Zotero-API-Version", API_VERSION);
        if let Some(key) = &self.key {
            request = request.header("Zotero-API-Key", key.expose());
        }
        let response = request.call()?;
        check_status(RawResponse::from_http(response)?)
    }

    fn send_json(
        &self,
        method: WriteMethod,
        url: &str,
        body: String,
        extra_headers: &[(&str, String)],
    ) -> Result<RawResponse, ZError> {
        self.ensure_online()?;
        let Some(key) = &self.key else {
            return Err(ZError::MissingKey);
        };
        let mut request = match method {
            WriteMethod::Post => self.agent.post(url),
            WriteMethod::Patch => self.agent.patch(url),
        };
        request = request
            .header("Zotero-API-Version", API_VERSION)
            .header("Zotero-API-Key", key.expose())
            .content_type("application/json");
        for (name, value) in extra_headers {
            request = request.header(*name, value.as_str());
        }
        let response = request.send(body)?;
        check_status(RawResponse::from_http(response)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn raw(status: u16, headers: &[(&str, &str)], body: &str) -> RawResponse {
        RawResponse {
            status,
            headers: headers
                .iter()
                .map(|(n, v)| ((*n).to_string(), (*v).to_string()))
                .collect(),
            body: body.to_string(),
        }
    }

    const TWO_ITEMS: &str = r#"[
      {"key": "AAAA2222", "version": 5, "data": {"key": "AAAA2222", "version": 5,
        "itemType": "book", "title": "A Book", "publisher": "Press",
        "creators": [{"creatorType": "author", "firstName": "B", "lastName": "Author"}],
        "date": "1999", "tags": [], "collections": [], "relations": {}}},
      {"key": "BBBB3333", "version": 6, "data": {"key": "BBBB3333", "version": 6,
        "itemType": "attachment", "parentItem": "AAAA2222", "linkMode": "imported_file",
        "title": "Full Text PDF", "contentType": "application/pdf", "filename": "a.pdf",
        "tags": [], "relations": {}}}
    ]"#;

    #[test]
    fn items_page_reads_paging_headers() {
        let response = raw(
            200,
            &[
                ("total-results", "5040"),
                ("Last-Modified-Version", "1234"),
                (
                    "Link",
                    "<https://api.zotero.org/users/1/items?limit=2&start=2>; rel=\"next\", <https://api.zotero.org/users/1/items?limit=2&start=5038>; rel=\"last\"",
                ),
                ("Backoff", "30"),
            ],
            TWO_ITEMS,
        );
        let page = parse_items_page(&response).unwrap();
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.total_results, Some(5040));
        assert_eq!(page.last_modified_version, Some(1234));
        assert_eq!(page.backoff_secs, Some(30));
        assert_eq!(
            page.next.as_deref(),
            Some("https://api.zotero.org/users/1/items?limit=2&start=2")
        );
        let book = page.items[0].to_record();
        assert_eq!(book.venue.as_deref(), Some("Press"));
        assert_eq!(book.authors, vec!["B Author"]);
        assert_eq!(book.year, Some(1999));
        assert_eq!(page.items[1].parent_item(), Some("AAAA2222"));
    }

    #[test]
    fn not_modified_page_is_empty() {
        let page = parse_items_page(&raw(304, &[("Last-Modified-Version", "9")], "")).unwrap();
        assert!(page.items.is_empty());
        assert_eq!(page.last_modified_version, Some(9));
    }

    #[test]
    fn status_mapping() {
        assert!(matches!(
            check_status(raw(403, &[], "Forbidden")),
            Err(ZError::Forbidden)
        ));
        assert!(matches!(
            check_status(raw(412, &[], "")),
            Err(ZError::PreconditionFailed)
        ));
        assert!(matches!(
            check_status(raw(429, &[("Retry-After", "7")], "")),
            Err(ZError::RateLimited {
                retry_after_secs: Some(7)
            })
        ));
        assert!(matches!(
            check_status(raw(500, &[], "boom")),
            Err(ZError::Status { code: 500, .. })
        ));
        assert!(check_status(raw(204, &[], "")).is_ok());
    }

    #[test]
    fn urls_are_built_from_the_query() {
        let client = ZoteroClient::new(Library::Group(42), None).offline(true);
        let query = ItemQuery {
            q: Some("deep learning".to_string()),
            item_type: Some("journalArticle || preprint".to_string()),
            since: Some(10),
            limit: Some(500),
            start: Some(100),
            sort: Some("dateModified".to_string()),
            top: true,
        };
        assert_eq!(
            client.items_url(&query),
            "https://api.zotero.org/groups/42/items/top?q=deep%20learning\
             &itemType=journalArticle%20%7C%7C%20preprint&since=10&limit=100&start=100\
             &sort=dateModified"
        );
        assert_eq!(
            client.attachment_file_url("ABCD2345").unwrap(),
            "https://api.zotero.org/groups/42/items/ABCD2345/file"
        );
        assert!(matches!(
            client.attachment_file_url("../x"),
            Err(ZError::InvalidKey(_))
        ));
        assert_eq!(
            ZoteroClient::new(Library::User(7), None).items_url(&ItemQuery::default()),
            "https://api.zotero.org/users/7/items"
        );
    }

    #[test]
    fn offline_client_never_touches_the_network() {
        let client = ZoteroClient::new(Library::User(1), Some(ApiKey::new("secret"))).offline(true);
        assert!(matches!(
            client.items(&ItemQuery::default()),
            Err(ZError::Offline)
        ));
        assert!(matches!(client.item("ABCD2345"), Err(ZError::Offline)));
        assert!(matches!(client.collections(), Err(ZError::Offline)));
        assert!(matches!(
            client.create_note("ABCD2345", "<p>x</p>"),
            Err(ZError::Offline)
        ));
        assert!(matches!(
            client.update_item("ABCD2345", 3, &ZItemPatch::new()),
            Err(ZError::Offline)
        ));
    }

    #[test]
    fn writes_need_a_key_and_at_most_fifty_items() {
        let client = ZoteroClient::new(Library::User(1), None);
        // No key: refused before any request is built.
        assert!(matches!(
            client.create_note("ABCD2345", "x"),
            Err(ZError::MissingKey)
        ));
        let many = vec![ZItemPatch::new(); 51];
        assert!(matches!(
            write_items_body(&many),
            Err(ZError::TooMany { count: 51 })
        ));
        assert_eq!(write_items_body(&[]).unwrap(), "[]");
        assert!(client.write_items(&[]).unwrap().successful.is_empty());
    }

    #[test]
    fn write_body_is_a_json_array_of_patches() {
        let body = write_items_body(&[
            ZItemPatch::note("ABCD2345", "<p>n</p>"),
            ZItemPatch::linked_url_attachment("ABCD2345", "https://x.org/a.pdf", "A"),
        ])
        .unwrap();
        let value: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value[0]["itemType"], json!("note"));
        assert_eq!(value[1]["linkMode"], json!("linked_url"));
    }

    #[test]
    fn write_result_parses_documented_shapes() {
        // Shape from "Creating Multiple Objects" plus the `success` map.
        let body = r#"{
          "successful": {"0": {"key": "AAAA2222", "version": 10, "data": {}},
                         "2": {"key": "CCCC4444", "version": 10, "data": {}}},
          "success": {"0": "AAAA2222", "2": "CCCC4444"},
          "unchanged": {"4": "EEEE6666"},
          "failed": {"1": {"key": "BBBB3333", "code": 400, "message": "Invalid field"},
                     "3": {"code": 413, "message": "Too large"}}
        }"#;
        let result = parse_write_result(body, Some(10)).unwrap();
        assert_eq!(
            result.successful.get(&0).map(String::as_str),
            Some("AAAA2222")
        );
        assert_eq!(
            result.successful.get(&2).map(String::as_str),
            Some("CCCC4444")
        );
        assert_eq!(
            result.unchanged.get(&4).map(String::as_str),
            Some("EEEE6666")
        );
        assert_eq!(result.failed[&1].code, 400);
        assert_eq!(result.failed[&1].key.as_deref(), Some("BBBB3333"));
        assert_eq!(result.failed[&3].key, None);
        assert_eq!(result.last_modified_version, Some(10));
    }

    #[test]
    fn collections_parse_parent_false_and_key() {
        let body = r#"[
          {"key": "COLL2345", "version": 3, "data": {"key": "COLL2345", "version": 3,
            "name": "Reading", "parentCollection": false, "relations": {}}},
          {"key": "SUBC2345", "version": 4, "data": {"key": "SUBC2345", "version": 4,
            "name": "Sub", "parentCollection": "COLL2345", "relations": {}}}
        ]"#;
        let collections = parse_collections(body).unwrap();
        assert_eq!(collections[0].parent, None);
        assert_eq!(collections[1].parent.as_deref(), Some("COLL2345"));
        assert_eq!(collections[1].name, "Sub");
    }

    #[test]
    fn write_tokens_are_32_hex_and_distinct() {
        let a = new_write_token();
        let b = new_write_token();
        assert_eq!(a.len(), 32);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn api_key_debug_is_redacted() {
        let client = ZoteroClient::new(
            Library::User(1),
            Some(ApiKey::new("P9NiFoyLeZu2bZNvvuQPDWsd")),
        );
        let text = format!("{client:?}");
        assert!(text.contains("ApiKey(***)"));
        assert!(!text.contains("P9Ni"));
    }

    #[test]
    fn foreign_next_links_are_refused() {
        let client = ZoteroClient::new(Library::User(1), None);
        let page = Page::<ZItem> {
            items: Vec::new(),
            total_results: None,
            last_modified_version: None,
            next: Some("https://evil.example/users/1/items".to_string()),
            backoff_secs: None,
        };
        assert!(matches!(
            client.next_page(&page),
            Err(ZError::ForeignLink(_))
        ));
    }
}
