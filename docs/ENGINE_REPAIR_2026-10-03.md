# Engine repair integration — 2026-10-03

This repair is based on current main, including the merged PDFium Unicode
status change (#168). It retains the native worker, publication, and release
identity changes from #167/#169. It does not change live repository policy.

## Repairs

- Port the previously reconciled Form, font, CMap, CID-width, geometry,
  horizontal grouping, superscript, and bibliography parsing repairs from
  source `435fd8da47df4b9c8d16f0ac5cc4b4ba9c83640c`. The worker and release
  implementation remains based on current main.
- Terminate disposable workers on allocation failure, including fallible
  reservations whose errors upstream decoders can otherwise swallow (#170).
  Controller and ordinary library allocation behavior remain unchanged.
- Preserve unmapped glyph positions and mapped characters following gaps in
  Unicode maps. Incomplete extraction remains visible through page, chunk,
  document, bibliography, JSON, and ledger status.
- Retain usable native output when another backend loses text, changes
  source/page coverage, or returns inconsistent completeness evidence.
  Unresolved PDFium mapping evidence cannot silently trigger OCR replacement.
- Reject native PDFium page counts that the binding's 16-bit page indexing
  cannot represent. Include the effective configured library path and SHA-256
  in backend identity rather than asserting an unverified runtime version.
- Prevent CSV and ledger destinations, including aliases and prospective
  filenames, from creating or changing selected inputs.
- Require independent venue evidence in resolver query ranking. Joined
  citations with conflicting printed DOIs remain ambiguous; publisher footer
  metadata cannot verify the preceding reference. Raw evidence is preserved.
- Restore the reviewed PMC9866638 XML snapshot with its verified provenance.
  Missing, duplicate, malformed, or wrong-version evaluation records and
  invalid truth now fail validation after preserving reports. The full cohort
  remains in denominators; extraction accuracy is reported separately.

## Validation contract

Run locked workspace tests and strict Clippy, the PDFium native regressions,
Python tests and dependency audit, then the native release worker tests on
Linux x64/ARM64 and macOS ARM64. The workflows retain their failure evidence.
The local process sandbox cannot discover child PIDs through `ps`; three
existing process-control tests therefore require the hosted native runners.
Those tests are not removed, weakened, or counted as locally passing.

Corpus measurement uses the unchanged 70-paper arXiv input pins and the
reviewed 200-paper PMC manifest. The five earlier vector-limited arXiv papers
retain genuine mapping uncertainty after the geometry repair; neither their
status nor the strict native completeness gate is relaxed to obtain green CI.

The resolver's remaining historical disagreement for PMC4344110 reference 68
is a literal publisher DOI discrepancy: the printed/JATS value is
`10.1177/11545968313491001`, while PMID 23757295 identifies
`10.1177/1545968313491001`. This repair does not rewrite that evaluation label.

## Operating limits

`Complete` means that no supported extraction failure or uncertainty was
detected; it is not a proof of perfect text, reading order, or reference fields.
Scans needing new OCR and unresolved source mappings remain Partial. Native
PDFium still extracts eagerly, including for a requested page range. Its
trusted configured library must remain unchanged during a job; the fingerprint
is not an immutable native-library snapshot. Only `extract` has the documented
disposable-worker boundary; other commands and library calls do not inherit it.

These changes do not establish the M1 latency, daily throughput, or universal
accuracy targets. Historical measurement files retain their original source
identities and should not be mistaken for measurements of this integration.
