/** Streaming archive imports. Originals are never changed; no document-size or item-count caps. */
export type ImportProgress = { path: string; phase: string; completed: number; total: number };
export type ImportItem =
  | { file: File; path: string; sourceArchive?: string; dispose?: () => Promise<void> }
  | { path: string; error: string; sourceArchive?: string };
type Progress = (event: ImportProgress) => void;
const utf8 = new TextDecoder('utf-8', { fatal: true });
const BLOCK = 64 * 1024;

export function isArchive(file: File | string): boolean {
  return /\.(?:zip|tar|tgz|gz|gzip)$/i.test(typeof file === 'string' ? file : file.name);
}
function aborted(signal?: AbortSignal) { signal?.throwIfAborted(); }
function failure(error: unknown): string {
  if (error instanceof DOMException && error.name === 'QuotaExceededError') return 'The browser ran out of local storage while expanding this archive. The original remains saved.';
  return (error instanceof Error ? error.message : String(error)) || 'Archive expansion failed: the data may be corrupt or unsupported by this browser. The original remains saved.';
}
export function safeArchivePath(raw: string): string {
  if (!raw || /[\x00-\x1f\x7f]/.test(raw) || /^[\\/]/.test(raw) || /^[a-z]:/i.test(raw)) throw new Error(`Unsafe archive path: ${JSON.stringify(raw)}`);
  const parts = raw.replace(/\\/g, '/').split('/');
  if (parts.some(part => part === '..' || /:/.test(part))) throw new Error(`Unsafe archive path: ${JSON.stringify(raw)}`);
  const path = parts.filter(part => part !== '' && part !== '.').join('/');
  if (!path) throw new Error('Archive entry has no usable relative name.');
  return path;
}
function mime(path: string): string {
  const extension = path.split('.').pop()?.toLowerCase();
  return ({ pdf: 'application/pdf', png: 'image/png', jpg: 'image/jpeg', jpeg: 'image/jpeg', webp: 'image/webp', html: 'text/html', htm: 'text/html', xml: 'application/xml', rss: 'application/rss+xml', atom: 'application/atom+xml', json: 'application/json', jsonl: 'application/x-ndjson', txt: 'text/plain', md: 'text/markdown' } as Record<string, string>)[extension || ''] || 'application/octet-stream';
}
function exactNumber(value: bigint): number {
  if (value < BigInt(0) || value > BigInt(Number.MAX_SAFE_INTEGER)) throw new Error('This archive offset cannot be represented exactly by this browser.');
  return Number(value);
}
function u64(view: DataView, offset: number): number { return exactNumber(view.getBigUint64(offset, true)); }
async function range(file: Blob, start: number, length: number): Promise<Uint8Array<ArrayBuffer>> {
  if (!Number.isSafeInteger(start) || !Number.isSafeInteger(length) || start < 0 || length < 0 || start + length > file.size) throw new Error('Truncated or invalid archive structure.');
  const result = new Uint8Array(await file.slice(start, start + length).arrayBuffer());
  if (result.length !== length) throw new Error('The browser could not read the complete archive structure.');
  return result;
}
function view(bytes: Uint8Array): DataView { return new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength); }
function decodeStream(input: ReadableStream<Uint8Array<ArrayBuffer>>, format: 'gzip' | 'deflate-raw'): ReadableStream<Uint8Array<ArrayBuffer>> {
  let decoder: DecompressionStream;
  try { decoder = new DecompressionStream(format); }
  catch { throw new Error(`This browser does not support streaming ${format} decompression. Update the browser or extract this archive on your device.`); }
  const stop = new AbortController();
  const reader = decoder.readable.getReader();
  const pump = input.pipeTo(decoder.writable, { signal: stop.signal });
  // The same failure is delivered through decoder.readable. Attach its handler
  // immediately so an upstream error cannot become an unhandled rejection.
  void pump.catch(() => {});
  let cancelled = false;
  return new ReadableStream<Uint8Array<ArrayBuffer>>({
    async pull(controller) {
      try {
        const next = await reader.read();
        if (cancelled) return;
        if (next.done) { await pump; controller.close(); }
        else controller.enqueue(next.value);
      } catch (error) { if (!cancelled) controller.error(error); }
    },
    async cancel(reason) {
      cancelled = true;
      // Stop upstream first. Cancelling a live native decompressor's readable
      // side can close its controller while its writable side still emits data
      // (reproducible in Node 24's InflateRaw WebStream adapter). Drain only the
      // already in-flight write while abort settles; never consume new input.
      stop.abort(reason);
      const drain = (async () => { try { while (!(await reader.read()).done) { /* discard in-flight output */ } } catch { /* pump/read error belongs to cancellation */ } })();
      await Promise.all([pump.catch(() => {}), drain]);
      reader.releaseLock();
    },
  }, { highWaterMark: 0 });
}

class Reader {
  private reader: ReadableStreamDefaultReader<Uint8Array<ArrayBuffer>>;
  private chunk = new Uint8Array(0);
  private offset = 0;
  private abort: () => void;
  private closing?: Promise<void>;
  private cancel(reason?: unknown): Promise<void> {
    return this.closing ??= this.reader.cancel(reason).catch(() => {});
  }
  constructor(stream: ReadableStream<Uint8Array<ArrayBuffer>>, private signal?: AbortSignal) {
    this.reader = stream.getReader();
    this.abort = () => { void this.cancel(signal?.reason); };
    signal?.addEventListener('abort', this.abort, { once: true });
  }
  async part(max = BLOCK): Promise<Uint8Array<ArrayBuffer> | undefined> {
    aborted(this.signal);
    while (this.offset === this.chunk.length) {
      const next = await this.reader.read();
      aborted(this.signal);
      if (next.done) return undefined;
      this.chunk = next.value;
      this.offset = 0;
    }
    const end = Math.min(this.chunk.length, this.offset + max);
    const result = this.chunk.subarray(this.offset, end);
    this.offset = end;
    return result;
  }
  async take(length: number): Promise<Uint8Array<ArrayBuffer>> {
    const output = new Uint8Array(length);
    let used = 0;
    while (used < length) {
      const next = await this.part(length - used);
      if (!next) throw new Error('Truncated archive member or header.');
      output.set(next, used); used += next.length;
    }
    return output;
  }
  async *chunks(length: number): AsyncGenerator<Uint8Array<ArrayBuffer>> {
    let remaining = length;
    while (remaining) {
      const next = await this.part(Math.min(BLOCK, remaining));
      if (!next) throw new Error('Truncated archive member.');
      remaining -= next.length; yield next;
    }
  }
  async skip(length: number) { for await (const chunk of this.chunks(length)) { void chunk; } }
  async close() { this.signal?.removeEventListener('abort', this.abort); await this.cancel(); }
}

const crcTable = Uint32Array.from({ length: 256 }, (_, byte) => {
  for (let bit = 0; bit < 8; bit++) byte = byte & 1 ? 0xedb88320 ^ (byte >>> 1) : byte >>> 1;
  return byte >>> 0;
});
function crcUpdate(crc: number, bytes: Uint8Array): number { for (const byte of bytes) crc = crcTable[(crc ^ byte) & 255] ^ (crc >>> 8); return crc; }

async function spool(chunks: AsyncIterable<Uint8Array<ArrayBuffer>>, path: string, sourceArchive: string, onProgress?: Progress, signal?: AbortSignal, expected?: { size: number; crc: number }): Promise<Extract<ImportItem, { file: File }>> {
  aborted(signal);
  if (!navigator.storage?.getDirectory) throw new Error('This browser has no private streaming file storage. Update the browser or extract the archive on your device.');
  const root = await navigator.storage.getDirectory();
  const directory = await root.getDirectoryHandle('tpe-import-tmp', { create: true });
  const id = crypto.randomUUID();
  const handle = await directory.getFileHandle(id, { create: true });
  const dispose = async () => { await directory.removeEntry(id).catch(() => {}); };
  let writable: FileSystemWritableFileStream | undefined;
  try {
    if (!handle.createWritable) throw new Error('This browser cannot stream expanded files to private storage. Update the browser or extract the archive on your device.');
    writable = await handle.createWritable();
    let written = 0, crc = 0xffffffff, reported = 0;
    for await (const chunk of chunks) {
      aborted(signal);
      if (expected && written + chunk.length > expected.size) throw new Error(`ZIP member exceeds its declared size: ${path}`);
      await writable.write(chunk as Uint8Array<ArrayBuffer>);
      written += chunk.length; crc = crcUpdate(crc, chunk);
      const now = performance.now();
      if (now - reported >= 100) {
        onProgress?.({ path, phase: 'Expanding archive member', completed: written, total: expected?.size || 0 });
        reported = now;
      }
    }
    aborted(signal);
    if (expected && (written !== expected.size || ((crc ^ 0xffffffff) >>> 0) !== expected.crc)) throw new Error(`ZIP size or checksum mismatch: ${path}`);
    await writable.close(); writable = undefined;
    onProgress?.({ path, phase: 'Archive member ready', completed: written, total: written });
    const stored = await handle.getFile();
    return { file: new File([stored], path.split('/').pop()!, { type: mime(path) }), path, sourceArchive, dispose };
  } catch (error) {
    await writable?.abort().catch(() => {});
    await dispose();
    throw error;
  }
}
// The consumer may dispose early; iterator close also removes its current temporary file.
async function* deliver(item: Extract<ImportItem, { file: File }>): AsyncGenerator<ImportItem> {
  try { yield item; } finally { await item.dispose?.(); }
}
async function* allChunks(stream: ReadableStream<Uint8Array<ArrayBuffer>>, signal?: AbortSignal): AsyncGenerator<Uint8Array<ArrayBuffer>> {
  const reader = new Reader(stream, signal);
  try { for (;;) { const chunk = await reader.part(); if (!chunk) break; yield chunk; } }
  finally { await reader.close(); }
}

function tarNumber(bytes: Uint8Array): number {
  if (bytes[0] & 128) {
    if (bytes[0] & 64) throw new Error('Negative TAR member size is invalid.');
    let value = BigInt(bytes[0] & 127);
    for (const byte of bytes.subarray(1)) value = value * BigInt(256) + BigInt(byte);
    return exactNumber(value);
  }
  const field = new TextDecoder().decode(bytes).replace(/\0.*$/, '').trim();
  if (!/^[0-7]*$/.test(field)) throw new Error('Invalid TAR numeric field.');
  return field ? exactNumber(BigInt(`0o${field}`)) : 0;
}
function tarText(bytes: Uint8Array): string { const end = bytes.indexOf(0); return utf8.decode(end < 0 ? bytes : bytes.subarray(0, end)); }
function pax(bytes: Uint8Array): Record<string, string> {
  const result: Record<string, string> = {};
  let offset = 0;
  while (offset < bytes.length) {
    const space = bytes.indexOf(32, offset);
    if (space < 0) throw new Error('Invalid PAX record length.');
    const token = utf8.decode(bytes.subarray(offset, space));
    if (!/^[0-9]+$/.test(token)) throw new Error('Invalid PAX record length.');
    const length = exactNumber(BigInt(token));
    if (length <= space - offset + 1 || offset + length > bytes.length || bytes[offset + length - 1] !== 10) throw new Error('Truncated PAX record.');
    const record = utf8.decode(bytes.subarray(space + 1, offset + length - 1));
    const equals = record.indexOf('=');
    if (equals < 1) throw new Error('Invalid PAX metadata.');
    const key = record.slice(0, equals), value = record.slice(equals + 1);
    if (key === 'path' || key === 'size' || key === 'linkpath') result[key] = value;
    if (key.startsWith('GNU.sparse') || (key === 'SCHILY.filetype' && value !== 'file')) throw new Error('Sparse, linked or special TAR members are not supported.');
    offset += length;
  }
  return result;
}
async function* tarEntries(stream: ReadableStream<Uint8Array<ArrayBuffer>>, source: string, onProgress?: Progress, signal?: AbortSignal): AsyncGenerator<ImportItem> {
  const reader = new Reader(stream, signal);
  let global: Record<string, string> = {}, next: Record<string, string> = {};
  try {
    for (;;) {
      const header = await reader.take(512);
      if (header.every(byte => byte === 0)) {
        if (!(await reader.take(512)).every(byte => byte === 0)) throw new Error('Invalid TAR end marker.');
        // Drain the stream so GZIP's checksum and every trailer byte are validated.
        for (;;) { const tail = await reader.part(); if (!tail) return; if (tail.some(byte => byte !== 0)) throw new Error('Unexpected data after TAR end marker.'); }
      }
      const storedChecksum = tarNumber(header.subarray(148, 156));
      let checksum = 0;
      header.forEach((byte, index) => { checksum += index >= 148 && index < 156 ? 32 : byte; });
      if (checksum !== storedChecksum) throw new Error('TAR header checksum mismatch.');
      const declared = tarNumber(header.subarray(124, 136));
      const type = header[156];
      if ([120, 103, 76].includes(type)) {
        // This is a structural metadata budget, not a file/member-size cap.
        // Oversized names/PAX control records cannot become unbounded JS strings.
        if (declared > BLOCK) throw new Error('TAR control metadata exceeds the safe parser working buffer; file contents are not size-limited.');
        const data = await reader.take(declared);
        if (type === 76) next.path = tarText(data);
        else if (type === 103) global = { ...global, ...pax(data) };
        else next = { ...next, ...pax(data) };
        await reader.skip((512 - declared % 512) % 512);
        continue;
      }
      const metadata = { ...global, ...next }; next = {};
      const prefix = tarText(header.subarray(257, 263)) === 'ustar' ? tarText(header.subarray(345, 500)) : '';
      const rawPath = metadata.path || [prefix, tarText(header.subarray(0, 100))].filter(Boolean).join('/');
      // Root directory markers from `tar -cf archive.tar .` have no file payload.
      if (type === 53 && /^\.(?:\/\.)*\/?$/.test(rawPath) && !declared && !metadata.size && !metadata.linkpath) continue;
      const path = safeArchivePath(rawPath);
      if (![0, 48, 53].includes(type) || metadata.linkpath) throw new Error(`Linked or special TAR member rejected: ${path}`);
      if (metadata.size && !/^\d+$/.test(metadata.size)) throw new Error(`Invalid PAX member size: ${path}`);
      const size = metadata.size ? exactNumber(BigInt(metadata.size)) : declared;
      if (type === 53) { if (size) throw new Error(`TAR directory contains unexpected data: ${path}`); continue; }
      if (isArchive(path)) {
        await reader.skip(size);
        yield { path, sourceArchive: source, error: 'Nested archives are not expanded. This member remains in the saved original archive.' };
      } else yield* deliver(await spool(reader.chunks(size), path, source, onProgress, signal));
      await reader.skip((512 - size % 512) % 512);
    }
  } finally { await reader.close(); }
}

async function zipDirectory(file: File): Promise<{ offset: number; size: number; count: number }> {
  const base = Math.max(0, file.size - 65557), tail = await range(file, base, file.size - base), data = view(tail);
  for (let at = tail.length - 22; at >= 0; at--) {
    if (data.getUint32(at, true) !== 0x06054b50 || at + 22 + data.getUint16(at + 20, true) !== tail.length) continue;
    if (data.getUint16(at + 4, true) || data.getUint16(at + 6, true)) throw new Error('Multi-disk ZIP archives are not supported.');
    let count = data.getUint16(at + 10, true), size = data.getUint32(at + 12, true), offset = data.getUint32(at + 16, true);
    if (count === 65535 || size === 0xffffffff || offset === 0xffffffff) {
      const locator = view(await range(file, base + at - 20, 20));
      if (locator.getUint32(0, true) !== 0x07064b50 || locator.getUint32(4, true) || locator.getUint32(16, true) !== 1) throw new Error('Invalid or multi-disk ZIP64 locator.');
      const record = view(await range(file, u64(locator, 8), 56));
      if (record.getUint32(0, true) !== 0x06064b50 || record.getUint32(16, true) || record.getUint32(20, true)) throw new Error('Invalid ZIP64 directory.');
      count = u64(record, 32); size = u64(record, 40); offset = u64(record, 48);
    }
    if (offset + size > base + at || count > Math.floor(size / 46)) throw new Error('Invalid ZIP directory bounds.');
    return { count, size, offset };
  }
  throw new Error('ZIP end-of-directory record is missing or truncated.');
}
function zipName(bytes: Uint8Array, flags: number): string {
  if (flags & 2048 || bytes.every(byte => byte < 128)) return utf8.decode(bytes);
  throw new Error('This ZIP uses a legacy filename encoding. Repack it with UTF-8 filenames to preserve names exactly.');
}
async function* zipEntries(file: File, source: string, onProgress?: Progress, signal?: AbortSignal): AsyncGenerator<ImportItem> {
  const directory = await zipDirectory(file);
  let cursor = directory.offset;
  for (let index = 0; index < directory.count; index++) {
    aborted(signal);
    const header = view(await range(file, cursor, 46));
    if (header.getUint32(0, true) !== 0x02014b50) throw new Error('Invalid ZIP central-directory entry.');
    const flags = header.getUint16(8, true), method = header.getUint16(10, true), crc = header.getUint32(16, true);
    let compressed = header.getUint32(20, true), size = header.getUint32(24, true), local = header.getUint32(42, true);
    const nameLength = header.getUint16(28, true), extraLength = header.getUint16(30, true), commentLength = header.getUint16(32, true);
    if (cursor + 46 + nameLength + extraLength + commentLength > directory.offset + directory.size) throw new Error('Truncated ZIP directory entry.');
    const rawName = zipName(await range(file, cursor + 46, nameLength), flags), path = safeArchivePath(rawName);
    const mode = (header.getUint32(38, true) >>> 16) & 0xf000;
    if ((mode && mode !== 0x8000 && mode !== 0x4000) || flags & 0x2061) throw new Error(`Linked, special, patched or encrypted ZIP member rejected: ${path}`);
    if (header.getUint16(34, true) !== 0) throw new Error('Multi-disk ZIP member is not supported.');
    const extra = await range(file, cursor + 46 + nameLength, extraLength);
    let zip64 = false;
    for (let at = 0; at < extra.length;) {
      if (at + 4 > extra.length) throw new Error('Truncated ZIP extra field.');
      const info = view(extra), tag = info.getUint16(at, true), length = info.getUint16(at + 2, true);
      if (at + 4 + length > extra.length) throw new Error('Truncated ZIP extra field.');
      if (tag === 1) {
        let item = at + 4; const end = item + length;
        const number = () => { if (item + 8 > end) throw new Error('Truncated ZIP64 size.'); const n = u64(info, item); item += 8; return n; };
        if (size === 0xffffffff) size = number();
        if (compressed === 0xffffffff) compressed = number();
        if (local === 0xffffffff) local = number();
        zip64 = true;
      }
      if (tag === 0x000d && length > 12) throw new Error(`ZIP Unix link/device metadata is not supported: ${path}`);
      at += 4 + length;
    }
    if (!zip64 && [size, compressed, local].includes(0xffffffff)) throw new Error('ZIP64 metadata is missing.');
    cursor += 46 + nameLength + extraLength + commentLength;
    onProgress?.({ path, phase: 'Reading archive directory', completed: index, total: directory.count });
    if (rawName.endsWith('/') || mode === 0x4000) { if (size || compressed) throw new Error(`ZIP directory unexpectedly contains data: ${path}`); continue; }
    if (method !== 0 && method !== 8) throw new Error(`ZIP compression method ${method} is not supported: ${path}`);
    const localHeader = view(await range(file, local, 30));
    if (localHeader.getUint32(0, true) !== 0x04034b50 || localHeader.getUint16(6, true) !== flags || localHeader.getUint16(8, true) !== method) throw new Error(`ZIP local/directory metadata disagree: ${path}`);
    const localNameLength = localHeader.getUint16(26, true), localExtraLength = localHeader.getUint16(28, true);
    if (zipName(await range(file, local + 30, localNameLength), flags) !== rawName) throw new Error(`ZIP local/directory paths disagree: ${path}`);
    const start = local + 30 + localNameLength + localExtraLength;
    if (start + compressed > directory.offset) throw new Error(`ZIP data overlaps its directory: ${path}`);
    if (isArchive(path)) { yield { path, sourceArchive: source, error: 'Nested archives are not expanded. This member remains in the saved original archive.' }; continue; }
    let stream: ReadableStream<Uint8Array<ArrayBuffer>> = file.slice(start, start + compressed).stream();
    if (method === 8) stream = decodeStream(stream, 'deflate-raw');
    yield* deliver(await spool(allChunks(stream, signal), path, source, onProgress, signal, { size, crc }));
  }
  if (cursor !== directory.offset + directory.size) throw new Error('ZIP directory has unparsed trailing records.');
}

export async function* expandUploads(files: Iterable<File>, onProgress?: Progress, signal?: AbortSignal): AsyncGenerator<ImportItem> {
  for (const file of files) {
    aborted(signal);
    const originalPath = file.webkitRelativePath || file.name;
    try {
      const path = safeArchivePath(originalPath);
      if (!isArchive(file)) { yield { file, path }; continue; }
      onProgress?.({ path, phase: 'Opening archive', completed: 0, total: file.size });
      if (/\.zip$/i.test(file.name)) { yield* zipEntries(file, path, onProgress, signal); continue; }
      let stream: ReadableStream<Uint8Array<ArrayBuffer>> = file.stream();
      const gzip = /\.(?:tgz|gz|gzip)$/i.test(file.name);
      if (gzip) stream = decodeStream(stream, 'gzip');
      if (/\.(?:tar|tar\.gz|tar\.gzip|tgz)$/i.test(file.name)) { yield* tarEntries(stream, path, onProgress, signal); continue; }
      const member = safeArchivePath(path.replace(/\.(?:gz|gzip)$/i, ''));
      if (isArchive(member)) throw new Error('Nested compressed archives are not expanded. The original remains saved.');
      yield* deliver(await spool(allChunks(stream, signal), member, path, onProgress, signal));
    } catch (error) {
      aborted(signal);
      yield { path: originalPath, sourceArchive: originalPath, error: failure(error) };
    }
  }
}
