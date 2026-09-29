# Resource and storage policy

`tpe` applies one resolved `ResourcePolicy` to extraction, corpus fetching,
evaluation, ledger publication, benchmarks, and optional figure/diagnostic
exports. Select `--resource-profile balanced`, `low-write`, or `durable`.
Overrides such as `--max-heavy-jobs`, `--max-write-bytes-per-second`, and
`--max-write-bytes-per-day` are applied after the preset. At process startup
the complete resolved policy is printed as JSON on stderr; a preset therefore
never hides an effective setting.

`balanced` uses bounded concurrency and SQLite WAL with `synchronous=NORMAL`.
`low-write` limits heavyweight work to one job, disables optional exports
unless the corresponding output option is explicitly supplied, reuses
content-addressed artifacts, batches ledger work in bounded transactions, and
enforces write-rate and daily logical-byte budgets. `durable` selects
`synchronous=FULL` and single-item ledger batches. Required output is never
dropped, and none of the profiles weakens correctness checks or required
durability merely to save writes. A budget violation is an explicit failure,
not a partial success.

Resource reports record logical bytes generated; physical bytes written when
the OS exposes that counter; bytes avoided by deduplication; WAL/checkpoint
bytes; temporary bytes; cache growth; free-space headroom; and resource-limit
failures. Unsupported physical-write counters are JSON `null`, rather than an
estimate. Evaluation `report.json` and benchmark output include both the
resolved policy and these counters.

## macOS and APFS

The application confines itself to file-level controls. APFS copy-on-write,
compression, snapshots, purgeable space, swap behavior, TRIM, and SSD firmware
can make logical and physical writes differ substantially. These are host
administrator decisions and **are never changed automatically by `tpe`**.

Read-only diagnostics:

```sh
diskutil info /
diskutil apfs list
tmutil listlocalsnapshots /
sysctl vm.swapusage
df -h /
```

Deploy the cache and ledger on a volume with monitored headroom, retain the
default APFS/OS durability behavior, size snapshot retention outside the
application, and validate backups by restoration. Administrators should make
swap, snapshot, TRIM, and firmware decisions according to their fleet policy.

## Linux and ext4

Linux swap or zram configuration, ext4 mount options and journal mode,
discard/TRIM scheduling, block-device caches, and drive firmware are likewise
administrator-owned and **are never modified automatically by `tpe`**.

Read-only diagnostics:

```sh
findmnt -no SOURCE,FSTYPE,OPTIONS /
cat /proc/swaps
zramctl
df -hT /
lsblk -o NAME,FSTYPE,SIZE,ROTA,DISC-GRAN,DISC-MAX,MOUNTPOINTS
cat /sys/block/nvme0n1/queue/write_cache
```

Use a dedicated, capacity-monitored filesystem for large caches; keep SQLite
WAL, database, and temporary files on reliable local storage; arrange backups
and periodic restore tests; and use the distribution's normal `fstrim` service
when appropriate. Evaluate ext4 journaling/mount changes and device firmware
with the storage administrator—do not embed privileged tuning scripts in the
application deployment.
