# Engineering project setup

The repeatable setup creates **PDFTextract Engineering**, or reuses the exact
matching project or a uniquely repository-linked project owned by `benpshore`.
It creates **Engineering Kanban** with Backlog, In progress, In review, Blocked,
Done, and Cancelled columns. Existing project names and authored content remain
intact. If several projects qualify, it reports the ambiguity; supply
`--project-number NUMBER` to select one instead of creating a duplicate.

Install [GitHub CLI](https://cli.github.com/) and Python 3.9 or later. The script
uses only the standard library; it does not install packages or run Cargo.
From a checkout containing this change:

```sh
python3 scripts/sync_github_project.py --ask-token --apply --install-automation
```

Paste a classic GitHub PAT with `repo` and `project` scopes at the hidden local
prompt. The account needs project write access and repository administration
access to configure the secret/variable. The token is never printed, committed,
or passed as a command-line argument. `--install-automation` deliberately stores
it in GitHub's encrypted repository secret **PROJECTS_TOKEN** and sets
**PROJECT_NUMBER**. Omit that flag for a one-time board setup without storing a
secret. An existing `GH_TOKEN` can be used instead of `--ask-token`.

Without `--apply`, discovery is read-only. A failed discovery aborts; it is not
interpreted as evidence that the project does not exist. Rerun after a partial
failure: existing projects, fields, views, cards, and managed relationship blocks
are reused. The script does not check out code, merge PRs, close issues, remove
cards, change project visibility, replace field options, or modify archived cards.

## Ongoing updates

Merge the focused setup PR normally after its checks and review. The workflow
must exist on the default branch before its event/scheduled automation activates.
Saving the PAT and project number alone does not install that workflow. Thereafter
issue/PR events, completed CI runs, and a half-hour schedule refresh the board.
The workflow always checks out trusted default-branch code; it never executes PR
code with the PAT. Reports include the actual board URL and per-card statuses.

The board uses its own **Engineering status** field so setup cannot invalidate
existing Status option IDs or disturb unrelated items. Closed unmerged PRs are
Cancelled, merged PRs are Done, failing checks are Blocked, open drafts are
In progress, and open ready PRs are In review. Issues are Done only when explicitly
closed as completed; closing as not planned is Cancelled. For an open issue, a
first line such as `Status: In review.` takes precedence, followed by an existing
board choice and then the inventory's initial status. An open issue is never
automatically marked Done when one of its PRs merges.

All open repository issues and PRs are imported. Closed inventory items and
already-tracked cards remain visible and refresh to their actual states. Unknown
PRs link to master issue [#144](https://github.com/benpshore/pdftextract/issues/144)
until assigned to a focused workstream. Update
[project-tracking.json](../.github/project-tracking.json) and
[WORK_INVENTORY.md](WORK_INVENTORY.md) together for each major change. Relationship
blocks are reciprocal references, not automatic closing directives. Existing
authored body text is preserved.

Body updates re-read the latest description immediately before adding the managed
block, and the workflow serializes its own runs. GitHub's GraphQL body mutation
has no compare-and-swap precondition: a human edit during that final read/write
interval can still race. Run initial setup once at a time; do not start concurrent
local setup processes. Existing read/write permissions remain GitHub's authority.

## Verification scope

Tests use fake API responses to exercise pagination, existing/draft/deleted cards,
status transitions, body preservation, creation payloads and repeated execution.
The view payload follows GitHub's current REST `POST /users/{user_id}/projectsV2/
{project_number}/views` contract (`layout: board`, `vertical_group_by` field IDs).
GraphQL manages project discovery, fields, cards and values.

This workspace's connector does not expose Projects and its CLI credential is
denied GraphQL access. Consequently real Project creation cannot be claimed here;
the local PAT run is the live integration check. API errors stop with a nonzero
exit, rather than reporting an unverified board as configured.
