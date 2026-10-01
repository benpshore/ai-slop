"""The recovered XML is usable offline only for its original verified pin."""

import hashlib
import importlib.util
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "scripts/pmc_sample.py"
SPEC = importlib.util.spec_from_file_location("pmc_sample", SCRIPT)
pmc = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(pmc)


def test_verified_snapshot_preserves_both_digests_without_network(tmp_path, monkeypatch):
    def no_network(_url):
        raise AssertionError("the original verified XML must not need a live source")

    monkeypatch.setattr(pmc, "fetch_bytes", no_network)
    target = tmp_path / "PMC9866638.1.xml"
    url = pmc.BUCKET + "PMC9866638.1/PMC9866638.1.xml"
    assert pmc.fetch_one(url, "707148ab259c325c84acd030be5a5c8b", target) == "verified snapshot"
    data = target.read_bytes()
    assert len(data) == 110342
    assert (
        hashlib.sha256(data).hexdigest()
        == "4bc081237097ef294c09e1f50b5f0a6d383f270ded3fcedbe8eb811661e6630b"
    )
    assert pmc.fetch_one(url, "707148ab259c325c84acd030be5a5c8b", target) == "cached"


def test_different_pin_uses_normal_fetch_and_verification(tmp_path, monkeypatch):
    data = b"a different explicitly selected source"
    monkeypatch.setattr(pmc, "fetch_bytes", lambda _url: data)
    target = tmp_path / "PMC9866638.1.xml"
    md5 = hashlib.md5(data, usedforsecurity=False).hexdigest()
    assert pmc.fetch_one(pmc.BUCKET + "PMC9866638.1/PMC9866638.1.xml", md5, target) == "downloaded"
    assert target.read_bytes() == data
