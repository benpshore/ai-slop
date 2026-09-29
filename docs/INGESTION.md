# Native mixed-document ingestion

`tpe ingest` extracts format-specific evidence as JSONL. It does not run
scholarly bibliography cleanup over arbitrary documents. This is an additional
command; `extract`, its ledger, and its output schema retain their behavior.

```sh
cargo build --release --features formats
tpe ingest paper.pdf report.docx workbook.xlsx page.html notes.txt > records.jsonl
```

`formats` uses Docling **Rust** 1.69.2 with default features off. It adds no
Python runtime, PDFium, ONNX, OCR/ASR models, browser, or HTTP image fetching.
The default build supports PDF and UTF-8 text; Office/HTML needs `formats`.

## Routing and fidelity

| Input | Extractor | Stored evidence | Limitations |
| --- | --- | --- | --- |
| Digital PDF | Existing lopdf backend | Positioned spans, ordered lines, pages, PDF Info | No header removal, dehyphenation, inferred citations or OCR. Existing font/reading-order limitations remain. |
| UTF-8 text | Strict decoder | Exact text, including whitespace and line endings | Invalid UTF-8 and NUL-containing inputs fail; no encoding guess. |
| DOCX | Docling declarative backend | Native Docling JSON: paragraphs, lists, tables, links and recognized nodes | Content extraction, not a Word renderer or lossless OOXML round trip. Pictures require review/OCR. |
| Saved HTML | Docling declarative backend | Native Docling JSON, including text, links and tables | UTF-8 only. No scripts, CSS layout, browser rendering, URL fetching, or remote images. JavaScript-loaded content may be absent from the snapshot. |
| Markdown / CSV | Docling declarative backend | Native Docling JSON | UTF-8 only. CSV quotes and multiline fields are tested. |
| XLSX | Calamine 0.36.1 sparse reader | Sheet names/order/visibility, cell coordinates, typed values, formula metadata, merged ranges | Includes hidden sheets. No formula evaluation; caches may be stale. Charts, macros and display formatting are outside scope. |
| Scanned PDF / image | Routing only | Available PDF text and candidate page numbers | `needs_ocr`, never a successful empty transcription. Blank pages can also trigger this conservative result. |
| Audio | Routing only | Source identity | `needs_transcription`; no ASR has been invoked. |
| Other | None | Source identity where readable | Explicit `unsupported`. |

PDF magic precedes extension detection. Office containers are identified by
internal roots, so extensionless downloads work. Packages containing both Word
and Excel roots fail. Media extensions are routing hints; an OCR/ASR adapter
must validate the actual stream when decoding it.

### Why Excel has its own adapter

Pinned Docling 1.69.2's `backend/xlsx.rs` builds dense used-area rectangles.
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

### PDF and HTML policies

Scholarly cleanup removes recurring headers, page numbers, script fragments
and line-end hyphens. Those operations can erase general-document content.
`ingest` retains spans and ordered text without those transformations. The
lopdf backend still performs its documented Unicode/ligature normalization.
This does not fix every decoding defect in the independent bibliography audit.

Docling 1.69.2's HTML path can prioritize a declared legacy charset even when
`SourceDocument.encoding` is set. After strict UTF-8 validation, the adapter
supplies a UTF-8 BOM to that converter. The source hash covers the original
bytes. A fixture checks an outdated windows-1252 declaration with `Café α₂`.

## Output and failure semantics

One `tpe.ingest.v1` object is written per input: `path`, source observation,
`sha256`, detected `format`, `outcome`, extractor identity, `policy_digest`
(including limits), native `content`, warnings and `elapsed_ms`. Timings cover
reading, hashing and extraction, excluding startup and JSON serialization.
No database is created. Retain originals by SHA-256; JSON is not an archival
replacement.

`extracted` means the operation returned successfully, **not** that every
visible element was verified. `review_required`, `needs_ocr`,
`needs_transcription`, `unsupported` and `failed` are distinct non-success
outcomes. Failed conversions have null content. Subsequent inputs are still
attempted; any non-success makes the command exit nonzero. Missing source
identities remain null, never guessed.

## Resource bounds

| CLI flag | Default | Per-input bound |
| --- | ---: | --- |
| `--max-bytes` | 67,108,864 | Source bytes |
| `--max-expanded-bytes` | 268,435,456 | Office ZIP expansion |
| `--max-archive-entries` | 10,000 | Office ZIP members |
| `--max-cells` | 250,000 | Stored cells across all XLSX sheets |
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
normal/shared formulas, empty caches, errors, hidden sheets, merged ranges,
format spoofing, scans, duplicate ZIP entries and visible limit failures.
It verifies that a failed first CLI input does not block the next. Existing PDF
tests still run. CI tests `formats` on Linux x86-64, Linux ARM64 and macOS ARM64.

Fixtures establish regression contracts, not arbitrary-corpus accuracy or
million-document throughput. Production acceptance still requires independent
real-file truth, exact text/order, cell/formula and missing/extra-content checks,
OCR character error, ASR word error, tail latency, peak memory, and failure
rates. No new corpus accuracy percentage or speed ranking is claimed.

OCR and ASR are pending adapters requiring their own models, budgets and ground
truth. Successful extraction is not verified publication-grade fidelity.
