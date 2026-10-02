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
policy identity changed from 4 to 5 for these limits; revision 6 below folds
empty save/restore pairs so prior ledger results are not confused
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

## Revision 5: bounded work, unchanged expansion

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

## Revision 6: remove redundant empty saves, preserve repeated output

Form compilation now removes adjacent balanced `q Q` pairs, including nested
empty pairs. Every retained text, paint, transform and Form invocation remains
a barrier; unbalanced saves/restores remain. Operands keep their original
indexes and order. Only changed programs shrink their operation allocation.
No rendered output or interpreter result is memoized. Cached programs still
consume invocation and execution budgets on every visit, including empty ones.

The extended diagnostic counts actually interpreted operations, elided empty
operations, Form calls, allocation-based execution charge and emitted spans/
text bytes, alongside page-text time and process peak RSS. A second fixture
puts one `X` inside each leaf save/restore pair: its N-cubed output is necessary
and is preserved exactly. Tests compare complete `PageText` values with folding
disabled, including repeated placements, transforms, ordering, paint barriers
and unbalanced restores. Both an invocation exhaustion and a retained-text
execution exhaustion are tested on the production folded path.

Measurements below use Rust 1.98.1, debug x86-64 Linux, median of three fresh
processes per case on a shared executor. Loading is outside the time interval;
RSS is `/proc/self/status` VmHWM for the whole diagnostic process. The reference
uses the same instrumented binary with folding disabled. Raw trials and source
identity are in [form-execution-measurements.json](analysis/form-execution-measurements.json).

| Empty fixture N | PDF bytes | Reference operations | Folded operations | Output spans (both) | Reference ms | Folded ms |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 20 | 1,316 | 16,421 | 421 | 0 | 2.58 | 1.03 |
| 40 | 1,637 | 129,641 | 1,641 | 0 | 12.21 | 4.18 |
| 80 | 2,277 | 1,030,481 | 6,481 | 0 | 65.43 | 12.32 |
| 160 | 3,557 | 6,825,237 (limited) | 25,761 | reference fails; folded 0 | 468.51 | 49.75 |

Reference N=160 stops at the explicit execution limit after 21,397 calls; it
does not finish. Folding completes 25,761 calls, eliding 8,192,000 empty
operations. Empty-fixture RSS across these runs is 15,568–16,064 KiB. The
remaining Form invocation work is quadratic in this fixture; this is not a
general elimination of nested expansion. N=400 still reaches the invocation
limit rather than silently succeeding.

| Text fixture N | Operations (both) | Emitted spans / text bytes (both) | Reference ms | Folded ms |
| ---: | ---: | ---: | ---: | ---: |
| 4 | 405 | 64 / 64 | 1.15 | 0.47 |
| 8 | 3,145 | 512 / 512 | 1.53 | 1.85 |
| 16 | 24,849 | 4,096 / 4,096 | 8.35 | 9.29 |
| 32 | 197,665 | 32,768 / 32,768 | 64.91 | 64.25 |

The text fixture elides zero operations and retains the same execution charge.
RSS grows from 15,980–16,132 KiB at N=4 to 20,768–20,864 KiB at N=32 as output
grows. Timing noise is visible; no text-work speedup or M1 claim is supported.
N=64 reaches the execution limit with calls still available. All resource
outcomes remain explicit.

Reproduce with the ignored `measure_shallow_nested_forms` test, setting
`TPE_FORM_DIAGNOSTIC_N`; add `TPE_FORM_DIAGNOSTIC_TEXT=1` for retained output
and `TPE_FORM_DIAGNOSTIC_UNOPTIMIZED=1` for the reference. Run each case in a
fresh process. The test's elapsed interval excludes fixture construction.

## Separate font audit

`SessionCache::fonts` still has no entry/byte limit. `resolve_font` eagerly
loads every font in resource dictionaries. `get_font_encoding` is used in
several production paths, including ToUnicode CMaps, although lopdf 0.45
provides `get_font_encoding_with_limit`. Embedded Type1 program inspection
already uses `get_plain_content_with_limit(MAX_FONT_PROGRAM)`.

The Form repair does not establish a whole-document memory bound or resolve
these font paths. A follow-up needs bounded CMap decoding, cardinality and
allocation accounting (including composite widths), and failure semantics
that preserve warning visibility rather than silently substituting encodings.
The controlled reproducer and upstream range-expansion evidence are recorded
in [FONT_RESOURCES.md](FONT_RESOURCES.md).
