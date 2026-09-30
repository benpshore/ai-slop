# PDFTextract local API (`tpe-serve`)

Status: **new; built and tested on Linux (x86-64) only. Not yet run on a Mac,
and no client (app, TUI, browser page, MCP server) uses it yet.**

`crates/tpe-serve` is one HTTP API on the loopback interface so that the
PDFTextract app, a TUI, a browser page, an MCP server and scripts can all
drive the engine the same way. It offers the app's two actions, **get text**
and **get bibliography**, with the app's own job semantics: it calls
`tpe_app::jobs::run` (the code behind the app's two buttons), keeps its queue
in the app's `JobList` (one job at a time, oldest first) and stops jobs with
the app's `CancelToken`. Outputs land next to each PDF exactly as they do
from the app (`paper.txt`, `paper.references.json` + `.txt`, never
overwriting: `paper 2.txt`, …). Text jobs record their run in the same
ledger the app uses.

It is a library (`tpe_serve::Server`, `tpe_serve::spawn`) plus a small
binary, so the app could embed it in-process and other clients use it out of
process.

## Quick start

```sh
cargo run --release -p tpe-serve            # prints http://127.0.0.1:47470 on stdout
```

In another shell (the header comes from a file descriptor, so the token
never appears in the process list, which other users can read):

```sh
API=http://127.0.0.1:47470
auth() { printf 'Authorization: Bearer %s' "$(tpe-serve --print-token)"; }

curl -s $API/v1/health                                   # {"ok":true}, no token needed
curl -s -H @<(auth) -H 'Content-Type: application/json' \
     -d '{"action":"text","paths":["/Users/ben/Papers/paper.pdf"]}' $API/v1/jobs
curl -sN -H @<(auth) $API/v1/jobs/<id>/events            # progress, until it finishes
curl -s  -H @<(auth) $API/v1/jobs/<id>/output/0          # the text it wrote
curl -s  -H @<(auth) -X POST $API/v1/jobs/<id>/cancel
curl -s  -H @<(auth) -X DELETE $API/v1/jobs/<id>
```

`tpe-serve` options: `--bind 127.0.0.1|::1` (nothing else is accepted),
`--port N` (default **47470**, an arbitrary port outside the ephemeral range;
`--port 0` picks a free one), `--address-file FILE` (the URL, written once
listening), `--state-dir DIR`, `--allow-origin ORIGIN` (repeatable, none by
default), `--print-token`. The URL is the only line on stdout; logs go to
stderr. Ctrl-C or SIGTERM stops it: no new jobs, the running job is asked to
stop, and the process waits for it.

## Authentication

On first start the server draws 32 bytes from the operating system's CSPRNG
(`getrandom`) and writes them as 64 hex digits to `api-token` in the state
directory, which is the app's: `~/Library/Application Support/PDFTextract`
on macOS, `$XDG_DATA_HOME/PDFTextract` (or `~/.local/share/PDFTextract`)
elsewhere. The file is created with mode 0600 (the directory with 0700 if
the server creates it), written to a temporary name and hard-linked into
place, so two servers starting at once agree on one token. Later starts
reuse it. The server refuses to start if the file is a symbolic link, is
readable or writable by group or others, belongs to another user, or is
damaged. Rotate it by deleting the file and restarting.

Clients send `Authorization: Bearer <token>` on every request except
`GET /v1/health`. The comparison is constant-time (`subtle`). The token is
never accepted in a query string or a cookie, and never logged.

A local client gets the token by reading the file (it runs as the same
user) or with `tpe-serve --print-token`. A program embedding the server gets
it from `Server::token()`.

## Endpoints

All under `/v1`; the version is in the path. The machine-readable
description is `GET /v1/openapi.json` (hand-written, and a test checks it
lists exactly the routes the router serves and exactly the properties real
responses carry).

| method and path | what it does |
| --- | --- |
| `GET /v1/health` | `{"ok":true}`. No token; nothing else. |
| `GET /v1/version` | `api`, `name`, `version`, this run's `instance`, `actions`. |
| `GET /v1/openapi.json` | The OpenAPI 3.1 document. |
| `POST /v1/jobs` | Body `{"action":"text"\|"bibliography","paths":[…]}`; one job per path, all or none. 201 with `{"jobs":[Job…]}`. |
| `GET /v1/jobs` | `{"jobs":[Job…]}`, every job, arrival order. |
| `GET /v1/jobs/{id}` | One `Job`. |
| `POST /v1/jobs/{id}/cancel` | Stop a job (below). 200 `{"accepted":true,"job":Job}` or 409. |
| `DELETE /v1/jobs/{id}` | Forget a queued or finished job; 204. Its files stay on disk. A running job: 409 `job_running`. |
| `GET /v1/jobs/{id}/events` | Server-Sent Events (below). |
| `GET /v1/jobs/{id}/output/{n}` | Output `n` of a finished job, as written. |

**Paths.** Each must be an absolute path to an existing regular file ending
in `.pdf`, at most 4096 bytes, with no `.` or `..` segment (checked on the
text, before any file system access). Symbolic links are allowed (macOS keeps
`/tmp` and `/var` behind one) and resolved when the job is submitted: the job
reads the resolved file, writes its outputs next to it, and the resolved file
must also end in `.pdf` (so `x.pdf -> ~/.ssh/id_ed25519` is refused).
Directories, FIFOs, devices and sockets are refused. A refused batch queues
nothing and names each bad path by index and reason (`not_absolute`,
`dot_segment`, `not_found`, `not_a_file`, `not_pdf`, `unreadable`,
`invalid`), never by path.

**Job** (every field always present):

```json
{"id": "3f9a0c1e-7", "action": "text", "path": "/Users/ben/Papers/paper.pdf",
 "state": "finished", "done": 2, "total": 2,
 "progress": {"event": "page", "page": 2, "done": 2, "total": 2},
 "summary": "2 pages, 3 references", "warnings": [], "error": null,
 "outputs": [{"index": 0, "name": "paper.txt", "path": "/Users/ben/Papers/paper.txt",
              "url": "/v1/jobs/3f9a0c1e-7/output/0"}],
 "seq": 12}
```

`state` is `queued`, `running`, `cancelling`, `finished`, `failed` or
`cancelled` (the app's rows). `progress` is the engine's last progress event
in the shape `tpe extract --progress` prints. `path` is the path as sent;
`outputs[].path` is where the job wrote (next to the resolved PDF). `seq`
increases with every change and is the event id. Ids are
`<instance>-<n>`: `instance` is random per server run, so an id kept from an
earlier run is unknown (404), never another job.

**Bibliographies** are the engine's `bibliography::Record`, the JSON line
`tpe bibliography` prints and the app writes; `output/0` returns the file's
bytes unchanged, so the CLI, the app and the API agree on the shape (tests
check the served bytes equal the file and the keys equal the engine's
`Record`). A PDF with no reference list finishes with
`summary: "No reference list found"` and no outputs, as in the app.

**Cancel** follows the app: a queued job is cancelled at once and never
runs; a running job stops at its next page and writes nothing, not even to
the ledger. Once a job has started writing its results the stop is refused,
truthfully: 409 `cancel_too_late`, and the job finishes. A finished or failed
job answers 409 `already_finished`; cancelling a cancelled job is accepted
again (idempotent).

**Retries.** `POST /v1/jobs` accepts an optional `Idempotency-Key` header
(1–128 visible ASCII characters): repeating the same batch with the same key
returns the jobs it made (200) instead of queueing them again, for as long as
any of them exists; the same key with a different batch is 422
`idempotency_key_reused`. `GET`, `DELETE` and cancel are safe to repeat.

**Nothing expires.** Jobs stay until a client deletes them; there is no
timer on any state, no session and no prompt to answer in time. Event
streams are never timed out (a comment line every 15 s keeps them visibly
alive). A client driven slowly, through a switch or eye tracker, loses
nothing by being slow. Jobs are kept in memory: a restart of the server
forgets the list (the files it wrote stay).

### Events

`GET /v1/jobs/{id}/events` is `text/event-stream`. Each event is

```text
id: 12
event: job
data: {…the Job…}
```

A stream starts with the job's current state and sends its state again after
each change. It never queues: a watcher that is slower than the engine is
woken once and sends the state as it is then, skipping intermediate pages,
which is the app's own coalescing (the window draws only the newest progress
event per frame); each stream holds at most one event in memory. After the
final state (`finished`, `failed`, `cancelled`) the stream ends. If the job
is deleted the stream sends `event: removed` and ends.

Resume with `Last-Event-ID`: the server sends the current state if it is
newer than that id, and answers **204** if the client already has the final
state, which is how the HTML standard tells a browser `EventSource` to stop
reconnecting.

**Browsers.** `EventSource` cannot send an `Authorization` header, and the
token is never accepted in a URL. A browser client reads the stream with
`fetch()` and a `ReadableStream` (supported by every current browser), sending
`Authorization` and `Last-Event-ID` itself. No ticket endpoint was built; see
"Decisions" if that should change.

### Errors

Every non-2xx response is

```json
{"error": {"code": "invalid_path", "message": "…", "details": [{"index": 1, "reason": "not_pdf"}]}}
```

`code` is stable within `/v1`; branch on it, not on `message`. `details` is
always present (empty unless paths were refused). No message names a file
system path the client did not send: an engine failure mentioning the
resolved path or the state directory is rewritten to the path as sent and
`<state directory>`.

| status | codes |
| --- | --- |
| 400 | `malformed_json`, `invalid_request` (bad `Last-Event-ID`, unreadable body) |
| 401 | `unauthorized` (with `WWW-Authenticate: Bearer`) |
| 403 | `bad_host`, `bad_origin` |
| 404 | `no_route`, `not_found` (job or output) |
| 405 | `method_not_allowed` (with `Allow`) |
| 408 | `request_timeout` |
| 409 | `job_running`, `cancel_too_late`, `already_finished`, `not_finished` |
| 410 | `output_changed` |
| 413 | `payload_too_large` |
| 415 | `unsupported_media_type` |
| 422 | `invalid_request`, `invalid_path`, `idempotency_key_reused` |
| 429 | `job_limit` |
| 500 | `internal` |
| 503 | `shutting_down` |

A test checks that the codes the source uses and the codes the OpenAPI
document lists are the same set.

## Limits

| limit | default | on excess |
| --- | --- | --- |
| request body | 256 KiB | 413, whether announced by `Content-Length` or not |
| paths per request | 1000 | 422 |
| jobs kept (queued + running + finished, until deleted) | 10,000 | 429 `job_limit` |
| connections served at once | 64 | further connections wait in the kernel's accept queue |
| request head (line + headers) | 64 KiB | connection refused by hyper (431) |
| time to send a request head (also idle keep-alive) | 30 s | connection closed |
| time to send a body and get a response head | 60 s | 408 |
| jobs running at once | 1 (the ledger has one writer, as in the app) | queued |

All are fields of `tpe_serve::Limits`. Streams (events, output files) are not
cut by the 60 s once their response has started.

## Threat model

The server runs as the user, can read every PDF the user can read, and
writes new files (never over existing ones) next to them. Everything below is
about who else can make it do that.

**A remote machine** cannot connect: the server binds only `127.0.0.1` or
`::1` and refuses any other address at start-up (there is no override), and
it drops any connection whose peer is not loopback as a second fence.

**A web page in the user's browser** is the main reason for the checks on
every request, in this order:

1. *DNS rebinding.* A page on `evil.example` can make its own name resolve
   to 127.0.0.1, but the browser still sends `Host: evil.example`. Only
   `127.0.0.1:<port>`, `[::1]:<port>` and `localhost:<port>` pass, one `Host`
   header only, and absolute-form request targets are refused. This applies
   to `/v1/health` too.
2. *Cross-origin requests.* A browser names the page in `Origin` on every
   cross-origin request (and same-origin non-GET ones). Only the server's own
   origins pass, plus any `--allow-origin` (none by default); `null`
   (sandboxed frames, `file:` pages) never does. `Sec-Fetch-Site: cross-site`
   or `same-site` (another port on localhost counts as same-site) is refused,
   which covers no-CORS loads like `<img>` that carry no `Origin`. No
   `Access-Control-Allow-*` header is ever sent, so no other origin can read
   a response, and a CORS preflight is refused.
3. *CSRF.* The token travels only in `Authorization`, which a page can set
   only on a CORS request (refused above). There are no cookies, so the
   browser has no ambient credential to attach.
4. *Content-type confusion.* A JSON body must be `Content-Type:
   application/json`, which a page cannot send cross-origin without a
   preflight; form and `text/plain` posts are 415.

What a page can still do: learn that something listens on the port (timing,
connection errors), cause refused-request log lines, and hold some of the 64
connection slots while it is open (browsers limit connections per host, but
many tabs could add up), delaying other clients. It cannot queue, cancel,
delete or read anything.

**Another user account on the same machine** can connect to loopback (it is
not per-user) but lacks the token: the token file is mode 0600 in the user's
directory, and the server refuses a token file with looser permissions or
another owner. Other users can read the command lines of this user's
processes on most systems (`ps`, `/proc/<pid>/cmdline`), so never put the
token in a command's arguments (`curl -H "Authorization: Bearer $TOKEN"`
does); pass it through a file descriptor as in the quick start.

**A malicious process running as the same user** can read the token file,
as it can read the user's PDFs and everything else the user owns. With the
token it can do whatever the API allows: extract text from any `.pdf` the
user can read into new files beside it (in any directory the user can write),
read those files back, fill the queue up to the job limit, and cancel or
delete other clients' jobs. The token file's permissions do not protect
against this, and nothing inside the server can; such a process could run
the engine itself. What the token does stop is a same-user process that is
confined by an operating-system sandbox which allows loopback networking but
not reading `~/Library/Application Support/PDFTextract` (macOS App Sandbox
apps, for example): it can reach the port but cannot authenticate. The token
file is also in the user's backups; delete it to rotate. Between submission
and the job starting, a same-user process could swap the file at a
submitted path; the job then reads whatever is there.

**A malicious PDF** is parsed by the engine inside the server process, with
no sandbox. An engine panic fails that job only (`jobs::run` catches it) and
writes nothing. The engine bounds known pathological inputs (the repository's
"Bound …" changes), but a PDF that makes one page run for a very long time
holds the queue (cancel takes effect only between pages; restart the server
to stop it), and one that exhausts memory can take the server down. PDF text
is written only to the output files, never logged; it is served with its
file type (`text/plain` or `application/json`), `X-Content-Type-Options:
nosniff` and `Content-Security-Policy: default-src 'none'`, so a browser
never renders it as a page.

**Outputs** are served only through `/v1/jobs/{id}/output/{n}`, only for
files the job wrote, and only while the name still refers to that file: the
server records each output's device and inode when the job finishes, opens
the name without following symbolic links, and answers 410 `output_changed`
if either check fails. There is no endpoint that takes a path to read.

The server makes no network requests and runs no subprocesses. It writes
only the token file, the ledger (text jobs, in the state directory) and the
job outputs beside each PDF.

## Connecting each consumer

- **The app (GPUI).** Can embed the server with `tpe_serve::spawn(config)`,
  which starts it on its own thread and runtime and returns the address and
  token; GPUI's executor is not Tokio, so the server keeps its own. Today the
  server keeps its own `JobList`, separate from the window's: an embedded
  server would show API-submitted jobs only through the API. Sharing one
  job list between the window and the API is the next step if the app should
  show them. (Not done in this change; no app code changed.)
- **A TUI or script.** Read the token file (or `--print-token`), use
  `--address-file` or the default port, and call the endpoints; follow
  progress with the event stream and fetch results with `output/{n}`.
- **A browser page (PWA).** Not built. It must be served by this server
  (same origin), so no CORS is ever needed; `/` and `/app/…` are left free
  for it. It would be a small static shell embedded in the binary and served
  without a token (like `/v1/health`, and behind the same `Host`,
  `Origin` and `Sec-Fetch-Site` checks), with a strict CSP. The person would
  paste the token once (or a pairing flow would supply it); the page keeps it
  in `sessionStorage` and reads events with `fetch()` streaming.
- **An MCP server.** Not built. A stdio MCP server would read the token file
  and wrap the endpoints as tools: `get_text(paths)` and
  `get_bibliography(paths)` (`POST /v1/jobs`, then follow
  `/events`, forwarding each event as an MCP progress notification, then
  return `output/0`: the text, or the bibliography `Record`),
  `list_jobs` (`GET /v1/jobs`), `job_status(id)`, `cancel_job(id)` and
  `delete_job(id)`. It should pass an `Idempotency-Key` per tool call so a
  retried call does not queue twice, and report `cancel_too_late` to the
  model as "already writing, will finish".

## Decisions to review

- **hyper directly, not axum.** hyper 1, hyper-util and tokio were already
  in `Cargo.lock` (through reqwest and tpe-speech), so using them directly
  adds one package to the lock (`httpdate`); axum would add at least eight
  (axum, axum-core, matchit, serde_path_to_error, serde_urlencoded,
  sync_wrapper, tower-layer, mime and more). Ten fixed routes need no router
  library; all checks run in one function in a fixed order (`http.rs`
  `dispatch`), every error has the one shape without overriding extractor
  rejections, and the accept loop is ours, which is where the connection
  limit and the header timeout live. Overrule if axum's ergonomics matter
  more for later endpoints.
- **No token for browser event streams.** Browsers use `fetch()` streaming
  with the header, not `EventSource`. The alternative is a single-use,
  short-lived ticket (`POST /v1/tickets` → a random ticket valid once, for
  one stream, for about a minute, passed as `?ticket=`); it was not built
  because it puts a credential in a URL and adds a timed state.
- **No Unix domain socket.** One transport keeps one set of checks to review.
  A socket (mode 0600 in a 0700 directory) would be a good second transport
  for the TUI and MCP server: no port, no `Host`/`Origin` concerns, and
  access decided by the file system. It is a small addition (the same
  `serve_connection` on a `UnixListener`).
- **Symbolic links allowed, resolved at submission**, with the target also
  required to end in `.pdf`. Refusing them would break ordinary macOS paths
  (`/tmp`, `/var`) and gains nothing against the same-user attacker who
  could create them.
- **One ledger with the app.** Text jobs write the app's ledger; SQLite's WAL
  mode and 5 s busy timeout (`src/ledger.rs`) let the app and the server
  write it from two processes. The server's queue and the window's are
  separate, so two jobs can run at once, one in each process.
- **Jobs live in memory**, until deleted or the server stops. Persisting the
  list is possible but was left out to keep the change small.
- **Job limit 10,000 by default**, because the full list is built under the
  engine's progress lock (see "Measured").

## Dependencies

What `tpe-serve` compiles on Linux beyond what `tpe-app` already does (from
`cargo tree -e normal`): tokio, hyper, hyper-util, http-body,
http-body-util, httpdate, mio, socket2, signal-hook-registry, atomic-waker,
errno. All but httpdate were already in `Cargo.lock`. Its direct
dependencies and why:

| crate | why | licence |
| --- | --- | --- |
| tokio (`rt-multi-thread`, `net`, `time`, `sync`, `fs`, `io-util`, `signal`) | async I/O, timers, the per-job `watch` channel, SIGTERM | MIT |
| hyper (`server`, `http1`), hyper-util (`tokio`) | HTTP/1.1 server, header timeout | MIT |
| http, http-body-util, bytes | request/response types, body limits, streamed bodies | MIT OR Apache-2.0 / MIT / MIT |
| getrandom 0.4 | the token and instance id from the OS CSPRNG | MIT OR Apache-2.0 |
| subtle | constant-time token comparison | BSD-3-Clause |
| libc | `O_NOFOLLOW`, `ELOOP`, `geteuid` | MIT OR Apache-2.0 |
| hex, serde, serde_json, clap, futures | as elsewhere in the workspace | MIT OR Apache-2.0 |
| text-processing-engine, tpe-app | the engine and the app's job model | MIT |

Checked on 2026-09-30 against the RustSec advisory database (revision
`f23b768`, 2026-09-29): every advisory for these crates is patched at the
versions in `Cargo.lock` (tokio 1.53.1, hyper 1.11.1, http 1.5.0, mio 1.2.3,
socket2 0.6.5, bytes 1.12.1). The repository has no `cargo audit` or
`cargo deny` step, so this was a manual check. Depending on `tpe-app` means
that on macOS building `tpe-serve` also builds GPUI (a macOS-only dependency
of that package), though the server does not use it; moving `jobs.rs` into a
small crate of its own would remove that once the app's open PRs have landed.

## Measured

`tests/latency.rs` (ignored; `cargo test --release -p tpe-serve --test
latency -- --ignored --nocapture`), 2026-09-30, on a **Linux x86-64 cloud VM,
not Apple silicon**: 4 vCPUs (Intel Xeon @ 2.80 GHz), 15 GiB RAM, kernel
6.18. The VM was **shared and busy** during every run (load average 5–6 on
4 vCPUs, CPU pressure `some` 30–41 %, another session's compiler running),
so the tails include scheduling delay. Client-side wall time, milliseconds,
each request on a new loopback TCP connection (connect included), to the
last byte of the response. Three runs; n, min, median, p95, p99 (n ≥ 100
only), max, max/median.

| what | run | n | min | median | p95 | p99 | max | max/median |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `GET /v1/health` | 1 | 1000 | 0.085 | 0.165 | 0.304 | 0.777 | 3.963 | 24.0 |
|  | 2 | 1000 | 0.071 | 0.098 | 0.193 | 0.351 | 1.472 | 15.0 |
|  | 3 | 1000 | 0.071 | 0.092 | 0.179 | 0.317 | 0.914 | 10.0 |
| `GET /v1/jobs/{id}`, 5,000 jobs, engine busy | 1 | 1000 | 0.086 | 0.144 | 0.297 | 0.720 | 1.928 | 13.4 |
|  | 2 | 1000 | 0.075 | 0.094 | 0.180 | 0.325 | 4.295 | 45.6 |
|  | 3 | 1000 | 0.074 | 0.135 | 0.280 | 0.573 | 1.472 | 10.9 |
| `GET /v1/jobs`, 5,000 jobs, engine busy (1.0 MB) | 1 | 200 | 3.329 | 5.216 | 8.670 | 13.078 | 14.854 | 2.8 |
|  | 2 | 200 | 2.336 | 2.777 | 4.866 | 8.194 | 21.559 | 7.8 |
|  | 3 | 200 | 2.481 | 3.448 | 8.064 | 13.023 | 14.651 | 4.2 |
| `GET /v1/jobs`, 5,000 finished jobs (2.6 MB) | 1 | 200 | 9.634 | 11.400 | 18.435 | 20.613 | 23.181 | 2.0 |
|  | 2 | 200 | 9.209 | 11.370 | 18.355 | 22.399 | **98.704** | 8.7 |
|  | 3 | 200 | 10.226 | 11.277 | 15.070 | 16.447 | 18.655 | 1.7 |
| cancel → each of 100 event streams shows it | 1 | 100 | 1.087 | 3.246 | 5.933 | 8.064 | 8.098 | 2.5 |
|  | 2 | 100 | 6.205 | 8.239 | 13.495 | 14.427 | 14.856 | 1.8 |
|  | 3 | 100 | 0.577 | 2.713 | 4.340 | 4.484 | 4.500 | 1.7 |
| `POST /v1/jobs`, 1000 paths | 1 | 5 | 17.018 | 21.095 | 27.627 | n/a | 27.627 | 1.3 |
|  | 2 | 5 | 17.491 | 22.519 | 75.820 | n/a | 75.820 | 3.4 |
|  | 3 | 5 | 16.846 | 19.085 | 30.894 | n/a | 30.894 | 1.6 |

What the numbers say:

- Single-job requests and health checks are about 0.1 ms at the median with
  tails to 1–4 ms; the tails are the same at the first byte as at the last,
  so they are not transfer time. Not investigated further; on this busy VM
  scheduling delay is the likely cause, but that is not shown.
- **The full job list grows with the number of jobs**: about 2 µs per
  finished job (first byte at a median of 9.7–10.6 ms for 5,000, so most of
  the time is building the JSON). That work happens under the lock the engine
  thread takes to report each page, so a large list delays progress by as
  long. This is why the job limit defaults to 10,000 (about 20 ms at worst
  by this measurement) rather than more. One ~100 ms stall occurred in run 2
  (and a 124–177 ms tail in an earlier run with a slower client); it was on
  the server side of the first byte, it did not recur in the other runs, and
  its cause is not known. A client that polls should use the event streams,
  or a future `GET /v1/jobs?since=<seq>` that returns only what changed
  (not built).
- Fan-out: with 100 streams on one running job, a cancel reached all of
  them in 4.5–15 ms (worst stream), 2.7–8.2 ms at the median.
- `POST` of 1000 paths (each resolved on the file system) takes about 20 ms.
- Engine throughput varied 2.9× between runs (5,000 two-page bibliography
  jobs in 13.0, 38.4 and 29.8 s), another sign of the shared machine.

Not measured: Apple silicon, the app's own path, a persistent (keep-alive)
connection, memory use.

## Verified and reasoned

Verified on Linux x86-64 by `cargo test -p tpe-serve` (unit tests and
`tests/api.rs`, a real server on an ephemeral loopback port and an HTTP/1.1
client written on a socket so every header is under the test's control):
both actions end to end on the synthetic paper, with the event stream and
the output bytes; token missing, wrong, upper-cased, truncated, `Basic`,
bare, doubled or in the query string (401), right (200); rebinding and
foreign `Host` values, a missing or doubled `Host` and an absolute-form
target (403); foreign, `null` and near-miss origins on GET, POST, the event
stream and a preflight, `Sec-Fetch-Site` cross-site and same-site (403), no
CORS header ever; an extra origin only when configured; non-loopback
addresses refused by the library and the binary with nothing created; the
token file made 0600, reused, printable, and refused at 0644; wrong content
types (415), oversized bodies with and without a length (413), malformed and
mis-shaped JSON (400/422); path refusals by reason with nothing queued and no
path in the error; a followed link writing beside its target; cancel of a
queued job, of a running job part-way (nothing written, not even the
ledger), of a finished job (409) and of a job blocked writing the ledger
(409 `cancel_too_late`, then it finishes); delete rules; idempotent retries;
the job limit; resume with `Last-Event-ID` (204 when up to date);
keep-alive comments on an idle stream; three clients at once; the output
endpoint refusing other indices, traversal attempts, a replaced file and a
link (404/410); header, body and idle timeouts; the connection limit; and the
OpenAPI document against the routes, the responses and the error codes.

Each control was also removed, one at a time, and its test was run to see
it fail (24 mutations, each replacing one exact line and restoring it after):
the `Host` allowlist; the absolute-form check; the `Origin` allowlist (with
`null`); `Sec-Fetch-Site`; the token comparison; auth being applied at all;
the loopback-only bind; the token file's 0600 mode; the refusal of a
readable token file; the `application/json` check; the streamed body limit;
the `.`/`..` check; the link-target `.pdf` check; the regular-file check;
the output's device-and-inode check; the header read timeout; the request
timeout; the connection limit; the truthful cancel refusal; the security
response headers; the redaction of engine messages; the job limit. All of
these made their test fail. Two did not, as expected: replacing the
constant-time comparison with `==` (same answers, different timing), and
dropping `O_NOFOLLOW` when opening an output (the device-and-inode check
still refuses a link to another file; the two are layered).

Reasoned, not tested: that constant-time comparison resists timing (the
tests cannot tell it from `==`); that no-CORS browser loads carry
`Sec-Fetch-Site` (tested with the header, not with a browser); behaviour on
macOS (not built there; the code uses only POSIX calls); the `::1` listener
(the test machine has no IPv6: binding it fails with "Address family not
supported", which `tpe-serve` reports before exiting 1; `[::1]:<port>` is
tested only as a `Host` value); anything a real browser, the app or an MCP
client would do. Checked by hand with the binary and curl on 127.0.0.1: the
URL on stdout and in `--address-file`, the quick-start `curl -H @<(auth)`
form, 401 without the token, 403 for a rebinding `Host`, and a clean exit on
SIGTERM.

## Not done

- No browser shell (PWA), no MCP server, no Unix socket, no app
  integration.
- Job list not persisted across restarts.
- Not run on macOS or measured on Apple silicon.
- No passwords for encrypted PDFs (the app has none either).
