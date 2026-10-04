# Region evidence contract experiment

This standalone Rust library retains an immutable `lopdf` extraction artifact,
alternative artifacts and terminal attempt records. Its only decision is
`retain_baseline_and_abstain`. It cannot select, merge, repair or replace text.
It is an opt-in foundation for issue #196 (parents #181 and #182), with no
production routing or native-correctness claim.

The nested `[workspace]` keeps this crate outside the production workspace. No
root dependency, parser, schema, CLI, feature or output format uses it. Opt in by
running its manifest explicitly or adding a path dependency from an experimental
consumer. `rust-version = "1.98.1"` is the minimum accepted compiler, not an
executable selector. The repository toolchain selects Rust 1.98.1. The independent
lockfile pins this experiment's dependencies; no package version was hand-edited.

```sh
cargo fmt --manifest-path experiments/region-evidence/Cargo.toml --check
cargo clippy --locked --manifest-path experiments/region-evidence/Cargo.toml --all-targets -- -D warnings
cargo test --locked --manifest-path experiments/region-evidence/Cargo.toml

# Existing raw ExtractionResult JSON files from the same PDF and ordered pages:
cargo run --locked --manifest-path experiments/region-evidence/Cargo.toml \
  --example verify_artifacts -- source.pdf lopdf-result.json candidate-result.json
```

The example reads files and constructs a serialized sidecar, validates it against
the original source and artifact bytes, and reports byte retention. It invokes no
backend. It declares unknown runtime and coordinate frame rather than inventing
provenance. It does not accept the CLI's wrapped publication response: use the
raw extraction `.json` file containing `schema_version`, `document`, `backend`
and `pages` at its root.

## Contract and consuming API

1. Keep the existing schema-5 extraction JSON bytes. `ArtifactStore::insert`
   computes SHA-256 and size without decoding or reserializing them. Persist those
   exact bytes under their content hash if persistence is needed; this crate
   supplies an in-memory store only.
2. Construct a `Sidecar` with source SHA-256/size, total document page count and
   strictly increasing selected page numbers. The baseline identifies `lopdf`.
   Every attempt records the same source and ordered selection, a unique attempt
   ID, backend/version/configuration digest, explicit known or unknown runtime
   provenance, terminal outcome, optional artifact and coordinate frame.
3. Optional `RegionEvidence` entries bind an attempt and artifact digest/size to a
   page array index, checked page number, and a span/line/link/figure array index.
   Text, layout, URI and figure evidence are distinct variants. A span's `seq`,
   repeated text, figure's `index` field and string matching are never identities.
   Omitting locators leaves the complete artifact retained but unannotated;
   a supplied region must have a valid locator. There are no text byte offsets.
4. Load serialized sidecars through `Sidecar::from_json`, then call
   `sidecar.validate(source_bytes, &store, &limits)`. Merely deserializing public
   structs does not validate them. Duplicate JSON members are rejected at every
   depth in both sidecars and artifacts. Unknown sidecar fields and enum variants
   are errors; unknown artifact fields remain preserved in the original bytes.
5. The returned immutable borrowed `Validated` view exposes
   `retained_baseline_bytes()`, `alternative_bytes(attempt_id)` and `evidence(id)`.
   Rust borrowing prevents mutation of its sidecar/store while that view exists.
   Dropping the view and changing inputs requires validation again.

Validation compares supplied source bytes with SHA-256/size, verifies artifact
bytes against references, checks embedded source/backend/config/status claims,
and checks exact ordered page identity and array locators. It checks schema-5
envelope fields and addressable page arrays, internal line-to-span references,
finite ordered bounding boxes, duplicate IDs/locators and caller-controlled
budgets. All regions, text, geometry, links, figures, warnings and other fields in
the complete baseline artifact remain byte-for-byte intact, including regions
without sidecar locators. External figure files are not loaded or authenticated;
their recorded references remain in the artifact.

`Span.text` is existing normalized extraction text, including the backend's NFC
normalization/ligature handling. These are exact **artifact bytes**, not original
PDF glyph bytes or a reversible font-encoding representation.

## Hard limitations

- SHA-256 equality establishes byte identity/integrity, not trusted execution,
  authenticity, reading order or semantic correctness. Embedded source hashes
  remain producer claims whose consistency is checked against the supplied PDF;
  this library does not prove that an extractor actually processed that PDF.
- Backend/config/runtime identities and outcomes are declarations. Runtime
  provenance may explicitly be `unknown` with a reason, including failed startup.
  `complete` means the artifact and attempt agree on that status; it cannot mean
  verified text. There is no numeric confidence, ranking or longest-text rule.
- This crate parses no PDF. It compares total page count and selected pages
  between artifacts without independently proving the PDF's page tree. Every
  artifact must contain the **same complete ordered selected page list**, even
  for a partial/failed attempt. Subset artifacts must wait for a later contract.
- Unknown frame preserves original boxes in artifact bytes and exposes no
  geometry claim. `producer_declared_pdf_user_space_unrotated` explicitly declares
  points with bottom-left origin and unrotated PDF user space. Its accessor can
  expose only an existing box; a missing box remains absent. It does not verify a
  native transform. No attempt-to-attempt correspondence is represented, even
  when both declare that frame. Candidates remain unaligned alternatives.
- The schema-5 envelope checks are intentionally narrower than complete production
  schema validation. Preserving metadata/references/figure references does not
  validate their contents or external bytes. Artifact hashes are over the original
  serialization, so even whitespace changes produce a different identity.
- Outcomes distinguish complete, partial, failed, cancelled and resource limit.
  Complete/partial attempts require an artifact; failed/cancelled/limited attempts
  can omit it. Cancellation includes a reason and requester. These are terminal
  records only: no scheduling, cancellation delivery, timeouts, worker cleanup,
  memory accounting, retry or native process supervision is implemented.
- `Limits` belongs to the caller rather than the untrusted sidecar. Defaults bound
  the sidecar to 1 MiB, each artifact to 16 MiB, their total to 64 MiB, alternative
  attempts to 8, stored artifacts to 9 and locators to 10,000. The source-byte
  default is `u64::MAX`; callers may opt into a source limit. These are experimental
  accepted-input budgets, not new production PDF file limits, streaming I/O limits
  or peak-memory guarantees. Source/store buffers already exist before validation.
  The example likewise reads each file before checking contract budgets.

The tests use synthetic JSON to isolate invariants. They retain all alternatives,
warnings and original whitespace; distinguish repeated text and repeated `seq`;
reject tampered/source/page/backend/locator references, duplicate members and
malformed boxes; retain failure/cancellation/resource-limit records; enforce
budgets; and demonstrate that a longer complete candidate cannot replace a
partial baseline. They do not establish native extraction quality.
