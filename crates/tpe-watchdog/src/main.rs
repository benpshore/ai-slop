use std::fs;
use std::path::PathBuf;
use std::sync::mpsc;

use anyhow::Result;
use clap::Parser;

#[derive(Parser)]
struct Args {
    #[arg(long)]
    config: PathBuf,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let config = serde_json::from_slice(&fs::read(args.config)?)?;
    let (_stop_tx, stop_rx) = mpsc::channel();
    tpe_watchdog::run(config, stop_rx)
}
