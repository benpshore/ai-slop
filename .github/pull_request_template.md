## What

<!-- One or two sentences: what does this change and why? -->

Closes #

## Relationship to other changes

<!--
Choose one and delete the others. This keeps overlapping Codex PRs from
quietly becoming competing implementations.

- Standalone; no known overlap.
- Consolidates/supersedes #... (the superseded PRs should be closed only
  after this PR contains their required tests and fixes).
- Depends on #...; review and merge that PR first.

For a consolidation PR, list each source PR and the disposition of every
user-visible change (kept, replaced, or intentionally dropped).

`gh pr create --fill` takes the body from the commit message, not from this
template. To use it, fill in a copy and pass `--body-file <copy>` (see
docs/PR_CONSOLIDATION.md).
-->

- Standalone; no known overlap.

## Scope

<!-- Keep this list concrete enough to compare with related PRs. -->

- [ ] The implementation and regression tests are in this PR.
- [ ] Unrelated follow-up work is linked rather than bundled here.
- [ ] Generated files, dependency changes, and documentation changes are identified below.

## How to verify

- [ ] `uv run ruff format --check`
- [ ] `uv run ruff check`
- [ ] `uv run pytest`
- [ ] `uv audit --preview-features audit-command`
- [ ] `cargo fmt --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test --workspace`
- [ ] `swift build && swift test`
- [ ] `cmake -S . -B build && cmake --build build && ctest --test-dir build`

<!-- Mark an irrelevant check N/A and explain why; do not silently omit it. -->

## Consolidation checklist

<!-- Delete this section for a standalone PR. -->

- [ ] The branch starts from the current target branch, not from another open PR's stale base.
- [ ] Source PR commits were reviewed individually; broad cherry-picks were avoided or explained.
- [ ] Competing implementations were reduced to one code path.
- [ ] Regression tests from every source PR were retained or replaced with equivalent coverage.
- [ ] The full required check suite passes on the combined result.
- [ ] Source PR authors receive credit in commit trailers or the notes below.
- [ ] Superseded PRs are listed and can be closed after this PR is opened.

## Notes

<!-- Anything reviewers (human or AI) should know: risks, follow-ups, screenshots. -->
