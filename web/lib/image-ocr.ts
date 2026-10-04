import type { Extracted, LinkEvidence } from './types';

export type OcrProgress = { status: string; progress: number };
export const OCR_ASSET_BASE = '/ocr/7.0.0';
const WORKING_PIXELS = 4_000_000;
const WORKING_SIDE = 4096;
type Dimensions = { width: number; height: number; orientation: number };
const ascii = new TextDecoder('latin1');

export function isOcrImage(file: File | string): boolean {
  return typeof file === 'string' ? /\.(?:png|jpe?g|webp)$/i.test(file) : /^(?:image\/png|image\/jpeg|image\/webp)$/i.test(file.type) || /\.(?:png|jpe?g|webp)$/i.test(file.name);
}
async function bytes(file: Blob, offset: number, length: number): Promise<Uint8Array> {
  if (offset < 0 || length < 0 || offset + length > file.size) throw new Error('The image header is truncated.');
  return new Uint8Array(await file.slice(offset, offset + length).arrayBuffer());
}
function dataView(value: Uint8Array) { return new DataView(value.buffer, value.byteOffset, value.byteLength); }
async function exifOrientation(file: Blob, offset: number, length: number, signal?: AbortSignal): Promise<number | undefined> {
  if (length < 8) return undefined;
  if (ascii.decode(await bytes(file, offset, 6)) === 'Exif\0\0') { offset += 6; length -= 6; }
  if (length < 8) return undefined;
  const header = dataView(await bytes(file, offset, 8));
  const little = header.getUint16(0, false) === 0x4949;
  if (!little && header.getUint16(0, false) !== 0x4d4d || header.getUint16(2, little) !== 42) return undefined;
  const first = header.getUint32(4, little);
  if (first + 2 > length) return undefined;
  const count = dataView(await bytes(file, offset + first, 2)).getUint16(0, little);
  for (let index = 0; index < count && first + 2 + (index + 1) * 12 <= length; index++) {
    signal?.throwIfAborted();
    const item = dataView(await bytes(file, offset + first + 2 + index * 12, 12));
    if (item.getUint16(0, little) === 0x112 && item.getUint16(2, little) === 3 && item.getUint32(4, little) === 1) {
      const orientation = item.getUint16(8, little);
      return orientation >= 1 && orientation <= 8 ? orientation : undefined;
    }
  }
  return undefined;
}
/** Inspect dimensions before invoking the pixel decoder; never read an entire original into JS. */
export async function inspectOcrImage(file: Blob, signal?: AbortSignal): Promise<Dimensions> {
  signal?.throwIfAborted();
  const signature = await bytes(file, 0, Math.min(file.size, 32));
  let width = 0, height = 0, orientation = 1;
  if (signature.length >= 24 && signature[0] === 137 && ascii.decode(signature.subarray(1, 4)) === 'PNG' && signature[4] === 13 && signature[5] === 10 && signature[6] === 26 && signature[7] === 10) {
    if (ascii.decode(signature.subarray(12, 16)) !== 'IHDR') throw new Error('PNG does not begin with its image header.');
    width = dataView(signature).getUint32(16); height = dataView(signature).getUint32(20);
    for (let at = 8; at + 12 <= file.size;) {
      signal?.throwIfAborted();
      const chunk = await bytes(file, at, 8), length = dataView(chunk).getUint32(0), kind = ascii.decode(chunk.subarray(4));
      if (at + 12 + length > file.size) throw new Error('Truncated PNG chunk.');
      if (kind === 'acTL') throw new Error('Animated PNG is retained as an original; image OCR requires a single still image.');
      if (kind === 'eXIf') orientation = await exifOrientation(file, at + 8, length, signal) ?? orientation;
      at += 12 + length;
      if (kind === 'IEND') break;
    }
  } else if (signature[0] === 255 && signature[1] === 216) {
    for (let at = 2; at < file.size;) {
      signal?.throwIfAborted();
      let marker = await bytes(file, at++, 1);
      if (marker[0] !== 255) throw new Error('Invalid JPEG segment marker.');
      do { marker = await bytes(file, at++, 1); } while (marker[0] === 255);
      if (marker[0] === 0xda || marker[0] === 0xd9) break;
      if (marker[0] === 1 || marker[0] >= 0xd0 && marker[0] <= 0xd7) continue;
      const length = dataView(await bytes(file, at, 2)).getUint16(0);
      if (length < 2 || at + length > file.size) throw new Error('Truncated JPEG segment.');
      if ([0xc0, 0xc1, 0xc2, 0xc3, 0xc5, 0xc6, 0xc7, 0xc9, 0xca, 0xcb, 0xcd, 0xce, 0xcf].includes(marker[0])) {
        const frame = dataView(await bytes(file, at + 2, 5)); height = frame.getUint16(1); width = frame.getUint16(3);
      }
      if (marker[0] === 0xe1) orientation = await exifOrientation(file, at + 2, length - 2, signal) ?? orientation;
      if (marker[0] === 0xe2 && length >= 6 && ascii.decode(await bytes(file, at + 2, 4)) === 'MPF\0') throw new Error('Multi-picture JPEG is retained as an original; export the individual still images for OCR.');
      at += length;
    }
  } else if (signature.length >= 16 && ascii.decode(signature.subarray(0, 4)) === 'RIFF' && ascii.decode(signature.subarray(8, 12)) === 'WEBP') {
    const end = dataView(signature).getUint32(4, true) + 8;
    if (end > file.size) throw new Error('Truncated WebP container.');
    for (let at = 12; at + 8 <= end;) {
      signal?.throwIfAborted();
      const chunk = await bytes(file, at, 8), kind = ascii.decode(chunk.subarray(0, 4)), length = dataView(chunk).getUint32(4, true);
      if (at + 8 + length > end) throw new Error('Truncated WebP chunk.');
      if (kind === 'ANIM' || kind === 'ANMF') throw new Error('Animated WebP is retained as an original; image OCR requires a single still image.');
      const prefix = await bytes(file, at + 8, Math.min(length, 10));
      if (kind === 'VP8X' && prefix.length >= 10) {
        if (prefix[0] & 2) throw new Error('Animated WebP requires separate still images for OCR.');
        width = 1 + prefix[4] + prefix[5] * 256 + prefix[6] * 65536;
        height = 1 + prefix[7] + prefix[8] * 256 + prefix[9] * 65536;
      } else if (kind === 'VP8 ' && prefix.length >= 10 && prefix[3] === 0x9d && prefix[4] === 1 && prefix[5] === 0x2a && !width) {
        width = dataView(prefix).getUint16(6, true) & 0x3fff; height = dataView(prefix).getUint16(8, true) & 0x3fff;
      } else if (kind === 'VP8L' && prefix.length >= 5 && prefix[0] === 0x2f && !width) {
        const bits = dataView(prefix).getUint32(1, true); width = (bits & 0x3fff) + 1; height = ((bits >>> 14) & 0x3fff) + 1;
      }
      if (kind === 'EXIF') orientation = await exifOrientation(file, at + 8, length, signal) ?? orientation;
      at += 8 + length + length % 2;
    }
  } else throw new Error('Image OCR currently decodes JPEG, PNG and WebP still images. HEIC, GIF and other originals remain saved; export a still JPEG or PNG for OCR.');
  if (!Number.isInteger(width) || !Number.isInteger(height) || width <= 0 || height <= 0) throw new Error('The image does not contain valid pixel dimensions.');
  return orientation >= 5 ? { width: height, height: width, orientation } : { width, height, orientation };
}

/** Direct adapter to the exact Tesseract.js 7.0.0 worker protocol.
 * The public createWorker Promise hides its worker until initialization finishes;
 * owning this handle lets AbortSignal terminate model initialization immediately.
 * Protocol matches pinned src/createWorker.js. Node/WASM protocol and optional
 * offline browser fixtures are provided in scripts/test-*-imports.mjs.
 */
class OcrWorker {
  private worker: Worker;
  private counter = 0;
  private closed?: Error;
  private pending = new Map<string, { resolve: (data: unknown) => void; reject: (error: Error) => void }>();
  private abort: () => void;
  constructor(private progress?: (event: OcrProgress) => void, private signal?: AbortSignal) {
    signal?.throwIfAborted();
    this.worker = new Worker(`${OCR_ASSET_BASE}/worker.min.js`);
    this.abort = () => this.close(new DOMException('Image OCR cancelled. The original remains saved.', 'AbortError'));
    signal?.addEventListener('abort', this.abort, { once: true });
    this.worker.onerror = event => this.close(new Error(event.message || 'The OCR worker failed to load.'));
    this.worker.onmessageerror = () => this.close(new Error('The OCR worker returned an unreadable message.'));
    this.worker.onmessage = event => {
      const packet = event.data;
      if (packet.status === 'progress') {
        const value = Number(packet.data?.progress);
        this.progress?.({ status: String(packet.data?.status || 'Recognizing image'), progress: Number.isFinite(value) ? Math.max(0, Math.min(1, value)) : 0 });
        return;
      }
      const operation = this.pending.get(packet.jobId);
      if (!operation) return;
      this.pending.delete(packet.jobId);
      if (packet.status === 'resolve') operation.resolve(packet.data);
      else operation.reject(new Error(typeof packet.data === 'string' ? packet.data : 'OCR failed to process the image.'));
    };
  }
  call(action: string, payload: Record<string, unknown>, transfer: Transferable[] = []): Promise<unknown> {
    this.signal?.throwIfAborted();
    if (this.closed) return Promise.reject(this.closed);
    const jobId = `image-${++this.counter}`;
    return new Promise((resolve, reject) => {
      this.pending.set(jobId, { resolve, reject });
      this.worker.postMessage({ workerId: 'tpe-image-ocr', jobId, action, payload }, transfer);
    });
  }
  close(error = new Error('OCR worker closed.')) {
    this.closed = error;
    this.worker.terminate();
    this.signal?.removeEventListener('abort', this.abort);
    for (const operation of this.pending.values()) operation.reject(error);
    this.pending.clear();
  }
}

function ocrLinks(text: string): LinkEvidence[] {
  const matches = text.match(/10\.\d{4,9}\/[^\s<>"?#]+/gi) || [];
  return Array.from(new Set(matches.map(doi => doi.replace(/[.,;]+$/, '')))).map(doi => ({ url: `https://doi.org/${doi}`, doi, kind: 'OCR DOI (unverified)' }));
}
export async function recognizeImage(file: File, onProgress?: (event: OcrProgress) => void, signal?: AbortSignal): Promise<Extracted> {
  signal?.throwIfAborted();
  onProgress?.({ status: 'Inspecting image', progress: 0 });
  const original = await inspectOcrImage(file, signal);
  // Working-raster memory budget, not an accepted-image size cap. Preserve the
  // full original and disclose the lower processing resolution in the result.
  const scale = Math.min(1, Math.sqrt(WORKING_PIXELS / (original.width * original.height)), WORKING_SIDE / Math.max(original.width, original.height));
  const width = Math.max(1, Math.floor(original.width * scale)), height = Math.max(1, Math.floor(original.height * scale));
  let bitmap: ImageBitmap | undefined, canvas: HTMLCanvasElement | undefined, worker: OcrWorker | undefined;
  try {
    bitmap = await createImageBitmap(file, { resizeWidth: width, resizeHeight: height, resizeQuality: 'high', imageOrientation: 'from-image' });
    signal?.throwIfAborted();
    canvas = document.createElement('canvas'); canvas.width = bitmap.width; canvas.height = bitmap.height;
    const context = canvas.getContext('2d');
    if (!context) throw new Error('The browser could not allocate the OCR working image. The full original remains saved.');
    context.fillStyle = '#fff'; context.fillRect(0, 0, canvas.width, canvas.height); context.drawImage(bitmap, 0, 0);
    const png = await new Promise<Blob>((resolve, reject) => canvas!.toBlob(value => value ? resolve(value) : reject(new Error('The browser could not encode the OCR working image.')), 'image/png'));
    const image = new Uint8Array(await png.arrayBuffer());
    signal?.throwIfAborted();
    worker = new OcrWorker(onProgress, signal);
    await worker.call('load', { options: { lstmOnly: true, corePath: new URL(`${OCR_ASSET_BASE}/core`, location.origin).href, logging: false } });
    await worker.call('loadLanguage', { langs: 'eng', options: { langPath: new URL(`${OCR_ASSET_BASE}/lang`, location.origin).href, gzip: true, cacheMethod: 'write', cachePath: 'tpe-eng-best-int-1.0.0', lstmOnly: true } });
    await worker.call('initialize', { langs: 'eng', oem: 1, config: {} });
    const output = await worker.call('recognize', { image, options: {}, output: { text: true, blocks: false, hocr: false, tsv: false, pdf: false } }, [image.buffer]) as { text?: unknown; confidence?: unknown };
    if (typeof output.text !== 'string') throw new Error('OCR returned no valid text result.');
    const text = output.text.trim(), confidence = typeof output.confidence === 'number' && Number.isFinite(output.confidence) ? output.confidence : null;
    const warnings = ['OCR is an English-model transcription and may omit or misread text. Compare important details and identifiers with the saved original.'];
    if (scale < 1) warnings.push(`The full original was retained. OCR used a ${canvas.width} × ${canvas.height} working image to control browser memory; small print may be missed.`);
    if (!text) warnings.push('No text was recognized; this does not establish that the original image has no text.');
    onProgress?.({ status: 'OCR finished; review the transcription', progress: 1 });
    return { title: file.name, text, markdown: text, links: ocrLinks(text), warnings, engine: 'Tesseract.js 7.0.0 (CPU/WASM)', status: 'partial', metadata: { ocr: { language: 'eng', confidence, originalWidth: original.width, originalHeight: original.height, processedWidth: canvas.width, processedHeight: canvas.height, downscaled: scale < 1, runtime: 'cpu-wasm', model: 'eng best_int 1.0.0' } } };
  } catch (error) {
    if (signal?.aborted) throw signal.reason;
    if (error instanceof RangeError) throw new Error('The browser could not allocate memory to decode this image. The full original remains saved; try a lower-resolution working copy.');
    throw error;
  } finally {
    worker?.close(); bitmap?.close();
    if (canvas) { canvas.width = 1; canvas.height = 1; }
  }
}
