# /// script
# dependencies = ["pymupdf"]
# ///
"""Find real supplementary PDFs in the PMC OA bucket for papers already in the sample; report which contain a reference list heading."""
import glob, json, os, re, sys
sys.path.insert(0, os.path.dirname(__file__))
from common import *
import pymupdf
REPO = "/home/user/pdftextract-audit"
out = []
for p in sorted(glob.glob(f"{REPO}/docs/bibaudit/parts/*.json")):
    for it in json.load(open(p))["items"]:
        if not it.get("pmcid"):
            continue
        base = it["pdf_url"].rsplit("/", 1)[0]
        pref = base.split(".amazonaws.com/")[1] + "/"
        x = get(f"https://pmc-oa-opendata.s3.amazonaws.com/?list-type=2&prefix={pref}")[0]
        for k, sz in re.findall(r"<Key>([^<]*)</Key>.*?<Size>(\d+)</Size>", x):
            if k.lower().endswith(".pdf") and k.count("/") == 1 and "-S" in k or (k.lower().endswith(".pdf") and not k.endswith(pref.rstrip('/') + ".pdf") and k.count('/') == 1):
                out.append({"id": it["id"], "key": k, "size": int(sz)})
print(len(out))
res = []
for o in sorted(out, key=lambda o: o["size"])[:25]:
    if o["size"] > 6_000_000:
        continue
    try:
        data = get("https://pmc-oa-opendata.s3.amazonaws.com/" + o["key"], binary=True, max_bytes=8_000_000)[0]
        d = pymupdf.open(stream=data, filetype="pdf")
        txt = "\n".join(d[i].get_text() for i in range(d.page_count))
        has = bool(re.search(r"(?im)^\s*(references|bibliography|literature cited)\s*$", txt))
        nb = len(re.findall(r"(?m)^\s*(\[\d+\]|\d+\.)\s", txt))
        res.append({**o, "pages": d.page_count, "ref_heading": has, "numbered_lines": nb})
        print(o["id"], o["key"], o["size"], d.page_count, has, nb, flush=True)
    except Exception as e:
        print("err", o, e)
json.dump(res, open(f"{WORK}/supp_candidates.json", "w"), indent=1)
