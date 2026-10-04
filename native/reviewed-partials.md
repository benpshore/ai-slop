The Native workflow validates corpus coverage and known bounded outcomes. A
reviewed Partial remains Partial; passing this validation does not establish
complete text, correct reference fields, or production quality.

`reviewed-partials.json` records the existing six bounded outcomes from run
37112052229 / artifact 11270691126: five lopdf vector-region coalescing cases and
one PDFium superscript candidate-window cutoff. The artifact digest, exact
public input pins, host, split, dependency/configuration/truth/metric pins,
page counts, document warnings, resource-warning pages and full ordered page
diagnostic hashes are retained. No wildcard reason or page is allowed.

The validator checks every expected ID exactly once, requires retained dumps,
and cross-checks page, warning and reference counts and one-to-one extracted
match targets. Repeated source truth keys retain their exact multiplicity.
Missing truth, zero extracted references, or zero matches cannot become a
success merely because the backend reported Complete. These checks are minimum
evaluation integrity requirements, not a substitute for quality acceptance.

For the six reviewed inputs, exact page counts and reference baselines continue
to apply if the status improves to Complete. Matching fewer references or
producing more unmatched extracted references is rejected; improved matching
is allowed. A genuine Complete result must also remove its cutoff warnings.
The Partial baseline never permits a new failure reason or a changed ordinary
page diagnostic to pass unnoticed.

When an input, dependency/configuration, truth/metric implementation, or retained
diagnostic changes, rerun and review that evidence before updating its explicit
baseline. Do not suppress warnings or relabel the result to obtain a pass. The
baseline pins are deliberately specific to the recorded Linux ARM64 dev run.

All four backend validators run, even after an earlier failure, and the shell
step returns failure if any validator fails. Replaying the saved artifact with
this validator recognizes the six bounded outcomes but still fails PDFium,
docling-text and docling on eight existing zero-extraction/zero-match results.
Only lopdf passes this coverage/integrity check; this is not a quality signoff.
