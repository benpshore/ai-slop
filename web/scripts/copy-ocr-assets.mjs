import { createRequire } from 'node:module';
import { readFile, mkdir, copyFile, writeFile } from 'node:fs/promises';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createHash } from 'node:crypto';
const require = createRequire(import.meta.url);
const web = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const output = join(web, 'public/ocr/7.0.0');
const packages = { 'tesseract.js': '7.0.0', 'tesseract.js-core': '7.0.0', '@tesseract.js-data/eng': '1.0.0' };
const roots = {};
for (const [name, version] of Object.entries(packages)) {
  const manifest = require.resolve(`${name}/package.json`);
  const actual = JSON.parse(await readFile(manifest, 'utf8'));
  if (actual.version !== version) throw new Error(`OCR package version mismatch: ${name} ${actual.version}, expected ${version}`);
  roots[name] = dirname(manifest);
}
const files = [
  ['tesseract.js', 'dist/worker.min.js', 'worker.min.js'],
  ['tesseract.js', 'LICENSE.md', 'licenses/tesseract.js.txt'],
  ['tesseract.js-core', 'LICENSE', 'licenses/tesseract.js-core.txt'],
  ['@tesseract.js-data/eng', 'README.md', 'licenses/eng-model-readme.txt'],
  ['@tesseract.js-data/eng', '4.0.0_best_int/eng.traineddata.gz', 'lang/eng.traineddata.gz'],
];
for (const variant of ['', '-simd', '-relaxedsimd', '-lstm', '-simd-lstm', '-relaxedsimd-lstm']) files.push(['tesseract.js-core', `tesseract-core${variant}.wasm.js`, `core/tesseract-core${variant}.wasm.js`]);
const manifest = { packages, generatedBy: 'scripts/copy-ocr-assets.mjs', files: [] };
for (const [name, source, target] of files) {
  const path = join(roots[name], source), bytes = await readFile(path);
  await mkdir(dirname(join(output, target)), { recursive: true });
  await copyFile(path, join(output, target));
  manifest.files.push({ path: target, bytes: bytes.length, sha256: createHash('sha256').update(bytes).digest('hex') });
}
await writeFile(join(output, 'manifest.json'), `${JSON.stringify(manifest, null, 2)}\n`);
console.log(`Copied ${manifest.files.length} pinned same-origin OCR assets (${manifest.files.reduce((sum, item) => sum + item.bytes, 0)} bytes).`);
