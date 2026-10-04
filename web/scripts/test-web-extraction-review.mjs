// Independent content-retention checks. No fixture pages are fetched or scripts executed.
// Optional: node scripts/test-web-extraction-review.mjs --fixtures /absolute/fixture/directory
import assert from 'node:assert/strict';
import { readFileSync, writeFileSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { createRequire } from 'node:module';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { JSDOM, VirtualConsole } from 'jsdom';
import { Readability } from '@mozilla/readability';
const require = createRequire(import.meta.url);
const { build } = require(require.resolve('esbuild', { paths: [require.resolve('vite')] }));
const web = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const window = new JSDOM('', { virtualConsole: new VirtualConsole() }).window;
for (const key of ['window', 'document', 'DOMParser', 'Node', 'Element', 'HTMLElement']) globalThis[key] = window[key];
const clipSource = readFileSync(resolve(web, 'lib/clip.ts'), 'utf8');
const clipSha256 = createHash('sha256').update(clipSource).digest('hex');
const bundle = await build({ stdin: {contents: clipSource, loader:'ts', resolveDir:resolve(web,'lib'), sourcefile:'clip.ts'}, bundle: true, write: false, format: 'esm', platform: 'node' });
const { clipHtml, parseFeed } = await import(`data:text/javascript;base64,${Buffer.from(bundle.outputFiles[0].text).toString('base64')}`);
const norm = text => text.replace(/\s+/g, ' ').trim();
const cases = [
  ['Atom plain text keeps literal markup and resolves inherited base', () => {
    const result = parseFeed('<feed xmlns="http://www.w3.org/2005/Atom" xml:base="https://example.test/root/"><title>Feed</title><entry xml:base="posts/"><title>Example</title><link href="entry"/><content type="text">Literal &lt;em&gt;markup&lt;/em&gt; and &amp; signs.</content><link rel="enclosure" href="audio.ogg" type="audio/ogg"/></entry></feed>', 'https://example.test/feed');
    assert.equal(result.entries[0].text, 'Literal <em>markup</em> and & signs.');
    assert.equal(result.entries[0].url, 'https://example.test/root/posts/entry');
    assert.equal(result.entries[0].enclosures[0].url, 'https://example.test/root/posts/audio.ogg');
  }],
  ['RSS full content uses its namespace, not an unrelated extension', () => {
    const result = parseFeed('<rss version="2.0" xmlns:content="http://purl.org/rss/1.0/modules/content/" xmlns:other="urn:other"><channel><title>Feed</title><item><title>Example</title><other:encoded>Wrong extension payload</other:encoded><content:encoded><![CDATA[<p>Full authored content.</p>]]></content:encoded><description>Short summary.</description></item></channel></rss>', 'https://example.test/feed');
    assert.match(result.entries[0].text, /Full authored content/);
    assert.doesNotMatch(result.entries[0].text, /Wrong extension payload|Short summary/);
  }],
  ['Atom external content retains available summary and unresolved target', () => {
    const result = parseFeed('<feed xmlns="http://www.w3.org/2005/Atom" xml:base="https://example.test/"><title>Feed</title><entry><id>urn:entry</id><title>Example</title><content type="text/html" src="full.html"/><summary>Useful authored summary.</summary></entry></feed>', 'https://example.test/feed');
    assert.match(result.entries[0].text, /Useful authored summary/);
    assert.match(JSON.stringify(result), /https:\/\/example\.test\/full\.html/);
    assert.equal(result.status, 'partial');
  }],
  ['Static script-only shell does not invent the browser-rendered article', () => {
    const result = clipHtml('<title>Dynamic page</title><nav>Navigation only</nav><main id="app"></main><script>document.querySelector("main").innerHTML="Unexecuted article"</script>', 'https://example.test/dynamic');
    assert.doesNotMatch(result.text, /Unexecuted article|Navigation only/);
    assert.equal(result.status, 'partial');
    assert(result.warnings.some(value => /script|render/i.test(value)));
  }],
  ['Selected article retains lazy images and their caption without page chrome evidence', () => {
    const source = '<title>Camera report</title><nav><a href="/account">Account</a><img src="/nav.png"></nav><article><h1>Camera report</h1><p>' + 'The camera records detailed photographs in natural light. '.repeat(12) + '</p><figure><img src="/spacer.gif" data-src="/real.jpg" alt="Test scene"><figcaption>Observed light.</figcaption></figure><p>Study <a href="https://doi.org/10.1234/example">supporting evidence</a>.</p></article>';
    const result = clipHtml(source, 'https://example.test/report');
    assert.match(result.html, /<img[^>]+https:\/\/example\.test\/real\.jpg/);
    assert.match(result.html, /Observed light/);
    assert(!result.links.some(link => /\/account$/.test(link.url)));
    assert(!result.metadata.images.some(image => /nav\.png/.test(image.url)));
    assert.doesNotMatch(result.html, /\[Image:/);
  }],
];
let failed = 0;
for (const [name, check] of cases) {
  try { check(); console.log(`PASS ${name}`); }
  catch (error) { failed++; console.error(`FAIL ${name}: ${error.message}`); }
}
const fixtureOption = process.argv.indexOf('--fixtures');
if (fixtureOption >= 0) {
  const directory = resolve(process.argv[fixtureOption + 1]);
  const fixtures = JSON.parse(readFileSync(resolve(directory, 'fetch-results.json'), 'utf8'));
  fixtures.unshift({ name: 'howtogeek', url: 'https://www.howtogeek.com/lenovo-yoga-mini-gen-11-review/', final_url: 'https://www.howtogeek.com/lenovo-yoga-mini-gen-11-review/', file: 'howtogeek.html', http_status: '200', sha256: '641925e077f0bf0f4d3d95d4931b79fe1ca39e9d4402c1e21b47c47260267eb1' });
  const references = {
    howtogeek: { root: '#article-body', paragraphs: '.content-block-regular > p', heading: 'h2,h3', table: 'table', noise: ['Google is updating how articles', 'Unlock Personalized Content', 'We go hands-on with every product'] },
    python: { root: '[role="main"]', paragraphs: 'p', table: 'table', noise: ['Quick search', 'Navigation'] },
    mdn: { root: 'main#content', paragraphs: 'p', table: 'table', noise: ['Help improve MDN', 'Sign in'] },
    wikipedia: { root: '.mw-parser-output', paragraphs: 'p', table: 'table.wikitable', noise: ['Create account', 'Personal tools'] },
    arxiv: { root: 'article.ltx_document', paragraphs: 'p.ltx_p', table: 'table', noise: ['Instructions for reporting errors', 'Report an issue'] },
    pmc: { root: 'article', paragraphs: 'p', table: 'table', noise: ['An official website of the United States government', 'Search PMC Full-Text Archive'] },
    nasa: { root: 'main', paragraphs: 'p', table: 'table', noise: ['Suggested Searches', 'Search All NASA Missions'] },
    nasa_image: { root: 'main', paragraphs: 'p', table: 'table', noise: ['Suggested Searches', 'Search All NASA Missions'] },
    blog: { root: 'article', paragraphs: 'p', table: 'table', noise: ['Subscribe via email'] },
  };
  const dom = (source, url) => new JSDOM(source, { url, virtualConsole: new VirtualConsole() });
  const measures = [];
  function sample(run) { const timings = []; let result; for (let i = 0; i < 5; i++) { const start = performance.now(); result = run(); timings.push(performance.now() - start); } return { result, cold_ms: +timings[0].toFixed(2), median_ms: +timings.slice(1).sort((a,b) => a-b)[1].toFixed(2) }; }
  for (const item of fixtures) {
    if (item.http_status !== '200') { measures.push({ ...item, skipped: 'non-200 response' }); continue; }
    const bytes = readFileSync(resolve(directory, item.file));
    assert.equal(createHash('sha256').update(bytes).digest('hex'), item.sha256, `${item.name}: fixture bytes changed`);
    const source = bytes.toString('utf8'); const url = item.final_url || item.url;
    if (/rss|atom/.test(item.name)) {
      const {result, ...timing} = sample(() => parseFeed(source, url));
      const xml = new window.DOMParser().parseFromString(source, 'application/xml');
      const sourceEntries = xml.getElementsByTagNameNS('*', item.name === 'atom' ? 'entry' : 'item').length;
      measures.push({ ...item, sourceEntries, entries: result.entries.length, images: result.entries.reduce((sum,e) => sum+(e.images?.length||0),0), links: result.links.length, ...timing });
      assert.equal(result.entries.length, sourceEntries); continue;
    }
    const document = dom(source, url).window.document; const rule = references[item.name];
    const selected = document.querySelector(rule.root); assert(selected, `${item.name}: reference boundary missing`);
    const paragraphs = [...selected.querySelectorAll(rule.paragraphs)].map(e => norm(e.textContent)).filter(Boolean);
    const headings = [...selected.querySelectorAll(rule.heading || 'h2,h3,h4')].map(e => norm(e.textContent).replace(/¶$/,'')).filter(Boolean);
    const cells = [...selected.querySelectorAll(`${rule.table} th,${rule.table} td`)].map(e => norm(e.textContent)).filter(Boolean);
    const code = [...selected.querySelectorAll('pre')].map(e => norm(e.textContent)).filter(Boolean);
    const variants = [ ['readability', () => new Readability(dom(source, url).window.document, {charThreshold:100}).parse()], ['fixed', () => clipHtml(source, url)] ];
    const scores = {};
    for (const [engine, run] of variants) {
      const { result, ...timing } = sample(run); const output = dom(result?.html || result?.content || '', url).window.document;
      const text = norm(output.body.textContent);
      scores[engine] = { ...timing, textCharacters: text.length, paragraphs: paragraphs.filter(p => text.includes(p)).length, headings: headings.filter(p => text.includes(p)).length, tableCells: cells.filter(p => text.includes(p)).length, codeBlocks: code.filter(p => text.includes(p)).length, images: output.querySelectorAll('img').length, uniqueImages: new Set([...output.querySelectorAll('img')].map(e=>e.src)).size, links: output.querySelectorAll('a[href]').length, tables: output.querySelectorAll('table').length, noise: rule.noise.filter(p => text.includes(p)), warnings: result?.warnings || [] };
      if (item.name === 'wikipedia' && engine === 'fixed') assert.equal(scores[engine].tableCells, cells.length, 'Wikipedia comparison tables must survive article scoring');
      if (item.name === 'howtogeek' && engine === 'fixed') { assert.equal(scores[engine].paragraphs, 24); assert.equal(scores[engine].headings, 10); assert(scores[engine].images > 0); assert.equal(scores[engine].noise.length, 0); }
    }
    measures.push({ ...item, bytes: bytes.length, reference: { selector: rule.root, paragraphs: paragraphs.length, headings: headings.length, tableCells: cells.length, codeBlocks: code.length }, scores });
  }
  const report = { clipSha256, generatedAt: new Date().toISOString(), node: process.version, method: 'One cold run and median of four subsequent runs (lower middle), includes DOM construction, excludes fetch, module load, and asset downloads. Reference paragraphs and cells are exact normalized-text matches, not a universal completeness score.', fixtures: measures };
  writeFileSync(resolve(directory, 'benchmark.json'), JSON.stringify(report, null, 2));
  console.log(`Wrote ${measures.length}-source benchmark to ${resolve(directory, 'benchmark.json')}`);
}
if (failed) process.exitCode = 1;
