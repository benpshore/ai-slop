# /// script
# dependencies = ["pymupdf"]
# ///
"""Build the DOI-based strata (DOAJ pools, Crossref publisher pools). Resumable; writes WORK/parts/<stratum>.json."""
import json, os, sys
sys.path.insert(0, os.path.dirname(__file__))
from common import *
import sample_doi as D

os.makedirs(f"{WORK}/pdf", exist_ok=True); os.makedirs(f"{WORK}/truth", exist_ok=True); os.makedirs(f"{WORK}/parts", exist_ok=True)
Y = " AND bibjson.year:[2015 TO 2025]"
DOAJ = [  # stratum, n, field, layout hint, query
    ("doaj-economics", 5, "economics", "author-year (Harvard/APA) expected", "bibjson.subject.term:Economics" + Y),
    ("doaj-social-science", 4, "social science (sociology / political science)", "author-year expected", '(bibjson.subject.term:Sociology OR bibjson.subject.term:"Political science")' + Y),
    ("doaj-history", 4, "history / humanities", "footnotes or author-year", "bibjson.subject.term:History" + Y),
    ("doaj-law", 4, "law", "footnote-style expected", "bibjson.subject.term:Law" + Y),
    ("doaj-law-manual", 4, "law, no Crossref references deposited (manual count)", "footnote-style expected", "bibjson.subject.term:Law" + Y, 0),
    ("doaj-lang-es", 2, "non-English: Spanish", "mixed", "bibjson.journal.language:ES" + Y),
    ("doaj-lang-pt", 2, "non-English: Portuguese", "mixed", "bibjson.journal.language:PT" + Y),
    ("doaj-lang-fr", 2, "non-English: French", "mixed", "bibjson.journal.language:FR" + Y),
    ("doaj-lang-de", 1, "non-English: German", "mixed", "bibjson.journal.language:DE" + Y),
    ("doaj-lang-ru", 1, "non-English: Russian (Cyrillic)", "mixed", "bibjson.journal.language:RU" + Y),
]
CR = [  # stratum, n, field, layout, filter
    ("cr-elife", 3, "biomedicine (eLife)", "author-year, one column", "prefix:10.7554,has-references:true,from-pub-date:2019-01-01,type:journal-article"),
    ("cr-frontiers", 3, "neuroscience/medicine (Frontiers)", "author-year, one column", "prefix:10.3389,has-references:true,from-pub-date:2019-01-01,type:journal-article"),
    ("cr-plos", 3, "biomedicine/social (PLOS)", "numeric [1]", "prefix:10.1371,has-references:true,from-pub-date:2019-01-01,type:journal-article"),
]

def finish(stratum, field, layout, n, pool_desc, pool, source, min_refs=1):
    out = f"{WORK}/parts/{stratum}.json"
    if os.path.exists(out): log("skip", stratum); return
    log("stratum", stratum, "pool", len(pool))
    acc, rej = D.draw(stratum, n, pool, min_refs=min_refs)
    items = []
    for i, a in enumerate(acc, 1):
        pid = f"{stratum}-{i:02d}"
        open(f"{WORK}/pdf/{pid}.pdf", "wb").write(a["pdf"])
        json.dump({"kind": "crossref" if min_refs else "manual", "n": len(a["truth_refs"]), "refs": a["truth_refs"]}, open(f"{WORK}/truth/{pid}.json", "w"), ensure_ascii=False)
        items.append({"id": pid, "stratum": stratum, "field": field, "layout_hint": layout, "source": source, "doi": a["doi"],
                      "title": a["title"], "journal": a["journal"], "license": a["license"], "url": "https://doi.org/" + a["doi"],
                      "pdf_url": a["pdf_url"], "pdf_host": a["host"], "pdf_bytes": len(a["pdf"]), "pdf_sha256": sha256(a["pdf"]),
                      "pages": a["pages"], "language": a["language"], "crossref_type": a["type"],
                      "truth_kind": "crossref" if min_refs else "manual", "truth_n": len(a["truth_refs"]) if min_refs else None, "pool_rank": a["pool_rank"], "version": a["version"]})
    json.dump({"stratum": stratum, "seed": f"{SEED}:{stratum}", "pool_desc": pool_desc, "pool": pool, "items": items,
               "rejected_draws": rej, "date": time.strftime("%Y-%m-%d")}, open(out, "w"), ensure_ascii=False, indent=1)
    log("done", stratum, len(items), "rejected", len(rej))

if __name__ == "__main__":
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    if which in ("doaj", "all"):
        for row in DOAJ:
            stratum, n, field, layout, q = row[:5]
            min_refs = row[5] if len(row) > 5 else 1
            if os.path.exists(f"{WORK}/parts/{stratum}.json"): continue
            tot, pool = D.doaj_pool(stratum, q, 120 if stratum == "doaj-law" else 40)
            finish(stratum, field, layout, n, {"doaj_query": q, "total_per_year": tot, "window": "first 1000 hits per year"}, pool, "DOAJ + Unpaywall + Crossref", min_refs)
    if which in ("cr", "all"):
        for stratum, n, field, layout, f in CR:
            tot, pool = D.crossref_pool(stratum, f, 100)
            finish(stratum, field, layout, n, {"crossref_filter": f, "total_results": tot, "sample": 100}, pool, "Crossref sample + Unpaywall")
