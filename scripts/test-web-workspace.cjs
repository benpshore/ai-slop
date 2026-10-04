#!/usr/bin/env node
/**
 * Component integration regression for the actual web/app/workspace.tsx.
 *
 * Run from any directory after installing the locked web dependencies:
 *   node /path/to/pdftextract/scripts/test-web-workspace.cjs
 *
 * React, jsdom, DOMPurify, and the TypeScript compiler resolve from web/.
 * The DOM is emulated; extraction helpers, network, and persistence are mocked.
 * These checks validate queue/reader orchestration, not real browser layout,
 * PDF/OCR quality, IndexedDB durability, private storage, or a deployed Site.
 */
const assert = require('node:assert/strict');
const { createRequire } = require('node:module');
const Module = require('node:module');
const fs = require('node:fs');
const { File } = require('node:buffer');
const path = require('node:path');
const repositoryRoot = path.resolve(__dirname, '..');
const webRoot = path.join(repositoryRoot, 'web');
const req = createRequire(path.join(webRoot, 'package.json'));
const { JSDOM } = req('jsdom');
const dom = new JSDOM('<div id="root"></div>', { url: 'https://tpe.test/' });
for (const key of ['window', 'document', 'DOMParser', 'HTMLElement', 'HTMLTextAreaElement', 'Node', 'Event', 'MouseEvent', 'history', 'location'])
    global[key] = key === 'window' ? dom.window : dom.window[key];
Object.defineProperty(global, 'navigator', { value: dom.window.navigator, configurable: true });
global.File = File;
global.IS_REACT_ACT_ENVIRONMENT = true;
global.requestAnimationFrame = fn => { queueMicrotask(fn); return 1; };
let scroll = 0;
Object.defineProperty(window, 'scrollY', { get: () => scroll });
window.scrollTo = ({ top }) => { scroll = top; };
const React = req('react');
const { createRoot } = req('react-dom/client');
const { act } = React;
const ts = req('typescript');
const rows = new Map(), results = new Map(), saveCalls = [];
let failSecond = true, stored = null, releaseHold, failParse = false, holdOriginal = false, legacyOriginal = false;
const hold = new Promise(resolve => { releaseHold = resolve; });
const blank = (title, text) => ({ title, text, html: '<p>' + text + '</p><img src="https://tracking.invalid/pixel"><img src="/api/documents/doc1/assets/owned"><img src="/api/documents/other/assets/private"><style>body{display:none!important}</style><table background="https://tracking.invalid/background"><tbody><tr><td style="background:url(https://tracking.invalid/css)"></td></tr></tbody></table><input type="image" src="https://tracking.invalid/input">', links: [], warnings: [], engine: 'HTML parser', status: 'partial' });
// Exercise the real charset decoder through the component's saved-original path.
const uploadPath = path.join(webRoot, 'lib', 'upload-client.ts');
const uploadModule = new Module(uploadPath);
uploadModule.filename = uploadPath;
uploadModule._compile(ts.transpileModule(fs.readFileSync(uploadPath, 'utf8'), { compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022 } }).outputText, uploadPath);
const helpers = {
    '@/components/ui/button': { Button: ({ asChild, children, variant, ...props }) => asChild ? React.cloneElement(children, props) : React.createElement('button', props, children) },
    '@/lib/clip': { clipHtml: (text, url, name) => { if (failParse === 'result')
            return { ...blank(name, ''), status: 'failed', warnings: ['Parser returned Failed'] }; if (failParse)
            throw new Error('Simulated parser failure'); return blank(name, 'HTML:' + text); }, parseFeed: (text) => blank('Feed', text), textDois: () => [], doiFrom: () => undefined, safeUrl: (value) => {
            try {
                const u = new URL(value);
                return /https?:/.test(u.protocol) ? u.href : null;
            }
            catch {
                return null;
            }
        } },
    '@/lib/imports': { expandUploads: async function* (files) {
            for (const file of files)
                yield { file, path: file.name };
        } },
    '@/lib/image-ocr': { recognizeImage: async () => { throw Error('Unexpected OCR'); } },
    '@/lib/office': { extractOffice: async () => { throw Error('Unexpected Office extraction'); } },
    '@/lib/article-assets': { retainArticleImages: async (record, result) => result, retainOfficeAssets: async (record, result) => result },
    '@/lib/workspace-storage': { readWorkspace: async () => stored, writeWorkspace: async (owner, snapshot) => { assert.equal(owner, 'owner-A'); stored = snapshot; } },
    '@/lib/upload-client': { decodeSource: uploadModule.exports.decodeSource, uploadOriginal: async (file, { onProgress }) => {
            const text = await file.text(), id = 'doc' + (rows.size + 1), row = { id, title: file.name, kind: text.startsWith('<') ? 'html' : 'text', original_name: file.name, status: 'uploaded', engine: '', created_at: new Date().toISOString(), sha256: 'hash', bytes: file.size, source_url: null };
            rows.set(id, row);
            if (file.name === 'hold.txt')
                await hold;
            onProgress?.(1);
            return row;
        }, saveExtracted: async (record, result) => {
            saveCalls.push(record.id);
            if (record.original_name === 'second.txt' && failSecond) {
                failSecond = false;
                throw Error('Simulated interrupted result save');
            }
            results.set(record.id, result);
            rows.set(record.id, { ...record, title: result.title, status: result.status });
        }, captureSource: async () => { throw Error('Not exercised'); }, uploadAssetFile: async () => { throw Error('Not exercised'); } },
};
global.fetch = async (value, options = {}) => {
    const url = new URL(value, 'https://tpe.test');
    if (url.pathname === '/api/documents')
        return Response.json({ documents: [...rows.values()] });
    const id = url.pathname.split('/')[3];
    if (url.pathname.endsWith('/original')) {
        if (holdOriginal)
            await new Promise((resolve, reject) => options.signal.addEventListener('abort', () => reject(new DOMException('Cancelled', 'AbortError')), { once: true }));
        if (legacyOriginal)
            return new Response(Buffer.from('<html><meta charset="windows-1252"><body>Caf\xe9</body></html>', 'latin1'));
        return new Response('<html>First</html>');
    }
    return Response.json({ record: rows.get(id), result: results.get(id) || null });
};
global.Worker = class {
    constructor() { throw Error('HTML must never reach the PDF worker'); }
};
const source = path.join(webRoot, 'app', 'workspace.tsx');
const compiled = ts.transpileModule(fs.readFileSync(source, 'utf8'), { compilerOptions: { module: ts.ModuleKind.CommonJS, jsx: ts.JsxEmit.ReactJSX, target: ts.ScriptTarget.ES2022, esModuleInterop: true } }).outputText;
const mod = new Module(source);
mod.filename = source;
mod.paths = Module._nodeModulePaths(webRoot);
mod.require = name => helpers[name] || req(name);
mod._compile(compiled, source);
const Workspace = mod.exports.default;
(async () => {
    const root = createRoot(document.getElementById('root'));
    await act(async () => { root.render(React.createElement(Workspace, { userId: 'owner-A' })); await new Promise(resolve => setTimeout(resolve, 20)); });
    const fileInput = document.querySelector('input[type=file]');
    Object.defineProperty(fileInput, 'files', { configurable: true, value: [new File(['<html>First</html>'], 'renamed.pdf'), new File(['Second'], 'second.txt')] });
    await act(async () => { fileInput.dispatchEvent(new Event('change', { bubbles: true })); await new Promise(resolve => setTimeout(resolve, 50)); });
    assert.equal(rows.size, 2);
    assert.equal(document.querySelectorAll('.reading img').length, 1);
    assert(document.querySelector('.reading img').src.endsWith('/api/documents/doc1/assets/owned'));
    assert.equal(document.querySelector('.reading style'), null);
    assert.equal(document.querySelector('.reading input'), null);
    assert.equal(document.querySelector('.reading table').hasAttribute('background'), false);
    assert.equal(document.querySelector('.reading [style]'), null);
    assert.equal(document.querySelector('.reading').textContent, 'HTML:First');
    assert.equal(new URL(location.href).searchParams.get('document'), 'doc1');
    const secondRow = [...document.querySelectorAll('.queue-item')].find(el => el.textContent.includes('second.txt'));
    assert.match(secondRow.textContent, /Save again/);
    assert.match(secondRow.textContent, /Simulated interrupted/);
    await act(async () => { secondRow.querySelector('.queue-open').click(); });
    assert.equal(document.querySelector('.reading').textContent, 'Second');
    scroll = 321;
    await act(async () => { document.querySelector('.queue-item .queue-open').click(); });
    assert.equal(document.querySelector('.reading').textContent, 'HTML:First');
    await act(async () => { [...secondRow.querySelectorAll('button')].find(button => button.textContent === 'Save again').click(); await new Promise(resolve => setTimeout(resolve, 30)); });
    assert.equal(saveCalls.at(-1), 'doc2');
    assert.equal(document.querySelector('.reading').textContent, 'HTML:First');
    assert.equal(results.get('doc2').text, 'Second');
    await act(async () => { history.back(); await new Promise(resolve => setTimeout(resolve, 30)); });
    assert.equal(new URL(location.href).searchParams.get('document'), 'doc2');
    assert.equal(document.querySelector('.reading').textContent, 'Second');
    assert.equal(scroll, 321);
    await act(async () => { await new Promise(resolve => setTimeout(resolve, 300)); });
    assert.equal(stored.version, 1);
    assert.equal(stored.items.length, 2);
    assert(stored.items.every(item => item.source.type === 'stored'));
    assert(stored.items.every(item => !item.result));
    await act(async () => { root.unmount(); });
    const reopened = createRoot(document.getElementById('root'));
    await act(async () => { reopened.render(React.createElement(Workspace, { userId: 'owner-A' })); await new Promise(resolve => setTimeout(resolve, 40)); });
    assert.equal(document.querySelector('.reading').textContent, 'Second');
    assert.equal(document.querySelectorAll('.queue-item').length, 2);
    const pick = document.querySelector('input[type=file]');
    Object.defineProperty(pick, 'files', { configurable: true, value: [new File(['Hold'], 'hold.txt'), new File(['Cancel'], 'cancel.txt')] });
    await act(async () => { pick.dispatchEvent(new Event('change', { bubbles: true })); await new Promise(resolve => setTimeout(resolve, 20)); });
    await act(async () => { document.querySelector('button[aria-label="Cancel cancel.txt"]').click(); releaseHold(); await new Promise(resolve => setTimeout(resolve, 30)); });
    assert(![...rows.values()].some(row => row.original_name === 'cancel.txt'));
    assert.match([...document.querySelectorAll('.queue-item')].find(row => row.textContent.includes('cancel.txt')).textContent, /Cancelled/);
    await act(async () => { reopened.unmount(); });
    stored = { ...stored, items: [{ ...stored.items[0], phase: 'extracting' }] };
    history.replaceState({}, '', '/?queue=' + stored.items[0].id);
    const rowCount = rows.size;
    const recovered = createRoot(document.getElementById('root'));
    await act(async () => { recovered.render(React.createElement(Workspace, { userId: 'owner-A' })); await new Promise(resolve => setTimeout(resolve, 30)); });
    assert(document.querySelector('.phase-interrupted'));
    await act(async () => { [...document.querySelectorAll('.queue-item button')].find(button => button.textContent === 'Retry').click(); await new Promise(resolve => setTimeout(resolve, 30)); });
    assert.equal(rows.size, rowCount);
    assert(document.querySelector('.phase-saved'));
    const rereadButton = () => [...document.querySelectorAll('button')].find(button => button.textContent === 'Re-read original');
    legacyOriginal = true;
    const beforeReread = rows.size;
    await act(async () => { rereadButton().click(); await new Promise(resolve => setTimeout(resolve, 40)); });
    assert.equal(rows.size, beforeReread);
    assert.match(document.querySelector('.reading').textContent, /Café/);
    assert.doesNotMatch(document.querySelector('.reading').textContent, /\ufffd/);
    const validResult = results.get('doc1'), savesAfterSuccess = saveCalls.length;
    await act(async () => { document.querySelector('.queue-item .queue-open').click(); });
    assert.match(document.querySelector('.reading').textContent, /Café/);
    failParse = true;
    await act(async () => { rereadButton().click(); await new Promise(resolve => setTimeout(resolve, 35)); });
    assert.equal(saveCalls.length, savesAfterSuccess);
    assert.strictEqual(results.get('doc1'), validResult);
    assert.match(document.querySelector('.reading').textContent, /Café/);
    assert.match(document.querySelector('.queue-item:last-child').textContent, /Simulated parser failure/);
    failParse = 'result';
    await act(async () => { rereadButton().click(); await new Promise(resolve => setTimeout(resolve, 35)); });
    assert.equal(saveCalls.length, savesAfterSuccess);
    assert.strictEqual(results.get('doc1'), validResult);
    assert.match(document.querySelector('.reading').textContent, /Café/);
    assert.match(document.querySelector('.queue-item:last-child').textContent, /Parser returned Failed/);
    failParse = false;
    holdOriginal = true;
    await act(async () => { rereadButton().click(); await new Promise(resolve => setTimeout(resolve, 20)); });
    await act(async () => { document.querySelector('.queue-item:last-child button[aria-label^="Cancel "]').click(); await new Promise(resolve => setTimeout(resolve, 25)); });
    holdOriginal = false;
    assert.equal(saveCalls.length, savesAfterSuccess);
    assert.strictEqual(results.get('doc1'), validResult);
    assert.match(document.querySelector('.reading').textContent, /Café/);
    assert.match(document.querySelector('.queue-item:last-child').textContent, /Cancelled/);
    await act(async () => { recovered.unmount(); });
    console.log(JSON.stringify({ multi_file_independent_failure: true, content_dispatch_over_extension: true, stable_selection: true, retry_owner_preserved: true, back_reader_and_scroll: true, indexeddb_snapshot_drops_saved_files: true, remount_reader_restored: true, private_asset_only_rendering: true, queued_cancel_skips_upload: true, interrupted_retry_reuses_owned_original: true, reader_css_and_active_inputs_removed: true, reread_reuses_owned_original: true, saved_html_charset_recovered: true, failed_reread_keeps_valid_result: true, failed_status_reread_keeps_valid_result: true, cancelled_reread_keeps_valid_result: true }));
})().catch(error => { console.error(error); process.exitCode = 1; });
