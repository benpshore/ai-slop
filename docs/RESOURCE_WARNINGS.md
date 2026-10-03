# Resource-limit diagnostics

Existing extraction budgets remain in force. This change adds page warnings
with a stable `resource_limit:` prefix when:

- lopdf discards raster placements beyond 2,000 on a page;
- lopdf coalesces excess vector regions into a covering box;
- vertical grouping runs out of comparisons and leaves spans separate;
- superscript search truncates its 256-entry baseline window;
- script cleanup runs out of its per-page work budget.
- Form nesting reaches the configured depth limit;
- accent composition or line ordering reaches its work limit;
- caption candidates exceed the page limit.

Warnings use the serialized `PageText.warnings` array and survive ledger
storage and evaluation dumps. A `resource_limit:` warning makes the document
and its affected chunk `partial`, including limits reached after decoding in
ordering, cleanup, or region tagging. The document warnings also include the
page number and cutoff, so JSON consumers and ledger run records see it.
Retained text is still published. Ordinary diagnostic warnings do not change
completeness. Headless job summaries carry the partial status and warnings.

Bibliography JSON keeps `status: found|not_found|failed` for list detection
and adds `extraction_status: complete|partial|failed` for the inspected pages.
A found boundary is not an accuracy guarantee. A scan without a list preserves
cutoff warnings instead of discarding them. No GUI layout changes are included.

A warning is emitted once per affected page, not for every discarded item.
Window warnings mean a search was truncated, not proof that a particular
attachment was missed. Budget and selection rules are unchanged, including
furniture counting toward the superscript window. Caption, accent, line and
depth limits now use the same stable prefix.
This adds diagnostics, not a claim that every resource cap in the repository
is now instrumented or that the cutoffs are optimal for every document class.

Tests exercise actual raster overflow, vertical exhaustion propagated to the
page without duplicate warnings, script exhaustion, a crowded superscript
window and preservation of resource warnings in a successful job outcome.
Integration tests cover decoder and postprocessing cutoffs through document
JSON, chunk status, SQLite and the not-found bibliography path.
