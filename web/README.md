# TPE private alpha

Private, owner-authenticated document workspace. PDF files run through the published `pdf-oxide-wasm` 0.3.77 Rust/WASM package in a disposable browser worker. Public HTML is fetched without executing scripts, then clipped using Mozilla Readability, DOMPurify and GFM conversion. RSS/Atom entries retain identifiers, dates, article links and enclosure links.

Original bytes and extraction JSON are separate objects in private managed R2 storage. D1 stores owner-scoped document metadata and searchable text. Every document API authenticates the user and checks ownership; originals download as attachments. This storage is separate from ChatGPT Library.

## Limits and evidence

- Uploads: 8 MiB; public HTML/feed capture: 4 MiB; PDF: 150 pages and a 60-second worker deadline.
- Results are explicitly marked for review. Browser extraction does not certify Unicode completeness and does not run OCR or the native fallback engines.
- PDF annotations retain URI targets and rectangles, independently of printed DOI candidates. DOI candidates are not registry-verified.
- HTML preserves links, image source metadata, tables and JSON-LD. It does not retain image binaries, execute JavaScript, reproduce canvas charts, or fetch authenticated resources. Saved HTML snapshots can be imported.
- Feed reads are manual; article pages and enclosures are not fetched automatically.
- Native TPE JSON can be imported as unverified evidence. The native engine has not been ported wholesale to WASM.

## Next: Ask ChatGPT

Add a selected-document handoff containing readable text, extracted image assets, source provenance, a manifest and a prepared prompt. A `.tar.gz` archive is a useful export format; automatic attachment to a new ChatGPT conversation is not currently implemented or assumed supported. A Site-hosted MCP capability is the preferred later path for authenticated retrieval of selected documents by ChatGPT. ChatGPT Library is not used as an arbitrary application database.

## Development

Use the committed pnpm lockfile. Run `node scripts/copy-pdf-wasm.mjs` before building to copy the pinned package assets. Type-check with `node node_modules/typescript/bin/tsc --noEmit`. D1 migrations live in `drizzle/` and are applied by Sites deployment.

Design references: [Paperless-ngx](https://docs.paperless-ngx.com/), [Bear Web Clipper](https://bear.app/faq/browser-extensions/), [Mozilla Readability](https://github.com/mozilla/readability), [RSS 2.0](https://www.rssboard.org/rss-specification), [Atom](https://www.rfc-editor.org/rfc/rfc4287).
