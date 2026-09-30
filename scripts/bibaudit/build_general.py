"""Build the general (field/layout stratified) sample: PMC + arXiv parts. Resumable; writes WORK/parts/<stratum>.json."""
import json, os, sys
sys.path.insert(0, os.path.dirname(__file__))
from common import *
import sample_pmc as P
import sample_arxiv as A

os.makedirs(f"{WORK}/pdf", exist_ok=True); os.makedirs(f"{WORK}/truth", exist_ok=True); os.makedirs(f"{WORK}/parts", exist_ok=True)
MONTHS = [f"{y}{m:02d}" for y in (2022, 2023, 2024, 2025) for m in range(1, 13)] + [f"2026{m:02d}" for m in range(1, 9)]

PMC = [  # stratum, n, field, layout note, term or id_range, historical
    ("pmc-random", 8, "biomedicine (uniform random PMC ids)", "mixed", {"id_range": (1000, 13000000), "historical": False}),
    ("pmc-als-mnd", 8, "neurology: ALS / motor neuron disease", "mixed", {"term": '("amyotrophic lateral sclerosis"[Title] OR "motor neuron disease"[Title] OR "motor neurone disease"[Title]) AND "open access"[filter]'}),
    ("pmc-neuro-rare", 4, "neurology: rare neuromuscular disease", "mixed", {"term": '("spinal muscular atrophy"[Title] OR "Duchenne"[Title] OR "myasthenia"[Title] OR "Charcot-Marie-Tooth"[Title] OR "Friedreich"[Title] OR "Huntington"[Title]) AND "open access"[filter]'}),
    ("pmc-clinical-trial", 5, "clinical trials", "mixed", {"term": '"randomized controlled trial"[Title] AND "open access"[filter]'}),
    ("pmc-chemistry", 4, "chemistry", "mixed", {"term": '("ACS Omega"[Journal] OR "RSC Adv"[Journal] OR "Beilstein J Org Chem"[Journal]) AND "open access"[filter]'}),
    ("pmc-materials", 4, "materials science", "mixed", {"term": '("Materials (Basel)"[Journal] OR "Nanomaterials (Basel)"[Journal] OR "Polymers (Basel)"[Journal]) AND "open access"[filter]'}),
    ("pmc-superscript", 4, "biomedicine, superscript-numeric journals (Nat Commun / Sci Rep)", "superscript numeric", {"term": '("Nat Commun"[Journal] OR "Sci Rep"[Journal]) AND "open access"[filter]'}),
]
PMC_SCAN = [
    ("pmc-scanned-with-refs", 4, "scanned/OCR'd 1968-1985 articles (Elsevier back-file in PMC; no CC licence, PMC OA-subset membership only)", "scanned page images + OCR text layer", {"term": '1968:1985[pdat] AND "open access"[filter]', "require_license": False, "need_refs": 5}),
    ("pmc-scanned-historical", 3, "scanned/OCR'd 1900-1960 articles (historical OCR, expect no reference list)", "scanned, footnotes at most", {"term": '1900:1960[pdat] AND "open access"[filter] AND review[Title]', "historical": True, "require_license": True}),
]
ARX = [  # stratum, n, field, cats
    ("arxiv-math", 5, "mathematics", ["math.NT", "math.PR", "math.AP", "math.CO", "math.AG", "math.DG", "math.OC", "math.ST"]),
    ("arxiv-physics", 6, "physics / astronomy", ["cond-mat.mes-hall", "cond-mat.str-el", "hep-th", "hep-ph", "quant-ph", "astro-ph.CO", "astro-ph.GA", "physics.optics", "cond-mat.mtrl-sci"]),
    ("arxiv-cs", 5, "computer science / ML", ["cs.LG", "cs.CL", "cs.CV", "cs.CR", "cs.DS", "cs.SE"]),
    ("arxiv-econ-fin", 4, "economics / finance", ["econ.GN", "econ.EM", "q-fin.GN", "q-fin.PM", "q-fin.ST", "econ.TH"]),
    ("arxiv-social", 3, "social science / society", ["physics.soc-ph", "cs.CY", "cs.SI", "stat.AP"]),
]

def save_pdf(pid, pdf):
    p = f"{WORK}/pdf/{pid}.pdf"
    open(p, "wb").write(pdf)
    return p

def run_pmc(only=None, strata=None, need_refs=1):
    for stratum, n, field, layout, kw in (strata or PMC):
        if only and stratum not in only: continue
        out = f"{WORK}/parts/{stratum}.json"
        if os.path.exists(out): log("skip", stratum); continue
        log("PMC stratum", stratum)
        kw = dict(kw); nr = kw.pop('need_refs', need_refs)
        acc, rej, pool = P.draw(stratum, n, need_refs=nr, **kw)
        items = []
        for i, a in enumerate(acc, 1):
            rec = a["rec"]; pid = f"{stratum}-{i:02d}"
            try:
                pdf = P.fetch_pdf(rec["base"])
            except Exception as ex:
                rej.append({"pmcid": a["pmcid"], "why": f"pdf-fetch:{ex}"}); continue
            if not pdf.startswith(b"%PDF"):
                rej.append({"pmcid": a["pmcid"], "why": "not-pdf"}); continue
            save_pdf(pid, pdf)
            json.dump({"kind": "jats", "n": len(rec["refs"]), "refs": rec["refs"]}, open(f"{WORK}/truth/{pid}.json", "w"), ensure_ascii=False)
            m = rec["meta"]
            items.append({"id": pid, "stratum": stratum, "field": field, "layout_hint": layout, "source": "PMC OA (AWS pmc-oa-opendata)",
                          "pmcid": a["pmcid"], "doi": m.get("doi"), "title": m.get("title"), "citation": m.get("citation"),
                          "license": m.get("license_code"), "url": f"https://pmc.ncbi.nlm.nih.gov/articles/{a['pmcid']}/",
                          "pdf_url": rec["base"] + ".pdf", "pdf_bytes": len(pdf), "pdf_sha256": sha256(pdf),
                          "truth_kind": "jats", "truth_n": len(rec["refs"]), "rank_in_shuffled_pool": a["rank"],
                          "historical_ocr": bool(m.get("is_historical_ocr"))})
        json.dump({"stratum": stratum, "seed": f"{SEED}:{stratum}", "pool": pool, "items": items, "rejected_draws": rej,
                   "date": time.strftime("%Y-%m-%d")}, open(out, "w"), ensure_ascii=False, indent=1)
        log("done", stratum, len(items), "rejected", len(rej))

def run_arxiv(only=None):
    for stratum, n, field, cats in ARX:
        if only and stratum not in only: continue
        out = f"{WORK}/parts/{stratum}.json"
        if os.path.exists(out): log("skip", stratum); continue
        log("arXiv stratum", stratum)
        acc, rej = A.draw(stratum, n, cats, MONTHS)
        items = []
        for i, a in enumerate(acc, 1):
            pid = f"{stratum}-{i:02d}"
            save_pdf(pid, a["pdf"])
            json.dump({"kind": "bbl", "n": a["truth_n"], "detail": a["truth_kind"]}, open(f"{WORK}/truth/{pid}.json", "w"))
            items.append({"id": pid, "stratum": stratum, "field": field, "layout_hint": "LaTeX (arXiv)", "source": "arXiv",
                          "arxiv_id": a["arxiv_id"], "version": a["version"], "title": a["title"], "license": a["license"],
                          "url": f"https://arxiv.org/abs/{a['arxiv_id']}v{a['version']}", "pdf_url": f"https://arxiv.org/pdf/{a['arxiv_id']}v{a['version']}",
                          "pdf_bytes": len(a["pdf"]), "pdf_sha256": sha256(a["pdf"]), "truth_kind": "bbl", "truth_n": a["truth_n"],
                          "truth_detail": a["truth_kind"], "draw": {k: a[k] for k in ("draw", "cat", "ym", "offset", "total")}})
        json.dump({"stratum": stratum, "seed": f"{SEED}:{stratum}", "cats": cats, "months": [MONTHS[0], MONTHS[-1]], "items": items,
                   "rejected_draws": rej, "date": time.strftime("%Y-%m-%d")}, open(out, "w"), ensure_ascii=False, indent=1)
        log("done", stratum, len(items), "rejected", len(rej))

if __name__ == "__main__":
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    if which in ("pmc", "all"): run_pmc()
    if which in ("scan", "all"): run_pmc(strata=PMC_SCAN, need_refs=0)
    if which in ("arxiv", "all"): run_arxiv()
