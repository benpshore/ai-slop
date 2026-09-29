#!/usr/bin/env python3
"""Run one evaluation with process memory evidence and reproducible provenance."""

import argparse
import hashlib
import json
import os
import platform
import subprocess
import time
from pathlib import Path


def digest(path: Path) -> str:
    with path.open("rb") as source:
        return hashlib.sha256(source.read()).hexdigest()


def measure(command: list[str]) -> dict:
    """Measure this child only; ru_maxrss is bytes on Darwin, KiB on Linux."""
    system = platform.system()
    if system not in {"Linux", "Darwin"}:
        raise RuntimeError("peak RSS measurement requires Linux or macOS")
    started = time.perf_counter()
    with subprocess.Popen(command) as child:  # noqa: S603 -- argv, never a shell
        _, status, usage = os.wait4(child.pid, 0)
        child.returncode = os.waitstatus_to_exitcode(status)
    return {
        "exit_code": child.returncode,
        "wall_seconds": time.perf_counter() - started,
        "peak_rss_bytes": int(usage.ru_maxrss) * (1 if system == "Darwin" else 1024),
        "scope": "whole eval process over the selected corpus; not per-document or per-chunk",
        "method": "wait4 rusage.ru_maxrss",
    }


def provenance(manifest: Path, backend: str, split: str) -> dict:
    items = json.loads(manifest.read_text())["items"]
    commit = subprocess.check_output(
        ["git", "rev-parse", "HEAD"],  # noqa: S607 -- fixed read-only git
        text=True,
    ).strip()
    return {
        "schema_version": 1,
        "git_commit": commit,
        "backend": backend,
        "split": split,
        "host": f"{'macos' if platform.system() == 'Darwin' else platform.system().lower()} "
        f"{'aarch64' if platform.machine() == 'arm64' else platform.machine()}",
        "runner_os": os.environ.get("RUNNER_OS"),
        "runner_arch": os.environ.get("RUNNER_ARCH"),
        "github_run_id": os.environ.get("GITHUB_RUN_ID"),
        "github_run_attempt": os.environ.get("GITHUB_RUN_ATTEMPT"),
        "corpus_manifest_sha256": digest(manifest),
        "native_manifest_sha256": digest(Path("native/manifest.json")),
        "metric_source_sha256": digest(Path("src/eval.rs")),
        "truth_source_sha256": digest(Path("src/latex_refs.rs")),
        "cargo_lock_sha256": digest(Path("Cargo.lock")),
        "timing_definition": "summed_stages_per_nominal_20_page_chunk_v1",
        "chunk_pages": 20,
        "timing_note": "report latency sums document stages and divides by nominal chunks; "
        "not measured 20-page streaming latency",
        "paper_inputs": {
            item["id"]: {
                "pdf_sha256": item.get("pdf_sha256"),
                "source_sha256": item.get("source_sha256"),
            }
            for item in items
            if item.get("split") == split
        },
        "paper_inputs_note": "manifest expectations; corpus fetch verifies pinned file hashes",
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--backend", required=True)
    parser.add_argument("--split", default="dev")
    parser.add_argument("--manifest", type=Path, default=Path("corpus/manifest.json"))
    parser.add_argument("--cache", type=Path, default=Path(".corpus-cache"))
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--executable", type=Path, default=Path("target/release/tpe"))
    args = parser.parse_args()
    if args.out.exists() and any(args.out.iterdir()):
        parser.error("--out must be empty; use a new directory to avoid stale report evidence")
    args.out.mkdir(parents=True, exist_ok=True)
    record = provenance(args.manifest, args.backend, args.split)
    (args.out / "provenance.json").write_text(json.dumps(record, indent=2) + "\n")
    command = [
        str(args.executable.resolve()),
        "eval",
        "--manifest",
        str(args.manifest),
        "--cache",
        str(args.cache),
        "--backend",
        args.backend,
        "--split",
        args.split,
        "--out",
        str(args.out),
        "--dump-dir",
        str(args.out / "dumps"),
    ]
    try:
        measurement = measure(command)
    except OSError as error:
        measurement = {"exit_code": 127, "error": str(error)}
    (args.out / "resources.json").write_text(json.dumps(measurement, indent=2) + "\n")
    print(json.dumps({"backend": args.backend, **measurement}), flush=True)
    code = measurement["exit_code"]
    return 128 - code if code < 0 else code


if __name__ == "__main__":
    raise SystemExit(main())
