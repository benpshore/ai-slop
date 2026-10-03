# Bounded native CLI extraction

`tpe extract paper.pdf --db results.sqlite --out extracted --json` extracts each
input in a disposable native worker. It accepts regular PDF files and folders
(nonrecursive), retains native text and existing OCR text, and never writes an
input file. The default backend is `lopdf`; `pdfium` is supported when compiled
and installed. `auto` is accepted only in builds where it cannot route to an OCR
subprocess. Figure byte export is rejected at preflight; figure metadata remains
in the result. Other commands and the library API are outside this worker boundary.

## Limits

| Option | Default | Meaning |
| --- | ---: | --- |
| `--jobs` | 1 | Concurrent extraction processes, allowed range 1–4 |
| `--max-memory-growth-mib` | 1024 | Hard virtual-address-space allowance above initialized worker startup mappings, range 32–4096 MiB |
| `--timeout-ms` | 60000 | Per-document deadline through extraction, publication queueing, publication and stdout delivery; range 1–300000 ms |
| `--max-bytes` | 67108864 | Maximum source size; the descriptor read is capped too |
| `--max-output-bytes` | 67108864 | Capture/delivery limit; range 1024–268435456 bytes |
| `--max-files` | 256 | Selection limit; at most 10000 entries are examined per folder |

Linux and macOS workers install and read back soft and hard `RLIMIT_AS` before
reading requests or PDFs. They preserve stricter inherited limits and disable
core dumps. Linux also disables dumpability, including piped core handlers.
Unsupported or failed limit installation terminates the worker. A low allowance
can reject an ordinary document; no unbounded retry occurs. Successful JSON
records include actual startup, requested and effective limits for extraction and
publication under `worker_limits`. `--progress` replays buffered opened/page
events after extraction; it is not live per-page delivery.

After those limits are installed, Rust allocation failure terminates the worker,
including failures from fallible reservation APIs. This prevents a decoder from
swallowing an allocation error and returning a truncated prefix as success. The
controller's allocator is unchanged. This policy does not intercept native
`malloc`/`mmap` calls, impose a decoded-byte cap, or change ordinary corrupt-stream
recovery; it complements the OS limits and deadline.

This is an address-space limit, **not an RSS limit or a security sandbox**. Already
reserved startup mappings can become resident. There may be up to `jobs` extracting
processes plus one serialized publisher or output relay, each with its own allowance.
`--jobs 4` does not mean one shared 1 GiB budget. Rayon/OpenMP thread counts are
restricted to one per worker. Capture limits are polled and checked after exit;
they are not a hard filesystem quota. Filesystem calls still depend on the OS.

The controller holds bounded paths, small receipts and input selection, and never
decodes document JSON. A separate limited publisher decodes results and stages
exports. A limited output relay copies bytes with an 8 KiB buffer. A full output
pipe can be interrupted without changing the caller-settable shared file flags.

## Failure, cancellation and publication

A failed extraction produces a `failed` record, no result rows and no exports;
other selected inputs continue. Page coverage is checked before publication.
Partial results are retained, but make the batch exit nonzero. SIGINT/SIGTERM stops
starting queued work and kills/reaps active workers. Pending inputs need not have
individual records after cancellation. The controller may attempt one bounded
cancellation diagnostic. Linux parent-death protection also kills a stopped worker.
On macOS, parent death closes the stdin lease and an EOF thread exits; that thread
cannot run while the entire worker is externally stopped. Ordinary cancellation
and deadlines still kill/reap from the live controller.

Publication uses the complete-file, exclusive-link transaction in
[PUBLICATION.md](PUBLICATION.md). Existing exports, including input aliases, are
preserved and a numbered generation is chosen. JSON includes actual `output_paths`.
The ledger and SQLite sidecars must not alias any selected input. `--db` accepts a
literal persistent filename; SQLite URI names, `:memory:`, and empty temporary
names are rejected. A concurrent adversary changing the namespace is outside this
contract.

Full JSON and a small receipt are prepared before the ledger commits. A successful
publisher exit plus a valid receipt confirms publication. If a publisher dies or
its receipt cannot be verified, the record says `publication_unknown`: complete
outputs or a committed ledger may exist. Inspect them before retrying. Forced
termination can leave staging files; this change has no restart/reconciliation
journal. A delivery failure after commit stops the stream and exits nonzero; it
does not append a second failure object after part of a success line. Consumers
must reject a truncated final JSON line and consult the ledger.

## Reproduction

```sh
cargo test --locked --bin tpe worker_limits:: -- --test-threads=1
cargo test --locked --bin tpe worker_allocator:: -- --test-threads=1
cargo test --locked --test cli_containment --test worker_startup --test end_to_end -- --test-threads=2
cargo test --release --locked --bin tpe worker_limits:: -- --test-threads=1
cargo test --release --locked --bin tpe worker_allocator:: -- --test-threads=1
cargo test --release --locked --test cli_containment --test worker_startup --test end_to_end -- --test-threads=2
```

Tests use generated inputs and checked-in public-safe native/existing-OCR controls.
They exercise real mmap/allocation failures, compressed expansion, descriptor
races, source/output/ledger aliases, stopped-worker deadlines, signal cancellation,
parent death, request/capture bounds, and pipe backpressure. The release workflow
runs these on native Linux x64/ARM64 and macOS ARM64. A configured workflow is not
evidence of a passing run; retain exact commit, runner and run IDs with each result.
