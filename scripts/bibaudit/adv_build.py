# /// script
# dependencies = ["pymupdf"]
# ///
"""Build adversarial PDFs from real sample papers (recipes recorded in docs/bibaudit/adv/adversarial.json). No PDF is committed.

Base A = pmc-clinical-trial-01 (JATS truth, 56 refs); Base B = arxiv-physics-01 (bbl truth, 100 refs, two-column REVTeX-like);
Supplement PDFs = real supplementary files from the PMC bucket. Every derived PDF is written to WORK/pdf/adv-*.pdf.
"""
import json, os, re, shutil, sys
sys.path.insert(0, os.path.dirname(__file__))
from common import *
import pymupdf

REPO = "/home/user/pdftextract-audit"
os.makedirs(f"{WORK}/pdf", exist_ok=True); os.makedirs(f"{WORK}/truth", exist_ok=True); os.makedirs(f"{WORK}/adv", exist_ok=True)
os.makedirs(f"{REPO}/docs/bibaudit/adv", exist_ok=True)

def fetch(url, name):
    p = f"{WORK}/adv/{name}"
    if not os.path.exists(p):
        open(p, "wb").write(get(url, binary=True, max_bytes=30_000_000, timeout=120)[0])
    return p

def find_ref_page(doc):
    """last page index whose text has a References heading (or the first page of the tail list)"""
    last = None
    for i in range(doc.page_count):
        if re.search(r"(?im)^\s*(references|bibliography|literature cited)\s*$", doc[i].get_text()):
            last = i
    return last

def text_page(doc, title, paras, numbered=False):
    pg = doc.new_page(width=595, height=842)
    y = 72
    pg.insert_text((72, y), title, fontsize=14)
    y += 30
    for k, p in enumerate(paras, 1):
        txt = (f"{k}. " if numbered else "") + p
        r = pymupdf.Rect(72, y, 523, y + 60)
        pg.insert_textbox(r, txt, fontsize=10)
        y += 62
    return pg

items = []
A = fetch("https://pmc-oa-opendata.s3.amazonaws.com/PMC13523376.1/PMC13523376.1.pdf", "baseA.pdf")
B = fetch("https://arxiv.org/pdf/2507.15209v1", "baseB.pdf")
dA, dB = pymupdf.open(A), pymupdf.open(B)
refA, refB = find_ref_page(dA), find_ref_page(dB)
print("baseA pages", dA.page_count, "ref heading page idx", refA, "| baseB pages", dB.page_count, "ref page idx", refB)

def add(pid, doc_or_path, recipe, truth_from=None, truth_n=None, expect="", kind=None):
    p = f"{WORK}/pdf/{pid}.pdf"
    if isinstance(doc_or_path, str):
        shutil.copy(doc_or_path, p)
    else:
        doc_or_path.save(p, garbage=0)
    data = open(p, "rb").read()
    if truth_from:
        shutil.copy(f"{WORK}/truth/{truth_from}.json", f"{WORK}/truth/{pid}.json")
    items.append({"id": pid, "stratum": "adversarial", "field": "adversarial", "layout_hint": recipe, "source": "derived from sample papers (recipe field)",
                  "truth_kind": kind or ("jats" if truth_from and truth_from.startswith("pmc") else "bbl" if truth_from else "manual"),
                  "truth_n": truth_n, "truth_source_id": truth_from, "expect": expect, "pdf_bytes": len(data), "pdf_sha256": sha256(data), "license": "derived from CC-licensed sources, never committed"})

# truth files for the bases (copied from earlier download; rebuild JATS truth from the frozen sample)
import sample_pmc as P
from jats import parse_refs
mA = f"{WORK}/truth/pmc-clinical-trial-01.json"
if not os.path.exists(mA):
    x = get("https://pmc-oa-opendata.s3.amazonaws.com/PMC13523376.1/PMC13523376.1.xml")[0]
    refs = parse_refs(x); json.dump({"kind": "jats", "n": len(refs), "refs": refs}, open(mA, "w"), ensure_ascii=False)
nA = json.load(open(mA))["n"]
if not os.path.exists(f"{WORK}/truth/arxiv-physics-01.json"):
    json.dump({"kind": "bbl", "n": 100}, open(f"{WORK}/truth/arxiv-physics-01.json", "w"))

# adv-01: refs in the middle, then a 4-page appendix (body pages of base B, no reference list)
d = pymupdf.open(A); d.insert_pdf(dB, from_page=1, to_page=4)
add("adv-01-refs-then-appendix", d, "Base A (56-ref list ends the paper) + 4 body pages of base B appended as an appendix", "pmc-clinical-trial-01", nA,
    "all 56 entries, none of the appended appendix text")
# adv-02: real main paper + its real supplementary PDF appended (supplement has its own list)
SUPP = fetch("https://pmc-oa-opendata.s3.amazonaws.com/PMC13470671.1/RA-OLF-D6RA02082H-s001.pdf", "supp_chem03.pdf")
MAIN = fetch("https://pmc-oa-opendata.s3.amazonaws.com/PMC13470671.1/PMC13470671.1.pdf", "main_chem03.pdf")
d = pymupdf.open(MAIN); d.insert_pdf(pymupdf.open(SUPP))
if not os.path.exists(f"{WORK}/truth/pmc-chemistry-03.json"):
    x = get("https://pmc-oa-opendata.s3.amazonaws.com/PMC13470671.1/PMC13470671.1.xml")[0]
    refs = parse_refs(x); json.dump({"kind": "jats", "n": len(refs), "refs": refs}, open(f"{WORK}/truth/pmc-chemistry-03.json", "w"), ensure_ascii=False)
add("adv-02-main-plus-supplement", d, "pmc-chemistry-03 main paper PDF + its real supplementary PDF (own reference list) concatenated", "pmc-chemistry-03",
    json.load(open(f"{WORK}/truth/pmc-chemistry-03.json"))["n"], "owner wants BOTH lists; tool documented to return the last list only")
# real supplement alone
add("adv-03-supplement-alone", SUPP, "real supplementary PDF PMC13470671 (own reference list), scored by manual count", None, None, "list found", kind="manual")
# adv-04: no bibliography: base A without its reference pages
d = pymupdf.open(A)
if refA is not None:
    d.delete_pages(refA, d.page_count - 1)
add("adv-04-no-bibliography", d, "Base A with the reference-list pages deleted (in-text [n] citations remain)", None, 0, "not_found (and no invented entries)", kind="jats")
json.dump({"kind": "jats", "n": 0, "refs": []}, open(f"{WORK}/truth/adv-04-no-bibliography.json", "w"))
# adv-05: refs then acknowledgements/author contributions/funding page
d = pymupdf.open(A)
text_page(d, "Acknowledgements", ["We thank the participants and the clinical staff of the three centres for their time and effort.",
                                  "Author contributions: A.B. designed the study; C.D. analysed the data; E.F. wrote the manuscript.",
                                  "Funding: This work was supported by grant 12345 from a national research council.",
                                  "Competing interests: The authors declare no competing interests."])
add("adv-05-refs-before-acknowledgements", d, "Base A + final page with Acknowledgements/Author contributions/Funding/Competing interests after the list", "pmc-clinical-trial-01", nA,
    "56 entries; last entry must not swallow the acknowledgement text")
# adv-06: numbered list appendix after the references (headingless numbered run of >=3 items after the real list)
d = pymupdf.open(A)
text_page(d, "Appendix A. Inclusion criteria", ["Age between 18 and 75 years at screening.", "Ability to walk 10 metres with or without an aid.",
                                                 "Written informed consent obtained before any study procedure.", "No change in medication in the previous four weeks."], numbered=True)
add("adv-06-numbered-appendix-after-refs", d, "Base A + appendix page containing a numbered 1..4 list of study criteria", "pmc-clinical-trial-01", nA,
    "56 entries (list before the appendix); a wrong answer picks the appendix list")
# adv-07: two-column real paper (base B) unchanged with page furniture; scored on count
add("adv-07-two-column-real", B, "Base B unchanged: two-column arXiv paper with running heads / page numbers", "arxiv-physics-01", 100, "100 entries, no furniture")
# adv-08: image-only (rasterised) version of base A
d = pymupdf.open()
for i in range(dA.page_count):
    pix = dA[i].get_pixmap(dpi=90)
    pg = d.new_page(width=dA[i].rect.width, height=dA[i].rect.height)
    pg.insert_image(pg.rect, pixmap=pix)
add("adv-08-image-only", d, "Base A with every page rasterised at 90 dpi (no text layer, no OCR)", "pmc-clinical-trial-01", nA, "explicit failure or not_found; never a silent empty success")
# adv-09..: encrypted, corrupted
def save_enc(pid, user, owner, recipe, expect):
    p = f"{WORK}/pdf/{pid}.pdf"
    d = pymupdf.open(A)
    d.save(p, encryption=pymupdf.PDF_ENCRYPT_AES_256, user_pw=user, owner_pw=owner, garbage=0)
    data = open(p, "rb").read()
    items.append({"id": pid, "stratum": "adversarial", "field": "adversarial", "layout_hint": recipe, "source": "derived", "truth_kind": "jats", "truth_n": nA,
                  "truth_source_id": "pmc-clinical-trial-01", "expect": expect, "pdf_bytes": len(data), "pdf_sha256": sha256(data), "license": "derived, never committed"})
    shutil.copy(mA, f"{WORK}/truth/{pid}.json")
save_enc("adv-09-encrypted-user-pw", "s3cret", "owner", "Base A, AES-256, user password 's3cret'", "failed with an explicit password error without the password; 56 entries with --password s3cret")
save_enc("adv-10-encrypted-owner-only", "", "owner", "Base A, AES-256, empty user password + owner password (opens without a password)", "56 entries")
raw = open(A, "rb").read()
def add_bytes(pid, data, recipe, expect):
    open(f"{WORK}/pdf/{pid}.pdf", "wb").write(data)
    items.append({"id": pid, "stratum": "adversarial", "field": "adversarial", "layout_hint": recipe, "source": "derived", "truth_kind": "jats", "truth_n": nA,
                  "truth_source_id": "pmc-clinical-trial-01", "expect": expect, "pdf_bytes": len(data), "pdf_sha256": sha256(data), "license": "derived, never committed"})
    shutil.copy(mA, f"{WORK}/truth/{pid}.json")
add_bytes("adv-11-truncated-60pct", raw[: int(len(raw) * 0.6)], "Base A cut at 60% of its bytes (no xref/trailer)", "failed or partial; explicit, no crash")
add_bytes("adv-12-zero-bytes", b"", "empty file", "failed with explicit error")
add_bytes("adv-13-not-a-pdf", b"This is a plain text file with a .pdf extension.\n" * 50, "text file named .pdf", "failed with explicit error")
add_bytes("adv-14-trailer-stripped", raw[:-2048], "Base A with the last 2 KB (startxref/trailer/xref) removed", "failed, or recovered by a scan (report which)")
mid = bytearray(raw)
for k in range(20000, len(mid) - 20000, 9000):  # flip bytes inside streams
    mid[k] ^= 0xFF
add_bytes("adv-15-bitflips-in-streams", bytes(mid), "Base A with 1 byte flipped every 9 KB inside the body", "explicit failure or degraded result; no crash/hang")
add_bytes("adv-16-junk-prefix", b"\x00" * 1500 + raw, "Base A with 1,500 NUL bytes before %PDF-", "opens (spec allows 1024 bytes of junk) or explicit error")
json.dump({"generated": time.strftime("%Y-%m-%d %H:%M:%S"), "items": items}, open(f"{REPO}/docs/bibaudit/adv/adversarial.json", "w"), indent=1, ensure_ascii=False)
print(len(items), "adversarial items")
