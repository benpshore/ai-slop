# Explicit GROBID scholarly structure module

`tpe grobid FILE` is a separately selected server client, available with the
`grobid` Cargo feature. It is not an automatic fallback or an extraction backend.
It sends the selected PDF only to the endpoint you configure. Ordinary `extract`,
`bibliography`, and `backends` commands never invoke it.

The latest stable upstream release verified on 2026-10-04 is **GROBID 0.9.1**,
published 2026-08-04. Official sources:

- [Release 0.9.1](https://github.com/grobidOrg/grobid/releases/tag/0.9.1)
- [REST API](https://grobid.readthedocs.io/en/latest/Grobid-service/)
- [PDF coordinates](https://grobid.readthedocs.io/en/latest/Coordinates-in-PDF/)
- [Deployment and models](https://grobid.readthedocs.io/en/latest/Grobid-docker/)

The Rust client implements the documented multipart `processFulltextDocument`
API and checks `/api/version` before sending the PDF. The actual version/revision
response is retained in its result; the client does not claim a server runs
0.9.1 merely because that is the documented integration target.

## Configuration and use

```sh
cargo build --locked --release --features grobid
export TPE_GROBID_URL=http://127.0.0.1:8070
./target/release/tpe grobid paper.pdf > paper.grobid.json
```

There is no default endpoint. This command is explicit authorization to upload
that PDF to the configured service. Use HTTPS for a remote server. A protected
reverse proxy can use `TPE_GROBID_BEARER_TOKEN`; credentials are read only inside
the worker, never serialized into request files, command arguments, configuration
digests, or results. URLs containing user information, query parameters, or
fragments are rejected. Redirects, implicit environment proxies, and retries are
disabled, so a server cannot redirect a document or credential to another host.

No server, container, JVM, GPU, model, or hosted account is installed, started, or
billed by this module. GROBID is a separately operated Apache-2.0 service with
Java, PDFalto, and trained model dependencies; consult the upstream deployment
documentation and the licenses of the chosen runtime/model distribution. CRF and
full model deployments have different resource and quality profiles. A running
server was not provisioned for this change: tests use a local contract server,
so model accuracy and live 0.9.1 throughput are **not measured** here.

## Scholarly evidence

The JSON contains the exact UTF-8 TEI, its SHA-256, input SHA-256, endpoint,
reported server version, effective options, page surfaces, scholarly elements,
and reference records. Each element and reference carries a byte range into the
original TEI. Namespaced attributes are retained, including the server's
consolidation and source annotations. Text, DOI identifiers, reference URLs, raw
citation notes, and native coordinates stay separately inspectable.

Coordinates retain GROBID's original upper-left page-point convention. A
coordinate contains page, x, y, width, and height. A paragraph or reference can
have multiple boxes. Missing page surfaces do not acquire guessed dimensions.
URI targets are server-provided TEI evidence, distinct from visible label text;
they are not asserted to be byte-identical PDF annotations. The Poppler/MuPDF
adapters expose direct annotation evidence for that separate purpose.

GROBID adds learned scholarly segmentation, citation fields, affiliation/header
structure, and citation linking. It is not OCR and is not an exhaustive glyph
extractor. `coverage: "semantic_projection"` and a warning make that distinction
explicit; HTTP 200 and valid TEI never imply complete PDF text coverage. Raw TEI
remains authoritative when a convenience projection omits an uncommon element.

All three consolidation parameters are explicitly `0` by default, overriding
the server API's header-consolidation default. `--consolidation 1` explicitly
authorizes the server to enrich metadata through its configured Crossref or
biblio-glutton service; `--consolidation 2` requests DOI-only enrichment. Those
extra lookups are performed by the server and retain their TEI provenance.

## Limits and failure behavior

There are no default PDF, TEI, projected-text, or output-capture byte caps.
`--max-input-bytes` and `--max-response-bytes` apply only when explicitly supplied;
they have no policy ceiling. The PDF is streamed in 64 KiB chunks into a private
temporary file while hashing, then that immutable snapshot is streamed through
multipart upload. The whole PDF is never placed in a new heap buffer. Temporary
storage must have room for the selected document; real filesystem failures remain
errors, and snapshots are removed on close or worker termination.

Defaults remain one document/request at a time, 60 seconds total CLI time, and
512 MiB worker address-space growth. `--timeout-ms` (up to 300000) and
`--max-memory-growth-mib` (32 through 4096) configure execution budgets. These
contain runaway work without rejecting a file merely for its byte size.

XML processing rejects DTDs and external entities. The full TEI tree and its
projection use memory inside the supervised worker; there is no default document
node, depth, or projected-text cap. An explicit response limit also limits
projected text. Invalid namespaces,
coordinates, duplicate surfaces, malformed UTF-8/XML, and oversized responses
fail explicitly. HTTP 204 is no structured content, 503 is a busy server, and
other non-200 responses are failures. No response body is copied into error logs.

The existing CLI worker supervises local memory, deadline, cancellation, and
kill/reap behavior. It captures a complete successful JSON result before bounded
stdout delivery. Failed processing yields a nonzero exit with no partial JSON.
Stopping a local request cannot guarantee cancellation of work already accepted
by the remote server; configure independent server limits as well. Direct Rust
library callers get request deadlines and any explicitly selected byte limits,
but must supply their own process boundary if they require hard local memory or
cancellation guarantees. The server version response is separately capped at
4 KiB, endpoint URLs at 4 KiB, and bearer tokens at 8 KiB; these are protocol
identity/credential checks and do not constrain PDF or TEI size.

`cargo test --features grobid --test grobid_contract` covers the real HTTP client
against loopback fixtures: transmitted PDF/parameters, disabled consolidation,
TEI/citation/URI evidence, version checks, no redirects/retries, byte/time limits,
XXE refusal, malformed coordinates, supervised CLI success/failure without
source modification or credential leakage, and a streamed 65 MiB input with a
TEI response exceeding 16 MiB. These are contract tests, not claims
about statistical model accuracy.
