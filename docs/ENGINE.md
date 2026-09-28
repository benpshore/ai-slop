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
| `lopdf` | baseline, pure Rust | content-stream interpreter with glyph geometry; no native deps, runs on every CI target |
| `pdfium` | planned | `pdfium-render` pinned to the same major `docling-pdf` uses (0.8) |
| `poppler` | planned | comparator; GPL, subprocess or separately licensed adapter |
| `docling` | planned | layout/OCR/tables for routed pages only |

## Commands

```sh
tpe extract paper.pdf --db corpus.sqlite --out out/ --jobs 4
tpe stats --db corpus.sqlite
tpe show --db corpus.sqlite --hash 3f2a --refs --meta
tpe bench paper.pdf --iterations 5
```

## Accuracy protocol

A chunk counts as correct only when every page text, every reference entry (raw) and every
citation link match the independently checked reference exactly. Character/word error rates
are diagnostics, not the acceptance metric. Fixtures in `tests/` are synthetic and generated
in-test; a licensed real-paper corpus (arXiv CC-BY, PMC OA) is added by manifest, not committed.
