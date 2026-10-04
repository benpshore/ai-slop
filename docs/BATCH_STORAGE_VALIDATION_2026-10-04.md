# Batch/storage validation — 2026-10-04

This records the independently reviewed module patch. Its subsequent workspace
wiring and combined verification are in the
[web integration handoff](WEB_ALPHA_INTEGRATION_20261004.md).

Tracking: [#186](https://github.com/benpshore/pdftextract/issues/186), child of [#179](https://github.com/benpshore/pdftextract/issues/179), with controls for [#178](https://github.com/benpshore/pdftextract/issues/178).

Base fetched and verified from draft PR #175: `baafb472873750e7e84d32c3c8e6fa7a680e37db`, branch `feat/native-toolkit-private-alpha-20261004`. Implementation branch: `feat/web-batch-storage-lifecycle-20261004`. The draft PR targets that integration branch. Native repair branches were not edited. The bounded implementation agent and independent reviewer used the requested Astra/ultra configuration; the delegation tool exposed no separate double-speed switch.

## Result and integration boundary

The code adds reusable additive intake/progress/cancel/retry/remove controls, an explicit saved-document deletion dialog/client/API, durable deletion markers, late-writer guards, and owner-specific clearing of redundant saved IndexedDB copies. `web/app/workspace.tsx` is unchanged and **must be wired by the UX integration owner**. No live Site behavior or deployment is claimed. Browser PDF parsing remains upstream `pdf-oxide-wasm` 0.3.77, separate from native TPE.

Exact wiring contracts: [`web/docs/BATCH_CONTROLS.md`](../web/docs/BATCH_CONTROLS.md) and [`web/docs/STORAGE_LIFECYCLE.md`](../web/docs/STORAGE_LIFECYCLE.md). Deployment must apply new D1 migration `0002_document_deletions.sql` before the API revision. No deployed database migration or document deletion was performed here.

## Executed local checks

| Command | Result |
| --- | --- |
| `cd web && node node_modules/typescript/bin/tsc --noEmit` | Passed. |
| `cd web && node scripts/copy-pdf-wasm.mjs && node scripts/copy-ocr-assets.mjs && npm run build` | Passed, five Vite/vinext stages; local-only logical D1/R2 bindings. |
| `cd web && node scripts/test-import-queue.mjs` | 12 grouped synthetic checks passed: nested/mixed selection, directory batches, modern handle capture, duplicate names/reimport, missing browser API, cancellation/late responses, attempt replacement/removal, failed-save retention, refresh, aggregate counts. |
| `cd web && node scripts/test-import-controls.mjs` | 7 grouped actual React/jsdom checks passed: confirmation/cancel/failure/retry, duplicate clicks, cache scope, per-item actions, progress, additive input, drop callback. |
| `cd web && node scripts/test-workspace-storage.mjs` | Passed: in-memory IndexedDB owner scope, saved-copy clear, pending File/result/draft preservation, sequenced writes, stale saved-byte checkpoints, refresh, failed-write recovery. Not general multi-tab queue merging. |
| `node --experimental-strip-types scripts/test-site-lifecycle-workers.mjs` | 11 grouped local Miniflare D1/R2 checks passed: foreign-owner rejection without R2 access; exact route confirmation; paginated cleanup and neighbor retention; completed receipts; in-flight original insertion; late result/asset receipts; legacy PATCH; late captured image; delayed upload-session creation; retryable partial cleanup. |
| `node --experimental-strip-types scripts/test-site-uploads-workers.mjs` | Passed real local R2 multipart/D1 CAS/hash/ownership/lost-receipt/stale-save contracts with new deletion table. |
| `node scripts/test-web-workspace.cjs` | Existing 16 React integration checks passed with mocked network/storage. Workspace wiring unchanged. |
| `cd web && node scripts/test-import-flow.mjs` | Passed routing, >8 MiB synthetic multipart transfer, cancel and lost-save-response recovery. |
| Focused ESLint on new controls, lifecycle, client, queue and cache modules | Passed after hydration-safe folder capability detection fix. |
| `uv run ruff format && uv run ruff check && uv run pytest` | Passed: 100 files unchanged, lint clean, 92 Python tests. Writable temporary `UV_CACHE_DIR` and `UV_PYTHON_INSTALL_DIR` needed in this environment. |

Source exchange checks also passed: `uv run pytest web/tests/test_site_source.py` (28 tests) and `scripts/site_source.py export` (164 portable files). The regenerated manifest describes this repo revision, not deployment parity.

New scripts are wired into `.github/workflows/web.yml`. Fixtures and cleanup use synthetic data and disposable test-owned storage only. Independent review found no blocking issue for this bounded scope; tests forced upload finalization/deletion interleavings, exercised actual route confirmation and retained neighboring owner data.

## Required checks unavailable here

- `uv audit --preview-features audit-command`: failed after retries because the environment proxy could not connect to `https://api.osv.dev/v1/querybatch`. Not counted as passed.
- Required `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`, `swift build && swift test`, and `cmake -S . -B build && cmake --build build && ctest --test-dir build` could not start: `cargo`, `swift`, and `cmake` are not installed. No native source changed. Hosted checks remain required before integration/merge.

## Remaining acceptance

Wire controls/attempt guards/cache/delete callbacks into the UX-owned workspace, then test selection invalidation and delayed list/reader responses there. General cross-tab whole-snapshot merge, archive-member recovery after failed checkpoint, durable multipart resume, abandoned-upload garbage collection, indexed/durable server cleanup and real iPhone/iPad permission/eviction behavior remain unfinished. R2/D1 cleanup is retryable rather than crash-atomic physical erasure. Large-batch/account-quota acceptance is unmeasured; no 50 GB or unlimited-capacity claim is made.
