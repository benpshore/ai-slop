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

## Integrated extraction, October 1, 2026

Fresh extraction confirms the replay on the same 200 PDFs and 9,590 JATS
references. The integrated resource/publication/evaluator repairs preserve
#133's original accuracy counts. The title repair then improves 278 strict
titles in each direction. Every paper and truth entry remains in the report,
including missing lists, unmatched entries and eight partial forward results.

| Metric | Integrated baseline | With title repair |
| --- | --- | --- |
| Backward lists found | 180/200 | 180/200 |
| Forward lists found | 183/200 | 183/200 |
| Backward exact reference counts | 159/200 | 159/200 |
| Forward exact reference counts | 158/200 | 158/200 |
| Backward strict title agreement | 6,116/7,880 | 6,394/7,880 |
| Forward strict title agreement | 6,131/7,876 | 6,409/7,876 |
| Backward strict first surname | 7,675/8,200 | 7,687/8,200 |
| Forward strict first surname | 7,675/8,199 | 7,687/8,199 |

The downloaded reports have identical paper IDs, truth, list/count outcomes,
warnings and extraction statuses. All 8,353 backward and 8,422 forward
alignment pairs are unchanged. Each direction has 278 improved strict titles
and zero strict title regressions. Year and DOI metrics are unchanged. Exact
reference counts do not imply exact transcription, and these runs make no
registry-resolution or M1 performance claim.

Provenance is embedded in `report.json`, with full per-paper/per-entry truth,
extraction, alignment and field outcomes. Both reports use scorer version `2`,
source SHA-256 `67e56da1c3d419afab4e9c1f8c9bec2ccfce97fbebcced70468fe870ccec0565`,
and corpus manifest SHA-256
`9465c36c416fbee0a17757ba9af41a13e89aced02cb6aad5883a49c1cf1a922e`.
The manifest pin has not changed: the recovered PMC9866638 XML is verified by
both its original MD5 and SHA-256 (see `corpus/verified/README.md`).

| Evidence | Baseline | Title repair |
| --- | --- | --- |
| PR head | `7de09bfa473e29fc82335b4783b6a6c26269a6cc` | `3830732849bd212cd7b4eb7b3e597f1caa44fa91` |
| Actual tested merge SHA | `f67a9183f1e29a2863ce6b606eebaae6a5521399` | `c4c31c6915465e105ebba21b55133068452a0093` |
| Successful run | [36938552541](https://github.com/benpshore/pdftextract/actions/runs/36938552541) | [36938602126](https://github.com/benpshore/pdftextract/actions/runs/36938602126) |
| Scored artifact ID | `11198947657` | `11199087209` |
| Scored ZIP SHA-256 | `0fe2db67a7b21a920adca71be7e371e77564d8b0881d9c2fc448e454d05c1e10` | `2592e452ef146c232992c8fcfb1ee8632a5b0ae73e1656c2cd2195dc80e5ecf5` |
| Full forward artifact ID | `11199102150` | `11199082313` |

Backend identity in both directions is `lopdf` `0.45.0`, config digest
`9f8970ba688e6c296481485165e5a0e14cf025007355a9def0f83a510c9b032d`.
Raw backward JSONL accompanies each scored artifact; full forward output is
stored separately to keep scored evidence independently downloadable. GitHub
artifacts expire after 90 days; retain copies when extending the experiment.

Earlier run 36936337244 failed while fetching an upstream-mutated XML object,
before scoring. Standalone #119 run 36819980628 and #121 run 36937684989 also
failed fetching their older pin. These are acquisition failures, not missing
papers silently removed from the successful denominators.
