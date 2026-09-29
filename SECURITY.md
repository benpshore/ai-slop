# Security Policy

Please report vulnerabilities privately via
**Security → Report a vulnerability** on this repository (GitHub private vulnerability reporting).
Do not open public issues for security problems.

## Untrusted-document limits

PDFs are hostile input. The CLI rejects files larger than 256 MiB by default
(`--max-bytes` may select a lower or higher explicit ceiling), limits a batch
to 64 extraction workers, and catches a backend panic per document. The
pure-Rust backend additionally limits each decompressed page or Form content
stream to 64 MiB, limits retained operators and operands, bounds recursive
Form traversal, and caps the number of figures. These are denial-of-service
guards, not a sandbox: native PDFium and ONNX code still runs in-process.

The quadratic vector-figure clustering pass receives at most 2,000 boxes.
Rules and raster objects, which do not enter that pass, are capped separately
so a stream made entirely of those objects cannot consume memory without
bound. Batch worker counts are capped independently of the number of input
paths to prevent thread-exhaustion ("fork bomb") behavior.

## docling.rs audit scope

The `docling` feature exposes only `docling-pdf`'s PDF page-count, text-layer,
and PDF pipeline APIs. It does **not** route input to docling.rs's generic
format dispatcher, XML/DocLang/JATS readers, archive readers, or remote-image
fetching, so XML entity expansion, external entities, deeply nested XML, and
archive traversal are not reachable through this application. Keep this API
allowlist when upgrading docling.rs; enabling its generic converter requires a
new security review and explicit XML/archive limits.

PDF picture nodes are treated as opaque output. Their bytes never enter text,
are never decoded again by this project, and retained picture payloads are
capped at 256 MiB per document. The cap is cumulative and metadata remains
available when bytes are discarded. docling's own PDF parsing, rendering and
image creation happen before this output cap, so deployments processing
untrusted files should additionally isolate workers and enforce OS memory and
execution-time limits.
