# Remaining font and ToUnicode resource path

This audit covers the current lopdf 0.45.0 path separately from Form resource
limits. It does not implement a font-memory policy or claim a process bound.

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
share that exact stream. It asserts zero output, the number of cached fonts,
one compact forward range per font, and reverse entries for a repeated Unicode
value. Test-only input caps keep the diagnostic to at most 262,144 codes and
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

A separate repair needs lazy loading of used fonts, bounded decoding before
parsing, bounded range/cardinality work or a forward-only CMap representation,
and cache/transient allocation accounting. Limits must report `resource_limit:`
and partial/failed outcomes through existing JSON/ledger/job paths instead of
silently switching encodings. Whole-document loading and other allocations
remain outside component budgets; worker containment is a separate control.
