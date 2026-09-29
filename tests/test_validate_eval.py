"""A green evaluation process must not hide incomplete Native coverage."""

import importlib.util
import json
from pathlib import Path

import pytest

SPEC = importlib.util.spec_from_file_location(
    "validate_eval", Path(__file__).parents[1] / "native" / "validate_eval.py"
)
assert SPEC and SPEC.loader
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


@pytest.fixture
def inputs():
    manifest = {
        "items": [
            {"id": "a", "split": "dev"},
            {"id": "b", "split": "dev"},
            {"id": "c", "split": "holdout"},
        ]
    }
    report = {
        "backend": "pdfium",
        "host": MODULE.host_label(),
        "papers": [
            {"id": "a", "status": "complete", "pages": 20},
            {"id": "b", "status": "complete", "pages": 1},
        ],
        "summary": {"papers": 2, "failed": 0, "ref_recall": 0.0},
    }
    return manifest, report


def test_valid_coverage_does_not_gate_accuracy(inputs):
    assert MODULE.validate(*inputs, "dev", "pdfium") == []


def test_all_failed_is_rejected_even_with_consistent_summary(inputs):
    manifest, report = inputs
    for paper in report["papers"]:
        paper.update(status="failed:pdfium library unavailable", pages=0)
    report["summary"]["failed"] = 2
    errors = MODULE.validate(manifest, report, "dev", "pdfium")
    assert len(errors) == 2
    assert all("incomplete paper" in error for error in errors)


@pytest.mark.parametrize("status", ["partial", "deferred", "failed", None, "unknown"])
def test_noncomplete_status_cannot_hide_in_zero_failed_summary(inputs, status):
    manifest, report = inputs
    report["papers"][0]["status"] = status
    assert "incomplete paper a" in "\n".join(MODULE.validate(manifest, report, "dev", "pdfium"))


def test_missing_and_duplicate_cannot_cancel_out_counts(inputs):
    manifest, report = inputs
    report["papers"][1]["id"] = "a"
    errors = MODULE.validate(manifest, report, "dev", "pdfium")
    assert "duplicate paper IDs: a" in errors
    assert "missing paper IDs: b" in errors


def test_unexpected_papers_and_summary_mismatch(inputs):
    manifest, report = inputs
    report["papers"].append({"id": "c", "status": "complete", "pages": 1})
    errors = MODULE.validate(manifest, report, "dev", "pdfium")
    assert "unexpected paper IDs: c" in errors
    assert "summary.papers: expected 3, got 2" in errors


@pytest.mark.parametrize("field,value", [("backend", "lopdf"), ("host", "wrong host")])
def test_wrong_backend_or_host_is_rejected(inputs, field, value):
    manifest, report = inputs
    report[field] = value
    assert f"{field} mismatch" in "\n".join(MODULE.validate(manifest, report, "dev", "pdfium"))


@pytest.mark.parametrize("field,value", [("papers", True), ("failed", 1), ("failed", None)])
def test_summary_counts_are_checked(inputs, field, value):
    manifest, report = inputs
    report["summary"][field] = value
    assert f"summary.{field}:" in "\n".join(MODULE.validate(manifest, report, "dev", "pdfium"))


@pytest.mark.parametrize("pages", [0, -1, True, "20", None])
def test_complete_requires_real_pages(inputs, pages):
    manifest, report = inputs
    report["papers"][0]["pages"] = pages
    assert "page count" in "\n".join(MODULE.validate(manifest, report, "dev", "pdfium"))


def test_empty_selection_and_duplicate_manifest_are_rejected(inputs):
    manifest, report = inputs
    with pytest.raises(ValueError, match="must contain"):
        MODULE.validate({"items": []}, report, "dev", "pdfium")
    manifest["items"].append(manifest["items"][0])
    with pytest.raises(ValueError, match="duplicate IDs"):
        MODULE.validate(manifest, report, "dev", "pdfium")


def test_all_split_requires_holdout_too(inputs):
    assert "missing paper IDs: c" in MODULE.validate(*inputs, "all", "pdfium")


@pytest.mark.parametrize(
    "system,machine,expected",
    [("Darwin", "arm64", "macos aarch64"), ("Linux", "aarch64", "linux aarch64")],
)
def test_host_names_match_rust(monkeypatch, system, machine, expected):
    monkeypatch.setattr(MODULE.platform, "system", lambda: system)
    monkeypatch.setattr(MODULE.platform, "machine", lambda: machine)
    assert MODULE.host_label() == expected


def test_cli_missing_malformed_valid_and_incomplete_reports(tmp_path, inputs, capsys):
    manifest, report = inputs
    manifest_path = tmp_path / "manifest.json"
    report_path = tmp_path / "report.json"
    manifest_path.write_text(json.dumps(manifest))
    argv = [
        "--manifest",
        str(manifest_path),
        "--split",
        "dev",
        "--backend",
        "pdfium",
        "--report",
        str(report_path),
    ]
    assert MODULE.main(argv) == 1
    report_path.write_text("{")
    assert MODULE.main(argv) == 1
    report_path.write_text(json.dumps(report))
    assert MODULE.main(argv) == 0
    report["papers"] = []
    report["summary"] = {"papers": 0, "failed": 0}
    report_path.write_text(json.dumps(report))
    assert MODULE.main(argv) == 1
    captured = capsys.readouterr()
    assert "accuracy remains diagnostic" in captured.out
    assert "missing paper IDs: a, b" in captured.err
