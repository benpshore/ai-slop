//! Exercise real shipped worker processes, not a mock allocator or parser.
mod common;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use lopdf::{Object, Stream, dictionary};
use serde_json::Value;
use tempfile::TempDir;

fn command(root: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tpe"));
    cmd.args(["extract", "--backend", "lopdf", "--json", "--db"])
        .arg(root.join("ledger.sqlite"))
        .arg("--out")
        .arg(root.join("out"));
    cmd
}

fn record(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "{error}: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/native-worker")
        .join(name)
}

fn wait_bounded(child: &mut Child, timeout: Duration) -> std::process::ExitStatus {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("child exceeded test deadline");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn no_outputs(root: &Path) {
    let out = root.join("out");
    assert!(!out.exists() || fs::read_dir(out).unwrap().next().is_none());
    if root.join("ledger.sqlite").exists() {
        let ledger = tpe::ledger::Ledger::open(&root.join("ledger.sqlite")).unwrap();
        assert_eq!(ledger.stats().unwrap().runs, 0);
    }
}

#[test]
fn native_and_existing_ocr_survive_bounded_workers_and_no_clobber_publication() {
    for name in ["native.pdf", "existing-ocr.pdf"] {
        let root = TempDir::new().unwrap();
        let input = fixture(name);
        let original = fs::read(&input).unwrap();
        let first = command(root.path()).arg(&input).output().unwrap();
        assert!(
            first.status.success(),
            "{}",
            String::from_utf8_lossy(&first.stdout)
        );
        let a = record(&first);
        assert_eq!(a["status"], "complete");
        assert!(
            a["pages"][0]["text"]
                .as_str()
                .unwrap()
                .contains("Existing OCR already reads this sentence.")
        );
        for phase in ["extraction", "publication"] {
            let limits = &a["worker_limits"][phase];
            assert_eq!(limits["kind"], "address_space_growth");
            assert_eq!(limits["requested_growth_bytes"], 1024 * 1024 * 1024_u64);
            let startup = limits["startup_virtual_bytes"].as_u64().unwrap();
            let hard = limits["effective_address_space_bytes"].as_u64().unwrap();
            assert!(hard > startup && hard <= startup + 1024 * 1024 * 1024);
            assert_eq!(limits["core_dump_bytes"], 0);
        }
        let first_paths = a["output_paths"].as_array().unwrap();
        assert_eq!(first_paths.len(), 2);
        let exported: Value =
            serde_json::from_slice(&fs::read(first_paths[0].as_str().unwrap()).unwrap()).unwrap();
        let mut streamed = a.clone();
        streamed.as_object_mut().unwrap().remove("output_paths");
        streamed.as_object_mut().unwrap().remove("worker_limits");
        assert_eq!(
            streamed, exported,
            "stdout must preserve typed coordinate serialization"
        );
        let first_bytes: Vec<_> = first_paths
            .iter()
            .map(|p| fs::read(p.as_str().unwrap()).unwrap())
            .collect();
        let second = command(root.path()).arg(&input).output().unwrap();
        assert!(second.status.success());
        assert_ne!(a["output_paths"], record(&second)["output_paths"]);
        for (path, bytes) in first_paths.iter().zip(first_bytes) {
            assert_eq!(fs::read(path.as_str().unwrap()).unwrap(), bytes);
        }
        assert_eq!(fs::read(input).unwrap(), original);
        assert_eq!(fs::read_dir(root.path().join("out")).unwrap().count(), 4);
    }
}

#[test]
fn limits_fail_per_document_without_publication_and_clean_captures() {
    for flags in [
        ["--max-bytes", "1"],
        ["--max-output-bytes", "1024"],
        ["--timeout-ms", "1"],
    ] {
        let root = TempDir::new().unwrap();
        let captures = root.path().join("captures");
        fs::create_dir(&captures).unwrap();
        let input = fixture("native.pdf");
        let original = fs::read(&input).unwrap();
        let output = command(root.path())
            .args(flags)
            .arg(&input)
            .env("TMPDIR", &captures)
            .env("TMP", &captures)
            .output()
            .unwrap();
        assert!(!output.status.success(), "{flags:?}");
        let value = record(&output);
        assert_eq!(value["status"], "failed", "{flags:?}: {value}");
        no_outputs(root.path());
        assert_eq!(fs::read_dir(captures).unwrap().count(), 0);
        assert_eq!(fs::read(input).unwrap(), original);
    }
}

#[test]
fn malformed_input_does_not_stop_later_input_and_folders_are_nonrecursive() {
    let root = TempDir::new().unwrap();
    let folder = root.path().join("input");
    fs::create_dir_all(folder.join("nested")).unwrap();
    fs::write(folder.join("a.pdf"), b"not a PDF").unwrap();
    fs::copy(fixture("native.pdf"), folder.join("b.PDF")).unwrap();
    fs::write(folder.join("ignored.txt"), b"not a PDF").unwrap();
    fs::write(folder.join("nested/c.pdf"), b"not a PDF").unwrap();
    let output = command(root.path()).arg(folder).output().unwrap();
    assert!(!output.status.success());
    let rows: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["status"], "failed");
    assert_eq!(rows[1]["status"], "complete");
}

#[test]
fn excessive_selection_and_non_native_options_fail_before_writes() {
    for flags in [
        ["--max-files", "1"],
        ["--jobs", "5"],
        ["--max-memory-growth-mib", "0"],
        ["--figures-dir", "unused"],
    ] {
        let root = TempDir::new().unwrap();
        let output = command(root.path())
            .args(flags)
            .arg(fixture("native.pdf"))
            .arg(fixture("existing-ocr.pdf"))
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(!root.path().join("ledger.sqlite").exists());
        assert!(!root.path().join("out").exists());
    }
}

#[cfg(unix)]
#[test]
fn ledger_and_sidecar_aliases_never_modify_selected_sources() {
    for sidecar in ["", "-wal", "-shm", "-journal"] {
        for symlink in [false, true] {
            let root = TempDir::new().unwrap();
            let input = root.path().join("paper.pdf");
            fs::copy(fixture("native.pdf"), &input).unwrap();
            let original = fs::read(&input).unwrap();
            let alias = root.path().join(format!("ledger.sqlite{sidecar}"));
            if symlink {
                std::os::unix::fs::symlink(&input, alias).unwrap();
            } else {
                fs::hard_link(&input, alias).unwrap();
            }
            let output = command(root.path()).arg(&input).output().unwrap();
            assert!(!output.status.success());
            assert!(String::from_utf8_lossy(&output.stderr).contains("aliases input"));
            assert_eq!(fs::read(input).unwrap(), original);
        }
    }
}

#[cfg(unix)]
#[test]
fn output_aliases_are_skipped_without_writing_through_them() {
    for symlink in [false, true] {
        let root = TempDir::new().unwrap();
        let input = root.path().join("paper.pdf");
        fs::copy(fixture("native.pdf"), &input).unwrap();
        let original = fs::read(&input).unwrap();
        let hash = tpe::schema::sha256_hex(&original);
        let out = root.path().join("out");
        fs::create_dir(&out).unwrap();
        let collision = out.join(format!("{hash}.json"));
        if symlink {
            std::os::unix::fs::symlink(&input, &collision).unwrap();
        } else {
            fs::hard_link(&input, &collision).unwrap();
        }
        let output = command(root.path()).arg(&input).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        let value = record(&output);
        assert_ne!(
            value["output_paths"][0].as_str().unwrap(),
            collision.to_str().unwrap()
        );
        assert_eq!(fs::read(input).unwrap(), original);
        assert_eq!(fs::read(collision).unwrap(), original);
    }
}

fn compressed_pressure_pdf() -> Vec<u8> {
    // Stream a 2 GiB expansion through a 1 MiB scratch buffer; the test
    // generator never holds the expanded adversarial document in memory.
    // Use the supported default budget below: macOS allocator reservations
    // can reject even the small native control at a 256 MiB growth allowance.
    let mut compressor = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    let block = vec![b' '; 1024 * 1024];
    for _ in 0..2048 {
        compressor.write_all(&block).unwrap();
    }
    let content = compressor.finish().unwrap();
    let mut doc = lopdf::Document::load_mem(&common::synthetic_paper()).unwrap();
    let stream = doc.add_object(Stream::new(
        dictionary! {"Filter" => "FlateDecode"},
        content,
    ));
    let page = doc.get_pages()[&1];
    doc.get_object_mut(page)
        .unwrap()
        .as_dict_mut()
        .unwrap()
        .set("Contents", stream);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    bytes
}

#[test]
fn allocation_pressure_is_contained_and_the_next_document_succeeds() {
    let root = TempDir::new().unwrap();
    let input = root.path().join("pressure.pdf");
    let bytes = compressed_pressure_pdf();
    assert!(
        bytes.len() < 32 * 1024 * 1024,
        "{} compressed bytes",
        bytes.len()
    );
    fs::write(&input, &bytes).unwrap();
    let output = command(root.path())
        .args(["--max-memory-growth-mib", "1024", "--timeout-ms", "10000"])
        .arg(fixture("native.pdf"))
        .arg(&input)
        .arg(fixture("existing-ocr.pdf"))
        .output()
        .unwrap();
    eprintln!(
        "pressure-evidence exit={} expected_decoded_bytes={} input_bytes={} input_sha256={} stdout={} stderr={}",
        output.status,
        2 * 1024 * 1024 * 1024_u64,
        bytes.len(),
        hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&bytes)),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(!output.status.success());
    let rows: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["status"], "complete", "{}", rows[0]);
    assert_eq!(rows[1]["status"], "failed", "{}", rows[1]);
    assert!(
        rows[1]["error"]
            .as_str()
            .unwrap()
            .contains("memory allocation failed in native worker"),
        "{}",
        rows[1]
    );
    assert_eq!(rows[2]["status"], "complete", "{}", rows[2]);
    assert_eq!(fs::read(input).unwrap(), bytes);
    let ledger = tpe::ledger::Ledger::open(&root.path().join("ledger.sqlite")).unwrap();
    assert_eq!(ledger.stats().unwrap().runs, 2);
}

fn many_pages_pdf(count: u32) -> Vec<u8> {
    let mut doc = lopdf::Document::load_mem(&common::synthetic_paper()).unwrap();
    let template = doc.get_object(doc.get_pages()[&1]).unwrap().clone();
    let parent = template
        .as_dict()
        .unwrap()
        .get(b"Parent")
        .unwrap()
        .as_reference()
        .unwrap();
    let kids: Vec<Object> = (0..count)
        .map(|_| Object::Reference(doc.add_object(template.clone())))
        .collect();
    let pages = doc.get_object_mut(parent).unwrap().as_dict_mut().unwrap();
    pages.set("Kids", kids);
    pages.set("Count", count);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    bytes
}

#[cfg(unix)]
fn child_pid(parent: &mut Child) -> String {
    let parent_pid = parent.id().to_string();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let ps = Command::new("ps")
            .args(["-axo", "pid=,ppid="])
            .output()
            .unwrap();
        let found = String::from_utf8(ps.stdout)
            .unwrap()
            .lines()
            .find_map(|line| {
                let mut fields = line.split_whitespace();
                let pid = fields.next()?;
                (fields.next()? == parent_pid).then(|| pid.to_owned())
            });
        if let Some(pid) = found {
            return pid;
        }
        if Instant::now() >= deadline || parent.try_wait().unwrap().is_some() {
            let _ = parent.kill();
            let _ = parent.wait();
            panic!("worker did not become observable");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(unix)]
fn signal(pid: &str, name: &str) {
    assert!(
        Command::new("kill")
            .args([name, pid])
            .status()
            .unwrap()
            .success()
    );
}

#[cfg(unix)]
fn not_running(pid: &str) -> bool {
    !Command::new("kill")
        .args(["-0", pid])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
}

#[cfg(unix)]
#[test]
fn deadline_and_cancellation_kill_and_reap_a_stopped_worker() {
    for cancel in [None, Some("-INT"), Some("-TERM")] {
        let root = TempDir::new().unwrap();
        let bytes = many_pages_pdf(2000);
        let input = root.path().join("many-pages.pdf");
        fs::write(&input, &bytes).unwrap();
        let output_path = root.path().join("stdout");
        let mut child = command(root.path())
            .args(["--timeout-ms", "1000"])
            .arg(&input)
            .stdout(fs::File::create(&output_path).unwrap())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let worker_pid = child_pid(&mut child);
        signal(&worker_pid, "-STOP");
        if let Some(name) = cancel {
            signal(&child.id().to_string(), name);
        }
        assert!(!wait_bounded(&mut child, Duration::from_secs(5)).success());
        let value: Value = serde_json::from_slice(&fs::read(output_path).unwrap()).unwrap();
        assert_eq!(value["status"], "failed");
        assert!(
            value["error"]
                .as_str()
                .unwrap()
                .contains(if cancel.is_some() {
                    "cancelled"
                } else {
                    "timed out"
                })
        );
        assert!(
            not_running(&worker_pid),
            "worker must have been killed and reaped"
        );
        no_outputs(root.path());
        assert_eq!(fs::read(input).unwrap(), bytes);
    }
}

#[cfg(unix)]
#[test]
fn lease_eof_terminates_a_worker_blocked_opening_a_request() {
    let root = TempDir::new().unwrap();
    let fifo = root.path().join("blocked-request");
    assert!(
        Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    let mut worker = Command::new(env!("CARGO_BIN_EXE_tpe"))
        .arg("native-worker")
        .arg(&fifo)
        .args([
            "--phase",
            "extract",
            "--growth-bytes",
            "268435456",
            "--parent",
        ])
        .arg(std::process::id().to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let lease = worker.stdin.take().unwrap();
    std::thread::sleep(Duration::from_millis(100));
    assert!(worker.try_wait().unwrap().is_none());
    drop(lease);
    assert!(!wait_bounded(&mut worker, Duration::from_secs(3)).success());
}

#[cfg(unix)]
#[test]
fn full_stdout_pipe_obeys_deadline_after_commit_without_changing_caller_flags() {
    use std::os::fd::AsFd;
    let root = TempDir::new().unwrap();
    let input = root.path().join("paper.pdf");
    fs::write(&input, many_pages_pdf(50)).unwrap();
    // Retain a duplicate of the write end so any shared-OFD flag mutation can
    // be detected even after the controller exits or is abruptly killed.
    let (read_end, write_end) = rustix::pipe::pipe().unwrap();
    let flags = rustix::fs::fcntl_getfl(&write_end).unwrap();
    let mut child = command(root.path())
        .args(["--timeout-ms", "3000"])
        .arg(input)
        .stdout(Stdio::from(write_end.as_fd().try_clone_to_owned().unwrap()))
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    assert!(!wait_bounded(&mut child, Duration::from_secs(5)).success());
    let after = rustix::fs::fcntl_getfl(&write_end).unwrap();
    let changed = (after ^ flags).bits();
    // Darwin exposes FWASWRITTEN (0x10000) through F_GETFL. The kernel sets
    // this history bit when bytes are written; it is not a caller-settable
    // status flag. Keep checking every other bit, including O_NONBLOCK.
    // https://github.com/apple-oss-distributions/xnu/blob/xnu-11417.140.69/bsd/sys/fcntl.h#L135
    #[cfg(target_os = "macos")]
    let changed = changed & !0x0001_0000;
    assert_eq!(changed, 0, "caller flags changed: {flags:?} -> {after:?}");
    let ledger = tpe::ledger::Ledger::open(&root.path().join("ledger.sqlite")).unwrap();
    assert_eq!(
        ledger.stats().unwrap().runs,
        1,
        "publication preceded blocked delivery"
    );
    drop(write_end);
    let mut stdout = Vec::new();
    std::io::Read::read_to_end(&mut fs::File::from(read_end), &mut stdout).unwrap();
    assert!(!stdout.is_empty());
    assert!(!String::from_utf8_lossy(&stdout).contains("\"status\":\"failed\""));
}

#[cfg(unix)]
#[test]
fn cancellation_does_not_launch_relays_for_the_whole_pending_batch() {
    let root = TempDir::new().unwrap();
    let input = root.path().join("many-pages.pdf");
    fs::write(&input, many_pages_pdf(2000)).unwrap();
    let output_path = root.path().join("stdout");
    let mut cmd = command(root.path());
    for _ in 0..256 {
        cmd.arg(&input);
    }
    let mut controller = cmd
        .stdout(fs::File::create(&output_path).unwrap())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let worker = child_pid(&mut controller);
    signal(&worker, "-STOP");
    signal(&controller.id().to_string(), "-TERM");
    assert!(!wait_bounded(&mut controller, Duration::from_secs(2)).success());
    let output = fs::read_to_string(output_path).unwrap();
    assert!(
        output.lines().count() <= 1,
        "queued cancellations must not spawn per-file relays"
    );
    assert!(not_running(&worker));
    no_outputs(root.path());
}

#[cfg(target_os = "linux")]
#[test]
fn kernel_parent_death_signal_terminates_even_a_stopped_parser() {
    let root = TempDir::new().unwrap();
    let input = root.path().join("many-pages.pdf");
    fs::write(&input, many_pages_pdf(2000)).unwrap();
    let mut controller = command(root.path())
        .arg(&input)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let worker = child_pid(&mut controller);
    // Do not stop the child until it has actually installed the limit and
    // PDEATHSIG. This distinguishes kernel protection from the EOF fallback.
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let limits = fs::read_to_string(format!("/proc/{worker}/limits")).unwrap();
        let installed = limits
            .lines()
            .any(|line| line.starts_with("Max address space") && !line.contains("unlimited"));
        if installed {
            break;
        }
        if Instant::now() >= deadline {
            let _ = controller.kill();
            let _ = controller.wait();
            panic!("worker did not install limits");
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    signal(&worker, "-STOP");
    controller.kill().unwrap();
    controller.wait().unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let stat = fs::read_to_string(format!("/proc/{worker}/stat"));
        if stat.is_err()
            || stat.as_ref().is_ok_and(|s| {
                s.rsplit_once(')')
                    .is_some_and(|(_, fields)| fields.trim_start().starts_with('Z'))
            })
        {
            break;
        }
        if Instant::now() >= deadline {
            signal(&worker, "-KILL");
            panic!("stopped parser survived controller death");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    // Orphans can be zombies until PID 1 reaps them; this assertion is death,
    // not a claim that a dead controller can reap its former children.
    no_outputs(root.path());
}

#[test]
fn oversized_hidden_worker_request_is_rejected_before_json_decode() {
    let root = TempDir::new().unwrap();
    let request = root.path().join("request");
    fs::write(&request, vec![b' '; 128 * 1024]).unwrap();
    let stderr = root.path().join("stderr");
    let mut worker = Command::new(env!("CARGO_BIN_EXE_tpe"))
        .arg("native-worker")
        .arg(request)
        .args([
            "--phase",
            "extract",
            "--growth-bytes",
            "268435456",
            "--parent",
        ])
        .arg(std::process::id().to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(fs::File::create(&stderr).unwrap())
        .spawn()
        .unwrap();
    let _lease = worker.stdin.take().unwrap();
    assert!(!wait_bounded(&mut worker, Duration::from_secs(3)).success());
    assert!(
        fs::read_to_string(stderr)
            .unwrap()
            .contains("exceeds 65536 bytes")
    );
}

#[test]
fn sqlite_uri_and_temporary_ledger_names_are_rejected_before_writes() {
    let root = TempDir::new().unwrap();
    let input = root.path().join("paper.pdf");
    fs::copy(fixture("native.pdf"), &input).unwrap();
    let original = fs::read(&input).unwrap();
    let encoded = input.to_string_lossy().replace('p', "%70");
    for db in [
        format!("file:{}?mode=rwc", input.display()),
        format!("file:{encoded}"),
        ":memory:".into(),
        String::new(),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_tpe"))
            .args(["extract", "--json", "--db", &db])
            .arg(&input)
            .output()
            .unwrap();
        assert!(!output.status.success());
        if db.is_empty() {
            assert!(
                !output.stderr.is_empty(),
                "clap rejects an empty FILE argument"
            );
        } else {
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("literal persistent file path")
            );
        }
        assert_eq!(fs::read(&input).unwrap(), original);
    }
}

#[cfg(unix)]
#[test]
fn canonical_ledger_sidecars_cannot_delete_an_input_behind_a_symlink() {
    for link_parent in [false, true] {
        let root = TempDir::new().unwrap();
        let actual = root.path().join("real");
        fs::create_dir(&actual).unwrap();
        let real_db = actual.join("ledger.sqlite");
        fs::write(&real_db, b"").unwrap();
        let input = actual.join("ledger.sqlite-wal");
        fs::copy(fixture("native.pdf"), &input).unwrap();
        let original = fs::read(&input).unwrap();
        let db = if link_parent {
            let parent = root.path().join("alias");
            std::os::unix::fs::symlink(&actual, &parent).unwrap();
            parent.join("ledger.sqlite")
        } else {
            let alias = root.path().join("alias.sqlite");
            std::os::unix::fs::symlink(&real_db, &alias).unwrap();
            alias
        };
        let output = Command::new(env!("CARGO_BIN_EXE_tpe"))
            .args(["extract", "--json", "--db"])
            .arg(&db)
            .arg(&input)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("aliases input"));
        assert_eq!(fs::read(input).unwrap(), original);
        assert!(fs::read(real_db).unwrap().is_empty());
    }
}
