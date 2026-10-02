"""Native cohort runner retains failed inputs and cutoff diagnostics."""

import hashlib
import importlib.util
import json
import sys
from argparse import Namespace
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "native" / "pmc_validate.py"
SPEC = importlib.util.spec_from_file_location("native_pmc", SCRIPT)
runner = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(runner)


def test_crash_and_timeout_remain_failed_records(tmp_path):
    pdf = tmp_path / "PMC123.1.pdf"
    for name, code, timeout in (
        ("crash", "import os, signal; os.kill(os.getpid(), signal.SIGABRT)", 5),
        ("cutoff", "import time; time.sleep(20)", 0.05),
    ):
        record, execution = runner.invoke(
            [sys.executable, "-c", code], pdf, tmp_path / name, timeout
        )
        assert record["path"] == str(pdf)
        assert record["status"] == "failed"
        assert execution["returncode"] < 0
        assert "runner:" in record["error"]
        assert (tmp_path / f"{name}.stderr").exists()


def test_valid_partial_output_preserves_entries_and_cutoff(tmp_path):
    pdf = tmp_path / "PMC123.1.pdf"
    emitted = {
        "path": str(pdf),
        "status": "found",
        "references": [{"raw": "1. A reference"}],
        "extraction_status": "partial",
        "warnings": ["resource_limit: chars"],
    }
    code = f"import json; print(json.dumps({emitted!r}))"
    record, execution = runner.invoke([sys.executable, "-c", code], pdf, tmp_path / "partial", 5)
    assert record == emitted
    assert execution["returncode"] == 0
    assert json.loads((tmp_path / "partial.stdout").read_text()) == emitted


def test_wrong_input_and_changed_truth_are_not_scored_as_success(tmp_path):
    pdf = tmp_path / "PMC123.1.pdf"
    code = 'print(\'{"path":"other.pdf","status":"found"}\')'
    record, execution = runner.invoke([sys.executable, "-c", code], pdf, tmp_path / "wrong", 5)
    assert record["status"] == "failed"
    assert "different input" in execution["record_error"]
    xml = tmp_path / "PMC123.1.xml"
    xml.write_text("<article>changed</article>")
    checked = runner.verified(xml, "00000000000000000000000000000000")
    assert checked["verified"] is False
    assert checked["error"] == "pinned MD5 mismatch"


def test_forward_source_observation_is_accepted(tmp_path):
    pdf = tmp_path / "PMC123.1.pdf"
    emitted = {
        "document": {"sources": [{"path": str(pdf)}], "pages": 1},
        "status": "complete",
        "references": [],
        "backend": {"name": "lopdf"},
    }
    code = f"import json; print(json.dumps({emitted!r}))"
    record, execution = runner.invoke([sys.executable, "-c", code], pdf, tmp_path / "forward", 5)
    assert record == emitted
    assert "record_error" not in execution


def test_missing_pdf_keeps_truth_entries_and_missing_truth_fails_comparison(tmp_path, monkeypatch):
    cache = tmp_path / "cache"
    cache.mkdir()
    jats = (
        b"<article><back><ref-list><ref><mixed-citation>One reference</mixed-citation>"
        b"</ref></ref-list></back></article>"
    )
    items = []
    for index in range(2):
        pmcid = f"PMC{index + 123}"
        items.append(
            {
                "pmcid": pmcid,
                "version": 1,
                "ref_count": 1,
                "xml_md5": hashlib.md5(jats, usedforsecurity=False).hexdigest(),
                "pdf_md5": "00000000000000000000000000000000",
            }
        )
        if index == 0:
            (cache / f"{pmcid}.1.xml").write_bytes(jats)
    manifest = tmp_path / "manifest.json"
    manifest.write_text(json.dumps({"items": items}))
    monkeypatch.setattr(runner, "PAPERS", 2)
    monkeypatch.setattr(runner, "TRUTH_ENTRIES", 2)
    monkeypatch.setattr(
        runner, "MANIFEST_SHA256", hashlib.sha256(manifest.read_bytes()).hexdigest()
    )
    out = tmp_path / "report"
    result = runner.main(
        [
            "--binary",
            sys.executable,
            "--backend",
            "lopdf",
            "--manifest",
            str(manifest),
            "--cache",
            str(cache),
            "--out",
            str(out),
            "--code-sha",
            "test-sha",
        ]
    )
    assert result == 1
    execution = json.loads((out / "execution.json").read_text())
    assert execution["coverage"]["paper_denominator"] == 2
    assert execution["coverage"]["truth_entry_denominator"] == 2
    assert execution["coverage"]["rows"] == 2
    assert execution["coverage"]["paths"]["backward"]["records"] == 2
    report = json.loads((out / "report.json").read_text())
    assert len(report["papers"]) == 2
    assert report["papers"][0]["backward"]["entry_results"][0]["truth"]["raw"] == "One reference"
    assert report["papers"][0]["backward"]["entry_results"][0]["extracted"] is None
    assert report["papers"][1]["truth_error"]


def test_total_budget_keeps_every_queued_paper_as_failed(tmp_path):
    pdf = tmp_path / "PMC123.1.pdf"
    pdf.write_bytes(b"verified input")
    item = {
        "pmcid": "PMC123",
        "version": 1,
        "pdf_md5": hashlib.md5(pdf.read_bytes(), usedforsecurity=False).hexdigest(),
        "xml_md5": "00000000000000000000000000000000",
    }
    args = Namespace(cache=tmp_path, binary=Path("does-not-run"), backend="lopdf", deadline=0)
    row = runner.run_paper(item, args, tmp_path)
    assert row["pdf"]["verified"]
    for name in ("backward", "forward"):
        assert row["paths"][name]["record"]["status"] == "failed"
        assert row["paths"][name]["execution"]["outcome"] == "budget_exhausted"
