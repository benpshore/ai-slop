//! Streaming, bounded process supervision for mixed-document ingestion.
//! Workers own no shared extraction state and publish only through the parent.

use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::ingest::{Options, Outcome, Record};

const MAX_MANIFEST_LINE: u64 = 64 * 1024;
const MAX_STDERR: u64 = 16 * 1024;

pub const fn default_memory_mib() -> u64 {
    if cfg!(target_os = "linux") { 1024 } else { 0 }
}

#[derive(Clone, Debug, Serialize)]
pub struct Limits {
    pub jobs: usize,
    pub timeout_ms: u64,
    pub max_output_bytes: u64,
    /// Linux `RLIMIT_AS` (virtual address space), not an RSS measurement.
    pub memory_mib: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            jobs: 2,
            timeout_ms: 30_000,
            max_output_bytes: 64 * 1024 * 1024,
            memory_mib: default_memory_mib(),
        }
    }
}

#[derive(Serialize)]
pub struct BatchRecord {
    pub schema_version: &'static str,
    pub input_line: u64,
    pub worker_ms: f64,
    pub supervisor: Limits,
    pub result: Record,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    path: PathBuf,
}

struct Job {
    line: u64,
    path: PathBuf,
    error: Option<String>,
}

/// Run a JSONL manifest with at most `jobs` active worker processes. Results are
/// emitted in completion order, with the 1-based input line for correlation.
/// No list of all documents or results is ever materialized.
/// The caller owns signal handling. Use a fresh cancellation token per run;
/// this function sets it on exit to stop any outstanding work.
pub fn run(
    input: std::fs::File,
    output: std::fs::File,
    executable: &Path,
    options: &Options,
    limits: &Limits,
    cancelled: &Arc<AtomicBool>,
) -> Result<bool> {
    if !(1..=64).contains(&limits.jobs) {
        bail!("jobs must be between 1 and 64");
    }
    if limits.timeout_ms == 0 {
        bail!("timeout must be positive");
    }
    if !(1..=1024 * 1024 * 1024).contains(&limits.max_output_bytes) {
        bail!("max output must be between 1 byte and 1 GiB");
    }
    #[cfg(not(target_os = "linux"))]
    if limits.memory_mib != 0 {
        bail!(
            "a hard address-space limit is only implemented on Linux; use --worker-memory-mib 0 here"
        );
    }
    if limits.memory_mib > u64::MAX / (1024 * 1024) {
        bail!("memory limit overflow");
    }
    if !cfg!(unix) {
        bail!("isolated batch supervision currently requires Unix");
    }
    thread::scope(|scope| -> Result<bool> {
        let (send_jobs, recv_jobs) = mpsc::sync_channel::<Job>(limits.jobs);
        let receiver = Arc::new(Mutex::new(recv_jobs));
        let (send_results, recv_results) = mpsc::sync_channel(limits.jobs);
        for _ in 0..limits.jobs {
            let jobs = Arc::clone(&receiver);
            let results = send_results.clone();
            let cancel = Arc::clone(cancelled);
            scope.spawn(move || {
                loop {
                    // Release the queue mutex before running the document.
                    let next = { jobs.lock().expect("job queue mutex").recv() };
                    let Ok(job) = next else { break };
                    if cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    let record = execute(job, executable, options, limits, &cancel);
                    if results.send(Ok(record)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(receiver);
        let cancel = Arc::clone(cancelled);
        scope.spawn(move || {
            let mut input = std::io::BufReader::new(InterruptibleInput {
                file: input,
                cancelled: cancel,
            });
            let mut line = 0;
            loop {
                match next_job(&mut input, &mut line) {
                    Ok(Some(job)) => {
                        if send_jobs.send(job).is_err() {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        let _ = send_results.send(Err(error));
                        break;
                    }
                }
            }
        });
        let mut output = std::io::BufWriter::new(InterruptibleOutput {
            file: output,
            cancelled: Arc::clone(cancelled),
        });
        let result = publish(&mut output, &recv_results, cancelled);
        // On broken output, cancel active workers too; don't wait out their
        // full document timeout or leave their child processes running.
        cancelled.store(true, Ordering::Relaxed);
        drop(recv_results);
        result
    })
}

fn publish(
    output: &mut impl Write,
    results: &mpsc::Receiver<Result<BatchRecord>>,
    cancelled: &AtomicBool,
) -> Result<bool> {
    let mut success = true;
    for result in results {
        let record = result?;
        success &= record.result.outcome == Outcome::Extracted;
        serde_json::to_writer(&mut *output, &record)?;
        writeln!(output)?;
        output.flush()?;
    }
    if cancelled.load(Ordering::Relaxed) {
        bail!("batch interrupted; resume from the input-line records already emitted");
    }
    Ok(success)
}

/// Poll before reading a pipe so a signal/output failure can also cancel an
/// idle manifest producer. Reading and publication run independently: a live
/// producer may send one path, wait for its result, and then send another.
struct InterruptibleInput {
    file: std::fs::File,
    cancelled: Arc<AtomicBool>,
}

struct InterruptibleOutput {
    file: std::fs::File,
    cancelled: Arc<AtomicBool>,
}

impl Write for InterruptibleOutput {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        if !poll_file(&self.file, true, &self.cancelled)? {
            return Err(std::io::Error::other("batch output cancelled"));
        }
        // POSIX guarantees at least 512 bytes of atomic pipe-write capacity.
        // A bounded write after POLLOUT cannot get stuck behind a stalled
        // consumer while SIGINT is waiting to cancel the batch.
        self.file.write(&bytes[..bytes.len().min(512)])
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

impl Read for InterruptibleInput {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if buffer.is_empty() || !poll_file(&self.file, false, &self.cancelled)? {
            return Ok(0);
        }
        self.file.read(buffer)
    }
}

fn poll_file(file: &std::fs::File, writing: bool, cancelled: &AtomicBool) -> std::io::Result<bool> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        loop {
            if cancelled.load(Ordering::Relaxed) {
                return Ok(false);
            }
            let mut descriptor = libc::pollfd {
                fd: file.as_raw_fd(),
                events: if writing { libc::POLLOUT } else { libc::POLLIN },
                revents: 0,
            };
            // SAFETY: one initialized descriptor lives across this call.
            let ready = unsafe { libc::poll(&raw mut descriptor, 1, 50) };
            if ready > 0 {
                return Ok(true);
            }
            if ready < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (file, writing, cancelled);
        Ok(true)
    }
}

fn next_job(input: &mut impl BufRead, line: &mut u64) -> Result<Option<Job>> {
    loop {
        let mut bytes = Vec::new();
        let count = input
            .take(MAX_MANIFEST_LINE + 1)
            .read_until(b'\n', &mut bytes)?;
        if count == 0 {
            return Ok(None);
        }
        *line += 1;
        let parsed = if count as u64 > MAX_MANIFEST_LINE {
            // Drain the rest of this one line without growing a buffer.
            if bytes.last() != Some(&b'\n') {
                loop {
                    let buffer = input.fill_buf()?;
                    if buffer.is_empty() {
                        break;
                    }
                    let end = buffer.iter().position(|b| *b == b'\n');
                    let consumed = end.map_or(buffer.len(), |i| i + 1);
                    input.consume(consumed);
                    if end.is_some() {
                        break;
                    }
                }
            }
            Err("manifest line exceeds 64 KiB".to_owned())
        } else if bytes.iter().all(u8::is_ascii_whitespace) {
            continue;
        } else {
            serde_json::from_slice::<Input>(&bytes)
                .map_err(|e| format!("invalid manifest record: {e}"))
                .and_then(|record| {
                    if record.path.as_os_str().is_empty() {
                        Err("empty input path".into())
                    } else {
                        Ok(record)
                    }
                })
        };
        return Ok(Some(match parsed {
            Ok(record) => Job {
                line: *line,
                path: record.path,
                error: None,
            },
            Err(error) => Job {
                line: *line,
                path: PathBuf::new(),
                error: Some(error),
            },
        }));
    }
}

fn execute(
    job: Job,
    executable: &Path,
    options: &Options,
    limits: &Limits,
    cancelled: &AtomicBool,
) -> BatchRecord {
    let start = Instant::now();
    let result = job.error.map_or_else(
        || {
            let mut command = Command::new(executable);
            command
                .arg("ingest")
                .arg("--max-bytes")
                .arg(options.max_bytes.to_string())
                .arg("--max-expanded-bytes")
                .arg(options.max_expanded_bytes.to_string())
                .arg("--max-archive-entries")
                .arg(options.max_archive_entries.to_string())
                .arg("--max-cells")
                .arg(options.max_cells.to_string())
                .arg("--max-pages")
                .arg(options.max_pages.to_string())
                .arg("--")
                .arg(&job.path)
                // Bound nested parallelism; process-level jobs own the CPU budget.
                .env("RAYON_NUM_THREADS", "1");
            process(&mut command, limits, cancelled)
                .and_then(|bytes| {
                    let record: Record = serde_json::from_slice(&bytes)
                        .context("worker did not return one complete ingestion record")?;
                    if record.path != job.path.to_string_lossy() {
                        bail!("worker returned a different input path");
                    }
                    if record.schema_version != "tpe.ingest.v1" {
                        bail!("unexpected worker record version");
                    }
                    Ok(record)
                })
                .unwrap_or_else(|error| Record::failure(&job.path, options, error.to_string()))
        },
        |error| Record::failure(&job.path, options, error),
    );
    BatchRecord {
        schema_version: "tpe.ingest-batch.v1",
        input_line: job.line,
        worker_ms: start.elapsed().as_secs_f64() * 1000.0,
        supervisor: limits.clone(),
        result,
    }
}

fn kill_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    if let Ok(pid) = libc::pid_t::try_from(child.id()) {
        // SAFETY: the child was placed in its own process group. No shared
        // process group or caller-controlled PID is used.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
}

fn configure(command: &mut Command, limits: &Limits) {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        let bytes = limits.memory_mib * 1024 * 1024;
        // SAFETY: getpid has no arguments or memory preconditions.
        let parent_pid = unsafe { libc::getpid() };
        // SAFETY: this post-fork closure calls only async-signal-safe libc
        // resource-limit functions, with stack data and no Rust locks.
        unsafe {
            command.pre_exec(move || {
                // Also terminate the direct worker if its supervising thread
                // dies without running Rust cleanup (e.g. supervisor SIGKILL).
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() != parent_pid {
                    return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
                }
                if bytes == 0 {
                    return Ok(());
                }
                let mut limit = libc::rlimit {
                    rlim_cur: 0,
                    rlim_max: 0,
                };
                if libc::getrlimit(libc::RLIMIT_AS, &raw mut limit) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                limit.rlim_cur = bytes.min(limit.rlim_max);
                if libc::setrlimit(libc::RLIMIT_AS, &raw const limit) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = limits;
}

struct CaptureControl<'a> {
    overflow: AtomicBool,
    closing: AtomicBool,
    incomplete: AtomicBool,
    start: Instant,
    timeout: Duration,
    cancelled: &'a AtomicBool,
}

fn capture(
    mut reader: impl Read,
    fd: i32,
    cap: u64,
    control: &CaptureControl<'_>,
) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        if control.cancelled.load(Ordering::Relaxed) || control.start.elapsed() >= control.timeout {
            control.incomplete.store(true, Ordering::Relaxed);
            break;
        }
        #[cfg(unix)]
        {
            let closing = control.closing.load(Ordering::Relaxed);
            let mut descriptor = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: fd belongs to the reader owned by this thread.
            let ready = unsafe { libc::poll(&raw mut descriptor, 1, if closing { 0 } else { 10 }) };
            if ready == 0 {
                if closing {
                    control.incomplete.store(true, Ordering::Relaxed);
                    break;
                }
                continue;
            }
            if ready < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
        }
        #[cfg(not(unix))]
        let _ = fd;
        let remaining = usize::try_from((cap + 1).saturating_sub(bytes.len() as u64))
            .unwrap_or(usize::MAX)
            .min(buffer.len());
        let count = reader.read(&mut buffer[..remaining])?;
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..count]);
        if bytes.len() as u64 > cap {
            control.overflow.store(true, Ordering::Relaxed);
            break;
        }
    }
    Ok(bytes)
}

fn process(command: &mut Command, limits: &Limits, cancelled: &AtomicBool) -> Result<Vec<u8>> {
    configure(command, limits);
    let start = Instant::now();
    let mut child = command
        .spawn()
        .context("starting isolated ingestion worker")?;
    let stdout = child.stdout.take().context("missing worker stdout")?;
    let stderr = child.stderr.take().context("missing worker stderr")?;
    let control = CaptureControl {
        overflow: AtomicBool::new(false),
        closing: AtomicBool::new(false),
        incomplete: AtomicBool::new(false),
        start,
        timeout: Duration::from_millis(limits.timeout_ms),
        cancelled,
    };
    #[cfg(unix)]
    let (out_fd, err_fd) = {
        use std::os::fd::AsRawFd;
        (stdout.as_raw_fd(), stderr.as_raw_fd())
    };
    #[cfg(not(unix))]
    let (out_fd, err_fd) = (-1, -1);
    thread::scope(|scope| -> Result<Vec<u8>> {
        let out = scope.spawn(|| capture(stdout, out_fd, limits.max_output_bytes, &control));
        let err = scope.spawn(|| capture(stderr, err_fd, MAX_STDERR, &control));
        let status = loop {
            if cancelled.load(Ordering::Relaxed) {
                break Err(anyhow::anyhow!("batch output closed; worker cancelled"));
            }
            if control.overflow.load(Ordering::Relaxed) {
                break Err(anyhow::anyhow!("worker exceeded stdout/stderr byte limit"));
            }
            if start.elapsed() >= Duration::from_millis(limits.timeout_ms) {
                break Err(anyhow::anyhow!(
                    "worker exceeded {} ms deadline",
                    limits.timeout_ms
                ));
            }
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) => thread::sleep(Duration::from_millis(2)),
                Err(error) => break Err(error.into()),
            }
        };
        // Kill descendants too, including ones holding pipes after the main
        // worker exits. Reap the worker on every path before joining readers.
        kill_group(&mut child);
        let _ = child.wait();
        control.closing.store(true, Ordering::Relaxed);
        let output = out
            .join()
            .map_err(|_| anyhow::anyhow!("stdout reader panicked"))??;
        let errors = err
            .join()
            .map_err(|_| anyhow::anyhow!("stderr reader panicked"))??;
        let status = status?;
        if control.overflow.load(Ordering::Relaxed) {
            bail!("worker exceeded stdout/stderr byte limit");
        }
        if control.incomplete.load(Ordering::Relaxed) {
            bail!("worker output pipes remained open or capture deadline elapsed");
        }
        // Exit 1 with a valid record is normal for needs_ocr/failed/etc.
        // Signals, aborts and startup failures must never look like success.
        if status.code().is_none() || !matches!(status.code(), Some(0 | 1)) || output.is_empty() {
            bail!(
                "worker exited {status}: {}",
                String::from_utf8_lossy(&errors).trim()
            );
        }
        Ok(output)
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn capture_stops_when_an_unowned_writer_keeps_the_pipe_open() {
        use std::os::fd::AsRawFd;
        use std::os::unix::net::UnixStream;
        let (reader, mut writer) = UnixStream::pair().unwrap();
        writer.write_all(b"{}").unwrap();
        let control = CaptureControl {
            overflow: AtomicBool::new(false),
            closing: AtomicBool::new(true),
            incomplete: AtomicBool::new(false),
            start: Instant::now(),
            timeout: Duration::from_secs(1),
            cancelled: &AtomicBool::new(false),
        };
        let fd = reader.as_raw_fd();
        let output = capture(reader, fd, 1024, &control).unwrap();
        assert_eq!(output, b"{}");
        assert!(control.incomplete.load(Ordering::Relaxed));
        assert!(control.start.elapsed() < Duration::from_secs(1));
        drop(writer);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_worker_receives_the_requested_address_space_limit() {
        let mut command = Command::new("sh");
        command.args(["-c", "ulimit -v"]);
        let limits = Limits {
            memory_mib: 128,
            ..Limits::default()
        };
        let output = process(&mut command, &limits, &AtomicBool::new(false)).unwrap();
        assert_eq!(String::from_utf8(output).unwrap().trim(), "131072");
    }

    #[test]
    fn deadline_kills_descendants_and_closes_pipes() {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 30 & wait"]);
        let limits = Limits {
            timeout_ms: 100,
            ..Limits::default()
        };
        let start = Instant::now();
        let error = process(&mut command, &limits, &AtomicBool::new(false)).unwrap_err();
        assert!(error.to_string().contains("deadline"), "{error}");
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn excess_output_is_killed_without_waiting_for_the_deadline() {
        let mut command = Command::new("sh");
        command.args(["-c", "while :; do printf 'xxxxxxxxxxxxxxxx'; done"]);
        let limits = Limits {
            max_output_bytes: 512,
            timeout_ms: 5000,
            ..Limits::default()
        };
        let error = process(&mut command, &limits, &AtomicBool::new(false)).unwrap_err();
        assert!(error.to_string().contains("byte limit"), "{error}");
    }

    #[test]
    fn oversized_manifest_line_is_drained_and_next_record_survives() {
        let mut data = vec![b'x'; usize::try_from(MAX_MANIFEST_LINE).unwrap() * 2];
        data.extend_from_slice(b"\n{\"path\":\"next.txt\"}\n");
        let mut input = std::io::Cursor::new(data);
        let mut line = 0;
        assert!(
            next_job(&mut input, &mut line)
                .unwrap()
                .unwrap()
                .error
                .is_some()
        );
        let next = next_job(&mut input, &mut line).unwrap().unwrap();
        assert_eq!(next.path, Path::new("next.txt"));
        assert_eq!(next.line, 2);
    }
}
