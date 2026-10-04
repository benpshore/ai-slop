# Optional embedded CFF recovery

Build with `cargo build --locked --features pdf-extract`, then use the normal
`--backend lopdf`. `tpe backends` reports the helper's compiled capability.
The backend identity includes the exact pdf-extract and cff-parser versions and
probe policy, so caches cannot confuse feature-enabled and default builds.

[pdf-extract 0.12.1](https://github.com/jrmuizel/pdf-extract) provides Adobe glyph
name recovery for embedded Type1C/CFF font encodings that the primary lopdf
backend does not decode. The helper applies only to a selected simple Type1
font with a Type1C FontFile3 program and no PDF Encoding or ToUnicode entry.
Explicit PDF mappings retain priority. Multi-font CFF sets are refused because
upstream selects their first font. Type0/CID fonts, OpenType containers,
OCR and arbitrary full-document pdf-extract processing are outside this helper.

Only the selected, bounded, decompressed font program enters a synthetic
one-font PDF. No original page content, resource dictionaries or other fonts
enter that document. Two controlled probes each show all 256 byte codes with
different fallback encodings. A mapping is accepted only when both outputs
agree on one non-control Unicode character and the source CFF code-to-glyph-to-
SID mapping agrees with the bulk mapping used by pdf-extract. This extra check
rejects upstream omissions such as ignored encoding supplements. Unsupported
predefined charsets may remain unmapped. Genuine A and B glyphs are preserved;
probe fallback characters never establish a mapping by themselves.

The original lopdf interpreter retains page coverage, geometry, width advances,
font names, input hash and provenance. Unmapped bytes remain U+FFFD at their
original positions and make extraction Partial. Disabled recovery, malformed
programs and caught upstream parser panics retain the existing uncertain
Latin-1 fallback and Partial status. They never establish Complete. Resource
limit failures are reported explicitly through the existing font budget.

Encoded and decoded CFF programs are capped at 256 KiB; predictors are rejected.
Work is charged to the existing per-page font budget before foreign parsing,
including two bounded probes and table lookup work. Fonts are loaded only when
selected by interpreted text. Existing process isolation, worker memory and
time limits continue to apply; this helper does not execute font outlines.

The optional exact-pinned dependencies add pdf-extract 0.12.1 and the same
cff-parser 0.2.0 it uses. pdf-extract requires lopdf 0.42, so enabled builds also
carry that version alongside the engine's lopdf 0.45. It sees only controlled
synthetic documents. The default feature set adds no parser dependencies.

`tests/pdf_extract_cff.rs` constructs owned CFF and PDF fixtures directly:
A/B/C decode to alpha/beta/gamma while preserving original geometry and font
identity. The same fixture with the feature disabled preserves ABC with Partial
status. Further cases cover genuine A/B mappings, unknown/interior missing
codes, encoding supplements, malformed programs, explicit PDF mapping priority,
and bounded loading of used versus unused fonts. No external font download or
OCR executable is required.
