"""Select, prepare and score independent PMC/JATS resolver cases (stdlib only)."""

import argparse
import copy
import hashlib
import json
import re
from collections import Counter, defaultdict
from pathlib import Path

import pmc_sample

VERSION = "1"
SCORER_VERSION = "2"  # Separate verified label eligibility from acquisition failure.
DOI_RE = re.compile(r"(?:https?://(?:dx\.)?doi\.org/)?10\.\d{4,9}/[^\s\"']+", re.I)


def digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def rank(value: str) -> str:
    return digest(("registry-cohort-v1:" + value).encode())


def read_json(path: Path) -> dict:
    return json.loads(path.read_bytes())


def write_json(path: Path, value: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n")


def identifiers(doi=None, pmid=None, pmcid=None) -> dict:
    return {key: value for key, value in (("doi", doi), ("pmid", pmid), ("pmcid", pmcid)) if value}


def group_for(entry: dict) -> str:
    if entry.get("doi") or DOI_RE.search(entry["raw"]):
        return "extracted_printed_doi"
    if entry.get("doi_link"):
        return "extracted_annotation_only"
    if re.search(r"\b(?:PMID\s*:|PMC\d)", entry["raw"], re.I):
        return "extracted_biomedical_id"
    return "extracted_query_only"


def canonical(truth: dict) -> str:
    return (
        ". ".join(
            str(truth[key]).strip(". ") for key in ("surname", "title", "year") if truth.get(key)
        )
        + "."
    )


def reference(raw: str, truth: dict | None = None) -> dict:
    truth = truth or {}
    return {
        "index": truth.get("index", 1),
        "label": None,
        "raw": raw,
        "authors": [truth["surname"]] if truth.get("surname") else [],
        "title": truth.get("title"),
        "year": truth.get("year"),
        "venue": None,
        "volume": None,
        "issue": None,
        "pages": None,
        "doi": None,
        "arxiv_id": None,
        "url": None,
        "page": 1,
        "anchor": None,
        "doi_link": None,
        "attempts": [],
        "resolved": None,
    }


def select(report: dict, report_hash: str, manifest: dict, manifest_hash: str) -> dict:
    if report["provenance"]["corpus_sha256"] != manifest_hash:
        raise ValueError("report and corpus manifest differ")
    papers = {paper["pmcid"]: paper for paper in report["papers"]}
    candidates, exclusions = {}, []
    for pmcid, paper in papers.items():
        actual = {entry["index"]: entry for entry in paper["backward"].get("entries", [])}
        eligible = []
        for row in paper["backward"].get("entry_results", []):
            truth, extracted = row.get("truth") or {}, row.get("extracted")
            if not truth.get("doi") or not extracted:
                continue
            entry = copy.deepcopy(actual[extracted["index"]])
            entry["attempts"], entry["resolved"] = [], None
            case_id = f"natural:{pmcid}:{truth['index']}"
            eligible.append(
                {
                    "id": case_id,
                    "group": group_for(entry),
                    "input_kind": "unchanged_extracted",
                    "source_pmcid": pmcid,
                    "source_reference_index": truth["index"],
                    "publisher": paper["publisher"],
                    "journal": paper["journal"],
                    "extraction_status": paper["backward"].get("extraction_status"),
                    "alignment": paper["backward"].get("alignment"),
                    "truth": truth,
                    "expected": {"kind": "positive", "identifiers": identifiers(doi=truth["doi"])},
                    "entry": entry,
                }
            )
        if eligible:
            candidates[pmcid] = sorted(eligible, key=lambda case: rank(case["id"]))
        else:
            exclusions.append(
                {
                    "pmcid": pmcid,
                    "reason": "no DOI-labeled matched backward reference",
                    "status": paper["backward"]["status"],
                    "truth_entries": paper["truth_count"],
                }
            )
    # Every eligible paper contributes before any paper receives a second case.
    selected = [cases[0] for _, cases in sorted(candidates.items())]
    remaining = sorted(
        (case for cases in candidates.values() for case in cases[1:]),
        key=lambda case: (case["group"] != "extracted_printed_doi", rank(case["id"])),
    )
    counts = Counter(case["source_pmcid"] for case in selected)
    for case in remaining:
        if len(selected) >= 240:
            break
        if counts[case["source_pmcid"]] < 3:
            selected.append(case)
            counts[case["source_pmcid"]] += 1
    if len(selected) != 240:
        raise ValueError(f"expected 240 natural cases, found {len(selected)}")
    controls = []
    usable = [
        case for case in selected if all(case["truth"].get(k) for k in ("surname", "title", "year"))
    ]
    usable.sort(key=lambda case: rank(case["id"] + ":control"))
    # Distinct source papers, and target DOIs, for the 20 control families.
    control_sources, seen_papers, seen_dois = [], set(), set()
    for case in usable:
        if case["source_pmcid"] not in seen_papers and case["truth"]["doi"] not in seen_dois:
            control_sources.append(case)
            seen_papers.add(case["source_pmcid"])
            seen_dois.add(case["truth"]["doi"])
        if len(control_sources) == 20:
            break
    if len(control_sources) != 20:
        raise ValueError("not enough independent control sources")
    for index, source in enumerate(control_sources):
        truth = source["truth"]
        donor = control_sources[(index + 1) % len(control_sources)]
        variants = [
            ("controlled_query_only", canonical(truth), "positive", truth),
            (
                "wrong_doi_recovery",
                canonical(truth) + " doi:" + donor["truth"]["doi"],
                "positive",
                truth,
            ),
            ("missing_metadata", "doi:" + truth["doi"], "must_abstain", {}),
            (
                "tampered_metadata",
                "Xylophonic Unrelatedauthor. Fabricated calibration control unrelated "
                "to scholarly evidence. 2099. doi:" + truth["doi"],
                "must_abstain",
                {},
            ),
        ]
        for group, raw, kind, fields in variants:
            case = copy.deepcopy(source)
            case.update(
                {
                    "id": f"{group}:{source['source_pmcid']}:{truth['index']}",
                    "group": group,
                    "input_kind": "controlled_transformation",
                    "entry": reference(raw, fields),
                    "derived_from": source["id"],
                    "expected": {
                        "kind": kind,
                        "identifiers": identifiers(doi=truth["doi"]) if kind == "positive" else {},
                    },
                }
            )
            if group == "wrong_doi_recovery":
                case["distractor_source"] = donor["id"]
                case["distractor_doi"] = donor["truth"]["doi"]
            controls.append(case)
    # Deliberately merged independent citations: no unique identity is labeled.
    # These are a diagnostic ambiguity challenge, excluded from positive precision.
    short = [case for case in usable if len(canonical(case["truth"])) <= 130]
    ambiguities = []
    for index in range(0, min(len(short) - 1, 20), 2):
        first, second = short[index : index + 2]
        if first["truth"]["doi"] == second["truth"]["doi"]:
            continue
        case = copy.deepcopy(first)
        case.update(
            {
                "id": f"ambiguous:{first['id']}:{second['id']}",
                "group": "ambiguous_merged_input",
                "input_kind": "controlled_transformation",
                "second_source": second,
                "entry": reference(canonical(first["truth"]) + " / " + canonical(second["truth"])),
                "expected": {
                    "kind": "ambiguous",
                    "alternatives": [first["truth"]["doi"], second["truth"]["doi"]],
                },
            }
        )
        ambiguities.append(case)
    # Fetch publisher XML only for these deterministic biomedical source papers.
    biomedical_sources = sorted(
        [
            item
            for item in manifest["items"]
            if any("PMC" in ref["raw"] for ref in papers[item["pmcid"]]["truth"])
        ],
        key=lambda item: rank(item["pmcid"] + ":biomedical"),
    )[:24]
    return {
        "contract_version": VERSION,
        "selector_version": VERSION,
        "source": {
            "report_sha256": report_hash,
            "extraction": report["provenance"],
            "corpus_sha256": manifest_hash,
        },
        "source_population": {
            "papers": len(papers),
            "truth_references": sum(p["truth_count"] for p in papers.values()),
            "eligible_papers": len(candidates),
            "exclusions": exclusions,
        },
        "natural_cases": selected,
        "controlled_cases": controls + ambiguities,
        "biomedical_sources": biomedical_sources,
    }


def jats_text(element) -> str:
    return " ".join("".join(element.itertext()).split()) if element is not None else ""


def biomedical_truth(data: bytes) -> list[dict]:
    article = pmc_sample.parse_xml(data)
    references = []
    for index, ref in enumerate(article.findall(".//back//ref-list/ref"), 1):
        ids = {}
        for pub_id in ref.findall(".//pub-id"):
            key = pub_id.get("pub-id-type", "").lower()
            key = {"pmc": "pmcid", "pubmed": "pmid"}.get(key, key)
            value = jats_text(pub_id).strip()
            if key in ("doi", "pmid", "pmcid") and value:
                ids[key] = value.lower() if key == "doi" else value
        if not all(ids.get(key) for key in ("doi", "pmid", "pmcid")):
            continue
        if not re.fullmatch(r"[1-9]\d*", ids["pmid"]) or not re.fullmatch(
            r"PMC[1-9]\d*", ids["pmcid"], re.I
        ):
            continue
        surname = jats_text(ref.find(".//name/surname"))
        title = jats_text(ref.find(".//article-title"))
        year = jats_text(ref.find(".//year"))
        if not surname or not title or not year.isdigit():
            continue
        references.append(
            {
                "index": index,
                "surname": surname,
                "title": title,
                "year": int(year),
                "identifiers": ids,
                "citation_xml": pmc_sample.ET.tostring(ref, encoding="unicode"),
            }
        )
    return references


def prepare(selection: dict, cache: Path) -> dict:
    cases = copy.deepcopy(selection["natural_cases"] + selection["controlled_cases"])
    biomedical, acquisition, label_exclusions = [], [], []
    for item in selection["biomedical_sources"]:
        path = pmc_sample.cache_paths(item, cache)[1]
        record = {"pmcid": item["pmcid"], "xml_url": item["xml_url"], "xml_md5": item["xml_md5"]}
        try:
            record["fetch_outcome"] = pmc_sample.fetch_one(item["xml_url"], item["xml_md5"], path)
            data = path.read_bytes()
            record["xml_sha256"] = digest(data)
            candidates = biomedical_truth(data)
            if candidates:
                truth = min(candidates, key=lambda ref: rank(item["pmcid"] + str(ref["index"])))
                if truth["identifiers"]["doi"] not in {
                    entry["truth"]["identifiers"]["doi"] for entry in biomedical
                }:
                    biomedical.append(
                        {
                            "source_pmcid": item["pmcid"],
                            "truth": truth,
                            "xml_provenance": record.copy(),
                        }
                    )
                else:
                    label_exclusions.append(
                        {
                            "pmcid": item["pmcid"],
                            "reason": "duplicate labeled target",
                            "xml_sha256": record["xml_sha256"],
                        }
                    )
            else:
                label_exclusions.append(
                    {
                        "pmcid": item["pmcid"],
                        "reason": "verified XML has no usable reference with DOI, PMID, PMCID, "
                        "surname, title and year",
                        "xml_sha256": record["xml_sha256"],
                    }
                )
            record["eligible_references"] = len(candidates)
        except (pmc_sample.FetchError, OSError, ValueError) as error:
            record["error"] = str(error)
        acquisition.append(record)
    # Preserve failures rather than silently drawing replacement papers.
    selected = biomedical
    for index, source in enumerate(selected):
        truth = source["truth"]
        donor = selected[(index + 1) % len(selected)]
        variants = [
            ("biomedical_pmid", " PMID:" + truth["identifiers"]["pmid"], "positive"),
            ("biomedical_pmcid", " PMCID:" + truth["identifiers"]["pmcid"], "positive"),
        ]
        if len(selected) > 1:
            variants.append(
                (
                    "contradictory_biomedical_ids",
                    " PMID:"
                    + truth["identifiers"]["pmid"]
                    + " PMCID:"
                    + donor["truth"]["identifiers"]["pmcid"],
                    "must_abstain",
                )
            )
        for group, suffix, kind in variants:
            case = copy.deepcopy(source)
            case.update(
                {
                    "id": f"{group}:{source['source_pmcid']}:{truth['index']}",
                    "group": group,
                    "source_reference_index": truth["index"],
                    "input_kind": "controlled_jats_citation",
                    "entry": reference(canonical(truth) + suffix, truth),
                    "expected": {
                        "kind": kind,
                        "identifiers": truth["identifiers"] if kind == "positive" else {},
                    },
                }
            )
            if kind == "must_abstain":
                case["distractor_source"] = donor
            cases.append(case)
    return {
        "contract_version": VERSION,
        "source": selection["source"],
        "source_population": selection["source_population"],
        "acquisition": acquisition,
        "biomedical_source_goal": len(selection["biomedical_sources"]),
        "biomedical_source_count": len(selected),
        "biomedical_label_exclusions": label_exclusions,
        "cases": cases,
    }


def normalize_identifier(key: str, value: str | None) -> str | None:
    if not value:
        return None
    value = value.strip()
    if key == "doi":
        return value.lower().removeprefix("https://doi.org/").removeprefix("http://doi.org/")
    return value.upper() if key == "pmcid" else value


def classify(expected: dict, resolved: dict | None) -> str:
    kind = expected["kind"]
    if kind == "must_abstain":
        return "false_accept" if resolved else "abstained"
    if kind == "ambiguous":
        if not resolved:
            return "withheld_ambiguous_input"
        accepted = normalize_identifier("doi", resolved.get("doi"))
        alternatives = {normalize_identifier("doi", doi) for doi in expected["alternatives"]}
        return (
            "accepted_ambiguous_alternative"
            if accepted in alternatives
            else "wrong_ambiguous_accept"
        )
    if not resolved:
        return "unresolved"
    matches = []
    for key, truth in expected["identifiers"].items():
        actual = normalize_identifier(key, resolved.get(key))
        if actual is not None:
            matches.append(actual == normalize_identifier(key, truth))
    if not matches:
        return "unverifiable_accept"
    return "correct" if all(matches) else "wrong_identifier"


def score(cohort: dict, records: list[dict], errors: list[str]) -> dict:
    results = {row["case_id"]: row for row in records if row.get("type") == "case"}
    seen = Counter(row["case_id"] for row in records if row.get("type") == "case")
    errors = list(errors)
    errors.extend(f"duplicate result: {case_id}" for case_id, count in seen.items() if count > 1)
    expected_ids = {case["id"] for case in cohort["cases"]}
    unexpected = sorted(set(results) - expected_ids)
    metadata = [row for row in records if row.get("type") == "run"]
    rows, groups = [], defaultdict(Counter)
    for case in cohort["cases"]:
        result = results.get(case["id"])
        if result is None:
            classification = "not_run"
        elif result.get("harness_error"):
            classification = "harness_error"
        else:
            classification = classify(case["expected"], result["entry"].get("resolved"))
            request_error = any(
                a["outcome"] == "error" for a in result["entry"].get("attempts", [])
            )
            if request_error:
                classification = {
                    "unresolved": "unresolved_with_error",
                    "abstained": "abstained_after_error",
                    "withheld_ambiguous_input": "ambiguous_input_request_error",
                }.get(classification, classification)
        count = groups[case["group"]]
        count["planned"] += 1
        count[classification] += 1
        if result:
            count["attempted"] += 1
            count["request_error_cases"] += int(
                any(a["outcome"] == "error" for a in result.get("entry", {}).get("attempts", []))
            )
            count["ambiguous_outcome_cases"] += int(
                any(
                    a["outcome"] == "ambiguous" for a in result.get("entry", {}).get("attempts", [])
                )
            )
        rows.append({"case": case, "classification": classification, "result": result})
    positive = [row for row in rows if row["case"]["expected"]["kind"] == "positive"]
    natural = [row for row in positive if row["case"]["input_kind"] == "unchanged_extracted"]
    attempt_outcomes, accepted_sources, accepted_methods = Counter(), Counter(), Counter()
    identifier_metrics = defaultdict(Counter)
    for row in rows:
        entry = (row["result"] or {}).get("entry", {})
        resolved = entry.get("resolved") or {}
        for attempt in entry.get("attempts", []):
            attempt_outcomes[f"{attempt['method']}:{attempt['outcome']}"] += 1
        if resolved:
            accepted_sources[resolved.get("source", "unknown")] += 1
            accepted_methods[resolved.get("method", "unknown")] += 1
        if row["case"]["expected"]["kind"] == "positive":
            for key, truth in row["case"]["expected"]["identifiers"].items():
                counts = identifier_metrics[key]
                counts["planned"] += 1
                actual = normalize_identifier(key, resolved.get(key))
                counts[
                    "missing"
                    if actual is None
                    else "correct"
                    if actual == normalize_identifier(key, truth)
                    else "incorrect"
                ] += 1

    def metrics(subset):
        counts = Counter(row["classification"] for row in subset)
        accepted = counts["correct"] + counts["wrong_identifier"] + counts["unverifiable_accept"]
        return {
            "planned": len(subset),
            "accepted": accepted,
            "correct": counts["correct"],
            "wrong_identifier": counts["wrong_identifier"],
            "unverifiable_accept": counts["unverifiable_accept"],
            "unresolved": counts["unresolved"] + counts["unresolved_with_error"],
            "unresolved_with_error": counts["unresolved_with_error"],
            "not_run": counts["not_run"],
            "harness_error": counts["harness_error"],
            "precision_lower_bound": {"numerator": counts["correct"], "denominator": accepted},
            "correct_coverage": {"numerator": counts["correct"], "denominator": len(subset)},
        }

    return {
        "scorer_version": SCORER_VERSION,
        "scorer_sha256": digest(Path(__file__).read_bytes()),
        "run_metadata": metadata,
        "source": cohort["source"],
        "source_population": cohort["source_population"],
        "acquisition": cohort["acquisition"],
        "biomedical_source_goal": cohort["biomedical_source_goal"],
        "biomedical_source_count": cohort["biomedical_source_count"],
        "biomedical_label_exclusions": cohort.get("biomedical_label_exclusions", []),
        "input_errors": errors,
        "unexpected_case_ids": unexpected,
        "planned": len(rows),
        "attempted": len(results),
        "distinct_source_papers": len({case["source_pmcid"] for case in cohort["cases"]}),
        "natural_positive_metrics": metrics(natural),
        "all_positive_metrics": metrics(positive),
        "groups": dict(groups),
        "attempt_outcomes": dict(attempt_outcomes),
        "accepted_sources": dict(accepted_sources),
        "accepted_methods": dict(accepted_methods),
        "independently_labeled_identifier_metrics": dict(identifier_metrics),
        "cases": rows,
    }


def evidence_failures(report: dict) -> list[str]:
    failures = list(report["input_errors"])
    if report["unexpected_case_ids"]:
        failures.append("unexpected case identities in result journal")
    if report["planned"] != report["attempted"]:
        failures.append("not every planned resolver case ran")
    if (
        report["biomedical_source_count"] + len(report.get("biomedical_label_exclusions", []))
        != report["biomedical_source_goal"]
    ):
        failures.append("some planned biomedical JATS labels are unavailable")
    if report["biomedical_source_count"] < 10:
        failures.append("fewer than ten independent biomedical source papers have usable labels")
    if any(item.get("error") for item in report["acquisition"]):
        failures.append("publisher XML acquisition or verification failed")
    for group, counts in report["groups"].items():
        for field in (
            "wrong_identifier",
            "false_accept",
            "unverifiable_accept",
            "wrong_ambiguous_accept",
            "request_error_cases",
            "harness_error",
        ):
            if counts.get(field):
                failures.append(f"{group}: {field}={counts[field]}")
    return failures


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    selection = commands.add_parser("select")
    selection.add_argument("--report", type=Path, required=True)
    selection.add_argument("--manifest", type=Path, default=Path("corpus/pmc-manifest.json"))
    selection.add_argument("--out", type=Path, required=True)
    preparation = commands.add_parser("prepare")
    preparation.add_argument(
        "--selection", type=Path, default=Path("corpus/registry-selection.json")
    )
    preparation.add_argument("--cache", type=Path, default=Path(".corpus-cache/pmc"))
    preparation.add_argument("--out", type=Path, required=True)
    scoring = commands.add_parser("score")
    scoring.add_argument("--cohort", type=Path, required=True)
    scoring.add_argument("--results", type=Path, required=True)
    scoring.add_argument("--out", type=Path, required=True)
    checking = commands.add_parser("check")
    checking.add_argument("--report", type=Path, required=True)
    args = parser.parse_args()
    if args.command == "select":
        result = select(
            read_json(args.report),
            digest(args.report.read_bytes()),
            read_json(args.manifest),
            digest(args.manifest.read_bytes()),
        )
    elif args.command == "prepare":
        result = prepare(read_json(args.selection), args.cache)
    elif args.command == "check":
        failures = evidence_failures(read_json(args.report))
        print(
            "\n".join(failures)
            if failures
            else "Complete cohort; no false accepts or request errors observed. "
            "See coverage denominators in the report."
        )
        raise SystemExit(bool(failures))
    else:
        records, errors = [], []
        for number, line in enumerate(args.results.read_text().splitlines(), 1):
            try:
                records.append(json.loads(line))
            except json.JSONDecodeError as error:
                errors.append(f"line {number}: {error}")
        cohort = read_json(args.cohort)
        cohort_hash = digest(args.cohort.read_bytes())
        runs = [row for row in records if row.get("type") == "run"]
        if len(runs) != 1:
            errors.append("expected exactly one run provenance record")
        elif (
            runs[0].get("cohort_sha256") != cohort_hash or runs[0].get("source") != cohort["source"]
        ):
            errors.append("run provenance does not match the scored cohort")
        result = score(cohort, records, errors)
        result["cohort_sha256"] = cohort_hash
    write_json(args.out, result)
    print(
        json.dumps(
            {
                key: value
                for key, value in result.items()
                if key
                in (
                    "planned",
                    "attempted",
                    "groups",
                    "natural_positive_metrics",
                    "all_positive_metrics",
                    "biomedical_source_count",
                )
            },
            indent=2,
        )
    )


if __name__ == "__main__":
    main()
