# Reading order: body-alignment failure taxonomy (lopdf backend, 2026-09-28)

Source: the 60 dev-split dumps (`body_text_extracted` and `body_text_truth`) and `report.md` from the
`eval-ubuntu-24.04-arm` artefact (eval-art6, mean body alignment 0.611). Code references are to
`src/reading_order.rs`, `src/backend/lopdf_backend.rs`, `src/eval.rs` and `src/latex_refs.rs` on
`feat/accuracy-3`. Geometry claims were checked against `pdftotext -bbox-layout` on the cached
PDFs where the text says so. Nothing was compiled.

## Headline

**Reading order is not the main loss.** Body alignment is `2·LCS / (|ours| + |truth|)` over
lower-case word tokens (`eval::word_alignment`). Over the 60 papers:

| | tokens | matched | share |
| --- | --- | --- | --- |
| ours (`body_text_extracted`) | 942,403 | 518,712 | **55.0 % precision** |
| truth (`body_text_truth`) | 575,110 | 518,712 | **90.2 % recall** |

We recover nine truth words in ten. The score is held down by the ~424k extracted words that
have no counterpart in the truth: the bibliography, display math, table and figure interiors,
captions, and `\cite`/`\ref` output. Most of this is **eval scope**, meaning the truth builder
drops it by design (`latex_refs::body_text` removes floats, display math, cites, refs and the
bibliography). It is not an extraction error.

Ordering itself is bounded by oracles:

- Re-sorting every extracted **line** of every page into truth order raises the mean from 0.614
  to only **0.627 (+0.013)**.
- Re-sorting whole **blocks** gives +0.004.

The largest real extractor defects are:

- hyphenation left in place: +0.010;
- two-column pages read line-interleaved: ≤ +0.009, and half of these come from the rotated
  arXiv margin stamp;
- running headers and footers: +0.0035;
- sub/superscript line splits: +0.002 to +0.005.

### Method

- **Tokens and alignment.** Tokens are `eval::words` reimplemented in Python (Unicode
  alphanumeric runs, lower-cased). Alignment scores use an exact bit-parallel LCS with the same
  12,000-token even sampling as `eval.rs`. The recomputed per-paper scores match `report.md` to
  three decimals except for the two papers marked † below.
- **Truncated dumps (†).** `arxiv:2503.13415` and `arxiv:2510.26824` hit the dump's 200 kB
  `BODY_TEXT_CAP`, so their simulations run on truncated text. The recomputed mean is 0.6136
  against the report's 0.611, and all Δ values below are relative to 0.6136.
- **Word-level matches.** Which words matched comes from `difflib.SequenceMatcher`, which finds
  99 % of the true LCS on arxiv:2108.04588 (9,339 vs 9,442 words).
- **Line classes.** Each extracted line gets one heuristic class:
  - REF: inside `reference_section_text`;
  - HF: top or bottom 3 lines of a page, repeating on ≥ 3 pages or a bare page number;
  - STAMP: the arXiv stamp or a venue banner;
  - TITLE: page-1 lines before "Abstract";
  - CAPTION: a `Figure/Table/Algorithm N:` line and the rest of its paragraph;
  - FOOTNOTE: a marker-led line near the page bottom;
  - MATH: symbol-dense lines;
  - TABLE: numeric lines and runs of ≥ 3 short lines;
  - PROSE: everything else.
- **Gains.** Every "gain" is a simulation: the class is edited out of our text (or the edit is
  applied), both sides are re-aligned, and the per-paper scores are averaged.
  - Oracle gains are upper bounds.
  - Gains of separate rows are **not additive**; joint runs are given where it matters.
- **Scripts.** All scripts are in the session scratchpad (`ro/*.py`). They are not committed.

### Where the unmatched words are

| extracted line class | tokens | unmatched | share of all unmatched ours-side tokens |
| --- | --- | --- | --- |
| REF (bibliography) | 246,217 | 168,748 | 39.8 % |
| PROSE | 482,714 | 92,692 | 21.9 % |
| MATH | 114,118 | 82,332 | 19.4 % |
| TABLE (table and figure interiors) | 41,991 | 37,490 | 8.8 % |
| CAPTION | 36,505 | 29,599 | 7.0 % |
| HF (headers and footers) | 7,232 | 7,186 | 1.7 % |
| STAMP | 1,987 | 1,972 | 0.5 % |
| FOOTNOTE | 8,690 | 2,253 | 0.5 % |
| TITLE | 2,949 | 1,419 | 0.3 % |

On the truth side, 56,398 words (9.8 %) are unmatched:

| truth-side unmatched words | count | note |
| --- | --- | --- |
| math-like runs | 27,692 (49 %) | ≥ half the tokens are ≤ 2 characters, e.g. `u 1 u 2 v 1 v n` |
| present in our text but at another position | 14,532 (2.5 % of truth) | the real ordering signal |
| short word runs (≤ 3 words) | 7,706 | local noise around hyphens, markers and cites |
| other | 6,468 | truth noise, glyph loss, small-caps splits |

## Summary by class

Kind: **D** = extractor defect (our text is wrong); **S** = eval-scope asymmetry (our text is
right, but the truth omits it by construction); **T** = truth-side noise.

| # | class | kind | papers / pages affected | simulated Δ mean alignment | code path |
| --- | --- | --- | --- | --- | --- |
| 1 | column order (interleave, stamp merge, block swaps) | D | 22 interleaved pages in 15 papers; stamp merged on page 1 in 22 of 59 papers | **+0.0086** (line oracle on those 33 pages; all-pages line oracle +0.0131) | `reading_order::xy_cut`/`column_cut`, `find_line`; backend `emit` (no text direction) |
| 2 | headers, footers, page numbers | D | 2,320 lines, 1,433 pages, 58 papers | **+0.0035** (drop) | `order_page`: no header/footer pass |
| 3 | captions and table/figure interiors | S | captions: 4,937 lines / 577 pages / 54 papers; interiors: 24,976 lines / 1,038 pages / 60 papers; 68 floats split a sentence in 30 papers | captions **+0.0124**, interiors **+0.0168** | no zone tagging in `order_page`; truth drops floats |
| 4 | footnotes | D (minor) | 51 papers (heuristic) | moving them to the marker: **+0.0004**; dropping: −0.0020 | page-bottom placement by `xy_cut` |
| 5 | math | S (display) / D (scripts) | "display" = every MATH line not counted as a script fragment: 36,958 lines, including about 22k short fragments that are not next to prose; its drop changes the score of 59 papers. Script fragments = 1–3 tokens of ≤ 3 characters next to a prose line: 5,811 lines, in all 60 papers | display **+0.0183**; scripts +0.0034 (drop) to about +0.005 (reattach) | truth drops display math; `find_line` splits scripts |
| 6 | hyphenation and ligature/U+FFFD residue | D | 6,352 hyphen splits on 955 pages in 59 papers; 136 ligature-loss words in 4 papers | hyphens **+0.0104**; ligatures +0.0002 | no de-hyphenation in `order_page`; `check_unmapped` puts U+FFFD at the end of the span |
| 7 | accents | D | 118 spacing accents in 13 papers (49 are false: minus signs in 2602.02748) | +0.0000 | no composition code on this branch |
| 8 | paragraph breaks | D | 647 spurious mid-sentence breaks in 49 papers; ≥ 1,988 lost breaks in 60 papers | **0 by construction** | `separator` |
| 9 | rotated/vertical text and margin stamps | D | stamp on 59/60 papers (page 1); rotated axis labels in ≥ 13 papers | stamp drop +0.0010; merged-stamp pages via class 1 (+0.0049) | backend `emit` loses direction; `group_lines` |
| 10 | truth-side noise | T/S | macro and preamble junk in 8 papers; `\cite`/`\ref` removal in 60 papers; supplementary material in a second `.tex` in 1 paper | cleaning truth +0.0002; stripping cite/ref markers from ours **+0.0097** | `latex_refs::body_text`, `expand_macros`, `ground_truth` main-file choice |
| – | bibliography inside the body text | S | 57 papers | **+0.0389** (drop) | `eval.rs` aligns all page text |

## 1. Column order errors

**What happens.** A two-column region is emitted as one XY-cut leaf, which `xy_cut` then sorts
top-to-bottom (`idx.sort_by(top_first)`). Lines of the two columns then alternate. We count a
page as interleaved when ≥ 3 anchored lines (a unique 4-gram in the truth) jump back and return
within 3 lines, and those zigzags are ≥ 15 % of the page's anchored lines. That finds **22 pages
in 15 papers**.

**Stamp merge.** `pdftotext` shows that **11 of the 22 are page 1, where the rotated arXiv stamp
(x ≈ 18–36 pt, a tall vertical box) has been merged into a body line.** The stamp is merged into
a body line on page 1 in **22 of the 59** papers that carry it. The merged line box then runs
from the margin into the other column, so `column_cut` finds no clear gutter and `row_cut` finds
no full-width gap.

**Other pages.** The remaining pages are a second pattern:
- a full-width element with no clean horizontal gap above it (arxiv:2305.13843 p. 24:
  pdftotext shows a two-column band flowing into a full-width paragraph with overlapping y
  ranges);
- or a gutter bridge we could not see from the dump (arxiv:2508.19485 pp. 2–3,
  arxiv:2507.08599 p. 4).

On arxiv:2507.08599 p. 4 every pdftotext word lies in x 45–300 or 312–567 (a 12 pt gutter), yet
the whole page is interleaved. Some span box of ours must therefore bridge the gutter. Why that
happens could not be verified without running the backend.

**Why the stamp joins a line: unverified.**
- `find_line` should reject a 20 pt-wide box 270 pt away.
- The candidates are the stamp span's size or box, since `Span` carries no text direction, and
  the line's `size` becomes `max(size)` of its members, which inflates `LINE_REACH` and
  `BASELINE_TOLERANCE` for the whole line.

**Block swaps.** Outside the interleaved pages, 361 whole-block backward jumps remain in 52
papers. Most are floats placed at the top of a column before the text that cites them
(PROSE after CAPTION: 87), or reference and header blocks. Only 37 are prose-after-prose.

Examples (ours vs truth):

- arxiv:2508.19485 p. 2 (interleave, no stamp):
  - ours: `spatial module is designed to analyzes motion and spatial\nworks (CNNs) with Vision Transformers (ViT) to develop a\nlightweight model for gas leak segmentation. However, they\ncues of gas leaks using these enhanced features. The de-\nstruggle to capture motion cu…`
  - truth: `…with Vision Transformers (ViT) to develop a lightweight model for gas leak segmentation. However, they struggle to capture motion cues from faint and ambiguous targ…` and, separately, `…a temporal-spatial module is designed to analyzes motion and spatial cues of gas leaks using these enhanced features. The decoder…`
- arxiv:2507.14211 p. 1 (stamp merged):
  - ours: `arXiv:2507.14211v1  [cs.NI]  15 Jul 2025 via the introduction of Machine Learning (ML) or other\nhave been explored to support advanced applications like\nAbstract—Predictive Quality of Service (PQoS) makes it pos-` and further down `…countermeasures to avoid ser-\nI. I\nNTRODUCTION\nvice degradation [10].`
  - truth: `pqos makes it possible to anticipate QoS changes, e.g., in wireless networks, and trigger appropriate cou…` and `Introduction\n\nThe transition towards the next generation of…`

Gains (line-level oracle: every line of the selected pages sorted into truth order):

| pages re-ordered | pages | papers | Δ mean |
| --- | --- | --- | --- |
| page 1 with the stamp merged | 22 | 22 | +0.0049 |
| interleaved pages | 22 | 15 | +0.0078 |
| union | 33 | 24 | **+0.0086** |
| every page (upper bound for all ordering) | 1,705 | 60 | +0.0131 |

**Control for the stamp attribution.** The same line oracle on page 1 of the 38 papers whose
stamp is *not* merged gains 0.0003 per paper (+0.0002 on the mean). On the 22 merged papers it
gains 0.0133 per paper. Page 1 is not intrinsically disordered: the merged stamp is the cause.

Per paper, the union oracle is worth:

| paper | before | after |
| --- | --- | --- |
| 2508.19485 | 0.624 | 0.701 |
| 2412.11061 | 0.698 | 0.767 |
| 2603.19305 | 0.682 | 0.737 |
| 2509.10402 | 0.671 | 0.712 |
| 2604.03540 | 0.667 | 0.699 |

**Rule changes**

1. **Separate rotated spans.**
   - In `lopdf_backend::emit`, record whether the text direction is horizontal, i.e.
     `|full.b|, |full.c| ≪ |full.a|, |full.d|`. `Span` has no field for this today.
   - `group_lines` skips non-horizontal spans. `order_page` appends them as their own block,
     grouped along their own axis, the same way loose spans are appended.
   - This removes the stamp merge on 22 papers' first pages (≈ +0.005, from the oracle above,
     confirmed by the control) and turns reversed axis labels into words (class 9).
2. **Gutter with tolerance.**
   - When neither `row_cut` nor `column_cut` fires on a block wider than about 1.6× the median
     line width, retry `column_cut` while ignoring the few boxes (≤ 3 % of the block's lines)
     that cross the widest near-empty vertical band. Then place those boxes by y as rows of their
     own.
   - This does not cover the L-shape on arxiv:2305.13843 p. 24, where a whole full-width
     paragraph crosses the gutter. That case needs a second relaxation: allow a `row_cut` when
     the full-width block and the two-column band overlap by less than about one line height in y.
   - Upper bound for both relaxations: the rest of the +0.0086.
3. **Do not merge sizes across a line.** Contingent: it matters only if the unverified
   size-inflation mechanism is what merges the stamp.
   - `find_line` should use the span's own size for reach and tolerance, not `line.size.max(size)`.
   - One oversized member (stamp, big delimiter) then cannot pull in text 20 pt or more away.

## 2. Headers, footers, page numbers

`order_page` has no header/footer pass, so running heads and page numbers land in the page text.
A running head sits first in the page text (it is its own row), and a footer or page number sits
last. When a sentence crosses a page break, they land between its two halves.

Examples:
- arxiv:2504.10389:
  - ours: `…different demographic groups, experiences,\n\n1\n\x0c\n2\n\n:\nArticle submitted to Management Science; manuscript no. (Please, provide the manuscript number!)\n\nand perspectives so that…`
  - truth: `…different demographic groups, experiences, and perspectives so that its feedback reflects…`
- arxiv:2602.17690 pp. 1–2:
  - ours: `ture. To address these challenges, we propose DesignAsCode, a\nACM ISBN 979-8-4007-2213-4/2026/11\nhttps://doi.org/10.1145/3767308.3835962\n\x0c\nMM ’26, November 10–14, 2026, Rio de Janeiro, Brazil\n\nZiyuan Liu et al.\n\nnovel framework that reimagines…`
  - truth: `…we propose DesignAsCode, a novel framework that reimagines graphic design…`
  - The ACM permission block is also interleaved with the column text.

**Rule change.**
- Add a document-level pass after all pages are ordered. It takes lines in the top or bottom
  ~6 % of page height whose digit-normalised text repeats on ≥ 3 pages, or that are bare page
  numbers (`\d+`, roman, `Page n of m`), plus stamp-like banners.
- Tag those lines as header or footer and leave them out of the body text.
- Simulated gain: **+0.0035** (37 papers change). It also rejoins page-crossing sentences and
  hyphens, which class 6 then fixes.

## 3. Captions, table cells and figure text inside paragraphs

A float is emitted where it physically sits, which is usually between two halves of a sentence.
The truth drops `figure`, `table`, `tabular` and `algorithm` environments, captions included
(`latex_refs::is_dropped_env`). A caption block with a table or math block beside it lands
between a sentence-ending-less prose line and a lower-case continuation **68 times in 30
papers**. Short-line and numeric float interiors split prose far more often.

Examples:
- arxiv:2601.12491 p. 6:
  - ours: `Value-Augmented prompting improves calibra-\n\x0c\nModel\ncomment-only\n+community\n…\nGPT-4o-mini\n0.56\n0.38\n…\nTable 3: Context ablation across mod…` and then `tion but not discrimination: …`
  - truth: `Value-Augmented prompting improves calibration but not discrimination: Value-Augmented prompting…`
- arxiv:2511.15503 p. 11:
  - ours: `layer with head count 32, input size 128 and various output\n\ner GPU\n6\n\nv 4\nSpeedup o\n2\n1\n0\n\n6.69\n\n3.35…` and then `Fig. 9: Speedup of HBM-PIM and HBM-PIM+` and `sizes. In both PIM backends,`
  - truth: none of the chart text or the caption.

A large share of PROSE-labelled misses is also float content. In arxiv:2509.04183, pp. 35–46 are
prompt listings inside `figure*`/`tcolorbox` (about 10k words): we extract them and the truth
drops them.

**Rule change (a zone tag, not deletion).**
- In `order_page`, after `xy_cut`, classify each leaf block:
  - *caption*: first line matches `^(Figure|Fig\.|Table|TABLE|Algorithm|Listing)\s*\w?\d+[:.]`;
  - *float interior*: ≥ 3 consecutive lines of ≤ 5 words with no sentence punctuation, or ≥ 50 % numeric tokens.
- Store the zone on `Line`, keep it in the output, and have `eval.rs` align only body zones.
- Captions alone: **+0.0124** (52 papers). Interiors alone: **+0.0168** (58 papers). Jointly with
  display math (class 5): **+0.0495** (0.6136 → 0.6631).

## 4. Footnotes

Footnote text is placed at the bottom of its page (column), while the truth inlines
`\footnote{}` at the marker. The effect is small. Moving every footnote line to its truth anchor
gains only **+0.0004** (15 papers change), and dropping them costs −0.0020 because the truth
does contain them. Recommendation: **no change**. Keeping footnotes at the page end is the right
reading order for a product. (The FOOTNOTE line class is heuristic and also catches some prose;
the 51-paper count is an upper bound.)

Examples:
- arxiv:2507.14211 p. 5:
  - ours: `…ultimately affect the user’s experience.\n3Notice that, in the 3GPP specifications [47], the term “packet reception\nprobability” is referred to as “reliability.”.\n\x0c\n6`
  - truth: `…the minimum tolerable prp, as specified in Notice that, in the 3GPP specifications …` (inlined at the marker)
- arxiv:2509.04183 p. 34:
  - ours: `We set seed 42 for reproducibility. The Hugging\n9\nFace , vLLM, and DeepSpeed libraries…` in the body, and `…8\nhttps://github.com/microsoft/DeepSpeed\n9\nhttps://huggingface.co/\n\n34` at the page bottom
  - truth: `The Hugging Facehttps://huggingface.co/, vLLM, and DeepSpeed libraries…`
  - The truth inlines `\footnote{\url{…}}` and glues it to the previous word, which is also a
    class 10 case. The marker `9` splits `Hugging Face` in ours, which is a class 5 script
    fragment.

## 5. Math (quantified so it is not mistaken for order errors)

Math has three parts:

1. **Display math** is dropped by the truth (`equation`, `align`, `\[…\]`) and kept by us. Here
   that means every MATH line that is not a script fragment next to prose: 36,958 lines,
   including about 22k short fragments that sit away from prose. Dropping them changes the
   score of 59 papers. MATH lines hold 19.4 % of all unmatched words on our side. Dropping the display lines
   gains **+0.0183**. Scope, not a defect.
2. **Inline math** is kept raw by the truth (`G^hom(A)`, `d_W(X,Y)=_hLip_1 E[…]`, with `\sup`,
   `\alpha` and the like deleted). It accounts for **49 % of all unmatched truth words** (27,692).
   These cannot match glyph output well and should not be read as order errors.
3. **Sub/superscript fragments (defect).**
   - `find_line` tests `|line.baseline − span.y0| ≤ 0.4·size`, with y0 = baseline − 0.2·size
     from the fixed `DESCENT`, and tests reach against the *partial* line box built in
     y0-sorted order.
   - A raised or lowered script, or a same-baseline glyph separated from its line by a script
     wider than 1 em, therefore starts a new line. `top_first` then puts the superscript line
     *above* its base.
   - This produces 5,811 fragment lines (1–3 tokens of ≤ 3 characters) next to prose, in all 60
     papers.
   - pdftotext confirms the geometry for arxiv:2108.04588 p. 1: `G` sits at x 99–107, `hom` is
     raised at x 107–122, and `(A)` is at x 122.5.

Examples:
- arxiv:2108.04588 p. 1 (scripts):
  - ours: `we classify the classes\nhom\nsim\nG\n(A) and G\n(A) of intersection graphs`
  - truth: `we classify the classes G^hom(A) and G^sim(A) of intersection graphs`
- arxiv:2603.04447 (display):
  - ours: `(2) subject to a smooth initial condition with the spatial symmetry of rotation\n(3) and/or translation (4), if its solution ψ(x, y, t) at t = t0 ≥ 0 contains a\n′\n′`
  - truth: `spatial symmetry of rotation\n\nand/or translation\n\nDefine\n\nwith\n\nAssume that (x,y,t) can be expanded…`
  - The equations are gone from the truth; their numbers `(2) (3) (4)` and primes remain in ours.

**Rule change (scripts, `reading_order::group_lines`/`find_line`).**
- First cluster spans into baseline bands in x order.
- Attach a span to a band when its vertical extent overlaps the band's core
  `[baseline, baseline + 0.7·size]` by ≥ 50 % and its size is ≤ the band's size.
- Test horizontal nearness against the band's full x range, not the partial box.
- Estimated gain:
  - dropping the fragments: **+0.0034** (51 papers);
  - reattaching them additionally matches 1,581 of 7,645 fragment tokens that occur in the
    aligned truth window, worth about **+0.0018** more;
  - net about +0.002 to +0.005.
- Display math belongs to the zone tag in class 3 (*math* zone: symbol-dense block).

## 6. Hyphenation and ligature residue

**Hyphenation.**
- Line-final `letters-` followed by a lower-case line, where the joined word is a truth word,
  occurs **6,352 times on 955 pages in 59 papers**. Top papers: 2509.04183 (501), 2305.13843
  (433), 2601.09974 (333), 2601.12491 (327).
- Each split costs two unmatched words on our side and one on the truth side.
- Nothing in `reading_order.rs` joins them: the module says "No text repair of any kind".

**Ligatures.**
- No U+FB00–FB06 residue remains (the backend expands them).
- Where a ligature glyph has no mapping, `check_unmapped` (lopdf_backend.rs ≈ 1091–1095)
  appends U+FFFD at the *end* of the span, so `signi[fi]cant` becomes `signicant�`.
- This is concentrated in 2 papers (2506.03828 and 2509.17930): 136 words across 4 papers.
- U+FFFD overall: 10,817 characters in the dumps, mostly in math (2506.23487: 2,725;
  2603.05575: 1,222).

Examples:
- arxiv:2305.13843 p. 3:
  - ours: `We provide a comprehen-\nsive review of state-of-the-art MTRS`
  - truth: `We provide a comprehensive review of state-of-the-art MTRSs`
- arxiv:2506.03828 p. 1:
  - ours: `Despite signicant� advances in LLM-based agents and gener-\na…`
  - truth: `Despite significant advances in LLM-based agents and general…`
- A footnote-marker superscript can also land between the two halves, so a hyphen join has to
  look past fragment lines:
  - arxiv:2601.09974 ours: `validating its robust-\n1\nness for real-world`
  - truth: `validating its robustness for real-world`

**Rule changes.**
- **De-hyphenation**, applied when assembling text after ordering (document level, so it also
  spans page breaks once headers and footers are tagged):
  - Join `X-⏎y` (y lower-case) into `Xy` when `Xy` occurs elsewhere in the document
    unhyphenated.
  - Keep the hyphen when both halves are ≥ 3-letter words seen elsewhere (compounds).
  - Otherwise join.
  - Simulated gain: **+0.0104** (53 papers). A truth-vocabulary oracle gives +0.0105 and
    always-join gives +0.0096.
- **Ligatures:**
  - put U+FFFD at the position of the dropped byte (per-byte results are already available in
    `ByteTable`);
  - map `Differences` glyph names `fi fl ff ffi ffl` to their letters.
  - Oracle gain: +0.0002.

## 7. Accents

There is **no accent composition on this branch**: grepping `src/` finds U+00B4/U+00A8 only in
`citations.rs`'s marker filter. TeX-style spacing accents are still in our text, placed before
the base letter:

| accent | count |
| --- | --- |
| ´ | 179 |
| ¨ | 46 |
| ¯ | 21 |
| ˆ | 16 |
| ˜ | 11 |
| ˚ | 8 |
| ¸ | 7 |

Of these, 118 compose with a following letter in 13 papers. **49 of those are false: in
arxiv:2602.02748, `´` is a minus sign from a broken font encoding** (`d´1q` = `d−1)`,
`´ kq` = `− k)`). Genuine cases:

- arxiv:2504.09409:
  - ours: `On the other hand, by H¨older’s`
  - truth: `On the other hand, by Hölder's inequality`
- arxiv:2504.09409:
  - ours: `Gonz´alez`
  - truth: `recent work by González et al.`
- also 2401.15719 and 2603.21379 (`H¨older`), and in author lists 2410.17124 `Hern´andez` and
  2509.24852 `Ran¸con`.

Composing them gains **+0.0000**, because they are too rare to move the mean.

**Rule change.**
- In backend `emit`, after NFC, replace a spacing accent (U+00B4, U+00A8, U+02C6, U+02DC,
  U+02C7, U+02DA, U+00AF, U+02D8, U+02D9) that is immediately followed by a letter (no space)
  with the combining mark after that letter, then NFC again. Treat U+00B8 (cedilla) as applying
  to the preceding letter.
- Skip fonts whose `´` is used standalone before digits or spaces, as in 2602.02748.
- Worth doing for correctness. It changes nothing on this metric.

## 8. Paragraph breaks lost or spurious

The alignment works on words, so **paragraph breaks have exactly zero effect** on the score. For
the record:

- The truth has 10,541 paragraphs; our pages have 21,729 `\n\n`-separated blocks.
- 647 blocks end mid-sentence and continue in lower case (49 papers).
- At least 1,988 truth paragraph starts follow a single `\n` in ours (all 60 papers).
- Neither metric changes.

Examples:
- arxiv:2306.11313:
  - ours: `…to demonstrate the model’s ability\n\nto capture dynamic graph influence.`
  - truth: one paragraph: `…to demonstrate the model’s ability to capture dynamic graph influence.`
  - Cause: a vertical gap > `PARAGRAPH_GAP` (1.5 line heights), or a leaf change in `separator`.
- Small-caps headings split into two words, 23 times in 11 papers:
  - arxiv:2507.14211 ours: `I. I\nNTRODUCTION`
  - truth: `Introduction`
  - This costs a word each time, not a paragraph. Mechanism unverified: probably the
    size-dependent y0 in `find_line`, as in class 5.

## 9. Rotated or vertical text and margin stamps

**arXiv stamp.**
- The rotated stamp appears on page 1 of 59 of the 60 papers.
- Dropping the stamp line alone gains +0.0010 (28 papers change).
- Its real cost is the merge into a body line on 22 papers, which drives the class 1
  interleaving (+0.0049 oracle on those pages).

**Rotated figure labels.** These are emitted one glyph or fragment per line, bottom-to-top,
because each rotated glyph gets its own baseline:
- arxiv:2505.16990 p. 16: ours `y\nc\nra\nu\nc\nc\nA` ("Accuracy" rotated 90°, reversed). The
  truth drops the figure.
- arxiv:2306.11313 p. 15: ours `y\nc\nn\ne\nd` (the tail of "…dency", reversed).
- At least 34 such runs occur in 13 papers. They are counted under table/figure interiors
  (class 3).

**Rule change.** Same as class 1, rule 1. Once text direction is known, group rotated spans
along their own axis and give them a *figure* zone (or drop the arXiv stamp outright).

## 10. Truth-side noise and asymmetries

**Supplementary material in a second document.**
- arxiv:2510.26824 (the worst paper, 0.219) has two `\begin{document}` files:
  `LeMat-Synth.tex` (truth: 6,407 words) and `supporting_information.tex`.
- The PDF has 93 pages: the main paper, references from p. 10, then the supplement from p. 13.
- Our pages 1–10 alone (the body plus the first references page) align at **0.797** with the truth, and pages 1–12 at 0.708.
- The same selection issue exists in 2412.11061 (`rebuttal.tex`) and 2601.13206
  (`acl_lualatex.tex`); there the first file is the right one.
- No `\input` or `\include` is unresolved in any of the 60 source trees (checked by walking
  every archive).

**Macro and preamble leakage.**
- arxiv:2511.15503:
  - truth: `In individual ML kernels, DCCprovides up to 7.68× speedup`
  - ours: `DCC provides up to 7.68× speedup`
  - Cause: `\newcommand{\SysName}{\texttt{DCC}\xspace}`. `\xspace` is deleted and the control
    word swallowed the space.
  - Same pattern: 2502.00857 `HintEvalprovides`.
- arxiv:2603.03010:
  - truth: `<ccs2012> <concept> <concept_id>10002951.10003317</concept_id> <concept_desc>Information systems Information retrieval</concept_desc>…`
  - ours: `CCS Concepts\n• Information systems → Information retrieval`
  - CCSXML is kept by the truth; also 2602.01390 and 2506.03828.
- Also:
  - 2510.26824 truth starts with `setfontsizesetfontsize15pt1715pt17…`;
  - 2506.08311 truth contains `tcolorbox` and `lstdefinestyle` option lists (`colback=codebg, colframe=codeframe, boxrule=0.5pt…`);
  - 2603.04445 contains `\definecolor` residue (`diffcolHTMLDBEAFE…`).
- Cleaning all of these from the truth gains only **+0.0002** (8 papers).

**`\cite` and `\ref` asymmetry.** The truth deletes `\cite{…}` and `\ref{…}`, but the PDF prints
`[3, 4, 5]`, `(Chen et al., 2023b)` and `Figure 2`. Stripping them from ours gains:

| markers stripped from ours | count | papers | cumulative Δ |
| --- | --- | --- | --- |
| `[n]` markers | 5,291 | 52 | +0.0029 |
| + author-year markers | 1,396 | 34 | +0.0073 |
| + reference numbers after Figure/Table/Section/Eq./Theorem… | 3,899 | 60 | **+0.0097** |

- arxiv:2108.04588:
  - ours: `…taking disk graphs in the input [3, 4, 5, 9, 13, 16, 17, 30]\nas well as papers studying disk graphs from a mathematical angle [25, 26].`
  - truth: `…taking disk graphs in the input as well as papers studying disk graphs from a mathematical angle .`

**Rule changes, in `latex_refs.rs`, not the extractor.**
- Treat `\xspace` as a space.
- Drop `CCSXML`/`ccs2012` blocks, `\tcbset`/`\newtcolorbox`/`\lstdefinestyle`/`\definecolor`
  arguments and font-size preamble junk.
- In `ground_truth`, concatenate a second `\begin{document}` file whose name matches
  `supp|supplement|appendix|SI`, or have the eval compare only the main paper's pages.
- The fairest cite/ref fix is on the eval side: normalise both sides by removing bracketed
  numeric and author-year cite markers and the number after `Figure|Table|Section|Eq.` before
  aligning. Worth +0.0097.

## Per-paper alignment

The first score column is the value from `report.md`. "Largest single fix" is the argmax over
the per-paper simulation deltas:

- references;
- display math;
- table/figure interiors;
- captions;
- headers/footers;
- stamp;
- hyphenation;
- cite/ref markers;
- column interleave/stamp (line oracle);
- script fragments.

The last column is after the three joint changes below (A+B+C). † marks a truncated dump:
its word counts and simulations cover only the first 200 kB of our text.

Across papers the dominant class is:

| dominant class | papers |
| --- | --- |
| references | 29 |
| display math | 10 |
| hyphenation | 6 |
| column interleave/stamp | 6 |
| captions | 5 |
| table/figure interiors | 3 |
| headers/footers | 1 |

| paper | align (report) | ext / truth words | largest single fix (Δ) | second (Δ) | after A+B+C |
| --- | --- | --- | --- | --- | --- |
| arxiv:2510.26824 † | 0.219 | 33666 / 6407 | display math (+0.036) | table/figure interiors (+0.021) | 0.329 |
| arxiv:2503.13415 † | 0.281 | 28299 / 22649 | captions (+0.040) | cite/ref markers (truth asym.) (+0.020) | 0.510 |
| arxiv:2603.05575 | 0.285 | 36415 / 25621 | display math (+0.066) | captions (+0.003) | 0.387 |
| arxiv:2509.04183 | 0.357 | 31513 / 11684 | display math (+0.022) | hyphenation (+0.021) | 0.421 |
| arxiv:2608.28714 | 0.377 | 31501 / 14314 | references (+0.159) | table/figure interiors (+0.014) | 0.615 |
| arxiv:2305.13843 | 0.383 | 29868 / 18689 | references (+0.112) | column interleave/stamp (oracle) (+0.014) | 0.561 |
| arxiv:2506.23487 | 0.405 | 22278 / 12379 | display math (+0.069) | references (+0.069) | 0.528 |
| arxiv:2410.17124 | 0.408 | 21718 / 7397 | table/figure interiors (+0.105) | references (+0.080) | 0.811 |
| arxiv:2504.10389 | 0.466 | 19992 / 17713 | display math (+0.059) | script fragments (+0.019) | 0.538 |
| arxiv:2509.24852 | 0.489 | 23272 / 14337 | display math (+0.048) | table/figure interiors (+0.042) | 0.642 |
| arxiv:2601.09974 | 0.493 | 20334 / 9582 | hyphenation (+0.025) | table/figure interiors (+0.021) | 0.555 |
| arxiv:2511.15503 | 0.495 | 22491 / 13197 | references (+0.107) | display math (+0.047) | 0.765 |
| arxiv:2504.09409 | 0.501 | 20418 / 12782 | display math (+0.104) | references (+0.033) | 0.652 |
| arxiv:2505.22973 | 0.517 | 19185 / 9054 | references (+0.095) | cite/ref markers (truth asym.) (+0.026) | 0.659 |
| arxiv:2602.16061 | 0.543 | 20969 / 14816 | references (+0.070) | display math (+0.023) | 0.664 |
| arxiv:2603.04445 | 0.546 | 21589 / 11938 | references (+0.253) | cite/ref markers (truth asym.) (+0.017) | 0.892 |
| arxiv:2510.07065 | 0.559 | 19138 / 17230 | references (+0.022) | display math (+0.019) | 0.651 |
| arxiv:2507.08599 | 0.563 | 6016 / 3032 | captions (+0.028) | display math (+0.013) | 0.602 |
| arxiv:2506.03828 | 0.567 | 16026 / 9112 | references (+0.045) | table/figure interiors (+0.018) | 0.632 |
| arxiv:2602.01390 | 0.568 | 19639 / 11674 | references (+0.238) | hyphenation (+0.020) | 0.863 |
| arxiv:2503.00030 | 0.585 | 15231 / 7614 | references (+0.036) | cite/ref markers (truth asym.) (+0.025) | 0.665 |
| arxiv:2108.04588 | 0.608 | 15705 / 11996 | references (+0.037) | captions (+0.013) | 0.620 |
| arxiv:2602.00685 | 0.608 | 19154 / 13168 | captions (+0.034) | references (+0.020) | 0.655 |
| arxiv:2309.10334 | 0.619 | 7715 / 4680 | column interleave/stamp (oracle) (+0.027) | hyphenation (+0.015) | 0.652 |
| arxiv:2508.19485 | 0.624 | 10065 / 6738 | column interleave/stamp (oracle) (+0.077) | references (+0.067) | 0.759 |
| arxiv:2509.08395 | 0.630 | 15628 / 9824 | references (+0.096) | display math (+0.034) | 0.701 |
| arxiv:2502.00857 | 0.631 | 7589 / 4085 | hyphenation (+0.039) | captions (+0.023) | 0.717 |
| arxiv:2306.11313 | 0.631 | 17075 / 10491 | captions (+0.026) | display math (+0.024) | 0.599 |
| arxiv:2603.12824 | 0.634 | 12038 / 6141 | hyphenation (+0.027) | captions (+0.025) | 0.727 |
| arxiv:2401.15719 | 0.638 | 12013 / 6383 | display math (+0.086) | references (+0.046) | 0.785 |
| arxiv:2603.21379 | 0.644 | 16798 / 12177 | references (+0.053) | display math (+0.045) | 0.829 |
| arxiv:2505.16990 | 0.645 | 13071 / 7053 | captions (+0.032) | display math (+0.027) | 0.598 |
| arxiv:2509.17930 | 0.645 | 7963 / 4341 | references (+0.092) | hyphenation (+0.039) | 0.853 |
| arxiv:2501.17300 | 0.656 | 13529 / 8925 | references (+0.043) | captions (+0.024) | 0.749 |
| arxiv:2506.08311 | 0.662 | 15837 / 10320 | references (+0.110) | table/figure interiors (+0.049) | 0.886 |
| arxiv:2604.03540 | 0.667 | 15986 / 11657 | column interleave/stamp (oracle) (+0.032) | display math (+0.027) | 0.464 |
| arxiv:2509.10402 | 0.671 | 9871 / 5641 | references (+0.132) | column interleave/stamp (oracle) (+0.041) | 0.803 |
| arxiv:2406.19204 | 0.674 | 13656 / 7590 | references (+0.159) | captions (+0.055) | 0.950 |
| arxiv:2603.03010 | 0.676 | 11226 / 6279 | references (+0.093) | table/figure interiors (+0.040) | 0.866 |
| arxiv:2503.15734 | 0.677 | 9411 / 6268 | display math (+0.022) | captions (+0.011) | 0.659 |
| arxiv:2603.19305 | 0.682 | 9854 / 6272 | column interleave/stamp (oracle) (+0.055) | hyphenation (+0.013) | 0.681 |
| arxiv:2505.23862 | 0.687 | 14400 / 8960 | references (+0.175) | display math (+0.036) | 0.925 |
| arxiv:2412.06210 | 0.690 | 9678 / 6502 | references (+0.054) | column interleave/stamp (oracle) (+0.014) | 0.797 |
| arxiv:2412.11061 | 0.698 | 7528 / 4988 | column interleave/stamp (oracle) (+0.070) | references (+0.064) | 0.817 |
| arxiv:2602.17690 | 0.709 | 14384 / 9369 | headers/footers (+0.025) | captions (+0.025) | 0.543 |
| arxiv:2601.12491 | 0.722 | 11838 / 7183 | hyphenation (+0.044) | references (+0.016) | 0.779 |
| arxiv:2507.14212 | 0.728 | 12270 / 7857 | table/figure interiors (+0.052) | references (+0.051) | 0.873 |
| arxiv:2602.02748 | 0.732 | 13264 / 10461 | references (+0.056) | display math (+0.033) | 0.809 |
| arxiv:2512.10223 | 0.733 | 5634 / 4358 | references (+0.069) | column interleave/stamp (oracle) (+0.012) | 0.781 |
| arxiv:2511.22707 | 0.736 | 10383 / 6327 | table/figure interiors (+0.030) | cite/ref markers (truth asym.) (+0.019) | 0.771 |
| arxiv:2603.04447 | 0.757 | 6612 / 4728 | references (+0.037) | hyphenation (+0.006) | 0.806 |
| arxiv:2503.04404 | 0.757 | 11040 / 6784 | references (+0.085) | captions (+0.048) | 0.981 |
| arxiv:2410.19245 | 0.758 | 10979 / 7552 | references (+0.072) | hyphenation (+0.024) | 0.908 |
| arxiv:2509.12458 | 0.758 | 11634 / 8325 | column interleave/stamp (oracle) (+0.031) | references (+0.026) | 0.823 |
| arxiv:2505.01811 | 0.759 | 11901 / 7964 | display math (+0.063) | references (+0.053) | 0.932 |
| arxiv:2508.02208 | 0.762 | 10003 / 7031 | hyphenation (+0.022) | captions (+0.020) | 0.646 |
| arxiv:2507.14211 | 0.768 | 12662 / 9578 | references (+0.085) | column interleave/stamp (oracle) (+0.031) | 0.883 |
| arxiv:2601.13206 | 0.793 | 11661 / 8206 | hyphenation (+0.027) | captions (+0.013) | 0.771 |
| arxiv:2608.03351 | 0.794 | 12607 / 10885 | references (+0.049) | captions (+0.013) | 0.670 |
| arxiv:2511.13979 | 0.796 | 10183 / 6802 | references (+0.127) | captions (+0.022) | 0.960 |

## The 5 worst papers (by report alignment)

| paper | align | dominant cause (evidence) |
| --- | --- | --- |
| arxiv:2510.26824 † | 0.219 | **Truth scope (class 10).** The truth is only `LeMat-Synth.tex` (6,407 words), while the PDF carries a further 81 pages of `supporting_information.tex`. On top of that the reference section was not detected (0/90 references, so no REF lines) and the truth starts with `setfontsize…` junk. Our pages 1–10 alone score 0.797. The largest ablation on the truncated dump is display math (+0.036). |
| arxiv:2503.13415 † | 0.281 | **Captions (class 3)**, +0.040 on the truncated dump (2,499 of 2,788 caption tokens unmatched), then cite/ref markers (+0.020). The report's 0.281 against 0.419 recomputed on the dump shows most of the loss sits beyond the 200 kB cap. |
| arxiv:2603.05575 | 0.285 | **Display math (class 5)**, +0.066. 12,699 MATH tokens (9,456 unmatched) and 1,222 U+FFFD in a 123-page math paper. REF adds 10,577 tokens. |
| arxiv:2509.04183 | 0.357 | **Display math** (+0.022) and **hyphenation** (+0.021; the most hyphen splits in the corpus, 501). Beyond these, about 10k words of prompt listings in `figure*`/`tcolorbox` (pp. 35–46) are dropped by the truth, as are 8,752 REF tokens. |
| arxiv:2608.28714 | 0.377 | **References (bibliography)**, +0.159. 299 references make 9,395 REF tokens against 14,314 truth words. |

## The three changes with the largest expected gain

Simulated jointly: each column is one run with the combined edits, not a sum.

| change | where | alone | cumulative |
| --- | --- | --- | --- |
| **B. Zone-tag floats and display math** (caption, float interior, math) and align only body zones | `reading_order::order_page` (classify XY-cut leaf blocks, add a zone to `Line`); `eval.rs` filters zones | +0.0495 (0.6631) | – |
| **A. Leave the bibliography out of the aligned body** (`reference_section_text`, or a *references* zone) | `eval.rs` (`body_alignment` joins every page) | +0.0389 (0.6525) | A+B **0.7039** |
| **C. De-hyphenate line-final `X-⏎y`** with the document-vocabulary rule | `reading_order.rs` text assembly (document level) | +0.0104 (0.6239) | A+B+C **0.7162** |

- A+B+C adds **+0.103** (0.614 → 0.716).
- Adding the header/footer pass, stamp removal and script-fragment removal gives 0.7240.
- The remaining ordering oracle is worth at most about +0.01 more.
- **Docling comparison.** Docling's full pipeline scored 0.730 on 20 papers (the same 20 ids;
  `native-eval-ubuntu-24.04-arm/docling/report.json`).
  - On those papers we score 0.644, or 0.659 with the extractor-only fixes (header/footer, stamp,
    hyphenation, script fragments).
  - A+B+C would take us to 0.705, but A and B are eval-side filters. Docling was scored
    unfiltered, and the same filters would lift it too: it labels captions, tables, formulas
    and references natively.
  - The docling dumps carry no body text, so they could not be filtered here. The
    like-for-like gap is therefore about **0.07** (0.659 vs 0.730), not the 0.025 that
    comparing A+B+C against unfiltered docling would suggest.

A and B are **scope** changes. They make the metric measure body text as the truth defines it:

- They are worth doing because body text without bibliography, floats and display math is also
  what downstream chunking wants.
- They are not reading-order fixes.

The three largest **reading-order / extractor defect** fixes are:

1. **De-hyphenation** (C above): **+0.010**, 53 papers.
2. **Text direction in the backend, plus gutter tolerance and per-span sizes in `xy_cut` and
   `find_line`** (class 1): at most **+0.009** (line oracle over 33 pages in 24 papers). About
   half of that (+0.0049) comes from isolating the rotated arXiv stamp. The control on first pages without a merged stamp gains only +0.0002.
3. **Header/footer tagging** (+0.0035) together with **baseline-band grouping for scripts**
   (+0.002 to +0.005).

Low value on this metric: accent composition (+0.0000), ligature U+FFFD placement (+0.0002),
footnote relocation (+0.0004) and paragraph-break fixes (0 by construction).

## Caveats

- Line classes are heuristics over text without geometry. FOOTNOTE in particular over-matches
  prose (its simulated effects are tiny either way).
- difflib under-counts the true LCS by about 1 %. All reported scores are exact LCS.
- The oracles re-sort lines by unique truth 4-grams. They are upper bounds and can slightly
  flatter any reordering.
- The two † papers are truncated in the dumps. Their per-paper numbers are indicative only.
- Mechanism statements marked "unverified" were not checked against our own span boxes: the
  backend was not run.
