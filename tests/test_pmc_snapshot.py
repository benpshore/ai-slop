"""Recovered JATS truth is accepted only for its reviewed exact source pin."""

import hashlib
import importlib.util
import json
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("pmc_sample", ROOT / "scripts" / "pmc_sample.py")
pmc = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(pmc)
URL = pmc.BUCKET + "PMC9866638.1/PMC9866638.1.xml"
MD5 = "707148ab259c325c84acd030be5a5c8b"
SHA256 = "4bc081237097ef294c09e1f50b5f0a6d383f270ded3fcedbe8eb811661e6630b"


def no_network(_url):
    raise AssertionError("reviewed source recovery must not request newer upstream bytes")


def test_snapshot_and_cache_preserve_exact_bytes_offline(tmp_path, monkeypatch):
    monkeypatch.setattr(pmc, "fetch_bytes", no_network)
    target = tmp_path / "cache" / "PMC9866638.1.xml"
    assert pmc.fetch_one(URL, MD5, target) == "verified snapshot"
    data = target.read_bytes()
    assert len(data) == 110342
    assert hashlib.md5(data, usedforsecurity=False).hexdigest() == MD5
    assert hashlib.sha256(data).hexdigest() == SHA256
    assert pmc.fetch_one(URL, MD5, target) == "cached"
    target.write_bytes(b"stale or corrupted cache")
    assert pmc.fetch_one(URL, MD5, target) == "verified snapshot"
    assert target.read_bytes() == data


@pytest.mark.parametrize("missing", [False, True])
def test_invalid_snapshot_fails_closed_without_replacing_cache(tmp_path, monkeypatch, missing):
    target = tmp_path / "cached.xml"
    original = (pmc.SNAPSHOT_DIR / "PMC9866638.1.xml").read_bytes()
    target.write_bytes(original)
    monkeypatch.setattr(pmc, "SNAPSHOT_DIR", tmp_path)
    monkeypatch.setattr(pmc, "fetch_bytes", no_network)
    if not missing:
        # Same-sized corruption must fail as well as truncated/missing files.
        (tmp_path / "PMC9866638.1.xml").write_bytes(original[:-1] + b"!")
    with pytest.raises(pmc.FetchError, match=r"verified PMC9866638\.1\.xml snapshot"):
        pmc.fetch_one(URL, MD5, target)
    assert target.read_bytes() == original


def test_different_pin_or_url_cannot_use_snapshot(tmp_path, monkeypatch):
    data = b"a different explicitly selected source"
    requested = []

    def fetch(url):
        requested.append(url)
        return data

    monkeypatch.setattr(pmc, "fetch_bytes", fetch)
    target = tmp_path / "PMC9866638.1.xml"
    md5 = hashlib.md5(data, usedforsecurity=False).hexdigest()
    assert pmc.fetch_one(URL, md5, target) == "downloaded"
    assert target.read_bytes() == data
    with pytest.raises(pmc.FetchError, match="md5"):
        pmc.fetch_one(URL + "?different-object", MD5, target)
    assert requested == [URL, URL + "?different-object"]
    assert target.read_bytes() == data


def test_manifest_and_snapshot_match_reviewed_cohort_and_article():
    manifest_bytes = (ROOT / "corpus" / "pmc-manifest.json").read_bytes()
    assert (
        hashlib.sha256(manifest_bytes).hexdigest()
        == "9465c36c416fbee0a17757ba9af41a13e89aced02cb6aad5883a49c1cf1a922e"
    )
    manifest = json.loads(manifest_bytes)
    assert len(manifest["items"]) == 200
    assert sum(item["ref_count"] for item in manifest["items"]) == 9590
    item = next(item for item in manifest["items"] if item["pmcid"] == "PMC9866638")
    provenance = json.loads((pmc.SNAPSHOT_DIR / "PMC9866638.1.provenance.json").read_bytes())
    assert item == provenance["manifest_item"]
    data = (pmc.SNAPSHOT_DIR / "PMC9866638.1.xml").read_bytes()
    root = pmc.parse_xml(data)
    meta = root.find("front/article-meta")
    ids = {node.get("pub-id-type"): node.text for node in meta.findall("article-id")}
    assert ids["pmcid-ver"] == f"{item['pmcid']}.{item['version']}"
    assert ids["doi"] == item["doi"]
    assert ids["pmid"] == str(item["pmid"])
    assert pmc.text_of(meta.find("title-group/article-title")) == item["title"]
    facts = pmc.article_facts(data)
    for field in ("doi", "journal", "publisher", "article_type", "year", "ref_count"):
        assert facts[field] == item[field]
    assert provenance["xml_sha256"] == hashlib.sha256(data).hexdigest() == SHA256


def test_other_items_still_reject_changed_upstream_bytes(tmp_path, monkeypatch):
    monkeypatch.setattr(pmc, "fetch_bytes", lambda _: b"unreviewed replacement")
    target = tmp_path / "other.xml"
    with pytest.raises(pmc.FetchError, match="md5"):
        pmc.fetch_one(pmc.BUCKET + "PMC9866640.1/PMC9866640.1.xml", MD5, target)
    assert not target.exists()
