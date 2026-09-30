//! `tpe` command-line interface: extract PDFs into a ledger, query the ledger,
//! benchmark the extraction stages, and evaluate the engine against the
//! `arXiv` corpus described by `corpus/manifest.json`.

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::io::{self, Write};
use std::panic;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Mutex, mpsc};
use std::thread;
use std::time::{Instant, SystemTime};

use anyhow::{Context, anyhow, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};

use tpe::backend::{self, Extractor};
use tpe::bibdb::{BibDb, BibDbError};
use tpe::bibliography;
use tpe::corpus::{self, Manifest, ManifestItem};
use tpe::eval::{self, CorpusReport, PaperEval};
use tpe::inputs::{self, Input, Planned};
use tpe::latex_refs;
use tpe::ledger::Ledger;
use tpe::pipeline::{self, PipelineError, Progress};
use tpe::schema::{ExtractionResult, Job, Metadata, Status};
use tpe::update;

/// Service-time target per 20-page chunk, in milliseconds.
const TARGET_MS_PER_CHUNK: f64 = 30.0;

/// User agent sent with corpus downloads.
const USER_AGENT: &str =
    "text-processing-engine eval (github.com/benpshore/text-processing-engine)";

/// `--version` text after the program name: `<version> (<git sha>)`, both
/// baked in by `build.rs`.
const VERSION_STRING: &str = concat!(env!("TPE_VERSION"), " (", env!("TPE_GIT_SHA"), ")");

/// Output directory of `tpe PATH...` when `--out` is not given.
const DEFAULT_OUT_DIR: &str = "tpe-out";

// `tpe PATH...` extracts text (and images) from PDFs; `tpe --bib PATH...`
// scans them for their bibliographies. The subcommands are the older,
// ledger-centred tools. Without any argument the help is printed and the
// process exits with status 2.
#[derive(Parser)]
#[command(name = "tpe", version = VERSION_STRING, about)]
#[command(args_conflicts_with_subcommands = true, subcommand_negates_reqs = true)]
#[command(arg_required_else_help = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,
    #[command(flatten)]
    run: RunArgs,
}

/// Arguments of the default command, `tpe [--bib] PATH...`.
#[derive(Args)]
struct RunArgs {
    /// PDF files, or directories walked recursively for `*.pdf` (any case;
    /// hidden entries skipped).
    #[arg(required = true, value_name = "PATH")]
    paths: Vec<PathBuf>,
    /// Scan each PDF backward for its bibliography (one JSON line per PDF)
    /// instead of extracting the whole text.
    #[arg(long)]
    bib: bool,
    /// Output directory for text mode (default `./tpe-out`); with `--bib`,
    /// a `.jsonl` file that receives the records instead of stdout.
    #[arg(long, value_name = "PATH")]
    out: Option<PathBuf>,
    /// Number of worker threads (default: the available CPUs).
    #[arg(long, short, value_name = "N")]
    jobs: Option<usize>,
    /// Report progress as JSON lines on stderr (`opened`, `page`, `done`,
    /// `summary`) instead of one human line per finished file.
    #[arg(long)]
    progress: bool,
    /// Extraction backend name.
    #[arg(long, default_value = "lopdf")]
    backend: String,
    /// Password for encrypted documents.
    #[arg(long)]
    password: Option<String>,
    /// Reject inputs larger than this many bytes.
    #[arg(long, value_name = "N")]
    max_bytes: Option<u64>,
    #[command(flatten)]
    text: TextFlags,
    #[command(flatten)]
    bibliography: BibFlags,
}

/// Flags that only apply to text mode.
#[derive(Args)]
struct TextFlags {
    /// Also write `<stem>.json`, the full extraction result, per PDF.
    #[arg(long)]
    json: bool,
    /// Print the page text to stdout (files separated by a form feed)
    /// instead of writing files.
    #[arg(long)]
    stdout: bool,
    /// Do not export figure images to `<stem>.figures/`.
    #[arg(long)]
    no_images: bool,
}

/// Flags that only apply to `--bib`.
#[derive(Args)]
struct BibFlags {
    /// Also store every record in this `SQLite` database (`papers` and
    /// `refs` tables; see docs/CLI.md).
    #[arg(long, value_name = "FILE")]
    db: Option<PathBuf>,
    /// Retry a PDF with the `pdfium` backend when the first scan finds no
    /// list; ignored when `pdfium` is not compiled into this build.
    #[arg(long)]
    pdfium_fallback: bool,
}

#[derive(Args)]
struct UpdateArgs {
    /// Only report whether a newer release exists; install nothing.
    #[arg(long)]
    check: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Extract text, metadata and citations from PDF files into a ledger.
    Extract(ExtractArgs),
    /// Extract the final bibliography by reading PDF pages from the end.
    Bibliography(BibliographyArgs),
    /// Print ledger statistics as `key: value` lines.
    Stats {
        /// Path of the `SQLite` ledger.
        #[arg(long, value_name = "FILE")]
        db: PathBuf,
    },
    /// Show the latest run stored for a document hash prefix.
    Show(ShowArgs),
    /// Measure warm service time per 20-page chunk (no ledger writes).
    Bench(BenchArgs),
    /// Manage the evaluation corpus described by a manifest.
    Corpus {
        #[command(subcommand)]
        command: CorpusCmd,
    },
    /// Evaluate extraction against `arXiv` `LaTeX` ground truth and write a report.
    Eval(EvalArgs),
    /// List every known backend, whether it is compiled in, and whether it
    /// opens a one-page probe PDF (native libraries found).
    Backends,
    /// Replace this binary with the latest GitHub release (`--check` only reports).
    Update(UpdateArgs),
}

#[derive(Subcommand)]
enum CorpusCmd {
    /// Download (or reuse from the cache) the PDF and e-print source of each item.
    Fetch(FetchArgs),
}

/// Manifest split selected on the command line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum Split {
    /// Every item.
    All,
    /// Items whose manifest split is `dev`.
    Dev,
    /// Items whose manifest split is `holdout`.
    Holdout,
}

impl Split {
    /// Whether `item` belongs to this selection.
    fn includes(self, item: &ManifestItem) -> bool {
        match self {
            Self::All => true,
            Self::Dev => item.split == "dev",
            Self::Holdout => item.split == "holdout",
        }
    }
}

#[derive(Args)]
struct ExtractArgs {
    /// PDF files to process.
    #[arg(required = true, value_name = "PATH")]
    paths: Vec<PathBuf>,
    /// Path of the `SQLite` ledger; created when missing.
    #[arg(long, value_name = "FILE")]
    db: PathBuf,
    /// Extraction backend name.
    #[arg(long, default_value = "lopdf")]
    backend: String,
    /// Directory that receives `<hash>.json` and `<hash>.txt` per document.
    #[arg(long, value_name = "DIR")]
    out: Option<PathBuf>,
    /// Print one JSON object per file instead of a tab-separated line.
    #[arg(long)]
    json: bool,
    /// Password for encrypted documents.
    #[arg(long)]
    password: Option<String>,
    /// Inclusive 1-based page range such as `3-7`; a single number selects one page.
    #[arg(long, value_name = "A-B", value_parser = parse_pages)]
    pages: Option<(u32, u32)>,
    /// Number of worker threads.
    #[arg(long, short, default_value_t = 1, value_name = "N")]
    jobs: usize,
    /// Reject inputs larger than this many bytes.
    #[arg(long, value_name = "N")]
    max_bytes: Option<u64>,
    /// Directory that receives figure bytes as `<hash>/<backend>-<digest>/p<page>-f<index>.<ext>`.
    #[arg(long, value_name = "DIR")]
    figures_dir: Option<PathBuf>,
    /// Report progress as JSON lines on stderr: `opened` once per file, then `page` per page.
    #[arg(long)]
    progress: bool,
}

#[derive(Args)]
struct BibliographyArgs {
    /// PDF files to process; one JSON record per file is printed to stdout.
    #[arg(required = true, value_name = "PATH")]
    paths: Vec<PathBuf>,
    /// Extraction backend name.
    #[arg(long, default_value = "lopdf")]
    backend: String,
    /// Password for encrypted documents.
    #[arg(long)]
    password: Option<String>,
    /// Reject inputs larger than this many bytes.
    #[arg(long, value_name = "N")]
    max_bytes: Option<u64>,
    /// Report progress as JSON lines on stderr: `opened` once per file, then `page` per page read.
    #[arg(long)]
    progress: bool,
}

#[derive(Args)]
struct ShowArgs {
    /// Path of the `SQLite` ledger.
    #[arg(long, value_name = "FILE")]
    db: PathBuf,
    /// Hex prefix of the document hash.
    #[arg(long, value_name = "PREFIX")]
    hash: String,
    /// Print the reference list.
    #[arg(long)]
    refs: bool,
    /// Print the paper metadata.
    #[arg(long)]
    meta: bool,
    /// Print the ordered page text.
    #[arg(long)]
    text: bool,
}

#[derive(Args)]
struct BenchArgs {
    /// PDF files to benchmark.
    #[arg(required = true, value_name = "PATH")]
    paths: Vec<PathBuf>,
    /// Extraction backend name.
    #[arg(long, default_value = "lopdf")]
    backend: String,
    /// Number of `run_job` executions per file.
    #[arg(long, default_value_t = 5, value_name = "N")]
    iterations: usize,
}

#[derive(Args)]
struct FetchArgs {
    /// Path of the corpus manifest (JSON).
    #[arg(long, value_name = "FILE")]
    manifest: PathBuf,
    /// Directory that caches downloaded PDFs and unpacked sources.
    #[arg(long, value_name = "DIR")]
    cache: PathBuf,
    /// Never use the network; items missing from the cache fail.
    #[arg(long)]
    offline: bool,
    /// Record newly computed SHA-256 digests in the manifest file.
    #[arg(long)]
    update_manifest: bool,
    /// Which manifest split to fetch.
    #[arg(long, value_enum, default_value_t = Split::All)]
    split: Split,
}

#[derive(Args)]
struct EvalArgs {
    /// Path of the corpus manifest (JSON).
    #[arg(long, value_name = "FILE")]
    manifest: PathBuf,
    /// Directory that caches downloaded PDFs and unpacked sources.
    #[arg(long, value_name = "DIR")]
    cache: PathBuf,
    /// Directory that receives `report.json` and `report.md`.
    #[arg(long, value_name = "DIR")]
    out: PathBuf,
    /// Extraction backend name.
    #[arg(long, default_value = "lopdf")]
    backend: String,
    /// Which manifest split to evaluate.
    #[arg(long, value_enum, default_value_t = Split::Dev)]
    split: Split,
    /// Never use the network; items missing from the cache count as failed papers.
    #[arg(long)]
    offline: bool,
    /// Optional `SQLite` ledger that also receives every extraction result.
    #[arg(long, value_name = "FILE")]
    db: Option<PathBuf>,
    /// Directory that receives figure bytes as `<hash>/<backend>-<digest>/p<page>-f<index>.<ext>`.
    #[arg(long, value_name = "DIR")]
    figures_dir: Option<PathBuf>,
    /// Directory that receives one JSON diagnostics dump per evaluated paper
    /// (truth vs extracted references, matches, markers, warnings, timings).
    #[arg(long, value_name = "DIR")]
    dump_dir: Option<PathBuf>,
}

fn main() -> anyhow::Result<ExitCode> {
    let cli = Cli::parse();
    let Some(command) = cli.command else {
        return run_paths(&cli.run);
    };
    match command {
        Cmd::Extract(args) => run_extract(&args),
        Cmd::Bibliography(args) => run_bibliography(&args),
        Cmd::Stats { db } => {
            run_stats(&db)?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Show(args) => {
            run_show(&args)?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Bench(args) => {
            run_bench(&args)?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Corpus { command } => match command {
            CorpusCmd::Fetch(args) => run_corpus_fetch(&args),
        },
        Cmd::Eval(args) => {
            run_eval(&args)?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Backends => {
            run_backends()?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Update(args) => {
            update::run(args.check)?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// Parse `a-b` (or a single `a`) into an inclusive 1-based page range.
fn parse_pages(raw: &str) -> Result<(u32, u32), String> {
    let trimmed = raw.trim();
    let (start_text, end_text) = trimmed
        .split_once('-')
        .map_or((trimmed, trimmed), |(a, b)| (a.trim(), b.trim()));
    let start: u32 = start_text
        .parse()
        .map_err(|_| format!("invalid page range `{raw}`: `{start_text}` is not a number"))?;
    let end: u32 = end_text
        .parse()
        .map_err(|_| format!("invalid page range `{raw}`: `{end_text}` is not a number"))?;
    if start == 0 || end == 0 {
        return Err(format!("invalid page range `{raw}`: pages are 1-based"));
    }
    if start > end {
        return Err(format!("invalid page range `{raw}`: start is after end"));
    }
    Ok((start, end))
}

/// Milliseconds elapsed since `start`.
fn elapsed_ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

/// One `--progress` line: a JSON object naming the file and the event.
/// `opened` carries `pages` (the document's page count) and `total` (pages
/// this run will process); `page` carries the finished `page`, `done` and
/// `total`. Each line is written with one locked `stderr` write, so worker
/// threads never interleave within a line.
fn progress_line(path: &str, event: Progress) -> String {
    let value = match event {
        Progress::Opened { pages, total } => serde_json::json!({
            "event": "opened", "path": path, "pages": pages, "total": total,
        }),
        Progress::Page { page, done, total } => serde_json::json!({
            "event": "page", "path": path, "page": page, "done": done, "total": total,
        }),
    };
    value.to_string()
}

/// Print a `--progress` line to stderr.
fn report_progress(path: &str, event: Progress) {
    eprintln!("{}", progress_line(path, event));
}

/// Open the ledger at `db`, naming the path in any error.
fn open_ledger(db: &Path) -> anyhow::Result<Ledger> {
    Ledger::open(db).with_context(|| format!("opening ledger {}", db.display()))
}

/// Fail early when the backend name is unknown or not compiled into this build.
fn check_backend(name: &str) -> anyhow::Result<()> {
    let available = backend::available();
    if available.contains(&name) {
        return Ok(());
    }
    let compiled = available.join(", ");
    if let Some(feature) = backend::feature_for(name) {
        bail!(
            "backend `{name}` is not compiled into this build; rebuild with \
             `--features {feature}` (compiled in: {compiled})"
        );
    }
    bail!("unknown backend `{name}`; known backends: {compiled}");
}

/// `--figures-dir` as the job field.
fn figures_dir_field(dir: Option<&Path>) -> Option<String> {
    dir.map(|path| path.to_string_lossy().into_owned())
}

/// Process exit code for a batch: failure when any item failed.
fn exit_code(any_failed: bool) -> ExitCode {
    if any_failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// First twelve hex digits of a digest, or the whole digest when shorter.
fn short_hash(hash: &str) -> &str {
    &hash[..hash.len().min(12)]
}

/// Record `result` in the ledger: its source observation, then the full run.
fn store_result(
    ledger: &mut Ledger,
    result: &ExtractionResult,
    label: &str,
) -> anyhow::Result<i64> {
    // `write_result` upserts the document and its source observations itself,
    // so a separate `record_source` call would only add a second transaction.
    ledger
        .write_result(result)
        .with_context(|| format!("writing result for {label}"))
}

/// Outcome of one worker job, sent to the main thread over the channel.
struct Outcome {
    path: PathBuf,
    wall_ms: f64,
    result: Result<ExtractionResult, String>,
}

/// Human-readable text of a panic payload.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

/// Pop the next queued item, or `None` when the queue is empty or poisoned.
fn next_item<T>(queue: &Mutex<VecDeque<T>>) -> Option<T> {
    let mut guard = queue.lock().ok()?;
    guard.pop_front()
}

/// Run the pipeline for one path on a worker thread.
fn extract_one(args: &ExtractArgs, path: PathBuf) -> Outcome {
    let job = Job {
        path: path.to_string_lossy().into_owned(),
        backend: args.backend.clone(),
        pages: args.pages,
        password: args.password.clone(),
        max_bytes: args.max_bytes,
        figures_dir: figures_dir_field(args.figures_dir.as_deref()),
    };
    let start = Instant::now();
    let mut observe = |event: Progress| {
        if args.progress {
            report_progress(&job.path, event);
        }
    };
    // A panic inside a backend must fail this file only, not the whole batch.
    let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        pipeline::run_job_observed(&job, &mut observe)
    }))
    .map_or_else(
        |payload| Err(format!("panic: {}", panic_message(&*payload))),
        |outcome| outcome.map_err(|err| err.to_string()),
    );
    Outcome {
        path,
        wall_ms: elapsed_ms(start),
        result,
    }
}

fn run_extract(args: &ExtractArgs) -> anyhow::Result<ExitCode> {
    check_backend(&args.backend)?;
    pipeline::warm_up();
    let mut ledger = open_ledger(&args.db)?;
    if let Some(dir) = &args.out {
        fs::create_dir_all(dir)
            .with_context(|| format!("creating output directory {}", dir.display()))?;
    }

    let queue: Mutex<VecDeque<PathBuf>> = Mutex::new(args.paths.iter().cloned().collect());
    let workers = args.jobs.clamp(1, args.paths.len().max(1));
    let (sender, receiver) = mpsc::channel::<Outcome>();

    let any_failed = thread::scope(|scope| -> anyhow::Result<bool> {
        for _ in 0..workers {
            let sender = sender.clone();
            let queue = &queue;
            scope.spawn(move || {
                while let Some(path) = next_item(queue) {
                    let outcome = extract_one(args, path);
                    if sender.send(outcome).is_err() {
                        break;
                    }
                }
            });
        }
        drop(sender);

        let mut any_failed = false;
        for outcome in receiver {
            if publish(&mut ledger, args, outcome)? {
                any_failed = true;
            }
        }
        Ok(any_failed)
    })?;

    Ok(exit_code(any_failed))
}

/// A bibliography-only result does not enter the full-document ledger: it
/// neither covers the whole PDF nor contains the metadata or in-text markers
/// promised by an `ExtractionResult`. Each output line carries its own PDF
/// hash and page range so it can be imported into a separate store later.
fn run_bibliography(args: &BibliographyArgs) -> anyhow::Result<ExitCode> {
    check_backend(&args.backend)?;
    pipeline::warm_up();
    let extractor = backend::by_name(&args.backend)
        .ok_or_else(|| anyhow!("backend `{}` is unavailable", args.backend))?;
    let mut any_failed = false;
    for path in &args.paths {
        let started = Instant::now();
        let file = path.to_string_lossy();
        let mut hash = None;
        let mut observe = |event: Progress| {
            if args.progress {
                report_progress(&file, event);
            }
        };
        let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            let snapshot = tpe::acquire::snapshot(path, args.max_bytes)?;
            hash = Some(snapshot.hash.0);
            let scan = bibliography::scan_backward_observed(
                extractor.as_ref(),
                &snapshot.bytes,
                args.password.as_deref(),
                &mut observe,
            )?;
            Ok::<_, anyhow::Error>(scan)
        }));
        let elapsed = elapsed_ms(started);
        let identity = extractor.identity();
        let record = match result {
            Ok(Ok(scan)) => {
                any_failed |= !scan.found;
                bibliography::Record::from_scan(
                    &file,
                    hash.unwrap_or_default(),
                    identity,
                    scan,
                    elapsed,
                )
            }
            Ok(Err(err)) => {
                any_failed = true;
                bibliography::Record::failed(&file, hash, identity, err.to_string(), elapsed)
            }
            Err(payload) => {
                any_failed = true;
                let message = format!("panic: {}", panic_message(&*payload));
                bibliography::Record::failed(&file, hash, identity, message, elapsed)
            }
        };
        println!("{}", serde_json::to_string(&record)?);
    }
    Ok(exit_code(any_failed))
}

/// Record one outcome in the ledger (main thread only), write the optional
/// output files and print its line. Returns `true` when the file failed.
fn publish(ledger: &mut Ledger, args: &ExtractArgs, outcome: Outcome) -> anyhow::Result<bool> {
    let Outcome {
        path,
        wall_ms,
        result,
    } = outcome;
    let path_display = path.display().to_string();
    match result {
        Ok(mut result) => {
            let write_start = Instant::now();
            let run = store_result(ledger, &result, &path_display)?;
            result.timings.write_ms = elapsed_ms(write_start);
            ledger
                .update_timings(run, &result.timings)
                .with_context(|| format!("recording write time for {path_display}"))?;
            if let Some(dir) = &args.out {
                write_outputs(dir, &result)?;
            }
            if args.json {
                println!("{}", serde_json::to_string(&result)?);
            } else {
                println!("{}", summary_line(&result, &path_display));
            }
            Ok(result.status == Status::Failed)
        }
        Err(err) => {
            eprintln!("{path_display}: {err}");
            if args.json {
                let line = serde_json::json!({
                    "status": Status::Failed.as_str(),
                    "path": path_display,
                    "error": err.clone(),
                    "ms": wall_ms,
                });
                println!("{line}");
            } else {
                println!("failed\t-\t0p\t0 refs\t0 cites\t{wall_ms:.1} ms\t{path_display}");
            }
            Ok(true)
        }
    }
}

/// The tab-separated line printed per document.
fn summary_line(result: &ExtractionResult, path: &str) -> String {
    let short = short_hash(&result.document.hash.0);
    let t = &result.timings;
    let total_ms =
        t.acquire_ms + t.parse_ms + t.order_ms + t.metadata_ms + t.citations_ms + t.write_ms;
    format!(
        "{}\t{short}\t{}p\t{} refs\t{} cites\t{total_ms:.1} ms\t{path}",
        result.status.as_str(),
        result.document.pages,
        result.references.len(),
        result.citations.len(),
    )
}

/// Write `<hash>.json` and `<hash>.txt` for one result into `dir`.
fn write_outputs(dir: &Path, result: &ExtractionResult) -> anyhow::Result<()> {
    let hash = result.document.hash.0.as_str();
    let json_path = dir.join(format!("{hash}.json"));
    let json = serde_json::to_string_pretty(result)?;
    fs::write(&json_path, json).with_context(|| format!("writing {}", json_path.display()))?;
    let text_path = dir.join(format!("{hash}.txt"));
    let texts: Vec<&str> = result.pages.iter().map(|p| p.text.as_str()).collect();
    fs::write(&text_path, texts.join("\u{c}"))
        .with_context(|| format!("writing {}", text_path.display()))?;
    Ok(())
}

fn run_stats(db: &Path) -> anyhow::Result<()> {
    let ledger = open_ledger(db)?;
    let stats = ledger.stats().context("reading ledger statistics")?;
    println!("documents: {}", stats.documents);
    println!("runs: {}", stats.runs);
    println!("complete: {}", stats.complete);
    println!("partial: {}", stats.partial);
    println!("failed: {}", stats.failed);
    println!("pages: {}", stats.pages);
    println!("references: {}", stats.references);
    println!("citations: {}", stats.citations);
    println!("figures: {}", stats.figures);
    Ok(())
}

/// Print one line per known backend: `<name>\tavailable\t<probe outcome>`
/// or `<name>\tnot compiled\t<feature hint>`. Always succeeds when the probe
/// PDF can be built; a backend whose native library is missing is reported,
/// not treated as an error.
fn run_backends() -> anyhow::Result<()> {
    let probe = backend::probe_pdf().context("building the probe PDF")?;
    let available = backend::available();
    for name in backend::ALL_KNOWN {
        if available.contains(name) {
            println!("{name}\tavailable\t{}", probe_backend(name, &probe));
        } else {
            let feature = backend::feature_for(name).unwrap_or("?");
            println!("{name}\tnot compiled\trebuild with --features {feature}");
        }
    }
    Ok(())
}

/// Open `probe` with backend `name` and describe the outcome. Only `open` is
/// called (no page is converted, so no models load), and the session is
/// dropped before this returns: a live `pdfium` session blocks every other
/// `pdfium` use in the process.
fn probe_backend(name: &str, probe: &[u8]) -> String {
    let Some(extractor) = backend::by_name(name) else {
        return "not resolvable".to_string();
    };
    let outcome = panic::catch_unwind(panic::AssertUnwindSafe(
        || -> Result<u32, backend::BackendError> {
            let session = extractor.open(probe, None)?;
            Ok(session.page_count())
        },
    ));
    match outcome {
        Ok(Ok(pages)) => format!("opens ({pages} page probe)"),
        Ok(Err(err)) => format!("open failed: {err}"),
        Err(payload) => format!("open panicked: {}", panic_message(&*payload)),
    }
}

/// Find the most recently finished run whose document hash starts with
/// `prefix`, using the ledger's `runs` table directly.
fn latest_run_for_prefix(ledger: &Ledger, prefix: &str) -> anyhow::Result<Option<i64>> {
    ledger
        .latest_run_for_prefix(prefix)
        .context("looking up run by hash prefix")
}

/// Text shown for an optional field.
fn show_opt(value: Option<&str>) -> &str {
    value.unwrap_or("-")
}

fn print_metadata(meta: &Metadata) {
    println!("title: {}", show_opt(meta.title.as_deref()));
    let names: Vec<&str> = meta.authors.iter().map(|a| a.name.as_str()).collect();
    let authors = if names.is_empty() {
        "-".to_string()
    } else {
        names.join("; ")
    };
    println!("authors: {authors}");
    println!("doi: {}", show_opt(meta.doi.as_deref()));
    println!("arxiv_id: {}", show_opt(meta.arxiv_id.as_deref()));
    if let Some(year) = meta.year {
        println!("year: {year}");
    } else {
        println!("year: -");
    }
    println!("venue: {}", show_opt(meta.venue.as_deref()));
    println!("abstract: {}", show_opt(meta.abstract_text.as_deref()));
    if !meta.keywords.is_empty() {
        println!("keywords: {}", meta.keywords.join("; "));
    }
    for (field, source) in &meta.provenance {
        println!("provenance.{field}: {source}");
    }
}

fn run_show(args: &ShowArgs) -> anyhow::Result<()> {
    let prefix = args.hash.trim().to_ascii_lowercase();
    if prefix.is_empty() || !prefix.as_bytes().iter().all(u8::is_ascii_hexdigit) {
        bail!("--hash must be a hexadecimal prefix of a document hash");
    }
    let ledger = open_ledger(&args.db)?;
    let run_id = latest_run_for_prefix(&ledger, &prefix)?
        .ok_or_else(|| anyhow!("no run found for hash prefix {prefix}"))?;
    let result = ledger
        .load_result(run_id)
        .with_context(|| format!("loading run {run_id}"))?;

    println!("hash: {}", result.document.hash.0);
    println!("status: {}", result.status.as_str());
    let backend = &result.backend;
    println!("backend: {} {}", backend.name, backend.version);
    println!("pages: {}", result.document.pages);
    println!("references: {}", result.references.len());
    println!("citations: {}", result.citations.len());
    for warning in &result.warnings {
        println!("warning: {warning}");
    }
    if args.meta {
        println!("--- metadata ---");
        print_metadata(&result.metadata);
    }
    if args.refs {
        println!("--- references ---");
        for entry in &result.references {
            println!("[{}] {}", entry.index, entry.raw);
        }
    }
    if args.text {
        for page in &result.pages {
            println!("--- page {} ---", page.page);
            println!("{}", page.text);
        }
    }
    Ok(())
}

/// Timing samples collected for one file by `bench`.
struct FileBench {
    pages: usize,
    chunks: usize,
    /// Wall milliseconds of `run_job` divided by its chunk count, one per iteration.
    ms_per_chunk: Vec<f64>,
    seconds_total: f64,
}

/// Run `run_job` `iterations` times for one file and collect its samples.
fn bench_file(job: &Job, iterations: usize) -> Result<FileBench, PipelineError> {
    let mut bench = FileBench {
        pages: 0,
        chunks: 0,
        ms_per_chunk: Vec::with_capacity(iterations),
        seconds_total: 0.0,
    };
    for _ in 0..iterations {
        let start = Instant::now();
        let result = pipeline::run_job(job)?;
        let seconds = start.elapsed().as_secs_f64();
        bench.pages = result.pages.len();
        bench.chunks = result.chunks.len();
        let chunk_count = result.chunks.len().max(1) as f64;
        bench.ms_per_chunk.push(seconds * 1000.0 / chunk_count);
        bench.seconds_total += seconds;
    }
    Ok(bench)
}

/// Nearest-rank percentile of an ascending slice; `0.0` for an empty slice.
fn percentile(sorted: &[f64], fraction: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let last = sorted.len() - 1;
    let rank = (last as f64 * fraction).round() as usize;
    sorted[rank.min(last)]
}

fn run_bench(args: &BenchArgs) -> anyhow::Result<()> {
    check_backend(&args.backend)?;
    pipeline::warm_up();
    let iterations = args.iterations.max(1);
    let mut all_samples: Vec<f64> = Vec::new();
    for path in &args.paths {
        let job = Job {
            path: path.to_string_lossy().into_owned(),
            backend: args.backend.clone(),
            pages: None,
            password: None,
            max_bytes: None,
            figures_dir: None,
        };
        match bench_file(&job, iterations) {
            Ok(mut bench) => {
                bench.ms_per_chunk.sort_by(f64::total_cmp);
                let p50 = percentile(&bench.ms_per_chunk, 0.50);
                let p95 = percentile(&bench.ms_per_chunk, 0.95);
                let pages_done = (bench.pages * iterations) as f64;
                let pages_per_s = if bench.seconds_total > 0.0 {
                    pages_done / bench.seconds_total
                } else {
                    0.0
                };
                let pages = bench.pages;
                let chunks = bench.chunks;
                let rates = format!("p50 {p50:.2} ms/chunk\tp95 {p95:.2} ms/chunk");
                println!(
                    "{}\t{pages}p\t{chunks} chunks\t{rates}\t{pages_per_s:.1} pages/s",
                    path.display()
                );
                all_samples.extend_from_slice(&bench.ms_per_chunk);
            }
            Err(err) => println!("{}\tfailed: {err}", path.display()),
        }
    }
    if all_samples.is_empty() {
        bail!("no successful runs");
    }
    all_samples.sort_by(f64::total_cmp);
    let gap = percentile(&all_samples, 0.95) - TARGET_MS_PER_CHUNK;
    println!("target 30 ms/chunk: p95 gap = {gap:.2} ms");
    Ok(())
}

/// Load the manifest and make sure the cache directory exists.
fn load_corpus(manifest_path: &Path, cache: &Path) -> anyhow::Result<Manifest> {
    let manifest = corpus::load_manifest(manifest_path)
        .with_context(|| format!("loading manifest {}", manifest_path.display()))?;
    fs::create_dir_all(cache)
        .with_context(|| format!("creating cache directory {}", cache.display()))?;
    Ok(manifest)
}

/// Whether `path` was modified at or after `since`; `false` when unknown.
fn modified_since(path: &Path, since: SystemTime) -> bool {
    fs::metadata(path)
        .and_then(|meta| meta.modified())
        .is_ok_and(|modified| modified >= since)
}

fn run_corpus_fetch(args: &FetchArgs) -> anyhow::Result<ExitCode> {
    let mut manifest = load_corpus(&args.manifest, &args.cache)?;
    let selected: Vec<ManifestItem> = manifest
        .items
        .iter()
        .filter(|item| args.split.includes(item))
        .cloned()
        .collect();
    let mut any_failed = false;
    let mut changed = false;
    for item in &selected {
        let started = SystemTime::now();
        match corpus::fetch_item(item, &args.cache, USER_AGENT, args.offline) {
            Ok(fetched) => {
                let origin = if args.offline || !modified_since(&fetched.pdf_path, started) {
                    "cached"
                } else {
                    "downloaded"
                };
                let source = if fetched.source_dir.is_some() {
                    "yes"
                } else {
                    "no"
                };
                let short = short_hash(&fetched.pdf_sha256);
                println!("{}\t{short}\tsource {source}\t{origin}", item.id);
                if args.update_manifest
                    && corpus::update_manifest_hashes(&mut manifest, &item.id, &fetched)
                {
                    changed = true;
                }
            }
            Err(err) => {
                eprintln!("{}: {err}", item.id);
                println!("{}\t-\tsource no\tfailed", item.id);
                any_failed = true;
            }
        }
    }
    if changed {
        corpus::save_manifest(&args.manifest, &manifest)
            .with_context(|| format!("saving manifest {}", args.manifest.display()))?;
        println!("manifest updated: {}", args.manifest.display());
    }
    Ok(exit_code(any_failed))
}

/// Host label for reports: operating system and CPU architecture.
fn host_label() -> String {
    format!("{} {}", std::env::consts::OS, std::env::consts::ARCH)
}

/// Everything `eval` produced for one manifest item.
struct Evaluated {
    paper: PaperEval,
    /// The extraction result when the pipeline ran, for the optional ledger write.
    result: Option<ExtractionResult>,
}

/// Fetch, extract and score one manifest item. Every failure becomes a
/// `failed:` paper so the measurement continues with the next item.
fn eval_item(args: &EvalArgs, item: &ManifestItem) -> Evaluated {
    let id = item.id.as_str();
    let failed = |error: String| Evaluated {
        paper: eval::failed_paper(id, &error),
        result: None,
    };
    let fetched = match corpus::fetch_item(item, &args.cache, USER_AGENT, args.offline) {
        Ok(fetched) => fetched,
        Err(err) => return failed(format!("fetch: {err}")),
    };
    let Some(source_dir) = fetched.source_dir.as_deref() else {
        return failed("no LaTeX source in the e-print".to_string());
    };
    let files = match corpus::find_latex_files(source_dir) {
        Ok(files) => files,
        Err(err) => return failed(format!("source: {err}")),
    };
    let truth = match latex_refs::ground_truth(&files) {
        Ok(truth) => truth,
        Err(err) => return failed(format!("ground truth: {err}")),
    };
    let job = Job {
        path: fetched.pdf_path.to_string_lossy().into_owned(),
        backend: args.backend.clone(),
        pages: None,
        password: None,
        max_bytes: None,
        figures_dir: figures_dir_field(args.figures_dir.as_deref()),
    };
    let result = match pipeline::run_job(&job) {
        Ok(result) => result,
        Err(err) => return failed(format!("extract: {err}")),
    };
    let paper = eval::evaluate(id, &result, &truth);
    if let Some(dir) = &args.dump_dir {
        match eval::write_dump(dir, &eval::dump_paper(id, &result, &truth, &paper)) {
            Ok(path) => eprintln!("dump written: {}", path.display()),
            Err(err) => eprintln!("{id}: dump not written: {err}"),
        }
    }
    Evaluated {
        paper,
        result: Some(result),
    }
}

/// The tab-separated line printed per evaluated paper.
fn paper_line(paper: &PaperEval) -> String {
    let refs = format!(
        "{}/{}/{} refs (truth/extracted/matched)",
        paper.truth_refs, paper.extracted_refs, paper.matched_refs
    );
    format!(
        "{}\t{}\t{}p\t{refs}\t{:.1} ms/chunk",
        paper.id, paper.status, paper.pages, paper.ms_per_chunk
    )
}

/// Write `report.json` and `report.md` into `dir`.
fn write_report(dir: &Path, report: &CorpusReport) -> anyhow::Result<()> {
    let json_path = dir.join("report.json");
    let mut json = serde_json::to_string_pretty(report)?;
    json.push('\n');
    fs::write(&json_path, json).with_context(|| format!("writing {}", json_path.display()))?;
    let md_path = dir.join("report.md");
    fs::write(&md_path, eval::render_markdown(report))
        .with_context(|| format!("writing {}", md_path.display()))?;
    Ok(())
}

/// Print the corpus summary as `key: value` lines.
fn print_summary(report: &CorpusReport) {
    let s = &report.summary;
    println!("papers: {}", s.papers);
    println!("failed: {}", s.failed);
    println!("ref_count_exact_rate: {:.3}", s.ref_count_exact_rate);
    println!("ref_recall: {:.3}", s.ref_recall);
    println!("ref_precision: {:.3}", s.ref_precision);
    println!("doi_accuracy: {:.3}", s.doi_accuracy);
    println!("year_accuracy: {:.3}", s.year_accuracy);
    println!("title_accuracy: {:.3}", s.title_accuracy);
    println!("title_not_applicable: {}", s.title_not_applicable);
    println!("marker_resolution_rate: {:.3}", s.marker_resolution_rate);
    println!("marker_precision: {:.3}", s.marker_precision);
    println!("marker_key_recall: {:.3}", s.marker_key_recall);
    println!("marker_count_ratio: {:.3}", s.marker_recall);
    println!("marker_command_ratio: {:.3}", s.marker_command_ratio);
    match s.mean_body_alignment {
        Some(alignment) => println!("mean_body_alignment: {alignment:.3}"),
        None => println!("mean_body_alignment: -"),
    }
    match s.mean_body_alignment_raw {
        Some(alignment) => println!("mean_body_alignment_raw: {alignment:.3}"),
        None => println!("mean_body_alignment_raw: -"),
    }
    println!("body_word_recall: {:.3}", s.body_word_recall);
    println!("body_word_precision: {:.3}", s.body_word_precision);
    println!("p50_ms_per_chunk: {:.2}", s.p50_ms_per_chunk);
    println!("p95_ms_per_chunk: {:.2}", s.p95_ms_per_chunk);
    println!("target_ms_per_chunk: {:.1}", s.target_ms_per_chunk);
}

fn run_eval(args: &EvalArgs) -> anyhow::Result<()> {
    check_backend(&args.backend)?;
    pipeline::warm_up();
    let manifest = load_corpus(&args.manifest, &args.cache)?;
    fs::create_dir_all(&args.out)
        .with_context(|| format!("creating output directory {}", args.out.display()))?;
    let mut ledger: Option<Ledger> = args.db.as_deref().map(open_ledger).transpose()?;
    let mut papers: Vec<PaperEval> = Vec::new();
    for item in manifest
        .items
        .iter()
        .filter(|item| args.split.includes(item))
    {
        let mut evaluated = eval_item(args, item);
        if let (Some(ledger), Some(result)) = (ledger.as_mut(), evaluated.result.as_mut()) {
            let write_start = Instant::now();
            let run = store_result(ledger, result, &item.id)?;
            let write_ms = elapsed_ms(write_start);
            result.timings.write_ms = write_ms;
            ledger
                .update_timings(run, &result.timings)
                .with_context(|| format!("recording write time for {}", item.id))?;
            let paper = &mut evaluated.paper;
            paper.timings.write_ms = write_ms;
            paper.ms_total += write_ms;
            paper.ms_per_chunk = paper.ms_total / f64::from(paper.chunks.max(1));
        }
        println!("{}", paper_line(&evaluated.paper));
        papers.push(evaluated.paper);
    }
    let report = eval::build_report(&args.backend, &host_label(), papers);
    write_report(&args.out, &report)?;
    print_summary(&report);
    Ok(())
}

/// Worker count for the default command: `requested`, else the available
/// CPUs, never more than one per input and never zero.
fn worker_count(requested: Option<usize>, inputs: usize) -> usize {
    let available = thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    requested.unwrap_or(available).clamp(1, inputs.max(1))
}

/// Bytes as a short decimal size: `900 B`, `12.3 kB`, `1.3 MB`, `2.0 GB`.
fn human_size(bytes: u64) -> String {
    let value = bytes as f64;
    if bytes < 1_000 {
        format!("{bytes} B")
    } else if bytes < 1_000_000 {
        format!("{:.1} kB", value / 1e3)
    } else if bytes < 1_000_000_000 {
        format!("{:.1} MB", value / 1e6)
    } else {
        format!("{:.1} GB", value / 1e9)
    }
}

/// `<stem>.<suffix>` without touching any dot already in the stem.
fn with_suffix(stem: &Path, suffix: &str) -> PathBuf {
    let mut name = stem.as_os_str().to_owned();
    name.push(".");
    name.push(suffix);
    PathBuf::from(name)
}

/// How one input of the default command ended. Only `Ok` counts as success
/// for the exit code.
enum Verdict {
    Ok,
    /// The input could not be processed; the text says why.
    Failed(String),
    /// Not a PDF: the kind name and the refusal reason.
    Unsupported { kind: &'static str, reason: String },
    /// A PDF without a text layer (see `inputs::looks_scanned`).
    Scanned,
}

impl Verdict {
    fn succeeded(&self) -> bool {
        matches!(self, Self::Ok)
    }

    /// The status word used in JSON events and records.
    fn status(&self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Failed(_) => "failed",
            Self::Unsupported { .. } => "unsupported",
            Self::Scanned => "scanned",
        }
    }

    /// The refusal reason of an `Unsupported` or `Scanned` verdict.
    fn reason(&self) -> Option<&str> {
        match self {
            Self::Unsupported { reason, .. } => Some(reason.as_str()),
            Self::Scanned => Some(inputs::SCANNED_REASON),
            Self::Ok | Self::Failed(_) => None,
        }
    }

    /// The input kind of an `Unsupported` verdict.
    fn kind(&self) -> Option<&'static str> {
        if let Self::Unsupported { kind, .. } = self {
            Some(kind)
        } else {
            None
        }
    }
}

/// What the main thread learns about one finished input of the default
/// command; small on purpose so the channel never holds whole results.
struct Done {
    /// Position among the expanded inputs (stdout output keeps this order).
    index: usize,
    path: String,
    pages: u32,
    bytes: u64,
    ms: f64,
    verdict: Verdict,
    /// Page text for `--stdout`.
    text: Option<String>,
    /// The record for `--bib`.
    record: Option<bibliography::Record>,
    /// Something worth telling the user about this input (a renamed output).
    note: Option<String>,
}

impl Done {
    /// A blank, successful outcome for input `index` at `path`.
    fn new(index: usize, path: String) -> Self {
        Self {
            index,
            path,
            pages: 0,
            bytes: 0,
            ms: 0.0,
            verdict: Verdict::Ok,
            text: None,
            record: None,
            note: None,
        }
    }

    /// The small JSON record written for a refused input (`<stem>.json`
    /// with `--json`): status, kind, reason, path and page count.
    fn refusal_record(&self) -> serde_json::Value {
        serde_json::json!({
            "status": self.verdict.status(),
            "kind": self.verdict.kind(),
            "reason": self.verdict.reason(),
            "path": self.path,
            "pages": self.pages,
        })
    }
}

/// Print one finished input to stderr: a JSON `done` event with
/// `--progress`, else `ok  <pages>p  <size>  <ms> ms  <path>`,
/// `FAILED  <path>: <reason>`, `SKIP  <kind>  <path>` or `SCAN  <pages>p  <path>`.
fn report_done(args: &RunArgs, done: &Done) {
    if args.progress {
        let error = if let Verdict::Failed(error) = &done.verdict {
            Some(error.as_str())
        } else {
            None
        };
        let value = serde_json::json!({
            "event": "done", "path": done.path, "status": done.verdict.status(),
            "ok": done.verdict.succeeded(), "pages": done.pages, "bytes": done.bytes,
            "ms": done.ms, "error": error, "kind": done.verdict.kind(),
            "reason": done.verdict.reason(),
        });
        eprintln!("{value}");
    } else {
        match &done.verdict {
            Verdict::Ok => eprintln!(
                "ok  {}p  {}  {:.0} ms  {}",
                done.pages,
                human_size(done.bytes),
                done.ms,
                done.path
            ),
            Verdict::Failed(error) => eprintln!("FAILED  {}: {error}", done.path),
            Verdict::Unsupported { kind, .. } => eprintln!("SKIP  {kind}  {}", done.path),
            Verdict::Scanned => eprintln!("SCAN  {}p  {}", done.pages, done.path),
        }
    }
    if let Some(note) = &done.note {
        report_note(args, &done.path, note);
    }
}

/// Print a per-input remark to stderr (JSON `note` event with `--progress`).
fn report_note(args: &RunArgs, path: &str, message: &str) {
    if args.progress {
        let value = serde_json::json!({"event": "note", "path": path, "message": message});
        eprintln!("{value}");
    } else {
        eprintln!("note  {path}: {message}");
    }
}

/// Print the final summary line to stderr.
fn report_summary(args: &RunArgs, ok: usize, failed: usize, ms: f64) {
    if args.progress {
        let value = serde_json::json!({"event": "summary", "ok": ok, "failed": failed, "ms": ms});
        eprintln!("{value}");
    } else {
        eprintln!("{ok} ok, {failed} failed, {ms:.0} ms");
    }
}

/// Run `work(index)` for every index below `count` on `workers` scoped
/// threads that pull from one queue (the same bounded pool as
/// `run_extract`), handing each outcome to `sink` on the calling thread.
/// An error from `sink` stops the pool.
fn run_pool<T, W, S>(count: usize, workers: usize, work: W, mut sink: S) -> anyhow::Result<()>
where
    T: Send,
    W: Fn(usize) -> T + Sync,
    S: FnMut(T) -> anyhow::Result<()>,
{
    let queue: Mutex<VecDeque<usize>> = Mutex::new((0..count).collect());
    let (sender, receiver) = mpsc::channel::<T>();
    thread::scope(|scope| -> anyhow::Result<()> {
        for _ in 0..workers {
            let sender = sender.clone();
            let queue = &queue;
            let work = &work;
            scope.spawn(move || {
                while let Some(index) = next_item(queue) {
                    if sender.send(work(index)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(sender);
        for outcome in receiver {
            sink(outcome)?;
        }
        Ok(())
    })
}

/// Reject flag combinations that would silently do nothing.
fn check_run_flags(args: &RunArgs) -> anyhow::Result<()> {
    let text = &args.text;
    if args.bib {
        if text.json || text.stdout || text.no_images {
            bail!("--json, --stdout and --no-images apply to text mode, not to --bib");
        }
    } else {
        if args.bibliography.db.is_some() || args.bibliography.pdfium_fallback {
            bail!("--db and --pdfium-fallback require --bib");
        }
        if text.stdout && (args.out.is_some() || text.json) {
            bail!("--stdout writes no files, so --out and --json do not apply");
        }
    }
    Ok(())
}

/// The default command: expand the paths, run text or bibliography mode
/// over a worker pool, then print the once-a-day update notice if any.
fn run_paths(args: &RunArgs) -> anyhow::Result<ExitCode> {
    check_run_flags(args)?;
    check_backend(&args.backend)?;
    let passive = update::PassiveCheck::start();
    pipeline::warm_up();
    let inputs = inputs::expand(&args.paths).context("listing the input files")?;
    if inputs.is_empty() {
        bail!("no PDF files found under the given paths");
    }
    let exit = if args.bib {
        run_bib(args, &inputs)?
    } else {
        run_text(args, inputs)?
    };
    if let Some(notice) = passive.finish() {
        if args.progress {
            let value = serde_json::json!({"event": "update", "message": notice});
            eprintln!("{value}");
        } else {
            eprintln!("{notice}");
        }
    }
    Ok(exit)
}

/// The page texts of `result` separated by form feeds, as `write_outputs`
/// writes them.
fn joined_text(result: &ExtractionResult) -> String {
    let texts: Vec<&str> = result.pages.iter().map(|p| p.text.as_str()).collect();
    texts.join("\u{c}")
}

/// Write the refusal record of `done` as `<stem>.json` when `--json` asked
/// for JSON output.
fn write_refusal(args: &RunArgs, stem: &Path, done: &Done) -> Result<(), String> {
    if !args.text.json || args.text.stdout {
        return Ok(());
    }
    if let Some(parent) = stem.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|e| format!("creating {}: {e}", parent.display()))?;
    }
    let json_path = with_suffix(stem, "json");
    let json = serde_json::to_string_pretty(&done.refusal_record()).map_err(|e| e.to_string())?;
    fs::write(&json_path, json).map_err(|e| format!("writing {}: {e}", json_path.display()))
}

/// Extract one input and write its outputs, filling `done` as it goes.
fn text_job(args: &RunArgs, plan: &Planned, done: &mut Done) -> Result<(), String> {
    let path = done.path.clone();
    let mut stem = plan.stem.clone();
    // Leading bytes decide what the file is; the extension never does.
    let kind = inputs::classify(&plan.input.path).map_err(|e| format!("io: {e}"))?;
    if kind != inputs::Kind::Pdf {
        done.verdict = Verdict::Unsupported {
            kind: kind.name(),
            reason: kind.reason(),
        };
        return write_refusal(args, &stem, done);
    }
    if plan.needs_suffix && !args.text.stdout {
        // The suffix needs the hash before the job names its figures
        // directory, so a colliding input is read once more here.
        let snapshot =
            tpe::acquire::snapshot(&plan.input.path, args.max_bytes).map_err(|e| e.to_string())?;
        let name = stem.file_name().map(|n| n.to_string_lossy().into_owned());
        let name = name.unwrap_or_default();
        stem.set_file_name(format!("{name}-{}", short_hash(&snapshot.hash.0)));
        done.note = Some(format!(
            "output name already taken; writing {}.txt instead",
            stem.display()
        ));
    }
    let figures = with_suffix(&stem, "figures");
    let figures_dir = (!args.text.no_images && !args.text.stdout)
        .then(|| figures.to_string_lossy().into_owned());
    let job = Job {
        path: path.clone(),
        backend: args.backend.clone(),
        pages: None,
        password: args.password.clone(),
        max_bytes: args.max_bytes,
        figures_dir,
    };
    let mut observe = |event: Progress| {
        if args.progress {
            report_progress(&path, event);
        }
    };
    let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        pipeline::run_job_observed(&job, &mut observe)
    }))
    .map_or_else(
        |payload| Err(format!("panic: {}", panic_message(&*payload))),
        |outcome| outcome.map_err(|err| err.to_string()),
    )?;
    done.pages = result.document.pages;
    done.bytes = result.document.size;
    if inputs::looks_scanned(&result.pages) {
        // Figures (if enabled) were exported by the job; no empty text file.
        done.verdict = Verdict::Scanned;
        return write_refusal(args, &stem, done);
    }
    let text = joined_text(&result);
    if args.text.stdout {
        done.text = Some(text);
    } else {
        if let Some(parent) = stem.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|e| format!("creating {}: {e}", parent.display()))?;
        }
        let text_path = with_suffix(&stem, "txt");
        fs::write(&text_path, text).map_err(|e| format!("writing {}: {e}", text_path.display()))?;
        if args.text.json {
            let json_path = with_suffix(&stem, "json");
            let json = serde_json::to_string_pretty(&result).map_err(|e| e.to_string())?;
            fs::write(&json_path, json)
                .map_err(|e| format!("writing {}: {e}", json_path.display()))?;
        }
    }
    if result.status == Status::Failed {
        let reason = result
            .warnings
            .first()
            .cloned()
            .unwrap_or_else(|| "extraction failed".to_string());
        return Err(reason);
    }
    Ok(())
}

/// Text mode for one input, on a worker thread.
fn text_one(args: &RunArgs, plan: &Planned, index: usize) -> Done {
    let started = Instant::now();
    let mut done = Done::new(index, plan.input.path.to_string_lossy().into_owned());
    if let Err(error) = text_job(args, plan, &mut done) {
        done.verdict = Verdict::Failed(error);
    }
    done.ms = elapsed_ms(started);
    done
}

/// `tpe PATH...`: text (and images) per PDF into `--out`, or to stdout.
fn run_text(args: &RunArgs, inputs: Vec<Input>) -> anyhow::Result<ExitCode> {
    let out_dir = args
        .out
        .clone()
        .unwrap_or_else(|| PathBuf::from(DEFAULT_OUT_DIR));
    let plans = inputs::plan(inputs, &out_dir);
    if !args.text.stdout {
        fs::create_dir_all(&out_dir)
            .with_context(|| format!("creating output directory {}", out_dir.display()))?;
    }
    let workers = worker_count(args.jobs, plans.len());
    let started = Instant::now();
    let (mut ok, mut failed) = (0usize, 0usize);
    // `--stdout` prints inputs in command-line order, so texts that finish
    // early wait here for their turn.
    let mut pending: BTreeMap<usize, Option<String>> = BTreeMap::new();
    let mut next = 0usize;
    let mut first = true;
    let mut stdout = io::stdout().lock();
    run_pool(
        plans.len(),
        workers,
        |index| text_one(args, &plans[index], index),
        |done| {
            report_done(args, &done);
            if done.verdict.succeeded() {
                ok += 1;
            } else {
                failed += 1;
            }
            if args.text.stdout {
                pending.insert(done.index, done.text);
                while let Some(text) = pending.remove(&next) {
                    next += 1;
                    let Some(text) = text else { continue };
                    if !first {
                        stdout.write_all(b"\x0c")?;
                    }
                    stdout.write_all(text.as_bytes())?;
                    first = false;
                }
            }
            Ok(())
        },
    )?;
    stdout.flush()?;
    report_summary(args, ok, failed, elapsed_ms(started));
    Ok(exit_code(failed > 0))
}

/// One backward scan of a file, plus what the scanned-document check needs.
struct BibScan {
    record: bibliography::Record,
    /// Input size in bytes (0 when it could not be read).
    size: u64,
    /// The input bytes, kept so a `not_found` scan can be checked for a
    /// missing text layer without reading the file again.
    bytes: Option<Vec<u8>>,
}

/// One backward scan of `path` with `extractor`, exactly as
/// `tpe bibliography` performs it.
fn bib_scan(args: &RunArgs, extractor: &dyn Extractor, path: &Path, file: &str) -> BibScan {
    let started = Instant::now();
    let mut hash = None;
    let mut size = 0u64;
    let mut bytes: Option<Vec<u8>> = None;
    let mut observe = |event: Progress| {
        if args.progress {
            report_progress(file, event);
        }
    };
    let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        let snapshot = tpe::acquire::snapshot(path, args.max_bytes)?;
        hash = Some(snapshot.hash.0);
        size = snapshot.source.size;
        let scan = bibliography::scan_backward_observed(
            extractor,
            &snapshot.bytes,
            args.password.as_deref(),
            &mut observe,
        )?;
        bytes = Some(snapshot.bytes);
        Ok::<_, anyhow::Error>(scan)
    }));
    let elapsed = elapsed_ms(started);
    let identity = extractor.identity();
    let record = match result {
        Ok(Ok(scan)) => {
            bibliography::Record::from_scan(file, hash.unwrap_or_default(), identity, scan, elapsed)
        }
        Ok(Err(err)) => {
            bibliography::Record::failed(file, hash, identity, err.to_string(), elapsed)
        }
        Err(payload) => {
            let message = format!("panic: {}", panic_message(&*payload));
            bibliography::Record::failed(file, hash, identity, message, elapsed)
        }
    };
    BibScan {
        record,
        size,
        bytes,
    }
}

/// Whether the last `scanned` of `total` pages of `bytes` (the pages a
/// `not_found` scan read) have no text layer. Any failure answers `false`.
fn tail_is_scanned(
    extractor: &dyn Extractor,
    bytes: &[u8],
    password: Option<&str>,
    total: u32,
    scanned: u32,
) -> bool {
    if total == 0 || scanned == 0 {
        return false;
    }
    let first = total.saturating_sub(scanned).saturating_add(1).max(1);
    let outcome = panic::catch_unwind(panic::AssertUnwindSafe(|| -> Option<bool> {
        let mut session = extractor.open(bytes, password).ok()?;
        let count = session.page_count().min(total);
        let mut pages = Vec::new();
        for page in first..=count {
            pages.push(session.page_text(page).ok()?);
        }
        Some(inputs::looks_scanned(&pages))
    }));
    matches!(outcome, Ok(Some(true)))
}

/// Warning added to a `not_found` record whose scanned pages have no text.
const SCANNED_WARNING: &str = "scanned document: no text layer";

/// Bibliography mode for one input, on a worker thread. A non-PDF input is
/// refused before any scan. With a `fallback` backend, a scan that finds no
/// list is repeated with it and the fallback record is kept only when it
/// found one. A `not_found` scan over image-only pages is marked scanned.
fn bib_one(
    args: &RunArgs,
    extractor: &dyn Extractor,
    fallback: Option<&dyn Extractor>,
    input: &Input,
    index: usize,
) -> Done {
    let started = Instant::now();
    let file = input.path.to_string_lossy().into_owned();
    let mut done = Done::new(index, file.clone());
    let kind = match inputs::classify(&input.path) {
        Ok(kind) => kind,
        Err(err) => {
            let reason = format!("io: {err}");
            let identity = extractor.identity();
            let record = bibliography::Record::failed(&file, None, identity, reason.clone(), 0.0);
            done.record = Some(record);
            done.verdict = Verdict::Failed(reason);
            done.ms = elapsed_ms(started);
            return done;
        }
    };
    if kind != inputs::Kind::Pdf {
        let reason = kind.reason();
        let identity = extractor.identity();
        let mut record = bibliography::Record::failed(&file, None, identity, reason.clone(), 0.0);
        record.status = "unsupported";
        done.record = Some(record);
        done.verdict = Verdict::Unsupported {
            kind: kind.name(),
            reason,
        };
        done.ms = elapsed_ms(started);
        return done;
    }
    let mut scan = bib_scan(args, extractor, &input.path, &file);
    let mut backend_used = extractor;
    if !scan.record.found()
        && let Some(fallback) = fallback
    {
        let second = bib_scan(args, fallback, &input.path, &file);
        if second.record.found() {
            scan = second;
            backend_used = fallback;
        }
    }
    let BibScan {
        mut record,
        size,
        bytes,
    } = scan;
    if record.status == "not_found"
        && let Some(bytes) = &bytes
        && tail_is_scanned(
            backend_used,
            bytes,
            args.password.as_deref(),
            record.total_pages.unwrap_or(0),
            record.pages_scanned.unwrap_or(0),
        )
    {
        record.warnings.push(SCANNED_WARNING.to_string());
        done.verdict = Verdict::Scanned;
    } else if !record.found() {
        done.verdict = Verdict::Failed(
            record
                .error
                .clone()
                .unwrap_or_else(|| "no reference list found".to_string()),
        );
    }
    done.pages = record.total_pages.unwrap_or(0);
    done.bytes = size;
    done.ms = elapsed_ms(started);
    done.record = Some(record);
    done
}

/// The JSON line for a `--bib` record: the `tpe bibliography` shape, plus
/// `kind` and `reason` for a refused input and `reason: "scanned"` for a
/// scanned one.
fn bib_record_json(done: &Done, record: &bibliography::Record) -> anyhow::Result<String> {
    let mut value = serde_json::to_value(record)?;
    match &done.verdict {
        Verdict::Unsupported { kind, reason } => {
            value["kind"] = serde_json::json!(kind);
            value["reason"] = serde_json::json!(reason);
        }
        Verdict::Scanned => {
            value["reason"] = serde_json::json!("scanned");
        }
        Verdict::Ok | Verdict::Failed(_) => {}
    }
    Ok(serde_json::to_string(&value)?)
}

/// The `pdfium` extractor for `--pdfium-fallback`, when that backend is
/// compiled in and is not already the main backend.
fn fallback_extractor(args: &RunArgs) -> Option<Box<dyn Extractor>> {
    if !args.bibliography.pdfium_fallback || args.backend == "pdfium" {
        return None;
    }
    if !backend::available().contains(&"pdfium") {
        return None;
    }
    backend::by_name("pdfium")
}

/// `tpe --bib PATH...`: one `bibliography::Record` JSON line per PDF to
/// stdout or `--out FILE`, optionally mirrored into `--db FILE`.
fn run_bib(args: &RunArgs, inputs: &[Input]) -> anyhow::Result<ExitCode> {
    let extractor = backend::by_name(&args.backend)
        .ok_or_else(|| anyhow!("backend `{}` is unavailable", args.backend))?;
    let fallback = fallback_extractor(args);
    let mut db = match &args.bibliography.db {
        Some(path) => Some(
            BibDb::open(path)
                .with_context(|| format!("opening bibliography database {}", path.display()))?,
        ),
        None => None,
    };
    let mut out: Box<dyn Write> = match &args.out {
        Some(path) => {
            let file =
                fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
            Box::new(io::BufWriter::new(file))
        }
        None => Box::new(io::stdout().lock()),
    };
    let workers = worker_count(args.jobs, inputs.len());
    let started = Instant::now();
    let (mut ok, mut failed) = (0usize, 0usize);
    run_pool(
        inputs.len(),
        workers,
        |index| {
            bib_one(
                args,
                extractor.as_ref(),
                fallback.as_deref(),
                &inputs[index],
                index,
            )
        },
        |done| {
            report_done(args, &done);
            if done.verdict.succeeded() {
                ok += 1;
            } else {
                failed += 1;
            }
            let Some(record) = &done.record else {
                return Ok(());
            };
            writeln!(out, "{}", bib_record_json(&done, record)?)?;
            if let Some(db) = db.as_mut() {
                match db.write(record) {
                    Ok(_) => {}
                    Err(BibDbError::NoHash(_)) => {
                        report_note(args, &done.path, "not stored in the database (no hash)");
                    }
                    Err(err) => bail!("writing {} to the database: {err}", done.path),
                }
            }
            Ok(())
        },
    )?;
    out.flush()?;
    report_summary(args, ok, failed, elapsed_ms(started));
    Ok(exit_code(failed > 0))
}

#[cfg(test)]
mod tests {
    use super::{
        ManifestItem, Split, check_backend, human_size, parse_pages, percentile, probe_backend,
        short_hash, with_suffix, worker_count,
    };
    use tpe::backend;

    #[test]
    fn human_sizes_are_short() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(999), "999 B");
        assert_eq!(human_size(12_345), "12.3 kB");
        assert_eq!(human_size(1_300_000), "1.3 MB");
        assert_eq!(human_size(2_000_000_000), "2.0 GB");
    }

    #[test]
    fn suffix_keeps_dots_in_the_stem() {
        let stem = std::path::Path::new("out/paper.v2");
        assert_eq!(with_suffix(stem, "txt"), std::path::Path::new("out/paper.v2.txt"));
        let short = std::path::Path::new("a");
        assert_eq!(with_suffix(short, "figures"), std::path::Path::new("a.figures"));
    }

    #[test]
    fn worker_count_is_bounded_by_inputs() {
        assert_eq!(worker_count(Some(8), 3), 3);
        assert_eq!(worker_count(Some(0), 3), 1);
        assert_eq!(worker_count(Some(2), 0), 1);
        assert!(worker_count(None, 100) >= 1);
        assert_eq!(worker_count(None, 1), 1);
    }

    /// A manifest item in the given split; the other fields do not matter here.
    fn item(split: &str) -> ManifestItem {
        ManifestItem {
            id: "arxiv:2108.04588".to_string(),
            kind: "arxiv".to_string(),
            license: "http://creativecommons.org/licenses/by/4.0/".to_string(),
            pdf_url: "https://arxiv.org/pdf/2108.04588".to_string(),
            source_url: Some("https://arxiv.org/e-print/2108.04588".to_string()),
            pdf_sha256: None,
            source_sha256: None,
            categories: vec!["cs.CG".to_string()],
            split: split.to_string(),
            notes: None,
        }
    }

    #[test]
    fn split_selects_manifest_items() {
        assert!(Split::All.includes(&item("dev")));
        assert!(Split::All.includes(&item("holdout")));
        assert!(Split::Dev.includes(&item("dev")));
        assert!(!Split::Dev.includes(&item("holdout")));
        assert!(Split::Holdout.includes(&item("holdout")));
        assert!(!Split::Holdout.includes(&item("dev")));
    }

    #[test]
    fn short_hash_takes_twelve_digits() {
        assert_eq!(short_hash("0123456789abcdef"), "0123456789ab");
        assert_eq!(short_hash("abc"), "abc");
        assert_eq!(short_hash(""), "");
    }

    #[test]
    fn parses_ranges_and_single_pages() {
        assert_eq!(parse_pages("3-7").unwrap(), (3, 7));
        assert_eq!(parse_pages(" 2 - 2 ").unwrap(), (2, 2));
        assert_eq!(parse_pages("5").unwrap(), (5, 5));
    }

    #[test]
    fn rejects_bad_ranges() {
        assert!(parse_pages("0-3").is_err());
        assert!(parse_pages("7-3").is_err());
        assert!(parse_pages("a-b").is_err());
        assert!(parse_pages("").is_err());
    }

    #[test]
    fn percentile_uses_nearest_rank() {
        let samples = [1.0, 2.0, 3.0, 4.0, 5.0];
        assert!((percentile(&samples, 0.5) - 3.0).abs() < f64::EPSILON);
        assert!((percentile(&samples, 0.95) - 5.0).abs() < f64::EPSILON);
        assert!((percentile(&samples, 0.0) - 1.0).abs() < f64::EPSILON);
        assert!(percentile(&[], 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn backend_names_are_checked_against_this_build() {
        assert!(check_backend("lopdf").is_ok());
        let unknown = check_backend("nope").unwrap_err().to_string();
        assert!(unknown.contains("unknown backend"), "{unknown}");
        for name in backend::ALL_KNOWN {
            let outcome = check_backend(name);
            if backend::available().contains(name) {
                assert!(outcome.is_ok(), "{name}");
            } else {
                let message = outcome.unwrap_err().to_string();
                assert!(message.contains("not compiled"), "{message}");
            }
        }
    }

    #[test]
    fn lopdf_opens_the_probe() {
        let probe = backend::probe_pdf().unwrap();
        assert_eq!(probe_backend("lopdf", &probe), "opens (1 page probe)");
        assert_eq!(probe_backend("nope", &probe), "not resolvable");
    }
}
