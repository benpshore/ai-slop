# Claude Code / Fable implementation brief

Read the live repository, `AGENTS.md`, README, and `docs/UPSTREAMS.md` before editing. This is an implementation brief, not evidence of existing features. Inspect current code, open PRs, and current upstreams; the project changes quickly.

## Mission and hard targets

Build a Rust PDF text-mining engine for `aarch64-apple-darwin` first and `aarch64-unknown-linux-gnu` second, using native Poppler/PDFium and official Docling Rust. Investigate MLX for measured model bottlenecks. Later expose the same engine through a compact, accessible Zed-inspired Rust workbench.

Target **30 ms per 20-page chunk, at least 99% entirely error-free chunks, and 20 million completed documents per day per M1**. Inputs span **5–10 pages through 15,000-page complex documents**. The README defines capacity arithmetic, workload reporting, and quality semantics. Meet latency, throughput, and quality together; do not demonstrate each with a different undisclosed workload. A target miss requires evidence and the next focused experiment, not weaker definitions.

## First assignment: runnable baseline comparison

Deliver this assignment through small prerequisite PRs if needed: schema/fixtures, native adapters, Docling/model integration, and native CI/measurement. Finish with one comparison report covering the combined accepted state. Each PR must have a working, testable increment; do not bundle all dependencies and experiments into one oversized change.

1. Inspect the template's CI, release, and version rules; record the base SHA and existing PRs. Confirm upstream identities and select buildable pinned candidates on both architectures. Keep unrelated changes intact.
2. Add a small reusable Rust job/result schema and CLI comparison harness. One package with modules is sufficient. Use the intended worker boundary so backend crashes are contained. No distributed queues, HTTP service, or daemon.
3. Build a legally redistributable starter corpus with source/license/hash records. Include real openly licensed academic layouts and synthetic edge cases: digital/scan/mixed, columns, tables, Unicode/math, rotation/crop boxes, corruption, encryption with known passwords, short documents, and long/complex documents. Repeated blank pages alone do not represent a 15,000-page complex input.
4. Define independent reference text/order/structure and an exact-match protocol before tuning. Separate development and held-out sets. Freeze the corpus mix and statistical acceptance rule; preserve a private local fixture route for later authorized data. Never invent a corpus or results.
5. Run three modes against identical snapshots: Poppler, PDFium, and Docling Rust. Compare equal-output tracks for raw text, required structure/tables, and OCR; unsupported output is not a speed win. Each mode must execute or report a concrete unsupported capability. Inspect parser/model sharing before treating agreement as independent confirmation. Preserve backend evidence and costs.
6. Measure acquisition/hash, parse/render, inference/reconstruction, and durable writes; RSS, queueing, bytes read/written, and quality by class/length. Separate cold start, warm service time, total latency, and sustained throughput. Compare at increasing concurrency; a seven-worker setting alone proves nothing about the daily goal.
7. Add native macOS ARM64 and Linux ARM64 build/runtime jobs. Retain the required `ci` aggregator and ensure it fails on relevant failed/cancelled/unexpectedly skipped jobs. Cross-compilation is not native runtime verification.
8. Scope one Apple acceleration experiment early: identify a costly layout/OCR/table model, compare CPU reference with existing CoreML and/or a feasible MLX implementation, and verify model/preprocessing/numerics/device/memory/timing. Missing physical M1 or a viable model is a precise blocker, not permission to claim GPU performance.

The completed first assignment delivers real execution, repeatable commands, references, results, and explicit target gaps. Do not call the engine releasable before both native gates pass. No GUI in this assignment, and no production default selected before comparing quality and total cost.

## Delegate bounded work

Use parallel agents if available, with one integration owner and non-overlapping file ownership. Agree the schema first. If delegation is unavailable, execute these scopes sequentially.

| Owner | Scope | Evidence required |
| --- | --- | --- |
| Integration lead | Schema, CLI, scheduling/routing, merge order, final review. | End-to-end execution and reconciled capability differences. |
| Native backends | Poppler/PDFium, persistent document sessions, build recipes, passwords/process ownership. | Same-input outputs, Unicode/geometry, crash/timeout tests, exact native identity and licenses. |
| Docling/acceleration | Official Rust adapter, models/provider selection, one MLX/CoreML spike. | CPU/candidate diffs, actual providers, models, stage costs, and unsupported cases. |
| Corpus/adversarial review | Licensed fixtures, reference protocol, holdout evaluation. | Missing/duplicate text, numeric/symbol/order errors, partial failures, and large-document behavior. |
| CI/packaging | Native matrix, artifact locks/cache identity, install tests, later update lane. | Runs tied to PR SHA and clean installs without developer library paths. |

Each agent reports files, decisions, commands/results, and blockers. The lead owns shared manifests/workflows and reviews actual artifacts/logs. Do not concurrently edit shared files without coordination or treat a claim of passing tests as proof.

## Subsequent focused PRs

**Useful extraction:** bounded queues/workers, coherent immutable input acquisition, credentials, evidence exports, and SQLite state. Test aliases/hard links, concurrent source changes, unavailable placeholders, duplicate jobs, worker death, disk-full conditions, and interruption between output publication and ledger commit. Execution is at-least-once with idempotent publication.

**Large-document scheduling:** persistent owner sessions, streaming chunk output, bounded raster/page caches, fair short/long queues, and checkpoint/recovery. Measure unavoidable full-document index memory and reopen cost. Verify continued tables, reading order, page coordinates, and deduplication across chunk boundaries. A 15,000-page file has 750 chunks but should not incur 750 whole-document parses. Disclose adapters that cannot offer this behavior before promising the target.

**Structured recovery:** integrate Docling layout/OCR/tables and assess page/region routing on holdout inputs, including apparently good native output incorrectly accepted. Preserve original evidence and alternatives. Evaluate existing `pages`/`set_pages`, `convert_streaming`, and `process_pages` APIs; test original page numbering, coordinates, outlines/headings, and confidence metadata. Measure one streaming traversal against repeated windows. If the required contract cannot be met without duplicate work, report that cost before patching upstream.

**Sustained engine:** implement accepted/candidate dependency lanes, artifact provenance, finite retries, rollback, and graduated soak tests. The 24-hour test reports actual completions, output verification, failures, memory drift, storage traffic, and latency/accuracy under load. Include repeated runs and workload composition. Do not extrapolate millions/day from a small warm cache of identical PDFs.

**Rust releases:** resolve inherited Python-wheel auto-release and Rust manifest/tag mismatch in a separate PR. Follow the current no-manual-version-edit rule; choose a reproducible tag-derived Rust version scheme. Keep one authoritative release path; publish ARM binaries with libraries/notices/hashes and validate clean installs.

**GUI prototype:** use real evidence and job controls in a small GPUI app. Prove PDF/text correspondence, VoiceOver, scalable text, contrast, keyboard access, and pause/resume before expanding. Borrow Zed's interaction ideas without rebuilding its full editor. Report any necessary accessibility capability that the prototype cannot satisfy.

## Acceptance and reporting

- Every change is a PR. No direct/forced push to `main`, self-merge, history rewrite, or weakening required checks to make CI green.
- Use the repository workspace and public/synthetic fixtures. Personal machine installation, runner setup, or private corpus processing needs its own authorization.
- Use a GitHub noreply commit identity. No personal contact data, credentials, databases, or tool attribution footers in commits; retain required upstream notices.
- Preserve sources/evidence. No fabricated benchmarks, automatic golden refreshes, unlabelled model corrections, or deletion of hard fixtures to improve scores.
- Provision pinned models explicitly; production runs cannot fetch moving dependencies. Record CPU fallback; strict requested GPU mode must fail visibly if unavailable.
- Follow repository checks. State missing toolchains/runners/data plainly and use appropriate CI; unrun checks are not passes.
- PRs explain the problem, resulting behavior, commands/CI links, measured accuracy/performance/resource changes, and unresolved limitations. README status changes only for demonstrated capabilities.

End the first assignment with the PR link and a compact decision: which path wins for each tested document class/length, the measured bottleneck, what remains inaccurate, distance from the three targets, and the next smallest implementation PR. A plan or empty interface is not completion.
