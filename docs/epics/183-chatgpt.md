# [Web app][Epic] Connect read-only MCP, portable ChatGPT bundles, and future chapter audio

GitHub: https://github.com/benpshore/pdftextract/issues/183
Created: 2026-10-04T01:11:40Z
Status at export: open

Recorded 2026-10-04T01:09:25Z (2026-10-03 19:09:25 America/Denver). This epic is part of the user's requested Dot/Codex handoff for **benpshore/pdftextract**. Implementation is tracked in [PR #175](https://github.com/benpshore/pdftextract/pull/175); the core repair baseline is [merged PR #173](https://github.com/benpshore/pdftextract/pull/173), main commit e27e1fb28a5b40dbca7f517c6fe396e6f11ed4ec.

Current distinction: PR #175's published head was 77c58ec89a1c9c07425b77dec90ab00316aa07be at this record's start; follow-up fixes are being validated for the next revision. The live Site's first alpha uses upstream PDF Oxide WASM, not the complete native Rust engine. See the final handoff/source manifest for the exact subsequent publication revision.

## Implemented foundation
- `web/app/mcp/route.ts` and `lib/mcp.ts` implement stateless read-only MCP discovery and owner-authenticated document search/retrieval.
- Large stored results use streamed, revision-bound pagination; Unicode/escaped JSON and image metadata paths are tested. Actual Workers tests verify parameterized owner-filtered D1 search.
- Browser WebMCP capture action is separate from server MCP. Neither proves a plugin is installed or that a new ChatGPT conversation can receive attachments automatically.

## Remaining acceptance
- [ ] Publish Site MCP capability, retrieve the platform-provisioned connection/plugin identifier and offer installation through the supported Site flow. Verify a real authenticated read after connection; do not create an unrelated plugin or bypass auth.
- [ ] Implement a portable document bundle with text, actual image bytes, source hashes/provenance and a prepared prompt; verify export/reimport and corruption detection.
- [ ] Implement 'Ask ChatGPT' using a supported attachment/conversation handoff only after verifying the available API. No claim that ChatGPT subscription entitlements are API credits.
- [ ] Chapterize from verified heading/section evidence and implement accessible background-capable playback/resume.
- [ ] Evaluate an actually available authorized neural-voice API and quality settings. 'ChatGPT's own voice in UHD' is a requested future experience, not an exposed/implemented capability.

See `docs/SITE_MCP.md` and `docs/MOBILE_OCR_READER.md` for protocol and platform evidence. Voice generation and transcription are not implemented by the current alpha.
