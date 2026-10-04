# Mobile OCR and chapter-reader feasibility

Evidence reviewed on 2026-10-04. This is an implementation review, not a claim that the current application has passed iPhone or iPad testing. **Document size and page count are not application admission limits.** The user’s account storage allocation is a separate quota; the current requested allocation is 100 GB.

## What can be supported honestly

| Capability | Evidence and implementation decision | Unproven or unavailable guarantee |
| --- | --- | --- |
| Local browser OCR | Tesseract.js 7 and its WASM core provide a concrete CPU path. Download same-origin worker/core and only the selected language model; keep document pixels local. | No measured iPhone/iPad mini speed, battery, peak memory, or accuracy claim yet. Tesseract.js does not establish WebGPU, WebNN, or Apple Neural Engine execution. |
| Safari GPU compute | WebKit states that Safari 26 ships WebGPU on iOS and iPadOS. Probe the API, request an adapter, and smoke-test the exact model/runtime. | WebGPU availability does not establish access to the Apple Neural Engine, nor that every ONNX operator/model works. |
| Browser NPU inference | WebNN describes an accelerator abstraction. Treat it as an optional experiment after a real context/model test. | Reviewed evidence does not establish production Safari/iOS WebNN-to-ANE availability. Do not advertise ANE OCR. |
| Chrome on iPhone/iPad | Detect actual APIs and successful inference on the installed browser/OS. | Do not copy the desktop Chrome support matrix onto iOS. Regional alternative-engine policy also makes an unconditional engine assumption inappropriate. |
| Background chapter playback | Use a user-started HTML audio element, chapter metadata, and supported Media Session controls. Save progress independently of playback. | No promise of indefinitely running background JavaScript, background OCR, or uninterrupted chapter transitions after suspension/discard. |
| Programmatic neural narration | A future, explicit OpenAI audio integration can generate narration. Keep the provider/model/auth choice replaceable. | ChatGPT consumer voice-session reuse or subscription-funded speech is not established by the reviewed API/auth documentation. No voice requests were made. |

Sources: [Tesseract.js releases](https://github.com/naptha/tesseract.js/releases), [Tesseract WASM core releases](https://github.com/naptha/tesseract.js-core/releases), [language-data choices](https://github.com/naptha/tessdata/blob/gh-pages/README.md), [Safari 26 release](https://webkit.org/blog/17333/webkit-features-in-safari-26-0/), [WebNN specification](https://www.w3.org/TR/webnn/), [WebKit WebNN position issue](https://github.com/WebKit/standards-positions/issues/486), [Apple alternative browser engines](https://developer.apple.com/support/alternative-browser-engines/).

The ONNX Runtime Web compatibility table still marks Safari/iOS WebGPU unsupported while WebKit’s later release announcement says the browser API ships. These are different claims: the runtime/model combination needs its own test. A reviewed Chromium revision also disables its iOS CoreML backend because of sandbox issues; that pinned source is not proof of every current Chrome build’s behavior. Sources: [ONNX Runtime Web support](https://onnxruntime.ai/docs/get-started/with-javascript/web.html), [inspected Chromium feature definition](https://chromium.googlesource.com/chromium/src/+/3756e69d285fdf20d2821fa0b0911f77e5ae85a3/services/webnn/public/mojom/features.mojom).

## Proposed browser execution path

1. Extract reliable native PDF text before rendering/OCR. Preserve the source page number and real transform from raster pixels back to PDF coordinates. OCR must not manufacture native font or Unicode-map provenance.
2. Use a dedicated worker and one page or image at a time. Start with the verified WASM CPU backend. A future GPU backend must pass a tiny inference test, then fall back cleanly on session creation, operator, allocation, or device-loss errors. Feature presence alone is insufficient.
3. Adapt working raster resolution or use tiles when an image is too costly to process at once. Retain original bytes, record any downsampling and its accuracy consequence, and keep the result partial where coverage is incomplete. This is a working-memory policy, not a maximum accepted document size.
4. Release decoded canvases, images, and tensors promptly. Persist completed page results and cancellation state so a mobile tab interruption does not require starting again. Never route a document to a remote OCR service merely because local inference failed.
5. Model acquisition should show progress and permit cancellation. Cache by version and integrity identity, not an unversioned URL. The engine label must identify the execution provider actually used; “WebGPU” must not be relabeled “ANE.”

Required device evidence: one supported iPhone and one iPad mini, Safari and Chrome, browser and installed app mode where applicable; typed PDF, scanned PDF, photographed text, rotated image, mixed languages, damaged text layer, long document, cancel/resume, offline cached model, and foreground/background transitions. Record OS/browser/runtime/model versions, recognition accuracy, elapsed time, peak observable memory, failures, and output coverage. Simulator or desktop browser results cannot substitute for this matrix.

## Chapter navigation and audio

Use extracted heading evidence, bookmarks, and user corrections to define chapters before narration. Preserve source page/element anchors and distinguish exact reading from summarization. Keep one HTML audio element; attach metadata and supported play/pause/seek/previous/next handlers through Media Session. Optional actions need capability checks and exception handling. Start playback from an explicit user action and handle a rejected `play()` promise. Source: [Apple Media Session guidance](https://developer.apple.com/videos/play/wwdc2021/10189/).

Generate or cache seekable audio segments and prefetch conservatively, with an explicit budget and cancellation. Save chapter, segment, and playback position. Test lock screen, Bluetooth interruption, long pause, chapter boundary, network loss, tab discard, and resume on actual devices. Historical WebKit audio fixes demonstrate why API support is not a reliability guarantee; they do not prove that historical bugs still exist in current Safari. Source: [Safari 17.5 audio fixes](https://webkit.org/blog/15383/webkit-features-in-safari-17-5/).

### Future OpenAI voice integration

The speech documentation describes streamed audio and requires disclosure that the voice is AI-generated. Its legacy examples must be read alongside the newer deprecation schedule: on 2026-10-01 OpenAI announced removal on 2027-01-06 of `tts-1`, `tts-1-hd`, and the listed dated `gpt-4o-mini-tts` models, recommending `gpt-realtime-2.1-mini`. Avoid hard-wiring a future reader to a deprecated example. Sources: [speech guide](https://developers.openai.com/api/docs/guides/text-to-speech), [deprecations](https://developers.openai.com/api/docs/deprecations).

The reviewed `gpt-realtime-2.1-mini` page lists audio token prices of $10 input / $20 output per million and text token prices of $0.60 input / $2.40 output per million, before applicable cached-input rates. This does not establish a fixed cost per chapter or minute: measure actual token usage and multiply by the then-current rates. Bound prefetch spending, cache by text/model/voice/instructions identity, and show estimated versus actual cost. Sources: [model](https://developers.openai.com/api/docs/models/gpt-realtime-2.1-mini), [pricing](https://developers.openai.com/api/docs/pricing).

A standard API secret belongs on the trusted server. The documented browser Realtime flow uses a server-issued ephemeral client secret and WebRTC. A chapter reader does not require microphone access unless an interactive conversation feature is separately requested. Source: [Realtime WebRTC authentication](https://developers.openai.com/api/docs/guides/voice-webrtc).

Sign in with ChatGPT is now documented as a preview with a constrained model catalogue and Responses endpoint. Reviewed preview restrictions and inference documentation do not establish access to speech or Realtime through that entitlement. Recheck the exact supported auth flow before implementing; do not automate private ChatGPT endpoints or imply that consumer voice can be exported through them. Sources: [preview inference](https://developers.openai.com/siwc/token-sharing-open-source/models-and-inference), [preview limitations](https://developers.openai.com/siwc/token-sharing-open-source/preview-limitations).

## Header and section detection: reusable ideas

| Primary API | Useful evidence | Independent implementation idea |
| --- | --- | --- |
| Ghostscript `txtwrite` | Documented XML text output exposes Unicode, positions, and font information; formatting modes differ in block processing. | Use as an optional comparison oracle for positions/fonts, not as proof of semantic headings. |
| PyMuPDF text dictionaries | Blocks, lines, spans and characters expose bounding boxes, font sizes, flags, and directions. | Feed existing native extraction evidence into Rust heading scoring with explicit coordinate contracts. |
| PyMuPDF4LLM `IdentifyHeaders` / `TocHeaders` | The documented font-frequency heuristic and bookmark-based strategy are distinct approaches. | Combine relative font hierarchy with bookmark/text agreement; retain disagreement as uncertainty. |
| pdfplumber | Character/word font attributes, crop/filter, duplicate-character handling, and page-cache release are documented. | Preserve source-character association, normalize coordinates, detect repeated running heads across pages, and release page working data. |

Sources: [Ghostscript 10.05 text-device documentation](https://ghostscript.readthedocs.io/en/gs10.05.0/VectorDevices.html), [PyMuPDF TextPage](https://pymupdf.readthedocs.io/en/latest/textpage.html), [PyMuPDF4LLM API](https://pymupdf.readthedocs.io/en/latest/pymupdf4llm/api.html), [pdfplumber README](https://github.com/jsvine/pdfplumber/blob/stable/README.md).

These are API/algorithm ideas, not imported GPL source. A practical independent detector should consider numbering, relative font size/weight, whitespace, column position, repeated header/footer placement, and bookmarks. No single point-size threshold proves a heading. Test unnumbered chapters, misleading bookmarks, multiple columns, varying page sizes, CJK headings, subset font names, and OCR with unknown font information. Preserve DOI link targets, image/caption relations, and source anchors across chapter segmentation.

## No arbitrary document-size ceiling

Removing a file-size rejection does not make an existing full-buffer parser streaming. A browser `File.arrayBuffer()`, whole-response JSON, a full ZIP accumulator, and native adapters accepting one borrowed byte slice still have real memory costs. Surface an actual failure with recoverable progress; do not replace the removed cap with a hidden page or image count limit.

Cloudflare’s stable Workers limits page reviewed here states **128 MB memory per isolate, including WASM**, Free CPU of 10 ms, and Paid CPU defaulting to 30 seconds with a configurable five-minute maximum. Incoming request-body limits depend on the account plan: 100 MB for Free/Pro, 200 MB for Business, and Enterprise up to 5 GB through its documented controls. Responses have no enforced body-size ceiling; cache limits are a separate concern. These are hosting constraints, not permission to cap accepted document size. Source: [Workers limits](https://developers.cloudflare.com/workers/platform/limits/).

R2 documents a single-part maximum of 5 GiB and multipart objects up to 4.995 TiB, with at most 10,000 parts. Multipart lets a logical upload exceed the limit of one Worker request; individual requests still obey their route’s limits. The user’s 100 GB account allocation is an application/account quota, not an R2 per-document limit. Sources: [R2 limits](https://developers.cloudflare.com/r2/platform/limits/), [Worker multipart example](https://developers.cloudflare.com/r2/api/workers/workers-multipart-usage/).

Proposed large-document path:

- Stream upload parts into R2 without `request.arrayBuffer()`. Persist upload ID, ordered part numbers and verified completion evidence; support retry, resume and abort. Choose bounded working chunks and concurrency independently of total document size.
- Stream downloads and extraction artifacts. Produce page/entry records incrementally and stream archive output rather than assembling the entire archive in memory. Multipart ETags must not be represented as the original document’s SHA-256.
- Retain original bytes and source identity. PDF parsing often needs random access to cross-reference/object data; a range/file-backed parser is required before claiming truly bounded input memory. Do not call a full-buffer API “streaming” merely because transport arrived in chunks.
- Move long native parsing/OCR to a suitable worker service when explicitly selected; do not run a full heavyweight engine inside a 128 MB edge isolate by default. Persist a job/page ledger, enforce cancellation/deadline and operational memory controls, and report partial coverage honestly.
- Keep R2 operations/storage, compute, model downloads, and future narration costs visible. Account quota and metered cost controls are distinct from an arbitrary file or page ceiling. No automatic external document transfer is implied by this proposal.
