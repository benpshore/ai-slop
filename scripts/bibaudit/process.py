# /// script
# dependencies = ["pymupdf"]
# ///
"""Run the tool on one PDF (timing reps + memory), take an independent PyMuPDF text dump of the tail pages, then optionally delete the PDF.

Outputs (in WORK/results): <id>.json (tool outputs, timings, RSS, ledger tables) and <id>.mupdf.json.gz (independent text).
Timing: wall-clock around the subprocess (includes process start), plus the tool's own elapsed_ms. Load average is recorded because
other jobs share the host.
"""
import gzip, json, os, re, shutil, sqlite3, subprocess, sys, tempfile, time
sys.path.insert(0, os.path.dirname(__file__))
from common import WORK, sha256
import pymupdf

TPE = os.environ.get("TPE", "/home/user/pdftextract/target/release/tpe")
os.makedirs(f"{WORK}/results", exist_ok=True)

def run(cmd, timeout=600, env=None):
    t0 = time.perf_counter()
    p = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env)
    try:
        out, err = p.communicate(timeout=timeout)
        timed_out = False
    except subprocess.TimeoutExpired:
        p.kill(); out, err = p.communicate(); timed_out = True
    dt = time.perf_counter() - t0
    # wait4 is consumed by communicate(); use resource for children high-water mark delta is unreliable, so
    # re-run through /usr/bin/time when RSS is wanted (see rss()).
    return {"rc": p.returncode, "wall_s": dt, "out": out, "err": err, "timed_out": timed_out, "load1": os.getloadavg()[0]}

def rss_kb(cmd, timeout=600):
    """Peak RSS of one run via os.wait4 rusage."""
    p = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    t0 = time.perf_counter()
    try:
        _, status, ru = os.wait4(p.pid, 0) if timeout is None else _wait(p, timeout)
    except Exception:
        return None
    return ru.ru_maxrss

def _wait(p, timeout):
    t0 = time.time()
    while True:
        r = os.wait4(p.pid, os.WNOHANG)
        if r[0] != 0:
            return r
        if time.time() - t0 > timeout:
            p.kill(); r = os.wait4(p.pid, 0); return r
        time.sleep(0.01)

def bib(pdf, reps=5):
    runs = []
    first = None
    for i in range(reps):
        r = run([TPE, "bibliography", pdf])
        line = r["out"].decode("utf-8", "replace").strip().splitlines()
        rec = None
        try:
            rec = json.loads(line[0]) if line else None
        except Exception:
            rec = None
        if i == 0:
            first = {"record": rec, "rc": r["rc"], "stderr": r["err"].decode("utf-8", "replace")[-2000:], "timed_out": r["timed_out"]}
        runs.append({"wall_s": r["wall_s"], "elapsed_ms": (rec or {}).get("elapsed_ms"), "load1": r["load1"], "rc": r["rc"]})
    first["runs"] = runs
    first["peak_rss_kb"] = rss_kb([TPE, "bibliography", pdf])
    return first

def extract(pdf, reps=3, keep_pages_tail=25):
    runs = []
    first = None
    for i in range(reps):
        d = tempfile.mkdtemp(prefix="bibaudit-")
        db = os.path.join(d, "l.db")
        r = run([TPE, "extract", "--db", db, "--json", pdf])
        rec = None
        try:
            rec = json.loads(r["out"].decode("utf-8", "replace").strip().splitlines()[0])
        except Exception:
            rec = None
        runs.append({"wall_s": r["wall_s"], "load1": r["load1"], "rc": r["rc"], "timings": (rec or {}).get("timings")})
        if i == 0:
            tabs = {}
            try:
                con = sqlite3.connect(db)
                for (t,) in con.execute("select name from sqlite_master where type='table'"):
                    tabs[t] = con.execute(f'select count(*) from "{t}"').fetchone()[0]
                cols = {t: [c[1] for c in con.execute(f'pragma table_info("{t}")')] for t in tabs}
                con.close()
            except Exception as e:
                tabs, cols = {"error": str(e)}, {}
            slim = None
            if rec:
                pages = rec.get("pages") or []
                tail = pages[-keep_pages_tail:]
                slim = {"status": rec.get("status"), "references": rec.get("references"), "n_citations": len(rec.get("citations") or []),
                        "citations_sample": (rec.get("citations") or [])[:3], "metadata": rec.get("metadata"), "warnings": rec.get("warnings"),
                        "timings": rec.get("timings"), "n_pages": len(pages),
                        "top_level_keys": list(rec.keys()),
                        "tail_span_text": {str(p["page"]): " ".join(s["text"] for s in p.get("spans", [])) for p in tail}}
            first = {"rc": r["rc"], "stderr": r["err"].decode("utf-8", "replace")[-1500:], "rec": slim, "tables": tabs, "columns": cols}
        shutil.rmtree(d, ignore_errors=True)
    first["runs"] = runs
    return first

def mupdf(pdf, tail=30):
    out = {"ok": False}
    try:
        d = pymupdf.open(pdf)
    except Exception as e:
        out["error"] = f"{type(e).__name__}: {e}"; return out
    out["ok"] = True
    out["encrypted"] = d.is_encrypted
    if d.is_encrypted:
        return out
    n = d.page_count
    out["pages"] = n
    out["metadata"] = d.metadata
    first = d[0].get_text("text", sort=True) if n else ""
    out["first_page_text"] = first[:3000]
    chars = []
    cols = []
    txt = {}
    for i in range(max(0, n - tail), n):
        pg = d[i]
        t = pg.get_text("text", sort=True)
        txt[str(i + 1)] = t
        # column layout: cluster x-centres of text blocks
        blocks = [b for b in pg.get_text("blocks") if b[6] == 0 and len(b[4].strip()) > 40]
        w = pg.rect.width
        left = sum(1 for b in blocks if (b[0] + b[2]) / 2 < w * 0.45 and b[2] < w * 0.55)
        right = sum(1 for b in blocks if (b[0] + b[2]) / 2 > w * 0.55 and b[0] > w * 0.45)
        cols.append(2 if (left >= 2 and right >= 2) else 1)
    out["tail_pages_text"] = txt
    out["tail_cols_by_page"] = cols
    # text-layer presence over up to 12 sampled pages -> scanned detection
    sample = list(range(0, n, max(1, n // 12)))[:12]
    tl = []
    for i in sample:
        pg = d[i]
        tl.append({"chars": len(pg.get_text("text").strip()), "images": len(pg.get_images())})
    out["text_layer_sample"] = tl
    out["fonts_tail"] = sorted({f[3] for i in range(max(0, n - 3), n) for f in d[i].get_fonts()})[:12]
    return out

def process(item, reps_bib=5, reps_ext=3, keep=False):
    pid = item["id"]
    pdf = f"{WORK}/pdf/{pid}.pdf"
    if not os.path.exists(pdf):
        return None
    got = sha256(open(pdf, "rb").read())
    res = {"id": pid, "pdf_sha256_local": got, "sha_matches_manifest": got == item.get("pdf_sha256"), "pdf_bytes": os.path.getsize(pdf),
           "date": time.strftime("%Y-%m-%d %H:%M:%S")}
    res["bibliography"] = bib(pdf, reps_bib)
    res["extract"] = extract(pdf, reps_ext)
    m = mupdf(pdf)
    with gzip.open(f"{WORK}/results/{pid}.mupdf.json.gz", "wt") as f:
        json.dump(m, f, ensure_ascii=False)
    res["mupdf_pages"] = m.get("pages"); res["mupdf_ok"] = m.get("ok")
    json.dump(res, open(f"{WORK}/results/{pid}.json", "w"), ensure_ascii=False)
    if not keep:
        os.remove(pdf)
    return res

if __name__ == "__main__":
    import glob
    keep = "--keep" in sys.argv
    parts = [a for a in sys.argv[1:] if not a.startswith("--")]
    for p in parts:
        j = json.load(open(p))
        for it in j["items"]:
            if os.path.exists(f"{WORK}/results/{it['id']}.json"):
                continue
            t0 = time.time()
            r = process(it, keep=keep)
            print(it["id"], "done" if r else "MISSING", f"{time.time()-t0:.1f}s", flush=True)
