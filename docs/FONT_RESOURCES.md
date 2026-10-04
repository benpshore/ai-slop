# Bounded font and ToUnicode resource path

Font decoding now has a separate resource policy from Form execution. Fonts
are resolved only when a non-empty string uses their resource name. Unused
fonts allocate no loaded decoders. Direct fonts remain local to the active
page/Form context; indirect fonts can be reused through a FIFO cache. Encoding
policy 2 introduced these bounds. Policy 4 also prioritizes a simple font's
ToUnicode map over its rendering Encoding, as required for extraction by
PDF Reference section 5.9.1. The same stream, cardinality and allocation
preflight runs before this map is parsed, including fonts with named encodings
or Differences dictionaries. A malformed map retains a Unicode warning and
Partial status. Missing mappings preserve replacement characters at their
original byte positions without swallowing later mapped characters.

| Component | Bound |
| --- | ---: |
| Font cache | 512 entries and 64 MiB of allocation charges |
| Per-page font loading | 4,096 context loads and 64 MiB of work/allocation charges |
| Encoded/decoded font stream | 8 MiB per stream, also limited by remaining page work |
| CMap source cardinality | 65,536 mappings, including repeated/overlapping definitions |
| CMap allocation/copy charge | 32 MiB before the upstream parser is called |
| CMap array-interval preflight | 1,000,000 comparisons |
| Width/encoding arrays | 65,536 inspected entries per array and its immediate children |
| Font/resource names | 1,024 bytes |

The stream guard reuses the existing filter-chain and predictor row/auxiliary
allocation checks. It reserves for both preflight and decoding, with refunds
only for a single non-predictor layer. Embedded Type1 streams retain their
8 MiB decode bound; their clear-text tokenization is charged separately.
Only the routes which consume a font program inspect it: an irrelevant program
cannot block a named encoding, known Differences base, usable ToUnicode map or
complete built-in TeX table. Width arrays are checked before copying/flattening.
Width and Differences entries reserve 128 bytes each, including transient
interval-sweep events, heap entries and output runs for width normalization.

The CMap preflight scans without allocating target strings. It counts actual
mapping rows rather than trusting declared section counts, skips names/strings/
comments, bounds metadata nesting, and refuses unsupported mapping syntax.
Besides source cardinality and Unicode payload, it models surviving array-valued
intervals: lopdf can clone the full target array on overlaps and even touching
inserts. Those copies and equality checks are charged before construction, and
the preflight's own interval work is bounded. Limits abort the page with a
`resource_limit:` error; they cannot silently substitute a fallback encoding.
JSON, chunk/job status and the ledger retain the incomplete outcome.

The scan stops only at the complete outer CMap epilogue (`endcmap CMapName
currentdict /CMap defineresource pop end end`), matching lopdf's parse boundary.
Bytes after that boundary still incur the decoded-input charge but cannot add
mappings in lopdf. Strings, comments, arrays and dictionaries cannot masquerade
as this boundary; mapping sections must finish first. An earlier bare epilogue
in the mandatory prolog/metadata makes lopdf use its existing malformed-font
fallback before constructing mappings.

Native run [36947313254](https://github.com/benpshore/pdftextract/actions/runs/36947313254)
exposed this compatibility case in pinned `arxiv:2510.26824v2`: font objects
80 and 81 contain small CMaps followed by 11 binary bytes. The previous scan
rejected that suffix and lost pages 1–12 (91/181 references instead of 181/181).
The bounded diagnostic in run
[36950255239](https://github.com/benpshore/pdftextract/actions/runs/36950255239),
artifact `11204540361`, records the PDF/CMap hashes and source SHA. Both CMaps
also lack `begincmap`; regression tests cover that existing fallback as well
as valid mapped fonts with the same binary suffixes. The temporary corpus
capture was removed after diagnosis; no PDF bytes were committed.

These are conservative component charges, not measured heap usage or a
whole-process memory guarantee. Cache eviction does not free a decoder still
referenced by a live context; such references are charged to the page as well.
lopdf still builds a reverse map for accepted CMaps, even though extraction only
needs forward decoding. Distinct fonts sharing one CMap are bounded but are not
deduplicated. A forward-only upstream representation remains a possible
optimization. Whole-document parsing and other components remain outside this
policy; process containment is separate.

Regressions cover a four-byte range describing 2^32 codes (unused: no decoder;
used: explicit refusal), compressed stream and predictor limits, array-copy
amplification, width-array limits, cache entry/byte eviction with live references,
normal decoding and JSON/SQLite outcome visibility.

## Evidence before the repair

The following audit and measurements describe the earlier code, before encoding
policy 2. They are retained as provenance, not presented as post-repair performance.

`SessionCache::fonts` retains an `Rc<LoadedFont>` for each indirect font object
without an entry or byte cap. `load_fonts_from_resources` eagerly loads every
font in a resource dictionary, even on a page with no text. Different font
objects sharing one ToUnicode stream resolve and own separate maps. Direct
font dictionaries are reloaded without this identity cache. Composite-width
maps and transient decoded/parsed data also need accounting.

Both `simple_decode` and `composite_decode` call unbounded
`Dictionary::get_font_encoding`. lopdf offers `get_font_encoding_with_limit`,
which limits stream decompression, but that alone does not bound CMap work:
`src/encodings/cmap.rs::ToUnicodeCMap::from_sections` first stores compact
forward ranges, then eagerly loops through every source code in each range to
build a reverse `HashMap<Vec<u16>, Vec<ReverseCMapEntry>>`. The extractor only
needs forward decoding. A compact four-byte source range can describe billions
of entries; no such enormous allocation was attempted for this audit.

The ignored `measure_unused_font_cmap_expansion` test uses a blank page and a
349-byte uncompressed CMap containing one range. Distinct font dictionaries
share that exact stream. The original diagnostic asserted zero output, the number of cached fonts,
one compact forward range per font, and reverse entries for a repeated Unicode
value. The current regression asserts zero cached fonts and reverse entries.
Test-only input caps keep the diagnostic to at most 262,144 codes and
four fonts; those are not production limits.

| Source codes per font | Cached unused fonts | Reverse entries | CMap bytes | Emitted spans | Median ms | Peak RSS KiB range |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 65,536 | 1 | 65,536 | 349 | 0 | 111.55 | 27,968–28,072 |
| 262,144 | 1 | 262,144 | 349 | 0 | 221.73 | 27,960–28,464 |
| 65,536 | 4 | 262,144 | 349 | 0 | 451.14 | 64,048–64,164 |

These are three fresh processes per case, Rust 1.98.1 debug x86-64 Linux on a
shared executor. Timing covers `page_text`; RSS is whole-process VmHWM and
includes fixture/loading overhead. Allocator capacity and repeated Unicode
keys mean RSS is not proportional to the source-range width. Raw trials are
in [font-audit-measurements.json](analysis/font-audit-measurements.json).

Reproduce with `TPE_FONT_DIAGNOSTIC_CODES=65536 TPE_FONT_DIAGNOSTIC_FONTS=4
cargo test --lib measure_unused_font_cmap_expansion -- --ignored --nocapture`
in a fresh process. Embedded Type1 inspection already uses
`get_plain_content_with_limit(MAX_FONT_PROGRAM)` (8 MiB); do not describe all
font paths as unbounded decompression.

Re-running the diagnostic now checks lazy loading. Fresh whole-corpus results
must be attributed to their tested code SHA; the historical timings above do
not predict production performance.
