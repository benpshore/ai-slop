# Web application integration audit

Reviewed **2026-10-04 01:09 UTC**, against the working tree based on
`46293b0e5128d125c4f842ae13604095e6e78a7b`. This is a focused source and fixture
review before publication, not a live penetration test or a large-account
capacity certification. No private user documents or production storage were read.

## Findings and disposition

**The final reader's saved-HTML boundary had a blocking defect, now corrected and
retested.** Its first sanitizer allowed `<style>`, image inputs, and legacy table
`background` attributes. Exact-function tests with the installed DOMPurify and
JSDOM reproduced CSS affecting the whole app and surviving external image URLs.
The final reader now forbids those tags/attributes. The same payloads are removed;
script handlers and JavaScript links are removed; an owned-document asset image
survives; an image targeting another document is removed. This final check matters
even though the HTML importer also sanitizes: saved result HTML remains untrusted.

**Stale extraction publication was corrected and verified.** Multipart result
sessions capture the previous result key. D1 publication now uses compare-and-swap;
an older completion/retry cannot overwrite a newer result or delete its blob.
The legacy PATCH route uses the same conditional publication rule. Actual
disposable Workers R2/D1 tests pass for streaming SHA-256, multipart completion,
owner checks, a failed receipt write followed by successful recovery, and an
interleaved stale publisher rejected with 409 while the selected blob remains.

**Private asset/media caching was tightened and the source was rechecked.**
Both routes now use `private, no-store`, matching original/document reads. This
avoids the previous one-hour browser-cache reuse across an account switch; a live
multi-account browser test is still distinct from this verified header change.

No additional blocking ownership or script-execution regression was found in the
reviewed paths after the reader fixes. The unverified boundaries below still apply.

## File and behavior map

| Boundary | Implementing files | Observed behavior |
| --- | --- | --- |
| Identity and documents | `web/app/chatgpt-auth.ts`, `web/lib/server.ts`, `web/app/api/documents/**` | Sites identity helper; owner-filtered records; original download is an attachment with no-store/nosniff/sandbox headers. Result JSON is streamed. |
| Uploads and publication | `web/lib/uploads.ts`, `web/lib/upload-client.ts`, `web/app/api/uploads/**` | Multipart original/result/asset uploads, declared-versus-stored length check, streaming original hash, recoverable completion receipt, CAS result selection. |
| Public-source capture | `web/lib/source-fetch.ts`, `web/app/api/capture/route.ts` | HTTP(S), credentials/port/host checks, A/AAAA preflight, checked redirects; no forwarding of the user's cookies or identity headers. Captured page scripts are not run. |
| Images and media | `web/lib/article-assets.ts`, `web/lib/asset-storage.ts`, document `assets`/`media` routes | Imported article images are copied through authenticated server routes. Original/embedded media reads require ownership. Media supports one byte range; multipart image MIME allowlists exclude SVG/HTML. |
| Reader and import routing | `web/app/workspace.tsx`, `web/lib/clip.ts`, `web/lib/imports.ts`, `web/lib/office.ts` | Sanitized article presentation, private image URLs, escaped plain text, content-based routing, archive/member status, Office extraction with embedded image retention. Unsupported extraction keeps the original and reports Partial. |
| Recovery | `web/lib/workspace-storage.ts`, `web/app/workspace.tsx` | User-keyed IndexedDB checkpoints and browser history preserve queue/selection/scroll. Quota errors are surfaced. This is not a background job service. |
| Agent reads | `web/lib/mcp.ts`, `web/app/mcp/route.ts` | Static discovery; authenticated owner-only tools; streamed, revision-bound section pagination; explicit untrusted-content/provenance/status labels; no external fetch or write tools. |

## Evidence and remaining limits

- `scripts/test-site-mcp.mjs` passes auth, cross-owner 404, SQL binding, cursor,
  Unicode, evidence and image checks, including a saved text field exceeding
  9 MiB. `scripts/test-site-uploads-workers.mjs` passes against real disposable
  Workers bindings and also executes the MCP search query against actual D1.
- The web PDF path is browser PDF Oxide WASM. The native Rust engine's
  PDFium/MuPDF/Poppler routes are a separate execution path; their availability
  must not be attributed to this browser worker. Image OCR has its own reported
  limitations. Audio/video storage/playback is not transcription.
- No application document-admission cap is added by the multipart or MCP paths.
  This does **not** prove a 50–100 GB import, account quota, browser storage quota,
  or bounded browser memory. Several extraction and UI presentation paths still
  materialize a whole file/result. The legacy upload/PATCH endpoints also buffer
  their request bodies. The legacy upload now assigns CSS/text MIME types and
  detects PNG/JPEG/WebP images, with an octet-stream fallback for other images;
  the current UI uses multipart instead. The saved-document UI lists at most
  200 rows; MCP search
  is paginated. These are not evidence that every saved document is browsable in
  one UI list or that every byte is indexed.
- Recovery does not persist multipart part receipts for byte-level resume across
  navigation. A recovered queue may need to restart an upload. Browser closure,
  interrupted cleanup and conflicting results can leave unfinished/orphaned R2
  objects; comprehensive garbage collection was not verified.
- Public-DNS preflight is **not DNS pinning**: the subsequent fetch resolves the
  hostname again. Private-egress/DNS-rebinding behavior of the deployed Sites
  runtime was not established by this review. No private-egress exploit was
  reproduced, and no private-network binding is used in these source paths.
- Image MIME/signature checks and DOM sanitization are not malware scanning or
  a guarantee that every browser codec can render a file. Linked images can fail
  individually, and their failures remain visible in extraction warnings.
- The MCP implementation advertises the supported `2025-11-25` revision. Plugin
  installation, OAuth connection and an authenticated tool call against the final
  deployed Site still need deployment-level verification. UI multi-account cache
  behavior, full browser navigation recovery and large-batch throughput were not
  exercised by the disposable storage fixtures.

Related evidence: [MCP contract](SITE_MCP.md),
[web extraction review](WEB_EXTRACTION_REVIEW.md),
[handoff](HANDOFF_TO_DOT.md).
