# Reverse bibliography extraction: five-paper audit

**Date:** 2026-09-29 UTC (2026-09-28 in America/Denver)  
**Repository baseline:** `ac5c050b9340f3a43707380015af44f106489e8c`  
**Decision:** Backward page selection is promising for bibliography-only work. The output does **not** meet a 100% exact-text or publication-grade acceptance criterion.

## Question and scope

Can the existing Rust `lopdf` backend read a scholarly PDF from its final page backward until it finds the start of the **end bibliography**, and return that list faster and accurately? This test does not evaluate in-text citation occurrences, Crossref, GROBID, a first-page metadata worker, concurrent workers, or other backends.

Five pinned, openly available arXiv PDFs from the existing comparison corpus were used. The selected end lists contain 319 printed entries. One 93-page paper (`2510.26824`) also has a *different* 90-entry bibliography near pages 10–12; its 91-entry **end** list begins on page 83. The 319 denominator is for the five end lists, not every bibliography in those five documents.

## Input provenance and computed checksums

The PDFs were downloaded from the versioned `pdf_url` values in `audit/comp/comparison-manifest.json` used by the earlier comparison. SHA-256 was computed on the actual local PDF bytes with `sha256sum`; each value matched the pinned manifest. The files are not committed here.

| arXiv PDF | PDF pages | SHA-256 of PDF bytes |
|---|---:|---|
| `2309.10334v1` | 11 | `f5dea342cce3a0968d3c364da4dedf74c51cb3ce59c1448c2366a5a3fc7a5794` |
| `2401.15719v5` | 33 | `048fb6e704c293246a561d091443ce1d0a8b996cc9e5cc56655a2df78b239cca` |
| `2410.17124v1` | 62 | `2eba8c5cb339f1ceed03ba0a78cd035cd030f19c29964dbb1babb5df35164386` |
| `2509.12458v3` | 14 | `d16e884cb01112608826d6875d2032a7a56b87eff8815129d992f9a760af9e0d` |
| `2510.26824v2` | 93 | `b0447c8f5e0ba689db050619f1bdb600bd021a4ac4be220101de74d9ea7b52b7` |

## Procedure and comparator

1. A temporary Rust test harness used this repository's `backend::by_name("lopdf")`, `DocumentSession::page_text`, reading-order pass, document cleanup, region tagging, and bibliography segmentation/parsing. For each PDF it opened the bytes and read page `N`, then `N-1`, and so on, stopping when the page just added contained a qualifying reference heading or a confirmed headingless numbered list with at least three entries. It returned the final list in forward entry order. A comparison run used the **same Rust stages** on every page and selected the same end list. No other PDF parser supplied the trial output.
2. Regexes were warmed before timing. Seven trials per mode and paper were timed; the table reports the median elapsed time. Each timed interval includes opening the in-memory PDF bytes, page decoding, reading order, cleanup, section detection, and entry parsing. The file read was performed **outside** the timed interval. Neither mode includes hashing, SQLite writes, metadata extraction, or in-text marker extraction. The harness was an uncommitted test aid; this PR documents the result and does not change the engine.
3. For an **independent text comparator**, Poppler `pdftotext -raw` read the same PDFs. A strict check looked for the Rust raw text of each whole entry as a contiguous sequence in that PDF text, after Unicode NFKC/case folding, whitespace collapsing, and joining a word broken by a hyphen at a PDF line end. This is *cross-engine text agreement*, not independently verified ground truth: Poppler can discard a real line-end hyphen, insert page furniture, or differ on mathematical glyph spacing.
4. A **loose diagnostic** additionally ignored punctuation and spacing, and removed the printed page numbers that Poppler interleaved with entries. It checks ordered letters and digits. It cannot establish punctuation, mathematical notation, or exact transcription. Rendered pages were inspected at list boundaries and at concrete discrepancies; all 319 entries were **not** visually transcribed character by character.
5. The existing arXiv-source evaluation dumps had matched all 319 corresponding tail entries to source-derived records. That supports entry identity/count, not literal PDF transcription or correctness of every parsed field. The source-derived matches are prior evidence, not a new independent PDF oracle.

## Measured results

| PDF | Tail pages / total | End entries found | Full Rust median | Backward Rust median | Speedup | Strict Poppler agreement | Loose letter/digit agreement |
|---|---:|---:|---:|---:|---:|---:|---:|
| `2309.10334` | 2 / 11 | 36 / 36 | 83.03 ms | 21.62 ms | 3.84× | 36 / 36 | 36 / 36 |
| `2401.15719` | 6 / 33 | 54 / 54 | 39.66 ms | 20.37 ms | 1.95× | 53 / 54 | 54 / 54 |
| `2410.17124` | 11 / 62 | 94 / 94 | 56.79 ms | 32.19 ms | 1.76× | 86 / 94 | 93 / 94 |
| `2509.12458` | 2 / 14 | 44 / 44 | 25.69 ms | 9.34 ms | 2.75× | 41 / 44 | 44 / 44 |
| `2510.26824` | 11 / 93 | 91 / 91 | 117.89 ms | 26.90 ms | 4.38× | 74 / 91 | 91 / 91 |
| **Total** | — | **319 / 319** | — | — | — | **290 / 319 (90.91%)** | **318 / 319 (99.69%)** |

These are warm measurements on the execution host, not an M1 benchmark, sustained throughput result, or the product's complete 20-page service-time metric. The 11-page tail of `2410.17124` took 32.19 ms median, above a 30 ms threshold if that threshold is applied to this *partial* timing boundary. The results do not establish 30 ms production service time.

## Confirmed errors and unresolved differences

- **Confirmed Rust text error:** the backward result for `Vaswani2017` in `2410.17124`, PDF page 61, inserts `�` between `Kaiser, L.,` and `Polosukhin, I.,`. The rendered PDF and Poppler reading have no such character. This single entry disproves 100% raw-text fidelity on the tested end lists.
- **Confirmed parsed-field failure:** entry `[34]` in `2510.26824`, PDF page 87, contains the printed title *Enhanced hydrogen evolution activity of mos2-rgo composite synthesized via hydrothermal technique*. Its raw Rust entry contains the title, but the structured `title` field is `null`.
- **Full-path loss avoided by the backward trial:** the whole-document Rust path lost the `D.` in `D. Loutchko` for `[19]` in `2309.10334`, page 11. The backward result preserved it, agreeing with the rendered PDF. This is evidence of a full-path failure, not proof that backward extraction repairs all initials.
- **Other strict disagreements:** 28 of 319 entries disagree with Poppler under the stated strict comparison but agree on ordered letters/digits under the much weaker diagnostic. Observed causes include line-end hyphen interpretation, page numbers interleaved by Poppler, and spacing around mathematical notation. These 28 were not all visually adjudicated. Their literal accuracy remains **unknown**; classifying them all as correct would repeat the original overclaim.

Entry capture, raw transcription, and parsed fields are separate claims. **319/319 is an entry count and source-identity result, not 319 perfectly transcribed citations.** Even the loose 318/319 score discards information that matters to researchers. A verified exact-output rate for this corpus was **not established**, and the tested output has at least the confirmed defects above.

## Test timeline and limits

| Sequence on 2026-09-29 UTC | Evidence produced |
|---|---|
| Pinned inputs acquired | Five versioned arXiv PDFs; byte hashes above checked against the corpus manifest. |
| Rust trial run | Seven warm runs per mode/PDF; end-list counts, pages visited, and elapsed medians above. |
| Independent check | Poppler `-raw` strict and loose agreement counts; rendered starts/ends of the five end lists. |
| Disagreements inspected | `[19]`, `Vaswani2017`, `[34]`; correction of the earlier claim that source matching proved perfect extraction. |

Exact wall-clock times for each individual phase were not recorded. Source-derived bibliography matching and visually checked boundaries are valuable evidence, but neither is a character-by-character audit of all 319 entries. Publication-grade acceptance requires an independently adjudicated reference transcript, a declared exactness policy for line breaks and mathematical notation, and fail-closed handling of replacement characters and missing structured fields. No such acceptance test passed here.
