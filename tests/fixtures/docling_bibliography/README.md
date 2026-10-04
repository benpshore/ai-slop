# Docling split reference band

`2309.10334-reference-band.json` contains the original backend spans and
warnings from PDF pages 10–11 of the derived diagnostic for
[arXiv:2309.10334v1](https://arxiv.org/abs/2309.10334), distributed by the
authors under [CC BY 4.0](https://creativecommons.org/licenses/by/4.0/).
Figure/link payloads and already-projected lines/text are omitted; span text,
sequence and coordinates are unchanged. The 36 truth references are copied
unchanged from the existing corpus dump, solely to replay its matching rule.
This fixture does not replace or update the corpus manifest or baseline.

Provenance: [PR #194](https://github.com/benpshore/pdftextract/pull/194),
[run 37170137040](https://github.com/benpshore/pdftextract/actions/runs/37170137040),
artifact **11290459456**, exact source commit `0ece133`.
ZIP SHA-256:
`6706dc05f51095c0e0452cc6e3f6aadf2356d8530b580ab104aabbc63a2724aa`.
Pinned PDF SHA-256:
`f5dea342cce3a0968d3c364da4dedf74c51cb3ce59c1448c2366a5a3fc7a5794`.
Raw upstream node dump SHA-256:
`584a326ca7795d0df29cd1d34c884e5e14c56c66ab050674e80ca275ec079497`.

The node dump identifies page 10 span 9 and page 11 spans 0–34 as `ListItem`
nodes. Page 10's first reference occupies the left bottom block
`[58.57, 60.33, 298.83, 78.89]`; its continuation is the right bottom block
`[335.88, 69.61, 442.27, 78.89]`. Upstream emits five right-column appendix
blocks between them despite their higher geometry. The fixture reproduces
one extracted reference and zero matches before correction, then 36 extracted
references and 36 matches after correction. Both pages remain Partial.

Negative variants retain upstream order when list typing, consecutive
reference evidence, positioned anchors, column separation or the clear
bottom-band gap is absent. Backend spans, geometry, warnings and the leading
unpositioned caption remain unchanged in the positive regression.
