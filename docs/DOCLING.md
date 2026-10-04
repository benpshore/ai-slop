# Docling modules

The integration remains pinned to **docling.rs 1.69.2**. Its APIs and behavior
were checked against the published crate source, commit
`908f080d5def736bbc094beeaf5b4a04fe504dd3`. Current official documentation exposes
1.91.0 and a newer default Rust renderer; those capabilities are not claimed by
this pinned integration: https://docs.rs/docling-pdf/latest/docling_pdf/ and
https://github.com/docling-project/docling.rs.

## Text parser: ready for supervised extraction

```sh
cargo build --features docling-text
./target/debug/tpe extract --backend docling-text --db output.sqlite input.pdf
```

This feature disables upstream default features. It needs no PDFium, ONNX,
models, environment-selected OCR, model downloads, or subprocesses. A retained
`PageTextParser` opens immutable bytes once and parses only requested pages. Rust
owns reading order and cleanup. Cells keep their real geometry, translated from
Docling's clipped crop frame to unrotated PDF coordinates. The public cells API
provides neither font name nor font size; those fields remain absent.

The lopdf evidence session preserves raw URI annotations, images, metadata, and
completeness warnings. Non-whitespace Unicode scalar coverage is compared against
that native evidence. A mismatch is Partial; a shorter Docling candidate retains
the native text. Matching scalar counts establish a coverage cross-check, not a
claim that reading order or punctuation interpretation is perfect. Existing
mapping or resource-limit evidence is never cleared. The upstream parser does
not support password input, so encrypted documents require another backend.

The CLI's existing disposable worker applies hard resource limits. The library
API does not itself install a process boundary. Output cells/text are capped,
while parser computation and native allocations remain contained by that worker.

## Layout and OCR: separate opt-in deployment

`--features docling` includes the text feature and explicitly enables the older
PDFium/ONNX pipeline. The `docling` backend remains unavailable in supervised CLI
extraction. It converts and caches the document rather than providing the bounded
page parser above; model discovery and inference still require a separately
provisioned trusted runtime. No models are downloaded by this adapter.

The adapter explicitly selects in-process PP-OCR, so an environment variable
cannot silently switch it to an external Tesseract command. Raw URI annotation
targets are retained separately from Docling's Markdown wrappers. When crop or
rotation frames cannot be verified, their rectangles remain absent with a Partial
diagnostic instead of combining incompatible geometry. Missing pages,
unknown geometry, undecoded formulas, and unverified reconstruction coverage are
reported as Partial. A successful upstream conversion is not completeness proof.

Enabling layout/OCR in the supervised CLI needs a further deployment contract:
absolute per-file model paths and hashes, PDFium and ONNX runtime identities,
explicit language/mode, fixed worker thread limits, no CWD asset fallback, and
verified page/output coverage. These assets are not included in standard releases.
The upstream model resolver checks the working directory, and missing recognition
models can degrade to warnings, so merely setting a model directory is not enough.
