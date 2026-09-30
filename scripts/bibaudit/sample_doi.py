# /// script
# dependencies = ["pymupdf"]
# ///
"""DOI-based sampling (DOAJ / Crossref pools) with Crossref `reference` ground truth and Unpaywall PDF location.

Pool: DOAJ article search at seeded random ranks (window: first 9,900 hits of the query), or a Crossref `sample=`
draw (server-side random; the drawn DOIs are recorded because Crossref's sample is not seedable).
Selection: shuffle pool with Random(f"{SEED}:{stratum}"), accept first N that (1) have Crossref reference count >= 1,
(2) have an Unpaywall OA location whose PDF downloads (%PDF, <= 30 MB) and whose first pages contain the title words.
All rejects are logged with a reason (including blocked hosts).
"""
import json, os, re, sys, urllib.parse, unicodedata
sys.path.insert(0, os.path.dirname(__file__))
from common import *
import pymupdf as fitz

EMAIL = "01.amusing_cozies@icloud.com"

def norm_words(s):
    s = unicodedata.normalize("NFKD", s).lower()
    s = "".join(c for c in s if not unicodedata.combining(c))
    return [w for w in re.findall(r"[a-z0-9]{3,}", s)]

def crossref(doi):
    j = cgetj("https://api.crossref.org/works/" + urllib.parse.quote(doi, safe="/") + f"?mailto={EMAIL}")["message"]
    return j

def crossref_truth(msg):
    refs = []
    for r in msg.get("reference", []) or []:
        refs.append({"key": r.get("key"), "doi": (r.get("DOI") or "").lower() or None, "author": r.get("author"),
                     "title": r.get("article-title") or r.get("volume-title") or "", "source": r.get("journal-title"),
                     "year": int(r["year"]) if str(r.get("year", "")).isdigit() else None, "text": r.get("unstructured", "")})
    return refs

def unpaywall(doi):
    return cgetj(f"https://api.unpaywall.org/v2/{urllib.parse.quote(doi, safe='/')}?email={EMAIL}")

def title_ok(pdf_bytes, title):
    try:
        d = fitz.open(stream=pdf_bytes, filetype="pdf")
    except Exception:
        return False, 0, 0
    npg = d.page_count
    txt = " ".join(d[i].get_text() for i in range(min(3, npg)))
    tw = set(norm_words(title)); pw = set(norm_words(txt))
    if not tw:
        return True, npg, 1.0
    frac = len(tw & pw) / len(tw)
    return frac >= 0.6, npg, frac

def try_pdf(doi, title, log_rej):
    try:
        up = unpaywall(doi)
    except Exception as e:
        log_rej.append({"doi": doi, "why": f"unpaywall:{type(e).__name__}"}); return None
    if not up.get("is_oa"):
        log_rej.append({"doi": doi, "why": "not-oa"}); return None
    locs = []
    if up.get("best_oa_location"): locs.append(up["best_oa_location"])
    locs += [l for l in up.get("oa_locations", []) if l not in locs]
    for l in locs[:5]:
        u = l.get("url_for_pdf")
        if not u:
            continue
        host = urllib.parse.urlparse(u).netloc
        try:
            data, ct, st, final = get(u, binary=True, max_bytes=30_000_000, timeout=90, retries=2)
        except Exception as e:
            log_rej.append({"doi": doi, "why": f"pdf-fetch:{host}:{str(e)[:60]}"}); continue
        if not data.startswith(b"%PDF"):
            log_rej.append({"doi": doi, "why": f"not-pdf:{host}"}); continue
        ok, npg, frac = title_ok(data, title)
        if not ok:
            log_rej.append({"doi": doi, "why": f"title-mismatch:{host}:{frac:.2f}"}); continue
        return {"pdf": data, "pdf_url": u, "host": host, "license": l.get("license") or up.get("best_oa_location", {}).get("license"),
                "pages": npg, "version": l.get("version"), "host_type": l.get("host_type")}
    log_rej.append({"doi": doi, "why": "no-usable-pdf-location"})
    return None

def doaj_pool(stratum, query, k):
    """k draws; each draw = (uniform year 2015-2025, uniform rank within the first 1,000 hits for that year).
    DOAJ refuses page*pageSize > 1000, so 1,000 hits per (query, year) is the reachable window."""
    r = rng(stratum)
    out, totals = [], {}
    base_q = query.replace(" AND bibjson.year:[2015 TO 2025]", "")
    for _ in range(k):
        year = r.randint(2015, 2025)
        q = f"({base_q}) AND bibjson.year:{year}"
        base = "https://doaj.org/api/search/articles/" + urllib.parse.quote(q)
        if year not in totals:
            totals[year] = cgetj(base + "?pageSize=1&page=1")["total"]
        lim = min(totals[year], 1000)
        if lim == 0:
            continue
        rk = r.randrange(lim)
        page, off = rk // 100 + 1, rk % 100
        j = cgetj(base + f"?pageSize=100&page={page}")
        try:
            h = j["results"][off]
        except IndexError:
            continue
        b = h["bibjson"]
        doi = next((i["id"] for i in b.get("identifier", []) if i["type"] == "doi"), None)
        if not doi:
            continue
        out.append({"doi": doi.lower(), "title": b.get("title", ""), "journal": b.get("journal", {}).get("title"),
                    "lang": b.get("journal", {}).get("language"), "country": b.get("journal", {}).get("country"),
                    "year": b.get("year"), "draw_year": year, "rank": rk, "subjects": [s.get("term") for s in b.get("subject", [])]})
    return totals, out

def crossref_pool(stratum, filt, k=100):
    u = f"https://api.crossref.org/works?sample={k}&filter={filt}&select=DOI,title,container-title,issued&mailto={EMAIL}"
    # sample is not cached (server-side random): cache the response once so a rerun reuses the same pool
    j = cgetj(u)
    tot = j["message"].get("total-results")
    out = []
    for it in j["message"]["items"]:
        out.append({"doi": it["DOI"].lower(), "title": (it.get("title") or [""])[0], "journal": (it.get("container-title") or [""])[0],
                    "year": (it.get("issued", {}).get("date-parts") or [[None]])[0][0]})
    return tot, out

def draw(stratum, n, pool, min_refs=1):
    r = rng(stratum, "accept")
    pool = list(pool)
    r.shuffle(pool)
    acc, rej = [], []
    for k, c in enumerate(pool):
        if len(acc) >= n:
            break
        doi = c["doi"]
        if any(a["doi"] == doi for a in acc):
            continue
        try:
            msg = crossref(doi)
        except Exception as e:
            rej.append({"doi": doi, "why": f"crossref:{type(e).__name__}"}); continue
        refs = crossref_truth(msg)
        if len(refs) < min_refs:
            rej.append({"doi": doi, "why": f"crossref-refs={len(refs)}"}); continue
        title = (msg.get("title") or [c.get("title", "")])[0] or c.get("title", "")
        got = try_pdf(doi, title, rej)
        if not got:
            continue
        acc.append({"doi": doi, "title": title, "journal": (msg.get("container-title") or [""])[0], "pool_rank": k,
                    "truth_refs": refs, "crossref_reference_count": msg.get("reference-count"), "language": msg.get("language"),
                    "type": msg.get("type"), **got, "pool_meta": c})
    return acc, rej
