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
step returns failure if any validator fails. Replaying the original saved artifact with
this validator recognizes the six bounded outcomes but still fails PDFium,
docling-text and docling on eight existing zero-extraction/zero-match results.
Only lopdf passes this coverage/integrity check; this is not a quality signoff.

## Current build identity and historical eligibility

The CLI verifies the artifact's recorded source commit and all five input hashes
against a clean source checkout before considering historical exceptions. The
default is the checkout containing the validator. For archived evidence, pass
`--source-root` pointing to the exact checkout that produced it, and use that
checkout's corpus manifest. A wrong commit or file hash remains a hard error.
This verifies recorded source/input provenance; it does not attest the executable
binary or its compiled feature graph. Build logs and executable evidence remain
separate requirements.

A verified current Cargo.lock can differ from the historical policy. In that
case **zero historical Partial exceptions apply**. The policy JSON, original
dependency hash, diagnostics and reference baselines remain unchanged. The six
same-input page counts and minimum reference matches/maximum spurious-reference
counts still apply, including to genuinely Complete results. Every unreviewed
Partial still fails, and all statuses are checked for invalid reference counts
and zero-match outcomes. Other historical input/scorer pin changes, malformed
policies and unverified current locks remain hard errors.

This separation repairs the PR175/#174 integration mismatch without declaring
different dependency graphs equivalent. The exact failing combined-tree runs
were [#191 run37169841972](https://github.com/benpshore/pdftextract/actions/runs/37169841972)
and [#194 run37170137003](https://github.com/benpshore/pdftextract/actions/runs/37170137003).
Their artifacts correctly record intended Cargo.lock `00ff969d6bc010d669a77e8c2341a81219e7d30dadbfee892ef5825a2aadd57c`;
the historical policy correctly records `ae0367f2f9391ec98fd0a125bc516e0179419b311e4f1c7693ad3052b3b7f8c0`.
The corpus/native manifests and metric/truth hashes match. PR175 added 73 package
versions; PDFium-render's active libloading edge changed from 0.8.9 to 0.9.0.
None of the six historical diagnostic signatures matches the combined current
run: their resource cutoffs disappeared, while Unicode/CFF limitations keep
them Partial. Reference matching did not regress. This is not a reason to repin
the historical outcomes or call the corpus green. Tracking: #190, #181 and #153.
