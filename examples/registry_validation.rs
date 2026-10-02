//! Run independently labeled entries through the real production Resolver.
//! No HTTP substitute, server, registry-derived labels or accuracy assertion.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use clap::Parser;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tpe::resolve::Resolver;
use tpe::schema::ReferenceEntry;

#[derive(Parser)]
struct Args {
    #[arg(long)]
    cohort: PathBuf,
    #[arg(long)]
    out: PathBuf,
    /// Exact source SHA of this build; must agree with the compile-time identity.
    #[arg(long)]
    code_sha: String,
    /// Optional contact email for the registries' normal polite pools.
    #[arg(long)]
    mailto: Option<String>,
    /// Stop between cases after this elapsed wall time; unrun cases remain scored.
    #[arg(long, default_value_t = 1200)]
    time_budget_seconds: u64,
}

#[derive(Deserialize)]
struct Cohort {
    contract_version: String,
    source: Value,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    id: String,
    group: String,
    entry: ReferenceEntry,
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn append(writer: &mut impl Write, value: &Value) -> Result<()> {
    serde_json::to_writer(&mut *writer, value)?;
    writeln!(writer)?;
    writer.flush()?;
    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();
    ensure!(!cfg!(debug_assertions), "live evidence must use --release");
    let build_sha = option_env!("REGISTRY_VALIDATION_CODE_SHA").unwrap_or("unrecorded");
    ensure!(
        build_sha == args.code_sha,
        "compile-time and supplied source SHAs differ"
    );
    ensure!(
        args.code_sha.len() == 40 && args.code_sha.bytes().all(|c| c.is_ascii_hexdigit()),
        "invalid source SHA"
    );
    let bytes = std::fs::read(&args.cohort).context("read cohort")?;
    let cohort: Cohort = serde_json::from_slice(&bytes).context("parse cohort")?;
    ensure!(
        cohort.contract_version == "1",
        "unsupported cohort contract"
    );
    ensure!(
        cohort.cases.len() >= 200,
        "expected at least 200 labeled cases"
    );
    let mut seen = std::collections::HashSet::new();
    for case in &cohort.cases {
        ensure!(seen.insert(&case.id), "duplicate case identity");
        ensure!(
            case.entry.attempts.is_empty() && case.entry.resolved.is_none(),
            "input already resolved"
        );
    }
    let mut writer = BufWriter::new(File::create(&args.out).context("create result journal")?);
    append(
        &mut writer,
        &json!({
            "type": "run", "contract_version": "1", "harness_version": "1",
            "code_sha": build_sha, "profile": "release",
            "cargo_lock_sha256": hex::encode(Sha256::digest(include_bytes!("../Cargo.lock"))),
            "rust_toolchain": include_str!("../rust-toolchain.toml"),
            "cohort_sha256": hex::encode(Sha256::digest(&bytes)),
            "source": cohort.source, "planned_cases": cohort.cases.len(),
            "started_unix_seconds": unix_seconds(),
            "backend_identity": "tpe::resolve::Resolver (Crossref + Europe PMC)",
            "contact_configured": args.mailto.is_some(),
            "time_budget_seconds": args.time_budget_seconds,
            "http_retry_detail": "production Attempt records; internal retry HTTP transactions are not exposed",
        }),
    )?;
    let resolver = Resolver::new(args.mailto.as_deref());
    let start = Instant::now();
    let mut completed = 0;
    let mut consecutive_errors = 0;
    for case in cohort.cases {
        if start.elapsed() >= Duration::from_secs(args.time_budget_seconds)
            || consecutive_errors >= 10
        {
            append(
                &mut writer,
                &json!({"type":"stopped", "completed_cases":completed,
                "reason": if consecutive_errors >= 10 {"ten consecutive unresolved request errors"} else {"time budget"}}),
            )?;
            bail!("cohort incomplete after {completed} cases; journal retained");
        }
        let case_start = Instant::now();
        let mut entries = [case.entry];
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            resolver.resolve_entries(&mut entries)
        }));
        let (outcome, error) = match result {
            Ok(outcome) => (Some(outcome), None),
            Err(_) => (
                None,
                Some("resolver panicked; entry and previous attempts retained"),
            ),
        };
        consecutive_errors = if outcome
            .as_ref()
            .is_some_and(|o| o.errors > 0 && o.resolved == 0)
        {
            consecutive_errors + 1
        } else {
            0
        };
        append(
            &mut writer,
            &json!({
                "type": "case", "case_id": case.id, "group": case.group,
                "elapsed_ms": case_start.elapsed().as_millis(), "finished_unix_seconds": unix_seconds(),
                "entry": entries[0], "outcome": outcome, "harness_error": error,
            }),
        )?;
        completed += 1;
        eprintln!("case {completed}: {} ({})", case.id, case.group);
    }
    append(
        &mut writer,
        &json!({"type":"completed", "completed_cases":completed,
        "elapsed_ms":start.elapsed().as_millis(), "finished_unix_seconds":unix_seconds()}),
    )?;
    Ok(())
}
