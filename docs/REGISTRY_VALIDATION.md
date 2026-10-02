# Independent resolver validation

Tracking: [#148](https://github.com/benpshore/pdftextract/issues/148).

The existing `Registry accuracy smoke` workflow exercises one publication through
DOI, PMID and PMCID lookups. Passing it proves that those requests and verification
paths worked for that publication. It does not estimate resolver precision or
coverage. The 60-paper native extraction corpus also does not measure registry
accuracy.

The separate `Registry cohort evidence` workflow calls the production
`tpe::resolve::Resolver` directly from a release-mode Rust example, built with
`--release --locked` and the repository's exact Rust toolchain. It makes ordinary
public Crossref/Europe PMC requests using the same host limits, retries, verification,
ambiguity policy and outcome accounting as the CLI. It starts no server. An optional
repository variable `CROSSREF_MAILTO` supplies a genuine contact for Crossref's polite
pool; absent that, the run records that it used the anonymous pool.

## Independent labels and population

`corpus/registry-selection.json` is a frozen deterministic selection from the
verified 200-paper PMC extraction report. Its labels are the publisher's JATS
reference identifiers, not identifiers returned by the registries under test.
The selection retains the full original extracted entry, independent truth,
alignment method, source paper/publisher, extraction status and provenance.

The natural cohort has **240 unchanged extracted references from 178 papers and
42 publishers**: 104 contain a parsed/printed DOI, 24 have an annotation-only DOI,
and 112 require a bibliographic query. Every eligible paper contributes before any
paper contributes twice; no paper contributes more than three. The remaining
**22/200 source papers** have no DOI-labeled matched backward reference. Their IDs,
extraction status and truth counts remain explicit exclusions. These are eligibility
limits for this resolver experiment, not successful extraction results.
The additional cases after the first per paper favor printed-DOI diagnostics.
These strata are not weighted to the prevalence of every reference in the corpus;
their precision/coverage must not be presented as a 9,590-reference population estimate.

Separate controls include 20 query-only citations reconstructed from JATS, 20 wrong
DOI recovery cases, 20 DOI-only entries with no corroborating metadata, 20 entries
with deliberately contradictory fictional metadata, and ten merged-citation
ambiguity challenges. They are reported separately from untouched extracted inputs.
The wrong-DOI controls can legitimately recover the correct identity by query;
they are not incorrectly labeled as mandatory abstentions. Ambiguous merged inputs
have two independently known identities and no unique positive label, so accepting
one earns no positive precision credit.

Biomedical controls use references from 24 predetermined source papers. Their XML
is downloaded or reused only after the corpus MD5 check; an additional SHA-256 and
the exact citation XML are retained. Structured `pub-id` fields supply DOI/PMID/PMCID
labels. The generator never guesses PMID/PMCID boundaries from concatenated text,
queries the tested registries for labels, or replaces source papers after a failed
fetch. Each eligible source adds a PMID case, a PMCID case and an explicitly
contradictory PMID/PMCID pair. All 24 source XMLs verified in the initial run;
18 supplied usable reference triples and six had no eligible structured reference.
Those six remain named eligibility exclusions. The resulting frozen inputs have
**384 cases**. Acquisition errors, unaccounted sources or fewer than ten independent
labeled biomedical source papers fail the evidence check; known label ineligibility
does not become a failed network request. No source is replaced based on its result.

Source extraction report:

- Extraction code SHA: `f6fdf8e8832142b0283713916e9c96447021e0c9`.
- Report SHA-256: `ecc4860bbdcae401516c15c228142f3d7dedb425f18e6cbed18c4859ceba1328`.
- Corpus manifest SHA-256: `9465c36c416fbee0a17757ba9af41a13e89aced02cb6aad5883a49c1cf1a922e`.
- Extraction scorer version: `2`; scorer SHA and complete lopdf backend identity
  remain in the frozen selection. The report contains 9,590 JATS truth references.

## Metrics and artifact contract

`cohort.json` has contract version 1 and contains every planned case, explicit
expectation and independently sourced label. `results.jsonl` begins with the exact
compiled source SHA, Cargo.lock hash, toolchain pin, release profile, cohort hash,
source extraction provenance, provider identity and timestamp. It flushes one record
per case containing the resulting entry, all public `Attempt` records, the API's
batch outcome, elapsed time and any harness failure. Individual internal HTTP retry
transactions and raw registry responses are not exposed by the production API and
are not claimed as captured. Errors on enrichment remain errors even when the
underlying DOI resolved successfully.

`report.json` retains every case and result. Missing/unrun cases remain in coverage
denominators. It reports natural and controlled groups separately:

- Correct coverage is independently correct accepted identities / all planned
  positive cases, including request failures and unrun cases.
- Precision's conservative lower bound is independently correct identities / all
  accepted positive cases. Wrong identities and acceptances lacking independently
  checkable identifiers remain separate visible counts.
- Negative false accepts, successful abstentions and abstentions after registry
  errors are separate. An unavailable service is not evidence of correct rejection.
- Ambiguous-input acceptance/withholding and explicit resolver ambiguity outcomes
  are reported without manufacturing a unique correct identity.

The live workflow fails on incomplete accounting, missing biomedical labels,
request failures, unverified/incorrect accepted identities and negative false
accepts. Coverage itself is measured, not assigned an arbitrary passing threshold.
The live job is separate from the required offline CI aggregate. Its artifact is
uploaded on failure as well as success, so a green smoke test cannot erase failed
cohort evidence. Existing extraction fidelity metrics remain independent of all
resolver metrics. Paper-level biomedical resolution is not evaluated by this
reference-entry API cohort.

## Reproduce

The original report is needed only to regenerate the frozen selection:

```sh
uv run --no-project python scripts/registry_cohort.py select \
  --report /path/to/verified/report.json --out corpus/registry-selection.json
```

Run the checked-in selection:

```sh
mkdir -p out/registry
uv run --no-project python scripts/registry_cohort.py prepare --out out/registry/cohort.json
REGISTRY_VALIDATION_CODE_SHA="$(git rev-parse HEAD)" \
  cargo build --release --locked --example registry_validation
target/release/examples/registry_validation --cohort out/registry/cohort.json \
  --out out/registry/results.jsonl --code-sha "$(git rev-parse HEAD)"
uv run --no-project python scripts/registry_cohort.py score \
  --cohort out/registry/cohort.json --results out/registry/results.jsonl \
  --out out/registry/report.json
uv run --no-project python scripts/registry_cohort.py check --report out/registry/report.json
```

Requests are serial. The harness stops between cases after its 20-minute budget or
ten consecutive unresolved request errors; the scorer retains unrun cases. Verified
JATS downloads are cached normally. The saved result journal can be rescored without
new registry calls; results from a different source SHA are never presented as a new
code run. Live registry changes, JATS mistakes, alignment errors, the small number
of references per paper, and the PMC biomedical/OA population limit generalization.
Results must be reported with exact denominators and the actual tested source SHA.
