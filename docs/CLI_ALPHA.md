# Native CLI alpha

This is a candidate build from an isolated validation branch. It does not imply
that the larger branch stack was merged or validated for a production release.
The archive contains the release-mode `tpe` executable. `BUILD-INFO-<target>.txt` records
its exact source revision, target, Rust version, linked libraries and execution
on the native CI runner. Check the artifact's `SHA256SUMS-<target>` before unpacking.

On Apple Silicon, unpack `tpe-aarch64-apple-darwin.tar.gz` and run:

```sh
./tpe extract "/path/to/paper.pdf" --backend lopdf --db "./extraction/ledger.sqlite" --out "./extraction/text" --json
```

Use a folder path in place of the PDF for a nonrecursive batch of its regular
`.pdf` files (case insensitive). Multiple file/folder paths are accepted. Sources
are only read. No network, OCR service or external AI is used by this command.
The default artifact includes the Rust/lopdf native extractor and preserves an
existing text layer, including invisible OCR. It does not transcribe image text
or interpret plots. Figure metadata remains in JSON; `--figures-dir` is currently
rejected by supervised extraction.

Each input prints one JSON record. Successful extraction records include
`status`, the original `document.pages` count, the selected `pages`, page-local
`warnings`, `chunks`, and the actual JSON/text `outputs` paths. Failure records
include `status: "failed"`, `path`, and `error`. `partial` retains usable text and
reports detected gaps: failed pages, mapping errors/replacement characters,
resource cutoffs, suspicious image-only pages, or an intentionally selected page
range. Exit status is zero only if every document is `complete`; Partial also
returns nonzero. `--pages 2-4` therefore intentionally returns Partial when it
omits other pages, even if all selected pages extracted successfully.

Complete means the selected native extraction encountered no recognized
incompleteness signal; it is not a guarantee that every visible label, equation,
image caption or character was recovered. The image heuristic can miss small
scans or image text alongside substantial native text. Review page warnings and
retain the PDF for visual interpretation. Valid native/OCR text is never replaced
with a guessed transcription. This alpha does not silently run OCR for Partial.

Default limits are one worker, 60 seconds per document, 64 MiB input, 64 MiB
captured worker output/diagnostics, and 256 input files. Options are `--jobs 1..4`,
`--timeout-ms 1..300000`, `--max-bytes N`, `--max-output-bytes N` (1 KiB..256 MiB),
and `--max-files N` (1..10000). A folder scan stops with an explicit error before
processing if it exceeds the file limit or 10,000 directory entries. Workers are
killed and reaped on handled timeout/output-limit errors; loss of the supervisor
closes a stdin lease and terminates the worker. Workers cannot invoke Docling/OCR
subprocesses through this command. `--progress` emits buffered diagnostics after
each completed worker, not live progress.

These are worker deadlines and capture/input limits, not a parser sandbox or a
hard address-space/RSS cap. Polling can overshoot the capture limit before the
next 10 ms check. Controller publication and filesystem operations are outside
the worker deadline. Force-killing the controller can leave temporary capture
files; the lease stops its workers. There is no crash-recovery journal.

Exports never overwrite an existing JSON/text pair. A collision chooses the
next numbered pair, such as `<hash> 2.json` and `<hash> 2.txt`; use the returned
`outputs` paths. Complete files are staged, linked exclusively, and the SQLite
transaction commits last. A handled error rolls back the pending result and new
links. Filesystems without hard-link/directory-sync support fail explicitly.
Power loss or forced termination during publication can leave complete orphan
outputs or staging files; see [the publication boundary](PUBLICATION.md).

The Apple Silicon artifact is tested on GitHub's native macOS ARM64 runner only
when the corresponding workflow succeeds. It has no Developer ID signing or
Apple notarization. Any ad-hoc signature is recorded in `BUILD-INFO-<target>.txt`;
Gatekeeper may block a downloaded candidate. The user's Mac has not been used or
modified. Linux test results alone are not Mac test evidence.
