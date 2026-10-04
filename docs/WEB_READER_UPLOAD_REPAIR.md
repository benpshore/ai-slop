# Reader and Upload repair — 2026-10-04

This records the initial reader patch. The subsequent combined implementation,
migration requirements, and final verification are in the
[web integration handoff](WEB_ALPHA_INTEGRATION_20261004.md).

Tracking: [child issue #184](https://github.com/benpshore/pdftextract/issues/184)
under [experience epic #178](https://github.com/benpshore/pdftextract/issues/178),
with source evidence under [#176](https://github.com/benpshore/pdftextract/issues/176)
and intake dependencies under [#179](https://github.com/benpshore/pdftextract/issues/179).
The existing `codex` issue label tracks this repository work; no release tag is
created. This patch branches from draft PR #175 at
`baafb472873750e7e84d32c3c8e6fa7a680e37db` and targets its integration branch,
`feat/native-toolkit-private-alpha-20261004`.

## Ownership and integration contract

This work owns `web/app/workspace.tsx`, primary reader styles, and focused UI
tests. Extraction/persistence and batch/storage remain separate workstreams.
The existing `DocumentRow`, `Extracted`, `QueueItem`, upload helpers, and private
image URL ownership checks remain the integration boundary. UI progress reports
the current stage's actual fraction where available, and is indeterminate when
that stage supplies no fraction. It is not an estimate of total completion time.

Sibling [#185](https://github.com/benpshore/pdftextract/issues/185) owns the
text-before-storage fast path on `perf/text-first-output-20261004` and supplies
an explicit workspace integration patch. Sibling
[#186](https://github.com/benpshore/pdftextract/issues/186) owns storage lifecycle
APIs/components on `feat/web-batch-storage-lifecycle-20261004`. These modules
must be integrated serially into the reader owner's branch after their reviews;
their behavior is not implied by this initial reader patch.

The web runtime remains separate from native TPE. Its browser PDF worker uses
`pdf-oxide-wasm` 0.3.77. This patch does not change native parsing, publish a Site,
merge main, release, change access, or remove existing user documents. Final Site
editing/publication belongs to the serialized parent handoff after all web
patches pass and source synchronization is reviewed.

The implementation and independent review were delegated to Astra sessions at
ultra effort. The executor exposes no independent double-speed switch, so this
record does not assert that such a switch was enabled. No `.agents/skills`
directory exists in the fetched checkout; root `AGENTS.md` and the branch's
handoff, requirements history, and epics were read before implementation.

## User-visible changes

- **Upload** is labeled and exposes multiple files, a supported folder picker,
  and photos. Each selection appends to the same queue. Supported mixed drops
  preserve ordinary files even when only directory entries expose the entry API.
- Reading is the default, with explicit **Plain text**, **Copy Markdown**, and
  **Download Markdown** controls. Markdown files render through pinned
  `marked` 18.0.14 and the existing sanitizer; ordinary text/CSS/XML remains
  literal, and external/unowned images and active content remain excluded.
- Background imports preserve the reader. A ready result has an explicit Open
  action that moves focus to the reader; selecting a queue or saved document
  also makes the reader reachable on mobile. Diagnostics remain inside details.
- Current-stage progress remains available with Imports collapsed. The
  translucent status uses a reduced-motion-aware spinner; percentages describe
  the measured stage, not overall completion. Save-in-progress no longer asks
  for a retry. Failed saves retain readable/copyable results and an explicit retry.
- Navigation guards prevent stale loads from replacing newer results, Back
  leaving a loading state behind, and upload completion replacing a pending
  saved-document URL. Reading mode survives navigation/recovery.

## Verification

Use the repository's pinned pnpm 11.25.0 and frozen lockfile. Recreate generated
WASM/OCR assets using the existing copy scripts. The ignored local binding file
contains only `{"d1":"DB","r2":"BUCKET"}`; it contains no deployment identity.

Existing verification completed in this executor:

- HTML cleanup, Unicode, DOI, lazy images/captions, table and feed regressions;
  five structured HTML/Atom/RSS review cases.
- Import routing, >8 MiB multipart transfer, cancellation and commit recovery.
- Office fixtures; 21 archive/OCR checks using Node/WASM and explicitly
  documented storage/image/worker shims.
- MCP contracts and actual local Workers R2/D1/DigestStream contracts.
- Ruff format/lint; 92 Python tests; 28 portable-source tests.

Focused final verification:

- `node scripts/test-web-reader.cjs`: **15 checks passed**. They cover labels,
  actual-stage progress, formatted/literal content, clipboard denial fallback,
  repeated/folder/mixed-drop intake, completion access, and navigation races.
- `node scripts/test-web-workspace.cjs`: **16 existing checks passed**, including
  Back/scroll/remount, owner-preserving retry, private-image sanitization, and
  preserved prior results after failed/cancelled re-reading.
- `node scripts/test-web-reader-browser.mjs`: **13 checks passed** in Chromium
  **151.0.7922.173**, using real React/DOM/CSS and synthetic extraction/service/
  recovery fixtures. This includes actual clipboard/download interaction,
  selection and Back/Forward, first queue growth while reading on mobile,
  cancellation/retry, 390/768px at 100/200% text, and reduced motion. Screenshots
  were visually reviewed; button overflow and a tall one-character drop hint
  were caught and fixed. No browser runtime errors were observed.
- TypeScript without incremental cache and the production build passed.
- `git diff --check` passed. Targeted UI ESLint has the same five
  `react-hooks/refs` errors and two warnings as baseline `baafb472`; it is not
  claimed clean. No new lint finding was introduced.

[Browser report with tested source hashes](validation/web-reader-browser-20261004.json)
and [390px/200% screenshot](validation/web-reader-390-200-20261004.png) are
synthetic evidence committed with this patch. Re-run commands from the repository
root, except TypeScript/build and web helper suites, which run from `web/`.
The component regression is included in `.github/workflows/web.yml`.

`uv audit --preview-features audit-command` could not contact
`https://api.osv.dev/v1/querybatch` through the executor proxy. `cargo`, `swift`
and `cmake` are not installed; their `AGENTS.md` checks were attempted and are
unavailable, not passed. No native source changed. Actual iPhone/iPad/VoiceOver
and private authenticated Site behavior require their own verification. The
parent's live browser probe stopped at ChatGPT login and did not bypass it. The
local Chromium probe separately failed with `ERR_TUNNEL_CONNECTION_FAILED`;
that local failure does not establish authentication behavior.

## Independent review and readiness

The reviewer reproduced concrete navigation failures with synthetic deferred
responses, verified their fixes, and reported no remaining blocking findings
in the final initial UI patch. Reviewed workspace SHA-256:
`67c79991fcf01056af722cbeb5d4a88739410075e96c264bf34fc3035546c96b`.
The reviewer independently ran the then-14-check component suite; the final
15th check adds mixed-folder-drop fallback coverage without changing UI source.
The final browser report identifies the same reviewed source.

The initial patch is ready for serialized integration of #185 and #186. Those
patches still require workspace wiring, affected tests and independent final
review. In particular, this initial UI patch does **not** move plain-text
projection ahead of original upload; #185 supplies that fast path. Regenerate
the source manifest after all integration edits. Final Site publication awaits
the parent's explicit handoff. The broader 50 GB batch target and claimed
100 GB allowance are not established by this UI patch.
