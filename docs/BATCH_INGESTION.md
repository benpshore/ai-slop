# Bounded ingestion batches

`tpe ingest-batch` feeds the native ingestion command from a JSONL manifest. It
does not keep the corpus or its results in a single in-memory collection. Each
document runs in a fresh child process; the parent publishes complete records
as they finish. This bounds concurrent parser work and lets a failed or timed
out document be reported without stopping subsequent inputs.

```sh
cargo build --release --features formats
printf '%s\n' '{"path":"/data/paper.pdf"}' '{"path":"/data/book.xlsx"}' > inputs.jsonl
./target/release/tpe ingest-batch --input-list inputs.jsonl --jobs 2 > results.jsonl
```

Use `--input-list -` for a live stdin producer. Input reading runs independently
from output publication: the producer can send one path, wait for its result,
then send another without closing the manifest. Each nonblank line must contain
exactly an object with a `path` field. Paths are passed directly as arguments,
not interpreted by a shell. Blank lines are ignored; malformed or oversized
lines produce a failed record and processing continues.

## Records and recovery

Each line has schema `tpe.ingest-batch.v1`, the original one-based `input_line`,
`worker_ms`, the configured `supervisor` limits, and a `result` containing the
`tpe.ingest.v1` record described in [INGESTION.md](INGESTION.md). Results are in
completion order. Duplicate paths remain separate jobs. The wrapper creates no
database and does not deduplicate or checkpoint inputs automatically.

`worker_ms` includes child startup, file acquisition, conversion, capture and
JSON decoding. It excludes queue delay and parent output publication; it is not
per-page extraction time. The nested `elapsed_ms` retains the ingestion command's
narrower timing definition. If the child crashes, times out, exceeds a capture
limit or emits an invalid record, the parent emits `failed` with null source
identity. It does not guess a hash from an unsuccessful child.

Exit zero means every emitted input result is `extracted`. Other outcomes,
including `review_required`, `needs_ocr` and `needs_transcription`, produce a
nonzero final exit while still preserving all completed records. SIGINT,
SIGTERM, unreadable manifest input and broken result output stop scheduling and
cancel active workers. Previously emitted records remain useful. After an
interruption, retain only complete JSON lines and reconcile their `input_line`
values with the original manifest before retrying. There is no exactly-once
processing or atomic result-file guarantee.

## Resource policy

| Control | Default | Meaning |
| --- | --- | --- |
| `--jobs` | 2 | At most this many child workers; allowed range 1–64. Job and result queues are each bounded by this count. |
| `--timeout-ms` | 30000 | Deadline per child, including startup and capture. Cancellation is polled; this is not a real-time scheduling guarantee. |
| `--max-output-bytes` | 67108864 | Captured child stdout limit; allowed range 1 byte–1 GiB. Stderr has a separate 16 KiB cap. |
| `--worker-memory-mib` | 1024 on Linux; 0 on macOS | Linux `RLIMIT_AS` soft limit, constrained by the inherited hard limit. This bounds virtual address space, not measured RSS. Zero disables this limit. |
| Manifest line | 64 KiB | Oversized lines are drained without retaining the remainder, then reported as failed. |
| Native ingestion limits | See [INGESTION.md](INGESTION.md) | Source size, expanded archive size, archive entries, cells and PDF pages are forwarded to every worker. |

The supervisor sets `RAYON_NUM_THREADS=1` in each child so process concurrency
owns that thread budget. Aggregate memory still increases with `--jobs`: each
worker has its own parser state, and the parent holds bounded queues plus a
small number of complete per-document records. JSON decoding and serialization
can allocate more memory than the encoded record size. The parent itself has no
hard address-space cap. Tune concurrency and output limits for the host; these
defaults are not a Raspberry Pi capacity measurement.

Workers use separate Unix process groups. On cancellation or completion, the
parent kills the group and reaps the direct worker. Capture has its own deadline
and stops even if a detached descendant holds a pipe open. Linux also requests
that the direct worker be killed if its supervising thread dies. This is process
supervision, not a sandbox: a deliberately detached descendant can escape the
group, and filesystem/network access is not restricted. For enforceable total
memory/process containment, deploy within an appropriate container or cgroup.

The initial implementation requires Unix. macOS supports time/output/process
controls, but a nonzero hard memory limit is rejected there rather than silently
ignored. The public Rust `batch::run` receives a caller-owned cancellation token
and never installs process signal handlers; only the CLI installs handlers for
its lifetime. Use a fresh token for each call.

## Scope and evidence

This supervisor runs the existing native ingestion adapters. It does not add
OCR, audio transcription, bibliography extraction, external metadata lookup or
credential forwarding. Password-protected inputs need the direct password-aware
`tpe ingest` interface until a separate batch credential policy is implemented.
`extracted` still means that the adapter completed, not independently certified
accuracy.

Regression fixtures exercise continued processing after malformed manifest
lines, missing files and excess worker output; exact Unicode text; result/input
correlation across concurrent repeated jobs; live-input publication before EOF;
SIGTERM cancellation of idle input; deadline and process-group cleanup; capture
when another writer retains a pipe; and Linux address-space limit installation.
These are deterministic functional checks, not an arbitrary-corpus accuracy
benchmark or a million-document throughput measurement.
