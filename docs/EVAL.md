# Evaluation harness

`tpe eval` measures the engine against real papers whose bibliography is known
independently of any PDF extraction. It is a **diagnostic**: it tells us where
reference and citation extraction breaks on real typesetting, quickly and for
free. It is **not** the acceptance protocol in [ENGINE.md](ENGINE.md), which
requires a human-checked reference for every page, entry and link.

## Why arXiv sources are usable ground truth

Every arXiv e-print ships the LaTeX the author compiled. The bibliography is in
one of two forms:

- a `.bbl` file: the typeset bibliography, one `\bibitem` per printed entry.
  This is exactly the list the PDF shows, in order, so the entry **count** is
  authoritative and the entry text is close to the printed text after macro
  expansion.
- a `.bib` file plus `\cite{...}` commands in the `.tex`: the database is a
  superset of what is printed, so the harness keeps only the keys that are
  cited (or every entry when `\nocite{*}` is present). BibTeX styles may still
  drop, merge or reformat fields, so the entry text is approximate.

The `\cite` commands also give the number of in-text citation commands and the
keys they target, which the marker extractor must resolve.

This truth is **independent** (it never looks at the PDF) but **imperfect**:
`.bib` entries can contain typos that the printed PDF also contains, some
authors edit the `.bbl` by hand, macros we do not expand leave residue, and a
`\cite` with three keys is one command but may print as one marker (`[1-3]`)
or three. Metrics therefore have a noise floor; they are for trend and triage,
not for sign-off.

## What is measured

Per paper (`PaperEval` in `report.json`):

| metric | meaning |
| --- | --- |
| `ref_count_exact` | the extracted reference count equals the truth count. This is the **exact-count rule**: a bibliography with one entry merged or split is wrong, however good the rest looks. |
| `matched_refs` / recall / precision | truth entries matched one-to-one to extracted entries, by DOI, then arXiv id, then normalised title, then first-author surname + year. |
| `doi_correct`, `year_correct`, `title_correct` | for matched pairs where the truth has the field, the extracted field equals it. |
| `resolved_markers` / `extracted_markers` | in-text markers found and linked to at least one reference entry, against the number of `\cite` commands in the source. |
| `body_alignment` | word-level LCS similarity between the extracted page text and the detexed body of the source; a reading-order diagnostic, not an accuracy score. |
| `ms_per_chunk` | pipeline stage time per 20-page chunk, against the 30 ms target. |

The corpus summary aggregates these over the non-failed papers (rates are sums
of counts, not means of per-paper rates) and reports nearest-rank p50 / p95 of
`ms_per_chunk`. A paper whose fetch, source unpacking, ground truth or
extraction failed is listed with status `failed:<reason>` and counted in
`summary.failed`.

## The corpus

`corpus/manifest.json` lists 30 CC-BY 4.0 arXiv papers chosen for diverse
primary categories (at most four per category): 20 in the `dev` split, used to
drive fixes, and 10 in `holdout`, used only to check that fixes generalise.
Papers are **not** committed; `tpe corpus fetch` downloads the PDF and the
e-print into a cache directory and verifies SHA-256 digests once they are
recorded in the manifest (`--update-manifest`).

## Running it

Locally:

```sh
cargo build --release
./target/release/tpe corpus fetch --manifest corpus/manifest.json --cache .corpus-cache --split dev
./target/release/tpe eval --manifest corpus/manifest.json --cache .corpus-cache --out eval-out --split dev
cat eval-out/report.md
```

`corpus fetch` prints one line per item (id, PDF digest prefix, whether a
LaTeX source was found, `cached` or `downloaded`) and exits non-zero if any
item could not be fetched. `eval` prints one line per paper and the summary as
`key: value` lines; it exits 0 however poor the metrics are and non-zero only
on I/O failures (unreadable manifest, unwritable output directory, ledger
errors). Add `--offline` to refuse network access and `--db <FILE>` to also
store every extraction result in a ledger for inspection with `tpe show`.

In CI the `Eval` workflow (`.github/workflows/eval.yml`) runs daily and on
demand on `ubuntu-24.04-arm` and `macos-15`, caches `.corpus-cache` keyed on
the manifest, appends `report.md` to the job summary and uploads `eval-out` as
an artifact. It is deliberately not part of the required `ci` check.

## Reading the report

Look at `ref_count_exact_rate` first: every miss is a segmentation defect that
the exact protocol would reject outright. Then the unmatched truth keys per
paper (capped at 10 in the Markdown) point at the entries the parser lost.
Field accuracies isolate the entry parser from segmentation. Marker
resolution below the `\cite` count usually means a citation style the marker
finder does not know yet. Treat `body_alignment` as a smoke test for reading
order on two-column pages.

## Citation-marker metric

`marker_recall` is the share of cited references recovered: the sum over
markers of resolved targets, divided by the number of keys the source cites
(`cited_keys`, duplicates kept), capped at 1. It does not count marker groups,
so IEEE `[17], [18]` (two groups) and `\cite{a,b}` (one command) score the
same. The older resolved-markers over cite-commands ratio is still reported
as `marker_command_ratio`, uncapped, as a diagnostic.
