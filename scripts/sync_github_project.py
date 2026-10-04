#!/usr/bin/env python3
"""Synchronize the engineering board using gh; Python has no third-party dependencies.

Reads are paginated and finish before item/body updates begin. Mutations require
--apply. The owned status field avoids replacing GitHub's existing Status options
(that operation can invalidate option IDs on unrelated items). Re-running after a
partial API failure is safe: project, field, view, item and body identities are
discovered again. No code is checked out, merged, or force-pushed by this tool.
"""

from __future__ import annotations

import argparse
import getpass
import json
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

TITLE = "PDFTextract Engineering"
FIELD = "Engineering status"
VIEW = "Engineering Kanban"
MARKER = "pdftextract-project-sync"
STATUSES = {
    "Backlog": ("GRAY", "Open work awaiting implementation."),
    "In progress": ("BLUE", "Implementation or validation is in progress."),
    "In review": ("PURPLE", "Ready for review; not yet complete or merged."),
    "Blocked": ("RED", "An explicit blocker or failing PR check needs attention."),
    "Done": ("GREEN", "Merged pull request or issue explicitly closed as completed."),
    "Cancelled": ("ORANGE", "Closed unmerged or closed as not planned."),
}
PAGE = "pageInfo { hasNextPage endCursor }"
PROJECT = "id number title url closed readme"
COMMON = "id number title url body state repository { nameWithOwner }"
CONTENT = (
    "__typename "
    "... on Issue { " + COMMON + " stateReason } "
    "... on PullRequest { " + COMMON + " isDraft merged "
    "commits(last: 1) { nodes { commit { statusCheckRollup { state } } } } }"
)


class SyncError(Exception):
    """An actionable error, without credentials or a misleading success result."""


class GitHub:
    def __init__(self):
        self.executable = shutil.which("gh")
        if not self.executable:
            raise SyncError("Install GitHub CLI first: https://cli.github.com/")

    def command(self, arguments, payload=None):
        env = os.environ.copy()
        env.pop("GH_DEBUG", None)
        # Keep secret/variable commands on the same host as the explicit API
        # calls, even when the caller normally uses a GitHub Enterprise host.
        env["GH_HOST"] = "github.com"
        # gh receives structured JSON and secrets through stdin/environment, never
        # a shell or command-line token. Suppress interactive credential prompts.
        env["GH_PROMPT_DISABLED"] = "1"
        result = subprocess.run(  # noqa: S603 -- fixed gh executable, argv, no shell
            [self.executable, *arguments],
            input=payload,
            text=True,
            capture_output=True,
            env=env,
            check=False,
        )
        if result.returncode:
            detail = result.stderr.strip()
            for key in ("GH_TOKEN", "GITHUB_TOKEN"):
                if env.get(key):
                    detail = detail.replace(env[key], "[redacted]")
            raise SyncError(f"GitHub command failed: {detail}")
        return result.stdout

    def rest(self, path, method="GET", body=None):
        args = [
            "api",
            "--hostname",
            "github.com",
            path,
            "--method",
            method,
            "-H",
            "Accept: application/vnd.github+json",
            "-H",
            "X-GitHub-Api-Version: 2026-03-10",
        ]
        if body is not None:
            args += ["--input", "-"]
        raw = self.command(args, json.dumps(body) if body is not None else None)
        return json.loads(raw) if raw.strip() else None

    def graphql(self, query, **variables):
        raw = self.command(
            ["api", "--hostname", "github.com", "graphql", "--input", "-"],
            json.dumps({"query": query, "variables": variables}),
        )
        response = json.loads(raw)
        if response.get("errors"):
            raise SyncError("GraphQL failed: " + json.dumps(response["errors"]))
        return response["data"]

    def mutation(self, name, input_type, value, selection):
        query = f"mutation($input: {input_type}!) {{ {name}(input: $input) {{ {selection} }} }}"
        return self.graphql(query, input=value)[name]


def pages(gh, query, path, **variables):
    """Never silently truncate a project/repository to its first 100 entries."""
    cursor = None
    seen = set()
    result = []
    while True:
        node = gh.graphql(query, cursor=cursor, **variables)
        for key in path:
            node = node[key]
        result.extend(value for value in node["nodes"] if value is not None)
        if not node["pageInfo"]["hasNextPage"]:
            return result
        cursor = node["pageInfo"]["endCursor"]
        if not cursor or cursor in seen:
            raise SyncError("Invalid pagination cursor; refusing an incomplete synchronization.")
        seen.add(cursor)


def select_project(owned, linked, number=None):
    if number is not None:
        candidates = [p for p in owned if p["number"] == number]
    else:
        candidates = [p for p in owned if not p["closed"] and p["title"] == TITLE]
        if not candidates:
            owned_ids = {p["id"] for p in owned}
            candidates = [p for p in linked if not p["closed"] and p["id"] in owned_ids]
    if len(candidates) > 1:
        raise SyncError("Multiple matching projects; choose one with --project-number NUMBER.")
    if number is not None and (not candidates or candidates[0]["closed"]):
        raise SyncError("The requested project is absent, inaccessible, or closed.")
    return candidates[0] if candidates else None


def managed_body(body, text):
    """Replace only our delimited block; retain the author's text verbatim."""
    start, end = f"<!-- {MARKER}:start -->", f"<!-- {MARKER}:end -->"
    block = f"{start}\n{text}\n{end}"
    if start not in body and end not in body:
        return body + ("\n\n" if body else "") + block
    if body.count(start) != 1 or body.count(end) != 1 or body.index(end) < body.index(start):
        raise SyncError("Malformed managed body markers; refusing to overwrite authored text.")
    return body[: body.index(start)] + block + body[body.index(end) + len(end) :]


def desired_status(content, current=None, declared=None):
    """CI failure is not completion; an unmerged closed PR is never Done."""
    is_pr = content["__typename"] == "PullRequest"
    if is_pr and content["merged"]:
        return "Done"
    if content["state"] == "CLOSED":
        if not is_pr and content.get("stateReason") == "COMPLETED":
            return "Done"
        return "Cancelled"
    if is_pr:
        commits = content.get("commits", {}).get("nodes", [])
        rollup = commits[-1]["commit"].get("statusCheckRollup") if commits else None
        if rollup and rollup["state"] in {"ERROR", "FAILURE"}:
            return "Blocked"
        return "In progress" if content["isDraft"] else "In review"
    # Issue owners can steer status with a simple first-line declaration. An
    # existing board choice wins over the initial inventory seed thereafter.
    match = re.match(r"Status:\s*([^\n.]+)\.?\s*(?:\n|$)", content["body"], re.I)
    status = match.group(1).strip() if match else current or declared or "Backlog"
    normalized = {name.casefold(): name for name in STATUSES}
    selected = normalized.get(status.casefold(), "Backlog")
    return "In review" if selected in {"Done", "Cancelled"} else selected


def project_nodes(gh, project_id, connection, selection):
    return pages(
        gh,
        "query($id: ID!, $cursor: String) { node(id: $id) { ... on ProjectV2 { "
        f"{connection}(first: 100, after: $cursor) {{ nodes {{ {selection} }} {PAGE} }}"
        " } } }",
        ("node", connection),
        id=project_id,
    )


def ensure_field(gh, project_id):
    fields = project_nodes(
        gh,
        project_id,
        "fields",
        "... on ProjectV2FieldCommon { id name databaseId } "
        "... on ProjectV2SingleSelectField { options { id name } }",
    )
    matches = [f for f in fields if f["name"] == FIELD]
    if len(matches) > 1:
        raise SyncError(f"Multiple {FIELD!r} fields; refusing an ambiguous update.")
    if matches:
        field = matches[0]
        if not set(STATUSES) <= {o["name"] for o in field.get("options", [])}:
            raise SyncError(f"Existing {FIELD!r} lacks required options; no options were replaced.")
    else:
        field = gh.mutation(
            "createProjectV2Field",
            "CreateProjectV2FieldInput",
            {
                "projectId": project_id,
                "name": FIELD,
                "dataType": "SINGLE_SELECT",
                "singleSelectOptions": [
                    {"name": n, "color": c, "description": d} for n, (c, d) in STATUSES.items()
                ],
            },
            "projectV2Field { ... on ProjectV2SingleSelectField "
            "{ id name databaseId options { id name } } }",
        )["projectV2Field"]
    return field, fields


def ensure_board(gh, owner, project, field, fields, repo):
    views = project_nodes(
        gh,
        project["id"],
        "views",
        "name number layout filter verticalGroupByFields(first: 10) "
        "{ nodes { ... on ProjectV2FieldCommon { id } } }",
    )
    for view in views:
        if view["name"] != VIEW:
            continue
        groups = [f["id"] for f in view["verticalGroupByFields"]["nodes"]]
        if (
            view["layout"] != "BOARD_LAYOUT"
            or groups != [field["id"]]
            or view["filter"] != f"repo:{repo}"
        ):
            raise SyncError(f"Existing {VIEW!r} has different settings; it was preserved.")
        return project["url"] + f"/views/{view['number']}"
    prefix = f"orgs/{owner['login']}" if owner["type"] == "Organization" else f"users/{owner['id']}"
    visible = [
        f["databaseId"]
        for f in fields
        if f["name"] in {"Title", "Assignees", "Labels"} and f["databaseId"]
    ]
    view = gh.rest(
        f"{prefix}/projectsV2/{project['number']}/views",
        "POST",
        {
            "name": VIEW,
            "layout": "board",
            "filter": f"repo:{repo}",
            "visible_fields": [*visible, field["databaseId"]],
            "vertical_group_by": [field["databaseId"]],
        },
    )
    return view.get("html_url") or project["url"]


def repo_contents(gh, owner, name, manifest, existing):
    contents = {}
    for connection, kind in (("issues", "Issue"), ("pullRequests", "PullRequest")):
        selection = COMMON + (
            " stateReason"
            if kind == "Issue"
            else " isDraft merged commits(last: 1) { nodes { commit "
            "{ statusCheckRollup { state } } } }"
        )
        found = pages(
            gh,
            "query($owner: String!, $name: String!, $cursor: String) { "
            "repository(owner: $owner, name: $name) { "
            f"{connection}(first: 100, after: $cursor, states: OPEN) {{ "
            f"nodes {{ __typename {selection} }} {PAGE} }} }} }}",
            ("repository", connection),
            owner=owner,
            name=name,
        )
        contents.update({c["number"]: c for c in found})
    numbers = {manifest["tracking_issue"]}
    for work in manifest["workstreams"]:
        numbers.add(work["issue"])
        numbers.update(work["prs"])
    numbers.update(
        item["content"]["number"]
        for item in existing
        if (item.get("content") or {}).get("repository", {}).get("nameWithOwner")
        == f"{owner}/{name}"
    )
    # Fetch closed tracked items too. Open-only scans cannot notice a merge or
    # issue closure and would leave cards permanently In progress.
    missing = sorted(numbers - contents.keys())
    for offset in range(0, len(missing), 30):
        batch = missing[offset : offset + 30]
        query = "query($owner: String!, $name: String!) { repository(owner: $owner, name: $name) { "
        query += " ".join(f"n{n}: issueOrPullRequest(number: {n}) {{ {CONTENT} }}" for n in batch)
        response = gh.graphql(query + " } }", owner=owner, name=name)["repository"]
        for n in batch:
            if response[f"n{n}"] is None:
                raise SyncError(f"Tracked issue/PR #{n} is inaccessible; no item updates applied.")
            contents[n] = response[f"n{n}"]
    return contents


def link_bodies(contents, manifest, board_url):
    """References deliberately do not close workstream issues upon one PR merge."""
    master = manifest["tracking_issue"]
    issue_for_pr = {pr: w["issue"] for w in manifest["workstreams"] for pr in w["prs"]}
    relations = {master: set()}
    for work in manifest["workstreams"]:
        relations.setdefault(work["issue"], set()).update(work["prs"])
        if work["issue"] != master:
            relations[master].add(work["issue"])
    for n, content in contents.items():
        if content["__typename"] == "PullRequest":
            target = issue_for_pr.get(n, master)
            relations.setdefault(n, set()).add(target)
            relations.setdefault(target, set()).add(n)
    updates = {}
    for n, references in relations.items():
        if n not in contents:
            continue
        text = f"Project: [{TITLE}]({board_url}).\n\n"
        text += "Related work: " + ", ".join(f"#{r}" for r in sorted(references)) + "."
        if n == master:
            text += "\n\nUnclassified PRs are tracked here until assigned a focused workstream."
        updates[n] = managed_body(contents[n]["body"], text)
    return updates


def refresh_managed_body(gh, node_id, property_name, planned):
    """Rebase our block on the latest body, not a stale paginated snapshot.

    GitHub has no GraphQL compare-and-swap body update. This narrows that race;
    workflow concurrency serializes our writers. A human edit in the final
    read/write interval remains possible and is documented in PROJECT_SETUP.md.
    """
    selection = (
        "... on ProjectV2 { readme }"
        if property_name == "readme"
        else "... on Issue { body } ... on PullRequest { body }"
    )
    node = gh.graphql(f"query($id: ID!) {{ node(id: $id) {{ {selection} }} }}", id=node_id)["node"]
    if node is None:
        raise SyncError("Content disappeared before the relationship update; rerun discovery.")
    current = node[property_name] or ""
    start, end = f"<!-- {MARKER}:start -->", f"<!-- {MARKER}:end -->"
    text = planned.split(start, 1)[1].split(end, 1)[0].strip("\n")
    updated = managed_body(current, text)
    return updated if updated != current else None


def sync(gh, args, manifest):
    owner_name, repo_name = args.repo.split("/")
    owner = gh.rest(f"users/{owner_name}")
    repository = gh.rest(f"repos/{args.repo}")
    owner_kind = "Organization" if owner["type"] == "Organization" else "User"
    owned = pages(
        gh,
        "query($id: ID!, $cursor: String) { node(id: $id) { ... on "
        + owner_kind
        + f" {{ projectsV2(first: 100, after: $cursor) {{ nodes {{ {PROJECT} }} {PAGE} }} }} }} }}",
        ("node", "projectsV2"),
        id=owner["node_id"],
    )
    linked = pages(
        gh,
        "query($owner: String!, $name: String!, $cursor: String) { "
        "repository(owner: $owner, name: $name) { "
        f"projectsV2(first: 100, after: $cursor) {{ nodes {{ {PROJECT} }} {PAGE} }} }} }}",
        ("repository", "projectsV2"),
        owner=owner_name,
        name=repo_name,
    )
    project = select_project(owned, linked, args.project_number)
    existing = (
        project_nodes(
            gh,
            project["id"],
            "items",
            f'id isArchived content {{ {CONTENT} }} fieldValueByName(name: "{FIELD}") '
            "{ ... on ProjectV2ItemFieldSingleSelectValue { name } }",
        )
        if project
        else []
    )
    contents = repo_contents(gh, owner_name, repo_name, manifest, existing)
    print(
        f"{'Reuse ' + project['url'] if project else 'Create ' + TITLE}; "
        f"synchronize {len(contents)} issues/PRs."
    )
    if not args.apply:
        print("Read-only preview. Use --apply to create/update the board and relationship blocks.")
        return
    if project is None:
        project = gh.mutation(
            "createProjectV2",
            "CreateProjectV2Input",
            {"ownerId": owner["node_id"], "title": TITLE},
            f"projectV2 {{ {PROJECT} }}",
        )["projectV2"]
    if project["id"] not in {p["id"] for p in linked}:
        gh.mutation(
            "linkProjectV2ToRepository",
            "LinkProjectV2ToRepositoryInput",
            {"projectId": project["id"], "repositoryId": repository["node_id"]},
            "clientMutationId",
        )
    field, fields = ensure_field(gh, project["id"])
    board_url = ensure_board(gh, owner, project, field, fields, args.repo)
    options = {o["name"]: o["id"] for o in field["options"]}
    by_content = {
        item["content"]["id"]: item for item in existing if (item.get("content") or {}).get("id")
    }
    declared = {w["issue"]: w["status"] for w in manifest["workstreams"]}
    bodies = link_bodies(contents, manifest, board_url)
    report = {
        "repository": args.repo,
        "project_url": project["url"],
        "board_url": board_url,
        "project_number": project["number"],
        "items": [],
    }
    for number, content in sorted(contents.items()):
        item = by_content.get(content["id"])
        if item and item["isArchived"]:
            continue  # Keep the user's archived items archived and unmodified.
        current = ((item or {}).get("fieldValueByName") or {}).get("name")
        status = desired_status(content, current, declared.get(number))
        if item is None:
            item = gh.mutation(
                "addProjectV2ItemById",
                "AddProjectV2ItemByIdInput",
                {"projectId": project["id"], "contentId": content["id"]},
                "item { id }",
            )["item"]
        if current != status:
            gh.mutation(
                "updateProjectV2ItemFieldValue",
                "UpdateProjectV2ItemFieldValueInput",
                {
                    "projectId": project["id"],
                    "itemId": item["id"],
                    "fieldId": field["id"],
                    "value": {"singleSelectOptionId": options[status]},
                },
                "clientMutationId",
            )
        if number in bodies and bodies[number] != content["body"]:
            kind = content["__typename"]
            updated = refresh_managed_body(gh, content["id"], "body", bodies[number])
            if updated is not None:
                gh.mutation(
                    f"update{kind}",
                    f"Update{kind}Input",
                    {
                        "id" if kind == "Issue" else "pullRequestId": content["id"],
                        "body": updated,
                    },
                    "clientMutationId",
                )
        report["items"].append({"number": number, "url": content["url"], "status": status})
    readme = managed_body(
        project.get("readme") or "",
        (
            f"[{VIEW}]({board_url}) tracks implementation and review using `{FIELD}`.\n\n"
            "Open drafts are In progress; ready PRs are In review; failing checks are Blocked. "
            "Merged PRs and explicitly completed issues are Done. "
            "Closed unmerged work is Cancelled. "
            "A first-line `Status: In progress.` (or another column) steers an issue. "
            "No issue is closed just because one related PR merges.\n\n"
            f"[Work inventory](https://github.com/{args.repo}/blob/{repository['default_branch']}"
            f"/docs/WORK_INVENTORY.md); tracking issue #{manifest['tracking_issue']}."
        ),
    )
    if readme != project.get("readme"):
        updated = refresh_managed_body(gh, project["id"], "readme", readme)
        if updated is not None:
            gh.mutation(
                "updateProjectV2",
                "UpdateProjectV2Input",
                {"projectId": project["id"], "readme": updated},
                "clientMutationId",
            )
    if args.install_automation:
        token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")
        if not token:
            raise SyncError("Automation needs --ask-token or GH_TOKEN; board setup succeeded.")
        gh.command(["secret", "set", "PROJECTS_TOKEN", "--repo", args.repo], token)
        gh.command(
            [
                "variable",
                "set",
                "PROJECT_NUMBER",
                "--repo",
                args.repo,
                "--body",
                str(project["number"]),
            ]
        )
        print("Saved encrypted PROJECTS_TOKEN and PROJECT_NUMBER for the project-sync workflow.")
        print("Continuous updates start after that workflow is merged into the default branch.")
    if args.report:
        args.report.write_text(json.dumps(report, indent=2) + "\n")
    print(f"Synchronized {len(report['items'])} cards: {board_url}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", default="benpshore/pdftextract")
    parser.add_argument("--project-number", type=int)
    parser.add_argument(
        "--manifest",
        type=Path,
        default=Path(__file__).resolve().parents[1] / ".github/project-tracking.json",
    )
    parser.add_argument("--apply", action="store_true")
    parser.add_argument(
        "--ask-token",
        action="store_true",
        help="Prompt locally without echo; never writes the PAT to disk.",
    )
    parser.add_argument(
        "--install-automation",
        action="store_true",
        help="Store PAT as encrypted PROJECTS_TOKEN and set PROJECT_NUMBER.",
    )
    parser.add_argument("--report", type=Path)
    args = parser.parse_args()
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", args.repo):
        parser.error("--repo must be OWNER/REPOSITORY")
    if args.install_automation and not args.apply:
        parser.error("--install-automation requires --apply")
    try:
        if args.ask_token:
            os.environ["GH_TOKEN"] = getpass.getpass("GitHub PAT (hidden; repo + project scopes): ")
        manifest = json.loads(args.manifest.read_text())
        if manifest["repository"] != args.repo:
            raise SyncError("Manifest repository differs from --repo.")
        sync(GitHub(), args, manifest)
    except (SyncError, OSError, ValueError) as exc:
        print(f"Project synchronization failed: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
