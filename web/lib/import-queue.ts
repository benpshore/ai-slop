/** Browser intake and queue lifecycle helpers. No storage or network side effects. */
export type ImportPhase = 'waiting' | 'fetching' | 'uploading' | 'extracting' | 'saving' | 'saved' | 'failed' | 'cancelled' | 'interrupted';
export type SelectedImportFile = { file: File; path: string };
export type DiscoveredImportFile = SelectedImportFile | { path: string; error: string };
export type ImportProgressItem = { id: string; name: string; phase: ImportPhase; progress: number | null; message: string; error?: string; savePending?: boolean };
export type RecoverableImportItem = ImportProgressItem & {
  source: { type: string };
  record?: unknown;
  result?: unknown;
  retrySave?: boolean;
};

export function isActiveImport(phase: ImportPhase): boolean {
  return ['waiting', 'fetching', 'uploading', 'extracting', 'saving'].includes(phase);
}

export function importPhaseLabel(phase: ImportPhase): string {
  return { waiting: 'Waiting', fetching: 'Fetching', uploading: 'Saving original', extracting: 'Extracting', saving: 'Saving result', saved: 'Saved', failed: 'Needs attention', cancelled: 'Cancelled', interrupted: 'Interrupted' }[phase];
}

/** Counts are real queue outcomes. Phase percentages must never be averaged into a batch percentage. */
export function summarizeImports(items: readonly Pick<ImportProgressItem, 'phase'>[]) {
  const counts = { total: items.length, saved: 0, active: 0, waiting: 0, failed: 0, cancelled: 0, interrupted: 0 };
  for (const { phase } of items) {
    if (isActiveImport(phase)) { counts.active++; if (phase === 'waiting') counts.waiting++; }
    else if (phase === 'saved' || phase === 'failed' || phase === 'cancelled' || phase === 'interrupted') counts[phase]++;
  }
  return counts;
}

/** Display-only relative path; never use this value as a storage key. */
export function importRelativePath(file: File, path = file.webkitRelativePath || file.name): string {
  const normalized = path.replace(/\\/g, '/');
  if (/^[\/]|^[a-z]:\/|[\x00-\x1f\x7f]/i.test(normalized) || normalized.split('/').includes('..')) {
    throw new Error('The selected file has an invalid relative path.');
  }
  const parts = normalized.split('/').filter(part => part && part !== '.');
  if (!parts.length) throw new Error('The selected file has no name.');
  return parts.join('/');
}

export function selectedImportFiles(files: Iterable<File>): SelectedImportFile[] {
  return Array.from(files, file => ({ file, path: importRelativePath(file) }));
}

/** Each explicit selection is a new import, even when its path/size/mtime matches an earlier one. */
export function createImportItems(files: readonly SelectedImportFile[], id = () => crypto.randomUUID()) {
  return files.map(({ file, path }) => ({
    id: id(), name: importRelativePath(file, path), source: { type: 'file' as const, file },
    phase: 'waiting' as const, progress: null, message: 'Waiting to import.',
  }));
}

type DropEntry = {
  isFile: boolean;
  isDirectory: boolean;
  name: string;
  file?: (done: (file: File) => void, fail: (error: DOMException) => void) => void;
  createReader?: () => { readEntries: (done: (entries: DropEntry[]) => void, fail: (error: DOMException) => void) => void };
};
type DropHandle = { kind: 'file'; name: string; getFile: () => Promise<File> }
  | { kind: 'directory'; name: string; values: () => AsyncIterable<DropHandle> };
type DropItem = {
  kind: string;
  getAsFile: () => File | null;
  webkitGetAsEntry?: () => DropEntry | null;
  getAsFileSystemHandle?: () => Promise<DropHandle | null>;
};
type DropData = { items: ArrayLike<DropItem>; files: ArrayLike<File> };
const errorMessage = (error: unknown) => error instanceof Error ? error.message : String(error);

async function* readEntry(entry: DropEntry, path: string, signal?: AbortSignal): AsyncGenerator<DiscoveredImportFile> {
  signal?.throwIfAborted();
  try {
    if (entry.isFile && entry.file) {
      const file = await new Promise<File>((resolve, reject) => entry.file!(resolve, reject));
      signal?.throwIfAborted();
      yield { file, path: importRelativePath(file, path) };
    } else if (entry.isDirectory && entry.createReader) {
      const reader = entry.createReader();
      for (;;) {
        const entries = await new Promise<DropEntry[]>((resolve, reject) => reader.readEntries(resolve, reject));
        signal?.throwIfAborted();
        if (!entries.length) break;
        for (const child of entries) yield* readEntry(child, path + '/' + child.name, signal);
      }
    } else throw new Error('This browser cannot read this folder. Use Add folder or select its files.');
  } catch (error) {
    signal?.throwIfAborted();
    yield { path, error: errorMessage(error) };
  }
}

async function* readHandle(handle: DropHandle, path: string, signal?: AbortSignal): AsyncGenerator<DiscoveredImportFile> {
  signal?.throwIfAborted();
  try {
    if (handle.kind === 'file') {
      const file = await handle.getFile();
      signal?.throwIfAborted();
      yield { file, path: importRelativePath(file, path) };
    } else {
      for await (const child of handle.values()) {
        signal?.throwIfAborted();
        yield* readHandle(child, path + '/' + child.name, signal);
      }
    }
  } catch (error) {
    signal?.throwIfAborted();
    yield { path, error: errorMessage(error) };
  }
}

/**
 * Call synchronously inside the drop handler. Browser drag data access expires
 * after that handler, so entries/files/handle promises are captured before await.
 * Results stream additively; a failed child does not discard successful siblings.
 */
export function filesFromDrop(data: DropData, signal?: AbortSignal): AsyncGenerator<DiscoveredImportFile> {
  const fallback = Array.from(data.files);
  const captured = Array.from(data.items).filter(item => item.kind === 'file').map(item => {
    try {
      const entry = item.webkitGetAsEntry?.();
      const file = item.getAsFile();
      // Legacy entries are sufficient where available. Starting a handle request
      // now also avoids Chromium's same-event-loop access restriction.
      const handle = !entry && item.getAsFileSystemHandle
        ? item.getAsFileSystemHandle().then(value => ({ value }), error => ({ error })) : undefined;
      return { entry, file, handle };
    } catch (error) { return { error }; }
  });
  return (async function* () {
    signal?.throwIfAborted();
    if (!captured.length) {
      for (const file of fallback) { signal?.throwIfAborted(); yield { file, path: importRelativePath(file) }; }
      return;
    }
    for (const item of captured) {
      signal?.throwIfAborted();
      if ('error' in item) { yield { path: 'Dropped item', error: errorMessage(item.error) }; continue; }
      if (item.entry) { yield* readEntry(item.entry, item.entry.name, signal); continue; }
      const result = await item.handle;
      signal?.throwIfAborted();
      if (result && 'value' in result && result.value) { yield* readHandle(result.value, result.value.name, signal); continue; }
      if (item.file) { yield { file: item.file, path: importRelativePath(item.file) }; continue; }
      yield { path: 'Dropped item', error: result && 'error' in result ? errorMessage(result.error) : 'This browser cannot read this dropped folder. Use Add folder or select its files.' };
    }
  })();
}

export type ImportAttempt = { readonly id: string; readonly signal: AbortSignal };

/** A retry/removal invalidates old callbacks even when a worker ignores abort. */
export class ImportAttemptRegistry {
  private attempts = new Map<string, { attempt: ImportAttempt; controller: AbortController }>();

  start(id: string): ImportAttempt {
    this.remove(id);
    const controller = new AbortController();
    const attempt = Object.freeze({ id, signal: controller.signal });
    this.attempts.set(id, { attempt, controller });
    return attempt;
  }
  isLatest(attempt: ImportAttempt): boolean { return this.attempts.get(attempt.id)?.attempt === attempt; }
  isCurrent(attempt: ImportAttempt): boolean { return this.isLatest(attempt) && !attempt.signal.aborted; }
  cancel(id: string): void { this.attempts.get(id)?.controller.abort(); }
  remove(id: string): void { this.cancel(id); this.attempts.delete(id); }
  finish(attempt: ImportAttempt): void { if (this.isLatest(attempt)) this.attempts.delete(attempt.id); }
  dispose(): void { for (const id of this.attempts.keys()) this.remove(id); }
}

/** Call after pending work settles. Preserve original/record/result; cancellation is never deletion. */
export function cancelImportItem<T extends ImportProgressItem>(item: T): T {
  return isActiveImport(item.phase) ? { ...item, phase: 'cancelled', progress: null, message: 'Import cancelled. Saved originals and local recovery are kept.' } : item;
}

export function retryImportItem<T extends RecoverableImportItem>(item: T, saveOnly = !!item.savePending): T {
  if (!['failed', 'cancelled', 'interrupted'].includes(item.phase)) throw new Error('Wait for the current import to settle before retrying.');
  if (saveOnly && (!item.record || !item.result)) throw new Error('The retained result is unavailable. Reselect the source to retry.');
  if (!saveOnly && item.source.type === 'stored' && !item.record) throw new Error('Reselect this source file to retry.');
  return { ...item, phase: 'waiting', progress: null, error: undefined, retrySave: saveOnly, message: saveOnly ? 'Waiting to retry the save.' : 'Waiting to retry.' };
}

/** Restore metadata and File/Blob recovery payloads; interrupted work does not auto-restart. */
export function restoreImportItems<T extends ImportProgressItem>(saved: readonly T[], live: readonly T[] = []): T[] {
  const existing = new Set(live.map(item => item.id));
  return [...saved.filter(item => !existing.has(item.id)).map(item => isActiveImport(item.phase)
    ? { ...item, phase: 'interrupted' as const, progress: null, message: 'Interrupted when this page closed. Retry to continue.' }
    : item), ...live];
}
