# Web extraction review and measured handoff

Review date: 2026-10-04. This review tests the HTML/RSS/Atom path, not the PDF engine.

## Decision and implementation

Keep Mozilla Readability as the default article scorer, with the repaired surrounding pipeline. The original alpha collected links, images, and tables from the entire page before scoring and then replaced article images with text placeholders. The repaired path selects and sanitizes article content first, derives its evidence from that selection, preserves real image elements and resolved lazy-image URLs, and reports incomplete static captures. It preserves the cleaned explicit main boundary when scoring would discard tables, code, or semantic footnotes. This broader fallback carries a warning because extra surrounding content can remain.

RSS and Atom are content sources, not decorative outbound links. Namespace-qualified full RSS content wins over a description; Atom text remains literal, HTML/XHTML remain sanitized fragments, inherited `xml:base` resolves links, and external Atom content retains an available summary plus its unresolved target. Feed-authored fragments bypass article scoring. No scripts, article links, or feed enclosures are fetched by the parser.

Independent regressions: `web/scripts/test-web-extraction-review.mjs` checks literal Atom markup/base resolution, RSS namespace collisions, external Atom content with a summary, an unrendered JavaScript shell, and lazy images with article-scoped evidence. It also checks all How-To Geek reference paragraphs/headings and Wikipedia table-cell preservation when the recorded fixtures are supplied.

## What the established tools actually do

- **Bear:** the developer's [Web Clipper 2.0 explanation](https://community.bear.app/t/web-clipper-2-0/17314) describes Readability operating on the currently displayed browser DOM; Bear then downloads images and converts the result to Markdown. That explains why a browser capture can include already loaded authenticated or asynchronous content that a fresh HTTP request lacks. Scroll-triggered content still needs to be loaded. We have not reproduced or claimed Bear's entire implementation.
- **Readability:** [official API and security guidance](https://github.com/mozilla/readability) says parsing mutates its DOM, supports explicit source URLs, and requires a separate sanitizer. Keep a clone and DOMPurify; do not enable page scripts in JSDOM. Its article score alone is not a completeness guarantee, especially for tables and documentation.
- **Defuddle:** [official source](https://github.com/kepano/defuddle) describes a browser-oriented alternative with metadata, footnote, math, and code normalization. Version 0.19.4 was tested outside production. Explicit `useAsync:false` avoids its optional remote fallbacks. Our JSDOM 30.1.1 experiment hit a selector-length error and returned a cluttered fallback; linkedom 0.18.12 completed. This compatibility failure must not be silently treated as a good extraction.
- **Trafilatura:** [2.3.0 extraction API](https://trafilatura.readthedocs.io/en/latest/corefunctions.html) offers precision/recall tradeoffs and explicit image/link/table retention. It was tested as an independent Python/lxml oracle, with comments disabled and links/images/tables enabled. It is not a drop-in browser/Workers dependency and is not installed in production by this work.
- **Feeds:** [RSS content guidance](https://www.rssboard.org/rss-profile#namespace-elements-content) distinguishes `content:encoded` full content from an ambiguous description. [Atom RFC 4287](https://www.rfc-editor.org/rfc/rfc4287) distinguishes text, escaped HTML, XHTML, external content, and summaries, and defines inherited `xml:base`. These distinctions are now regression inputs rather than interchangeable strings.

## Corpus, methodology, and reproducibility

Eleven benign public sources were fetched successfully with ordinary unauthenticated HTTP requests. The original How-To Geek request initially failed in a different retrieval path; a later HTTP 200 response supplied the actual 581,470-byte article. No substituted summary is used. Original pages are retained only in the validation workspace, not redistributed in this repository.

`WEB_EXTRACTION_BENCHMARK.json` records every source URL, redirect URL, byte length, SHA-256, and reference boundary. The harness rejects a changed fixture hash. It runs one cold parse plus four subsequent parses and reports the lower-middle warm timing. DOM construction is included; network, module loading, image downloads, and rendering are excluded. Numbers are Node 24.19.0/JSDOM 30.1.1 measurements on one shared machine, not browser responsiveness or server SLAs. Synthetic tests execute no page code or network requests.

Reference paragraph, heading, code, and table-cell matches are normalized-text substring checks inside a manually chosen source boundary. They measure retention, not semantic correctness, rendered visibility, universal precision, or complete web-page capture. In particular, metadata and widget paragraphs can legitimately disappear. The arXiv table count includes equation/layout tables, not only scientific data tables.

Run from `web`:

```sh
node scripts/test-web-extraction-review.mjs
node scripts/test-web-extraction-review.mjs --fixtures /absolute/path/to/validation/web-extraction
```

The optional run needs `fetch-results.json` and its original captured files. It writes `benchmark.json`. It does not refetch changing pages or install additional extractor dependencies.

| Source | Prose retained, baseline → fixed | Table-cell matches, baseline → fixed | Fixed images | Warm parse ms, baseline → fixed |
| --- | --- | --- | ---: | ---: |
| [How-To Geek review](https://www.howtogeek.com/lenovo-yoga-mini-gen-11-review/) | 24/24 → 24/24 | — | 30 | 187 → 274 |
| [Python tutorial](https://docs.python.org/3/tutorial/datastructures.html) | 78/79 → 79/79 | — | 0 | 162 → 617 |
| [MDN Array.map](https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/Array/map) | 31/34 → 34/34 | 2/2 → 2/2 | 0 | 106 → 172 |
| [Wikipedia comparison](https://en.wikipedia.org/wiki/Comparison_of_programming_languages) | 11/11 → 11/11 | 534/1374 → 1374/1374 | 1 | 334 → 1526 |
| [arXiv 2501.17300v2](https://arxiv.org/html/2501.17300v2) | 18/18 → 18/18 | 14/41 → 41/41 | 0 | 131 → 467 |
| [PMC10947171](https://pmc.ncbi.nlm.nih.gov/articles/PMC10947171/) | 55/57 → 55/57 | 20/20 → 20/20 | 6 | 160 → 522 |
| [NASA Webb landing page](https://science.nasa.gov/mission/webb/) | 45/55 → 45/55 | — | 26 | 248 → 375 |
| [Steph Ango blog](https://stephango.com/saw) | 5/5 → 5/5 | — | 0 | 13 → 26 |
| [NASA RSS](https://www.nasa.gov/news-release/feed/) | 10/10 entries | — | 53 | n/a → 383 |
| [Readability Atom](https://github.com/mozilla/readability/releases.atom) | 8/8 entries | — | 0 | n/a → 44 |
| [NASA image article](https://www.nasa.gov/image-article/nasas-chandra-finds-unusual-objects-in-pinwheel-galaxy/) | 5/5 → 5/5 | — | 1 | 85 → 96 |

Raw Readability is the baseline; the fixed pipeline includes cleanup, metadata/evidence extraction, sanitization, and Markdown generation, so their timings cover different work. Wikipedia now retains 9/9 source headings instead of 3/9; Python retains all 35 code blocks; MDN retains all 18. The MDN fallback restores its definition and also retains the compatibility banner and last-updated notice, an explicit cost of keeping the broader main boundary. The remaining PMC omissions are correspondence and issue-date metadata. These are not counted as missing body prose.

The original alpha produced 147 all-page links and 117 all-page image records for How-To Geek, while its article HTML contained zero images and 30 image placeholders. The repaired output has 8 selected-article links and 30 real image elements. Source fetches for the additional ten fixtures took 3.8–6.4 seconds each; they are excluded from the parser timings.

## Challenger check on the exact How-To Geek page

All three tested approaches retained the 24 authored paragraphs and 10 section headings. Media retention differed sharply:

| Extractor | Image elements | Distinct source pathnames | Observation |
| --- | ---: | ---: | --- |
| Repaired Readability pipeline | 30 | 26 | Product photos and benchmark charts survive; one small publisher logo remains. |
| Defuddle 0.19.4 + linkedom 0.18.12 | 43 | 12 | Repeated/resized image variants inflate the count; many review photographs disappear. |
| Trafilatura 2.3.0, images enabled | 4 | 2 | Product card/badge imagery survives, most review photos do not. |

Single cold probes were about 295 ms for Defuddle/linkedom and 40 ms for Trafilatura; these different runtime stacks are not an apples-to-apples speed ranking. Raw counts would misleadingly make Defuddle look best at retaining images. Neither challenger justifies a new production dependency on this evidence. The fixed pipeline also removes the checked preferred-source, account-signup, and review-policy clutter from the article text. The fixture still contains repeated gallery resolutions, and an image URL is not proof its bytes were downloaded.

## Remaining gaps and next bounded steps

1. **Dynamic pages:** fetching HTML does not execute JavaScript, expand interactive sections, traverse canvas state, or authenticate. The script-only regression intentionally returns no invented article and remains partial. A separate configured render worker is a future capability, not something this benchmark ran. [Cloudflare Browser Run `/content`](https://developers.cloudflare.com/browser-run/quick-actions/content-endpoint/) can return a rendered DOM; its docs also warn that initial page load may precede content readiness. A practical opt-in path would use an isolated browser job, explicit readiness selector, recorded final URL, deadline/cancellation, resource accounting, and the same downstream cleaner. [Playwright `page.content()`](https://playwright.dev/docs/api/class-page#page-content) supplies serialized DOM, not proof all scroll-triggered or cross-origin content was captured. No invisible browsing session or authentication reuse should be assumed.
2. **Media:** real image elements are retained, but asset-byte materialization, per-asset failure status, content-type verification, durable object storage, deduplication, and resumable large transfers belong to the separately implemented capture/storage path. Inline SVG, canvas, audio/video transcription, and OCR are not established by these tests. Do not label image placeholders or unresolved remote URLs as recovered media.
3. **Scale/performance:** largest tested HTML is 624,551 bytes. No 100 GB document, mixed-media archive, or hundred-gigabyte extraction was exercised. Removing arbitrary input caps does not make an in-memory DOM parser streaming. Large originals need resumable object storage and queued format-specific processing; large parse jobs need cancellation and progress. Keep fetch, parse, persistence, and media-download timings separate. Structured retention increases work because it retains substantial tables instead of dropping them.
4. **Coverage:** the NASA mission landing page is not a single article and loses some cards/index content. PMC's missing reference paragraphs were correspondence/issue-date metadata. Source boundaries and the warning on broader fallback remain visible; no blanket completeness claim follows from a high prose score. More formats require real adapters, not renamed generic uploads.

## Timestamped handoff

Recorded 2026-10-04 01:04:35 UTC / 2026-10-03 19:04:35 MDT (America/Denver); benchmark timestamps and source hash are recorded in the JSON.

- **Native engine epic:** earlier native engine work was delivered through the parent integration; [PR 173](https://github.com/benpshore/pdftextract/pull/173) was reported merged by the parent. Native default input-size rejection was removed in commit `dccbee9fabec3e5d3835fd845dcafc8ce80bee09`, with explicit `--max-bytes` still honored and a real PDF larger than 64 MiB tested. This document does not rerun or expand native claims.
- **Web extraction epic:** current work is tracked in [PR 175](https://github.com/benpshore/pdftextract/pull/175). Implemented by the HTML owner: clean article evidence, actual image elements, feed semantics, and structured-content recovery. Independently tested here: five targeted cases plus eleven HTTP fixtures, with quantified losses and timings. These are local source tests, not proof the production deployment includes this exact source hash.
- **Storage/presentation epic:** parent and UI agents own durable large-file upload, actual asset materialization, and presentation. This review supplies extraction evidence; it does not certify their deployment or a 100 GB transfer.
- **Explicitly unproven:** live browser render worker, voice/audio transcription, OCR of web imagery, universal format support, and a native extraction backend deployed behind the Sites UI. Keep these as gaps until an actual configured runtime and end-to-end test demonstrate them.
