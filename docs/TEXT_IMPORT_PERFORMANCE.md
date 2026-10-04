# Plain-text first output: implementation and integration handoff

Tracking: [#185](https://github.com/benpshore/pdftextract/issues/185), under [experience epic #178](https://github.com/benpshore/pdftextract/issues/178), with [storage #179](https://github.com/benpshore/pdftextract/issues/179) and [source evidence #176](https://github.com/benpshore/pdftextract/issues/176). UI integration remains owned by [#184](https://github.com/benpshore/pdftextract/issues/184).

This change supplies a lightweight text module and an exact, tested workspace integration patch. It does **not** edit `web/app/workspace.tsx`: the UX owner must integrate the patch with their UI work. The published Site has not changed. Browser PDF remains `pdf-oxide-wasm` 0.3.77; this work does not run or modify native TPE.

## Observed dependency and scope

The user reported about ten seconds for the two-word paste `some text`. We fetched PR #175 and its handoff/history/epics before work; its head was `baafb472873750e7e84d32c3c8e6fa7a680e37db`. The branch was created from that exact head. `AGENTS.md` was read; there is no checkout `.agents/skills` directory.

At that head, `runItem` awaits `uploadOriginal` before decoding and publishing the result. Even nine bytes take three sequential requests: multipart session creation, part upload, and commit. Original commit completes R2 multipart, rereads bytes through Workers `DigestStream` for SHA-256, inserts the D1 document and writes the completion receipt. Result storage takes another three requests, but **baseline text is already visible before result storage**. Thus the specific avoidable dependency is original storage before first output. This is source evidence, not attribution of the hosted ten seconds to network, hashing or CPU.

The baseline does not instantiate PDF/OCR on a text import. It does eagerly load other application/parser code. The new module has no runtime imports, workers, WebAssembly, DOM, network or hashing. The patch leaves existing application imports in place; it does not claim a cold application-load improvement. The serialized import queue and selection policy remain the UX/batch owners' responsibility, so this patch does not make a paste jump ahead of an existing long-running import.

## Integration contract

`prepareTextImport(file, {sourceUrl?, signal?})` returns `null` for the ordinary router, or `{original, result, timings}`. `result.text` and `result.markdown` preserve source whitespace and Unicode; printed DOI evidence and filename/MIME/byte count/mtime/encoding/source URL provenance remain available. No original hash is invented. `timings` separates detection, decoding and projection and is not placed in the normal UI or persisted as a claimed network measurement.

The gate conservatively accepts text/plain or `.txt`, `.md`, `.markdown` names only when content does not indicate another format. To preserve the existing authoritative router, Markdown starting `[` or `{` still falls back to that router. This existing classification limitation is not silently redefined. Unsupported/invalid encodings and binary control bytes also fall back. Text decoding honors BOM and explicit MIME charset; arbitrary prose containing `encoding=` or `charset=` is not interpreted as an encoding directive.

Apply/adapt [the integration patch](integration/text-first-workspace.patch) against the verified PR #175 workspace source:

```sh
git apply --check docs/integration/text-first-workspace.patch
git apply docs/integration/text-first-workspace.patch
```

The patch sets an active phase before asynchronous preparation (so Cancel aborts the controller), publishes `result` and `savePending` before upload, and yields one task for React to commit. A task yield is not a guaranteed paint. Original and result still use the existing upload/commit/recovery protocol. A failed original upload retains the File and readable result in the existing owner-scoped IndexedDB checkpoint; a failed result save reuses its confirmed original. Re-reading an existing original keeps the existing path and prior-result preservation behavior. Active saving has honest status and no premature retry notice. The original hash appears only after the authoritative storage receipt.

`createTextImportSession(draft, storage, existingRecord?)` is an optional reusable controller for other callers: save attempts yield before persistence, coalesce concurrent calls, preserve confirmed original receipts, and resolve with saved/unsaved/cancelled outcomes while retaining readable output. The workspace patch uses its existing `persistItem` lifecycle instead. Session unit tests alone are not evidence of workspace wiring; the browser tests below exercise the actual patched component.

## Measurements and tests

[Raw benchmark evidence](validation/text-import-benchmark.json) records exact baseline/candidate/helper hashes and every sample. This is a local synthetic component harness: real React workspace, upload client, browser File/Blob and IndexedDB, with a local HTTP multipart fixture. It omits the production framework, CSS, authentication and real R2/D1. It never contacts the private Site or uses user documents.

Cold browser means a fresh Chromium context and empty cache/IndexedDB; warm means preloaded code in the same context. Resource Timing verifies zero transferred JS bytes for the warm samples. Module fetch/evaluation is timed separately. Import timing starts at form submit **after hydration**, ends at exact text observed in the DOM, and separately records the second animation frame after that observation. Neither is a physical display/compositor measurement. Node cold means a fresh module identity within one process, excluding process startup.

Three cold and three warm local HTTP imports per variant, ten cold/warm Node pairs, and one intentionally delayed original-storage import per variant are recorded. An injected 3333 ms delay on each original request demonstrates how this dependency can produce about ten seconds. It does not reproduce the user's production connection or prove the cause of that report. Request timestamps, response completion, server fixture hashing, and in-memory persistence/acknowledgement are separate fields; overlapping client/server durations must not be added. The fixture's actual SHA-256 operation is not a measurement of production Workers hashing or storage.

Final measured run, Chromium 151.0.7922.173 / Node 24.19.0 on this Linux executor (milliseconds; browser rows are median of three unless marked single):

| Boundary | Baseline | Candidate with patch |
| --- | ---: | ---: |
| Cold submit → readable DOM | 107.0 | 24.6 |
| Warm submit → readable DOM | 70.7 | 23.4 |
| Cold submit → second-frame observation | 135.3 | 55.2 |
| Warm submit → second-frame observation | 76.0 | 48.6 |
| Delayed original storage → readable DOM, single run | 10060.4 | 17.3 |
| Delayed original storage → second-frame observation, single run | 10070.9 | 40.5 |

Cold readable ranges were 65.5–159.0 ms baseline and 23.1–26.2 ms candidate; warm ranges were 58.7–71.2 and 21.8–24.6 ms. Module import/evaluation was separate: cold medians 134.8/104.0 ms and warm medians 10.3/11.5 ms. Those small, noisy samples establish no module-load speedup; the candidate adds a small helper to the bundle. New-helper Node preparation medians were 0.351 ms on first call and 0.231 ms on the second call, excluding a median 0.801 ms fresh-module import. The raw report preserves every detection/decode/projection interval and every HTTP request.

The benchmark also asserts:

- exact `some text` readability and Markdown download while original storage is pending;
- zero Worker creation and zero WebAssembly compile/instantiate calls;
- original-upload and result-save failure, actual IndexedDB reload, retained readable/exportable output, successful retry, and no second original on result retry;
- cancellation during text preparation before any upload;
- active saving status without a premature retry prompt.

Commands (from `web/`, except the React regression):

```sh
node scripts/test-text-import.mjs
node scripts/test-import-flow.mjs
node node_modules/typescript/bin/tsc --noEmit --strict --target ES2022 --moduleResolution Bundler --module ESNext --skipLibCheck --lib ES2022,DOM lib/text-import.ts
node node_modules/eslint/bin/eslint.js lib/text-import.ts scripts/test-text-import.mjs scripts/benchmark-text-import.mjs
PLAYWRIGHT_MODULE=/path/to/playwright/index.mjs CHROMIUM_PATH=/usr/bin/chromium node scripts/benchmark-text-import.mjs
node ../scripts/test-web-workspace.cjs
```

The new module's 18 contract groups, existing multipart/routing regressions, and 16 baseline React regressions pass. Accepted fixtures are also checked against the actual upload client's authoritative kind detector. Focused strict TypeScript and ESLint pass. The browser benchmark bundles the actual candidate from the patch without modifying the UX-owned file. Independent Astra ultra implementation and review were used; a separate double-speed setting is not exposed by this executor. Review caught and resolved a cancellation race during preparation and format-signature parity errors. Final independent review reports no remaining code blocker at helper SHA-256 `52dff7f83bc3f7f3419c0093c21caa22a5248f3674a40e82489d5c2393e38ac3`, also recorded in the final benchmark. The regenerated source manifest verifies 156 portable files.

Full `tsc --noEmit --incremental false` initially stopped on the checkout's absent `.openai/hosting.json` imported by `vite.config.ts`. It passes with the existing web CI's temporary **local-only** `{d1:"DB",r2:"BUCKET"}` fixture; that file is removed after the check and contains no deployed identity. All other web TypeScript, including a compiler-host substitution of the proposed workspace patch, also passes separately. The focused text regression is added to web CI. No broad Rust/Swift/CMake builds were duplicated for this web-only task; those repository-wide gates are not claimed passed. Source export/verification regenerates the portable manifest for this source-only revision, not the published Site.

## Remaining acceptance

The UX owner must integrate this patch (including cancellation and honest status), regenerate the combined source manifest and repeat the browser checks against the final UI. Final Site editing/publication is a separate serialized operation. Private Site authentication is unavailable in this task; hosted network/server timing, real R2/D1 throughput, mobile Safari, and iPhone/iPad rendering remain unmeasured. No zero-latency, deployment, native-backend or large-batch guarantee follows from these fixtures.
