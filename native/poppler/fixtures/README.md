`password.pdf` is a synthetic single-page fixture made from `test_provider.py`'s
`pdf()` bytes, rewritten with PyMuPDF 1.26.6 using `PDF_ENCRYPT_AES_256`, owner
password `test-owner`, and user password `test-user`. These are public test
credentials, not secrets. Its text is `Visible citation`; its separate link target
is `https://doi.org/10.1234/raw?x=1&y=2`. It contains no third-party content.

The regression test itself needs only Python's standard library and the actual
Poppler provider/runtime. PyMuPDF is not a build or test dependency.
