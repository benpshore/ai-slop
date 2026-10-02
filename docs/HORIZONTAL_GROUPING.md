# Bounded horizontal line grouping

Ordinary spans used to scan every preceding line on a shared baseline until
one matched their horizontal reach. A row of N separated spans needed
N(N-1)/2 checks before the later line-ordering limit could apply. Accent and
vertical-group limits did not cover this loop.

The horizontal index keeps the original build order at its leaves. Each node
stores the smallest left edge, largest right edge, largest font size, and
lowest baseline below it. A node is skipped only when those conservative
bounds prove that no leaf can match. Right children are visited first, so the
latest matching line still wins. The leaf predicate, floating-point operation
order, baseline ordering, glyph sorting, accent handling, and XY-cut decisions
are unchanged when the budget suffices.

Adjacent words usually join the latest line. That case checks the line directly
and defers ancestor updates until a later search needs the index. Multiple
extensions of that line replace one pending summary. Tree traversal uses an
explicit stack with at most 16 entries; there is no new recursion.

Each page permits 1,000,000 work units for search visits, pending updates,
ancestor updates, and conservative tree growth. The index contains at most
20,000 line builds, matching the existing layout limit. Its nodes occupy at
most 1 MiB; growth can temporarily retain an old half-size tree as well. These
are component bounds, not a bound on the input spans or all page memory.
If either bound is reached, later ordinary spans remain separate. A warning
names the exhausted bound and its value with the stable `resource_limit:`
prefix. Page, document, and affected chunk status therefore become `partial`.
The index is never consulted again after a failed partial update.

The general tree search can still visit many leaves when coarse bounds
overlap. It is bounded by the explicit work allowance; this is not a claim
that every possible page has O(N log N) grouping. On separated spans, root
bounds reject the previous groups immediately and index maintenance costs
O(N log N). Common adjacent text retains a constant-time lookup with deferred
updates.

Local optimized Rust 1.98.1 probes, using the original scan and production
index with identical output, measured:

| Ordinary spans | Geometry | Original grouping loop | Indexed loop | Charged index work |
| ---: | --- | ---: | ---: | ---: |
| 1,000 | separated, one baseline | 0.675 ms | 0.099 ms | 21,152 |
| 4,000 | separated, one baseline | 10.359 ms | 0.451 ms | 92,654 |
| 8,000 | separated, one baseline | 43.685 ms | 1.042 ms | 193,325 |
| 16,000 | separated, one baseline | 181.842 ms | 2.113 ms | 402,668 |
| 16,000 | 100 adjacent spans per line | 0.177 ms | 0.250 ms | 35,378 |

These are nine-sample medians after two warmups of a synthetic grouping-loop
probe, including line/member allocation. They are not end-to-end timings or
corpus accuracy measurements. The adjacent-text control exposes the additional
bookkeeping cost rather than hiding it in the large sparse-page improvement.

Regressions compare the original reverse-scan winner at every one of 6,000
generated prefixes, verify ties and refreshed bounds, exercise floating-point
boundaries, and test exhaustion during tree construction. A 20,001-span page
keeps every span and character and reports the indexed-line limit with partial
status. Corpus validation must still check ordinary documents and all native
backends independently.
