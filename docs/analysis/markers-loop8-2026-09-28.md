# Citation-marker recall: failure taxonomy (loop 8, dev60 eval-dev9 dumps)

Source: `eval-dev9/eval-ubuntu-24.04-arm/{report.md,dumps/*.json}`; code read: `src/citations.rs`
(`find_citation_markers`, `numeric_markers`, `author_year_markers`, `RefIndex`), `src/eval.rs`.
Scripts: `scratchpad/mk/cl2.py` (per missed truth key), `scratchpad/mk/cl3.py` (per in-text occurrence).

## What the metric measures (read first)

* The report's `marker recall` = `targets / cited keys` = (sum of marker target counts) / (key
  occurrences in the LaTeX `\cite` commands), capped at 100% (2108.04588: 34/33 gives 100%). This is
  not the local `eval.rs` formula (`resolved_markers / truth_cite_commands`), so the CI binary is newer
  than this checkout. The dumps confirm this in two more places: markers are found on pages *after*
  the reference list (2602.16061 p34, list on p30–33), and 2510.26824's SI list has its own number space.
  Local `find_citation_markers` does neither.
* **The metric never checks that a target is the right entry.** 2509.12458 shows 63.6%, but 0 of its
  35 targets are correct (see below). Fix: count a target only when `matches[target] ∈ keys of that cite`,
  or at least only when it is in the truth cited-key set. Spurious targets also offset misses before
  the cap: 2602.17690 91/80, 2603.19305 92/80, 2506.23487 61/55.
* Occurrence accounting works: for the four author-year papers, OK + classified misses equals the
  report's denominator to within ±4. 2504.10389: 52+25 vs 77. 2602.16061: 43+38 vs 82.
  2401.15719: 74+36 vs 112. 2508.02208: 48+24 vs 76.

## dev60 overview

11 of 60 papers are below 90%: 2309.10334 (0%), 2510.26824 (30%), 2602.16061 (52%), 2509.12458 (64%),
2508.02208 (66%), 2401.15719 (68%), 2504.10389 (69%), 2509.17930 (75%), 2501.17300 (83%),
2410.17124 (85%), 2603.05575 (89.5%). Also sampled: 2602.02748, 2601.12491, 2511.13979, 2603.12824,
2503.00030 (91–94%).

Missed occurrences by cause, over 13 author-year papers (cl3.py, deduped by text position):

| cause | occurrences | papers |
|---|---:|---|
| a3 comma-separated clauses inside `( … )`; only the first clause is parsed | 77 | 2602.16061 (24), 2401.15719 (32), 2504.10389 (21) |
| a4 multi-token / particle / organisation names | ~40 (45 raw, ~5 false hits) | 2508.02208 (17), 2401.15719, 2603.05575, 2603.12824, 2602.16061, 2410.17124, 2601.12491 |
| a9a table cells `Duan et al 2020` (no parentheses, no period after `al`) | 12 | 2410.17124 |
| a9b `(` separated from the citation by an interleaved float, footnote or column (`…Boltzmann⏎2016; Perc et al., 2017)`, 2509.17930 `…text-to-⏎Barrault et al., 2023). See⏎speech (TTS) vocoder (⏎`) | ~10 | 2509.17930 (7), 2501.17300 (2), 2503.00030 (1) |
| a9c bare narrative without parentheses: `As Alexander et al. 2015 put it`, `cf. Walter et al. 2015` | 3 | 2501.17300 |
| a1 parenthetical split across a page break (per-page scanning); includes 2 continuation halves first counted as a9 (`(Shi⏎`\f`and Wardlaw, 2016; Duering et al., 2023)`, `(Braun⏎`\f`and Schwartz, 2025; Braun et al., 2024)`) | 10 | 2503.00030 (4), 2602.16061 (2), 2509.17930 (2), 2410.17124, 2511.13979 |
| a7 narrative with locator or year list: `Politis and Romano (1994, Theorem 3.1)` | 6 | 2602.16061 (4), 2602.02748, 2511.13979 |
| a5 year list, entry is not the first year: `(Banerjee et al. 2023a, 2022)`, `(Yang et al. 2024, 2025)` | 4 | 2504.10389, 2508.02208, 2601.12491 |
| a2 three or more authors, `(Nipkow, Paulson, and Wenzel 2002)` | 4 | 2508.02208 |
| a6 letter-suffix list `2023a,b`, `2025c,b,a` (only the first letter resolves) | 1 counted by position; 4 keys | 2602.16061, 2504.10389, 2508.02208 |
| b unresolved (surname key mismatch) | ≤1 (Hofstätter, probably an artefact of my normalisation) | 2603.12824 |
| c resolved to the wrong entry (author-year) | 2 of 133 single-suffix markers checked across dev60 | 2603.04445, 2505.22973 |

None of the a9 cases is a nested-parenthesis failure. Every one was inspected.

Residuals the classifier does not explain: 2410.17124 has 212 OK + 17 classified against 250, so 21 are
unexplained (likely in figure or table text, or cited under a different name/year). 2509.17930 leaves 7.
12 "name + year nearby but not a citation" hits (`z`) were excluded as non-citations.

Numeric papers, counted separately:

| cause | occurrences | paper |
|---|---:|---|
| N1 RSC superscript numbers detached as one-line fragments (`8⏎9⏎10,11⏎`) | ~119 (222 − 67 − 36) | 2510.26824 |
| N2 bracket list split across a page break: `[6, 7, … 25,` (p27) / `26, … 41]` (p28) | 36 (one marker, 36 keys) | 2510.26824 SI |
| N3 label column detached, so labels are shifted by +15/+16 | 55 (35 wrong + 20 dropped) | 2509.12458 |
| N4 no `References` heading (REVTeX list starts directly with `[1]`), so 0 entries and 0 markers | all 23 cites | 2309.10334 |

**Class (b) is almost empty.** Once a clause is parsed, `resolve_author_year` finds the entry. The
losses are extraction losses (a), plus one numbering misalignment (N3, class c).

## Per-paper taxonomy

| paper | cause class | example marker text (body) | example truth entry | count |
|---|---|---|---|---:|
| 2510.26824 (30%) | N1 superscript fragments (a, not extracted) | p1: `…body of⏎5⏎1⏎literature. Initiatives such as the Materials Genome Initiative ,⏎Materials discovery underpins advances in energy conversion ,⏎6⏎7⏎2⏎3⏎4⏎Harvard Clean Energy Project , and Materials Project exemplify⏎energy storage , catalysis ,`. Numbers are detached from their anchor word, leaving a trailing ` ,`. Fig. 1 caption: `…corpus >2M⏎8⏎9⏎10,11⏎open-access papers from arXiv , ChemRxiv , and Semantic Scholar`. 67 distinct numbers 1–90 appear as fragments on pp1–9. | [1] Zhang 2013 (zhang2013nanomaterials), [2] Liu 2010, [3] Vogt 2022, [4] Butler 2018, [5] de Pablo 2014 (MaterialsGenomeInitiative), [6] Hachmann 2011, [7] Jain 2013 (MaterialsProject) | ~119 occ. **No** main-list entry (1–90) is ever targeted. The 147 − 56 other matched keys got credit only through SI copies. |
| 2510.26824 | N2 bracket split over page break (a) | `([6, 7, 8, … 25,` ⏎page⏎ `26, … 41] by inferring synthesis…` (SI p27–28) | SI [6] Pruszyńska-Karbownik 2026 … [41] | 36 keys |
| 2510.26824 | OK: SI `[n]` markers use a per-list number space (SI `[1]`→entry 91) | `[1]` p22 → 91 | — | 28 markers |
| 2509.12458 (64%, really 0% correct) | N3 (c) wrong entry: every label = printed number + 15 (+16 after a skipped `[37]`) | `[16]` p1 → entry 1 (Mozaffari 2021, printed `[1]`) | mozaffari2021toward | 35 occ wrong |
| 2509.12458 | N3 (a) dropped: `[1]`–`[15]` have no `by_number` entry, and `numeric_markers` silently drops brackets with no targets | `[4] [5] [6] [7] [8] [3] [9] …` p1 | lemic2021survey etc. | 19 occ |
| 2509.12458 | N3 dropped: label `[37]` skipped | `[37]` p8 | — | 1 |
| 2602.16061 (52%) | a3 comma clauses | `(Gui and⏎Toubia 2023, Li et al. 2024, Gao et al. 2025)` resolves Gui only | li2024frontiers, gao2025take | 24 |
| 2602.16061 | a3 + layout debris | `(Rubin 1976, Robins et al. 1994,⏎⊥⊥⏎Qin et al. 2008)` | robins1994estimation, qin2008efficient | (in 24) |
| 2602.16061 | a1 page split | `(Horton 2023, Goli and Singh 2024, Brand⏎` \f `et al. 2024)` | horton2023large, goli2024frontiers, brand2024using | 2 (3 keys) |
| 2602.16061 | a4 multi-token second author | `(Miao and Tchetgen Tchetgen⏎2016)` | miao2016varieties | 3 |
| 2602.16061 | a4 debris inside clause | `(Goff and Mbakop⏎2/3⏎3/4⏎2025, Voronin 2025)`: no clause parses, marker dropped | voronin2025linear | 1 |
| 2602.16061 | a7 locator | `Politis and Romano (1994, Theorem 3.1)`, `Shapiro (1991, Theorem⏎3.5)` | politis1994large, shapiro1991asymptotic | 4 |
| 2602.16061 | a6 letter list | `(Angelopoulos et al. 2023a,b, Ji et al. 2025)` gives 2023a only | angelopoulos2023ppi++ (2023b) | 1 key |
| 2602.16061 | x not extracted | — | d2010new (`d'Haultfoeuille`, unmatched reference) | 1 key |
| 2401.15719 (68%) | a3 comma clauses | `(Borkar et al.⏎2021, Lauand and Meyn 2023, Huo et al. 2023)`, `(Gupta⏎et al. 2019, Doan 2021, 2022)` | lauand2023curse, huo2023bias, doan2021finite, doan2022nonlinear | 32 |
| 2401.15719 | a4 particle second author | `(Tsitsiklis and Van Roy⏎1996)` | tsitsiklis1996analysis | 4 |
| 2504.10389 (69%) | a3 comma clauses | `(Krengel and Sucheston 1978, Samuel-Cahn 1984, Ma 2024)`, `(Freund et al. 2023, Ma et al. 2023, Nanda et al. 2020, Benadè et al. 2024)` | samuel1984comparison, ma2024randomized, nanda2020balancing, benade2024fair | 21 |
| 2504.10389 | a5 year list | `(Banerjee et al. 2023a, 2022)`, `(Jin⏎and Ma 2022, Balseiro et al. 2026, Banerjee et al. 2022, 2023a)` | banerjee2022online | 2 |
| 2504.10389 | a6 letter list | `(Banerjee et al. 2023a,b, Barman et al. 2022, …)` | banerjee2023online (2023b) | 1 |
| 2508.02208 (66%) | a4 organisation names | `(Qwen Team 2025)`, `Qwen Team⏎2024, 2025; Alibaba Cloud Qwen Team 2025a;` | qwen3_235b_A22B, qwen2.5_72b_instruct, qwen3_30b_a3b2025, qwen2025qwq32b | ~11 |
| 2508.02208 | a4 lower-case particle | `(de Moura and Ullrich 2021; …)`: `clause_re` needs `\p{Lu}`, and the entry has `authors=[]` (key comes from the raw-text fallback, `de moura`) | lean4 | 6 |
| 2508.02208 | a2 three authors | `(… Nipkow,⏎Paulson, and Wenzel 2002)`, `(Zheng, Han,⏎and Polu 2022; …)` | nipkow2002isabelle, zheng2021minif2f | 4 |
| 2508.02208 | a6 / a5 year and letter lists | `OpenAI 2025c,b,a, 2024; Anthropic⏎2025b,a` gives 2025c and 2025b only | openai2025_o4mini, openai2025gpt4.1, openai2024gpt4o, anthropic_opus_web | 4 keys |
| 2508.02208 | no year / no author (not resolvable) | `(Maxwell-Jia)`, `xAI Elon Musk's AI Company` | jia2024aime2024, grok42025 | 2 keys |
| 2509.17930 (75%) | a9 `(` detached by column interleave | `…text-to-⏎Barrault et al., 2023). See⏎speech (TTS) vocoder (⏎Figure` | barrault2023seamlessm4t | 7 |
| 2509.17930 | a1 page split | `(Shao and Feng, 2022; Inaguma et al., 2021;⏎` \f | shao2022non | 2 |
| 2501.17300 (83%) | a9 bare narrative without parentheses | `As Alexander et al. 2015 put⏎it`, `cf. Walter et al. 2015` | alexander2015epistemic, Walter2015 | 5 |
| 2501.17300 | a4 initial before the surname | `(J. Wu et al., 2023)` | Wu2023 | 1 |
| 2410.17124 (85%) | a9 table cells without parentheses or period | `…Good⏎Duan et al 2020⏎Good…`, `Liu et al 2020 (a)` | duan_primary_2020, liu_deep_2020 | 13 |
| 2410.17124 | a4 compound surname / organisation | `(Hernandez Petzsche et al., 2022)`, `(MONAI Consortium et al., 2020)` | hernandez_petzsche_isles_2022, monai2020monai | 4 |
| 2603.05575 (89.5%) | a4 particle second author | `(Caponnetto and De Vito, 2007)` | caponnetto2007optimal | 4 |
| 2603.12824 | a4 organisation / particle | `(Ltd, 2025; Nomic AI, 2025)`, `(van den Oord …)` | nomic2025colnomic, oord2019… | 4 |
| 2601.12491 | a4 particle / organisation | `(El Malki et al., 2026)`, `(Hanu and Unitary⏎team, 2020)` | 10.1145/3772318.3791855, Detoxify | 3 |
| 2503.00030 | a1 page split | four parentheticals cut at page ends | — | 4 |
| 2309.10334 (0%) | N4 no heading | list begins `[1] U. Seifert, Stochastic thermodynamics…` after Appendix A, with no `References` line | all 36 | 23 cites |

### 2509.12458: why `[4]`–`[7]` are not markers

This is not a page, offset or number-space issue. In reading order the reference list's label column
is emitted as separate label-only lines: `[1]`…`[14]` land *above* `REFERENCES` (outside the section),
`[15]`…`[36]` form one block after entry 14 (Wawrla), and `[37]`…`[44]` form another on p14. The entry
labels in the dump **are not printed labels**. They run `[16]`…`[36]` (index + 15), then `[38]`…`[59]`
(index + 16). The document has no `[45]`–`[59]` at all; the bracket scan of the whole body tops out at
`[44]`. So the build that produced these dumps synthesises labels sequentially, seeded from a label-only
line near the section start, instead of reading them. Local `segment_numbered` only parses labels from
line text and cannot do this, so the build is newer than this checkout. Fix agents should look at the
label-assignment code in that build, not at `same_row`.

Two more defects:
* Printed `[15]` De Petris (de2022rmf, the one unmatched key) is merged into entry 14 (Wawrla). The
  `[15]`–`[36]` label block sits between them, so no entry start is seen.
* Printed and assigned numbers therefore disagree everywhere: entry k is printed `[k]` for k ≤ 14 and
  `[k+1]` after the merge.

Consequences:

* `by_number` has no key 1–15 (or 37), so `numeric_markers` finds `[4]` but `targets` is empty and the
  `if targets.is_empty() { continue; }` guard drops it without trace. That removes 19 + 1 occurrences.
* Every `[n]` with n ≥ 16 resolves to the entry printed as `[n−15]`: 35 targets, all wrong.
* The report still shows 63.6%. A cheap guard: a numbered list whose first label is not 1 (or 0),
  or whose labels exceed the entry count, should fall back to ordinal numbering (entry k is `[k]`)
  or warn.

## Ranked fix list (by missed occurrences on dev60; papers affected in brackets)

1. **Superscript numeric citations** (N1, ~119 occurrences, 2510.26824, RSC). This needs span
   evidence: a digit span with a smaller size and a raised baseline inside a body line. The text layer
   has already detached them, so use `PageText.spans` (size, bbox) to rebuild `^{n,m–k}` markers anchored
   to the preceding word, and only when the list is numbered and has no `[n]` markers in the main body.
2. **Comma-separated author-year clauses** (a3, 77 occurrences, 3 papers at 52–69%). In
   `author_year_markers`, split the inner text on `;` **and** on `,` when it follows a year token
   (`(19|20)\d{2}[a-z]?`) and precedes a capitalised name or particle. Keep `, 2022` / `,b` year
   continuations with the preceding author (see 7). Also skip layout debris lines (`⊥⊥`, `2/3`)
   between clauses.
3. **Numbered-list label integrity** (N3, 55 occurrences, all wrong or dropped, 2509.12458). Detect
   label-only lines (a column strip) and assign them to entry starts in order, or fall back to ordinal
   numbering. Add a warning when labels do not start at 1 or exceed the entry count. In eval, count
   only *correct* targets (join via `matches` to the cite keys), otherwise class (c) stays invisible.
4. **Page-break continuation** (a1 10 + N2 36 = 46 occurrences). Scan the concatenation of page N's
   tail and page N+1's head whenever a `(` or `[` is still open at the end of page N. Map offsets back to
   the page where the marker starts.
5. **Multi-token, particle and organisation names** (a4, ~40). Widen the name pattern in `clause_re`
   and `narrative_marker_re` to 1–4 tokens: lower-case particles (`de`, `van den`, `El`, `Della`),
   capitalised multi-word surnames (`Tchetgen Tchetgen`, `Hernandez Petzsche`, `Van Roy`, `De Vito`),
   organisations (`Qwen Team`, `Alibaba Cloud Qwen Team`, `Nomic AI`, `MONAI Consortium`,
   `Unitary team`), and a leading initial (`J. Wu`). Resolve by trying the full name, then its last
   token, then its first token against a by_author_year key built from the **whole** first-author
   surname string. Today `marker_surname` takes the first token while `author_surname` takes the last
   token, so `alibaba`≠`team` and `qwen`≠`qwen team`. Also give entries with `authors=[]` (e.g. `de Moura, L.; and Ullrich`) a proper author parse.
6. **Detached `(` and bare narrative forms** (a9, 25 after moving 2 to a1).
   - a9a (12): table cells `Duan et al 2020`. Accept `et al` without a period, and accept a bare `Name et al\.? ,? year` when that author-year exists in the list.
   - a9c (3): bare narrative, handled by the same rule.
   - a9b (~10): a `(` separated by an interleaved float, footnote or column is a reading-order problem upstream. The bare-clause rule also recovers the `Name et al., year)` tails, at low false-positive risk.
7. **Year lists, letter lists and locators** (a5 + a6 + a7, ~14 occurrences / ~12 keys). After a
   parsed `Name year`, consume `,\s*[a-z]` (same year, next letter) and `,\s*(19|20)\d{2}[a-z]?`
   (same author, next year) as extra targets. In the narrative pattern, allow `(1994, Theorem 3.1)` /
   `(2023a,b)` / `(2018, 2019)`; the regex now requires `)` right after the suffix.
8. **Three or more authors** (a2, 4): `Name, Name,? (and|&) Name,? year`, taking the first surname.
9. **Reference list without a heading** (N4, 2309.10334, a whole paper). In
   `find_reference_section`, fall back to the last run of consecutive `[1]`, `[2]`, … entry starts at
   the end of the document.

Minor: 2 of 133 single-suffix markers across dev60 resolve to a different same-year entry
(2603.04445 `Wu et al. (2025b)`, 2505.22973 `(Chung et al., 2022b)`). The suffix is chosen by list
position, not by the letter printed in the entry. Parse the entry's printed suffix (`(2023b)` /
`2025b.`) into `RefIndex` and match on it.
