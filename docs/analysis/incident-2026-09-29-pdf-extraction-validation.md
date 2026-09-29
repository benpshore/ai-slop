# Incident and diagnostics: PDF extraction claims outran validation

**Date:** 2026-09-29 UTC (2026-09-28 MDT)

**System:** text-processing-engine / ai-slop, `lopdf` backend

**Disposition:** useful reference and prose extraction prototype; citation-link and exact-text acceptance remain open

**Evidence:** [60-paper ARM evaluation](https://github.com/benpshore/ai-slop/actions/runs/36512749414), [five-paper Poppler comparison](https://github.com/benpshore/ai-slop/actions/runs/36521321081), and the `visual-audit-inputs` artifact from [the follow-up run](https://github.com/benpshore/ai-slop/actions/runs/36521802762). The five PDF versions and SHA-256 values are in the comparison artifact's `comparison-manifest.json`.

## Executive finding

The binary extracts usable prose in the sampled scholarly PDFs, restores two-column reading order better than `pdftotext -layout` in the inspected examples, and produces structured bibliography entries. The later 60-paper evaluation matched 3,829 of 3,830 source-derived reference records. The earlier 69.3% recall described a 20-paper intermediate state, heavily affected by defective bibliography ground truth; it is not the current result.

The reported near-perfect citation-marker precision does **not** establish accurate citation links. In arXiv:2309.10334, six of 29 extracted markers are mathematical vectors such as `[1, 1, 1, 1]` that the engine incorrectly links to reference 1. All six can pass the existing correctness check because it only asks whether reference 1 is cited *somewhere* in the paper. Mathematical notation is also corrupted on an inspected equation-heavy page. These defects prevent acceptance for unsupervised citation graphs or exact scientific text.

The owner reported approximately $1,000 in testing spend without authorization. This audit did not inspect billing records or attribution, so the monetary amount and authorization history are reported context, not independently established findings. No loss or modification of the PDF inputs was identified.

## Evidence and scope

| Question | Observation | Interpretation |
| --- | --- | --- |
| Can `lopdf` extract text and references? | Five pinned PDFs, 11–93 pages: 409/409 structured references matched source-derived truth; 60-paper dev run: 3,829/3,830, exact entry count on 60/60 papers. | Strong evidence of a functioning reference extractor on this tuned corpus; matching is not independent PDF transcription. |
| Does the page text preserve reading order? | Rendered and inspected pages 1, 8, 11 of 2309.10334; 1, 29, 31 of 2401.15719; 1, 13 of 2509.12458. Sampled two-column prose and numbered entries proceed by column in TPE output. Poppler `-layout` puts adjacent columns on the same lines in the first and third papers. | Useful layout reconstruction in these examples, not proof of 99% error-free chunks. |
| Are individual citation targets correct? | On 2309.10334, six bracketed vectors were linked to reference 1. On 2401.15719, all 112 extracted author-year target pairs contain their target's first-author surname and year in the marker string. Numeric entries and sampled marker labels on 2509.12458 agree with the rendered page. | A demonstrated false-positive class; surname/year agreement is a plausibility check and says nothing about missed occurrences. |
| Is every glyph or equation correct? | TPE's text for 2401.15719 p. 31 has replacement glyphs and disordered sums compared with the rendered page. Across 11 sampled pages, multiset word recall relative to Poppler `-raw` spans 82.6–98.2%; the lowest page contains extensive equations. | Exact-text quality is unproven. Poppler is a second extractor, not a transcription standard. |

The five-paper matched counts are 36/36, 54/54, 94/94, 44/44, and 181/181 in manifest order. The five-run median document times on one Linux ARM runner were, respectively, 72.77/204.54, 41.08/114.22, 54.20/125.81, 25.75/44.76, and 111.10/370.55 ms (TPE / Poppler). **These are unequal work boundaries:** Poppler is a fresh text-only CLI process; TPE is a warm process including acquisition, hashing, layout, metadata, and citation parsing. They do not establish an intrinsic `lopdf` versus Poppler speed ratio or M1 throughput.

The 60-paper dev summary reports 98.5% title accuracy after excluding 66 entries with no applicable truth title, 97.75% body-word recall and 93.10% precision relative to its source-derived text, and p50/p95 27.74/58.50 ms per nominal 20-page chunk on Linux ARM. Its filtered body alignment is 0.954, whereas raw alignment is 0.709. These diagnostics do not measure completely error-free chunks, citation occurrence accuracy, or 20 million documents/day on an M1.

## Failure mechanism

1. `src/citations.rs::numeric_marker_re` accepts a bracketed list of positive integers. `numeric_markers` rejects symbol indices and detached bibliography labels, but does not distinguish a mathematical vector containing repeated valid integers from a citation group. The PDF itself prints `[1, 1]` on p. 5 and `[1, 1, 1, 1]` on pp. 7–9 as values of a mathematical vector. The dump records six such occurrences, each resolved to entry 1.
2. `src/eval.rs::marker_correctness` maps extracted reference indices to truth keys and increments `correct` whenever a target's key belongs to the document-level set of cited keys. It does not align a PDF marker to the corresponding `\cite` at that location, nor check whether the marker is a citation at all. Thus the six vector errors can count as correct targets. The 60-paper report's `marker_precision = 0.9995709` is **precision under this weaker definition**, not observed occurrence-level precision.
3. The bibliography matcher in `src/eval.rs::match_references` is separate from marker correctness. Matching an entry to source-derived `.bib`/`.bbl` truth validates a bibliography record under its matching rules, but cannot validate a specific in-text citation, a missing citation, or a cited paper's real-world bibliographic correctness.

The previous [20-paper failure taxonomy](eval-2026-09-28-lopdf.md) measured 851/1,228 references (69.3%) at an intermediate revision and found substantial truth-generation and bibliography parsing problems. Later code and the 60-paper run materially changed the observed result; the intermediate failures should remain historical evidence, not a description of the current binary. Neither run establishes independent end-to-end correctness.

## Measurement limitations

- The five comparison papers are all from the existing development corpus. This is neither a blind holdout nor a random sample of scholarly PDFs.
- Poppler's bibliography-title-hit table is **not a valid winner table**: its heading detector returns zero on two papers with an absent or split heading, while the TPE reference text dump is capped at 60 kB on the 93-page paper. Inspect the full raw texts and structured dumps instead.
- The direct visual audit covers eight rendered pages across three PDFs, with an additional 11 page-level token-overlap checks; it does not inspect every page or every marker.
- The author-year check only finds author surname and year inside already extracted labels. It cannot detect omitted labels, wrong links to same-author/same-year entries, or whether a printed marker exists at a source `\cite` position.
- PDF text and source-derived truth can both omit, reorder, or encode content differently. Equations, figures, page furniture, and ligatures require visual or independently annotated ground truth.

## Corrective actions and acceptance gates

1. **P0 — repair citation evaluation:** hand-annotate occurrence-level citations, non-citation bracket groups, and expected targets on a held-out PDF set. Score precision *and* recall at the occurrence level, separately from bibliography matching. Add the six vector occurrences as regression fixtures; include equation and table contexts. Recompute published marker claims.
2. **P1 — harden marker detection:** reject repeated-value mathematical vectors using context and geometry, while preserving genuine multi-reference groups. Verify on rendered pages, numeric and author-year styles, and existing citation tests. Do not optimize the detector against only this paper.
3. **P1 — validate exact text:** compare randomly sampled complete 20-page chunks with independent visual transcription, including equations, tables, captions, and two-column pages. Report the proportion of *fully error-free* chunks and categorize errors. Treat body-word overlap as a diagnostic only.
4. **P2 — fair performance/capacity test:** define equivalent end-to-end output, cold/warm conditions and concurrency; benchmark on the target M1 under sustained load. Report p50/p95 per 20-page chunk, document throughput, memory, failures, and the distribution of document sizes. Keep the 30 ms and 20 million documents/day goals distinct until both have been measured.

**Decision:** preserve the work and use it experimentally for prose and bibliography triage. Do not advertise validated citation linkage, exact scientific text, or the stated M1 throughput until the corresponding acceptance gates pass.
