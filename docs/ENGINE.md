# Engine baseline

The `tpe` binary extracts, for each PDF, into one SQLite database:

- **Document identity**: SHA-256 of the complete bytes; paths/inodes are observations.
- **Page text** with positioned spans (evidence) and the reading order the engine inferred (`lines`, `text`).
- **Metadata**: title, authors, DOI, arXiv id, year, venue, abstract, with per-field provenance.
- **Reference list**: every entry, raw text authoritative, parsed fields best-effort and never invented.
- **In-text citation markers** linked to reference entries.
- **Chunks**: 20-page groups with a text digest and service time, the unit of the 30 ms target.

Results are keyed by input hash **and** backend identity (name, version, config digest, schema
version). Publishing a run is one transaction and idempotent: re-running replaces the same key.

## Backends

| name | status | notes |
| --- | --- | --- |
| `lopdf` | baseline, pure Rust, always compiled | content-stream interpreter with glyph geometry; no native deps, runs on every CI target |
| `pdfium` | implemented behind feature `pdfium` | `pdfium-render` 0.8 (same major as `docling-pdf`, so one instance links `libpdfium`); needs the PDFium shared library at run time; spans ordered by the engine's XY-cut; image objects become `raster` figures |
| `docling-text` | implemented behind feature `docling` | `docling-pdf` text layer; no models, no PDFium; docling's own reading order is kept |
| `docling` | implemented behind feature `docling` (implies `pdfium`) | full docling pipeline: layout, OCR (scanned pages), pictures as `layout` figures; needs PDFium, the ONNX models and ONNX Runtime |
| `poppler` | planned | comparator; GPL, subprocess or separately licensed adapter |

The default build has only `lopdf`. Build the native ones with
`cargo build --release --features docling` (or `--features pdfium`); provisioning of the
libraries and models is described in [NATIVE.md](NATIVE.md). `tpe backends` lists every
backend, whether it is compiled in, and whether it opens a one-page probe PDF (for example
`open failed: unsupported: pdfium library not found: ...`). Asking `extract`/`eval` for a
backend that is not compiled in fails with a hint naming the feature.

A backend that declares its own reading order (`docling-text`, `docling`) gets one line per
span in `seq` order; the others are ordered by the XY-cut.

## Measured status (2026-09-28)

The `lopdf` backend's reference/metadata/marker/timing numbers from Eval run 36498236950
(`dev`, 60 papers) and run 36498705659 (`holdout`, 10 papers), both after PR #36 on
`ubuntu-24.04-arm`, are in [README.md](../README.md) and GitHub issue #15; per-loop
taxonomies are in [docs/analysis/](analysis/). Body-text alignment (0.877 `dev` / 0.883
`holdout`, exact word LCS, appendices included) is far from the 99% error-free-chunk goal, and
body word precision (82.3% `dev` / 82.0% `holdout`) is held down by untagged figure/table/math
fragments; a few papers remain at 0.65–0.78 body alignment. Citation-marker precision (targets
that resolve to the correct entry) is 99.5% `dev` / 99.9% `holdout`, and marker key recall is
97.2% `dev` / 99.2% `holdout`. Perf loop (PR #35) took arm p50 from 35.4 ms to 18.7 ms, before
loop 10's region tagging added about 4 ms back; p50 is now 22.9 ms `dev` / 20.8 ms `holdout`,
a sub-30 ms diagnostic on these hosted runners; the M1 service-time target is not yet measured.

A pre-loop-10 three-backend comparison on the reference-metrics path (Native run 36491886979)
found `lopdf` at 99.6%/99.7% with body alignment 0.738 at p50 18.7 ms; `pdfium` at 97.3%/99.6%
with body alignment 0.714 at p50 109 ms, about 6x slower than `lopdf`; `docling-text` at
78.1%/94.7%; and the full `docling` layout+OCR pipeline at 96.9%/98.8% with body alignment
0.785 at p50 4892 ms (about 4.9 s per chunk), a routed exception. `lopdf` remains the fast
path. Accuracy reports for `pdfium`, `docling-text` and `docling` come from the Native workflow
(first docling comparison on issue #15).

## Figures

Images never enter page text. Each page records `figures` (index, bbox, kind, MIME, pixel
size, SHA-256, file, caption). When the backend has the bytes, their SHA-256 is recorded;
with `--figures-dir DIR` they are also written to `DIR/<document hash>/p<page>-f<index>.<ext>`
(`png`, `jpg`, `jp2`, else `bin`) and `file` holds that path relative to `DIR`. The ledger
stores them in the `figures` table (`run_id`, `page`, `idx`, `kind`, `mime`, `width_px`,
`height_px`, `sha256`, `file`, `caption`, bbox columns); `tpe stats` prints the count.

## Commands

```sh
tpe extract paper.pdf --db corpus.sqlite --out out/ --jobs 4
tpe extract paper.pdf --db corpus.sqlite --backend docling --figures-dir figures/
tpe backends
tpe stats --db corpus.sqlite
tpe show --db corpus.sqlite --hash 3f2a --refs --meta
tpe bench paper.pdf --iterations 5
```

## Accuracy protocol

A chunk counts as correct only when every page text, every reference entry (raw) and every
citation link match the independently checked reference exactly. Character/word error rates
are diagnostics, not the acceptance metric. Fixtures in `tests/` are synthetic and generated
in-test; a licensed real-paper corpus (arXiv CC-BY, PMC OA) is added by manifest, not committed.
