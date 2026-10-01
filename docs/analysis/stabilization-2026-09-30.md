# Stabilization verification — 2026-09-30 UTC

## Scope and inputs

Compared the forensic baseline `28c0deaad9be5886ad5accb23fce094f50dc165e`,
main `8fb4dd1668991045823027866e1ca804e514367c`, the final Form-resource fix
(PR #119), and resource diagnostics. No wholesale revert, GUI rewrite,
repository-setting change or automatic merge was performed.

All 70 PDFs and their source archives in `corpus/manifest.json` were fetched
and verified against the existing SHA-256 values. The manifest was not edited.
The engine downloader could not connect in this executor; curl through its
normal configured proxy fetched the same pinned URLs successfully, then the
engine evaluated the populated cache with `--offline`.

Each release binary was built from its specified source. Evaluation used:

```sh
tpe eval --manifest corpus/manifest.json --cache CORPUS_CACHE \
  --split all --offline --out OUTPUT --dump-dir OUTPUT/dumps
```

Comparison checked every field in each of the 70 JSON dumps except `timings`.
For the diagnostics change only `warnings` and `page_warnings` were additionally
excluded and then compared separately. Dumps cover reference entries/matches,
markers, body text, tagged lines, roles, metadata, truth and warnings; they are
not a complete comparison of every raw span/figure geometry field.

## Results

- **Morning batch:** baseline versus current main has zero differences in
  non-timing dump fields across all 70 papers.
- **Final Form fix:** current main versus PR #119 also has zero differences.
- **Diagnostics:** zero non-warning output differences. Seven pages across
  five papers now report existing vector-region coalescing above 2,000 boxes:
  `2503.04404` pages 11/12, `2506.23487` page 17, `2603.05575` page 25,
  `2501.17300` page 15, `2601.09974` pages 17/18.
- The corpus does not trigger the new raster, vertical, superscript-window or
  script-budget warnings. Adversarial tests exercise those cases.

All four evaluations completed 70 papers with no failed papers. Shared
aggregate diagnostics: exact reference-count rate 100%, reference recall and
precision 0.9997802, marker recall 0.9910628, body alignment 0.95213324. These
are existing diagnostic definitions, not 99% entirely error-free chunks.

| Release build, this Linux x86-64 executor | p50 ms / nominal chunk | p95 ms / nominal chunk |
| --- | ---: | ---: |
| Pre-morning baseline | 27.47 | 73.18 |
| Current main | 28.10 | 71.94 |
| Final Form fix | 27.84 | 69.96 |
| Diagnostics | 28.71 | 71.00 |

Single runs on a shared executor: do not infer a speed improvement from these
small differences. Durable writes are excluded; this is not M1 validation.

## A regression caught during repair

The initial Form patch's 16,384-call limit rejected pages 17 and 18 of
`2601.09974`. A corpus-wide interpreter diagnostic found a legitimate maximum
of **64,554 Form calls** on page 17, with **116,183,200 bytes of execution
charge**. Maximum page decode charge was **6,628,971 bytes**, on page 31 of
`2501.17300`. The initial patch was held in draft, then corrected to 131,072
calls and 256 MiB execution charge before review. Full extraction parity was
rerun and passed. The first candidate's degradation is not hidden by the final
aggregate scores.

Synthetic release reuse diagnostic: 1,000 identical 100-span Form pages took
24.6 ms with persistent bounded caching and 83.1 ms clearing the cache between
pages. Every PageText matched. See `FORM_RESOURCES.md` for reproducible tests.

## Other verification and remaining boundaries

Rust workspace tests and Clippy pass for the focused patches; Python formatting,
lint, 64 tests and dependency audit passed on the unchanged Python tree.
Publication tests cover staging/open errors, failure after the first link,
second-file races, eight concurrent publishers, output ownership and an actual
SQLite commit failure restoring a prior run. `PUBLICATION.md` distinguishes
handled-error rollback from crash recovery; no distributed filesystem/SQLite
transaction is claimed.

The font audit found unresolved unbounded font-cache retention and unbounded
ToUnicode decode calls; details are in `FORM_RESOURCES.md`. The 70-paper academic
corpus does not establish safety/accuracy for arbitrary mixed-media or extreme
inputs. Small macOS engine CI remains a separate decision: this repair preserves
the current workflows and the previous instruction to stop macOS engine CI.
No native macOS runtime, Swift or Foundation/CMake checks ran in this Linux
executor. The existing App workflow remains the available macOS app check.
