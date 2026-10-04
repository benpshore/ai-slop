"""Score `tpe bibliography` and `tpe extract` reference lists against PMC JATS truth.

    python3 scripts/pmc_bib_eval.py --manifest corpus/pmc-manifest.json --cache DIR \\
        --bibliography out/bibliography.jsonl --extract out/extract.jsonl --out out/report \\
        --code-sha SHA_OF_EXTRACTION_BINARY_SOURCE

Writes `report.md`, `report.json` and `failures.md` into `--out`. The truth is
the publisher's own reference list from the JATS XML pinned by the manifest;
what each metric means, and does not mean, is in docs/PMC_EVAL.md. Stdlib only.
"""

import argparse
import hashlib
import json
import math
import re
import statistics
import sys
import unicodedata
import xml.etree.ElementTree as ET
from collections import Counter
from dataclasses import asdict, dataclass, field
from difflib import SequenceMatcher
from pathlib import Path

SCORER_VERSION = "3"  # Complete-cohort coverage and verified truth; accuracy stays diagnostic.

TITLE_ALIGN_MIN = 0.85
TITLE_LOOSE_MIN = 0.9
WORST_MISMATCHES = 30
BREAKDOWN_KEYS = ("publisher", "is_manuscript", "style")
RAW_PREVIEW = 220
XLINK_HREF = "{http://www.w3.org/1999/xlink}href"
DOI_PREFIXES = ("https://doi.org/", "http://doi.org/", "https://dx.doi.org/", "http://dx.doi.org/")
HEADING_RE = re.compile(
    r"^\s*(?:\d+\.?\s*)?(?:references?|bibliography|literature cited|works cited|reference list)\b",
    re.IGNORECASE,
)
CONTEXT_LINES = 6
HYPHENS = str.maketrans({"\u2010": "-", "\u2011": "-"})
CLOSEST_TITLES = 20
# Letters with no canonical decomposition, so NFD cannot strip their mark.
FOLD_LETTERS = str.maketrans(
    {
        "\u0142": "l",
        "\u0141": "L",
        "\u00f8": "o",
        "\u00d8": "O",
        "\u0111": "d",
        "\u0110": "D",
        "\u00f0": "d",
        "\u00d0": "D",
        "\u00fe": "th",
        "\u00de": "Th",
        "\u00df": "ss",
        "\u00e6": "ae",
        "\u00c6": "AE",
        "\u0153": "oe",
        "\u0152": "OE",
        "\u0131": "i",
        "\u0127": "h",
        "\u0126": "H",
        "\u0167": "t",
        "\u0166": "T",
    }
)
LEAK_PATTERNS = [
    r"\bpage \d+ of \d+\b",
    r"\bauthor manuscript\b",
    r"\bdownloaded from\b",
    r"\bavailable in pmc\b",
]


@dataclass
class TruthRef:
    index: int
    label: str | None
    kind: str
    surname: str | None
    year: int | None
    doi: str | None
    title: str | None
    raw: str


@dataclass
class ExtractedRef:
    index: int
    label: str | None
    raw: str
    author: str | None
    surname: str | None
    year: int | None
    doi: str | None
    title: str | None


@dataclass
class FieldScore:
    correct: int = 0
    total: int = 0

    def add(self, ok: bool) -> None:
        self.total += 1
        self.correct += int(ok)


@dataclass
class Fields:
    surname_strict: FieldScore = field(default_factory=FieldScore)
    surname_loose: FieldScore = field(default_factory=FieldScore)
    year: FieldScore = field(default_factory=FieldScore)
    doi: FieldScore = field(default_factory=FieldScore)
    doi_present: FieldScore = field(default_factory=FieldScore)
    title_strict: FieldScore = field(default_factory=FieldScore)
    title_loose: FieldScore = field(default_factory=FieldScore)
    doi_extra: int = 0
    doi_missing: int = 0
    surname_missing: int = 0
    title_missing: int = 0


@dataclass
class Mismatch:
    pmcid: str
    entry: int
    name: str
    truth: str
    extracted: str
    similarity: float


# ---------------------------------------------------------------- normalisation


def squash(value: str | None) -> str:
    return " ".join((value or "").split())


def nfc(value: str | None) -> str:
    return unicodedata.normalize("NFC", squash(value))


def loose(value: str | None) -> str:
    """NFKC, diacritics stripped, casefolded, letters and digits only."""
    text = unicodedata.normalize("NFKC", value or "").translate(FOLD_LETTERS)
    text = unicodedata.normalize("NFD", text)
    text = "".join(c for c in text if not unicodedata.combining(c))
    text = re.sub(r"[^\w\s]|_", " ", text.casefold())
    return " ".join(text.split())


def norm_doi(value: str | None) -> str | None:
    text = squash(value).strip()
    if not text:
        return None
    lower = text.lower()
    for prefix in DOI_PREFIXES:
        if lower.startswith(prefix):
            lower = lower[len(prefix) :]
            break
    lower = lower.removeprefix("doi:").strip()
    lower = lower.rstrip(".;,)")
    return lower or None


def strip_title(value: str | None) -> str:
    """Strict title form: NFC, typographic hyphens (U+2010, U+2011) as `-`, no final period."""
    return nfc(value).translate(HYPHENS).rstrip(".").strip()


def is_initial(token: str) -> bool:
    core = re.sub(r"[.\-']", "", token)
    has_letter = any(c.isalpha() for c in core)
    return not has_letter or (len(core) <= 3 and core.isupper())


def surname_of_name(name: str | None) -> str | None:
    """Surname part of an author string in any common printed shape.

    `Smith, J.` -> `Smith`; `J. A. Smith` -> `Smith`; `Smith AB` -> `Smith`;
    `van der Berg, J.` -> `van der Berg`.
    """
    text = squash(name)
    if not text:
        return None
    base = text.split(",")[0].strip() if "," in text else text
    words = [token for token in base.split() if not is_initial(token)]
    return " ".join(words) if words else base


def surname_loose_equal(truth: str, extracted: str) -> bool:
    a, b = loose(truth), loose(extracted)
    return bool(a and b) and (a == b or a.split()[-1] == b.split()[-1])


def similarity(a: str, b: str) -> float:
    matcher = SequenceMatcher(None, a, b, autojunk=False)
    if matcher.quick_ratio() < 0.5:
        return 0.0
    return matcher.ratio()


# ---------------------------------------------------------------- truth (JATS)


def parse_xml(data: bytes) -> ET.Element:
    # S314: the input is the pinned PMC JATS file this run downloaded and
    # MD5-checked; only text fields are read.
    return ET.fromstring(data)  # noqa: S314


def element_text(element: ET.Element | None) -> str | None:
    if element is None:
        return None
    return squash("".join(element.itertext())) or None


def first_surname(citation: ET.Element) -> str | None:
    groups = list(citation.iter("person-group"))
    authors = [g for g in groups if g.get("person-group-type") == "author"]
    ordered = authors or groups
    for group in ordered:
        for child in group:
            if child.tag == "name":
                return element_text(child.find("surname"))
            if child.tag == "string-name":
                return element_text(child.find("surname")) or element_text(child)
            if child.tag == "collab":
                return element_text(child)
    for tag in ("name", "string-name"):
        found = citation.find(f".//{tag}")
        if found is not None:
            return element_text(found.find("surname")) or element_text(found)
    return element_text(citation.find(".//collab"))


def truth_year(citation: ET.Element) -> int | None:
    for year in citation.iter("year"):
        match = re.search(r"\d{4}", element_text(year) or "")
        if match:
            return int(match.group())
    return None


def truth_doi(citation: ET.Element) -> str | None:
    for pub_id in citation.iter("pub-id"):
        if pub_id.get("pub-id-type") == "doi":
            return norm_doi(element_text(pub_id))
    for link in citation.iter("ext-link"):
        href = link.get(XLINK_HREF) or element_text(link) or ""
        if link.get("ext-link-type") == "doi" or "doi.org/" in href:
            return norm_doi(href)
    return None


def truth_title(citation: ET.Element) -> str | None:
    for tag in ("article-title", "chapter-title", "data-title", "part-title"):
        found = citation.find(f".//{tag}")
        if found is not None:
            return element_text(found)
    return None


def parse_truth(xml_bytes: bytes) -> tuple[list[TruthRef], dict]:
    """The `<back>` reference list, plus article facts used by the leakage check."""
    article = parse_xml(xml_bytes)
    back = article.find(".//back")
    refs: list[TruthRef] = []
    if back is not None:
        for index, ref in enumerate(back.iter("ref"), start=1):
            citation = ref.find("element-citation")
            kind = "element"
            if citation is None:
                citation = ref.find("mixed-citation")
                kind = "mixed"
            if citation is None:
                citation = ref
                kind = "other"
            label = element_text(ref.find("label"))
            refs.append(
                TruthRef(
                    index=index,
                    label=label,
                    kind=kind,
                    surname=first_surname(citation),
                    year=truth_year(citation),
                    doi=truth_doi(citation),
                    title=truth_title(citation),
                    raw=element_text(citation) or "",
                )
            )
    meta = article.find(".//article-meta")
    facts = {"doi": None, "fpage": None, "lpage": None, "elocation": None}
    if meta is not None:
        for identifier in meta.iter("article-id"):
            if identifier.get("pub-id-type") == "doi":
                facts["doi"] = norm_doi(element_text(identifier))
        facts["fpage"] = element_text(meta.find("fpage"))
        facts["lpage"] = element_text(meta.find("lpage"))
        facts["elocation"] = element_text(meta.find("elocation-id"))
    return refs, facts


def numbered_labels(labels: list[str | None]) -> bool:
    """At least half of the entries carry a label with a digit."""
    with_digit = sum(1 for label in labels if label and re.search(r"\d", label))
    return bool(labels) and with_digit * 2 >= len(labels)


# ---------------------------------------------------------------- extracted


def extracted_ref(entry: dict) -> ExtractedRef:
    authors = entry.get("authors") or []
    author = authors[0] if authors else None
    return ExtractedRef(
        index=int(entry.get("index") or 0),
        label=entry.get("label"),
        raw=entry.get("raw") or "",
        author=author,
        surname=surname_of_name(author),
        year=entry.get("year"),
        doi=norm_doi(entry.get("doi")),
        title=entry.get("title"),
    )


def pmcid_of_path(path: str) -> str:
    return Path(path).name.split(".")[0]


def read_jsonl(path: Path | None, errors: list[str]) -> list[dict]:
    records = []
    if path is None:
        return records
    try:
        with path.open(encoding="utf-8") as handle:
            for number, line in enumerate(handle, 1):
                if not line.strip():
                    continue
                try:
                    record = json.loads(line)
                    if not isinstance(record, dict):
                        raise ValueError("record is not an object")
                    records.append(record)
                except ValueError as error:
                    errors.append(f"{path}:{number}: invalid record: {error}")
    except (OSError, UnicodeError) as error:
        errors.append(f"{path}: {error}")
    return records


def record_identity(record: dict) -> tuple[str, str]:
    document = record.get("document")
    path = document["sources"][0]["path"] if document is not None else record["path"]
    if not isinstance(path, str) or not re.fullmatch(r"PMC\d+\.\d+\.pdf", Path(path).name):
        raise ValueError("record has no versioned PMC PDF source path")
    status = record.get("status")
    if not isinstance(status, str) or not status:
        raise ValueError("record has no status")
    entries = record.get("references", [])
    if not isinstance(entries, list) or any(not isinstance(entry, dict) for entry in entries):
        raise ValueError("references must be a list of objects")
    for entry in entries:
        extracted = extracted_ref(entry)
        # Reject corrupt field types before they can break scoring/reporting.
        if not isinstance(extracted.raw, str):
            raise ValueError("reference raw must be text")
        for key in ("raw", "title", "label", "doi"):
            if entry.get(key) is not None and not isinstance(entry[key], str):
                raise ValueError(f"reference {key} must be text")
    for key in ("elapsed_ms", "ms", "pages_scanned", "total_pages"):
        if record.get(key) is not None and (
            type(record[key]) not in (int, float) or not math.isfinite(record[key])
        ):
            raise ValueError(f"record {key} must be finite numeric data")
    return pmcid_of_path(path), path


def load_records(path: Path | None, errors: list[str], *, forward: bool) -> dict[str, dict]:
    out: dict[str, dict] = {}
    for number, record in enumerate(read_jsonl(path, errors), 1):
        try:
            pmcid, source = record_identity(record)
            allowed = (
                {"complete", "partial", "failed", "deferred"}
                if forward
                else {"found", "not_found", "failed"}
            )
            if record["status"] not in allowed:
                raise ValueError(f"unknown record status: {record['status']!r}")
            if pmcid in out:
                errors.append(f"{path}: duplicate record for {pmcid}")
                continue
            if not forward:
                result = record.copy()
            elif "document" in record:
                timings = record.get("timings") or {}
                ms = sum(timings.get(key, 0.0) for key in ("parse_ms", "order_ms", "citations_ms"))
                if not math.isfinite(ms):
                    raise ValueError("nonfinite extraction timing")
                result = {
                    "status": record["status"],
                    "total_pages": record["document"].get("pages"),
                    "references": record.get("references") or [],
                    "elapsed_ms": ms,
                    "error": None,
                    "backend": record.get("backend"),
                    "warnings": record.get("warnings") or [],
                    "pages_text": [page.get("text") or "" for page in record.get("pages") or []],
                }
            else:
                result = {
                    "status": record["status"],
                    "total_pages": None,
                    "references": [],
                    "elapsed_ms": record.get("ms"),
                    "error": record.get("error"),
                    "backend": record.get("backend"),
                    "warnings": record.get("warnings") or [],
                }
            result["source_path"] = source
            out[pmcid] = result
        except (ValueError, TypeError, KeyError, IndexError, AttributeError) as error:
            errors.append(f"{path}: record {number}: invalid record: {error}")
    return out


def backward_records(path: Path | None, errors: list[str]) -> dict[str, dict]:
    return load_records(path, errors, forward=False)


def forward_records(path: Path | None, errors: list[str]) -> dict[str, dict]:
    return load_records(path, errors, forward=True)


def coverage_errors(manifest: dict, records: dict[str, dict], name: str) -> list[str]:
    expected = {
        item["pmcid"]: f"{item['pmcid']}.{item['version']}.pdf" for item in manifest["items"]
    }
    errors = []
    if not expected:
        errors.append("manifest contains no papers")
    if len(expected) != len(manifest["items"]):
        errors.append("manifest contains duplicate PMCIDs")
    for pmcid in sorted(expected.keys() - records.keys()):
        errors.append(f"{name}: missing record for {pmcid}")
    for pmcid in sorted(records.keys() - expected.keys()):
        errors.append(f"{name}: unexpected record for {pmcid}")
    for pmcid in sorted(expected.keys() & records.keys()):
        actual = Path(records[pmcid]["source_path"]).name
        if actual != expected[pmcid]:
            errors.append(f"{name}: {pmcid} source version mismatch: {actual} != {expected[pmcid]}")
    return errors


# ---------------------------------------------------------------- alignment


Pairs = list[tuple[int, int]]


def align(truth: list[TruthRef], extracted: list[ExtractedRef]) -> tuple[Pairs, str]:
    """Pairs of (truth index, extracted index) into the two lists."""
    if len(truth) == len(extracted):
        return [(i, i) for i in range(len(truth))], "position"
    pairs: list[tuple[int, int]] = []
    free_t = set(range(len(truth)))
    free_e = set(range(len(extracted)))

    def take(i: int, j: int) -> None:
        pairs.append((i, j))
        free_t.discard(i)
        free_e.discard(j)

    by_doi: dict[str, list[int]] = {}
    for j, ext in enumerate(extracted):
        if ext.doi:
            by_doi.setdefault(ext.doi, []).append(j)
    for i, ref in enumerate(truth):
        candidates = [j for j in by_doi.get(ref.doi or "", []) if j in free_e]
        if candidates:
            take(i, candidates[0])

    by_key: dict[tuple[int, str], list[int]] = {}
    for j, ext in enumerate(extracted):
        last = loose(ext.surname).split()[-1:] if ext.surname else []
        if j in free_e and ext.year and last:
            by_key.setdefault((ext.year, last[0]), []).append(j)
    for i in sorted(free_t):
        ref = truth[i]
        last = loose(ref.surname).split()[-1:] if ref.surname else []
        if not (ref.year and last):
            continue
        candidates = [j for j in by_key.get((ref.year, last[0]), []) if j in free_e]
        if candidates:
            take(i, candidates[0])

    loose_titles = {j: loose(extracted[j].title) for j in free_e if extracted[j].title}
    for i in sorted(free_t):
        title = loose(truth[i].title)
        if len(title) < 10:
            continue
        best, best_score = None, TITLE_ALIGN_MIN
        for j, candidate in loose_titles.items():
            if j not in free_e:
                continue
            score = similarity(title, candidate)
            if score >= best_score:
                best, best_score = j, score
        if best is not None:
            take(i, best)
    pairs.sort()
    return pairs, "matched"


def heading_context(pages_text: list[str]) -> dict:
    """Forward page text at the last reference-heading line, else the last page's start."""
    for page_no in range(len(pages_text), 0, -1):
        lines = pages_text[page_no - 1].splitlines()
        for i in range(len(lines) - 1, -1, -1):
            if HEADING_RE.match(lines[i]):
                shown = [line[:160] for line in lines[i : i + CONTEXT_LINES]]
                return {"page": page_no, "heading": True, "lines": shown}
    if pages_text:
        lines = pages_text[-1].splitlines()[:CONTEXT_LINES]
        return {"page": len(pages_text), "heading": False, "lines": [line[:160] for line in lines]}
    return {"page": None, "heading": False, "lines": []}


# ---------------------------------------------------------------- scoring


def leaks(raw: str, facts: dict) -> bool:
    """Whether an entry carries the article's own running head or page furniture."""
    text = loose(raw)
    hits = [pattern for pattern in LEAK_PATTERNS if re.search(pattern, text)]
    own_doi = facts.get("doi")
    if own_doi and loose(own_doi) in text:
        hits.append("own doi")
    elocation = facts.get("elocation")
    if elocation and len(elocation) >= 5 and loose(elocation) in text.split():
        hits.append("own elocation id")
    fpage, lpage = facts.get("fpage"), facts.get("lpage")
    if fpage and lpage and fpage != lpage:
        span = re.escape(fpage) + r"\s*[-\u2013]\s*" + re.escape(lpage)
        if re.search(rf"\b{span}\b", raw):
            hits.append("own page range")
    return bool(hits)


def score_fields(
    pmcid: str,
    truth: list[TruthRef],
    extracted: list[ExtractedRef],
    pairs: list[tuple[int, int]],
    mismatches: list[Mismatch],
) -> Fields:
    fields = Fields()
    for i, j in pairs:
        ref, ext = truth[i], extracted[j]
        if ref.surname:
            if ext.surname:
                strict = nfc(ext.surname) == nfc(ref.surname)
                fields.surname_strict.add(strict)
                fields.surname_loose.add(strict or surname_loose_equal(ref.surname, ext.surname))
                if not strict:
                    score = similarity(loose(ref.surname), loose(ext.surname))
                    mismatches.append(
                        Mismatch(pmcid, ref.index, "surname", ref.surname, ext.author or "", score)
                    )
            else:
                fields.surname_strict.add(False)
                fields.surname_loose.add(False)
                fields.surname_missing += 1
        if ref.year:
            ok = ext.year == ref.year
            fields.year.add(ok)
            if not ok:
                years = (str(ref.year), str(ext.year or ""))
                mismatches.append(Mismatch(pmcid, ref.index, "year", *years, 0.0))
        if ref.doi:
            ok = ext.doi == ref.doi
            fields.doi.add(ok)
            if ext.doi:
                fields.doi_present.add(ok)
            else:
                fields.doi_missing += 1
            if not ok and ext.doi:
                score = similarity(ref.doi, ext.doi)
                mismatches.append(Mismatch(pmcid, ref.index, "doi", ref.doi, ext.doi, score))
        elif ext.doi:
            fields.doi_extra += 1
        if ref.title:
            if ext.title:
                strict = strip_title(ext.title) == strip_title(ref.title)
                score = 1.0 if strict else similarity(loose(ref.title), loose(ext.title))
                fields.title_strict.add(strict)
                fields.title_loose.add(score >= TITLE_LOOSE_MIN)
                if not strict:
                    mismatch = Mismatch(pmcid, ref.index, "title", ref.title, ext.title, score)
                    mismatches.append(mismatch)
            else:
                fields.title_strict.add(False)
                fields.title_loose.add(False)
                fields.title_missing += 1
    return fields


def score_list(
    pmcid: str,
    truth: list[TruthRef],
    entries: list[dict],
    facts: dict,
    mismatches: list[Mismatch],
) -> dict:
    extracted = [extracted_ref(entry) for entry in entries]
    pairs, method = align(truth, extracted)
    fields = score_fields(pmcid, truth, extracted, pairs, mismatches)
    return {
        "extracted_count": len(extracted),
        "count_exact": len(extracted) == len(truth),
        "count_diff": len(extracted) - len(truth),
        "alignment": method,
        "matched": len(pairs),
        "unmatched_truth": len(truth) - len(pairs),
        "unmatched_extracted": len(extracted) - len(pairs),
        "fields": asdict(fields),
        "fffd_entries": sum(1 for ext in extracted if "\ufffd" in ext.raw),
        "leak_entries": sum(1 for ext in extracted if leaks(ext.raw, facts)),
        "numbered": numbered_labels([ext.label for ext in extracted]),
        "entry_results": entry_results(pmcid, truth, extracted, pairs),
    }


def entry_results(
    pmcid: str, truth: list[TruthRef], extracted: list[ExtractedRef], pairs: Pairs
) -> list[dict]:
    """Keep every truth and extracted entry, including absent/unmatched ones."""
    matched = dict(pairs)
    used = {j for _, j in pairs}
    results = []
    for i, ref in enumerate(truth):
        j = matched.get(i)
        results.append(
            {
                "truth": asdict(ref),
                "extracted": asdict(extracted[j]) if j is not None else None,
                "fields": asdict(score_fields(pmcid, truth, extracted, [(i, j)], []))
                if j is not None
                else None,
            }
        )
    results.extend(
        {"truth": None, "extracted": asdict(ext), "fields": None}
        for j, ext in enumerate(extracted)
        if j not in used
    )
    return results


def empty_list_score() -> dict:
    return {
        "extracted_count": 0,
        "count_exact": False,
        "count_diff": None,
        "alignment": None,
        "matched": 0,
        "unmatched_truth": None,
        "unmatched_extracted": 0,
        "fields": asdict(Fields()),
        "fffd_entries": 0,
        "leak_entries": 0,
        "numbered": False,
    }


def evaluate_paper(
    item: dict,
    cache: Path,
    backward: dict | None,
    forward: dict | None,
    mismatches: dict[str, list[Mismatch]],
) -> dict:
    pmcid = item["pmcid"]
    stem = f"{pmcid}.{item['version']}"
    paper = {
        "pmcid": pmcid,
        "publisher": item.get("publisher") or "(unknown)",
        "journal": item.get("journal") or "(unknown)",
        "year": item.get("year"),
        "article_type": item.get("article_type"),
        "is_manuscript": bool(item.get("is_manuscript")),
        "truth_count": None,
        "truth_kinds": {},
        "truth_error": None,
        "style": "unknown",
        "total_pages": None,
    }
    xml_path = cache / f"{stem}.xml"
    truth: list[TruthRef] = []
    facts: dict = {}
    try:
        data = xml_path.read_bytes()
        expected_md5 = item.get("xml_md5")
        if expected_md5 and hashlib.md5(data, usedforsecurity=False).hexdigest() != expected_md5:
            raise ValueError("JATS checksum differs from the manifest pin")
        truth, facts = parse_truth(data)
        if not truth:
            raise ValueError("JATS contains no reference truth")
        if item.get("ref_count") is not None and len(truth) != item["ref_count"]:
            raise ValueError("JATS reference count differs from the manifest")
    except (OSError, ValueError) as err:
        paper["truth_error"] = str(err)
    except ET.ParseError as err:
        paper["truth_error"] = str(err)
    if paper["truth_error"]:
        truth = []
        facts = {}
    paper["truth_count"] = len(truth)
    paper["truth_kinds"] = dict(Counter(ref.kind for ref in truth))
    paper["truth"] = [asdict(ref) for ref in truth]
    truth_numbered = numbered_labels([ref.label for ref in truth])

    for name, record in (("backward", backward), ("forward", forward)):
        if record is None:
            result = {"status": "missing", "elapsed_ms": None, "error": "no record", "found": False}
            result.update(empty_list_score())
            result["entries"] = []
            result["entry_results"] = entry_results(pmcid, truth, [], [])
        else:
            entries = record.get("references") or []
            result = {
                "status": record.get("status"),
                "elapsed_ms": record.get("elapsed_ms"),
                "error": record.get("error"),
                "backend": record.get("backend"),
                "warnings": record.get("warnings") or [],
                "extraction_status": record.get("extraction_status", record.get("status")),
            }
            if name == "backward":
                result["pages_scanned"] = record.get("pages_scanned")
                result["section_page"] = record.get("section_page")
                result["heading"] = record.get("heading")
                result["found"] = record.get("status") == "found"
            else:
                result["found"] = bool(entries)
            if paper["total_pages"] is None:
                paper["total_pages"] = record.get("total_pages")
            if entries and truth:
                result.update(score_list(pmcid, truth, entries, facts, mismatches[name]))
            else:
                result.update(empty_list_score())
                result["extracted_count"] = len(entries)
                result["entry_results"] = entry_results(
                    pmcid, truth, [extracted_ref(e) for e in entries], []
                )
            result["entries"] = entries
        if result["unmatched_truth"] is None:
            result["unmatched_truth"] = len(truth)
        paper[name] = result
    paper["forward_context"] = heading_context((forward or {}).get("pages_text") or [])
    numbered = truth_numbered or paper["backward"].get("numbered", False)
    paper["style"] = "numbered" if numbered else "unnumbered"
    return paper


# ---------------------------------------------------------------- aggregation


def percentile(values: list[float], q: float) -> float | None:
    """Nearest-rank percentile, as the arXiv eval reports it."""
    if not values:
        return None
    ordered = sorted(values)
    rank = max(1, math.ceil(q / 100 * len(ordered)))
    return ordered[min(rank, len(ordered)) - 1]


def add_scores(total: dict, fields: dict) -> None:
    for key, value in fields.items():
        if isinstance(value, dict):
            total.setdefault(key, {"correct": 0, "total": 0})
            total[key]["correct"] += value["correct"]
            total[key]["total"] += value["total"]
        else:
            total[key] = total.get(key, 0) + value


def diff_bucket(diff: int | None) -> str:
    if diff is None:
        return "no list"
    if diff <= -5:
        return "<=-5"
    if diff >= 5:
        return ">=+5"
    return f"{diff:+d}" if diff else "0"


def summarize_path(papers: list[dict], name: str, wall_s: float | None) -> dict:
    results = [(paper, paper[name]) for paper in papers]
    statuses = Counter(result["status"] for _, result in results)
    found = [result for _, result in results if result["found"]]
    fields: dict = {}
    for result in found:
        add_scores(fields, result["fields"])
    elapsed = [result["elapsed_ms"] for _, result in results if result["elapsed_ms"] is not None]
    entries = sum(result["extracted_count"] for result in found)
    histogram = Counter(diff_bucket(result["count_diff"]) for _, result in results)
    summary = {
        "papers": len(results),
        "status_counts": dict(statuses),
        "found": len(found),
        "count_exact": sum(1 for result in found if result["count_exact"]),
        "count_diff_histogram": dict(histogram),
        "matched_entries": sum(result["matched"] for result in found),
        "unmatched_truth": sum(result["unmatched_truth"] or 0 for _, result in results),
        "unmatched_extracted": sum(result["unmatched_extracted"] for result in found),
        "truth_entries": sum(paper["truth_count"] for paper, _ in results),
        "extracted_entries": entries,
        "fields": fields,
        "fffd_entries": sum(result["fffd_entries"] for result in found),
        "leak_entries": sum(result["leak_entries"] for result in found),
        "elapsed_ms_p50": percentile(elapsed, 50),
        "elapsed_ms_p95": percentile(elapsed, 95),
        "elapsed_ms_mean": statistics.fmean(elapsed) if elapsed else None,
        "wall_s": wall_s,
    }
    if name == "backward":
        scanned = [
            (r["pages_scanned"], p["total_pages"]) for p, r in results if r.get("pages_scanned")
        ]
        summary["pages_scanned"] = sum(s for s, _ in scanned)
        summary["total_pages"] = sum(t or 0 for _, t in scanned)
        ratios = [s / t for s, t in scanned if t]
        summary["pages_scanned_ratio_mean"] = statistics.fmean(ratios) if ratios else None
    return summary


def breakdown(papers: list[dict], key: str) -> list[dict]:
    groups: dict[str, list[dict]] = {}
    for paper in papers:
        groups.setdefault(str(paper[key]), []).append(paper)
    rows = []
    for value, members in groups.items():
        rows.append(
            {
                key: value,
                "papers": len(members),
                "backward_found": sum(1 for p in members if p["backward"]["found"]),
                "backward_count_exact": sum(1 for p in members if p["backward"]["count_exact"]),
                "forward_found": sum(1 for p in members if p["forward"]["found"]),
                "forward_count_exact": sum(1 for p in members if p["forward"]["count_exact"]),
            }
        )
    rows.sort(key=lambda row: (-row["papers"], row[key]))
    return rows


# ---------------------------------------------------------------- rendering


def pct(numerator: int, denominator: int) -> str:
    if not denominator:
        return "n/a"
    return f"{100 * numerator / denominator:.1f}% ({numerator}/{denominator})"


def ms(value: float | None) -> str:
    return "n/a" if value is None else f"{value:.1f}"


def field_cell(fields: dict, key: str) -> str:
    score = fields.get(key)
    if not score:
        return "n/a"
    return pct(score["correct"], score["total"])


def counts_cell(counts: dict) -> str:
    return ", ".join(f"{key} {value}" for key, value in sorted(counts.items())) or "(none)"


def missing_cell(fields: dict) -> str:
    return f"{fields.get('surname_missing', 0)} / {fields.get('title_missing', 0)}"


def timing_cell(path: dict) -> str:
    p50, p95 = ms(path["elapsed_ms_p50"]), ms(path["elapsed_ms_p95"])
    return f"{p50} / {p95} / {ms(path['elapsed_ms_mean'])}"


def is_problem(result: dict) -> bool:
    return not result["found"] or not result["count_exact"]


def table(header: list[str], rows: list[list[str]]) -> list[str]:
    lines = ["| " + " | ".join(header) + " |", "|" + "|".join(" --- " for _ in header) + "|"]
    lines.extend("| " + " | ".join(row) + " |" for row in rows)
    return lines


def cell(value: str | None) -> str:
    return squash(value).replace("|", "\\|") or "(none)"


def preview(value: str | None) -> str:
    text = squash(value)
    if len(text) > RAW_PREVIEW:
        text = text[: RAW_PREVIEW - 3] + "..."
    return text.replace("|", "\\|") or "(empty)"


def render_report(summary: dict, papers: list[dict]) -> str:
    corpus = summary["corpus"]
    back, fwd = summary["backward"], summary["forward"]
    lines = ["# PMC bibliography measurement", ""]
    lines.append(
        f"{corpus['scored_papers']} articles with a parsed JATS reference list "
        f"({corpus['manifest_items']} in the manifest, {corpus['truth_errors']} truth failures), "
        f"{corpus['journals']} journals, {corpus['publishers']} publishers, "
        f"{corpus['manuscripts']} author manuscripts, "
        f"years {corpus['year_min']}-{corpus['year_max']}, "
        f"{back['truth_entries']} truth entries."
    )
    lines.append("")
    lines.append("## Headline: backward `tpe bibliography` vs forward `tpe extract`")
    lines.append("")
    n = back["papers"]
    rows = [
        ["status counts", counts_cell(back["status_counts"]), counts_cell(fwd["status_counts"])],
        ["list found", pct(back["found"], n), pct(fwd["found"], n)],
        ["entry count exact (all papers)", pct(back["count_exact"], n), pct(fwd["count_exact"], n)],
        [
            "entry count exact (found only)",
            pct(back["count_exact"], back["found"]),
            pct(fwd["count_exact"], fwd["found"]),
        ],
        [
            "truth entries unmatched (all papers with verified truth)",
            str(back["unmatched_truth"]),
            str(fwd["unmatched_truth"]),
        ],
        [
            "extracted entries unmatched (found papers)",
            str(back["unmatched_extracted"]),
            str(fwd["unmatched_extracted"]),
        ],
    ]
    for label, key in (
        ("first-author surname, strict", "surname_strict"),
        ("first-author surname, loose", "surname_loose"),
        ("year", "year"),
        ("DOI (truth has one; missing counts as wrong)", "doi"),
        ("DOI, when one was extracted", "doi_present"),
        ("title, strict", "title_strict"),
        ("title, loose (similarity >= 0.9)", "title_loose"),
    ):
        rows.append([label, field_cell(back["fields"], key), field_cell(fwd["fields"], key)])
    rows.extend(
        [
            [
                "matched entries with a truth DOI but none extracted",
                str(back["fields"].get("doi_missing", 0)),
                str(fwd["fields"].get("doi_missing", 0)),
            ],
            [
                "extracted DOI where truth has none",
                str(back["fields"].get("doi_extra", 0)),
                str(fwd["fields"].get("doi_extra", 0)),
            ],
            [
                "matched entries without an extracted first author / title",
                missing_cell(back["fields"]),
                missing_cell(fwd["fields"]),
            ],
            [
                "entries with U+FFFD",
                pct(back["fffd_entries"], back["extracted_entries"]),
                pct(fwd["fffd_entries"], fwd["extracted_entries"]),
            ],
            [
                "entries with running-head / page-furniture leakage",
                pct(back["leak_entries"], back["extracted_entries"]),
                pct(fwd["leak_entries"], fwd["extracted_entries"]),
            ],
            [
                "per-PDF ms p50 / p95 / mean",
                timing_cell(back),
                timing_cell(fwd),
            ],
            ["batch wall time (s)", ms(back["wall_s"]), ms(fwd["wall_s"])],
            [
                "pages scanned / total pages",
                f"{back.get('pages_scanned')} / {back.get('total_pages')} "
                f"(mean ratio {ms(back.get('pages_scanned_ratio_mean'))})",
                "all pages",
            ],
        ]
    )
    lines.extend(table(["metric", "backward", "forward"], rows))
    lines.append("")
    lines.append(
        "Backward `ms` is the CLI's `elapsed_ms` (acquire, hash, backward scan, parse); forward "
        "`ms` is `timings.parse_ms + order_ms + citations_ms`. Worker count and host depend "
        "on the invoking harness; these are diagnostics, not isolated service-time measurements."
    )
    lines.append("")
    lines.append("## Entry-count difference (extracted minus truth)")
    lines.append("")
    buckets = ["<=-5", "-4", "-3", "-2", "-1", "0", "+1", "+2", "+3", "+4", ">=+5", "no list"]
    back_hist, fwd_hist = back["count_diff_histogram"], fwd["count_diff_histogram"]
    rows = [
        [bucket, str(back_hist.get(bucket, 0)), str(fwd_hist.get(bucket, 0))] for bucket in buckets
    ]
    lines.extend(table(["difference", "backward papers", "forward papers"], rows))
    lines.append("")
    for key, title in (
        ("publisher", "By publisher"),
        ("is_manuscript", "By author-manuscript flag"),
        ("style", "By reference style (numbered label present)"),
    ):
        lines.append(f"## {title}")
        lines.append("")
        rows = [
            [
                cell(row[key]),
                str(row["papers"]),
                pct(row["backward_found"], row["papers"]),
                pct(row["backward_count_exact"], row["papers"]),
                pct(row["forward_found"], row["papers"]),
                pct(row["forward_count_exact"], row["papers"]),
            ]
            for row in summary["breakdowns"][key]
        ]
        header = [
            key,
            "papers",
            "backward found",
            "backward count exact",
            "forward found",
            "forward count exact",
        ]
        lines.extend(table(header, rows))
        lines.append("")
    lines.append("## Per paper (backward path)")
    lines.append("")
    rows = []
    for paper in papers:
        back_p = paper["backward"]
        rows.append(
            [
                paper["pmcid"],
                cell(paper["publisher"]),
                str(paper["total_pages"]),
                str(paper["truth_count"]),
                str(back_p["status"]),
                str(back_p["extracted_count"]),
                str(back_p.get("pages_scanned")),
                ms(back_p["elapsed_ms"]),
                str(paper["forward"]["extracted_count"]),
            ]
        )
    header = [
        "pmcid",
        "publisher",
        "pages",
        "truth",
        "status",
        "extracted",
        "scanned",
        "ms",
        "forward extracted",
    ]
    lines.extend(table(header, rows))
    lines.append("")
    return "\n".join(lines) + "\n"


def truth_line(ref: dict) -> str:
    title = preview(ref.get("title")) if ref.get("title") else "(no title)"
    year = ref.get("year") or "(no year)"
    surname = cell(ref.get("surname"))
    index = ref["index"]
    return f"{index}. {surname} / {year} / {title}"


def ends(items: list, count: int = 3) -> list:
    if len(items) <= 2 * count:
        return list(items)
    return [*items[:count], None, *items[-count:]]


def render_failures(papers: list[dict], mismatches: dict[str, list[Mismatch]]) -> str:
    lines = ["# Failures (backward `tpe bibliography`)", ""]
    problem = [paper for paper in papers if paper["truth_count"] and is_problem(paper["backward"])]
    lines.append(
        f"{len(problem)} of {sum(1 for p in papers if p['truth_count'])} papers are listed: "
        "every not_found / failed / missing record and every found list whose entry count "
        "differs from the JATS reference list."
    )
    lines.append("")
    for paper in problem:
        back = paper["backward"]
        lines.append(f"## {paper['pmcid']}")
        lines.append("")
        lines.append(
            f"- publisher: {cell(paper['publisher'])}; journal: {cell(paper['journal'])}; "
            f"type: {paper['article_type']}; manuscript: {paper['is_manuscript']}; "
            f"style: {paper['style']}"
        )
        lines.append(
            f"- pages: {paper['total_pages']}; truth refs: {paper['truth_count']} "
            f"({paper['truth_kinds']}); backward status: {back['status']}; "
            f"extracted: {back['extracted_count']}; pages scanned: {back.get('pages_scanned')}; "
            f"section page: {back.get('section_page')}; heading: {cell(back.get('heading'))}"
        )
        fwd = paper["forward"]
        lines.append(
            f"- forward path: status {fwd['status']}, {fwd['extracted_count']} entries, "
            f"count exact: {fwd['count_exact']}"
        )
        if back.get("error"):
            lines.append(f"- error: {cell(back['error'])}")
        lines.append("")
        lines.append("Extracted (first 3 and last 3):")
        lines.append("")
        entries = back.get("entries") or []
        if not entries:
            lines.append("- (no entries)")
        for entry in ends(entries):
            if entry is None:
                lines.append("- ...")
            else:
                label, page = cell(entry.get("label")), entry.get("page")
                lines.append(f"- [{label}] p{page}: {preview(entry.get('raw'))}")
        lines.append("")
        lines.append("Truth (first 3 and last 3, surname / year / title):")
        lines.append("")
        for ref in ends(paper["truth"]):
            lines.append("- ..." if ref is None else f"- {truth_line(ref)}")
        lines.append("")
        context = paper.get("forward_context") or {}
        page = context.get("page")
        if context.get("heading"):
            lines.append(f"Forward page text at the last reference heading (page {page}):")
        else:
            lines.append(f"No reference heading in the forward page text; page {page} starts:")
        lines.append("")
        lines.extend(f"    {line}" for line in context.get("lines") or ["(no page text)"])
        lines.append("")
    lines.append("## Worst field mismatches (backward path, both values present)")
    lines.append("")
    worst = sorted(mismatches["backward"], key=lambda m: (m.similarity, m.name, m.pmcid, m.entry))
    rows = [
        [m.pmcid, str(m.entry), m.name, cell(m.truth), cell(m.extracted), f"{m.similarity:.2f}"]
        for m in worst[:WORST_MISMATCHES]
    ]
    lines.extend(table(["pmcid", "entry", "field", "truth", "extracted", "similarity"], rows))
    lines.append("")
    closest = [m for m in mismatches["backward"] if m.name == "title"]
    closest.sort(key=lambda m: (-m.similarity, m.pmcid, m.entry))
    per_paper: Counter[str] = Counter()
    rows = []
    for m in closest:
        per_paper[m.pmcid] += 1
        if per_paper[m.pmcid] > 2:
            continue
        row = [m.pmcid, str(m.entry), cell(m.truth), cell(m.extracted), f"{m.similarity:.2f}"]
        rows.append(row)
        if len(rows) >= CLOSEST_TITLES:
            break
    lines.append("## Closest title mismatches (backward path, at most two per paper)")
    lines.append("")
    lines.extend(table(["pmcid", "entry", "truth", "extracted", "similarity"], rows))
    lines.append("")
    counts = Counter(m.name for m in mismatches["backward"])
    lines.append(f"All backward field mismatches by field: {counts_cell(counts)}")
    lines.append("")
    return "\n".join(lines) + "\n"


# ---------------------------------------------------------------- main


def evaluate(
    manifest: dict,
    cache: Path,
    backward: dict[str, dict],
    forward: dict[str, dict],
    bib_wall_s: float | None,
    extract_wall_s: float | None,
) -> tuple[dict, list[dict], dict[str, list[Mismatch]]]:
    mismatches: dict[str, list[Mismatch]] = {"backward": [], "forward": []}
    papers = []
    for item in manifest["items"]:
        pmcid = item["pmcid"]
        records = (backward.get(pmcid), forward.get(pmcid))
        papers.append(evaluate_paper(item, cache, *records, mismatches))
    scored = [paper for paper in papers if paper["truth_count"]]
    years = [paper["year"] for paper in scored if paper["year"]]
    summary = {
        "manifest_seed": manifest.get("seed"),
        "corpus": {
            "manifest_items": len(papers),
            "papers": len(papers),
            "scored_papers": len(scored),
            "truth_errors": sum(1 for paper in papers if paper["truth_error"]),
            "journals": len({paper["journal"] for paper in scored}),
            "publishers": len({paper["publisher"] for paper in scored}),
            "manuscripts": sum(1 for paper in scored if paper["is_manuscript"]),
            "year_min": min(years) if years else None,
            "year_max": max(years) if years else None,
        },
        "backward": summarize_path(papers, "backward", bib_wall_s),
        "forward": summarize_path(papers, "forward", extract_wall_s),
        "breakdowns": {key: breakdown(papers, key) for key in BREAKDOWN_KEYS},
    }
    return summary, papers, mismatches


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--cache", type=Path, required=True)
    parser.add_argument("--bibliography", type=Path, required=True)
    parser.add_argument("--extract", type=Path)
    parser.add_argument("--bibliography-wall-s", type=float)
    parser.add_argument("--extract-wall-s", type=float)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument(
        "--code-sha", required=True, help="SHA of the extraction binary's source checkout"
    )
    args = parser.parse_args(argv)
    manifest = json.loads(args.manifest.read_text(encoding="utf-8"))
    errors: list[str] = []
    backward = backward_records(args.bibliography, errors)
    forward = forward_records(args.extract, errors)
    errors.extend(coverage_errors(manifest, backward, "backward"))
    if args.extract is not None:
        errors.extend(coverage_errors(manifest, forward, "forward"))
    summary, papers, mismatches = evaluate(
        manifest,
        args.cache,
        backward,
        forward,
        args.bibliography_wall_s,
        args.extract_wall_s,
    )
    errors.extend(
        f"{paper['pmcid']}: truth unavailable: {paper['truth_error']}"
        for paper in papers
        if paper["truth_error"]
    )
    summary["coverage"] = {"valid": not errors, "errors": errors}
    args.out.mkdir(parents=True, exist_ok=True)
    (args.out / "manifest.json").write_bytes(args.manifest.read_bytes())
    report = render_report(summary, papers)
    if errors:
        report += (
            "\n## Invalid evaluation coverage\n\n"
            + "\n".join(f"- {error}" for error in errors)
            + "\n"
        )
    (args.out / "report.md").write_text(report, encoding="utf-8")
    (args.out / "failures.md").write_text(render_failures(papers, mismatches), encoding="utf-8")
    payload = {
        "provenance": provenance(args.manifest, args.code_sha, backward, forward),
        "summary": summary,
        "papers": papers,
        "mismatches": {k: [asdict(m) for m in v] for k, v in mismatches.items()},
    }
    encoded = json.dumps(payload, indent=1, ensure_ascii=False) + "\n"
    (args.out / "report.json").write_text(encoded, encoding="utf-8")
    sys.stdout.write(report.split("## Entry-count difference")[0])
    for error in errors:
        print(f"PMC evaluation invalid: {error}", file=sys.stderr)
    return 1 if errors else 0


def provenance(manifest: Path, code_sha: str, backward: dict, forward: dict) -> dict:
    """Fingerprint the actual manifest/scorer, preserving per-record backend identity."""
    identities = {}
    for name, records in (("backward", backward), ("forward", forward)):
        unique = {
            json.dumps(r["backend"], sort_keys=True) for r in records.values() if r.get("backend")
        }
        identities[name] = [json.loads(value) for value in sorted(unique)]
    return {
        "code_sha": code_sha,
        "corpus_sha256": hashlib.sha256(manifest.read_bytes()).hexdigest(),
        "scorer_version": SCORER_VERSION,
        "scorer_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        "backend_identities": identities,
        "resolution": "not_measured",
    }


if __name__ == "__main__":
    sys.exit(main())
