//! Native support gates for the shipped extraction and publication workers.
//! Run with `--nocapture` (or `--show-output`) to retain the measured limits.
//! The default 1 GiB allowance must succeed. Smaller 32/256 MiB allowances
//! record explicit resource failures: allocator regions and lazy mappings can
//! require much more virtual space than the individual allocation requested.
//! The test process never decodes the generated large result or loses ownership
//! of a live child: captures, deadlines, and the stdin lease remain bounded.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::fs::{self, File};
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use tempfile::TempDir;

const MIB: u64 = 1024 * 1024;
const LOW_GROWTH: u64 = 32 * MIB;
const INTERMEDIATE_GROWTH: u64 = 256 * MIB;
const DEFAULT_GROWTH: u64 = 1024 * MIB;
const CAPTURE_BYTES: u64 = 128 * 1024;
const WORKER_DEADLINE: Duration = Duration::from_secs(15);
const REPEATS: usize = 3;

struct Worker {
    child: Child,
    _lease: ChildStdin,
}

impl Drop for Worker {
    fn drop(&mut self) {
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

struct Run {
    status: ExitStatus,
    stdout: PathBuf,
    stderr: String,
    elapsed: Duration,
}

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/native-worker/native.pdf")
}

fn bounded_read(path: &Path) -> Vec<u8> {
    let mut bytes = Vec::new();
    File::open(path)
        .unwrap()
        .take(CAPTURE_BYTES + 1)
        .read_to_end(&mut bytes)
        .unwrap();
    assert!(
        bytes.len() as u64 <= CAPTURE_BYTES,
        "oversized test capture"
    );
    bytes
}

fn run_worker(root: &Path, label: &str, phase: &str, growth: u64, request: &Value) -> Run {
    let directory = root.join(label);
    fs::create_dir(&directory).unwrap();
    let request_path = directory.join("request.json");
    let encoded = serde_json::to_vec(request).unwrap();
    assert!(encoded.len() <= 64 * 1024);
    fs::write(&request_path, encoded).unwrap();
    let stdout = directory.join("stdout");
    let stderr = directory.join("stderr");
    let started = Instant::now();
    let mut child = Command::new(env!("CARGO_BIN_EXE_tpe"))
        .arg("native-worker")
        .arg(request_path)
        .args(["--phase", phase, "--growth-bytes"])
        .arg(growth.to_string())
        .arg("--parent")
        .arg(std::process::id().to_string())
        .env("RAYON_NUM_THREADS", "1")
        .env("OMP_NUM_THREADS", "1")
        .stdin(Stdio::piped())
        .stdout(File::create_new(&stdout).unwrap())
        .stderr(File::create_new(&stderr).unwrap())
        .spawn()
        .unwrap();
    let lease = child.stdin.take().expect("worker lease was not created");
    let mut worker = Worker {
        child,
        _lease: lease,
    };
    let status = loop {
        // Test-driver termination is always a test failure, never evidence
        // that the production worker enforced its own resource limit.
        let captured = fs::metadata(&stdout).unwrap().len() + fs::metadata(&stderr).unwrap().len();
        assert!(
            captured <= CAPTURE_BYTES,
            "{label}: excessive worker output"
        );
        if let Some(status) = worker.child.try_wait().unwrap() {
            break status;
        }
        assert!(
            started.elapsed() < WORKER_DEADLINE,
            "{label}: test deadline exceeded; killing and reaping worker"
        );
        std::thread::sleep(Duration::from_millis(5));
    };
    assert!(
        fs::metadata(&stdout).unwrap().len() + fs::metadata(&stderr).unwrap().len()
            <= CAPTURE_BYTES,
        "{label}: excessive final worker output"
    );
    Run {
        status,
        stdout,
        stderr: String::from_utf8_lossy(&bounded_read(&stderr)).into_owned(),
        elapsed: started.elapsed(),
    }
}

fn extraction_request() -> Value {
    json!({
        "job": {
            "path": fixture(), "backend": "lopdf", "pages": null,
            "password": null, "max_bytes": MIB, "figures_dir": null
        },
        "progress": false
    })
}

fn publication_request(root: &Path, response: &Path, max_output_bytes: u64) -> Value {
    fs::create_dir(root).unwrap();
    json!({
        "response": response, "receipt": root.join("receipt.json"),
        "db": root.join("ledger.sqlite"), "out": root.join("out"),
        "path": fixture(), "pages": null, "json": true,
        "max_output_bytes": max_output_bytes
    })
}

fn evidence(label: &str, phase: &str, growth: u64, run: &Run, limits: Option<&Value>) {
    eprintln!(
        "worker-startup-evidence {}",
        json!({
            "case": label, "phase": phase, "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH, "debug_assertions": cfg!(debug_assertions),
            "requested_growth_bytes": growth, "exit": run.status.to_string(),
            "elapsed_ms": run.elapsed.as_millis(), "limits": limits,
            "stderr": run.stderr
        })
    );
}

fn assert_limits(limits: &Value, growth: u64) {
    assert_eq!(limits["kind"], "address_space_growth");
    assert_eq!(limits["requested_growth_bytes"], growth);
    let startup = limits["startup_virtual_bytes"].as_u64().unwrap();
    let effective = limits["effective_address_space_bytes"].as_u64().unwrap();
    assert!(startup > 0 && effective > startup, "{limits}");
    assert!(
        effective <= startup.checked_add(growth).unwrap(),
        "{limits}"
    );
    for field in ["inherited_soft_bytes", "inherited_hard_bytes"] {
        if let Some(inherited) = limits[field].as_u64() {
            assert!(effective <= inherited, "limit widened: {limits}");
        } else {
            assert!(limits[field].is_null(), "invalid inherited limit: {limits}");
        }
    }
    assert_eq!(limits["core_dump_bytes"], 0);
    assert_eq!(
        limits["kernel_parent_death_signal"],
        cfg!(target_os = "linux")
    );
}

fn assert_fixture_result(result: &Value) {
    assert_eq!(result["status"], "complete", "{result}");
    assert!(
        result["pages"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Existing OCR already reads this sentence.")
    );
}

fn allocation_failure(stderr: &str) -> bool {
    let diagnostic = stderr.to_ascii_lowercase();
    [
        "memory allocation",
        "cannot allocate memory",
        "out of memory",
        "allocation failed",
        "malloc",
        "os error 12",
    ]
    .iter()
    .any(|message| diagnostic.contains(message))
}

fn assert_allowed_startup_failure(label: &str, growth: u64, run: &Run) {
    assert!(
        matches!(growth, LOW_GROWTH | INTERMEDIATE_GROWTH),
        "default allowance failed: {label}: {}",
        run.stderr
    );
    assert!(
        allocation_failure(&run.stderr)
            || run.stderr.to_ascii_lowercase().contains("address-space"),
        "{label}: unexpected smaller-allowance failure {}: {}",
        run.status,
        run.stderr
    );
}

fn assert_published(root: &Path, run: &Run, growth: u64) -> Value {
    assert!(run.status.success(), "{}: {}", run.status, run.stderr);
    let value: Value = serde_json::from_slice(&bounded_read(&run.stdout)).unwrap();
    assert_fixture_result(&value);
    assert_limits(&value["worker_limits"]["publication"], growth);
    let receipt: Value = serde_json::from_slice(&bounded_read(&root.join("receipt.json"))).unwrap();
    assert_eq!(receipt["version"], 1);
    assert_eq!(receipt["status"], "complete");
    assert_eq!(
        receipt["output_bytes"],
        fs::metadata(&run.stdout).unwrap().len()
    );
    let ledger =
        Connection::open_with_flags(root.join("ledger.sqlite"), OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let runs: i64 = ledger
        .query_row("SELECT COUNT(*) FROM runs", [], |row| row.get(0))
        .unwrap();
    assert_eq!(runs, 1);
    assert_eq!(value["output_paths"].as_array().unwrap().len(), 2);
    value
}

#[test]
fn actual_extract_and_publish_startup_records_low_and_default_limits_repeatedly() {
    let root = TempDir::new().unwrap();
    let original = fs::read(fixture()).unwrap();
    let mut fallback_response = None;
    // Establish an actual valid response first, so publication at 32/256 MiB
    // is still exercised if extraction requires more address-space growth.
    // Keep these diagnostics distinct from the strict same-budget pressure
    // control below: startup failure cannot prove decoder containment.
    for growth in [DEFAULT_GROWTH, INTERMEDIATE_GROWTH, LOW_GROWTH] {
        for attempt in 0..REPEATS {
            let extract_label = format!("extract-{}-{attempt}", growth / MIB);
            let extraction = run_worker(
                root.path(),
                &extract_label,
                "extract",
                growth,
                &extraction_request(),
            );
            let response_path = if extraction.status.success() {
                let response: Value =
                    serde_json::from_slice(&bounded_read(&extraction.stdout)).unwrap();
                assert_eq!(response["version"], 1);
                assert_fixture_result(&response["result"]);
                assert_limits(&response["limits"], growth);
                evidence(
                    &extract_label,
                    "extract",
                    growth,
                    &extraction,
                    Some(&response["limits"]),
                );
                if fallback_response.is_none() {
                    fallback_response = Some(extraction.stdout.clone());
                }
                extraction.stdout
            } else {
                evidence(&extract_label, "extract", growth, &extraction, None);
                assert_allowed_startup_failure(&extract_label, growth, &extraction);
                fallback_response.as_ref().unwrap().clone()
            };
            let publish_label = format!("publish-{}-{attempt}", growth / MIB);
            let artifacts = root
                .path()
                .join(format!("artifacts-{}-{attempt}", growth / MIB));
            let request = publication_request(&artifacts, &response_path, MIB);
            let publication = run_worker(root.path(), &publish_label, "publish", growth, &request);
            if publication.status.success() {
                let value = assert_published(&artifacts, &publication, growth);
                evidence(
                    &publish_label,
                    "publish",
                    growth,
                    &publication,
                    Some(&value["worker_limits"]["publication"]),
                );
            } else {
                evidence(&publish_label, "publish", growth, &publication, None);
                assert_allowed_startup_failure(&publish_label, growth, &publication);
            }
        }
    }
    assert_eq!(fs::read(fixture()).unwrap(), original);
}

/// Stream a valid result whose short JSON strings expand into a large vector.
/// Only the small real response and a 3 KiB generation block live in the parent.
fn write_warning_expansion(response: &Value, path: &Path, count: usize) -> u64 {
    const MARKER: &str = "__worker_startup_warning_array__";
    assert!(count > 0);
    let mut template = response.clone();
    template["result"]["warnings"] = json!(MARKER);
    let encoded = serde_json::to_vec(&template).unwrap();
    let marker = serde_json::to_vec(MARKER).unwrap();
    let offset = encoded
        .windows(marker.len())
        .position(|bytes| bytes == marker)
        .unwrap();
    let mut output = BufWriter::new(File::create_new(path).unwrap());
    output.write_all(&encoded[..offset]).unwrap();
    output.write_all(b"[\"\"").unwrap();
    let block = b",\"\"".repeat(1024);
    for _ in 0..(count - 1) / 1024 {
        output.write_all(&block).unwrap();
    }
    output
        .write_all(&block[..((count - 1) % 1024) * 3])
        .unwrap();
    output.write_all(b"]").unwrap();
    output.write_all(&encoded[offset + marker.len()..]).unwrap();
    output.flush().unwrap();
    fs::metadata(path).unwrap().len()
}

fn assert_no_publication(root: &Path, run: &Run) {
    assert!(!run.status.success());
    assert_eq!(fs::metadata(&run.stdout).unwrap().len(), 0);
    for name in [
        "ledger.sqlite",
        "ledger.sqlite-wal",
        "ledger.sqlite-shm",
        "receipt.json",
        "out",
    ] {
        assert!(
            !root.join(name).exists(),
            "unexpected publication artifact: {name}"
        );
    }
}

#[test]
fn oversized_and_expanding_result_json_is_rejected_before_ledger_publication() {
    let root = TempDir::new().unwrap();
    let seed = run_worker(
        root.path(),
        "seed",
        "extract",
        DEFAULT_GROWTH,
        &extraction_request(),
    );
    assert!(seed.status.success(), "{}", seed.stderr);
    let response: Value = serde_json::from_slice(&bounded_read(&seed.stdout)).unwrap();
    assert_fixture_result(&response["result"]);
    assert_limits(&response["limits"], DEFAULT_GROWTH);

    // A successful small control at the SAME allowance establishes that the
    // publisher can install its limit and decode/publish this generated shape.
    // A platform that cannot run this control lacks decoder-pressure evidence;
    // setup failure must never count as a passing allocation-pressure test.
    let control = root.path().join("control.json");
    write_warning_expansion(&response, &control, 32);
    let control_artifacts = root.path().join("control-artifacts");
    let request = publication_request(&control_artifacts, &control, MIB);
    let published = run_worker(root.path(), "control", "publish", LOW_GROWTH, &request);
    evidence("pressure-control", "publish", LOW_GROWTH, &published, None);
    let value = assert_published(&control_artifacts, &published, LOW_GROWTH);
    evidence(
        "pressure-control-limits",
        "publish",
        LOW_GROWTH,
        &published,
        Some(&value["worker_limits"]["publication"]),
    );
    assert_eq!(value["warnings"].as_array().unwrap().len(), 32);

    let hostile = root.path().join("expanding.json");
    let warning_count = 4 * 1024 * 1024;
    let bytes = write_warning_expansion(&response, &hostile, warning_count);
    assert!(bytes < 64 * MIB, "test generator exceeded its disk budget");
    eprintln!(
        "publisher-pressure-input {}",
        json!({"bytes": bytes, "warning_count": warning_count, "generation_block_bytes": 3072})
    );

    let oversized_artifacts = root.path().join("oversized-artifacts");
    let request = publication_request(&oversized_artifacts, &hostile, bytes - 1);
    let oversized = run_worker(
        root.path(),
        "oversized",
        "publish",
        DEFAULT_GROWTH,
        &request,
    );
    evidence("oversized", "publish", DEFAULT_GROWTH, &oversized, None);
    assert_no_publication(&oversized_artifacts, &oversized);
    assert!(oversized.stderr.contains("exceeds"), "{}", oversized.stderr);

    let pressure_artifacts = root.path().join("pressure-artifacts");
    let request = publication_request(&pressure_artifacts, &hostile, 64 * MIB);
    let pressure = run_worker(root.path(), "expanding", "publish", LOW_GROWTH, &request);
    evidence("expanding", "publish", LOW_GROWTH, &pressure, None);
    assert_no_publication(&pressure_artifacts, &pressure);
    assert!(allocation_failure(&pressure.stderr), "{}", pressure.stderr);
    assert!(
        !pressure
            .stderr
            .to_ascii_lowercase()
            .contains("address-space"),
        "limit setup rejection is not decoder-pressure evidence: {}",
        pressure.stderr
    );
    assert!(
        !pressure.stderr.contains("exceeds"),
        "capture rejection did not exercise decoding"
    );
}
