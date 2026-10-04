#!/usr/bin/env python3
"""Validate full corpus coverage and explicit bounded outcomes, not quality acceptance."""

import argparse
import hashlib
import json
import platform
import re
import subprocess
import sys
from collections import Counter
from pathlib import Path

PROVENANCE_FILES = {
    "native_manifest_sha256": "native/manifest.json",
    "cargo_lock_sha256": "Cargo.lock",
    "metric_source_sha256": "src/eval.rs",
    "truth_source_sha256": "src/latex_refs.rs",
}


def source_provenance(root: Path, manifest: Path) -> dict:
    """Read intended extraction inputs from a source checkout, never a report."""
    commit = subprocess.check_output(  # noqa: S603 -- fixed git argv, no shell
        ["git", "-C", str(root), "rev-parse", "HEAD"],  # noqa: S607 -- fixed read-only git
        text=True,
    ).strip()
    subprocess.run(  # noqa: S603 -- fixed git argv, no shell
        ["git", "-C", str(root), "diff", "--quiet", "--no-ext-diff", "--no-textconv", "HEAD"],  # noqa: S607 -- fixed read-only git
        check=True,
    )
    return {
        "git_commit": commit,
        "corpus_manifest_sha256": hashlib.sha256(manifest.read_bytes()).hexdigest(),
        **{
            key: hashlib.sha256((root / path).read_bytes()).hexdigest()
            for key, path in PROVENANCE_FILES.items()
        },
    }


def verify_current_provenance(provenance: dict, expected: dict) -> dict:
    """A historical allowance cannot establish the current artifact's identity."""
    if not isinstance(provenance, dict):
        raise ValueError("current evaluation requires provenance")
    if expected.keys() != {*PROVENANCE_FILES, "corpus_manifest_sha256", "git_commit"}:
        raise ValueError("current-source provenance expectations are incomplete")
    for field, value in expected.items():
        if provenance.get(field) != value:
            raise ValueError(f"current-source provenance mismatch: {field}")
    return {key: value for key, value in expected.items() if key != "git_commit"}


def host_label() -> str:
    """Match Rust's std::env::consts names on the Native CI runners."""
    system = {"Darwin": "macos", "Linux": "linux"}.get(platform.system())
    machine = platform.machine().lower()
    arch = {"arm64": "aarch64", "amd64": "x86_64"}.get(machine, machine)
    if system is None:
        raise ValueError("Native evaluation validation requires Linux or macOS")
    return f"{system} {arch}"


def validate(
    manifest: dict,
    report: dict,
    split: str,
    backend: str,
    reviewed_partials: dict | None = None,
) -> list[str]:
    """Return coverage/integrity errors; this is not full quality acceptance."""
    items = manifest.get("items")
    if not isinstance(items, list) or any(not isinstance(item, dict) for item in items):
        raise ValueError("manifest.items must be a list of objects")
    selected = [item for item in items if split == "all" or item.get("split") == split]
    expected = [item.get("id") for item in selected]
    if not expected or any(not isinstance(item, str) or not item for item in expected):
        raise ValueError("selected manifest split must contain nonempty string IDs")
    if len(set(expected)) != len(expected):
        raise ValueError("selected manifest split contains duplicate IDs")
    papers = report.get("papers")
    if not isinstance(papers, list) or any(not isinstance(paper, dict) for paper in papers):
        raise ValueError("report.papers must be a list of objects")
    actual = [paper.get("id") for paper in papers]
    if any(not isinstance(item, str) or not item for item in actual):
        raise ValueError("report papers must contain nonempty string IDs")
    summary = report.get("summary")
    if not isinstance(summary, dict):
        raise ValueError("report.summary must be an object")

    errors = []
    if report.get("backend") != backend:
        errors.append(f"backend mismatch: expected {backend!r}, got {report.get('backend')!r}")
    expected_host = host_label()
    if report.get("host") != expected_host:
        errors.append(f"host mismatch: expected {expected_host!r}, got {report.get('host')!r}")
    duplicates = sorted(item for item, count in Counter(actual).items() if count > 1)
    missing = sorted(set(expected) - set(actual))
    unexpected = sorted(set(actual) - set(expected))
    for label, ids in (("duplicate", duplicates), ("missing", missing), ("unexpected", unexpected)):
        if ids:
            errors.append(f"{label} paper IDs: {', '.join(ids)}")
    for paper in papers:
        reviewed = (reviewed_partials or {}).get(paper["id"])
        if reviewed is not None and paper.get("pages") != reviewed["pages"]:
            errors.append(f"reviewed input page count mismatch: {paper['id']}")
        recognized_partial = (
            paper.get("status") == "partial"
            and reviewed is not None
            and reviewed.get("partial_eligible", True)
            and paper.get("pages") == reviewed["pages"]
            and paper.get("warnings") == reviewed["warning_count"]
        )
        if paper.get("status") != "complete" and not recognized_partial:
            errors.append(f"incomplete paper {paper['id']}: status {paper.get('status')!r}")
        elif type(paper.get("pages")) is not int or paper["pages"] <= 0:
            errors.append(f"paper {paper['id']} has no positive integer page count")
        # Even an unreviewed Partial must retain valid counts and disclose
        # zero-match failures. Its status error must not mask those failures.
        counts = [paper.get(key) for key in ("truth_refs", "extracted_refs", "matched_refs")]
        if any(type(count) is not int or count < 0 for count in counts):
            errors.append(f"invalid reference counts: {paper['id']}")
        else:
            truth, extracted, matched = counts
            if truth == 0:
                errors.append(
                    f"missing reference truth for {paper['id']}: cannot validate bibliography"
                )
            if matched > min(truth, extracted):
                errors.append(f"inconsistent reference counts: {paper['id']}")
            if truth > 0 and (extracted == 0 or matched == 0):
                errors.append(
                    f"reference extraction failed for {paper['id']}: "
                    f"{truth} expected, {extracted} extracted, {matched} matched; "
                    f"status {paper['status']!r} does not establish quality"
                )
            if reviewed is not None:
                baseline = reviewed["reference_baseline"]
                if (
                    truth != baseline["truth_refs"]
                    or matched < baseline["matched_refs"]
                    or extracted - matched > baseline["spurious_refs"]
                ):
                    errors.append(f"reviewed-partial reference regression: {paper['id']}")

    # Mirrors eval::is_failed. Unreviewed partial/deferred/plain failed remain
    # rejected above even though Rust's summary excludes them from failed.
    failed = sum(str(paper.get("status", "")).startswith("failed:") for paper in papers)
    for field, expected_count in (("papers", len(papers)), ("failed", failed)):
        value = summary.get(field)
        if type(value) is not int or value != expected_count:
            errors.append(f"summary.{field}: expected {expected_count}, got {value!r}")
    return errors


def load_reviewed_partials(
    manifest: dict,
    manifest_hash: str,
    policy: dict,
    provenance: dict,
    split: str,
    backend: str,
    dumps: dict,
    partial_ids: set[str] | None = None,
    current_pins: dict | None = None,
) -> dict:
    """Apply explicit reviewed limitations only to the same verified public inputs."""
    if (
        not isinstance(policy, dict)
        or type(policy.get("version")) is not int
        or policy["version"] != 1
    ):
        raise ValueError("reviewed-partial policy must have version 1")
    if not isinstance(provenance, dict):
        raise ValueError("reviewed partials require provenance")
    if policy.get("split") != split or policy.get("host") != host_label():
        raise ValueError("reviewed-partial policy split/host mismatch")
    pins = policy.get("provenance_pins")
    required = {
        "corpus_manifest_sha256",
        "native_manifest_sha256",
        "cargo_lock_sha256",
        "metric_source_sha256",
        "truth_source_sha256",
    }
    if not isinstance(pins, dict) or pins.keys() != required:
        raise ValueError("reviewed-partial policy provenance pins are incomplete")
    if not isinstance(pins["cargo_lock_sha256"], str) or not re.fullmatch(
        r"[0-9a-f]{64}", pins["cargo_lock_sha256"]
    ):
        raise ValueError("invalid reviewed-partial dependency pin")
    if manifest_hash != pins["corpus_manifest_sha256"]:
        raise ValueError("reviewed-partial corpus manifest hash mismatch")
    if current_pins is not None and (
        current_pins.keys() != required
        or any(provenance.get(field) != value for field, value in current_pins.items())
        or current_pins["corpus_manifest_sha256"] != manifest_hash
    ):
        raise ValueError("current-source provenance pins are incomplete or mismatched")
    dependency_changed = provenance.get("cargo_lock_sha256") != pins["cargo_lock_sha256"]
    for field, expected in pins.items():
        if provenance.get(field) != expected:
            if field == "cargo_lock_sha256" and current_pins is not None:
                # Verified current inputs are different from historical inputs.
                # No historical Partial exception is valid for this lock.
                continue
            raise ValueError(f"reviewed-partial provenance mismatch: {field}")
    for field, expected in (("backend", backend), ("split", split), ("host", host_label())):
        if provenance.get(field) != expected:
            raise ValueError(f"reviewed-partial provenance mismatch: {field}")
    selected = {
        item["id"]: {key: item[key] for key in ("pdf_sha256", "source_sha256")}
        for item in manifest["items"]
        if split == "all" or item.get("split") == split
    }
    if provenance.get("paper_inputs") != selected:
        raise ValueError("reviewed-partial input provenance mismatch")
    policies = policy.get("papers")
    if not isinstance(policies, dict) or not isinstance(policies.get(backend, {}), dict):
        raise ValueError("reviewed-partial paper policies must be objects")
    allowed = policies.get(backend, {})
    for paper_id, outcome in allowed.items():
        if not isinstance(outcome, dict) or outcome.get("inputs") != selected.get(paper_id):
            raise ValueError(f"reviewed-partial source pin mismatch: {paper_id}")
        warnings = outcome.get("warnings")
        if (
            type(outcome.get("pages")) is not int
            or outcome["pages"] <= 0
            or not isinstance(warnings, list)
            or not warnings
            or any(not isinstance(w, str) or ": resource_limit:" not in w for w in warnings)
            or type(outcome.get("warning_count")) is not int
            or outcome["warning_count"] < len(warnings)
        ):
            raise ValueError(f"invalid reviewed-partial outcome: {paper_id}")
        baseline = outcome.get("reference_baseline")
        if (
            not isinstance(baseline, dict)
            or baseline.keys() != {"truth_refs", "matched_refs", "spurious_refs"}
            or any(type(count) is not int or count < 0 for count in baseline.values())
            or baseline["truth_refs"] <= 0
            or not 0 < baseline["matched_refs"] <= baseline["truth_refs"]
        ):
            raise ValueError(f"invalid reviewed-partial reference baseline: {paper_id}")
        # A genuinely Complete result no longer uses a Partial exception.
        # validate_dumps still rejects a Complete label hiding a cutoff.
        if dependency_changed or (partial_ids is not None and paper_id not in partial_ids):
            continue
        dump = dumps.get(paper_id)
        if not isinstance(dump, dict) or any(
            dump.get(key) != expected
            for key, expected in (
                ("id", paper_id),
                ("pages", outcome["pages"]),
                ("warnings", warnings),
            )
        ):
            raise ValueError(f"reviewed-partial dump mismatch: {paper_id}")
        resource_warnings = [
            row for row in dump["page_warnings"] if row[1].startswith("resource_limit:")
        ]
        if resource_warnings != outcome["resource_page_warnings"]:
            raise ValueError(f"reviewed-partial page-warning mismatch: {paper_id}")
        if diagnostics_digest(dump["page_warnings"]) != outcome.get("page_warnings_sha256"):
            raise ValueError(f"reviewed-partial diagnostics mismatch: {paper_id}")
    return {
        paper_id: {**outcome, "partial_eligible": not dependency_changed}
        for paper_id, outcome in allowed.items()
    }


def diagnostics_digest(warnings: list) -> str:
    """Pin the full ordered page diagnostics without duplicating large dumps."""
    encoded = json.dumps(warnings, ensure_ascii=True, separators=(",", ":")).encode()
    return hashlib.sha256(encoded).hexdigest()


def validate_dumps(report: dict, dumps: dict) -> list[str]:
    """Check retained page evidence for every reported outcome, including Complete."""
    errors = []
    for paper in report["papers"]:
        paper_id = paper["id"]
        dump = dumps.get(paper_id)
        if not isinstance(dump, dict) or dump.get("id") != paper_id:
            errors.append(f"dump identity mismatch: {paper_id}")
            continue
        pages = paper.get("pages")
        if (
            type(pages) is not int
            or pages <= 0
            or type(dump.get("pages")) is not int
            or dump["pages"] != pages
        ):
            errors.append(f"dump page count mismatch: {paper_id}")
            continue
        warnings = dump.get("page_warnings")
        if not isinstance(warnings, list) or any(
            not isinstance(row, list)
            or len(row) != 2
            or type(row[0]) is not int
            or not 1 <= row[0] <= pages
            or not isinstance(row[1], str)
            for row in warnings
        ):
            errors.append(f"invalid page diagnostics: {paper_id}")
            continue
        document_warnings = dump.get("warnings")
        if not isinstance(document_warnings, list) or any(
            not isinstance(warning, str) for warning in document_warnings
        ):
            errors.append(f"invalid document diagnostics: {paper_id}")
            continue
        if type(paper.get("warnings")) is not int or paper["warnings"] != len(warnings) + len(
            document_warnings
        ):
            errors.append(f"dump warning count mismatch: {paper_id}")
        if any(warning.startswith("failed:") for _, warning in warnings):
            errors.append(f"page extraction failure: {paper_id}")
        if paper.get("status") == "complete" and (
            any(warning.startswith("resource_limit:") for _, warning in warnings)
            or any(
                "resource_limit:" in warning or "failed:" in warning
                for warning in document_warnings
            )
        ):
            errors.append(f"Complete hides a cutoff or failure: {paper_id}")
        errors.extend(validate_reference_dump(paper, dump))
    return errors


def validate_reference_dump(paper: dict, dump: dict) -> list[str]:
    """Cross-check counts against retained evidence, not a success label."""
    paper_id = paper["id"]
    if any(
        not isinstance(dump.get(field), list)
        or any(not isinstance(row, dict) for row in dump[field])
        for field in ("truth", "extracted", "matches")
    ):
        return [f"invalid reference dump: {paper_id}"]
    truth_keys = [row.get("key") for row in dump["truth"]]
    extracted_indices = [row.get("index") for row in dump["extracted"]]
    match_keys = [row.get("truth_key") for row in dump["matches"]]
    matched_indices = [
        row["extracted_index"] for row in dump["matches"] if row.get("extracted_index") is not None
    ]
    if (
        any(not isinstance(key, str) or not key for key in truth_keys + match_keys)
        or any(
            type(index) is not int or index <= 0 for index in extracted_indices + matched_indices
        )
        or any("extracted_index" not in row for row in dump["matches"])
        or len(set(extracted_indices)) != len(extracted_indices)
        # Source truth can repeat a key. Match rows must retain that exact
        # multiplicity, while extracted targets must still be one-to-one.
        or Counter(match_keys) != Counter(truth_keys)
        or len(set(matched_indices)) != len(matched_indices)
        or not set(matched_indices) <= set(extracted_indices)
    ):
        return [f"invalid reference alignment: {paper_id}"]
    errors = [
        f"dump reference count mismatch: {paper_id} {field}"
        for field, count in (
            ("truth_refs", len(truth_keys)),
            ("extracted_refs", len(extracted_indices)),
            ("matched_refs", len(matched_indices)),
        )
        if type(paper.get(field)) is not int or paper[field] != count
    ]
    if paper.get("matches") != dump["matches"]:
        errors.append(f"dump reference matches mismatch: {paper_id}")
    return errors


def dump_name(paper_id: str) -> str:
    """Read only names inside the supplied artifact directory."""
    if not isinstance(paper_id, str) or not re.fullmatch(r"[A-Za-z0-9:_.-]+", paper_id):
        raise ValueError("invalid paper ID for dump path")
    return paper_id.replace(":", "_") + ".json"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--split", choices=("dev", "holdout", "all"), required=True)
    parser.add_argument("--backend", required=True)
    parser.add_argument("--report", type=Path, required=True)
    parser.add_argument("--reviewed-partials", type=Path)
    parser.add_argument("--provenance", type=Path)
    parser.add_argument("--dumps", type=Path)
    parser.add_argument(
        "--source-root",
        type=Path,
        default=Path(__file__).resolve().parents[1],
        help="checkout that produced the evaluated artifact (defaults to this checkout)",
    )
    args = parser.parse_args(argv)
    try:
        manifest_bytes = args.manifest.read_bytes()
        manifest = json.loads(manifest_bytes)
        report = json.loads(args.report.read_text())
        if not isinstance(manifest, dict) or not isinstance(report, dict):
            raise ValueError("manifest and report must be JSON objects")
        reviewed = None
        supplied = [bool(args.reviewed_partials), bool(args.provenance), bool(args.dumps)]
        if any(supplied) and not all(supplied):
            raise ValueError("--reviewed-partials, --provenance and --dumps are required together")
        if args.reviewed_partials:
            policy = json.loads(args.reviewed_partials.read_text())
            provenance = json.loads(args.provenance.read_text())
            current_pins = verify_current_provenance(
                provenance, source_provenance(args.source_root, args.manifest)
            )
            dumps = {
                paper_id: json.loads((args.dumps / dump_name(paper_id)).read_text())
                for paper_id in (paper["id"] for paper in report["papers"])
            }
            dump_errors = validate_dumps(report, dumps)
            if dump_errors:
                raise ValueError("; ".join(dump_errors))
            reviewed = load_reviewed_partials(
                manifest,
                hashlib.sha256(manifest_bytes).hexdigest(),
                policy,
                provenance,
                args.split,
                args.backend,
                dumps,
                {paper["id"] for paper in report["papers"] if paper.get("status") == "partial"},
                current_pins,
            )
            if current_pins["cargo_lock_sha256"] != policy["provenance_pins"]["cargo_lock_sha256"]:
                print(
                    "Historical Partial exceptions are inapplicable: dependency lock differs; "
                    "zero exceptions applied; historical pins and reference baselines retained",
                    file=sys.stderr,
                )
        errors = validate(manifest, report, args.split, args.backend, reviewed)
    except (
        KeyError,
        TypeError,
        IndexError,
        AttributeError,
        OSError,
        ValueError,
        subprocess.CalledProcessError,
    ) as error:
        errors = [str(error)]
    if errors:
        for error in errors:
            print(f"Native evaluation invalid: {error}", file=sys.stderr)
        return 1
    counts = Counter(paper["status"] for paper in report["papers"])
    print(
        f"{args.backend}: {len(report['papers'])} evaluated {args.split} papers; "
        f"{counts['complete']} complete, {counts['partial']} reviewed partial; "
        "coverage validated; accuracy remains diagnostic, not quality acceptance"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
