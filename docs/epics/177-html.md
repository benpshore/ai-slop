# [Web app][Epic] Clean HTML/RSS/Atom extraction, actual images, and rendered-page coverage

GitHub: https://github.com/benpshore/pdftextract/issues/177
Created: 2026-10-04T01:11:36Z
Status at export: open

Recorded 2026-10-04T01:09:25Z (2026-10-03 19:09:25 America/Denver). This epic is part of the user's requested Dot/Codex handoff for **benpshore/pdftextract**. Implementation is tracked in [PR #175](https://github.com/benpshore/pdftextract/pull/175); the core repair baseline is [merged PR #173](https://github.com/benpshore/pdftextract/pull/173), main commit e27e1fb28a5b40dbca7f517c6fe396e6f11ed4ec.

Current distinction: PR #175's published head was 77c58ec89a1c9c07425b77dec90ab00316aa07be at this record's start; follow-up fixes are being validated for the next revision. The live Site's first alpha uses upstream PDF Oxide WASM, not the complete native Rust engine. See the final handoff/source manifest for the exact subsequent publication revision.

## User-reported defects
At 2026-10-04T00:45:46Z, the user reported embedded blobs/garbled content on https://www.howtogeek.com/lenovo-yoga-mini-gen-11-review/. At 00:47:32Z they rejected reference hyperlinks as a substitute for learning from Bear/RSS. HTML, CSS, and PHP-served HTML must never go through PDF engines.

## Implemented fixes and evidence
- `web/lib/clip.ts` cleans hidden/script/style/template/hydration noise before article-scoped extraction; preserves Unicode, real DOI targets, headings, captions, lazy image sources, tables, and code. A cleaned semantic boundary preserves structures Readability drops.
- No former 4 MiB, 500-feed-entry, or collection truncation caps. RSS full-content namespaces and Atom text/HTML/XHTML/base/external-content-summary semantics have dedicated regressions.
- `source-fetch.ts` captures public sources without executing scripts; `decodeSource` honors BOM/charset/meta encoding. Original bytes remain separate from decoded analysis.
- `article-assets.ts` and owner-protected asset endpoints retain actual image bytes and render private URLs; no image-placeholder implementation is claimed as extraction.
- Bear's documented live-DOM Readability approach was researched. Readability, Defuddle and Trafilatura were compared on the real example; no dependency was selected solely for word counts.
- `WEB_EXTRACTION_BENCHMARK.json` / `WEB_EXTRACTION_REVIEW.md`: 11 benign sources, URL/hash provenance, separate fetch/parse timings. HTG retains 24/24 checked authored paragraphs and 10/10 section headings; Wikipedia cells improve 534/1374→1374/1374; remaining NASA 45/55 paragraphs and PMC55/57 are explicit.

## Remaining work / acceptance
- [ ] Add a separately authorized rendered-DOM acquisition path for JavaScript-only content, canvas charts, authenticated pages and browser-only blob URLs. Static source capture must not pretend it captured these.
- [ ] Measure end-to-end time including retained-image fetch/storage on real iPhone/iPad browsers; warm Node parsing is not an end-user latency claim.
- [ ] Make failed images distinguishable without inserting fake content; preserve existing saved results on re-extraction failures.
- [ ] Expand independent fixtures for documentation, scientific mathematics, galleries, live data, encodings and adversarial-but-benign structural edge cases.

Native PDF engine work does not close this epic.
