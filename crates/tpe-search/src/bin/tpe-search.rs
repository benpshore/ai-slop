//! `tpe-search`: build and query a lexical + semantic search index over the
//! text stored in a `tpe` ledger.
//!
//! ```text
//! tpe-search index --ledger tpe.sqlite --index search/ [--embedder hash|onnx] [--rebuild]
//! tpe-search query --index search/ [--k 10] [--mode lexical|semantic|hybrid] [--alpha 0.5] <TEXT>...
//! tpe-search stats --index search/
//! ```

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use tpe_search::{Embedder, HashEmbedder, SearchError, SearchIndex, SearchMode};

#[derive(Parser)]
#[command(
    name = "tpe-search",
    version,
    about = "Lexical and semantic search over a tpe ledger"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Index (or update the index with) every document in a ledger.
    Index {
        /// The `tpe` ledger (opened read-only).
        #[arg(long)]
        ledger: PathBuf,
        /// Index directory (created if missing).
        #[arg(long)]
        index: PathBuf,
        /// Embedder for semantic search; `onnx` needs the `onnx` feature.
        #[arg(long, value_enum, default_value = "hash")]
        embedder: EmbedderKind,
        /// Discard the existing index first (needed to switch embedder).
        #[arg(long)]
        rebuild: bool,
    },
    /// Search the index.
    Query {
        /// Index directory.
        #[arg(long)]
        index: PathBuf,
        /// Number of results.
        #[arg(long, default_value = "10")]
        k: usize,
        /// Ranking mode.
        #[arg(long, value_enum, default_value = "hybrid")]
        mode: ModeArg,
        /// Hybrid weight of the semantic ranking (0 = lexical only, 1 = semantic only).
        #[arg(long, default_value = "0.5")]
        alpha: f64,
        /// Query text.
        #[arg(required = true)]
        text: Vec<String>,
    },
    /// Print index size and configuration.
    Stats {
        /// Index directory.
        #[arg(long)]
        index: PathBuf,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum EmbedderKind {
    Hash,
    Onnx,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum ModeArg {
    Lexical,
    Semantic,
    Hybrid,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match &cli.command {
        Command::Index {
            ledger,
            index,
            embedder,
            rebuild,
        } => run_index(ledger, index, *embedder, *rebuild),
        Command::Query {
            index,
            k,
            mode,
            alpha,
            text,
        } => run_query(index, *k, *mode, *alpha, &text.join(" ")),
        Command::Stats { index } => run_stats(index),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("tpe-search: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run_index(
    ledger: &Path,
    dir: &Path,
    kind: EmbedderKind,
    rebuild: bool,
) -> Result<(), SearchError> {
    let embedder = make_embedder(kind)?;
    let mut index = SearchIndex::open(dir)?;
    if rebuild {
        index.reset()?;
    }
    let stats = index.index_ledger(ledger, embedder.as_ref())?;
    println!("documents_seen: {}", stats.documents_seen);
    println!("documents_indexed: {}", stats.documents_indexed);
    println!("documents_unchanged: {}", stats.documents_unchanged);
    println!("chunks_added: {}", stats.chunks_added);
    println!("chunks_removed: {}", stats.chunks_removed);
    println!("rebuilt: {}", stats.rebuilt);
    Ok(())
}

fn run_query(
    dir: &Path,
    k: usize,
    mode: ModeArg,
    alpha: f64,
    text: &str,
) -> Result<(), SearchError> {
    let index = SearchIndex::open(dir)?;
    // Lexical search never embeds, so it must not need (or load) a model.
    let embedder: Box<dyn Embedder> = if mode == ModeArg::Lexical {
        Box::new(HashEmbedder::default())
    } else {
        embedder_for_index(&index)?
    };
    let mode = match mode {
        ModeArg::Lexical => SearchMode::Lexical,
        ModeArg::Semantic => SearchMode::Semantic,
        ModeArg::Hybrid => SearchMode::Hybrid { alpha },
    };
    for (rank, hit) in index
        .search(text, k, mode, embedder.as_ref())?
        .iter()
        .enumerate()
    {
        let short = hit.doc_hash.get(..12).unwrap_or(&hit.doc_hash);
        let title = hit.title.as_deref().unwrap_or("-");
        let doi = hit.doi.as_deref().unwrap_or("-");
        println!(
            "{}\t{:.4}\t{short}\tp{}\t#{}\t{title}\t{doi}\t{}",
            rank + 1,
            hit.score,
            hit.page,
            hit.idx,
            hit.snippet
        );
    }
    Ok(())
}

fn run_stats(dir: &Path) -> Result<(), SearchError> {
    let summary = SearchIndex::open(dir)?.stats()?;
    println!("documents: {}", summary.documents);
    println!("chunks: {}", summary.chunks);
    println!("vectors: {}", summary.vectors);
    match summary.dim {
        Some(dim) => println!("dim: {dim}"),
        None => println!("dim: -"),
    }
    println!("embedder: {}", summary.embedder.as_deref().unwrap_or("-"));
    println!("store: {}", summary.store);
    Ok(())
}

/// The embedder the index was built with, reconstructed from its name.
fn embedder_for_index(index: &SearchIndex) -> Result<Box<dyn Embedder>, SearchError> {
    let Some(name) = index.stats()?.embedder else {
        return Ok(Box::new(HashEmbedder::default()));
    };
    if let Some(dim) = name
        .strip_prefix("hash-")
        .and_then(|d| d.parse::<usize>().ok())
    {
        return Ok(Box::new(HashEmbedder::new(dim)));
    }
    if name.starts_with("onnx-") {
        return make_embedder(EmbedderKind::Onnx);
    }
    Err(SearchError::Embed(format!(
        "unknown embedder `{name}` recorded in the index"
    )))
}

fn make_embedder(kind: EmbedderKind) -> Result<Box<dyn Embedder>, SearchError> {
    match kind {
        EmbedderKind::Hash => Ok(Box::new(HashEmbedder::default())),
        EmbedderKind::Onnx => onnx_embedder(),
    }
}

#[cfg(feature = "onnx")]
fn onnx_embedder() -> Result<Box<dyn Embedder>, SearchError> {
    Ok(Box::new(tpe_search::OnnxEmbedder::from_env()?))
}

#[cfg(not(feature = "onnx"))]
fn onnx_embedder() -> Result<Box<dyn Embedder>, SearchError> {
    Err(SearchError::Embed(
        "tpe-search was built without the `onnx` feature".to_string(),
    ))
}
