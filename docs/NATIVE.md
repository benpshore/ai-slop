# Native backends: provisioning

The default build (`cargo build`) has no native dependencies: the `lopdf`
backend is pure Rust. Two optional Cargo features add backends that need
artifacts which are not crates:

| feature | backends | needs at run time |
| --- | --- | --- |
| `pdfium` | `pdfium` | the PDFium shared library |
| `docling-text` | `docling-text` | nothing; pure Rust, supervised page parser |
| `docling` (implies `docling-text`, `pdfium`) | `docling-text`, `docling` | `docling`: PDFium, the layout and OCR models, ONNX Runtime; not enabled for supervised CLI extraction |

`native/manifest.json` pins the exact set of artifacts and `native/fetch.sh`
provisions them. The `Native` workflow (`.github/workflows/native.yml`) does the
same in CI.

## What gets provisioned, and where

Paths are relative to the repository root. `fetch.sh` always works from there,
whatever directory you run it from.

| file | source | size |
| --- | --- | --- |
| `.pdfium/lib/libpdfium.so` (Linux) or `.pdfium/lib/libpdfium.dylib` (macOS) | the `lib/` member of `pdfium-<platform>.tgz` from [bblanchon/pdfium-binaries](https://github.com/bblanchon/pdfium-binaries), release `chromium/8066` | not recorded yet (the first run prints it) |
| `.models/layout_heron_int8.onnx` | docling.rs release `models-v1` | about 65 MB |
| `.models/ocr_det.onnx` | docling.rs release `models-v1` | about 9 MB |
| `.models/ocr_rec_en.onnx` | docling.rs release `models-v1` | about 8 MB |
| `.models/en_dict.txt` | docling.rs release `models-v1` | small text file (the first run prints the size) |

The models come to about 82 MB, plus the PDFium library. The model sizes are
approximate, taken from the project's notes on the release. The `bytes` column
that `fetch.sh` prints is authoritative.

PDFium URLs, one per platform:

- `https://github.com/bblanchon/pdfium-binaries/releases/download/chromium%2F8066/pdfium-linux-arm64.tgz`
- `https://github.com/bblanchon/pdfium-binaries/releases/download/chromium%2F8066/pdfium-mac-arm64.tgz`
- `https://github.com/bblanchon/pdfium-binaries/releases/download/chromium%2F8066/pdfium-linux-x64.tgz`

Model URLs: `https://github.com/docling-project/docling.rs/releases/download/models-v1/<file>`.

This is a deliberate subset of docling.rs's own `download_dependencies.sh`, and
it differs from that script in four ways:

- **Pinned PDFium.** The upstream script takes bblanchon's `releases/latest`,
  which is a moving target. This manifest uses `chromium/8066`. On Linux x64,
  upstream uses the `libpdfium.so` hosted in `models-v1` (its conformance
  build). We use bblanchon's `chromium/8066` there too, so that every platform
  has the same release line.
- **INT8 layout model only.** docling-pdf uses `layout_heron_int8.onnx`
  automatically when it is present and `DOCLING_LAYOUT_ONNX` is not set. The
  fp32 `layout_heron.onnx` (164 MB) is only needed when `DOCLING_RS_FP32=1`
  forces full precision, so it is not provisioned.
- **No extras.** There is no TableFormer (the `docling` backend runs with
  tables off), no multilingual OCR pair, no ASR, chunker, picture classifier or
  enrichment models.
- **One URL per file, no fallback mirrors.** Upstream falls back to Hugging
  Face, ModelScope or the PaddleOCR `main` branch when a release asset is
  missing. Those locations are mutable and would defeat the pin, so here a
  missing asset fails the run and names the URL.

### ONNX Runtime

ONNX Runtime is not in the manifest. docling-pdf depends on the `ort` crate
(2.0.0-rc.13) with the `download-binaries` feature. At build time, the `ort-sys`
build script downloads a prebuilt ONNX Runtime for the target, so building with
`--features docling` needs network access once. The Native workflow caches
`target/`, and the paths where `ort-sys` is expected to keep its download
cache, so rebuilds reuse it.

Not verified offline:

- which ONNX Runtime version and URL `ort-sys` fetches;
- whether it links statically or ships a dylib next to the binary;
- where its cache lives.

Check the build log of the first Native run for these. Pinning ONNX Runtime
(`ORT_LIB_LOCATION` or equivalent, with a hash) is a follow-up.

## Environment variables

| variable | meaning | default when unset |
| --- | --- | --- |
| `PDFIUM_DYNAMIC_LIB_PATH` | absolute directory containing `libpdfium.{so,dylib}`, or the absolute library file itself | required for `pdfium` and full `docling` |
| `DOCLING_RS_MODELS_DIR` | the `.models` directory **itself** (not its parent). It is consulted for a `.models/<file>` path only when that path does not exist under the CWD | `.models/` under the CWD, then next to the executable and one level above it |
| `DOCLING_LAYOUT_ONNX`, `DOCLING_OCR_DET_ONNX`, … | per-file overrides from docling.rs; they bypass the resolver entirely | unset |
| `DOCLING_RS_FP32` | `1` forces the fp32 layout model, which is not provisioned here | unset |

Relative PDFium paths are rejected. In particular, the runtime never loads
`.pdfium/lib` relative to the working directory; pass the absolute path to a
provisioned library instead (for example,
`PDFIUM_DYNAMIC_LIB_PATH="$(pwd)/.pdfium/lib"`).

The resolution order comes from docling-core 1.69.2 (`assets.rs`) and
docling-pdf 1.69.2 (`pdfium_backend.rs`, `layout.rs`). Running `tpe` from the
repository root after `fetch.sh` needs an absolute `PDFIUM_DYNAMIC_LIB_PATH`.
CI sets both variables to absolute paths, so tests and `tpe` do not depend on
the CWD.

## Licences

| artifact | licence |
| --- | --- |
| PDFium | BSD-3-Clause (Google's PDFium licence), plus the notices of the third-party code it bundles (FreeType, libjpeg-turbo, zlib and others). bblanchon's archives include licence files. Check the tgz before redistributing: that was not verified offline |
| pdfium-binaries packaging | the bblanchon repository's own licence; not verified offline |
| `layout_heron_int8.onnx` | the docling.rs README says that the layout and TableFormer models are "PyTorch→ONNX exports of docling-project's own models (Apache-2.0 / CDLA-Permissive-2.0)" and points to its `docs/MODELS_NOTICE.md` for attribution. Which of the two licences covers which file was not verified offline |
| `ocr_det.onnx`, `ocr_rec_en.onnx`, `en_dict.txt` | re-hosted unmodified from PaddleOCR (PP-OCRv3 recognition and dictionary) and RapidOCR (PP-OCRv6 detector), per the docling.rs README. Their licence is not stated in the sources available offline: **unknown**, check upstream before redistributing |
| ONNX Runtime | fetched by `ort-sys`; licence not verified offline (upstream ONNX Runtime is MIT) |
| docling.rs crates | MIT |
| `pdfium-render`, `ort` crates | MIT OR Apache-2.0 |

None of these artifacts is committed to the repository. The `.models/` and
`.pdfium/` directories must stay out of git.

## Running locally (Apple Silicon, e.g. an M1)

```sh
sh native/fetch.sh                                   # about 82 MB of models plus PDFium; prints a table
export PDFIUM_DYNAMIC_LIB_PATH="$PWD/.pdfium/lib"    # required: relative library paths are rejected
cargo build --release --features docling,pdfium      # the first build downloads ONNX Runtime
cargo test --features docling,pdfium -- --nocapture  # native smoke tests must not print "skipped:"
# Full model-backed docling remains restricted to explicitly provisioned library/eval use.
# For supervised text parsing without native runtimes or models:
cargo build --release --features docling-text
./target/release/tpe extract paper.pdf --backend docling-text --db local.sqlite --out out/
```

The platform is detected from `uname`: `Darwin`/`arm64` maps to `mac-arm64`,
`Linux`/`aarch64` to `linux-arm64` and `Linux`/`x86_64` to `linux-x64`.
Intel Macs (`mac-x64`) have no manifest entry, so `fetch.sh` stops with an
error. To run the docling evaluation, follow [EVAL.md](EVAL.md) and add
`--backend docling`.

`fetch.sh` is idempotent. Files that are already present are not downloaded
again, but they are re-hashed on every run. Use `--force` to re-download
everything, and `--print-hashes` to hash what is on disk without touching the
network.

## How hashes get pinned

Every manifest entry has `sha256`, the digest of the file at `dest`. PDFium
entries also have `archive_sha256`, the digest of the downloaded `.tgz`. Both
start as `null`:

1. A run with `null` hashes downloads, prints `file, bytes, sha256` (and
   `archive_sha256` for a fresh PDFium download) with the state
   `downloaded,unpinned`, and passes.
2. Copy the digests from the log of the `Provision PDFium and the docling
   models` step of a Native run on each platform into
   `native/manifest.json`. Each PDFium platform entry takes its own digests
   from the run on that platform. `linux-x64` has no CI leg, so pin it from a
   local run on such a host or leave it `null`. Commit the change on a branch
   and open a PR.
3. From then on, a mismatch fails the run and deletes a freshly downloaded
   file, and a cached file that no longer matches fails with a hint to rerun
   with `--force`.

Pinning a hash changes the manifest, and with it the `.models`/`.pdfium` cache
key, so the next run downloads fresh files and verifies them against the pins.
Changing a URL (a new PDFium release, a new models tag) works the same way:
edit the entry, reset its hashes to `null`, run the workflow, then pin.

The bounded Docling text module and remaining layout/OCR deployment requirements
are documented in [DOCLING.md](DOCLING.md).
