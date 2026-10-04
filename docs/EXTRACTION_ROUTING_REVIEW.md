# Independent extraction and routing review

Reviewed 2026-10-04; initial snapshot `543fd29223bbb06f938ee24077b8b0f9d2593faa`,
updated through root `a5fbd4d` for native providers and fallback routing, plus
the active integration worktrees. This is a capability and policy review,
not an assertion that every integration has merged or met a production SLO.

**Recommendation:** retain the bounded native pass as the baseline; spend extra
work on a diagnosed failure, preserving its original evidence. Different tools
can contribute font recovery, spatial layout, reference fields, and OCR without
being interchangeable whole-document extractors. More characters, HTTP 200, a
successful process, or a plausible rendering does not prove exact extraction.

Evidence labels below are **M** (locally measured/reproduced), **C** (inspected
adapter contract/code), **U** (upstream claim/API), and **P** (proposal/inference).
An integration agent's reported runtime test is identified as such; it is not a
new independent corpus run. No new broad benchmark was started for this review.

## Capability and gap matrix

| Component and inspected pin | Distinct contribution | Evidence and limits | Deployment / routing consequence |
| --- | --- | --- | --- |
| Engine `lopdf` 0.45 | Baseline object/font interpretation, positioned text, image and annotation evidence | **M:** same-binary five-paper control recovered 207/207 references, 205/207 reference titles; mean body alignment 0.952. All five remained Partial. **C:** bounded executed-font work and explicit mapping/resource diagnostics. | Pure Rust baseline; do not infer browser build support for the whole application from its parser language. Preserve source bytes and page coverage. |
| `pdf-extract` 0.12.1 + `cff-parser` 0.2.0 helper | Embedded simple Type1C/CFF code-to-glyph recovery when PDF mappings are absent | **M/C:** owned fixture recovers alpha/beta/gamma with original geometry; two probes and direct CFF cross-check reject unsupported mappings. Not a general extra PDF pass. Explicit Encoding/ToUnicode wins; no Type0/OpenType recovery. | Opt-in pure Rust helper inside the existing font budget. 256 KiB encoded/decoded program cap; only selected fonts enter controlled synthetic PDFs. |
| PDFium, provisioned Chromium 8066 | Independent native font/text interpretation and rendering API | **M/C:** fixtures distinguish normal text, existing invisible OCR, partial CMaps, and zero-Unicode omissions; mapping uncertainty stays Partial. **C:** native character diagnostics supplement object text. This is not proof that every mapping is correct. | Explicit native library and bounded worker. Keep one binding's initialization/mutex ownership; parallel throughput comes from bounded processes, not concurrent uncoordinated wrappers. |
| PDF Oxide 0.3.78 + office_oxide 0.1.9 | Independent character/font parser; ASCII ActualText recovery; raw URI annotation extraction | **M:** five papers/266 pages: 207/207 references, 206/207 titles, body alignment 0.894; author recall 0.533 versus baseline 1.000. ASCII ActualText recovers `Recovered text` instead of painted `X`; valid Unicode ActualText alpha became `Î±`. **C:** all pages intentionally Partial because upstream silent fallbacks prevent certification. | Optional parser candidate for a diagnosed field/page disagreement. Do not prefer it globally or by output length. Adapter does not currently export image pixels or its upstream structure-tree ordering. |
| LiteParse 2.15.1 projection | Spatial alignment of existing PDFium text for forms/table-like rows | **C/M:** pure `stages::project` path preserves source character multiset and original spans, boxes, fonts, links and figures; rejects unsupported rotation/geometry/limits with Partial. This is layout evidence, not semantic table correctness. | Opt-in local layout only; does not call LiteParse's second PDFium wrapper, OCR, or network APIs. Current cap: 4,096 spans and 1 MiB source text per page. |
| MuPDF provider, exercised 1.26.11 | Independent native text/font parser, raw PDF geometry and image-region evidence | **M:** configured runtime tests confirm raw URI actions, affine geometry and output-cap recovery; reviewer reproduced malformed-content warnings. The reproduced `fz_load_links` URI rewriting/GoTo issue was repaired by reading raw PDF URI actions. No paired corpus superiority established. | Separate optional trusted provider/runtime, private session context, C exception boundary. Two-file fingerprint does not capture all system dependencies. User supplies a suitably licensed runtime. Current Rust loader is native, not a browser/WASM adapter. |
| Poppler provider 26.09.0 | Independent word/font extraction and raw URI annotation evidence with crop/rotation transforms | **M:** exact-release native build and configured runtime tests cover passwords, malformed mappings, raw URI targets and affine geometry. **C:** shared global API calls serialized; image extraction and document metadata are not supplied by this provider. No paired corpus superiority established. | Optional C++ provider under its applicable license; no bundled engine. Font files, poppler-data and system dependencies remain outside provider/runtime fingerprints. Tested Linux x64; other targets unverified. |
| docling.rs adapter, `docling-pdf/core` 1.69.2 | Text-layer assembly; optional learned layout/OCR and region-level structure | **C:** current adapter caches a conversion on first page access, exposes region rather than glyph boxes, and no fonts; normalizes text and may move a paragraph across a page boundary. Tables are disabled in the default full configuration. New review/repair work is active; no fresh comparative result is asserted here. | Gate model work behind scan/layout evidence and explicit configuration. Current CLI containment does not admit this OCR-capable backend. Pinned native/model policy must be checked independently of upstream HEAD. |
| GROBID, server integration under development | Structured reference/header fields and citation relationships | **U:** API accepts isolated raw citations or PDFs and can return TEI with selected structural coordinates. This review has no measured GROBID result on the pinned corpus. TEI fields are candidates, not original glyph evidence. | Opt-in configured service. Prefer local/self-hosted operation and raw-reference requests when sufficient. Never upload a PDF or extracted text to an external service automatically. |

For measurements, see [PDF Oxide evidence](PDF_OXIDE.md), its
[paired report](analysis/pdf-oxide-five-paper-2026-10-03.json),
[LiteParse contract](LITEPARSE_LAYOUT.md), and [native evaluation definitions](NATIVE_EVIDENCE.md).
The CFF and provider contracts were reviewed in their integration worktrees;
their final merge commits should be attached to any release evaluation.

## Native, browser and server are different deployment lanes

**C:** the current Rust application uses native libraries, OS worker limits,
SQLite and filesystem publication. The C ABI is a native embedding boundary;
it does not establish browser support. The pure Rust parser/helper portions are
reasonable WASM candidates, but this repository has not demonstrated feature
selection, compilation, memory limits or output parity for such a build.

**U:** current official docling.rs HEAD advertises a browser converter, including
an ONNX Runtime Web model path, and a newer pure Rust rendering stack with
optional PDFium. Those claims do not describe the pinned 1.69.2 adapter.
LiteParse HEAD also exposes a WASM package; this integration uses only one
projection stage. Neither upstream browser package validates our native loader,
model pins, schema, cancellation or extraction status in a browser.

**P:** keep the browser lane as a separately tested artifact with its own runtime,
model and feature identity. Browser cancellation and memory budgets need their
own implementation; they cannot inherit `RLIMIT_AS` semantics. A server fallback
must be a visible configured operation, not a consequence of a missing WASM
feature. Current GROBID documentation describes an HTTP service; no local WASM
GROBID implementation is established by this review.

## Proposed routing by element and page

This table is a **P** design target. The current auto router replaces whole passes
over the requested page selection, retaining one backend identity. It does not
implement provenance-preserving element fusion. The implemented native fallback
extension is opt-in and caps the chain at lopdf, PDFium, MuPDF, Poppler; see
[native fallback policy](NATIVE_FALLBACK.md). Independent read-only review found
no blocking issue in its source/page/status, URI/figure retention, attempt-limit
or policy-identity guards. Forty-eight focused tests were reported passing.

| Evidence / requested element | First action | Escalation and stopping rule |
| --- | --- | --- |
| Text maps cleanly with plausible geometry | Keep native spans and raw text | No alternative parser merely because another is installed. |
| Selected simple CFF font lacks PDF mapping | Bounded CFF helper, if enabled | Keep unresolved byte positions; then one independent native parser. OCR cannot certify unknown native mappings. |
| Unresolved glyphs, broken content, or suspicious missing text | PDFium candidate on affected selected pages | Optional MuPDF then Poppler, at most once each. Retain failed attempts and original evidence. Stop on budget/coverage regression; do not erase usable output. |
| ActualText or reference-title disagreement | Targeted Oxide candidate | Compare the specific span/field against preserved source evidence. Its known Unicode ActualText error forbids blanket preference. |
| Aligned form/table text needs spatial presentation | LiteParse projection of existing spans | Preserve raw spans; reject changed character coverage. Grid alignment alone does not establish cell semantics or reading order. |
| Low text under a large raster | Preserve image region; offer configured OCR/layout pass | One bounded docling window. Keep OCR hypotheses separate from any existing text. No automatic full-document OCR retry. |
| Citation fields incomplete but raw reference retained | Local parser, URI DOI, then printed DOI | Optional GROBID raw-citation request; preserve parsed fields as alternatives with raw-string identity. Full-document GROBID only for an explicit structural task. |
| DOI link exists behind a non-DOI label | Keep the URI action and true annotation rectangle | Attach by page/geometry and entry evidence; never replace the target with the label. Keep raw target separately from any resolution/normalization. |
| Figures or tables requested | Preserve native region/object evidence first | Image decoding/export and learned table recognition get distinct limits and provenance; a box is not exported pixels, and an OCR caption is not source text. |

The existing hash, source-size, exact page-coverage, status-consistency and
non-whitespace-count checks are useful admission guards. **They do not detect
equally long wrong text, word-order corruption, lost URLs or missing images.**
The opt-in native policy now checks exact annotation and figure evidence
multisets; different geometric rounding can conservatively refuse replacement. A
mapping warning from the original engine remains in route history even when a
candidate has no corresponding warning. `Complete` currently means no detected
extraction gap, not independently adjudicated character-perfect output.

## Proposed work budget

These are initial **P** admission limits to calibrate, not achieved latency or
quality promises. Existing worker limits remain authoritative.

| Lane | Initial budget | On exhaustion |
| --- | --- | --- |
| Default local extraction | One baseline pass; selected-font helper within existing font budget | Retain Partial with explicit reason. |
| Opt-in native recovery | At most three alternate passes; never revisit an engine; stop starting alternatives with less than 10% of document deadline left | Retain best eligible completed pass and all route reasons. |
| Local layout | At most one projection per selected page; use current LiteParse bounds | Preserve ordinary layout and mark the refusal. |
| Model work | Zero by default; explicit request/configuration permits one window of at most 20 pages, initially one model worker; proposed raster cap 20 million pixels/page | Return preserved native evidence with unfinished pages recorded; no silent resolution reduction or full-document retry. |
| GROBID reference fields | Zero by default; proposed batches at most 32 references/64 KiB, response at most 2 MiB, one in-flight request and 15 s within the job deadline | Keep original parsed references; a timeout or 204 is not an empty successful bibliography. |
| GROBID PDF structure | Explicit task only; at most one bounded upload of the immutable snapshot | No automatic retry to another host or endpoint. |

The current CLI defaults are 60 s/document, 64 MiB source and output capture,
one extraction worker, and 1,024 MiB address-space growth allowance above startup
mappings. This is not a 1 GiB RSS guarantee. Multiple workers have separate
allowances. Native provider JSON is capped at 16 MiB/page and one million
characters. Model sessions must be accounted for separately: a warm-process
policy must not multiply model copies implicitly. See [worker limits](CLI-WORKERS.md).

For GROBID, explicitly disable header/citation/funder consolidation unless a
separate enrichment operation is authorized. Consolidation may contact external
bibliographic services even when the GROBID endpoint is local. Record endpoint
identity, server version/revision and request settings; disallow redirects or
host changes that would broaden document transfer. Fulltext TEI is not evidence
that every page or glyph was extracted.

## Preserve evidence before merging outputs

**P:** field/page fusion needs a schema extension, not an invented aggregate
backend identity. Record source hash, page, original span/object or annotation
identity where available, backend/runtime/model versions, configuration digest,
coordinate transform, mapping method, warning history and the selection reason.
Keep source text, normalized text, model hypothesis and bibliographic enrichment
distinguishable. Provider-derived boxes must retain their native granularity.

Raw URI actions, relative targets, URI base context and displayed labels are
separate facts. Do not run URI fetching during extraction. Keep image objects,
region boxes and optional decoded payload hashes separate; deduplication must
not remove repeated page placements. Figure pixels remain outside the current
bounded `extract` CLI's supported export contract.

## Weakest modules and focused next evidence

1. **Cross-backend selection and provenance.** Missing-link and missing-image
   candidate regressions are now covered by the opt-in policy's guards and tests.
   An equal-length wrong-text candidate still exposes what character-count
   admission cannot prove. Element provenance and semantic acceptance remain
   unfinished. A hard native crash ends the shared worker and cannot preserve
   an in-memory baseline; recovery across crashes needs separate supervised
   passes and persisted evidence, beyond this policy.
2. **Docling status and page ownership.** At review, missing converted pages and
   unavailable geometry use ordinary `docling:` warnings, which do not imply
   Partial. The adapter strips formula placeholders and copies no links into
   PageText. Assigned reviewer is checking these paths. Target a two-page
   paragraph, missing page marker, undecoded formula, rotated scan and hidden OCR
   layer; verify both page accounting and retained source evidence.
3. **Annotation coverage.** Bibliography currently uses native links if any page
   contains one, otherwise runs a lopdf fallback. A partially populated set can
   suppress fallback for other pages. Require page-level coverage or an immutable
   annotation side channel; a per-page repair is assigned. Native-normalized
   crop/rotation coordinates and lopdf source coordinates must be reconciled
   before attaching a fallback rectangle to a reference. Keep relative URI + Base, embedded NUL, indirect
   action/rectangle, rotated crop, and internal GoTo as targeted fixtures.
4. **New provider selection.** MuPDF/Poppler runtime smoke tests prove the adapters
   execute; they do not prove corpus advantage. Use the existing five-paper set
   plus one known mapping failure each, record per-field gains/losses, links,
   images, Partial reasons, cold/warm time and peak RSS on the same machine.
5. **Reference recognition versus enrichment.** Compare exact title/author/year
   fields and entry boundaries for native versus GROBID on existing unresolved
   references. Record printed DOI and annotation DOI separately from source-record
   DOI. More resolved Crossref records do not prove the printed reference was
   extracted correctly.
6. **Expensive structure.** Benchmark a small adjudicated set containing one
   merged-cell table, one multicolumn page, one mixed scan/native document and
   one figure-caption case. Score cell topology, reading order, page ownership
   and links, not Markdown appearance. Capture model/runtime pins and real
   provider selection before claiming docling or WASM parity.

Historical `analysis/routing-and-resolution-2026-09-30.md` describes an older
automatic OCR/longer-list policy and old defaults; it is not the current routing
contract. `NATIVE.md` has stale "not pinned" prose although the inspected manifest
contains PDFium/model hashes. ONNX runtime identity and current feature behavior
still need reconciliation. The 30 ms/M1 and 99% exact-chunk goals remain goals;
development timings and five selected difficult papers cannot establish them.

## Primary upstream sources checked

- [PDFium text API](https://pdfium.googlesource.com/pdfium/+/main/public/fpdf_text.h)
  and the versioned local binding/fixture review.
- [PDF Oxide character API](https://pdf.oxide.fyi/rust/docs/extraction/text),
  checked against the actual 0.3.78 source used by this adapter.
- [pdf-extract source](https://github.com/jrmuizel/pdf-extract), checked against
  the helper's pinned 0.12.1 behavior.
- [LiteParse source](https://github.com/run-llama/liteparse); local projection
  contract is pinned separately and does not inherit every HEAD feature.
- [Poppler release source](https://poppler.freedesktop.org/) and the native
  provider's exact-release build/test record.
- [docling.rs current project](https://github.com/docling-project/docling.rs)
  and [PDF conformance](https://github.com/docling-project/docling.rs/blob/master/docs/PDF_CONFORMANCE.md).
  Upstream conformance/performance claims are not this engine's measurements.
- [GROBID service API](https://grobid.readthedocs.io/en/latest/Grobid-service/),
  [coordinates](https://grobid.readthedocs.io/en/latest/Coordinates-in-PDF/),
  [consolidation](https://grobid.readthedocs.io/en/latest/Consolidation/) and
  [service deployment](https://grobid.readthedocs.io/en/latest/Grobid-docker/).

Provider licensing and deployment details remain in their respective integration
documents; this review neither changes those obligations nor establishes a new
distribution permission.
