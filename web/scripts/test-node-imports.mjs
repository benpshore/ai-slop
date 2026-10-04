/** Node/WASM regression. Disk-backed storage and pixel-decoder shims are NOT browser QA. */
import assert from 'node:assert/strict';
import { readFile, writeFile, mkdir, mkdtemp, rm, open, readdir } from 'node:fs/promises';
import { openAsBlob } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { pathToFileURL, fileURLToPath } from 'node:url';
import { tmpdir } from 'node:os';
import { gzipSync } from 'node:zlib';
import { createRequire } from 'node:module';
import { Worker as NodeWorker } from 'node:worker_threads';
import ts from 'typescript';
import { zipSync } from 'fflate';
const require = createRequire(import.meta.url), web = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const temporary = await mkdtemp(join(tmpdir(), 'tpe-node-imports-'));
const checks = [];
const check = (condition, message) => { assert(condition, message); checks.push(message); if (process.env.TPE_IMPORT_TRACE) console.log(message); };
let quota = false;
class Directory {
  constructor(path) { this.path = path; }
  async getDirectoryHandle(name) { const path = join(this.path, name); await mkdir(path, { recursive: true }); return new Directory(path); }
  async getFileHandle(name) {
    const path = join(this.path, name); await writeFile(path, '');
    return { getFile: async () => new File([await openAsBlob(path)], name), createWritable: async () => {
      const handle = await open(path, 'w'); let closed = false;
      const close = async () => { if (!closed) { closed = true; await handle.close(); } };
      return { write: async chunk => { if (quota) throw new DOMException('No local space', 'QuotaExceededError'); let used = 0; while (used < chunk.length) used += (await handle.write(chunk.subarray(used))).bytesWritten; }, close, abort: close };
    } };
  }
  async removeEntry(name) { await rm(join(this.path, name)); }
}
Object.defineProperty(globalThis, 'navigator', { configurable: true, value: { storage: { getDirectory: async () => new Directory(temporary) } } });
try {
  for (const name of ['imports', 'image-ocr']) {
    const source = await readFile(join(web, 'lib', `${name}.ts`), 'utf8');
    const { outputText } = ts.transpileModule(source, { compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ES2022 } });
    await writeFile(join(temporary, `${name}.mjs`), outputText);
  }
  const { expandUploads, safeArchivePath } = await import(pathToFileURL(join(temporary, 'imports.mjs')));
  const { recognizeImage, inspectOcrImage } = await import(pathToFileURL(join(temporary, 'image-ocr.mjs')));
  const enc = new TextEncoder(), data = text => enc.encode(text);
  const collect = async file => {
    const result = [];
    for await (const item of expandUploads([file])) {
      result.push('file' in item ? { path: item.path, size: item.file.size, text: item.file.size < 100 ? await item.file.text() : null } : item);
      await item.dispose?.();
    }
    return result;
  };
  const makeZip = (name, entries) => new File([zipSync(entries)], name);
  const largeZip = makeZip('large.zip', { 'folder/large.txt': new Uint8Array(9 * 1024 * 1024).fill(65), 'last.txt': data('last member') });
  const large = await collect(largeZip);
  check(large.length === 2 && large[0].size === 9 * 1024 * 1024 && large[1].text === 'last member', 'ZIP member above 8 MiB streams fully before its sibling');
  const many = await collect(makeZip('many.zip', Object.fromEntries(Array.from({ length: 251 }, (_, index) => [`${index}.txt`, data(String(index))]))));
  check(many.length === 251 && many[250].text === '250', 'Archive has no 200-member cap');
  const gzip = new File([Buffer.concat([gzipSync(data('first ')), gzipSync(data('second'))])], 'concat.txt.gz');
  const concatenated = await collect(gzip);
  check(concatenated[0].text === 'first second' || concatenated[0].error, 'Concatenated GZIP is consumed fully or explicitly rejected by platform decoder');
  const broken = new File([gzipSync(data('no silent truncation')).subarray(0, -4)], 'broken.txt.gz');
  check((await collect(broken))[0].error, 'Truncated GZIP is an explicit error');
  function tar(entries) {
    const parts = [];
    for (const [name, text, type = '0'] of entries) {
      const body = Buffer.from(text), header = Buffer.alloc(512);
      header.write(name, 0, 100); header.write(`${body.length.toString(8).padStart(11, '0')}\0`, 124); header.fill(32, 148, 156); header.write(type, 156); header.write('ustar\0', 257);
      header.write(`${header.reduce((sum, byte) => sum + byte, 0).toString(8).padStart(6, '0')}\0 `, 148);
      parts.push(header, body, Buffer.alloc((512 - body.length % 512) % 512));
    }
    return Buffer.concat([...parts, Buffer.alloc(1024)]);
  }
  const rootTar = await collect(new File([gzipSync(tar([['./', '', '5'], ['./notes.txt', 'streamed TAR']]))], 'root.tar.gz'));
  check(rootTar.length === 1 && rootTar[0].text === 'streamed TAR' && rootTar[0].path === 'notes.txt', 'TAR root marker and GZIP stream preserve content');
  for (const [name, content] of [['unsafe', tar([['../escape', 'bad']])], ['link', tar([['link', '', '2']])]]) check((await collect(new File([content], `${name}.tar`)))[0].error, `${name} TAR member rejected`);
  check((await collect(makeZip('unsafe.zip', { '../escape': data('bad') })))[0].error, 'ZIP traversal rejected');
  const corrupt = Buffer.from(zipSync({ 'bad.txt': data('checksum') })); const at = corrupt.indexOf(Buffer.from([80, 75, 1, 2])); corrupt[at + 16] ^= 255;
  check((await collect(new File([corrupt], 'crc.zip')))[0].error, 'ZIP CRC mismatch rejected');
  const linked = Buffer.from(zipSync({ 'link.txt': data('target') })); linked.writeUInt32LE(0xa1ff0000, linked.indexOf(Buffer.from([80, 75, 1, 2])) + 38);
  check((await collect(new File([linked], 'link.zip')))[0].error, 'ZIP symbolic link rejected');
  const nested = await collect(makeZip('nested.zip', { 'again.zip': data('nested'), 'good.txt': data('sibling') }));
  check(nested[0].error && nested[1].text === 'sibling', 'Nested archive remains explicit while siblings continue');
  for (const path of ['/absolute', 'C:\\drive', 'a/../escape', 'a\0b']) assert.throws(() => safeArchivePath(path));
  const tempCount = async () => (await readdir(join(temporary, 'tpe-import-tmp'))).length;
  const iterator = expandUploads([largeZip]); await iterator.next(); check(await tempCount() === 1, 'Only current yielded archive member is staged'); await iterator.return(); check(await tempCount() === 0, 'Iterator close cleans its temporary member');
  const signal = new AbortController(); let cancelled = false;
  try { for await (const item of expandUploads([largeZip], event => { if (event.completed) signal.abort(); }, signal.signal)) void item; } catch (error) { cancelled = error.name === 'AbortError'; }
  check(cancelled && await tempCount() === 0, 'Archive cancellation removes partial files');
  quota = true; const failed = await collect(largeZip); quota = false;
  check(failed[0].error.includes('local storage') && await tempCount() === 0, 'Actual storage quota error is explicit and cleans temporary files');
  // Regression for a late InflateRaw callback enqueueing into a closed native
  // DecompressionStream controller. Do not install uncaught-error handlers:
  // a lifecycle regression must still crash this process and fail the test.
  for (let attempt = 0; attempt < 8; attempt++) {
    const stop = new AbortController();
    await assert.rejects(async () => {
      for await (const item of expandUploads([largeZip], event => { if (event.completed) stop.abort(); }, stop.signal)) void item;
    }, error => error.name === 'AbortError');
    quota = true; const failure = await collect(largeZip); quota = false;
    assert(failure[0].error.includes('local storage'));
    assert.equal(await tempCount(), 0);
  }
  check(true, 'Repeated compressed-stream cancellation and quota failures settle without late native callbacks');
  const png = await readFile(join(web, 'scripts/fixtures/ocr-still.png'));
  const file = new File([png], 'ocr-still.png', { type: 'image/png' });
  const dimensions = await inspectOcrImage(file); check(dimensions.width === 1500 && dimensions.height === 340, 'PNG headers preserve real dimensions');
  let active = 0;
  // Browser-only pixel decoding/canvas and Worker construction are explicit test shims.
  // The recognizer below is the actual pinned Tesseract CPU/WASM worker on actual PNG bytes.
  globalThis.createImageBitmap = async (_file, options) => ({ width: options.resizeWidth, height: options.resizeHeight, close() {} });
  globalThis.document = { createElement: () => ({ width: 0, height: 0, getContext: () => ({ fillRect() {}, drawImage() {} }), toBlob: callback => callback(new Blob([png], { type: 'image/png' })) }) };
  globalThis.location = { origin: 'https://local-assets.invalid' };
  globalThis.Worker = class {
    constructor(url) {
      assert.equal(url, '/ocr/7.0.0/worker.min.js'); active++;
      this.worker = new NodeWorker(require.resolve('tesseract.js/src/worker-script/node/index.js'));
      this.worker.on('message', packet => this.onmessage?.({ data: packet })); this.worker.on('error', error => this.onerror?.({ message: error.message }));
    }
    postMessage(packet, transfer) {
      if (packet.action === 'loadLanguage') packet.payload.options = { ...packet.payload.options, langPath: join(web, 'public/ocr/7.0.0/lang'), cacheMethod: 'none' };
      this.worker.postMessage(packet, transfer);
    }
    terminate() { if (this.worker) { active--; void this.worker.terminate(); this.worker = null; } }
  };
  const progress = [], result = await recognizeImage(file, event => progress.push(event));
  check(result.text.includes('AUTONOMOUS OCR TEST') && result.text.includes('10.1000/example') && result.text.includes('Local pixels stay on this device.'), 'Actual Node CPU/WASM worker recognizes known text and DOI through application protocol');
  check(result.status === 'partial' && result.links[0].kind === 'OCR DOI (unverified)' && result.metadata.ocr.confidence > 0, 'OCR evidence is explicitly partial and identifiers unverified');
  check(progress.some(event => event.status === 'recognizing text') && active === 0, 'Worker progress reported and worker disposed after OCR');
  const cancel = new AbortController(); let ocrCancelled = false;
  try { await recognizeImage(file, event => { if (event.status === 'loading tesseract core') cancel.abort(); }, cancel.signal); } catch (error) { ocrCancelled = error.name === 'AbortError'; }
  check(ocrCancelled && active === 0, 'Worker is terminated when initialization is cancelled');
  console.log(JSON.stringify({ environment: 'Node/WASM; disk storage, image decoder and worker construction are test shims, not browser QA', checks, text: result.text, metadata: result.metadata }, null, 2));
} finally { await rm(temporary, { recursive: true, force: true }); }
