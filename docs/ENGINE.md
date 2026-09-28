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
