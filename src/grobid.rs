//! Explicit GROBID server client. No default endpoint, retries, redirects,
//! deployment, or automatic PDF transfer. TEI is scholarly evidence, not a
//! claim of complete glyph coverage. The CLI runs this inside its bounded worker.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::{Cursor, Read, Seek, SeekFrom, Write as _};
use std::time::{Duration, Instant};

use roxmltree::{Document, Node, ParsingOptions};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

const TEI_NAMESPACE: &str = "http://www.tei-c.org/ns/1.0";

#[derive(Debug, Error)]
pub enum GrobidError {
    #[error("GROBID configuration: {0}")]
    Configuration(&'static str),
    #[error("GROBID resource limit: {0}")]
    Limit(&'static str),
    #[error("GROBID transport failed or timed out; check the configured server")]
    Transport,
    #[error("GROBID operation exceeded its deadline")]
    Timeout,
    #[error("GROBID input snapshot: {0}")]
    Input(#[from] std::io::Error),
    #[error("GROBID HTTP {0}; 204 means no structured content, 503 means server busy")]
    Http(u16),
    #[error("invalid GROBID TEI: {0}")]
    Tei(String),
}

/// Limits apply before sending and while reading/building output.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Options {
    pub timeout_ms: u64,
    pub max_input_bytes: Option<u64>,
    pub max_response_bytes: Option<usize>,
    /// 0: disabled; 1: metadata enrichment; 2: DOI-only enrichment.
    /// Any nonzero value authorizes the server's separate external lookups.
    pub consolidation: u8,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            timeout_ms: 60_000,
            max_input_bytes: None,
            max_response_bytes: None,
            consolidation: 0,
        }
    }
}
impl Options {
    pub fn validate(self) -> Result<Self, GrobidError> {
        if !(1..=300_000).contains(&self.timeout_ms)
            || self.max_input_bytes == Some(0)
            || self.max_response_bytes == Some(0)
            || self.consolidation > 2
        {
            return Err(GrobidError::Configuration(
                "invalid timeout, byte limit, or consolidation mode",
            ));
        }
        Ok(self)
    }
}

/// Credentials intentionally implement neither Debug nor Serialize.
pub struct Client {
    endpoint: String,
    bearer: Option<String>,
    options: Options,
    agent: ureq::Agent,
}
impl Client {
    pub fn from_env(options: Options) -> Result<Self, GrobidError> {
        let endpoint = std::env::var("TPE_GROBID_URL").map_err(|_| {
            GrobidError::Configuration("set TPE_GROBID_URL explicitly; no default server is used")
        })?;
        let bearer = match std::env::var("TPE_GROBID_BEARER_TOKEN") {
            Ok(value) => Some(value),
            Err(std::env::VarError::NotPresent) => None,
            Err(std::env::VarError::NotUnicode(_)) => {
                return Err(GrobidError::Configuration("bearer token must be ASCII"));
            }
        };
        Self::new(endpoint, bearer, options)
    }
    pub fn new(
        mut endpoint: String,
        bearer: Option<String>,
        options: Options,
    ) -> Result<Self, GrobidError> {
        let options = options.validate()?;
        if endpoint.len() > 4096 {
            return Err(GrobidError::Configuration("endpoint exceeds 4096 bytes"));
        }
        let uri: ureq::http::Uri = endpoint.parse().map_err(|_| {
            GrobidError::Configuration("endpoint must be an absolute HTTP(S) base URL")
        })?;
        if !matches!(uri.scheme_str(), Some("http" | "https"))
            || uri.host().is_none()
            || uri.authority().is_some_and(|a| a.as_str().contains('@'))
            || uri.query().is_some()
            || endpoint.contains('#')
        {
            return Err(GrobidError::Configuration(
                "endpoint cannot contain credentials, query parameters, or fragments",
            ));
        }
        if bearer.as_ref().is_some_and(|s| {
            s.is_empty() || s.len() > 8192 || !s.is_ascii() || s.bytes().any(|b| b < 32 || b == 127)
        }) {
            return Err(GrobidError::Configuration("invalid bearer token"));
        }
        let timeout = Duration::from_millis(options.timeout_ms);
        let agent = ureq::Agent::new_with_config(
            ureq::Agent::config_builder()
                .proxy(None)
                .max_redirects(0)
                .http_status_as_error(false)
                .timeout_global(Some(timeout))
                .timeout_resolve(Some(timeout))
                .timeout_connect(Some(timeout))
                .timeout_send_request(Some(timeout))
                .timeout_recv_response(Some(timeout))
                .timeout_recv_body(Some(timeout))
                .build(),
        );
        endpoint.truncate(endpoint.trim_end_matches('/').len());
        Ok(Self {
            endpoint,
            bearer,
            options,
            agent,
        })
    }
    pub fn process(&self, pdf: &[u8]) -> Result<GrobidDocument, GrobidError> {
        self.process_reader(Cursor::new(pdf))
    }
    /// Snapshot and upload a stream without placing the full PDF in memory.
    /// The private temporary file is removed on close or worker termination.
    pub fn process_reader(&self, mut input: impl Read) -> Result<GrobidDocument, GrobidError> {
        let started = Instant::now();
        let mut header = [0_u8; 5];
        input.read_exact(&mut header)?;
        if &header != b"%PDF-" {
            return Err(GrobidError::Configuration(
                "input must begin with a PDF header",
            ));
        }
        let mut snapshot = tempfile::tempfile()?;
        let mut digest = Sha256::new();
        let mut input_length = 0_u64;
        let mut buffer = vec![0_u8; 64 * 1024];
        let mut chunk: &[u8] = &header;
        loop {
            input_length =
                input_length
                    .checked_add(chunk.len() as u64)
                    .ok_or(GrobidError::Limit(
                        "input length exceeds addressable file size",
                    ))?;
            if self
                .options
                .max_input_bytes
                .is_some_and(|limit| input_length > limit)
            {
                return Err(GrobidError::Limit(
                    "input exceeds explicitly configured PDF byte limit",
                ));
            }
            if started.elapsed() >= Duration::from_millis(self.options.timeout_ms) {
                return Err(GrobidError::Timeout);
            }
            snapshot.write_all(chunk)?;
            digest.update(chunk);
            let count = input.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            chunk = &buffer[..count];
        }
        snapshot.seek(SeekFrom::Start(0))?;
        let source_sha256 = hex::encode(digest.finalize());
        let boundary = format!("tpe-grobid-{source_sha256}");
        let mut version_request = self.agent.get(format!("{}/api/version", self.endpoint));
        if let Some(token) = &self.bearer {
            version_request = version_request.header("Authorization", format!("Bearer {token}"));
        }
        let remaining = Duration::from_millis(self.options.timeout_ms)
            .checked_sub(started.elapsed())
            .ok_or(GrobidError::Timeout)?;
        let mut version_response = version_request
            .config()
            .timeout_global(Some(remaining))
            .build()
            .call()
            .map_err(|_| GrobidError::Transport)?;
        check_status(version_response.status().as_u16())?;
        let version = read_body(&mut version_response, Some(4096))?;
        let server_version = String::from_utf8(version)
            .map_err(|_| GrobidError::Configuration("server version is not UTF-8"))?;
        if server_version.trim().is_empty() {
            return Err(GrobidError::Configuration(
                "server returned an empty version",
            ));
        }
        let mut prefix = String::new();
        for name in [
            "consolidateHeader",
            "consolidateCitations",
            "consolidateFunders",
        ] {
            field(
                &mut prefix,
                &boundary,
                name,
                &self.options.consolidation.to_string(),
            );
        }
        for name in [
            "includeRawCitations",
            "includeRawAffiliations",
            "includeRawCopyrights",
            "generateIDs",
        ] {
            field(&mut prefix, &boundary, name, "1");
        }
        for name in [
            "head",
            "p",
            "s",
            "ref",
            "biblStruct",
            "figure",
            "formula",
            "title",
            "persName",
            "affiliation",
            "note",
        ] {
            field(&mut prefix, &boundary, "teiCoordinates", name);
        }
        let _ = write!(
            prefix,
            "--{boundary}\r\nContent-Disposition: form-data; name=\"input\"; filename=\"input.pdf\"\r\nContent-Type: application/pdf\r\n\r\n"
        );
        let suffix = format!("\r\n--{boundary}--\r\n");
        let content_length = input_length
            .checked_add(prefix.len() as u64)
            .and_then(|n| n.checked_add(suffix.len() as u64))
            .ok_or(GrobidError::Limit(
                "multipart length exceeds addressable file size",
            ))?;
        let mut body = Cursor::new(prefix.as_bytes())
            .chain(snapshot)
            .chain(Cursor::new(suffix.as_bytes()));
        let mut request = self
            .agent
            .post(format!("{}/api/processFulltextDocument", self.endpoint))
            .header("Accept", "application/xml")
            .header(
                "Content-Type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .header("Content-Length", content_length.to_string());
        if let Some(token) = &self.bearer {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        let remaining = Duration::from_millis(self.options.timeout_ms)
            .checked_sub(started.elapsed())
            .ok_or(GrobidError::Timeout)?;
        let mut response = request
            .config()
            .timeout_global(Some(remaining))
            .build()
            .send(ureq::SendBody::from_reader(&mut body))
            .map_err(|_| GrobidError::Transport)?;
        check_status(response.status().as_u16())?;
        let bytes = read_body(&mut response, self.options.max_response_bytes)?;
        let tei = String::from_utf8(bytes)
            .map_err(|_| GrobidError::Tei("response is not UTF-8".into()))?;
        let mut result = parse_tei(tei, self.options.max_response_bytes)?;
        result.source_sha256 = source_sha256;
        server_version.trim().clone_into(&mut result.server_version);
        result.endpoint.clone_from(&self.endpoint);
        result.options = self.options;
        Ok(result)
    }
}
fn field(out: &mut String, boundary: &str, name: &str, value: &str) {
    let _ = write!(
        out,
        "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
    );
}
fn check_status(status: u16) -> Result<(), GrobidError> {
    if status == 200 {
        Ok(())
    } else {
        Err(GrobidError::Http(status))
    }
}
fn read_body(
    response: &mut ureq::http::Response<ureq::Body>,
    limit: Option<usize>,
) -> Result<Vec<u8>, GrobidError> {
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(limit.map_or(u64::MAX, |n| (n as u64).saturating_add(1)))
        .read_to_end(&mut bytes)
        .map_err(|_| GrobidError::Transport)?;
    if limit.is_some_and(|limit| bytes.len() > limit) {
        return Err(GrobidError::Limit(
            "server response exceeds configured byte limit",
        ));
    }
    Ok(bytes)
}

/// Coordinates are original GROBID page points: upper-left origin, x/y/width/height.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Coordinate {
    pub page: u32,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Page {
    pub page: u32,
    pub width: f64,
    pub height: f64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Element {
    pub kind: String,
    pub section: Option<String>,
    pub text: String,
    pub attributes: BTreeMap<String, String>,
    /// UTF-8 byte range in `raw_tei`, retaining the exact source and nested structure.
    pub source_range: [usize; 2],
    pub coordinates: Vec<Coordinate>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Citation {
    pub xml_id: Option<String>,
    pub raw: Option<String>,
    pub titles: Vec<String>,
    pub authors: Vec<String>,
    pub identifiers: BTreeMap<String, Vec<String>>,
    /// Exact server date, publisher, place, and biblScope values; no inferred year.
    pub publication: BTreeMap<String, Vec<String>>,
    pub uris: Vec<String>,
    pub source_range: [usize; 2],
    pub coordinates: Vec<Coordinate>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct HeaderMetadata {
    pub titles: Vec<String>,
    pub authors: Vec<String>,
    pub identifiers: BTreeMap<String, Vec<String>>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GrobidDocument {
    pub source_sha256: String,
    pub endpoint: String,
    /// Actual /api/version response, including revision when reported by the server.
    pub server_version: String,
    pub options: Options,
    pub tei_sha256: String,
    pub raw_tei: String,
    pub coverage: String,
    pub pages: Vec<Page>,
    pub header: HeaderMetadata,
    pub elements: Vec<Element>,
    pub citations: Vec<Citation>,
    pub warnings: Vec<String>,
}

/// Non-resolving XML parser with an optional explicit byte/projection limit.
/// No network or filesystem entity loader. Use a process boundary for hard memory limits.
pub fn parse_tei(raw_tei: String, limit: Option<usize>) -> Result<GrobidDocument, GrobidError> {
    if limit.is_some_and(|limit| limit == 0 || raw_tei.len() > limit) {
        return Err(GrobidError::Limit("TEI exceeds configured byte limit"));
    }
    let doc = Document::parse_with_options(
        &raw_tei,
        ParsingOptions {
            allow_dtd: false,
            ..ParsingOptions::default()
        },
    )
    .map_err(|e| {
        let position = e.pos();
        GrobidError::Tei(format!(
            "malformed or forbidden XML at {}:{}",
            position.row, position.col
        ))
    })?;
    if !doc.root_element().has_tag_name((TEI_NAMESPACE, "TEI")) {
        return Err(GrobidError::Tei(
            "root must be TEI in the TEI namespace".into(),
        ));
    }
    let mut pages = Vec::new();
    let mut page_numbers = BTreeSet::new();
    let mut header = HeaderMetadata::default();
    let mut elements = Vec::new();
    let mut citations = Vec::new();
    let mut warnings = Vec::new();
    let mut budget = limit;
    for node in doc.descendants().filter(Node::is_element) {
        if node.tag_name().namespace() != Some(TEI_NAMESPACE) {
            continue;
        }
        let name = node.tag_name().name();
        let section = node
            .ancestors()
            .find(|p| matches!(p.tag_name().name(), "teiHeader" | "body" | "back" | "front"))
            .map(|p| p.tag_name().name().to_owned());
        if section.as_deref() == Some("teiHeader") {
            match name {
                "title" => header.titles.push(text(node, &mut budget)?),
                "persName" => header.authors.push(person_name(node, &mut budget)?),
                "idno" => header
                    .identifiers
                    .entry(node.attribute("type").unwrap_or("unknown").to_owned())
                    .or_default()
                    .push(text(node, &mut budget)?),
                _ => {}
            }
        }
        if name == "surface" {
            let page = node
                .attribute("n")
                .and_then(|s| s.parse::<u32>().ok())
                .filter(|n| *n > 0)
                .ok_or_else(|| GrobidError::Tei("surface has invalid page number".into()))?;
            let width = attr_number(node, "lrx")? - attr_number(node, "ulx")?;
            let height = attr_number(node, "lry")? - attr_number(node, "uly")?;
            if width <= 0.0 || height <= 0.0 || !page_numbers.insert(page) {
                return Err(GrobidError::Tei("invalid or duplicate page surface".into()));
            }
            pages.push(Page {
                page,
                width,
                height,
            });
        }
        if matches!(
            name,
            "head"
                | "p"
                | "s"
                | "ref"
                | "biblStruct"
                | "figure"
                | "table"
                | "formula"
                | "title"
                | "persName"
                | "affiliation"
                | "note"
                | "idno"
        ) {
            let text = text(node, &mut budget)?;
            let coordinates = coords(node.attribute("coords"))?;
            let range = node.range();
            let attributes = node
                .attributes()
                .map(|a| {
                    let name = a
                        .namespace()
                        .map_or_else(|| a.name().to_owned(), |ns| format!("{{{ns}}}{}", a.name()));
                    (name, a.value().to_owned())
                })
                .collect();
            elements.push(Element {
                kind: name.into(),
                section,
                text,
                attributes,
                source_range: [range.start, range.end],
                coordinates,
            });
        }
        if name == "biblStruct"
            && node
                .ancestors()
                .any(|p| p.has_tag_name((TEI_NAMESPACE, "listBibl")))
        {
            let range = node.range();
            let mut citation = Citation {
                xml_id: node
                    .attribute(("http://www.w3.org/XML/1998/namespace", "id"))
                    .map(str::to_owned),
                raw: None,
                titles: Vec::new(),
                authors: Vec::new(),
                identifiers: BTreeMap::new(),
                publication: BTreeMap::new(),
                uris: Vec::new(),
                source_range: [range.start, range.end],
                coordinates: coords(node.attribute("coords"))?,
            };
            for child in node.descendants().filter(Node::is_element) {
                if child.tag_name().namespace() != Some(TEI_NAMESPACE) {
                    continue;
                }
                match child.tag_name().name() {
                    "title" => citation.titles.push(text(child, &mut budget)?),
                    "persName" => citation.authors.push(person_name(child, &mut budget)?),
                    "idno" => citation
                        .identifiers
                        .entry(child.attribute("type").unwrap_or("unknown").to_owned())
                        .or_default()
                        .push(text(child, &mut budget)?),
                    "date" | "biblScope" | "publisher" | "pubPlace" => {
                        let key = if child.has_tag_name((TEI_NAMESPACE, "biblScope")) {
                            child.attribute("unit").unwrap_or("biblScope")
                        } else {
                            child.tag_name().name()
                        };
                        let value = if let Some(when) = child.attribute("when") {
                            charge(&mut budget, when.len())?;
                            when.to_owned()
                        } else {
                            text(child, &mut budget)?
                        };
                        citation
                            .publication
                            .entry(key.to_owned())
                            .or_default()
                            .push(value);
                    }
                    "ptr" | "ref" => {
                        if let Some(uri) = child.attribute("target") {
                            charge(&mut budget, uri.len())?;
                            citation.uris.push(uri.into());
                        }
                    }
                    "note" if child.attribute("type") == Some("raw_reference") => {
                        citation.raw = Some(text(child, &mut budget)?);
                    }
                    _ => {}
                }
            }
            citations.push(citation);
        }
    }
    if pages.is_empty() {
        warnings.push("server returned no page surfaces; no page geometry was inferred".into());
    }
    if elements
        .iter()
        .flat_map(|e| &e.coordinates)
        .any(|c| !page_numbers.contains(&c.page))
    {
        warnings.push("some coordinates refer to pages without a reported surface; coordinates retained without inferred dimensions".into());
    }
    if elements.is_empty() {
        return Err(GrobidError::Tei(
            "TEI has no scholarly text elements".into(),
        ));
    }
    warnings.push("semantic structure is not proof of complete PDF text or glyph coverage".into());
    let tei_sha256 = hex::encode(Sha256::digest(raw_tei.as_bytes()));
    Ok(GrobidDocument {
        source_sha256: String::new(),
        endpoint: String::new(),
        server_version: String::new(),
        options: Options::default(),
        tei_sha256,
        raw_tei,
        coverage: "semantic_projection".into(),
        pages,
        header,
        elements,
        citations,
        warnings,
    })
}
fn charge(budget: &mut Option<usize>, count: usize) -> Result<(), GrobidError> {
    if let Some(remaining) = budget {
        *remaining = remaining.checked_sub(count).ok_or(GrobidError::Limit(
            "TEI text projection exceeds explicitly configured byte budget",
        ))?;
    }
    Ok(())
}
fn text(node: Node<'_, '_>, budget: &mut Option<usize>) -> Result<String, GrobidError> {
    let mut value = String::new();
    for child in node.descendants().filter(Node::is_text) {
        let part = child.text().unwrap_or_default();
        charge(budget, part.len())?;
        value.push_str(part);
    }
    Ok(value.trim().to_owned())
}
fn person_name(node: Node<'_, '_>, budget: &mut Option<usize>) -> Result<String, GrobidError> {
    let mut parts = Vec::new();
    for child in node.children().filter(Node::is_element) {
        if matches!(
            child.tag_name().name(),
            "forename" | "surname" | "nameLink" | "genName"
        ) {
            parts.push(text(child, budget)?);
        }
    }
    if parts.is_empty() {
        text(node, budget)
    } else {
        charge(budget, parts.len().saturating_sub(1))?;
        Ok(parts.join(" "))
    }
}
fn attr_number(node: Node<'_, '_>, name: &str) -> Result<f64, GrobidError> {
    node.attribute(name)
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|n| n.is_finite() && n.abs() <= 1e8)
        .ok_or_else(|| GrobidError::Tei(format!("invalid {name} coordinate")))
}
fn coords(value: Option<&str>) -> Result<Vec<Coordinate>, GrobidError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let mut output = Vec::new();
    for group in value.split(';') {
        let values: Vec<_> = group.split(',').collect();
        if values.len() != 5 {
            return Err(GrobidError::Tei(
                "coordinate must have page,x,y,width,height".into(),
            ));
        }
        let page = values[0]
            .parse::<u32>()
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| GrobidError::Tei("invalid coordinate page".into()))?;
        let mut numbers = [0.0; 4];
        for (out, input) in numbers.iter_mut().zip(&values[1..]) {
            *out = input
                .parse::<f64>()
                .ok()
                .filter(|n| n.is_finite() && n.abs() <= 1e8)
                .ok_or_else(|| GrobidError::Tei("invalid finite coordinate".into()))?;
        }
        if numbers[2] < 0.0 || numbers[3] < 0.0 {
            return Err(GrobidError::Tei("negative coordinate extent".into()));
        }
        output.push(Coordinate {
            page,
            x: numbers[0],
            y: numbers[1],
            width: numbers[2],
            height: numbers[3],
        });
    }
    Ok(output)
}
