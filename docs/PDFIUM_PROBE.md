# Rust-supervised PDFium evidence experiment

This is the next architecture experiment, separate from production extraction
and its existing PDFium object traversal. `tpe-pdfium-probe` reads PDFium's
native text-page character API directly. It has no project XObject mapping,
reading-order reconstruction, citation parsing, OCR or Docling stage. The
Rust controller owns acquisition, limits, worker termination and result
validation. A worker owns its PDFium library/document/page handles until exit.

## Run

Build with `cargo build --features pdfium --bin tpe-pdfium-probe`. Provision the
pinned library as described in [NATIVE.md](NATIVE.md), then on Linux:

```sh
PDFIUM_DYNAMIC_LIB_PATH=/absolute/trusted/pdfium/lib \
  target/debug/tpe-pdfium-probe run paper.pdf > evidence.json
```

The library location must be explicit and absolute. No Python, models, server,
credential broker or GUI is involved at runtime. Optional Docling processing
is a later decision after evaluating this evidence contract. The existing
native adapters remain available through `tpe`; this probe changes no default
backend or production routing. The binary's default Cargo feature is off.

## Contract version 1

One JSON object is emitted. Exit status is zero only for `complete`; all other
record outcomes use exit status 2. Invalid CLI/input-path arguments remain CLI
errors. `complete` means the probe collected every requested character record,
not that the text is an accurate transcription or in semantic reading order.

| Field | Meaning |
| --- | --- |
| `contract_version` | `1` |
| `outcome` | `complete`, `limited`, `timeout`, `worker_failed`, `unavailable` |
| `detail` | Explicit limit/failure reason, or null |
| `input_sha256` | Hash of the bounded private byte snapshot actually parsed; null if acquisition exceeded its limit |
| `backend` | `pdfium-text-page`, binding version, actual dynamic-library SHA-256; null if the library was unavailable |
| `limits`, `applied_limits` | Requested limits and actual child kernel limits, which may be stricter if inherited |
| `worker_pid`, `worker_exit`, `elapsed_ms` | Process evidence and controller elapsed time, including snapshot creation |
| `total_pages` | Native signed page count checked before conversion; never the binding wrapper's truncated 16-bit count |
| `pages` | Complete page records; empty on limits/failures so partial JSON is never published as complete evidence |

Each page has its one-based number, PDFium-reported width/height in points,
rotation in degrees, zero-based character indexes and count of characters
whose origin or tight bounds are unavailable. Each character retains the raw
Unicode value and optional scalar (zero/unmapped/invalid values have no scalar),
tight box `[left, bottom, right, top]`, origin `[x, y]`, unscaled native font
size in points, angle in radians, and optional native generated/hyphen flags.
Missing/nonfinite geometry becomes null. Values stay in PDF user space as
returned by `FPDFText_*`; no crop/rotation/viewer transform is inferred.

Character indexes refer to the native text page, including generated spaces
and line separators. They are not guaranteed glyph IDs, PDF source byte
offsets or logical reading-order positions. Unicode substitution, ligatures,
rotated/cropped pages, annotations and complex scripts need corpus validation
before this can replace production evidence. Native text API results can
already lose information; avoiding our object mapping does not prove fidelity.
No table structure, figure bytes, identifiers or resolver results are supplied.

## Containment and termination

Defaults are 64 MiB input, 50 pages, 250,000 characters across the document,
32 MiB serialized output, a 30-second worker deadline, 15 CPU seconds and
512 MiB address space. The first four CLI overrides are `--max-pages`,
`--max-chars`, `--max-output-bytes`, and `--timeout-ms` (see `--help`). Input
must be a completed regular local file; concurrent source writers are outside
this experiment's acquisition protocol. The private snapshot fixes the exact
bytes/hash used by the child.

On Linux the child lowers `RLIMIT_AS`, `RLIMIT_CPU`, `RLIMIT_FSIZE` and disables
core dumps before loading the PDF or PDFium. The controller's limits are never
changed. It kills and waits for the child on deadline and also reaps it on
early error paths. A native crash, allocation abort or unexplained kernel
signal is `worker_failed`, preserving the exit description without guessing
an OOM cause. Reported page/character/output cutoffs are `limited`.

The serializer bounds its writes (reserving 1 KiB for controller metadata),
discards incomplete JSON and emits a compact limit result. The controller
also bounds reading and validates contract/input identity. Child stdout/stderr
cannot fill an IPC pipe. Temporary files are private and cleaned up. The
deadline includes worker startup, native loading, extraction and serialization;
source acquisition occurs before it. Native allocation can happen before the
character count becomes available, so character/page caps alone are not memory
protection. Address-space limits are not an RSS guarantee.

This proves disposable-process resource/crash containment on Linux, not a
filesystem/network security sandbox or containment of adversarial descendants.
macOS/Windows currently report `unavailable` rather than claiming equivalent
enforcement. Apple Silicon performance and production worker/session pooling
remain unmeasured and unimplemented.

## C binding review and upstream compilation

The worker's native handles borrow the bindings and input buffer. Text pages
close before pages, pages before documents, and documents before library
destruction. Error returns use the same scoped cleanup. One worker initializes
one PDFium instance and calls it on one thread. The locked pdfium-render 0.8.37
`thread_safe` wrapper holds a process-wide mutex from initialization through
destruction; the controller never initializes PDFium. Native signed page and
character counts are checked before conversion or allocation. Character box
and origin calls receive pointers to live `double` locals, and retained results
copy their values into Rust-owned data.

The controller snapshots the explicit trusted library into its private temporary
directory before starting the worker. The worker loads and hashes that same
private copy, so replacing the configured deployment path cannot make the
report identify another library. This copy has a 64 MiB bound and occurs before
the worker's output file limit is applied. A trusted library with additional
shared-library dependencies must have those dependencies provisioned by the
system loader; this experiment's verified artifact is a single PDFium library.

The Linux x86-64 and ARM64 platforms use LP64. In this binding's dynamic API,
`FPDF_LoadMemDocument64` uses `c_ulong` for upstream `size_t`; those have matching
widths on these platforms. That declaration needs separate review on Windows
LLP64. Windows and macOS worker containment remain unavailable. Symbol loading
at bind time checks API availability, not ABI correctness or native parser
memory safety. Resource limits and process disposal bound crash/resource
effects; they do not turn the C++ parser into a memory-safe implementation.

[PDFium upstream source probe](../.github/workflows/pdfium-source.yml) compiles
the explicit commit in [pdfium-source.json](../native/pdfium-source.json) on
Linux x86-64. The immutable source `DEPS` pins the full depot_tools SHA; the
recipe extracts that literal before running tools, fetches exactly that commit,
disables its updates, and verifies both bootstrap and DEPS checkouts. GN, Clang,
sysroot and other dependencies are supplied by that revision's dependency
hooks. The artifact records DEPS, dependency revisions, GN arguments, source
changes, compiler version/hash, library hash, code/Rust identity and build log.
The Ubuntu runner image and apt packages are recorded by Actions and remain
host dependencies; this is reproducible source selection and build provenance,
not a promise of byte-identical binaries across runner image updates.

The small shared-library/public-export patch comes from
[bblanchon/pdfium-binaries at f2e9a1c](https://github.com/bblanchon/pdfium-binaries/blob/f2e9a1c45bb17b85b540abf1af30146ef65416ac/patches/shared_library.patch),
whose reviewed release build uses the same deployment shape. The experiment
commit is listed in the 8066 release notes; it is not claimed to reconstruct the
production prebuilt artifact's exact source checkout. V8 and XFA stay disabled.
The source-built library runs the existing real worker evidence/containment
tests and a public C header signature/runtime sentinel. This covers the APIs
used by the experiment; it is not PDFium's complete upstream test suite.

The source compile runs manually or when a PR changes its pin/recipe/workflow.
It is excluded from ordinary Rust PRs and capped at 40 minutes with two compile
jobs. The reference 8066 Linux x64 upstream build compiled approximately 1,195
objects in 4.5 minutes after checkout/setup; cold dependency downloads and
runner resources can cost more. No schedule runs until that cost is reviewed.
Source ARM64, macOS and Windows compilation are separate work; existing
verified-prebuilt CI still exercises Linux ARM64. Updating the source pin
requires reviewing the same evidence in a PR. Passing it never updates
`native/manifest.json`, installs the artifact in production, or merges a PR.

## Evidence and checks

`tests/pdfium_probe.rs` runs the actual controller and native library against
a generated two-page nested-Form PDF. Four translated occurrences of `Nested`
survive per page, including a rotated page; the origins include the leaf
Form's matrix. Native geometry is obtained without traversing project objects.
The test checks input/library identities, actual kernel-limit metadata, page,
character and serialized-output cutoffs, a 65,536-page count boundary, malformed
input followed by a successful new worker, and a real worker timeout. Every
observed child PID is reaped. Real-library tests require
`PDFIUM_DYNAMIC_LIB_PATH`. The dedicated PDFium probe workflow runs on Linux
x86-64 and ARM64 using verified library/archive pins, without Docling models.
It uploads the generated PDF, actual evidence JSON and tested code/toolchain
identity; the existing full Native workflow also exercises these tests.

Unit tests force a worker crash and a hung worker, prove the writer's byte
bound, and use a separate child to reject a 256 MiB allocation under a 128 MiB
address-space limit and terminate at one CPU second while leaving the parent
limits unchanged. These are containment/evidence tests, not accuracy or speed
benchmarks. Keep the existing 200-paper lopdf baseline distinct until the
probe is measured on the same inputs with an explicit scorer.
