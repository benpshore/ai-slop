//! Request latency with thousands of jobs, and event fan-out to many
//! watchers. A measurement, not a check: run it with
//!
//! ```sh
//! cargo test --release -p tpe-serve --test latency -- --ignored --nocapture
//! ```
//!
//! Every number is client-side wall time on this machine, including a new
//! loopback TCP connection per request (the test client does not reuse
//! connections). Each line gives n, min, median, p95, p99 (n >= 100 only),
//! max and max/median.

mod common;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use common::{Events, Harness, send};
use serde_json::json;

fn report(label: &str, mut samples: Vec<Duration>) {
    samples.sort();
    let n = samples.len();
    let at = |q: f64| {
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let index = ((n as f64 * q).ceil() as usize).clamp(1, n) - 1;
        samples[index]
    };
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    let median = at(0.5);
    let p99 = if n >= 100 {
        format!("{:.3}", ms(at(0.99)))
    } else {
        "n/a".to_string()
    };
    println!(
        "{label}: n={n} min={:.3} median={:.3} p95={:.3} p99={p99} max={:.3} ms, max/median={:.1}",
        ms(samples[0]),
        ms(median),
        ms(at(0.95)),
        ms(samples[n - 1]),
        ms(samples[n - 1]) / ms(median),
    );
}

fn time<T>(f: impl FnOnce() -> T) -> (T, Duration) {
    let started = Instant::now();
    let value = f();
    (value, started.elapsed())
}

/// One GET on a new connection: time to the first response byte (the
/// server's work, the JSON built under the lock included) and to the last.
fn timed_get(addr: std::net::SocketAddr, head: &[u8]) -> (Duration, Duration) {
    use std::io::{Read as _, Write as _};
    let started = Instant::now();
    let mut stream = std::net::TcpStream::connect(addr).unwrap();
    stream.write_all(head).unwrap();
    let mut buffer = vec![0u8; 256 * 1024];
    let mut first = None;
    loop {
        let n = stream.read(&mut buffer).unwrap();
        first.get_or_insert_with(|| started.elapsed());
        if n == 0 {
            break;
        }
    }
    (first.unwrap(), started.elapsed())
}

#[test]
#[ignore = "measurement; run with --release -- --ignored --nocapture"]
#[allow(clippy::too_many_lines)]
fn latency() {
    const JOBS: usize = 5000;
    const WATCHERS: usize = 100;
    let h = Harness::with(|config| {
        config.limits.max_jobs = JOBS + 10;
        config.limits.max_connections = WATCHERS + 20;
    });
    // One name per job, so each writes its own `pN.references.*`.
    let paper = h.paper("paper.pdf");
    let paths: Vec<PathBuf> = (0..JOBS)
        .map(|i| {
            let path = h.root.join(format!("p{i}.pdf"));
            std::fs::hard_link(&paper, &path).unwrap();
            path
        })
        .collect();
    let host = h.host();
    let bearer = h.bearer();
    let get = |target: &str| {
        send(
            h.addr(),
            "GET",
            target,
            &[("Host", host.as_str()), ("Authorization", bearer.as_str())],
            None,
        )
    };

    let head = |target: &str| {
        format!(
            "GET {target} HTTP/1.1\r\nHost: {host}\r\nAuthorization: {bearer}\r\n\
             Connection: close\r\n\r\n"
        )
        .into_bytes()
    };
    // Time to first byte and to last byte of `n` GETs of `target`.
    let measure = |label: &str, target: &str, n: usize| {
        let request = head(target);
        let (first, last): (Vec<Duration>, Vec<Duration>) =
            (0..n).map(|_| timed_get(h.addr(), &request)).unzip();
        report(&format!("{label}, first byte"), first);
        report(&format!("{label}, last byte"), last);
    };
    measure("GET /v1/health, empty server", "/v1/health", 1000);

    let mut submits = Vec::new();
    let mut first_id = String::new();
    for batch in paths.chunks(1000) {
        let batch: Vec<&str> = batch.iter().map(|p| p.to_str().unwrap()).collect();
        let (reply, took) = time(|| {
            h.post_json(
                "/v1/jobs",
                &json!({"action": "bibliography", "paths": batch}),
            )
        });
        assert_eq!(reply.status, 201);
        if first_id.is_empty() {
            first_id = reply.json()["jobs"][0]["id"].as_str().unwrap().to_string();
        }
        submits.push(took);
    }
    report("POST /v1/jobs, 1000 paths per batch", submits);

    // While the engine works through the queue (rows mostly queued).
    let size = get("/v1/jobs").body.len();
    measure(
        &format!("GET /v1/jobs, {JOBS} jobs, engine busy ({size} bytes)"),
        "/v1/jobs",
        200,
    );
    measure(
        &format!("GET /v1/jobs/{{id}}, {JOBS} jobs, engine busy"),
        &format!("/v1/jobs/{first_id}"),
        1000,
    );

    // Every job finished (rows carry summaries and outputs).
    let started = Instant::now();
    loop {
        let list = get("/v1/jobs").json();
        let active = list["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|j| matches!(j["state"].as_str(), Some("queued" | "running")))
            .count();
        if active == 0 {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(600),
            "{active} left"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    println!(
        "engine: {JOBS} bibliography jobs on the 2-page paper in {:.1} s",
        started.elapsed().as_secs_f64()
    );
    let size = get("/v1/jobs").body.len();
    measure(
        &format!("GET /v1/jobs, {JOBS} finished jobs, engine idle ({size} bytes)"),
        "/v1/jobs",
        200,
    );

    // Fan-out: WATCHERS streams on one long job, then a cancel; how long
    // until each watcher sees it.
    let long = h.root.join("long.pdf");
    std::fs::write(&long, common::long_pdf(3000)).unwrap();
    let id = h.submit("text", &[&long])[0].clone();
    let addr = h.addr();
    let mut watchers: Vec<Events> = (0..WATCHERS)
        .map(|_| {
            Events::open(
                addr,
                &format!("/v1/jobs/{id}/events"),
                &[("Host", host.as_str()), ("Authorization", bearer.as_str())],
            )
        })
        .collect();
    // Wait until every watcher is past page 50, so all are live.
    for watcher in &mut watchers {
        while watcher.next().unwrap().json()["done"].as_u64().unwrap_or(0) < 50 {}
    }
    let (results, cancelled_at) = std::thread::scope(|scope| {
        let handles: Vec<_> = watchers
            .into_iter()
            .map(|mut watcher| {
                scope.spawn(move || {
                    loop {
                        let event = watcher.next().expect("stream ends after the final state");
                        let job = event.json();
                        if matches!(job["state"].as_str(), Some("cancelling" | "cancelled")) {
                            // Drain to the end so the connection closes cleanly.
                            let _ = watcher.until_final();
                            return event.at;
                        }
                    }
                })
            })
            .collect();
        std::thread::sleep(Duration::from_millis(300));
        let cancelled_at = Instant::now();
        let reply = h.call("POST", &format!("/v1/jobs/{id}/cancel"), &[], None);
        assert_eq!(reply.status, 200, "{}", reply.text());
        let results: Vec<Instant> = handles.into_iter().map(|t| t.join().unwrap()).collect();
        (results, cancelled_at)
    });
    let delays: Vec<Duration> = results
        .iter()
        .map(|at| at.saturating_duration_since(cancelled_at))
        .collect();
    report(
        &format!("cancel request to each of {WATCHERS} watchers seeing it (SSE)"),
        delays,
    );
}
