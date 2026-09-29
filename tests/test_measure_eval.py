"""Exercise real child processes, including failure and memory unit conversion."""

import importlib.util
import json
import platform
import sys
from pathlib import Path

import pytest

SPEC = importlib.util.spec_from_file_location(
    "measure_eval", Path(__file__).parents[1] / "native" / "measure_eval.py"
)
assert SPEC and SPEC.loader
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


@pytest.mark.skipif(platform.system() not in {"Linux", "Darwin"}, reason="Unix rusage")
def test_measure_real_child_memory_and_failure():
    result = MODULE.measure(
        [sys.executable, "-c", "data = bytearray(32 * 1024 * 1024); raise SystemExit(7)"]
    )
    assert result["exit_code"] == 7
    assert result["wall_seconds"] > 0
    assert result["peak_rss_bytes"] >= 32 * 1024 * 1024


@pytest.mark.skipif(platform.system() not in {"Linux", "Darwin"}, reason="Unix wait4")
def test_measure_does_not_reuse_previous_child_peak():
    large = MODULE.measure([sys.executable, "-c", "data = bytearray(80 * 1024 * 1024)"])
    small = MODULE.measure([sys.executable, "-c", "pass"])
    assert large["exit_code"] == small["exit_code"] == 0
    assert large["peak_rss_bytes"] > small["peak_rss_bytes"] + 40 * 1024 * 1024


def test_provenance_records_pinned_inputs_and_sources():
    result = MODULE.provenance(Path("corpus/manifest.json"), "lopdf", "dev")
    assert result["backend"] == "lopdf"
    assert len(result["git_commit"]) == 40
    assert len(result["metric_source_sha256"]) == 64
    assert len(result["truth_source_sha256"]) == 64
    assert len(result["paper_inputs"]) > 0
    assert "not measured" in result["timing_note"]


def test_macos_host_matches_rust_report(monkeypatch):
    monkeypatch.setattr(MODULE.platform, "system", lambda: "Darwin")
    monkeypatch.setattr(MODULE.platform, "machine", lambda: "arm64")
    result = MODULE.provenance(Path("corpus/manifest.json"), "pdfium", "dev")
    assert result["host"] == "macos aarch64"


def test_stale_output_is_refused_without_relabeling_it(tmp_path, monkeypatch):
    report = tmp_path / "report.json"
    report.write_text("old evidence")
    monkeypatch.setattr(
        sys, "argv", ["measure_eval", "--backend", "pdfium", "--out", str(tmp_path)]
    )
    with pytest.raises(SystemExit) as error:
        MODULE.main()
    assert error.value.code == 2
    assert report.read_text() == "old evidence"
    assert not (tmp_path / "provenance.json").exists()


def test_missing_executable_records_failure(tmp_path, monkeypatch):
    output = tmp_path / "new"
    monkeypatch.setattr(
        sys,
        "argv",
        [
            "measure_eval",
            "--backend",
            "pdfium",
            "--out",
            str(output),
            "--executable",
            str(tmp_path / "missing"),
        ],
    )
    assert MODULE.main() == 127
    assert json.loads((output / "resources.json").read_text())["exit_code"] == 127
    assert json.loads((output / "provenance.json").read_text())["backend"] == "pdfium"
