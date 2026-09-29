# Comparing evaluation runs

Use the standard-library comparator to recompute timing on the intersection
of paper IDs. It never compares the stored corpus summaries directly:

```sh
uv run python -m text_processing_engine.compare_eval before/report.json after/report.json
```

The JSON output lists added/removed IDs, failures and status changes, exclusions
from timing, matched IDs, matched parse means, nearest-rank nominal-chunk p50/p95,
and per-paper accuracy score increases/decreases. Redirect stdout to retain the
comparison as a review artifact. Missing accuracy fields and zero denominators
remain unknown; they are not treated as zero accuracy.

Timing and accuracy comparisons require status `complete` in both reports.
Timing also requires identical page and chunk counts. Incomplete, failed, and
changed-size papers remain visible in the output, so a
speed improvement cannot conceal disappearing or failing papers. Malformed,
non-finite or negative measurements, duplicate IDs, disjoint corpora, non-20-page
chunk definitions, and inconsistent `ms_total / chunks` values are rejected.

**These percentiles are across documents' summed stage time divided by nominal
20-page chunk counts. They are not measured streaming chunk latency, warm service
time, or evidence that the 30 ms acceptance target has been met.** Equal host
labels do not establish equivalent hardware, load, build flags or warm-up.

Optional provenance sidecars can be supplied explicitly:

```sh
uv run python -m text_processing_engine.compare_eval before/report.json after/report.json \
  --before-provenance before/provenance.json --after-provenance after/provenance.json
```

Sidecars use `schema_version: 1`, matching `backend` and `host`, and
`timing_definition: "summed_stages_per_nominal_20_page_chunk_v1"`. `chunk_pages`, if supplied,
must be 20. The comparator retains sidecars and flags changed or missing
`metric_source_sha256`, `truth_source_sha256`, `cargo_lock_sha256`,
`corpus_manifest_sha256`, `native_manifest_sha256`, and `split` values. An incompatible schema/timing definition or report/sidecar
backend/host mismatch is rejected.

Report-only comparisons cannot establish identical input PDF/source bytes,
ground truth, or scorer versions. Equal manifest digests also do not establish
that every downloaded input was pinned and verified. Changed truth counters
or methods are called out separately; equal counters cannot prove equal truth.
All score differences remain observations requiring review, never automatic
confirmed extraction regressions. Field accuracy denominators may change when
reference matching changes. Historical reports that both omit
`title_not_applicable` use the legacy `title_truth` denominator. A comparison
across presence/absence of that field warns and omits title accuracy rather
than silently treating unlike definitions as comparable. Body alignment is a reading-order diagnostic, not
a percentage of perfectly extracted text. The tool does not merge, set a gate,
or issue a keep/rework verdict.
