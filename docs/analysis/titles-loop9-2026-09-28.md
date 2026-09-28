# Reference-title misses after loop 6b (dev60 run 36470860921)

3778 matched entries with a truth title; 456 title mismatches (title accuracy 89.3%).

| class | count | example (extracted → truth) | side / fix |
| --- | --- | --- | --- |
| different | 139 | `c. Öztürk` → `Adaptive diffusion priors…`; `A. v. Niekerk`, `E. d. Weerdt, and M. Mossa-Basha` (2608.28714, IEEE) | parser: author lists with particles (`v.`, `d.`, `de`, `van`) and accented initials stop early, the remaining authors become the title |
| extra text in extracted | 133 | `…turbulence (2026)` → `…turbulence`; `…work? arXiv preprint…` | parser: trailing `(year)` and `arXiv preprint arXiv:…` after the title (Springer/`unsrt` styles) |
| same start, diverges | 79 | `pre-serving` vs `preserving`; `noiseregularized` vs `noise-regularized`; `Dualdomain` vs `Dual-domain` | cleanup: line-end hyphen policy in reference entries (prefix list → join; both halves words ≥ 4 chars → keep) |
| extracted None | 76 | RSC entries followed by a DOI/URL (strict title-less check misses them); `Regulation (EU) 2017/745…` | eval: relax the title-less tail to allow a trailing DOI/URL; parser for regulation-style entries |
| truncated | 29 | `Regularization of Inverse Problems` → `…, volume 375 of Mathematics and Its Applications` | truth: bbl title field carries the series; split `, volume N of …` off the truth title |
