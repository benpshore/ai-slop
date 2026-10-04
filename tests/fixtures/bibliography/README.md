# Bibliography column-heading fixtures

Tracked by [issue #190](https://github.com/benpshore/pdftextract/issues/190)
under [engine epic #181](https://github.com/benpshore/pdftextract/issues/181).

The two deterministic gzip JSON files retain **pages 8 and 9** of
arXiv:2502.00857v2 from the exact-head
[diagnostic run](https://github.com/benpshore/pdftextract/actions/runs/37170137040),
[artifact 11290459456](https://github.com/benpshore/pdftextract/actions/runs/37170137040/artifacts/11290459456),
commit `0ece133` in [PR #194](https://github.com/benpshore/pdftextract/pull/194).
The artifact ZIP SHA-256 is
`6706dc05f51095c0e0452cc6e3f6aadf2356d8530b580ab104aabbc63a2724aa`.
The pinned source PDF hash is
`e50212a32e349f5528780dbb39d023acc9daecdd281f88db79abc1d2517d33b5`.
No original PDF bytes are included.

Each fixture copies page dimensions, rotation, original spans (including
text, sequence, font, size and boxes), figures and links from `result.json`.
Already-computed lines, text and warnings are empty so the test recomputes
ordering, cleanup, region tags and citations. The 29 truth references are
copied unchanged from `dump.json`; they are existing source-derived truth,
not a newly relabeled baseline. Source case and PDF hash are included in
each JSON envelope. JSON is UTF-8, compact, and gzip-compressed with mtime 0.

| Fixture | Source case | Span counts (pages 8, 9) | Fixture SHA-256 |
| --- | --- | --- | --- |
| `arxiv-2502.00857-pdfium-spans.json.gz` | `case-01-pdfium-arxiv_2502.00857` | 165, 168 | `60b97d5884e81ec96d713bb6e4d2f9d6848a41d3faf75acce5eb5a668b6da2e0` |
| `arxiv-2502.00857-docling-text-spans.json.gz` | `case-09-docling-text-arxiv_2502.00857` | 628, 695 | `caf95faa31a2873d1c0a96c62965c1e42551114ba1e42b858e426ddb1f38b11e` |

Source `result.json` hashes, respectively:
`aeda81d793d7e827381d4f429633050d38d66f3f71891da0e1d1ac40e01c4a36`
and `2a3862280a579812b6d08b282f314328a30441238f3cba203b890480ca1f2d35`.
Source `dump.json` hashes, respectively:
`d93f56b41880524244717f69ce7ea77e57f7e5f68e9d7c1eda6974e50765c0f1`
and `73d36571df0a850d240bf75cd2e7faffbd9f0f2ab359a850426abe98ee6b3f75`.

Both backends place `Ethical Considerations` at x=70.866 and `References`
at x=306.142, with their top edges at approximately y=768. The old ordering
detaches these headings before either column's prose, making ethics text
the start of the reference list. The test requires each heading to remain
with its column, all 29 references to match, no ethics/limitations prose in
the references, and the original spans to remain unchanged.

These are retained-span regressions, not fresh PDF extraction tests. The
PDFium source result remains Partial and the Docling-text source result
Complete; the fix changes neither extraction-status rules nor corpus
denominators. Full-document offline replay is available with:

```sh
cargo run --example bibliography_replay -- ARTIFACT_CASE_DIRECTORY NEW_OUTPUT_DIRECTORY
```

Replay records the input result/dump hashes and original backend identity,
document identity and status as source provenance. It supports PDFium and
Docling-text; full Docling requires its adapter-projected lines and is
rejected. Derived `replay.json` contains pages, references and markers,
with original page warnings separately identified. It emits no new
extraction status, metadata or timings. It does not rerun native decoding
or certify field-level transcription accuracy.
