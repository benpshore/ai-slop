"""Project automation must preserve existing work and fail visibly on API errors."""

import copy
import importlib.util
from pathlib import Path
from types import SimpleNamespace

import pytest

SPEC = importlib.util.spec_from_file_location(
    "project_sync", Path(__file__).resolve().parents[1] / "scripts/sync_github_project.py"
)
sync = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(sync)


def content(number, kind="Issue", **overrides):
    result = {
        "__typename": kind,
        "id": f"C{number}",
        "number": number,
        "title": f"Work {number}",
        "url": f"https://github.com/benpshore/pdftextract/issues/{number}",
        "repository": {"nameWithOwner": "benpshore/pdftextract"},
        "body": "Authored description.\n",
        "state": "OPEN",
        "stateReason": None,
        "merged": False,
        "isDraft": True,
        "commits": {"nodes": []},
    }
    result.update(overrides)
    return result


def connection(nodes):
    return {"nodes": copy.deepcopy(nodes), "pageInfo": {"hasNextPage": False, "endCursor": None}}


class FakeGitHub:
    """Stateful API, including normal deleted/draft/archived project card cases."""

    def __init__(self):
        self.projects = []
        self.linked = []
        self.fields = [{"id": "TITLE", "name": "Title", "databaseId": 1}]
        self.views = []
        self.items = [
            {"id": "DELETED", "content": None, "isArchived": False},
            {"id": "DRAFT", "content": {"__typename": "DraftIssue"}, "isArchived": False},
            {"id": "ARCHIVED", "content": content(90), "isArchived": True},
            {
                "id": "UNRELATED",
                "isArchived": False,
                "content": content(91, repository={"nameWithOwner": "elsewhere/unrelated"}),
            },
        ]
        self.contents = {
            144: content(144),
            145: content(145),
            140: content(140, "PullRequest"),
            90: content(90),
        }
        self.mutations = []
        self.commands = []
        self.concurrent_body = None

    def rest(self, path, method="GET", body=None):
        if path == "users/benpshore":
            return {"login": "benpshore", "id": 42, "node_id": "OWNER", "type": "User"}
        if path == "repos/benpshore/pdftextract":
            return {"node_id": "REPO", "default_branch": "main"}
        assert path == "users/42/projectsV2/7/views"
        assert method == "POST"
        assert body["layout"] == "board"
        assert body["vertical_group_by"] == [99]
        assert body["filter"] == "repo:benpshore/pdftextract"
        self.mutations.append(("create-view", copy.deepcopy(body)))
        self.views.append(
            {
                "name": sync.VIEW,
                "number": 2,
                "layout": "BOARD_LAYOUT",
                "filter": body["filter"],
                "verticalGroupByFields": {"nodes": [{"id": "STATUS"}]},
            }
        )
        return {"html_url": "https://github.com/users/benpshore/projects/7/views/2"}

    def graphql(self, query, **variables):
        if "projectsV2(first:" in query:
            key = "node" if variables.get("id") else "repository"
            return {
                key: {"projectsV2": connection(self.projects if key == "node" else self.linked)}
            }
        for name, values in (("fields", self.fields), ("views", self.views), ("items", self.items)):
            if f"{name}(first:" in query:
                return {"node": {name: connection(values)}}
        for name, kind in (("issues", "Issue"), ("pullRequests", "PullRequest")):
            if f"{name}(first:" in query:
                values = [
                    c
                    for c in self.contents.values()
                    if c["__typename"] == kind and c["state"] == "OPEN"
                ]
                return {"repository": {name: connection(values)}}
        if "issueOrPullRequest" in query:
            import re

            numbers = re.findall(r"n(\d+): issueOrPullRequest", query)
            return {
                "repository": {f"n{n}": copy.deepcopy(self.contents.get(int(n))) for n in numbers}
            }
        if "... on ProjectV2 { readme }" in query:
            return {"node": copy.deepcopy(self.projects[0])}
        if "... on Issue { body }" in query:
            obj = next(c for c in self.contents.values() if c["id"] == variables["id"])
            if self.concurrent_body and obj["number"] == 145:
                obj["body"] = self.concurrent_body
            return {"node": copy.deepcopy(obj)}
        raise AssertionError(query)

    def mutation(self, name, input_type, value, selection):
        self.mutations.append((name, copy.deepcopy(value)))
        if name == "createProjectV2":
            project = {
                "id": "PROJECT",
                "number": 7,
                "title": sync.TITLE,
                "closed": False,
                "url": "https://github.com/users/benpshore/projects/7",
                "readme": "",
            }
            self.projects.append(project)
            return {"projectV2": copy.deepcopy(project)}
        if name == "linkProjectV2ToRepository":
            self.linked = self.projects[:]
        elif name == "createProjectV2Field":
            field = {
                "id": "STATUS",
                "name": sync.FIELD,
                "databaseId": 99,
                "options": [{"id": n, "name": n} for n in sync.STATUSES],
            }
            self.fields.append(field)
            return {"projectV2Field": copy.deepcopy(field)}
        elif name == "addProjectV2ItemById":
            obj = next(c for c in self.contents.values() if c["id"] == value["contentId"])
            item = {
                "id": f"I{obj['number']}",
                "content": copy.deepcopy(obj),
                "isArchived": False,
                "fieldValueByName": None,
            }
            self.items.append(item)
            return {"item": copy.deepcopy(item)}
        elif name == "updateProjectV2ItemFieldValue":
            item = next(i for i in self.items if i["id"] == value["itemId"])
            item["fieldValueByName"] = {"name": value["value"]["singleSelectOptionId"]}
        elif name in {"updateIssue", "updatePullRequest"}:
            node_id = value.get("id") or value["pullRequestId"]
            obj = next(c for c in self.contents.values() if c["id"] == node_id)
            obj["body"] = value["body"]
        elif name == "updateProjectV2":
            self.projects[0]["readme"] = value["readme"]
        else:
            raise AssertionError(name)
        return {}

    def command(self, argv, payload=None):
        self.commands.append((argv, payload))
        return ""


@pytest.fixture
def manifest():
    return {
        "repository": "benpshore/pdftextract",
        "tracking_issue": 144,
        "workstreams": [
            {"issue": 144, "status": "In progress", "prs": []},
            {"issue": 145, "status": "In review", "prs": [140]},
        ],
    }


def arguments(**overrides):
    return SimpleNamespace(
        **{
            "repo": "benpshore/pdftextract",
            "apply": True,
            "project_number": None,
            "install_automation": False,
            "report": None,
            **overrides,
        }
    )


def test_repeat_setup_preserves_unrelated_draft_deleted_archived_cards(manifest):
    gh = FakeGitHub()
    original = copy.deepcopy(gh.items)
    sync.sync(gh, arguments(), manifest)
    assert gh.items[:4] == original
    count = len(gh.mutations)
    sync.sync(gh, arguments(), manifest)
    assert len(gh.mutations) == count
    assert gh.items[:4] == original
    assert gh.contents[145]["body"].startswith("Authored description.\n")
    assert "#140" in gh.contents[145]["body"]
    assert "#145" in gh.contents[140]["body"]
    assert "Related work: #145." in gh.contents[144]["body"]
    assert "#144" not in gh.contents[144]["body"]


def test_closed_items_refresh_even_when_absent_from_open_query(manifest):
    gh = FakeGitHub()
    sync.sync(gh, arguments(), manifest)
    gh.contents[140].update(state="MERGED", merged=True)
    gh.contents[145].update(state="CLOSED", stateReason="COMPLETED")
    sync.sync(gh, arguments(), manifest)
    for number in (140, 145):
        item = next(i for i in gh.items if i["id"] == f"I{number}")
        assert item["fieldValueByName"]["name"] == "Done"


def test_preview_never_mutates_or_stores_token(manifest):
    gh = FakeGitHub()
    sync.sync(gh, arguments(apply=False), manifest)
    assert gh.mutations == []
    assert gh.commands == []


def test_rebase_block_on_latest_human_body(manifest):
    gh = FakeGitHub()
    gh.concurrent_body = "Human edit made after initial discovery.\n"
    sync.sync(gh, arguments(), manifest)
    assert gh.contents[145]["body"].startswith(gh.concurrent_body)


@pytest.mark.parametrize(
    "kind,changes,expected",
    [
        ("PullRequest", {"merged": True, "state": "MERGED"}, "Done"),
        ("PullRequest", {"state": "CLOSED"}, "Cancelled"),
        ("PullRequest", {"isDraft": False}, "In review"),
        ("PullRequest", {}, "In progress"),
        ("Issue", {"state": "CLOSED", "stateReason": "NOT_PLANNED"}, "Cancelled"),
        ("Issue", {"state": "CLOSED", "stateReason": "COMPLETED"}, "Done"),
        ("Issue", {"body": "Status: In progress.\n\nWork"}, "In progress"),
        ("Issue", {"body": "Status: Done.\n"}, "In review"),
    ],
)
def test_state_mapping(kind, changes, expected):
    assert sync.desired_status(content(1, kind, **changes)) == expected


def test_failed_check_blocks_pr_and_merged_pr_is_done():
    pr = content(
        1,
        "PullRequest",
        commits={"nodes": [{"commit": {"statusCheckRollup": {"state": "FAILURE"}}}]},
    )
    assert sync.desired_status(pr) == "Blocked"
    pr["merged"] = True
    assert sync.desired_status(pr) == "Done"


def test_existing_issue_board_choice_and_first_line_precedence():
    obj = content(1)
    assert sync.desired_status(obj, "Blocked", "In review") == "Blocked"
    obj["body"] = "Status: In progress.\nWork"
    assert sync.desired_status(obj, "Blocked", "In review") == "In progress"


def test_existing_ambiguous_project_never_creates_duplicate():
    first = {"id": "A", "number": 1, "closed": False, "title": "Existing project"}
    second = {"id": "B", "number": 2, "closed": False, "title": "Other project"}
    assert sync.select_project([first], [first]) == first
    with pytest.raises(sync.SyncError, match="Multiple"):
        sync.select_project([first, second], [first, second])
    assert sync.select_project([first, second], [first, second], 2) == second


def test_unknown_pr_gets_reciprocal_master_reference(manifest):
    bodies = sync.link_bodies(
        {144: content(144), 999: content(999, "PullRequest")}, manifest, "url"
    )
    assert "#144" in bodies[999]
    assert "#999" in bodies[144]


def test_malformed_managed_markers_abort_instead_of_destroying_text():
    with pytest.raises(sync.SyncError, match="Malformed"):
        sync.managed_body(f"Human text <!-- {sync.MARKER}:start -->", "new")


def test_pagination_follows_cursors_and_rejects_cycles():
    class TwoPages:
        def graphql(self, query, **variables):
            cursor = variables["cursor"]
            return {
                "x": {
                    "nodes": [2] if cursor else [1],
                    "pageInfo": {"hasNextPage": cursor is None, "endCursor": "next"},
                }
            }

    assert sync.pages(TwoPages(), "query", ["x"]) == [1, 2]

    class BadPages:
        def graphql(self, query, **variables):
            return {"x": {"nodes": [1], "pageInfo": {"hasNextPage": True, "endCursor": "same"}}}

    with pytest.raises(sync.SyncError, match="pagination"):
        sync.pages(BadPages(), "query", ["x"])


def test_token_only_sent_to_secret_stdin(manifest, monkeypatch, capsys):
    gh = FakeGitHub()
    monkeypatch.setenv("GH_TOKEN", "synthetic-secret-value")
    sync.sync(gh, arguments(install_automation=True), manifest)
    secret_args, payload = gh.commands[0]
    assert payload == "synthetic-secret-value"
    assert payload not in str(secret_args)
    assert payload not in capsys.readouterr().out


def test_automation_uses_github_com_with_enterprise_cli_defaults(monkeypatch):
    monkeypatch.setenv("GH_HOST", "enterprise.example")
    monkeypatch.setenv("GH_DEBUG", "api")
    monkeypatch.setattr(sync.shutil, "which", lambda _: "/usr/bin/gh")
    calls = []

    def run(argv, **kwargs):
        calls.append((argv, kwargs))
        return SimpleNamespace(returncode=0, stdout="", stderr="")

    monkeypatch.setattr(sync.subprocess, "run", run)
    gh = sync.GitHub()
    gh.command(["secret", "set", "PROJECTS_TOKEN", "--repo", "benpshore/pdftextract"], "token")
    gh.command(["variable", "set", "PROJECT_NUMBER", "--repo", "benpshore/pdftextract"])
    assert all(options["env"]["GH_HOST"] == "github.com" for _, options in calls)
    assert all("GH_DEBUG" not in options["env"] for _, options in calls)
    assert calls[0][1]["input"] == "token"
    assert "token" not in calls[0][0]
    assert sync.os.environ["GH_HOST"] == "enterprise.example"


def test_existing_incompatible_field_or_view_is_not_replaced(manifest):
    gh = FakeGitHub()
    sync.sync(gh, arguments(), manifest)
    gh.fields[-1]["options"] = []
    count = len(gh.mutations)
    with pytest.raises(sync.SyncError, match="no options were replaced"):
        sync.sync(gh, arguments(), manifest)
    assert len(gh.mutations) == count
    gh.fields[-1]["options"] = [{"id": n, "name": n} for n in sync.STATUSES]
    gh.views[0]["filter"] = "is:issue"
    with pytest.raises(sync.SyncError, match="different settings"):
        sync.sync(gh, arguments(), manifest)
    assert len(gh.mutations) == count
