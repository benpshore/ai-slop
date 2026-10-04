"""Focused diagnostic failures must not remove cases or rewrite Partial status."""

import importlib.util
import json
import sys
from pathlib import Path

import pytest

ROOT = Path(__file__).parents[1]
SPEC = importlib.util.spec_from_file_location(
    "bibliography_evidence", ROOT / "native" / "bibliography_evidence.py"
)
assert SPEC and SPEC.loader
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


def test_timeout_is_recorded_and_partial_process_output_is_retained(tmp_path):
    log = tmp_path / "timeout.log"
    result = MODULE.run(
        [sys.executable, "-c", "import time; print('started', flush=True); time.sleep(30)"],
        log,
        0.2,
    )
    assert result["exit_code"] == 1
    assert "timed out" in result["error"]
    assert "started" in log.read_text()


def test_manifest_selection_preserves_exact_pins_and_original_bytes():
    path = ROOT / "corpus" / "manifest.json"
    original = path.read_bytes()
    manifest = json.loads(original)
    selected = MODULE.selected_items(manifest)
    assert len(selected) == 7
    assert all(item in manifest["items"] for item in selected)
    assert path.read_bytes() == original
    manifest["items"].append(selected[0])
    with pytest.raises(ValueError, match="expected one manifest entry"):
        MODULE.selected_items(manifest)


@pytest.mark.parametrize(
    "failure",
    ["fetch", "missing_score", "truncated_score", "invalid_score", "nodes", "zero_match", None],
)
def test_failures_and_partial_scores_remain_in_fixed_denominator(tmp_path, monkeypatch, failure):
    calls = []

    def execute(command, log, timeout):
        assert timeout > 0
        calls.append(command)
        log.write_text("fixture process output\n")
        if command[1] == "corpus":
            return {"exit_code": int(failure == "fetch")}
        output = Path(command[-1])
        output.mkdir()
        if failure == "missing_score" and len(calls) == 2:
            return {"exit_code": 1, "error": "fixture missing input"}
        score = {
            "id": command[-2],
            "status": "partial",
            "truth_refs": 10,
            "extracted_refs": 10,
            "matched_refs": 0 if failure == "zero_match" and len(calls) == 2 else 10,
        }
        (output / "score.json").write_text(json.dumps(score))
        if failure == "truncated_score" and len(calls) == 2:
            (output / "score.json").write_text('{"id":')
        if failure == "invalid_score" and len(calls) == 2:
            (output / "score.json").write_text('{"matched_refs": "invalid"}')
        return {"exit_code": int(failure == "nodes" and command[-3] == "docling")}

    monkeypatch.setattr(MODULE, "run", execute)
    monkeypatch.chdir(ROOT)
    output = tmp_path / "evidence"
    result = MODULE.collect(
        ROOT / "corpus" / "manifest.json",
        tmp_path / "cache",
        output,
        Path("unused-tpe"),
        Path("unused-example"),
    )
    report = json.loads((output / "diagnostic-summary.json").read_text())
    assert result == int(failure is not None)
    assert len(calls) == 10  # Fetch plus every case, even after acquisition fails.
    assert len(report["cases"]) == 9
    assert report["historical_case_denominator"] == 8
    assert report["additional_current_cases"] == 1
    assert report["unique_input_denominator"] == 7
    assert report["full_native_corpus_denominator"] == 60
    bad_score = failure in {"missing_score", "truncated_score", "invalid_score"}
    assert report["observed_scores"] == (8 if bad_score else 9)
    assert report["failed_cases"] == int(bad_score or failure == "nodes")
    assert report["zero_match_cases"] == int(failure == "zero_match")
    assert all(case["score"]["status"] == "partial" for case in report["cases"] if "score" in case)
    with pytest.raises(ValueError, match="output must be empty"):
        MODULE.collect(
            ROOT / "corpus" / "manifest.json", tmp_path, output, Path("tpe"), Path("example")
        )
