"""Prepare the next 0.0.x version with uv on a clean branch for a reviewed PR."""

import re
import subprocess
import sys
from pathlib import Path

project = Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else Path(__file__).resolve().parents[1]


def read(*command):
    return subprocess.check_output(command, text=True).strip()


branch = read("git", "-C", str(project), "branch", "--show-current")
if branch in {"", "main", "master"}:
    raise SystemExit("Create a review branch before bumping the package version")
if read("git", "-C", str(project), "status", "--porcelain"):
    raise SystemExit("Commit existing changes before preparing a version PR")
current = read("uv", "version", "--directory", str(project), "--short")
if not re.fullmatch(r"0\.0\.(0|[1-9][0-9]*)", current):
    raise SystemExit("Python releases must stay in the 0.0.x series")
subprocess.run(
    ["uv", "version", "--directory", str(project), "--bump", "patch", "--no-sync"], check=True
)
expected = f"0.0.{int(current.rsplit('.', 1)[1]) + 1}"
if read("uv", "version", "--directory", str(project), "--short") != expected:
    raise SystemExit("uv did not produce the next patch version")
print(f"Review pyproject.toml and uv.lock in a PR, then tag its merge as python-v{expected}")
