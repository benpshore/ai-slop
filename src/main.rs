//! `tpe` command-line interface: extract PDFs into a ledger, query the ledger
//! and benchmark the extraction stages.

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Mutex, mpsc};
use std::thread;
use std::time::Instant;

use anyhow::{Context, anyhow, bail};
use clap::{Args, Parser, Subcommand};
use rusqlite::OptionalExtension;

use tpe::backend;
use tpe::ledger::Ledger;
use tpe::pipeline::{self, PipelineError};
use tpe::schema::{ExtractionResult, Job, Metadata, Status};

/// Service-time target per 20-page chunk, in milliseconds.
const TARGET_MS_PER_CHUNK: f64 = 30.0;

#[derive(Parser)]
#[command(name = "tpe", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Extract text, metadata and citations from PDF files into a ledger.
    Extract(ExtractArgs),
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

fn main() -> anyhow::Result<ExitCode> {
    let cli = Cli::parse();
    match cli.command {
        Cmd::Extract(args) => run_extract(&args),
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

/// Open the ledger at `db`, naming the path in any error.
fn open_ledger(db: &Path) -> anyhow::Result<Ledger> {
    Ledger::open(db).with_context(|| format!("opening ledger {}", db.display()))
}

/// Fail early when the backend name is unknown.
fn check_backend(name: &str) -> anyhow::Result<()> {
    if backend::by_name(name).is_none() {
        let known = backend::NAMES.join(", ");
        bail!("unknown backend `{name}`; known backends: {known}");
    }
    Ok(())
}

/// Outcome of one worker job, sent to the main thread over the channel.
struct Outcome {
    path: PathBuf,
    wall_ms: f64,
    result: Result<ExtractionResult, PipelineError>,
}

/// Pop the next input path, or `None` when the queue is empty or poisoned.
fn next_path(queue: &Mutex<VecDeque<PathBuf>>) -> Option<PathBuf> {
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
    };
    let start = Instant::now();
    let result = pipeline::run_job(&job);
    Outcome {
        path,
        wall_ms: elapsed_ms(start),
        result,
    }
}

fn run_extract(args: &ExtractArgs) -> anyhow::Result<ExitCode> {
    check_backend(&args.backend)?;
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
                while let Some(path) = next_path(queue) {
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

    Ok(if any_failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
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
            if let Some(source) = result.document.sources.first() {
                ledger
                    .record_source(&result.document.hash, result.document.size, source)
                    .with_context(|| format!("recording source for {path_display}"))?;
            }
            ledger
                .write_result(&result)
                .with_context(|| format!("writing result for {path_display}"))?;
            result.timings.write_ms = elapsed_ms(write_start);
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
                    "error": err.to_string(),
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
    let hash = result.document.hash.0.as_str();
    let short = &hash[..hash.len().min(12)];
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
    Ok(())
}

/// Find the most recently finished run whose document hash starts with
/// `prefix`, using the ledger's `runs` table directly.
fn latest_run_for_prefix(db: &Path, prefix: &str) -> anyhow::Result<Option<i64>> {
    let connection = rusqlite::Connection::open(db)
        .with_context(|| format!("opening ledger {}", db.display()))?;
    let prefix_len = i64::try_from(prefix.len())?;
    let run_id: Option<i64> = connection
        .query_row(
            "SELECT id FROM runs WHERE substr(hash, 1, ?2) = ?1 \
             ORDER BY finished_at DESC, id DESC LIMIT 1",
            rusqlite::params![prefix, prefix_len],
            |row| row.get(0),
        )
        .optional()
        .context("looking up run by hash prefix")?;
    Ok(run_id)
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
    let run_id = latest_run_for_prefix(&args.db, &prefix)?
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
    let iterations = args.iterations.max(1);
    let mut all_samples: Vec<f64> = Vec::new();
    for path in &args.paths {
        let job = Job {
            path: path.to_string_lossy().into_owned(),
            backend: args.backend.clone(),
            pages: None,
            password: None,
            max_bytes: None,
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

#[cfg(test)]
mod tests {
    use super::{parse_pages, percentile};

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
}
