# PDFium Unicode acceptance fixtures

Synthetic public-safe PDFs copied byte-for-byte from the isolated backend
comparison on 2026-10-02. No user-library documents or external AI were used.

- `partial-cmap.pdf`: PyMuPDF 1.28.2 inserted `ALPHA BETA GAMMA` at (50, 80),
  22 pt, in an embedded unmodified DejaVuSans.ttf. Only the PDF's ToUnicode
  stream was then replaced with a two-byte codespace and one mapping,
  `<0024> <0041>` (A). Glyph shapes/font program were retained. Independent
  page rendering visibly reads **ALPHA BETA GAMMA**; agreement of extractor
  strings is not the oracle. PDFium chromium/8066 reports 11 mapping errors
  while returning plausible character-code echoes. This fixture reproduces
  the production `--backend auto` false-Complete acceptance defect.
- `native.pdf`: ReportLab 5.0.1, Helvetica 14 pt, three lines at (45, 732),
  30 pt leading. Plain text and PDFium-generated line separators are valid.
- `existing-ocr.pdf`: same three lines in invisible text rendering mode 3,
  over a page-sized raster of those lines. The existing OCR text is usable
  and must be retained. It is not a request to run OCR again.

The three sentences in both controls are:

```text
Faithful native text remains available.
Existing OCR already reads this sentence.
Numbers 12345 and alpha beta gamma.
```

SHA-256 (original evaluation inputs):

51b1dc237d90c2c2b5dbc16469f7016f6f54bb4a44650e649480ce6e1fff7f73  tests/fixtures/pdfium-unicode/existing-ocr.pdf
099a620bd29179e329704c152808ad8e3e34f0d5388894c43d17fb3340d373c8  tests/fixtures/pdfium-unicode/native.pdf
b4ff9bf4458c656b0ef416de1e44bb0a99272f2e73d6f9f91fa59cf2847a8244  tests/fixtures/pdfium-unicode/partial-cmap.pdf

The embedded DejaVu font's redistribution notice is in `FONT-LICENSE.txt`.
The tests load the checked-in PDFs directly; neither Python nor a system font
installation is needed. Tests requiring the real library skip only when
`PDFIUM_DYNAMIC_LIB_PATH` is absent, and fail if it is supplied but unusable.

Run from the repository with the pinned PDFium library:

```sh
PDFIUM_DYNAMIC_LIB_PATH=/absolute/path/to/pdfium/lib \
  cargo test --locked --features pdfium --test pdfium_unicode
```
