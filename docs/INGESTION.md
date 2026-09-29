# Native mixed-document ingestion

`tpe ingest` extracts format-specific evidence as JSONL. It does not run
scholarly bibliography cleanup over arbitrary documents. This is an additional
command; `extract`, its ledger, and its output schema retain their behavior.

```sh
cargo build --release --features formats
tpe ingest paper.pdf report.docx workbook.xlsx page.html notes.txt > records.jsonl
```

`formats` uses Docling **Rust** 1.74.1 with default features off. It adds no
Python runtime, PDFium, ONNX, OCR/ASR models, browser, or HTTP image fetching.
The default build supports PDF and UTF-8 text; Office/HTML needs `formats`.

For a password-protected PDF, provide an already-set environment variable:

```sh
tpe ingest --password-env TPE_PDF_PASSWORD locked.pdf > records.jsonl
```

The CLI passes its value directly to the PDF backend without putting the secret
in command arguments, output, policy or policy digest. The same credential
applies to all PDFs in that invocation; non-PDF inputs ignore it. A missing or
non-UTF-8 credential variable fails before ingestion. Wrong credentials produce
a normal failed document record. Library callers can use
`ingest::run_with_password(path, options, Some(password))`; credentials are
deliberately separate from serializable `Options`. The batch supervisor does
not yet forward credentials; use this command for encrypted inputs.

## Routing and fidelity

| Input | Extractor | Stored evidence | Limitations |
| --- | --- | --- | --- |
| Digital PDF | Existing lopdf backend | Positioned spans, ordered lines, pages, PDF Info | No header removal, dehyphenation, inferred citations or OCR. Existing font/reading-order limitations remain. |
| UTF-8 text | Strict decoder | Exact text, including whitespace and line endings | Invalid UTF-8 and NUL-containing inputs fail; no encoding guess. |
| DOCX | Docling declarative backend plus TPE supplemental evidence | Docling JSON: paragraphs, lists, tables, links and recognized nodes; exact UTF-8 XML and SHA-256 for known note/header/footer/comment parts | Side parts trigger `review_required`; preserved XML has no inferred reading order. Pictures require review/OCR. Not a Word renderer or lossless OOXML round trip. |
| Saved HTML | Docling declarative backend | Native Docling JSON, including text, links and tables | UTF-8 only. No scripts, CSS layout, browser rendering, URL fetching, or remote images. JavaScript-loaded content may be absent from the snapshot. |
| Markdown | Docling declarative backend | Native Docling JSON | UTF-8 only. |
| CSV | Rust csv 1.4.0 with explicit comma dialect | Rows of string cells, recorded dialect, exact UTF-8 `source_text` | No header inference, delimiter guessing, numeric conversion or rectangular padding. Empty lines are omitted from parsed rows but remain in `source_text`; ambiguous quotes fail. |
| XLSX | Calamine 0.36.1 sparse reader | Sheet names/order/visibility, cell coordinates, typed values, formula metadata, merged ranges | Includes hidden sheets. No formula evaluation; caches may be stale. Charts, macros and display formatting are outside scope. |
| Scanned PDF / image | Routing only | Available PDF text, candidate page numbers and detection evidence | `needs_ocr` for empty text or sparse text with a dominant raster. This heuristic can miss scans; blank pages and captioned photographs can trigger it. |
| Audio | Routing only | Source identity | `needs_transcription`; no ASR has been invoked. |
| Other | None | Source identity where readable | Explicit `unsupported`. |

Non-worksheet Excel sheets retain their name, type and visibility (`visible`,
`hidden` or `very_hidden`). Their `cells` is null and the record requires review;
this preserves metadata without claiming that charts were extracted.

PDF header lines precede extension detection. A header must have the form
`%PDF-1.x` or `%PDF-2.x` followed by a line ending, at the start or after a short
whitespace/binary prefix. Inline mentions in prose do not route to PDF. Textual
preambles and malformed headers are not supported by this conservative detector.
Office containers are identified by
internal roots, so extensionless downloads work. Packages containing both Word
and Excel roots fail. Media extensions are routing hints; an OCR/ASR adapter
must validate the actual stream when decoding it.

### Why Excel has its own adapter

The Docling 1.69.2 audit found that `backend/xlsx.rs` builds dense used-area rectangles.
Its default limit is 10 million cells. Oversized or unreadable sheets can be
skipped while conversion continues. Document tables omit the formula metadata
required by this ingestion contract.

The adapter uses Calamine's `next_cell_with_formula_metadata`, keeping stored
cells. A1 and XFD1048576 do not allocate a 17-billion-cell rectangle. Shared
formulas remain an anchor plus indexed derived cells, including when an
anchor appears later in XML order. Normal/array formula text is retained,
but array/spill relationships are not fully represented by the upstream API.
Numbers use binary64; original XML decimal spellings and rich-text formatting
remain in the source file. These are implementation findings, not an overall
accuracy/speed ranking against Docling.

Excel serial dates contain a calendar value, not a timezone-qualified instant.
The adapter retains the serial and decoded calendar with **`timezone: null`**.
It does not guess the researcher's, server's or host's timezone. UTC conversion
requires an explicitly supplied source timezone and a policy for ambiguous or
nonexistent daylight-saving times. Duration cells remain durations, not dates.

### Word supplemental evidence and CSV dialect

The Docling 1.69.2 audit found missing Word footnotes/endnotes and even-page
headers in its document JSON. Version 1.74.1 now includes a footnote/endnote
conversion pass; supplemental evidence remains available independently of
the converter's coverage. A successful conversion status does not
certify that these parts were consumed. TPE preserves `word/footnotes.xml`,
`word/endnotes.xml`, and `word/header*.xml`, `word/footer*.xml` and
`word/comments*.xml` in **`content.tpe_supplemental_parts`**, a TPE extension to
Docling JSON. Each entry contains the ZIP member `path`, exact UTF-8 `xml`, and
`sha256` of the original member bytes. Associated relationship parts, the main
document and its relationships, and settings are retained when present so
anchors and targets remain inspectable. A non-UTF-8 supplemental part fails
explicitly rather than being decoded lossily.

Presence of these parts makes the result `review_required`, including headers
that the converter already extracted. This deliberately conservative status
does not claim integrated note order or complete Word coverage. Other unusual
Word structures can still be unsupported; the original package remains the
archival source.

Docling's delimiter sniffing counts punctuation inside quoted CSV headers; a
valid comma-separated header such as `"Name; aliases; initials",Count` can be
misclassified and rejected. The CSV adapter therefore uses an explicit comma
delimiter, double-quote escaping, and `has_headers=false`. Semicolons and tabs
are ordinary field content. Ragged rows, quoted newlines and trailing empty
cells are retained without padding. `max_cells` applies to the whole CSV.
Original whitespace, blank lines and line endings remain in `source_text`.

### PDF and HTML policies

Scholarly cleanup removes recurring headers, page numbers, script fragments
and line-end hyphens. Those operations can erase general-document content.
`ingest` retains spans and ordered text without those transformations. The
lopdf backend still performs its documented Unicode/ligature normalization.
This does not fix every decoding defect in the independent bibliography audit.
Backend warnings other than informational ligature expansion require review,
including missing-font fallbacks and skipped or undecodable form content.

An empty-text PDF page is an OCR candidate. A page containing at most 80
non-whitespace characters and a single raster covering at least 65% of its
page rectangle is also a candidate: a digital page number must not disguise a
scanned body. Raster bounds are clipped to the page. `ocr_evidence` records the
reason and the dominant raster fraction for this case. This is a routing
heuristic, not image recognition. It can flag photographs with short captions
and miss tiled scans or incomplete OCR layers containing more text. It does
not run OCR or establish transcription accuracy.

The audited Docling HTML path can prioritize a declared legacy charset even when
`SourceDocument.encoding` is set. After strict UTF-8 validation, the adapter
supplies a UTF-8 BOM to that converter. The source hash covers the original
bytes. A fixture checks an outdated windows-1252 declaration with `Café α₂`.

## Output and failure semantics

One `tpe.ingest.v1` object is written per input: `path`, source observation,
`sha256`, detected `format`, `outcome`, extractor identity, public `policy` and
its `policy_digest`, native `content`, warnings and `elapsed_ms`. Policy includes
the adapter revision, compiled `formats` flag, limits and CSV dialect. Timings cover
reading, hashing and extraction, excluding startup and JSON serialization.
No database is created. Retain originals by SHA-256; JSON is not an archival
replacement.

`extracted` means the operation returned successfully, **not** that every
visible element was verified. `review_required`, `needs_ocr`,
`needs_transcription`, `unsupported` and `failed` are distinct non-success
outcomes. Failed conversions have null content. Subsequent inputs are still
attempted; any non-success makes the command exit nonzero. Missing source
identities remain null, never guessed.

With `formats`, policy also records the effective numeric allowlisted settings
`DOCLING_RS_MAX_XML_DEPTH` (default 512), `DOCLING_RS_MAX_HTML_DEPTH` (2000), and
`DOCLING_RS_MAX_PART_BYTES` (536,870,912). Parsing matches the pinned upstream:
surrounding whitespace is ignored; invalid numbers use the default; XML depth
zero also uses its default. No unrelated environment variables or invalid raw
values are exported. Set these variables before using the parsers and leave
them fixed for the process lifetime: upstream caches the XML cap on first use.
TPE snapshots these effective settings and rejects later changes instead of
attributing different limits to the same policy. Embedding applications must
also set them before calling Docling directly.

## Resource bounds

| CLI flag | Default | Per-input bound |
| --- | ---: | --- |
| `--max-bytes` | 67,108,864 | Source bytes |
| `--max-expanded-bytes` | 268,435,456 | Office ZIP expansion |
| `--max-archive-entries` | 10,000 | Office ZIP members |
| `--max-cells` | 250,000 | Parsed CSV cells or stored cells across all XLSX sheets |
| `--max-pages` | 10,000 | PDF pages |

Files use one descriptor with before/after metadata checks and a read bound
even if the file grows. Archives are checked before conversion for declared
and actual expansion, CRCs and duplicate directory entries. Cell-reader errors
or exceeded workbook limits fail the file instead of dropping a sheet.

The command processes files sequentially and flushes each record. Previous
document content is not retained. These bounds are **not** hard memory or CPU
limits: object inflation, DOMs, decoded images and JSON can occupy more memory.
A streaming job source, bounded concurrency and process isolation for expensive
inputs are separate changes.

## Validation and release criteria

`tests/ingest.rs` checks authored expectations for initials, Unicode, Word
table cells, HTML links, CSV multiline fields, sparse Excel coordinates,
normal/shared formulas (including a derived cell before its anchor in XML),
empty caches, errors, hidden sheets, merged ranges, format spoofing, scans with
digital page numbers, omitted Word side parts with exact checksums, CSV quoted
headers and ragged rows, parser-limit provenance, duplicate ZIP entries and
visible limit failures.
Encrypted-PDF checks cover absent, incorrect and correct credentials through
both the library and CLI, source hashes, and exclusion of secrets from records
and policy digests. Chart-sheet checks cover all three visibility states.
The duplicate-member regression deliberately changes the two equal-length ZIP
filenames in local and central headers because the normal ZIP writer refuses
to create duplicates. The test asserts exactly two replacements; member data,
lengths and CRCs stay unchanged. This corruption is confined to the test fixture.
It verifies that a failed first CLI input does not block the next. Existing PDF
tests still run. CI tests `formats` on Linux x86-64, Linux ARM64 and macOS ARM64.

Fixtures establish regression contracts, not arbitrary-corpus accuracy or
million-document throughput. Production acceptance still requires independent
real-file truth, exact text/order, cell/formula and missing/extra-content checks,
OCR character error, ASR word error, tail latency, peak memory, and failure
rates. No new corpus accuracy percentage or speed ranking is claimed.

OCR and ASR are pending adapters requiring their own models, budgets and ground
truth. Successful extraction is not verified publication-grade fidelity.
