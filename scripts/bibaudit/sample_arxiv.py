"""arXiv sampling with .bbl / inline thebibliography ground truth.

For each stratum: a category list and a submission-date window. Draw (category, month, offset) triples from
Random(f"{SEED}:{stratum}"), fetch one record through the arXiv API at that offset (sorted by submittedDate),
then require: latest version, not in the tool's own corpus (.corpus-cache or corpus/manifest.json), PDF <= 15 MB,
e-print source available with a .bbl or an inline \\bibitem/\\entry list. Rejected draws are logged.
"""
import gzip, io, json, os, re, sys, tarfile, xml.etree.ElementTree as ET
sys.path.insert(0, os.path.dirname(__file__))
from common import *

NS = {"a": "http://www.w3.org/2005/Atom", "o": "http://a9.com/-/spec/opensearch/1.1/"}
REPO = os.environ.get("BIBAUDIT_REPO", "/home/user/pdftextract-audit")
CORPUS_CACHE = "/home/user/pdftextract/.corpus-cache"

def own_corpus_ids():
    ids = set()
    if os.path.isdir(CORPUS_CACHE):
        for n in os.listdir(CORPUS_CACHE):
            m = re.match(r"arxiv_(.+)$", n)
            if m:
                ids.add(m.group(1))
    try:
        man = json.load(open(os.path.join(REPO, "corpus/manifest.json")))
        txt = json.dumps(man)
        ids |= set(re.findall(r"\b(\d{4}\.\d{4,5})\b", txt))
    except Exception:
        pass
    return ids

_tot = {}
def api(cat, ym, start, n=1):
    q = f"cat:{cat} AND submittedDate:[{ym}010000 TO {ym}312359]"
    u = ("https://export.arxiv.org/api/query?search_query=" + urllib.parse.quote(q) +
         f"&start={start}&max_results={n}&sortBy=submittedDate&sortOrder=ascending")
    return cget(u)

def total(cat, ym):
    k = (cat, ym)
    if k not in _tot:
        root = ET.fromstring(api(cat, ym, 0, 1))
        _tot[k] = int(root.find("o:totalResults", NS).text)
    return _tot[k]

def bbl_count(eprint_bytes):
    """Return (n_entries, kind) from e-print, or (None, why)."""
    try:
        raw = gzip.decompress(eprint_bytes)
    except Exception:
        raw = eprint_bytes
    files = {}
    try:
        with tarfile.open(fileobj=io.BytesIO(raw)) as t:
            for m in t.getmembers():
                if m.isfile() and m.name.lower().endswith((".bbl", ".tex")) and m.size < 5_000_000:
                    files[m.name] = t.extractfile(m).read().decode("utf-8", "replace")
    except tarfile.TarError:
        files["main.tex"] = raw.decode("utf-8", "replace")
    best = (0, None, None)
    for name, txt in files.items():
        n_item = len(re.findall(r"\\bibitem\b", txt))
        n_ent = len(re.findall(r"^\s*\\entry\{", txt, re.M))
        n, kind = (n_item, "bibitem") if n_item >= n_ent else (n_ent, "biblatex-entry")
        if n > best[0]:
            best = (n, kind, name)
    if best[0] == 0:
        return None, "no-bbl-or-bibitem"
    return best[0], f"{best[1]}:{best[2]}"

def oai_license(aid):
    u = f"https://export.arxiv.org/oai2?verb=GetRecord&identifier=oai:arXiv.org:{aid}&metadataPrefix=arXiv"
    x = cget(u)
    m = re.search(r"<license>([^<]*)</license>", x)
    return m.group(1) if m else "arXiv.org perpetual non-exclusive license (no <license> element)"

def draw(stratum, n, cats, months, max_tries=80):
    r = rng(stratum)
    own = own_corpus_ids()
    acc, rej = [], []
    for k in range(max_tries):
        if len(acc) >= n:
            break
        cat = r.choice(cats); ym = r.choice(months)
        try:
            tot = total(cat, ym)
            if tot == 0:
                rej.append({"draw": k, "cat": cat, "ym": ym, "why": "empty"}); continue
            off = r.randrange(tot)
            root = ET.fromstring(api(cat, ym, off, 1))
            e = root.find("a:entry", NS)
            idurl = e.find("a:id", NS).text
            m = re.search(r"abs/(.+?)v(\d+)$", idurl)
            aid, ver = m.group(1), int(m.group(2))
            title = re.sub(r"\s+", " ", e.find("a:title", NS).text).strip()
            base = {"draw": k, "cat": cat, "ym": ym, "offset": off, "total": tot, "arxiv_id": aid, "version": ver}
            if aid in own:
                rej.append({**base, "why": "in-tool-corpus"}); continue
            if any(a["arxiv_id"] == aid for a in acc):
                rej.append({**base, "why": "duplicate"}); continue
            ep = get(f"https://arxiv.org/e-print/{aid}v{ver}", binary=True, max_bytes=25_000_000, timeout=120)[0]
            nb, kind = bbl_count(ep)
            if nb is None:
                rej.append({**base, "why": kind}); continue
            pdf = get(f"https://arxiv.org/pdf/{aid}v{ver}", binary=True, max_bytes=15_000_000, timeout=120)[0]
            if not pdf.startswith(b"%PDF"):
                rej.append({**base, "why": "not-pdf"}); continue
            acc.append({**base, "title": title, "pdf": pdf, "truth_n": nb, "truth_kind": kind,
                        "license": oai_license(aid), "primary_cat": cat})
        except Exception as ex:
            rej.append({"draw": k, "cat": cat, "ym": ym, "why": f"error:{type(ex).__name__}:{str(ex)[:80]}"})
    return acc, rej
