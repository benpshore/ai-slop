# PDFium compatibility and maintenance audit

Audit date: **2026-10-04 UTC**. This is a source and dependency audit with a proposed qualification plan. It makes no dependency, parser, workflow, configuration, or deployment change and creates no scheduled monitoring.

## Scope and evidence boundaries

The inspected sources are main **`e27e1fb28a5b40dbca7f517c6fe396e6f11ed4ec`** at `/workspace/pdftextract`, and PR #175 **`baafb472873750e7e84d32c3c8e6fa7a680e37db`** at `/workspace/pdftextract-native-audit`. Links below fix the relevant source revision; line references are not floating branch references. Work belongs under [native epic #181](https://github.com/benpshore/pdftextract/issues/181) and [service integration epic #182](https://github.com/benpshore/pdftextract/issues/182), with existing [#152](https://github.com/benpshore/pdftextract/issues/152), [#153](https://github.com/benpshore/pdftextract/issues/153), and [unsafe inventory #145](https://github.com/benpshore/pdftextract/issues/145) retained rather than duplicated.

The audit read `AGENTS.md`, PR175's `HANDOFF_TO_DOT.md`, `NATIVE.md`, `NATIVE_EVIDENCE.md`, manifests, native provisioning and CI, PDFium/Docling adapters, and the historical unsafe inventory. The two PDFium adapter files, native artifact manifest, provisioner, native workflow, and Rust toolchain declaration have no differences between these two revisions. The Cargo feature graphs and lockfiles do differ.

Historical [inventory PR162 at `6c2c92d`](https://github.com/benpshore/pdftextract/blob/6c2c92d8d79a4fb5a5befec54fb2bed9da6cb2cb/docs/unsafe-rust-inventory-ffi.md) reports 35 unsafe syntax occurrences across 95 files at source **`68ee9b4e08fde2f28e1042e1d6abb89e3547770d`**. Its scan excludes dependencies and generated code. Its [build manifest](https://github.com/benpshore/pdftextract/blob/6c2c92d8d79a4fb5a5befec54fb2bed9da6cb2cb/docs/unsafe-rust-inventory-ffi/build-manifest.json) records a real older Linux x64/PDFium run. Those counts and successes are historical evidence, not a fresh whole-tree safety inventory or runtime pass on main/PR175. This audit reuses that boundary and examines the PDFium-specific ownership contracts separately.

## Separate the six compatibility identities

| Layer | Existing audited value | What it establishes |
| --- | --- | --- |
| Google's PDFium source | Distributor release says it builds branch `chromium/8066`. Google's branch resolved during this audit to `fc46361ce75055cd549cb938fae5d6a3fe3a1a05`. | A source identity; record the exact resolved commit and build recipe separately from the branch label. It is not the distributor's packaging commit. |
| Binary distributor | `bblanchon/pdfium-binaries`, release `chromium/8066` / PDFium `156.0.8066.0`; Linux x64, Linux ARM64, macOS ARM64 archives. | Third-party builds, not Google-provided binaries. Target, GN options, patches, compiler, dependencies and notices matter. |
| Rust wrapper | Cargo requirement `0.8`, locked **0.8.37**, defaults disabled for the direct dependency, `thread_safe` enabled. | `0.8` allows compatible 0.8 releases; it is not an exact requirement. `--locked` retains the resolved package checksum. |
| Compiled PDFium API | In the published **0.8.37** crate, `pdfium_latest` expands to **`pdfium_7543`**. | A symbol/signature selection, not a native runtime version or always-current API. |
| Deployed native runtime | Three archive and extracted-library SHA-256 pairs are populated in `native/manifest.json`. | Provisioning checks identity. The general runtime loader does not compare its configured library against this manifest. |
| Rust build environment | Toolchain declaration **1.98.1**, minimal profile, Clippy/rustfmt; edition 2024, resolver 3, no root/workspace `rust-version`. | These are separate from the actually executed rustc/Cargo/Clippy, selected target, feature graph and lockfile. |

Local anchors: [main Cargo.toml:36–57](https://github.com/benpshore/pdftextract/blob/e27e1fb28a5b40dbca7f517c6fe396e6f11ed4ec/Cargo.toml#L36-L57), [PR175 Cargo.toml:36–75](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/Cargo.toml#L36-L75), [PR175 lock:5752–5776](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/Cargo.lock#L5752-L5776), [native pins:4–13](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/native/manifest.json#L4-L13), [toolchain:1–4](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/rust-toolchain.toml#L1-L4).

Primary upstream checks: [Google source commit](https://pdfium.googlesource.com/pdfium/+/fc46361ce75055cd549cb938fae5d6a3fe3a1a05), [distributor 8066 release](https://github.com/bblanchon/pdfium-binaries/releases/tag/chromium/8066), [distributor README](https://github.com/bblanchon/pdfium-binaries#readme), and [published wrapper 0.8.37 archive](https://static.crates.io/crates/pdfium-render/pdfium-render-0.8.37.crate). The archive was fetched into memory and its SHA-256 matched the repository lock: `6553f6604a52b3203db7b4e9d51eb4dd193cf455af9e56d40cab6575b547b679`. Its `Cargo.toml` and `src/bindings/thread_safe.rs` were read directly; no dependency was installed or changed by this audit.

On this date, the distributor's `latest` redirected to [8076 / 156.0.8076.0](https://github.com/bblanchon/pdfium-binaries/releases/tag/chromium/8076), released September 29. That release includes text-direction/ActualText and font-related changes worth targeted regression tests. The published wrapper's newest version was **0.9.4**, and its API alias selects **7881**. These are update candidates only; none has been qualified here as a replacement for the existing tuple. [Wrapper manifest](https://raw.githubusercontent.com/ajrcarey/pdfium-render/master/Cargo.toml), [published 0.9.4 archive](https://static.crates.io/crates/pdfium-render/pdfium-render-0.9.4.crate).

Google describes its public headers as the supported embedder interface and aims for stability; this is not a blanket promise that every newer binary satisfies every older wrapper, feature combination or semantic assumption. Check exported symbols, signatures, conditional APIs and actual behavior. The comment that 8066 is sufficiently recent in [pdfium_backend.rs:68–79](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/src/backend/pdfium_backend.rs#L68-L79) cannot substitute for this qualification. [Google embedding/build guidance](https://pdfium.googlesource.com/pdfium/+/refs/heads/main/README.md).

## Loading, ownership and upgrade hazards

| Contract and exact source | Finding and required regression |
| --- | --- |
| [Loader:331–379](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/src/backend/pdfium_backend.rs#L331-L379) | Direct PDFium requires an explicit absolute path; no CWD/system fallback. It checks a regular file and the 64 MiB native-library ceiling before binding. Missing, wrong-architecture and missing-symbol libraries must fail explicitly. The ceiling concerns the library file, not an input PDF limit. |
| [Identity and hashing:155–211](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/src/backend/pdfium_backend.rs#L155-L211) | Identity uses wrapper version, effective path and configured-file hash. It does not authenticate that file against the artifact manifest, snapshot the loaded bytes, or cover dependencies/fonts. Deployment must keep the file immutable during a job; check identity stability and reject missing qualification evidence. |
| [Provisioning:144–204](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/native/fetch.sh#L144-L204) | Archive hash is checked before extracting the selected member, then the library hash is checked. Cached files are rehashed. All three PDFium entries are populated now; `NATIVE.md`'s null-hash bootstrap instructions are not the current pin state. |
| [Per-open lifetime:214–247](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/src/backend/pdfium_backend.rs#L214-L247), [borrowed load:383–391](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/src/backend/pdfium_backend.rs#L383-L391) | `Pdfium`, borrowed document and supplemental mapping document remain local; reverse drop order closes documents before the binding, while input bytes remain alive. Returned sessions contain extracted Rust data. Concurrent opens are serialized by the pinned wrapper's instance-lifetime mutex. Preserve this contract through errors/unwinding and sequential PDFium/Docling use. |
| [Raw handles:12–78](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/src/backend/pdfium_backend/unicode_mapping.rs#L12-L78), [Drop:100–124](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/src/backend/pdfium_backend/unicode_mapping.rs#L100-L124) | A second document borrows bindings and bytes. A mapping page borrows that document; null handles are checked; text page closes before page; document closes last. These are FFI ownership obligations even though 0.8.37 exposes the calls through safe methods and these local files contain no `unsafe` token. A syntax inventory cannot certify them. |
| [Page-count guard:82–97](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/src/backend/pdfium_backend/unicode_mapping.rs#L82-L97) | Raw signed native count is checked before the wrapper's `u16` narrowing: empty/invalid fails, above 65,535 returns a limit error. A wrapper upgrade must preserve explicit handling; this is a binding limitation, not a proposed arbitrary product page cap. |
| [Eager-memory contract:35–54](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/src/backend/pdfium_backend.rs#L35-L54) | Page ranges still cause whole-document native extraction. Image deduplication/capping does not bound total RSS, text, metadata or native caches. Test real long/repeated-image/large-image cases under CLI containment and record measured peak RSS. |

**A wrapper-only 0.9 upgrade is not a routine maintenance change.** In the published 0.9.4 archive (SHA-256 `8948a803616a9e936b15a6637af2cd48c5fb8ae0fcdeb3c32eac3a540e255a19`), `src/pdfium.rs:66–75,210–219` stores bindings in a process-global `OnceCell`, and `src/bindings/thread_safe.rs:123–141` locks individual calls. Raw FPDF calls are unsafe. These differ materially from 0.8.37's binding-owned `MutexGuard` at `thread_safe.rs:89–99,120–145` and `Pdfium` destruction at `pdfium.rs:427–432`. A migration must review first-library selection, loaded-library lifetime, identity reporting, raw-handle callers and cross-backend sequencing. Adding local unsafe blocks merely to compile would not establish ownership correctness. [Published 0.9.4 source archive](https://static.crates.io/crates/pdfium-render/pdfium-render-0.9.4.crate).

Upstream 0.9.4 notes memory-safety fixes in font/page-object handling and thread-safe operation. Treat these as a priority to assess reachability and supported remediation against the pinned version; this audit has not demonstrated a corresponding exploitable defect in these adapters. [Upstream release notes](https://github.com/ajrcarey/pdfium-render#whats-new).

Docling 1.69.2 still requests wrapper `0.8`. Updating only TPE's direct dependency to `0.9` would permit **two wrapper packages**, with separate initialization/global-state locks around native PDFium. The existing single-wrapper rationale is explicit in [Cargo.toml:69–71](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/Cargo.toml#L69-L71). A coordinated migration or demonstrably isolated runtime boundary must precede acceptance. For a future pinning proposal, consider an explicit supported `pdfium_NNNN` API feature rather than a moving alias, but inspect feature unification first: Docling's wrapper dependency enables its defaults, so changing only TPE's feature list does not necessarily remove `pdfium_latest`. PR175's pure LiteParse adapter also has transitive PDFium build dependencies: [config paths](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/.cargo/config.toml#L1-L8) and [verified matching headers](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/native/fetch-liteparse.sh#L19-L59) matter to builds. A lockfile package listing alone does not prove an additional wrapper is invoked at runtime.

## Rust, Cargo, Clippy and target evidence

`Cargo.toml` specifies edition/resolver/dependency requirements; it does **not** pin or install the Cargo executable. `rust-version`, if later introduced, states a supported Rust version and informs diagnostics/resolution; it does not repair rustup selection. `Cargo.lock` fixes dependency resolution, not the compiler or external native libraries. rustup's command-line/environment/directory overrides can supersede the toolchain file. [Cargo rust-version documentation](https://doc.rust-lang.org/cargo/reference/rust-version.html), [Cargo manifest versus lock](https://doc.rust-lang.org/cargo/guide/cargo-toml-vs-cargo-lock.html), [rustup override precedence](https://rust-lang.github.io/rustup/overrides.html).

This audit initially found no Rust commands on the default shell PATH. The parent then provisioned the exact declared toolchain separately under `/workspace/toolchains`. Subsequent read-only version commands from PR175 directly verified:

| Executed component | Observed value |
| --- | --- |
| rustup active toolchain | `1.98.1-x86_64-unknown-linux-gnu`, selected by PR175's `rust-toolchain.toml` |
| rustc | `1.98.1`; full commit `48a229ceaefd4985c50990b14116b6d856af0985`; LLVM 22.1.8 |
| Cargo | `1.98.1`; full commit `797e8a9bca276c1c9f9f738d2a20f484fa4eea9d` |
| Clippy | `0.1.98 (48a229ceae 2026-09-01)` |
| Resolved rustc/Cargo path | `/workspace/toolchains/rustup/toolchains/1.98.1-x86_64-unknown-linux-gnu/bin/` |
| Installed target | `x86_64-unknown-linux-gnu` only |

Reproduce executable selection with `RUSTUP_HOME=/workspace/toolchains/rustup`, `CARGO_HOME=/workspace/toolchains/cargo` and the explicit proxies `/workspace/toolchains/cargo/bin/{rustup,rustc,cargo}`. Record `rustup show active-toolchain`, `rustup which rustc`, `rustup which cargo`, `rustc -Vv`, `cargo -Vv`, `cargo clippy -V`, installed targets and any override variables for each matrix run. Do not silently substitute `stable` after a missing pin or infer that another shell has the same PATH.

Root/workspace manifests omit `rust-version`. The published dependency manifests declare wrapper0.8.37 Rust1.61/edition2021, Docling PDF1.69.2 Rust1.88/edition2021, Docling core1.69.2 Rust1.85/edition2021, and ort/ort-sys rc13 Rust1.88/edition2024. These lower bounds do not establish the whole locked graph's MSRV. The exact pinned toolchain and each enabled target/feature graph must actually compile and run. Clippy alone cannot establish dynamic loading, ABI, extraction accuracy, panic/crash behavior or WASM compatibility.

Reproducibility hashes measured from the inspected files:

| File | main SHA-256 | PR175 SHA-256 |
| --- | --- | --- |
| `Cargo.lock` | `ae0367f2f9391ec98fd0a125bc516e0179419b311e4f1c7693ad3052b3b7f8c0` | `00ff969d6bc010d669a77e8c2341a81219e7d30dadbfee892ef5825a2aadd57c` |
| `native/manifest.json` | `194f06dd0a127a2faea3e2c815c9278e61d8cada1b7477f3bafddddee27fb156` | same |
| `rust-toolchain.toml` | `c910997cb152c6dc8ed13b1fdf9fa28a4ed30d6776123e27dd6170e5c66067a0` | same |

## Docling Rust/runtime/model compatibility

CPU OCR implementation and qualification now have dedicated tracking in
[#197](https://github.com/benpshore/pdftextract/issues/197) under
[OCR-PDF epic #199](https://github.com/benpshore/pdftextract/issues/199).
This audit supplies compatibility gates to that owner; it does not implement
or deploy the OCR lane or duplicate those tickets.

Main enables Docling's default `ml` feature with its single `docling` switch. PR175 separates `docling-text` with `default-features=false` from `docling`, which explicitly enables `ml` and PDFium. Therefore the old full-native feature graph is not evidence for the new pure-text graph, or vice versa. [Main manifest](https://github.com/benpshore/pdftextract/blob/e27e1fb28a5b40dbca7f517c6fe396e6f11ed4ec/Cargo.toml#L36-L57), [PR175 manifest](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/Cargo.toml#L43-L71).

Both locks retain `docling-core/pdf/onnx`1.69.2, wrapper0.8.37 and ort/ort-sys2.0.0-rc.13. Published archives for these pinned components were inspected; PDF/core/ort/ort-sys archive digests match the respective PR175 lock entries at [2234–2266](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/Cargo.lock#L2234-L2266) and [5581–5597](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/Cargo.lock#L5581-L5597).

The pinned `docling-pdf` and `docling-onnx` request ort with defaults disabled and `std`, `download-binaries`, `tls-rustls`. In published `ort-sys`rc13, `build/download/dist.tsv` includes hashed native **1.28.0** CPU archives for Linux x64/ARM64 and a CoreML-capable macOS ARM64 archive. The crate's resolver and build environment select the actual artifact and linking route; this audit did not execute that build, inspect a resulting ONNX library, or certify execution-provider selection. This is more precise than calling every ONNX detail unknown, but it remains short of deployment evidence. Record selected URL/hash, native version/API, dynamic/static linkage, dependent libraries and actual CPU execution provider in a qualification run. [Published ort-sys source](https://static.crates.io/crates/ort-sys/ort-sys-2.0.0-rc.13.crate), [published Docling PDF source](https://static.crates.io/crates/docling-pdf/docling-pdf-1.69.2.crate), [published Docling ONNX source](https://static.crates.io/crates/docling-onnx/docling-onnx-1.69.2.crate).

The four model/dictionary hashes are populated at [native/manifest.json:10–13](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/native/manifest.json#L10-L13): INT8 layout, OCR detector, English recognizer and character dictionary. Pins alone do not validate tensor names/shapes, opset support, preprocessing, dictionary alignment or CPU result accuracy. Run CPU OCR first on a fixed benign fixture set: native-text, scanned page, mixed page, rotated scan, low-resolution scan and a negative language case. Save exact recognized strings/boxes, uncertainty, model/runtime identities, wall time and peak RSS. Acceleration is a separate optional qualification row; do not equate available CoreML binaries with verified GPU/ANE execution.

There is a concrete loader-contract gap for full Docling: [local preflight:236–265](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/src/backend/docling_backend.rs#L236-L265) accepts an absolute existing file without verifying that it loads. Published Docling1.69.2 `src/pdfium_backend.rs:224–238` then falls back to CWD-resolved `.pdfium/lib` and the system loader if binding that configured path fails. Thus an existing incompatible configured file can reach fallback. This differs from the direct PDFium adapter's fail-closed loader and requires an isolated regression before admitting full Docling to supervised extraction. [Published source archive](https://static.crates.io/crates/docling-pdf/docling-pdf-1.69.2.crate).

The cached [pipeline registry:268–306](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/src/backend/docling_backend.rs#L268-L306) keys OCR/table/force flags and serializes conversions. Test sequential documents with different page windows and explicit immutable model paths; environment/model changes after first use cannot be assumed to rebuild the pipeline. Full-model CLI containment remains unfinished per the handoff; this audit does not lift that restriction.

The live crates registry listed Docling PDF **1.91.0**. Its published manifest changes the feature graph: PDFium is a separate optional feature and still depends on wrapper `0.8`. Updating Docling therefore does not automatically solve a wrapper0.9 migration. No Docling update is recommended without a separate API/feature/runtime/model qualification. [Published 1.91.0 source](https://static.crates.io/crates/docling-pdf/docling-pdf-1.91.0.crate).

## Reproducible qualification matrix

Run each candidate against a frozen baseline on the same host, fixtures and scorer. Preserve failures/skips in denominators. Capture code/lock/manifest hashes, exact tool commands, target, enabled dependency features, library/model hashes, loaded paths, exit/signal, raw output, status/warnings, cold/warm timing and peak RSS. Save `cargo tree --locked -e features` and package duplicates for each feature row. A successful compile or `tpe backends` probe is only one gate.

| Platform/target | Feature rows | Existing source coverage | Required runtime result |
| --- | --- | --- | --- |
| Linux x64 / `x86_64-unknown-linux-gnu` | default; `pdfium`; PR175 `docling-text`; `pdfium,liteparse-layout`; `docling,pdfium` | Default CI; PDFium probe; full-model native workflow does not include this host. | Exact pinned library + negative loader cases, PDFium fixtures, sequencing/resource tests; CPU Docling row separately provisioned. |
| Linux ARM64 / `aarch64-unknown-linux-gnu` | same relevant rows | Default CI, PDFium probe and full-model Native declaration. | Same contracts; do not infer runtime pass from workflow presence. |
| macOS ARM64 / `aarch64-apple-darwin` | `pdfium`; PR175 text/layout rows; separate full-model CPU row | PDFium probe; app checks have their own scope. | Exact dylib, fonts/geometry and lifetime cases; independently qualify ONNX/model row. |
| macOS x64, Windows, musl | not provisioned by this manifest | No matching artifact pin or runtime qualification here. | Unsupported/unqualified until an explicit target/build/runtime contract is added. |
| Browser/WASM | Current PR175 web build; any future native Rust WASM work separate | Web workflow consumes locked published WASM packages, not a native TPE/PDFium build. | Browser worker/import/cancellation/memory tests; no native ABI pass can stand in for them. |

The existing [PDFium probe:41–93](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/.github/workflows/pdfium-probe.yml#L41-L93) already declares all three provisioned platforms and verifies both hashes. Its PR175 combined feature graph also builds optional adapters, which does not prove their external runtimes were present. [Full-model Native:44–97](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/.github/workflows/native.yml#L44-L97) declares only `ubuntu-24.04-arm`. [Default CI:53–80](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/.github/workflows/ci.yml#L53-L80) checks a different graph. The PDFium probe path filter omits `rust-toolchain.toml`, so a toolchain-only change should explicitly trigger/require its matrix; no workflow was edited here.

PR175's [web workflow:26–36](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/.github/workflows/web.yml#L26-L36) uses Node22/pnpm11.25.0 with a frozen pnpm lock and copies published WASM assets. Its lock resolves `pdf-oxide-wasm`0.3.77 and Tesseract.js7.0.0. The installed Rust target list contains no WASM target; this audit did not compile or test a Rust WASM artifact. These are distinct products/toolchains, as required by the handoff.

Core PDFium commands, after independently verifying/provisioning the host's pinned archive and setting the absolute library path:

```sh
export PDFIUM_DYNAMIC_LIB_PATH=/absolute/verified/libpdfium.so
cargo +1.98.1 tree --locked --features pdfium -e features
cargo +1.98.1 clippy --locked --all-targets --features pdfium -- -D warnings
cargo +1.98.1 test --locked --features pdfium --lib backend::pdfium_backend:: -- --nocapture
cargo +1.98.1 test --locked --features pdfium --test pdfium_unicode --test resource_outcomes -- --nocapture
cargo +1.98.1 build --locked --release --features pdfium
./target/release/tpe backends
```

Use `.dylib` on macOS. Add each feature row separately rather than replacing the matrix with `--all-features`; main lacks PR175's `docling-text`/`liteparse-layout` switches. For corpus measurement, use a new output directory and the existing `native/measure_eval.py` plus `native/validate_eval.py` contract in [NATIVE_EVIDENCE.md](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/docs/NATIVE_EVIDENCE.md). Its reported RSS includes evaluator/truth/scoring overhead, not just the backend.

Qualification fixture assertions:

1. Unicode fixtures: native text, invisible existing OCR, partial CMap, zero Unicode; preserve page/chunk/JSON Partial and mapping evidence. A longer plausible string does not establish repair. [tests/pdfium_unicode.rs:38–109](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/tests/pdfium_unicode.rs#L38-L109).
2. Loading/lifetime: missing/relative/mismatched library, repeated open/drop, malformed PDF, failing mapping-page open, concurrent opens, sequential PDFium/full-Docling probes. Keep negative library tests inside a disposable subprocess; check that Docling does not silently bind another path.
3. Geometry/data: rotations/crop boxes, nested Form objects, adjacent text objects, fonts, link targets/placements, figures and byte hashes, repeated image deduplication, overflow warnings. Existing PDFium unit tests begin at [1041](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/src/backend/pdfium_backend.rs#L1041).
4. Limits/containment: empty/invalid page count, 65,535/65,536 boundary, passwords, actual long/repeated-image inputs, crash/timeout/cancellation and worker reaping. Unavailable runtime tests must be reported as **untested**, even when the Rust test process exits successfully after printing `skipped:`. The conditional native gates are visible at [adapter:818–829](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/src/backend/pdfium_backend.rs#L818-L829) and [integration test:12–17](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/tests/pdfium_unicode.rs#L12-L17).

## Maintenance proposal and remaining qualification

Propose a weekly human-reviewed dependency check, plus a prompt review of relevant upstream safety/ABI notices. This is a proposal only; no automation or subscription was created. Track four separate queues: Google's source/public API changes; distributor release/build/asset changes; wrapper release/safety/API/ownership changes; and Rust/Docling/ONNX/model feature and runtime changes. Record observed versions and primary links without automatically changing pins.

For a candidate, change one compatibility axis at a time in a future authorized branch, retaining exact baseline/candidate tuples. Review source/API/ownership changes; verify archives and extracted libraries; inspect native dependencies and notices; resolve one coherent wrapper graph; run strict Clippy and build checks; run the required runtime/fixture/corpus rows; review semantic and resource differences; then consider coordinated manifest/identity/docs updates. Review current upstream font/page-object/thread-safety fixes before accepting long-term retention of an older wrapper. A release being newest, loadable, or lint-clean is not an acceptance criterion.

This audit performed source inspection, hash comparisons of source/lock and downloaded crate archives, live primary-source checks, and actual compiler/Cargo/Clippy version verification. An independent FFI reviewer cross-checked the local PDFium ownership and Docling loader references and reported no correction; the broader provider/browser findings belong in [FFI_REGION_SAFETY_REVIEW.md](FFI_REGION_SAFETY_REVIEW.md).

The parent separately executed fresh Linux x64 checks against **unchanged main-based production sources and main's lockfile**, using the actual Rust1.98.1 tools above and `/workspace/runtimes/pdfium-8066/lib/libpdfium.so`. The archive SHA-256 was `0b43f405477cf2cfc4dbff06905093c3309756c6bca1fb9da99234a2ca97fed2`; extracted library SHA-256 was `7670b3c597b02dfa3f98b23b49c3bb52536312f1ea686b739321731b6011f5a9`, matching the Linux x64 manifest entry. These results were reported by the execution owner during this audit and are summarized in [REGION_FOUNDATION_VALIDATION.md](REGION_FOUNDATION_VALIDATION.md):

| Executed check | Fresh result and scope |
| --- | --- |
| `cargo clippy --locked --all-targets --features pdfium -- -D warnings` | Passed; main source/lock, Linux x64, wrapper0.8.37/API7543. |
| `cargo test --locked --features pdfium --lib backend::pdfium_backend -- --nocapture` | **28 passed, 0 ignored**; real pinned PDFium8066 runtime; no `skipped:` in log. |
| `cargo test --locked --features pdfium --test pdfium_unicode --test resource_outcomes -- --nocapture` | **5 Unicode + 2 resource tests passed, 0 ignored**; no `skipped:` in log. |
| Default root strict Clippy and `cargo test --locked` | Passed; **809 passed, 8 ignored**, including all 17 CLI-containment tests. This does not exercise feature-disabled native backends. |

The PDFium source files are identical in main and PR175, but their surrounding dependency/feature graphs and lockfiles differ; these successes do **not** establish a full PR175 build/runtime pass. Full Docling/ONNX/model inference, the broader PR175 native feature graph, any wrapper upgrade, release-profile build, macOS/ARM64 runtime execution and browser/WASM compatibility remain **untested in this run**. The successful focused baseline gives a reproducible starting point for candidate comparisons, not certification of every input or target.
