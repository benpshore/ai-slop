/** Synthetic Node tests. Browser paint/network timing is measured separately. */
import assert from 'node:assert/strict';
import {createRequire} from 'node:module';
const require = createRequire(import.meta.url);
const {build} = require(require.resolve('esbuild', {paths: [require.resolve('vite')]}));
const built = await build({entryPoints: ['lib/text-import.ts'], bundle: true, write: false, format: 'esm', platform: 'node', metafile: true});
assert.deepEqual(Object.keys(built.metafile.inputs), ['lib/text-import.ts'], 'Text preparation has no runtime toolkit imports');
const {prepareTextImport: prepareTextImportRaw, createTextImportSession} = await import(`data:text/javascript;base64,${Buffer.from(built.outputFiles[0].text).toString('base64')}`);
const routing = await build({entryPoints: ['lib/upload-client.ts'], bundle: true, write: false, format: 'esm', platform: 'node'});
const {detectFileKind} = await import(`data:text/javascript;base64,${Buffer.from(routing.outputFiles[0].text).toString('base64')}`);
const acceptedFixtures = [];
async function prepareTextImport(file, options) {
  const draft = await prepareTextImportRaw(file, options);
  if (draft) acceptedFixtures.push(file);
  return draft;
}
const checks = [];
const check = (condition, message) => { assert(condition, message); checks.push(message); };
const file = new File(['some text'], 'Pasted text.txt', {type: 'text/plain', lastModified: 123456789});
const realFetch = globalThis.fetch, realWorker = globalThis.Worker;
globalThis.fetch = () => { throw new Error('Text preparation attempted network access'); };
globalThis.Worker = class { constructor() { throw new Error('Text preparation attempted PDF/OCR initialization'); } };
let draft;
try {
  draft = await prepareTextImport(file, {sourceUrl: 'https://example.invalid/synthetic'});
  check(draft?.result.text === 'some text' && draft.result.markdown === 'some text', 'Two words produce readable text and Markdown without network or workers');
  check(draft.original === file && draft.result.engine === 'Plain text decoder', 'Original identity retained; two words use the text decoder');
  assert.deepEqual(draft.result.metadata, {
    sourceUrl: 'https://example.invalid/synthetic', contentType: 'text',
    textImport: {version: 1, format: 'text', encoding: 'utf-8', originalName: file.name, originalMime: file.type, originalBytes: 9, originalLastModified: 123456789},
  });
  check(Object.values(draft.timings).every(value => Number.isFinite(value) && value >= 0), 'Measured preparation phases are finite monotonic durations');
  assert(Math.abs(draft.timings.totalMs - draft.timings.detectMs - draft.timings.decodeMs - draft.timings.projectionMs) < 0.00001);
  const source = '  # Synthetic notes\r\n\r\n  café 漢字 😀\t\n10.1000/example. 10.1000/another;\n';
  const markdown = await prepareTextImport(new File([source], 'notes.md', {type: 'text/markdown'}));
  check(markdown.result.text === source && markdown.result.markdown === source, 'Markdown, whitespace and Unicode are preserved exactly');
  assert.deepEqual(markdown.result.links, [
    {url: 'https://doi.org/10.1000/example', doi: '10.1000/example', kind: 'printed DOI'},
    {url: 'https://doi.org/10.1000/another', doi: '10.1000/another', kind: 'printed DOI'},
  ]);
  check(markdown.result.metadata.textImport.format === 'markdown', 'Markdown identity and printed DOI evidence remain available');
  for (const [content, label] of [
    ['%PDF-1.7\nsynthetic', 'PDF'],
    ['padding %PDF-1.7\nsynthetic', 'PDF with leading bytes'],
    [' '.repeat(1200) + '%PDF-1.7\nsynthetic', 'PDF after leading whitespace'],
    ['<!doctype html><html><body>article</body></html>', 'HTML'],
    ['<article>article</article>', 'HTML fragment'],
    ['<?xml version="1.0"?><root>text</root>', 'XML'],
    ['<rss><channel><title>Feed</title></channel></rss>', 'RSS'],
    ['{"pages": [{"text": "synthetic"}]}', 'native JSON'],
    ['[1, 2]', 'JSON array'],
    ['[Markdown link](https://example.invalid)', 'ambiguous leading bracket'],
    ['{ incomplete JSON', 'ambiguous leading brace'],
    [new Uint8Array([80, 75, 3, 4]), 'ZIP'],
    [new Uint8Array([31, 139, 8]), 'GZIP'],
    [new Uint8Array([137, 80, 78, 71, 13, 10, 26, 10]), 'PNG'],
    [new Uint8Array([255, 216, 255, 224]), 'JPEG'],
    ['GIF89asynthetic', 'GIF'],
    ['  GIF89asynthetic', 'GIF after whitespace'],
    ['\uFEFF \nBMsynthetic', 'BMP after BOM and whitespace'],
    ['RIFFxxxxWEBPsynthetic', 'WebP'],
    ['  RIFFxxxxWEBPsynthetic', 'WebP after whitespace'],
    ['é'.repeat(128) + 'xustar', 'TAR magic at byte offset after Unicode'],
    [new Uint8Array([1, 0, 2, 0, 3]), 'binary controls'],
    [new Uint8Array([0xff, 0x80]), 'invalid UTF-8'],
  ]) {
    assert.equal(await prepareTextImport(new File([content], 'renamed.txt', {type: 'text/plain'})), null, `${label} must decline text fast path`);
  }
  check(true, 'Renamed PDF/HTML/XML/feed/JSON/archive/image/binary inputs decline the text path');
  assert.equal(await prepareTextImport(new File(['some text'], 'unknown.bin')), null);
  assert.equal(await prepareTextImport(new File(['body { color: red; }'], 'style.css', {type: 'text/css'})), null);
  assert.equal(await prepareTextImport(new File(['body { color: red; }'], 'style.txt', {type: 'text/css'})), null);
  assert.equal(await prepareTextImport(new File(['body { color: red; }'], 'style.css', {type: 'text/plain'})), null);
  assert.equal(await prepareTextImport(new File(['some text'], 'archive.tar', {type: 'text/plain'})), null);
  assert.equal(await prepareTextImport(new File(['some text'], 'untitled', {type: 'text/markdown'})), null);
  assert.equal(await prepareTextImport(new File(['some text'], 'untitled', {type: 'text/plain; charset=utf-8'})), null);
  const latin = await prepareTextImport(new File([new Uint8Array([0x63, 0x61, 0x66, 0xe9])], 'latin.txt', {type: 'text/plain; charset=windows-1252'}));
  check(latin.result.text === 'café' && latin.result.metadata.textImport.encoding === 'windows-1252', 'Declared text charset is honored');
  const utf16 = await prepareTextImport(new File([new Uint8Array([0xff, 0xfe, 0x41, 0, 0xa9, 3])], 'unicode.txt', {type: 'text/plain; charset=windows-1252'}));
  check(utf16.result.text === 'AΩ' && utf16.result.metadata.textImport.encoding === 'utf-16le', 'Unicode BOM takes precedence over MIME charset');
  for (const [name, mime] of [['plain.txt', ''], ['notes.md', ''], ['notes.markdown', 'text/markdown'], ['untitled', 'text/plain'], ['renamed.pdf', 'text/plain']]) {
    assert(await prepareTextImport(new File(['some text'], name, {type: mime})), `${name} ${mime} remains eligible text`);
  }
  for (const accepted of acceptedFixtures) assert.equal(await detectFileKind(accepted), 'text', `Preview and authoritative router agree for ${accepted.name} (${accepted.type})`);
  check(true, 'Every accepted text fixture matches the authoritative upload-client text route');
  const aborted = new AbortController(); aborted.abort();
  await assert.rejects(prepareTextImport(file, {signal: aborted.signal}), {name: 'AbortError'});
  const duringRead = new AbortController();
  class CancellingFile extends File {
    stream() { return new ReadableStream({pull(controller) {duringRead.abort(); controller.close();}}); }
  }
  await assert.rejects(prepareTextImport(new CancellingFile(['some text'], 'cancel.txt'), {signal: duringRead.signal}), {name: 'AbortError'});
  check(true, 'Preparation cancellation before and during decode remains explicit');
} finally {
  globalThis.fetch = realFetch;
  if (realWorker === undefined) delete globalThis.Worker; else globalThis.Worker = realWorker;
}

const record = {id: 'synthetic-record', title: file.name, kind: 'text', source_url: null, original_name: file.name, status: 'uploaded', engine: '', created_at: '2026-10-04T00:00:00Z', sha256: 'synthetic-receipt-hash', bytes: file.size};
const deferred = () => {let resolve; const promise = new Promise(done => {resolve = done;}); return {promise, resolve};};
{
  const order = ['readable'], gate = deferred();
  const session = createTextImportSession(draft, {
    yieldBeforeSave: () => gate.promise,
    uploadOriginal: async (original, options) => {assert.equal(original, file); assert.equal(options.sourceUrl, 'https://example.invalid/synthetic'); order.push('upload'); return record;},
    saveExtracted: async (receipt, result) => {assert.equal(receipt, record); assert.equal(result, draft.result); order.push('save');},
  });
  const saving = session.save({onOriginalSaved: value => {assert.equal(value, record); order.push('receipt');}});
  assert.equal(session.save(), saving, 'Concurrent save calls share one attempt');
  assert.deepEqual(order, ['readable'], 'No persistence begins until the render yield completes');
  gate.resolve();
  assert.equal((await saving).status, 'saved');
  assert.deepEqual(order, ['readable', 'upload', 'receipt', 'save']);
  await session.save();
  assert.deepEqual(order, ['readable', 'upload', 'receipt', 'save'], 'Confirmed saves are idempotent in this session');
  check(session.record === record, 'Readable output precedes deferred persistence and concurrent requests do not duplicate it');
}
{
  let called = false;
  const session = createTextImportSession(draft, {uploadOriginal: async () => {called = true; return record;}, saveExtracted: async () => {}});
  const saving = session.save();
  await Promise.resolve();
  check(!called, 'Default persistence yield crosses a task boundary, not only a microtask');
  assert.equal((await saving).status, 'saved');
}
{
  let originalCalls = 0, resultCalls = 0;
  const failure = new Error('Synthetic storage outage');
  const session = createTextImportSession(draft, {
    uploadOriginal: async () => {originalCalls++; return record;},
    saveExtracted: async () => {if (++resultCalls === 1) throw failure;},
  });
  const failed = await session.save();
  assert.equal(failed.status, 'unsaved'); assert.equal(failed.stage, 'result'); assert.equal(failed.error, failure);
  check(session.draft.result.text === 'some text' && session.draft.original === file && session.record === record, 'Save failure retains readable output, original and confirmed receipt for retry');
  assert.equal((await session.save()).status, 'saved');
  assert.equal(originalCalls, 1); assert.equal(resultCalls, 2);
  check(true, 'Result save retry reuses the original upload receipt');
}
{
  let attempts = 0;
  const session = createTextImportSession(draft, {uploadOriginal: async () => {if (++attempts === 1) throw new Error('Synthetic offline upload'); return record;}, saveExtracted: async () => {}});
  const failed = await session.save();
  check(failed.status === 'unsaved' && failed.stage === 'original' && !session.record && session.draft.original === file && session.draft.result.text === 'some text', 'Original-upload failure keeps both retry payload and readable output');
  assert.equal((await session.save()).status, 'saved'); assert.equal(attempts, 2);
}
{
  const controller = new AbortController(), gate = deferred(); let calls = 0;
  const session = createTextImportSession(draft, {yieldBeforeSave: () => gate.promise, uploadOriginal: async () => {calls++; return record;}, saveExtracted: async () => {calls++;}});
  const pending = session.save({signal: controller.signal}); controller.abort(); gate.resolve();
  check((await pending).status === 'cancelled' && calls === 0 && session.draft.result.text === 'some text', 'Cancelling during the render yield prevents storage and retains readable output');
  assert.equal((await session.save({signal: new AbortController().signal})).status, 'saved');
  assert.equal(calls, 2);
}
{
  const controller = new AbortController(); let originals = 0, saves = 0;
  const session = createTextImportSession(draft, {uploadOriginal: async () => {originals++; return record;}, saveExtracted: async () => {saves++;}});
  const outcome = await session.save({signal: controller.signal, onOriginalSaved: () => controller.abort()});
  check(outcome.status === 'cancelled' && session.record === record && saves === 0, 'Cancellation after original commit retains its authoritative receipt');
  assert.equal((await session.save()).status, 'saved'); assert.equal(originals, 1); assert.equal(saves, 1);
}
{
  const controller = new AbortController();
  const session = createTextImportSession(draft, {uploadOriginal: async () => {throw new Error('Confirmed original should not be uploaded again');}, saveExtracted: async () => {controller.abort();}}, record);
  check((await session.save({signal: controller.signal})).status === 'saved', 'Confirmed finalization remains saved when cancellation arrives during commit');
}
console.log(JSON.stringify({environment: 'Node synthetic contracts; no deployed Site/network or browser-paint claim', checks}, null, 2));
