# Form resource policy

The previous Form cache checked estimated payload bytes only after unbounded
decompression and lexing. Empty programs had a zero charge, map cardinality
was unlimited, and Forms that did not fit were decoded again on every use.

The replacement policy applies to Form XObjects in the lopdf interpreter:

| Resource | Limit / behavior |
| --- | --- |
| Encoded and decoded Form stream | 8 MiB each, checked before lexing; bounded lopdf decode with no raw-byte fallback on failure |
| Filter chain | 8 layers; checked predictor dimensions before decoding |
| Cached programs | 64 MiB allocation charge, including vector capacities and nested operands; minimum 256 bytes per entry |
| Cache cardinality | 4,096 entries and bounded FIFO eviction queue |
| Decode work | 64 MiB per page; chains/predictors retain their worst-case reservation |
| Form execution work | 128 MiB of program charge per page, charged on cached hits too |
| Form invocations | 16,384 per page, including empty cached Forms |

The cache evicts oldest entries instead of permanently refusing new reusable
Forms. Active interpreter frames can still hold evicted programs through Rc;
this is not a 64 MiB process-RSS ceiling. The 8 MiB raw stream cap bounds lexer
input, but object/vector expansion means transient program memory is larger
than the decoded stream. Whole-document parsing, page streams, font resources,
page outputs and other engine allocations are outside this policy.

Work budgets reset per page, including when pages are requested backwards.
Exhaustion returns a page error with the stable `resource_limit:` prefix;
it does not return a successful page containing silently truncated spans.
The pipeline's existing failed-page handling records this error. Content
policy identity changes from 4 to 5 so prior ledger results are not confused
with this extraction policy. Limits are conservative initial policy choices,
not corpus-derived accuracy guarantees or a wall-clock timeout.

Validation: tests cover empty/tiny entry floods, duplicate accounting,
spare-capacity and dictionary accounting, byte-budget eviction, a compressed
Form expanding past 8 MiB, repeated uncached direct Forms, repeated cached
empty Forms, per-page budget reset and hostile filter/predictor dimensions.
Existing warm/cold reuse and span/figure tests remain intact. Workspace Rust
tests and Clippy pass; Python checks pass (64 tests and dependency audit).

A release-mode synthetic diagnostic on this x86-64 Linux executor extracted
the same 100-span Form 1,000 times: 24.6 ms with retained bounded caching versus
83.1 ms clearing the Form cache between pages. Every extracted PageText was
compared for equality. This measures reuse only, not corpus throughput or M1
performance. Reproduce with:

`cargo test --release --lib measure_form_cache_reuse -- --ignored --nocapture`

## Adjacent font audit

`SessionCache::fonts` still has no entry/byte limit. `resolve_font` eagerly
loads every font in resource dictionaries. `get_font_encoding` is used in
several production paths, including ToUnicode CMaps, although lopdf 0.45
provides `get_font_encoding_with_limit`. Embedded Type1 program inspection
already uses `get_plain_content_with_limit(MAX_FONT_PROGRAM)`.

The Form repair does not establish a whole-document memory bound or resolve
these font paths. A follow-up needs bounded CMap decoding, cardinality and
allocation accounting (including composite widths), and failure semantics
that preserve warning visibility rather than silently substituting encodings.
