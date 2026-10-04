# Saved-document MCP

The Site exposes a stateless, read-only `POST /mcp` endpoint implementing MCP
`2025-11-25`: `initialize`, `ping`, `tools/list`, `tools/call`, and accepted
initialization/cancellation notifications. GET/DELETE return 405; no SSE stream,
session cookie, background task, sampling or custom authentication is added.
This is an explicitly supported protocol revision, not a claim to implement the
newer 2026 transport. Initialization negotiates the implemented revision; an
unsupported protocol-version header returns 400.

Discovery contains static tool definitions only. Data calls use the existing
Sites-authenticated `owner(request)` helper. The server does not derive identity
from tool arguments or custom headers. Every D1 query includes the owner, and an
owned record is required before R2 is read. Missing and another owner's document
both return 404. Sites owns OAuth and trusted identity headers at the hosting
boundary. Responses are private/no-store and an invalid Origin is rejected.

- `search_documents`: owner-only title/indexed-text search, stable keyset
  pagination, bounded source metadata and excerpts. It describes the indexed
  search scope; it does not claim every saved byte is indexed.
- `get_document`: reads text, markdown, links, warnings, outline, metadata,
  pages, tables or feed entries. Saved JSON is streamed, including when the
  requested section occurs after a large text field. Only the requested window
  is retained in memory. Text strings are decoded; structured sections are
  exact JSON fragments that can be concatenated. Offsets count UTF-16 code
  units, and continuation cursors are tied to the saved extraction revision.
  A missing outline is reported as unavailable; no outline is fabricated.
- `get_document_image`: only explicitly requested original image documents can
  produce MCP image content. Original PNG/JPEG/WebP signatures and stored MIME
  must agree. Other or large originals remain available through an authenticated
  original-download path. This does not render PDF pages, fetch external images,
  or trigger OCR.

There is no MCP document-size admission limit. A 64 KiB request-envelope budget,
50 search rows, 32,000 UTF-16 units per document response and 2 MiB inline image
budget are protocol/context windows, not rejection limits on saved documents.
Image overflow returns metadata successfully. Larger text/evidence is read by
following cursors. The underlying storage/worker still has its platform limits.

Tool results label all saved content as untrusted source evidence and include
the saved source hash, extraction engine and status. Reading a document does not
change Partial/Failed to Ready. No operation imports, writes, deletes, follows a
link, or automatically transmits source content during an import. Private data
is returned only in response to an authenticated, explicit tool call.

Protocol sources reviewed:

- https://modelcontextprotocol.io/specification/2025-11-25/basic/transports
- https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle
- https://modelcontextprotocol.io/specification/2025-11-25/server/tools

Run the dependency-free mocked auth/storage contract checks with Node 22.13+
(Node 24 was used for local verification):

```sh
node --experimental-strip-types scripts/test-site-mcp.mjs
```

The tests never read a private user document or contact an external service.

The adjacent upload publisher also has a disposable actual Workers binding
test. With the web dependencies installed, from the repository root run:

```sh
node --experimental-strip-types scripts/test-site-uploads-workers.mjs
```

It resolves Miniflare through the installed Wrangler dependency and creates only
temporary fixture R2/D1 storage. It verifies multipart completion, streaming
SHA-256, owner checks, missing-receipt recovery, and D1 compare-and-swap rejection
of a stale publisher. The test-only pause interleaves two publishers inside one
Workers request context; it changes no production compatibility flags. From
`web/`, both test paths can be invoked as `../scripts/<test filename>`.
