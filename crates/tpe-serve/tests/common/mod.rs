//! A real server on an ephemeral loopback port, and a small HTTP/1.1 client
//! written against the socket so tests control every header (`Host`,
//! `Origin`, duplicates, absolute-form targets) that a library client would
//! set for them.

#![allow(dead_code, clippy::missing_panics_doc, clippy::must_use_candidate)]

use std::fmt::Write as _;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use lopdf::content::{Content, Operation};
use lopdf::{Document, Object, Stream, dictionary};
use serde_json::Value;
use tpe_serve::{Config, RunningServer};

/// The engine's two-page synthetic paper (as the app's tests use).
pub const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../tpe-app/tests/fixtures/synthetic-paper.pdf"
);

/// A running server, its state directory, and a directory for PDFs.
pub struct Harness {
    pub server: RunningServer,
    pub state: tempfile::TempDir,
    pub files: tempfile::TempDir,
    /// `files`, resolved (the paths jobs report).
    pub root: PathBuf,
}

impl Harness {
    pub fn start() -> Self {
        Self::with(|_| {})
    }

    pub fn with(adjust: impl FnOnce(&mut Config)) -> Self {
        let state = tempfile::tempdir().unwrap();
        let files = tempfile::tempdir().unwrap();
        let mut config = Config::new(state.path().to_path_buf());
        config.port = 0;
        adjust(&mut config);
        let server = tpe_serve::spawn(config).expect("server starts");
        let root = std::fs::canonicalize(files.path()).unwrap();
        Self {
            server,
            state,
            files,
            root,
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.server.local_addr()
    }

    pub fn host(&self) -> String {
        format!("127.0.0.1:{}", self.addr().port())
    }

    pub fn origin(&self) -> String {
        format!("http://{}", self.host())
    }

    pub fn bearer(&self) -> String {
        format!("Bearer {}", self.server.token().expose())
    }

    /// A copy of the fixture paper at `root/<name>`.
    pub fn paper(&self, name: &str) -> PathBuf {
        let path = self.root.join(name);
        std::fs::copy(FIXTURE, &path).unwrap();
        path
    }

    /// A request with the right `Host` and token plus `extra` headers.
    pub fn call(
        &self,
        method: &str,
        path: &str,
        extra: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Reply {
        let host = self.host();
        let bearer = self.bearer();
        let mut headers = vec![("Host", host.as_str()), ("Authorization", bearer.as_str())];
        headers.extend_from_slice(extra);
        send(self.addr(), method, path, &headers, body)
    }

    pub fn get(&self, path: &str) -> Reply {
        self.call("GET", path, &[], None)
    }

    pub fn post_json(&self, path: &str, body: &Value) -> Reply {
        let bytes = serde_json::to_vec(body).unwrap();
        self.call(
            "POST",
            path,
            &[("Content-Type", "application/json")],
            Some(&bytes),
        )
    }

    /// Queue `paths` for `action` and return the job ids (asserting 201).
    pub fn submit(&self, action: &str, paths: &[&Path]) -> Vec<String> {
        let paths: Vec<&str> = paths.iter().map(|p| p.to_str().unwrap()).collect();
        let reply = self.post_json(
            "/v1/jobs",
            &serde_json::json!({"action": action, "paths": paths}),
        );
        assert_eq!(reply.status, 201, "{}", reply.text());
        reply.json()["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|job| job["id"].as_str().unwrap().to_string())
            .collect()
    }

    pub fn job(&self, id: &str) -> Value {
        let reply = self.get(&format!("/v1/jobs/{id}"));
        assert_eq!(reply.status, 200, "{}", reply.text());
        reply.json()
    }

    /// Poll until job `id` is in a final state (finished, failed, cancelled).
    pub fn wait(&self, id: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let job = self.job(id);
            if matches!(
                job["state"].as_str(),
                Some("finished" | "failed" | "cancelled")
            ) {
                return job;
            }
            assert!(Instant::now() < deadline, "job {id} did not finish: {job}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Open the event stream of job `id`.
    pub fn events(&self, id: &str, extra: &[(&str, &str)]) -> Events {
        let host = self.host();
        let bearer = self.bearer();
        let mut headers = vec![("Host", host.as_str()), ("Authorization", bearer.as_str())];
        headers.extend_from_slice(extra);
        Events::open(self.addr(), &format!("/v1/jobs/{id}/events"), &headers)
    }

    /// Names in the PDF directory, sorted.
    pub fn listing(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(&self.root)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}

/// A parsed response.
#[derive(Debug)]
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("not JSON ({e}): {}", self.text()))
    }

    /// `error.code` of an error body.
    pub fn code(&self) -> String {
        self.json()["error"]["code"]
            .as_str()
            .unwrap_or_else(|| panic!("not an error body: {}", self.text()))
            .to_string()
    }
}

/// Send one request (`Connection: close`) and read the whole response.
pub fn send(
    addr: SocketAddr,
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
    body: Option<&[u8]>,
) -> Reply {
    let mut head = format!("{method} {target} HTTP/1.1\r\n");
    for (name, value) in headers {
        let _ = write!(head, "{name}: {value}\r\n");
    }
    if let Some(body) = body {
        let _ = write!(head, "Content-Length: {}\r\n", body.len());
    }
    head.push_str("Connection: close\r\n\r\n");
    raw(addr, head.as_bytes(), body.unwrap_or_default())
}

/// Write `head` and `body` as they are and read the response to EOF.
pub fn raw(addr: SocketAddr, head: &[u8], body: &[u8]) -> Reply {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    // The server may answer (and close) before reading a refused body.
    let _ = stream.write_all(head).and_then(|()| stream.write_all(body));
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => bytes.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => break,
            Err(e) => panic!("reading the response: {e}"),
        }
    }
    parse(&bytes)
}

fn parse(bytes: &[u8]) -> Reply {
    let split = bytes
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or_else(|| panic!("no response head in {:?}", String::from_utf8_lossy(bytes)));
    let head = std::str::from_utf8(&bytes[..split]).unwrap();
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .unwrap()
        .split(' ')
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let headers: Vec<(String, String)> = lines
        .map(|line| {
            let (name, value) = line.split_once(':').unwrap();
            (name.to_string(), value.trim().to_string())
        })
        .collect();
    let rest = &bytes[split + 4..];
    let chunked = headers.iter().any(|(n, v)| {
        n.eq_ignore_ascii_case("transfer-encoding") && v.eq_ignore_ascii_case("chunked")
    });
    let body = if chunked {
        dechunk(rest)
    } else {
        rest.to_vec()
    };
    Reply {
        status,
        headers,
        body,
    }
}

fn dechunk(mut data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let Some(end) = data.windows(2).position(|w| w == b"\r\n") else {
            return out;
        };
        let size =
            usize::from_str_radix(std::str::from_utf8(&data[..end]).unwrap().trim(), 16).unwrap();
        if size == 0 {
            return out;
        }
        data = &data[end + 2..];
        out.extend_from_slice(&data[..size]);
        data = &data[size + 2..];
    }
}

/// One Server-Sent Event.
#[derive(Debug, Clone)]
pub struct Event {
    pub id: Option<u64>,
    /// The `event:` field.
    pub kind: String,
    pub data: String,
    /// When the client finished reading it.
    pub at: Instant,
}

impl Event {
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.data).unwrap()
    }
}

/// A client reading an event stream incrementally.
pub struct Events {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    reader: BufReader<TcpStream>,
    pending: Vec<u8>,
    ended: bool,
    /// Comment lines (keep-alives) seen.
    pub comments: usize,
}

impl Events {
    pub fn open(addr: SocketAddr, target: &str, headers: &[(&str, &str)]) -> Self {
        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(60)))
            .unwrap();
        let mut head = format!("GET {target} HTTP/1.1\r\n");
        for (name, value) in headers {
            let _ = write!(head, "{name}: {value}\r\n");
        }
        head.push_str("Accept: text/event-stream\r\n\r\n");
        stream.write_all(head.as_bytes()).unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let status = line.split(' ').nth(1).unwrap().parse().unwrap();
        let mut headers = Vec::new();
        loop {
            line.clear();
            reader.read_line(&mut line).unwrap();
            let trimmed = line.trim_end();
            if trimmed.is_empty() {
                break;
            }
            let (name, value) = trimmed.split_once(':').unwrap();
            headers.push((name.to_string(), value.trim().to_string()));
        }
        let chunked = headers.iter().any(|(n, v)| {
            n.eq_ignore_ascii_case("transfer-encoding") && v.eq_ignore_ascii_case("chunked")
        });
        let mut events = Self {
            status,
            headers,
            reader,
            pending: Vec::new(),
            ended: !chunked,
            comments: 0,
        };
        if !chunked {
            // An error body or an empty 204: keep it for inspection.
            let length = events
                .headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case("content-length"))
                .map_or(0, |(_, v)| v.parse().unwrap());
            let mut body = vec![0u8; length];
            events.reader.read_exact(&mut body).unwrap();
            events.pending = body;
        }
        events
    }

    /// The body of a non-streaming answer.
    pub fn body(&self) -> Value {
        serde_json::from_slice(&self.pending).unwrap()
    }

    /// The next event, or `None` when the stream ends.
    pub fn next(&mut self) -> Option<Event> {
        loop {
            if let Some(end) = self.pending.windows(2).position(|w| w == b"\n\n") {
                let block: Vec<u8> = self.pending.drain(..end + 2).collect();
                let block = String::from_utf8(block).unwrap();
                let mut event = Event {
                    id: None,
                    kind: String::new(),
                    data: String::new(),
                    at: Instant::now(),
                };
                let mut any = false;
                for line in block.lines() {
                    if let Some(value) = line.strip_prefix("id: ") {
                        event.id = Some(value.parse().unwrap());
                    } else if let Some(value) = line.strip_prefix("event: ") {
                        event.kind = value.to_string();
                        any = true;
                    } else if let Some(value) = line.strip_prefix("data: ") {
                        event.data = value.to_string();
                        any = true;
                    } else if line.starts_with(':') {
                        self.comments += 1;
                    }
                }
                if any {
                    return Some(event);
                }
                continue;
            }
            if self.ended {
                return None;
            }
            let mut size = String::new();
            if self.reader.read_line(&mut size).unwrap_or(0) == 0 {
                self.ended = true;
                continue;
            }
            let size = usize::from_str_radix(size.trim(), 16).unwrap();
            if size == 0 {
                self.ended = true;
                continue;
            }
            let mut chunk = vec![0u8; size + 2];
            self.reader.read_exact(&mut chunk).unwrap();
            chunk.truncate(size);
            self.pending.extend_from_slice(&chunk);
        }
    }

    /// Read events until one whose job is in a final state; return them all.
    pub fn until_final(&mut self) -> Vec<Event> {
        let mut seen = Vec::new();
        while let Some(event) = self.next() {
            let last = event.kind == "job"
                && matches!(
                    event.json()["state"].as_str(),
                    Some("finished" | "failed" | "cancelled")
                );
            seen.push(event);
            if last {
                break;
            }
        }
        seen
    }
}

/// A PDF of `pages` pages, each with a few lines of ordinary text, for jobs
/// that must run long enough to be stopped part-way.
pub fn long_pdf(pages: usize) -> Vec<u8> {
    let mut doc = Document::with_version("1.5");
    let tree_id = doc.new_object_id();
    let font_id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
        "Encoding" => "WinAnsiEncoding",
    });
    let resources_id = doc.add_object(dictionary! {
        "Font" => dictionary! { "F1" => font_id },
    });
    let mut kids = Vec::with_capacity(pages);
    for page in 1..=pages {
        let mut operations = Vec::new();
        for line in 0..12 {
            operations.push(Operation::new("BT", vec![]));
            operations.push(Operation::new(
                "Tf",
                vec![Object::Name(b"F1".to_vec()), Object::Real(10.0)],
            ));
            #[allow(clippy::cast_precision_loss)]
            let y = 720.0 - 14.0 * line as f32;
            operations.push(Operation::new(
                "Td",
                vec![Object::Real(60.0), Object::Real(y)],
            ));
            operations.push(Operation::new(
                "Tj",
                vec![Object::string_literal(format!(
                    "Page {page} line {line}: plain words for the extraction engine to order."
                ))],
            ));
            operations.push(Operation::new("ET", vec![]));
        }
        let content = Content { operations }.encode().unwrap();
        let content_id = doc.add_object(Stream::new(dictionary! {}, content));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => tree_id,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Resources" => resources_id,
            "Contents" => content_id,
        });
        kids.push(Object::Reference(page_id));
    }
    let count = i64::try_from(pages).unwrap();
    doc.objects.insert(
        tree_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => kids,
            "Count" => count,
        }),
    );
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => tree_id,
    });
    doc.trailer.set("Root", catalog_id);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    bytes
}
