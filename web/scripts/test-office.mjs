import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { zipSync, strToU8 } from 'fflate';
import { JSDOM } from 'jsdom';

const require = createRequire(import.meta.url);
const { build } = require(require.resolve('esbuild', { paths: [require.resolve('vite')] }));
const built = await build({ entryPoints: ['lib/office.ts'], bundle: true, write: false, format: 'esm', platform: 'node' });
const { extractOffice, detectOffice } = await import(`data:text/javascript;base64,${Buffer.from(built.outputFiles[0].text).toString('base64')}`);
const dom = new JSDOM('');
globalThis.DOMParser = dom.window.DOMParser;
const w = 'http://schemas.openxmlformats.org/wordprocessingml/2006/main';
const r = 'http://schemas.openxmlformats.org/officeDocument/2006/relationships';
const a = 'http://schemas.openxmlformats.org/drawingml/2006/main';
const p = 'http://schemas.openxmlformats.org/presentationml/2006/main';
const x = 'http://schemas.openxmlformats.org/spreadsheetml/2006/main';
const rels = items => `<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">${items.map(([id, type, target, external]) => `<Relationship Id="${id}" Type="${r}/${type}" Target="${target}"${external ? ' TargetMode="External"' : ''}/>`).join('')}</Relationships>`;
const png = Uint8Array.from(Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aRZkAAAAASUVORK5CYII=', 'base64'));
const packageBytes = parts => zipSync(Object.fromEntries(Object.entries(parts).map(([name, data]) => [name, typeof data === 'string' ? strToU8(data) : data])));
let reads = 0;
class StreamFile extends File {
  constructor(bytes, name) { super([bytes], name); this.bytes = bytes; }
  arrayBuffer() { throw new Error('Whole-file arrayBuffer is forbidden'); }
  stream() {
    let position = 0;
    const bytes = this.bytes;
    return new ReadableStream({ pull(controller) {
      reads++;
      if (position === bytes.length) { controller.close(); return; }
      const end = Math.min(position + 31, bytes.length);
      controller.enqueue(bytes.slice(position, end)); position = end;
    } });
  }
}
const file = (parts, name) => new StreamFile(packageBytes(parts), name);
const paragraph = text => `<w:p><w:r><w:t>${text}</w:t></w:r></w:p>`;
const document = body => `<w:document xmlns:w="${w}" xmlns:r="${r}" xmlns:a="${a}" xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing"><w:body>${body}</w:body></w:document>`;

const docxParts = {
  '[Content_Types].xml': '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"/>',
  '_rels/.rels': rels([['main', 'officeDocument', 'word/document.xml']]),
  'word/document.xml': document(`<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>Document title</w:t></w:r></w:p>${paragraph('Before')}<w:p><w:hyperlink r:id="link"><w:r><w:t>DOI source</w:t></w:r></w:hyperlink></w:p><w:p><w:r><w:drawing><wp:docPr id="1" descr="Actual red pixel"/><a:blip r:embed="image"/></w:drawing></w:r></w:p><w:tbl><w:tr><w:tc>${paragraph('Cell A')}</w:tc><w:tc>${paragraph('Cell B')}</w:tc></w:tr></w:tbl>${paragraph('After')}<w:p><w:del><w:r><w:t>DELETED</w:t></w:r></w:del><w:r><w:t>Kept</w:t></w:r></w:p>`),
  'word/_rels/document.xml.rels': rels([['link', 'hyperlink', 'https://doi.org/10.1234/example', true], ['image', 'image', 'media/pixel.png']]),
  'word/media/pixel.png': png,
  'word/media/unused.bin': new Uint8Array(2 * 1024 * 1024).fill(41),
  'docProps/core.xml': '<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>Fixture title</dc:title><dc:creator>Fixture author</dc:creator></cp:coreProperties>',
};
const docx = file(docxParts, 'fixture.docx');
assert.equal(await detectOffice(docx), 'docx');
assert.equal(reads, 0, 'Sniff reads only bounded tail/directory slices, never streams the archive');
const result = await extractOffice(docx);
assert.equal(result.extracted.status, 'ready', result.extracted.warnings.join('\n'));
assert.equal(result.extracted.title, 'Fixture title');
assert.match(result.extracted.html, /<h1>Document title<\/h1>/);
assert.match(result.extracted.html, /<table><tr><td><p>Cell A<\/p><\/td><td><p>Cell B<\/p>/);
assert.ok(result.extracted.html.indexOf('Before') < result.extracted.html.indexOf('data-image-id'));
assert.ok(result.extracted.html.indexOf('data-image-id') < result.extracted.html.indexOf('Cell A'));
assert.ok(result.extracted.html.indexOf('Cell B') < result.extracted.html.indexOf('After'));
assert.equal(result.extracted.links[0].url, 'https://doi.org/10.1234/example');
assert.doesNotMatch(result.extracted.text, /DELETED/);
assert.equal(result.assets.length, 1);
assert.equal(result.assets[0].file.type, 'image/png');
assert.deepEqual(new Uint8Array(await result.assets[0].file.arrayBuffer()), png);
assert.match(result.extracted.html, new RegExp(`data-image-id="${result.assets[0].id}"`));
assert.ok(reads > 1);

let uploaded = [];
const streamed = await extractOffice(docx, undefined, async asset => {
  await new Promise(resolve => setTimeout(resolve, 1));
  uploaded.push({ id: asset.id, bytes: new Uint8Array(await asset.file.arrayBuffer()) });
});
assert.equal(streamed.assets.length, 0);
assert.equal(uploaded.length, 1);
assert.deepEqual(uploaded[0].bytes, png);
assert.equal(streamed.extracted.status, 'ready');
const failedSink = await extractOffice(docx, undefined, async () => { throw new Error('Storage unavailable'); });
assert.equal(failedSink.extracted.status, 'partial');
assert.doesNotMatch(failedSink.extracted.html, /data-image-id/);
assert.match(failedSink.extracted.warnings.join(), /Storage unavailable/);

const spacing = await extractOffice(file({ 'word/document.xml': document('<w:p><w:r><w:t>First</w:t><w:tab/><w:t>Second</w:t><w:br/><w:t>Third</w:t></w:r></w:p>') }, 'spacing.docx'));
assert.equal(spacing.extracted.text, 'First\tSecond\nThird');

// Corrupt an unused deflated binary; a decoder that inflated every entry would fail.
const skipBytes = packageBytes(docxParts);
for (let offset = 0; offset + 30 < skipBytes.length;) {
  const view = new DataView(skipBytes.buffer, offset);
  if (view.getUint32(0, true) !== 0x04034b50) break;
  const size = view.getUint32(18, true), nameSize = view.getUint16(26, true), extraSize = view.getUint16(28, true);
  const name = new TextDecoder().decode(skipBytes.subarray(offset + 30, offset + 30 + nameSize));
  const start = offset + 30 + nameSize + extraSize;
  if (name === 'word/media/unused.bin') skipBytes.fill(255, start, start + size);
  offset = start + size;
}
assert.equal((await extractOffice(new StreamFile(skipBytes, 'skip.docx'))).extracted.status, 'ready');

const shape = (text, title = false) => `<p:sp><p:nvSpPr><p:nvPr>${title ? '<p:ph type="title"/>' : ''}</p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>${text}</a:t></a:r></a:p></p:txBody></p:sp>`;
const slide = body => `<p:sld xmlns:p="${p}" xmlns:a="${a}" xmlns:r="${r}"><p:cSld><p:spTree>${body}</p:spTree></p:cSld></p:sld>`;
const pptx = file({
  'ppt/presentation.xml': `<p:presentation xmlns:p="${p}" xmlns:r="${r}"><p:sldIdLst><p:sldId id="256" r:id="second"/><p:sldId id="257" r:id="first"/></p:sldIdLst></p:presentation>`,
  'ppt/_rels/presentation.xml.rels': rels([['first', 'slide', 'slides/slide1.xml'], ['second', 'slide', 'slides/slide2.xml']]),
  'ppt/slides/slide1.xml': slide(shape('Last slide')),
  'ppt/slides/slide2.xml': slide(`${shape('First slide', true)}<p:pic><p:nvPicPr><p:cNvPr id="1" descr="Slide image"/></p:nvPicPr><p:blipFill><a:blip r:embed="image"/></p:blipFill></p:pic><p:sp><p:txBody><a:p><a:r><a:rPr><a:hlinkClick r:id="url"/></a:rPr><a:t>Slide link</a:t></a:r></a:p></p:txBody></p:sp><p:graphicFrame><a:graphic><a:graphicData><a:tbl><a:tr><a:tc><a:txBody><a:p><a:r><a:t>Slide cell</a:t></a:r></a:p></a:txBody></a:tc></a:tr></a:tbl></a:graphicData></a:graphic></p:graphicFrame>`),
  'ppt/slides/_rels/slide2.xml.rels': rels([['image', 'image', '../media/pixel.png'], ['url', 'hyperlink', 'https://example.org/slide', true], ['notes', 'notesSlide', '../notesSlides/notesSlide1.xml']]),
  'ppt/notesSlides/notesSlide1.xml': `<p:notes xmlns:p="${p}" xmlns:a="${a}"><p:cSld><p:spTree><p:sp><p:nvSpPr><p:nvPr><p:ph type="body"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>Speaker notes</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:notes>`,
  'ppt/media/pixel.png': png,
}, 'fixture.pptx');
assert.equal(await detectOffice(pptx), 'pptx');
const slides = await extractOffice(pptx);
assert.equal(slides.extracted.status, 'ready', slides.extracted.warnings.join('\n'));
assert.ok(slides.extracted.text.indexOf('First slide') < slides.extracted.text.indexOf('Last slide'));
assert.match(slides.extracted.html, /<h2>First slide<\/h2>/);
assert.match(slides.extracted.html, /<aside><p>Speaker notes<\/p><\/aside>/);
assert.equal(slides.extracted.pages.length, 2);
assert.equal(slides.extracted.tables[0].rows[0][0], 'Slide cell');
assert.equal(slides.extracted.links[0].page, 1);
assert.equal(slides.assets.length, 1);

const xlsx = file({
  'xl/workbook.xml': `<workbook xmlns="${x}" xmlns:r="${r}"><sheets><sheet name="Second first" sheetId="2" r:id="s2"/><sheet name="First last" sheetId="1" r:id="s1"/></sheets></workbook>`,
  'xl/_rels/workbook.xml.rels': rels([['s1', 'worksheet', 'worksheets/sheet1.xml'], ['s2', 'worksheet', 'worksheets/sheet2.xml'], ['strings', 'sharedStrings', 'sharedStrings.xml']]),
  'xl/sharedStrings.xml': `<sst xmlns="${x}"><si><r><t>Shared </t></r><r><t>value</t></r></si></sst>`,
  'xl/worksheets/sheet1.xml': `<worksheet xmlns="${x}"><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>Final sheet</t></is></c></row></sheetData></worksheet>`,
  'xl/worksheets/sheet2.xml': `<worksheet xmlns="${x}" xmlns:r="${r}"><sheetData><row r="7"><c r="A7" t="s"><v>0</v></c><c r="C7"><f>2+3</f><v>5</v></c><c r="XFD7" t="b"><v>1</v></c></row></sheetData><hyperlinks><hyperlink ref="A7" r:id="link"/></hyperlinks><drawing r:id="draw"/></worksheet>`,
  'xl/worksheets/_rels/sheet2.xml.rels': rels([['link', 'hyperlink', 'https://example.org/cell', true], ['draw', 'drawing', '../drawings/drawing1.xml']]),
  'xl/drawings/drawing1.xml': `<xdr:wsDr xmlns:xdr="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing" xmlns:a="${a}" xmlns:r="${r}"><xdr:twoCellAnchor><xdr:pic><xdr:nvPicPr><xdr:cNvPr id="1" descr="Cell image"/></xdr:nvPicPr><xdr:blipFill><a:blip r:embed="image"/></xdr:blipFill></xdr:pic></xdr:twoCellAnchor></xdr:wsDr>`,
  'xl/drawings/_rels/drawing1.xml.rels': rels([['image', 'image', '../media/pixel.png']]),
  'xl/media/pixel.png': png,
}, 'fixture.xlsx');
assert.equal(await detectOffice(xlsx), 'xlsx');
const cells = await extractOffice(xlsx);
assert.equal(cells.extracted.status, 'ready', cells.extracted.warnings.join('\n'));
assert.ok(cells.extracted.html.indexOf('Second first') < cells.extracted.html.indexOf('First last'));
assert.match(cells.extracted.html, /data-cell="XFD7"/);
assert.equal(cells.extracted.tables[0].rows[0].cells[0].value, 'Shared value');
assert.deepEqual(cells.extracted.tables[0].rows[0].cells[1], { address: 'C7', value: '5', formula: '2+3' });
assert.equal(cells.extracted.tables[0].rows[0].cells[2].value, 'TRUE');
assert.equal(cells.assets.length, 1);
assert.equal(cells.extracted.links[0].url, 'https://example.org/cell');

const bad = await extractOffice(file({
  'word/document.xml': document('<w:p><w:hyperlink r:id="bad"><w:r><w:t>Unsafe link</w:t></w:r></w:hyperlink><w:r><w:drawing><a:blip r:embed="missing"/></w:drawing></w:r></w:p>'),
  'word/_rels/document.xml.rels': rels([['bad', 'hyperlink', 'javascript:alert(1)', true], ['missing', 'image', 'media/missing.png']]),
}, 'unsafe.docx'));
assert.equal(bad.extracted.status, 'partial');
assert.equal(bad.extracted.links.length, 0);
assert.doesNotMatch(bad.extracted.html, /javascript:|data-image-id/);
assert.ok(bad.extracted.warnings.some(w => w.includes('missing image')));

const brokenXml = await extractOffice(file({ 'word/document.xml': '<w:document>' }, 'broken.docx'));
assert.equal(brokenXml.extracted.status, 'partial');
assert.ok(brokenXml.extracted.warnings.some(w => w.includes('Malformed XML')));
const entity = await extractOffice(file({ 'word/document.xml': '<!DOCTYPE doc [<!ENTITY steal SYSTEM="file:///etc/passwd">]><doc>&steal;</doc>' }, 'entity.docx'));
assert.equal(entity.extracted.status, 'partial');
assert.equal(entity.extracted.text, '');
const utf16 = Uint8Array.from(Buffer.from('\ufeff' + document(paragraph('Unicode UTF-16')), 'utf16le'));
assert.equal((await extractOffice(file({ 'word/document.xml': utf16 }, 'utf16.docx'))).extracted.text, 'Unicode UTF-16');
const unsafeSvg = await extractOffice(file({
  'word/document.xml': document('<w:p><w:r><w:drawing><a:blip r:embed="svg"/></w:drawing></w:r></w:p>'),
  'word/_rels/document.xml.rels': rels([['svg', 'image', 'media/unsafe.svg']]),
  'word/media/unsafe.svg': '<svg xmlns="http://www.w3.org/2000/svg"><animate attributeName="href" values="javascript:alert(1)"/></svg>',
}, 'svg.docx'));
assert.equal(unsafeSvg.extracted.status, 'partial');
assert.equal(unsafeSvg.assets.length, 0);
assert.doesNotMatch(unsafeSvg.extracted.html, /data-image-id/);

const odf = await extractOffice(file({ 'mimetype': 'application/vnd.oasis.opendocument.text', 'content.xml': '<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><text:p>Available ODF text</text:p></office:body></office:document-content>' }, 'fixture.odt'));
assert.equal(odf.extracted.status, 'partial');
assert.match(odf.extracted.text, /Available ODF text/);
const iwork = await extractOffice(file({ 'Index/Document.iwa': new Uint8Array([1, 2, 3]), 'QuickLook/Thumbnail.png': png }, 'fixture.pages'));
assert.equal(iwork.extracted.status, 'partial');
assert.equal(iwork.assets.length, 1);
assert.equal(iwork.extracted.text, '');
assert.match(iwork.extracted.warnings.join(), /not decoded/);
assert.equal((await extractOffice(new File(['not a ZIP'], 'legacy.doc'))).extracted.status, 'failed');

const aborter = new AbortController(); aborter.abort();
await assert.rejects(extractOffice(docx, aborter.signal), { name: 'AbortError' });
await assert.rejects(detectOffice(docx, aborter.signal), { name: 'AbortError' });

console.log('Office extraction fixtures passed: DOCX/PPTX/XLSX structure, links, real images, selective streaming, asset sink, partial formats, invalid XML, and cancellation.');
