"""Compare matched papers in two tpe eval reports without trusting summaries."""

import argparse
import json
import math
from pathlib import Path
from statistics import mean

TRUTH_FIELDS = (
    "truth_method",
    "truth_refs",
    "doi_truth_total",
    "year_truth_total",
    "title_truth_total",
    "truth_cite_commands",
    "truth_cited_keys",
    "marker_keys_cited",
    "authors_truth",
    "body_words_truth",
)
RATIOS = {
    "ref_recall": ("matched_refs", "truth_refs"),
    "ref_precision": ("matched_refs", "extracted_refs"),
    "doi_accuracy_printed": ("doi_correct", "doi_printed"),
    "year_accuracy": ("year_correct", "year_truth"),
    "marker_precision": ("marker_targets_correct", "resolved_targets"),
    "marker_key_recall": ("marker_keys_hit", "marker_keys_cited"),
}


def number(value, label):
    """Reject malformed measurements instead of silently treating them as zero."""
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise ValueError(f"{label} must be numeric")
    if not math.isfinite(value) or value < 0:
        raise ValueError(f"{label} must be finite and nonnegative")
    return value


def index(report):
    papers = {}
    for paper in report["papers"]:
        identity = paper["id"]
        if not isinstance(paper.get("status"), str):
            raise ValueError(f"{identity}: status must be a string")
        if identity in papers:
            raise ValueError(f"duplicate paper id: {identity}")
        papers[identity] = paper
    return papers


def failed(paper):
    return paper["status"].startswith("failed")


def timing(paper):
    """Validate the current schema's stage-total / nominal-chunk definition."""
    pages = number(paper["pages"], "pages")
    chunks = number(paper["chunks"], "chunks")
    if pages < 1 or pages != int(pages) or chunks != math.ceil(pages / 20):
        raise ValueError(f"{paper['id']}: expected nominal 20-page chunks")
    total = number(paper["ms_total"], "ms_total")
    per_chunk = number(paper["ms_per_chunk"], "ms_per_chunk")
    parse = number(paper["timings"]["parse_ms"], "parse_ms")
    if not math.isclose(per_chunk, total / chunks, rel_tol=1e-6, abs_tol=1e-6):
        raise ValueError(f"{paper['id']}: incompatible ms_per_chunk definition")
    return parse, per_chunk


def percentile(values, fraction):
    return sorted(values)[math.ceil(len(values) * fraction) - 1] if values else None


def timing_summary(papers):
    values = [timing(paper) for paper in papers]
    chunks = [value[1] for value in values]
    return {
        "papers": len(values),
        "mean_parse_ms_per_document": mean(value[0] for value in values) if values else None,
        "p50_nominal_ms_per_chunk": percentile(chunks, 0.50),
        "p95_nominal_ms_per_chunk": percentile(chunks, 0.95),
    }


def ratio(numerator, denominator):
    return numerator / denominator if denominator else None


def scores(paper):
    result = {}
    for name, (numerator, denominator) in RATIOS.items():
        if numerator in paper and denominator in paper:
            result[name] = ratio(
                number(paper[numerator], numerator), number(paper[denominator], denominator)
            )
    if all(key in paper for key in ("title_correct", "title_truth")):
        denominator = number(paper["title_truth"], "title_truth") - number(
            paper.get("title_not_applicable", 0), "title_not_applicable"
        )
        if denominator < 0:
            raise ValueError("title_not_applicable exceeds title_truth")
        result["title_accuracy"] = ratio(
            number(paper["title_correct"], "title_correct"), denominator
        )
    if paper.get("body_alignment") is not None:
        result["body_alignment"] = number(paper["body_alignment"], "body_alignment")
    return result


def compare(before, after, before_provenance=None, after_provenance=None):
    """Return observational deltas, never an automatic merge/accuracy verdict."""
    old, new = index(before), index(after)
    common = sorted(old.keys() & new.keys())
    if not common:
        raise ValueError("reports have no paper IDs in common")
    warnings = [
        "IDs are matched, but these reports do not establish identical PDF/source/truth hashes "
        "or scorer versions. Score changes are observations, not confirmed extraction regressions.",
        "Timing is summed pipeline stage time divided by nominal 20-page chunks per document. "
        "Percentiles are over documents, not measured streaming chunk latency; "
        "the 30 ms warm-service target cannot be accepted from these timings.",
        "Host labels do not establish identical hardware, load, build flags, "
        "or warm-up conditions.",
    ]
    sidecars = {"before": before_provenance, "after": after_provenance}
    for label, metadata in sidecars.items():
        if metadata is None:
            continue
        if metadata.get("schema_version") != 1:
            raise ValueError(f"{label}: unsupported provenance schema")
        if metadata.get("timing_definition") != "summed_stages_per_nominal_20_page_chunk_v1":
            raise ValueError(f"{label}: incompatible provenance timing definition")
        if metadata.get("chunk_pages", 20) != 20:
            raise ValueError(f"{label}: incompatible chunk size")
        report = before if label == "before" else after
        for field in ("backend", "host"):
            if metadata.get(field) != report.get(field):
                raise ValueError(f"{label}: provenance {field} does not match report")
    if all(metadata is not None for metadata in sidecars.values()):
        for field in (
            "metric_source_sha256",
            "truth_source_sha256",
            "cargo_lock_sha256",
            "corpus_manifest_sha256",
            "native_manifest_sha256",
            "split",
        ):
            previous, current = before_provenance.get(field), after_provenance.get(field)
            if previous is None or current is None:
                warnings.append(f"Provenance {field} missing: compatibility unverified.")
            elif previous != current:
                warnings.append(f"Provenance {field} changed: attribution requires review.")
    if before.get("host") != after.get("host"):
        warnings.append("Host changed: timing deltas are not a controlled speed comparison.")
    if before.get("backend") != after.get("backend"):
        warnings.append("Backend changed: parser-specific changes cannot be isolated.")
    if old.keys() != new.keys():
        warnings.append(
            "Corpus changed: stored full-corpus summaries must not be compared directly."
        )
    successful = [
        identity
        for identity in common
        if old[identity]["status"] == new[identity]["status"] == "complete"
    ]
    changes = []
    excluded = []
    matched = []
    for identity in common:
        left, right = old[identity], new[identity]
        if left["status"] != right["status"]:
            changes.append({"id": identity, "before": left["status"], "after": right["status"]})
        if identity not in successful:
            excluded.append(
                {
                    "id": identity,
                    "reason": "not complete in one or both reports",
                    "before_status": left["status"],
                    "after_status": right["status"],
                }
            )
        elif (left["pages"], left["chunks"]) != (right["pages"], right["chunks"]):
            excluded.append({"id": identity, "reason": "page or chunk count changed"})
        else:
            timing(left)
            timing(right)
            matched.append(identity)
    truth_changes = []
    score_changes = []
    for identity in successful:
        left, right = old[identity], new[identity]
        changed = [field for field in TRUTH_FIELDS if left.get(field) != right.get(field)]
        if changed:
            truth_changes.append({"id": identity, "changed_fields": changed})
        left_scores, right_scores = scores(left), scores(right)
        if ("title_not_applicable" in left) != ("title_not_applicable" in right):
            left_scores.pop("title_accuracy", None)
            right_scores.pop("title_accuracy", None)
            warnings.append(
                f"{identity}: title accuracy denominator definition changed; comparison omitted."
            )
        for metric in sorted(left_scores.keys() & right_scores.keys()):
            previous, current = left_scores[metric], right_scores[metric]
            if previous is None or current is None or math.isclose(previous, current, abs_tol=1e-9):
                continue
            score_changes.append(
                {
                    "id": identity,
                    "metric": metric,
                    "before": previous,
                    "after": current,
                    "delta": current - previous,
                    "direction": "decrease" if current < previous else "increase",
                    "interpretation": "truth indicators changed"
                    if changed
                    else "provenance unverified",
                }
            )
    if not matched:
        warnings.append("No successful papers with stable page/chunk counts: no timing comparison.")
    return {
        "provenance": {
            label: {key: report.get(key) for key in ("backend", "host", "generated_unix")}
            for label, report in (("before", before), ("after", after))
        },
        "provenance_sidecars": sidecars,
        "warnings": warnings,
        "corpus": {
            "before_count": len(old),
            "after_count": len(new),
            "added_ids": sorted(new.keys() - old.keys()),
            "removed_ids": sorted(old.keys() - new.keys()),
            "matched_timing_ids": matched,
            "excluded_from_timing": excluded,
            "status_changes": changes,
            "failed_before_ids": sorted(
                identity for identity, paper in old.items() if failed(paper)
            ),
            "failed_after_ids": sorted(
                identity for identity, paper in new.items() if failed(paper)
            ),
        },
        "timing": {
            "before": timing_summary([old[identity] for identity in matched]),
            "after": timing_summary([new[identity] for identity in matched]),
        },
        "truth_indicator_changes": truth_changes,
        "observed_score_changes": score_changes,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("before", type=Path)
    parser.add_argument("after", type=Path)
    parser.add_argument("--before-provenance", type=Path)
    parser.add_argument("--after-provenance", type=Path)
    args = parser.parse_args()
    try:
        result = compare(
            json.loads(args.before.read_text()),
            json.loads(args.after.read_text()),
            json.loads(args.before_provenance.read_text()) if args.before_provenance else None,
            json.loads(args.after_provenance.read_text()) if args.after_provenance else None,
        )
    except (OSError, ValueError, KeyError, TypeError) as error:
        parser.error(str(error))
    print(json.dumps(result, indent=2, allow_nan=False))


if __name__ == "__main__":
    main()
