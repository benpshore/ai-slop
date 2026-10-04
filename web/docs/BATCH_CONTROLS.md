# Browser batch and storage control integration

Tracked in [#186](https://github.com/benpshore/pdftextract/issues/186), under storage epic [#179](https://github.com/benpshore/pdftextract/issues/179) and experience epic [#178](https://github.com/benpshore/pdftextract/issues/178). This change supplies reusable controls and lifecycle helpers; the UX integration owner must wire them into `app/workspace.tsx`. They are not proof of changed behavior on the published Site. Browser PDF extraction still uses `pdf-oxide-wasm`, separately from native TPE.

## Intake contract

`components/import-controls.tsx` exports:

- `ImportPicker({onFiles, disabled?})`: `onFiles` receives `{file: File, path: string}[]`. Append these selections to the existing queue. `path` preserves `webkitRelativePath`; the `File` and its original name remain unchanged. Files and folders use separate browser pickers. Repeated folder selections and repeated file selections add to the same queue. Unsupported folder pickers are disclosed.
- `ImportDropZone({onFile, onError, children, disabled?})`: streams additive discoveries to `onFile({file,path})`; per-path failures reach `onError(path,error)`. It handles mixed file/folder drops where legacy entry or modern handle APIs exist, repeated directory-reader batches, multiple roots and nested directories. Unmount aborts discovery. It only consumes file drops, allowing an outer handler to keep URL/text drop behavior.
- Owners keeping their existing drop area can instead call `filesFromDrop(event.dataTransfer, signal)` **inside the synchronous drop handler**, then iterate the returned async generator. Drag handles and files are captured before browser access expires. Do not defer the initial call until after an `await`.

`lib/import-queue.ts` exports `selectedImportFiles(files)` and `createImportItems(selected)` for the existing `QueueItem` shape. `createImportItems` assigns a new UUID for every explicit selection, including reimports of the same file. Do not deduplicate by filename, relative path, size or modification time. Display paths are not storage keys. Separate sources named `same.txt` must remain separate imports.

## Progress and actions

- `ImportBatchProgress({items})` reports saved/active/waiting/failed/cancelled/interrupted counts. Archive discoveries may increase the denominator. It does not average unrelated stage percentages or claim a whole-batch percentage.
- `ImportItemProgress({item})` accepts the existing `id`, `name`, `phase`, `progress`, `message` and optional `error`. Numeric `progress` is **0–100 for the current step**. Unknown progress uses an indeterminate element; waiting has no false progress bar. Supply measured upload byte/PDF page/archive member/OCR progress, with a matching `message`.
- `ImportItemControls({item,onCancel,onRetry,onRemove,canRetry?,removeDisabled?,hasLocalOnlyData?})` displays separate cancel, retry/save-again and remove controls. Removal is disabled during active phases. Pass `hasLocalOnlyData=true` for an uncommitted local File/Blob or any recovery state removal would discard. `savePending` always triggers removal confirmation. The confirmation describes loss of the unfinished local recovery copy. `onRemove` is an async **local queue removal** callback and must never invoke the saved-document DELETE API.

Local removal must await cancellation/commit settlement, invalidate outstanding attempt callbacks, remove the queue item and update the owner snapshot. It must leave saved records/originals intact. Do not silently drop a failed/cancelled source or `savePending` result in a batch clear operation. Use the explicit per-item discard confirmation where the user actually requests removal of unfinished recovery data.

## Attempt and refresh contract

Create one stable `ImportAttemptRegistry` per mounted workspace. Begin each run with `attempt = registry.start(id)` and pass `attempt.signal` to upload/extraction/save work.

1. Gate progress and ordinary asynchronous result callbacks with `registry.isCurrent(attempt)` **and** continued queue-item existence.
2. On cancel, call `registry.cancel(id)` and keep the item in its active phase with “Cancelling…” until its pending operation settles. This prevents an overlapping retry/removal. Aborting a request does not prove that a server commit was rolled back.
3. A confirmed original/result commit receipt that arrives while cancellation settles can still be recorded with `registry.isLatest(attempt)`, which intentionally ignores the aborted signal. Preserve that receipt and any retained extraction before presenting `cancelImportItem(currentItem)` or the actual committed outcome. Do not use `isLatest` for ordinary progress or to resume extraction.
4. In `finally`, call `registry.finish(attempt)`. The old attempt cannot finish or overwrite a newer attempt. Only release other per-item controller/completion entries if they still belong to this attempt.
5. On local removal, call `registry.remove(id)` after settlement, then purge the item and checkpoint. On unmount call `registry.dispose()`. Late callbacks from a removed item must not recreate it.
6. `retryImportItem(item)` preserves original/record/result and sets `retrySave` when `savePending` is true. It rejects retry during an active phase. An unavailable retained result or a stored source without a record needs source reselection, not a pretend successful retry.
7. `restoreImportItems(saved,live)` marks active saved entries as interrupted without restarting them, preserves local File/Blob and unsaved result references, and lets live entries win on an identical queue ID. Persist using the existing owner-keyed IndexedDB API. The helper does not claim persistence succeeded; show checkpoint failures and preserve in-memory recovery payloads.

The existing archive iterator owns temporary OPFS member files. A failed checkpoint does not make that member durable. Retain the saved archive and provide archive retry when member bytes are unavailable. Never implement these controls by recursively clearing the shared `tpe-import-tmp` directory.

## Storage callback contract

`DeleteStoredDocumentButton({documentName,onDelete,disabled?})` shows an explicit confirmation naming the saved document and its original/result/attachments. `onDelete` is called only after confirmation. While pending, duplicate clicks and dismissal are blocked; errors remain visible for retry. Wire it to the owner-scoped `deleteStoredDocument(id,{confirmDocumentId:id})` API helper. Disable deletion until every queue writer for that record has settled. After success, remove every local reference to the record, invalidate list/reader requests and selection, and checkpoint the purged snapshot. A stale reader/list response must not restore a deleted document.

`ClearCachedFilesButton({onClear,disabled?})` is labelled **Clear saved copies**. Its confirmation says local completed-import bytes/text are cleared, saved library documents/originals remain, and unfinished work/unsaved results are preserved. Wire it to `clearSavedWorkspaceCache(userId)` and reconcile the returned snapshot into live queue state before a later autosave can restore cleared bytes. Do not call document DELETE, remove another owner's snapshot, clear all IndexedDB databases, or clear shared OPFS staging files. Coordinate this with the workspace's checkpoint serialization.

The components do not perform network requests or storage writes themselves. Storage failures must reject their callbacks; otherwise the dialog would show completion without evidence.

## Verification and remaining acceptance

Run from `web/`:

```sh
node scripts/test-import-queue.mjs
node scripts/test-import-controls.mjs
```

The queue script checks synthetic nested/mixed drops, batched directory readers, duplicate names/reimports, unsupported APIs, late discovery cancellation, attempt replacement/removal races, save-payload preservation, refresh merging and factual aggregate counts. The controls script mounts the actual React components in jsdom and verifies destructive confirmation, cancelled confirmation, visible failure/retry, duplicate clicks, cache-scope copy, cancel/retry/remove separation, stage progress, repeated selections and a drop callback. It never deletes stored documents or accesses user data.

These are Node/React/jsdom checks with synthetic File/drag objects and callbacks. They do not prove real browser permission behavior, IndexedDB eviction recovery, iOS background continuation, a 50 GB batch or any account quota. The UX owner must run integrated workspace checks and actual target-browser acceptance after wiring; Site publication remains a separate synchronized task.
