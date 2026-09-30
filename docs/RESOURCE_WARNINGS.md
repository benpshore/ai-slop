# Resource-limit diagnostics

Existing extraction budgets remain in force. This change adds page warnings
with a stable `resource_limit:` prefix when:

- lopdf discards raster placements beyond 2,000 on a page;
- lopdf coalesces excess vector regions into a covering box;
- vertical grouping runs out of comparisons and leaves spans separate;
- superscript search truncates its 256-entry baseline window;
- script cleanup runs out of its per-page work budget.

Warnings use the existing serialized `PageText.warnings` array and therefore
survive ledger storage and evaluation dumps. The job model also forwards
resource warnings from complete pages, instead of hiding them whenever the
overall extraction status is Complete. No GUI behavior or layout is changed.

A warning is emitted once per affected page, not for every discarded item.
Window warnings mean a search was truncated, not proof that a particular
attachment was missed. Budget and selection rules are unchanged, including
furniture counting toward the superscript window. Caption-candidate and accent
composition limits already have explicit page warnings; those remain intact.
This adds diagnostics, not a claim that every resource cap in the repository
is now instrumented or that the cutoffs are optimal for every document class.

Tests exercise actual raster overflow, vertical exhaustion propagated to the
page without duplicate warnings, script exhaustion, a crowded superscript
window and preservation of resource warnings in a successful job outcome.
