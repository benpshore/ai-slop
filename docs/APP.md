# PDFTextract: the macOS app

Status: **first cut, built and unit-tested in CI, not yet exercised by hand
on a Mac.** `crates/tpe-app` is a GPUI window over the engine, in one
process. It does two things to a PDF: get its text, or get its bibliography.
Output lands next to the PDF; the window shows one row per job with a
per-page progress bar.

## What it does

| action | engine call | files written next to `paper.pdf` |
| --- | --- | --- |
| Get text | `pipeline::run_job_observed` (what `tpe extract` runs), then the ledger write `tpe extract` does | `paper.txt` (ordered page text, pages separated by form feed, as `tpe extract --out` writes it) |
| Get bibliography | `bibliography::scan_backward_observed` (what `tpe bibliography` runs) | `paper.references.json` (the CLI's record, `bibliography::Record`, [BIBLIOGRAPHY](BIBLIOGRAPHY.md)) and `paper.references.txt` (one entry per line, label then `raw`) |

An existing file is never overwritten: the next run writes `paper 2.txt`
(`jobs::output_path`, test `output_names_avoid_existing_files`). A failed
job, an engine panic included, writes nothing next to the source (test
`malformed_input_fails_and_writes_nothing`). A bibliography the engine
reports `not_found` is shown as such and writes nothing. The ledger `tpe
extract` requires lives at
`~/Library/Application Support/PDFTextract/ledger.sqlite`; bibliography jobs
need none.

Ways in:

- the two buttons (press, or Enter/Space when focused, to choose files; each
  is also a drop target for PDFs);
- File menu: Get Text… (⌘O), Get Bibliography… (⌘B), Clear Finished (⌘K);
- Finder: right-click a PDF → **Get Text with PDFTextract** / **Get
  Bibliography with PDFTextract** (`NSServices` in `bundle/Info.plist`,
  answered by the Objective-C provider `src/services.rs` declares at run
  time; test `provider_answers_both_service_messages`, macOS only);
- Finder: Open With PDFTextract, or drop PDFs on the Dock icon
  (`CFBundleDocumentTypes`, delivered through `Application::on_open_urls`;
  these run *Get text*, since Open With cannot say which action);
- `tpe-app paper.pdf …` from a shell queues the files for text.

Jobs run one at a time (the ledger has one writer). A queued row can be
removed; a running one runs to completion (the in-process engine has no
cancellation hook yet). Finished rows offer Copy (the text output to the
clipboard) and Show in Finder.

## Fast by construction

- The engine runs in this process on GPUI's background executor: no
  subprocess, no serialisation, no parsing of its output. Its regexes are
  compiled at launch, off the main thread, so the first job does not pay.
- Everything the user does is answered on the main thread before the engine
  is asked: a dropped PDF is a row before its first page is read.
- Progress is one `Progress` event per page over an unbounded channel; the
  foreground task keeps only the newest event waiting at each frame, so a
  15,000-page document does not queue 15,000 re-renders.
- Outputs are written by the background task and moved into place only when
  complete; the ledger write is the engine's own (`Ledger::write_result`).

Measured numbers are still owed: launch time, click-to-row latency and
per-page progress cost on an M1 have not been recorded.

## Layout

- `src/jobs.rs` (library, every platform, tested): `Action`, the `JobList`
  (rows, one job at a time, progress, finish, remove, clear), where output
  files go, `file_url_to_path` for `on_open_urls`, and `run`, the in-process
  engine call behind each button.
- `src/gui.rs` (binary, macOS): the window. Two buttons that are drop
  targets, the job list with bars, keys, menus, the Finder hooks.
- `src/services.rs` (binary, macOS): the `NSServices` provider.
- `bundle/Info.plist`, `bundle.sh`: assemble `target/release/PDFTextract.app`
  with the release binary, ad-hoc signed.
- `tests/fixtures/synthetic-paper.pdf`: the engine's synthetic paper
  (`tests/common/mod.rs::synthetic_paper`), used by the `jobs` tests.
- `.github/workflows/app.yml`: `macos-15`; fmt, clippy and the crate's tests
  with GPUI compiled in, then `bundle.sh` and a bundle smoke test, and the
  zipped app as an artifact. It runs only when the crate changes and is not
  part of the required `ci` check, so engine PRs never queue macOS runners.
  GPUI stays a macOS-only dependency: it type-checks on Linux but needs
  `libxkbcommon-x11` to link there.

The workbench library modules (`ledger`, `view`, `tpe_ai`, `keys`) are
untouched; the three-pane workbench window they served was replaced by this
one (it is in the history of `src/gui.rs`).

## Build and run

```sh
sh crates/tpe-app/bundle.sh --open     # needs Rust and Xcode's command-line tools, macOS 14+
cargo run -p tpe-app -- paper.pdf     # the window without a bundle (no Finder integration)
```

The bundle is ad-hoc signed and not notarized: on first launch macOS asks for
confirmation (right-click → Open). There is no app icon yet.

## Accessibility: verified vs reasoned

Nothing below has been run on a Mac yet. Verified by CI: the crate builds
with GPUI and passes clippy and its tests, the bundle declares its document
type and Services and signs, the Services provider answers both messages.
Reasoned from the code:

- **Screen readers and switch access: not supported by the framework.**
  GPUI 0.2.2 exposes no accessibility tree (see the accessibility note in
  the previous `src/gui.rs`), so VoiceOver, Voice Control and Switch Control
  cannot see these controls. This is the app's largest known gap and needs
  an upstream accessibility layer.
- Keyboard: every action is on a key (⌘O, ⌘B, ⌘K, ⌘Q; Tab/Shift-Tab between
  the buttons; Enter or Space on the focused one, which shows an accent
  border). Nothing is timed, nothing expires.
- Targets: the two buttons are full-width and at least 132 pt tall; row
  buttons are padded. Drop is an alternative to the button, never the only
  path.
- Text: fixed sizes for now; the previous window's ⌘= / ⌘- scaling was not
  carried over yet.

## Known limits

- One job at a time; a 15,000-page document holds the queue while the engine
  works on it, and cannot be cancelled once started.
- Progress for a bibliography counts pages read from the end against the
  whole page count, so its bar usually finishes early. That is the true
  state of the backward scan, not an estimate.
- No preferences: the `lopdf` backend, no password support (the engine takes
  one; the window does not ask).
- No icon, no notarization, no installer: `bundle.sh` produces the bundle
  and the App workflow attaches it to each run.
