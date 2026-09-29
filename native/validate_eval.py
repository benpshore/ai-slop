#!/usr/bin/env python3
"""Require complete Native evaluation coverage; accuracy remains diagnostic."""

import argparse
import json
import platform
import sys
from collections import Counter
from pathlib import Path


def host_label() -> str:
    """Match Rust's std::env::consts names on the Native CI runners."""
    system = {"Darwin": "macos", "Linux": "linux"}.get(platform.system())
    machine = platform.machine().lower()
    arch = {"arm64": "aarch64", "amd64": "x86_64"}.get(machine, machine)
    if system is None:
        raise ValueError("Native evaluation validation requires Linux or macOS")
    return f"{system} {arch}"


def validate(manifest: dict, report: dict, split: str, backend: str) -> list[str]:
    """Return coverage/integrity errors without interpreting accuracy metrics."""
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
        if paper.get("status") != "complete":
            errors.append(f"incomplete paper {paper['id']}: status {paper.get('status')!r}")
        elif type(paper.get("pages")) is not int or paper["pages"] <= 0:
            errors.append(f"complete paper {paper['id']} has no positive integer page count")

    # Mirrors eval::is_failed; partial/deferred/plain failed are rejected
    # above even though the current Rust summary excludes them from failed.
    failed = sum(str(paper.get("status", "")).startswith("failed:") for paper in papers)
    for field, expected_count in (("papers", len(papers)), ("failed", failed)):
        value = summary.get(field)
        if type(value) is not int or value != expected_count:
            errors.append(f"summary.{field}: expected {expected_count}, got {value!r}")
    return errors


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--split", choices=("dev", "holdout", "all"), required=True)
    parser.add_argument("--backend", required=True)
    parser.add_argument("--report", type=Path, required=True)
    args = parser.parse_args(argv)
    try:
        manifest = json.loads(args.manifest.read_text())
        report = json.loads(args.report.read_text())
        if not isinstance(manifest, dict) or not isinstance(report, dict):
            raise ValueError("manifest and report must be JSON objects")
        errors = validate(manifest, report, args.split, args.backend)
    except (OSError, ValueError) as error:
        errors = [str(error)]
    if errors:
        for error in errors:
            print(f"Native evaluation invalid: {error}", file=sys.stderr)
        return 1
    print(f"{args.backend}: complete {args.split} corpus coverage; accuracy remains diagnostic")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
