# FFI and region evidence safety review

Review date: 2026-10-04 UTC. Scope: source review of PR #175 at
`baafb472873750e7e84d32c3c8e6fa7a680e37db`, plus independent review of the isolated
region-evidence foundation when available. The production baseline is
`e27e1fb28a5b40dbca7f517c6fe396e6f11ed4ec`. This report does not change native
providers, parsers, root Cargo configuration, workflows, routing, or publication.

**Decision:** retaining independently attributable, owned artifacts and abstaining
from region replacement is appropriate for the foundation. Existing native
buffers need not escape into that representation. The native adapters remain
trusted FFI code; safe Rust around their output does not establish native parser
memory safety. No exploitable memory corruption was demonstrated in this review.
Production region selection remains blocked on correspondence and acceptance
evidence, separately from whether a sidecar validates.

## Evidence and inventory reuse

Read `AGENTS.md`, the implementation handoff, README, upstream/native provisioning
notes, provider contracts, and PR #175 handoff/routing documentation. Source links
below are pinned to the inspected PR revision, not its moving branch.

The existing [unsafe inventory, PR #162](https://github.com/benpshore/pdftextract/blob/6c2c92d8d79a4fb5a5befec54fb2bed9da6cb2cb/docs/unsafe-rust-inventory-ffi.md)
describes source `68ee9b4e08fde2f28e1042e1d6abb89e3547770d`: 35 unsafe syntax
occurrences across 95 Rust files, all in two macOS modules. Its findings JSONL,
summary, source manifest and build manifest were read from the Git object
`6c2c92d8d79a4fb5a5befec54fb2bed9da6cb2cb`; the historical source commit itself
was not present locally. Both current module hashes exactly match that inventory:

| File | SHA-256 at reviewed PR #175 |
| --- | --- |
| `crates/tpe-app/src/services.rs` | `c5130a00b7f52582fe65decacd853d90ecc29966bccd7a473267c56343b085be` |
| `crates/tpe-speech/src/av_ffi.rs` | `48187b4c1c18449b55795749adcecc95c988fccc01fa55e492408ae03fb4b139` |

Their previous receiver, callback, pointer and ownership findings remain applicable;
this review does not recount them as new findings. The old count is **not** the
current repository count: `src/backend/native_provider.rs`, `src/worker_limits.rs`
and `src/worker_allocator.rs` now contain additional unsafe operations. Searches
for unsafe/extern/raw ownership syntax located those changes; this was not a new
AST inventory of dependencies or generated code.

Actual executables were queried directly under
`/workspace/toolchains/rustup/toolchains/1.98.1-x86_64-unknown-linux-gnu/bin/`:

| Executable | Observed output |
| --- | --- |
| `rustc -Vv` | `rustc 1.98.1 (48a229cea 2026-09-01)`; full commit `48a229ceaefd4985c50990b14116b6d856af0985`; host `x86_64-unknown-linux-gnu`; LLVM 22.1.8 |
| `cargo -V` | `cargo 1.98.1 (797e8a9bc 2026-08-05)` |
| `cargo-clippy -V` | `clippy 0.1.98 (48a229ceae 2026-09-01)` |

This verifies executable identity, not a native build or a Clippy pass. The
historical inventory's release build used the same reported compiler/Cargo/Clippy
versions, but a different source revision and artifact. Historical successful
PDFium runs are not execution evidence for this review's source.

## Boundary and ownership assessment

| Boundary | Source evidence at the reviewed pin | Assessment |
| --- | --- | --- |
| Rust input to C/C++ document | [`native_provider.rs:159–211`](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/src/backend/native_provider.rs#L159-L211) copies input into a boxed slice; session owns bytes and API. [`Session::drop:338–349`](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/src/backend/native_provider.rs#L338-L349) closes the native handle before fields are dropped. | The visible owner survives through `close`. Password is temporary; current providers consume it during `open`. Any future provider retaining the password pointer would violate that lifetime and must not be admitted without a contract change. |
| Provider functions and dynamic libraries | [`native_provider.rs:235–308`](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/src/backend/native_provider.rs#L235-L308) retains both libraries alongside copied function pointers; validates ABI, engine, version and runtime anchor. | The library owners prevent ordinary calls through already unloaded function pointers. This cannot prove that a same-named C++ runtime implements the provider's compiled ABI, or that the native implementation obeys pointer contracts. |
| Native JSON to Rust | [`native_provider.rs:351–413`](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/src/backend/native_provider.rs#L351-L413) wraps returned allocation in `Output`, validates nonzero length/16 MiB cap/non-null pointer, deserializes into owned fields, frees on errors and success. | Correct visible allocator pairing and no returned borrow of native output. `from_raw_parts` still requires readable initialized storage for the entire declared length; a length cap cannot validate a pointer. |
| Native identity strings | [`native_provider.rs:313–326`](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/src/backend/native_provider.rs#L313-L326) reads at most 256 bytes to NUL. | Bounds the search, not the allocation. Provider must return a readable NUL-terminated allocation. Replacing it with `CStr::from_ptr` would retain the unsafe precondition and remove the explicit scan bound. |
| C++ exceptions | [`provider.cc:186–217,220–333`](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/native/poppler/provider.cc#L186-L333) catches exceptions; JSON uses owning `unique_ptr` with `free`. | Normal failure paths stay in C++ and leave null/zero output. Segmentation faults and upstream memory bugs do not become recoverable Rust errors. |
| MuPDF nonlocal error handling | [`provider.c:68–100,114–208`](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/native/mupdf/provider.c#L68-L208) keeps `fz_try/fz_always/fz_catch` in C and uses `fz_var` for locals requiring protection. | No Rust frame is visibly entered by the error mechanism. Drop functions are still native calls; this review did not dynamically fault-inject them. |
| Session concurrency | [`DocumentSession:63–84`](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/src/backend/mod.rs#L63-L84) uses mutable page access and imposes no `Send` on sessions. Poppler serializes exported engine calls with its mutex. | Existing safe interface does not hand the same session to concurrent Rust callers. This does not serialize other consumers of the same Poppler global state outside this provider. |
| macOS VM accounting | [`worker_limits.rs:180–207`](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/src/worker_limits.rs#L180-L207) supplies a correctly typed `MaybeUninit` buffer and checks exact returned size before reading. | Local invariant is explicit; macOS runtime was not exercised. This is not a region data interface. |
| Worker allocator | [`worker_allocator.rs:19–67`](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/src/worker_allocator.rs#L19-L67) delegates allocation/free/realloc to `System` using caller layout and aborts on null only after worker policy activation. | No allocator mixing identified. Native `malloc`/`mmap` is outside this wrapper; it is not proof of bounded native RSS. |

The explicit provider ABI remains [a trusted-code contract](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/native/provider.h#L1-L27).
It is not an ABI for serialized Rust enums, `Vec`, `String`, `PageText` or region
types. Keep sidecars behind owned serialization; do not export their Rust memory
layout by pointer. No exported Rust embedding C ABI was found at this pin: root
`src/lib.rs` exports Rust modules, and root Cargo declares no `cdylib`/`staticlib`
or exported unmangled Rust functions. The actual native provider C ABI and
Objective-C callbacks should not be described as an implemented general C API
for the engine.

## Findings and proof limits

Severity here is engineering priority for accurate provenance and safe integration,
not a CVSS score. "Proven" means directly established from this pinned source or
the stated experiment; it does not imply exploitability.

### F1 — High: existing page evidence cannot certify source-character or transform provenance

**Proven source limitation.** The provider sends native bounds and `to_pdf`, but
[`parse_page:502–537`](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/src/backend/native_provider.rs#L502-L537)
retains only page dimensions/rotation and transformed boxes.
[`parse_page:552–566`](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/src/backend/native_provider.rs#L552-L566)
counts native Unicode scalars, then NFC-normalizes text. For example, decomposed
`e` plus combining acute can become one scalar. [`Span::seq`](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/src/schema.rs#L52-L63)
is ordering metadata, not a PDF object/glyph identifier.

Consequently, a serialized `PageText` can prove which stored normalized span was
observed, but cannot reconstruct raw native codepoints, original glyph offsets,
or the discarded affine transform. Its `BBox` contract is raw unrotated PDF user
space; it must not be re-labelled browser display space or crop-relative space.
Hash and retain the exact artifact, use explicit array-index locators tied to it,
and report unknown source correspondence. Any later substring contract needs an
explicit offset unit and normalized-payload hash. Neither geometric overlap nor
`seq` equality repairs the missing information.

**Foundation disposition:** exact artifact locators without character slicing,
raw-source claims or promotion avoid this blocker. A future selector does not.

### F2 — High: prior candidates are not retained by the current production route

**Proven source limitation.** [`pipeline.rs:543–561`](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/src/pipeline.rs#L543-L561)
replaces the whole result after guards and retains warning history, not the prior
candidate's spans/images/annotations. Its quality tuple at
[`619–643`](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/src/pipeline.rs#L619-L643)
counts complete pages, unresolved mapping pages and non-whitespace text scalars.
Those are diagnostics, not semantic correspondence or accuracy evidence.

An offline sidecar must receive the separately preserved candidate artifacts;
it cannot infer all alternatives from a final routed JSON result or manufacture
region lineage from a warning. Retain the baseline and every observed alternative,
including rejected, failed, cancelled and resource-limited attempts with honest
availability. Preserve duplicate occurrences rather than deduplicating by text.

### F3 — Medium: MuPDF warning scope is broader than the returned page

**Proven source behavior; quality impact inferred.** The warning counter belongs
to the session ([`provider.c:17–27`](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/native/mupdf/provider.c#L17-L27)),
is incremented during open/pages, never reset, and is emitted with every page at
[`196`](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/native/mupdf/provider.c#L196).
Rust marks any positive count as that page's incomplete-extraction evidence at
[`native_provider.rs:593–597`](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/src/backend/native_provider.rs#L593-L597).
Thus an earlier page warning propagates to later pages, and page order can affect
the observed count. This conservatively preserves uncertainty but is not localized
region evidence. A sidecar must retain the emitted diagnostic without claiming
its count measures defects in that region. A provider follow-up can separate
open/document diagnostics from page-local deltas. No fixture was executed here
to measure the resulting routing changes.

### F4 — Medium: Poppler's text-only coverage cannot distinguish blank from scan

**Proven coverage gap; concrete input outcome not dynamically reproduced.**
[`TextDevice:149–162`](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/native/poppler/provider.cc#L149-L162)
overrides text handling. Its page serializer emits text blocks and URI annotations
at [`257–322`](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/native/poppler/provider.cc#L257-L322),
with no image blocks. [`router::looks_scanned:154–174`](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/src/router.rs#L154-L174)
requires raster figure coverage. A no-text/no-warning Poppler record therefore
cannot itself demonstrate whether the source is blank or a scan, even if its
status becomes Complete. The whole-result fallback's figure-retention guard is
useful when earlier evidence exists, but cannot create missing image observations.
Record the adapter capability and preserve other artifacts; do not promote its
status into region-completeness proof. Image boxes from MuPDF likewise do not
constitute exported image bytes.

### F5 — Medium investigation: process-global Poppler callback survives its owner

**Proven installation/restoration gap; possible dangling callback is a hypothesis.**
[`WarningScope:35–42`](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/native/poppler/provider.cc#L35-L42)
sets Poppler's global callback to `on_error`, a function in the dynamically loaded
provider. Its destructor restores only the thread-local warning destination;
it does not restore the global callback. [`close:330–332`](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/native/poppler/provider.cc#L330-L332)
does not restore it either. Rust can unload the provider after its last API/session
owner disappears.

If an independent in-process consumer keeps that same Poppler runtime loaded and
later triggers an error, it may call a provider address whose library was unloaded.
The provider mutex does not cover that independent consumer. This review did not
load the actual Poppler runtime, establish its unload behavior, or reproduce a
use-after-unload. Treat it as an embedding/lifetime qualification gap, not a
demonstrated PDF exploit. A focused follow-up should test two consumers and library
unload, then choose explicit runtime-wide ownership/callback restoration or a
documented dedicated-process lifetime. This does not block offline sidecar storage.

### F6 — Medium qualification: hashes and ABI version are identity, not compatibility proof

**Proven contract limit.** The loader checks the provider ABI number and runtime
anchor, but it does not independently prove Poppler core C++ layout compatibility.
[`provider.cc:23–24`](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/native/poppler/provider.cc#L23-L24)
checks compile-time headers; a runtime anchor identifies where a symbol comes
from. Preserve provider and runtime hashes, compiler/build recipe, target and
actual load outcome as distinct facts. The native docs already require trusted
libraries to remain unchanged and disclose that system fonts/mapping/transitive
dependencies are outside the two hashes. There is no reproduced incompatible
runtime incident in this review. PDFium/Docling/model compatibility is covered by
the [separate specialist report](PDFIUM_COMPATIBILITY_AUDIT.md) and must retain its
own pins and execution limits. Its local raw-handle lifetime analysis was
cross-checked against `unicode_mapping.rs:12–124` and
`pdfium_backend.rs:214–247,331–391`; safe wrapper method syntax does not remove
the underlying FFI ownership obligations.

## Safe Rust reductions that are justified

The most valuable reduction is to keep region validation, bounds checking,
artifact hashing, duplicate detection, provenance comparison and abstention in
safe Rust operating on owned bytes and typed values. None needs a new unsafe block.
Use validated array indices bound to an immutable artifact digest; avoid storing
native handles, slice pointers, object addresses, or JavaScript/WASM views in a
region record. A `NonNull<c_void>` handle can express the checked non-null invariant
inside the adapter, but it does not validate ownership or remove unsafe calls.

Keep native output allocation/deallocation paired with the provider. Replacing
`free` with `Vec::from_raw_parts` or Rust deallocation would create an allocator
contract that the current ABI does not provide. Safe wrappers around dynamic
loading remain wrappers of unsafe operations. Moving MuPDF error handling into a
Rust callback would risk a native longjmp crossing Rust; the existing C boundary
should remain until a genuinely different error contract exists. Removing the
worker allocator's unsafe implementation without preserving its failure behavior
would also be a behavioral change, not a documentation cleanup.

The unchanged Objective-C inventory identifies candidates for safer typed APIs
and smaller helper scopes, but a migration requires actual macOS receiver,
threading, callback and runtime tests. It is outside this region foundation.

## Browser JavaScript/WASM boundary

The browser worker is a different runtime. At this pin,
[`pdf-worker.js:1–20`](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/web/public/pdf-worker.js#L1-L20)
uses `pdf-oxide-wasm` 0.3.77, emits page strings/annotation objects and explicitly
reports Partial. It emits no native TPE `Span`, glyph mapping or text-region
coordinate record. Its `{page,text,mediaBox,rotation,annotations}` output cannot
be used as a faithful text-region source merely because both engines use Rust.
`pages?: unknown[]` in [`types.ts:2–5`](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/web/lib/types.ts#L2-L5)
is not a versioned geometry contract. This is a **high-priority provenance blocker
for browser region selection**, not a demonstrated WASM memory defect.

Visible application ownership is appropriate for its present single request:
[`workspace.tsx:34–42`](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/web/app/workspace.tsx#L34-L42)
creates a worker, transfers the input ArrayBuffer and terminates on completion,
error or cancellation. The worker frees its document in `finally`. No native
handle or borrowed WASM memory view is visibly returned in the page record.
The generated glue and actual WASM assets are not checked in; the
[`copy script`](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/web/scripts/copy-pdf-wasm.mjs#L1-L4)
copies them from the pinned package. Their internal allocation, pointer validation
and object ownership were not audited here. An unguarded async message handler
alone does not demonstrate a reachable reentrancy defect when the application
sends one message to a dedicated worker.

**Medium, demonstrated wrapper behavior with a mocked dependency:** the delegated
browser reviewer and this reviewer each executed the exact worker body in Node
VM, replacing its import with a stub WASM document. The independently computed
worker SHA-256 was
`ac9a9c54b9b074643e6b3418466d73ee02c08e748a60e942caa6f08511724cda`.
Success and recoverable text errors each freed/closed
once and emitted a result. A second-page `pageMediaBox` exception emitted only
progress then error, discarding the accumulated first-page result. The source
explains this outcome: extraction/annotations have page-local catches, while
geometry at line 15 reaches the outer error catch at line 20. No real malformed
PDF causing that exception was tested; this is evidence of wrapper error behavior,
not of a defect in PDF Oxide. A later region artifact exporter should retain
completed page evidence and the failed attempt separately.

Observed cases from `node --input-type=module` (all assertions passed, exit 0):

| Injected behavior on page 2 | Messages | `free` / `close` counts |
| --- | --- | --- |
| None | progress, progress, result | 1 / 1 |
| `extractText` throws | progress, progress, result | 1 / 1 |
| `pageMediaBox` throws | progress, error; no retained result | 1 / 1 |

The read-only harness removed the worker's import line, provided two pages from a
stub `WasmPdfDocument`, and called `self.onmessage` once with an ArrayBuffer. It
collected `postMessage` values with `structuredClone`, counted `free`/`close`, and
asserted Partial/two pages for the first two cases and only the injected error
for the third. No fixture or WASM package was substituted into the repository.

Whole-document provenance is not absent: [`uploads.ts:33–37`](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/web/lib/uploads.ts#L33-L37)
hashes the stored original, and the JSON export at
[`workspace.tsx:329`](https://github.com/benpshore/pdftextract/blob/baafb472873750e7e84d32c3c8e6fa7a680e37db/web/app/workspace.tsx#L329)
includes the source record. This still does not give region-level lineage.
The application materializes the full input and worker accumulates all pages plus
combined text/Markdown; cancellation can terminate it but does not constitute
bounded incremental extraction or a device/large-document qualification.

## Region contract gates

The independent foundation review must verify all of the following before this
report can approve its narrow artifact-retention contract:

1. Exact artifact bytes and their digest bind every reference; identifiers cannot
   silently substitute a new document, backend, run, page, or element.
2. Page numbers are one-based physical PDF indices and array positions are checked
   separately; duplicate candidate/element identifiers are rejected. A page array
   index is not interchangeable with a printed page label or `Span::seq`.
3. Numeric coordinates are finite and ordered. Known coordinate spaces are named;
   unknown spaces remain unknown. A box accessor must not assert a transform that
   the artifact does not contain, or invent sub-span glyph boxes.
4. Diagnostics and status retain their observed scope and original content.
   Image regions, image payloads, text, links and their duplicate occurrences remain
   distinct evidence. Unknown evidence is not equivalent to absent source content.
5. Baseline retention is deterministic. Every alternative and failure/absence
   reason remains attributable. Validation cannot promote an alternative, remove
   baseline bytes, rewrite text, or upgrade status because checks passed.
6. Malformed external JSON returns an error, including duplicate JSON members,
   cross-document/page/artifact references, out-of-range locators and oversized
   identity/locator integers. No unchecked indexing, raw pointers or lifetime
   escapes are needed.

Passing these checks certifies internal consistency of retained records. It does
not authenticate who ran a parser, prove an asserted backend actually produced an
artifact, compare source rendering with text, or establish safe replacement.

## Independent foundation review

The implemented [standalone crate](../experiments/region-evidence/src/lib.rs) was
reviewed after recursive duplicate-member rejection, caller-side budget checks and
explicit unknown runtime identity were added. **Approved for the narrow offline
baseline-retention and abstention contract.** This is not approval of runtime
region routing, crash persistence, parser execution or alternative promotion.

`ArtifactStore` privately owns exact JSON byte vectors. `Sidecar::validate`
checks the actual supplied source bytes, artifact digest/size, declared
backend/configuration/outcome, ordered page set, attempt IDs, region IDs, page and
typed array locators, and internal line-to-span indices. It checks all addressable
boxes, including unreferenced items, for finite ordered coordinates. Recursive
JSON parsing rejects duplicate decoded member names before a map can silently
overwrite evidence. Unknown artifact fields survive in the retained bytes;
unknown sidecar fields are rejected.

`Validated` has private fields and immutable borrows of the sidecar/store; callers
cannot mutate their backing data while using that view. Its accessors' indexed
reads and `expect` calls rely on those checked invariants. This is a justified
safe-Rust ownership boundary. `ResolvedEvidence` borrows the validated view and
cannot outlive its parsed values. The manifest forbids unsafe code in this crate;
that does not assert that dependencies or native engines are unsafe-free.

Known geometry is explicitly producer-declared, and an unknown frame yields no
geometry claim even when the raw artifact contains a box. Missing geometry stays
missing. No character offsets or fabricated glyph positions are introduced;
identical text and identical `seq` values can have distinct array locators. The
only decision variant is `RetainBaselineAndAbstain`. A longer Complete alternative
cannot replace a Partial baseline. Failure, cancellation and resource-limit
attempts can remain without an artifact and cannot assert its geometry.

The validation deliberately checks a minimal schema-5 envelope plus addressable
arrays. It does not reimplement every production result field, validate all
metadata/citation semantics, authenticate runtime declarations, verify physical
PDF page counts, or localize native warning scope. It is an in-memory owned store,
not durable recovery after a native worker crashes. These limitations match the
[design's later admission gates](REGION_FUSION_DESIGN.md).

Independent command run from `/workspace/pdftextract-fusion`:

```sh
env PATH=/workspace/toolchains/rustup/toolchains/1.98.1-x86_64-unknown-linux-gnu/bin:/usr/local/bin:/usr/bin:/bin \
  CARGO_HOME=/workspace/toolchains/cargo \
  CARGO_TARGET_DIR=/workspace/targets/ffi-region-review \
  cargo test --locked --manifest-path experiments/region-evidence/Cargo.toml
```

Result: **12 integration tests passed, 0 failed, 0 ignored**; unit/doc test targets
contained zero tests. Tests cover byte-identical baseline/alternative retention,
no longest-text promotion, repeated occurrences, unknown geometry, tampered
source/artifact/size, mismatched backend/page/status, duplicate IDs/locators/JSON
members, malformed boxes and line references, terminal outcomes and caller
budgets. These fixtures explicitly use synthetic source bytes and fabricated
artifact records; they do not parse PDFs or exercise native providers.

The executed source snapshot had SHA-256
`f56a6c49e84e5027cf91e87b196d4200e7c453f3a47b64a27dc428519394418b`
for `src/lib.rs` and
`5aa3cef1e3101202a06ce65451c2fca313e0f1414049893cf6dfdbcfd8855d7a`
for `tests/contract.rs`. Later edits require their own verification; formatting
alone changes these byte identities. Strict Clippy/format and final integration
verification belong to the implementation owner, and are not reported here as
this reviewer's executions.

## Validation boundary and outstanding work

Completed: pinned source inspection; historical inventory artifact reuse and
matching module hashes; direct compiler/Cargo/Clippy identity queries; native
ownership/error-path and region-contract analysis; independent foundation source
review and 12 passing contract tests. No native provider, macOS,
ASan/UBSan, Miri, device, malformed-PDF corpus, or production selection test was
executed by this reviewer. The browser wrapper experiment above used a mocked
dependency and is not a real WASM execution. Previous CI and local-runtime reports
remain previous evidence, not newly run checks.

Tracking: region provenance/fusion belongs to parent issues #181/#182 and focused
foundation work; unsafe inventory remains #145, with existing compatibility/safety
work tracked under #152/#153. This review created no GitHub records or commits.
