# GROBID versus our own extraction plus Crossref, for file naming and reference parsing

Status: recommendation memo. No GROBID code exists in this repository and this memo adds none.

## Recommendation

Keep the engine's own extraction, and use Crossref (when a DOI is known and the user opts in) for
naming. Do not add GROBID now. Revisit it as an optional, out-of-process fallback only if the
test at the end shows a clear accuracy gap on Ben's own kind of PDFs.

Why: (1) our reference-list results on the tuned dev set are already exact, while GROBID's published numbers are on a different, larger, harder corpus, so the two
cannot be ranked from the numbers we have; (2) GROBID's own DOI and PMID figures come from a
consolidation step against Crossref or biblio-glutton, which is the step we already plan;
(3) it adds a JVM or Docker service, a 14 GB amd64-only image for its accurate configuration,
and a native PDF component its docs say can crash, against a project that ships native Rust for
aarch64 and wants less surface, not more.

## What GROBID is, from primary sources

Read 2026-09-30 from the GROBID documentation (grobid.readthedocs.io: Introduction, Build from
source, Docker, License, Benchmarks/PubMed Central) and Docker Hub tag metadata
(`hub.docker.com/v2/repositories/grobid/grobid/tags`).

- **Licence:** Apache 2.0; documentation CC-0; annotated data CC-BY.
- **Version:** docs name 0.9.1 as latest stable and 0.9.2-SNAPSHOT as development. Docker Hub tags
  `0.9.1-full`/`0.9.1-crf` were last updated 2026-08-04, `0.9.0*` on 2026-04-07. GitHub release
  notes could not be read (GitHub is not reachable from this session), so release dates rest on
  Docker Hub.
- **Runtime:** Java (JNI to native CRF and, optionally, TensorFlow via a Python bridge); building
  needs OpenJDK 21; Linux 64-bit and macOS Intel/ARM "out of the box". Docker images: `-crf`
  about 480 MiB (amd64 and arm64 on Docker Hub); `-full` (deep-learning models, GPU
  recommended) 14,136 MiB compressed on Docker Hub, amd64 only. Docs: run with
  `--ulimit core=0` because "the crash of the PDF parsing C++ component" can dump core. Memory:
  header only under 2 GB, citations about 3 GB, full structure about 4 GB.
- **What it returns:** header (title, authors, affiliations, abstract, keywords), parsed
  references with optional consolidation, citation contexts, full text, coordinates, TEI XML.
- **Throughput claims (docs, Introduction):** on a "low profile Linux machine (8 threads)" header
  extraction of 4,000 PDFs in 2 minutes (36 PDF/s over the REST API) and full processing 2.5 PDF/s;
  10.6 PDF/s full text "during one week on one 16 CPU machine (16 threads, 32GB RAM)". The
  benchmark page reports 1,943 PDFs in 1,467 s (0.75 s per PDF) on 16 CPU/32 threads, 128 GB RAM
  and a GTX 1080 Ti with deep-learning models, and 470 s (0.24 s per PDF) with CRF-only models on
  4 CPU/8 threads. How threads were used per PDF, and the pages per PDF, are not stated.
- **Accuracy (PubMed Central page, which states GROBID 0.9.0: 1,943 PDFs, 90,125 references,
  truth from the PDFs' JATS XML; header consolidated with biblio-glutton):**

| measure | strict | soft (punctuation, case, spacing ignored) | Levenshtein 0.8 |
| --- | --: | --: | --: |
| header title F1 | 84.25 | 91.88 | 98.07 |
| header authors F1 | 92.86 | 94.82 | 96.68 |
| header first author F1 | 96.68 | 97.14 | 97.35 |
| header abstract F1 | 16.20 | 62.43 | 89.08 |
| reference title F1 | 76.85 | 88.12 | 90.29 |
| reference authors F1 | 78.77 | 79.22 | 84.64 |
| references, all fields micro F1 | 83.13 | 86.35 | 87.63 |
| references, whole-entry (instance) F1 | 44.03 | 57.72 | 63.20 |

The headline "around .87 F1" for references is the field-level soft micro average. Counted per
whole entry, F1 is 44 to 63 depending on matching. The docs also report about .90 on a bioRxiv set
of 2,000 PDFs and 76 to 91 for citation contexts. Not read here: the bioRxiv, PLOS and eLife pages
in detail.

## Our own numbers, measured here

`tpe eval --split dev --offline` on the 60 cached arXiv PDFs, release build, 4-vCPU Xeon VM shared
with other builds, 2026-09-30. Truth is the LaTeX `.bbl`/`.bib` of each paper; the dev split was
used to drive fixes, so these are tuned-on numbers.

| measure | value |
| --- | --: |
| entry count exactly right | 60/60 |
| reference recall / precision | 100.0% / 100.0% |
| reference title / year accuracy | 98.5% / 99.5% |
| DOI accuracy, all matched refs with a DOI / of DOIs printed in the PDF | 58.4% / 99.6% |
| paper title accuracy | 92.9% |
| paper author recall / precision | 98.4% / 96.1% |
| whole pipeline per 20-page chunk (n=60): median / p95 | 80.8 / 239.8 ms |
| whole pipeline per document, mean (mean 29 pages) | 184.9 ms |
| `tpe rename` wall time per PDF, pages 1-3 plus process start (n=60): min / median / p95 / max | 46 / 87 / 161 / 445 ms |

The chunk time is above the 30 ms target and slower than the documented CI figures because the
machine was busy; it is an upper bound. The DOI line shows that most reference DOIs are simply not
printed; no parser recovers them, only a lookup does.

## Why the numbers do not decide it

Different corpora (60 arXiv LaTeX PDFs, 5 to 123 pages, tuned on, versus 1,943 mainstream-publisher
PMC PDFs never used for training), different truth (LaTeX bibliography versus JATS XML), and
different definitions (our one-to-one entry matching versus GROBID's per-field matching), on
different hardware, with GROBID's header including a consolidation call. No GROBID run was made
here: this machine has the Docker client but no daemon, and building GROBID from source
(Gradle, native libraries, models) does not fit the roughly 2 GB of free disk. Speed is likewise
order-of-magnitude only: 0.24 to 0.75 s per PDF reported by GROBID (concurrency unknown) against
0.18 s mean pipeline time per document for us on a loaded VM.

## Strongest counter-case

Our engine is heuristic Rust tuned on born-digital arXiv PDFs. Ben's library is clinical literature:
publisher layouts, older scans, multilingual references, and reference styles the dev set never
saw (on the earlier 10-paper holdout a single unseen style, Springer LNCS, cost 78 of 114 title
failures, `docs/analysis/eval-2026-09-28-holdout.md`). GROBID was evaluated on
1,000 to 2,000 PDFs per corpus (PMC alone: 90,125 references), it returns affiliations and
coordinates we do not, and it is Apache-licensed, mature and production-deployed. If our
heuristics collapse on that material, "less code" turns into more failure modes than a sidecar.

## What evidence would flip it

Run both on one held-out set of at least 200 PDFs from Ben's real library (biomedical,
publisher-typeset, some scanned), with truth that neither tool made: Crossref records for the
PDFs that carry a DOI (title, first author, year) and hand-checked reference lists for a 30-paper
subset. Report n, min, median, p95 and max per field. Flip to an optional GROBID fallback if, on
that set, its first-author or title accuracy beats ours by 5 or more points, or our references
are exactly right for fewer than 90% of papers, and the `-crf` image (arm64, about 480 MiB) is
enough (the `-full` image has no aarch64 build on Docker Hub). Any adapter would stay out of
process, opt-in, offline, and behind the same `PaperRecord` type.
