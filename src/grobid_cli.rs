//! Explicit server command, using the existing disposable-worker supervisor.
use std::fs::File;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use tpe::grobid::{Client, Options};

#[derive(clap::Args, Clone, Serialize, Deserialize)]
pub(super) struct Args {
    /// PDF sent only to `TPE_GROBID_URL`. No server is chosen automatically.
    path: PathBuf,
    #[arg(long, default_value_t = 60_000)]
    pub timeout_ms: u64,
    #[arg(long, default_value_t = 512)]
    pub max_memory_growth_mib: u64,
    /// Optional explicit input cap; omitted means no PDF byte cap.
    #[arg(long)]
    max_input_bytes: Option<u64>,
    /// Optional explicit TEI/projection cap; omitted means no response byte cap.
    #[arg(long)]
    max_response_bytes: Option<usize>,
    /// Explicitly authorize server-side external metadata lookups: 0 off, 1 full, 2 DOI only.
    #[arg(long, default_value_t = 0)]
    consolidation: u8,
}
impl Args {
    fn options(&self) -> anyhow::Result<Options> {
        ensure!(
            (32..=4096).contains(&self.max_memory_growth_mib),
            "--max-memory-growth-mib must be 32..=4096"
        );
        Ok(Options {
            timeout_ms: self.timeout_ms,
            max_input_bytes: self.max_input_bytes,
            max_response_bytes: self.max_response_bytes,
            consolidation: self.consolidation,
        }
        .validate()?)
    }
}
pub(super) fn run(args: &Args) -> anyhow::Result<ExitCode> {
    args.options()?;
    super::cli_worker::run_grobid(args)
}
pub(super) fn worker(encoded: &[u8]) -> anyhow::Result<()> {
    let args: Args = serde_json::from_slice(encoded)?;
    let options = args.options()?;
    // Validate endpoint/token before reading any document; neither goes in a request file.
    let client = Client::from_env(options)?;
    let source = File::open(&args.path).context("opening GROBID input")?;
    ensure!(
        source.metadata()?.is_file(),
        "GROBID input must be a regular file"
    );
    let result = client.process_reader(source)?;
    let mut output = io::stdout().lock();
    serde_json::to_writer(&mut output, &result)?;
    output.write_all(b"\n")?;
    Ok(())
}
