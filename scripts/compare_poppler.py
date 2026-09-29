"""Small, read-only Poppler layout comparison against TPE's eval dumps.

This measures raw bibliography title preservation, not structured reference
parsing or occurrence-level citation correctness. Both tools read identical
pinned PDF bytes. Timings deliberately label their different work boundaries.
"""

import json
import re
import statistics
import subprocess
import sys
import time
import unicodedata
from pathlib import Path


def tokens(value: str) -> list[str]:
    value = unicodedata.normalize("NFKC", value).casefold()
    value = re.sub(r"(?<=\w)-\s*\n\s*(?=\w)", "", value)
    return re.findall(r"[^\W_]+", value, flags=re.UNICODE)


def title_hits(truth: list[dict], text: str) -> tuple[int, int, list[str]]:
    stream = " " + " ".join(tokens(text)) + " "
    eligible = [ref for ref in truth if len(tokens(ref.get("title") or "")) >= 3]
    missing = []
    for ref in eligible:
        needle = " " + " ".join(tokens(ref["title"])) + " "
        if needle not in stream:
            missing.append(ref["key"])
    return len(eligible) - len(missing), len(eligible), missing


def ref_section(text: str) -> tuple[str, str]:
    # Deliberately report a missing heading instead of silently inventing a
    # bibliography boundary. Some of the selected papers have no heading.
    matches = list(
        re.finditer(
            r"(?im)^\s*(?:references|bibliography|works cited|literature cited|"
            r"notes and references)\s*$",
            text,
        )
    )
    if not matches:
        return "", "heading absent"
    match = matches[-1]
    return text[match.end() :], "last reference heading"


def median_ms(values: list[float]) -> float:
    return round(statistics.median(values), 2)


def run_poppler(pdf: Path) -> tuple[str, list[float]]:
    cmd = ["pdftotext", "-layout", "-enc", "UTF-8", str(pdf), "-"]
    samples = []
    output = b""
    for _ in range(5):
        start = time.perf_counter()
        result = subprocess.run(cmd, capture_output=True, check=True, timeout=60)
        samples.append((time.perf_counter() - start) * 1000)
        output = result.stdout
    return output.decode("utf-8"), samples


def run_tpe_bench(pdf: Path, executable: Path) -> tuple[float, str]:
    result = subprocess.run(
        [str(executable), "bench", str(pdf), "--iterations", "5"],
        capture_output=True,
        text=True,
        check=True,
        timeout=120,
    )
    match = re.search(r"p50\s+([\d.]+)\s+ms/chunk", result.stdout)
    if not match:
        raise ValueError(f"missing TPE timing for {pdf}: {result.stdout}")
    return float(match.group(1)), result.stdout


def compare(manifest_path: Path, cache: Path, eval_dir: Path, output: Path, tpe: Path) -> None:
    manifest = json.loads(manifest_path.read_text())
    report = json.loads((eval_dir / "report.json").read_text())
    papers = {p["id"]: p for p in report["papers"]}
    if report["summary"]["failed"] or len(papers) != len(manifest["items"]):
        raise ValueError("incomplete TPE evaluation")
    output.mkdir(parents=True, exist_ok=True)
    records = []
    for item in manifest["items"]:
        paper_id = item["id"]
        stem = paper_id.replace(":", "_")
        pdf = cache / stem / "paper.pdf"
        dump = json.loads((eval_dir / "dumps" / f"{stem}.json").read_text())
        poppler, pop_ms = run_poppler(pdf)
        section, section_note = ref_section(poppler)
        lopdf_section = dump["reference_section_text"]
        p_hit, eligible, p_missing = title_hits(dump["truth"], section)
        l_hit, l_eligible, l_missing = title_hits(dump["truth"], lopdf_section)
        if eligible != l_eligible:
            raise ValueError("title denominator mismatch")
        tpe_ms_chunk, bench_output = run_tpe_bench(pdf, tpe)
        chunks = max(1, papers[paper_id]["chunks"])
        (output / f"{stem}.poppler-layout.txt").write_text(poppler)
        (output / f"{stem}.lopdf-references.txt").write_text(lopdf_section)
        (output / f"{stem}.lopdf-markers.json").write_text(
            json.dumps(dump["markers"], indent=2) + "\n"
        )
        records.append(
            {
                "id": paper_id,
                "pages": papers[paper_id]["pages"],
                "chunks": chunks,
                "truth_references": len(dump["truth"]),
                "lopdf_structured_references": len(dump["extracted"]),
                "lopdf_matched_references": papers[paper_id]["matched_refs"],
                "lopdf_markers": len(dump["markers"]),
                "title_denominator": eligible,
                "poppler_title_hits": p_hit,
                "lopdf_raw_title_hits": l_hit,
                "poppler_missing_keys": p_missing,
                "lopdf_missing_keys": l_missing,
                "poppler_reference_boundary": section_note,
                "poppler_ms_document": median_ms(pop_ms),
                "poppler_ms_nominal_chunk": median_ms(pop_ms) / chunks,
                "lopdf_ms_nominal_chunk": tpe_ms_chunk,
                "lopdf_ms_document": round(tpe_ms_chunk * chunks, 2),
                "lopdf_bench_output": bench_output.strip(),
            }
        )
    (output / "comparison.json").write_text(json.dumps(records, indent=2) + "\n")
    lines = [
        "# Poppler `pdftotext -layout` vs TPE `lopdf` (five pinned PDFs)",
        "",
        "Raw-title hits: an exact normalized title word sequence in the detected "
        "bibliography region. This tests preservation, not a parsed citation "
        "record or occurrence-level linkage. If a heading is absent, Poppler "
        "gets no scored region; inspect its full text artifact manually.",
        "",
        "Timing: Poppler is a fresh CLI process doing text extraction only; "
        "TPE bench is a warm Rust process doing acquisition, hashing, text, "
        "layout, metadata and citation parsing. Both medians use five runs "
        "on the same runner, but their work is not equivalent.",
        "",
        "| Paper | Pages | Reference titles Poppler / lopdf | TPE matched refs | "
        "Poppler ms/doc | TPE ms/doc |",
        "|---|---:|---:|---:|---:|---:|",
    ]
    for row in records:
        lines.append(
            f"| {row['id']} | {row['pages']} | "
            f"{row['poppler_title_hits']}/{row['title_denominator']} / "
            f"{row['lopdf_raw_title_hits']}/{row['title_denominator']} | "
            f"{row['lopdf_matched_references']}/{row['truth_references']} | "
            f"{row['poppler_ms_document']:.2f} | {row['lopdf_ms_document']:.2f} |"
        )
    (output / "comparison.md").write_text("\n".join(lines) + "\n")
    print((output / "comparison.md").read_text())


if __name__ == "__main__":
    if len(sys.argv) != 6:
        raise SystemExit("usage: compare_poppler.py MANIFEST CACHE EVAL_DIR OUTPUT TPE")
    compare(*(Path(arg) for arg in sys.argv[1:]))
