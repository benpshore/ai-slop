import subprocess
import sys
import tomllib
from pathlib import Path

import pytest

SCRIPT = Path(__file__).parents[1] / "scripts" / "release.py"


def project(tmp_path, version="0.0.1", branch="codex/version"):
    subprocess.run(["git", "init", "--initial-branch", branch, str(tmp_path)], check=True)
    for key, value in [("user.name", "Test"), ("user.email", "test@example.invalid")]:
        subprocess.run(["git", "-C", str(tmp_path), "config", key, value], check=True)
    (tmp_path / "pyproject.toml").write_text(
        f'[project]\nname="release-test"\nversion="{version}"\n'
        'requires-python=">=3.14,<3.15"\ndependencies=[]\n'
    )
    (tmp_path / ".python-version").write_text("3.14.7\n")
    subprocess.run(["git", "-C", str(tmp_path), "add", "."], check=True)
    subprocess.run(["git", "-C", str(tmp_path), "commit", "-m", "fixture"], check=True)
    return tmp_path


def bump(folder):
    return subprocess.run(
        [sys.executable, str(SCRIPT), str(folder)], capture_output=True, text=True
    )


def test_real_uv_bumps_patch_and_lock_on_review_branch(tmp_path):
    folder = project(tmp_path)
    result = bump(folder)
    assert result.returncode == 0, result.stderr
    assert tomllib.loads((folder / "pyproject.toml").read_text())["project"]["version"] == "0.0.2"
    assert (folder / "uv.lock").exists()


@pytest.mark.parametrize("branch", ["main", "master"])
def test_no_bump_on_protected_conventional_branches(tmp_path, branch):
    folder = project(tmp_path, branch=branch)
    assert bump(folder).returncode != 0
    assert tomllib.loads((folder / "pyproject.toml").read_text())["project"]["version"] == "0.0.1"


def test_dirty_tree_is_preserved(tmp_path):
    folder = project(tmp_path)
    (folder / "work.txt").write_text("unfinished")
    assert bump(folder).returncode != 0
    assert (folder / "work.txt").read_text() == "unfinished"


def test_rejects_versions_outside_patch_series(tmp_path):
    folder = project(tmp_path, version="0.1.0")
    assert bump(folder).returncode != 0
