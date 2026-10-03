//! Disposable native workers with hard address-space limits. The controller
//! never decodes document results: it transfers private capture paths to a
//! separately bounded publisher, then streams the publisher's validated output.
//! This is resource containment, not a security sandbox.

use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use tempfile::TempDir;
use tpe::ledger::Ledger;
use tpe::pipeline;
use tpe::publication::StagedOutputs;
use tpe::schema::{ExtractionResult, Job, Status};

use super::ExtractArgs;
use super::worker_limits::{self, LimitEvidence};

const MAX_INPUT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_CAPTURE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_DIRECTORY_ENTRIES: usize = 10_000;
const REQUEST_BYTES: u64 = 64 * 1024;
const DIAGNOSTIC_BYTES: u64 = 16 * 1024;

#[derive(Serialize, Deserialize)]
struct ExtractRequest {
    job: Job,
    progress: bool,
}

#[derive(Serialize, Deserialize)]
struct Response {
    version: u32,
    result: ExtractionResult,
    limits: LimitEvidence,
}

#[derive(Serialize, Deserialize)]
struct PublishRequest {
    response: PathBuf,
    receipt: PathBuf,
    db: PathBuf,
    out: Option<PathBuf>,
    path: PathBuf,
    pages: Option<(u32, u32)>,
    json: bool,
    max_output_bytes: u64,
}

#[derive(Serialize)]
struct Published<'a> {
    #[serde(flatten)]
    result: &'a ExtractionResult,
    output_paths: &'a [PathBuf],
    worker_limits: WorkerEvidence<'a>,
}

#[derive(Serialize)]
struct WorkerEvidence<'a> {
    extraction: &'a LimitEvidence,
    publication: &'a LimitEvidence,
}

#[derive(Serialize, Deserialize)]
struct Receipt {
    version: u32,
    status: Status,
    output_bytes: u64,
}

#[derive(Serialize, Deserialize)]
struct DeliveryRequest {
    source: PathBuf,
    max_bytes: u64,
}

#[derive(Clone, Copy)]
enum OutputSink {
    Capture,
    Stdout,
    Stderr,
}

struct Capture {
    directory: TempDir,
    stdout: PathBuf,
    stderr: PathBuf,
}

struct Outcome {
    path: PathBuf,
    started: Instant,
    result: anyhow::Result<Capture>,
}

/// Validated and bounded before any document or output is opened.
fn input_paths(args: &ExtractArgs) -> anyhow::Result<Vec<PathBuf>> {
    check_supervised_backend(&args.backend)?;
    ensure!((1..=4).contains(&args.jobs), "--jobs must be 1..=4");
    ensure!(
        (1..=300_000).contains(&args.timeout_ms),
        "--timeout-ms must be 1..=300000"
    );
    ensure!(
        (32..=4096).contains(&args.max_memory_growth_mib),
        "--max-memory-growth-mib must be 32..=4096"
    );
    ensure!(
        (1024..=MAX_CAPTURE_BYTES).contains(&args.max_output_bytes),
        "--max-output-bytes must be 1024..=268435456"
    );
    ensure!(
        (1..=MAX_DIRECTORY_ENTRIES).contains(&args.max_files),
        "--max-files must be 1..=10000"
    );
    ensure!(
        args.max_bytes.is_none_or(|n| n > 0),
        "--max-bytes must be positive"
    );
    ensure!(
        args.figures_dir.is_none(),
        "supervised extraction exports text/JSON only; omit --figures-dir (figure metadata is retained)"
    );
    let mut paths = Vec::new();
    for input in &args.paths {
        if input.is_dir() {
            let mut found = Vec::new();
            for (seen, entry) in fs::read_dir(input)
                .with_context(|| format!("reading folder {}", input.display()))?
                .enumerate()
            {
                ensure!(
                    seen < MAX_DIRECTORY_ENTRIES,
                    "folder {} exceeds the 10000-entry scan limit",
                    input.display()
                );
                let entry = entry?;
                let path = entry.path();
                if entry.file_type()?.is_file()
                    && path
                        .extension()
                        .is_some_and(|s| s.as_encoded_bytes().eq_ignore_ascii_case(b"pdf"))
                {
                    found.push(path);
                    ensure!(
                        paths.len() + found.len() <= args.max_files,
                        "input exceeds --max-files {}; no files were processed",
                        args.max_files
                    );
                }
            }
            found.sort();
            paths.extend(found);
        } else {
            paths.push(input.clone());
        }
        ensure!(
            paths.len() <= args.max_files,
            "input exceeds --max-files {}; no files were processed",
            args.max_files
        );
    }
    ensure!(
        !paths.is_empty(),
        "no regular PDF files in the selected folders (nonrecursive)"
    );
    protect_sources(&paths, &args.db)?;
    Ok(paths)
}

fn check_supervised_backend(name: &str) -> anyhow::Result<()> {
    ensure!(
        matches!(name, "lopdf" | "pdfium" | "auto"),
        "supervised extraction supports native lopdf/pdfium only; OCR backends may spawn unsupervised children"
    );
    ensure!(
        name != "auto" || !tpe::backend::available().contains(&"docling"),
        "auto may route to OCR in this build; choose --backend lopdf or --backend pdfium"
    );
    super::check_backend(name)
}

// Check the ledger and SQLite sidecars against every selected source before a
// publisher can create them. Existing aliases are compared by file identity.
// This assumes the user controls the directory namespace; it is not protection
// against an adversary continually changing symlinks during publication.
fn protect_sources(paths: &[PathBuf], db: &Path) -> anyhow::Result<PathBuf> {
    ensure!(
        !db.as_os_str().is_empty()
            && db.as_os_str().as_encoded_bytes() != b":memory:"
            && !db.as_os_str().as_encoded_bytes().starts_with(b"file:"),
        "--db requires a literal persistent file path, not a SQLite URI or in-memory name"
    );
    // SQLite resolves symlinks before choosing sidecar names. Use that actual
    // location too, otherwise link.sqlite -> real.sqlite could delete an input
    // named real.sqlite-wal when SQLite opens an empty database.
    let canonical = match db.canonicalize() {
        Ok(path) => path,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            ensure!(
                fs::symlink_metadata(db).is_err(),
                "ledger path is an unresolved symlink"
            );
            let parent = db
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            parent
                .canonicalize()
                .context("resolving ledger parent")?
                .join(db.file_name().context("ledger filename is missing")?)
        }
        Err(error) => return Err(error).context("resolving ledger path"),
    };
    let mut destinations = vec![db.to_path_buf(), canonical.clone()];
    for base in [db, canonical.as_path()] {
        for suffix in ["-wal", "-shm", "-journal"] {
            let mut name = base.as_os_str().to_owned();
            name.push(suffix);
            destinations.push(PathBuf::from(name));
        }
    }
    for source in paths {
        let Ok(source_meta) = fs::metadata(source) else {
            continue;
        };
        for destination in &destinations {
            if let Ok(destination_meta) = fs::metadata(destination) {
                #[cfg(unix)]
                let same = {
                    use std::os::unix::fs::MetadataExt;
                    source_meta.dev() == destination_meta.dev()
                        && source_meta.ino() == destination_meta.ino()
                };
                #[cfg(not(unix))]
                let same = source.canonicalize()? == destination.canonicalize()?;
                ensure!(
                    !same,
                    "ledger or sidecar aliases input {}; choose a different --db",
                    source.display()
                );
            }
        }
    }
    Ok(canonical)
}

pub(super) fn run(args: &ExtractArgs) -> anyhow::Result<ExitCode> {
    let paths = input_paths(args)?;
    let cancelled = Arc::new(AtomicBool::new(false));
    let _signals = CancelSignals::install(&cancelled)?;
    let queue = Mutex::new(VecDeque::from(paths));
    // Only a path plus a TempDir crosses this channel. Captures are bounded by
    // the worker count plus current publication/delivery, never decoded here.
    let (sender, receiver) = mpsc::sync_channel::<Outcome>(0);
    let mut any_failed = false;
    let publication = std::thread::scope(|scope| -> anyhow::Result<()> {
        let mut cancel_on_exit = CancelOnExit {
            flag: &cancelled,
            armed: true,
        };
        for _ in 0..args.jobs {
            let sender = sender.clone();
            let queue = &queue;
            let cancelled = &cancelled;
            scope.spawn(move || {
                loop {
                    if cancelled.load(Ordering::Relaxed) {
                        break;
                    }
                    let Some(path) = queue.lock().expect("input queue poisoned").pop_front() else {
                        break;
                    };
                    let started = Instant::now();
                    let result = extract(args, &path, started, cancelled);
                    if sender
                        .send(Outcome {
                            path,
                            started,
                            result,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            });
        }
        drop(sender);
        for outcome in receiver {
            if cancelled.load(Ordering::Relaxed) {
                report_failure(
                    args,
                    &outcome.path,
                    outcome.started,
                    &anyhow::anyhow!("cancelled; remaining queued inputs were not processed"),
                )?;
                break;
            }
            let result = match outcome.result {
                Ok(capture) => {
                    if args.progress {
                        deliver(
                            args,
                            &capture.stderr,
                            true,
                            outcome.started + Duration::from_millis(args.timeout_ms),
                            &cancelled,
                        )?;
                    }
                    publish(args, &outcome.path, &capture, outcome.started, &cancelled)
                }
                Err(error) => Err(error),
            };
            match result {
                Ok((status, capture)) => {
                    deliver(args, &capture.stdout, false, outcome.started + Duration::from_millis(args.timeout_ms), &cancelled)
                        .context("publication committed, but output delivery incomplete; inspect the ledger before retrying")?;
                    any_failed |= status != Status::Complete;
                }
                Err(error) => {
                    any_failed = true;
                    report_failure(args, &outcome.path, outcome.started, &error)?;
                }
            }
        }
        cancel_on_exit.armed = false;
        Ok(())
    });
    // On a broken stdout/stderr the receiver drops, releasing blocked senders;
    // cancellation stops running parsers while their supervisors reap them.
    if let Err(error) = publication {
        // Do not append a failure object after part of a success record. Stop
        // the stream and return failure; diagnostic delivery gets only 100 ms.
        let _ = deliver_bytes(
            args,
            format!("{error:#}\n").as_bytes(),
            true,
            Duration::from_millis(100),
        );
        return Ok(ExitCode::FAILURE);
    }
    Ok(super::exit_code(
        any_failed || cancelled.load(Ordering::Relaxed),
    ))
}

struct CancelOnExit<'a> {
    flag: &'a AtomicBool,
    armed: bool,
}

impl Drop for CancelOnExit<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.flag.store(true, Ordering::Relaxed);
        }
    }
}

struct CancelSignals(Vec<signal_hook::SigId>);

impl CancelSignals {
    fn install(cancelled: &Arc<AtomicBool>) -> anyhow::Result<Self> {
        let mut registered = Self(Vec::new());
        for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
            registered
                .0
                .push(signal_hook::flag::register(signal, Arc::clone(cancelled))?);
        }
        Ok(registered)
    }
}

impl Drop for CancelSignals {
    fn drop(&mut self) {
        for signal in &self.0 {
            signal_hook::low_level::unregister(*signal);
        }
    }
}

// Holding stdin open is a liveness lease. Linux also installs PDEATHSIG, which
// works even when a child is stopped. On Darwin the EOF watchdog must run;
// normal cancellation always kills and reaps from this live supervisor.
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

fn extract(
    args: &ExtractArgs,
    path: &Path,
    started: Instant,
    cancelled: &AtomicBool,
) -> anyhow::Result<Capture> {
    let request = ExtractRequest {
        job: Job {
            path: path
                .to_str()
                .context("input path is not valid Unicode")?
                .to_owned(),
            backend: args.backend.clone(),
            pages: args.pages,
            password: args.password.clone(),
            max_bytes: Some(args.max_bytes.unwrap_or(MAX_INPUT_BYTES)),
            figures_dir: None,
        },
        progress: args.progress,
    };
    supervise(
        args,
        "extract",
        &request,
        started + Duration::from_millis(args.timeout_ms),
        cancelled,
        OutputSink::Capture,
    )
}

fn supervise(
    args: &ExtractArgs,
    phase: &str,
    request: &impl Serialize,
    deadline: Instant,
    cancelled: &AtomicBool,
    sink: OutputSink,
) -> anyhow::Result<Capture> {
    check_deadline(deadline, cancelled)?;
    let directory = TempDir::new().context("creating private worker directory")?;
    let capture = Capture {
        stdout: directory.path().join("stdout"),
        stderr: directory.path().join("stderr"),
        directory,
    };
    let request_path = capture.directory.path().join("request.json");
    let encoded = serde_json::to_vec(request)?;
    ensure!(
        encoded.len() as u64 <= REQUEST_BYTES,
        "worker request exceeds 64 KiB"
    );
    fs::write(&request_path, encoded)?;
    let captured_stdout = File::create_new(&capture.stdout)?;
    let stdout = match sink {
        OutputSink::Capture => Stdio::from(captured_stdout),
        OutputSink::Stdout => Stdio::inherit(),
        OutputSink::Stderr => stderr_as_stdio()?,
    };
    let mut child = Command::new(std::env::current_exe()?)
        .env("RAYON_NUM_THREADS", "1")
        .env("OMP_NUM_THREADS", "1")
        .arg("native-worker")
        .arg(&request_path)
        .arg("--phase")
        .arg(phase)
        .arg("--growth-bytes")
        .arg((args.max_memory_growth_mib * 1024 * 1024).to_string())
        .arg("--parent")
        .arg(std::process::id().to_string())
        .stdin(Stdio::piped())
        .stdout(stdout)
        .stderr(File::create_new(&capture.stderr)?)
        .spawn()
        .with_context(|| format!("starting {phase} worker"))?;
    let lease = child.stdin.take().context("worker stdin unavailable")?;
    let mut worker = Worker {
        child,
        _lease: lease,
    };
    let status = loop {
        check_deadline(deadline, cancelled)?;
        check_capture(&capture, args.max_output_bytes)?;
        if let Some(status) = worker.child.try_wait()? {
            break status;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    check_capture(&capture, args.max_output_bytes)?;
    if !status.success() {
        let diagnostics = read_limited(&capture.stderr, DIAGNOSTIC_BYTES, false)?;
        anyhow::bail!(
            "{phase} worker exited {status}: {}",
            String::from_utf8_lossy(&diagnostics)
        );
    }
    Ok(capture)
}

fn check_deadline(deadline: Instant, cancelled: &AtomicBool) -> anyhow::Result<()> {
    ensure!(!cancelled.load(Ordering::Relaxed), "cancelled");
    ensure!(Instant::now() < deadline, "document timed out");
    Ok(())
}

#[cfg(unix)]
fn stderr_as_stdio() -> anyhow::Result<Stdio> {
    use std::os::fd::AsFd;
    Ok(Stdio::from(io::stderr().as_fd().try_clone_to_owned()?))
}

#[cfg(not(unix))]
fn stderr_as_stdio() -> anyhow::Result<Stdio> {
    anyhow::bail!("bounded native extraction requires Linux or macOS")
}

fn deliver(
    args: &ExtractArgs,
    source: &Path,
    stderr: bool,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> anyhow::Result<()> {
    let request = DeliveryRequest {
        source: source.to_path_buf(),
        max_bytes: args.max_output_bytes,
    };
    let sink = if stderr {
        OutputSink::Stderr
    } else {
        OutputSink::Stdout
    };
    supervise(args, "deliver", &request, deadline, cancelled, sink)?;
    Ok(())
}

fn deliver_bytes(
    args: &ExtractArgs,
    bytes: &[u8],
    stderr: bool,
    timeout: Duration,
) -> anyhow::Result<()> {
    let directory = TempDir::new()?;
    let path = directory.path().join("message");
    fs::write(&path, bytes)?;
    deliver(
        args,
        &path,
        stderr,
        Instant::now() + timeout,
        &AtomicBool::new(false),
    )
}

fn check_capture(capture: &Capture, max: u64) -> anyhow::Result<()> {
    let bytes = fs::metadata(&capture.stdout)?
        .len()
        .saturating_add(fs::metadata(&capture.stderr)?.len());
    ensure!(
        bytes <= max,
        "worker output exceeded --max-output-bytes {max}"
    );
    Ok(())
}

fn publish(
    args: &ExtractArgs,
    path: &Path,
    source: &Capture,
    started: Instant,
    cancelled: &AtomicBool,
) -> anyhow::Result<(Status, Capture)> {
    check_deadline(started + Duration::from_millis(args.timeout_ms), cancelled)?;
    let receipt_path = source.directory.path().join("receipt.json");
    let request = PublishRequest {
        response: source.stdout.clone(),
        receipt: receipt_path.clone(),
        db: args.db.clone(),
        out: args.out.clone(),
        path: path.to_path_buf(),
        pages: args.pages,
        json: args.json,
        max_output_bytes: args.max_output_bytes,
    };
    let capture = supervise(
        args,
        "publish",
        &request,
        started + Duration::from_millis(args.timeout_ms),
        cancelled,
        OutputSink::Capture,
    )
    .context(
        "publication outcome unknown: inspect the ledger and complete output files before retrying",
    )?;
    let receipt = (|| -> anyhow::Result<Receipt> {
        let receipt: Receipt =
            serde_json::from_slice(&read_limited(&receipt_path, REQUEST_BYTES, true)?)?;
        ensure!(
            receipt.version == 1 && receipt.output_bytes == fs::metadata(&capture.stdout)?.len(),
            "invalid publisher receipt"
        );
        Ok(receipt)
    })()
    .context("publication outcome unknown: publisher receipt could not be verified")?;
    Ok((receipt.status, capture))
}

fn report_failure(
    args: &ExtractArgs,
    path: &Path,
    started: Instant,
    error: &anyhow::Error,
) -> anyhow::Result<()> {
    let error = format!("{error:#}");
    let status = if error.contains("publication outcome unknown") {
        "publication_unknown"
    } else {
        "failed"
    };
    let bytes = if args.json {
        let mut bytes = serde_json::to_vec(&serde_json::json!({
            "status": status, "path": path.to_string_lossy(), "error": error, "ms": super::elapsed_ms(started)
        }))?;
        bytes.push(b'\n');
        bytes
    } else {
        deliver_bytes(
            args,
            format!("{}: {error}\n", path.display()).as_bytes(),
            true,
            Duration::from_secs(1),
        )?;
        format!(
            "{status}\t-\t0p\t0 refs\t0 cites\t{:.1} ms\t{}\n",
            super::elapsed_ms(started),
            path.display()
        )
        .into_bytes()
    };
    deliver_bytes(args, &bytes, false, Duration::from_secs(1))?;
    Ok(())
}

fn read_limited(path: &Path, limit: u64, reject_oversize: bool) -> anyhow::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(limit + u64::from(reject_oversize))
        .read_to_end(&mut bytes)?;
    ensure!(
        !reject_oversize || bytes.len() as u64 <= limit,
        "{} exceeds {limit} bytes",
        path.display()
    );
    Ok(bytes)
}

pub(super) fn run_worker(
    request_path: &Path,
    phase: &str,
    growth_bytes: u64,
    parent: u32,
) -> anyhow::Result<ExitCode> {
    // Include the lease thread's initialized stdin/TLS in startup mappings.
    let (ready, initialized) = mpsc::sync_channel(0);
    std::thread::Builder::new()
        .name("controller-lease".into())
        .stack_size(128 * 1024)
        .spawn(move || {
            let mut stdin = io::stdin();
            if ready.send(()).is_err() {
                std::process::exit(1);
            }
            let mut byte = [0_u8; 1];
            let _ = stdin.read(&mut byte);
            std::process::exit(1);
        })?;
    initialized
        .recv()
        .context("initializing controller lease")?;
    let limits = worker_limits::install(growth_bytes, parent)?;
    super::worker_allocator::enforce();
    let encoded = read_limited(request_path, REQUEST_BYTES, true)?;
    match phase {
        "extract" => extract_worker(&serde_json::from_slice(&encoded)?, limits)?,
        "publish" => publish_worker(&serde_json::from_slice(&encoded)?, &limits)?,
        "deliver" => deliver_worker(&serde_json::from_slice(&encoded)?)?,
        _ => anyhow::bail!("unknown native worker phase"),
    }
    Ok(ExitCode::SUCCESS)
}

fn deliver_worker(request: &DeliveryRequest) -> anyhow::Result<()> {
    let mut source = File::open(&request.source)?;
    ensure!(
        source.metadata()?.len() <= request.max_bytes,
        "delivery exceeds output limit"
    );
    // A relay may block on a full inherited pipe. Its live controller retains
    // deadline/cancellation control and can kill/reap it, without altering the
    // caller's shared open-file-description flags (even after abrupt death).
    let mut buffer = [0_u8; 8192];
    let mut stdout = io::stdout().lock();
    let mut remaining = request.max_bytes;
    loop {
        let count = source.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        remaining = remaining
            .checked_sub(count as u64)
            .context("delivery exceeds output limit")?;
        stdout.write_all(&buffer[..count])?;
    }
    stdout.flush()?;
    Ok(())
}

fn extract_worker(request: &ExtractRequest, limits: LimitEvidence) -> anyhow::Result<()> {
    check_supervised_backend(&request.job.backend)?;
    ensure!(
        request.job.figures_dir.is_none(),
        "worker may not export figures"
    );
    let mut observe = |event| {
        if request.progress {
            super::report_progress(&request.job.path, event);
        }
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        pipeline::run_job_observed(&request.job, &mut observe)
    }))
    .map_err(|payload| anyhow::anyhow!("panic: {}", super::panic_message(&*payload)))??;
    serde_json::to_writer(
        io::stdout().lock(),
        &Response {
            version: 1,
            result,
            limits,
        },
    )?;
    Ok(())
}

fn validate_pages(result: &ExtractionResult, range: Option<(u32, u32)>) -> anyhow::Result<()> {
    let count = result.document.pages;
    ensure!(count > 0, "worker returned no document pages");
    let (first, last) = range.map_or((1, count), |(a, b)| (a, b.min(count)));
    ensure!(
        first <= last && result.pages.len() as u64 == u64::from(last - first) + 1,
        "worker page coverage does not match requested range {first}-{last} of {count}"
    );
    ensure!(
        result
            .pages
            .iter()
            .zip(first..=last)
            .all(|(page, number)| page.page == number),
        "worker returned missing, duplicate or unordered page outcomes"
    );
    Ok(())
}

fn publish_worker(request: &PublishRequest, limits: &LimitEvidence) -> anyhow::Result<()> {
    let response: Response = serde_json::from_slice(&read_limited(
        &request.response,
        request.max_output_bytes,
        true,
    )?)?;
    ensure!(
        response.version == 1,
        "unsupported extraction response version"
    );
    let mut result = response.result;
    validate_pages(&result, request.pages)?;
    let db = protect_sources(std::slice::from_ref(&request.path), &request.db)?;
    let started = Instant::now();
    let mut ledger = Ledger::open(&db)?;
    let pending = ledger.prepare_result(&result)?;
    result.timings.write_ms = super::elapsed_ms(started);
    pending.update_timings(&result.timings)?;
    let mut outputs = if let Some(directory) = &request.out {
        fs::create_dir_all(directory)?;
        let base = directory.join(format!("{}.pdf", result.document.hash.0));
        let json = serde_json::to_vec_pretty(&result)?;
        let text = result
            .pages
            .iter()
            .map(|page| page.text.as_str())
            .collect::<Vec<_>>()
            .join("\u{c}")
            .into_bytes();
        Some(StagedOutputs::stage(
            &base,
            &[(".json", json), (".txt", text)],
        )?)
    } else {
        None
    };
    let commit = |paths: &[PathBuf]| -> Result<(), String> {
        // Prepare and flush the entire controller response before SQL commit.
        // Allocation/serialization failures can still roll back at this point.
        let output = if request.json {
            // Serialize the typed result directly: converting f32 coordinates
            // through serde_json::Value would widen their decimal representation.
            let published = Published {
                result: &result,
                output_paths: paths,
                worker_limits: WorkerEvidence {
                    extraction: &response.limits,
                    publication: limits,
                },
            };
            let mut bytes = serde_json::to_vec(&published).map_err(|e| e.to_string())?;
            bytes.push(b'\n');
            bytes
        } else {
            format!(
                "{}\n",
                super::summary_line(&result, &request.path.to_string_lossy())
            )
            .into_bytes()
        };
        if output.len() as u64 > request.max_output_bytes {
            return Err("publisher response exceeds output limit".into());
        }
        let mut stdout = io::stdout().lock();
        stdout.write_all(&output).map_err(|e| e.to_string())?;
        stdout.flush().map_err(|e| e.to_string())?;
        let receipt = serde_json::to_vec(&Receipt {
            version: 1,
            status: result.status,
            output_bytes: output.len() as u64,
        })
        .map_err(|e| e.to_string())?;
        // Prepare the receipt before commit as well: a receipt alone does not
        // prove publication. The controller also requires a successful exit.
        fs::write(&request.receipt, receipt).map_err(|e| e.to_string())?;
        pending.commit().map_err(|e| e.to_string())?;
        Ok(())
    };
    if let Some(staged) = &mut outputs {
        staged
            .publish_then_with_paths(commit)
            .map_err(anyhow::Error::msg)?;
    } else {
        commit(&[]).map_err(anyhow::Error::msg)?;
    }
    Ok(())
}
