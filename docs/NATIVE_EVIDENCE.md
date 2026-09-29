# Native evaluation evidence

The Native workflow evaluates `lopdf`, `pdfium`, `docling-text`, and `docling`
on the same checkout, corpus, and runner. Each backend's artifact directory
contains its report/dumps plus:

- `provenance.json`: commit, host, manifest hashes, expected per-paper input
  hashes, evaluator/truth-source hashes, dependency-lock hash, and timing definition.
- `resources.json`: exit code, process wall time, and peak resident memory in
  bytes. Linux reports KiB and macOS bytes; the wrapper normalizes these units.

Run from the repository root on Linux or macOS:

```sh
uv run python native/measure_eval.py --backend pdfium --out eval-native/pdfium
uv run python native/validate_eval.py --manifest corpus/manifest.json --split dev \
  --backend pdfium --report eval-native/pdfium/report.json
```

Use a new or empty output directory for every run. The wrapper refuses stale
output rather than attach a new commit identity to an old report. The command's
failure is propagated, with resource evidence saved even for a nonzero exit.

Report validation requires exactly the selected manifest IDs, one complete
result per paper, matching backend/host, and consistent summary counts. A
successful process exit alone does not establish complete extraction. Numerical
accuracy/performance thresholds remain diagnostic. Native still publishes reports
and uploads artifacts after a validation failure.

Peak RSS covers the whole evaluation process, including truth extraction,
scoring, and report generation across the corpus. It is not an isolated backend
or per-document measurement and does not prove a 256 MiB total-memory bound.
The report's nominal chunk timing sums pipeline stages for each document and
divides by its nominal 20-page chunk count; its percentiles are over documents,
not individually measured streaming chunks. These hosted runs do not establish
the 30 ms target on an M1.

Manifest hashes are expected input identities verified by corpus fetching;
the sidecar itself does not rehash the cached files. Equal scorer file hashes
also cannot prove equal behavior in every dependency. Keep these limits in view
when comparing different runs or source revisions. Dedicated repeated-image,
oversized-image, and very-long-document stress measurements remain necessary.
