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
