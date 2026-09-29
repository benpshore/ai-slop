# Consolidating overlapping pull requests

Use one integration pull request when several Codex pull requests solve the
same problem, touch the same subsystem, or only work when landed together.
This keeps review focused on the final behavior without losing the tests or
authorship from the source changes. It is not a reason to combine unrelated
work.

## Decide whether to consolidate

Create a consolidation PR when at least one of these is true:

- two PRs provide competing implementations of the same behavior;
- later PRs repeatedly rewrite an earlier PR in the same subsystem;
- none of the PRs is independently releasable; or
- reviewing the combined invariant is safer than reviewing intermediate
  states.

Keep PRs separate when they are independently testable and releasable, or
when a small prerequisite can merge first without changing user-visible
behavior. Do not use an integration PR merely to bypass a failing source PR.

## Build the integration branch

1. Fetch the target branch and create a new branch from its current tip.
2. Record the source PR number, head SHA, intent, tests, and author before
   changing code. This is the immutable input list for the consolidation.
3. Compare the PRs by behavior and invariant, not by diff size. Choose one
   implementation for each behavior. Port only the missing pieces from the
   alternatives; do not retain parallel code paths as a compromise.
4. Bring across every relevant regression test first. When two tests express
   the same contract, keep the clearer or stricter test and note which one it
   replaces.
5. Apply the smallest implementation that makes the combined tests pass.
   Separate mechanical refactoring from behavioral changes in the commit
   history when practical.
6. Run the complete repository check suite from `AGENTS.md` on the final tree.
   A source PR's green checks do not validate the combined result.
7. Open the integration PR and mark each source PR as **kept**, **replaced**,
   or **dropped**, with a reason. Close superseded PRs only after the
   integration PR exists and contains their required work.

Prefer `git cherry-pick -n` when a source commit is a useful starting point:
it permits conflicts, duplicate changes, and commit boundaries to be cleaned
up before committing. Preserve authorship with `Co-authored-by` trailers for
material contributions that are rewritten rather than cherry-picked.

## Opening the PR

`gh pr create --fill`, the invocation `AGENTS.md` prescribes, takes the title
and body from the commit message and never loads
`.github/pull_request_template.md`. For a consolidation PR, copy the
template, fill it in, and pass it explicitly:

```sh
cp .github/pull_request_template.md /tmp/pr-body.md   # then edit it
gh pr create --title "<title>" --body-file /tmp/pr-body.md
```

## Required PR inventory

Put this table in the integration PR under **Relationship to other changes**:

| Source PR | Head SHA | Disposition | Behavior/tests retained | Reason |
| --- | --- | --- | --- | --- |
| `#123` | `abcdef0` | kept / replaced / dropped | concise list | concise rationale |

Also state:

- the single implementation that now owns each formerly competing behavior;
- any intentionally deferred work and its issue;
- migrations, generated files, dependency changes, and compatibility risks;
- the exact commands run on the combined branch; and
- which source PRs should be closed as superseded.

## Commit and review shape

A manageable consolidation normally has these commits:

1. combined regression tests and fixtures;
2. the selected implementation plus the necessary pieces from alternatives;
3. removal of superseded paths and documentation updates.

Fold trivial fixups into those commits before review. Avoid one commit per
source PR when that would preserve competing designs, but do not squash away
authorship or make behavioral changes impossible to audit. The final PR must
still satisfy the protected-`main`, release, secret-handling, and tooling rules
in `AGENTS.md`.

## Completion criteria

The consolidation is ready only when:

- each source PR has a recorded disposition;
- one code path owns each behavior;
- all retained behavior has regression coverage;
- the combined branch passes the full check suite;
- the PR description names the superseded PRs and credits contributors; and
- no source PR is closed before its retained work is visible in the
  integration PR.
