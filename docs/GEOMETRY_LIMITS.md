# Dense geometry without arbitrary truncation

Vector paths now merge into fixed-point clusters as they are interpreted. The
insertion, union and rescan order matches the previous complete batch algorithm.
A page with thousands of paths in two drawings retains two clusters instead of
collapsing at the 2,001st path. Coordinates, transforms, thin rules and image
placements retain their existing treatment. Backend content policy is 8.

The retained-cluster cap remains 2,000; a separate per-page budget permits at
most 4,000,000 box-pair checks, including containment probes. If either is exhausted, the covering
extent is emitted with a `resource_limit:` warning and partial status. These
are component bounds, not a whole-process memory guarantee.

A paint contained in the last retained cluster cannot touch any other cluster:
the retained clusters already form a fixed point and containment cannot widen
proximity. It therefore keeps the exact remove-last/append result without a
search. Similarly, absorbing an earlier cluster without enlarging it proves
that no restart is needed. Other merges keep their original search and restart
order. Failed containment probes still consume the comparison allowance.

These shortcuts reduce work for repeated detail in existing drawings; they do
not make general clustering linear. With `N` painted boxes and at most `C`
retained clusters, ordinary searches remain `O(N C)` before the explicit cap.
Each successful merge removes one cluster, so there are at most `N` successful
merges across the page; nested rescans do not imply cubic total work.

Superscript search examines the complete indexed baseline window, including
candidates beyond the old 256-entry prefix. It retains the existing 1,000,000
unit page cleanup budget and charges every inspected entry, including furniture.
An unaffordable window produces no candidate, preserves the fragment and reports
the existing cleanup budget warning; it cannot select a misleading prefix winner.
Candidate scoring and line-index tie breaking are unchanged.

Regressions compare incremental clusters with the original batch algorithm,
retain separate drawings across 6,000 paths, cover bridge merges, and exercise
both cluster and comparison limits. Superscript tests compare full-search results
after more than 256 candidates and cover furniture and insufficient page work.
Corpus completeness and accuracy must still be measured independently; these
tests do not establish an accuracy percentage or a platform performance claim.
