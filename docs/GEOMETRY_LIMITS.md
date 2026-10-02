# Dense geometry without arbitrary truncation

Vector paths now merge into fixed-point clusters as they are interpreted. The
insertion, union and rescan order matches the previous complete batch algorithm.
A page with thousands of paths in two drawings retains two clusters instead of
collapsing at the 2,001st path. Coordinates, transforms, thin rules and image
placements retain their existing treatment. Backend content policy is 7.

The retained-cluster cap remains 2,000; a separate per-page budget permits at
most 4,000,000 proximity comparisons. If either is exhausted, the covering
extent is emitted with a `resource_limit:` warning and partial status. These
are component bounds, not a whole-process memory guarantee.

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
