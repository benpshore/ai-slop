# tpe

A Python 3.14 wrapper around Poppler's `pdftotext -layout`. One command, no subcommands, no Python runtime dependencies. The implementation is under 100 physical lines.

Install Poppler (`brew install poppler` on macOS, `apt install poppler-utils` on Debian/Ubuntu). URL input also uses `curl`.

```sh
uv tool install --python 3.14 'git+https://github.com/benpshore/pdftextract@codex/python-core-validation#subdirectory=python-engine'

tpe
tpe paper.pdf
tpe ./papers
tpe https://example.org/paper.pdf
```

Bare `tpe` processes PDFs in the current directory. A folder processes its immediate PDF files, including uppercase `.PDF`; add `--recursive` for subfolders. Output is UTF-8 layout text in a sibling `.txt` file. URL input must return a PDF over HTTPS, and writes text into the current directory. Redirects must remain HTTPS. This does not scrape publisher landing pages.

Optional flags: `-o output-directory` preserves relative folder paths, `--overwrite` replaces existing text files, and `--version` prints the package version. Existing outputs are otherwise preserved. Failed conversions never publish partial text. A folder continues after individual errors and exits nonzero if any file failed; an empty folder also reports an error.

Downloads have a 100 MiB curl limit and a 110-second deadline; each subprocess has a 120-second deadline. These are execution limits, not a filesystem/network sandbox or whole-process memory guarantee. Scanned PDFs can produce empty text: OCR and registry/GROBID comparison are deferred from this first module.

The Python API is `from pdftextract_engine import convert_pdf, convert_url`. Both return the output `Path` and raise on failure.

Development uses the independent project in this directory:

```sh
uv sync --locked
uv run ruff format --check
uv run ruff check
uv run pytest
uv build
uv version
```

Python versions follow `0.0.x`; `uv version --bump patch` increments `x`. Python release tags use `python-v0.0.x` so they do not alter existing Rust release numbering. Release automation is kept separate from this small module and runs only after its checks.
