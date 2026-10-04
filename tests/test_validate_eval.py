"""A green evaluation process must not hide incomplete Native coverage."""

import importlib.util
import json
import os
import shutil
import subprocess
import textwrap
from copy import deepcopy
from pathlib import Path

import pytest

SPEC = importlib.util.spec_from_file_location(
    "validate_eval", Path(__file__).parents[1] / "native" / "validate_eval.py"
)
assert SPEC and SPEC.loader
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


def reviewed_inputs(inputs):
    manifest, report = deepcopy(inputs)
    for item in manifest["items"]:
        item.update(pdf_sha256=f"pdf-{item['id']}", source_sha256=f"source-{item['id']}")
    pins = dict.fromkeys(
        (
            "corpus_manifest_sha256",
            "native_manifest_sha256",
            "cargo_lock_sha256",
            "metric_source_sha256",
            "truth_source_sha256",
        ),
        "pinned",
    )
    pins["cargo_lock_sha256"] = "a" * 64
    sources = {
        item["id"]: {key: item[key] for key in ("pdf_sha256", "source_sha256")}
        for item in manifest["items"]
        if item["split"] == "dev"
    }
    outcome = {
        "inputs": sources["a"],
        "pages": 20,
        "warning_count": 2,
        "warnings": ["page 3: resource_limit: candidate window truncated (limit=256)"],
        "resource_page_warnings": [[3, "resource_limit: candidate window truncated (limit=256)"]],
        "page_warnings_sha256": MODULE.diagnostics_digest(
            [[3, "resource_limit: candidate window truncated (limit=256)"]]
        ),
        "reference_baseline": {"truth_refs": 1, "matched_refs": 1, "spurious_refs": 0},
    }
    report["papers"][0].update(status="partial", warnings=2)
    policy = {
        "version": 1,
        "host": MODULE.host_label(),
        "split": "dev",
        "provenance_pins": pins,
        "papers": {"pdfium": {"a": outcome}},
    }
    provenance = {
        **pins,
        "backend": "pdfium",
        "host": MODULE.host_label(),
        "split": "dev",
        "paper_inputs": deepcopy(sources),
    }
    return manifest, report, policy, provenance


def allowances(manifest, policy, provenance, changed_dump=None):
    dumps = {
        paper_id: {
            "id": paper_id,
            "pages": outcome["pages"],
            "warnings": outcome["warnings"].copy(),
            "page_warnings": deepcopy(outcome["resource_page_warnings"]),
        }
        for paper_id, outcome in policy["papers"]["pdfium"].items()
    }
    if changed_dump:
        dumps["a"].update(changed_dump)
    return MODULE.load_reviewed_partials(
        manifest, "pinned", policy, provenance, "dev", "pdfium", dumps
    )


@pytest.mark.parametrize(
    "change",
    [
        {"warnings": []},
        {"warnings": ["page 4: resource_limit: candidate window truncated (limit=256)"]},
        {"page_warnings": [[4, "resource_limit: candidate window truncated (limit=256)"]]},
        {"pages": 19},
    ],
)
def test_reviewed_partial_rejects_changed_cutoff_dump(inputs, change):
    manifest, _, policy, provenance = reviewed_inputs(inputs)
    with pytest.raises(ValueError, match="mismatch"):
        allowances(manifest, policy, provenance, change)


def test_reviewed_partial_is_explicit_and_does_not_mutate_the_report(inputs):
    manifest, report, policy, provenance = reviewed_inputs(inputs)
    original = deepcopy(report)
    assert MODULE.validate(manifest, report, "dev", "pdfium")
    reviewed = allowances(manifest, policy, provenance)
    assert MODULE.validate(manifest, report, "dev", "pdfium", reviewed) == []
    assert report == original


@pytest.mark.parametrize(
    "change",
    [
        {"status": "failed"},
        {"status": "deferred"},
        {"pages": 19},
        {"pages": True},
        {"warnings": []},
        {"warnings": ["page 4: resource_limit: candidate window truncated (limit=256)"]},
        {"warnings": ["page 3: resource_limit: unreviewed cutoff"]},
    ],
)
def test_reviewed_partial_rejects_changed_outcome(inputs, change):
    manifest, report, policy, provenance = reviewed_inputs(inputs)
    reviewed = allowances(manifest, policy, provenance)
    report["papers"][0].update(change)
    assert MODULE.validate(manifest, report, "dev", "pdfium", reviewed)


def test_exceptions_do_not_disable_unknown_partial_or_coverage_checks(inputs):
    manifest, report, policy, provenance = reviewed_inputs(inputs)
    reviewed = allowances(manifest, policy, provenance)
    report["papers"][1]["status"] = "partial"
    assert MODULE.validate(manifest, report, "dev", "pdfium", reviewed)
    report["papers"] = report["papers"][:1] * 2
    errors = MODULE.validate(manifest, report, "dev", "pdfium", reviewed)
    assert "duplicate paper IDs: a" in errors
    assert "missing paper IDs: b" in errors


@pytest.mark.parametrize(
    "mutation", ["source", "manifest", "native", "backend", "host", "split", "policy_input"]
)
def test_reviewed_partial_requires_same_input_and_native_provenance(inputs, mutation):
    manifest, _, policy, provenance = reviewed_inputs(inputs)
    if mutation == "source":
        provenance["paper_inputs"]["a"]["pdf_sha256"] = "different"
    elif mutation == "policy_input":
        policy["papers"]["pdfium"]["a"]["inputs"] = {"pdf_sha256": "different"}
    elif mutation in ("manifest", "native"):
        field = "corpus_manifest_sha256" if mutation == "manifest" else "native_manifest_sha256"
        provenance[field] = "different"
    else:
        provenance[mutation] = "different"
    with pytest.raises(ValueError, match="mismatch"):
        allowances(manifest, policy, provenance)


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
    for paper in report["papers"]:
        paper.update(truth_refs=1, extracted_refs=1, matched_refs=1, warnings=0)
        paper["matches"] = [{"truth_key": "ref", "extracted_index": 1}]
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


def retained_dumps(report):
    return {
        paper["id"]: {
            "id": paper["id"],
            "pages": paper["pages"],
            "warnings": [],
            "page_warnings": [],
            "truth": [{"key": "ref"}],
            "extracted": [{"index": 1}],
            "matches": [{"truth_key": "ref", "extracted_index": 1}],
        }
        for paper in report["papers"]
    }


@pytest.mark.parametrize(
    "counts",
    [
        {"truth_refs": 0, "extracted_refs": 0, "matched_refs": 0},
        {"truth_refs": 36, "extracted_refs": 0, "matched_refs": 0},
        {"truth_refs": 36, "extracted_refs": 4, "matched_refs": 0},
        {"truth_refs": 1, "extracted_refs": 1, "matched_refs": 2},
        {"truth_refs": None},
        {"extracted_refs": -1},
        {"matched_refs": True},
    ],
)
def test_complete_status_does_not_excuse_zero_or_invalid_reference_counts(inputs, counts):
    manifest, report = inputs
    report["papers"][0].update(counts)
    assert any("reference" in error for error in MODULE.validate(manifest, report, "dev", "pdfium"))


@pytest.mark.parametrize("status", ["partial", "complete"])
def test_reviewed_outcomes_reject_reference_regression_but_allow_improvement(inputs, status):
    manifest, report, policy, provenance = reviewed_inputs(inputs)
    baseline = policy["papers"]["pdfium"]["a"]["reference_baseline"]
    baseline.update(truth_refs=3, matched_refs=2, spurious_refs=1)
    reviewed = allowances(manifest, policy, provenance)
    row = report["papers"][0]
    row.update(status=status, truth_refs=3, extracted_refs=3, matched_refs=3)
    assert MODULE.validate(manifest, report, "dev", "pdfium", reviewed) == []
    row["matched_refs"] = 1
    assert "reference regression" in "\n".join(
        MODULE.validate(manifest, report, "dev", "pdfium", reviewed)
    )
    row.update(matched_refs=2, extracted_refs=4)
    assert "reference regression" in "\n".join(
        MODULE.validate(manifest, report, "dev", "pdfium", reviewed)
    )


def test_same_cutoff_cannot_hide_changed_ordinary_diagnostic(inputs):
    manifest, _, policy, provenance = reviewed_inputs(inputs)
    with pytest.raises(ValueError, match="diagnostics mismatch"):
        allowances(
            manifest,
            policy,
            provenance,
            {
                "page_warnings": policy["papers"]["pdfium"]["a"]["resource_page_warnings"]
                + [[3, "new mapping uncertainty"]]
            },
        )


def test_reviewed_partial_can_become_complete_only_with_cutoff_removed(inputs):
    manifest, report, policy, provenance = reviewed_inputs(inputs)
    row = report["papers"][0]
    row.update(status="complete", warnings=0)
    dumps = retained_dumps(report)
    reviewed = MODULE.load_reviewed_partials(
        manifest, "pinned", policy, provenance, "dev", "pdfium", dumps, set()
    )
    assert MODULE.validate_dumps(report, dumps) == []
    assert MODULE.validate(manifest, report, "dev", "pdfium", reviewed) == []
    dumps["a"]["page_warnings"] = deepcopy(
        policy["papers"]["pdfium"]["a"]["resource_page_warnings"]
    )
    assert "Complete hides a cutoff" in "\n".join(MODULE.validate_dumps(report, dumps))


@pytest.mark.parametrize(
    "change",
    [
        {"id": "wrong"},
        {"pages": True},
        {"pages": 19},
        {"warnings": "none"},
        {"page_warnings": None},
        {"page_warnings": [[0, "bad page"]]},
        {"page_warnings": [[21, "bad page"]]},
        {"page_warnings": [[True, "bad page"]]},
        {"page_warnings": [[1, "failed: page could not be read"]]},
        {"page_warnings": [[1, "resource_limit: unreviewed work cutoff"]]},
        {"warnings": ["page 1: resource_limit: unreviewed work cutoff"]},
        {"truth": None},
        {"truth": [{"key": "ref"}, {"key": "ref"}]},
        {"extracted": []},
        {"extracted": [{"index": True}]},
        {"extracted": [{"index": 1}, {"index": 1}]},
        {"matches": []},
        {"matches": [{"truth_key": "wrong", "extracted_index": 1}]},
        {"matches": [{"truth_key": "ref", "extracted_index": 2}]},
        {"matches": [{"truth_key": "ref", "extracted_index": True}]},
        {"matches": [{"truth_key": "ref"}]},
    ],
)
def test_retained_dump_evidence_rejects_inconsistent_complete_results(inputs, change):
    _, report = inputs
    dumps = retained_dumps(report)
    assert MODULE.validate_dumps(report, dumps) == []
    dumps["a"].update(change)
    assert MODULE.validate_dumps(report, dumps)


@pytest.mark.parametrize("field", ["truth_refs", "extracted_refs", "matched_refs", "warnings"])
def test_report_counts_must_match_retained_evidence(inputs, field):
    _, report = inputs
    dumps = retained_dumps(report)
    report["papers"][0][field] = 2
    assert "count mismatch" in "\n".join(MODULE.validate_dumps(report, dumps))


def test_zero_report_counts_cannot_hide_positive_retained_truth(inputs):
    _, report = inputs
    dumps = retained_dumps(report)
    report["papers"][0].update(truth_refs=0, extracted_refs=0, matched_refs=0)
    dumps["a"]["extracted"] = []
    dumps["a"]["matches"][0]["extracted_index"] = None
    report["papers"][0]["matches"][0]["extracted_index"] = None
    assert MODULE.validate_dumps(report, dumps) == ["dump reference count mismatch: a truth_refs"]


def test_complete_promotion_cannot_reduce_known_page_count(inputs):
    manifest, report, policy, provenance = reviewed_inputs(inputs)
    row = report["papers"][0]
    row.update(status="complete", pages=1, warnings=0)
    dumps = retained_dumps(report)
    reviewed = MODULE.load_reviewed_partials(
        manifest, "pinned", policy, provenance, "dev", "pdfium", dumps, set()
    )
    assert MODULE.validate_dumps(report, dumps) == []
    assert "reviewed input page count mismatch: a" in MODULE.validate(
        manifest, report, "dev", "pdfium", reviewed
    )


def test_report_matches_must_agree_with_dump(inputs):
    _, report = inputs
    dumps = retained_dumps(report)
    report["papers"][0]["matches"][0]["method"] = "fabricated"
    assert MODULE.validate_dumps(report, dumps) == ["dump reference matches mismatch: a"]


def test_two_truth_entries_cannot_claim_the_same_extracted_reference(inputs):
    _, report = inputs
    dumps = retained_dumps(report)
    dumps["a"]["truth"].append({"key": "second"})
    dumps["a"]["matches"].append({"truth_key": "second", "extracted_index": 1})
    assert MODULE.validate_dumps(report, dumps) == ["invalid reference alignment: a"]


def test_duplicate_source_truth_keys_preserve_match_multiplicity(inputs):
    _, report = inputs
    dumps = retained_dumps(report)
    dumps["a"]["truth"].append({"key": "ref"})
    dumps["a"]["matches"].append({"truth_key": "ref", "extracted_index": None})
    report["papers"][0].update(truth_refs=2, matches=deepcopy(dumps["a"]["matches"]))
    assert MODULE.validate_dumps(report, dumps) == []


def test_cli_uses_complete_retained_evidence_for_reviewed_partial(
    tmp_path, inputs, capsys, monkeypatch
):
    manifest, report, policy, provenance = reviewed_inputs(inputs)
    manifest_path = tmp_path / "manifest.json"
    manifest_path.write_text(json.dumps(manifest))
    manifest_hash = MODULE.hashlib.sha256(manifest_path.read_bytes()).hexdigest()
    policy["provenance_pins"]["corpus_manifest_sha256"] = manifest_hash
    provenance["corpus_manifest_sha256"] = manifest_hash
    provenance["git_commit"] = "recorded-source-head"
    expected = {**policy["provenance_pins"], "git_commit": provenance["git_commit"]}
    monkeypatch.setattr(MODULE, "source_provenance", lambda *_: expected)
    dumps = retained_dumps(report)
    outcome = policy["papers"]["pdfium"]["a"]
    dumps["a"].update(warnings=outcome["warnings"], page_warnings=outcome["resource_page_warnings"])
    dump_dir = tmp_path / "dumps"
    dump_dir.mkdir()
    for paper_id, dump in dumps.items():
        (dump_dir / MODULE.dump_name(paper_id)).write_text(json.dumps(dump))
    for name, value in [("report", report), ("policy", policy), ("provenance", provenance)]:
        (tmp_path / f"{name}.json").write_text(json.dumps(value))
    argv = [
        "--manifest",
        str(manifest_path),
        "--split",
        "dev",
        "--backend",
        "pdfium",
        "--report",
        str(tmp_path / "report.json"),
        "--reviewed-partials",
        str(tmp_path / "policy.json"),
        "--provenance",
        str(tmp_path / "provenance.json"),
        "--dumps",
        str(dump_dir),
    ]
    assert MODULE.main(argv) == 0
    assert "1 complete, 1 reviewed partial" in capsys.readouterr().out
    provenance["git_commit"] = "wrong-source-head"
    (tmp_path / "provenance.json").write_text(json.dumps(provenance))
    assert MODULE.main(argv) == 1
    assert "current-source provenance mismatch: git_commit" in capsys.readouterr().err
    provenance["git_commit"] = expected["git_commit"]
    provenance["cargo_lock_sha256"] = expected["cargo_lock_sha256"] = "b" * 64
    (tmp_path / "provenance.json").write_text(json.dumps(provenance))
    assert MODULE.main(argv) == 1
    errors = capsys.readouterr().err
    assert "zero exceptions applied" in errors
    assert "incomplete paper a: status 'partial'" in errors
    provenance["cargo_lock_sha256"] = expected["cargo_lock_sha256"] = "a" * 64
    (tmp_path / "provenance.json").write_text(json.dumps(provenance))
    dumps["a"]["page_warnings"][0][0] = 4
    (dump_dir / "a.json").write_text(json.dumps(dumps["a"]))
    assert MODULE.main(argv) == 1
    assert "mismatch" in capsys.readouterr().err
    (dump_dir / "b.json").unlink()
    assert MODULE.main(argv) == 1


@pytest.mark.parametrize("paper_id", ["../a", "a/b", "a\\b", "", None])
def test_dump_paths_reject_non_identifier_input(paper_id):
    with pytest.raises(ValueError, match="invalid paper ID"):
        MODULE.dump_name(paper_id)


@pytest.mark.parametrize("failed_backend", ["lopdf", "pdfium", "docling-text", "docling", ""])
def test_workflow_runs_every_validator_and_aggregates_failure(tmp_path, failed_backend):
    bash = shutil.which("bash")
    if not bash:
        pytest.skip("bash is required to exercise the Native workflow shell")
    workflow = (Path(__file__).parents[1] / ".github/workflows/native.yml").read_text()
    section = workflow.split("- name: Validate coverage and bounded outcomes for every backend", 1)[
        1
    ]
    script = textwrap.dedent(section.split("\n      - ", 1)[0].split("run: |\n", 1)[1])
    log = tmp_path / "calls"
    stub = """
uv() {
  while [ "$#" -gt 0 ]; do
    if [ "$1" = "--backend" ]; then backend="$2"; break; fi
    shift
  done
  echo "$backend" >> "$VALIDATOR_CALL_LOG"
  [ "$backend" != "$VALIDATOR_FAIL_BACKEND" ]
}
"""
    completed = subprocess.run(  # noqa: S603 - executes only the checked-in workflow and test stub
        [bash, "-e", "-c", stub + script],
        env={
            **os.environ,
            "VALIDATOR_CALL_LOG": str(log),
            "VALIDATOR_FAIL_BACKEND": failed_backend,
        },
        capture_output=True,
        timeout=10,
        check=False,
    )
    assert log.read_text().splitlines() == ["lopdf", "pdfium", "docling-text", "docling"]
    assert completed.returncode == (1 if failed_backend else 0)


@pytest.mark.parametrize(
    "field", [*MODULE.PROVENANCE_FILES, "corpus_manifest_sha256", "git_commit"]
)
def test_current_provenance_rejects_wrong_checkout_or_input(inputs, field):
    _, _, policy, provenance = reviewed_inputs(inputs)
    expected = {**policy["provenance_pins"], "git_commit": "intended-source"}
    provenance["git_commit"] = expected["git_commit"]
    assert MODULE.verify_current_provenance(provenance, expected) == policy["provenance_pins"]
    provenance[field] = "wrong-artifact-input"
    with pytest.raises(ValueError, match=f"current-source provenance mismatch: {field}"):
        MODULE.verify_current_provenance(provenance, expected)


def changed_dependency_outcomes(inputs):
    manifest, report, policy, provenance = reviewed_inputs(inputs)
    unchanged_policy = deepcopy(policy)
    provenance["cargo_lock_sha256"] = "b" * 64
    current_pins = {**policy["provenance_pins"], "cargo_lock_sha256": "b" * 64}
    reviewed = MODULE.load_reviewed_partials(
        manifest,
        "pinned",
        policy,
        provenance,
        "dev",
        "pdfium",
        retained_dumps(report),
        {"a"},
        current_pins,
    )
    assert policy == unchanged_policy
    assert reviewed["a"]["partial_eligible"] is False
    return manifest, report, reviewed


def test_verified_dependency_change_never_grants_historical_partial_exception(inputs):
    manifest, report, reviewed = changed_dependency_outcomes(inputs)
    # Counts and warning totals happen to match the old policy; this must not
    # make a Partial result eligible under a different dependency graph.
    errors = MODULE.validate(manifest, report, "dev", "pdfium", reviewed)
    assert errors == ["incomplete paper a: status 'partial'"]


@pytest.mark.parametrize("status", ["partial", "complete"])
def test_changed_dependency_keeps_same_input_page_and_reference_baselines(inputs, status):
    manifest, report, reviewed = changed_dependency_outcomes(inputs)
    paper = report["papers"][0]
    paper.update(status=status, pages=19, truth_refs=1, extracted_refs=1, matched_refs=0)
    errors = MODULE.validate(manifest, report, "dev", "pdfium", reviewed)
    assert "reviewed input page count mismatch: a" in errors
    assert "reviewed-partial reference regression: a" in errors
    assert any("reference extraction failed for a" in error for error in errors)


@pytest.mark.parametrize(
    "field", ["native_manifest_sha256", "metric_source_sha256", "truth_source_sha256"]
)
def test_verified_dependency_change_does_not_accept_other_historical_pin_drift(inputs, field):
    manifest, _, policy, provenance = reviewed_inputs(inputs)
    provenance.update(cargo_lock_sha256="b" * 64, **{field: "new-other-input"})
    current_pins = {key: provenance[key] for key in policy["provenance_pins"]}
    with pytest.raises(ValueError, match=f"reviewed-partial provenance mismatch: {field}"):
        MODULE.load_reviewed_partials(
            manifest, "pinned", policy, provenance, "dev", "pdfium", {}, {"a"}, current_pins
        )


def test_current_pin_assertion_cannot_hide_an_unverified_dependency_change(inputs):
    manifest, _, policy, provenance = reviewed_inputs(inputs)
    provenance["cargo_lock_sha256"] = "unverified-lock"
    with pytest.raises(ValueError, match="current-source provenance pins"):
        MODULE.load_reviewed_partials(
            manifest,
            "pinned",
            policy,
            provenance,
            "dev",
            "pdfium",
            {},
            {"a"},
            policy["provenance_pins"],
        )


@pytest.mark.parametrize("status", ["partial", "deferred", "failed", "failed: parser"])
@pytest.mark.parametrize("count", [0, -1, None, True])
def test_noncomplete_status_never_masks_zero_or_malformed_reference_counts(inputs, status, count):
    manifest, report = deepcopy(inputs)
    report["papers"][0].update(status=status, matched_refs=count)
    errors = MODULE.validate(manifest, report, "dev", "pdfium")
    assert any("incomplete paper a" in error for error in errors)
    expected = (
        "reference extraction failed"
        if type(count) is int and count == 0
        else "invalid reference counts"
    )
    assert any(expected in error for error in errors)


def test_source_checkout_provenance_reads_real_inputs_and_rejects_dirty_source(tmp_path):
    files = [*MODULE.PROVENANCE_FILES.values(), "corpus/manifest.json", "src/reading_order.rs"]
    for name in files:
        path = tmp_path / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(f"fixture {name}\n")
    for args in [
        ["init", "--quiet"],
        ["add", "."],
        [
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "fixture",
        ],
    ]:
        subprocess.run(["git", "-C", str(tmp_path), *args], check=True)  # noqa: S603,S607 -- isolated fixture, fixed argv
    expected = MODULE.source_provenance(tmp_path, tmp_path / "corpus/manifest.json")
    assert len(expected["git_commit"]) == 40
    assert (
        expected["cargo_lock_sha256"]
        == MODULE.hashlib.sha256((tmp_path / "Cargo.lock").read_bytes()).hexdigest()
    )
    assert (
        MODULE.verify_current_provenance(expected, expected)["cargo_lock_sha256"]
        == expected["cargo_lock_sha256"]
    )
    (tmp_path / "src/reading_order.rs").write_text("uncommitted source drift\n")
    with pytest.raises(subprocess.CalledProcessError):
        MODULE.source_provenance(tmp_path, tmp_path / "corpus/manifest.json")


@pytest.mark.parametrize("malformed", [None, "", "different", "g" * 64, 123, []])
def test_dependency_change_does_not_hide_malformed_historical_pin(inputs, malformed):
    manifest, _, policy, provenance = reviewed_inputs(inputs)
    current_pins = {**policy["provenance_pins"], "cargo_lock_sha256": "b" * 64}
    provenance.update(current_pins)
    policy["provenance_pins"]["cargo_lock_sha256"] = malformed
    with pytest.raises(ValueError, match="invalid reviewed-partial dependency pin"):
        MODULE.load_reviewed_partials(
            manifest, "pinned", policy, provenance, "dev", "pdfium", {}, {"a"}, current_pins
        )


def test_dependency_change_does_not_hide_missing_historical_pin(inputs):
    manifest, _, policy, provenance = reviewed_inputs(inputs)
    current_pins = {**policy["provenance_pins"], "cargo_lock_sha256": "b" * 64}
    provenance.update(current_pins)
    del policy["provenance_pins"]["cargo_lock_sha256"]
    with pytest.raises(ValueError, match="provenance pins are incomplete"):
        MODULE.load_reviewed_partials(
            manifest, "pinned", policy, provenance, "dev", "pdfium", {}, {"a"}, current_pins
        )
