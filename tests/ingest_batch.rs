#![cfg(unix)]

use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

fn run(manifest: &[u8], args: &[&str]) -> (bool, Vec<Value>) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_tpe"))
        .args(["ingest-batch", "--input-list", "-"])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(manifest).unwrap();
    let result = child.wait_with_output().unwrap();
    let records = String::from_utf8(result.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    (result.status.success(), records)
}

#[test]
fn manifest_errors_do_not_lose_valid_jobs_and_results_correlate_by_line() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("source α with spaces.txt");
    let text = "D. Loutchko\r\nCafé α₂";
    fs::write(&path, text).unwrap();
    let good = json!({"path": path}).to_string();
    let manifest = format!("{good}\ninvalid json\n{{\"path\":\"missing.txt\"}}\n{good}\n");
    let (success, records) = run(manifest.as_bytes(), &["--jobs", "2"]);
    assert!(!success);
    assert_eq!(records.len(), 4);
    for line in [1, 4] {
        let record = records.iter().find(|r| r["input_line"] == line).unwrap();
        assert_eq!(record["result"]["outcome"], "extracted", "{record}");
        assert_eq!(record["result"]["content"]["text"], text);
    }
    for line in [2, 3] {
        let record = records.iter().find(|r| r["input_line"] == line).unwrap();
        assert_eq!(record["result"]["outcome"], "failed");
        assert!(record["result"]["content"].is_null());
    }
}

#[test]
fn oversized_worker_output_does_not_block_the_next_document() {
    let directory = tempfile::tempdir().unwrap();
    let large = directory.path().join("large.txt");
    let small = directory.path().join("small.txt");
    fs::write(&large, "x".repeat(10_000)).unwrap();
    fs::write(&small, "exact").unwrap();
    let manifest = format!("{}\n{}\n", json!({"path":large}), json!({"path":small}));
    let (success, records) = run(
        manifest.as_bytes(),
        &["--jobs", "1", "--max-output-bytes", "4096"],
    );
    assert!(!success);
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["result"]["outcome"], "failed");
    assert!(
        records[0]["result"]["warnings"][0]
            .as_str()
            .unwrap()
            .contains("byte limit")
    );
    assert_eq!(
        records[1]["result"]["outcome"], "extracted",
        "{}",
        records[1]
    );
    assert_eq!(records[1]["result"]["content"]["text"], "exact");
}

#[test]
fn repeated_stream_jobs_are_not_deduplicated_or_accumulated_in_a_result_array() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("one.txt");
    fs::write(&path, "one").unwrap();
    let manifest = format!("{}\n", json!({"path":path})).repeat(40);
    let (success, records) = run(manifest.as_bytes(), &["--jobs", "3"]);
    assert!(success);
    assert_eq!(records.len(), 40);
    let mut lines: Vec<u64> = records
        .iter()
        .map(|r| r["input_line"].as_u64().unwrap())
        .collect();
    lines.sort_unstable();
    assert_eq!(lines, (1..=40).collect::<Vec<_>>());
    assert!(
        records
            .iter()
            .all(|r| r["result"]["sha256"] == records[0]["result"]["sha256"])
    );
}

#[test]
fn live_manifest_publishes_before_eof_and_sigterm_cancels_idle_input() {
    use std::io::{BufRead, BufReader};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("streamed.txt");
    fs::write(&path, "first result before producer EOF").unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_tpe"))
        .args(["ingest-batch", "--input-list", "-", "--jobs", "1"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let output = child.stdout.take().unwrap();
    let (send, receive) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut line = String::new();
        let result = BufReader::new(output).read_line(&mut line);
        let _ = send.send(result.map(|_| line));
    });
    writeln!(input, "{}", json!({"path":path})).unwrap();
    input.flush().unwrap();
    let first = receive.recv_timeout(Duration::from_secs(5));
    if first.is_err() {
        let _ = child.kill();
        let _ = child.wait();
        reader.join().unwrap();
        panic!("no result while manifest input remained open: {first:?}");
    }
    let record: Value = serde_json::from_str(&first.unwrap().unwrap()).unwrap();
    assert_eq!(record["result"]["outcome"], "extracted", "{record}");
    assert_eq!(record["input_line"], 1);
    reader.join().unwrap();
    // The first record proves the CLI installed its handlers and entered the
    // supervisor. Keep its input pipe open while asking it to stop.
    unsafe {
        assert_eq!(
            libc::kill(i32::try_from(child.id()).unwrap(), libc::SIGTERM),
            0
        );
    }
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if start.elapsed() > Duration::from_secs(5) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("SIGTERM did not cancel the idle manifest reader");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(!status.success());
    drop(input);
}
