# [Web app][Epic] 50 GB mixed-batch uploads, durable resume, and storage lifecycle

GitHub: https://github.com/benpshore/pdftextract/issues/179
Created: 2026-10-04T01:11:38Z
Status at export: open

Recorded 2026-10-04T01:09:25Z (2026-10-03 19:09:25 America/Denver). This epic is part of the user's requested Dot/Codex handoff for **benpshore/pdftextract**. Implementation is tracked in [PR #175](https://github.com/benpshore/pdftextract/pull/175); the core repair baseline is [merged PR #173](https://github.com/benpshore/pdftextract/pull/173), main commit e27e1fb28a5b40dbca7f517c6fe396e6f11ed4ec.

Current distinction: PR #175's published head was 77c58ec89a1c9c07425b77dec90ab00316aa07be at this record's start; follow-up fixes are being validated for the next revision. The live Site's first alpha uses upstream PDF Oxide WASM, not the complete native Rust engine. See the final handoff/source manifest for the exact subsequent publication revision.

## Required behavior
At 2026-10-04T00:41:23Z the user required removal of size caps. At 00:53:31Z they specified a 50 GB mixed-media batch. At 00:54:38Z they stated their account has a 100 GB allowance. This allowance is user-provided information, not a completed Site capacity/load test.

## Implemented foundation
- Originals, results and embedded assets use R2 multipart transfers with 8 MiB-or-larger transport chunks. Chunk size is not a total-file acceptance cap.
- Input size/page/text/feed/archive-total caps were removed from the web app; native CLI source/output byte limits are opt-in.
- Original SHA-256 is computed through Workers DigestStream rather than buffering the whole original. Result GET and MCP use streamed reads.
- D1 compare-and-swap selects immutable result objects. Completion receipts recover a lost final response and reject stale overwrites.
- `scripts/test-site-uploads-workers.mjs` verifies real R2/D1/DigestStream, ownership, missing-receipt recovery and deterministic concurrent stale-save rejection.
- ZIP/TAR/GZIP expansion stages one member at a time to OPFS; actual quota failures and cancellation clean temporary files.

## Not yet implemented or proven
- [ ] Durable, verifiable per-part upload resume across reload, network loss, browser eviction and another device. Current receipt recovery is finalization recovery, not complete resumable ingestion.
- [ ] Server-side durable queue/lease/checkpoint scheduling so phone tab suspension does not stop heavy processing.
- [ ] A measured 50 GB mixed workload with bytes/counts/checksums, interruption/retry, memory/disk/network traces, duplicate prevention and complete source/result inventory.
- [ ] Verify actual Site R2/account entitlements through the deployed environment; do not silently assume ChatGPT account storage and Site R2 share a quota.
- [ ] Abandoned multipart/session/asset cleanup and storage accounting; avoid unbounded orphan accumulation.
- [ ] Paginated large libraries and bounded active work without arbitrary document rejection.

Keep real platform failures visible; do not substitute arbitrary app caps or an unbounded browser heap.
