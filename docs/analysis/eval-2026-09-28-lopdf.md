# Evaluation failure taxonomy — lopdf backend, 2026-09-28

Source: 20 per-paper dumps + `report.md` from the `eval-ubuntu-24.04-arm` artefact
(backend `lopdf`). All numbers below were recomputed from the
dumps with throw-away Python scripts; every "simulated" gain is a Python re-implementation
of the proposed rule run over the same dumps, not a Rust change.

Caveat: dumps carry `raw` entry text and `reference_section_text` (= `page.text`), but no
line geometry. Mechanisms that depend on `x0`/baselines (class 1, 9) are inferred from code +
line order in `page.text`, and are labelled as such. Source files were edited by another
agent during this analysis (line numbers in `latex_refs.rs`/`lopdf_backend.rs` shifted); the
code paths cited were re-checked against the files at write time and are referenced by
function name only.

## 0. Headline arithmetic (read this first)

| | value |
|---|---|
| truth refs / extracted / matched | 1228 / 1269 / 851 |
| recall / precision | 69.3% / 67.1% |
| unmatched truth refs | 377 |
| … of which in 2608.28714 + 2504.09409 + 2603.21379 | **369** (299 + 56 + 14) |

In those three papers the ground truth comes from `.bbl` files without `\newblock`, so
`latex_refs::parse_bibitem` leaves `authors = []` and `title = None` for **every** entry
(299/299, 58/58, 35/35). `eval::match_references` has four keys (DOI, arXiv id, title,
first-author+year); with title and authors empty only DOI/arXiv remain, and 2608.28714 has
0 DOIs, 2504.09409 has 0 DOIs and 2 arXiv ids. Of the 377 unmatched truth refs:
**358 cannot be moved by any extractor change** (299 + 56 + the 3 DOI-less ones in
2603.21379); **11 are an extractor bug** — DOIs split at a line break in 2603.21379, fixable in
`find_doi` (§6) and matchable by the existing DOI pass; the remaining 8 are small cases
(ligature/hyphenated titles, Vancouver misparse, duplicate truth, 2 refs not printed). The same 369 entries are also
counted as spurious, which is why precision is also ~67%.

Simulated eval fix (see §8): a last-resort pass in `match_references` pairing unmatched truth
with unused extracted entries by word-Jaccard ≥ 0.5 of NFKC+dehyphenated `truth.text` vs
`ext.raw` recovers **375 of 377** unmatched truth refs; for all 369 in the three papers the
recovered extracted index equals the truth position (numbered lists, so this is verifiably the
right pairing). Recall 69.3% → 99.8%, precision 67.1% → 96.6%. It changes no existing pair.

Denominators (from `eval::summarize`): title accuracy = correct / matched-truth-with-title
(828), DOI accuracy = correct / matched-truth-with-DOI (321), "of printed" uses the 200 whose
DOI appears in the squashed page text.

---

## 1. Over-segmentation (entries split mid-entry)

**Affected:** 5 papers, 23 spurious entries caused by mid-entry splits (2505.16990: 15,
2502.00857: 4 (#13, #28–30), 2501.17300: 2, 2511.13979: 1, 2306.11313: 1) plus the fragments
that *did* match (8 matched entries whose `raw` is only a tail, e.g. `arXiv:2308.12966.`).
Downstream: 9 of 15 year failures and 8 title failures are on such fragments.

Examples (split point = start of the second fragment):

- 2505.16990 #11/#12: `…Tom Rainforth, and Tommi Jaakkola. 2024. Genera-` | `tive ﬂows on discrete state-spaces: …`
  page text: `Rainforth, and Tommi Jaakkola. 2024.\nGenera-\ntive ﬂows…` — the printed line was emitted as two lines.
- 2505.16990 #3–#7: one entry (Qwen-VL) became five: `Jinze Bai, … Chang Zhou,` | `and Jingren Zhou. 2023. Qwen-vl: A versatile` | `vision-language model for understanding, localiza-` | `tion, text reading, and beyond. arXiv preprint` | `arXiv:2308.12966.` (truth `qwen-vl` matched #7 by arXiv id; extracted year/title null).
- 2502.00857 #28–#30: `Libo Qin, … Wanxiang` | `Che, and Philip S. Yu. 2024. Large Language` | `Models Meet NLP: A Survey. arXiv e-prints,`; page text shows `2024.\nLarge Language\nModels Meet NLP: A Survey.\narXiv e-prints,` (fragments).
- 2511.13979 #39: `11899719-customizing-your-chatgpt-personality, 2025a. Accessed: 2025-08-29.` split from `OpenAI. Customizing your ChatGPT personality. https://help.openai.com/en/articles/` (page text: `OpenAI.\nCustomizing your ChatGPT personality.\nhttps://…/articles/\n11899719-…`).
- 2306.11313 #29 (different cause): `Series A, containing papers of a mathematical or physical character, 209(441-458):415–446.` — previous entry ended in `London.` and `author_start_re` accepts `Series A,` as `Surname Initial,`.

**Code path (inferred for the first four):** `reading_order::find_line` joins spans only when
the horizontal gap ≤ `LINE_REACH` (1.0 × font size). Justified reference columns contain
inter-word gaps > 1 em, so one printed line becomes 2+ `Line`s (`Genera-` is a separate line
at the right of the column). `citations::indent_says_start` then compares a line's `x0` with
the previous `Line` in the same column — which is the right-hand fragment — so the next
real continuation line (indented) is *outdented relative to the fragment* → `Some(true)` →
new entry. The fallback `author_start_re && ends_like_entry` is never consulted.
The 2306.11313 case is `segment_author_year`'s fallback: `author_start_re` + `ends_like_entry`.

**Proposed rules:**
1. In `citations::section_lines`, merge consecutive `Line`s on the same page/column whose
   baselines agree within 0.4 × size (i.e. same row) into one `SectionLine` with `x0` of the
   leftmost; alternatively in `indent_says_start` compare against the leftmost `x0` of the
   previous *row*, not the previous `Line`. (Root fix; also candidate: raise `LINE_REACH`
   inside a column block, but that risks joining across gutters.)
2. Evidence guard in both segmenters: a candidate entry start whose text up to the next start
   contains no year (19xx/20xx), DOI, arXiv id or URL is appended to the previous entry
   (or dropped if it is in a trailing run). Simulated: this applies to 34 of the 49 spurious
   entries outside the three truth-broken papers and to 0 of 828 matched entries.
3. For the `author_start_re` fallback require the candidate line (or the next line) to
   contain a year; `Series A, containing papers …` has none.

## 2. Under-segmentation (merged entries)

**Affected: none found as entry-into-entry merges.** Checked: every truth title (≥ 4 words)
was searched in every extracted `raw`; all 29 raws containing 2+ truth titles are duplicate
truth entries (e.g. 2604.03540 `yan2025maniflow` / `maniflow2025` both "ManiFlow …") or a short
title contained in a longer one (2305.13843 #7). No extracted raw contains an inner `[n]`
label. The only real absorption is trailing non-reference content, covered in §3.

## 3. Section detection / section end

Heading detection: `find_reference_section` found a heading in 20/20 papers and the first
extracted entry is the first printed reference in all 20 (checked in `reference_section_text`).
Failures are all at the **end** of the section or page furniture inside it:

| paper | what got in | entries |
|---|---|---|
| 2502.00857 | Table 5 rows after the list: #34 `Preferred Cost Execution`, #37 `ROUGE CPU High Low Very Fast`, … #53 `and speed are qualitative assessments intended to guide method selection.` | 20 spurious |
| 2503.15734 | Author biographies appended to #33 (7 820 chars): `…pp. 3615–3620, 2023. DAVID E. J. VAN WIJK is a Postdoctoral Scholar in the Department…` | 1 bloated |
| 2505.16990 | `…arXiv:2505.19223. - Supplementary Material -` appended to #71 | 1 |
| 2305.13843 | `Page 30 of 35`, `Page 31 of 35`, … appended to 6 entries (e.g. #28 `…(2023) 1–51. Page 30 of 35`) | 6 |
| 2412.06210 | running header glued into #44: `…Federated Learning forConfIoTerenceApplicationsacronym ’XX, June 03–05, 2018, Woodstock, NY` | 1 |

**Code path:** `is_end_heading` (`end_heading_re` requires the keyword at line start, so
`- Supplementary Material -` and `DAVID E. J. VAN WIJK is a …` miss; table rows are neither
headings nor larger font). `section_lines` drops repeated edge lines only when the text is
*identical* on ≥ 2 pages, so `Page 30 of 35` / `Page 31 of 35` never repeat.

**Proposed rules:**
- `end_heading_re`: allow leading punctuation (`^\W*`), and add IEEE bio starts
  `^\p{Lu}{2,}(?:\s+\p{Lu}\.?)*\s+\p{Lu}{2,}.*\b(is|was|received)\b`.
- Stop the list at a line matching `^(Table|TABLE|Fig\.?|Figure)\s*\d+[:.]` and, in
  combination with the evidence guard of §1, drop a trailing run of entries with no
  year/identifier.
- Furniture: normalise digits (`\d+` → `#`) before counting repeated edge lines, and strip
  `^Page \d+ of \d+$` explicitly.
- Cap: an entry longer than ~1 500 chars is almost certainly absorbing trailing matter —
  cut at the first line after a year that starts with an all-caps word run.

## 4. Numbered vs author-year style detection

`detect_style` (first line only) chose correctly in 20/20: bracket `[n]` for the 14 numbered
papers, author-year for 2306.11313, 2501.17300, 2502.00857, 2505.16990, 2511.13979,
2601.13206 (matches the printed first line in every `reference_section_text`). No
dot/paren-numbered papers are in this corpus.

Side defect in the same area — `author_year_label`: the label is the first capitalised
word, i.e. the given name for `First Last` styles (`Sahar2023` for "Sahar Abdelnabi … 2023",
`Jacob2023`, `Matt2015`), and `None` when the entry starts with an initial (2511.13979: 57/60
labels are `None`, e.g. `N. Agarwal, A. Moehring, …`). Proposed: derive the label from
`parse_entry`'s first author via the same surname logic as `author_surname`, after parsing.

## 5. `parse_entry` title failures

828 matched truth refs have a truth title; 394 are correct (47.6%). Attribution of the 434
failures (script: truth title located in NFKC+dehyphenated `raw`?):

| sub-class | count | papers |
|---|---|---|
| A. correct span, ligature only (NFKC fixes) | 82 | 2604.03540 19, 2509.10402 12, 2511.13979 12, … |
| B. correct span, line-break hyphen only | 61 | 2505.16990 20, 2601.13206 12, 2502.00857 7, … |
| C. comma-separated, initials-first styles (Elsevier `elsarticle-num`, SIAM, IEEE unquoted book titles): title is the field after the author list, but parser took something else | ~220 (60 + 82 + 24 no-title + part of 45 "other"/15 truncated) | 2305.13843 ≈ 170, 2603.04447 23, 2503.15734 4, 2509.08395 4 |
| D. ACM `Authors. 2019. Title.` — title = the year | 26 | 2412.06210 |
| E. `First M. Last` read as Vancouver | 6 | 2108.04588, 2509.08395, 2412.06210, 2601.13206 |
| F. `’` inside “…” closes the quote | 5 (3 in matched pairs) | 2507.08599, 2509.10402, 2608.28714 |
| G. wrong pairing / fragment (title not in raw at all) | 33 | 2305.13843 20 (author-year collisions), 2505.16990 8 (fragments) |

Examples:
- C (Elsevier): 2305.13843 #13 raw `Y. Zhang, Q. Yang, An overview of multi-task learning, National Science Review 5 (2018) 30–43.` → title `30–43.`; #85 `T. Sun, …, X. Huang, Learning sparse sharing architectures for multiple tasks, in: Proceedings of the AAAI…, 2020, pp. 8936–8943.` → title `pp`; 2603.04447 #4 `E. N. Lorenz, Deterministic nonperiodic flow, J. Atmos. Sci. 20 (2) (1963) 130– 141.` → title `Sci`.
- C (IEEE book): 2503.15734 #21 `H. Khalil, Nonlinear Systems. Pearson Education, Prentice Hall, 2 ed., 2002.` → title `Pearson Education, Prentice Hall, 2 ed., 2002.`
- D: 2412.06210 #40 `Qiang Yang, Yang Liu, Tianjian Chen, and Yongxin Tong. 2019. Federated machine learning: Concept and applications. ACM …` → title `2019`.
- E: 2108.04588 #13 `Brent N. Clark, Charles J. Colbourn, and David S. Johnson. Unit disk graphs. …` → title `Clark, Charles J. Colbourn, and David S. Johnson`.
- F: 2507.08599 #16 `“A nonuniform local limit theorem for poisson binomial random variables via stein’s method,”` → title `…via stein`.

**Code paths:**
- C: in `parse_entry`, the first branch fires when `is_author_only(masked[..year])` — for
  `Y. Zhang, Q. Yang, An overview of multi-task learning, National Science Review 5 ` every
  `. ` follows an initial, so the whole prefix up to `(2018)` is judged "authors" and the title
  is read *after* the year (`30–43.`). When the year is at the end, `author_terminator`
  skips initial periods (not Vancouver, not surname-first) and stops at the first
  non-initial `. ` — i.e. inside the venue (`J. Atmos. Sci.` → `Sci`) or after the title.
- D: `author_terminator` ends authors at `Tong.`, then `title_end` stops at `2019.`.
- E: `vancouver_start_re` (`^\p{Lu}[\p{L}'’\-]+\s+\p{Lu}{1,3}\b[,.]`) matches `Brent N.`,
  so `author_terminator` returns at the first `. `.
- F: `find_quoted` closes on any of `” " “ ’`.

**Proposed rules (in `parse_entry`, before the year branch):**
1. *Comma field rule.* If the body starts with an initials-first name
   (`^(\p{Lu}\.[\s-]?)+ (particle )*Surname`) and the author list is comma-separated, consume
   comma fields while each is a name (`X. Y. Surname`, `and X. Surname`, `et al.`); a field
   of the form `X. Surname. Rest` ends the list at that `. `. The title is the next field, up
   to the first non-abbreviation `. ` or a `, ` followed by `in:`/`in `/`arXiv`/`vol.`/`pp.`/
   a digit/an uppercase word. Guard `is_author_only` so it rejects a prefix containing a
   field that is not a name. Simulated: 540/828 (from 394) alone, with 2 regressions (2511.13979: parsed title
   `J. a. Sedoc` — lower-case middle initial breaks the name test; `Z. Hatﬁeld- Dodds` — a
   line-broken surname); these need the name test to accept `\p{Ll}\.` initials and to run
   after dehyphenation; on 2504.09409 and
   2603.21379 it yields the truth-text comma title for 55/57 and 33/35 entries (currently 0).
2. *ACM year sentence.* When the author segment is followed by `YYYY[a-z]?. `, skip it and
   start the title after it (+21 titles).
3. *Vancouver guard.* Treat as Vancouver only when the first two comma-separated names both
   match `Surname AB`; `Brent N. Clark,` is not.
4. *Quotes.* After `“` close only on `”` (or `"`); accept `’` as a closer only after `‘`.

## 6. DOI printed but not (fully) parsed

321 matched truth refs have a DOI; 157 correct (48.9%). Classes:
- 122 **not printed** in the PDF at all (the `.bib` has it, the paper doesn't: 2503.15734 27,
  2502.00857 23, 2608.03351 22, …) — not fixable by extraction; the "of printed" metric
  (78.5%) is the meaningful one.
- **40 split by whitespace from line joining** (2305.13843 15, 2501.17300 8, 2509.10402 6,
  2108.04588 4, 2412.06210 4, 2511.13979 3), plus 11 of the 14 unmatched truth refs in
  2603.21379 (same cause; see §8).
- 2 wrong DOI because the pair itself is wrong (2305.13843 `wang_causalint_2022` ↔
  `wang_escm2_2022` swapped by the author-year pass).

Examples (raw tail → extracted `doi`):
- 2108.04588 #2 `doi:10.4230/LIPIcs.SoCG. 2023.2.` → `10.4230/LIPIcs.SoCG`
- 2108.04588 #7 `doi:10.37236/ 6040.` → `null`
- 2305.13843 #93 `doi: 10. 1145/3292500.3330861.` → `null`
- 2509.10402 #15 `https://doi.org/10.1145/364399 1.3648400` → `10.1145/364399`
- 2603.21379 #24 `https://doi.org/10.1016/j. patcog.2006.08.004.` → `10.1016/j` (truth `10.1016/j.patcog.2006.08.004`)
- 2501.17300: `doi:` and its value land in different entries (#8 ends `…Academy of 10.1073/pnas.1418838112.`, #9 is `Sciences 112.7, pp. 1989–1994. doi:`) — see §9.

**Code path:** `append_continuation` joins lines with a single space; `find_doi`/`doi_re`
(`10\.\d{4,9}/[^\s"<>]+`) stops at that space, `trim_trailing_punct` removes the dangling `.`.

**Proposed rule** (simulated, `util.find_doi_v2`):
1. Rejoin `10. 1234/` → `10.1234/` after `doi:`/`doi.org/`.
2. Allow an empty suffix after `/` and then glue the next token.
3. While the DOI ends in `.`, `/`, `-` or `_` and the next token contains a digit and is not a
   capitalised word, glue it.
4. If the DOI is followed only by ≤ 3 DOI-charset tokens up to the end of the entry (no
   `http`, `URL`, `www`, capitalised words), glue them (handles `364399 1.3648400`,
   `AC CESS.2025.3538050`).
Result: 157 → **197/321 correct (61.4%; of printed 197/200 = 98.5%)**, 0 regressions, 55
extracted DOIs changed, all visually correct; and 11 of 14 missing 2603.21379 pairs become
DOI-matchable even without the §8 eval fix. Better still: do it at line-join time
(`append_continuation`: when the previous line ends inside a DOI/URL token, join without a
space).

## 7. Year failures

15 of 828 matched pairs (98.2% correct):
- 9 on fragments (§1): 2505.16990 ×7 (`arXiv:2308.12966.` → year null), 2502.00857 ×1, 2511.13979 ×1.
- 2 bare-year picks from numbers:
  - 2503.15734 #26 `… vol. 8, pp. 2087– 2098, 2024.` → 2098. `year_bare_re` rejects digits
    adjacent to a dash, but after line joining there is a space (`– 2098`). Rule: treat
    `[–-]\s+` as glued, and in numbered styles prefer the last year candidate.
  - 2608.03351 #12 `RFC, 1952:1–12, 1996.` → 1952. Rule: reject a candidate followed by
    `:\d` (volume:pages).
- 4 are truth-side: 2511.13979 `Brown … 1877–1901, 2020` truth year 1901 (`latex_refs::first_year`
  takes the page number); 2412.06210 and 2604.03540 pairs (`khan2021federated` /
  `Khan2020Federated`, `flowpolicy2024` / `zhang2024flowpolicy`) are duplicate truth entries
  matched crosswise.

## 8. The catastrophic papers

**2608.28714 (299/299/0) — eval only.** Truth entries are IEEE `\bibitem`s without
`\newblock` → `authors=[]`, `title=None`, `doi=None` for all 299 (e.g. key `zaitsev2015motion`,
text `M. Zaitsev, J. Maclaren, and M. Herbst, "Motion artifacts in MRI: A review," …`). The
extraction is good: extracted #1 `authors=[M. Zaitsev, J. Maclaren, M. Herbst]`,
`title="Motion artifacts in MRI: A review"`, `year=2015`, `pages=911–935`. Comparing the
quoted title in `truth.text` with the extracted title: 177 exact, 108 equal after NFKC +
dehyphenation, 1 truth without a quoted title, 13 other (5 apostrophe-truncated as in §5F,
e.g. `…expert radiologists` for `…radiologists’ scoring…`; the rest are hyphenation of real
compounds, e.g. `Patient- speciﬁc`). `match_references` fails only because no key exists.

**2504.09409 (58/58/2) — eval first, parse_entry second.** SIAM style, no `\newblock` →
truth authors/title empty for 58/58, 0 DOIs, 2 arXiv ids (the 2 matches). Extraction is
also weak here: only 34/58 titles, and those are wrong, e.g. #3 `K. Balasubramanian and S. Ghadimi, Zeroth-order nonconvex stochastic optimization: …, Found. Comput. Math., 22 (2022), pp. 35–76.` → title `Comput`; #5 → title `pp`; #4 `A. Beck, First-order methods in optimization, SIAM, 2017.` → no title. This is class 5C;
the comma rule gives the right title for 55/57.

**2603.21379 (35/35/21) — both.** SIAM style, truth authors/title empty; 31 truth DOIs.
Of the 14 unmatched: 11 have a truth DOI that is printed but split across a line
(`https://doi.org/10.1137/ 090764189.`, `…/10. 1137/22M1540363.`, `…/10.1109/SC.2018. 00045.`)
so `find_doi` returns `None` or a prefix (§6); 3 (`Coherence2`, `Vasilescu2002Tensorfaces`,
`Wang2013`) have no DOI/arXiv and cannot match without a title. Titles: 0/35 match the truth-text
comma title today (e.g. #1 → `pp`); the comma rule fixes 33/35.

**Eval fixes:**
1. `match_references`: add a final pass — unmatched truth vs unused extracted, word-Jaccard
   of normalised (`NFKC`, dehyphenated, label-stripped) `truth.text` vs `ext.raw`, threshold
   0.5, best-first. Simulated: +375 pairs (2608.28714 +299, 2504.09409 +56, 2603.21379 +14,
   2108.04588/2305.13843/2412.06210/2604.03540 +1 each, 2507.08599 +2), all 369 in the three
   papers land on index = position; the other 6 land on the paper's listed spurious entry that
   carries the same reference (2108.04588 #27 Perepelitsa, 2305.13843 #165 Avinesh/J3R,
   2412.06210 #10 Khan, 2507.08599 #6/#7 `——, "Block-fading…"`/`"Quasi-static…"`,
   2604.03540 #30 ManiFlow); 0 existing pairs change.
2. Author-year pass: when several extracted entries share `surname|year`, pick by raw-text
   Jaccard instead of first-found. 16 current 2305.13843 author-year pairs have raw-text Jaccard
   < 0.3 (e.g. `zhang_leaving_2022` ↔ #82 "CTnoCVR…", `li_multi-task_2022` ↔ #124 "SPEX…"),
   and 2502.00857 pairs `mozafari-triviahg`/`mozafari-hintqa` are swapped. These inflate recall and
   deflate title/DOI accuracy.
3. `latex_refs::parse_bibitem` without `\newblock`: take the title from the `"…"`/``` ``…'' ```
   span (IEEEtran; verified on 2608.28714's detexed text). The SIAM `{\em …}` idea is
   **unverified**: only 2108.04588's `.bbl` is cached locally. Do not fill truth with
   `citations::parse_entry` (circular).
4. `first_year` in `latex_refs` picks page numbers (1901 above) — apply the same page-range
   exclusion as the extractor.
5. 2601.13206: truth keys `nielsen2012patient`, `new-recruit` do not appear anywhere in the
   dumped reference text (`Nielsen`, `Posthuma` absent) — truth over-count, not an extraction miss.

## 9. Column / reading-order artefacts inside the section

No cross-column interleaving of whole lines was found (every multi-line entry reads in order in
`raw`; §2 check). Observed artefacts are within-row:
- **Row split into fragments** (root of §1): `Rainforth, and Tommi Jaakkola. 2024.\nGenera-`,
  `Huiwen\nChang,\nHan\nZhang,\nJarred\nBarber,` (2505.16990 page text: every word its own line).
- **Row order inverted**: 2501.17300 page text `…National Academy of\n10.1073/pnas.1418838112.\nSciences 112.7, pp. 1989–1994. doi:` and `…Study of Scientific\n10 .\nCoauthorship Networks”. … doi:\n1007/978-3-540-44485-5_16.` — the DOI fragment (probably a different font with a different ascent) sorts above the row it belongs to, splitting both the entry (#9, #44 spurious) and the DOI.
- **Glyph interleaving** with a running header: 2412.06210 #44 `…Federated Learning forConfIoTerenceApplicationsacronym ’XX, June 03–05, 2018, Woodstock, NY`.

**Code path:** `reading_order::group_lines`/`find_line` (`LINE_REACH`, `BASELINE_TOLERANCE`),
`line_top_first` sorting by top edge (font-dependent), and `finish_line` sorting spans purely
by `x0` (interleaves two overlapping baselines).
**Proposed:** sort rows by baseline (`y0` of the text baseline, not bbox top); merge same-baseline
fragments within a column (§1 rule 1); in `find_line` refuse to merge spans whose sizes differ by
> 20% when their x-ranges overlap (header vs body).

## 10. Hyphenation, ligatures, accents

- **Ligatures:** 243 extracted raws contain U+FB00–FB06 (`ﬁ`, `ﬂ`, `ﬀ`, `ﬃ`), e.g. 2108.04588 #7
  `Reﬁning the hierarchies…`, 2604.03540 #30 `Maniﬂow`. `lopdf_backend::emit` normalises with
  `nfc()`, which keeps compatibility ligatures. 82 title failures are fixed by NFKC alone;
  it also recovers `maniflow2025` (2604.03540) and `fading_channels_2`/`fading_channel_3`
  (2507.08599) via the title pass. **Rule:** map U+FB00–FB06 to their letters at span emission
  (targeted NFKC, not full NFKC, to keep superscripts etc.).
- **Hyphenation:** 401 raws contain `\w- \w` from line joins (e.g. 2608.28714 #2 `P. Boe- siger`,
  #19 `MRI recon- struction`). Against truth titles, the break hyphen should be dropped 117
  times and kept 51 times (`privacy- preserving`, `Multi- Task`, `Trust- aware`), so "always
  drop" is wrong 30% of the time. Simulated rule (93% correct, 157/168): keep if the next
  fragment starts uppercase; else keep if `a-b` occurs unbroken elsewhere in the document; else
  drop if `ab` occurs; else keep for common compound prefixes/heads (`multi`, `cross`, `co`,
  `non`, `self` … / `aware`, `based`, `scale`, `free`, `level`, `task` …); else drop. The two
  document lookups need the whole reference-section text passed into segmentation; the
  local-only variant (uppercase rule + prefix/head lists, usable directly in
  `append_continuation`) scores 150/168 (89%). Apply at line-join time (the boundary is known
  there), not on `raw` after the fact.
- **Accents:** 56 raws carry detached spacing accents (2608.28714 31, 2608.03351 8, 2603.21379 5),
  e.g. 2502.00857 #3 `Dragan Gaševic,´`, 2503.15734 #8 `E. Das¸`, 2603.21379 #11 `…090764189. ¨`.
  TeX-built accents are separate glyphs on a raised baseline; they become separate lines or land at
  the wrong x. Affects author names (and author-year matching), rarely titles. **Rule:** in
  `group_lines`, attach a span consisting only of a spacing diacritic (U+00A8, U+00B4, U+0060, U+02C6,
  U+02DC, U+00B8, U+02C7, U+02DA) to the base glyph horizontally overlapping it and compose (NFC).

---

## Prioritised changes (largest expected gain first)

| # | change | where | simulated effect on this corpus |
|---|---|---|---|
| 1 | Raw-text fallback pass + surname-year disambiguation in the matcher; quoted-title truth for `\newblock`-less `.bbl` | `eval::match_references`, `latex_refs::parse_bibitem` | recall 69.3% → 99.8% (+375), precision 67.1% → 96.6%; the disambiguation part is not simulated — it targets 18 suspect author-year pairs with raw-text Jaccard < 0.3. Measurement only, extraction unchanged. **Caveat:** the quoted-title truth fix adds ~298 2608.28714 titles to the title-accuracy denominator (828 → ~1126), of which only 177 are exact today, so landing #1 without #3 *lowers* reported title accuracy. |
| 2 | Comma-field title rule for initials-first styles (+ ACM year-sentence, Vancouver guard, `’` quote fix) | `citations::parse_entry`, `author_terminator`, `find_quoted` | title accuracy 47.6% → 65.2% alone; also fixes 55/57 titles in 2504.09409 and 33/35 in 2603.21379 once they are matched |
| 3 | Ligature expansion at emit + context-aware dehyphenation at line join | `lopdf_backend::emit`, `citations::append_continuation` | titles 47.6% → 64.0% alone; with #2: **91.8%** (760/828). Recovers 3 unmatched refs. |
| 4 | DOI line-break joining (`10. 1145`, `10.1137/ 0907…`, `j. patcog…`, trailing fragments) | `citations::find_doi` (or join-time in `append_continuation`) | DOI accuracy 48.9% → 61.4%, of printed 78.5% → 98.5%; +11 matches in 2603.21379 without #1 |
| 5 | Segmentation hygiene: merge same-baseline fragments before `indent_says_start`; year/identifier evidence guard; section-end stops (table captions/rows, IEEE bios, `- Supplementary …`, digit-normalised furniture such as `Page N of M`) | `citations::section_lines`, `segment_*`, `is_end_heading`; `reading_order::find_line` | evidence guard alone removes 34 of the 49 spurious entries outside the three truth-broken papers (precision 67.1% → 68.9% under today's matcher), fixes 9 of 15 year failures and 8 fragment-only matches; 0 of 828 matched entries hit by the evidence guard |

Minor, not in the top five: year picker (`– 2098`, `1952:`), label from the parsed first
author's surname (§4), accent composition (§10).
