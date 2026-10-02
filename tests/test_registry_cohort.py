"""Independent identifier scoring must not reward service agreement or drop failures."""

import copy
import importlib
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
cohort = importlib.import_module("registry_cohort")


def positive(doi="10.1234/truth"):
    return {"kind": "positive", "identifiers": {"doi": doi}}


def test_wrong_doi_is_wrong_even_when_other_fields_and_registry_claims_agree():
    resolved = {"doi": "10.1234/wrong", "title": "Expected title", "score": 1.0}
    assert cohort.classify(positive(), resolved) == "wrong_identifier"
    assert cohort.classify(positive(), {"doi": "https://doi.org/10.1234/TRUTH"}) == "correct"
    assert cohort.classify(positive(), {"pmid": "1234"}) == "unverifiable_accept"


def test_conflicting_cross_identifier_is_not_credit_for_one_matching_identifier():
    expected = {"kind": "positive", "identifiers": {"doi": "10.1234/a", "pmid": "123"}}
    assert cohort.classify(expected, {"doi": "10.1234/a", "pmid": "456"}) == "wrong_identifier"
    assert cohort.classify(expected, {"doi": "10.1234/a"}) == "correct"


def test_controls_and_ambiguous_inputs_do_not_inflate_positive_precision():
    assert cohort.classify({"kind": "must_abstain"}, None) == "abstained"
    assert cohort.classify({"kind": "must_abstain"}, {"doi": "10.1234/a"}) == "false_accept"
    expected = {"kind": "ambiguous", "alternatives": ["10.1234/a", "10.1234/b"]}
    assert cohort.classify(expected, {"doi": "10.1234/a"}) == "accepted_ambiguous_alternative"
    assert cohort.classify(expected, {"doi": "10.1234/c"}) == "wrong_ambiguous_accept"
    assert cohort.classify(expected, None) == "withheld_ambiguous_input"


def small_cohort():
    return {
        "source": {},
        "source_population": {},
        "acquisition": [],
        "biomedical_source_goal": 2,
        "biomedical_source_count": 1,
        "cases": [
            {
                "id": str(index),
                "source_pmcid": "PMC123",
                "group": "natural",
                "input_kind": "unchanged_extracted",
                "expected": positive(),
            }
            for index in range(3)
        ],
    }


def test_unrun_and_request_error_cases_stay_in_planned_coverage_denominator():
    records = [
        {
            "type": "case",
            "case_id": "0",
            "entry": {"resolved": {"doi": "10.1234/truth"}, "attempts": []},
        },
        {
            "type": "case",
            "case_id": "1",
            "entry": {"resolved": None, "attempts": [{"method": "query", "outcome": "error"}]},
        },
    ]
    report = cohort.score(small_cohort(), records, [])
    assert report["natural_positive_metrics"]["correct_coverage"] == {
        "numerator": 1,
        "denominator": 3,
    }
    assert report["natural_positive_metrics"]["not_run"] == 1
    assert report["natural_positive_metrics"]["unresolved_with_error"] == 1
    assert report["groups"]["natural"]["request_error_cases"] == 1
    assert report["biomedical_source_goal"] == 2
    assert report["biomedical_source_count"] == 1


def test_error_only_abstention_is_not_reported_as_successful_verification():
    data = small_cohort()
    data["cases"][0]["expected"] = {"kind": "must_abstain"}
    records = [
        {
            "type": "case",
            "case_id": "0",
            "entry": {"resolved": None, "attempts": [{"method": "query", "outcome": "error"}]},
        }
    ]
    report = cohort.score(data, records, [])
    assert report["cases"][0]["classification"] == "abstained_after_error"


def test_duplicate_case_results_are_visible_as_input_errors():
    record = {"type": "case", "case_id": "0", "entry": {"resolved": None, "attempts": []}}
    report = cohort.score(small_cohort(), [record, copy.deepcopy(record)], [])
    assert report["input_errors"] == ["duplicate result: 0"]


def test_false_accept_and_incomplete_cohort_fail_the_live_evidence_check():
    data = small_cohort()
    data["cases"][0]["expected"] = {"kind": "must_abstain"}
    record = {
        "type": "case",
        "case_id": "0",
        "entry": {"resolved": {"doi": "10.1234/truth"}, "attempts": []},
    }
    report = cohort.score(data, [record], [])
    failures = cohort.evidence_failures(report)
    assert "not every planned resolver case ran" in failures
    assert "some planned biomedical JATS labels are unavailable" in failures
    assert "natural: false_accept=1" in failures


def test_independent_identifier_field_coverage_includes_missing_and_wrong_values():
    data = small_cohort()
    data["cases"][0]["expected"]["identifiers"]["pmid"] = "1234"
    record = {
        "type": "case",
        "case_id": "0",
        "entry": {"resolved": {"doi": "10.1234/truth", "pmid": "4321"}, "attempts": []},
    }
    report = cohort.score(data, [record], [])
    assert report["independently_labeled_identifier_metrics"]["doi"] == {
        "planned": 3,
        "correct": 1,
        "missing": 2,
    }
    assert report["independently_labeled_identifier_metrics"]["pmid"] == {
        "planned": 1,
        "incorrect": 1,
    }


def test_verified_label_ineligibility_is_distinct_from_failed_acquisition():
    report = cohort.score(small_cohort(), [], [])
    report["biomedical_source_goal"] = 24
    report["biomedical_source_count"] = 18
    report["biomedical_label_exclusions"] = [{"pmcid": str(index)} for index in range(6)]
    assert "some planned biomedical JATS labels are unavailable" not in cohort.evidence_failures(
        report
    )
    report["acquisition"] = [{"pmcid": "PMC123", "error": "checksum mismatch"}]
    assert "publisher XML acquisition or verification failed" in cohort.evidence_failures(report)


def test_biomedical_labels_come_from_structured_jats_not_concatenated_numbers():
    xml = b"""<article><back><ref-list><ref><element-citation>
    <name><surname>Smith</surname></name><article-title>Independent study</article-title>
    <year>2020</year>
    <pub-id pub-id-type="doi">10.1234/STUDY</pub-id><pub-id pub-id-type="pmid">123456</pub-id>
    <pub-id pub-id-type="pmcid">PMC765432</pub-id></element-citation></ref>
    <ref><mixed-citation>Smith 2020 10.1234/study PMC765432123456</mixed-citation></ref>
    </ref-list></back></article>"""
    refs = cohort.biomedical_truth(xml)
    assert len(refs) == 1
    assert refs[0]["identifiers"] == {
        "doi": "10.1234/study",
        "pmid": "123456",
        "pmcid": "PMC765432",
    }


def test_frozen_selection_is_broad_and_keeps_untouched_inputs_and_exclusions():
    selection = json.loads(
        (Path(__file__).resolve().parents[1] / "corpus/registry-selection.json").read_bytes()
    )
    natural = selection["natural_cases"]
    assert len(natural) == 240
    assert len({case["source_pmcid"] for case in natural}) == 178
    assert len({case["publisher"] for case in natural}) == 42
    assert len(selection["source_population"]["exclusions"]) == 22
    assert all(case["input_kind"] == "unchanged_extracted" for case in natural)
    assert all(
        case["entry"]["resolved"] is None and case["entry"]["attempts"] == [] for case in natural
    )
    assert all(case["expected"]["identifiers"]["doi"] == case["truth"]["doi"] for case in natural)
