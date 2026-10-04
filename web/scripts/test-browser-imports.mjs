/** Actual browser regression: local OPFS archive streams and pinned, offline CPU/WASM OCR.
 * Install Playwright Chromium separately; PLAYWRIGHT_MODULE may point to a provisioned module.
 * Run after `node scripts/copy-ocr-assets.mjs`.
 */
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { readFile, mkdtemp, rm } from 'node:fs/promises';
import { createReadStream } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';
import { gzipSync } from 'node:zlib';
import { spawn } from 'node:child_process';
import { zipSync } from 'fflate';
const { chromium } = await import(process.env.PLAYWRIGHT_MODULE || 'playwright');
const web = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const temporary = await mkdtemp(join(tmpdir(), 'tpe-browser-imports-'));
const bytes = text => new TextEncoder().encode(text);
function tar(entries) {
  const chunks = [];
  for (const [name, content = '', type = '0'] of entries) {
    const body = typeof content === 'string' ? Buffer.from(content) : content;
    const header = Buffer.alloc(512);
    header.write(name, 0, 100); header.write('0000644\0', 100); header.write('0000000\0', 108); header.write('0000000\0', 116);
    header.write(`${body.length.toString(8).padStart(11, '0')}\0`, 124); header.write('00000000000\0', 136);
    header.fill(32, 148, 156); header.write(type, 156); header.write('ustar\0', 257); header.write('00', 263);
    const checksum = header.reduce((sum, byte) => sum + byte, 0); header.write(`${checksum.toString(8).padStart(6, '0')}\0 `, 148);
    chunks.push(header, body, Buffer.alloc((512 - body.length % 512) % 512));
  }
  return Buffer.concat([...chunks, Buffer.alloc(1024)]);
}
const fixtures = new Map();
const large = new Uint8Array(9 * 1024 * 1024).fill(65);
fixtures.set('large.zip', zipSync({ 'folder/large.txt': large, 'last.txt': bytes('last member') }));
fixtures.set('many.zip', zipSync(Object.fromEntries(Array.from({ length: 251 }, (_, index) => [`${index}.txt`, bytes(String(index))]))));
fixtures.set('root.tar.gz', gzipSync(tar([['./', '', '5'], ['./notes.txt', 'streamed tar text']])));
fixtures.set('unsafe.tar', tar([['../escape.txt', 'escape']]));
fixtures.set('symlink.tar', tar([['pointer.txt', '', '2']]));
fixtures.set('nested.zip', zipSync({ 'again.zip': bytes('not expanded'), 'good.txt': bytes('sibling retained') }));
fixtures.set('broken.txt.gz', gzipSync(bytes('must not truncate')).subarray(0, -4));
fixtures.set('concat.txt.gz', Buffer.concat([gzipSync(bytes('first ')), gzipSync(bytes('second'))]));
const badCrc = Buffer.from(zipSync({ 'bad.txt': bytes('checksum test') }));
const central = badCrc.indexOf(Buffer.from([0x50, 0x4b, 0x01, 0x02])); badCrc[central + 16] ^= 255; fixtures.set('crc.zip', badCrc);
const symlink = Buffer.from(zipSync({ 'pointer.txt': bytes('target') }));
const linkCentral = symlink.indexOf(Buffer.from([0x50, 0x4b, 0x01, 0x02])); symlink.writeUInt32LE(0xa1ff0000, linkCentral + 38); fixtures.set('symlink.zip', symlink);
const unsafeZip = zipSync({ '../escape.txt': bytes('escape') }); fixtures.set('unsafe.zip', unsafeZip);
await new Promise((resolve, reject) => {
  const child = spawn(join(web, 'node_modules/.bin/esbuild'), ['--bundle', '--format=esm', '--platform=browser', `--outfile=${join(temporary, 'helpers.js')}`], { cwd: web, stdio: ['pipe', 'inherit', 'inherit'] });
  child.on('error', reject); child.on('exit', code => code === 0 ? resolve() : reject(new Error(`esbuild exited ${code}`)));
  child.stdin.end("export * from './lib/imports.ts'; export * from './lib/image-ocr.ts';");
});
const server = createServer(async (request, response) => {
  const url = new URL(request.url, 'http://localhost');
  if (url.pathname === '/') { response.setHeader('Content-Type', 'text/html'); response.end('<!doctype html><title>Offline ingestion regression</title>'); return; }
  if (url.pathname === '/helpers.js') { response.setHeader('Content-Type', 'text/javascript'); response.end(await readFile(join(temporary, 'helpers.js'))); return; }
  if (url.pathname.startsWith('/fixtures/')) { const fixture = fixtures.get(url.pathname.slice(10)); if (fixture) { response.end(fixture); return; } }
  if (url.pathname.startsWith('/ocr/7.0.0/') && !url.pathname.includes('..')) {
    response.setHeader('Content-Type', url.pathname.endsWith('.js') ? 'text/javascript' : 'application/octet-stream');
    const stream = createReadStream(join(web, 'public', url.pathname)); stream.on('error', () => { response.statusCode = 404; response.end(); }); stream.pipe(response); return;
  }
  response.statusCode = 404; response.end();
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
const origin = `http://127.0.0.1:${server.address().port}`;
let browser;
try {
  browser = await chromium.launch({ headless: true, args: ['--no-sandbox'] });
  const context = await browser.newContext();
  const unexpected = [], requests = [];
  await context.route('**/*', async route => {
    const url = route.request().url();
    if (url.startsWith(origin + '/')) { requests.push(url.slice(origin.length)); await route.continue(); }
    else { unexpected.push(url); await route.abort(); }
  });
  const page = await context.newPage();
  await page.goto(origin);
  const result = await page.evaluate(async () => {
    const { expandUploads, recognizeImage, inspectOcrImage } = await import('/helpers.js');
    const results = [];
    const check = (condition, message) => { if (!condition) throw new Error(message); results.push(message); };
    const fixture = async name => new File([await (await fetch(`/fixtures/${name}`)).blob()], name);
    const entries = async name => {
      const output = [];
      for await (const item of expandUploads([await fixture(name)])) {
        output.push('file' in item ? { path: item.path, size: item.file.size, text: item.file.size < 100 ? await item.file.text() : null } : item);
        await item.dispose?.();
      }
      return output;
    };
    const large = await entries('large.zip');
    check(large.length === 2 && large[0].size === 9 * 1024 * 1024 && large[1].text === 'last member', 'ZIP member above old size cap streams completely, followed by next member');
    const many = await entries('many.zip'); check(many.length === 251 && many[250].text === '250', 'ZIP has no 200-member cap');
    const tar = await entries('root.tar.gz'); check(tar.length === 1 && tar[0].path === 'notes.txt' && tar[0].text === 'streamed tar text', 'GZIP/TAR root directory and body are streamed correctly');
    const concat = await entries('concat.txt.gz'); check(concat[0].text === 'first second' || concat[0].error, 'Concatenated GZIP is fully consumed or explicitly rejected by platform decoder');
    for (const name of ['unsafe.tar', 'symlink.tar', 'unsafe.zip', 'symlink.zip', 'crc.zip', 'broken.txt.gz']) {
      const bad = await entries(name); check(bad.length === 1 && typeof bad[0].error === 'string', `${name} is an explicit error with no fabricated member`);
    }
    const nested = await entries('nested.zip'); check(nested.length === 2 && nested[0].error && nested[1].text === 'sibling retained', 'Nested archive is explicit and does not suppress ordinary siblings');
    const root = await navigator.storage.getDirectory(), temp = await root.getDirectoryHandle('tpe-import-tmp');
    const count = async () => { let size = 0; for await (const entry of temp.values()) { void entry; size++; } return size; };
    const iterator = expandUploads([await fixture('large.zip')]); await iterator.next();
    check(await count() === 1, 'Archive yields with only its current member staged');
    await iterator.return(); check(await count() === 0, 'Closing an archive iterator disposes its current OPFS file');
    const abort = new AbortController(); let cancelled = false;
    try { for await (const item of expandUploads([await fixture('large.zip')], event => { if (event.completed > 0) abort.abort(); }, abort.signal)) void item; }
    catch (error) { cancelled = error.name === 'AbortError'; }
    check(cancelled && await count() === 0, 'Cancellation aborts streaming and removes partial OPFS files');
    const canvas = document.createElement('canvas'); canvas.width = 1500; canvas.height = 340;
    const draw = canvas.getContext('2d'); draw.fillStyle = 'white'; draw.fillRect(0, 0, canvas.width, canvas.height); draw.fillStyle = 'black'; draw.font = '52px Arial';
    draw.fillText('AUTONOMOUS OCR TEST', 45, 85); draw.fillText('Reference 10.1000/example', 45, 175); draw.fillText('Local pixels stay on this device.', 45, 265);
    const file = new File([await new Promise(resolve => canvas.toBlob(resolve, 'image/png'))], 'ocr-fixture.png', { type: 'image/png' });
    const dimensions = await inspectOcrImage(file); check(dimensions.width === 1500 && dimensions.height === 340, 'Image headers report actual dimensions');
    const progress = [];
    const ocr = await recognizeImage(file, event => progress.push(event));
    check(ocr.text.includes('AUTONOMOUS OCR TEST') && ocr.text.includes('10.1000/example') && ocr.text.includes('Local pixels stay on this device.'), 'Real CPU/WASM OCR recovers the fixture text and DOI');
    check(ocr.status === 'partial' && ocr.links[0]?.kind === 'OCR DOI (unverified)' && ocr.metadata.ocr.confidence > 0, 'OCR retains uncertainty and unverified identifier evidence');
    check(progress.some(event => event.progress > 0 && event.progress < 1), 'OCR reports actual worker progress');
    const cancel = new AbortController(); let aborted = false;
    try { await recognizeImage(file, event => { if (event.status.includes('loading tesseract core')) cancel.abort(); }, cancel.signal); }
    catch (error) { aborted = error.name === 'AbortError'; }
    check(aborted, 'OCR initialization cancellation terminates the owned worker');
    return { checks: results, text: ocr.text, metadata: ocr.metadata };
  });
  assert.deepEqual(unexpected, [], 'OCR must not contact third-party hosts');
  assert(requests.some(path => path.includes('/core/tesseract-core') && path.endsWith('.wasm.js')));
  assert(requests.some(path => path.endsWith('/lang/eng.traineddata.gz')));
  console.log(JSON.stringify({ ...result, requests: [...new Set(requests.filter(path => path.startsWith('/ocr/')))], network: 'same-origin assets only; no remote OCR' }, null, 2));
} finally {
  await browser?.close(); await new Promise(resolve => server.close(resolve)); await rm(temporary, { recursive: true, force: true });
}
