"""Shared helpers for the bibliography adversarial audit (stdlib only)."""
import hashlib, json, os, random, re, sys, time, urllib.parse, urllib.request, urllib.error

UA = "pdftextract-bibaudit/0.1 (research audit; mailto:01.amusing_cozies@icloud.com)"
WORK = os.environ.get("BIBAUDIT_WORK", "/tmp/claude-0/-home-user-pdftextract/52e2dee1-4435-5635-aefc-9c6b94bc2e53/scratchpad/audit/work")
SEED = 20260930
_last = {}

def polite(host, gap):
    t = _last.get(host, 0)
    d = gap - (time.time() - t)
    if d > 0:
        time.sleep(d)
    _last[host] = time.time()

GAPS = {"eutils.ncbi.nlm.nih.gov": 0.4, "api.crossref.org": 0.15, "export.arxiv.org": 3.1,
        "arxiv.org": 3.1, "api.unpaywall.org": 0.12, "doaj.org": 0.5, "zenodo.org": 0.6}

def get(url, timeout=60, binary=False, retries=3, headers=None, max_bytes=60_000_000):
    host = urllib.parse.urlparse(url).netloc
    last = None
    for a in range(retries):
        polite(host, GAPS.get(host, 0.2))
        req = urllib.request.Request(url, headers={"User-Agent": UA, **(headers or {})})
        try:
            with urllib.request.urlopen(req, timeout=timeout) as r:
                data = r.read(max_bytes + 1)
                if len(data) > max_bytes:
                    raise IOError("too large")
                ct = r.headers.get("Content-Type", "")
                return (data if binary else data.decode("utf-8", "replace")), ct, r.status, r.geturl()
        except urllib.error.HTTPError as e:
            last = e
            if e.code in (429, 503):
                time.sleep(5 * (a + 1))
                continue
            raise
        except Exception as e:
            last = e
            if str(e) == "too large":
                raise
            time.sleep(2 * (a + 1))
    raise last

def getj(url, **kw):
    return json.loads(get(url, **kw)[0])

def sha256(b):
    return hashlib.sha256(b).hexdigest()

def rng(*parts):
    """Deterministic RNG per stratum: seed + stratum name."""
    return random.Random(f"{SEED}:{':'.join(map(str, parts))}")

def exists(url, timeout=20):
    host = urllib.parse.urlparse(url).netloc
    polite(host, GAPS.get(host, 0.05))
    req = urllib.request.Request(url, method="HEAD", headers={"User-Agent": UA})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status == 200
    except urllib.error.HTTPError:
        return False
    except Exception:
        return False

def cget(url, binary=False, **kw):
    """Cached GET (text or bytes). Cache lives in WORK/cache; used so a run can resume."""
    d = os.path.join(WORK, "cache")
    os.makedirs(d, exist_ok=True)
    p = os.path.join(d, hashlib.sha1(url.encode()).hexdigest() + (".bin" if binary else ".txt"))
    if os.path.exists(p):
        return open(p, "rb").read() if binary else open(p, encoding="utf-8").read()
    data = get(url, binary=binary, **kw)[0]
    if binary:
        open(p, "wb").write(data)
    else:
        open(p, "w", encoding="utf-8").write(data)
    return data

def cgetj(url, **kw):
    return json.loads(cget(url, **kw))

def log(*a):
    print(time.strftime("%H:%M:%S"), *a, flush=True)
