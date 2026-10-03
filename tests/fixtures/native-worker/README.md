# Native worker controls

Public-safe synthetic controls from the isolated backend evaluation on 2026-10-02,
created with ReportLab 5.0.1. No private documents or external AI are involved.
`native.pdf` contains three Helvetica text lines. `existing-ocr.pdf` contains the
same lines as invisible text over a page-sized raster. The expected lines are:

```
Faithful native text remains available.
Existing OCR already reads this sentence.
Numbers 12345 and alpha beta gamma.
```

SHA-256:
- native.pdf: 099a620bd29179e329704c152808ad8e3e34f0d5388894c43d17fb3340d373c8
- existing-ocr.pdf: 51b1dc237d90c2c2b5dbc16469f7016f6f54bb4a44650e649480ce6e1fff7f73

They are copied unchanged from the previously reviewed public evaluation
fixtures. The worker tests verify text preservation, source-byte preservation,
publication collision handling, and actual OS-limit receipts.
