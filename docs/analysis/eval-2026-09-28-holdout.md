# Holdout evaluation: failure analysis (lopdf backend, 2026-09-28)

Source: the 10 holdout dumps and `report.md` from the `eval-ubuntu-24.04-arm` artefact
(backend `lopdf`). None of these papers was used for tuning. For comparison I used the 20
dev dumps from after loop 2 (`eval-art4`). Numbers come from throw-away Python scripts run
over the dumps.

**Evidence labels.** Everything is from the dumps unless it carries one of these labels:
- **[src]**: the cached e-print source (`scratchpad/corpus-cache/<id>.src`).
- **[pdftotext]**: text from `pdftotext` on the cached PDF. The dumps do not include body
  page text, and `reference_section_text` is capped at 60 kB. This is a proxy, not lopdf
  output.
- **[bbox]**: coordinates from `pdftotext -bbox`.
- **[sim]**: a Python re-implementation of the current Rust logic, plus a proposed rule, run
  over the raws of all 30 dumps. The re-implementation of `find_doi` reproduces the
  report's DOI counts exactly (39/73, 39/42, 6/22).

All code references are to the current `src/citations.rs` and `src/latex_refs.rs`, cited by
function name.

## 0. Headline arithmetic

| | value |
|---|---|
| truth / extracted / matched refs | 677 / 709 / 667 |
| recall / precision | 98.5% / 94.1% |
| without 2608.14461 (truth bug, §1) | 667/668 = **99.85%** / 667/667 = **100%** |
| count exact | 8/10. The two misses are 2608.14461 (§1) and 2505.12196 (one merged entry, §5) |

**Segmentation generalised to the unseen styles.** Every recall, precision and count-exact
deficit comes from one truth-loader bug and one merged entry. Three things did *not*
generalise:
- **Titles, 82.9%.** A single unseen style, Springer LNCS, accounts for 78 of the 114
  failures.
- **Printed DOIs, 70.6%.** A DOI-joining rule from loop 1 glues back-reference page numbers
  onto the DOI.
- **Markers, 87.7%.** This number is not a recall figure (see §3). Pages after the
  bibliography are never scanned, and the count includes false positives.

---

## 1. arxiv:2608.14461 (9 truth / 42 extracted / 0 matched)

**Both the truth and the extraction are wrong.** Each one saw a different half of the
paper's two bibliographies.

**What the PDF has.** A 42-entry LNCS list ("References", pages 17–18) and a second list,
"References for the Appendices", entries 43–51, on page 39 **[pdftotext]**:
```
References for the Appendices
43. Arnborg, S., Proskurowski, A.: Linear time algorithms for NP-hard problems restricted to partial k-trees. …
…
51. Micali, S., Vazirani, V.V.: An O( V E) algorithm for finding maximum matching
```
The source uses `\usepackage{multibib}` and `\newcites{app}{References for the Appendices}`.
It builds the main list from `\bibliography{references}` and the appendix list from
`\bibliographyapp{references}`. The tarball ships only `app.bbl`, which holds 9 `\bibitem`s.
There is no `main.bbl` **[src]**.

**The truth is wrong.** `latex_refs::ground_truth` uses the `.bbl` files whenever any exist.
It therefore took `app.bbl` alone: `ArnborgP1989`, `Chlebikova2002`, … `MicaliV1980`.
- It never considers `references.bib`, which has 128 entries, filtered by the 43 main
  `\cite` keys.
- `\citeapp{…}` is not in `CITE_COMMANDS`, so the appendix citations are not counted either.

This is not the bib-cited filter and not an unrecognised `\cite` variant in the main text.
It is a bibliography held in a separate file, and only one of the two files shipped.

**The extraction is right for the main list and misses the second list.** All 42 main
entries are cleanly segmented. Examples:
- `1. Agarwal, P., Agarwal, H., Raj, V., Nath, S.: Harmonious balanced partitioning of a network of agents. In: AAMAS ’25. pp. 41–49 (2025)`
- `42. Wright, M., Vorobeychik, Y.: Mechanism design for team formation. In: AAAI ’15. pp. 1050–1056 (2015) …`

The appendix list is not extracted, for two reasons:
- `heading_re` only accepts a bare `References` / `Bibliography` line.
- `find_reference_section` keeps only the **last** matching heading.

The extractor has three smaller defects on this paper:
- 42/42 extracted titles are venue fields, for example `In: AAMAS ’25`, `Soc`, `Artif`
  (LNCS class, §4a). They are not scored because nothing matched.
- Running headers are glued onto entries (§5):
  `#20 … (2024) Šimon Schierreich and Ildikó Schl`,
  `#42 … (2015) Individual Rationality in Constrained Hedonic Games`.
- There are 12 false `[1]` markers from `W[1]-hard` (§3).

**Fixes.**
- *Truth.*
  - When the `.bbl` keys cover fewer than about 50% of the cited keys, add bib-cited entries
    for the keys it does not cover.
  - Count `\cite<name>` for every name declared with `\newcites{<name>}`.
  - Emit a truth warning whenever cited-key coverage is below 50%. This would have flagged
    the paper as soon as it was loaded.
- *Extractor.* Collect **every** reference list, not just the last. A list start is a short
  line beginning `References|Bibliography` (trailing words allowed) whose next non-empty
  line is a numbered label or an author-year entry start.
- **Trap.** Widening `heading_re` alone would make the last-heading rule select only the
  9-entry appendix list and drop the correct 42.
- **Dev check (prediction, no dump yet).** New dev paper 2504.10389 shows two `References`
  headings in **[pdftotext]**, at line 2801 (`Aminian MR, …`) and line 4824
  (`Johnson DS, Garey MR (1979) …`). New dev 2509.24852 uses multibib with 3 `.bbl`
  files **[src]**. Verify both on the next dev run.

## 2. arxiv:2604.01549 (39 of 71 printed DOIs)

Of the 73 truth DOIs, 71 are printed. The 2 that are not are `Bluestein2017…` and
`Fraser2011…`; their entries end in `ISSN 15251594. 2` and `ISSN 13504533.` with no DOI.
The 32 printed failures fall into two classes:

| class | n | example (raw tail → extracted `doi`) |
|---|---|---|
| **Back-reference page list glued on** | 31 | `doi: 10.1016/j.jcp.2017.08.039. 4` → `10.1016/j.jcp.2017.08.039.4`; `doi: 10.1002/cnm.3639. 2, 3, 8, 11, 18, 21` → `10.1002/cnm.3639.2`; `doi: 10.1016/ j.finel.2010.01.007. 3` → `…007.3` (the wrap join itself worked) |
| truth-side `{\_}` | 1 | `Rygiel2023…`: the `.bib` has `10.1007/978-3-031-43990-2{\_}73`; the truth DOI is cut to `…43990-2`. The PDF prints `{\_}` literally: `doi: 10.1007/978-3-031-43990-2{\_}73. 20` |

None of the forms the task asked about applies here: plain line wraps, `https://doi.org/`
inside parentheses, `<>`, `;` and trailing punctuation all parse fine. The paper uses
hyperref `backref`: `unsrtnat` plus backref prints each entry's citing pages after the
entry **[src]**.

**Mechanism.** `extend_across_wraps` joins the next token when the DOI so far ends in `.`
and the token starts with a digit. The DOI's closing period followed by ` 4` looks exactly
like a wrapped `…2018.` / `00045`. `trim_trailing_punct` then only removes the `,`.

The two other holdout papers with printed-DOI failures add 3 more:
- **2603.29003, numeric-only continuation line dropped as a page number (2).**
  `reference_section_text` has `https://doi.org/10.1016/j.jmp.2013.05.\n005\nNiedermann, …`,
  and likewise `…tics.2020.05.\n007`. The extracted raw ends `…j.jmp.2013.05.` and no
  entry contains `005`. In `section_lines`, `page_number_re` (`^\s*\d{1,4}\s*$`) drops any
  numeric-only line anywhere on the page, not just in the margin.
- **2603.29003, hyphen dropped inside a URL (1).** The page text is
  `https://doi.org/10.1214/14-\nsts504`; the raw has `…/10.1214/14sts504`.
  `hyphen_break` treats `14-`/`sts504` as a word break: neither form occurs in the context,
  and neither half is in the compound lists.

**Proposed rules, all generic.**
1. **Pure-digit token after a DOI-final `.`.** Join it only if it has a leading zero, has 4
   or more digits, or contains a `.`. **[sim]** over all 30 dumps: **+31 fixed, 0
   regressions**. Loop-1 dev cases still join: `SC.2018. 00045.` (2603.21379),
   `SoCG. 2023.2.` (2108.04588), `364399 1.3648400` (2509.10402),
   `jbiomech.2025. 112670.`. The 2603.29003 `05. 005` also joins, once rule 2 keeps the line.
   An alternative with the same **[sim]** result (+31/0) works at document level: when at
   least 30% of a list's raws end in `[.)] \d{1,3}(, \d{1,3})*`, strip that tail before
   parsing and keep it as back-references. It also stops the tail from reaching titles,
   years and pages. In dev, 2412.11061 and 2504.10389 use `backref` **[src]**; neither has
   a dump yet.
2. **Page-number lines.** Drop a numeric-only line only when its value equals the page's own
   `page.page` number. Do not use "only in the edge band": §5 shows the fixed 8% band misses
   real furniture. `005` on a reference page survives; a real folio still goes.
3. **Hyphens in identifiers.** In `hyphen_break`, always keep the hyphen when the previous
   token is inside a URL or DOI (starts `http`, or contains `10.\d+/`). This affects 1
   holdout DOI. In dev, 2507.08599 has `https://tor-\nlattimore.com/…` and 2511.13979 has
   `…/personality-\npairing.`; both are URLs, not DOIs.
4. **Truth.** Unescape `{\_}`, `\_` and `{\&}` in `.bib` DOI fields.

Expected effect: holdout printed-DOI accuracy goes from 84/119 (70.6%) to 118/119, and to
119/119 with rule 4.

## 3. arxiv:2505.11298 (42 of 132 cite commands) and the marker metric

**The missing 90 are almost all in the appendix, not a marker style.** From the source
**[src]**:
- 44 of the 132 commands come before `\bibliography{example_paper}` (line 1384).
- 88 come after it: `\appendix` is at line 1620, and the appendix runs to page 48.

`find_citation_markers` skips every page after the reference heading's page (`continue`
when `page.page > s.first_page`). Before the bibliography, 42 of 44 commands are found:
- natbib `(Scarselli et al., 2009; Bronstein et al., 2017)`
- narrative `Bechler-Speicher et al. (2024)`
- line-split forms such as `(Xu\net al., 2019)` and `Morris\net al. (2023a)`

Superscripts, `[3–7]` ranges and `\citet` narrative forms are not the problem.

Commands after the bibliography across all 10 holdout papers **[src]**, computed the same
way as `truth_cite_commands` (the totals match the report exactly):

| paper | commands | after bibliography | markers |
|---|---|---|---|
| 2505.11298 | 132 | **88** | 42 |
| 2505.22850 | 110 | 22 | 88 |
| 2602.17044 | 120 | 11 | 111 |
| 2603.29003 | 95 | 10 | 70 |
| 2505.12196 | 84 | 8 | 78 |
| others | | 0 | |

In total, 139 of the 1016 truth commands cannot be found by design. Among the 60 dev
papers, 26 put an appendix after the bibliography **[src]**, for example 2306.11313,
2412.06210, 2502.00857, 2509.08395 and 2601.13206.

**Other missed styles (holdout).**
- **Bracket with a note, `[22, Theorem 4]`.** 2510.00443 has 43 `\cite[…]{…}` commands
  **[src]** and 37 printed `[n, Theorem|Lemma|Corollary|…]` forms **[pdftotext]**.
  `numeric_marker_re` accepts only digits, ranges and separators, so none are found.
  - Rule: allow an optional `, <note>` tail after the last number, where the note is a
    word from {Theorem, Lemma, Corollary, Proposition, Section, Sec., Chapter, Ch., Eq.,
    p., pp., Def., Remark, Appendix, Fig.} followed by up to about 20 characters.
  - Dev numbered papers with `\cite[note]`: 2603.21379 (11), 2503.15734 (8),
    2108.04588 (5) **[src]**.
- **Author-year names with particles or two words.** `(van Rijn et al., 2021)`,
  `(de Leeuw,\n2014)` and `(Silva Filho et al., 2023)` in 2603.29003 **[pdftotext]** find no
  marker.
  - Cause: `clause_re` and `narrative_marker_re` require an uppercase first character and a
    one-word surname. The index keys come from the first author's surname (`de Leeuw`,
    `Silva Filho`).
  - Rule: allow a lowercase particle prefix
    `(?:(?:van|von|de|der|den|da|di|du|dos|del|la|le)\s+)*`, and allow a second capitalised
    word before `et al.` / `,`. Look the name up with the existing suffix match
    (`s.ends_with(" {needle}")`) extended to a prefix match.
  - Dev: 2506.23487 `(van der Vaart, 1998)` and 2508.02208 `(de Moura and Ullrich 2021)`
    **[pdftotext]**.
- **Detached accents in body text.** `(Markovi´c et al.,\n2021; Wakayama & Ahmed, 2023; …)`
  resolves only `[71, 72]`: `´` breaks `[\p{L}'’\-]+`. The fix is the accent composition
  from loop-1 §10, applied to body text too.
- **Unexplained.** `(Mussati et al., 2025)` on page 24 of 2603.29003 **[pdftotext]** looks
  matchable (entry #41 `Mussati, B. L., …(2025)`), but no marker was found. The lopdf body
  text is not dumped, so I cannot attribute it.

**The metric is a count ratio, not recall.** `marker_recall = resolved markers / truth
commands`, and the numerator is inflated:

| paper | ratio | inflation |
|---|---|---|
| 2412.00571 | 284/238 = 119% | IEEEtran/`cite` prints `\cite{a,b}` as `[17], [18]`, which is 2 markers for 1 command; 46 excess, 31 adjacent pairs such as `[17]`,`[18]`, `[41]`,`[42]` |
| 2510.26060 | 49/38 = 129% | same (`[1]`,`[2]`, `[7]`,`[8]`) |
| 2608.14461 | 49/43 | 12 markers `[1]` on pages 3 and 14 that are `W[1]-hard` / `W[1]-hardness` (**[pdftotext]** has 12 `W[1]` on pages 1–17) |
| 2510.00443 | 81/116 | 8 markers `[0, 1]`: math intervals, partially resolved to ref 1 because item `0` is skipped silently |

The "Marker resolution 100%" line hides all of this, because a marker counts as resolved
if any item resolves.
- **Guards**, which are needed *before* scanning appendix pages, since appendices are
  math-heavy:
  - reject the whole numeric marker if any item does not resolve (a `0`, or a number above
    the largest label);
  - reject a marker whose `]` is immediately followed by `-` and a letter (`W[1]-hard`).
  - Do **not** reject glued `Word[n]` in general: dev 2305.13843 has about 125 real ones,
    such as `PEPNet[43], MoME[44]` **[pdftotext]**.
  - Dev papers with `W[1]`: 2510.07065 (20) and 2602.02748 (10) **[pdftotext]**.
- **Eval.** Merge adjacent numeric markers (`[a], [b]`, `[a]–[b]`) into one group, or
  better, match marker target sets against each command's key set, so the ratio becomes a
  real recall.
- **Scan scope.** Scan every page except the lines between each reference heading and the
  end of its list, as `segment_entries` already determines it.

## 4. Title failures (553/667 = 82.9%; 114 wrong)

The denominator leaves out 2608.14461, whose 42 LNCS titles are all wrong but unmatched.
Counting it, the holdout has 120 LNCS entries.

### 4a. Springer LNCS `Surname, I., Surname, I.: Title. Venue`: 78 (all of 2505.22850)

`splncs04` never appears in dev. Dev 2508.19485 (`spmpsci`) prints the same form **[src,
pdftotext]**: `1. Badawi, D., Pan, H., Cetin, S.C., Enis Çetin, A.: Computationally …`.

Extracted versus truth:

| # | extracted title | truth |
|---|---|---|
| 6 | `In: Advances in Neural Information Processing Systems (NeurIPS) (2024)` | `CountGD: Multi-Modal Open-World Counting` |
| 39 | `European Conference on Computer Vision (2024)` | `Grounding DINO: Marrying DINO With Grounded Pre-training …` |
| 69 | `arXiv preprint arXiv:2309.13097 (2023)` | `Zero-shot object counting with language-vision models` |
| 27 | `IEEE International Conference on Computer Vision (ICCV) pp. 2980–2988 (2017)` | `Mask R-CNN` (raw `He, K., …, Girshick, R.B.: Mask r-cnn. 2017 IEEE …`) |

- **Rule that failed.** The year is last, in parentheses `(2024)`. `author_terminator` skips
  every `. ` that follows an initial (`N.`, `T.`, `A.`) and stops at the first
  non-initial `. `, which is the one after the title. It never treats `:` as the end of the
  author list. The title is then read from the segment after that period, which is the
  venue.
- **Rule.** If the entry, after its label, matches
  `^(Surname, (I\.)+(-I\.)*, )*(Surname, (I\.)+|et al\.):\s`, the authors end at that `:`.
  The title runs from there to the first `. ` that is not an abbreviation.
- **Same class in 2608.14461 (42/42, unscored).** `Harmonious balanced partitioning of a
  network of agents` is extracted as `In: AAMAS ’25`.
- **The swapped pairs are a side effect, not a separate bug.** 8 of the 78 have a truth
  title that is not in the matched raw, and 6 of those are pairs swapped by the eval's
  author-year fallback (`dave`↔`geco` Pelhan 2024, `zhizhong2024point`↔`huang2024count`,
  `amini2025open`↔`amini2025countgd++`). The title pass has nothing to match on, so they
  should come right once LNCS titles parse.

### 4b. Hyphen at a line break: 12

| paper # | extracted | truth | page text |
|---|---|---|---|
| 2412.00571 #80 | `Dualchannel deepfake …` | `Dual-channel …` | `“Dual-\nchannel` |
| 2412.00571 #91 | `Multi-path gmmmobilenet …` | `… GMM-MobileNet …` | `gmm-\nmobilenet` |
| 2412.00571 #142 | `… modalityinformation …` | `modality-information` | `modality-\ninformation` |
| 2505.12196 #8 | `An opensource autoregressive …` | `Open-Source` | `open-\nsource` |
| 2505.12196 #22 | `Lexicalsemantic content …` | `Lexical-Semantic` | `Lexical-\nsemantic` |
| 2510.00443 #15 | `… early faulttolerant …` | `fault-tolerant` | `fault-\ntolerant` |
| 2602.17044 #34 | `Toward nextgeneration …` | `next-generation` | |
| 2604.01549 #21 | `… heartarterial …` | `heart-arterial` | |
| 2412.00571 #151 | `Music inter-ventions …` | `interventions` | `inter-\nventions` |
| 2505.12196 #26 | `… pre-dicts fMRI …` | `predicts` | |
| 2604.01549 #20 | `svZeroD-Solver` | `svZeroDSolver` | `svZeroD-\nSolver` |
| 2604.01549 #85 | `CenterlinePoint-Net++` | `CenterlinePointNet++` | |

- **Rule that failed.** `hyphen_break`:
  - The first 8 fall through to the final `Drop`: the pair is not in the context, and
    neither half is in `COMPOUND_PREFIXES` / `COMPOUND_HEADS`.
  - `inter-` and `pre-` are wrongly kept by the prefix list.
  - The last two are kept by the uppercase-next rule.
- **Whole-document context does not rescue this class.** Using `pdftotext -bbox` word lists,
  where line-end fragments are separate words, the unbroken compound occurs elsewhere in
  the paper for only 2 of 12 (`interventions` once, `svZeroDSolver` once) **[bbox]**.
- **Candidate rule (unverified).** Drop when the second half starts with a common
  syllable-continuation suffix: `tion, sion, ing, ment, ity, ive, ical, ence, ance, ture,
  guage, hension, ization`, and so on. Otherwise keep when both halves are at least 4
  letters. Simulate this on the loop-1 dev boundary set (168 cases) before adopting it.
- **Keep the uppercase-next rule.** It is right far more often than the 2 cases above.

### 4c. Truth-side or unfixable: 9

- **`.bbl` without `\newblock` keeps `, volume N` in the title (4, 2510.00443).**
  - `Accuracy and stability of numerical algorithms, volume 80` versus the correct extracted
    `Accuracy and stability of numerical algorithms`.
  - Also #10, #29, #55.
  - Fix in `bbl_title`: cut `,\s*volume \d+$`.
- **Math in titles (3).**
  - 2510.00443 #11 truth `… a_n ^4 and a_n+ 1- a_n ^2 …`.
  - #62 `H^p` versus the extracted `H spaces`, where the superscript was lost.
  - 2505.11298 #31 `I^2-GNNs` versus `I -GNNs`.
- **Ambiguous (2).**
  - 2604.01549 #1: the bib title includes `. Its Translation to the Clinic/Bedside`.
  - #76: figure glyphs `A A0 A` sit inside the title on both sides.

### 4d. Parser rules that failed on unseen forms: 15

| paper # | extracted | truth | rule that failed → generic fix | dev example |
|---|---|---|---|---|
| 2505.11298 #7 | `Graph Neural Networks Use Graphs When They Shouldn’t. International Conference on Machine Learning (ICML), 2024.` | `… Shouldn't` | `period_is_abbreviation`: `word_before` stops at `’`, so `t.` reads as an initial. Fix: a letter preceded by `’`/`'` is not an initial | none in dumps |
| 2412.00571 #31 | `C.-i. Wang` | `Tonet: Tone-octave network …` | comma-field name test rejects the lowercase hyphenated initial `C.-i.`, so this field becomes the title. Fix: accept `\p{Lu}\.-\p{Ll}\.` | 2305.13843 #152 `X.-m. Wu`, 2509.08395 #3 `W.-t. Yih` (same wrong title) |
| 2412.00571 #24 | `A. Dabro˛ wski` | `Technical, Musical, and Legal Aspects …` | a detached ogonek plus a space splits the name. Fix: accent composition (loop-1 §10) | loop-1: 56 raws with detached accents |
| 2412.00571 #25 | `[Online]` | `Foundation models for music: A survey` | 43-name IEEE list: the comma-field scan took the quoted field `“Foundation models …` as the 43rd author. Fix: a field that opens with a quote ends the author list | not checked |
| 2412.00571 #23 | `Music` | `Music, Subjectivity, and Schumann` | IEEE unquoted book title containing commas. Fix: when there are no quotes, the title after the author list runs to the first `. `, not the first `, ` | loop-1 §5 C (2503.15734 #21) |
| 2604.01549 #87 | `Hao Su` | `PointNet: Deep Learning on Point Sets …` | mixed-format list `R. Qi Charles, Hao Su, Mo Kaichun, and …`: the initials-first test on field 1 does not carry over to full-name fields. Fix: accept any field that is 2–4 capitalised tokens as a name | none found |
| 2505.11298 #43 | `Acta Mathematica Academiae Scientiarum Hungarica, 18:321–328, 1967.` | `Operations With Structures` | `L´ aszlo´ Miklos´ Lovasz.´ Operations …`: detached accents after the period hide the author/title boundary. Fix: accent composition | as above |
| 2505.11298 #17, #18 | `On the Holder Stability …`, `Lovasz Meets …` | `Hölder`, `Lovász` | `section_lines` drops accent-only lines (`is_accent_only`), so the umlaut or acute is lost rather than composed. Fix: compose into the base glyph in reading order; eval could also fold diacritics | 2608.28714 (31 detached accents, loop-1) |
| 2505.12196 #14 | `What are large language models mapping to in the brain` | `… in the Brain? A Case Against Over-Reliance on Brain Scores` | the title ends at `?`. Fix: continue past `? ` when the next sentence is capitalised and the one after it starts a venue cue (`Preprint`, `In`, `arXiv`, `Journal`, …) | not checked |
| 2603.29003 #21, #49 | `… design [Doctoral dissertation, University of Oxford]`, `… Memory [Pyro Tutorial].` | without the bracket | APA bracketed descriptor. Fix: strip a trailing `\[[^\]]+\]` from the title | 2511.13979 #25 `… [analysis code]` |
| 2604.01549 #8, 2602.17044 #1 | `…assist devices, 4 2011`, `Adobe lightroom community presets, 2025` | without the date | the title ends at `. ` after a date. Fix: strip a trailing `, (\d{1,2} )?(19\|20)\d\d` | none in dumps |
| 2510.00443 #7 | `Orthogonal polynomials` | `Orthogonal polynomials. II` | `. II.` ends the title. Fix: a Roman-numeral sentence of 1–4 characters continues the title | none |

## 5. Segmentation elsewhere

- **Under-segmentation: 1.** 2505.12196 #29 ends
  `…Neurobiology of Language, 5(1):107–135. nostalgebraist. 2020. Interpreting GPT: The logit lens. Blog Post. Retrieved: 2026-03-15.`
  - Truth `nostalgebraist2020logit` is unmatched.
  - Cause: `segment_author_year` requires `entry_start_re` (an uppercase letter, a quote,
    a bracket or a particle) even when `layout_starts` says `Some(true)`.
  - Rule: with `Some(true)` from the layout, allow a lowercase start when the previous entry
    `ends_like_entry` and the candidate has a year within 2 lines. The evidence guard
    already protects against fragments.
  - Dev: none in the 20 dumps (the grep for a lowercase author before `. YYYY.` found only
    this entry).
- **Over-segmentation: 0** in the holdout. There are no fragment entries, and every
  extracted index in 8 papers matches.
- **Running headers glued into entries: 7 entries.** Examples:
  - 2505.22850 #17, #47, #78 `… (2023) What is the Right Embedding Space for Contrastive Learning in REC?`
  - 2505.22850 #31, #64 `… (2024) K. Triaridis et al.`
  - 2608.14461 #20, #42 (quoted in §1)
  - 2510.26060 #22 `… published by USENIX. Keshvadi et al.: Performance Analysis of Dynamic Equilibria in Joint Path Selection`

  Why they survive:
  - The repeated-furniture rule only looks at lines with `edge` set (top or bottom 8% of
    the page). LNCS running headers sit at y = 91–103 pt from the top of a 792 pt page,
    11.5% down **[bbox]** (2505.22850 page 17, 2608.14461 page 18).
  - Repetition is counted only on the reference pages, and alternating even/odd headers
    appear only 2–3 times there. The 2510.26060 header appears once in the section.

  Rule: detect running headers over the **whole document**. A top- or bottom-most line
  whose digit-normalised text (`digit_key`) repeats on at least 3 pages is a header. This
  replaces the fixed 8% band for that purpose. Loop-1 dev had the same symptom on
  2412.06210 #44 and 2305.13843 (`Page 30 of 35`).
- **Row-order artefacts: 2 small ones.**
  - 2510.00443 #31 `… arXiv:2506.20484, 2025. arXiv preprint`: the words are printed before
    the ID but ordered after it.
  - #61 ends in a stray `p`: the superscript of `H^p` in #62 was sorted into the previous
    row.
  - These are `reading_order::line_top_first` sorting by bbox top (loop-1 §9). Low priority.

## 6. Style-generic or paper-specific

| failure class | holdout impact | generic? | dev papers with the same class |
|---|---|---|---|
| Truth from a partial `.bbl` set (multibib, missing `main.bbl`) | 2608.14461: recall 9, precision 42 | generic (truth loader) | 2509.24852 (multibib, 3 `.bbl`) |
| Only the last reference list is extracted | 2608.14461: 9 entries | generic | 2504.10389 (two `References` headings, **[pdftotext]**) |
| Appendix after the bibliography is not scanned for markers | 139 commands | generic | 26 of 60 dev papers, e.g. 2306.11313, 2412.06210, 2502.00857 |
| `[n, Theorem k]` markers | about 37–43 commands (2510.00443) | generic | 2603.21379, 2503.15734, 2108.04588 |
| False markers `[0, 1]`, `W[1]-hard` | +20 markers | generic | 2510.07065, 2602.02748 (`W[1]`) |
| Particle or two-word surnames in markers | ≥ 4 markers (2603.29003) | generic | 2506.23487, 2508.02208 |
| Back-reference tail glued onto the DOI | 31 DOIs | generic | backref: 2412.11061, 2504.10389 (no DOIs checked) |
| Numeric-only line dropped as a page number | 2 DOIs | generic | not checked |
| URL/DOI hyphen dropped at a wrap | 1 DOI | generic | 2507.08599, 2511.13979 (URLs) |
| LNCS / Springer `Surname, I.: Title.` | 78 titles (+42 unscored) | generic, common | 2508.19485 (`spmpsci`) |
| Compound hyphen dropped at a line break | 12 titles | generic | loop-1: 51 keep cases on dev |
| Running headers outside the 8% band | 7 entries | generic (LNCS, IEEE journals) | loop-1: 2412.06210, 2305.13843 |
| Lowercase hyphenated initials `C.-i.` | 1 title | generic | 2305.13843, 2509.08395 |
| `n’t.` read as an initial | 1 title | generic | none seen |
| Detached accents (dropped lines, split names) | 4 titles, 1 marker | generic | 2608.28714, 2603.21379, 2503.15734 |
| APA `[descriptor]`, trailing `, 4 2011`, `? Subtitle`, `. II` | 6 titles | generic but rare | 2511.13979 (`[analysis code]`) |
| Lowercase author handle (`nostalgebraist.`) | 1 entry | generic but rare | none |
| `.bbl` `, volume N` in the truth title | 4 titles | truth-side | not checked |
| `{\_}` in a `.bib` DOI | 1 DOI | truth-side | not checked |
| Math and superscripts in titles | 3 titles | paper-specific / unfixable | none |

**Nothing above is holdout-specific.** Every rule is stated as a style pattern: LNCS colon,
back-reference tails, particles, `[n, note]`, multiple lists. None matches a holdout string.
For classes with no dev example, add a paper in that style to the enlarged dev split before
tuning; do not tune against these holdout papers. The classes are LNCS `splncs04`,
back-references printed after DOIs, and a lowercase handle author.

## Prioritised changes (largest measured holdout gain first)

| # | change | where | holdout effect | dev evidence |
|---|---|---|---|---|
| 1 | LNCS/Springer colon author terminator | `parse_entry` / `author_terminator` | titles 553 → up to 631 of 667 (≤ 94.6%); also resolves 6 swapped eval pairs | 2508.19485 |
| 2 | Truth: add bib-cited entries for cited keys missing from `.bbl`; `\newcites` commands; coverage warning. Extractor: collect every reference list | `latex_refs::ground_truth`, `find_reference_section` | 2608.14461 goes from 0/9/42 to about 51/51 matchable; recall and precision reach about 100% | 2509.24852, 2504.10389 |
| 3 | Scan markers after the reference list, **with** the whole-marker-must-resolve and `]-letter` guards; add `[n, note]`; eval groups adjacent `[a], [b]` | `find_citation_markers`, `numeric_marker_re`, `eval` | up to +139 commands in reach; removes about 20 false and about 57 split-count markers | 26 dev papers with an appendix after the bibliography |
| 4 | DOI: pure-digit token rule (or document-level backref stripping); page-number line = own folio only; keep the hyphen inside a URL | `extend_across_wraps`, `section_lines`, `hyphen_break` | printed DOIs 84 → 118 of 119 **[sim]** (31 of them), 0 dev regressions **[sim]** | 2603.21379, 2108.04588, 2509.10402 still join |
| 5 | Document-wide running-header detection | `section_lines` | 7 polluted entries | 2412.06210, 2305.13843 |
| 6 | Small parser rules: `’t.`, `C.-i.`, quote ends authors, APA `[..]`, trailing date, particles in markers, lowercase start with layout evidence, accent composition | `parse_entry`, `clause_re`, `segment_author_year`, `group_lines` | about 15 titles, 1 entry, ≥ 4 markers | see §4d, §5 |
| 7 | Hyphenation suffix rule (needs simulation first) | `hyphen_break` | up to 10 titles | loop-1 168-case set |
