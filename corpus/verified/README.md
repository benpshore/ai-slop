# Verified source recovery

`PMC9866638.1.xml` is the exact CC BY source from the successful 200-paper
#133 measurement, run 36819388522 at source head
`0254da617f872ba862521012068cc2f6069d03b3`. Its article title, authors, journal
and license attribution are retained in the XML. The original source URL
and complete manifest item are in `PMC9866638.1.provenance.json`.

The S3 object at the same version-1 URL later returned MD5
`f303c1b2d310de7b52d0ea99dbf7a32b`, which correctly stopped integration run
36936337244. No pin was changed or verification relaxed.

Recovered the earlier bytes from #133's PR-scoped corpus cache in run
36937217326 (head `102638dfa417f8d16f067121d329fcf773d3cabf`, artifact
11198119892). Both that workflow and local recovery verified **110,342 bytes**,
MD5 `707148ab259c325c84acd030be5a5c8b` and SHA-256
`4bc081237097ef294c09e1f50b5f0a6d383f270ded3fcedbe8eb811661e6630b`.
The archive digest and source/recovery identities are recorded in provenance.

The fetcher seeds only this exact URL/MD5 combination, verifies both digests
and size, and otherwise keeps normal download/checksum behavior. The pinned
manifest remains byte-identical (SHA-256
`9465c36c416fbee0a17757ba9af41a13e89aced02cb6aad5883a49c1cf1a922e`).
No PDF is checked in, and new upstream bytes are not accepted as old truth.

## Rechecked against main on 2026-10-03

Main still used the original `01f4938a135da25e29d8ea722024b73e` XML pin.
This repair restores the reviewed `707148ab259c325c84acd030be5a5c8b` pin and
its recovered bytes together, preserving the exact manifest of the successful
200-paper run. The other 199 records and every PDF pin are unchanged.

A fresh public S3 check at 23:29 UTC returned metadata and XML both naming
`f303c1b2d310de7b52d0ea99dbf7a32b` (Last-Modified 2026-10-01 15:08:34 GMT).
The live and recovered XML agree on PMCID version, PMID, DOI, title and all
40 references. Their `<back><ref-list>` subtrees serialize identically with
Python ElementTree, SHA-256
`c1598a2763e480adbaa307f3ecc4ef47b78002cc3fe1f2ba65e8e6b2f1e61222`.
This confirms article and bibliography continuity; evaluation still uses the
recovered exact bytes, not a freshly accepted live digest. The older
`01f4938a...` bytes remain unavailable, so no claim is made that the initial
change was metadata-only.

A raw XML comparison of the recovered and live sources found only a
processing date change from `2026-09-30 10:33:41.727` to
`2026-10-01 10:52:25.903`. Git text conversion is disabled for the recovered
XML so a platform checkout cannot rewrite its pinned bytes.

Regression tests exercise offline recovery, stale-cache repair, missing and
same-sized corrupt snapshots, changed pins/URLs, unchanged checksum rejection
for other papers, and manifest/article/provenance identity.

GitHub's API was also rechecked: source run 36819388522 and recovery run
36937217326 both succeeded at their recorded PR heads; artifact 11198119892
reports the recorded archive SHA-256 and belongs to recovery run 36937217326.
The provenance's `recovery_commit` is GitHub's tested merge commit; its
second parent is recovery PR head `102638dfa417f8d16f067121d329fcf773d3cabf`.
