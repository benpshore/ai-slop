# Job publication boundary

Applies to `tpe-app::jobs`, independently of the GPUI frontend.

1. Extract and serialize all output bytes. Stage every file in the source
   directory using exclusively created temporary files. On Unix they request
   mode `0666` at creation; the caller's umask narrows it (for example, `022`
   produces `0644`, and `077` produces `0600`). Publication preserves this mode;
   no code reads or changes the process-wide umask or widens permissions later. Flush each
   file with `sync_all`. A staging failure never starts a ledger replacement.
2. For text jobs, prepare the full result and timings inside one SQLite
   transaction. Replacing an older result is still reversible at this point.
3. Hard-link each complete staged file to its final sibling name. Link creation
   is exclusive: an existing file, directory, or symlink is never overwritten.
   A collision on either bibliography filename removes this attempt's links
   and retries both names with the next generation. No partial-write fallback
   is used on filesystems without hard links; those fail explicitly.
4. Sync the output directory, then commit SQLite last. A handled publication
   failure rolls back the pending result; a commit failure removes the new
   output links. Dropping the transaction restores the previous run as well
   as its timings. Bibliography has no ledger and finishes after directory sync.
5. Report success and release staging names. No further fallible ledger write
   happens after success. Temporary files are cleaned up on ordinary errors
   and Rust unwinding. If removal or directory sync fails, the returned error
   states that cleanup was incomplete and names retained output paths.

The guaranteed boundary is **handled failure**, with cleanup errors reported
explicitly. An empty initialized ledger or its parent directory may remain
when no result committed. The source PDF is never modified.

SQLite and separate sibling files do not share a crash-atomic transaction.
A reader can briefly see the first bibliography file before the second, or a
complete text output before the ledger commit. Power loss, SIGKILL, or forced
termination in that interval can leave complete orphan outputs and hidden
staging files. The previous ledger result remains intact if the transaction
has not committed. A rerun preserves those files and chooses a new generation.
There is no automatic orphan adoption/deletion or crash recovery journal in
this change. Such recovery needs persistent artifact identities and a journal;
do not advertise this as all-or-nothing crash durability.

Concurrent cooperating publishers are covered. Rollback checks device/inode
identity before unlinking, preserving a name already replaced by another
owner. This is not a security boundary against a process actively renaming
files between that check and unlink. Publication currently targets macOS and
Linux on filesystems supporting hard links and directory sync.

Tests inject failure after the first bibliography link, a collision during the
second link, and a commit failure; exercise eight concurrent publishers and
replacement ownership; and force a real deferred-constraint SQLite commit
failure to verify restoration of the previous result. Existing job tests check
successful text/bibliography output and extraction failures before staging.
