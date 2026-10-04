# Private web alpha integration — 2026-10-04

This is the serialized repository integration of three independently reviewed
web patches, based on draft [PR #175](https://github.com/benpshore/pdftextract/pull/175)
at `baafb472873750e7e84d32c3c8e6fa7a680e37db`.

| Input | Exact commit | Tracking |
| --- | --- | --- |
| Reader / Upload [#188](https://github.com/benpshore/pdftextract/pull/188) | `0d92038160dfbdf0ec39c36f5a816e5887759ef5` | #184, #178, #176 |
| Batch / storage [#187](https://github.com/benpshore/pdftextract/pull/187) | `5c0a3aa6d33ac22814e5906785aede118d772c87` | #186, #179, #178 |
| Text before storage [#189](https://github.com/benpshore/pdftextract/pull/189) | `8194834e779795692732651077cada2e10cb0767` | #185, #178, #179, #176 |

The integration branch is `fix/web-alpha-integration-20261004`. Constituent
commits are preserved in its Git ancestry. Reader wiring belongs to the web
integration owner; native repair PRs and native source are outside this change.
The browser still runs `pdf-oxide-wasm` 0.3.77, not native TPE.

## Contracts and combined acceptance

See [batch controls](../web/docs/BATCH_CONTROLS.md),
[storage lifecycle](../web/docs/STORAGE_LIFECYCLE.md),
[text preparation](TEXT_IMPORT_PERFORMANCE.md), and
[initial reader evidence](WEB_READER_UPLOAD_REPAIR.md).

The text helper must publish exact readable/copyable text before original
storage starts, while retaining the original and preview for recovery. A
preview with no confirmed record must retry original storage, not take a
result-only retry path. Re-reading existing saved records retains the prior
result if parsing fails. The shared queue remains serialized; this does not
make a new paste bypass an earlier long import.

Cancellation remains active until outstanding operations settle. Ordinary
callbacks require a current, non-aborted attempt; confirmed commit receipts may
still be retained for the latest attempt after cancellation. Local queue
removal cannot delete saved documents. Clearing redundant saved browser copies
must preserve current unfinished imports/results and cannot replace a changing
queue with an old snapshot. Confirmed saved-document deletion must exclude all
record writers, invalidate pending reader/library responses, and remain
retryable when physical cleanup is incomplete. Tests use synthetic data only.

A partial cleanup response removes the tombstoned document from the reader and
library immediately. The owner-scoped recovery snapshot retains a named manual
cleanup retry across an ordinary reload; recovery does not send a DELETE
automatically. Failed local recovery writes remain explicit and retryable.

## Required migration and eventual publication order

No migration or deployment is performed by this repository task. The Site owner
must use the managed deployment/database workflow and preserve the existing
private audience; local fixture bindings are not production identities.

1. Finish combined review and tests, record the exact final commit, regenerate
   and verify the portable source manifest, then synchronize that exact source
   through the parent's serialized Site handoff.
2. Apply `web/drizzle/0002_document_deletions.sql` to the intended D1 database
   **before deploying this API revision**. It creates the additive
   `document_deletions(id PRIMARY KEY, owner)` table. Preserve existing tables,
   document contents, and migration history. Verify the migration completed;
   stop deployment on a failed or wrong-database migration.
3. Deploy a coherent API/client revision with the tombstone checks in original,
   result, and asset paths. Do not expose the new delete UI against old APIs or
   deploy upload guards before their table exists.
4. Verify authenticated owner-only behavior with explicitly created synthetic
   documents: upload/read, confirmed deletion, retry after incomplete cleanup,
   denied cross-owner access, and rejection of late commits. Check actual Site
   behavior separately from local test results. Existing user documents are not
   test fixtures.

## Rollback constraints

The database change is additive. **Keep the migration and reserved deletion
IDs during rollback.** Removing them loses the evidence needed to reject late
original commits. A deleted document and its original bytes are not restored
by rolling back code; deletion is an explicitly confirmed destructive action.

Prefer a forward fix or a prepared compatibility rollback that preserves the
deletion guards and IndexedDB v2 recovery/cache policy while reverting only the
affected UI or text-preparation behavior. Do not blindly roll back to PR #175's
unguarded upload APIs once deletion has been accepted: an in-flight or resumed
old upload could recreate a deleted record. The old IndexedDB v1 opener is also
not a valid downgrade for devices already upgraded to v2. If a safe compatible
artifact is unavailable, pause the deployment workflow and resolve that gap
before enabling mutations. Do not erase recovery databases, OPFS staging, or
tombstones as a rollback shortcut.

## Evidence boundaries

Sibling benchmark numbers in `TEXT_IMPORT_PERFORMANCE.md` are local synthetic
measurements of the sibling's patched component. They are not production
timings and are not measurements of this final integrated screen. Its injected
ten-second storage delay demonstrates removal of a dependency, not attribution
of the user's reported hosted latency.

The prior reader's actual Chromium checks used synthetic services and storage;
the storage suite uses disposable local D1/R2 and synthetic IndexedDB tests.
Real iOS/iPad behavior, general multi-tab queue merging, durable multipart or
archive resume, crash-atomic byte erasure, 50 GB capacity, and actual account
quota remain unproven. The parent is separately handling the private Site login;
no authentication bypass, Site deployment, access change, release, main merge,
or deletion of existing user documents is performed here.

## Combined checks

The integration owner ran the unchanged module contracts once after combining
the dependency commits: 12 import-queue groups, 7 control groups, IndexedDB
cache/recovery scope, 18 text-preparation cases, 11 real local Workers deletion
groups, and the real local Workers upload suite all passed. HTML/feed cleanup,
five extraction review cases, import routing/multipart/commit recovery, Office,
21 archive/OCR checks, and MCP contracts also passed. These results identify
their synthetic services and Node/WASM shims; they do not validate hosted PDF
behavior or native TPE.

Ruff formatting/lint, all 92 repository Python tests, and all 28 portable-source
tests passed after combining the patches. The initial reader commit and both
dependency commits have terminal green repository and Web CI.

The final integrated TypeScript check (`tsc --noEmit --incremental false`) and
production build passed with the frozen dependencies and local fixture bindings.
Portable source export and verification passed for **167 files**; manifest
SHA-256: `4fcb739bbd8677cb08f78f1c5218bfef4cdf5c4896f49a35e872a873df3463c0`.

All final component suites passed: **15 reader checks, 16 existing workspace
checks, and 14 integrated lifecycle checks**. The lifecycle suite uses the real
confirmation controls, attempt registry, text helper, and deletion client with
synthetic network/storage. It covers confirmed commit receipts after Cancel,
strict checkpoint failure and retry, concurrent queue arrivals during cache
clearing, pre-deletion failure unlocking, cleanup recovery, and delayed local
restore/reader/library responses. These suites are included in Web CI.

The real Chromium **151.0.7922.173** harness passed **23 checks**, using actual
React, styles, text preparation, import lifecycle, and browser IndexedDB with
synthetic remote services. Exact `some text` was readable, copyable, and
downloadable before the held original upload completed. The suite verified
original-upload failure and File/preview recovery through reload, result-only
retry, cancellation before upload/Worker creation, no false empty reader while
opening a saved document, local removal versus saved deletion, cancelled
confirmations without writes, cache-clear failure/retry and unfinished payload
preservation, and partial deletion through stale reads, Back, reload, cache
clearing, and an explicit final cleanup retry. Desktop intake at 1280/1440px and
390/768px layouts at 100/200% text had no tested horizontal overflow. No browser
runtime errors were observed. The final cleanup-reload scenario also asserts
that unrelated saved documents remain visible without a manual Search.

[Browser report and source hashes](validation/web-alpha-integration-browser-20261004.json),
[mobile 200% screenshot](validation/web-alpha-integration-390-200-20261004.png),
and [cleanup recovery screenshot](validation/web-alpha-cleanup-recovery-20261004.png)
are committed synthetic evidence. Both screenshots were visually reviewed.
The exact browser-tested workspace SHA-256 is
`192077819d5208b68fc482b38d52b10f35dc5812097b7c0345eb3916da16999b`.

Independent review approved that exact final workspace source, with no
remaining blocking findings. It reproduced and verified the cancellation and
restoration races independently. A final regression caught startup cleanup
restoration hiding unrelated saved documents by invalidating the initial
library response; that defect was fixed and the affected component, browser,
TypeScript and production-build checks were rerun successfully.

The local Site navigation still failed with `ERR_TUNNEL_CONNECTION_FAILED`.
This is separate from the parent's authenticated baseline QA and does not
establish production authentication or final published behavior. No precise
production latency improvement is claimed.

`uv audit --preview-features audit-command` could not reach OSV through the
executor proxy. The required cargo, Swift and CMake commands were unavailable
in this executor, as recorded for the initial reader patch; no native code was
changed. Targeted ESLint reports the five existing React-ref errors and three
warnings (two effect-cleanup ref warnings and the existing image warning); it
is not claimed clean.
