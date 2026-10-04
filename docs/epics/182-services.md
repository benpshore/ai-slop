# [Integration][Epic] Deploy and qualify native, GROBID, and Docling processing services

GitHub: https://github.com/benpshore/pdftextract/issues/182
Created: 2026-10-04T01:11:40Z
Status at export: open

Recorded 2026-10-04T01:09:25Z (2026-10-03 19:09:25 America/Denver). This epic is part of the user's requested Dot/Codex handoff for **benpshore/pdftextract**. Implementation is tracked in [PR #175](https://github.com/benpshore/pdftextract/pull/175); the core repair baseline is [merged PR #173](https://github.com/benpshore/pdftextract/pull/173), main commit e27e1fb28a5b40dbca7f517c6fe396e6f11ed4ec.

Current distinction: PR #175's published head was 77c58ec89a1c9c07425b77dec90ab00316aa07be at this record's start; follow-up fixes are being validated for the next revision. The live Site's first alpha uses upstream PDF Oxide WASM, not the complete native Rust engine. See the final handoff/source manifest for the exact subsequent publication revision.

## Current boundary
The private Site is a Workers web app with browser extraction and R2/D1 storage. A deployed native Rust/GROBID/ML processing backend is still absent. Browser upstream PDF Oxide WASM is not a substitute for this service integration.

## Implemented modules
- `src/grobid.rs`, `grobid_cli.rs`: explicit opt-in supervised HTTP client for configured GROBID, streamed immutable input snapshot, version evidence, TEI hash/raw output/typed metadata/citations/coordinates; no automatic external PDF upload or service deployment. Nine HTTP/CLI tests cover >64MiB input, >16MiB response, redirects/errors/credentials and explicit optional limits.
- `docling-text`: retained pure per-page parser without PDFium/models/processes; native text/links/images and uncertainties retained. Existing full `docling` ML path remains separate and not supervised-CLI enabled.
- No new runtime credentials are embedded in GitHub. Site auth/storage are managed; optional external services need their own runtime credential configuration.

## Required next implementation
- [ ] Deploy a separately provisioned native processing worker/service with authenticated job submission and owner-bound artifact retrieval, durable jobs/cancellation/retries and versioned results.
- [ ] Deploy/configure a pinned GROBID model server; benchmark real model output versus labeled citation/fulltext truth and preserve remote cancellation limits.
- [ ] Compile/run/qualify full Docling ML models and OCR runtime; audit process containment and model provenance before routing user data there.
- [ ] Connect the Site to these services through HTTP job contracts, never by bypassing Site auth or inventing in-Worker native execution.
- [ ] Evidence-driven page/element routing with cost/latency budgets, user-data provenance and opt-in remote metadata consolidation.

Proposed execution owner: dedicated Codex/Dot service workstream; Site-capable agent handles frontend integration/publication after contracts are tested.
