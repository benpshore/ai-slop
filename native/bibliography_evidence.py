#!/usr/bin/env python3
"""Run the nine focused native bibliography diagnostics with fixed denominators.

Eight historical backend/paper cases plus one newly observed current case.
This diagnostic subset never replaces the existing 60-paper Native evaluation.
PDFs and source archives remain in the corpus cache and are not exported.
"""

import argparse
import hashlib
import json
import os
import subprocess
from pathlib import Path

CASES = [
    ("pdfium", "arxiv:2502.00857", "historical"),
    ("docling-text", "arxiv:2503.15734", "historical"),
    ("docling-text", "arxiv:2309.10334", "historical"),
    ("docling-text", "arxiv:2503.13415", "historical"),
    ("docling-text", "arxiv:2506.23487", "historical"),
    ("docling-text", "arxiv:2507.14211", "historical"),
    ("docling-text", "arxiv:2602.16061", "historical"),
    ("docling", "arxiv:2309.10334", "historical"),
    ("docling-text", "arxiv:2502.00857", "current_additional"),
]


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def write_json(path: Path, value: dict) -> None:
    temporary = path.with_suffix(".tmp")
    temporary.write_text(json.dumps(value, indent=2) + "\n")
    temporary.replace(path)


def selected_items(manifest: dict) -> list[dict]:
    selected = []
    for paper in dict.fromkeys(paper for _, paper, _ in CASES):
        matches = [item for item in manifest["items"] if item["id"] == paper]
        if len(matches) != 1:
            raise ValueError(f"expected one manifest entry for {paper}")
        item = matches[0]
        if not item.get("pdf_sha256") or not item.get("source_sha256"):
            raise ValueError(f"missing pinned input hashes for {paper}")
        selected.append(item)
    return selected


def run(command: list[str], log: Path, timeout: int) -> dict:
    try:
        with log.open("w") as output:
            result = subprocess.run(  # noqa: S603 -- explicit argument vector
                command, stdout=output, stderr=subprocess.STDOUT, timeout=timeout, check=False
            )
        return {"exit_code": result.returncode}
    except (OSError, subprocess.TimeoutExpired) as error:
        return {"exit_code": 1, "error": str(error)}


def collect(manifest: Path, cache: Path, output: Path, executable: Path, example: Path) -> int:
    if output.exists() and any(output.iterdir()):
        raise ValueError("output must be empty; do not reuse stale evidence")
    items = selected_items(json.loads(manifest.read_text()))
    output.mkdir(parents=True, exist_ok=True)
    selected = output / "selected-manifest.json"
    write_json(selected, {"version": 1, "items": items})
    report = {
        "schema_version": 1,
        "scope": "focused diagnostics, separate from full Native corpus evaluation",
        "git_commit": subprocess.check_output(
            ["git", "rev-parse", "HEAD"],  # noqa: S607 -- fixed read-only command
            text=True,
        ).strip(),
        "git_tree": subprocess.check_output(
            ["git", "rev-parse", "HEAD^{tree}"],  # noqa: S607 -- fixed read-only command
            text=True,
        ).strip(),
        "github_run_id": os.environ.get("GITHUB_RUN_ID"),
        "github_run_attempt": os.environ.get("GITHUB_RUN_ATTEMPT"),
        "corpus_manifest_sha256": digest(manifest),
        "native_manifest_sha256": digest(Path("native/manifest.json")),
        "cargo_lock_sha256": digest(Path("Cargo.lock")),
        "metric_source_sha256": digest(Path("src/eval.rs")),
        "truth_source_sha256": digest(Path("src/latex_refs.rs")),
        "rust_toolchain_sha256": digest(Path("rust-toolchain.toml")),
        "executable_sha256": digest(executable) if executable.is_file() else None,
        "example_sha256": digest(example) if example.is_file() else None,
        "historical_case_denominator": 8,
        "additional_current_cases": 1,
        "unique_input_denominator": len(items),
        "full_native_corpus_denominator": 60,
        "cases": [
            {"backend": backend, "paper": paper, "group": group, "status": "pending"}
            for backend, paper, group in CASES
        ],
    }
    report_path = output / "diagnostic-summary.json"
    write_json(report_path, report)
    report["fetch"] = run(
        [
            str(executable.resolve()),
            "corpus",
            "fetch",
            "--manifest",
            str(selected),
            "--cache",
            str(cache),
        ],
        output / "fetch.log",
        900,
    )
    write_json(report_path, report)
    for number, case in enumerate(report["cases"], 1):
        directory = (
            output / f"case-{number:02d}-{case['backend']}-{case['paper'].replace(':', '_')}"
        )
        case["execution"] = run(
            [
                str(example.resolve()),
                str(selected),
                str(cache),
                case["backend"],
                case["paper"],
                str(directory),
            ],
            output / f"case-{number:02d}.log",
            240,
        )
        score = directory / "score.json"
        try:
            parsed = json.loads(score.read_text())
            if not isinstance(parsed, dict) or any(
                not isinstance(parsed.get(field), int)
                or isinstance(parsed[field], bool)
                or parsed[field] < 0
                for field in ("truth_refs", "extracted_refs", "matched_refs")
            ):
                raise ValueError("score lacks nonnegative integer reference counts")
            if parsed.get("id") != case["paper"] or parsed.get("status") not in (
                "complete",
                "partial",
            ):
                raise ValueError("score identity or extraction status does not match the case")
            case["score"] = parsed
            case["status"] = (
                "observed" if case["execution"]["exit_code"] == 0 else "diagnostic_failed"
            )
        except (OSError, ValueError) as error:
            case["status"] = "failed"
            case["score_error"] = str(error)
        write_json(report_path, report)
    report["observed_scores"] = sum("score" in case for case in report["cases"])
    report["zero_match_cases"] = sum(
        case["score"]["matched_refs"] == 0 for case in report["cases"] if "score" in case
    )
    report["failed_cases"] = sum(case["status"] != "observed" for case in report["cases"])
    report["extraction_status_note"] = (
        "Partial/Complete are retained in each score; neither certifies accuracy"
    )
    write_json(report_path, report)
    return int(
        bool(report["fetch"]["exit_code"] or report["failed_cases"] or report["zero_match_cases"])
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, default=Path("corpus/manifest.json"))
    parser.add_argument("--cache", type=Path, default=Path(".corpus-cache"))
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--executable", type=Path, default=Path("target/release/tpe"))
    parser.add_argument(
        "--example", type=Path, default=Path("target/release/examples/bibliography_evidence")
    )
    args = parser.parse_args()
    return collect(args.manifest, args.cache, args.out, args.executable, args.example)


if __name__ == "__main__":
    raise SystemExit(main())
