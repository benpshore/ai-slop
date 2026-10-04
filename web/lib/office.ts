import { Unzip, UnzipInflate, type AsyncFlateStreamHandler, type UnzipDecoder } from 'fflate';
import type { Extracted, LinkEvidence } from './types';

export type OfficeKind = 'docx' | 'pptx' | 'xlsx' | 'odf' | 'iwork';
export type OfficeAsset = { id: string; file: File };
export type OfficeResult = { extracted: Extracted; assets: OfficeAsset[] };
type Relationship = { target: string; type: string; external: boolean };
type Package = { names: Set<string>; xml: Map<string, string>; warnings: Set<string> };
const empty = new Uint8Array(0);
const escape = (s: string) => s.replace(/[&<>"']/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]!);
const local = (n: Node) => n.nodeType === 1 ? (n as Element).localName : '';
// Cast after the nodeType check: Cloudflare's server Element declaration conflicts
// with DOM ChildNode's before/after methods in the shared Next.js type environment.
const children = (n: Node, name?: string): Element[] => Array.from(n.childNodes).filter(x => x.nodeType === 1 && (!name || local(x) === name)) as unknown as Element[];
const all = (n: Element | Document, name: string) => Array.from(n.getElementsByTagNameNS('*', name));
const first = (n: Element | Document, name: string) => all(n, name)[0];
const attr = (n: Element | undefined, name: string): string => n ? Array.from(n.attributes).find(a => a.localName === name)?.value || '' : '';
const direct = (n: Node, name: string) => children(n, name)[0];

function abort(signal?: AbortSignal) { if (signal?.aborted) throw signal.reason || new DOMException('Import cancelled', 'AbortError'); }
function path(name: string): string | null {
  if (!name || /[\u0000-\u001f\\]/.test(name) || name.startsWith('/') || /^[a-z]:/i.test(name)) return null;
  const out: string[] = [];
  for (const part of name.split('/')) {
    if (!part || part === '.') continue;
    if (part === '..') { if (!out.length) return null; out.pop(); }
    else out.push(part);
  }
  return out.join('/');
}
function relative(base: string, target: string): string | null {
  let decoded: string;
  try { decoded = decodeURIComponent(target); } catch { return null; }
  if (/^[a-z][a-z\d+.-]*:/i.test(decoded) || /[?#]/.test(decoded)) return null;
  return path(decoded.startsWith('/') ? decoded.slice(1) : `${base.slice(0, base.lastIndexOf('/') + 1)}${decoded}`);
}
function xml(text: string, name: string, warnings: Set<string>): Document | null {
  if (/<!DOCTYPE|<!ENTITY/i.test(text)) { warnings.add(`Unsafe XML declarations rejected: ${name}`); return null; }
  const doc = new DOMParser().parseFromString(text, 'application/xml');
  if (all(doc, 'parsererror').length || doc.documentElement.localName === 'parsererror') {
    warnings.add(`Malformed XML: ${name}`); return null;
  }
  return doc;
}

/** Start every ZIP member: fflate otherwise retains compressed bytes of skipped entries. */
async function readZip(file: File, select: (name: string) => boolean, receive: (name: string, chunks: Uint8Array[], size: number) => void | Promise<void>, warnings: Set<string>, signal?: AbortSignal, onName?: (name: string) => void) {
  let error: unknown;
  let pending: Promise<void>[] = [];
  const selected = new Set<string>();
  const names = new Set<string>();
  const active = new Set<string>();
  const decoder = (compression: number) => class implements UnzipDecoder {
    static compression = compression;
    ondata: AsyncFlateStreamHandler = () => {};
    private inflate?: UnzipInflate;
    private keep: boolean;
    constructor(name: string) {
      this.keep = selected.has(name);
      if (this.keep && compression === 8) {
        this.inflate = new UnzipInflate();
        this.inflate.ondata = (err, data, final) => this.ondata(err, data, final);
      }
    }
    push(chunk: Uint8Array<ArrayBuffer>, final: boolean) {
      if (this.inflate) this.inflate.push(chunk, final);
      else this.ondata(null, this.keep && compression === 0 ? chunk : empty, final);
    }
  };
  const unzip = new Unzip(entry => {
    const name = path(entry.name);
    if (!name || name !== entry.name.replace(/\/$/, '')) warnings.add(`Ignored unsafe ZIP member path: ${entry.name}`);
    const duplicate = name !== null && names.has(name);
    if (duplicate) warnings.add(`Duplicate ZIP member ignored: ${name}`);
    if (name) { names.add(name); onName?.(name); }
    const keep = !!name && name === entry.name && !duplicate && select(name);
    if (keep && (entry.compression === 0 || entry.compression === 8)) selected.add(entry.name);
    else if (keep) warnings.add(`Unsupported ZIP compression ${entry.compression}: ${name}`);
    unzip.register(decoder(entry.compression));
    const chunks: Uint8Array[] = [];
    let size = 0;
    active.add(entry.name);
    entry.ondata = (err, data, final) => {
      if (err) { error = err; return; }
      if (selected.has(entry.name) && data.length) { chunks.push(data.slice()); size += data.length; }
      if (final) {
        active.delete(entry.name);
        if (selected.delete(entry.name)) {
          if (entry.originalSize !== undefined && size !== entry.originalSize) warnings.add(`Truncated ZIP member: ${name}`);
          pending.push(Promise.resolve(receive(name!, chunks, size)));
        }
      }
    };
    entry.start();
  });
  const reader = file.stream().getReader();
  try {
    for (;;) {
      abort(signal);
      const { value, done } = await reader.read();
      if (done) break;
      unzip.push(value, false);
      await Promise.all(pending); pending = [];
      if (error) throw error;
    }
    unzip.push(empty, true);
    await Promise.all(pending);
    if (error) throw error;
    if (active.size) throw new Error('Truncated ZIP archive');
  } finally { await reader.cancel().catch(() => {}); reader.releaseLock(); }
}

function kindFromNames(names: Set<string>): OfficeKind | null {
  if (names.has('word/document.xml')) return 'docx';
  if (names.has('ppt/presentation.xml')) return 'pptx';
  if (names.has('xl/workbook.xml')) return 'xlsx';
  if (names.has('content.xml') && (names.has('META-INF/manifest.xml') || names.has('mimetype'))) return 'odf';
  if (Array.from(names).some(n => /^Index\/.*\.iwa$/i.test(n)) || names.has('index.xml') || names.has('index.apxl')) return 'iwork';
  return null;
}

/** Read ZIP's directory, not the potentially enormous compressed members. */
export async function detectOffice(file: File, signal?: AbortSignal): Promise<OfficeKind | null> {
  const names = new Set<string>();
  let reader: ReadableStreamDefaultReader<Uint8Array> | undefined;
  try {
    abort(signal);
    // EOCD's comment is at most 65535 bytes by the ZIP format definition.
    const tailStart = Math.max(0, file.size - 65557);
    const tail = new DataView(await file.slice(tailStart).arrayBuffer());
    let eocd = -1;
    for (let i = tail.byteLength - 22; i >= 0; i--) if (tail.getUint32(i, true) === 0x06054b50 && i + 22 + tail.getUint16(i + 20, true) === tail.byteLength) { eocd = i; break; }
    if (eocd < 0 || tail.getUint16(eocd + 4, true) !== 0 || tail.getUint16(eocd + 6, true) !== 0) return null;
    let length = tail.getUint32(eocd + 12, true);
    let offset = tail.getUint32(eocd + 16, true);
    if (length === 0xffffffff || offset === 0xffffffff || tail.getUint16(eocd + 10, true) === 0xffff) {
      const position = tailStart + eocd - 20;
      if (position < 0) return null;
      const locator = new DataView(await file.slice(position, position + 20).arrayBuffer());
      if (locator.getUint32(0, true) !== 0x07064b50 || locator.getUint32(4, true) !== 0 || locator.getUint32(16, true) !== 1) return null;
      const zip64Offset = Number(locator.getBigUint64(8, true));
      if (!Number.isSafeInteger(zip64Offset) || zip64Offset + 56 > position) return null;
      const record = new DataView(await file.slice(zip64Offset, zip64Offset + 56).arrayBuffer());
      if (record.getUint32(0, true) !== 0x06064b50 || record.getUint32(16, true) !== 0 || record.getUint32(20, true) !== 0) return null;
      length = Number(record.getBigUint64(40, true)); offset = Number(record.getBigUint64(48, true));
    }
    if (!Number.isSafeInteger(length) || !Number.isSafeInteger(offset) || offset < 0 || length < 0 || offset + length > tailStart + eocd) return null;
    reader = file.slice(offset, offset + length).stream().getReader();
    let remaining = empty as Uint8Array;
    for (;;) {
      abort(signal);
      const { value, done } = await reader.read();
      if (done) break;
      const joined = new Uint8Array(remaining.length + value.length);
      joined.set(remaining); joined.set(value, remaining.length);
      let consumed = 0;
      while (joined.length - consumed >= 46) {
        const header = new DataView(joined.buffer, consumed);
        if (header.getUint32(0, true) !== 0x02014b50) return null;
        const nameSize = header.getUint16(28, true);
        const recordSize = 46 + nameSize + header.getUint16(30, true) + header.getUint16(32, true);
        if (joined.length - consumed < recordSize) break;
        const name = path(new TextDecoder().decode(joined.subarray(consumed + 46, consumed + 46 + nameSize)));
        if (name) names.add(name);
        consumed += recordSize;
        const kind = kindFromNames(names);
        if (kind) return kind;
      }
      remaining = joined.slice(consumed);
    }
    return kindFromNames(names);
  } catch (e) { abort(signal); if (e instanceof DOMException && e.name === 'AbortError') throw e; return null; }
  finally { if (reader) { await reader.cancel().catch(() => {}); reader.releaseLock(); } }
}

function relationships(pkg: Package, part: string): Map<string, Relationship> {
  const slash = part.lastIndexOf('/');
  const name = `${part.slice(0, slash + 1)}_rels/${part.slice(slash + 1)}.rels`;
  const text = pkg.xml.get(name);
  const result = new Map<string, Relationship>();
  const doc = text === undefined ? null : xml(text, name, pkg.warnings);
  if (!doc) return result;
  for (const r of all(doc, 'Relationship')) {
    const external = attr(r, 'TargetMode') === 'External';
    const raw = attr(r, 'Target');
    const target = external ? raw : relative(part, raw);
    if (!target) { pkg.warnings.add(`Invalid relationship target in ${name}`); continue; }
    const id = attr(r, 'Id');
    if (result.has(id)) pkg.warnings.add(`Duplicate relationship ${id} in ${name}`);
    else result.set(id, { target, type: attr(r, 'Type'), external });
  }
  return result;
}
function documentPart(pkg: Package, name: string): Document | null {
  const text = pkg.xml.get(name);
  if (text === undefined) { pkg.warnings.add(`Missing document part: ${name}`); return null; }
  return xml(text, name, pkg.warnings);
}
function safeLink(target: string): string | null {
  if (target.startsWith('#')) return target;
  try { const u = new URL(target); return ['https:', 'http:', 'mailto:'].includes(u.protocol) && !u.username && !u.password ? u.href : null; } catch { return null; }
}
const mime = (name: string) => ({ png: 'image/png', jpg: 'image/jpeg', jpeg: 'image/jpeg', gif: 'image/gif', webp: 'image/webp', bmp: 'image/bmp', tif: 'image/tiff', tiff: 'image/tiff', svg: 'image/svg+xml' })[name.split('.').pop()!.toLowerCase() as 'png'];

class Render {
  links: LinkEvidence[] = [];
  images = new Map<string, string>();
  tables: unknown[] = [];
  pages: unknown[] = [];
  text: string[] = [];
  constructor(readonly pkg: Package) {}
  link(label: string, target: string, page?: number): string {
    const url = safeLink(target);
    if (!url) { this.pkg.warnings.add('An unsafe or unsupported hyperlink target was omitted'); return label; }
    this.links.push({ url, label: label.replace(/<[^>]*>/g, ''), ...(page ? { page } : {}), kind: 'office-hyperlink' });
    return `<a href="${escape(url)}">${label}</a>`;
  }
  image(part: string, rels: Map<string, Relationship>, id: string, alt = ''): string {
    const rel = rels.get(id);
    if (!rel || rel.external || !rel.type.endsWith('/image')) {
      this.pkg.warnings.add(`Unavailable or external image in ${part}`); return alt ? `<span>${escape(alt)}</span>` : '';
    }
    return this.imagePath(rel.target, alt);
  }
  imagePath(name: string, alt = ''): string {
    if (!this.pkg.names.has(name) || !mime(name)) {
      this.pkg.warnings.add(`Unsupported or missing image: ${name}`); return alt ? `<span>${escape(alt)}</span>` : '';
    }
    let id = this.images.get(name);
    if (!id) { id = `office-image-${this.images.size + 1}`; this.images.set(name, id); }
    return `<img data-image-id="${id}" alt="${escape(alt)}">`;
  }
}

function word(pkg: Package, out: Render): string {
  const doc = documentPart(pkg, 'word/document.xml');
  if (!doc) return '';
  const styles = new Map<string, number>();
  const styleText = pkg.xml.get('word/styles.xml');
  const styleDoc = styleText ? xml(styleText, 'word/styles.xml', pkg.warnings) : null;
  for (const s of styleDoc ? all(styleDoc, 'style') : []) {
    const name = attr(first(s, 'name'), 'val') || attr(s, 'styleId');
    const level = attr(first(s, 'outlineLvl'), 'val');
    const matched = /heading\s*([1-6])/i.exec(name);
    if (matched) styles.set(attr(s, 'styleId'), Number(matched[1]));
    else if (/^[0-5]$/.test(level)) styles.set(attr(s, 'styleId'), Number(level) + 1);
  }
  const renderPart = (document: Document, part: string, root: Element): string => {
    const rels = relationships(pkg, part);
    const plain = (n: Element): string => {
      if (['del', 'moveFrom', 'instrText', 'delText', 'rPr', 'pPr'].includes(local(n))) return '';
      if (local(n) === 't') return n.textContent || '';
      if (local(n) === 'tab') return '\t';
      if (['br', 'cr'].includes(local(n))) return '\n';
      if (local(n) === 'noBreakHyphen') return '‑';
      if (local(n) === 'softHyphen') return '\u00ad';
      return children(n).map(plain).join('');
    };
    const inline = (n: Element): string => {
      switch (local(n)) {
        case 't': return escape(n.textContent || '');
        case 'tab': return '\t';
        case 'br': case 'cr': return '<br>';
        case 'noBreakHyphen': return '‑';
        case 'softHyphen': return '\u00ad';
        case 'del': case 'moveFrom': case 'instrText': case 'delText': return '';
        case 'drawing': case 'pict': {
          const alt = attr(first(n, 'docPr'), 'descr') || attr(first(n, 'docPr'), 'title');
          const found = [...all(n, 'blip').map(x => attr(x, 'embed') || attr(x, 'link')), ...all(n, 'imagedata').map(x => attr(x, 'id'))];
          if (!found.length) pkg.warnings.add(`Drawing without a supported embedded image in ${part}`);
          const content = found.map(id => out.image(part, rels, id, alt)).join('');
          const click = first(n, 'hlinkClick');
          const link = click && rels.get(attr(click, 'id'));
          return link?.external ? out.link(content, link.target) : content;
        }
        case 'hyperlink': {
          const content = children(n).map(inline).join('');
          const rel = rels.get(attr(n, 'id'));
          const bookmark = attr(n, 'anchor');
          if (rel?.external) return out.link(content, rel.target);
          if (bookmark) return out.link(content, `#${bookmark}`);
          pkg.warnings.add(`Missing hyperlink relationship in ${part}`); return content;
        }
        case 'r': {
          let value = children(n).filter(x => local(x) !== 'rPr').map(inline).join('');
          const props = direct(n, 'rPr');
          const enabled = (name: string) => { const p = props && direct(props, name); return p && !['0', 'false', 'off'].includes(attr(p, 'val')); };
          if (enabled('b')) value = `<strong>${value}</strong>`;
          if (enabled('i')) value = `<em>${value}</em>`;
          return value;
        }
        case 'footnoteReference': case 'endnoteReference': return `<sup>${escape(attr(n, 'id'))}</sup>`;
        case 'AlternateContent': return children(n).filter(x => local(x) === 'Choice').slice(0, 1).map(inline).join('') || children(n, 'Fallback').map(inline).join('');
        case 'rPr': case 'pPr': return '';
        default: return children(n).map(inline).join('');
      }
    };
    const block = (n: Element): string => {
      if (local(n) === 'p') {
        const content = children(n).map(inline).join('');
        const props = direct(n, 'pPr');
        const style = props ? attr(direct(props, 'pStyle'), 'val') : '';
        const heading = styles.get(style) || Number(/^Heading([1-6])$/i.exec(style)?.[1] || 0);
        const tag = heading ? `h${heading}` : 'p';
        const text = plain(n);
        out.text.push(text);
        const numbered = props && direct(props, 'numPr');
        if (numbered) pkg.warnings.add('Word list numbering is retained as paragraph text; automatic list labels are not reconstructed');
        return `<${tag}>${content}</${tag}>`;
      }
      if (local(n) === 'tbl') {
        const rows: string[][] = [];
        const html = children(n, 'tr').map(row => {
          const cells: string[] = [];
          const html = children(row, 'tc').map(cell => {
            cells.push(all(cell, 't').map(t => t.textContent || '').join(''));
            const span = attr(first(cell, 'gridSpan'), 'val');
            const merge = first(cell, 'vMerge');
            if (merge) pkg.warnings.add('Word vertically merged table cells are retained as separate source cells');
            return `<td${/^[1-9]\d*$/.test(span) ? ` colspan="${span}"` : ''}>${children(cell).map(block).join('')}</td>`;
          }).join('');
          rows.push(cells); return `<tr>${html}</tr>`;
        }).join('');
        out.tables.push({ part, rows }); return `<table>${html}</table>`;
      }
      if (['altChunk', 'object'].includes(local(n))) pkg.warnings.add(`Embedded content is not decoded: ${local(n)} in ${part}`);
      if (['sectPr', 'tcPr', 'tblPr', 'tblGrid', 'trPr'].includes(local(n))) return '';
      return children(n).map(block).join('');
    };
    return children(root).map(block).join('');
  };
  const body = first(doc, 'body');
  if (!body) { pkg.warnings.add('Word document has no body'); return ''; }
  let html = renderPart(doc, 'word/document.xml', body);
  const rels = relationships(pkg, 'word/document.xml');
  const included = new Set<string>();
  for (const rel of rels.values()) {
    const type = rel.type.split('/').pop()!;
    if (!rel.external && ['header', 'footer', 'footnotes', 'endnotes'].includes(type) && !included.has(rel.target)) {
      included.add(rel.target);
      const extra = documentPart(pkg, rel.target);
      if (extra) html += `<section data-office-part="${escape(type)}"><h2>${escape(type)}</h2>${renderPart(extra, rel.target, extra.documentElement)}</section>`;
    }
  }
  return html;
}

function drawingParagraph(p: Element, rels: Map<string, Relationship>, out: Render, page?: number): string {
  const render = (n: Element): string => {
    if (local(n) === 't') return escape(n.textContent || '');
    if (local(n) === 'br') return '<br>';
    if (local(n) === 'rPr' || local(n) === 'pPr' || local(n) === 'endParaRPr') return '';
    let value = children(n).map(render).join('');
    const props = direct(n, 'rPr');
    if (props) {
      if (attr(props, 'b') === '1') value = `<strong>${value}</strong>`;
      if (attr(props, 'i') === '1') value = `<em>${value}</em>`;
      const click = first(props, 'hlinkClick');
      if (click) {
        const rel = rels.get(attr(click, 'id'));
        if (rel?.external) value = out.link(value, rel.target, page);
        else out.pkg.warnings.add('An internal presentation hyperlink is not projected');
      }
    }
    return value;
  };
  out.text.push(all(p, 't').map(t => t.textContent || '').join(''));
  return children(p).map(render).join('');
}

function presentation(pkg: Package, out: Render): string {
  const doc = documentPart(pkg, 'ppt/presentation.xml');
  if (!doc) return '';
  const rels = relationships(pkg, 'ppt/presentation.xml');
  const ids = all(doc, 'sldId');
  if (!ids.length) pkg.warnings.add('Presentation contains no slide references');
  return ids.map((s, index) => {
    // sldId has both an unqualified numeric id and a relationship r:id.
    const id = Array.from(s.attributes).find(a => a.localName === 'id' && a.namespaceURI)?.value || '';
    const rel = rels.get(id);
    if (!rel || rel.external) { pkg.warnings.add(`Missing slide relationship: ${id}`); return ''; }
    const slide = documentPart(pkg, rel.target);
    if (!slide) return '';
    const slideRels = relationships(pkg, rel.target);
    const page = index + 1;
    const render = (node: Element): string => {
      if (local(node) === 'sp') {
        const placeholder = attr(first(node, 'ph'), 'type');
        const tag = ['title', 'ctrTitle'].includes(placeholder) ? 'h2' : 'p';
        return all(node, 'p').map(p => `<${tag}>${drawingParagraph(p, slideRels, out, page)}</${tag}>`).join('');
      }
      if (local(node) === 'pic') {
        const blip = first(node, 'blip');
        const content = out.image(rel.target, slideRels, attr(blip, 'embed') || attr(blip, 'link'), attr(first(node, 'cNvPr'), 'descr'));
        const click = first(node, 'hlinkClick');
        const link = click && slideRels.get(attr(click, 'id'));
        return link?.external ? out.link(content, link.target, page) : content;
      }
      if (local(node) === 'tbl') {
        const rows: string[][] = [];
        const html = children(node, 'tr').map(row => {
          const cells: string[] = [];
          const html = children(row, 'tc').map(cell => {
            cells.push(all(cell, 't').map(t => t.textContent || '').join(''));
            return `<td>${all(cell, 'p').map(p => `<p>${drawingParagraph(p, slideRels, out, page)}</p>`).join('')}</td>`;
          }).join('');
          rows.push(cells); return `<tr>${html}</tr>`;
        }).join('');
        out.tables.push({ page, rows }); return `<table>${html}</table>`;
      }
      if (['chart', 'oleObj', 'videoFile', 'audioFile', 'relIds'].includes(local(node))) pkg.warnings.add(`Presentation ${local(node) === 'relIds' ? 'SmartArt' : local(node)} is not decoded on slide ${page}`);
      return children(node).map(render).join('');
    };
    const tree = first(slide, 'spTree');
    if (!tree) pkg.warnings.add(`Slide ${page} has no shape tree`);
    let content = tree ? render(tree) : '';
    const notesRel = Array.from(slideRels.values()).find(r => !r.external && r.type.endsWith('/notesSlide'));
    if (notesRel) {
      const notes = documentPart(pkg, notesRel.target);
      if (notes) {
        const notesRels = relationships(pkg, notesRel.target);
        const bodyShapes = all(notes, 'sp').filter(n => attr(first(n, 'ph'), 'type') === 'body');
        content += `<aside>${bodyShapes.flatMap(n => all(n, 'p')).map(p => `<p>${drawingParagraph(p, notesRels, out, page)}</p>`).join('')}</aside>`;
      }
    }
    out.pages.push({ page, part: rel.target });
    return `<section data-page="${page}">${content}</section>`;
  }).join('');
}

function spreadsheet(pkg: Package, out: Render): string {
  const doc = documentPart(pkg, 'xl/workbook.xml');
  if (!doc) return '';
  const workbookRels = relationships(pkg, 'xl/workbook.xml');
  const stringRel = Array.from(workbookRels.values()).find(r => r.type.endsWith('/sharedStrings') && !r.external);
  const stringsText = pkg.xml.get(stringRel?.target || 'xl/sharedStrings.xml');
  const stringsDoc = stringsText ? xml(stringsText, stringRel?.target || 'xl/sharedStrings.xml', pkg.warnings) : null;
  const strings = stringsDoc ? all(stringsDoc, 'si').map(si => children(si).filter(x => ['t', 'r'].includes(local(x))).map(x => local(x) === 't' ? x.textContent || '' : all(x, 't').map(t => t.textContent || '').join('')).join('')) : [];
  return all(doc, 'sheet').map(sheet => {
    const id = Array.from(sheet.attributes).find(a => a.localName === 'id' && a.namespaceURI)?.value || '';
    const rel = workbookRels.get(id);
    const title = attr(sheet, 'name');
    if (!rel || rel.external) { pkg.warnings.add(`Missing worksheet relationship: ${title}`); return ''; }
    const sheetDoc = documentPart(pkg, rel.target);
    if (!sheetDoc) return '';
    if (!rel.type.endsWith('/worksheet')) { pkg.warnings.add(`Non-worksheet sheet is not decoded: ${title}`); return ''; }
    const sheetRels = relationships(pkg, rel.target);
    out.text.push(title);
    const hyperlink = new Map<string, string>();
    for (const h of all(sheetDoc, 'hyperlink')) {
      const r = sheetRels.get(attr(h, 'id'));
      const target = r?.external ? r.target : attr(h, 'location') ? `#${attr(h, 'location')}` : '';
      if (target) hyperlink.set(attr(h, 'ref'), target);
      else pkg.warnings.add(`Missing spreadsheet hyperlink: ${attr(h, 'ref')}`);
    }
    const rows: { row: string; cells: { address: string; value: string; formula?: string; style?: string }[] }[] = [];
    const data = first(sheetDoc, 'sheetData');
    if (!data) pkg.warnings.add(`Worksheet has no cell data: ${title}`);
    const html = (data ? children(data, 'row') : []).map(row => {
      const cells = children(row, 'c').map(cell => {
        const type = attr(cell, 't');
        const address = attr(cell, 'r');
        const raw = direct(cell, 'v')?.textContent || '';
        let value = raw;
        if (type === 's') {
          if (!/^\d+$/.test(raw) || strings[Number(raw)] === undefined) { value = ''; pkg.warnings.add(`Missing shared string at ${title}!${address}`); }
          else value = strings[Number(raw)];
        } else if (type === 'inlineStr') value = all(cell, 't').map(t => t.textContent || '').join('');
        else if (type === 'b') value = raw === '1' ? 'TRUE' : raw === '0' ? 'FALSE' : raw;
        const formula = direct(cell, 'f');
        if (formula && !direct(cell, 'v')) pkg.warnings.add(`Formula has no cached value at ${title}!${address}`);
        return { address, value, ...(formula ? { formula: formula.textContent || '' } : {}), ...(attr(cell, 's') ? { style: attr(cell, 's') } : {}) };
      });
      rows.push({ row: attr(row, 'r'), cells });
      out.text.push(cells.map(c => `${c.address}: ${c.value}`).join('\t'));
      return `<tr data-row="${escape(attr(row, 'r'))}">${cells.map(c => `<td data-cell="${escape(c.address)}"><span class="cell-address">${escape(c.address)}</span> ${hyperlink.has(c.address) ? out.link(escape(c.value), hyperlink.get(c.address)!) : escape(c.value)}</td>`).join('')}</tr>`;
    }).join('');
    const representedLinks = new Set(rows.flatMap(row => row.cells.map(cell => cell.address)));
    let additionalLinks = '';
    for (const [reference, target] of hyperlink) if (!representedLinks.has(reference)) additionalLinks += `<p>${out.link(escape(reference), target)}</p>`;
    const merged = all(sheetDoc, 'mergeCell').map(c => attr(c, 'ref'));
    out.tables.push({ sheet: title, rows, merged, valueRepresentation: 'stored-values-and-cached-formulas' });
    let images = '';
    for (const drawing of all(sheetDoc, 'drawing')) {
      const drawingRel = sheetRels.get(attr(drawing, 'id'));
      if (!drawingRel || drawingRel.external) { pkg.warnings.add(`Missing drawing in worksheet: ${title}`); continue; }
      const drawingDoc = documentPart(pkg, drawingRel.target);
      if (!drawingDoc) continue;
      const drawingRels = relationships(pkg, drawingRel.target);
      for (const pic of all(drawingDoc, 'pic')) images += out.image(drawingRel.target, drawingRels, attr(first(pic, 'blip'), 'embed'), attr(first(pic, 'cNvPr'), 'descr'));
      if (all(drawingDoc, 'chart').length) pkg.warnings.add(`Spreadsheet chart is not rendered: ${title}`);
    }
    return `<section><h2>${escape(title)}</h2><table>${html}</table>${additionalLinks}${images}</section>`;
  }).join('');
}

function partialPackage(pkg: Package, out: Render, kind: OfficeKind): string {
  pkg.warnings.add(kind === 'iwork' ? 'Apple iWork binary document structure is not decoded; only available XML text and image previews are extracted' : 'OpenDocument support is partial; available XML text, tables and images are extracted without full layout or formula interpretation');
  let html = '';
  const name = kind === 'odf' ? 'content.xml' : pkg.xml.has('index.xml') ? 'index.xml' : 'index.apxl';
  const source = pkg.xml.get(name);
  if (source) {
    const doc = xml(source, name, pkg.warnings);
    if (doc) {
      const render = (n: Element): string => {
        if (['p', 'h', 'string'].includes(local(n))) {
          const text = n.textContent || ''; out.text.push(text);
          return `<${local(n) === 'h' ? 'h2' : 'p'}>${escape(text)}</${local(n) === 'h' ? 'h2' : 'p'}>`;
        }
        if (local(n) === 'image') {
          const target = relative(name, attr(n, 'href'));
          if (target) return out.imagePath(target);
        }
        return children(n).map(render).join('');
      };
      html += render(doc.documentElement);
    }
  }
  for (const n of pkg.names) if (/^(QuickLook|preview)\//i.test(n) && mime(n)) html += out.imagePath(n, 'Document preview');
  return html;
}

/** Local browser extraction only. An optional asset sink releases each image after storage. */
export async function extractOffice(file: File, signal?: AbortSignal, onAsset?: (asset: OfficeAsset) => Promise<void>): Promise<OfficeResult> {
  const pkg: Package = { names: new Set(), xml: new Map(), warnings: new Set() };
  const assets: OfficeAsset[] = [];
  try {
    await readZip(file, n => /(?:\.xml|\.rels|\.apxl)$/.test(n) && !/^(customXml|_xmlsignatures)\//.test(n), async (name, chunks) => {
      const prefix: number[] = [];
      for (const chunk of chunks) { prefix.push(...chunk.subarray(0, 4 - prefix.length)); if (prefix.length === 4) break; }
      const encoding = (prefix[0] === 255 && prefix[1] === 254) || (prefix[0] === 60 && prefix[1] === 0) ? 'utf-16le' : (prefix[0] === 254 && prefix[1] === 255) || (prefix[0] === 0 && prefix[1] === 60) ? 'utf-16be' : 'utf-8';
      const decoder = new TextDecoder(encoding, { fatal: true });
      let text = '';
      try { for (const chunk of chunks) text += decoder.decode(chunk, { stream: true }); text += decoder.decode(); }
      catch { pkg.warnings.add(`Invalid Unicode XML: ${name}`); return; }
      pkg.xml.set(name, text);
    }, pkg.warnings, signal, n => pkg.names.add(n));
  } catch (e) {
    abort(signal);
    pkg.warnings.add(`Archive could not be fully read: ${e instanceof Error ? e.message : 'ZIP error'}`);
  }
  const kind = kindFromNames(pkg.names);
  if (!kind) return { extracted: { title: file.name, text: '', links: [], warnings: [...pkg.warnings, 'Not a supported Office ZIP package (legacy or encrypted Office files require another reader)'], engine: 'browser-office', status: 'failed' }, assets };
  abort(signal);
  const out = new Render(pkg);
  let html = kind === 'docx' ? word(pkg, out) : kind === 'pptx' ? presentation(pkg, out) : kind === 'xlsx' ? spreadsheet(pkg, out) : partialPackage(pkg, out, kind);
  const loaded = new Set<string>();
  if (out.images.size) {
    try {
      await readZip(file, n => out.images.has(n), async (name, chunks) => {
        let type = mime(name)!;
        if (type === 'image/svg+xml') {
          // Keep SVG local and inert: disallow active/external content instead of forwarding arbitrary XML.
          const text = await new Blob(chunks as BlobPart[]).text();
          const svg = xml(text, name, pkg.warnings);
          if (!svg || svg.documentElement.localName !== 'svg' || /<\?(?!xml\s)/i.test(text) || ['script', 'foreignObject', 'style', 'animate', 'animateMotion', 'animateTransform', 'set'].some(tag => all(svg, tag).length) || Array.from(svg.getElementsByTagName('*')).some(n => Array.from(n.attributes).some(a => /^on/i.test(a.name) || (['href', 'src'].includes(a.localName) && !a.value.startsWith('#')) || /url\s*\(/i.test(a.value)))) {
            pkg.warnings.add(`Unsafe SVG image omitted: ${name}`); return;
          }
          type = 'image/svg+xml';
        }
        const id = out.images.get(name)!;
        const asset = { id, file: new File(chunks as BlobPart[], name.split('/').pop()!, { type }) };
        if (onAsset) await onAsset(asset); else assets.push(asset);
        loaded.add(id);
      }, pkg.warnings, signal);
    } catch (e) { abort(signal); pkg.warnings.add(`Image extraction failed: ${e instanceof Error ? e.message : 'ZIP error'}`); }
    for (const [name, id] of out.images) if (!loaded.has(id)) {
      pkg.warnings.add(`Image could not be extracted: ${name}`);
      html = html.replace(new RegExp(`<img data-image-id="${id}"[^>]*>`, 'g'), '');
    }
  }
  const core = pkg.xml.get('docProps/core.xml');
  const metadata = core ? xml(core, 'docProps/core.xml', pkg.warnings) : null;
  const title = metadata ? first(metadata, 'title')?.textContent?.trim() || file.name : file.name;
  const text = out.text.join('\n');
  const warnings = [...pkg.warnings];
  return { extracted: {
    title, text, html, links: out.links, warnings, engine: 'browser-office',
    status: warnings.length ? 'partial' : 'ready',
    metadata: { format: kind, sourceName: file.name, bytes: file.size, localExtraction: true, ...(kind === 'xlsx' ? { cellValues: 'Stored values and cached formula results; original styles, calculation, and chart rendering are not reproduced' } : {}), ...(metadata ? { creator: first(metadata, 'creator')?.textContent || '', modified: first(metadata, 'modified')?.textContent || '' } : {}) },
    ...(out.pages.length ? { pages: out.pages } : {}), ...(out.tables.length ? { tables: out.tables } : {}),
  }, assets };
}
