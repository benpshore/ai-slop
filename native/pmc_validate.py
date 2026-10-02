"""Run each explicit native backend on the fixed PMC cohort without dropping failures."""

import argparse
import hashlib
import json
import os
import signal
import subprocess
import sys
import tempfile
import time
from collections import Counter
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import pmc_bib_eval as scorer
import pmc_sample as corpus

MANIFEST_SHA256 = "9465c36c416fbee0a17757ba9af41a13e89aced02cb6aad5883a49c1cf1a922e"
PAPERS = 200
TRUTH_ENTRIES = 9590
BACKENDS = ("lopdf", "pdfium", "docling-text", "docling")


def verified(path: Path, expected: str) -> dict:
    try:
        data = path.read_bytes()
    except OSError as err:
        return {"verified": False, "error": str(err)}
    actual = hashlib.md5(data, usedforsecurity=False).hexdigest()
    return {
        "verified": actual == expected,
        "expected_md5": expected,
        "md5": actual,
        "sha256": hashlib.sha256(data).hexdigest(),
        "bytes": len(data),
        "error": None if actual == expected else "pinned MD5 mismatch",
    }


def invoke(command: list[str], pdf: Path, raw: Path, timeout: float) -> tuple[dict, dict]:
    """Keep emitted records/diagnostics; kill the process group on a wall cutoff."""
    start = time.monotonic()
    outcome = "exited"
    with (
        raw.with_suffix(".stdout").open("wb") as stdout,
        raw.with_suffix(".stderr").open("wb") as stderr,
    ):
        # S603: argv is the selected local executable plus explicit CLI arguments; no shell.
        process = subprocess.Popen(  # noqa: S603
            command, stdout=stdout, stderr=stderr, start_new_session=True
        )
        try:
            process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            outcome = "timeout"
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()
    run = {
        "argv": command,
        "outcome": outcome,
        "returncode": process.returncode,
        "wall_s": time.monotonic() - start,
        "timeout_s": timeout,
    }
    try:
        lines = raw.with_suffix(".stdout").read_text().splitlines()
        record = json.loads(lines[0]) if len(lines) == 1 else None
        if not isinstance(record, dict):
            raise ValueError("expected exactly one JSON object")
        sources = (record.get("document") or {}).get("sources") or [{}]
        recorded_path = record.get("path") or sources[0].get("path")
        if recorded_path != str(pdf):
            raise ValueError("output belongs to a different input")
    except (ValueError, UnicodeError) as err:
        record = {"path": str(pdf), "status": "failed", "error": f"runner: {err}"}
        run["record_error"] = str(err)
    if outcome == "timeout" or process.returncode < 0:
        error = f"runner: {outcome}, returncode={process.returncode}"
        run["error"] = error
        record["error"] = error
        record.setdefault("warnings", []).append(error)
        if record.get("document"):
            record["status"] = "partial"
        elif record.get("status") == "found":
            record["extraction_status"] = "partial"
        else:
            record["status"] = "failed"
    return record, run


def run_paper(item: dict, args: argparse.Namespace, scratch: Path) -> dict:
    pdf, xml = corpus.cache_paths(item, args.cache)
    stem = f"{item['pmcid']}.{item['version']}"
    row = {
        "pmcid": item["pmcid"],
        "pdf": verified(pdf, item["pdf_md5"]),
        "xml": verified(xml, item["xml_md5"]),
        "paths": {},
    }
    for name in ("backward", "forward"):
        command = [str(args.binary), "bibliography" if name == "backward" else "extract"]
        command += [str(pdf), "--backend", args.backend]
        if name == "forward":
            command += ["--json", "--db", str(scratch / f"{stem}.db")]
        remaining = args.deadline - time.monotonic()
        if remaining <= 0:
            error = "runner: total evaluation wall budget exhausted"
            record = {"path": str(pdf), "status": "failed", "error": error, "warnings": [error]}
            execution = {"outcome": "budget_exhausted", "error": error, "argv": command}
        elif row["pdf"]["verified"]:
            try:
                record, execution = invoke(
                    command, pdf, args.out / "raw" / f"{stem}-{name}", min(args.timeout, remaining)
                )
            except OSError as err:
                record = {"path": str(pdf), "status": "failed", "error": f"runner: {err}"}
                execution = {"outcome": "spawn_failed", "error": str(err), "argv": command}
        else:
            error = f"acquisition: {row['pdf']['error']}"
            record = {"path": str(pdf), "status": "failed", "error": error}
            execution = {"outcome": "acquisition_failed", "error": error, "argv": command}
        identity = record.get("backend")
        if (identity and identity.get("name") != args.backend) or (
            record.get("status") in {"complete", "partial", "found", "not_found"} and not identity
        ):
            execution["identity_error"] = f"requested {args.backend}, emitted {identity}"
        row["paths"][name] = {"record": record, "execution": execution}
    statuses = ", ".join(
        f"{name}={path['record']['status']}" for name, path in row["paths"].items()
    )
    print(f"{args.backend} {item['pmcid']}: {statuses}", file=sys.stderr, flush=True)
    return row


def cohort_coverage(rows: list[dict], report: dict) -> dict:
    """Fixed denominators alongside scorer v2's conditional alignment metrics."""
    return {
        "paper_denominator": PAPERS,
        "truth_entry_denominator": TRUTH_ENTRIES,
        "rows": len(rows),
        "verified_pdfs": sum(row["pdf"]["verified"] for row in rows),
        "verified_jats": sum(row["xml"]["verified"] for row in rows),
        "scored_truth_entries": sum(paper["truth_count"] for paper in report["papers"]),
        "paths": {
            name: {
                "records": len(rows),
                "status_counts": dict(Counter(paper[name]["status"] for paper in report["papers"])),
                "execution_counts": dict(
                    Counter(row["paths"][name]["execution"]["outcome"] for row in rows)
                ),
                "complete": sum(
                    paper[name].get("extraction_status") == "complete" for paper in report["papers"]
                ),
                "matched_truth_entries": sum(paper[name]["matched"] for paper in report["papers"]),
                "unmatched_truth_entries": sum(
                    paper["truth_count"] - paper[name]["matched"] for paper in report["papers"]
                ),
            }
            for name in ("backward", "forward")
        },
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--backend", choices=BACKENDS, required=True)
    parser.add_argument("--manifest", type=Path, default=Path("corpus/pmc-manifest.json"))
    parser.add_argument("--cache", type=Path, default=Path(".corpus-cache/pmc"))
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--code-sha", required=True)
    parser.add_argument("--workers", type=int, default=2)
    parser.add_argument("--timeout", type=float, default=180)
    parser.add_argument("--budget", type=float, default=5400, help="total evaluation wall seconds")
    args = parser.parse_args(argv)
    if args.workers < 1 or args.timeout <= 0 or args.budget <= 0:
        parser.error("workers, timeout and budget must be positive")
    if hashlib.sha256(args.manifest.read_bytes()).hexdigest() != MANIFEST_SHA256:
        parser.error("manifest differs from the reviewed 200-paper cohort")
    manifest = json.loads(args.manifest.read_bytes())
    if (
        len(manifest["items"]) != PAPERS
        or sum(i["ref_count"] for i in manifest["items"]) != TRUTH_ENTRIES
    ):
        parser.error("unexpected cohort denominators")
    args.binary = args.binary.resolve(strict=True)
    args.cache = args.cache.resolve()
    (args.out / "raw").mkdir(parents=True, exist_ok=True)
    started = time.monotonic()
    args.deadline = started + args.budget
    with tempfile.TemporaryDirectory(prefix="pmc-native-") as temporary:
        scratch = Path(temporary)
        with ThreadPoolExecutor(max_workers=args.workers) as pool:
            rows = list(pool.map(lambda item: run_paper(item, args, scratch), manifest["items"]))
        truth_cache = scratch / "truth"
        truth_cache.mkdir()
        for item, row in zip(manifest["items"], rows, strict=True):
            if row["xml"]["verified"]:
                xml = corpus.cache_paths(item, args.cache)[1]
                (truth_cache / xml.name).symlink_to(xml)
        for name in ("backward", "forward"):
            text = "".join(json.dumps(row["paths"][name]["record"]) + "\n" for row in rows)
            (args.out / f"{name}.jsonl").write_text(text)
        scorer.main(
            [
                "--manifest",
                str(args.manifest),
                "--cache",
                str(truth_cache),
                "--bibliography",
                str(args.out / "backward.jsonl"),
                "--extract",
                str(args.out / "forward.jsonl"),
                "--out",
                str(args.out),
                "--code-sha",
                args.code_sha,
            ]
        )
    report = json.loads((args.out / "report.json").read_bytes())
    coverage = cohort_coverage(rows, report)
    comparable = (
        coverage["rows"] == PAPERS
        and coverage["verified_pdfs"] == PAPERS
        and coverage["verified_jats"] == PAPERS
        and coverage["scored_truth_entries"] == TRUTH_ENTRIES
        and not any(
            path["execution"].get("identity_error")
            for row in rows
            for path in row["paths"].values()
        )
    )
    payload = {
        "requested_backend": args.backend,
        "code_sha": args.code_sha,
        "binary_sha256": hashlib.sha256(args.binary.read_bytes()).hexdigest(),
        "manifest_sha256": MANIFEST_SHA256,
        "scorer_version": scorer.SCORER_VERSION,
        "resolution": "not_measured",
        "workers": args.workers,
        "per_path_timeout_s": args.timeout,
        "evaluation_budget_s": args.budget,
        "total_wall_s": time.monotonic() - started,
        "comparable_cohort": comparable,
        "coverage": coverage,
        "papers": rows,
    }
    (args.out / "execution.json").write_text(json.dumps(payload, indent=1) + "\n")
    summary = f"Backend {args.backend}: comparable cohort={comparable}. "
    summary += (
        f"PDFs {coverage['verified_pdfs']}/{PAPERS}; JATS {coverage['verified_jats']}/{PAPERS}; "
    )
    summary += f"truth entries {coverage['scored_truth_entries']}/{TRUTH_ENTRIES}.\n"
    summary += (
        "Failures and cutoffs remain in the paper/entry denominators; "
        "resolver accuracy is not measured.\n"
    )
    summary += "```json\n" + json.dumps(coverage, indent=2) + "\n```\n"
    (args.out / "coverage.md").write_text(summary)
    print(summary)
    return 0 if comparable else 1


if __name__ == "__main__":
    sys.exit(main())
