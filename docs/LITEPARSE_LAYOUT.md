# LiteParse spatial layout

`liteparse-layout` is an optional layout mode over the engine's existing PDFium
extraction. LiteParse 2.15.1 contributes its pure `stages::project` spatial grid
projection: aligned form fields and table rows retain horizontal spacing in text.
It is not another PDF parser, OCR route, or tagged-PDF structure extractor.

Build and run from the checkout root:

```sh
sh native/fetch-liteparse.sh
cargo build --locked --release --features liteparse-layout
export PDFIUM_DYNAMIC_LIB_PATH="$PWD/.pdfium/lib"
./target/release/tpe extract --backend liteparse-layout --db index.sqlite document.pdf
```

The first command provisions the already-reviewed `native/manifest.json` PDFium
8066 library and matching headers. Both the downloaded archive and the extracted
library are checked before installation. To reuse that exact downloaded archive
offline, pass `--archive /absolute/path/pdfium.tgz`.

The optional dependency has default features disabled: no Tesseract build or OCR
is enabled. Its transitive native build script otherwise auto-downloads another
PDFium artifact without a checksum. The repository's `.cargo/config.toml` supplies
both explicit build paths, so absent provisioning fails at directory validation
before that download path. Explicit `PDFIUM_LIB_PATH` and `PDFIUM_INCLUDE_PATH`
overrides must name your verified library/header directories. They do not change
the runtime `PDFIUM_DYNAMIC_LIB_PATH` loader policy.

The adapter never calls LiteParse's parser, conversion, network/OCR functions, or
native initialization. This matters because its separate PDFium wrapper has a
different process mutex and initialization lifetime from `pdfium-render`; mixing
the native APIs would not be safe. Only the existing engine PDFium backend opens
documents. Projection runs after those native handles have closed, inside the
same bounded extraction worker, with Rust-owned data.

Raw spans, font metadata, boxes, figures, link targets and link rectangles, and
Unicode/coverage warnings are retained. Grid rows have no invented source boxes
or span references. Backend identity includes the LiteParse version, projection
policy/limits, and the PDFium binding and configured-library fingerprint.

Projection accepts unrotated pages with finite, in-page positioned text, at most
4,096 spans and 1 MiB of source text. Grid dimensions and output text are bounded.
It must preserve the complete multiset of non-whitespace source characters.
Missing geometry, rotation, a limit, or changed character coverage retains the
ordinary native layout and adds an `extraction_incomplete:` warning, so the result
is Partial. Native Unicode warnings remain Partial even when projection succeeds.
Page coverage and native figure-byte ownership are unchanged.

The mode is opt-in and does not participate in automatic parser replacement.
Its benefit is spatial alignment; semantic reading order of arbitrary tables or
multicolumn prose is not established by this adapter's character-coverage check.

Upstream evidence reviewed on 2026-10-03:

- [Published crate 2.15.1](https://crates.io/crates/liteparse/2.15.1), pinned exactly
  in Cargo.lock (archive SHA-256
  `aebab0be136b71fe6003ee23eb397f798863165ff3a6f959438267c5317b894c`).
- [Public pure-stage contract](https://github.com/run-llama/liteparse/blob/b2a87350e5a40638d8e39d47875a6b8f4e4a0faf/crates/liteparse/src/stages.rs).
- [Separate native lifetime/lock](https://github.com/run-llama/liteparse/blob/b2a87350e5a40638d8e39d47875a6b8f4e4a0faf/crates/pdfium/src/library.rs).
- [Native build acquisition paths](https://github.com/run-llama/liteparse/blob/b2a87350e5a40638d8e39d47875a6b8f4e4a0faf/crates/pdfium-sys/build.rs).

The crate still brings image/SVG and HTTP dependencies into the optional build;
those dependencies are not additional runtime capabilities of this adapter.
