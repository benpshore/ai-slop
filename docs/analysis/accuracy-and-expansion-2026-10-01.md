# Accuracy and expansion follow-up, 2026-10-01

## Scope and reproducibility

This local diagnostic uses the pinned PMC manifest at main `c8eeaaa`, excluding
only **PMC9866638**: its current XML fails the manifest MD5 check. The manifest
and expected checksum were not changed to accept different truth. All PDF/XML
files for the remaining 199 articles passed their pinned checksums; all 199 JATS
reference lists parsed. They contain 9,550 entries from 138 journals and 48
publishers, with no author manuscripts in this subset.

Original manifest SHA-256:
`61fd7d4bbd088cf653e0e6380e77f44b840b04f0137cd8d9cca807e576ab2bb5`.
The excluded XML was expected to have MD5
`01f4938a135da25e29d8ea722024b73e`; the downloaded bytes had
`707148ab259c325c84acd030be5a5c8b`.

Commands use `scripts/pmc_sample.py --fetch` and `--list` with the filtered
manifest, `tpe bibliography --backend lopdf`, and
`scripts/pmc_bib_eval.py --manifest ... --cache ... --bibliography ... --out ...`.
The forward comparison uses `tpe extract --backend lopdf --json -j 4 --db ...`
and the scorer's `--extract` option. Both runs use identical pinned bytes and
four workers. Builds are debug Rust 1.98.1 on a shared x86-64 Linux executor;
timings are not release-performance evidence. No registry resolution is enabled.
The baseline binary includes the separate Crossref/PubMed changes, but its
extraction/segmentation code is identical to `c8eeaaa`.

## Detached reference-number columns

On pages such as PMC10045480 page 11, reading order emits the entire `17.` to
`40.` label column before the entry text. The first bibliography page often has
interleaved labels, so the parser selects dot-numbered style, then truncates or
mis-segments the later detached pages.

The fix reconnects standalone `n.` and `n)` labels to nearby text on the same
page and baseline before style detection and end-heading checks. It keeps bare
integers and unplaced labels unchanged. It sorts an index by baseline and uses
binary searches; each label examines at most 1,024 row candidates, so crowded
geometry cannot turn matching into an unbounded all-pairs scan. The index and
flags use linear auxiliary storage. Existing bracket-label handling remains.

| Backward bibliography metric | Before | After |
| --- | ---: | ---: |
| Reference lists found | 172/199 | 179/199 |
| Exact reference count | 126/199 (63.3%) | 158/199 (79.4%) |
| MDPI exact reference count | 1/29 | 28/29 |
| Matched reference entries | 6,658 | 8,313 |
| Unmatched extracted entries | 132 | 118 |
| Pages scanned | 741 | 653 |

No previously exact paper loses its exact count, and no paper's absolute count
error increases. Matching/count agreement is not proof of correct transcription.
For example, PMC10045480 now has all 55 entries instead of 16; PMC10222613 has
140 instead of 6. PMC3757428 still has 99 of 134 and PMC12138822 has 18 of 20.
Twenty lists remain unfound. Strict title agreement is 6,083/7,840 matched
eligible entries (77.6%); replacement characters remain in 84 entries.

The forward full-document path also completed all 199 PDFs. Lists found remain
182/199; exact reference counts improve from 127/199 to 157/199, with no loss
of a previously exact count. This independently exercises the shared segmentation
fix through the normal extraction pipeline.

The DOI field score covers printed DOI extraction, not `doi_link` annotations or
resolved registry records. JATS often includes DOIs absent from the printed PDF.
Its denominator also grows as more references are recovered. It must not be
presented as an end-to-end DOI/PMID resolution success rate.

## Registry verification

The separate resolver PRs preserve balanced biomedical DOI suffixes, reject
ambiguous Crossref query candidates, and support exact PMID/PMCID lookup plus
DOI enrichment through Europe PMC. Returned identifiers and printed metadata
must agree; missing DOIs are represented as null. Ledger v4 data migrates to
schema 5 without rewriting historical results; CSV refuses incompatible headers.

Both live tests passed in [Actions run 36809248078](https://github.com/benpshore/pdftextract/actions/runs/36809248078):
Crossref DOI lookup and Europe PMC PMID, PMCID and DOI routes identify Piwowar's
2018 article as `10.7717/peerj.4375`, `29456894`, and `PMC5815332`. Offline tests
cover conflicts, namespace mistakes, missing DOI, ambiguity and transport errors.
This is a live smoke check, not a corpus-wide registry accuracy estimate.
Paper-level PubMed resolution and PMID links present only in PDF annotations
remain outside that patch.

## Stack, heap and remaining expansion

The caption `box_kinds` frontier change does remove its former cubic propagation:
each region becomes a source once and scans all regions, for O(R² + R·C) work
with R region boxes and C captions. Frontier vectors are on the heap. Their
location does not change the number of comparisons.

Nested Form execution still admits cubic work even with shallow recursion.
A synthetic one-page PDF calls Form A once; A contains N calls to B; B contains
N calls to C; C contains N `q Q` pairs. Only three Form levels are needed, below
the default depth limit of eight. The input grows linearly with N, but executes
N³ save/restore pairs. The graphics-state `Vec` never needs more than one saved
state in C, and decoded programs are cached.

| N | PDF bytes | Executed q/Q pairs | Median CLI elapsed, 3 runs |
| ---: | ---: | ---: | ---: |
| 20 | 1,160 | 8,000 | 4.0 ms |
| 40 | 1,482 | 64,000 | 11.2 ms |
| 80 | 2,122 | 512,000 | 56.4 ms |
| 160 | 3,402 | 4,096,000 | 387.2 ms |

Child peak RSS measured with `wait4` stayed between 32,428 and 32,984 KiB.
These debug/shared-host timings illustrate the reproduced path, not throughput
targets. Moving recursive frames to an explicit heap stack would not reduce
the execution count. Per-page invocation and execution budgets are needed.

Other remaining limits at `c8eeaaa`:

- Form `get_plain_content()` inflates before the cache budget check. The 64 MiB
  estimate counts payload lengths, not all capacities/allocations, and an empty
  program has zero charge, allowing unlimited cache entries. An uncached Form
  is decoded again on each use.
- Font cache cardinality is unbounded. The individual embedded-font program
  limit does not establish a total font-memory bound.
- `scan_window` clones and analyzes the growing page suffix on every iteration.
  Even linear analysis would accumulate quadratic work on long unsuccessful
  scans; source pages and each deep clone also coexist on the heap.

[PR #119](https://github.com/benpshore/pdftextract/pull/119) addresses Form decode,
cache and execution budgets, but remains unmerged and conflicts with current
main. [PR #96](https://github.com/benpshore/pdftextract/pull/96) proposes geometric
suffix checkpoints; its scheduling test alone does not prove unchanged section
selection. Neither should be described as an already deployed or complete
process-memory bound. Validate their integration against both adversarial
inputs and the corpus before merging.
