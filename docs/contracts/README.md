# Agent contracts

Each batch of implementation agents codes against a written contract: exact module
paths, public signatures, behaviour rules, tests to write, and the offline crate
sources they may read. CI is the only compiler, so the contract is the shared spec.

| Batch | Scope | Landed as |
| --- | --- | --- |
| [batch1](batch1.md) | schema, lopdf backend, reading order, metadata, citations, ledger, CLI | #2 |
| [batch2](batch2.md) | corpus manifest, LaTeX ground truth, evaluation, `tpe eval` | #4 |
| [batch3](batch3.md) | docling.rs and PDFium backends, figures, OCR fixture, native CI | #16 |
| [batch4](batch4.md) | workbench tracks: credentials, biblio, Zotero, search, speech, app, browser | #7, #8, #9, #10, #11, #12, #14 |

`<scratchpad>` refers to the session scratch directory that held offline crate sources;
`<repo>` and `<worktree>` to the checkout and per-track worktrees.
