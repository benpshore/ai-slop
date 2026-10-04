# PMC bibliography measurement

`tpe bibliography` reads a PDF from its last page backward until it finds the
final reference list ([BIBLIOGRAPHY.md](BIBLIOGRAPHY.md)). The arXiv harness
([EVAL.md](EVAL.md)) measures the forward, full-document path on LaTeX-born
preprints. This measurement asks a narrower product question on **publisher
PDFs**: does the backward scan find the end-of-paper reference list, is the
entry count exact, and are the per-entry fields a citation-chain database
would key on (first-author surname, year, DOI, title) correct?

It is a diagnostic, run in GitHub Actions only; nothing here is the acceptance
protocol in [ENGINE.md](ENGINE.md), and the numbers it reports are described
in [analysis/pmc-bibliography-2026-09-30.md](analysis/pmc-bibliography-2026-09-30.md).

## Truth: the PubMed Central Open Access dataset

The anonymous S3 bucket `pmc-oa-opendata` holds, per article version, a
metadata JSON, the JATS XML, plain text, and (when the licence allows) the
publisher's PDF. The JATS `<back><ref-list>` is the publisher's own reference
list, so the truth is independent of any PDF extraction. It is not perfect
truth for the *printed* list:

- `<mixed-citation>` entries can carry little structure (no `<article-title>`,
  a `<string-name>` without a `<surname>`), so some fields are simply absent
  from the truth and drop out of that field's denominator.
- The XML list can differ from the PDF: a publisher may add DOIs the print
  lacks, or the PDF may print a reference the XML merged or split. Count
  mismatches are therefore usually, not always, extraction defects.
- Author manuscripts (`is_manuscript`) are NIH-formatted PDFs, not the
  publisher's typesetting; they are kept and reported as their own class.

## Sampling (`scripts/pmc_sample.py`)

`python3 scripts/pmc_sample.py --seed 20260930 --target 200 --out manifest.json`
draws seeded random PMCID start points across PMC3.2M to PMC12.8M (roughly
2012 to 2026), lists a dozen neighbouring article prefixes per start, picks
three at random from each, and reads each candidate's JSON and XML. It keeps
articles that are `is_pmc_openaccess`, have a `pdf_url`, are not historical
OCR or retracted, carry a `CC*` licence, have at least five references and a
usual article type (research, review, case/brief report, systematic review,
meta-analysis, protocol, clinical trial, letter, other), and caps each journal
at four articles for publisher diversity. Author manuscripts are kept and
flagged. The result is `corpus/pmc-manifest.json`: every item pins the PDF and
XML by HTTPS URL and MD5 (the bucket's own digests), plus journal, publisher,
article type, licence, year and truth reference count.

Sampling is deterministic for a seed **given the bucket's contents**; it is
re-run only on purpose (workflow input `sample: true`, or when the pinned
manifest is missing), and the manifest is printed in the job log between
`===== BEGIN PMC MANIFEST =====` and `===== END PMC MANIFEST =====` so it can
be copied into the repository and committed. Later runs use the committed
manifest unchanged.

`python3 scripts/pmc_sample.py --fetch --manifest corpus/pmc-manifest.json --cache DIR`
downloads the pinned files, verifies MD5, and skips files already present with
the right digest. `--list` prints the cache paths of the PDFs. Requests go to
S3 with four workers and exponential back-off; nothing is fetched from any
other host. PDFs are never committed or uploaded as artifacts.

Article-version URLs can change upstream without changing the version in
their path. For the reviewed `PMC9866638.1.xml` URL/MD5 pair, the fetcher uses
the exact source recovered from the successful 200-paper run, committed in
`corpus/verified/` with provenance and CC BY attribution. It verifies size,
MD5 and SHA-256 before copying or reusing that source; a missing or corrupt
snapshot fails closed. Other URLs or pins still require the normal checksum
verification and never inherit this exception. See
[source verification](analysis/pmc9866638-pin-2026-10-01.md) and
[recovery provenance](../corpus/verified/README.md).

## Running it

The `PMC bibliography` workflow (`.github/workflows/pmc-bibliography.yml`)
runs on `ubuntu-24.04-arm`, on demand and on pull requests that touch the
bibliography code paths, the scripts, the manifest or the workflow. It is
not part of the required `ci` check. It:

1. builds `tpe` in release mode (cargo cache shared with the Eval workflow);
2. fetches the corpus into `.corpus-cache/pmc` (cached on the manifest hash);
3. runs `tpe bibliography` over the PDFs in four parallel batches
   (`out/bibliography.jsonl`) and records the batch wall time;
4. runs the forward path, `tpe extract --db out/ledger.db --json -j 4`, over
   the same PDFs (`out/extract.jsonl`) for comparison;
5. scores both with `scripts/pmc_bib_eval.py`, appends `report.md` to the job
   summary and uploads `out/report` (report, JSON, failures) as an artifact.

Both `tpe` commands can exit nonzero for individual extraction outcomes; the
workflow retains those JSON records for scoring. The scorer requires exactly
one version-matching record per manifest paper in each requested extraction
stream, and rejects duplicate, missing, unexpected or malformed records.
It rechecks JATS checksums and reference counts, retains every manifest paper
in the cohort denominator, and reports unavailable truth explicitly. Reports
are written before the scorer exits nonzero for coverage or integrity errors.
Low accuracy, `not_found`, `failed` and `partial` records remain diagnostic;
none of these statuses is relabeled or excluded to pass the coverage gate.

The scorer requires `--code-sha` identifying the extraction binary's source
checkout. Its report preserves the exact manifest, source/scorer/manifest
fingerprints, backend identities and every matched or unmatched reference.
The full forward JSONL is retained in a separate workflow artifact.

## Metrics (`scripts/pmc_bib_eval.py`)

Alignment of extracted entries to truth entries, per paper: when the counts
are equal, by position; otherwise by normalised DOI, then by (year, last word
of the first-author surname, loosely compared), then by title similarity of at
least 0.85 (`difflib`); the rest stay unmatched on either side. Field
accuracies are over aligned pairs where the truth has the field, summed over
papers (rates are sums of counts, not means of per-paper rates).

| metric | meaning | what it does not mean |
| --- | --- | --- |
| list found | backward: `status == found`; forward: at least one reference extracted | not that the list is the right one or complete |
| entry count exact | extracted count equals the JATS `<ref>` count; reported over all papers and over found papers | a count match with one entry merged and one split still passes |
| count difference histogram | extracted minus truth, bucketed; `no list` for not_found/failed/missing | |
| surname, strict | NFC, whitespace-collapsed, case- and diacritic-sensitive equality of the surname part of the first extracted author and the truth `<surname>` (or `<collab>`) | strict fails for `van der Berg` vs `Berg` and for case differences; those show up as loose passes |
| surname, loose | NFKC, casefolded, diacritics stripped; equal, or equal last word | passes `Obtulowicz` for `Obtułowicz`, which the user does not consider correct |
| year | integer equality | |
| DOI (truth has one) | lowercase, prefix-stripped equality where the truth has a DOI; a missing extracted DOI counts as wrong | publishers add DOIs to the XML that the PDF never prints, so this is mostly a measure of what the PDF shows |
| DOI, when one was extracted | the same equality over pairs where a DOI was extracted (a precision); `truth DOI but none extracted` and `extracted DOI where truth has none` are counted separately | a DOI printed in the PDF but absent from the XML is not an error, but is not verified either |
| title, strict | NFC, whitespace-collapsed, U+2010/U+2011 hyphens read as `-` (publisher XML uses them where the print has `-`), one trailing period ignored, otherwise exact | |
| title, loose | similarity of the loose forms at least 0.9 | |
| entries with U+FFFD | share of extracted entries with a replacement character | |
| leakage | share of extracted entries containing the article's own DOI, elocation id or page range, or `Page n of m` / `Author manuscript` / `Downloaded from` / `available in PMC` | a rough running-head detector; journal names are not used because entries legitimately cite the same journal |
| per-PDF ms p50 / p95 | backward: the CLI's `elapsed_ms`; forward: `parse_ms + order_ms + citations_ms` | not a service-time measurement (four concurrent workers on a shared hosted runner) |
| pages scanned / total pages | backward only | |
| batch wall time | `date` around each batch | includes process start-up and JSON output |

Breakdowns of found rate and count-exact rate are by publisher, by
`is_manuscript`, and by reference style: `numbered` when at least half of the
truth labels (or, when found, the extracted labels) contain a digit, otherwise
`unnumbered` (author-year or unlabeled).

`failures.md` lists every paper whose backward record is `not_found`, `failed`
or missing, and every found list whose count differs from the truth, with the
first and last three extracted raw entries and the first and last three truth
entries (surname / year / title), plus the forward path's page text at the
last reference-heading line (or the start of the last page when no heading
line exists) so a `not_found` can be read against what the PDF shows; then
the 30 worst field mismatches where both values are present, and the 20
closest title mismatches (the typical strict-title failure). Both files
contain article data only.

`tests/test_pmc_bib_eval.py` exercises the truth parser on both citation
element types, the surname shapes and strictness, the alignment order, and
the report's key numbers with inline JATS and JSONL fixtures.
