//! The API end to end: a real server on an ephemeral loopback port and a
//! real HTTP/1.1 client on a socket.

mod common;

use std::collections::BTreeSet;
use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::time::{Duration, Instant};

use common::{Harness, raw, send};
use serde_json::{Value, json};
use tpe_serve::routes::{OPENAPI, ROUTES};

// ---------------------------------------------------------------- health, auth

#[test]
fn health_needs_no_token_and_says_only_ok() {
    let h = Harness::start();
    let host = h.host();
    let reply = send(h.addr(), "GET", "/v1/health", &[("Host", &host)], None);
    assert_eq!(reply.status, 200);
    assert_eq!(reply.body, br#"{"ok":true}"#);
    assert_eq!(reply.header("content-type"), Some("application/json"));
    assert_eq!(reply.header("cache-control"), Some("no-store"));
    assert_eq!(reply.header("x-content-type-options"), Some("nosniff"));
}

#[test]
fn every_other_endpoint_needs_the_exact_token() {
    let h = Harness::start();
    let host = h.host();
    let token = h.server.token().expose().to_string();
    let wrong = format!("Bearer {}", "0".repeat(64));
    let upper = format!("Bearer {}", token.to_uppercase());
    let short = format!("Bearer {}", &token[..63]);
    let basic = format!("Basic {token}");
    let bare = token.clone();
    for (method, target) in [
        ("GET", "/v1/version"),
        ("GET", "/v1/jobs"),
        ("GET", "/v1/openapi.json"),
        ("POST", "/v1/jobs"),
        ("GET", "/v1/nothing-here"),
    ] {
        let no_token = send(h.addr(), method, target, &[("Host", &host)], None);
        assert_eq!(no_token.status, 401, "{method} {target}");
        assert_eq!(no_token.code(), "unauthorized");
        assert_eq!(no_token.header("www-authenticate"), Some("Bearer"));
        for value in [&wrong, &upper, &short, &basic, &bare] {
            let reply = send(
                h.addr(),
                method,
                target,
                &[("Host", &host), ("Authorization", value)],
                None,
            );
            assert_eq!(reply.status, 401, "{method} {target} with {value:?}");
        }
    }
    // Never in the query string, under any name.
    for query in ["token", "access_token", "auth"] {
        let reply = send(
            h.addr(),
            "GET",
            &format!("/v1/jobs?{query}={token}"),
            &[("Host", &host)],
            None,
        );
        assert_eq!(reply.status, 401, "{query}");
    }
    // Two Authorization headers, even with the right token first.
    let right = h.bearer();
    let doubled = send(
        h.addr(),
        "GET",
        "/v1/jobs",
        &[
            ("Host", &host),
            ("Authorization", &right),
            ("Authorization", &wrong),
        ],
        None,
    );
    assert_eq!(doubled.status, 401);
    // The scheme is case-insensitive (RFC 9110 §11.1); the token is not.
    let lower = format!("bearer {token}");
    let reply = send(
        h.addr(),
        "GET",
        "/v1/jobs",
        &[("Host", &host), ("Authorization", &lower)],
        None,
    );
    assert_eq!(reply.status, 200);
    assert_eq!(h.get("/v1/jobs").json(), json!({"jobs": []}));
}

// ------------------------------------------------------------- host and origin

#[test]
fn a_rebound_or_foreign_host_is_refused_even_with_the_token() {
    let h = Harness::start();
    let port = h.addr().port();
    let bearer = h.bearer();
    let refused = [
        "evil.example".to_string(),
        format!("evil.example:{port}"),
        format!("127.0.0.1:{}", port.wrapping_add(1)),
        "127.0.0.1".to_string(),
        format!("localhost.evil.example:{port}"),
        format!("127.0.0.2:{port}"),
        format!("0.0.0.0:{port}"),
        String::new(),
    ];
    for host in &refused {
        let reply = send(
            h.addr(),
            "GET",
            "/v1/jobs",
            &[("Host", host), ("Authorization", &bearer)],
            None,
        );
        assert_eq!(reply.status, 403, "Host {host:?}");
        assert_eq!(reply.code(), "bad_host");
    }
    for host in [
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        format!("LOCALHOST:{port}"),
        format!("[::1]:{port}"),
    ] {
        let reply = send(
            h.addr(),
            "GET",
            "/v1/jobs",
            &[("Host", &host), ("Authorization", &bearer)],
            None,
        );
        assert_eq!(reply.status, 200, "Host {host:?}");
    }
    // Health is checked too: a rebinding page cannot even probe it.
    let reply = send(
        h.addr(),
        "GET",
        "/v1/health",
        &[("Host", &format!("evil.example:{port}"))],
        None,
    );
    assert_eq!(reply.status, 403);
    // No Host at all, two Host headers, and an absolute-form target naming
    // another host.
    let own = h.host();
    let head =
        format!("GET /v1/jobs HTTP/1.1\r\nAuthorization: {bearer}\r\nConnection: close\r\n\r\n");
    assert_eq!(raw(h.addr(), head.as_bytes(), b"").status, 403);
    let doubled = send(
        h.addr(),
        "GET",
        "/v1/jobs",
        &[
            ("Host", &own),
            ("Host", "evil.example"),
            ("Authorization", &bearer),
        ],
        None,
    );
    assert_eq!(doubled.status, 403);
    let absolute = send(
        h.addr(),
        "GET",
        &format!("http://evil.example:{port}/v1/jobs"),
        &[("Host", &own), ("Authorization", &bearer)],
        None,
    );
    assert_eq!(absolute.status, 403);
    assert_eq!(absolute.code(), "bad_host");
}

#[test]
fn other_origins_and_null_are_refused_on_every_method() {
    let h = Harness::start();
    let paper = h.paper("paper.pdf");
    let body = serde_json::to_vec(&json!({"action": "text", "paths": [paper]})).unwrap();
    let json_type = ("Content-Type", "application/json");
    for origin in [
        "https://evil.example",
        "null",
        "http://127.0.0.1",
        &format!("http://127.0.0.1:{}", h.addr().port() + 1),
        &format!("https://127.0.0.1:{}", h.addr().port()),
        "http://localhost:3000",
    ] {
        let post = h.call(
            "POST",
            "/v1/jobs",
            &[json_type, ("Origin", origin)],
            Some(&body),
        );
        assert_eq!(post.status, 403, "POST from {origin}");
        assert_eq!(post.code(), "bad_origin");
        let get = h.call("GET", "/v1/jobs", &[("Origin", origin)], None);
        assert_eq!(get.status, 403, "GET from {origin}");
        let preflight = h.call(
            "OPTIONS",
            "/v1/jobs",
            &[
                ("Origin", origin),
                ("Access-Control-Request-Method", "POST"),
                (
                    "Access-Control-Request-Headers",
                    "authorization,content-type",
                ),
            ],
            None,
        );
        assert_eq!(preflight.status, 403, "preflight from {origin}");
        for reply in [&post, &get, &preflight] {
            assert!(
                reply
                    .headers
                    .iter()
                    .all(|(n, _)| !n.to_ascii_lowercase().starts_with("access-control-")),
                "no CORS header is ever sent: {:?}",
                reply.headers
            );
        }
    }
    // Requests a browser sends without Origin but with Sec-Fetch-Site.
    for site in ["cross-site", "same-site"] {
        let reply = h.call("GET", "/v1/jobs", &[("Sec-Fetch-Site", site)], None);
        assert_eq!(reply.status, 403, "{site}");
    }
    assert_eq!(h.listing(), ["paper.pdf"], "nothing was queued or written");
    assert_eq!(h.get("/v1/jobs").json(), json!({"jobs": []}));

    // The server's own origin (a page it would serve) is fine.
    let own = h.origin();
    let ok = h.call(
        "POST",
        "/v1/jobs",
        &[
            json_type,
            ("Origin", &own),
            ("Sec-Fetch-Site", "same-origin"),
        ],
        Some(&body),
    );
    assert_eq!(ok.status, 201, "{}", ok.text());
    let id = ok.json()["jobs"][0]["id"].as_str().unwrap().to_string();
    // The event stream is checked like everything else.
    let events = h.events(&id, &[("Origin", "https://evil.example")]);
    assert_eq!(events.status, 403);
    h.wait(&id);
}

#[test]
fn an_extra_origin_is_allowed_only_when_configured() {
    let h = Harness::with(|config| config.extra_origins = vec!["http://localhost:5173".into()]);
    let ok = h.call(
        "GET",
        "/v1/jobs",
        &[("Origin", "http://localhost:5173")],
        None,
    );
    assert_eq!(ok.status, 200);
    let other = h.call(
        "GET",
        "/v1/jobs",
        &[("Origin", "http://localhost:5174")],
        None,
    );
    assert_eq!(other.status, 403);
}

// ----------------------------------------------------------- binding and token

#[test]
fn a_non_loopback_address_is_refused_and_nothing_is_created() {
    for ip in ["0.0.0.0", "::", "127.0.0.2", "192.0.2.1"] {
        let state = tempfile::tempdir().unwrap();
        let mut config = tpe_serve::Config::new(state.path().join("state"));
        config.bind = ip.parse().unwrap();
        config.port = 0;
        let error = tpe_serve::spawn(config).err().expect("refused");
        assert!(
            matches!(
                error,
                tpe_serve::StartError::Config(tpe_serve::ConfigError::NotLoopback(_))
            ),
            "{ip}: {error}"
        );
        assert!(
            !state.path().join("state").exists(),
            "{ip}: no token was made"
        );

        let output = std::process::Command::new(env!("CARGO_BIN_EXE_tpe-serve"))
            .args(["--bind", ip, "--port", "0", "--state-dir"])
            .arg(state.path().join("state"))
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{ip}");
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("refusing to bind"));
        assert!(!state.path().join("state").exists());
    }
}

#[test]
fn the_token_file_is_private_reused_and_printable() {
    let h = Harness::start();
    let path = h.state.path().join("api-token");
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let token = h.server.token().expose().to_string();
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        format!("{token}\n")
    );
    let state = h.state.path().to_path_buf();
    let Harness {
        server,
        state: keep,
        files: _files,
        ..
    } = h;
    server.shutdown().unwrap();

    let mut config = tpe_serve::Config::new(state.clone());
    config.port = 0;
    let again = tpe_serve::spawn(config).unwrap();
    assert_eq!(
        again.token().expose(),
        token,
        "the same token after a restart"
    );
    drop(again);

    let printed = std::process::Command::new(env!("CARGO_BIN_EXE_tpe-serve"))
        .arg("--print-token")
        .arg("--state-dir")
        .arg(&state)
        .output()
        .unwrap();
    assert!(printed.status.success());
    assert_eq!(
        String::from_utf8(printed.stdout).unwrap(),
        format!("{token}\n")
    );

    // A token file others could read is not used.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let mut config = tpe_serve::Config::new(state.clone());
    config.port = 0;
    assert!(matches!(
        tpe_serve::spawn(config),
        Err(tpe_serve::StartError::Token(
            tpe_serve::TokenError::Permissions(..)
        ))
    ));
    drop(keep);
}

// --------------------------------------------------------------- request bodies

#[test]
fn bodies_must_be_json_of_the_right_shape_and_size() {
    let h = Harness::with(|config| config.limits.max_body_bytes = 4096);
    let paper = h.paper("paper.pdf");
    let good = serde_json::to_vec(&json!({"action": "text", "paths": [paper]})).unwrap();
    for content_type in [
        None,
        Some("text/plain"),
        Some("application/x-www-form-urlencoded"),
        Some("multipart/form-data; boundary=x"),
        Some("application/jsonp"),
        Some("application/json-seq"),
    ] {
        let headers: Vec<(&str, &str)> = content_type
            .map(|t| ("Content-Type", t))
            .into_iter()
            .collect();
        let reply = h.call("POST", "/v1/jobs", &headers, Some(&good));
        assert_eq!(reply.status, 415, "{content_type:?}");
        assert_eq!(reply.code(), "unsupported_media_type");
    }
    let charset = h.call(
        "POST",
        "/v1/jobs",
        &[("Content-Type", "Application/JSON; charset=utf-8")],
        Some(&good),
    );
    assert_eq!(charset.status, 201, "{}", charset.text());
    let id = charset.json()["jobs"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    h.wait(&id);

    let json_type = [("Content-Type", "application/json")];
    for (body, status, code) in [
        (r#"{"action": "text", "paths": ["#, 400, "malformed_json"),
        ("not json", 400, "malformed_json"),
        ("", 400, "malformed_json"),
        (
            r#"{"action": "summarise", "paths": ["/x.pdf"]}"#,
            422,
            "invalid_request",
        ),
        (r#"{"action": "text"}"#, 422, "invalid_request"),
        (r#"{"action": "text", "paths": []}"#, 422, "invalid_request"),
        (
            r#"{"action": "text", "paths": "/x.pdf"}"#,
            422,
            "invalid_request",
        ),
        (
            r#"{"action": "text", "paths": ["/x.pdf"], "extra": 1}"#,
            422,
            "invalid_request",
        ),
        (r#"["text"]"#, 422, "invalid_request"),
    ] {
        let reply = h.call("POST", "/v1/jobs", &json_type, Some(body.as_bytes()));
        assert_eq!(reply.status, status, "{body}");
        assert_eq!(reply.code(), code, "{body}");
    }

    // Announced too large: refused before the body is read.
    let host = h.host();
    let bearer = h.bearer();
    let head = format!(
        "POST /v1/jobs HTTP/1.1\r\nHost: {host}\r\nAuthorization: {bearer}\r\n\
         Content-Type: application/json\r\nContent-Length: 10000000\r\nConnection: close\r\n\r\n"
    );
    let reply = raw(h.addr(), head.as_bytes(), b"");
    assert_eq!(reply.status, 413);
    assert_eq!(reply.code(), "payload_too_large");
    // Too large without a length (chunked).
    let big = format!(r#"{{"action": "text", "paths": ["{}"]}}"#, "a".repeat(5000));
    let head = format!(
        "POST /v1/jobs HTTP/1.1\r\nHost: {host}\r\nAuthorization: {bearer}\r\n\
         Content-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
    );
    let chunked = format!("{:x}\r\n{big}\r\n0\r\n\r\n", big.len());
    let reply = raw(h.addr(), head.as_bytes(), chunked.as_bytes());
    assert_eq!(reply.status, 413);
    // Too many paths.
    let many = vec![paper.to_str().unwrap(); 1001];
    let h2 = Harness::start();
    let reply = h2.post_json("/v1/jobs", &json!({"action": "text", "paths": many}));
    assert_eq!(reply.status, 422);
    assert_eq!(reply.code(), "invalid_request");
}

// ------------------------------------------------------------------------ paths

#[test]
fn paths_are_checked_all_or_nothing_and_errors_name_no_paths() {
    let h = Harness::start();
    let paper = h.paper("paper.pdf");
    let root = h.root.to_str().unwrap().to_string();
    std::fs::create_dir(h.root.join("sub")).unwrap();
    std::fs::create_dir(h.root.join("folder.pdf")).unwrap();
    std::fs::write(h.root.join("notes.txt"), b"x").unwrap();
    std::fs::write(h.root.join("secret.key"), b"k").unwrap();
    std::os::unix::fs::symlink(h.root.join("secret.key"), h.root.join("evil.pdf")).unwrap();
    std::os::unix::fs::symlink("/etc/passwd", h.root.join("passwd.pdf")).unwrap();
    let cases = [
        (paper.to_str().unwrap().to_string(), None),
        ("paper.pdf".to_string(), Some("not_absolute")),
        (format!("{root}/sub/../paper.pdf"), Some("dot_segment")),
        (format!("{root}/./paper.pdf"), Some("dot_segment")),
        (format!("{root}/missing.pdf"), Some("not_found")),
        (format!("{root}/folder.pdf"), Some("not_a_file")),
        (format!("{root}/notes.txt"), Some("not_pdf")),
        (format!("{root}/evil.pdf"), Some("not_pdf")),
        (format!("{root}/passwd.pdf"), Some("not_pdf")),
        ("/etc/passwd".to_string(), Some("not_pdf")),
        (String::new(), Some("invalid")),
    ];
    let paths: Vec<&str> = cases.iter().map(|(p, _)| p.as_str()).collect();
    let reply = h.post_json("/v1/jobs", &json!({"action": "text", "paths": paths}));
    assert_eq!(reply.status, 422, "{}", reply.text());
    assert_eq!(reply.code(), "invalid_path");
    let details = reply.json()["error"]["details"].clone();
    let expected: Vec<Value> = cases
        .iter()
        .enumerate()
        .filter_map(|(index, (_, reason))| reason.map(|r| json!({"index": index, "reason": r})))
        .collect();
    assert_eq!(details, Value::Array(expected));
    let text = reply.text();
    assert!(!text.contains(&root), "the error names no path: {text}");
    assert!(!text.contains("/etc"), "{text}");
    assert_eq!(
        h.get("/v1/jobs").json(),
        json!({"jobs": []}),
        "nothing queued"
    );
    assert!(!h.listing().iter().any(|n| {
        Path::new(n)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("txt"))
            && n != "notes.txt"
    }));

    // A link to a PDF is followed: the job reads the real file and writes
    // next to it, and reports the path as sent.
    let elsewhere = h.root.join("sub").join("real.pdf");
    std::fs::copy(common::FIXTURE, &elsewhere).unwrap();
    let link = h.root.join("link.pdf");
    std::os::unix::fs::symlink(&elsewhere, &link).unwrap();
    let ids = h.submit("text", &[&link]);
    let job = h.wait(&ids[0]);
    assert_eq!(job["state"], "finished", "{job}");
    assert_eq!(job["path"], link.to_str().unwrap());
    assert_eq!(
        job["outputs"][0]["path"],
        h.root.join("sub/real.txt").to_str().unwrap()
    );
}

// ------------------------------------------------------------------ happy paths

#[test]
fn get_text_end_to_end() {
    let h = Harness::start();
    let paper = h.paper("paper.pdf");
    let ids = h.submit("text", &[&paper]);
    let mut events = h.events(&ids[0], &[]);
    assert_eq!(events.status, 200);
    assert_eq!(
        events
            .headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("content-type"))
            .unwrap()
            .1,
        "text/event-stream"
    );
    let seen = events.until_final();
    let last = seen.last().unwrap().json();
    assert_eq!(last["state"], "finished", "{last}");
    assert_eq!(last["summary"], "2 pages, 3 references");
    assert_eq!(last["done"], 2);
    assert_eq!(last["total"], 2);
    assert_eq!(
        last["progress"],
        json!({"event": "page", "page": 2, "done": 2, "total": 2})
    );
    assert_eq!(last["outputs"][0]["name"], "paper.txt");
    assert!(
        events.next().is_none(),
        "the stream ends after the final state"
    );
    let ids_seen: Vec<u64> = seen.iter().filter_map(|e| e.id).collect();
    assert!(
        ids_seen.windows(2).all(|w| w[0] < w[1]),
        "ids increase: {ids_seen:?}"
    );

    let output = h.get(&format!("/v1/jobs/{}/output/0", ids[0]));
    assert_eq!(output.status, 200);
    assert_eq!(
        output.header("content-type"),
        Some("text/plain; charset=utf-8")
    );
    let on_disk = std::fs::read(h.root.join("paper.txt")).unwrap();
    assert_eq!(output.body, on_disk, "served byte for byte");
    assert!(
        output
            .text()
            .contains("Faithful Extraction of Citations from Academic PDFs")
    );
    assert_eq!(h.listing(), ["paper.pdf", "paper.txt"]);
    assert!(
        h.state.path().join("ledger.sqlite").exists(),
        "the ledger was written"
    );
}

#[test]
fn get_bibliography_end_to_end_with_the_engine_record() {
    let h = Harness::start();
    let paper = h.paper("paper.pdf");
    let ids = h.submit("bibliography", &[&paper]);
    let job = h.wait(&ids[0]);
    assert_eq!(job["state"], "finished", "{job}");
    assert_eq!(job["summary"], "3 references from the last 1 page");
    let names: Vec<&str> = job["outputs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["paper.references.json", "paper.references.txt"]);

    let record = h.get(&format!("/v1/jobs/{}/output/0", ids[0]));
    assert_eq!(record.status, 200);
    assert_eq!(record.header("content-type"), Some("application/json"));
    let on_disk = std::fs::read(h.root.join("paper.references.json")).unwrap();
    assert_eq!(record.body, on_disk, "the file the job wrote, unchanged");
    // The engine's `Record`, serialised as `tpe bibliography` prints it:
    // same fields as a record the engine itself makes.
    let value: Value = serde_json::from_slice(&record.body).unwrap();
    let identity = tpe::backend::by_name("lopdf").unwrap().identity();
    let engine = serde_json::to_value(tpe::bibliography::Record::failed(
        "x",
        None,
        identity,
        "e".into(),
        0.0,
    ))
    .unwrap();
    let keys = |v: &Value| {
        v.as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>()
    };
    assert_eq!(keys(&value), keys(&engine));
    assert_eq!(value["status"], "found");
    assert_eq!(value["references"].as_array().unwrap().len(), 3);
    assert!(
        record.body.ends_with(b"}\n"),
        "one JSON line, as the CLI prints"
    );

    let plain = h.get(&format!("/v1/jobs/{}/output/1", ids[0]));
    assert_eq!(
        plain.body,
        std::fs::read(h.root.join("paper.references.txt")).unwrap()
    );
    assert!(
        !h.state.path().join("ledger.sqlite").exists(),
        "a bibliography needs no ledger"
    );
}

#[test]
fn version_and_the_job_list() {
    let h = Harness::start();
    let version = h.get("/v1/version").json();
    assert_eq!(version["api"], "v1");
    assert_eq!(version["actions"], json!(["text", "bibliography"]));
    let instance = version["instance"].as_str().unwrap().to_string();
    let a = h.paper("a.pdf");
    let b = h.paper("b.pdf");
    let ids = h.submit("text", &[&a, &b]);
    assert!(ids.iter().all(|id| id.starts_with(&format!("{instance}-"))));
    for id in &ids {
        h.wait(id);
    }
    let list = h.get("/v1/jobs").json();
    let listed: Vec<&str> = list["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|j| j["id"].as_str().unwrap())
        .collect();
    assert_eq!(listed, ids.iter().map(String::as_str).collect::<Vec<_>>());
    // An id from another run of the server (another instance) is unknown.
    let foreign = format!("00000000-{}", ids[0].split_once('-').unwrap().1);
    let reply = h.get(&format!("/v1/jobs/{foreign}"));
    assert_eq!(reply.status, 404);
    assert_eq!(reply.code(), "not_found");
    for bad in [
        "x",
        "-1",
        &format!("{instance}-"),
        &format!("{instance}-+1"),
        &format!("{instance}-1-1"),
    ] {
        assert_eq!(h.get(&format!("/v1/jobs/{bad}")).status, 404, "{bad}");
    }
}

// ---------------------------------------------------------------- cancel/delete

#[test]
fn cancel_is_accepted_until_the_job_writes_and_then_refused() {
    let h = Harness::start();
    let long = h.root.join("long.pdf");
    std::fs::write(&long, common::long_pdf(3000)).unwrap();
    let paper = h.paper("paper.pdf");
    let ids = h.submit("text", &[&long, &paper]);
    let (running, queued) = (&ids[0], &ids[1]);

    // The queued job: cancelled at once, never run.
    let reply = h.call("POST", &format!("/v1/jobs/{queued}/cancel"), &[], None);
    assert_eq!(reply.status, 200, "{}", reply.text());
    assert_eq!(reply.json()["accepted"], true);
    assert_eq!(reply.json()["job"]["state"], "cancelled");
    let again = h.call("POST", &format!("/v1/jobs/{queued}/cancel"), &[], None);
    assert_eq!(
        again.status, 200,
        "cancelling a cancelled job is accepted again"
    );

    // The running job: stopped at its next page after the first.
    let mut events = h.events(running, &[]);
    loop {
        let event = events.next().expect("the job reports pages");
        if event.json()["done"].as_u64().unwrap_or(0) >= 1 {
            break;
        }
    }
    let reply = h.call("POST", &format!("/v1/jobs/{running}/cancel"), &[], None);
    assert_eq!(reply.status, 200, "{}", reply.text());
    assert!(
        matches!(
            reply.json()["job"]["state"].as_str(),
            Some("cancelling" | "cancelled")
        ),
        "{}",
        reply.text()
    );
    let rest = events.until_final();
    let last = rest.last().unwrap().json();
    assert_eq!(last["state"], "cancelled", "{last}");
    assert!(
        last["done"].as_u64().unwrap() < 3000,
        "it stopped part-way: {last}"
    );
    assert_eq!(
        h.listing(),
        ["long.pdf", "paper.pdf"],
        "nothing was written"
    );
    assert!(
        !h.state.path().join("ledger.sqlite").exists(),
        "not even the ledger"
    );

    // A finished job refuses.
    let ids = h.submit("text", &[&paper]);
    h.wait(&ids[0]);
    let reply = h.call("POST", &format!("/v1/jobs/{}/cancel", ids[0]), &[], None);
    assert_eq!(reply.status, 409);
    assert_eq!(reply.code(), "already_finished");

    // A job already writing refuses: hold the ledger so its write waits
    // (SQLite busy timeout, 5 s), then ask to stop it.
    let ledger = rusqlite_lock(&h.state.path().join("ledger.sqlite"));
    let ids = h.submit("text", &[&paper]);
    let mut events = h.events(&ids[0], &[]);
    loop {
        let event = events.next().expect("the job runs");
        if event.json()["done"] == 2 {
            break;
        }
    }
    // Past the last page the job orders, cleans up and commits in a few
    // milliseconds, then blocks on the ledger for up to 5 s.
    std::thread::sleep(Duration::from_millis(300));
    let reply = h.call("POST", &format!("/v1/jobs/{}/cancel", ids[0]), &[], None);
    assert_eq!(reply.status, 409, "{}", reply.text());
    assert_eq!(reply.code(), "cancel_too_late");
    assert_eq!(h.job(&ids[0])["state"], "running", "not marked cancelling");
    drop(ledger);
    let job = h.wait(&ids[0]);
    assert_eq!(job["state"], "finished", "{job}");
    assert_eq!(job["outputs"][0]["name"], "paper 2.txt");
}

/// Hold a write lock on the ledger (created, with its schema, if missing)
/// until dropped.
fn rusqlite_lock(path: &Path) -> LedgerLock {
    drop(tpe::ledger::Ledger::open(path).unwrap());
    LedgerLock::new(path)
}

/// An exclusive `SQLite` transaction on the ledger, held open by a thread.
struct LedgerLock {
    stop: std::sync::mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl LedgerLock {
    fn new(path: &Path) -> Self {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (stop, stop_rx) = std::sync::mpsc::channel::<()>();
        let path = path.to_path_buf();
        let thread = std::thread::spawn(move || {
            let connection = rusqlite::Connection::open(&path).unwrap();
            connection.execute_batch("BEGIN EXCLUSIVE").unwrap();
            ready_tx.send(()).unwrap();
            let _ = stop_rx.recv();
            connection.execute_batch("ROLLBACK").unwrap();
        });
        ready_rx.recv().unwrap();
        Self {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for LedgerLock {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

#[test]
fn delete_forgets_queued_or_done_jobs_only() {
    let h = Harness::start();
    let long = h.root.join("long.pdf");
    std::fs::write(&long, common::long_pdf(3000)).unwrap();
    let paper = h.paper("paper.pdf");
    let ids = h.submit("text", &[&long, &paper]);
    let mut events = h.events(&ids[0], &[]);
    while events.next().expect("the job starts").json()["state"] != "running" {}
    let running = h.call("DELETE", &format!("/v1/jobs/{}", ids[0]), &[], None);
    assert_eq!(running.status, 409);
    assert_eq!(running.code(), "job_running");

    let mut watcher = h.events(&ids[1], &[]);
    assert_eq!(watcher.next().unwrap().json()["state"], "queued");
    let queued = h.call("DELETE", &format!("/v1/jobs/{}", ids[1]), &[], None);
    assert_eq!(queued.status, 204);
    assert_eq!(
        watcher.next().unwrap().kind,
        "removed",
        "its stream says so"
    );
    assert!(watcher.next().is_none());
    assert_eq!(h.get(&format!("/v1/jobs/{}", ids[1])).status, 404);

    h.call("POST", &format!("/v1/jobs/{}/cancel", ids[0]), &[], None);
    h.wait(&ids[0]);
    let ids = h.submit("text", &[&paper]);
    h.wait(&ids[0]);
    let done = h.call("DELETE", &format!("/v1/jobs/{}", ids[0]), &[], None);
    assert_eq!(done.status, 204);
    assert_eq!(
        h.call("DELETE", &format!("/v1/jobs/{}", ids[0]), &[], None)
            .status,
        404
    );
    assert!(h.root.join("paper.txt").exists(), "outputs stay on disk");
}

#[test]
fn an_idempotency_key_replays_the_batch_instead_of_queueing_it_again() {
    let h = Harness::start();
    let paper = h.paper("paper.pdf");
    let body = serde_json::to_vec(&json!({"action": "text", "paths": [paper]})).unwrap();
    let headers = [
        ("Content-Type", "application/json"),
        ("Idempotency-Key", "retry-1"),
    ];
    let first = h.call("POST", "/v1/jobs", &headers, Some(&body));
    assert_eq!(first.status, 201);
    let id = first.json()["jobs"][0]["id"].as_str().unwrap().to_string();
    let second = h.call("POST", "/v1/jobs", &headers, Some(&body));
    assert_eq!(second.status, 200);
    assert_eq!(second.json()["jobs"][0]["id"], id.as_str());
    let other = serde_json::to_vec(&json!({"action": "bibliography", "paths": [paper]})).unwrap();
    let reused = h.call("POST", "/v1/jobs", &headers, Some(&other));
    assert_eq!(reused.status, 422);
    assert_eq!(reused.code(), "idempotency_key_reused");
    h.wait(&id);
    assert_eq!(
        h.get("/v1/jobs").json()["jobs"].as_array().unwrap().len(),
        1
    );
    assert_eq!(h.listing(), ["paper.pdf", "paper.txt"], "one run, one file");
    // Once its jobs are deleted the key is free again.
    h.call("DELETE", &format!("/v1/jobs/{id}"), &[], None);
    let fresh = h.call("POST", "/v1/jobs", &headers, Some(&body));
    assert_eq!(fresh.status, 201);
    h.wait(fresh.json()["jobs"][0]["id"].as_str().unwrap());
    let bad = h.call(
        "POST",
        "/v1/jobs",
        &[
            ("Content-Type", "application/json"),
            ("Idempotency-Key", "has space"),
        ],
        Some(&body),
    );
    assert_eq!(bad.status, 422);
}

#[test]
fn the_job_limit_refuses_a_batch_that_would_exceed_it() {
    let h = Harness::with(|config| config.limits.max_jobs = 2);
    let paper = h.paper("paper.pdf");
    let paths: Vec<&Path> = vec![&paper; 3];
    let paths: Vec<&str> = paths.iter().map(|p| p.to_str().unwrap()).collect();
    let reply = h.post_json(
        "/v1/jobs",
        &json!({"action": "bibliography", "paths": paths}),
    );
    assert_eq!(reply.status, 429);
    assert_eq!(reply.code(), "job_limit");
    assert_eq!(h.get("/v1/jobs").json(), json!({"jobs": []}));
}

// -------------------------------------------------------------------------- SSE

#[test]
fn events_resume_from_last_event_id() {
    let h = Harness::start();
    let paper = h.paper("paper.pdf");
    let ids = h.submit("text", &[&paper]);
    let seen = h.events(&ids[0], &[]).until_final();
    let last = seen.last().unwrap();
    let last_id = last.id.unwrap();
    assert_eq!(last.json()["seq"], last_id);

    // Already has the final state: 204, which stops a browser EventSource.
    let done = h.events(&ids[0], &[("Last-Event-ID", &last_id.to_string())]);
    assert_eq!(done.status, 204);
    // Behind: one event, the current (final) state, then the end.
    let earlier = seen.first().unwrap().id.unwrap();
    let mut resumed = h.events(&ids[0], &[("Last-Event-ID", &earlier.to_string())]);
    assert_eq!(resumed.status, 200);
    let event = resumed.next().unwrap();
    assert_eq!(event.id, Some(last_id));
    assert_eq!(event.json()["state"], "finished");
    assert!(resumed.next().is_none());
    // Not an id.
    let bad = h.events(&ids[0], &[("Last-Event-ID", "soon")]);
    assert_eq!(bad.status, 400);
    assert_eq!(bad.body()["error"]["code"], "invalid_request");
    let missing = h.events("00000000-1", &[]);
    assert_eq!(missing.status, 404);
}

#[test]
fn an_idle_stream_is_kept_alive_not_timed_out() {
    let h = Harness::with(|config| {
        config.limits.sse_keepalive = Duration::from_millis(200);
        config.limits.request_timeout = Duration::from_millis(300);
    });
    let long = h.root.join("long.pdf");
    std::fs::write(&long, common::long_pdf(3000)).unwrap();
    let paper = h.paper("paper.pdf");
    let ids = h.submit("text", &[&long, &paper]);
    // The queued job does not change while the long one runs.
    let mut events = h.events(&ids[1], &[]);
    assert_eq!(events.next().unwrap().json()["state"], "queued");
    std::thread::sleep(Duration::from_millis(900));
    h.call("POST", &format!("/v1/jobs/{}/cancel", ids[0]), &[], None);
    let rest = events.until_final();
    assert_eq!(rest.last().unwrap().json()["state"], "finished");
    assert!(
        events.comments >= 2,
        "keep-alive comments were sent: {}",
        events.comments
    );
}

#[test]
fn two_clients_at_once() {
    let h = Harness::start();
    let a = h.paper("a.pdf");
    let b = h.paper("b.pdf");
    let results: Vec<Value> = std::thread::scope(|scope| {
        let handles: Vec<_> = [("text", &a), ("bibliography", &b), ("text", &a)]
            .into_iter()
            .map(|(action, path)| {
                let h = &h;
                scope.spawn(move || {
                    let ids = h.submit(action, &[path]);
                    let mut events = h.events(&ids[0], &[]);
                    events.until_final().last().unwrap().json()
                })
            })
            .collect();
        handles.into_iter().map(|t| t.join().unwrap()).collect()
    });
    for job in &results {
        assert_eq!(job["state"], "finished", "{job}");
    }
    let mut names: Vec<String> = results
        .iter()
        .flat_map(|job| {
            job["outputs"]
                .as_array()
                .unwrap()
                .iter()
                .map(|o| o["name"].as_str().unwrap().to_string())
        })
        .collect();
    names.sort();
    assert_eq!(
        names,
        ["a 2.txt", "a.txt", "b.references.json", "b.references.txt"]
    );
}

// ----------------------------------------------------------------------- output

#[test]
fn the_output_endpoint_serves_only_the_file_the_job_wrote() {
    let h = Harness::start();
    let paper = h.paper("paper.pdf");
    let ids = h.submit("text", &[&paper]);
    h.wait(&ids[0]);
    let id = &ids[0];
    let output = h.root.join("paper.txt");
    let secret = h.root.join("secret.key");
    std::fs::write(&secret, b"do not serve").unwrap();

    for n in [
        "1",
        "99",
        "-1",
        "0x0",
        "+0",
        "00000000000000000001",
        "%30",
        "0.txt",
    ] {
        let reply = h.get(&format!("/v1/jobs/{id}/output/{n}"));
        assert_eq!(reply.status, 404, "{n}");
        assert!(!reply.text().contains("do not serve"));
    }
    for target in [
        format!("/v1/jobs/{id}/output/../../../etc/passwd"),
        format!("/v1/jobs/{id}/output/0/../1"),
        format!("/v1/jobs/{id}/output/%2e%2e%2fsecret.key"),
        "/v1/jobs/../jobs/x/output/0".to_string(),
    ] {
        let reply = h.get(&target);
        assert_eq!(reply.status, 404, "{target}");
        assert!(!reply.text().contains("do not serve"), "{target}");
    }
    assert_eq!(h.get(&format!("/v1/jobs/{id}/output/0")).status, 200);

    // The name now points somewhere else: a link, then a new file.
    std::fs::rename(&output, h.root.join("moved.txt")).unwrap();
    std::os::unix::fs::symlink(&secret, &output).unwrap();
    let reply = h.get(&format!("/v1/jobs/{id}/output/0"));
    assert_eq!(reply.status, 410, "{}", reply.text());
    assert_eq!(reply.code(), "output_changed");
    assert!(!reply.text().contains("do not serve"));
    std::fs::remove_file(&output).unwrap();
    std::fs::write(&output, b"a different file").unwrap();
    let reply = h.get(&format!("/v1/jobs/{id}/output/0"));
    assert_eq!(reply.status, 410);
    std::fs::remove_file(&output).unwrap();
    assert_eq!(h.get(&format!("/v1/jobs/{id}/output/0")).status, 410);
    // Moved back, it is the same file again.
    std::fs::rename(h.root.join("moved.txt"), &output).unwrap();
    assert_eq!(h.get(&format!("/v1/jobs/{id}/output/0")).status, 200);

    // Not finished yet, or never produced.
    let long = h.root.join("long.pdf");
    std::fs::write(&long, common::long_pdf(3000)).unwrap();
    let ids = h.submit("text", &[&long]);
    let reply = h.get(&format!("/v1/jobs/{}/output/0", ids[0]));
    assert_eq!(reply.status, 409);
    assert_eq!(reply.code(), "not_finished");
    h.call("POST", &format!("/v1/jobs/{}/cancel", ids[0]), &[], None);
    h.wait(&ids[0]);
    assert_eq!(h.get(&format!("/v1/jobs/{}/output/0", ids[0])).status, 404);
}

// ---------------------------------------------------------------------- limits

#[test]
fn an_idle_or_slow_connection_is_closed() {
    let h = Harness::with(|config| {
        config.limits.header_read_timeout = Duration::from_millis(500);
        config.limits.request_timeout = Duration::from_millis(500);
    });
    // Sends nothing.
    let mut idle = TcpStream::connect(h.addr()).unwrap();
    idle.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let started = Instant::now();
    let mut buffer = [0u8; 256];
    let _ = idle.read(&mut buffer);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "closed by the header timeout"
    );
    // Sends half a head.
    let mut slow = TcpStream::connect(h.addr()).unwrap();
    slow.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    slow.write_all(b"GET /v1/jobs HTTP/1.1\r\nHost: x").unwrap();
    let started = Instant::now();
    let _ = slow.read(&mut buffer);
    assert!(started.elapsed() < Duration::from_secs(5));
    // Sends a head and part of the body.
    let head = format!(
        "POST /v1/jobs HTTP/1.1\r\nHost: {}\r\nAuthorization: {}\r\n\
         Content-Type: application/json\r\nContent-Length: 100\r\n\r\n{{\"action\"",
        h.host(),
        h.bearer()
    );
    let reply = raw(h.addr(), head.as_bytes(), b"");
    assert_eq!(reply.status, 408);
    assert_eq!(reply.code(), "request_timeout");
}

#[test]
fn connections_beyond_the_limit_wait_their_turn() {
    let h = Harness::with(|config| config.limits.max_connections = 2);
    let first = TcpStream::connect(h.addr()).unwrap();
    let second = TcpStream::connect(h.addr()).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    let host = h.host();
    let addr = h.addr();
    let waiting = std::thread::spawn(move || {
        let started = Instant::now();
        let reply = send(addr, "GET", "/v1/health", &[("Host", &host)], None);
        (reply.status, started.elapsed())
    });
    std::thread::sleep(Duration::from_millis(400));
    drop(first);
    let (status, waited) = waiting.join().unwrap();
    assert_eq!(status, 200);
    assert!(
        waited >= Duration::from_millis(350),
        "it waited for a slot: {waited:?}"
    );
    drop(second);
}

// --------------------------------------------------------------------- OpenAPI

#[test]
#[allow(clippy::too_many_lines)]
fn the_openapi_document_matches_the_routes_and_the_responses() {
    let h = Harness::start();
    let served = h.get("/v1/openapi.json");
    assert_eq!(served.status, 200);
    assert_eq!(served.body, OPENAPI.as_bytes());
    let doc: Value = serde_json::from_str(OPENAPI).unwrap();

    let mut documented = BTreeSet::new();
    for (path, item) in doc["paths"].as_object().unwrap() {
        for method in ["get", "post", "delete", "put", "patch", "head", "options"] {
            if item.get(method).is_some() {
                documented.insert((method.to_uppercase(), path.clone()));
            }
        }
    }
    let routed: BTreeSet<(String, String)> = ROUTES
        .iter()
        .map(|(m, p, _)| ((*m).to_string(), (*p).to_string()))
        .collect();
    assert_eq!(documented, routed);

    // Every documented operation is answered by its endpoint, not "no route".
    for (method, path) in &documented {
        let concrete = path.replace("{id}", "00000000-1").replace("{n}", "0");
        let headers: &[(&str, &str)] = if method == "POST" && path == "/v1/jobs" {
            &[("Content-Type", "application/json")]
        } else {
            &[]
        };
        let reply = h.call(method, &concrete, headers, Some(b"{}"));
        assert!(
            reply.status != 405 && !(reply.status == 404 && reply.code() == "no_route"),
            "{method} {path}: {} {}",
            reply.status,
            reply.text()
        );
    }
    let wrong = h.call("PUT", "/v1/jobs", &[], None);
    assert_eq!(wrong.status, 405);
    assert_eq!(wrong.header("allow"), Some("GET, POST"));
    assert_eq!(h.get("/v1/unknown").code(), "no_route");

    // Real responses have exactly the documented properties.
    let schemas = &doc["components"]["schemas"];
    let properties = |name: &str| -> BTreeSet<String> {
        schemas[name]["properties"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect()
    };
    let required = |name: &str| -> BTreeSet<String> {
        schemas[name]["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect()
    };
    let keys = |v: &Value| {
        v.as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>()
    };
    let paper = h.paper("paper.pdf");
    let ids = h.submit("bibliography", &[&paper]);
    let job = h.wait(&ids[0]);
    let long = h.root.join("long.pdf");
    std::fs::write(&long, common::long_pdf(3000)).unwrap();
    let pair = h.submit("text", &[&long, &paper]);
    let cancelled = h.call("POST", &format!("/v1/jobs/{}/cancel", pair[1]), &[], None);
    h.call("POST", &format!("/v1/jobs/{}/cancel", pair[0]), &[], None);
    h.wait(&pair[0]);
    let health = send(
        h.addr(),
        "GET",
        "/v1/health",
        &[("Host", h.host().as_str())],
        None,
    );
    for (name, value) in [
        ("Job", &job),
        ("Output", &job["outputs"][0]),
        ("Version", &h.get("/v1/version").json()),
        ("Health", &health.json()),
        ("Error", &h.get("/v1/unknown").json()),
        ("JobList", &h.get("/v1/jobs").json()),
        ("CancelResult", &cancelled.json()),
    ] {
        assert_eq!(keys(value), properties(name), "{name}: {value}");
        assert_eq!(
            required(name),
            properties(name),
            "{name}: all properties are required"
        );
    }
    let codes: BTreeSet<String> =
        schemas["Error"]["properties"]["error"]["properties"]["code"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
    let source = [
        include_str!("../src/error.rs"),
        include_str!("../src/http.rs"),
        include_str!("../src/store.rs"),
        include_str!("../src/sse.rs"),
    ]
    .concat();
    let used = error_codes(&source);
    assert_eq!(
        used, codes,
        "every error code the code uses is documented, and only those"
    );
}

/// The string literals passed as an error `code`: the second argument of
/// `ApiError::new(` calls and the `code` fields of the named constructors.
fn error_codes(source: &str) -> BTreeSet<String> {
    let mut codes = BTreeSet::new();
    let mut rest = source;
    while let Some(at) = rest.find("StatusCode::") {
        rest = &rest[at..];
        // `StatusCode::X,\n  "code",` — the literal right after a status.
        let after = &rest[rest.find(',').map_or(rest.len(), |i| i + 1)..];
        let trimmed = after.trim_start();
        if let Some(stripped) = trimmed.strip_prefix('"')
            && let Some(end) = stripped.find('"')
        {
            let code = &stripped[..end];
            if !code.is_empty() && code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') {
                codes.insert(code.to_string());
            }
        }
        rest = &rest["StatusCode::".len()..];
    }
    codes
}
