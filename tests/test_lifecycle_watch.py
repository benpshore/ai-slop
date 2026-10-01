"""Offline tests for scripts/lifecycle_watch.py: no network, no GitHub."""

import json
import sys
from datetime import UTC, datetime, timedelta
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))

import lifecycle_watch as lw  # noqa: E402

NOW = datetime(2026, 9, 30, tzinfo=UTC)
SHA = "a" * 40
OTHER = "b" * 40
HEX = "0" * 64
PDFIUM = "bblanchon/pdfium-binaries"


def test_this_repository_is_pinned():
    """The property this repo relies on: every action and image pinned, manifest hashed."""
    assert lw.static_problems(ROOT) == []


def workflow(tmp_path, text):
    path = tmp_path / ".github/workflows"
    path.mkdir(parents=True, exist_ok=True)
    (path / "w.yml").write_text(text)
    return tmp_path


@pytest.mark.parametrize(
    ("line", "expected"),
    [
        (f"- uses: actions/checkout@{SHA} # v7.0.1", 0),
        (f"- uses: github/codeql-action/init@{SHA} # v4.38.2", 0),
        ("- uses: ./.github/actions/local", 0),
        ("- uses: actions/checkout@v4", 1),
        ("- uses: actions/checkout@main # v4", 1),
        (f"- uses: actions/checkout@{SHA}", 1),
        (f"- uses: actions/checkout@{SHA} # trust me", 1),
        ("- uses: docker://alpine:3", 1),
        (f"- uses: docker://alpine@sha256:{HEX}", 0),
        (f"    container: swift:6@sha256:{HEX}", 0),
        ("    container: swift:6", 1),
        ("    # uses: actions/checkout@v1", 0),
    ],
)
def test_pin_rules(tmp_path, line, expected):
    root = workflow(tmp_path, f"jobs:\n  a:\n    steps:\n      {line}\n")
    assert len(lw.collect_pins(root)[1]) == expected


def manifest(tmp_path, **changes):
    entry = {
        "kind": "pdfium",
        "platform": "linux-arm64",
        "url": f"https://github.com/{PDFIUM}/releases/download/chromium%2F8066/x.tgz",
        "dest": ".pdfium/lib/libpdfium.so",
        "archive_sha256": HEX,
        "sha256": HEX,
    }
    entry.update(changes)
    (tmp_path / "native").mkdir(exist_ok=True)
    (tmp_path / "native/manifest.json").write_text(
        json.dumps(
            {"pdfium_release": "chromium/8066", "models_release": "models-v1", "entries": [entry]}
        )
    )
    return tmp_path


@pytest.mark.parametrize(
    "changes",
    [
        {"sha256": None},
        {"archive_sha256": None},
        {"url": "https://example.com/x.tgz"},
        {"url": f"https://github.com/{PDFIUM}/releases/download/chromium%2F9999/x.tgz"},
    ],
)
def test_manifest_problems(tmp_path, changes):
    assert lw.check_native_manifest(manifest(tmp_path, **changes))


def test_manifest_ok(tmp_path):
    assert lw.check_native_manifest(manifest(tmp_path)) == []


class Fake:
    """Stands in for lw.Live with canned answers."""

    def __init__(self, tags=None, published=None, text="", fail=()):
        self._tags, self._published, self._text, self._fail = (
            tags or {},
            published or {},
            text,
            fail,
        )

    def now(self):
        return NOW

    def tags(self, url, pattern):
        if "tags" in self._fail:
            raise OSError("offline")
        return self._tags.get(url, {})

    def release_published(self, repo, tag):
        return self._published.get((repo, tag))

    def text(self, url, headers=None):
        if "text" in self._fail:
            raise OSError("offline")
        return self._text


def native(tmp_path, source, min_age=7):
    report = lw.Report()
    lw.watch_native(manifest(tmp_path), source, report, min_age)
    return report


PDFIUM_URL = f"https://github.com/{PDFIUM}"
MODELS_URL = "https://github.com/docling-project/docling.rs"


def test_pdfium_candidate_needs_age_and_a_release(tmp_path):
    tags = {PDFIUM_URL: {f"chromium/{n}": SHA for n in (8066, 8076, 8090)}}
    published = {
        (PDFIUM, "chromium/8090"): NOW - timedelta(days=1),
        (PDFIUM, "chromium/8076"): NOW - timedelta(days=10),
    }
    report = native(tmp_path, Fake(tags=tags, published=published))
    assert [f.key for f in report.findings] == ["pdfium:chromium/8076"]
    assert "chromium/8090: 1 day(s) old" in report.findings[0].body
    # No release record (or the API failed): never proposed, and the notes say why.
    report = native(tmp_path, Fake(tags=tags))
    assert report.findings == []
    assert any("no published release" in line for line in report.info)
    # Nothing newer than the pin.
    assert native(tmp_path, Fake(tags={PDFIUM_URL: {"chromium/8066": SHA}})).findings == []


def test_new_model_track_is_a_finding_without_age_gate(tmp_path):
    report = native(tmp_path, Fake(tags={MODELS_URL: {"models-v1": SHA, "models-v2": SHA}}))
    assert [f.key for f in report.findings] == ["models:models-v2"]
    assert native(tmp_path, Fake(tags={MODELS_URL: {"models-v1": SHA}})).findings == []


def test_lookup_failure_is_a_warning_not_a_candidate(tmp_path):
    report = native(tmp_path, Fake(fail=("tags",)))
    assert report.findings == [] and len(report.warnings) == 2


def test_verify_pins_online():
    pin = lw.Pin("w.yml", 3, "github/codeql-action/init", SHA, "v4.38.2")
    url = "https://github.com/github/codeql-action"
    ok = lw.Report()
    lw.verify_pins_online([pin], Fake(tags={url: {"v4.38.2": SHA}}), ok)
    assert ok.problems == [] and ok.warnings == []
    moved = lw.Report()
    lw.verify_pins_online([pin], Fake(tags={url: {"v4.38.2": OTHER}}), moved)
    assert "altered" in moved.problems[0]
    missing = lw.Report()
    lw.verify_pins_online([pin], Fake(tags={url: {}}), missing)
    assert "does not exist" in missing.problems[0]
    offline = lw.Report()
    lw.verify_pins_online([pin], Fake(fail=("tags",)), offline)
    assert offline.problems == [] and offline.warnings


README = """
| Image | Architecture | YAML Label | Included Software |
| --- | --- | --- | --- |
| Ubuntu 26.04<br>![Endpoint Badge](x) | x64 | `ubuntu-26.04` | [a] |
| Ubuntu 24.04<br>![Endpoint Badge](x) | x64 | `ubuntu-latest` or `ubuntu-24.04` | [a] |
| macOS 14 [![deprecated](x)](y)<br>![Endpoint Badge](x) | x64 | `macos-14-large` | [a] |
| macOS 14 Arm64 [![deprecated](x)](y)<br>![Endpoint Badge](x) | arm64 | `macos-14` | [a] |
"""


def test_runner_table_and_findings(tmp_path):
    table = lw.runner_table(README)
    assert table["ubuntu-latest"] == ("Ubuntu 24.04", False)
    assert table["macos-14"] == ("macOS 14 Arm64", True)
    text = "jobs:\n  a:\n    runs-on: ubuntu-latest\n  b:\n    runs-on: macos-14\n"
    root = workflow(tmp_path, text)
    assert lw.runner_labels(root) == {"ubuntu-latest", "macos-14"}
    report = lw.Report()
    same = {"runner_latest": {"ubuntu-latest": "Ubuntu 24.04"}}
    lw.watch_runners(root, same, Fake(text=README), report)
    assert [f.key for f in report.findings] == ["runner-deprecated:macos-14"]
    report = lw.Report()
    moved = {"runner_latest": {"ubuntu-latest": "Ubuntu 22.04"}}
    lw.watch_runners(root, moved, Fake(text=README), report)
    assert "runner-latest:ubuntu-latest=Ubuntu-24.04" in [f.key for f in report.findings]
    offline = lw.Report()
    lw.watch_runners(root, {}, Fake(fail=("text",)), offline)
    assert offline.findings == [] and offline.warnings


def finding(key):
    return lw.Finding(key, "g", f"title {key}", "body")


def gh_stub(existing):
    calls = []

    def run(argv):
        calls.append(argv)
        return json.dumps([{"title": t} for t in existing]) if argv[1] == "list" else ""

    return run, calls


def test_issue_cursor_is_any_existing_issue_open_or_closed():
    report = lw.Report(findings=[finding("pdfium:chromium/8076"), finding("models:models-v2")])
    run, calls = gh_stub(["[lifecycle-watch] pdfium:chromium/8076: PDFium ..."])
    assert lw.file_issues(report, "o/r", gh=run) == [
        "[lifecycle-watch] models:models-v2: title models:models-v2"
    ]
    creates = [c for c in calls if c[1] == "create"]
    assert len(creates) == 1
    assert "models:models-v2" in creates[0][creates[0].index("--title") + 1]


def test_issue_dry_run_and_cap():
    report = lw.Report(findings=[finding(f"k{i}") for i in range(9)])
    run, calls = gh_stub([])
    assert len(lw.file_issues(report, "o/r", gh=run, dry_run=True)) == lw.MAX_NEW_ISSUES
    assert all(c[1] == "list" for c in calls)
    assert any("the rest wait" in w for w in report.warnings)
