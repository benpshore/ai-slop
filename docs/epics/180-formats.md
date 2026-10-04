# [Web app][Epic] Office/archive/OCR/media format coverage with real retained content

GitHub: https://github.com/benpshore/pdftextract/issues/180
Created: 2026-10-04T01:11:38Z
Status at export: open

Recorded 2026-10-04T01:09:25Z (2026-10-03 19:09:25 America/Denver). This epic is part of the user's requested Dot/Codex handoff for **benpshore/pdftextract**. Implementation is tracked in [PR #175](https://github.com/benpshore/pdftextract/pull/175); the core repair baseline is [merged PR #173](https://github.com/benpshore/pdftextract/pull/173), main commit e27e1fb28a5b40dbca7f517c6fe396e6f11ed4ec.

Current distinction: PR #175's published head was 77c58ec89a1c9c07425b77dec90ab00316aa07be at this record's start; follow-up fixes are being validated for the next revision. The live Site's first alpha uses upstream PDF Oxide WASM, not the complete native Rust engine. See the final handoff/source manifest for the exact subsequent publication revision.

## Scope
Automatic content handling for PDFs/PDF-A, HTML/XML/feeds, ZIP/GZIP/folders, photos, DOCX/PPTX/XLSX, Pages/Numbers and audio/video. Accepting/storing a file is not evidence that its contents were extracted.

## Implemented evidence
- `public/pdf-worker.js`: upstream PDF Oxide0.3.77 browser PDF text/links with explicit Partial status and no arbitrary page/text cap. It is not the native multi-engine backend.
- `imports.ts`: streaming archive staging, CRC/path/link validation and explicit unsupported structures. 21 Node/WASM/storage-shim assertions include >8MiB member, >200 members, cancellation/quota teardown and repeated compressed-stream lifecycle regressions.
- `image-ocr.ts`: pinned same-origin Tesseract7 English CPU/WASM worker, cancellation during initialization, real fixture text/DOI recovery, explicit OCR uncertainty and working-raster downscaling evidence.
- `office.ts`: real DOCX/PPTX/XLSX text/structure/tables/hyperlinks/embedded-image fixtures, selective ZIP streams, fast directory detection and async asset persistence. ODF/iWork evidence is explicitly partial.
- Original image/audio/video media can be displayed/played through owner-checked byte-range endpoints; unsupported originals are saved honestly.

## Remaining acceptance
- [ ] Browser and actual iPhone/iPad OCR verification; scanned-PDF render→OCR with page/region provenance and retained PDF figures.
- [ ] Speech transcription, video/audio track processing and time-aligned text; playback alone does not satisfy extraction.
- [ ] Complete Pages/Numbers/Keynote decoding or documented tested alternate conversion; legacy Office/PDF-A validation and encrypted formats as needed.
- [ ] Office ZIP CRC verification, formula evaluation policy, visual layout/style fidelity and large embedded-media stress.
- [ ] Reliable multilingual OCR and image/table/layout accuracy corpus.
- [ ] Device acceleration only after feature detection and measured implementation. WebGPU presence is not proof of ANE access.

Browser harness exists, but its execution was blocked by a failed Chromium download; Node/WASM success must not be described as iOS validation.
