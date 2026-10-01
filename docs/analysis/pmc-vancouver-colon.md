# Vancouver author-colon repair

The 200-paper #133 artifact (run 36819388522, source
`0254da617f872ba862521012068cc2f6069d03b3`) exposes a recurring BMC pattern:
`Schmader KE: Epidemiology ... . Clin J Pain ...` loses the title before
generic comma/organisation parsing reaches the journal. A validated personal
author list can safely identify that first colon as the author/title boundary.

The predicate validates every comma-delimited name, including particles and
suffixes, with optional terminal `et al`. A lone one-initial prefix such as
`Vitamin D:` remains ambiguous and is rejected. Corporate lists, malformed
lists and colons within titles remain on the existing parser path. Validated
author names and the original raw entry are preserved.

## Captured-entry replay

Reparse every one of the **8,471** captured raw entries across the same **200**
papers. Keep the original **8,353** truth/extracted pairings fixed, so changed
titles cannot improve their own alignment denominator. Input JSONL SHA-256:
`dc6edc1d26ea31df636bad111e6839cabb246d51efe8b58c190d7fa4fca08d01`.

| Field | Before | After | Scored title regressions |
| --- | --- | --- | --- |
| Strict title | 6,116/7,880 | 6,394/7,880 | 0 |
| Loose title | 6,923/7,880 | 6,965/7,880 | 0 |
| Strict first surname | 7,675/8,200 | 7,687/8,200 | — |
| Year | 7,942/8,163 | 7,942/8,163 | — |
| DOI against truth | 1,477/7,140 | 1,477/7,140 | — |

The actual implementation changes 320 entries' titles, 238 author lists,
33 venues, 13 volumes, one issue and 13 page ranges. No raw, year or identifier
fields change. The first-surname metric does not validate entire author lists.
This is a parser replay, not fresh PDF extraction or evidence of improved
boundary detection/counts. The integrated PDF rerun is reported separately.

Tests use original single-author, multi-author, compound-surname and subtitle
examples, preserve numeric suffixes, and reject title/non-author colons and
incomplete lists. Full workspace tests, strict Clippy and formatting pass.

## Remaining failure categories

The baseline has 20/200 missing lists and 21/180 found lists with wrong counts.
Examples distinguish boundaries from splitting: three no-list papers have a
reference heading but only one forward entry; PMC7051178's single entry has
9,909 characters. Some count mismatches instead reflect publisher grouping
(PMC7756430: 24 printed groups versus 82 JATS subreferences).

At least seven of the 20 no-list papers have visibly garbled character
mappings, accounting for 274 truth references. Among extracted entries,
84/8,471 contain U+FFFD; substitution errors can occur without U+FFFD.
Of 957 loose title failures, 830 have recoverable title text in the raw entry,
but some are merged entries requiring splitting rather than field parsing.

Identifier recovery remains distinct: 5,643/7,140 aligned truth-DOI entries
have no parsed DOI, and only 27 of those contain the literal truth DOI after
removing whitespace. JATS may supply identifiers absent from print. Existing
annotation-only identifiers include disagreements and require verification.
All 200 baseline records have `resolution: null`, so registry precision and
coverage are unmeasured. Paper-level biomedical resolution is also a follow-up.

#57's title-boundary repair was separately replayed over all 8,471 entries:
no parsed fields changed. It is not evidence of improvement for this corpus.
The MDPI semicolon-author pattern remains separate because its proposed
recognizer has a demonstrated regression on a malformed list.
