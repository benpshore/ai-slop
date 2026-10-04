# Region foundation validation — 2026-10-04

Tracking: [#196](https://github.com/benpshore/pdftextract/issues/196), parents
[#181](https://github.com/benpshore/pdftextract/issues/181) and
[#182](https://github.com/benpshore/pdftextract/issues/182). Read the
[design](REGION_FUSION_DESIGN.md), [compatibility audit](PDFIUM_COMPATIBILITY_AUDIT.md)
and [independent review](FFI_REGION_SAFETY_REVIEW.md) for evidence and limitations.

The implementation is a standalone, explicitly invoked experiment under
`experiments/region-evidence/`, with its own Cargo workspace and lockfile. It does
not join the production workspace or change an extractor, production schema,
router, root manifest/lockfile, runtime pin, existing CI workflow, or release path.
Its new dedicated CI workflow tests that standalone contract. The only supported
decision retains the baseline and abstains. No region replacement, durable
supervisor, service deployment or measured fusion accuracy is delivered.

## Executed source and environment

Production checks used main source
`e27e1fb28a5b40dbca7f517c6fe396e6f11ed4ec`, unchanged in this branch. The source
audit additionally inspected PR175 at
`baafb472873750e7e84d32c3c8e6fa7a680e37db` in a detached read-only worktree. These
are different trees and dependency graphs. Local main checks do not certify all
of PR175. The native release/bibliography and web owners' branches were not edited.

The executor initially lacked Rust. The exact repository pin was installed with
rustup into separate `/workspace/toolchains/{rustup,cargo}` directories, without
changing the repository pin or global shell configuration. Observed tools:

| Tool | Actual executable evidence |
| --- | --- |
| rustup active | `1.98.1-x86_64-unknown-linux-gnu`, overridden by repository `rust-toolchain.toml` |
| rustc | `1.98.1`, commit `48a229ceaefd4985c50990b14116b6d856af0985`, LLVM 22.1.8 |
| Cargo | `1.98.1`, commit `797e8a9bca276c1c9f9f738d2a20f484fa4eea9d` |
| Clippy | `0.1.98 (48a229ceae 2026-09-01)` |
| rustfmt | `1.9.0-stable (48a229ceae 2026-09-01)` |
| Installed Rust target | `x86_64-unknown-linux-gnu` only |
| Python | uv-managed CPython 3.14.7 |

`rust-version` is a minimum compiler declaration. It does not install or select
Cargo. Compiler identity, dependency resolution, native ABI, runtime behavior,
model/provider selection and extraction correctness are distinct checks.

## Completed production-baseline checks

All Rust commands used the observed toolchain, `CARGO_BUILD_JOBS=2`, and a target
directory outside the checkout. Python commands used uv and writable external
cache/install directories. No existing test was removed or weakened.

| Command / check | Result |
| --- | --- |
| `cargo metadata --locked --no-deps --format-version 1` | Passed |
| `cargo fmt --check` | Passed |
| `cargo clippy --locked --all-targets -- -D warnings` | Passed |
| `cargo test --locked` | 809 passed, 0 failed, 8 ignored across 16 result groups; all 17 CLI containment tests passed |
| `uv run --locked ruff format --check` and `uv run --locked ruff check` | Passed; 67 files already formatted |
| `uv run --locked pytest` | 92 passed |
| `uv audit --preview-features audit-command` | Blocked: `api.osv.dev/v1/querybatch` connection tunnel failed after retries; not a clean audit |
| `swift build`, `swift test` | Blocked: Swift executable unavailable |
| CMake configure/build and `ctest` | Blocked: CMake/ctest unavailable; repository's Objective-C++/Foundation targets also require macOS |

Ignored tests are not passes. The default build does not execute optional native
test rows merely because its test command succeeds.

## Standalone contract checks and adversarial scope

The experiment passed `cargo fmt --manifest-path ... --check`, locked strict
Clippy with `--all-targets -- -D warnings`, and **12 integration tests, zero
failures or ignored tests**, using the same pinned Rust toolchain. An independent
reviewer also ran the 12 tests in a separate target directory and reviewed
ownership, indexing, duplicate-key handling and the absence of a promotion API.
The independent review records exact source/test SHA-256 values.

Negative cases cover changed source/artifact hashes and sizes, foreign backend
identity, wrong page order, missing/cross-attempt/out-of-range locators, repeated
text and sequence numbers, duplicate identifiers and nested JSON members,
malformed geometry even when unreferenced, invalid line indices, unknown
frames/decisions/fields, missing artifacts, terminal failure/cancellation/resource
records, and every declared validator budget. A longer complete candidate cannot
replace a partial baseline. These are adversarial **contract tests**, not a
hostile-PDF fuzz campaign or proof of native memory safety.

## Pinned Linux PDFium execution

Downloaded only the existing `chromium/8066` Linux x64 PDFium archive from the
manifest's distributor URL. Both hashes matched before use:

- Archive: `0b43f405477cf2cfc4dbff06905093c3309756c6bca1fb9da99234a2ca97fed2`.
- Library: `7670b3c597b02dfa3f98b23b49c3bb52536312f1ea686b739321731b6011f5a9`.

The absolute configured runtime was
`/workspace/runtimes/pdfium-8066/lib/libpdfium.so`, using locked wrapper 0.8.37.

| Command | Result |
| --- | --- |
| `cargo clippy --locked --all-targets --features pdfium -- -D warnings` | Passed |
| `cargo test --locked --features pdfium --lib backend::pdfium_backend -- --nocapture` | 28 passed, 0 ignored |
| `cargo test --locked --features pdfium --test pdfium_unicode --test resource_outcomes -- --nocapture` | 5 Unicode and 2 resource tests passed, 0 ignored |

No `skipped:` runtime messages appeared. These are focused debug-profile checks
against the existing tuple, not a release benchmark, source-built PDFium result,
new wrapper qualification, or entire platform/feature matrix.

## Actual artifact compatibility probe

Separate explicit lopdf and PDFium CLI invocations processed the existing synthetic
`tests/fixtures/pdfium-unicode/native.pdf`. Both returned complete schema-5 output
with one page and three spans. This is a manually invoked data compatibility
probe, not the proposed cheap-first scheduler or region-selection implementation.

| Evidence | SHA-256 |
| --- | --- |
| Source, 1,433 bytes | `099a620bd29179e329704c152808ad8e3e34f0d5388894c43d17fb3340d373c8` |
| Debug CLI, reports `tpe git-e27e1fb` | `2674dbc4712d65b7f41ddee341086fb3ee1be8a5497fd4c5b18c07272b5ec498` |
| Exact lopdf JSON artifact | `b079ca848e863aef956c06ec7d3fdb9cc693f0f869ee54d83d5e859637a782be` |
| Exact PDFium JSON artifact | `e6a15066a35cd0f14636cfbe01e5020deb1b5a615fc04e30f0b0c513476be698` |

The texts agree in this simple fixture while their reported boxes differ. That
does not verify correspondence or semantic accuracy. Artifact hashes vary with
timings/source observations; reproducing a run must retain its own exact bytes.
No external document was submitted to a service.

The new `verify_artifacts` example successfully validated these actual saved
lopdf/PDFium outputs: **one page, 12 typed locators, 3,811 baseline bytes and
3,830 alternative bytes retained**, with `retain_baseline_and_abstain`. It reads
already produced artifacts and executes no extraction itself. The example
declares runtime/frame unknown; the separately measured binary/runtime identity
above is not silently invented as a field in the extraction artifact.

## Remaining gates

CPU OCR implementation is separately tracked in #197 under OCR-PDF epic #199;
this branch contributes compatibility requirements rather than model execution.

Full PR175 feature combinations, Docling CPU inference/models/ONNX selection,
MuPDF/Poppler runtime fixes, source-build C ABI sentinel, release measurements,
ARM64/macOS, generated JS/WASM internals and real browser tests were not executed
here. The independent review marks the Poppler callback lifetime concern as an
unreproduced hypothesis and distinguishes it from proven provenance limitations.

Actual region fusion still requires adapters that retain raw evidence and frame
witnesses, durable baseline persistence before isolated engine attempts, verified
region correspondence, cancellation/resource/crash tests, and independently
adjudicated rendered plus semantic ground truth across real engines. No numeric
confidence or longest-output rule substitutes for those gates.
