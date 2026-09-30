"""PMC Open Access sampling (AWS bucket pmc-oa-opendata: publisher PDF + JATS XML + JSON per article).

Every stratum: build a pool (Entrez esearch, or seeded uniform random PMC IDs), shuffle the pool with
Random(f"{SEED}:{stratum}"), walk the shuffled pool in order, accept the first N that satisfy the
eligibility rules; log every rejected draw with the reason. The tool is not run until the manifest is frozen.
"""
import json, os, sys, urllib.parse
sys.path.insert(0, os.path.dirname(__file__))
from common import *
from jats import parse_refs

S3 = "https://pmc-oa-opendata.s3.amazonaws.com"
EUT = "https://eutils.ncbi.nlm.nih.gov/entrez/eutils/esearch.fcgi"

def esearch(term, retmax=5000):
    u = f"{EUT}?db=pmc&retmode=json&retmax={retmax}&term={urllib.parse.quote(term)}"
    j = cgetj(u)
    return int(j["esearchresult"]["count"]), j["esearchresult"]["idlist"]

def load_record(pid):
    """Return (json_meta, xml_text, version) or raise. Tries .1 then .2/.3."""
    for v in (1, 2, 3):
        base = f"{S3}/PMC{pid}.{v}/PMC{pid}.{v}"
        if not exists(base + ".json"):
            continue
        meta = json.loads(get(base + ".json")[0])
        return meta, base, v
    raise LookupError("not in OA bucket")

def eligible(pid, need_refs=1, min_pages=None, max_pdf=30_000_000, historical=None, require_license=True):
    try:
        meta, base, v = load_record(pid)
    except LookupError:
        return None, "not-in-bucket"
    if not meta.get("is_pmc_openaccess"):
        return None, "not-oa"
    if historical is not None and bool(meta.get("is_historical_ocr")) != historical:
        return None, "historical-mismatch"
    if require_license and not meta.get("license_code"):
        return None, "no-open-license"
    if not meta.get("pdf_url"):
        return None, "no-pdf"
    try:
        xml = get(base + ".xml")[0]
        refs = parse_refs(xml)
    except Exception as e:
        return None, f"xml-fail:{type(e).__name__}"
    if len(refs) < need_refs:
        return None, f"refs<{need_refs} ({len(refs)})"
    return {"meta": meta, "base": base, "version": v, "xml": xml, "refs": refs}, "ok"

def fetch_pdf(base):
    return get(base + ".pdf", binary=True, max_bytes=40_000_000, timeout=120)[0]

def draw(stratum, n, term=None, id_range=None, historical=None, need_refs=1, pool_max=5000, extra=None, require_license=True):
    r = rng(stratum)
    if term:
        total, pool = esearch(term, pool_max)
        r.shuffle(pool)
        pool = [int(x) for x in pool]
        pooldesc = {"term": term, "count": total, "pool_used": len(pool)}
    else:
        lo, hi = id_range
        pool = [r.randint(lo, hi) for _ in range(4000)]
        pooldesc = {"uniform_random_ids": [lo, hi]}
    acc, rej = [], []
    for k, pid in enumerate(pool):
        if len(acc) >= n:
            break
        rec, why = eligible(pid, need_refs=need_refs, historical=historical, require_license=require_license)
        if rec is None:
            rej.append({"pmcid": f"PMC{pid}", "rank": k, "why": why})
            continue
        acc.append({"pmcid": f"PMC{pid}", "rank": k, "rec": rec})
    return acc, rej, pooldesc
