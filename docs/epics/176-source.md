# [Web app][Epic] Reproducible Site source, release evidence, and Dot handoff

GitHub: https://github.com/benpshore/pdftextract/issues/176
Created: 2026-10-04T01:11:36Z
Status at export: open

Recorded 2026-10-04T01:09:25Z (2026-10-03 19:09:25 America/Denver). This epic is part of the user's requested Dot/Codex handoff for **benpshore/pdftextract**. Implementation is tracked in [PR #175](https://github.com/benpshore/pdftextract/pull/175); the core repair baseline is [merged PR #173](https://github.com/benpshore/pdftextract/pull/173), main commit e27e1fb28a5b40dbca7f517c6fe396e6f11ed4ec.

Current distinction: PR #175's published head was 77c58ec89a1c9c07425b77dec90ab00316aa07be at this record's start; follow-up fixes are being validated for the next revision. The live Site's first alpha uses upstream PDF Oxide WASM, not the complete native Rust engine. See the final handoff/source manifest for the exact subsequent publication revision.

## Problem and scope
The user needs every aspect of the ChatGPT Site linked to this repository and clearly identified as the web application, plus a full, timestamped work/requirements history. An app screenshot or a compiling WASM frontend is not evidence that the native engine is connected.

## Implemented or under final validation
- `web/` contains the app, API routes, auth, schema/migrations, worker, parsers, UI, build and asset-copy recipes. Package identity is `tpe-web-app`.
- `scripts/site_source.py` exports/verifies/imports a deterministic SHA-256/mode/size source manifest into a new staging directory. It rejects tampering, traversal, symlinks, and secret/deployment-state inclusion.
- Web CI builds the app and runs focused UI/parser/Workers tests independently of native Rust CI.
- `docs/USER_REQUIREMENTS_HISTORY.md` preserves all visible user requests; earlier unavailable timestamps are explicitly not invented.
- `docs/HANDOFF_TO_DOT.md`, `WEB_APP_INTEGRATION_AUDIT.md`, and `WEB_ALPHA.md` distinguish implemented, tested, deployed, and planned work.

## Completion criteria
- [ ] Publish the corrected owner-private Site and record deployment version, source SHA, repository revision, portable source manifest hash and terminal status.
- [ ] Verify every portable application file in the publication matches the GitHub manifest; enumerate intentional exclusions: runtime credentials/configuration, user uploads/database contents, installed dependencies and reproducible generated WASM/OCR assets.
- [ ] Attach exact-head CI status and reproducible commands; identify locally unavailable browser/device and native PID-observability tests.
- [ ] Export a self-contained handoff document bundle and link all sibling epics/PRs.

## Proposed next owner
Site-capable agent owns private publication and source exchange. Codex/Dot owns repo-based implementation and tests. Do not bypass Site identity checks or embed credentials to make an external build work.
