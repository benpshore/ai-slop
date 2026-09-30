# Command line

`tpe` has two everyday commands and a self-updater. The older subcommands
(`extract`, `bibliography`, `stats`, `show`, `bench`, `corpus`, `eval`,
`backends`) are unchanged; see [ENGINE.md](ENGINE.md) and
[BIBLIOGRAPHY.md](BIBLIOGRAPHY.md).

```sh
tpe paper.pdf ~/papers/            # text and images under ./tpe-out
tpe --bib ~/papers/ --db bib.sqlite # one JSON line per PDF, mirrored into SQLite
tpe update                          # replace the binary with the latest release
tpe --version                       # tpe 0.7.0 (1a2b3c4)
```

`tpe` with no arguments prints the help and exits with status 2. A path
that happens to be a subcommand name (`update`, `stats`, ...) must be given
as `./update`.

## Inputs

Every `PATH` is a PDF file or a directory. Directories are walked
recursively in sorted, deterministic order; only files whose extension is
`pdf` in any case are selected, hidden entries (names starting with `.`,
which includes AppleDouble `._*` files) are skipped, and symbolic links to
directories are not followed. Files named directly are taken whatever their
extension.

The leading bytes of every input decide what it is; the extension never
does. Only `%PDF-` (within the first kilobyte) is processed. Images (JPEG,
PNG, GIF, TIFF, HEIC/HEIF, WebP), Office documents (a ZIP holding `word/`,
`xl/` or `ppt/`), other ZIP archives and unrecognised bytes are refused with
`status: "unsupported"`, a `kind` (`image`, `docx`, `xlsx`, `pptx`, `zip`,
`unknown`) and a `reason`; the batch continues and the exit status is
nonzero. A PDF whose pages are image-only (80% or more of the pages have
fewer than 20 non-blank characters and a `raster` figure covering at least
half the page) is reported as `status: "scanned"` with the reason `no text
layer; OCR is not included in this build`; nothing is written for it except
its images. Encrypted PDFs fail with `encrypted: password required` (or
`wrong password`); `--password` applies to both commands.

Each input is read once into memory as an immutable snapshot: the file is
stat'ed before and after the read and a size or mtime change is reported as
`file changed while it was being read` instead of being hashed. Concurrent
`tpe` invocations therefore never race on a PDF; the only shared writer is
the optional SQLite database, where SQLite's own locking (WAL journal, 10 s
busy timeout) serialises them.

## `tpe PATH...` (text and images)

Extracts the page text in reading order and the figures of every PDF.

| flag | meaning |
| --- | --- |
| `--out DIR` | output directory, default `./tpe-out` |
| `--json` | also write `<stem>.json`, the full `ExtractionResult` (pretty-printed) |
| `--stdout` | print the text to stdout instead of writing files; inputs in command-line order, separated by a form feed |
| `--no-images` | do not export figure bytes |
| `--jobs N`, `-j N` | worker threads, default the available CPUs |
| `--progress` | JSON-lines progress on stderr instead of human lines |
| `--backend NAME` | extraction backend, default `lopdf` |
| `--password PW` | password for encrypted documents |
| `--max-bytes N` | reject inputs larger than N bytes |

Outputs per PDF, under `--out`: a file given directly becomes `<stem>.txt`;
a file found under a directory argument mirrors its path relative to that
directory (`~/papers/2024/x.pdf` given `~/papers` becomes
`tpe-out/2024/x.txt`). `<stem>.txt` holds the pages separated by a form feed
(`\f`), exactly as `tpe extract --out` writes them. `<stem>.figures/` receives
figure bytes as `<document hash>/<backend>-<digest>/p<page>-f<index>.<ext>`
when the backend supplies them (the default `lopdf` backend records figure
boxes but no bytes, so the directory appears with `pdfium` or `docling`).
When two inputs would map to the same stem, the later one gets a
`-<first 12 hex of its SHA-256>` suffix and a `note` line says so.

Stderr gets one line per finished file:

```
ok  12p  1.3 MB  84 ms  /path/to/paper.pdf
FAILED  /path/to/broken.pdf: not a PDF or damaged: ...
SKIP  image  /path/to/photo.pdf
SCAN  9p  /path/to/scan.pdf
3 ok, 3 failed, 412 ms
```

With `--progress` every stderr line is a JSON object: `opened` and `page`
as `tpe extract --progress` emits them, then `done` per file (`path`,
`status` = `ok` | `failed` | `unsupported` | `scanned`, `ok`, `pages`,
`bytes`, `ms`, `error`, `kind`, `reason`), `note` for remarks and one final
`summary` (`ok`, `failed`, `ms`). The exit status is nonzero when any input
did not end `ok`.

Files are processed by a bounded pool of `--jobs` workers pulling from one
queue, and each worker writes its own outputs, so memory is bounded by the
number of workers, not the number of inputs. A panic inside a backend fails
that file only.

## `tpe --bib PATH...` (bibliographies)

Scans each PDF backward for its final reference list, exactly as
`tpe bibliography` does, and prints one JSON record per PDF to stdout: the
same shape as `tpe bibliography` ([BIBLIOGRAPHY.md](BIBLIOGRAPHY.md)),
with `kind`/`reason` added for refused inputs and `reason: "scanned"` (plus
the warning `scanned document: no text layer`) for a `not_found` scan over
image-only pages. Records are written as they finish, so their order can
differ from the command line; every record carries its `path`.

| flag | meaning |
| --- | --- |
| `--out FILE.jsonl` | write the records to this file instead of stdout |
| `--db FILE.sqlite` | also store every record in this database (below) |
| `--pdfium-fallback` | when the first scan finds no list, retry with the `pdfium` backend; ignored unless `pdfium` is compiled in |
| `--jobs`, `--progress`, `--backend`, `--password`, `--max-bytes` | as above |

A `not_found` or `failed` record counts as a failure for the exit status,
as with `tpe bibliography`.

### SQLite schema (`--db`)

Separate from the full-document ledger of `tpe extract`. One transaction per
paper; a re-run on the same SHA-256 deletes that paper's rows and inserts the
new ones. WAL journal mode and a 10 s busy timeout let two concurrent
invocations write the same file. A record without a hash (the file could not
be read, or it was not a PDF) is not stored, and a `note` line says so.

```sql
CREATE TABLE papers (
    sha256 TEXT PRIMARY KEY,   -- SHA-256 of the PDF bytes
    path TEXT,                 -- path as given on the command line
    status TEXT,               -- found | not_found | failed
    total_pages INTEGER,
    pages_scanned INTEGER,     -- pages read from the end
    section_page INTEGER,      -- page on which the list starts
    heading TEXT,              -- the reference-list heading, if any
    backend TEXT,              -- e.g. lopdf
    backend_version TEXT,
    elapsed_ms REAL,
    error TEXT,
    warnings TEXT,             -- JSON array of strings
    scanned_at TEXT            -- RFC 3339 UTC, e.g. 2026-09-30T12:00:00Z
);
CREATE TABLE refs (
    sha256 TEXT,               -- papers.sha256
    idx INTEGER,               -- 1-based position in the list
    label TEXT,                -- printed label, e.g. [12]
    raw TEXT,                  -- the entry text, unchanged
    authors TEXT,              -- JSON array of strings
    first_author TEXT,
    title TEXT,
    year INTEGER,
    venue TEXT,
    volume TEXT,
    issue TEXT,
    pages TEXT,
    doi TEXT,                  -- lower-cased, doi.org prefix stripped
    arxiv_id TEXT,
    url TEXT,
    page INTEGER,              -- page on which the entry starts
    PRIMARY KEY (sha256, idx)
);
CREATE INDEX refs_doi ON refs(doi);
CREATE INDEX refs_year ON refs(year);
CREATE INDEX refs_title ON refs(title);
CREATE INDEX refs_first_author ON refs(first_author);
```

`doi` is stored lower-cased with any `doi:`, `https://doi.org/` or
`https://dx.doi.org/` prefix removed; `raw` keeps the original spelling.

Every paper citing a DOI:

```sql
SELECT p.path, r.idx, r.label, r.raw
FROM refs r JOIN papers p ON p.sha256 = r.sha256
WHERE r.doi = '10.1000/abc456'
ORDER BY p.path, r.idx;
```

References per year across the corpus, with the number of citing papers:

```sql
SELECT r.year, COUNT(*) AS refs, COUNT(DISTINCT r.sha256) AS papers
FROM refs r JOIN papers p ON p.sha256 = r.sha256
WHERE p.status = 'found' AND r.year IS NOT NULL
GROUP BY r.year
ORDER BY r.year DESC;
```

## `tpe update`

Releases of `benpshore/pdftextract` carry `tpe-<target>.tar.gz` (a single
file `tpe`) for `aarch64-apple-darwin`, `aarch64-unknown-linux-gnu` and
`x86_64-unknown-linux-gnu`, plus `SHA256SUMS`. `tpe update` reads the latest
release from the GitHub API (20 s timeout, user agent `tpe/<version>`),
compares its tag with the running version, downloads the archive for this
platform and `SHA256SUMS` next to the running executable, verifies the
SHA-256, unpacks `tpe`, sets mode `0755` and renames it over the current
binary (renaming over a running executable is fine on macOS and Linux). A
directory that cannot be written is reported by name. `tpe update --check`
only reports. A binary installed by mise (`mise use -g
github:benpshore/pdftextract`, a path containing a `mise` component) is
never replaced: `tpe update` prints `tpe is managed by mise; run: mise
upgrade`. `TPE_RELEASES_API` overrides the API URL.

Versions are `MAJOR.MINOR.PATCH` with an optional `v`; `build.rs` bakes the
tag from `TPE_VERSION` into the binary (`0.0.0-dev` otherwise) with the short
git commit, and `tpe --version` prints `tpe <version> (<commit>)`. A dev build
counts as older than every release.

Passive check: the two everyday commands ask the API at most once per 24
hours, on a background thread with a short timeout, remembering the time and
the answer in `tpe/last-update-check` under `$XDG_CACHE_HOME` (else
`$HOME/Library/Caches` on macOS, else `$HOME/.cache`). When a newer release
is known, one line is printed to stderr at the end of the run:
`tpe 0.8.0 is available (running 0.7.0); run: tpe update` (or `run: mise
upgrade`). Nothing here can fail the run. `TPE_NO_UPDATE_CHECK=1` disables
it; every test sets it.
