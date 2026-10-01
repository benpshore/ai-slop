# Citation graph across a local library: design memo

Status: design only. Nothing here is implemented, and the numbers are measurements of the
current engine, not promises.

## Scope

An edge means "paper A's **official bibliography** lists work B". Edges come only from the
reference list (ledger `"references"`, `reference_authors`, `metadata`, `authors`). In-text
markers, `citations` and `citation_targets` are not used, and an edge is never weighted by how
often a work is cited in the body. The list is the start-at-the-end backward scan
(`tpe bibliography`, [BIBLIOGRAPHY.md](BIBLIOGRAPHY.md)) plus a supplementary bibliography once
the canonical bibliography work lands it. The reference route of `tpe extract` is not relied on.

## What the corpus looks like

Input: the 60 cached arXiv PDFs (`.corpus-cache`, the `dev` split of `corpus/manifest.json`),
release build of this repository on a 4-vCPU Xeon VM shared with other builds, 2026-09-30.
`tpe bibliography` found a list in 60/60 and 3,686 entries (per paper: n=60, min 12, median 48.5,
p95 139, max 341).

| field on a reference (share of a paper's entries, %) | n | min | median | p95 | max | pooled |
| --- | --: | --: | --: | --: | --: | --: |
| DOI | 60 | 0.0 | 0.0 | 64.8 | 93.5 | 15.3 (563/3,686) |
| arXiv id | 60 | 0.0 | 11.6 | 55.8 | 82.1 | 17.0 (628) |
| DOI or arXiv id | 60 | 0.0 | 22.8 | 82.3 | 93.5 | 31.0 (1,143) |
| title, year and at least one author | 60 | 86.7 | 100.0 | 100.0 | 100.0 | 98.9 (3,644) |

The median paper has no printed DOI at all, so identifier joins alone leave about 69% of
references unjoinable. The lead's 5-paper figure (19% DOI, 11% arXiv id) sits inside these ranges.
Time to read the lists: `elapsed_ms` per PDF n=60, min 12.6, median 122.9, p95 974.3, max 4,955.8
(median 6 of 23 pages scanned); loaded machine, so treat as an upper bound.

## Edge tables (proposal; the schema and migration mechanism belong to `db/wal-hardening`)

Two derived tables, both fully recomputable from `metadata` and `"references"`:

```sql
-- every identifier a library document answers to (its own DOI/arXiv id, and a published DOI
-- or preprint id found later by external resolution)
CREATE TABLE library_ids (
  hash   TEXT NOT NULL REFERENCES documents(hash) ON DELETE CASCADE,
  kind   TEXT NOT NULL CHECK (kind IN ('doi','arxiv')),   -- normalised; arXiv DOIs folded to arxiv
  value  TEXT NOT NULL,
  source TEXT NOT NULL,                                   -- 'pdf' | 'crossref' | 'openalex'
  PRIMARY KEY (kind, value, hash));

-- one row per (reference, candidate library document)
CREATE TABLE reference_links (
  run_id INTEGER NOT NULL, ref_idx INTEGER NOT NULL,
  target_hash TEXT NOT NULL REFERENCES documents(hash) ON DELETE CASCADE,
  rung   TEXT NOT NULL CHECK (rung IN ('doi','arxiv','title','external')),
  status TEXT NOT NULL CHECK (status IN ('resolved','ambiguous')),
  resolver_version INTEGER NOT NULL,
  PRIMARY KEY (run_id, ref_idx, target_hash),
  FOREIGN KEY (run_id, ref_idx) REFERENCES "references"(run_id, idx) ON DELETE CASCADE);
```

No row means "not in the library, or not resolvable yet"; the reference itself is never edited.

## Matching ladder (first rung that yields candidates wins)

1. **DOI.** Normalise (lower case, resolver prefix off). `10.48550/arXiv.X` is folded into arXiv id
   `X`; in the corpus one of the two identifier "disagreements" between references to the same work
   is exactly that spelling.
2. **arXiv id**, version suffix dropped.
3. **Title key.** NFKC, accents folded, lower case, alphanumeric tokens; at least 4 words; equal
   keys and years within 3. Authors are a tie-breaker, not a gate (below).
4. **External.** Crossref or OpenAlex resolves a reference without an id to a DOI, and a preprint
   DOI to the published DOI, stored in `library_ids`. Crossref exposes preprint-to-article links as
   `relation.is-preprint-of` (a live query returned 842,153 works with it, 2026-09-30). Matching a
   reference to Crossref, its statuses, cache and rate limits belong to `bibliography/verify`; this
   memo only consumes a resolved DOI and its status.

**Ambiguity is stored, never resolved silently.** If a reference has several candidates (or two
library files hold the same work, for example a preprint and the published PDF), every candidate
gets a row with `status='ambiguous'`; exactly one candidate gives `resolved`. The reader shows
"n candidates". There is no tie-break by year, size or hash.

### Evidence for the title rung

Measured on the same 3,686 references. A "true pair" is two references in different papers that
share a DOI or arXiv id (arXiv DOI folded): 82 pairs, 19 works (cited by 2 papers: 10 works; 3: 4;
4: 2; 5: 2; 8: 1), in 58 of 1,143 identifier-bearing references (5.1%).

| key on the 82 true pairs | recall |
| --- | --: |
| title + first-author surname + year within 1 | 45/82 (55%) |
| title + year within 3 | 80/82 (98%) |
| title only, 4+ words | 70/82 (85%) |
| title + year within 3, and a shared surname among the first 3 authors | 65/82 (79%) |

Version differences break the author key: `Dubey` vs `Grattafiori` vs `Team` for Llama 3,
`Achiam` vs `OpenAI`, `Guo` vs `DeepSeek-AI`. Years differ by 0 in 75 pairs, 1 in 4, 2 in 1, 3 in 2
(Adam: 2014 vs 2017). Precision proxy: 216 cross-paper reference pairs share an identical
normalised title (3+ words); 82 have an id on both sides, 80 agree, 2 differ and both are
preprint versus published DOI of one work (BERT, and a 2024 survey with a 2026 ACM DOI), not
false merges. That is 0 observed false merges in 80 checkable pairs (95% upper bound about 3.7%,
rule of three); the other 134 pairs cannot be checked. The sample is small and biased toward
famous papers.

## Incremental update

Adding paper P, in one transaction: (1) write P's `library_ids` from `metadata`; (2) forward:
resolve each of P's references against `library_ids` and the title-key index; (3) reverse: find
existing references (in any paper) whose DOI, arXiv id or title key equals one of P's, and add
rows; (4) re-evaluate `ambiguous` status only for references that gained a candidate. Cost is P's
reference count plus hits, from indexed lookups (`references_doi` already exists; a title-key
index needs a derived column or an in-memory map, about 3.7k references for 60 papers).
Removing a document cascades its rows; references left with one candidate become `resolved`.
Bumping `resolver_version` deletes older rows and recomputes, so a rule change never leaves a
mixed graph. `run_id` changes when a document is re-extracted with another backend, so links are
recomputed from the run chosen as current (rule: latest `complete` run per hash).

## The two queries

```sql
-- cites: what does A's bibliography list that is in the library
SELECT t.hash, l.rung, l.status FROM reference_links l
  JOIN runs r ON r.id = l.run_id JOIN documents t ON t.hash = l.target_hash
 WHERE r.hash = :a;
-- cited-by: which library papers list B
SELECT r.hash, l.rung, l.status FROM reference_links l JOIN runs r ON r.id = l.run_id
 WHERE l.target_hash = :b;
```

Both restrict `r` to the current run per hash. Chaining is a bounded recursive CTE over these
two selects; `ambiguous` rows are shown but not followed unless the user asks.

## Expected edge-resolution rate

Measured on this corpus: **0 edges** in the library at every rung (DOI 0, arXiv id 0, title key 0,
fuzzy title 0) out of 3,686 references. The 60 papers are unrelated by construction (at most five
per arXiv primary category), and no rung found a reference of one naming another. So the rate for a real library cannot
be estimated from `.corpus-cache`; it depends on topical density. The joins are correct where
testable (co-citation pairs above), and the first deliverable on a real library should be a
per-rung report: references, resolved by rung, ambiguous, in-library share. Not measured here:
the external rung (needs live Crossref matching, owned by `bibliography/verify`), and any rate on
a topical library.

## Migration safety

Both tables are derived and can be dropped and rebuilt, so rollback is `DROP TABLE` and nothing in
existing tables changes. Creation should follow whatever mechanism `db/wal-hardening` lands. The
existing precedent is `FIGURES_SQL` in `src/ledger.rs` (`CREATE TABLE IF NOT EXISTS` on open,
`SCHEMA_VERSION` unchanged). The ledger opens with `PRAGMA foreign_keys = ON`, so the cascades
above are enforced. This memo does not implement the tables.
