# Optional native fallback for auto extraction

`TPE_AUTO_NATIVE_FALLBACK=1 tpe extract --backend auto ...` enables one bounded
MuPDF/Poppler fallback sequence when the ordinary auto route needs PDFium and
PDFium is missing, fails, regresses the existing evidence, or leaves unresolved
character mappings. An absent variable or `0` keeps the existing auto policy.
Other values fail before acquisition. This setting applies to full extraction;
the separate bibliography-only auto route is unchanged.

The sequence is `lopdf -> PDFium -> MuPDF -> Poppler`, with each backend attempted
at most once. A complete accepted native recovery stops the sequence. MuPDF and
Poppler must be compiled into the build, and each requires both of its explicit
absolute library files:

| Backend | Provider library | Engine library |
| --- | --- | --- |
| MuPDF | `TPE_MUPDF_PROVIDER_PATH` | `MUPDF_DYNAMIC_LIB_PATH` |
| Poppler | `TPE_POPPLER_PROVIDER_PATH` | `POPPLER_DYNAMIC_LIB_PATH` |

No PATH or platform-loader discovery is added. Missing configuration skips that
optional route; library fingerprint/ABI/open failures are recorded and retain
the best result. The provider owns runtime validation. The enclosing extraction
worker keeps its existing process, memory and wall-time limits across the entire
cascade; four attempts do not receive four independent timeout budgets.

The router recovers from returned backend errors and unsuccessful results. A
native crash or the enclosing worker's resource-limit termination still ends the
whole worker; it cannot publish an earlier in-memory result afterward. Recovery
across hard crashes would require separately supervised passes and a persisted
baseline, which this pipeline-level policy does not implement.

Every candidate rereads the whole requested page range. Before replacement it
must match the source hash and size, total page count and exact requested page
sequence. The existing status/text guards reject unsuccessful or inconsistent
results, lost decoded text, and newly partial formerly complete pages. With the
policy enabled, PDFium/MuPDF/Poppler candidates must also retain the multiset of
known link targets and rectangles and the known figure geometry, kind, format,
dimensions and captured image hashes. Backend-specific figure indices, output
paths and derived captions may differ. This is intentionally conservative:
equivalent evidence with different geometric rounding can retain the prior
result rather than replacing it.

After those guards, MuPDF/Poppler must improve the number of complete pages, then
the number of pages without mapping uncertainty, then decoded character count,
in that priority order. A failed, missing, equal or weaker candidate never erases
the current best pass. Rejected PDFium candidates are not installed before the
native cascade. Partial candidates remain Partial, and unresolved mappings never
enter OCR through this fallback branch. Original source-specific mapping
uncertainty remains in routing history when another native backend is selected.

The result stays one whole backend pass with that backend's identity and derived
data; no spans, annotations or figures are combined across parsers. Enabling the
policy folds its revision and fixed maximum of four native passes into the
selected backend's configuration digest, so enabled and default runs have
distinct ledger identities. Backend name/version and native binary fingerprints
remain attributable to the selected extractor.

These guards establish eligibility, not semantic truth: equally long incorrect
text can pass a character-count comparison. A Complete result means that the
selected backend reported no detected extraction gap, not that a second engine
independently proved every character. Page/element-level fusion needs its own
provenance representation and is not implemented by this policy.
