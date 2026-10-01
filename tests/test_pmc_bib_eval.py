"""Tests for scripts/pmc_bib_eval.py with inline JATS fragments and JSONL records."""

import hashlib
import importlib.util
import json
from pathlib import Path

DOI_2 = "10.1000/def.2"
WHO = "World Health Organization"
SCRIPT = Path(__file__).resolve().parents[1] / "scripts" / "pmc_bib_eval.py"
SPEC = importlib.util.spec_from_file_location("pmc_bib_eval", SCRIPT)
pmc = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(pmc)

# The surname Obtu\u0142owicz keeps a diacritic that the loose comparison strips.
JATS = """<?xml version="1.0"?>
<article xmlns:xlink="http://www.w3.org/1999/xlink" article-type="research-article">
<front><journal-meta><journal-title-group><journal-title>Test Journal</journal-title>
</journal-title-group><publisher><publisher-name>Test Press</publisher-name></publisher>
</journal-meta><article-meta><article-id pub-id-type="doi">10.1000/self.1</article-id>
<fpage>100</fpage><lpage>110</lpage><pub-date pub-type="epub"><year>2021</year></pub-date>
</article-meta></front>
<body><p>Body.</p></body>
<back><ref-list>
<ref id="r1"><label>1</label><element-citation publication-type="journal">
<person-group person-group-type="author">
<name><surname>Obtu\u0142owicz</surname><given-names>K</given-names></name>
<name><surname>Smith</surname><given-names>J</given-names></name></person-group>
<article-title>Alpha <italic>beta</italic> gamma</article-title><source>J Test</source>
<year>2019</year><volume>3</volume><fpage>1</fpage><lpage>9</lpage>
<pub-id pub-id-type="doi">10.1000/ABC.1</pub-id></element-citation></ref>
<ref id="r2"><label>2</label><mixed-citation publication-type="journal">
<string-name><surname>Jones</surname> <given-names>A</given-names></string-name>.
Delta epsilon zeta. <source>Other J</source> <year>2020</year>;4:10-20.
<ext-link ext-link-type="uri" xlink:href="https://doi.org/10.1000/def.2">link</ext-link>
</mixed-citation></ref>
<ref id="r3"><label>3</label><element-citation publication-type="book">
<person-group person-group-type="author"><collab>World Health Organization</collab>
</person-group><source>Guidelines</source><year>2018</year></element-citation></ref>
</ref-list></back></article>
"""


def entry(index, raw, author, title, year, doi, label=None):
    return {
        "index": index,
        "label": label or f"{index}.",
        "raw": raw,
        "authors": [author] if author else [],
        "title": title,
        "year": year,
        "doi": doi,
        "page": 5,
    }


def leaky_entry():
    return entry(3, "3. Leak 10.1000/self.1 \ufffd", None, None, None, None)


def good_entries():
    return [
        entry(
            1,
            "1. Obtu\u0142owicz K, Smith J. Alpha beta gamma.",
            "Obtu\u0142owicz K",
            "Alpha beta gamma",
            2019,
            "10.1000/abc.1",
        ),
        entry(2, "2. Jones A. Delta epsilon zeta.", "Jones A", "Delta epsilon zeta", 2020, DOI_2),
        entry(3, "3. World Health Organization. Guidelines.", WHO, None, 2018, None),
    ]


def test_truth_parses_both_citation_element_types():
    refs, facts = pmc.parse_truth(JATS.encode())
    assert [ref.kind for ref in refs] == ["element", "mixed", "element"]
    assert refs[0].surname == "Obtu\u0142owicz"
    assert refs[0].title == "Alpha beta gamma"
    assert refs[0].year == 2019
    assert refs[0].doi == "10.1000/abc.1"
    assert refs[1].surname == "Jones"
    assert refs[1].doi == "10.1000/def.2"
    assert refs[1].year == 2020
    assert refs[1].title is None
    assert refs[2].surname == "World Health Organization"
    assert facts["doi"] == "10.1000/self.1"
    assert facts["fpage"] == "100"
    assert pmc.numbered_labels([ref.label for ref in refs])


def test_surname_shapes_and_strictness():
    assert pmc.surname_of_name("Smith, J.") == "Smith"
    assert pmc.surname_of_name("J. A. Smith") == "Smith"
    assert pmc.surname_of_name("Smith AB") == "Smith"
    assert pmc.surname_of_name("van der Berg, J.") == "van der Berg"
    assert pmc.surname_of_name("WHO") == "WHO"
    assert pmc.nfc("Obtulowicz") != pmc.nfc("Obtu\u0142owicz")
    assert pmc.surname_loose_equal("Obtu\u0142owicz", "Obtulowicz")
    assert pmc.surname_loose_equal("van der Berg", "Berg")
    assert pmc.loose("\u00d8stergaard \u00df") == "ostergaard ss"
    assert pmc.strip_title("Dose\u2011response.") == "Dose-response"
    assert not pmc.surname_loose_equal("Smith", "Smyth")


def test_alignment_by_position_then_doi_then_surname_then_title():
    refs, _ = pmc.parse_truth(JATS.encode())
    extracted = [pmc.extracted_ref(e) for e in good_entries()]
    pairs, method = pmc.align(refs, extracted)
    assert method == "position"
    assert pairs == [(0, 0), (1, 1), (2, 2)]

    # One entry lost and the order reversed: DOI matches r2, surname/year
    # matches r3 (title-less), nothing is left for r1.
    shuffled = [
        pmc.extracted_ref(entry(1, "x", "World Health Organization", None, 2018, None)),
        pmc.extracted_ref(entry(2, "y", "Someone Q", "Wrong title", 2020, "10.1000/def.2")),
    ]
    pairs, method = pmc.align(refs, shuffled)
    assert method == "matched"
    assert pairs == [(1, 1), (2, 0)]

    # Title similarity is the last resort.
    by_title = [pmc.extracted_ref(entry(1, "z", None, "Alpha beta gamma!", None, None))]
    pairs, _ = pmc.align(refs, by_title)
    assert pairs == [(0, 0)]


def test_field_scores_strict_and_loose(tmp_path):
    refs, _ = pmc.parse_truth(JATS.encode())
    entries = good_entries()
    entries[0]["authors"] = ["Obtulowicz K"]
    entries[0]["title"] = "Alpha beta gamma."
    entries[1]["year"] = 2021
    entries[1]["doi"] = "https://doi.org/10.1000/DEF.2"
    mismatches = []
    result = pmc.score_list("PMC1", refs, entries, {"doi": "10.1000/self.1"}, mismatches)
    fields = result["fields"]
    assert fields["surname_strict"] == {"correct": 2, "total": 3}
    assert fields["surname_loose"] == {"correct": 3, "total": 3}
    assert fields["year"] == {"correct": 2, "total": 3}
    assert fields["doi"] == {"correct": 2, "total": 2}
    assert fields["doi_present"] == {"correct": 2, "total": 2}
    assert fields["doi_missing"] == 0
    assert fields["title_strict"] == {"correct": 1, "total": 1}
    assert result["count_exact"]
    assert [m.name for m in mismatches] == ["surname", "year"]
    assert mismatches[0].truth == "Obtu\u0142owicz"


def test_report_numbers_end_to_end(tmp_path):
    cache = tmp_path / "cache"
    cache.mkdir()
    (cache / "PMC1.1.xml").write_text(JATS, encoding="utf-8")
    (cache / "PMC2.1.xml").write_text(JATS, encoding="utf-8")
    (cache / "PMC3.1.xml").write_text(JATS, encoding="utf-8")
    manifest = {
        "seed": 1,
        "items": [
            {
                "pmcid": f"PMC{i}",
                "version": 1,
                "journal": "Test Journal",
                "publisher": "Test Press",
                "article_type": "research-article",
                "is_manuscript": i == 3,
                "year": 2021,
            }
            for i in (1, 2, 3)
        ],
    }
    (tmp_path / "manifest.json").write_text(json.dumps(manifest), encoding="utf-8")
    backward = [
        {
            "path": str(cache / "PMC1.1.pdf"),
            "status": "found",
            "total_pages": 10,
            "pages_scanned": 2,
            "section_page": 9,
            "heading": "References",
            "backend": {"name": "lopdf", "version": "test", "config_digest": "bounded"},
            "warnings": ["resource_limit: retained diagnostic"],
            "extraction_status": "partial",
            "references": good_entries(),
            "elapsed_ms": 20.0,
            "error": None,
        },
        {
            "path": str(cache / "PMC2.1.pdf"),
            "status": "found",
            "total_pages": 8,
            "pages_scanned": 8,
            "section_page": 7,
            "heading": None,
            "references": [*good_entries()[:2], leaky_entry()],
            "elapsed_ms": 40.0,
            "error": None,
        },
        {
            "path": str(cache / "PMC3.1.pdf"),
            "status": "not_found",
            "total_pages": 6,
            "pages_scanned": 6,
            "references": [],
            "elapsed_ms": 60.0,
            "error": None,
        },
    ]
    forward = [
        {
            "document": {"pages": 10, "sources": [{"path": str(cache / "PMC1.1.pdf")}]},
            "status": "complete",
            "references": good_entries()[:2],
            "timings": {"parse_ms": 5.0, "order_ms": 1.0, "citations_ms": 2.0, "write_ms": 9.0},
        },
        {"status": "failed", "path": str(cache / "PMC2.1.pdf"), "error": "boom", "ms": 3.0},
    ]
    bib_path = tmp_path / "bib.jsonl"
    bib_path.write_text("\n".join(json.dumps(r) for r in backward) + "\n", encoding="utf-8")
    ext_path = tmp_path / "extract.jsonl"
    ext_path.write_text("\n".join(json.dumps(r) for r in forward) + "\n", encoding="utf-8")
    out = tmp_path / "report"

    rc = pmc.main(
        [
            "--code-sha",
            "a" * 40,
            "--manifest",
            str(tmp_path / "manifest.json"),
            "--cache",
            str(cache),
            "--bibliography",
            str(bib_path),
            "--extract",
            str(ext_path),
            "--bibliography-wall-s",
            "1.5",
            "--out",
            str(out),
        ]
    )
    assert rc == 0
    payload = json.loads((out / "report.json").read_text(encoding="utf-8"))
    provenance = payload["provenance"]
    assert provenance["code_sha"] == "a" * 40
    assert (
        provenance["corpus_sha256"]
        == hashlib.sha256((tmp_path / "manifest.json").read_bytes()).hexdigest()
    )
    assert provenance["scorer_sha256"] == hashlib.sha256(SCRIPT.read_bytes()).hexdigest()
    assert provenance["scorer_version"] == pmc.SCORER_VERSION
    assert provenance["resolution"] == "not_measured"
    assert provenance["backend_identities"]["backward"] == [
        {"name": "lopdf", "version": "test", "config_digest": "bounded"}
    ]
    assert payload["papers"][0]["backward"]["extraction_status"] == "partial"
    assert payload["papers"][0]["backward"]["warnings"] == ["resource_limit: retained diagnostic"]
    assert (out / "manifest.json").read_bytes() == (tmp_path / "manifest.json").read_bytes()
    # A failed/missing record still accounts for every truth entry.
    missing = payload["papers"][2]["forward"]["entry_results"]
    assert len(missing) == 3
    assert all(entry["truth"] is not None and entry["extracted"] is None for entry in missing)
    aligned = payload["papers"][0]["backward"]["entry_results"]
    assert len(aligned) == 3
    assert aligned[0]["fields"]["title_strict"] == {"correct": 1, "total": 1}
    assert payload["papers"][0]["backward"]["entries"][0]["doi"] == "10.1000/abc.1"
    summary = payload["summary"]
    back = summary["backward"]
    assert back["papers"] == 3
    assert back["status_counts"] == {"found": 2, "not_found": 1}
    assert back["found"] == 2
    assert back["count_exact"] == 2
    assert back["fields"]["surname_strict"] == {"correct": 5, "total": 6}
    assert back["fields"]["year"] == {"correct": 5, "total": 6}
    assert back["fields"]["doi"] == {"correct": 4, "total": 4}
    assert back["fields"]["doi_missing"] == 0
    assert back["fffd_entries"] == 1
    assert back["leak_entries"] == 1
    assert back["extracted_entries"] == 6
    assert back["elapsed_ms_p50"] == 40.0
    assert back["elapsed_ms_p95"] == 60.0
    assert back["pages_scanned"] == 16
    assert back["total_pages"] == 24
    assert back["wall_s"] == 1.5
    assert back["count_diff_histogram"] == {"0": 2, "no list": 1}
    fwd = summary["forward"]
    assert fwd["status_counts"] == {"complete": 1, "failed": 1, "missing": 1}
    assert fwd["found"] == 1
    assert fwd["count_exact"] == 0
    assert fwd["count_diff_histogram"] == {"-1": 1, "no list": 2}
    assert fwd["elapsed_ms_p50"] == 3.0
    assert fwd["elapsed_ms_p95"] == 8.0
    assert fwd["matched_entries"] == 2
    assert fwd["unmatched_truth"] == 1
    manuscripts = {row["is_manuscript"]: row for row in summary["breakdowns"]["is_manuscript"]}
    assert manuscripts["True"]["backward_found"] == 0
    assert manuscripts["False"]["backward_count_exact"] == 2
    assert summary["breakdowns"]["style"][0]["style"] == "numbered"

    report = (out / "report.md").read_text(encoding="utf-8")
    assert "| list found | 66.7% (2/3) | 33.3% (1/3) |" in report
    failures = (out / "failures.md").read_text(encoding="utf-8")
    assert "## PMC3" in failures
    assert "## PMC1" not in failures
    assert "Obtu\u0142owicz" in failures
