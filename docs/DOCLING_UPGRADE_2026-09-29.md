# Docling Rust dependency update, 29 September 2026

The application now pins the Docling Rust PDF, core and declarative-format
crates to **1.74.1**, replacing 1.69.2. `Cargo.lock` also resolves the shared
ONNX support crate and the optional speech crate to 1.74.1. Backend identities
report the new linked version. No model files, native binaries, extraction
policy or dependency features are changed by this update.

The version was checked against the [official release](https://github.com/docling-project/docling.rs/releases/tag/v1.74.1)
and the published crates.io records for
[docling](https://crates.io/crates/docling/1.74.1),
[docling-core](https://crates.io/crates/docling-core/1.74.1) and
[docling-pdf](https://crates.io/crates/docling-pdf/1.74.1).
The release was published on 28 September 2026. GitHub's `releases/latest`
endpoint currently selects the separate `npm-cuda-v1.74.1` track; it is not
a reliable selector for Rust crate updates.

PDFium remains **chromium/8066**, the latest non-prerelease returned by the
[binary distributor](https://github.com/bblanchon/pdfium-binaries/releases/tag/chromium/8066)
on the audit date. All three archive digests match the distributor's release
asset metadata. The existing extracted-library digests remain pinned; validation downloaded and verified the Linux x64 library against its installed-file
SHA-256. The other two platform binaries remain subject to native CI.

`pdfium-render` remains **0.8.37**, although 0.9.4 is available. The published
Docling PDF 1.74.1 manifest still requires `pdfium-render = "0.8"`. TPE shares
that binding version with Docling to avoid independent PDFium binding
instances in one process. Moving to 0.9 requires a coordinated integration
change and native runtime tests; it is not a safe lockfile-only update.

The separately released **models-v1** layout, detector, English recognizer
and dictionary assets still match all four existing manifest digests. Model
assets are not advanced merely because the crate version changes.

Existing fixtures and native CI remain the compatibility checks. Historical
accuracy and timing measurements were collected with earlier versions and
do not become measurements of 1.74.1. The adapters retain their UTF-8 policy,
sparse spreadsheet extraction and supplemental Word evidence. This update
does not claim that an upstream release fixes all previously audited losses.
Source inspection confirms that 1.74.1 adds a Word footnote/endnote pass.
Its page-selection and forced-OCR APIs remain available, and the model asset
resolver is unchanged. Recognition and detector fallback behavior still
requires explicit reporting by the caller.

## Adapter compatibility correction

The native-feature build exposed a new `docling_core::Node::KeyValueGraph`
variant that the adapter's exhaustive match did not yet handle. The adapter
now emits its nonempty cell text in the order supplied by Docling, using the
existing text cleanup policy. Cells have no individual geometry, so their
spans do not inherit the graph's region box.

Cell IDs, labels, original values and graph relations do not fit the current
span schema. The containing page explicitly warns that cell metadata and the
counted graph links are not represented. This preserves readable cell text;
it does not claim a lossless structured graph conversion. The match remains
exhaustive so a future upstream variant still requires an explicit decision.

The targeted adapter regression checks cell text and order, blank-cell
handling, span sequence, absent geometry, the loss warning and its page
scoping. It uses constructed Docling nodes and needs no model downloads or
corpus run. Native compilation and test execution are checked on the
integrated branch; the local adapter follow-up was formatted and inspected
without starting a duplicate native build.

## Integrated validation

The integrated branch passed all 666 tests with `docling,formats` enabled,
including the real scanned-text OCR fixture and the new graph-node regression.
Native Clippy with warnings denied, Rust formatting, all 64 Python tests,
Python formatting/lint and the dependency audit also passed. Swift/CMake are
unavailable locally and remain CI gates. This is fixture validation, not a
new corpus accuracy or throughput measurement.

The local build could not reach the ort crate's download CDN. Validation
instead used Microsoft's official `onnxruntime-linux-x64-1.28.0.tgz` from its
[v1.28.0 release](https://github.com/microsoft/onnxruntime/releases/tag/v1.28.0),
verified against release SHA-256
`a3e1b79d7bb1bf09696ce675f49e4064e6c81f6202b8225624fff0e93f8d6407`.
`ORT_LIB_LOCATION` pointed at its `lib` directory, with dynamic linking and
`LD_LIBRARY_PATH` configured. No repository dependency pin was changed for
this local provisioning workaround. PDF/OCR worker and thread counts were one;
Cargo used unoptimized test builds without debug symbols or incremental files.
