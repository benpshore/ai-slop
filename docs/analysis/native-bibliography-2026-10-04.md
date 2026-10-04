# Native bibliography: current-source reproduction

Tracked in [issue #190](https://github.com/benpshore/pdftextract/issues/190)
under [engine epic #181](https://github.com/benpshore/pdftextract/issues/181).
This is a bounded investigation of eight recorded backend/paper failures.
It does not replace the 60-paper native regression corpus or certify field accuracy.

PDF Oxide defects are a separate workstream. The browser's `pdf-oxide-wasm`
0.3.77, optional native `pdf_oxide` 0.3.78 and TPE's fallback policy are distinct.
The recorded symbol-substitution, column-order and Unicode ActualText problems
must retain their runtime/API attribution; see [PDF_OXIDE.md](../PDF_OXIDE.md).
These bibliography changes neither repair browser Oxide nor establish
multi-engine fusion. No default-engine switch or repeated broad
Oxide/Inspector evaluation is part of this work.

## Source reconciliation

Main is `e27e1fb`. The separate native repair stack ends at PR #174,
`c484b73a6882431a12d8a061a307491fbc5e02cc`; PR #175 is
`baafb472873750e7e84d32c3c8e6fa7a680e37db`. Both branches remain untouched.
[Draft #191](https://github.com/benpshore/pdftextract/pull/191) combines them
without conflicts at `e9e41ceb308356c98c810f489cd4a0f1dfd11da9`, tree
`1a22fdc77d900b1e2dceba2509293cd416119bdb`. Focused diagnostics and repairs
stack on that combined native base; no web files change.

Two authorized artifacts were independently inspected:

- PR #174 [run 37168134953](https://github.com/benpshore/pdftextract/actions/runs/37168134953),
  artifact `11290813136`, SHA-256
  `83509461a8376801f444dd0a8a5fff76dec39c76d2ba90b4a02938f35a1f7084`.
  Its exact source is `c484b73`.
- PR #175 [run 37167782132](https://github.com/benpshore/pdftextract/actions/runs/37167782132),
  artifact `11290528450`, SHA-256
  `b4025bce44993f4368f0f65e00c484568560ea66746f5cf5e5106729675c6b75`.
  Its tested merge `35f96747b68afe3401a84ca27ea91f72797e68d9` has exactly the
  same tree as PR #175: `19a0ed2921f74455def912be3e426aff8fe4f372`.

Each artifact retains four backends, each with 60 unique papers, 1,747 pages,
and 60 dumps. The companion [JSON](native-bibliography-2026-10-04.json)
retains report/provenance hashes, input pins and all scoped outcomes. Counts
were cross-checked against truth, extracted entries and one-to-one match rows.

## Which failures remain

Counts below are **truth / extracted / matched**. The six resolved rows are
existing PR #175 improvements; this workstream does not claim to implement them.

| Backend | Paper | PR #174 | Current PR #175 | Current extraction status |
| --- | --- | --- | --- | --- |
| PDFium | 2502.00857 | 29 / 4 / 0 | 29 / 4 / 0 | Partial |
| Docling text | 2503.15734 | 33 / 0 / 0 | 33 / 33 / 33 | Partial |
| Docling text | 2309.10334 | 36 / 0 / 0 | 36 / 36 / 36 | Partial |
| Docling text | 2503.13415 | 341 / 0 / 0 | 341 / 341 / 341 | Partial |
| Docling text | 2506.23487 | 28 / 0 / 0 | 28 / 28 / 28 | Partial |
| Docling text | 2507.14211 | 54 / 0 / 0 | 54 / 54 / 54 | Partial |
| Docling text | 2602.16061 | 54 / 0 / 0 | 54 / 54 / 54 | Partial |
| Full Docling | 2309.10334 | 36 / 1 / 0 | 36 / 1 / 0 | Partial |

An additional current-source failure is Docling text **2502.00857: 29 / 4 / 0**,
with **Complete** extraction status. It is tracked as a ninth diagnostic row,
not substituted into the original eight-row denominator. This is direct
evidence that Complete extraction does not establish bibliography accuracy.

PR #175 emits retained per-page word cells for Docling text, replacing the old
node-based projection. Its six recovered lists match all 546 truth entries;
that is matching coverage, not exact transcription, title/author accuracy, or
evidence that the whole document is complete.

## Demonstrated observations and evidence limits

The retained PDFium and current Docling text dumps for 2502.00857 place the
References heading before ethics/limitations prose, then stop entries at
Acknowledgments while the actual author-year bibliography follows. The full
Docling 2309.10334 dump interleaves appendix prose inside reference 1 and stops
at an appendix heading. Existing dumps retain text and roles but omit raw
backend span geometry and upstream nodes. They demonstrate bad output; they
alone cannot locate the original layout error.

All seven exact pinned PDF URLs currently return local proxy CONNECT 403.
In particular, 2502.00857v2 is pinned to
`e50212a32e349f5528780dbb39d023acc9daecdd281f88db79abc1d2517d33b5`.
No access restriction is bypassed and no alternative PDF is substituted.
The focused hosted diagnostic uses the existing authorized corpus fetch and
native runtime setup to run the requested checks. Its artifacts contain
derived spans, nodes, ordered lines, scores and failures, not PDF/source bytes.
Acquisition failures remain failures in the fixed nine-case accounting.

`native/bibliography_evidence.py` and `examples/bibliography_evidence.rs`
capture original backend spans, the actual final pipeline output, a separately
identified order-only projection, and raw full-Docling nodes. The latter are
from a separate upstream diagnostic pass with the same model configuration;
they are not silently substituted into scored results. The normal 60-paper
Native workflow and its strict historical pins remain unchanged.

## Initial validation

The combined base passed Rust formatting, strict no-feature workspace Clippy,
full workspace tests (including all 18 containment tests), Ruff and all 164
Python tests locally. Dependency audit was blocked by the OSV proxy connection.
Swift/CMake/CTest are unavailable locally; the Foundation targets need macOS.
Full Docling compilation was blocked by the pinned ONNX Runtime CDN returning
403. These limitations are not passes. Hosted checks and focused repair results
must be recorded separately at their actual final heads.
