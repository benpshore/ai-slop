# Accuracy loop 6: dev60 after loop 4 (2026-09-28)

Source: Eval run 36462018737 (lopdf, 60 dev papers). Recall 96.4%, precision
96.8%, count-exact 80.0%, titles 88.7%, printed-DOI 99.6%, paper title 91.1%,
paper DOI 0/5, marker recall 92.3%, body alignment 0.611, p50 28.4 ms/chunk.

The precision drop from 99.1% is two papers, both truth or matcher defects.
Every item below was traced to the dump and the LaTeX source.

| paper | symptom | root cause | side |
| --- | --- | --- | --- |
| 2504.10389 | truth 1, extracted 40 | inline `thebibliography` (one entry, in the appendix) chosen over 38 keys resolvable in `sample.bib`; also `Surname I, Surname J (year)` entries fold the last author into the title | truth + parser |
| 2510.26824 | 91 extracted, 30 matched | 00README.json lists two toplevel documents (paper + SI) compiled into one PDF; the paper's RSC list ("Notes and references", `1 Q. Zhang, …, Chem. Soc. Rev., 2013, 42, 3127–3171.`, no titles) was not detected; truth used only the first document; `Gemini 3 Flash` matched `Gemini 2 flash` by title | truth + section detection + parser + matcher |
| 2506.03828 | 12 missed | 5 cites inside `\begin{comment}` in an `\input` file counted as truth; `[n. d.]` parsed as the title | truth + parser |
| 2509.24852 | 9 missed | bibunits: `bu1.bbl` + `bu2.bbl` are printed (65), the stale top-level `.bbl` (9) is not | truth |
| 2506.23487 | 9 merged into entry 1 | author-year list with a blank line after every line (double spacing) | segmentation |
| 2508.02208 | 13 merged | ACL entries with organisation authors (`Alibaba Cloud Qwen Team. 2025a. …`) | segmentation |
| 2508.19485 | 49–56 dropped | LNCS list interrupted by author biographies, never resumed | segmentation |
| 2503.00030 | 5 missed | ICML page 8: XY-cut took row cuts across both columns before the column cut, interleaving the reference list with the acknowledgements | reading order |
| 5 ACM papers | paper DOI 0/5 | the arXiv `10.48550/arXiv.*` DOI won over the printed publisher DOI | metadata |
