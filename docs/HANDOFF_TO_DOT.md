# Handoff to Dot: pdftextract engine and private document workspace

Prepared from the shared checkout and agent evidence beginning **2026-10-04 01:03:51 UTC**, with final module reports, issue records and publication evidence reconciled through **2026-10-04 01:20 UTC**. This is an engineering work record and continuation guide, not an export of the complete chat transcript. Earlier message timestamps that are unavailable must not be invented.

## Read this first

The work has two separate runtimes:

1. **Native TPE** is this repository’s Rust extraction pipeline, optional native parsers, bibliography, ledger, CLI containment, and C/native embedding interfaces.
2. **The private Site** is the browser application mirrored under `web/`. Its PDF worker runs the published **`pdf-oxide-wasm` 0.3.77** package. It does **not** run the repaired native TPE pipeline, its native fallbacks, GROBID, or Docling. Writing most of the native engine in Rust does not make the whole application a tested WASM artifact.

Do not report a native fix as deployed browser behavior. Do not report local tests as a live Site deployment. Do not call a result complete because it is nonempty, longer, visually plausible, or returned HTTP 200.

The user wants autonomous implementation and independent specialist review, with actual measurements across varied benign inputs. They explicitly rejected arbitrary file/page limits, hyperlink-only “extraction,” placeholder images, and claiming that PDF tools handle HTML. They want clean article content, actual images, preserved DOI targets, robust imports, clear evidence, and a thorough handoff. The requested/claimed **100 GB account allocation is user-provided context, not a verified Site quota or a demonstrated 100 GB import**.

## Status and publication checkpoint

Recorded **2026-10-04 01:21 UTC**. Publication and validation are separate facts.

| Item | Verified checkpoint |
| --- | --- |
| Repository / repair baseline | [benpshore/pdftextract](https://github.com/benpshore/pdftextract); [PR #173](https://github.com/benpshore/pdftextract/pull/173) merged as `e27e1fb28a5b40dbca7f517c6fe396e6f11ed4ec`. |
| Integration code | [PR #175](https://github.com/benpshore/pdftextract/pull/175), branch `feat/native-toolkit-private-alpha-20261004`; implementation revision `f22b49c0497ed22d37d8661cb6595860693a58bd`, tree `3194addaf873f73487963646ef9e7e527a5fc23c`. This documentation is a subsequent docs-only checkpoint; consult the PR for its current head. |
| Private Site | [TPE private alpha](https://pdftextract-alpha.junkmail-edu228.chatgpt.site), version **2**, published successfully **2026-10-04 01:17:05 UTC**. Source `9f7bc798ca5b3002f77e52e9bb2c7bd2774c5f0b`; deployment `appgdep_6ac1a8ff550c8191819bbb2aaadeca3e`; saved version `appgprj_6ac197d389d48191baa66fe4f6bbb1ce~appgver_d3ab747773788191af9d80368bb48455`. |
| Audience and MCP | Owner-private with zero external visitors, verified after publication. Read-only `/mcp` is published and a managed plugin connection was offered; installation, OAuth connection and a live authenticated tool call have **not** been verified. |
| Source coverage | **153 portable files** match the Site checkout byte-for-byte and mode-for-mode; deterministic `web/SOURCE_MANIFEST.json` verified and a round-trip Site export compared equal. Native runtime, deployment identities, user data, dependencies and regenerated WASM/OCR assets are separately identified, not silently missing application code. |
| GitHub epics | Eight open, timestamped [epics #176–#183](EPICS.md), linked to both PRs and exported under `docs/epics/`. Open acceptance work remains. |
| Local verification | Python 92 + source-exchange 28 tests pass; Ruff format/lint and dependency audit pass. Combined native strict Clippy/format pass; 1,173 tests pass with 13 ignored and three documented local PID-observation exclusions. Three actual MuPDF/Poppler provider tests pass separately. Final web typecheck/build, 16 React checks, imports/Office/OCR, HTML/feed, MCP and actual R2/D1 fixture contracts pass. See [native summary](validation/native-final-summary.json), [web summary](validation/web-final-summary.json) and limitations below. |
| Hosted verification | [Web alpha source CI](https://github.com/benpshore/pdftextract/actions/runs/37167621935) **passed** on implementation `f22b49c`; CI, Native, PMC bibliography and binaries were still running. See the [hosted checkpoint](validation/hosted-checkpoint.json); unfinished runs are not counted as passed. PR remains draft; use its current checks before merging. The prior head's Native diagnostic completeness gate failed on 47 Partial baseline papers; build/Clippy/unit tests passed. See [exact prior evidence](analysis/native-ci-77c58ec.json). |

The publication build and local tests do not certify device behavior, 50 GB imports, native backend deployment, full PDF correctness or account storage capacity. No credentials, user documents, database files or installed dependencies were added to GitHub. No new GitHub secret was created; existing managed authentication and ephemeral credentials were used. External GROBID/voice services require separate runtime configuration when actually deployed.

Recent local commit sequence, with author timestamps converted to UTC:

| UTC time | Commit | Change |
| --- | --- | --- |
| 2026-10-04 00:29:02 | `e27e1fb` | Engine extraction/native safety/corpus repair, PR #173 merge. |
| 2026-10-04 00:36:10 | `77c58ec` | Complementary native components and portable private web alpha. |
| 2026-10-04 00:36:38 | `a4e8f51` | Bibliography annotation union independently for each page. |
| 2026-10-04 00:45:00 | `19c9cc1` | Supervised Docling text parser and evidence preservation. |
| 2026-10-04 00:53:02 | `2800710` | CLI input byte limit made explicitly opt-in. |
| 2026-10-04 00:55:25 | `46293b0` | Explicit supervised GROBID client and streamed uncapped input. |

These are commit timestamps, **not** timestamps of user messages, first implementation, successful testing, or deployment.

## Sources of truth and document precedence

- [`USER_REQUIREMENTS_HISTORY.md`](USER_REQUIREMENTS_HISTORY.md): parent-owned observed request/work history. The record distinguishes exact quoted requests from inferred acceptance criteria.
- [`ENGINE_REPAIR_2026-10-03.md`](ENGINE_REPAIR_2026-10-03.md): merged repair scope and limits.
- [`CLI-WORKERS.md`](CLI-WORKERS.md), [`PUBLICATION.md`](PUBLICATION.md): actual native containment and publication contracts.
- [`NATIVE_FALLBACK.md`](NATIVE_FALLBACK.md), [`PDF_OXIDE.md`](PDF_OXIDE.md), [`PDF_EXTRACT_CFF.md`](PDF_EXTRACT_CFF.md), [`LITEPARSE_LAYOUT.md`](LITEPARSE_LAYOUT.md): optional backend/helper behavior.
- [`DOCLING.md`](DOCLING.md), [`GROBID.md`](GROBID.md): implemented supervised text parser and explicit scholarly service client, with separate unqualified model/runtime deployment work.
- [`WEB_ALPHA.md`](WEB_ALPHA.md): source exchange and deployment/authentication boundary. Its source manifest describes a specific snapshot, not arbitrary later edits.
- [`SITE_MCP.md`](SITE_MCP.md): implemented MCP protocol and ownership contract.
- [`WEB_APP_INTEGRATION_AUDIT.md`](WEB_APP_INTEGRATION_AUDIT.md): final source/fixture audit, reader sanitizer and stale-publication repairs, private media caching, and remaining deployment/scale limitations.
- [`EPICS.md`](EPICS.md): the eight created issues and their offline creation-time exports; GitHub remains authoritative for later issue updates.
- [`MOBILE_OCR_READER.md`](MOBILE_OCR_READER.md): primary-source feasibility review; explicitly not device test evidence.
- [`EXTRACTION_ROUTING_REVIEW.md`](EXTRACTION_ROUTING_REVIEW.md): reconciled independent toolkit/routing review. It now distinguishes supervised Docling text from the blocked model path, implemented GROBID contracts from unmeasured service quality, and repaired per-page bibliography links from remaining geometry/provenance limits. Earlier arbitrary model/page/GROBID byte-budget proposals were withdrawn.
- [`WEB_EXTRACTION_REVIEW.md`](WEB_EXTRACTION_REVIEW.md) and [`WEB_EXTRACTION_BENCHMARK.json`](WEB_EXTRACTION_BENCHMARK.json): completed independent 11-source web benchmark, with URLs, hashes, retention measures and timing definitions.
- [`../web/docs/OFFICE.md`](../web/docs/OFFICE.md): browser Office projection, asset contract, tests and unsupported features.

The web-app README was rewritten during final integration with its ownership map, build/test commands, actual format capabilities and explicit unfinished work. Old 8 MiB/4 MiB/150-page alpha admission limits do not describe this revision.

## Native engine: implemented behavior and limits

| Module | Implemented contribution | Evidence and remaining limit |
| --- | --- | --- |
| `lopdf` baseline | Positioned text, fonts, annotations and figure evidence; repaired font/CMap/Form handling, executed-resource diagnostics, missing glyph positions, zero-page/range handling and status propagation. | `Complete` means no detected supported gap, not an independently certified perfect transcription. Malformed input and missing mappings remain Partial or fail explicitly. |
| PDFium | Independent native font/text interpretation; configured binary fingerprint in identity; preserves unresolved mapping evidence. | Native library must be provisioned. Eager extraction and binding page-index limits remain documented. No new OCR is inferred from an existing invisible OCR layer. |
| `pdf-extract` helper | Pinned 0.12.1 + `cff-parser` 0.2.0 recovers eligible simple Type1C font mappings inside the baseline parser. | Not a whole-document PDF fallback. No blanket Type0/OpenType or arbitrary font recovery. Existing explicit mappings win. |
| PDF Oxide native | Pinned 0.3.78, with compatibility pin `office_oxide` 0.1.9; genuine positioned characters/font data and URI annotations. | Intentionally Partial because upstream silent fallbacks cannot certify mapping coverage. ASCII ActualText fixture improves; Unicode alpha ActualText showed an upstream mojibake bug. Do not globally prefer longer Oxide output. |
| LiteParse | Pinned 2.15.1 spatial projection of existing PDFium spans. | Preserves character/spatial evidence; not semantic table recognition. Does not call another PDFium wrapper, remote OCR, or LiteParse network APIs. |
| MuPDF | Optional C shim/native adapter with real text/font/image-region evidence and raw URI annotation actions. | Prototype tested against 1.26.11; not the newer release mentioned in upstream docs. No bundled binary. AGPL/commercial terms remain applicable. |
| Poppler | Optional C++ provider against exact 26.09.0; word/font geometry, raw URI targets, password support, crop/rotation handling. | Actual Linux runtime tested. No image-pixel export or metadata parser in this provider. GPL obligations remain applicable; an optional loader is not a license exemption. |
| Docling text | Pinned 1.69.2 page parser; requested pages only, real region geometry, native URI/image/metadata side channel, coverage comparison and Partial evidence. | No exposed font names/sizes; no encrypted-document password input. Count agreement cannot prove identical reading order or punctuation. |
| Docling layout/OCR | Separate opt-in older PDFium/ONNX path; page/geometry/formula/coverage gaps explicitly Partial. | Not admitted to supervised CLI extraction. Runtime/model pinning, model-discovery containment and statistical/layout evaluation remain unfinished. |
| GROBID | Explicit `tpe grobid` command; configured endpoint only; immutable streamed input, raw TEI/hash/ranges, semantic reference/header projection and coordinates. | No real GROBID deployment or model-quality/throughput benchmark. Semantic projection is not glyph-complete PDF extraction. HTTP contract tests use a local fixture server. |
| Bibliography | References retain raw text and DOI evidence; per-page native/backend annotation union with target+rectangle dedupe and coordinate-frame guard. | Keeps distinct placements of the same DOI. Source DOI disagreement is not repaired by rewriting truth labels or silently substituting a resolver result. |

Shared native provider code is in `src/backend/native_provider.rs`, `native/provider.h`, `native/mupdf/`, and `native/poppler/`. Explicit provider/engine paths, binary hashes, ABI/anchor checks and ownership/freeing contracts are separate from system-font and mapping-data provenance. Native exceptions stay on the C/C++ side. Direct library callers do not automatically inherit CLI process containment.

### Routing and status

`TPE_AUTO_NATIVE_FALLBACK=1` opts into at most `lopdf -> PDFium -> MuPDF -> Poppler`, each once and within one overall worker budget. Absent/`0` preserves the default; malformed values fail. Optional native candidates require explicit compiled features and provider/runtime files.

Replacement checks source hash/size, exact requested page sequence, total page count, coherent status, text nonregression, and known link/figure evidence. The result remains one whole backend pass, with route history and that backend’s identity. It is **not** a page/element fusion engine. Equal-length wrong text can pass count checks. Returned errors preserve the current best pass; a hard native crash or outer timeout still kills the shared worker and cannot recover an in-memory earlier pass.

### Containment, large inputs and publication

The default source and output-capture byte caps were removed; explicit user-selected limits remain available. Parser/font/output safeguards, time, process and memory budgets remain operational protections. The CLI documentation currently lists a 60-second document deadline, 1,024 MiB virtual-address-space growth allowance, one worker by default, and a separate input-selection count option. These are not a demonstrated RSS, throughput, or arbitrary-size success guarantee.

Publication stages complete results, preserves input/output aliases, and commits ledger/export evidence carefully. `publication_unknown` means outputs or a ledger commit may exist; inspect before retrying. Cancellation kills/reaps workers; a truncated final JSON line is not a complete success record. The native controller does not parse large result JSON itself, but some backends still hold full source bytes in their worker.

GROBID has no default PDF/TEI/projection byte cap, but retains its separate 60-second/512 MiB worker defaults. It streams into a private immutable temporary snapshot and then multipart upload. Redirects, environment proxies and retries are disabled; consolidation defaults to zero. Remote processing already accepted by a server may outlive local cancellation. No automatic external document transfer is introduced into ordinary extraction.

## Browser workspace: implemented in the current tree

| Surface | Current implementation | Honest boundary |
| --- | --- | --- |
| File routing | Magic/content and MIME/name detection separates PDF, HTML, feeds, plain XML, CSS/text, images, archives, Office, JSON and unsupported originals. PHP-generated HTML uses HTML extraction; PHP source is text and is never executed. | Preserve tests where filename and content disagree. A `.pdf` suffix must not send HTML to PDF Oxide. |
| PDF | Dedicated disposable worker running `pdf-oxide-wasm` 0.3.77; pages, text and URI annotation evidence; browser results Partial. | Not repaired native TPE and not native fallback/OCR. Source is currently buffered for the WASM call; no arbitrary page/file admission cap does not mean bounded-memory arbitrary-size parsing. |
| HTML | DOMParser, pre-cleaning, Mozilla Readability, DOMPurify and GFM. Article-scoped links/tables, safe lazy image sources, captions and heading metadata; scripts/styles/hydration/hidden noise removed. | Fetched snapshot only. Does not execute client JavaScript, render canvas, obtain authenticated resources or prove all CSS-hidden content was detected. No headless/live-DOM capture implementation is claimed. |
| Structured HTML | Source table/code/semantic footnote retention guards choose cleaned main content when Readability drops meaningful blocks; explicit main-region introduction guard protects documentation definitions. | Broader fallback may include surrounding material and reports that fact. Generic landing-page completeness is not established. |
| HTML images | Stable `data-image-id`, ordered source metadata and real source URLs; parent asset helper fetches/stores private bytes and substitutes authenticated image paths. | Asset fetch failure warns and removes the unavailable display image. Dynamic blob-only images cannot be recovered from static HTML. Feed-entry media needs its own display/materialization verification. |
| RSS/Atom | Namespaced full-content selection, literal Atom text versus HTML/XHTML, inherited `xml:base`, IDs/dates/authors/enclosures and external-content evidence; feed-authored fragments bypass article scoring. | No automatic article/enclosure fetch. External Atom content retains available summary and unresolved URL with warning. DTD/entity declarations are refused. |
| CSS/plain XML/text | Stored/read as text, never PDF parsing or PHP/CSS execution. Saved/current HTML, feeds, text, CSS, XML and JSON use `decodeSource`; the Windows-1252 HTML meta case is covered. | Text capture is not code execution or format-specific semantic interpretation. `clipText` is available, while the UI uses its explicit plain-text branch. |
| Images/OCR | Local Tesseract.js 7.0.0 CPU/WASM, pinned same-origin assets and English model; PNG/JPEG/WebP still-image support, orientation inspection, cancellable owned worker. | Always Partial. Adaptive working raster (4 MP/4096 side) is disclosed while originals are retained. HEIC/GIF/animated/multi-picture recognition and PDF OCR are not implemented. No ANE/WebNN/WebGPU claim. |
| Archives | ZIP/TAR/GZIP/TGZ stream through bounded chunks, one current OPFS member, backpressure and cleanup; CRC/size/checksum/truncation evidence. No total-byte/member-count admission cap. | Traversal, links/devices, encrypted/unsupported compression and nested archives are refused with explicit member status. OPFS/streaming decompression/browser quota required. No expansion checkpoint/resume guarantee. |
| Office | Browser ZIP/XML projection for DOCX/PPTX/XLSX; real text, tables, ordered slides/sheets, links and embedded image assets. ODF/iWork preserve available XML/previews with Partial. | No full layout/list reconstruction, spreadsheet recalculation/date formatting/chart rendering, `.iwa` decoding, or legacy/encrypted binary Office support. Selected XML/image members still occupy browser memory. |
| Multipart storage | Original, result and asset uploads are separate; streamed R2 parts, owner-scoped D1 records, source SHA-256, cancellation and commit-response recovery. | Per-part protocol/hosting limits still exist. No real 50/100 GB batch or quota proof. Serialized extraction JSON and some browser blobs still use memory. |
| UI recovery | Queue/progress/cancel/retry states and owner-keyed IndexedDB snapshots preserve local File/Blob work where available; failed save can retain extraction for retry. Re-read reuses the saved record/original, and a valid existing result survives parser failure or cancellation. | Not proof of background upload continuation or full browser-kill recovery on iOS. Local browser quota and eviction remain real failures. |
| MCP | Stateless read-only `POST /mcp`, protocol 2025-11-25; authenticated owner-only search, field windows and original-image retrieval. | Plugin connection/installation and end-to-end ChatGPT use were not verified at this checkpoint. No automatic new-conversation attachment or voice feature. |

### Image, DOI and provenance contract

Visible link label, raw URL/URI target, normalized DOI candidate and externally resolved bibliographic identity are different facts. Keep each where available. Do not fetch DOI links during extraction merely because they exist. A native annotation rectangle, browser image dimensions, OCR box and semantic TEI coordinate have different origins; retain the transform and granularity.

Browser HTML image metadata uses `{id,url,alt,caption,width?,height?,source:'article'}` and matching `data-image-id`. Private retained assets add storage URLs and source provenance. Render only authorized private image paths in the saved reader; do not turn arbitrary imported source HTML into live third-party requests. Script text, base64 payloads and filesystem folders are not article images.

### Storage and authentication boundary

The Site uses private R2 originals/results/assets and owner-scoped D1 metadata; this is not ChatGPT Library. `app/chatgpt-auth.ts` trusts identity headers supplied by the managed Sites edge. Exposing that adapter behind an arbitrary public server would not establish authentication. Local mock-auth preview is not a production auth system.

The final integration audit also records unresolved operational boundaries: the saved-document UI currently lists at most 200 rows; upload part receipts are not durably persisted for byte-level resume; orphan cleanup is not fully qualified; DNS preflight is not DNS pinning; and legacy non-multipart endpoint MIME/buffering behavior needs reconciliation. These are audit findings, not new document-admission caps or claims that an exploit was reproduced. Re-check their disposition against the final deployed source.

MCP tool calls first require an owned D1 record before reading R2. Another owner’s ID is indistinguishable from a missing record. Responses are private/no-store; source content is labeled untrusted, with provenance and Partial/Failed status unchanged. Search is over the indexed subset, not a claim of full-corpus/full-byte indexing. Response windows and the inline image budget are continuation/context limits, not saved-document rejection caps.

## Module map for the next agent

| Path | Responsibility |
| --- | --- |
| `src/backend/mod.rs`, `src/schema.rs` | Extraction backend interfaces, shared page/span/link/figure/status model. |
| `src/backend/lopdf_backend*` | PDF content/font/geometry interpretation and CFF helper. |
| `src/backend/pdfium_backend*`, `pdf_oxide_backend.rs`, `docling*_backend.rs`, `liteparse_layout_backend.rs`, `native_provider.rs` | Optional native adapters and layout projection. |
| `src/pipeline.rs`, `src/router.rs` | Pipeline/routing, candidate acceptance and identity. |
| `src/bibliography.rs`, `src/citations.rs`, `crates/tpe-biblio/` | Reference segmentation, citation evidence, resolver/enrichment logic. |
| `src/cli_worker.rs`, `worker_limits.rs`, `worker_allocator.rs` | Disposable worker containment and supervision. |
| `src/publication.rs`, `src/ledger.rs`, `src/acquire.rs` | Immutable input, persistence and publication contract. |
| `src/grobid.rs`, `src/grobid_cli.rs` | Explicit external scholarly projection client. |
| `native/manifest.json`, `native/fetch*.sh`, provider subdirectories | Runtime pins/provisioning and optional shim builds. |
| `web/app/workspace.tsx` | Current browser queue, state, routing, reader and retry flow. |
| `web/lib/clip.ts` | HTML/feed/plain-source extraction; does not import a PDF engine. |
| `web/lib/upload-client.ts`, `uploads.ts`, API uploads routes | Detection, multipart upload/finalization and save recovery. |
| `web/lib/source-fetch.ts`, API capture route | Public-source retrieval and URL/redirect boundary. |
| `web/lib/article-assets.ts`, `asset-storage.ts`, document asset routes | Retain real article/Office images in private storage. |
| `web/lib/imports.ts`, `image-ocr.ts`, `office.ts` | Archive, still-image OCR and Office projection. |
| `web/lib/workspace-storage.ts` | Owner-keyed local recovery snapshots. |
| `web/lib/mcp.ts`, `web/app/mcp/route.ts` | Read-only saved-document MCP. |
| `web/public/pdf-worker.js`, `web/scripts/copy-pdf-wasm.mjs` | Published browser PDF package integration. |
| `scripts/site_source.py`, `web/SOURCE_MANIFEST.json` | Portable source snapshot/verification/staged import; no deployment. |
| `corpus/`, `src/eval.rs`, `native/measure_eval.py`, `native/validate_eval.py` | Pinned acquisition/truth, scoring and provenance validation. |

Other existing workspace crates (`tpe-app`, `tpe-browser`, `tpe-search`, `tpe-speech`, credentials/Zotero helpers) are not newly certified by this repair. Their existence does not mean the Site has native app/search/voice parity.

## Validation evidence already observed

| Work | Evidence | What it does not prove |
| --- | --- | --- |
| Baseline/Oxide paired difficult-paper comparison | Five papers, 266 pages, 207/207 references for both. Oxide 206/207 titles versus baseline 205/207; body alignment 0.894 versus 0.952; author recall 0.533 versus 1.000. See `docs/analysis/pdf-oxide-five-paper-2026-10-03.json`. | Universal accuracy, default-engine superiority or new corpus-wide speed. All five remained Partial. |
| Native routing | 48 focused pipeline/router tests reported passing; independent read-only contract review. | Recovery after process crash, equal-length semantic correctness or element fusion. |
| MuPDF/Poppler | Actual configured runtime fixtures: raw URI versus label/Base/GoTo, crop/rotation geometry, warning/missing-map status, output refusal/recovery and passwords where supported. | Paired corpus advantage, every platform, complete native dependency identity or licensing approval. |
| GROBID | Nine loopback HTTP/CLI contracts and strict Clippy passed; real 65 MiB streamed fixture and response exceeding 16 MiB accepted. | Real GROBID 0.9.1 model accuracy, throughput or deployment. |
| HTML focused suite | `web/scripts/test-clip.mjs` passed: script/style/hidden noise, Unicode, actual DOI target, lazy images/captions, structured tables, leading definition, semantic footnote, >4 MiB HTML, >1,000 DOI candidates, >500 feed entries, CSS/plain source. Focused ESLint passed. | Arbitrary modern sites, real browser layout/visibility, mobile runtime or live Site publication. |
| Independent HTML/feed contracts | Five independent checks passed in `test-web-extraction-review.mjs`, including namespace collision and external Atom content fallback. | Full standards conformance or statistical corpus completeness. |
| Exact reported How-To Geek page | Reviewer fetched HTTP 200, 581,470 bytes; SHA-256 `641925e077f0bf0f4d3d95d4931b79fe1ca39e9d4402c1e21b47c47260267eb1`. Fixed output retained 24/24 authored paragraphs and 10/10 measured section headings; 30 image elements, 27 unique source URLs, eight article links, no selected account/control noise. Local output text length 8,596 includes whitespace; independent normalized count differs. | Image bytes successfully fetched/stored in the deployed Site, dynamic browser content, or all possible chrome removed. |
| Varied benign web benchmark | Final 11-source report: Python 79/79 paragraphs, MDN 34/34, Wikipedia 1,374/1,374 cell matches and 9/9 headings, arXiv 41/41 cell matches; RSS 10/10 and Atom 8/8 entries. Fixed warm parses 26–1,526 ms in Node/JSDOM; fetches 3.8–6.4 s separately. Exact cleaner SHA-256 `136045b0df9249b22a1f36a4425651b64568f8df96e61926c81254ed123d7c50`. | NASA mission index remains 45/55 reference paragraphs; PMC 55/57 omits correspondence/date metadata. Normalized substring matches are retention evidence, not universal fidelity or browser latency. |
| MCP contracts | `scripts/test-site-mcp.mjs` passed mocked auth/owner/SQL/cursors/Unicode/>9 MiB saved JSON/image/protocol cases. | A live installed ChatGPT connector or arbitrary-scale search. |
| Final reader/security source audit | Exact installed DOMPurify/JSDOM repro confirmed the corrected reader removes style/global-CSS, image-input and legacy table-background payloads, retains owned assets and rejects another document’s image; private media/asset headers are `no-store`. | Focused source/fixture audit, not a live penetration or multi-account browser test. See `WEB_APP_INTEGRATION_AUDIT.md`. |
| Storage transactions | `scripts/test-site-uploads-workers.mjs` passed against disposable Miniflare R2/D1/DigestStream: hashing, owner 404, CAS, stale result 409, lost receipt recovery and no deletion of newer output. | Production concurrency/storage quota or huge live batch throughput. |
| Browser OCR/archives | `test-node-imports.mjs` passed **21 assertions in five fresh processes**, including eight alternating compressed-stream cancellation/quota teardown cycles per run, 9 MiB ZIP/251 members, integrity/path/link/cancel/quota cleanup, nested-member error with sibling continuation, and actual Tesseract 7 CPU/WASM recognition: exact three fixture lines and DOI, confidence 94. Pinned OCR assets total 28,865,102 bytes across 11 files; the browser selects one core. | Storage, pixel decoder and worker construction use test shims. Chromium download was invalid/truncated; no real browser/iPhone/iPad QA or 50/100 GB run. Concatenated gzip is explicitly refused by the tested runtime, not silently truncated. |
| Office | Actual generated DOCX/PPTX/XLSX ZIP fixtures passed text/order/table/link/exact-PNG checks, UTF-16/spacing, asset callback/backpressure, failed storage, unsafe SVG/links, DTD and cancellation. Full web TypeScript passed at owner freeze. | Not a real Office corpus, visual layout benchmark, live asset-store proof or 100 GB run; Office fflate path does not verify ZIP CRC. |
| UI | Portable `scripts/test-web-workspace.cjs` passed **16 actual React/jsdom component checks** in the final 01:11:59 UTC report, including re-read preservation, legacy charset decoding and final reader style/background/input-image sanitization. | Network/storage/extraction are mocked; not actual browser IndexedDB, device, reload or background QA. |

Exact-page snapshots are validation artifacts under `/workspace/scratch/b337de76c73c/validation/web-extraction/`, not committed full copyrighted articles. Checked-in regressions are independently authored minimal fixtures. Keep source URLs, retrieval status and hashes; do not commit entire publisher pages merely to preserve a benchmark.

The repo’s `AGENTS.md` requires broader Python/Rust/Swift/CMake gates before committing. Historical local process-control failures caused by unavailable child-PID observation were not waived or counted as passed; use hosted native runners for those cases. Final module owners reported full web TypeScript passing. Final native feature-graph Clippy passes, with 15/18 containment cases passing locally; three child-PID-observability cases remain blocked as in the baseline and require hosted validation. Swift and CMake are absent locally; hosted Swift is configured, while Objective-C++/Foundation CMake validation needs macOS. The integrated Site build is published; new-head hosted checks were still running at the checkpoint.

## Reproducible commands

Run from the repository root unless noted. These commands are instructions, not claims that all have passed on the final unpublished tree.

```sh
git status --short
git log -8 --oneline
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
uv run ruff format --check
uv run ruff check
uv run pytest
uv audit --preview-features audit-command
swift build
swift test
cmake -S . -B build
cmake --build build
ctest --test-dir build
```

Focused native checks (runtime paths/features must match the integration docs):

```sh
cargo test --locked --features pdf-oxide --test pdf_oxide_backend
cargo test --locked --features pdf-extract --test pdf_extract_cff
cargo test --locked --features docling-text --test docling_text
cargo test --locked --features grobid --test grobid_contract
cargo test --locked --test lopdf_unicode_status --test bibliography_input_safety
cargo test --locked --test cli_containment --test worker_startup --test end_to_end -- --test-threads=2
```

Provisioned native provider tests require the explicit MuPDF/Poppler environment paths documented in `native/*/README.md`; ignored runtime tests without those files are not a pass. Keep `CARGO_BUILD_JOBS=2` in this constrained shared workspace. Agents used external shared toolchain/target caches; coordinate ownership before cleaning or compiling them. Disk exhaustion happened during parallel native builds, so do not launch duplicate full builds casually.

Browser source checks:

```sh
cd web
pnpm install --frozen-lockfile
node scripts/copy-pdf-wasm.mjs
node scripts/copy-ocr-assets.mjs
node node_modules/typescript/bin/tsc --noEmit --incremental false
node node_modules/eslint/bin/eslint.js lib/clip.ts scripts/test-clip.mjs
node scripts/test-clip.mjs
node scripts/test-web-extraction-review.mjs
node scripts/test-import-flow.mjs
node scripts/test-office.mjs
node scripts/test-node-imports.mjs
pnpm build
```

Actual browser testing is separate: provision Playwright Chromium, then run `node scripts/test-browser-imports.mjs` from `web/` (or set `PLAYWRIGHT_MODULE` to a provisioned module). That browser run was not completed here. Read each script’s runtime setup before interpreting its result. Optional private validation snapshot run:

```sh
cd web
HTML_REVIEW_FIXTURE=/absolute/validation/web-extraction/howtogeek.html node scripts/test-clip.mjs
node scripts/test-web-extraction-review.mjs --fixtures /absolute/validation/web-extraction
```

Root-level MCP/storage tests:

```sh
node --experimental-strip-types scripts/test-site-mcp.mjs
node --experimental-strip-types scripts/test-site-uploads-workers.mjs
node scripts/test-web-workspace.cjs
uv run pytest web/tests/test_site_source.py
```

Source exchange follows `WEB_ALPHA.md`: export to a **new** directory, verify the exact manifest, stage an import against the destination read-only, review the plan, then use the managed Site deployment workflow. The script does not deploy, copy private documents, migrate identities, or transfer live database/storage state. Do not copy `.env`, credentials, user documents, local bindings or generated runtime directories into Git.

## Next tasks with acceptance criteria

The following open epics were created from the observed requests. Timestamps are copied from the creation-time exports under `docs/epics/`; they are **issue creation times, not user-message or completion times**. See [EPICS.md](EPICS.md) and [USER_REQUIREMENTS_HISTORY.md](USER_REQUIREMENTS_HISTORY.md).

| Epic | Created UTC on 2026-10-04 | Workstream |
| --- | --- | --- |
| [#176](https://github.com/benpshore/pdftextract/issues/176) | 01:11:36 | Source mapping, publication evidence and Dot handoff |
| [#177](https://github.com/benpshore/pdftextract/issues/177) | 01:11:36 | HTML/RSS/Atom, retained images and rendered-page coverage |
| [#178](https://github.com/benpshore/pdftextract/issues/178) | 01:11:37 | Minimal mobile UI, navigation recovery and accessibility |
| [#179](https://github.com/benpshore/pdftextract/issues/179) | 01:11:38 | 50 GB mixed batches, durable resume and storage lifecycle |
| [#180](https://github.com/benpshore/pdftextract/issues/180) | 01:11:38 | Office, archives, OCR and photo/audio/video coverage |
| [#181](https://github.com/benpshore/pdftextract/issues/181) | 01:11:39 | Native parser routing, provenance and performance |
| [#182](https://github.com/benpshore/pdftextract/issues/182) | 01:11:40 | Native/GROBID/Docling service deployment and qualification |
| [#183](https://github.com/benpshore/pdftextract/issues/183) | 01:11:40 | MCP connection, portable bundles and future chapter audio |

Implementation remains linked to [PR #175](https://github.com/benpshore/pdftextract/pull/175), with the merged repair baseline in [PR #173](https://github.com/benpshore/pdftextract/pull/173). Creating or linking an issue does not satisfy its acceptance criteria.

| Priority / task | Required acceptance evidence |
| --- | --- |
| P0: Close release verification | Implementation is pushed, source manifest matches and private Site v2 is published. Review current PR checks and finish remaining device/native completeness work before declaring full completion; keep failed diagnostic rows. Re-run affected contracts after any final fix. |
| P0: Exercise actual private reader and images | Import the reported article on the deployed Site; text/heading metrics and actual private image bytes retained, captions visible, no placeholder/base64/script noise, no raw external image rendering. Confirm cancellation/fetch failure leaves original and explicit warnings. |
| P0: Preserve multi-format routing | Real HTML named `.pdf`, PDF named `.html`, PHP-served HTML, raw PHP/CSS/plain XML, RSS/Atom and Office route correctly. UI engine labels match executed code. |
| P0: Publish final benign benchmark evidence | Preserve the completed 11-source report and cleaner hash with the release. Re-run after cleaner changes; attach paragraph/heading/table/code/URI/image metrics, timing definition and remaining misses. Do not select only successes. |
| P1: Improve observed capture latency | Measure fetch, parse, media download and persistence separately on target devices. Preserve the completed text/table/image coverage while reducing blocking work; keep early readable text and per-asset progress. The new structured-retention path does more work than raw Readability and is not yet a demonstrated speed improvement. |
| P1: Large import and recovery | Increase representative files/batches without arbitrary admission caps; measure memory, storage, throughput and actual failures. Test cancellation, tab navigation/reload, browser eviction, interrupted multipart commit, extraction save retry, archive member failure and storage exhaustion. Distinguish resume from restart. Do not claim 100 GB from a 9 MiB test. |
| P1: Verify account storage | Obtain actual managed account/bucket quota and usage evidence, distinguish the user’s 100 GB statement from configured enforcement, and show recoverable quota errors. Never create a hidden per-file ceiling in its place. |
| P1: Mobile OCR/reader | Real iPhone and iPad mini Safari/Chrome matrix with versioned runtime/model, accuracy, time, cancellation, orientation, thermal/memory behavior and offline model reuse. Keep CPU/WASM fallback; no ANE claim without actual provider evidence. |
| P1: Dynamic/live-DOM capture | Implement an explicit live-page/extension or controlled rendering lane only with a clear auth/network contract. Show a script-rendered article/image that static capture misses, preserve source/provenance, and retain honest Partial on unsupported content. Do not execute page scripts inside the saved reader. |
| P1: MCP connection | Install/connect through supported managed auth, exercise all three read tools on explicitly selected owner data, reject cross-owner IDs, follow revision cursors, and demonstrate images/download fallback. Static tool discovery and mocked calls are insufficient. |
| P1: Native toolkit comparative evidence | Same corpus/truth/runtime machine for baseline/PDFium/Oxide/MuPDF/Poppler/Docling text; record per-field wins/losses, geometry, raw URI, images, status, cold/warm time and peak RSS. Keep input hashes and all failed cases in denominators. |
| P2: GROBID model evaluation | Explicitly provision a licensed/configured local service, pin real reported version/models, compare reference/header fields against adjudicated existing cases, retain TEI/raw evidence, and measure latency/resources. No automatic third-party transfer or consolidation. |
| P2: Learned layout/OCR | Complete model/runtime identities and supervised deployment, then small adjudicated table/scan/multicolumn/figure-caption tests. Region boxes and plausible Markdown are insufficient. |
| P2: Chapter reader and future voice | Combine real bookmarks/headings with user correction and page anchors; test lock screen/seek/interruptions on devices. Verify current OpenAI model/auth/deprecation/cost contract at implementation time; explicit narration authorization and AI-voice disclosure. No API key request or live voice call is needed for the current research task. |
| P2: Deeper provenance/fusion | Introduce a schema for element/page alternatives and persisted baseline recovery before attempting cross-engine fusion or hard-crash recovery. Keep source versus normalized versus model-enriched text distinct. |

### Requested research still needing a concrete follow-through

The Bear/RSS request now has implemented extraction changes and the eleven-source benchmark. The mobile reader report documents concrete Ghostscript, PyMuPDF/PyMuPDF4LLM and pdfplumber heading strategies. **A separate Paperless-ngx workflow/source comparison has not been established by this handoff evidence.** Add it to service integration epic #182: inspect its ingestion queue, OCR decision logic, duplicate handling, original/archive policy and failure recovery; link specific source revisions and translate useful behavior into tested proposals. A product link alone is not completion.

The native C/C++ engines use reviewed FFI providers; their internals were not rewritten wholesale in Rust. The Site remains a distinct browser/Worker application. Full native-to-Site processing, page/element fusion and future voice are ongoing epics.

## Working rules for Dot

Continue authorized reversible implementation autonomously, but keep evidence current. Use specialized reviewers for independent error discovery, not duplicated broad builds. Announce what changed and what the next check resolves. If a test or runtime is unavailable, record the exact blocker and do not count it as a pass.

Read `AGENTS.md`; do not hand-edit release versions. Main is protected, PR CI gates remain in force, and every merge to main releases automatically. Do not merge a draft based only on this handoff. Do not deploy a Site while another agent is changing its source or lifecycle. Native/GROBID/Docling work can remain useful without being silently inserted into the browser import path.

Never “fix” a completeness problem by suppressing Partial, dropping an evaluation row, rewriting a truth label, removing a failure test, adding invented geometry, substituting a link label for its target, or hiding a practical limit behind a claim of unlimited processing. Keep original source bytes and explicit recovery paths.
