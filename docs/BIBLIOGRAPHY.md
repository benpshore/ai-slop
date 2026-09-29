# Backward bibliography extraction

`tpe bibliography` extracts the final detected reference list without running
the full-document metadata or in-text citation stages. The default backend is
the existing pure-Rust `lopdf` implementation. The command reads PDF pages
from last to first until it finds the beginning of a qualifying bibliography,
then emits its entries in printed order.

```sh
tpe bibliography paper.pdf another.pdf > bibliographies.jsonl
```

Standard output contains **one JSON object per input PDF**, including its
source path, SHA-256 of the input bytes, backend identity, total and scanned
page counts, the detected boundary, and the complete `ReferenceEntry` objects.
Each entry retains `raw`, printed `label`, starting `page`, and best-effort
parsed fields such as `authors`, `title`, `year`, and `doi`. The command writes
no SQLite ledger or document files; a caller can import the JSONL into its own
store. `tpe extract` remains the full-document, ledger-backed command.

`status` is `found`, `not_found`, or `failed`. A missing boundary produces an
empty reference array and `not_found`, rather than a guessed list. An unreadable
page produces `failed`, rather than a partial bibliography. Either case gives
the batch a nonzero exit code while still emitting a JSON record for each PDF.
The `warnings` array calls out any U+FFFD replacement character in an entry.
`elapsed_ms` includes file acquisition, hashing, PDF opening, backward page
processing, and reference parsing; it excludes CLI startup and JSON output.

The search reuses the existing reading-order, document cleanup, region tagging,
section detection, segmentation, and field parser. After each newly prepended
page, cleanup and section detection run on the currently selected pages. A
boundary qualifies when at least three entries can be segmented from a heading
or from a headingless numbered run. This conservative rule means a legitimate
one- or two-reference list is currently reported `not_found`. The current
implementation rechecks the growing suffix, so a very long bibliography or a
document with no detectable list can take more work than these test cases.

## Evidence and limits

The [five-paper audit](analysis/2026-09-29-reverse-bibliography-audit.md)
measured an experimental backward scanner against the full-document path using
the same `lopdf` stages. It selected 2/11, 6/33, 11/62, 2/14, and 11/93 pages,
and captured 36, 54, 94, 44, and 91 end-list entries respectively (319 total).
The new command's `label`, `page`, `raw`, `title`, and `doi` fields were checked
against those saved trial results and matched for all 319 entries. The audit's
9–32 ms per-PDF medians measure its **warm, release-mode, in-memory harness**;
they exclude file reads, hashing, and JSON publication. They are not timings of
this CLI or promises for a Raspberry Pi or a full production workload.

For this CLI, seven release-build runs over the same five cached PDFs on an
Ubuntu 24.04 x86-64 AMD EPYC 9V74 host yielded these median `elapsed_ms`
values (file read, hash, and backward scan included):

| PDF | Median ms | Pages scanned | Entries |
| --- | ---: | ---: | ---: |
| `2309.10334v1` | 34.68 | 2 | 36 |
| `2401.15719v5` | 22.64 | 6 | 54 |
| `2410.17124v1` | 34.92 | 11 | 94 |
| `2509.12458v3` | 31.08 | 2 | 44 |
| `2510.26824v2` | 35.03 | 11 | 91 |

These timings exclude CLI startup and serializing/writing the JSON lines. The
host's filesystem cache was warm; they are neither a Pi benchmark nor a
throughput measurement under concurrent load.

Finding all 319 entries means list capture and source identity in those PDFs,
not perfect text transcription or verified metadata. The audited output has a
spurious replacement character in one entry and a title that is present in
`raw` but missing from the parsed `title` field in another. This command keeps
the raw text and flags replacement characters, but does not repair the parser,
check bibliographic services, or establish publication-grade accuracy. One
paper also contains a separate earlier bibliography; this mode intentionally
returns its **last** list only. Exactness and field fixes deserve separate PRs.
