"""Deterministic sample of PubMed Central Open Access articles, and its fetcher.

Two modes, both stdlib only and both meant to run in GitHub Actions
(docs/PMC_EVAL.md):

    python3 scripts/pmc_sample.py --seed 20260930 --target 200 --out out/pmc-manifest.json
    python3 scripts/pmc_sample.py --fetch --manifest corpus/pmc-manifest.json --cache DIR

Sampling lists the anonymous `pmc-oa-opendata` S3 bucket from seeded random
PMCID start points, reads each candidate's metadata JSON and JATS XML, keeps
licensed born-digital articles with a usable reference list, caps the count
per journal, and writes a manifest that pins every PDF and XML by URL and MD5.
Fetching downloads the pinned files into a cache directory and verifies them.
"""

import argparse
import hashlib
import json
import random
import re
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
import xml.etree.ElementTree as ET
from collections import Counter
from concurrent.futures import ThreadPoolExecutor
from datetime import UTC, datetime
from pathlib import Path

BUCKET = "https://pmc-oa-opendata.s3.amazonaws.com/"
S3_PREFIX = "s3://pmc-oa-opendata/"
USER_AGENT = "pdftextract-pmc-eval/1 (GitHub Actions; bibliography measurement)"
MANIFEST_VERSION = 1

# PMCIDs roughly span 2012 (PMC3.2M) to 2026 (PMC12.8M). Starts are drawn
# uniformly over that numeric span; because S3 lists keys as strings, a
# 7-digit start also yields 8-digit neighbours, which is harmless.
ID_MIN = 3_200_000
ID_MAX = 12_800_000
LISTINGS_PER_ROUND = 12
LISTING_SIZE = 12
PICKS_PER_LISTING = 3
MIN_REFS = 5
MAX_PER_JOURNAL = 4
ALLOWED_TYPES = {
    "research-article",
    "review-article",
    "case-report",
    "brief-report",
    "systematic-review",
    "meta-analysis",
    "protocol",
    "clinical-trial",
    "letter",
    "other",
}


class FetchError(Exception):
    """A download that failed after every retry."""


def log(message: str) -> None:
    print(message, file=sys.stderr, flush=True)


def fetch_bytes(url: str, retries: int = 5, timeout: float = 60.0) -> bytes | None:
    """GET `url`; `None` on 404, raise `FetchError` after `retries` other failures."""
    delay = 1.0
    last = "no attempt"
    for attempt in range(retries):
        request = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
        try:
            # S310: the scheme is fixed to https by BUCKET; no user-supplied scheme.
            with urllib.request.urlopen(request, timeout=timeout) as response:  # noqa: S310
                return response.read()
        except urllib.error.HTTPError as err:
            if err.code == 404:
                return None
            last = f"HTTP {err.code}"
        except OSError as err:
            last = str(err)
        if attempt + 1 < retries:
            time.sleep(delay)
            delay = min(delay * 2, 30.0)
    raise FetchError(f"{url}: {last}")


def parse_xml(data: bytes) -> ET.Element:
    # S314: the input is the public PMC bucket's own JATS/S3 XML, parsed for
    # a few text fields; no entity expansion of untrusted DTDs is requested.
    return ET.fromstring(data)  # noqa: S314


def list_prefixes(start_after: str, max_keys: int = LISTING_SIZE) -> list[str]:
    """Article prefixes (`PMC123.1/`) that sort after `start_after`."""
    query = urllib.parse.urlencode(
        {
            "list-type": "2",
            "delimiter": "/",
            "max-keys": str(max_keys),
            "start-after": start_after,
        }
    )
    data = fetch_bytes(f"{BUCKET}?{query}")
    if data is None:
        return []
    root = parse_xml(data)
    return [element.text for element in root.iter("{*}Prefix") if element.text]


def split_s3_url(url: str) -> tuple[str, str | None]:
    """`s3://bucket/key?md5=...` -> (https URL without query, md5)."""
    if not url.startswith(S3_PREFIX):
        raise ValueError(f"unexpected URL {url}")
    rest = url[len(S3_PREFIX) :]
    key, _, query = rest.partition("?")
    md5 = urllib.parse.parse_qs(query).get("md5", [None])[0]
    return BUCKET + key, md5


def text_of(element: ET.Element | None) -> str | None:
    if element is None:
        return None
    text = " ".join("".join(element.itertext()).split())
    return text or None


def pub_year(article: ET.Element) -> int | None:
    """Year of the earliest-preferred pub-date (epub, ppub, then any)."""
    dates = list(article.iter("pub-date"))
    order = ["epub", "ppub", "collection"]

    def rank(date: ET.Element) -> int:
        kind = date.get("pub-type") or date.get("date-type") or ""
        return order.index(kind) if kind in order else len(order)

    for date in sorted(dates, key=rank):
        year = text_of(date.find("year"))
        if year and re.fullmatch(r"\d{4}", year):
            return int(year)
    return None


def article_facts(xml_bytes: bytes) -> dict:
    """Journal, publisher, type, year, DOI and reference count from JATS XML."""
    article = parse_xml(xml_bytes)
    if article.tag != "article":
        found = article.find(".//article")
        if found is None:
            raise ValueError("no <article> element")
        article = found
    back = article.find("back")
    ref_count = sum(1 for _ in back.iter("ref")) if back is not None else 0
    doi = None
    for identifier in article.iter("article-id"):
        if identifier.get("pub-id-type") == "doi":
            doi = text_of(identifier)
            break
    return {
        "journal": text_of(next(article.iter("journal-title"), None)),
        "publisher": text_of(next(article.iter("publisher-name"), None)),
        "article_type": article.get("article-type"),
        "year": pub_year(article),
        "doi": doi,
        "ref_count": ref_count,
    }


def inspect_candidate(prefix: str) -> tuple[str, dict | None]:
    """Return (reason, item): `item` is set only when the candidate qualifies."""
    base = prefix.rstrip("/")
    data = fetch_bytes(f"{BUCKET}{base}/{base}.json")
    if data is None:
        return "no-json", None
    meta = json.loads(data)
    if not meta.get("is_pmc_openaccess"):
        return "not-openaccess", None
    if not meta.get("pdf_url"):
        return "no-pdf", None
    if meta.get("is_historical_ocr"):
        return "historical-ocr", None
    if meta.get("is_retracted"):
        return "retracted", None
    license_code = str(meta.get("license_code") or "")
    if not license_code.upper().startswith("CC"):
        return "license", None
    if not meta.get("xml_url"):
        return "no-xml", None
    xml_url, xml_md5 = split_s3_url(meta["xml_url"])
    pdf_url, pdf_md5 = split_s3_url(meta["pdf_url"])
    xml_bytes = fetch_bytes(xml_url)
    if xml_bytes is None:
        return "xml-missing", None
    try:
        facts = article_facts(xml_bytes)
    except (ET.ParseError, ValueError):
        return "xml-unparsable", None
    if facts["ref_count"] < MIN_REFS:
        return "few-refs", None
    if facts["article_type"] not in ALLOWED_TYPES:
        return "article-type", None
    if not facts["journal"]:
        return "no-journal", None
    item = {
        "pmcid": meta["pmcid"],
        "version": meta.get("version", 1),
        "doi": meta.get("doi") or facts["doi"],
        "pmid": meta.get("pmid"),
        "title": meta.get("title"),
        "journal": facts["journal"],
        "publisher": facts["publisher"],
        "article_type": facts["article_type"],
        "license_code": license_code,
        "is_manuscript": bool(meta.get("is_manuscript")),
        "year": facts["year"],
        "ref_count": facts["ref_count"],
        "pdf_url": pdf_url,
        "pdf_md5": pdf_md5,
        "xml_url": xml_url,
        "xml_md5": xml_md5,
    }
    return "accepted", item


def sample(seed: int, target: int, workers: int, max_candidates: int) -> dict:
    """Seeded sample of up to `target` qualifying articles."""
    rng = random.Random(seed)  # noqa: S311 - reproducible sampling, not security
    accepted: list[dict] = []
    per_journal: Counter[str] = Counter()
    reasons: Counter[str] = Counter()
    seen: set[str] = set()
    tried = 0
    rounds = 0
    while len(accepted) < target and tried < max_candidates:
        rounds += 1
        starts = [f"PMC{rng.randint(ID_MIN, ID_MAX)}" for _ in range(LISTINGS_PER_ROUND)]
        with ThreadPoolExecutor(max_workers=workers) as pool:
            listings = list(pool.map(list_prefixes, starts))
        batch: list[str] = []
        for prefixes in listings:
            fresh = [p for p in prefixes if p not in seen and re.fullmatch(r"PMC\d+\.\d+/", p)]
            picks = rng.sample(fresh, min(PICKS_PER_LISTING, len(fresh)))
            seen.update(picks)
            batch.extend(picks)
        with ThreadPoolExecutor(max_workers=workers) as pool:
            results = list(pool.map(inspect_candidate, batch))
        for reason, item in results:
            tried += 1
            if item is None:
                reasons[reason] += 1
                continue
            journal = item["journal"]
            if per_journal[journal] >= MAX_PER_JOURNAL:
                reasons["journal-cap"] += 1
                continue
            per_journal[journal] += 1
            accepted.append(item)
            reasons["accepted"] += 1
            if len(accepted) >= target:
                break
        log(f"round {rounds}: {len(accepted)}/{target} accepted, {tried} tried, {dict(reasons)}")
    accepted.sort(key=lambda item: (int(item["pmcid"][3:]), item["version"]))
    return {
        "version": MANIFEST_VERSION,
        "generated_at": datetime.now(UTC).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "seed": seed,
        "target": target,
        "candidates_tried": tried,
        "rejections": dict(sorted(reasons.items())),
        "items": accepted,
    }


def manifest_text(manifest: dict) -> str:
    """JSON with one item per line, so diffs and the job log stay readable."""
    head = {key: value for key, value in manifest.items() if key != "items"}
    lines = [json.dumps(head, ensure_ascii=False)[:-1].rstrip() + ","]
    lines.append('  "items": [')
    items = manifest["items"]
    for i, item in enumerate(items):
        comma = "," if i + 1 < len(items) else ""
        lines.append("    " + json.dumps(item, ensure_ascii=False) + comma)
    lines.append("  ]")
    lines.append("}")
    return "\n".join(lines) + "\n"


def md5_of(path: Path) -> str:
    digest = hashlib.md5(usedforsecurity=False)
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def cache_paths(item: dict, cache: Path) -> tuple[Path, Path]:
    pmcid, version = item["pmcid"], item["version"]
    stem = f"{pmcid}.{version}"
    return cache / f"{stem}.pdf", cache / f"{stem}.xml"


def fetch_one(url: str, md5: str | None, target: Path) -> str:
    """Download `url` to `target` unless it is already there with the right MD5."""
    if target.exists() and (md5 is None or md5_of(target) == md5):
        return "cached"
    data = fetch_bytes(url)
    if data is None:
        raise FetchError(f"{url}: not found")
    if md5 is not None:
        actual = hashlib.md5(data, usedforsecurity=False).hexdigest()
        if actual != md5:
            raise FetchError(f"{url}: md5 {actual} != {md5}")
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_bytes(data)
    return "downloaded"


def fetch_item(item: dict, cache: Path) -> list[str]:
    """Fetch an item's PDF and XML; the list holds one error per failed file."""
    pdf_path, xml_path = cache_paths(item, cache)
    pmcid = item["pmcid"]
    errors = []
    for url, md5, target in (
        (item["pdf_url"], item["pdf_md5"], pdf_path),
        (item["xml_url"], item["xml_md5"], xml_path),
    ):
        try:
            outcome = fetch_one(url, md5, target)
        except FetchError as err:
            errors.append(str(err))
            continue
        log(f"{pmcid}: {target.suffix[1:]} {outcome}")
    return errors


def fetch_manifest(manifest: dict, cache: Path, workers: int) -> int:
    """Download every pinned file; returns the number of files that failed."""
    cache.mkdir(parents=True, exist_ok=True)
    with ThreadPoolExecutor(max_workers=workers) as pool:
        outcomes = list(pool.map(lambda item: fetch_item(item, cache), manifest["items"]))
    failures = [error for errors in outcomes for error in errors]
    for error in failures:
        log(f"fetch failed: {error}")
    return len(failures)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--fetch", action="store_true", help="download the pinned manifest")
    parser.add_argument("--list", action="store_true", help="print the manifest PDF cache paths")
    parser.add_argument("--manifest", type=Path, help="manifest to fetch (with --fetch)")
    parser.add_argument("--cache", type=Path, help="download directory (with --fetch)")
    parser.add_argument("--seed", type=int, default=20260930)
    parser.add_argument("--target", type=int, default=200)
    parser.add_argument("--max-candidates", type=int, default=3000)
    parser.add_argument("--workers", type=int, default=4)
    parser.add_argument("--out", type=Path, help="manifest to write (sampling mode)")
    args = parser.parse_args(argv)
    if args.fetch or args.list:
        if args.manifest is None or args.cache is None:
            parser.error("--fetch and --list need --manifest and --cache")
        manifest = json.loads(args.manifest.read_text(encoding="utf-8"))
        if args.list:
            for item in manifest["items"]:
                print(cache_paths(item, args.cache)[0])
            return 0
        failed = fetch_manifest(manifest, args.cache, args.workers)
        count = len(manifest["items"])
        log(f"{count} items, {failed} files failed")
        return 1 if failed else 0
    manifest = sample(args.seed, args.target, args.workers, args.max_candidates)
    text = manifest_text(manifest)
    if args.out is not None:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(text, encoding="utf-8")
    else:
        sys.stdout.write(text)
    count = len(manifest["items"])
    tried = manifest["candidates_tried"]
    log(f"sampled {count} articles from {tried} candidates")
    return 0


if __name__ == "__main__":
    sys.exit(main())
