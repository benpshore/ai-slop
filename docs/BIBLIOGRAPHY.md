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
`extraction_status` separately records `complete`, `partial`, or `failed` for
the inspected pages. Resource cutoffs produce `partial` even when `status`
is `found`; a `not_found` scan also retains its cutoff warnings.
Failed records retain the same fields: any acquired SHA-256 and backend
identity remain available, unknown scan fields are null, references are empty,
and the error appears in `error` and `warnings`. Acquisition failures have a
null hash. Successful records have a null `error`. When multiple qualified
lists start on the same page, the last one is selected.
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

## Resolution accuracy

`--resolve` preserves balanced DOI suffix punctuation, including older SICI
identifiers. Parser-repaired wrapped DOIs take precedence over raw prefixes;
raw text is preferred only when it extends the same parsed identifier. A DOI
response must carry the normalized requested DOI, including paper metadata
lookups. Paper-title searches also reject distinct DOI candidates within the
0.05 similarity margin. Ambiguity remains unresolved and is excluded from the
metadata-rejection count. Bibliographic searches
require title evidence as well as the existing author/year checks; records without
enough title evidence remain unresolved. Distinct DOIs with ranking scores less
than 0.05 apart are reported as `ambiguous`, not chosen by API response order.
`attempts` distinguishes metadata-compatible `candidate` records from the single
selected `verified` record; CSV reports `ambiguous` explicitly. This heuristic
does not certify exact matches or guarantee that the right result is among the
five query results.

Offline regression tests cover balanced identifiers, missing title evidence,
query-order independence, duplicate DOI hits, and venue-based version selection.
`cargo test --lib resolve::tests::live_crossref_exact_identifier -- --ignored`
checks the production resolver against one known live Crossref record. It is
opt-in because registry availability must not determine ordinary test success.

## PubMed identifiers

The resolver recognizes explicitly labeled `PMID:` values, PubMed URLs, and
`PMC`-prefixed identifiers in reference text. Bare numbers are never assumed to
be PMIDs. Exact lookups use Europe PMC core metadata (`EXT_ID:<id> AND SRC:MED`,
`PMCID:<id>`, or a quoted DOI), then check both returned identity and agreement
with the printed reference. Conflicting printed PMID/PMCID pairs and multiple
distinct registry identities remain unresolved. Europe PMC author names are
normalized to given-name-first for the shared verifier.

Successful Crossref matches are enriched by exact DOI lookup for PMID/PMCID;
a lookup failure retains the accepted Crossref result and records the error.
A Crossref request failure no longer prevents the exact biomedical fallback.
A verified PubMed record without a DOI has `resolved.doi: null`, with its PMID
and optional PMCID retained. The source and method identify Europe PMC. This is
not a claim that every reference can be found in PubMed: references without an
explicit biomedical ID or a resolvable DOI still depend on Crossref text search.
Annotation-only PMID links and the paper-level metadata resolver are not yet
connected to this path.

CSV appends `resolved_pmid` and `resolved_pmcid`. Appending to an older header
is refused; use a new output path. Ledger schema 5 opens version 4 with a metadata
version upgrade, retaining every old run and its version/provenance. New runs
use version 5. Old DOI strings deserialize correctly; new DOI-less records use
null, so older binaries must not write to the upgraded ledger. Other schema
versions are still refused.

`cargo test --lib resolve::tests::live_ -- --ignored --test-threads=1` checks the
production resolver with Crossref and Europe PMC. The path-scoped/manual `Registry accuracy
smoke` workflow runs that command on Ubuntu without making network availability
an ordinary CI gate. Verification is sequential and adds an identifier lookup
per accepted DOI; shared caching and batch APIs are future throughput work.

Explicit printed PMID/PMCID identities must verify together. Conflicting IDs,
metadata mismatches, missing registry records, ambiguity, or registry errors
leave the entry unresolved and stop DOI/query fallback. Europe PMC metadata
mismatches count as rejected when the entry remains unresolved; missing records
and transport failures alone do not. Counts use only the current resolution pass.
