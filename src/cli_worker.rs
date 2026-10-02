//! Bounded CLI workers. A document runs in a disposable child, not a thread
//! that the controller cannot stop. This is supervision, not a sandbox or a
//! hard peak-memory bound. No external child programs run inside the worker.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use tempfile::TempDir;
use tpe::pipeline;
use tpe::router;
use tpe::schema::{ExtractionResult, Job, Status};

use super::{ExtractArgs, Outcome};

const MAX_INPUT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_CAPTURE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_DIRECTORY_ENTRIES: usize = 10000;
const REQUEST_BYTES: u64 = 64 * 1024;

#[derive(Serialize, Deserialize)]
struct Request {
    job: Job,
    progress: bool,
}

#[derive(Serialize, Deserialize)]
struct Response {
    version: u32,
    result: Result<ExtractionResult, String>,
}

pub(super) fn input_paths(args: &ExtractArgs) -> anyhow::Result<Vec<PathBuf>> {
    check_supervised_backend(&args.backend)?;
    ensure!(
        (1..=300_000).contains(&args.timeout_ms),
        "--timeout-ms must be 1..=300000"
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
        args.figures_dir.is_none(),
        "supervised extraction currently exports text/JSON only; omit --figures-dir (figure metadata is retained)"
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
    Ok(paths)
}

fn check_supervised_backend(name: &str) -> anyhow::Result<()> {
    ensure!(
        matches!(name, "lopdf" | "pdfium" | "auto"),
        "supervised extraction supports native lopdf/pdfium only; OCR backends may spawn unsupervised children"
    );
    ensure!(
        name != "auto" || !tpe::backend::available().contains(&"docling"),
        "auto may route to OCR in this build; choose --backend lopdf or --backend pdfium for supervised extraction"
    );
    Ok(())
}

pub(super) fn extract_one(args: &ExtractArgs, path: PathBuf) -> Outcome {
    let started = Instant::now();
    let result = supervise(args, &path).map_err(|e| e.to_string());
    Outcome {
        path,
        wall_ms: super::elapsed_ms(started),
        result,
    }
}

// Holding stdin open also holds the worker's liveness lease. On controller
// death the OS closes it, and the worker's EOF watchdog exits the process.
// Ordinary timeout/error/unwinding additionally kills and reaps synchronously.
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

fn supervise(args: &ExtractArgs, path: &Path) -> anyhow::Result<ExtractionResult> {
    let path_text = path.to_str().context("input path is not valid Unicode")?;
    let request = Request {
        job: Job {
            path: path_text.to_string(),
            backend: args.backend.clone(),
            pages: args.pages,
            password: args.password.clone(),
            max_bytes: Some(args.max_bytes.unwrap_or(MAX_INPUT_BYTES)),
            figures_dir: None,
        },
        progress: args.progress,
    };
    let encoded = serde_json::to_vec(&request)?;
    ensure!(
        encoded.len() < REQUEST_BYTES as usize,
        "worker request exceeds 64 KiB"
    );
    let temp = TempDir::new().context("creating worker capture directory")?;
    let stdout_path = temp.path().join("result.json");
    let stderr_path = temp.path().join("diagnostics.txt");
    let request_path = temp.path().join("request.json");
    // A bounded private file avoids blocking on a full stdin pipe before the
    // supervisor can enforce the deadline. Stdin is only a liveness lease.
    fs::write(&request_path, encoded)?;
    let stdout = File::create_new(&stdout_path)?;
    let stderr = File::create_new(&stderr_path)?;
    let started = Instant::now();
    let mut child = Command::new(std::env::current_exe()?)
        .arg("extract-worker")
        .arg(&request_path)
        .stdin(Stdio::piped())
        .stdout(stdout)
        .stderr(stderr)
        .spawn()
        .context("starting extraction worker")?;
    let lease = child.stdin.take().context("worker stdin unavailable")?;
    let mut worker = Worker {
        child,
        _lease: lease,
    };
    let deadline = Duration::from_millis(args.timeout_ms);
    let status = loop {
        let captured = fs::metadata(&stdout_path)?
            .len()
            .saturating_add(fs::metadata(&stderr_path)?.len());
        ensure!(
            captured <= args.max_output_bytes,
            "worker output exceeded --max-output-bytes {}",
            args.max_output_bytes
        );
        ensure!(
            started.elapsed() < deadline,
            "worker timed out after {} ms",
            args.timeout_ms
        );
        if let Some(status) = worker.child.try_wait()? {
            break status;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    // Check final output sizes too: a child may exit between polls.
    ensure!(
        fs::metadata(&stdout_path)?
            .len()
            .saturating_add(fs::metadata(&stderr_path)?.len())
            <= args.max_output_bytes,
        "worker output exceeded --max-output-bytes {}",
        args.max_output_bytes
    );
    let diagnostics = fs::read_to_string(&stderr_path)?;
    if args.progress && !diagnostics.is_empty() {
        // Preserve complete JSON progress records; never interleave partial lines.
        std::io::stderr().lock().write_all(diagnostics.as_bytes())?;
    }
    ensure!(status.success(), "extraction worker exited {status}");
    let response: Response = serde_json::from_slice(&fs::read(stdout_path)?)
        .context("invalid extraction worker response")?;
    ensure!(
        response.version == 1,
        "unsupported extraction worker response version"
    );
    let result = response.result.map_err(anyhow::Error::msg)?;
    validate_pages(&result, args.pages)?;
    Ok(result)
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

fn retain_unresolved_text(result: &mut ExtractionResult) {
    for page in &mut result.pages {
        let reason = if router::has_unmapped_text(page)
            || page.text.contains('\u{fffd}')
            || page.warnings.iter().any(|w| w.contains("unmapped "))
        {
            Some("native text contains unmapped characters; retained text needs recovery")
        } else if router::looks_scanned(page) {
            Some("little or no text over a page-sized image; inspect for missing image text")
        } else if page.warnings.iter().any(|w| {
            w.starts_with("fonts:")
                || (w.starts_with("XObject ")
                    && (w.contains("not in resources") || w.contains("undecodable")))
        }) {
            Some("missing or undecodable page resources; retained text may be incomplete")
        } else if page
            .warnings
            .iter()
            .any(|w| w == "no MediaBox or CropBox: page size unknown")
        {
            Some("page size unavailable; retained text geometry is uncertain")
        } else {
            None
        };
        if let Some(reason) = reason {
            let warning = format!("unresolved_text: {reason}");
            page.warnings.push(warning.clone());
            result
                .warnings
                .push(format!("page {}: {warning}", page.page));
            if result.status == Status::Complete {
                result.status = Status::Partial;
            }
        }
    }
    result.chunks = pipeline::chunk_results(
        &result.pages,
        result.timings.parse_ms + result.timings.order_ms,
    );
    // CLI acceptance now includes unresolved text/scan evidence. Keep old runs
    // distinguishable without changing backend or release versions.
    result.backend.config_digest = tpe::schema::sha256_hex(
        format!("{}\ncli_text_acceptance=1\n", result.backend.config_digest).as_bytes(),
    );
}

pub(super) fn run_worker(request_path: &Path) -> anyhow::Result<ExitCode> {
    // This thread owns no parser state. EOF means the supervisor disappeared;
    // process exit terminates all parser/hash threads even if one is hung.
    std::thread::spawn(|| {
        let mut buffer = [0_u8; 1];
        let _ = std::io::stdin().read(&mut buffer);
        std::process::exit(1);
    });
    let encoded = fs::read(request_path)?;
    ensure!(
        encoded.len() < REQUEST_BYTES as usize,
        "oversized worker request"
    );
    let request: Request = serde_json::from_slice(&encoded)?;
    check_supervised_backend(&request.job.backend)?;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut observe = |event| {
            if request.progress {
                super::report_progress(&request.job.path, event);
            }
        };
        pipeline::run_job_observed(&request.job, &mut observe)
    }))
    .map_or_else(
        |payload| Err(format!("panic: {}", super::panic_message(&*payload))),
        |outcome| outcome.map_err(|e| e.to_string()),
    )
    .map(|mut result| {
        retain_unresolved_text(&mut result);
        result
    });
    serde_json::to_writer(std::io::stdout().lock(), &Response { version: 1, result })?;
    Ok(ExitCode::SUCCESS)
}
