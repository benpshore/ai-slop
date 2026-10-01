# Text Processing Engine

A native Rust engine for high-throughput, faithful text mining of academic PDFs, primarily on Apple Silicon macOS and also on Linux aarch64. Native Poppler, PDFium, and **Docling Rust (`docling-project/docling.rs`)** are the foundations. MLX is the intended Apple Silicon acceleration route wherever a measured model implementation improves complete-pipeline performance without reducing accuracy.

The eventual application is a compact, accessible, Zed-inspired Rust document workbench: corpus browser, PDF viewer, selectable extracted text, source highlighting, and job controls. The headless engine comes first and remains independently usable.

**Status: early engine, measured accuracy, nothing production-ready.** A pure-Rust extraction engine (`tpe`) and an evaluation harness exist. As of 2026-09-29 (Eval runs 36511221210 dev / 36511549138 holdout, backend `lopdf`, `ubuntu-24.04-arm`, after PRs #39, #40 and #41), reference recall/precision are 100%/100% on the 60-paper `dev` split and 100%/100% on the 10-paper `holdout` split; see "What exists today" below for the full table. Body-text alignment (0.950 dev / 0.940 holdout, body only; exact word LCS; math, digit and operator tokens dropped on both sides; appendices included) remains far from the error-free-chunk target below, and no chunk-level exact-match rate has been measured yet: these are reference/metadata/marker diagnostics, not the acceptance measurement. The eval timing (whole-document eval time on hosted arm64 runners, averaged over nominal 20-page chunks, without durable ledger writes) is a diagnostic and is not comparable with the M1 service-time target, though this diagnostic p50 is below 30 ms on these hosted arm64 runners (27.5 ms dev / 25.5 ms holdout); the M1 service-time target is not yet measured. The PDFium and docling backends build and pass their unit tests in the Native workflow; a pre-loop-10 three-backend comparison (Native run 36491886979) put the full `docling` pipeline at a routed exception of about 4.9 s per chunk, and `pdfium` at about 6x slower than `lopdf` on the reference-metrics path — see "What exists today" below for the full breakdown. The workbench crates are libraries with offline tests. A minimal macOS app (`crates/tpe-app`, GPUI over the engine in one process: text or bibliography per PDF, per-page progress, Finder Open With and Services) builds and passes its tests in the App workflow but has not yet been exercised by hand on a Mac ([App](docs/APP.md)); the Chromium embedding is design-only, and upstream synchronization and MLX acceleration are not implemented. The plan below is unchanged. Implementation notes are in the [Claude Code / Fable handoff](docs/CLAUDE_HANDOFF.md), the per-track status in [Tracks](docs/TRACKS.md), and technical sources and update policy in [Upstreams](docs/UPSTREAMS.md).

## Install

Every merge to `main` publishes a GitHub release with prebuilt `tpe` binaries
for Apple Silicon macOS (`aarch64-apple-darwin`), Linux arm64 and Linux
x86_64, plus `SHA256SUMS` and build-provenance attestations. Nothing needs to
be compiled on the installing machine.

With [mise](https://mise.jdx.dev) (installs from the release assets, verifies
the attestation, and `mise upgrade` keeps it current; the install check in
`.github/workflows/install-check.yml` runs this exact path on macOS and Linux
arm64 after every release):

```sh
mise use -g github:benpshore/pdftextract
tpe --version
```

mise deliberately waits before offering a brand-new release: its
`minimum_release_age` guard (one day by default) hides releases younger than
that, and its shared version cache (`mise-versions.jdx.dev`) can lag by some
hours. That is a reasonable default for a tool that releases on every merge.
To install a release the moment it is published, either name the version or
lift both for that one command:

```sh
mise use -g github:benpshore/pdftextract@0.42.0
MISE_MINIMUM_RELEASE_AGE=0 MISE_USE_VERSIONS_HOST=0 mise upgrade github:benpshore/pdftextract
```

Without mise, download the archive for your platform from the
[latest release](https://github.com/benpshore/pdftextract/releases/latest),
check it against `SHA256SUMS`, and put `tpe` on your `PATH`:

```sh
curl -fsSLO https://github.com/benpshore/pdftextract/releases/latest/download/tpe-aarch64-apple-darwin.tar.gz
curl -fsSLO https://github.com/benpshore/pdftextract/releases/latest/download/SHA256SUMS
shasum -a 256 -c --ignore-missing SHA256SUMS
tar -xzf tpe-aarch64-apple-darwin.tar.gz -C ~/.local/bin tpe
```

To verify that a downloaded archive was built by this repository's release
workflow:

```sh
gh attestation verify tpe-aarch64-apple-darwin.tar.gz --repo benpshore/pdftextract
```

A `tpe update` command that replaces the binary in place after checking the
checksum is in progress; when the binary is managed by mise it defers to
`mise upgrade`.

## What exists today

The engine is the root crate `tpe` ([Engine](docs/ENGINE.md)):

- `tpe extract` writes page text with span geometry, reading order, metadata, references, citation markers, chunks and figures to one SQLite ledger. Runs are keyed by input hash and backend identity.
- On-disk ledgers use a configurable WAL/storage policy with bounded checkpoints and preflight capacity checks. WAL limits reduce write amplification; they do not replace backups or filesystem-health and free-space monitoring.
- `tpe bibliography` scans backward for the final reference list and emits one JSON record per PDF without populating the full-document ledger ([behavior and limits](docs/BIBLIOGRAPHY.md)).
- Backends: `lopdf` (pure Rust, the default and only backend in the default build), `pdfium` behind feature `pdfium`, and `docling-text` / `docling` behind feature `docling`. Provisioning of the native libraries and models is in [Native](docs/NATIVE.md).
- Figures: images never enter page text. Each page lists its figures, and `--figures-dir` writes their bytes.
- Scanned-page fixture (`tests/scanned_fixture.rs`): a synthetic image-only page. `lopdf` must find no text, `pdfium` must report one raster figure, and docling OCR must read the text back.
- Evaluation ([Eval](docs/EVAL.md)): `tpe eval` scores the engine against arXiv LaTeX sources. `corpus/manifest.json` pins 70 CC-BY 4.0 arXiv papers by version and SHA-256, 60 in `dev` and 10 in `holdout`.

Measured status, 2026-09-29 (GitHub issue #15, backend `lopdf`, GitHub Actions `ubuntu-24.04-arm`, after PRs #39, #40 and #41; per-loop taxonomies in [docs/analysis/](docs/analysis/)):

`dev` split (60 papers, tuned on) — Eval run 36511221210:

| metric | value |
| --- | --- |
| reference recall / precision | 100% / 100% |
| reference-count exact | 100% |
| reference title accuracy | 98.5% (66 title-less RSC entries excluded) |
| reference printed-DOI accuracy | 99.6% |
| reference year accuracy | 99.5% |
| paper title accuracy | 92.9% |
| paper authors recall / precision | 98.4% / 96.1%* |
| paper DOI accuracy | 100% (5 of 5 stated) |
| citation-marker target precision (definition below) / marker key recall | 100% / 99.6% |
| body-text alignment, body only / raw (exact word LCS, appendices included) | 0.950 / 0.709 |
| body word recall / precision | 97.8% / 91.8% |
| eval time per nominal 20-page chunk, p50 / p95 (hosted arm64, no ledger write; not the M1 target measurement) | 27.5 ms / 70.7 ms |

`body only` removes math, digit and operator tokens from both sides. `raw` compares all extracted page text with the detexed source body without those filters (`src/eval.rs::evaluate`). These definitions apply to both splits.

\* Paper authors recall/precision are carried over from [dev run 36470860921](https://github.com/benpshore/pdftextract/actions/runs/36470860921) (after PR #32).

`holdout` split (10 papers, reported only; parser rules are tuned on `dev`, though a failure taxonomy of this split was published once in `docs/analysis/eval-2026-09-28-holdout.md`, so it is not fully blind) — Eval run 36511549138:

| metric | value |
| --- | --- |
| reference recall / precision | 100% / 100% |
| reference-count exact | 100% |
| reference title accuracy | 97.3% |
| reference printed-DOI accuracy | 100% |
| reference year accuracy | 99.6% |
| paper title accuracy | 100% |
| paper authors recall / precision | 100% / 100%* |
| paper DOI accuracy | n/a (no source states a DOI) |
| citation-marker target precision (definition below) / marker key recall | 100% / 99.3% |
| body-text alignment, body only / raw (exact word LCS, appendices included) | 0.940 / 0.701 |
| body word recall / precision | 94.6% / 89.2% |
| eval time per nominal 20-page chunk, p50 / p95 | 25.5 ms / 39.6 ms |

\* Paper authors recall/precision are carried over from [holdout run 36472085645](https://github.com/benpshore/pdftextract/actions/runs/36472085645) (after PR #32).

These are diagnostics from two runs on shared CI hardware. They are not the acceptance measurement described below.

`marker_precision` divides resolved targets whose extracted entries match a truth key cited somewhere in the paper by all resolved targets, summed over non-failed papers with source citations (`src/eval.rs::marker_correctness` and `summarize`). It does not compare each marker's position with the corresponding source citation, so 100% here does not establish occurrence-level citation precision.

Known gaps: body-text alignment (0.950 dev / 0.940 holdout, body only; exact word LCS; math, digit and operator tokens dropped on both sides; appendices included) is still short of the 99% error-free-chunk goal below, and body word precision is 91.8% dev / 89.2% holdout. Since the last refresh, loops 11 (math/footnote token dropping, lopdf figure boxes, geometry-based figure/table tagging, column overhangs) and 13 (caption continuations, figure labels, longtable pages, biography/front-matter roles, digit/listing handling) worked on body text, and loop 12 (marker residue, parser tail cases, hyphen pairs, INFORMS truth, NFKC title comparison) worked on references and markers; PR #42 has since merged; these measurements precede its figure-box change for paper 2509.04183 (0.711 body alignment in the reported run). Marker key recall is 99.6% dev / 99.3% holdout and reference title accuracy is 98.5% dev, leaving the remaining 1.5% of reference titles and 0.4% of cited keys on dev open. Perf loop (PR #35) took arm p50 from 35.4 ms to 18.7 ms, before loop 10's region tagging added about 4 ms back; the added tagging (loops 11 and 13) brought p50 to 27.5 ms dev / 25.5 ms holdout — still below 30 ms on these hosted runners but creeping toward it, so a second perf loop is due. The M1 service-time target, with durable writes, has not been measured natively. A pre-loop-10 comparison (Native run 36491886979) put the full `docling` pipeline at a routed exception of about 4.9 s per chunk, and `pdfium` at about 6x slower than `lopdf` on the reference-metrics path. See GitHub issue #15 for the running history and [docs/analysis/](docs/analysis/) for the per-loop taxonomies.

Native workflow: with pinned PDFium and model assets, the `docling` and `pdfium` features build and their unit tests pass on `ubuntu-24.04-arm`. A pre-loop-10 three-backend comparison on the reference-metrics path (Native run 36491886979) found: `lopdf` (the default) at 99.6%/99.7% reference recall/precision, body alignment 0.738, p50 18.7 ms; `pdfium` at 97.3%/99.6%, body alignment 0.714, p50 109 ms (about 6x slower than `lopdf`); `docling-text` at 78.1%/94.7%; and the full `docling` layout+OCR pipeline at 96.9%/98.8%, body alignment 0.785, p50 4892 ms (about 4.9 s per chunk) — `lopdf` is the fast path and `docling` remains a routed exception. The docling OCR fixture result is not yet known.

Workbench crates under `crates/` (each is a library with offline tests; see [Tracks](docs/TRACKS.md)):

| crate | what it is |
| --- | --- |
| `tpe-common` | shared paper record types |
| `tpe-credentials` | secret storage: macOS keychain, encrypted file, memory |
| `tpe-biblio` | OpenAlex, Crossref, Semantic Scholar, PMC, Europe PMC, Unpaywall, OpenURL clients; dedupe. Google Scholar has no API, so only a search URL builder exists |
| `tpe-zotero` | Zotero Web API client, local database reader, plugin import body ([Zotero](docs/ZOTERO.md)) |
| `tpe-search` | lexical (FTS5) and semantic search over ledger text |
| `tpe-speech` | text-to-speech with a speech-recognition round trip |
| `tpe-app` | PDFTextract, the GPUI macOS app: two buttons over the engine, progress bars, Finder integration ([App](docs/APP.md)); plus the workbench library modules |
| `tpe-browser` | research-browser model (DOI/PDF detection, host policy, cookies). CEF embedding is design-only ([Browser](docs/BROWSER.md)) |

## Performance and fidelity targets

The product goals are **30 ms per 20-page chunk on an M1 Mac**, **at least 99% completely error-free chunks**, and **20 million document extractions per day per M1**. These are simultaneous engineering targets to validate, not achieved capabilities or promises about arbitrary PDFs. The 99% target means exact, checked chunk-level output, not 99% correct characters or words.

| Target | Capacity implication |
| --- | --- |
| 30 ms per 20-page chunk | About 667 pages/second per continuously busy processing lane. |
| 20 million documents per 24 hours | About 231.5 completed documents/second sustained on each M1. |
| If every document is 20 pages | 400 million pages/day, about 4,630 pages/second. |
| 30 ms chunks at that assumed document length | One lane produces at most 2.88 million documents/day; at least seven lanes' worth of effective throughput is needed before overhead. |

A lane is a capacity calculation, not a promise that seven threads or processes scale linearly. Seven ideal lanes yield only 20.16 million 20-page documents/day, leaving less than 1% spare capacity. The benchmarks must demonstrate that throughput and latency hold together under contention on one machine. Account for document-length distribution, partial chunks, shared PDF objects, snapshots, hashing, I/O, rendering, inference, reconstruction, and durable publication. Report document/s and page/s separately; do not count chunks as documents. A document is complete only when all its required chunks and outputs are committed.

The supported workload ranges from **5–10-page PDFs to 15,000-page complex documents**. A 15,000-page document contains 750 nominal 20-page chunks. For an observed corpus, report both mean pages/document and mean `ceil(pages/20)` chunks/document; multiplying either by 231.5 documents/second gives the corresponding required sustained rate. Do not assume all documents contain 20 pages or pad short files into fictitious completed pages. Document complexity and bytes/page are independent workload dimensions.

Before benchmarking, freeze the latency boundary and acceptance percentile in the benchmark specification; neither was specified in the product goal. Measure warm 20-page service time and end-to-end latency including queueing separately, with p50/p95/p99. Service time includes all required parse/render/model/reconstruction work and durable chunk output; separately report and charge document acquisition/open/hash costs to total document time and sustained throughput. Cold starts, model loading, and dependency provisioning remain visible. Do not present parser-only timings as the 30 ms target. The daily target requires a representative 24-hour run; do not establish it by multiplying a microbenchmark.

Freeze the acceptance workload and required output fields before tuning: page-count/byte-size distributions, complexity, digital/scan/mixed proportions, and whether order/tables/coordinates are required. All three targets apply to that same contract. Publish corpus size, hardware model, RAM, storage, OS, thread counts, provider, and precision. Native-text, layout/table, and OCR workloads each need results. No target may be marked achieved by excluding difficult inputs without explicitly narrowing the supported workload. Arithmetic above assumes 20 pages/document only for illustration; actual capacity is measured on the declared workload.

Count newly executed document extractions separately from duplicate discoveries, aliases, cache hits, and resumed/already committed outputs. The 20-million target counts actual extraction work, not cheap rediscovery of previously processed content. Disclose restart work and repeated benchmark inputs; do not let idempotence inflate capacity.

## What survives from earlier attempts

Preserve immutable originals, restartability, bounded memory, idempotent processing, SQLite bookkeeping, and provenance. The first product is PDF text extraction and structure recovery; bibliography matching, web harvesting, citation graphs, and other formats can consume its outputs later.

`makeghrepo` supplies the repeatable repository and CI starting point. This project applies that workflow to a larger engine: track fast-moving upstreams while retaining a working, reproducible application. Avoid another unbounded integration effort by requiring runnable milestones and measured results.

The application and orchestration are Rust. Native C/C++ libraries are legitimate dependencies; rewriting the upstream engines or editor is not the initial task. Python/uv may support reference comparisons and fixtures, but must not become a required production coordinator. Begin with one Rust package and ordinary modules; split crates only at a demonstrated boundary.

## Evidence and accuracy contract

PDFs can contain missing character maps, scanned text, damaged objects, and ambiguous reading order. Preserve these limitations instead of silently emitting plausible text.

- Retain backend text and geometry as evidence. Store normalized text, inferred order, OCR, and model-derived structure as distinct, attributable results.
- Never silently repair spelling, numbers, symbols, negation, citations, or tables with a language model. Generative recovery, if later added, is a separately labelled candidate.
- Keep disagreements and uncertainty. Agreement between engines is not independent proof, especially when they share a parser or model. A heuristic score is not a calibrated correctness probability.
- Test Greek letters, ligatures, superscripts/subscripts, non-BMP characters, units, minus signs, tables, and reading order. A successful exit or attractive Markdown is insufficient.
- Define the reference representation and allowed transformations before benchmarking. Report strict raw text separately; no normalization may conceal a symbol, number, omission, duplicate, or order error.

The acceptance numerator is the number of chunks with **zero errors** against independently checked text/order references and the requested structural fields; the denominator is all eligible test chunks, including failures and timeouts. Report exact-match rate by document class and length, whole-document exact-match rate, character/word errors as diagnostics, and uncertainty on the estimate. A 99% chunk rate does not imply 99% entirely correct 15,000-page documents. Account for correlated errors within documents when estimating uncertainty. Use a held-out corpus and a predeclared statistical acceptance rule; a small observed 99% rate alone does not establish production reliability. No automated golden-file refresh can substitute for independent checking.

## Architecture

```mermaid
flowchart TD
    A["Completed input and immutable snapshot"] --> B["Rust coordinator and job ledger"]
    B --> C["Native extraction workers"]
    C --> D{"Page evidence and requested output"}
    D -->|"Sufficient text evidence"| E["Evidence record and exports"]
    D -->|"Layout, scan, or unresolved text"| F["Docling Rust and OCR workers"]
    F --> E
    E --> G["CLI and future document workbench"]
```

| Component | Planned role | Constraint |
| --- | --- | --- |
| Poppler | Native text/layout candidate and independent comparator. Benchmark `pdftotext` with coordinates; add a narrow C++ adapter if startup or missing metadata is material. | Layout is not universal ground truth; review GPL obligations before linking or bundling. |
| PDFium | Text/glyph geometry, rendering, and a second extraction candidate through a pinned Rust binding. | `pdfium-render` serializes native calls within a process; concurrent documents need bounded worker processes. |
| Docling Rust | Primary structured-document adapter for layout, reading order, OCR, tables, and difficult PDFs; included in the initial comparison. | Its current default text parser is Rust/lopdf; PDFium remains a renderer and text fallback. Test models, runtimes, and PDF conformance. |
| MLX | Optional macOS model backend for demonstrated expensive layout/OCR/table stages. | Requires a concrete model implementation and parity tests; not an automatic replacement for ONNX Runtime or acceleration of PDF parsing. |

These are roles, not a predetermined speed ranking. Compare all three extraction modes on identical inputs before selecting a default, with separate equal-output tracks for raw text, required structure/tables, and OCR. A backend that cannot produce the requested structure is unsupported on that track, not a faster equivalent. Keep an explicit full-Docling mode: a cheap text pass cannot establish that tables or columns were understood. Sample apparently successful native output for deeper comparison to measure missed failures.

Route by page/region where supported by the pinned adapter. Keep the complete immutable PDF available because pages share fonts and objects; a 20-page scheduling chunk is not permission to split bytes or discard document context. If an adapter only processes whole documents, report that cost rather than claiming selective execution. Do not run all engines or rasterize all pages in the production fast path without evidence that this meets the goals.

Retain a document session in its owning worker so chunking does not reopen and reconstruct a 15,000-page document 750 times. Stream page/chunk outputs, bound decoded-page/raster caches, and checkpoint committed chunks. Measure unavoidable whole-document parser/index memory; do not claim constant memory merely because output is streamed. Use fair queues and admission limits so huge files neither starve short PDFs nor consume all workers. Yield between chunks where the backend allows it; retain bounded resident sessions or reload explicitly with measured cost. Preserve cross-chunk reading order, continued tables, and page identity; bounded context overlap must not duplicate exported content. On recovery, rebuild necessary parser state and reuse committed chunk outputs rather than silently declaring an incomplete document finished.

### Safe input, concurrency, and recovery

1. Accept completed bytes and obtain a coherent read-only snapshot before hashing/parsing. An open file descriptor, `mmap`, or metadata check alone does not make a changing file immutable. Use clone/reflink optimizations where valid, with a portable copy fallback and a defined acquisition protocol for actively written files.
2. Track path/inode aliases as observations; content hash is the durable document identity. Defer cloud placeholders and incomplete downloads explicitly. Never modify originals.
3. Use bounded, long-lived worker processes. Keep native handles inside their owner. Load model sessions once where supported; coordinate process count with each library's own thread pool. Do not multiply Docling/ONNX concurrency blindly.
4. Bound queue bytes, pixels, RAM, temporary disk, and execution time. Apply backpressure. Record crashes/timeouts, retry finitely, then quarantine visibly. Process isolation is a crash boundary, not a complete security sandbox.
5. Handle password-protected PDFs using supplied credentials through backend APIs. Distinguish missing/wrong password, unsupported encryption, corruption, and extraction failure. Keep passwords out of argv, logs, and durable manifests.

Start with versioned local IPC, not a service. A job carries snapshot/hash, page selection, requested output, limits, configuration digest, and a transient credential channel. A result carries status, output references, warnings, stage timings, and execution identity.

### Durable records

Use a project-owned versioned schema that retains native evidence and original DoclingDocument JSON; avoid coupling the public contract to one upstream's changing types. Record source hash; page/span/glyph references where available; text, boxes, page dimensions, rotation and coordinate frame; backend/model/runtime hashes and settings; normalization mappings; reading-order edges; table structure; warnings; and complete/partial/failed/deferred status. Declare unavailable geometry rather than inventing it. Test crop/rotation transforms used by the eventual viewer.

TXT/Markdown are exports; structured evidence is authoritative. SQLite holds a small job ledger and manifests with a single writer and batched transactions. Keep large artifacts outside it initially. Key jobs by input hash **and processing identity** so upgrades produce new attributable output. Atomically publish outputs before marking completion; reconcile orphan outputs after interruption. Execution may repeat, but publication is idempotent. Avoid retaining every raster or logging per glyph; retain failure evidence and reproducible render settings.

## Native builds, MLX, and upstream updates

| Target | Validation |
| --- | --- |
| `aarch64-apple-darwin` | Primary native build; physical M1 performance tests, minimum macOS and library-loading checks, actual model/device probes. |
| `aarch64-unknown-linux-gnu` | Native ARM64 build/runtime checks and CPU baseline; record glibc/library requirements and target-machine resources. |

GitHub currently provides M1 ARM64 macOS and native Linux ARM64 runner labels. Use explicit OS labels and record image versions. Probe Metal availability; an M1-labelled hosted VM is not proof of physical M1 GPU performance. Hosted CI uses public/synthetic fixtures; bulk corpus processing stays on explicitly selected runtime machines.

Profile acquisition/hash, parse, render, preprocessing, inference, reconstruction, and writes separately. Investigate one MLX model early alongside a CPU reference. Measure conversion feasibility, numerical parity, cold start, sustained batches, and shared-memory pressure. Docling Rust offers CoreML through ONNX, with documented correctness/startup caveats; CoreML and MLX are separate paths. Linux ARM64 retains a CPU baseline even though MLX upstream now supports some Linux configurations.

Track upstream changes in a candidate lane while releases use a pinned accepted set. Planned scheduled workflows discover revisions, build changed candidates, compare the corpus, and open update PRs. Pin native sources, bindings/API features, build recipes, weights, precision, and artifact hashes together. Cache by the complete build identity; app PRs reuse accepted native artifacts. No moving dependencies or model downloads during ordinary processing. See [Upstreams](docs/UPSTREAMS.md).

## Delivery gates

| Stage | Deliverable and completion condition |
| --- | --- |
| 0. Comparison | Licensed fixtures/reference protocol; real Poppler, PDFium, Docling Rust runs on both native targets; accuracy/stage-cost report; scoped M1 acceleration experiment. Expose feasibility gaps against both performance goals. |
| 1. Useful CLI | Batch extraction, evidence JSON/TXT, bounded workers, passwords, and per-file outcomes. Demonstrate resume, aliases/duplicates, malformed inputs, and unchanged originals. |
| 2. Structured recovery | Docling layout/OCR/tables and measured routing. Demonstrate coverage and error-free-chunk rate on holdout inputs; report extra cost and unresolved cases. |
| 3. Sustained engine | Native artifacts, candidate update PRs, reproducible rebuilds, rollback/install tests, and graduated soak tests leading to a 24-hour target test on each M1 configuration claimed. |
| 4. Workbench | Separate Rust GUI over proven engine output, with PDF/text correspondence, search, job control, and accessible interaction. GUI failure cannot invalidate extraction. |

Report completed documents/s, pages/s, bytes/s, latency percentiles, peak RSS, CPU/GPU settings, disk writes, retries, and accuracy by stratum. Do not trade correctness for a faster headline. A missed target calls for a measured bottleneck report and a focused next experiment, not a redefinition of success.

## Zed-inspired workbench

Evaluate **GPUI** for a small standalone Rust application. Reuse Zed's responsiveness and panes as interaction references, building only corpus browsing, PDF/text viewing, source highlighting, disagreement inspection, search, and pause/resume. Keep language servers, terminals, collaboration, and editor-agent features out of scope.

GPUI is Apache-2.0 and pre-1.0; Zed application code has different licensing. Pin the GUI dependency separately. Its current accessibility integration does not establish our app's usability: prove VoiceOver, large adjustable text, contrast, keyboard access, selectable text, and reduced motion in a small prototype before expanding it. Keep processing state in the engine, not in UI-only storage.

## Development and checks

Follow [AGENTS.md](AGENTS.md). Existing scaffold checks remain:

```sh
uv run ruff format && uv run ruff check && uv run pytest && uv audit --preview-features audit-command
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
swift build && swift test
cmake -S . -B build && cmake --build build && ctest --test-dir build
```

`main` is protected: open a PR; the `ci` check must pass before merging (squash only). Never force-push `main`.

Inherited merge-release automation currently publishes Python artifacts; it is not the intended ARM Rust distribution pipeline. Align packaging, tag-derived versions, and native CI in a focused implementation PR before declaring the engine releasable. The software is proprietary (see LICENSE); native-library and model licenses still apply to their components.
