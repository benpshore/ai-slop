# /// script
# dependencies = ["pymupdf"]
# ///
"""Theses (Zenodo) and books (OAPEN): no independent machine-readable reference list exists, so truth is 'manual count'."""
import json, os, sys, urllib.parse
sys.path.insert(0, os.path.dirname(__file__))
from common import *
import pymupdf

for d in ("pdf", "truth", "parts"):
    os.makedirs(f"{WORK}/{d}", exist_ok=True)

def pages_of(b):
    try:
        return pymupdf.open(stream=b, filetype="pdf").page_count
    except Exception:
        return 0

def theses(n=4, tries=60):
    stratum = "thesis-zenodo"
    r = rng(stratum)
    q = "metadata.resource_type.id:publication-thesis"
    acc, rej = [], []
    for k in range(tries):
        if len(acc) >= n: break
        rk = r.randrange(10000)
        page, off = rk // 25 + 1, rk % 25
        u = f"https://zenodo.org/api/records?size=25&page={page}&sort=oldest&q=" + urllib.parse.quote(q, safe=":")
        try:
            j = cgetj(u)
            h = j["hits"]["hits"][off]
        except Exception as e:
            rej.append({"rank": rk, "why": f"api:{str(e)[:60]}"}); continue
        md = h["metadata"]
        lic = (md.get("license") or {}).get("id")
        pdfs = [f for f in h.get("files", []) if f["key"].lower().endswith(".pdf") and f["size"] <= 30_000_000]
        base = {"rank": rk, "zenodo_id": h["id"], "title": md.get("title"), "language": md.get("language")}
        if not lic: rej.append({**base, "why": "no-license"}); continue
        if not pdfs: rej.append({**base, "why": "no-pdf<=30MB"}); continue
        f = max(pdfs, key=lambda f: f["size"])
        try:
            data = get(f["links"]["self"], binary=True, max_bytes=30_000_000, timeout=180)[0]
        except Exception as e:
            rej.append({**base, "why": f"download:{str(e)[:60]}"}); continue
        np_ = pages_of(data)
        if not data.startswith(b"%PDF") or np_ < 40:
            rej.append({**base, "why": f"not-thesis-sized pages={np_}"}); continue
        acc.append({**base, "license": lic, "pdf": data, "pages": np_, "file": f["key"], "doi": md.get("doi") or h.get("doi"),
                    "url": h["links"]["self_html"]})
    items = []
    for i, a in enumerate(acc, 1):
        pid = f"{stratum}-{i:02d}"
        open(f"{WORK}/pdf/{pid}.pdf", "wb").write(a["pdf"])
        items.append({"id": pid, "stratum": stratum, "field": "thesis (Zenodo)", "layout_hint": "thesis, long list", "source": "Zenodo",
                      "title": a["title"], "doi": a["doi"], "license": a["license"], "url": a["url"], "pdf_url": a["url"] + "/files/" + urllib.parse.quote(a["file"]),
                      "pdf_bytes": len(a["pdf"]), "pdf_sha256": sha256(a["pdf"]), "pages": a["pages"], "language": a["language"],
                      "truth_kind": "manual", "truth_n": None, "rank": a["rank"]})
    json.dump({"stratum": stratum, "seed": f"{SEED}:{stratum}", "query": q, "window": "first 10,000 by oldest", "items": items,
               "rejected_draws": rej, "date": time.strftime("%Y-%m-%d")}, open(f"{WORK}/parts/{stratum}.json", "w"), ensure_ascii=False, indent=1)
    log("theses", len(items), "rej", len(rej))

def books(stratum, n, lo, hi, maxmb, tries=120):
    r = rng(stratum)
    acc, rej = [], []
    for k in range(tries):
        if len(acc) >= n: break
        off = r.randrange(30000)
        u = f"https://library.oapen.org/rest/search?query={urllib.parse.quote('dc.type:book')}&limit=1&offset={off}&expand=bitstreams,metadata"
        try:
            it = cgetj(u)[0]
        except Exception as e:
            rej.append({"offset": off, "why": f"api:{str(e)[:60]}"}); continue
        md = {}
        for m in it["metadata"]:
            md.setdefault(m["key"], m["value"])
        try:
            pg = int(md.get("oapen.pages", "0"))
        except ValueError:
            pg = 0
        pdfs = [b for b in it["bitstreams"] if b["name"].lower().endswith(".pdf") and b.get("mimeType") == "application/pdf"]
        base = {"offset": off, "title": it["name"], "handle": it.get("handle"), "pages_meta": pg, "language": md.get("dc.language")}
        if not (lo <= pg <= hi): rej.append({**base, "why": f"pages {pg} outside [{lo},{hi}]"}); continue
        if not pdfs: rej.append({**base, "why": "no-pdf"}); continue
        b = pdfs[0]
        if b["sizeBytes"] > maxmb * 1_000_000: rej.append({**base, "why": f"pdf {b['sizeBytes']} bytes > {maxmb} MB"}); continue
        try:
            data = get("https://library.oapen.org" + b["retrieveLink"], binary=True, max_bytes=maxmb * 1_000_000 + 1, timeout=300)[0]
        except Exception as e:
            rej.append({**base, "why": f"download:{str(e)[:60]}"}); continue
        if not data.startswith(b"%PDF"): rej.append({**base, "why": "not-pdf"}); continue
        acc.append({**base, "pdf": data, "pages": pages_of(data), "doi": md.get("oapen.identifier.doi"),
                    "license": md.get("dc.rights.uri") or md.get("oapen.rights") or "OAPEN open access (licence URL not in record)",
                    "uri": md.get("dc.identifier.uri"), "publisher": md.get("publisher.name")})
    items = []
    for i, a in enumerate(acc, 1):
        pid = f"{stratum}-{i:02d}"
        open(f"{WORK}/pdf/{pid}.pdf", "wb").write(a["pdf"])
        items.append({"id": pid, "stratum": stratum, "field": "book (OAPEN)", "layout_hint": "book", "source": "OAPEN", "title": a["title"],
                      "doi": a["doi"], "license": a["license"], "url": a["uri"], "pdf_bytes": len(a["pdf"]), "pdf_sha256": sha256(a["pdf"]),
                      "pages": a["pages"], "language": a["language"], "publisher": a["publisher"], "truth_kind": "manual", "truth_n": None})
    json.dump({"stratum": stratum, "seed": f"{SEED}:{stratum}", "items": items, "rejected_draws": rej, "date": time.strftime("%Y-%m-%d")},
              open(f"{WORK}/parts/{stratum}.json", "w"), ensure_ascii=False, indent=1)
    log(stratum, len(items), "rej", len(rej))

if __name__ == "__main__":
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    if which in ("thesis", "all"): theses()
    if which in ("books", "all"):
        books("book-oapen", 3, 150, 450, 40)
    if which in ("book500", "all"):
        books("adv-book-500p", 1, 500, 800, 60)
