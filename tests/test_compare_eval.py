from copy import deepcopy

import pytest

from text_processing_engine.compare_eval import compare


def paper(identity, elapsed=100):
    return {
        "id": identity,
        "status": "complete",
        "pages": 40,
        "chunks": 2,
        "ms_total": elapsed,
        "ms_per_chunk": elapsed / 2,
        "timings": {
            "acquire_ms": elapsed / 2,
            "parse_ms": elapsed / 2,
            "order_ms": 0,
            "metadata_ms": 0,
            "citations_ms": 0,
            "write_ms": 0,
            "hash_ms": elapsed / 4,
        },
        "truth_method": "bbl",
        "truth_refs": 10,
        "extracted_refs": 10,
        "matched_refs": 10,
        "body_words_truth": 100,
        "body_alignment": 0.8,
    }


def report(*papers):
    return {"backend": "pdfium", "host": "linux-aarch64", "papers": list(papers)}


def test_matches_ids_not_changed_corpus_summary():
    before = report(paper("a", 100), paper("b", 300))
    after = report(paper("b", 200), paper("new", 10000), paper("a", 80))
    before["summary"] = after["summary"] = {"mean_parse_ms": -999}
    result = compare(before, after)
    assert result["corpus"]["added_ids"] == ["new"]
    assert result["corpus"]["matched_timing_ids"] == ["a", "b"]
    assert result["timing"]["before"]["mean_parse_ms_per_document"] == 100
    assert result["timing"]["after"]["p50_nominal_ms_per_chunk"] == 40
    assert result["timing"]["after"]["p95_nominal_ms_per_chunk"] == 100


def test_failures_and_page_changes_are_visible_not_fast_zeroes():
    old = report(paper("failed"), paper("changed"), paper("removed"))
    new = deepcopy(old)
    new["papers"].pop()
    new["papers"][0]["status"] = "failed:parse"
    new["papers"][0]["ms_per_chunk"] = 0
    new["papers"][1]["pages"] = 39
    result = compare(old, new)
    assert result["corpus"]["removed_ids"] == ["removed"]
    assert result["corpus"]["failed_after_ids"] == ["failed"]
    assert len(result["corpus"]["excluded_from_timing"]) == 2
    assert result["timing"]["after"]["p50_nominal_ms_per_chunk"] is None


def test_accuracy_decreases_do_not_claim_confirmed_regression():
    old = report(paper("a"), paper("b"))
    new = deepcopy(old)
    new["papers"][0]["body_alignment"] = 0.7
    new["papers"][1]["truth_refs"] = 20
    result = compare(old, new)
    deltas = result["observed_score_changes"]
    assert deltas[0]["metric"] == "body_alignment"
    assert deltas[0]["direction"] == "decrease"
    assert deltas[0]["interpretation"] == "provenance unverified"
    assert deltas[1]["interpretation"] == "truth indicators changed"
    assert result["truth_indicator_changes"] == [{"id": "b", "changed_fields": ["truth_refs"]}]


def test_doi_regression_is_visible_when_printed_coverage_disappears():
    old = report(paper("a"))
    old["papers"][0].update(doi_correct=1, doi_truth=1, doi_printed=1)
    new = deepcopy(old)
    new["papers"][0].update(doi_correct=0, doi_printed=0)
    changes = compare(old, new)["observed_score_changes"]
    assert len(changes) == 1
    assert changes[0]["metric"] == "doi_accuracy"
    assert changes[0]["before"] == 1.0
    assert changes[0]["after"] == 0.0
    assert changes[0]["direction"] == "decrease"


@pytest.mark.parametrize("value", [float("nan"), float("inf"), -1, "100", True])
def test_invalid_timings_rejected(value):
    old = report(paper("a"))
    new = deepcopy(old)
    new["papers"][0]["ms_total"] = value
    with pytest.raises(ValueError):
        compare(old, new)


def test_metric_definition_mismatch_rejected():
    old = report(paper("a"))
    new = deepcopy(old)
    new["papers"][0]["ms_per_chunk"] = 99
    with pytest.raises(ValueError, match="incompatible ms_per_chunk"):
        compare(old, new)


def test_inconsistent_total_is_rejected_even_when_per_chunk_quotient_matches():
    old = report(paper("a", 10))
    new = deepcopy(old)
    new["papers"][0]["timings"]["parse_ms"] = 1000
    with pytest.raises(ValueError, match="ms_total differs from sequential stage sum"):
        compare(old, new)


def test_overlapping_hash_timing_is_excluded_from_stage_total():
    old = report(paper("a", 100))
    new = deepcopy(old)
    new["papers"][0]["timings"]["hash_ms"] = 1000
    result = compare(old, new)
    assert result["timing"]["before"] == result["timing"]["after"]
    assert result["timing"]["after"]["p50_nominal_ms_per_chunk"] == 50


@pytest.mark.parametrize("value", [float("nan"), float("inf"), -1, "100", True])
def test_invalid_component_stage_timings_rejected(value):
    old = report(paper("a"))
    new = deepcopy(old)
    new["papers"][0]["timings"]["metadata_ms"] = value
    with pytest.raises(ValueError, match="metadata_ms"):
        compare(old, new)


def test_duplicate_ids_and_disjoint_sets_rejected():
    with pytest.raises(ValueError, match="duplicate"):
        compare(report(paper("a"), paper("a")), report(paper("a")))
    with pytest.raises(ValueError, match="no paper IDs"):
        compare(report(paper("a")), report(paper("b")))


def test_host_and_backend_changes_are_explicit():
    old = report(paper("a"))
    new = deepcopy(old)
    new.update(host="macos-aarch64", backend="lopdf")
    warnings = compare(old, new)["warnings"]
    assert any("Host changed" in warning for warning in warnings)
    assert any("Backend changed" in warning for warning in warnings)


def test_missing_metrics_are_not_invented_as_zero():
    old = report(paper("a"))
    new = deepcopy(old)
    del new["papers"][0]["body_alignment"]
    assert compare(old, new)["observed_score_changes"] == []


def provenance(**overrides):
    return {
        "schema_version": 1,
        "backend": "pdfium",
        "host": "linux-aarch64",
        "timing_definition": "summed_stages_per_nominal_20_page_chunk_v1",
        "metric_source_sha256": "old-scorer",
        **overrides,
    }


def test_provenance_scorer_changes_and_missing_hashes_flagged():
    sample = report(paper("a"))
    result = compare(sample, sample, provenance(), provenance(metric_source_sha256="new-scorer"))
    assert any("metric_source_sha256 changed" in warning for warning in result["warnings"])
    assert any("corpus_manifest_sha256 missing" in warning for warning in result["warnings"])


@pytest.mark.parametrize(
    "overrides",
    [
        {"timing_definition": "measured_chunk_latency"},
        {"backend": "lopdf"},
        {"chunk_pages": 10},
        {"schema_version": 2},
    ],
)
def test_incompatible_or_mismatched_provenance_rejected(overrides):
    sample = report(paper("a"))
    with pytest.raises(ValueError):
        compare(sample, sample, provenance(**overrides), None)


@pytest.mark.parametrize("status", ["partial", "deferred", "ok", "unknown", "failed:parse"])
def test_only_complete_papers_enter_timing_or_accuracy_comparisons(status):
    old = report(paper("a"))
    new = deepcopy(old)
    new["papers"][0].update(status=status, body_alignment=0.1)
    result = compare(old, new)
    assert result["timing"]["after"]["papers"] == 0
    assert result["corpus"]["excluded_from_timing"][0]["after_status"] == status
    assert result["observed_score_changes"] == []


def test_malformed_status_rejected():
    old = report(paper("a"))
    old["papers"][0]["status"] = None
    with pytest.raises(ValueError, match="status must be a string"):
        compare(old, old)


def test_legacy_title_accuracy_uses_unadjusted_denominator():
    old = report(paper("a"))
    old["papers"][0].update(title_truth=10, title_correct=8)
    new = deepcopy(old)
    new["papers"][0]["title_correct"] = 7
    change = compare(old, new)["observed_score_changes"][0]
    assert change["metric"] == "title_accuracy"
    assert change["before"] == 0.8
    assert change["after"] == 0.7


def test_changed_title_definition_warns_instead_of_comparing():
    old = report(paper("a"))
    old["papers"][0].update(title_truth=10, title_correct=8)
    new = deepcopy(old)
    new["papers"][0].update(title_not_applicable=2)
    result = compare(old, new)
    assert result["observed_score_changes"] == []
    assert any("title accuracy denominator definition changed" in w for w in result["warnings"])
