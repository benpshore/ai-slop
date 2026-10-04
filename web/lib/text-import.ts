import type {DocumentRow, Extracted, LinkEvidence} from './types';

export type TextImportDraft = {
  original: File;
  result: Extracted;
  timings: {detectMs: number; decodeMs: number; projectionMs: number; totalMs: number};
};
export type TextSaveOutcome = {
  status: 'saved' | 'unsaved' | 'cancelled';
  stage: 'original' | 'result';
  record?: DocumentRow;
  error?: unknown;
};
type SaveOptions = {signal?: AbortSignal; onOriginalSaved?: (record: DocumentRow) => void};
type TextStorage = {
  uploadOriginal: (file: File, options: {sourceUrl?: string; signal?: AbortSignal}) => Promise<DocumentRow>;
  saveExtracted: (record: DocumentRow, result: Extracted, options: {signal?: AbortSignal}) => Promise<void>;
  yieldBeforeSave?: () => Promise<void>;
};

const now = () => performance.now();
const markdownType = /^text\/(?:x-)?markdown(?:;|$)/i;

function encodingFor(bytes: Uint8Array, mime: string): string {
  if (bytes[0] === 0xff && bytes[1] === 0xfe) return 'utf-16le';
  if (bytes[0] === 0xfe && bytes[1] === 0xff) return 'utf-16be';
  if (bytes[0] === 0xef && bytes[1] === 0xbb && bytes[2] === 0xbf) return 'utf-8';
  return mime.match(/charset\s*=\s*["']?([^\s;"']+)/i)?.[1] || 'utf-8';
}

function isOtherFormat(bytes: Uint8Array, prefix: string): boolean {
  // Content wins over a .txt/.md name. This gate intentionally declines other
  // formats; their normal adapters retain responsibility for extracting them.
  const raw = new TextDecoder().decode(bytes);
  const signaturePrefix = raw.replace(/^\uFEFF/, '').trimStart();
  if (prefix.trimStart().slice(0, 1024).includes('%PDF-') || signaturePrefix.slice(0, 1024).includes('%PDF-')) return true;
  if ((bytes[0] === 0x50 && bytes[1] === 0x4b && [3, 5, 7].includes(bytes[2])) ||
      (bytes[0] === 0x1f && bytes[1] === 0x8b) || new TextDecoder().decode(bytes.subarray(257, 262)) === 'ustar') return true;
  if ((bytes[0] === 0x89 && bytes[1] === 0x50 && bytes[2] === 0x4e && bytes[3] === 0x47) ||
      (bytes[0] === 0xff && bytes[1] === 0xd8 && bytes[2] === 0xff) ||
      /^(?:GIF8[79]a|BM|RIFF[\s\S]{4}WEBP)/.test(signaturePrefix) ||
      (bytes[0] === 0x49 && bytes[1] === 0x49 && bytes[2] === 42) ||
      (bytes[0] === 0x4d && bytes[1] === 0x4d && bytes[3] === 42)) return true;
  return [signaturePrefix, prefix.trimStart()].some(value =>
    /^[\[{]/.test(value) || /^<(?:[!?]|\/?[a-z][\w:.-]*(?:\s|\/?[>]))/i.test(value) ||
    /<(?:\w+:)?(?:rss|feed|RDF)(?:\s|>)/i.test(value));
}

function printedDois(text: string): LinkEvidence[] {
  // Keep the existing printed-DOI contract without importing the HTML toolkit.
  return Array.from(new Set(text.match(/10\.\d{4,9}\/[^\s<>"?#]+/gi) || [])).map(value => {
    const doi = value.replace(/[.,;]+$/, '');
    return {url: `https://doi.org/${doi}`, doi, kind: 'printed DOI'};
  });
}

/** Prepare readable text without hashing, network, storage, DOM or parser imports.
 * null means the caller must use its ordinary format router, never a PDF fallback.
 * Keep original and result in the queue before starting a TextImportSession save.
 */
export async function prepareTextImport(
  file: File,
  options: {sourceUrl?: string; signal?: AbortSignal} = {},
): Promise<TextImportDraft | null> {
  const started = now();
  options.signal?.throwIfAborted();
  // Match upload-client's existing text contract; richer MIME aliases without
  // an eligible name remain on the ordinary router until it supports them too.
  if (file.type !== 'text/plain' && !/\.(?:txt|md|markdown)$/i.test(file.name)) return null;
  if (file.type === 'text/css' || /\.(?:css|tar)$/i.test(file.name)) return null;
  const bytes = new Uint8Array(await file.slice(0, 4096).arrayBuffer());
  options.signal?.throwIfAborted();
  const encoding = encodingFor(bytes, file.type);
  let decoder: TextDecoder;
  try { decoder = new TextDecoder(encoding, {fatal: true}); }
  catch { return null; } // Let the ordinary decoder report unsupported encodings.
  let prefix: string;
  try { prefix = new TextDecoder(encoding).decode(bytes); }
  catch { return null; }
  if (isOtherFormat(bytes, prefix)) return null;
  const detected = now();
  const reader = file.stream().getReader(), chunks: string[] = [];
  const cancelRead = () => { void reader.cancel().catch(() => {}); };
  options.signal?.addEventListener('abort', cancelRead, {once: true});
  try {
    while (true) {
      options.signal?.throwIfAborted();
      const {done, value} = await reader.read();
      options.signal?.throwIfAborted();
      if (done) break;
      chunks.push(decoder.decode(value, {stream: true}));
    }
    chunks.push(decoder.decode());
  } catch (error) {
    if (options.signal?.aborted) throw options.signal.reason;
    if (error instanceof TypeError) return null; // Invalid text bytes, not a successful transcription.
    throw error;
  } finally {
    options.signal?.removeEventListener('abort', cancelRead);
    await reader.cancel().catch(() => {});
    reader.releaseLock();
  }
  const text = chunks.join('');
  if (/[\u0000-\u0008\u000b\u000e-\u001f]/.test(text)) return null;
  const decoded = now();
  const result: Extracted = {
    title: file.name, text, markdown: text, links: printedDois(text), warnings: [],
    engine: 'Plain text decoder', status: 'ready',
    metadata: {
      sourceUrl: options.sourceUrl || null, contentType: 'text',
      textImport: {
        version: 1, format: /\.(?:md|markdown)$/i.test(file.name) || markdownType.test(file.type) ? 'markdown' : 'text',
        encoding: decoder.encoding, originalName: file.name, originalMime: file.type,
        originalBytes: file.size, originalLastModified: file.lastModified,
      },
    },
  };
  const projected = now();
  return {original: file, result, timings: {detectMs: detected - started, decodeMs: decoded - detected, projectionMs: projected - decoded, totalMs: projected - started}};
}

/** Retain readable output and the confirmed original receipt when storage fails.
 * The default task yield lets a UI commit its readable state before saving; it
 * does not guarantee a browser paint. Callers may supply their own render yield.
 * A save resolves to an outcome, so background failures cannot discard the draft.
 * Keep this session for retries, or restore it with a previously confirmed record.
 */
export function createTextImportSession(draft: TextImportDraft, storage: TextStorage, existingRecord?: DocumentRow) {
  let record = existingRecord;
  let lastOutcome: TextSaveOutcome | undefined;
  let inFlight: Promise<TextSaveOutcome> | undefined;
  const yieldBeforeSave = storage.yieldBeforeSave || (() => new Promise<void>(resolve => setTimeout(resolve, 0)));
  const session = {
    draft,
    get record() { return record; },
    get lastOutcome() { return lastOutcome; },
    save(options: SaveOptions = {}): Promise<TextSaveOutcome> {
      if (inFlight) return inFlight;
      if (lastOutcome?.status === 'saved') return Promise.resolve(lastOutcome);
      let stage: TextSaveOutcome['stage'] = record ? 'result' : 'original';
      const run = async (): Promise<TextSaveOutcome> => {
        try {
          options.signal?.throwIfAborted();
          await yieldBeforeSave();
          options.signal?.throwIfAborted();
          if (!record) {
            const sourceUrl = draft.result.metadata?.sourceUrl;
            record = await storage.uploadOriginal(draft.original, {sourceUrl: typeof sourceUrl === 'string' ? sourceUrl : undefined, signal: options.signal});
            stage = 'result';
            options.onOriginalSaved?.(record);
          }
          options.signal?.throwIfAborted();
          await storage.saveExtracted(record, draft.result, {signal: options.signal});
          // Storage finalization is authoritative even if cancellation happened
          // while its commit was in flight. Do not label a confirmed save lost.
          return {status: 'saved', stage: 'result', record};
        } catch (error) {
          return {status: options.signal?.aborted || (error instanceof Error && error.name === 'AbortError') ? 'cancelled' : 'unsaved', stage, record, error};
        }
      };
      inFlight = run().then(outcome => {lastOutcome = outcome; inFlight = undefined; return outcome;});
      return inFlight;
    },
  };
  return session;
}
