# PMC bibliography measurement: publisher PDFs against JATS reference lists

**Date:** 2026-09-30 UTC
**Run:** [PMC bibliography 36723648301](https://github.com/benpshore/pdftextract/actions/runs/36723648301)
(`ubuntu-24.04-arm`, backend `lopdf`, PR #124, commit `9df794c`; the job took 2 min 1 s
with warm caches). Method and metric definitions: [PMC_EVAL.md](../PMC_EVAL.md).
**Decision:** no-go for "100% exact reference lists at scale" on publisher PDFs. The backward
scan is fast and, when it finds a list, its fields are mostly right, but it reports the exact
entry count for 61.0% of papers and no list at all for 13.5%.

## Corpus

`corpus/pmc-manifest.json`: 200 PubMed Central Open Access articles drawn with seed 20260930
(306 candidates read), 139 journals, 48 publishers, years 2011 to 2026, 9,590 truth references.
All are publisher PDFs; no author manuscripts survived the filters (in this bucket the
manuscript records are not `is_pmc_openaccess` or carry no PDF), so that class is unmeasured.
Truth is the publisher's own JATS `<ref-list>`; 195 papers have numbered labels in the XML,
5 do not. The sample is deliberately journal-capped, which is why MDPI (30), BMC (23), Wiley
(22), Oxford University Press (17), Frontiers (13) and Elsevier (11) dominate the counts.

## Headline (backward `tpe bibliography` vs forward `tpe extract`)

| metric | backward | forward |
| --- | --- | --- |
| list found | 86.5% (173/200) | 91.5% (183/200) |
| entry count exact, all papers | 61.0% (122/200) | 61.5% (123/200) |
| entry count exact, found only | 70.5% (122/173) | 67.2% (123/183) |
| truth entries unmatched in found papers | 1,926 of 9,590 | 2,342 |
| spurious extracted entries in found papers | 116 | 82 |
| first-author surname, strict (diacritics and case kept) | 92.4% (5935/6421) | 92.4% (5940/6429) |
| first-author surname, loose | 94.1% (6042/6421) | 94.1% (6047/6429) |
| year | 97.1% (6221/6404) | 97.1% (6229/6412) |
| DOI, truth has one (missing counts as wrong) | 23.9% (1327/5558) | 23.7% (1334/5632) |
| DOI, when one was extracted | 98.7% (1327/1345) | 98.9% (1334/1349) |
| title, strict | 75.1% (4597/6125) | 75.2% (4614/6133) |
| title, loose (similarity >= 0.9) | 87.8% (5378/6125) | 87.8% (5384/6133) |
| matched entries with no extracted first author / title | 167 / 131 | 170 / 133 |
| entries with U+FFFD | 1.2% (83/6662) | 1.2% (83/6710) |
| entries with running-head or page-furniture leakage | 0.4% (27/6662) | 0.2% (16/6710) |
| per-PDF ms p50 / p95 / mean | 12.9 / 96.7 / 26.0 | 30.8 / 79.8 / 40.6 |
| wall time, 200 PDFs, 4 workers | 1.7 s | 6.2 s |
| pages read | 744 of 2,337 (mean ratio 0.30) | all |

Field accuracies are over aligned pairs where the truth has the field, so they describe the
entries that were found; the 1,926 truth entries that no extracted entry matched are
invisible there. Every count difference but two is under-extraction: 42 papers are short by
five or more entries, 7 by one to four, 2 (Frontiers) are three entries long.

The DOI row needs care: 4,213 matched entries have a DOI in the XML that the PDF text did not
yield. Most publisher PDFs in this sample do not print DOIs, so 23.9% is mostly a fact about
the PDFs; where a DOI was printed and extracted it was right 98.7% of the time. Backward and
forward paths agree on every accuracy to within 0.2 points; the backward scan reads 30% of
the pages and is 2.4x faster per PDF at the median (its p95 is worse because a `not_found`
reads every page).

By publisher (backward, papers / found / count exact): MDPI 30 / 26 / **1**; BMC 23 / 23 /
22; Wiley 22 / 21 / 20; OUP 17 / 13 / 12; Frontiers 13 / 12 / **2**; Elsevier 11 / 8 / 5;
Nature 8 / 5 / 5; Dove Press 6 / 4 / 4; PLOS 5 / 4 / 2. BMJ, Springer, ASM, AAAS, Cambridge,
Spandidos and most one-paper publishers were exact. Unnumbered (author-year) lists: 0 of 5
found backward, 3 of 5 forward, none exact.

## Failure taxonomy

From `failures.md` in the run artifact (also printed in the job log): every `not_found` and
every count mismatch, with the extracted and truth entries and the forward path's page text at
the last reference heading. Causes are grouped as far as that evidence shows them; the PDFs
were not opened by hand.

### `not_found` (27 papers)

| cause | papers | evidence |
| --- | --- | --- |
| Numbered labels detached from their bodies: the page text reads `References`, `1.`, `2.`, ... with the entry bodies elsewhere, so no entry segments | 8 | MDPI 2013 PMC3757425, PMC3757428, PMC3757431; MDPI PMC10647287; PLOS PMC4488360; Royal Society PMC5311907; Radcliffe PMC6406133; OUP PMC10087010 |
| Unmapped fonts: the last pages decode to glyph-code noise (`$QZDU0(IIHFWV...`, `.# :57G 9"796:G`), so nothing is recognisable | 7 | Dove Press PMC9488596, PMC9488603; Elsevier PMC9817175, PMC11408821, PMC11408823; OUP PMC11983383, PMC11983392 |
| Heading found, list is author-year or unlabeled Vancouver, and fewer than three entries segment | 3 | eLife PMC7051178; OUP PMC10246717; Frontiers PMC11813883 |
| Numbered list is present but no boundary is accepted (bare `25 Nunnari` labels; Scientific Reports and PNAS layouts) | 3 | Wiley PMC5311918; Nature PMC7655840; PNAS PMC10410747 |
| No reference heading recognised in the trailing pages: list before acknowledgement or licence pages, heading with a glyph prefix (ACS `■ REFERENCES`), `Reference` inside a table, three-page layouts | 6 | Radcliffe PMC6406127; Nature PMC7655836, PMC7655838; PNAS PMC10410749; ACS PMC12138824; Nordic Orthopaedic Federation PMC12357179 |

### Found with the wrong entry count (51 papers)

| cause | papers | evidence |
| --- | --- | --- |
| MDPI hanging-indent lists: the label column detaches part-way, the remaining labels arrive as one run (`... [CrossRef] 10. 11. 12. 13. 14.`) and the bodies merge or vanish; 3 to 21 entries extracted of 31 to 140 | 25 | PMC8537375 (9 of 65), PMC11313967 (3 of 87), PMC10222613 (6 of 140), and 22 more |
| Frontiers author-year lists: entries merge across boundaries, inter-word spaces are lost (`Abiru,M.,Mihara,Y.,andKikuchi,Y.(2007)`), lists truncate; in two papers the last-page `Citation:` / `Copyright` block segments as three extra entries | 10 | PMC4344110 (+3), PMC4344112 (+3), PMC6346594 (14 of 56), PMC12745427 (10 of 75), PMC11813888 (20 of 103) |
| Other numbered lists truncated mid-list: labels stop being recognised (BMC PMC5011904, entry 20 swallows entry 21), body text interleaved with the list (Croatian PMC4210254), lists cut at a page turn (PLOS PMC3572078 15 of 65, Wiley PMC12138822 4 of 20) | 9 | PLOS x2, BMC, Croatian Society, Korean Society, ERS, Cureus, Wiley, one unknown publisher |
| Other author-year lists with merged entries (an entry starting after a wrapped URL is not split) | 4 | SAGE PMC7756429, PMC10676628; IUCr PMC6608616; OUP PMC10246718 |
| Elsevier author-year near misses | 3 | PMC3777682 (68 of 70), PMC3777684 (52 of 53), PMC3777687 (41 of 49) |

### Field errors on aligned entries (backward path)

- Surname (319 wrong, 167 missing): the dominant wrong case is the Vancouver single-initial
  author (`Hanahan D. Hallmarks of cancer: ...`), where the author segment swallows the title;
  others are merged entries whose first author is another entry's tail, collective authors
  (`MD Anderson Head and Neck Cancer Symptom Working Group` -> `MD Anderson Head`), and
  given-name-first lists. Strict and loose differ by only 107 entries, so diacritics and case
  are a small part of the surname problem.
- Title (1,397 strict mismatches, of which 781 are within the loose 0.9 band): a trailing `?`
  is dropped from titles, superscripts and spacing after a line-end dash differ, and the rest
  are merged or truncated entries. The scorer already folds the XML's U+2011 non-breaking
  hyphen, which alone accounted for 107 mismatches in the previous run.
- Year (183 wrong): almost all belong to merged or truncated entries.
- U+FFFD: 83 entries in 4 papers, all with the same partially unmapped fonts as the
  `not_found` font cases.

## Assessment against the goal

The goal is a citation-chain database built from exact end-of-paper reference lists at scale.
On this sample the backward scan returns an exact-count list for 122 of 200 papers, and even
an exact count does not certify entries: within found lists, 7.6% of first authors and 24.9%
of titles are not strictly right. The failures are concentrated and systematic rather than
random (MDPI 1 of 30 exact, Frontiers 2 of 13, Dove Press and two Elsevier titles unreadable
because of fonts), so they will recur in proportion to those publishers' share of any large
corpus. The forward path has the same accuracy and the same failures; it is not an
alternative, only 2.4x slower. Speed is not the problem: 12.9 ms median per PDF.

Verdict: **no-go** today. The engine cannot be relied on for exact lists without the fixes
below, and the measurement should be re-run on this pinned manifest after each one.

## Top three engine fixes (not implemented here)

1. **Re-attach hanging-indent labels to their bodies by geometry.** Numbered labels that sit
   in their own column must be joined to the body run on the same baseline before section
   detection and segmentation (`reading_order` / `regions` / `citations::segment_entries`).
   This is the single largest cause: 8 `not_found` and 25 MDPI truncations, plus several of
   the other truncated numbered lists, about 40 of the 78 failed papers.
2. **Author-year and unlabeled Vancouver segmentation from indentation, not text patterns.**
   Start a new entry at a hanging-indent return (first line flush, continuation lines
   indented) and require the boundary at a line start; refuse page-footer blocks
   (`Citation:`, `Copyright`) as entries; accept a heading followed by such a list as a
   boundary. About 20 papers (Frontiers, Elsevier, SAGE, IUCr, OUP, eLife, ACS).
3. **Detect unmapped fonts and route around them.** When a page's text is glyph-code noise
   (no usable `ToUnicode`, symbolic encodings), fall back to encoding-based decoding or to the
   `pdfium` backend instead of reporting nothing; 7 `not_found` papers and all U+FFFD
   entries. Two small parser fixes ride along: end the author segment at a single initial
   followed by a period (`Hanahan D.`), and keep a trailing `?` in titles.
