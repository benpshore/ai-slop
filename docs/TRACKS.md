# Tracks

One row per crate or track. "Verified" means what CI actually runs. The `ci`
check runs `cargo test --workspace` with default features on `ubuntu-latest`,
`ubuntu-24.04-arm` and `macos-15`. Optional features are not built there unless
stated. No track is production-ready.

| track | what it does | verified | known limits | next step |
| --- | --- | --- | --- | --- |
| engine, `lopdf` backend (root crate `tpe`, [ENGINE](ENGINE.md)) | extract page text, spans, reading order, metadata, references, citation markers, chunks and figures into a SQLite ledger | unit tests in `src/`, `tests/end_to_end.rs`, `tests/eval_cli.rs`, the `lopdf` cases of `tests/scanned_fixture.rs`; all three `ci` Rust legs | measured baseline (issue #15, `dev` split, hosted arm64): reference count exact 70.0%, DOI 48.9%, title 47.6%, p95 89 ms per 20-page chunk against a 30 ms target | raise reference segmentation and field accuracy on `dev`, confirm on `holdout` |
| native backends `pdfium`, `docling-text`, `docling` ([NATIVE](NATIVE.md)) | PDFium text and raster figures; docling.rs text layer, layout, OCR and pictures | Native workflow: build and unit tests pass on `ubuntu-24.04-arm` with pinned assets | docling OCR fixture (`docling_ocr_recovers_scanned_text`) result not yet known; no measured accuracy; ONNX Runtime is fetched by `ort-sys` and not pinned; some asset licences not verified offline | read the OCR fixture result and the docling `dev` eval from a Native run |
| evaluation ([EVAL](EVAL.md)) | `tpe corpus fetch` and `tpe eval` score extraction against arXiv LaTeX sources | unit tests in `src/eval.rs`, `src/corpus.rs`, `src/latex_refs.rs`; `tests/eval_cli.rs`; the Eval workflow (not a required check) | 30 papers only (20 `dev`, 10 `holdout`); hosted runners, not an M1; diagnostics, not the acceptance measurement | keep `holdout` untouched while tuning; add an M1 run |
| `tpe-common` | shared paper record types | unit tests, `ci` Rust legs | types only | none |
| `tpe-credentials` | secret storage: macOS keychain, encrypted file (XChaCha20-Poly1305, Argon2id), memory; cookie and API-key helpers | unit tests, `ci` Rust legs | keychain round trip runs only on macOS with `TPE_KEYCHAIN_TESTS=1`, so CI skips it | run the keychain test on a Mac; wire into `tpe-app` |
| `tpe-biblio` | OpenAlex, Crossref, Semantic Scholar, PMC, Europe PMC, Unpaywall and OpenURL clients; dedupe; full-text location | offline parser tests on recorded JSON, `ci` Rust legs | fixtures reproduce public response shapes, not verified captures; no live calls in CI; Google Scholar has no API, so only a search URL builder | check parsers against live responses |
| `tpe-zotero` ([ZOTERO](ZOTERO.md)) | Zotero Web API v3 client, local Zotero 7 database reader, plugin import body | offline parser tests, `ci` Rust legs | no network in tests; the plugin in `integrations/zotero-plugin` has no automated test; the local server is owned by the app track | test against a real library and the plugin |
| `tpe-search` | FTS5 (BM25) and semantic search over ledger text, with rank fusion | unit tests with the hash embedder and flat store, `ci` Rust legs | `onnx` and `usearch` features are not built in CI | build and test the optional features |
| `tpe-speech` | text-to-speech (AVSpeechSynthesizer; Kokoro behind `kokoro`) with a Whisper round trip (`asr`) | unit tests, `ci` Rust legs; the AVSpeech test skips when no voice is installed | `kokoro` and `asr` are not built in CI; no round-trip WER has been measured | run the round trip on a Mac |
| `tpe-app` | GPUI workbench skeleton: corpus list, document view, Ask Claude/ChatGPT panel | library tests (ledger, view models, API request and response parsing), `ci` Rust legs; the GUI compiles only on macOS | skeleton only, gated on extraction quality; API keys come from environment variables, not `tpe-credentials` yet | wait for extraction quality; then wire credentials |
| `tpe-browser` ([BROWSER](BROWSER.md)) | research-browser model: URL normalisation, DOI and PDF detection, host policy, library proxy, cookies | unit tests, `ci` Rust legs | CEF embedding is design-only; the `cef` feature is a documented TODO and is not built in CI | spike the CEF embedding on a Mac |

## Fact-check checklist for PRs

- Every claim in a PR body cites a test name, a CI run id, or a source line.
- Measured numbers come only from Eval or Native workflow artifacts. Give the run id, backend, split and runner.
- No README status change without a measurement behind it.
- Resolve every Codex review thread with a reference to the commit that addresses it.
