# Optional native provider ABI

`tpe` owns document identity, immutable input bytes, page selection, reading order,
cleanup, routing, completeness, storage, and the disposable worker. Optional C/C++
providers supply native parsing and positioned evidence through `provider.h` ABI 1.
They are separate, user-built libraries; the normal build and release ship no
MuPDF or Poppler libraries. These engines have their own licensing requirements;
dynamic loading does not waive them or change this repository's license.

Build Rust with `--features mupdf,poppler` (either feature works independently).
Configure exact absolute files:

| Engine | Provider | Native runtime |
|---|---|---|
| MuPDF | `TPE_MUPDF_PROVIDER_PATH` | `MUPDF_DYNAMIC_LIB_PATH` |
| Poppler | `TPE_POPPLER_PROVIDER_PATH` | `POPPLER_DYNAMIC_LIB_PATH` |

The loader fingerprints both files with streaming SHA-256 (256 MiB/file cap),
checks those hashes before and after loading, checks ABI/engine/version, and
verifies an exported runtime anchor belongs to the configured native library.
A missing runtime fails explicitly. Neither current-directory nor implicit engine
library searches are permitted. Keep these trusted deployment libraries and their
system dependencies unchanged during a job. Hash checks are not immutable native
code snapshots and cannot prevent concurrent hostile replacement. Native version
is reported in each page's `native_provider:` diagnostic; stored backend identity
uses the ABI version plus provider/runtime hashes and paths in `config_digest`.

CLI native extraction executes in the existing disposable worker under its wall,
CPU, address-space, and parent-lease containment. Providers must never spawn child
processes. Calling the Rust library directly does not install those process limits.
Native exceptions must terminate inside provider code, not unwind across Rust.
The input pointer remains immutable/alive through close; each session owns its
context and independent calls across sessions must be safe.

Each successful `page` result is UTF-8 JSON, bounded to the caller's limit **during
construction**, and released by the provider's `free`. Rust validates the length
before reading or copying. Rust rejects wrong page/ABI, invalid coordinates,
missing required fields, impossible coverage counts, and unsupported block types.
The output cap is 16 MiB/page and text cap is 1 million Unicode scalars/page.

Required JSON shape (coordinates are engine-normalized page space, origin top-left):

```json
{"abi":1,"page":1,"bounds":[0,0,612,792],"characters":3,"unmapped":0,
 "warnings":0,"rotation":0,"page_size":[612,792],"to_pdf":[1,0,0,-1,0,792],"structured":{"blocks":[{"type":"text",
 "bbox":{"x":72,"y":60,"w":20,"h":12},"lines":[{
 "bbox":{"x":72,"y":60,"w":20,"h":12},
 "font":{"name":"Helvetica","size":12},"text":"abc"}]}]},
 "links":[{"uri":"https://doi.org/10.1234/example","bounds":[72,60,92,72]}]}
```

A line is a native homogeneous font run, not inferred text or fabricated geometry.
`characters` counts exactly the emitted text's Unicode scalars before Rust NFC
normalization; `unmapped` counts unresolved characters. Preserve unknown text as
U+FFFD, never reinterpret CID/GID integers as Unicode. Native parsing warnings and
unmapped characters produce `Partial`. Image blocks use `type: "image"` and a real
bbox. They record image regions, not exported pixels. Unsupported native content
must be surfaced as a warning or error, never silently claimed complete. PDF `/Info`
metadata and vector paths are not yet exported by this ABI. The `to_pdf` affine matrix maps native text/image/link coordinates to raw,
unrotated PDF user space, preserving crop origins. `page_size` contains unrotated
CropBox extents and `rotation` contains 0/90/180/270; reading order applies it once.
Rust validates the transform and applies it to all bounding-box corners. All three fields are mandatory; missing coordinate provenance is rejected. Raw annotation targets are
retained separately from visible text, without URL rewriting or clickable-UI policy.
A link without a valid annotation rectangle uses `bounds: null`; no geometry is invented.

The ABI requires returned identity strings to be valid UTF-8 and NUL-terminated
within 256 bytes; returned JSON pointers must address their entire declared length.
This is a trusted-code contract, not a sandbox for malicious native plugins.
