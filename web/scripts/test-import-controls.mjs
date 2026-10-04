/** Actual React controls in jsdom; callbacks are synthetic and do not delete data. */
import assert from 'node:assert/strict';
import { createRequire, Module } from 'node:module';
import { fileURLToPath } from 'node:url';
const require = createRequire(import.meta.url);
const { JSDOM } = require('jsdom');
const dom = new JSDOM('<div id="root"></div>', { url: 'https://synthetic-import-controls.test/' });
for (const key of ['window', 'document', 'HTMLElement', 'Element', 'Node', 'NodeFilter', 'HTMLInputElement', 'Event', 'MouseEvent', 'CustomEvent', 'MutationObserver', 'getComputedStyle']) globalThis[key] = dom.window[key];
Object.defineProperty(globalThis, 'navigator', { configurable: true, value: dom.window.navigator });
globalThis.IS_REACT_ACT_ENVIRONMENT = true;
globalThis.requestAnimationFrame = callback => setTimeout(callback, 0);
globalThis.cancelAnimationFrame = clearTimeout;
const React = require('react'), { act } = React, { createRoot } = require('react-dom/client');
const { build } = require(require.resolve('esbuild', { paths: [require.resolve('vite')] }));
const web = fileURLToPath(new URL('../', import.meta.url));
const built = await build({ entryPoints: [web + 'components/import-controls.tsx'], tsconfig: web + 'tsconfig.json', bundle: true, write: false, platform: 'node', format: 'cjs', packages: 'external' });
const compiled = new Module(web + 'synthetic-import-controls.cjs');
compiled.filename = web + 'synthetic-import-controls.cjs'; compiled.paths = Module._nodeModulePaths(web);
compiled._compile(built.outputFiles[0].text, compiled.filename);
const { DeleteStoredDocumentButton, ClearCachedFilesButton, ImportItemControls, ImportItemProgress, ImportBatchProgress, ImportPicker, ImportDropZone } = compiled.exports;
const root = createRoot(document.getElementById('root')), checks = [];
const render = async (component, props) => { await act(async () => root.render(React.createElement(component, props))); };
const button = (label, within = document) => [...within.querySelectorAll('button')].find(element => element.textContent === label);
const dialog = () => document.querySelector('[role="alertdialog"]');
const click = async element => { assert(element, 'Expected a control'); await act(async () => element.click()); };
try {
  let deletes = 0, fail = true, settle;
  await render(DeleteStoredDocumentButton, { documentName: 'Synthetic <source>.pdf', onDelete: async () => {
    deletes++; if (fail) throw new Error('Synthetic storage deletion failed'); await new Promise(resolve => { settle = resolve; });
  } });
  await click(button('Delete saved document'));
  assert(dialog()); assert.equal(deletes, 0); assert.match(dialog().textContent, /Synthetic <source>.pdf/);
  assert.equal(dialog().querySelector('source'), null);
  await click(button('Keep', dialog())); assert.equal(deletes, 0); assert.equal(dialog(), null);
  checks.push('Stored deletion requires a named confirmation; cancelling leaves the callback untouched');
  await click(button('Delete saved document')); await click(button('Delete saved document', dialog()));
  assert.equal(deletes, 1); assert(dialog()); assert.match(dialog().querySelector('[role="alert"]').textContent, /Synthetic storage deletion failed/);
  fail = false;
  const confirm = button('Delete saved document', dialog());
  await act(async () => { confirm.click(); confirm.click(); });
  assert.equal(deletes, 2); assert.equal(dialog().getAttribute('aria-busy'), 'true'); assert.equal(button('Keep', dialog()).disabled, true);
  await act(async () => settle()); assert.equal(dialog(), null);
  checks.push('Failed deletion remains visible and retryable; double confirmation starts only one operation');

  let clears = 0;
  await render(ClearCachedFilesButton, { onClear: async () => { clears++; } });
  await click(button('Clear saved copies')); assert.equal(clears, 0);
  assert.match(dialog().textContent, /Saved documents and originals stay in your library/);
  assert.match(dialog().textContent, /Unfinished imports and unsaved results are kept/);
  await click(button('Clear saved copies', dialog())); assert.equal(clears, 1);
  checks.push('Cache clear explains local-only scope and preserved unfinished work before confirmation');

  const item = { id: 'synthetic', name: 'synthetic.txt', phase: 'uploading', progress: null, message: 'Saving the original' };
  let cancels = 0, retries = 0, removes = 0;
  const actions = { item, onCancel: () => { cancels++; }, onRetry: () => { retries++; }, onRemove: async () => { removes++; } };
  await render(ImportItemControls, actions);
  assert.equal(button('Remove from queue').disabled, true); await click(button('Cancel')); assert.equal(cancels, 1); assert.equal(removes, 0);
  await render(ImportItemControls, { ...actions, item: { ...item, phase: 'cancelled' }, hasLocalOnlyData: true });
  await click(button('Retry')); assert.equal(retries, 1);
  await click(button('Remove from queue')); assert(dialog()); assert.equal(removes, 0);
  assert.match(dialog().textContent, /unfinished recovery copy/); await click(button('Keep', dialog())); assert.equal(removes, 0);
  await render(ImportItemControls, { ...actions, item: { ...item, phase: 'failed', savePending: true } });
  assert(button('Save again')); await click(button('Remove from queue'));
  assert(dialog()); await click(button('Remove from queue', dialog())); assert.equal(removes, 1); assert.equal(deletes, 2);
  checks.push('Active removal is disabled; cancellation/retry are separate; discarding unsaved work needs confirmation');

  await render(ImportItemProgress, { item });
  assert.equal(document.querySelector('progress').hasAttribute('value'), false);
  assert.match(document.querySelector('progress').getAttribute('aria-label'), /Saving original progress for synthetic.txt/);
  await render(ImportItemProgress, { item: { ...item, progress: 25 } });
  assert.equal(document.querySelector('progress').getAttribute('value'), '25'); assert.match(document.body.textContent, /25% of this step/);
  await render(ImportBatchProgress, { items: [{ phase: 'saved' }, { phase: 'uploading' }, { phase: 'failed' }] });
  assert.match(document.body.textContent, /1 of 3 saved.*1 in progress.*1 need attention/);
  assert.equal(document.querySelector('progress'), null);
  checks.push('Progress distinguishes unknown work, current-step percentages and aggregate outcomes');

  const selections = [], source = new File(['synthetic'], 'repeat.txt');
  await render(ImportPicker, { onFiles: selected => selections.push(selected) });
  let input = document.querySelector('input[aria-label="Choose source files"]'); assert.equal(input.multiple, true);
  Object.defineProperty(input, 'files', { configurable: true, value: [source] });
  await act(async () => input.dispatchEvent(new Event('change', { bubbles: true })));
  input = document.querySelector('input[aria-label="Choose source files"]');
  Object.defineProperty(input, 'files', { configurable: true, value: [source] });
  await act(async () => input.dispatchEvent(new Event('change', { bubbles: true })));
  assert.equal(selections.length, 2); assert.equal(selections[0][0].path, 'repeat.txt'); assert.equal(input.value, '');
  assert.match(document.body.textContent, /each selection joins this queue/);
  checks.push('Picker is additive and resets its input so the same source can be explicitly reimported');

  const dropped = [], errors = [];
  await render(ImportDropZone, { onFile: value => dropped.push(value), onError: (path, error) => errors.push({ path, error }), children: React.createElement('span', null, 'Drop target') });
  const drop = new Event('drop', { bubbles: true, cancelable: true });
  Object.defineProperty(drop, 'dataTransfer', { value: { types: ['Files'], files: [source], items: [] } });
  await act(async () => document.querySelector('#root > div').dispatchEvent(drop));
  assert.equal(dropped.length, 1); assert.equal(errors.length, 0); assert.equal(drop.defaultPrevented, true);
  checks.push('Drop control adds a synthetic file through its callback');
  console.log(JSON.stringify({ environment: 'React/jsdom with synthetic callbacks; no actual browser, upload or deletion', checks }, null, 2));
} finally { await act(async () => root.unmount()); dom.window.close(); }
