/** Synthetic queue/drag tests. No network, user files, or persistent storage. */
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
const require = createRequire(import.meta.url);
const { build } = require(require.resolve('esbuild', { paths: [require.resolve('vite')] }));
const built = await build({ entryPoints: [new URL('../lib/import-queue.ts', import.meta.url).pathname], bundle: true, write: false, format: 'esm', platform: 'node' });
const { selectedImportFiles, createImportItems, filesFromDrop, ImportAttemptRegistry, cancelImportItem, retryImportItem, restoreImportItems, summarizeImports, importRelativePath } = await import(`data:text/javascript;base64,${Buffer.from(built.outputFiles[0].text).toString('base64')}`);
const checks = [];
function check(name, fn) { fn(); checks.push(name); }
async function collect(iterator) { const items = []; for await (const item of iterator) items.push(item); return items; }
const file = (name, text = 'synthetic fixture') => new File([text], name, { lastModified: 42 });
const first = file('same.txt'), second = file('same.txt', 'another fixture');
Object.defineProperty(first, 'webkitRelativePath', { value: 'Folder A/nested/same.txt' });
Object.defineProperty(second, 'webkitRelativePath', { value: 'Folder B/same.txt' });
const entries = createImportItems(selectedImportFiles([first, second, first]));
check('Nested paths and duplicate names survive additive selections; same-file reimport gets a new identity', () => {
  assert.deepEqual(entries.map(item => item.name), ['Folder A/nested/same.txt', 'Folder B/same.txt', 'Folder A/nested/same.txt']);
  assert.equal(new Set(entries.map(item => item.id)).size, 3);
  assert.strictEqual(entries[0].source.file, first);
  assert.strictEqual(entries[2].source.file, first);
  assert.equal(first.name, 'same.txt');
  for (const name of ['../outside', '/absolute', 'a/../b', 'a\0b']) assert.throws(() => importRelativePath(first, name));
});
const leaf = (name, value = file(name)) => ({ name, isFile: true, isDirectory: false, file: done => done(value) });
const directory = (name, batches) => ({ name, isFile: false, isDirectory: true, createReader: () => {
  let at = 0; return { readEntries: done => done(batches[at++] || []) };
} });
const bad = { name: 'unreadable.txt', isFile: true, isDirectory: false, file: (_done, fail) => fail(new DOMException('Permission denied', 'NotReadableError')) };
const dropItem = entry => ({ kind: 'file', webkitGetAsEntry: () => entry, getAsFile: () => null });
const mixed = await collect(filesFromDrop({ files: [], items: [
  dropItem(directory('one', [[leaf('same.txt'), bad], [directory('nested', [[leaf('same.txt')]])]])),
  { kind: 'file', getAsFile: () => second },
  dropItem(directory('two', [[leaf('same.txt')]])),
] }));
check('Mixed drop reads every folder batch and nested path, keeps fallback files and successful siblings after a read error', () => {
  assert.deepEqual(mixed.map(item => item.path), ['one/same.txt', 'one/unreadable.txt', 'one/nested/same.txt', 'Folder B/same.txt', 'two/same.txt']);
  assert.equal(mixed.filter(item => 'file' in item).length, 4);
  assert.match(mixed[1].error, /Permission denied/);
});
let captured = false, expired = false;
const handles = filesFromDrop({ files: [], items: [{ kind: 'file', getAsFile: () => null, getAsFileSystemHandle: () => {
  assert.equal(expired, false); captured = true;
  return Promise.resolve({ kind: 'directory', name: 'modern', async *values() { yield { kind: 'file', name: 'name.txt', getFile: async () => file('name.txt') }; } });
} }] });
expired = true;
const modern = await collect(handles);
check('Modern drop handle permission is captured before the event expires', () => {
  assert.equal(captured, true); assert.equal(modern[0].path, 'modern/name.txt');
});
const unsupported = await collect(filesFromDrop({ files: [], items: [{ kind: 'file', getAsFile: () => null }] }));
check('Unsupported folder drops report a limitation instead of an empty successful import', () => assert.match(unsupported[0].error, /cannot read.*folder/));
const fallback = await collect(filesFromDrop({ files: [first], items: [] }));
check('Files-only drag APIs preserve the relative path', () => assert.equal(fallback[0].path, 'Folder A/nested/same.txt'));
let completeRead;
const cancel = new AbortController();
const late = filesFromDrop({ files: [], items: [dropItem({ name: 'late', isFile: true, isDirectory: false, file: done => { completeRead = done; } })] }, cancel.signal);
const next = late.next(); cancel.abort(); completeRead(first);
await assert.rejects(next, { name: 'AbortError' });
checks.push('Cancelled folder discovery ignores a late file response');
const registry = new ImportAttemptRegistry();
const old = registry.start('one');
registry.cancel('one');
check('Cancellation blocks late progress but allows a confirmed stored-original receipt to be retained before settlement', () => {
  assert.equal(registry.isCurrent(old), false); assert.equal(registry.isLatest(old), true); assert.equal(old.signal.aborted, true);
});
const retried = registry.start('one'); registry.finish(old);
check('Late completion/finally from an older attempt cannot update or unregister its retry', () => {
  assert.equal(registry.isLatest(old), false); assert.equal(registry.isCurrent(retried), true);
});
registry.remove('one');
check('Removed queue entries cannot be resurrected by a late response', () => {
  assert.equal(registry.isLatest(retried), false); assert.equal(retried.signal.aborted, true);
});
const result = { text: 'retained extraction from synthetic source' }, record = { id: 'synthetic-document' };
const saving = { ...entries[0], phase: 'saving', progress: 25, record, result, savePending: true };
const cancelled = cancelImportItem(saving);
const retry = retryImportItem(cancelled);
check('Cancellation and save retry retain exact original, saved record and uncommitted result', () => {
  assert.equal(cancelled.phase, 'cancelled'); assert.equal(cancelled.progress, null);
  assert.strictEqual(cancelled.source.file, first); assert.strictEqual(cancelled.result, result); assert.strictEqual(cancelled.record, record);
  assert.equal(retry.phase, 'waiting'); assert.equal(retry.retrySave, true); assert.strictEqual(retry.result, result);
  assert.throws(() => retryImportItem(saving), /settle/);
});
const failed = { ...saving, phase: 'failed', error: 'Synthetic save failure' };
const noSavedOriginal = { ...entries[1], phase: 'cancelled' };
const restored = restoreImportItems([saving, noSavedOriginal, { ...failed, id: 'failed' }]);
check('Refresh turns in-flight work into interrupted work without losing failed/cancelled recovery payloads', () => {
  assert.equal(restored[0].phase, 'interrupted'); assert.equal(restored[0].progress, null);
  assert.strictEqual(restored[0].result, result); assert.strictEqual(restored[1].source.file, second);
  assert.equal(restored[2].phase, 'failed'); assert.strictEqual(restored[2].result, result);
  assert.equal(retryImportItem(restored[2]).retrySave, true);
  const live = { ...entries[0], message: 'Newly selected, do not overwrite' };
  const merged = restoreImportItems([saving], [live]);
  assert.equal(merged.length, 1); assert.strictEqual(merged[0], live);
});
check('Aggregate reports outcomes and active counts without fabricated whole-task progress', () => {
  assert.deepEqual(summarizeImports([{ phase: 'saved' }, { phase: 'waiting' }, { phase: 'uploading' }, { phase: 'failed' }, { phase: 'cancelled' }, { phase: 'interrupted' }]), { total: 6, saved: 1, active: 2, waiting: 1, failed: 1, cancelled: 1, interrupted: 1 });
});
console.log(JSON.stringify({ environment: 'Node; synthetic File and drag API objects; no browser or storage durability claim', checks }, null, 2));
