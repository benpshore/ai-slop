"""Read-only lifecycle watcher: pin hygiene, native upstream candidates, runner images.

Run through uv (`uv run --no-project scripts/lifecycle_watch.py ...`); the standard
library is enough. It edits nothing, opens no pull request and merges nothing. Its
only write is a GitHub issue (`--file-issues`) for a candidate that is past the
cooldown; closing that issue records the decision not to adopt it
(docs/LIFECYCLE.md). Cargo, uv and Actions versions are Dependabot's, not this
script's: it covers what Dependabot cannot see (PDFium binaries, model releases,
runner images) and checks that pins are what they claim to be.

Upstream text (tags) reaches an issue title or body only after a strict pattern
match, and issues are created with argument lists, never through a shell.
"""

# ruff: noqa: S603, S607
# (subprocess is used only with fixed argument lists, never a shell.)
from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass, field
from datetime import UTC, datetime
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
CONFIG = ".github/lifecycle.json"
ISSUE_PREFIX = "[lifecycle-watch]"
MAX_NEW_ISSUES = 5
PDFIUM_REPO = "bblanchon/pdfium-binaries"

SHA1 = re.compile(r"^[0-9a-f]{40}$")
SHA256 = re.compile(r"^[0-9a-f]{64}$")
DIGEST = re.compile(r"@sha256:[0-9a-f]{64}$")
USES = re.compile(r"^\s*(?:-\s*)?uses:\s*(?P<ref>[^\s#]+)(?:\s+#\s*(?P<comment>.*?))?\s*$")
IMAGE = re.compile(r"^\s*(?:container|image):\s*(?P<ref>[^\s#$]\S*)\s*$")
TAG_COMMENT = re.compile(r"^(?P<tag>v?\d+(?:\.\d+){0,2})(?:\s|$)")
RUNNER_LABEL = re.compile(r"\b(?:ubuntu|macos|windows)-(?:latest|\d+(?:\.\d+)?(?:-[a-z0-9]+)*)\b")
LOOKUP_ERRORS = (subprocess.SubprocessError, OSError, ValueError, KeyError, urllib.error.URLError)


@dataclass(frozen=True)
class Pin:
    file: str
    line: int
    action: str  # owner/repo[/path]
    ref: str
    comment: str

    @property
    def repo(self) -> str:
        return "/".join(self.action.split("/")[:2])


@dataclass(frozen=True)
class Finding:
    key: str  # cursor identity: one issue per key, ever
    group: str  # coupled group: one update PR per group
    title: str
    body: str


@dataclass
class Report:
    problems: list[str] = field(default_factory=list)  # integrity failures: exit 1
    warnings: list[str] = field(default_factory=list)  # failed lookups: visible, not fatal
    findings: list[Finding] = field(default_factory=list)
    info: list[str] = field(default_factory=list)


# --------------------------------------------------------------------------
# Static checks: offline, also run by tests/test_lifecycle_watch.py inside `ci`


def workflow_files(root: Path) -> list[Path]:
    return sorted((root / ".github/workflows").glob("*.y*ml"))


def collect_pins(root: Path) -> tuple[list[Pin], list[str]]:
    """Every third-party `uses:` and container image, plus what is not pinned."""
    pins: list[Pin] = []
    problems: list[str] = []
    for path in workflow_files(root):
        rel = path.relative_to(root).as_posix()
        for number, line in enumerate(path.read_text().splitlines(), start=1):
            if line.lstrip().startswith("#"):
                continue
            where = f"{rel}:{number}"
            if match := USES.match(line):
                ref, comment = match["ref"], (match["comment"] or "").strip()
                if ref.startswith("./") or "${{" in ref:
                    continue
                if ref.startswith("docker://"):
                    if not DIGEST.search(ref):
                        problems.append(f"{where}: {ref} is not pinned by sha256 digest")
                    continue
                action, _, version = ref.partition("@")
                if not SHA1.fullmatch(version):
                    problems.append(f"{where}: {ref} is not pinned by full commit SHA")
                elif not TAG_COMMENT.match(comment):
                    problems.append(f"{where}: {action}@{version[:12]} lacks a `# vX.Y.Z` comment")
                else:
                    pins.append(Pin(rel, number, action, version, comment))
            elif (match := IMAGE.match(line)) and not DIGEST.search(match["ref"]):
                problems.append(f"{where}: image {match['ref']} is not pinned by sha256 digest")
    return pins, problems


def check_native_manifest(root: Path) -> list[str]:
    """Every native artifact has its hashes and a URL inside the pinned release."""
    path = root / "native/manifest.json"
    if not path.exists():
        return []
    manifest = json.loads(path.read_text())
    release = {"pdfium": manifest["pdfium_release"], "model": manifest["models_release"]}
    problems = []
    for entry in manifest["entries"]:
        name = f"native/manifest.json {entry['dest']}"
        if not SHA256.fullmatch(entry.get("sha256") or ""):
            problems.append(f"{name}: sha256 is not pinned")
        if entry["kind"] == "pdfium" and not SHA256.fullmatch(entry.get("archive_sha256") or ""):
            problems.append(f"{name}: archive_sha256 is not pinned")
        url = urllib.parse.unquote(entry["url"])
        if not url.startswith("https://github.com/") or release[entry["kind"]] not in url:
            problems.append(f"{name}: url is not inside the pinned {release[entry['kind']]!r}")
    return problems


def static_problems(root: Path) -> list[str]:
    return collect_pins(root)[1] + check_native_manifest(root)


# --------------------------------------------------------------------------
# Live sources (a fake replaces this in the tests)


class Live:
    def __init__(self) -> None:
        self.token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")

    def now(self) -> datetime:
        return datetime.now(UTC)

    def tags(self, url: str, pattern: str) -> dict[str, str]:
        """Tag -> commit SHA (annotated tags peeled) for refs matching pattern."""
        ref = f"refs/tags/{pattern}"
        out = subprocess.run(
            ["git", "ls-remote", "--tags", url, ref, f"{ref}^{{}}"],
            capture_output=True,
            text=True,
            check=True,
            timeout=60,
        ).stdout
        tags: dict[str, str] = {}
        for line in out.splitlines():
            sha, _, name = line.partition("\t")
            name = name.removeprefix("refs/tags/")
            if name.endswith("^{}"):
                tags[name[:-3]] = sha
            else:
                tags.setdefault(name, sha)
        return tags

    def text(self, url: str, headers: dict[str, str] | None = None) -> str:
        if urllib.parse.urlsplit(url).scheme != "https":
            raise ValueError(f"refusing non-https URL: {url}")
        request = urllib.request.Request(  # noqa: S310 - https only, checked above
            url, headers={"User-Agent": "pdftextract-lifecycle-watch", **(headers or {})}
        )
        with urllib.request.urlopen(request, timeout=30) as response:  # noqa: S310
            return response.read(8 * 1024 * 1024).decode()

    def release_published(self, repo: str, tag: str) -> datetime | None:
        headers = {"Accept": "application/vnd.github+json"}
        if self.token:
            headers["Authorization"] = f"Bearer {self.token}"
        quoted = urllib.parse.quote(tag, safe="")
        try:
            stamp = json.loads(
                self.text(f"https://api.github.com/repos/{repo}/releases/tags/{quoted}", headers)
            ).get("published_at")
        except urllib.error.HTTPError as error:
            if error.code == 404:
                return None  # a bare tag with no release is not adoptable
            raise
        return datetime.fromisoformat(stamp.replace("Z", "+00:00")) if stamp else None


# --------------------------------------------------------------------------
# Candidates and pins


def body(summary: str, group: str, notes: list[str], how: str) -> str:
    parts = [summary, "", f"Coupled group `{group}`: one update PR per group, not per release."]
    if notes:
        parts += ["", "Cooldown notes:", *[f"- {n}" for n in notes]]
    parts += [
        "",
        f"How to act: {how}",
        "",
        "Closing this issue records a decision not to adopt this candidate; a newer one opens "
        "a new issue. Nothing merges without review (docs/LIFECYCLE.md). "
        "Opened by `.github/workflows/lifecycle-watch.yml`.",
    ]
    return "\n".join(parts)


def watch_native(root: Path, source, report: Report, min_age: int) -> None:
    manifest = json.loads((root / "native/manifest.json").read_text())

    # PDFium binaries: the newest chromium/N with a published release past the cooldown.
    pinned = re.fullmatch(r"chromium/(\d+)", manifest["pdfium_release"])
    try:
        found = source.tags(f"https://github.com/{PDFIUM_REPO}", "chromium/*")
        builds = sorted({m[1] for t in found if (m := re.fullmatch(r"chromium/(\d+)", t))}, key=int)
        if pinned and builds:
            report.info.append(
                f"PDFium binaries: pinned chromium/{pinned[1]}, newest tag chromium/{builds[-1]}"
            )
            notes = []
            for build in [b for b in builds if int(b) > int(pinned[1])][-6:][::-1]:
                published = source.release_published(PDFIUM_REPO, f"chromium/{build}")
                age = None if published is None else (source.now() - published).days
                if age is not None and age >= min_age:
                    report.findings.append(pdfium_finding(pinned[1], build, notes))
                    break
                notes.append(
                    f"chromium/{build}: "
                    + ("no published release" if age is None else f"{age} day(s) old")
                    + f" (cooldown {min_age} days)"
                )
            else:
                report.info += [f"  {n}" for n in notes]
    except LOOKUP_ERRORS as error:
        report.warnings.append(f"PDFium lookup failed: {error}")

    # Model releases are a separate track from the crates. Tags carry no dates.
    model = re.fullmatch(r"models-v(\d+)", manifest["models_release"])
    try:
        found = source.tags("https://github.com/docling-project/docling.rs", "models-*")
        tracks = sorted({int(m[1]) for t in found if (m := re.fullmatch(r"models-v(\d+)", t))})
        if model and tracks and tracks[-1] > int(model[1]):
            report.findings.append(
                Finding(
                    f"models:models-v{tracks[-1]}",
                    "native-pdf",
                    f"Docling model release models-v{model[1]} -> models-v{tracks[-1]}",
                    body(
                        f"A newer model release track exists: models-v{tracks[-1]}.",
                        "native-pdf",
                        ["tags carry no publication date; nothing to age-gate"],
                        "a new model family needs an explicit compatibility and quality "
                        "decision (preprocessing, precision, provider); the number implies none.",
                    ),
                )
            )
    except LOOKUP_ERRORS as error:
        report.warnings.append(f"Docling models lookup failed: {error}")


def pdfium_finding(pinned: str, build: str, notes: list[str]) -> Finding:
    return Finding(
        f"pdfium:chromium/{build}",
        "native-pdf",
        f"PDFium binaries chromium/{pinned} -> chromium/{build}",
        body(
            f"`native/manifest.json` pins chromium/{pinned}; chromium/{build} is published "
            "and past the cooldown.",
            "native-pdf",
            notes,
            "change `pdfium_release` and every pdfium `url`, `archive_sha256`, `sha256` in one "
            "PR (compute both digests yourself; do not copy them from a checksum file beside "
            "the archive), then run Native and the corpus comparison.",
        ),
    )


def verify_pins_online(pins: list[Pin], source, report: Report) -> None:
    """A pinned SHA must be what its `# vX.Y.Z` comment names upstream."""
    seen: set[tuple[str, str]] = set()
    for pin in pins:
        tag = TAG_COMMENT.match(pin.comment)["tag"]  # type: ignore[index]
        if (pin.repo, pin.ref) in seen:
            continue
        seen.add((pin.repo, pin.ref))
        try:
            resolved = source.tags(f"https://github.com/{pin.repo}", tag).get(tag)
        except LOOKUP_ERRORS as error:
            report.warnings.append(f"{pin.repo}@{tag}: tag lookup failed: {error}")
            continue
        if resolved is None:
            report.problems.append(f"{pin.file}:{pin.line}: tag {tag} does not exist in {pin.repo}")
        elif resolved != pin.ref:
            report.problems.append(
                f"{pin.file}:{pin.line}: {pin.repo}@{pin.ref[:12]} is not {tag} "
                f"(upstream {tag} is {resolved[:12]}); the comment or the tag was altered"
            )


def runner_labels(root: Path) -> set[str]:
    labels: set[str] = set()
    for path in workflow_files(root):
        for line in path.read_text().splitlines():
            if re.match(r"^\s*(?:-\s*)?(?:runs-on|os):", line):
                labels.update(RUNNER_LABEL.findall(line))
    return labels


def runner_table(readme: str) -> dict[str, tuple[str, bool]]:
    """YAML label -> (image name, deprecated) from actions/runner-images' table."""
    images: dict[str, tuple[str, bool]] = {}
    for row in readme.splitlines():
        cells = [c.strip() for c in row.strip().strip("|").split("|")]
        if len(cells) >= 4 and cells[2].startswith("`"):
            name = re.split(r"\s*[\[<]", cells[0], maxsplit=1)[0].strip()
            for label in re.findall(r"`([^`]+)`", cells[2]):
                images[label] = (name, "deprecated" in cells[0].lower())
    return images


def watch_runners(root: Path, config: dict, source, report: Report) -> None:
    try:
        url = "https://raw.githubusercontent.com/actions/runner-images/main/README.md"
        table = runner_table(source.text(url))
    except LOOKUP_ERRORS as error:
        report.warnings.append(f"runner image table lookup failed: {error}")
        return
    expected = config.get("runner_latest", {})
    for label in sorted(runner_labels(root)):
        if label not in table:
            report.warnings.append(f"runner label `{label}` is not in the runner-images table")
            continue
        name, deprecated = table[label]
        report.info.append(f"runner `{label}` -> {name}{' (DEPRECATED)' if deprecated else ''}")
        if deprecated:
            report.findings.append(
                Finding(
                    f"runner-deprecated:{label}",
                    "runners",
                    f"runner label {label} ({name}) is deprecated",
                    body(
                        f"`{label}` is used in `.github/workflows/`; the runner-images "
                        f"table marks {name} deprecated.",
                        "runners",
                        [],
                        "move the jobs to a supported image in a PR that runs them.",
                    ),
                )
            )
        if label in expected and expected[label] != name:
            report.findings.append(
                Finding(
                    f"runner-latest:{label}={re.sub(r'[^A-Za-z0-9.]+', '-', name)}",
                    "runners",
                    f"{label} now maps to {name} (was {expected[label]})",
                    body(
                        f"`.github/lifecycle.json` expects {label} = {expected[label]}; "
                        f"the runner-images table now says {name}, so jobs on `{label}` "
                        "change image.",
                        "runners",
                        [],
                        "run the affected workflows on the new image (or pin the old label), "
                        "then update `runner_latest` in `.github/lifecycle.json` in that PR.",
                    ),
                )
            )


# --------------------------------------------------------------------------
# Orchestration


def load_config(root: Path) -> dict:
    path = root / CONFIG
    return json.loads(path.read_text()) if path.exists() else {}


def run_watch(root: Path, source, min_age: int, offline: bool = False) -> Report:
    pins, problems = collect_pins(root)
    report = Report(problems=problems + check_native_manifest(root))
    if not offline:
        verify_pins_online(pins, source, report)
        watch_native(root, source, report, min_age)
        watch_runners(root, load_config(root), source, report)
    return report


def render(report: Report, min_age: int) -> str:
    out = ["# Lifecycle watch", "", f"Cooldown: {min_age} days.", ""]
    candidates = [f"`{f.key}` ({f.group}): {f.title}" for f in report.findings]
    for title, items in [
        ("Integrity problems (the run fails)", report.problems),
        ("Lookups that failed (visible, not fatal)", report.warnings),
        ("Candidates past the cooldown", candidates),
        ("Observed", report.info),
    ]:
        out += [f"## {title}", *([f"- {i}" for i in items] or ["- none"]), ""]
    return "\n".join(out)


def gh_cli(argv: list[str]) -> str:
    # Fixed argv, no shell; the token comes from the environment (GH_TOKEN).
    return subprocess.run(
        ["gh", *argv], capture_output=True, text=True, check=True, timeout=120
    ).stdout


def file_issues(report: Report, repo: str, gh=None, dry_run: bool = False) -> list[str]:
    """One issue per finding key, ever: an existing issue, open or closed, is the cursor."""
    run = gh or gh_cli
    listing = ["issue", "list", "--repo", repo, "--state", "all", "--limit", "500"]
    titles = [i["title"] for i in json.loads(run([*listing, "--json", "title"]))]
    created: list[str] = []
    for finding in report.findings:
        prefix = f"{ISSUE_PREFIX} {finding.key}:"
        if any(t.startswith(prefix) for t in titles):
            continue
        if len(created) >= MAX_NEW_ISSUES:
            report.warnings.append(f"more than {MAX_NEW_ISSUES} new candidates; the rest wait")
            break
        created.append(f"{prefix} {finding.title}")
        if not dry_run:
            run(["issue", "create", "--repo", repo, "--title", created[-1], "--body", finding.body])
    return created


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("check-pins", help="offline pin hygiene; exit 1 on problems")
    watch = sub.add_parser("watch", help="pins, native candidates, runner images")
    watch.add_argument("--offline", action="store_true", help="static checks only")
    watch.add_argument("--min-age-days", type=int)
    watch.add_argument("--file-issues", action="store_true")
    watch.add_argument("--dry-run", action="store_true", help="with --file-issues: do not create")
    watch.add_argument("--repo", default=os.environ.get("GITHUB_REPOSITORY", ""))
    args = parser.parse_args(argv)

    if args.command == "check-pins":
        problems = static_problems(ROOT)
        print("\n".join(problems) if problems else "all pins verified statically")
        return 1 if problems else 0

    min_age = args.min_age_days or load_config(ROOT).get("min_age_days", 7)
    report = run_watch(ROOT, Live(), min_age, args.offline)
    if args.file_issues and report.findings:
        if not args.repo:
            parser.error("--file-issues needs --repo (or GITHUB_REPOSITORY)")
        verb = "would open" if args.dry_run else "opened"
        for title in file_issues(report, args.repo, dry_run=args.dry_run):
            report.info.append(f"{verb} issue: {title}")
    text = render(report, min_age)
    print(text)
    if summary := os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(summary, "a") as handle:
            handle.write(text + "\n")
    for warning in report.warnings:
        print(f"::warning::{warning}")
    return 1 if report.problems else 0


if __name__ == "__main__":
    sys.exit(main())
