//! Narrow `PDFium` text-page evidence experiment; no production routing changes.
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use tpe::pdfium_probe::{Limits, Outcome, run, worker};

#[derive(Parser)]
#[command(about = "Experimental, Rust-supervised PDFium character evidence (Linux)")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Extract `PDFium` text-page characters in a disposable, bounded worker.
    Run {
        input: PathBuf,
        #[arg(long, default_value_t = 30_000)]
        timeout_ms: u64,
        #[arg(long, default_value_t = 50)]
        max_pages: u16,
        #[arg(long, default_value_t = 250_000)]
        max_chars: usize,
        #[arg(long, default_value_t = 32 * 1024 * 1024)]
        max_output_bytes: u64,
    },
    #[command(hide = true)]
    Worker {
        input: PathBuf,
        output: PathBuf,
        limits: String,
    },
}

fn main() -> ExitCode {
    let result = match Cli::parse().command {
        Command::Run {
            input,
            timeout_ms,
            max_pages,
            max_chars,
            max_output_bytes,
        } => run(
            &input,
            Limits {
                timeout_ms,
                max_pages,
                max_chars,
                max_output_bytes,
                ..Limits::default()
            },
        )
        .and_then(|report| {
            serde_json::to_writer(std::io::stdout().lock(), &report)?;
            println!();
            Ok(if report.outcome == Outcome::Complete {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(2)
            })
        }),
        Command::Worker {
            input,
            output,
            limits,
        } => serde_json::from_str(&limits)
            .map_err(anyhow::Error::from)
            .and_then(|limits| worker(&input, &output, limits))
            .map(|()| ExitCode::SUCCESS),
    };
    match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("{error:#}");
            ExitCode::from(2)
        }
    }
}
