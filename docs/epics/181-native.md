# [Native engine][Epic] Complementary parser routing, evidence fusion, and performance qualification

GitHub: https://github.com/benpshore/pdftextract/issues/181
Created: 2026-10-04T01:11:39Z
Status at export: open

Recorded 2026-10-04T01:09:25Z (2026-10-03 19:09:25 America/Denver). This epic is part of the user's requested Dot/Codex handoff for **benpshore/pdftextract**. Implementation is tracked in [PR #175](https://github.com/benpshore/pdftextract/pull/175); the core repair baseline is [merged PR #173](https://github.com/benpshore/pdftextract/pull/173), main commit e27e1fb28a5b40dbca7f517c6fe396e6f11ed4ec.

Current distinction: PR #175's published head was 77c58ec89a1c9c07425b77dec90ab00316aa07be at this record's start; follow-up fixes are being validated for the next revision. The live Site's first alpha uses upstream PDF Oxide WASM, not the complete native Rust engine. See the final handoff/source manifest for the exact subsequent publication revision.

## Implemented integration
PR #173 merged engine/font/Form/Unicode/resource/DOI/corpus repairs. PR #175 adds optional PDF Oxide, pdf-extract CFF recovery, LiteParse geometry projection, MuPDF and Poppler runtime providers. Real local MuPDF1.26.11 / Poppler26.09.0 providers were built/tested; raw embedded URI targets and geometry provenance are retained. Rust uses explicit C/FFI providers where appropriate; no claim that these C/C++ engines were fully ported to Rust.

`TPE_AUTO_NATIVE_FALLBACK=1` provides a conservative opt-in whole-pass cascade with source/page/status/text/link/figure guards. It is not per-element multi-engine fusion. `docs/EXTRACTION_ROUTING_REVIEW.md` documents capability gaps and promotion criteria.

## Evidence / related work
- 70-paper arXiv and 200-paper pinned PMC repair evidence is documented in the handoff; resource-limited/Partial cases remain visible.
- Combined optional-feature Clippy passes. Actual native-provider integration tests pass. Three local containment tests cannot observe child PIDs in this executor; they remain required in hosted CI, not weakened.
- Per-page bibliography annotation union and DOI-target preservation fix an all-or-nothing fallback gap.
- Related existing tracking: #145, #146, #147, #152, #159 and #174. Avoid duplicating or blindly merging overlapping historical PRs.

## Remaining acceptance
- [ ] Per-page/per-element provenance and conflict arbitration with independently labeled text/order/table/figure/link truth.
- [ ] Same-input release-build accuracy/latency/RSS comparison across all providers, including mixed scans and pathological but valid sources.
- [ ] Refresh upstream version pins based on reproducible tested builds, not an unsupported 'latest' claim; validate licensed runtime distribution strategy.
- [ ] Crash-isolated per-provider attempts that can preserve a previously successful baseline when a later native provider dies.
- [ ] Exact-final-head Linux/macOS/ARM64 runtime gates and reproducible native provider build jobs.

The Site does not yet call these native providers as a hosted processing service.
