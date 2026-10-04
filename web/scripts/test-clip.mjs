import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { readFileSync } from 'node:fs';
import { JSDOM } from 'jsdom';
const require = createRequire(import.meta.url);
const { build } = require(require.resolve('esbuild', { paths: [require.resolve('vite')] }));
const dom = new JSDOM('', { url: 'https://capture.test/' });
for (const name of ['window', 'document', 'DOMParser', 'Node', 'HTMLElement']) globalThis[name] = dom.window[name];
const built = await build({ entryPoints: ['lib/clip.ts'], bundle: true, write: false, format: 'esm', platform: 'node' });
const { clipHtml, clipText, parseFeed, textDois } = await import(`data:text/javascript;base64,${Buffer.from(built.outputFiles[0].text).toString('base64')}`);
// Independently authored reduced modern-page fixture; no copied article prose.
const paragraph = 'This compact computer review records a measured experiment, compares the hardware, and explains the outcome for a reader. '.repeat(4);
const source = `<!doctype html><html lang="en"><head><title>Compact computer review</title><base href="https://journal.test/reviews/"><meta name="author" content="Reviewer"><script type="application/ld+json">{"@type":"Review","headline":"Compact computer review","author":{"@type":"Person","name":"Reviewer"},"articleBody":"HYDRATION_SECRET"}</script></head><body>
<header><a href="/account">Account chrome</a></header><nav><a href="/noise">NAVIGATION_NOISE</a></nav><main><article><h1>Compact computer review</h1><p>${paragraph}</p>
<script>window.state={blob:"BLOB_SCRIPT_SECRET",base64:"AAAAAAAA"};</script><style>.x:before{content:"STYLE_SECRET"}</style><template>TEMPLATE_SECRET</template>
<div hidden>HIDDEN_SECRET</div><div aria-hidden="true">ARIA_SECRET</div><div style="display: none !important;">DISPLAY_SECRET</div><div style="visibility:hidden">VISIBILITY_SECRET</div>
<h2>Design and measurements</h2><p>${paragraph}<a href="https://doi.org/10.1234/real-target">Research paper</a></p>
<figure><picture><source srcset="../photo-small.jpg 300w, ../photo-large.jpg 1600w"><img src="data:image/gif;base64,R0lGODlhAQAB" alt="The tested computer"></picture><figcaption>Measured hardware on the bench.</figcaption></figure>
<figure><img data-src="../lazy-photo.jpg" src="/transparent.gif" alt="Rear ports"><figcaption>Ports and connectors.</figcaption></figure>
<img src="blob:https://journal.test/unresolvable" alt="Unavailable image"><img src="/pixel.gif" width="1" height="1">
<table><thead><tr><th>Test</th><th>Result</th></tr></thead><tbody><tr><td>Power</td><td>18 watts</td></tr></tbody></table>
<pre><code>const value = 42;</code></pre><p>Résumé, naïve, 日本語, αβ and é remain unchanged.</p><p>${paragraph}</p></article></main><aside><table><tr><td>SIDEBAR_TABLE_NOISE</td></tr></table><a href="/sidebar">SIDEBAR_LINK_NOISE</a></aside><footer>FOOTER_SECRET</footer></body></html>`;
const clipped = clipHtml(source, 'https://journal.test/original');
for (const noise of ['BLOB_SCRIPT_SECRET', 'STYLE_SECRET', 'TEMPLATE_SECRET', 'HIDDEN_SECRET', 'ARIA_SECRET', 'DISPLAY_SECRET', 'VISIBILITY_SECRET', 'NAVIGATION_NOISE', 'SIDEBAR_TABLE_NOISE', 'SIDEBAR_LINK_NOISE', 'FOOTER_SECRET', 'HYDRATION_SECRET']) assert.ok(!JSON.stringify(clipped).includes(noise), `leaked ${noise}`);
assert.match(clipped.text, /Résumé, naïve, 日本語, αβ and é/);
assert.match(clipped.text, /18 watts/);
assert.match(clipped.markdown, /const value = 42/);
assert.ok(clipped.links.some(link => link.doi === '10.1234/real-target' && link.label === 'Research paper'));
assert.ok(!clipped.links.some(link => /account|noise|sidebar/.test(link.url)));
assert.deepEqual(clipped.tables, [[['Test', 'Result'], ['Power', '18 watts']]]);
assert.deepEqual(clipped.metadata.images.map(image => image.url), ['https://journal.test/photo-large.jpg', 'https://journal.test/lazy-photo.jpg']);
assert.equal(clipped.metadata.images[0].caption, 'Measured hardware on the bench.');
assert.match(clipped.html, /data-image-id="image-1"/);
assert.ok(!/\[Image:|data:image|blob:|srcset=/.test(clipped.html));
assert.ok(clipped.metadata.headings.some(heading => heading.title === 'Design and measurements'));
const structuredSource = `<main><article><h1>Measured comparison</h1><p>${paragraph}</p><h2>Reference grid</h2><table role="presentation"><tr><td>Equation reference</td><td>Structured value 17</td></tr></table><h2>Second grid</h2><table><tr><td>Independent row</td><td>Structured value 29</td></tr></table><p>${paragraph}</p></article></main>`;
const structured = clipHtml(structuredSource, 'https://journal.test/structured');
assert.match(structured.text, /Structured value 17/);
assert.match(structured.text, /Structured value 29/);
assert.equal(structured.tables.length, 2);
const documentation = clipHtml(`<main><div class="layout__header"><h1>Function reference</h1><section><p>Critical definition before the body wrapper.</p></section></div><div class="layout__body"><h2>Examples</h2><p>${paragraph}</p><aside><aside role="doc-footnote"><p>Important authored footnote.</p></aside></aside><p>${paragraph}</p></div></main>`, 'https://docs.test/reference');
assert.match(documentation.text, /Critical definition before the body wrapper/);
assert.match(documentation.text, /Important authored footnote/);
const malformed = clipHtml('<article><h1>Encoding</h1><p>Damaged � but α stays.</p></article>', 'https://journal.test/');
assert.ok(malformed.warnings.some(warning => warning.includes('U+FFFD')));
assert.match(malformed.text, /�/);
const plain = clipText('body::after { content: "<script>text</script>"; }', 'style.css', 'css');
assert.equal(plain.text, 'body::after { content: "<script>text</script>"; }');
assert.ok(!plain.html.includes('<script>'));
assert.equal(plain.engine, 'CSS source capture');
const large = clipHtml(source.replace('</head>', `<script>${'x'.repeat(4 * 1024 * 1024 + 1)}</script></head>`), 'https://journal.test/');
assert.match(large.text, /Design and measurements/);
assert.equal(textDois(Array.from({ length: 1001 }, (_, i) => `10.1234/reference-${i}`).join(' ')).length, 1001);
const atom = parseFeed(`<feed xmlns="http://www.w3.org/2005/Atom" xml:base="https://feed.test/root/"><title>Research feed</title><entry xml:base="posts/"><id>1</id><title>Literal content</title><link href="one"/><content type="text">Use &lt;tag&gt; literally.</content><link rel="enclosure" href="audio.mp3" type="audio/mpeg"/></entry><entry><id>2</id><title>Full content</title><content type="html" xml:base="assets/">&lt;p&gt;Full entry.&lt;a href="paper"&gt;Paper&lt;/a&gt;&lt;/p&gt;</content></entry></feed>`, 'https://feed.test/feed.xml');
assert.equal(atom.entries[0].url, 'https://feed.test/root/posts/one');
assert.match(atom.entries[0].text, /<tag>/);
assert.equal(atom.entries[0].enclosures[0].url, 'https://feed.test/root/posts/audio.mp3');
assert.equal(atom.entries[1].links[0].url, 'https://feed.test/root/assets/paper');
const rss = `<rss version="2.0"><channel><title>Many entries</title>${Array.from({ length: 501 }, (_, index) => `<item><guid>${index}</guid><title>Entry ${index}</title><description><![CDATA[<p>Retained entry ${index}.</p>]]></description></item>`).join('')}</channel></rss>`;
assert.equal(parseFeed(rss, 'https://feed.test/').entries.length, 501);
assert.throws(() => parseFeed('<!DOCTYPE rss [<!ENTITY x "bad">]><rss/>', 'https://feed.test/'), /entity declarations/);
if (process.env.HTML_REVIEW_FIXTURE) {
  const actual = clipHtml(readFileSync(process.env.HTML_REVIEW_FIXTURE, 'utf8'), 'https://www.howtogeek.com/lenovo-yoga-mini-gen-11-review/');
  assert.match(actual.title, /Lenovo/i);
  assert.match(actual.text, /Yoga Mini Gen 11/i);
  assert.ok(actual.text.length > 5000);
  assert.ok(actual.metadata.images.length > 0);
  assert.ok(!/<script|data:image|blob:|\[Image:/.test(actual.html));
  console.log(JSON.stringify({ actualFixture: process.env.HTML_REVIEW_FIXTURE, textCharacters: actual.text.length, images: actual.metadata.images.length, tables: actual.tables.length, headings: actual.metadata.headings.length, links: actual.links.length }));
}
console.log('HTML cleanup, Unicode, DOI targets, lazy images/captions, tables, CSS, >4 MiB input, >1000 DOIs, and >500 feed entries passed.');
