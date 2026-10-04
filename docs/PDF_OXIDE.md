# PDF Oxide native extraction adapter

Build with `cargo build --features pdf-oxide`. The compiled backend is
`pdf-oxide`, available through the ordinary Rust extraction interface and
supervised CLI:

```sh
tpe extract paper.pdf --backend pdf-oxide --db extraction.sqlite --json
```

The adapter pins `pdf_oxide` 0.3.78 with default features disabled. It enables
no OCR, rendering, system fonts, models, external programs, or network access.
The source is opened from the immutable bytes supplied by the engine.
`office_oxide` is constrained to 0.1.9 because the otherwise selected 0.1.13
adds `DocumentIR.defined_names`, breaking PDF Oxide 0.3.78's constructor with
Rust error E0063. Both packages have crates.io checksums in Cargo.lock.

## Contract

- `extract_chars` supplies actual character positions, font names and sizes.
  The adapter retains U+FFFD and does not mutate the upstream global switch
  which otherwise filters replacement glyphs from high-level spans.
- Page dimensions come from validated, inherited MediaBox coordinates;
  rotation is preserved. Invalid geometry produces an error or an absent
  glyph box, never an invented rectangle.
- Native engine reading order assembles the positioned characters. This
  integration makes no claim to expose PDF Oxide's separate structure-tree
  reading-order strategies.
- URI annotations are read through PDF Oxide's object API, including indirect
  Annots, actions, targets and rectangles. `PageText.links` retains the action
  target rather than the displayed label. Reference resolution is bounded to
  eight steps and annotation processing to 4,096 entries per page. The adapter
  neither follows URLs nor executes annotation actions.
- Backend identity records the exact version, character API, link path and
  completeness policy. Structured upstream diagnostics are retained.

Every extracted page remains **Partial**. Source inspection found that this
version's character path can return an empty vector after a content-decoding
error and can continue after font-loading errors without a structured warning.
Absence of warnings therefore does not establish completeness. Partial status
survives page/chunk/job results, JSON and the ledger; the CLI returns nonzero
while preserving its usable output. This adapter is an independent candidate,
not a blanket replacement for a verified native pass.

## Measured value and limits

The same unoptimized feature-enabled binary evaluated the five previously
problematic, checksum-pinned arXiv papers in the recorded evidence. These are
diagnostic timings, not release-performance benchmarks.

| Measurement | lopdf | PDF Oxide |
| --- | ---: | ---: |
| References matched | 207/207 | 207/207 |
| Reference titles correct | 205/207 | 206/207 |
| Mean body alignment | 0.952 | 0.894 |
| Body word precision | 0.931 | 0.831 |
| Paper author recall | 1.000 | 0.533 |
| Paper title accuracy | 0.800 | 0.600 |
| Paper status | 5 Partial | 5 Partial |

The title improvement occurred on `arxiv:2601.09974` (33/35 to 34/35).
The lower body and metadata scores are concrete reasons to compare evidence
at the appropriate field rather than automatically choosing the longer output.
DOI accuracy against printed DOI truth was 100% for both; the lower 48.6%
accuracy against all source-record DOIs includes identifiers absent from the
printed PDFs and is not a claim of missing printed targets.

A synthetic marked-content fixture proves one separate capability: ASCII
`/ActualText` containing `Recovered text` is recovered while lopdf returns
the painted `X`. That result does not generalize to Unicode ActualText:
a second valid fixture with `/ActualText <FEFF03B1>` should produce `α`, but
PDF Oxide's character API produces `Î±` by remapping UTF-8 bytes through
WinAnsi. Its result remains Partial. Rendering and OCR were not used to
conceal that limitation.

Six focused integration regressions cover immutable input, real inherited
geometry/font data, out-of-range pages, the ASCII ActualText comparison,
replacement-glyph preservation, actual URI targets with indirect rectangles,
silent content-failure status propagation and supervised CLI execution.
The combined feature build passed 730 library tests (six ignored diagnostic
tests) and all six integration tests. Strict Clippy passed for all targets.
The repository's Python checks passed, including 92 tests and dependency audit.
The full Rust suite was also attempted: the existing `cli_containment` suite
passed 14 tests and failed three process-observation tests with "worker did not
become observable"; these failures were not classified as extraction results.
Swift and CMake checks could not run because those commands were unavailable.

Machine-readable inputs, dependency checksums, per-paper results and synthetic
observations are in
[`analysis/pdf-oxide-five-paper-2026-10-03.json`](analysis/pdf-oxide-five-paper-2026-10-03.json).

Upstream references:

- [Versioned crate and source](https://docs.rs/crate/pdf_oxide/0.3.78)
- [PDF Oxide extraction API](https://pdf.oxide.fyi/rust/docs/extraction/text)
- [Official repository](https://github.com/yfedoseev/pdf_oxide)

The relevant inspected paths are `src/document.rs` (`extract_chars_impl`,
`from_bytes`, structured warnings), `src/extractors/text.rs` (ActualText and
replacement glyphs), and `src/annotations.rs` (high-level annotation behavior).
