#![cfg(feature = "grobid")]

use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tpe::grobid::{Client, GrobidError, Options, parse_tei};

const PDF: &[u8] = b"%PDF-1.7\nsynthetic-client-contract-fixture\n%%EOF";
const TEI: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<TEI xmlns="http://www.tei-c.org/ns/1.0">
 <teiHeader><fileDesc><titleStmt><title>Native structure</title></titleStmt>
 <sourceDesc><biblStruct><analytic><idno type="DOI">10.1234/paper</idno></analytic></biblStruct></sourceDesc>
 </fileDesc></teiHeader>
 <facsimile><surface n="1" ulx="0" uly="0" lrx="612" lry="792"/></facsimile>
 <text><body><div><head coords="1,20,30,180,14">Results</head>
 <p coords="1,20,50,500,40">Visible <ref type="url" target="https://doi.org/10.1234/raw" coords="1,20,50,45,12">citation</ref>.</p>
 </div></body><back><listBibl><biblStruct xml:id="b0" coords="1,20,700,500,30">
 <analytic><title level="a">Source title</title><author><persName><forename>Jane</forename><surname>Doe</surname></persName></author><idno type="DOI">10.1234/source</idno></analytic>
 <monogr><imprint><date when="2026"/><biblScope unit="volume">42</biblScope></imprint></monogr>
 <note type="raw_reference">Doe. Source title. 2026.</note><ptr target="https://publisher.test/source"/>
 </biblStruct></listBibl></back></text></TEI>"#;

struct Reply {
    status: u16,
    body: String,
    extra: String,
    delay: Duration,
}
impl Reply {
    fn new(status: u16, body: &str) -> Self {
        Self {
            status,
            body: body.into(),
            extra: String::new(),
            delay: Duration::ZERO,
        }
    }
}
fn mock_server(replies: Vec<Reply>) -> (String, JoinHandle<Vec<Vec<u8>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let handle = thread::spawn(move || {
        let mut requests = Vec::new();
        for reply in replies {
            let started = Instant::now();
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            started.elapsed() < Duration::from_secs(5),
                            "mock request did not arrive"
                        );
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("mock listener: {error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            requests.push(read_request(&mut stream));
            thread::sleep(reply.delay);
            let header = format!(
                "HTTP/1.1 {} status\r\nContent-Type: application/xml\r\nContent-Length: {}\r\nConnection: close\r\n{}\r\n",
                reply.status,
                reply.body.len(),
                reply.extra
            );
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(reply.body.as_bytes());
        }
        requests
    });
    (endpoint, handle)
}
fn read_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut data = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        let count = stream.read(&mut buffer).unwrap();
        assert!(count > 0, "short mock request");
        data.extend_from_slice(&buffer[..count]);
        assert!(data.len() < 128 * 1024, "oversized mock request");
        if let Some(end) = data.windows(4).position(|s| s == b"\r\n\r\n") {
            let header = String::from_utf8_lossy(&data[..end]);
            let length = header
                .lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case("Content-Length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            if data.len() >= end + 4 + length {
                return data;
            }
        }
    }
}

#[test]
fn configured_server_preserves_tei_citations_and_coordinates_without_consolidation() {
    let (endpoint, server) = mock_server(vec![Reply::new(200, "0.9.1"), Reply::new(200, TEI)]);
    let result = Client::new(
        endpoint.clone(),
        Some("public-test-token".into()),
        Options::default(),
    )
    .unwrap()
    .process(PDF)
    .unwrap();
    assert_eq!(result.endpoint, endpoint);
    assert_eq!(result.server_version, "0.9.1");
    assert_eq!(result.header.identifiers["DOI"], ["10.1234/paper"]);
    assert_eq!(result.citations[0].authors, ["Jane Doe"]);
    assert_eq!(result.citations[0].publication["date"], ["2026"]);
    assert_eq!(result.citations[0].publication["volume"], ["42"]);
    assert_eq!(result.raw_tei, TEI);
    assert_eq!(result.coverage, "semantic_projection");
    assert!((result.pages[0].width - 612.0).abs() < f64::EPSILON);
    assert_eq!(
        result.citations[0].raw.as_deref(),
        Some("Doe. Source title. 2026.")
    );
    assert_eq!(result.citations[0].identifiers["DOI"], ["10.1234/source"]);
    assert_eq!(result.citations[0].coordinates[0].page, 1);
    let link = result.elements.iter().find(|e| e.kind == "ref").unwrap();
    assert_eq!(link.text, "citation");
    assert_eq!(link.attributes["target"], "https://doi.org/10.1234/raw");
    assert!(result.raw_tei[link.source_range[0]..link.source_range[1]].starts_with("<ref"));
    let requests = server.join().unwrap();
    assert!(requests[0].starts_with(b"GET /api/version "));
    let request = String::from_utf8_lossy(&requests[1]);
    assert!(request.starts_with("POST /api/processFulltextDocument "));
    assert!(request.contains("Bearer public-test-token"));
    for name in [
        "consolidateHeader",
        "consolidateCitations",
        "consolidateFunders",
    ] {
        assert!(request.contains(&format!("name=\"{name}\"\r\n\r\n0\r\n")));
    }
    assert!(requests[1].windows(PDF.len()).any(|p| p == PDF));
}

#[test]
fn failures_and_redirects_never_become_success_or_trigger_retries() {
    for status in [204, 302, 500, 503] {
        let mut reply = Reply::new(status, "not a TEI result");
        reply.extra = "Location: http://127.0.0.1:1/never-transfer\r\n".into();
        let (endpoint, server) = mock_server(vec![Reply::new(200, "0.9.1"), reply]);
        let result = Client::new(endpoint, None, Options::default())
            .unwrap()
            .process(PDF);
        assert!(matches!(
            result,
            Err(GrobidError::Http(_) | GrobidError::Transport)
        ));
        assert_eq!(server.join().unwrap().len(), 2);
    }
}

#[test]
fn unavailable_version_endpoint_does_not_upload_pdf() {
    let (endpoint, server) = mock_server(vec![Reply::new(503, "busy")]);
    assert!(
        Client::new(endpoint, None, Options::default())
            .unwrap()
            .process(PDF)
            .is_err()
    );
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(!requests[0].windows(PDF.len()).any(|p| p == PDF));
}

#[test]
fn byte_and_time_limits_fail_closed() {
    let (endpoint, server) = mock_server(vec![
        Reply::new(200, "0.9.1"),
        Reply::new(200, &"x".repeat(1024)),
    ]);
    let options = Options {
        max_response_bytes: Some(128),
        ..Options::default()
    };
    assert!(matches!(
        Client::new(endpoint, None, options).unwrap().process(PDF),
        Err(GrobidError::Limit(_))
    ));
    server.join().unwrap();
    let mut slow = Reply::new(200, TEI);
    slow.delay = Duration::from_millis(250);
    let (endpoint, server) = mock_server(vec![Reply::new(200, "0.9.1"), slow]);
    let options = Options {
        timeout_ms: 40,
        ..Options::default()
    };
    let started = Instant::now();
    assert!(matches!(
        Client::new(endpoint, None, options).unwrap().process(PDF),
        Err(GrobidError::Transport)
    ));
    assert!(started.elapsed() < Duration::from_secs(2));
    server.join().unwrap();
}

#[test]
fn endpoint_secrets_invalid_input_and_non_tei_are_rejected() {
    for endpoint in [
        "http://user:secret@localhost:8070",
        "http://localhost:8070?token=secret",
        "http://localhost:8070#secret",
        "file:///tmp/grobid",
    ] {
        assert!(Client::new(endpoint.into(), None, Options::default()).is_err());
    }
    let client = Client::new(
        "http://127.0.0.1:1".into(),
        None,
        Options {
            max_input_bytes: Some(8),
            ..Options::default()
        },
    )
    .unwrap();
    assert!(matches!(client.process(PDF), Err(GrobidError::Limit(_))));
    assert!(parse_tei("<html>not TEI</html>".into(), Some(100)).is_err());
}

#[test]
fn xxe_declarations_and_invalid_coordinates_are_rejected() {
    let xxe = r#"<!DOCTYPE TEI [<!ENTITY secret SYSTEM "file:///etc/passwd">]><TEI xmlns="http://www.tei-c.org/ns/1.0"><text><p>&secret;</p></text></TEI>"#;
    assert!(parse_tei(xxe.into(), Some(4096)).is_err());
    assert!(
        parse_tei(
            TEI.replace("1,20,50,500,40", "1,NaN,50,500,40"),
            Some(16 * 1024)
        )
        .is_err()
    );
    assert!(
        parse_tei(
            TEI.replace("1,20,50,500,40", "0,20,50,500,40"),
            Some(16 * 1024)
        )
        .is_err()
    );
    assert!(
        parse_tei(
            TEI.replace("1,20,50,500,40", "1,20,50,-500,40"),
            Some(16 * 1024)
        )
        .is_err()
    );
}

#[test]
fn cli_uses_bounded_worker_and_does_not_modify_source_or_leak_token() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source.pdf");
    std::fs::write(&source, PDF).unwrap();
    let (endpoint, server) = mock_server(vec![Reply::new(200, "0.9.1"), Reply::new(200, TEI)]);
    let output = Command::new(env!("CARGO_BIN_EXE_tpe"))
        .args(["grobid", source.to_str().unwrap()])
        .env("TPE_GROBID_URL", endpoint)
        .env("TPE_GROBID_BEARER_TOKEN", "public-test-token")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(document["raw_tei"], TEI);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("public-test-token"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("public-test-token"));
    assert_eq!(std::fs::read(&source).unwrap(), PDF);
    assert_eq!(server.join().unwrap().len(), 2);
}

#[test]
fn cli_server_failure_is_nonzero_without_partial_json() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source.pdf");
    std::fs::write(&source, PDF).unwrap();
    let (endpoint, server) = mock_server(vec![Reply::new(200, "0.9.1"), Reply::new(503, "busy")]);
    let output = Command::new(env!("CARGO_BIN_EXE_tpe"))
        .args(["grobid", source.to_str().unwrap()])
        .env("TPE_GROBID_URL", endpoint)
        .env("TPE_GROBID_BEARER_TOKEN", "public-test-token")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("503"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("public-test-token"));
    assert_eq!(server.join().unwrap().len(), 2);
}

#[test]
fn default_streaming_input_and_tei_exceed_the_removed_size_caps() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        let (mut version, _) = listener.accept().unwrap();
        version
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        read_request(&mut version);
        version
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\n0.9.1")
            .unwrap();
        drop(version);
        let (mut upload, _) = listener.accept().unwrap();
        upload
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut prefix = Vec::new();
        let mut buffer = vec![0; 64 * 1024];
        let header_end = loop {
            let count = upload.read(&mut buffer).unwrap();
            assert!(count > 0);
            prefix.extend_from_slice(&buffer[..count]);
            if let Some(end) = prefix.windows(4).position(|s| s == b"\r\n\r\n") {
                break end + 4;
            }
            assert!(prefix.len() < 128 * 1024);
        };
        let headers = String::from_utf8_lossy(&prefix[..header_end]);
        let length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<u64>().unwrap())
            })
            .unwrap();
        assert!(length > 64 * 1024 * 1024);
        let mut received = (prefix.len() - header_end) as u64;
        while received < length {
            let count = upload.read(&mut buffer).unwrap();
            assert!(count > 0);
            received += count as u64;
        }
        let padding = " ".repeat(17 * 1024 * 1024);
        let tei = TEI.replace("</TEI>", &format!("<!--{padding}--></TEI>"));
        write!(
            upload,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            tei.len()
        )
        .unwrap();
        upload.write_all(tei.as_bytes()).unwrap();
    });
    let mut source = tempfile::tempfile().unwrap();
    source.write_all(b"%PDF-1.7\n").unwrap();
    source.set_len(65 * 1024 * 1024).unwrap();
    source.seek(SeekFrom::Start(0)).unwrap();
    let result = Client::new(endpoint, None, Options::default())
        .unwrap()
        .process_reader(source)
        .unwrap();
    assert!(result.raw_tei.len() > 16 * 1024 * 1024);
    assert!(result.options.max_input_bytes.is_none());
    assert!(result.options.max_response_bytes.is_none());
    server.join().unwrap();
}
