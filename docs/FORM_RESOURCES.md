# Form resource policy

The previous Form cache checked estimated payload bytes only after unbounded
decompression and lexing. Empty programs had a zero charge, map cardinality
was unlimited, and Forms that did not fit were decoded again on every use.

The replacement policy applies to Form XObjects in the lopdf interpreter:

| Resource | Limit / behavior |
| --- | --- |
| Encoded and decoded Form stream | 8 MiB each, checked before lexing; bounded lopdf decode with no raw-byte fallback on failure |
| Filter chain | 8 layers; checked predictor dimensions before decoding |
| TIFF sub-byte predictor accumulator | 8 MiB (`Colors * size_of::<u16>()`), checked independently of the packed-row bound before decoding |
| Cached programs | 64 MiB allocation charge, including vector capacities and nested operands; minimum 256 bytes per entry |
| Cache cardinality | 4,096 entries and bounded FIFO eviction queue |
| Decode work | 64 MiB per page; chains and active predictors retain their worst-case reservation; single-layer decodes without prediction refund unused bytes, including LZW EarlyChange and Predictor 1 |
| Form execution work | 256 MiB of program charge per page, charged on cached hits too |
| Form invocations | 131,072 per page, including empty cached Forms |

The cache evicts oldest entries instead of permanently refusing new reusable
Forms. Active interpreter frames can still hold evicted programs through Rc;
this is not a 64 MiB process-RSS ceiling. The 8 MiB raw stream cap bounds lexer
input, but object/vector expansion means transient program memory is larger
than the decoded stream. Whole-document parsing, page streams, font resources,
page outputs and other engine allocations are outside this policy.

The predictor accumulator bound is an independent auxiliary-allocation cap,
not part of a combined 8 MiB peak-memory allowance. TIFF sub-byte reversal also
allocates a packed output row (bounded by the checked row size), while the
decoded input is live. Predictor detection mirrors lopdf 0.45: only Flate/LZW
use dictionary-form DecodeParms, and only Predictor 2 or 10..15 enable it.
Non-predictor parameters do not cause a full reservation to be retained.

Work budgets reset per page, including when pages are requested backwards.
Exhaustion returns a page error with the stable `resource_limit:` prefix;
it does not return a successful page containing silently truncated spans.
The pipeline's existing failed-page handling records this error. Content
policy identity changes from 4 to 5 so prior ledger results are not confused
with this extraction policy. Limits are conservative initial policy choices,
not general accuracy guarantees or a wall-clock timeout. The 70-paper pinned
corpus was used to reject and correct an overly restrictive initial invocation
limit: a real figure-heavy page invokes Forms 64,554 times, so 16,384 was
unacceptable. The adopted 131,072-call and 256 MiB execution limits provide
roughly twice the observed maximum, not a claim to cover every legitimate PDF.

Validation: tests cover empty/tiny entry floods, duplicate accounting,
spare-capacity and dictionary accounting, byte-budget eviction, a compressed
Form expanding past 8 MiB, repeated uncached direct Forms, repeated cached
empty Forms, per-page budget reset and hostile filter/predictor dimensions.
Review regressions cover the 128 MiB TIFF color-accumulator case and its 2-/4-bit
variants, accumulator boundaries, nine distinct tiny Flate Predictor-1 and LZW
EarlyChange Forms, and retained reservations for active TIFF/PNG predictors and
filter chains. Nested-Form tests verify exact repeated execution charges and
an explicit resource-limit error.
Existing warm/cold reuse and span/figure tests remain intact. A corpus
diagnostic reads every page of all 70 pinned PDFs successfully and measures
peaks of 64,554 calls, 6,628,971 decode bytes and 116,183,200 bytes of
execution charge per page. Workspace Rust
tests and Clippy pass; Python checks pass (64 tests and dependency audit).

A release-mode synthetic diagnostic on this x86-64 Linux executor extracted
the same 100-span Form 1,000 times: 24.6 ms with retained bounded caching versus
83.1 ms clearing the Form cache between pages. Every extracted PageText was
compared for equality. This measures reuse only, not corpus throughput or M1
performance. Reproduce with:

`cargo test --release --lib measure_form_cache_reuse -- --ignored --nocapture`

## Shallow nested Forms: bounded work, unchanged expansion

The three-level case reported with PR #133 is reproduced: the page invokes A
once, A invokes B N times, B invokes C N times, and C contains N `q Q` pairs.
Input size is O(N), but complete interpretation executes N cubed save/restore
pairs. The default depth limit of eight admits all three levels. C never needs
more than one saved graphics state, and the three decoded programs are cached,
so nearly flat RSS does not imply bounded execution work.

The following diagnostic uses the same generated PDF on PR #133's recorded
`5b26b33` revision (fixture injected locally, no remote change) and the reviewed
#119 follow-up. Values are median page-text time from three fresh processes per
N, debug x86-64 Linux on a shared executor; document loading is outside timing.

| N | PDF bytes | Complete q/Q pairs | #133 time | #119 time | #119 result |
| ---: | ---: | ---: | ---: | ---: | --- |
| 20 | 1,316 | 8,000 | 2.07 ms | 3.03 ms | success |
| 40 | 1,637 | 64,000 | 10.32 ms | 13.24 ms | success |
| 80 | 2,277 | 512,000 | 55.56 ms | 66.05 ms | success |
| 160 | 3,557 | 4,096,000 | 371.68 ms | 347.76 ms | execution budget error; incomplete |

Process peak RSS (`wait4`) ranges across all runs were 14,752–15,456 KiB on
#133 and 17,188–17,816 KiB on #119. Every #133 case completed. #119's N=160
case stops after 21,411 charged Form invocations and 268,434,864 bytes of
execution charge, just below the 256 MiB limit; it does not complete the
4,096,000 pairs. The count includes the invocation whose execution charge was
rejected. These are diagnostics, not wall-clock guarantees or M1 evidence.

**#119 caps work; it does not remove cubic expansion.** Below the fixed policy
cap, the interpreter still replays the same N-cubed operations. Above it, the
page fails explicitly rather than completing. Allocation-charge and invocation
budgets are enforcement mechanisms, not a new asymptotically faster algorithm.
There is no memoized execution result or semantic simplification of nested
programs in this patch. Moving frames from stack to heap would not change that.

The non-ignored regression checks exact invocation/execution charges for
N=20/40/80 and requires an execution-budget error for N=160. Reproduce the
manual patched diagnostic, one N per process, with:

`TPE_FORM_DIAGNOSTIC_N=80 cargo test --lib measure_shallow_nested_forms -- --ignored --nocapture`

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
