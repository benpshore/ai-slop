# Routing and resolution: measured state, 2026-09-30

Branch `fix/bibliography-publisher-styles`. All numbers below were produced
locally on Apple Silicon with the `docling` feature build (PDFium and the
docling models provisioned) against ten PubMed Central open-access PDFs and
their publisher JATS reference lists, fetched from the PMC AWS bucket
(`PMC3757425`, `PMC3777687`, `PMC4488360`, `PMC5432165`, `PMC6346594`,
`PMC7051178`, `PMC7655836`, `PMC8537375`, `PMC9131128`, `PMC9358619`).
Ten files are a diagnostic set chosen from the 200-paper measurement's
worst cases, not a representative sample.

## What changed

- `crate::router`: `lopdf` is the probe. Per page it reports unmapped fonts
  (`undecodable`, `decoded as Latin-1`, or a share of U+FFFD) and scans
  (no text under a page-sized raster). `--backend auto` (now the default
  for `extract` and `bibliography`) re-reads with `pdfium`, then docling.
- The backward bibliography scan routes the same way, checks a found list
  for plausibility (highest printed label and year count versus entries)
  and re-reads an implausible list with docling for the pages from its
  heading on; the longer list is kept. Interrupted lists resume past
  back-matter headings (numbered lists at the expected label when it
  carries reference evidence; author-year lists at the next run of entry
  starts).
- docling: page window (only the requested pages are converted), picture
  bytes kept only when a job exports figures, runs of spaces collapsed.
- `crate::resolve` (`tpe bibliography --resolve [--mailto] [--csv]`):
  every entry resolved from a `doi.org` link annotation on the entry, then
  the printed DOI, then Crossref `query.bibliographic`; each record verified
  against the raw printed entry (first-author family name present, year
  within one present, most title words present; a venue named in the
  entry ranks an article above its preprint). Every entry carries an
  attempt log (method, DOI tried, outcome, which field disagreed). The
  paper resolves from its metadata DOI or a title query. Link annotations
  are read with `lopdf` for every backend.

## Entry capture (backward scan, routed)

| file | publisher | before | now | backend |
| --- | --- | ---: | ---: | --- |
| PMC3757425 | MDPI 2013 | not found | 24 / 25 | docling |
| PMC3777687 | Elsevier | 41 / 49 | 41 / 49 | lopdf |
| PMC4488360 | PLOS 2015 | not found | 76 / 79 | docling |
| PMC5432165 | PLOS 2017 | 20 / 91 | 87 / 91 | docling |
| PMC6346594 | Frontiers 2019 | 14 / 56 | 42 / 56 | docling |
| PMC7051178 | eLife | not found | 66 / 66 | docling |
| PMC7655836 | Nature | not found | 23 / 23 | pdfium |
| PMC8537375 | MDPI 2021 | 9 / 65 | 65 / 65 | docling |
| PMC9131128 | MDPI 2022 | 5 / 57 | 56 / 57 | docling |
| PMC9358619 | Frontiers 2022 | 9 / 39 | 39 / 39 | lopdf |

Routed files cost 0.3–2 s each; unrouted ones 7–14 ms.

## Resolution (all ten files, 519 entries, anonymous Crossref pool)

| outcome | entries |
| --- | ---: |
| resolved, DOI equals the publisher's DOI | 402 |
| resolved, publisher lists no DOI (author and year verified) | 71 |
| resolved to a DOI that differs from the publisher's | 0 |
| unresolved, publisher has a DOI | 6 |
| unresolved, publisher has no DOI either | 40 |

After the raw-text verification and bibliographic query (measured on the
three worst files only): 149 of 176 entries resolved, 0 wrong, 27
unresolved of which 22 are Chinese-language journals absent from Crossref
and the rest books, reports and software. The attempt logs say so for each.
Wall time was 21 minutes for 519 entries: unusable at scale until the
polite pool (`--mailto`) and per-document parallelism are used.

## Not done, in priority order

1. `--resolve` speed: polite pool, parallel documents, DOI-first batching.
2. The 200-paper PMC measurement of the routed scan and of resolution
   (the workflow still builds `lopdf` only).
3. The two-command CLI (PR #126, red on six clippy lints) needs rebasing
   onto this branch; its SQLite schema must gain the resolved fields.
4. Self-contained binary (PDFium, models, ONNX runtime embedded), Apple
   PDFKit/Vision backend, docling image path, non-PDF inputs, tables, the
   VLM lane: see the memory notes.
5. Field exactness of parsed entries (first author 92%, title 73% strict
   on the old path) is now secondary: the resolved record supplies the
   fields; the printed string is kept verbatim.
