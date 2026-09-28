# Reading order loop 7: body-text taxonomy (2026-09-28)

Source: dev60 Eval run 36464411210 and holdout run 36463985607 after loop 5 (#29).
Body alignment (body only) 0.727 dev / 0.754 holdout; word recall 68.9% / 70.4%,
precision 75.0% / 78.4%. Word-level diffs of `body_text_extracted` against
`body_text_truth` on the lowest-scoring papers (2604.03540, 2602.17690,
2602.00685, 2503.13415, 2603.05575; holdout 2505.11298, 2608.14461) show six
classes. Extraction defects come first; metric defects only change the score.

| class | example | side | fix |
| --- | --- | --- | --- |
| Figure text in the body: vector plots and diagrams leave their labels as page text (`Observation sequence Robot State … Noise Prediction`, `Epoch 0 Epoch 50 Epoch 100`, `Input: labeled data unlabeled data ML model f(·)`) | 2604.03540, 2603.05575, 2602.00685, 2505.11298 | extraction | tag the region above a `Figure N` caption whose lines are short fragments (≤ 4 words, no sentence punctuation, mixed sizes) as figure text; keep it on the Figure record, out of the body |
| Table cells in the body (`Table 1: … Method Category Characteristics …`) | 2503.13415, 2602.00685, 2608.14461 | extraction | same for the grid between a `Table N` caption and the next prose paragraph; keep as table text |
| Running heads with page numbers survive furniture removal (`MM ’26, November 10–14, 2026, Rio de Janeiro, Brazil`, `Individual Rationality in Constrained Hedonic Games 3`) | 2602.17690, 2608.14461 | cleanup | furniture = a line repeated on ≥ 3 pages of the same parity or ≥ 40% of pages after digit normalisation; recto/verso heads alternate |
| Table of contents with dot leaders | 2503.13415 | cleanup | drop lines with ≥ 4 ` .` leader runs ending in a page number |
| Algorithm blocks (`Algorithm 1 … Input: … Output:`) | 2603.05575 | metric (truth strips `algorithm` envs) | drop from the body-only text: from an `Algorithm N` line until the next prose paragraph |
| Front matter: title, authors, affiliations, emails, keywords before the abstract on page 1; truth carries a detexed `\author{…}` with `organization=…` key–value noise (elsarticle) | every paper | metric both sides | extracted: drop page-1 lines before the `Abstract` heading (or the first section heading); truth: drop `\title/\author/\affiliation/\address/\email/\thanks/\keywords` and `\maketitle` |

p50 is 32.0 ms/chunk on dev (target 30): the cleanup pass runs inside `order`
(mean 9.2 ms on holdout) and needs a profile before loop 7 adds region tagging.
