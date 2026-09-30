#!/usr/bin/env python3
"""Score `tpe bibliography` output against independent ground truth (stdlib only).

Inputs (WORK = $BIBAUDIT_WORK):  parts/*.json manifests, truth/<id>.json, results/<id>.json, results/<id>.mupdf.json.gz,
manual_counts.json (optional, id -> {"n": int|null, "method": str}).
Output: WORK/scores.json (per paper, per author) and a printed summary.

Layers (every failure gets exactly one):
  extraction  the characters in the tool's raw entry text differ from what an independent extractor (PyMuPDF) reads at the same place
  recognition the raw entry text is right but segmentation / field parsing is wrong (merged or split entries, wrong author/title/year field)
  decision    the tool accepted the wrong text or rejected the right text (list missed, stopped early, spurious entry, wrong status)
  source      the ground truth is not what the PDF prints (JATS/Crossref differ from the PDF); never counted against the tool
"""
import difflib, glob, gzip, json, math, os, re, statistics, sys, unicodedata
from collections import Counter, defaultdict

WORK = os.environ.get("BIBAUDIT_WORK", "/tmp/claude-0/-home-user-pdftextract/52e2dee1-4435-5635-aefc-9c6b94bc2e53/scratchpad/audit/work")
REPO = os.environ.get("BIBAUDIT_REPO", "/home/user/pdftextract-audit")

# ---------------------------------------------------------------- text helpers
def fold(s):
    """lowercase, strip accents and ligatures, non-alphanumerics -> single space."""
    s = unicodedata.normalize("NFKD", s or "")
    s = "".join(c for c in s if not unicodedata.combining(c))
    s = s.replace("ø", "o").replace("Ø", "O").replace("ł", "l").replace("Ł", "L").replace("đ", "d").replace("Đ", "D").replace("ß", "ss").replace("æ", "ae").replace("œ", "oe").replace("ı", "i")
    s = re.sub(r"[^0-9a-zA-Z]+", " ", s.lower())
    return re.sub(r"\s+", " ", s).strip()

def tokens(s, minlen=3):
    return [t for t in fold(s).split() if len(t) >= minlen]

def bigrams(toks):
    return set(zip(toks, toks[1:]))

def dehyph(s):
    return re.sub(r"(\w)-\s*\n\s*(\w)", r"\1\2", s)

def folded_with_map(s):
    """Folded string and, for each folded char, the index in the original string."""
    out, idx = [], []
    prev_space = True
    for i, ch in enumerate(s):
        d = unicodedata.normalize("NFKD", ch)
        d = "".join(c for c in d if not unicodedata.combining(c))
        d = d.replace("ø", "o").replace("ł", "l").replace("đ", "d").replace("ß", "ss").replace("æ", "ae").replace("œ", "oe").replace("ı", "i")
        for c in d.lower():
            if c.isalnum() and c.isascii():
                out.append(c); idx.append(i); prev_space = False
            elif not prev_space:
                out.append(" "); idx.append(i); prev_space = True
    return "".join(out), idx

def percentile(xs, p):
    xs = sorted(xs)
    if not xs:
        return None
    k = (len(xs) - 1) * p / 100
    lo, hi = math.floor(k), math.ceil(k)
    return xs[lo] + (xs[hi] - xs[lo]) * (k - lo)

def dist(xs):
    xs = [x for x in xs if x is not None]
    if not xs:
        return {"n": 0}
    return {"n": len(xs), "min": min(xs), "median": statistics.median(xs), "p95": percentile(xs, 95), "max": max(xs), "mean": sum(xs) / len(xs)}

# ---------------------------------------------------------------- loading
def load_manifest():
    items = {}
    for p in sorted(glob.glob(f"{REPO}/docs/bibaudit/parts/*.json")) + sorted(glob.glob(f"{REPO}/docs/bibaudit/adv/*.json")):
        j = json.load(open(p))
        for it in j["items"]:
            it["_part"] = os.path.basename(p)
            items[it["id"]] = it
    return items

def load_truth(pid):
    p = f"{WORK}/truth/{pid}.json"
    return json.load(open(p)) if os.path.exists(p) else None

def load_mupdf(pid):
    p = f"{WORK}/results/{pid}.mupdf.json.gz"
    return json.load(gzip.open(p, "rt")) if os.path.exists(p) else None

# ---------------------------------------------------------------- truth keys
def truth_key_tokens(t, kind):
    if kind == "jats":
        toks = tokens(t.get("title", ""))
        if len(toks) < 3:
            toks = tokens((t.get("source") or "") + " " + " ".join(a["surname"] for a in t["authors"][:2]) + " " + str(t.get("year") or ""))
        return toks
    if kind == "crossref":
        toks = tokens(t.get("title", ""))
        if len(toks) < 3:
            toks = tokens(t.get("text", ""))
        return toks
    return []

def containment(key, raw_tokens_set):
    if not key:
        return 0.0
    return sum(1 for k in key if k in raw_tokens_set) / len(key)

def align(truth_refs, kind, entries):
    """Monotone alignment maximizing summed containment (threshold 0.6). Returns list of (i,j,sim)."""
    n, m = len(truth_refs), len(entries)
    keys = [truth_key_tokens(t, kind) for t in truth_refs]
    rawsets = [set(tokens(e["raw"])) for e in entries]
    sim = [[0.0] * m for _ in range(n)]
    for i in range(n):
        for j in range(m):
            s = containment(keys[i], rawsets[j])
            d1 = (truth_refs[i].get("doi") or "").lower()
            if d1 and (entries[j].get("doi") or "").lower() == d1:
                s = max(s, 1.0)
            sim[i][j] = s if s >= 0.6 else 0.0
    dp = [[0.0] * (m + 1) for _ in range(n + 1)]
    for i in range(1, n + 1):
        for j in range(1, m + 1):
            dp[i][j] = max(dp[i - 1][j], dp[i][j - 1], dp[i - 1][j - 1] + (sim[i - 1][j - 1] if sim[i - 1][j - 1] > 0 else -1))
    i, j, pairs = n, m, []
    while i > 0 and j > 0:
        if sim[i - 1][j - 1] > 0 and abs(dp[i][j] - (dp[i - 1][j - 1] + sim[i - 1][j - 1])) < 1e-9:
            pairs.append((i - 1, j - 1, sim[i - 1][j - 1])); i -= 1; j -= 1
        elif dp[i - 1][j] >= dp[i][j - 1]:
            i -= 1
        else:
            j -= 1
    pairs.reverse()
    return pairs, sim, keys

# ---------------------------------------------------------------- spurious classification
FURN = re.compile(r"(?i)(^\s*\d{1,4}\s*$|page\s+\d+\s+of\s+\d+|downloaded from|https?://\S*\s*$|©|all rights reserved|creative commons|this article is|received:|accepted:|published:|www\.)")
CAPT = re.compile(r"(?i)^\s*(fig(ure)?\.?|table|supplementary (fig|table))\s*[sS]?\d+")
ACK = re.compile(r"(?i)(acknowledg|funding|conflicts? of interest|competing interests|author contributions|data availability|ethics|declaration|disclosure|consent|supplementary|appendix)")

def classify_spurious(raw):
    if CAPT.search(raw):
        return "figure/table caption"
    if ACK.search(raw[:200]):
        return "back-matter section text (ack/funding/appendix/supplement)"
    if FURN.search(raw[:200]) and len(raw) < 200:
        return "page furniture"
    return "other (not matched to any truth reference)"

# ---------------------------------------------------------------- author scoring
def initials_of(given):
    given = given or ""
    letters = []
    for tok in re.split(r"[\s\-.]+", given):
        if not tok:
            continue
        if tok.isupper() and len(tok) <= 4 and tok.isalpha():
            letters += list(tok)
        else:
            letters.append(tok[0].upper())
    return letters

def upper_letters(s):
    return [c for c in unicodedata.normalize("NFC", s) if c.isalpha() and c.isupper()]

def nfc_ci_contains(hay, needle):
    return unicodedata.normalize("NFC", needle).lower() in unicodedata.normalize("NFC", hay).lower()

DIAC_CLASSES = [
    ("stroke/special letters (ø ł đ ð þ)", re.compile(r"[øØłŁđĐðÐþÞ]")),
    ("ligature letters (ß æ œ)", re.compile(r"[ßæÆœŒ]")),
    ("dotless/dotted i (ı İ)", re.compile(r"[ıİ]")),
]
COMB = {"́": "acute", "̀": "grave", "̂": "circumflex", "̈": "umlaut/diaeresis", "̧": "cedilla", "̃": "tilde",
        "̌": "caron", "̊": "ring", "̋": "double acute (ő ű)", "̆": "breve", "̨": "ogonek", "̇": "dot above", "̄": "macron"}

def diacritic_classes(s):
    out = set()
    for name, rx in DIAC_CLASSES:
        if rx.search(s):
            out.add(name)
    nfd = unicodedata.normalize("NFD", s)
    marks = [c for c in nfd if unicodedata.combining(c)]
    for c in marks:
        out.add(COMB.get(c, "other mark U+%04X" % ord(c)))
    if len(marks) and any(ord(c) > 0x1E00 for c in s):
        out.add("Vietnamese/extended Latin stacked")
    if re.search(r"[Ѐ-ӿ]", s):
        out.add("Cyrillic script")
    if re.search(r"[぀-ヿ一-鿿가-힯]", s):
        out.add("CJK script")
    return out

def name_flags(surname, given):
    f = []
    s = surname or ""
    if re.match(r"(?i)^(van|von|de|der|den|di|da|dal|del|della|dos|das|du|le|la|ter|ten|bin|ibn|abu|al|el|ben|af|av)\b", s) or re.search(r"(?i)^(al|el)-", s):
        f.append("particle/prefix")
    if " " in s.strip() or re.search(r"\w-\w", s):
        f.append("multi-word or hyphenated surname")
    if re.search(r"[A-Z][a-z]?\.?-[A-Z]", given or "") or re.search(r"\b[A-Z]\w+-[A-Z]\w+", given or ""):
        f.append("hyphenated given name (J.-P. type)")
    if re.match(r"^(Zh|Yu|Th|Ch|Sh|Ya|Yi|Xi|Ph|Kh|Ts|Ju|Yo|Ye)", (given or "").strip()):
        f.append("digraph initial candidate (Zh. Yu. Th. Ch.)")
    if diacritic_classes((surname or "") + " " + (given or "")):
        f.append("non-ASCII/diacritic")
    return f

def find_tool_author(t_sur, tool_authors, used):
    """Index of the unused tool author string containing the (folded) surname as whole-token sequence, else None."""
    ts = fold(t_sur).split()
    if not ts:
        return None
    for k, a in enumerate(tool_authors):
        if k in used:
            continue
        fa = fold(a).split()
        for s in range(len(fa) - len(ts) + 1):
            if fa[s:s + len(ts)] == ts:
                return k
    return None

def score_authors(truth_authors, tool_authors, raw, mupdf_window):
    """One record per printed truth author."""
    recs, used = [], set()
    printed = []
    for ta in truth_authors:
        if ta.get("collab"):
            continue
        sur = ta["surname"]
        printed_in_pdf = (fold(sur) in fold(mupdf_window)) if mupdf_window is not None else True
        printed.append((ta, printed_in_pdf))
    for ta, printed_pdf in printed:
        sur, giv = ta["surname"], ta["given"]
        rec = {"surname": sur, "given": giv, "flags": name_flags(sur, giv), "diac": sorted(diacritic_classes(sur + " " + giv)), "printed_in_pdf": printed_pdf}
        if not printed_pdf:
            rec["layer"] = "source"; rec["outcome"] = "surname not printed in PDF window (et al. / source mismatch)"
            recs.append(rec); continue
        k = find_tool_author(sur, tool_authors, used)
        raw_has_exact = nfc_ci_contains(raw, sur)
        win_has_exact = (nfc_ci_contains(mupdf_window, sur) if mupdf_window is not None else None)
        rec.update(raw_has_exact=raw_has_exact, window_has_exact=win_has_exact)
        if k is None:
            # surname (folded) not among tool authors
            if raw_has_exact or fold(sur) in fold(raw):
                rec.update(layer="recognition", outcome="surname in raw entry but not in parsed authors (dropped/merged/mis-split)")
            elif win_has_exact:
                rec.update(layer="extraction", outcome="surname in independent text but not in tool raw entry")
            else:
                rec.update(layer="decision", outcome="surname absent from raw entry (entry missing or wrong entry)")
            recs.append(rec); continue
        used.add(k)
        a = tool_authors[k]
        rec["tool_string"] = a
        sur_exact = nfc_ci_contains(a, sur)
        # residual = tool string minus surname tokens
        resid = re.sub(re.escape(unicodedata.normalize("NFC", sur)), " ", unicodedata.normalize("NFC", a), flags=re.I)
        if resid == unicodedata.normalize("NFC", a):  # accent-folded match only: strip by folded token
            ft = fold(sur).split()
            toks = a.split()
            resid = " ".join(t for t in toks if fold(t) not in ft)
        init_tool = upper_letters(resid)
        init_true = [c.upper() for c in initials_of(giv)]
        rec["surname_exact"] = sur_exact
        rec["initials_ok"] = (init_tool == init_true) if init_true else None
        extra_words = [w for w in re.findall(r"[^\W\d_]{3,}", resid) if not w.isupper()]
        # words of >=3 letters left over that are not part of the given name -> another person merged in / wrong slot
        given_words = set(fold(giv).split())
        rec["stray_words"] = [w for w in extra_words if fold(w) not in given_words]
        rec["merged_or_stray"] = bool(rec["stray_words"])
        rec["u_fffd"] = "�" in a
        rec["ok"] = bool(sur_exact and (rec["initials_ok"] in (True, None)) and not rec["merged_or_stray"] and not rec["u_fffd"])
        if rec["ok"]:
            rec["layer"] = None; rec["outcome"] = "ok"
        else:
            # attribute the layer
            if not sur_exact:
                if win_has_exact:
                    rec.update(layer="extraction", outcome="surname accent/char differs from independent text (tool raw wrong)")
                else:
                    rec.update(layer="source", outcome="PDF itself prints the surname differently from JATS")
            elif rec["u_fffd"]:
                rec.update(layer="extraction", outcome="U+FFFD in author string")
            elif rec["merged_or_stray"]:
                rec.update(layer="recognition", outcome="author string contains extra words (merged/mis-split names): " + " ".join(rec["stray_words"]))
            else:
                # initials wrong: is the truth initials sequence visible in raw?
                gi = "".join(init_true)
                if gi and not raw_initials_visible(raw, sur, init_true):
                    if window_initials_visible(mupdf_window, sur, init_true):
                        rec.update(layer="extraction", outcome="initials differ from independent text (raw wrong)")
                    else:
                        rec.update(layer="source", outcome="printed initials differ from JATS")
                else:
                    rec.update(layer="recognition", outcome="raw shows the right initials but parsed author string has them wrong/missing")
        recs.append(rec)
    return recs

def raw_initials_visible(raw, sur, init_true):
    return window_initials_visible(raw, sur, init_true)

def window_initials_visible(text, sur, init_true):
    if text is None:
        return True
    t = unicodedata.normalize("NFC", text)
    for m in re.finditer(re.escape(unicodedata.normalize("NFC", sur)), t, flags=re.I):
        seg = t[max(0, m.start() - 22): m.end() + 22]
        if upper_letters(seg.replace(sur, " ")).count(init_true[0]) and all(c in upper_letters(seg) for c in init_true):
            return True
    return False

# ---------------------------------------------------------------- per-paper scoring
def mupdf_window(mu, title_toks, tool_pages=None):
    """Original-text window around the truth title in the PyMuPDF tail text (or None)."""
    if not mu or not mu.get("tail_pages_text") or len(title_toks) < 3:
        return None
    txt = dehyph("\n".join(mu["tail_pages_text"][k] for k in sorted(mu["tail_pages_text"], key=int)))
    fs, idx = folded_with_map(txt)
    probe = " ".join(title_toks[:5])
    p = fs.find(probe)
    if p < 0:
        probe = " ".join(title_toks[:3])
        p = fs.find(probe)
    if p < 0:
        return ""
    o = idx[p]
    return txt[max(0, o - 400): o + 500]

def title_present(mu, key):
    """True/False/None: is the truth key visible (bigram coverage) in the independent tail text?"""
    if not mu or not mu.get("tail_pages_text"):
        return None
    txt = " ".join(mu["tail_pages_text"][k] for k in sorted(mu["tail_pages_text"], key=int))
    ft = fold(dehyph(txt))
    tb = bigrams(key)
    if not tb:
        return None
    hits = sum(1 for a, b in tb if f"{a} {b}" in ft)
    return hits / len(tb) >= 0.7

def score_paper(item, res, truth, mu, manual):
    pid = item["id"]
    out = {"id": pid, "stratum": item["stratum"], "field": item.get("field"), "source": item.get("source"), "layout_hint": item.get("layout_hint"),
           "truth_kind": item.get("truth_kind"), "language": item.get("language")}
    b = (res["bibliography"] or {}).get("record")
    out["mupdf_pages"] = res.get("mupdf_pages")
    if b is None:
        out.update(status="no-record", n_found=0, error=(res["bibliography"] or {}).get("stderr", "")[:300]); out["failure"] = "no JSON record from tool"
        return out
    entries = b.get("references") or []
    out.update(status=b["status"], n_found=len(entries), total_pages=b.get("total_pages"), pages_scanned=b.get("pages_scanned"), heading=b.get("heading"),
               section_page=b.get("section_page"), error=b.get("error"), warnings=b.get("warnings"))
    # truth count
    kind = item.get("truth_kind")
    tn = item.get("truth_n")
    truth_refs = (truth or {}).get("refs") or []
    if kind == "manual":
        m = manual.get(pid)
        if m and m.get("n") is not None:
            tn = m["n"]
        elif truth_refs:
            tn = len(truth_refs); kind = "crossref"; out["truth_kind"] = "crossref (found for a manual-stratum paper)"
        out["manual_method"] = (m or {}).get("method")
    out["truth_n"] = tn
    if tn is None:
        out["unscored_reason"] = "no ground-truth count yet"
        return out
    structured = kind in ("jats", "crossref") and len(truth_refs) > 0
    out["truth_structured"] = structured
    n = len(entries)
    if tn == 0:
        out.update(recall=None, precision=(0.0 if n else None), false_positive_entries=n)
        out["failure"] = ("false positive list (%d entries) on a paper with no reference list" % n) if n else None
        out["events"] = [{"layer": "decision", "kind": "spurious-list", "n": n}] if n else []
        return out
    events = []
    if not structured:
        m_ = min(n, tn)
        out.update(matched=None, count_recall_ub=m_ / tn, count_precision_ub=(m_ / n if n else 0.0), count_ratio=n / tn, count_exact=(n == tn))
        if b["status"] != "found":
            events.append({"layer": "decision", "kind": f"status={b['status']}", "n": tn})
        elif n != tn:
            events.append({"layer": "decision", "kind": "count differs (structured truth unavailable: layer not resolved)", "n": abs(n - tn)})
        out["events"] = events
        return out
    pairs, sim, keys = align(truth_refs, kind, entries)
    matched_i = {i for i, _, _ in pairs}
    matched_j = {j for _, j, _ in pairs}
    M = len(pairs)
    out.update(matched=M, recall=M / tn, precision=(M / n if n else 0.0), count_ratio=n / tn, count_exact=(n == tn))
    # merges: tool entry containing >=2 truth keys
    rawsets = [set(tokens(e["raw"])) for e in entries]
    merged, merged_i = [], set()
    for j, e in enumerate(entries):
        hit = [i for i in range(len(truth_refs)) if keys[i] and len(keys[i]) >= 3 and containment(keys[i], rawsets[j]) >= 0.8]
        if len(hit) >= 2:
            merged.append({"entry": j, "truth": hit}); merged_i.update(hit)
    # splits: unmatched truth whose key is covered by adjacent pair concatenation but by neither alone
    split_i = set()
    for i in range(len(truth_refs)):
        if i in matched_i or i in merged_i or not keys[i]:
            continue
        for j in range(len(entries) - 1):
            both = rawsets[j] | rawsets[j + 1]
            if containment(keys[i], both) >= 0.8 and containment(keys[i], rawsets[j]) < 0.6 and containment(keys[i], rawsets[j + 1]) < 0.6:
                split_i.add(i); break
    out["merged_entries"] = len(merged); out["split_entries"] = len(split_i)
    for mrg in merged:
        events.append({"layer": "recognition", "kind": "merged entries", "n": len(mrg["truth"]) - 1})
    for _ in split_i:
        events.append({"layer": "recognition", "kind": "split entry", "n": 1})
    # missing truth entries
    missing = [i for i in range(len(truth_refs)) if i not in matched_i and i not in merged_i and i not in split_i]
    first_m = min(matched_i) if matched_i else None
    last_m = max(matched_i) if matched_i else None
    miss_kinds = Counter()
    for i in missing:
        pres = title_present(mu, keys[i])
        if pres is False:
            events.append({"layer": "source", "kind": "truth entry not printed in PDF tail", "n": 1}); miss_kinds["not-in-pdf"] += 1
            continue
        if b["status"] != "found":
            events.append({"layer": "decision", "kind": f"list rejected (status={b['status']})", "n": 1}); miss_kinds["list-rejected"] += 1
        elif first_m is None:
            events.append({"layer": "decision", "kind": "wrong list/section accepted (no truth entry matched)", "n": 1}); miss_kinds["wrong-list"] += 1
        elif i < first_m:
            events.append({"layer": "decision", "kind": "list starts too late (backward scan stopped early / heading missed)", "n": 1}); miss_kinds["head-truncated"] += 1
        elif i > last_m:
            events.append({"layer": "decision", "kind": "list ends too early (tail cut)", "n": 1}); miss_kinds["tail-truncated"] += 1
        else:
            # inside the matched range: text either dropped by cleanup/region tagging (decision) or garbled (extraction)
            best = max(range(len(entries)), key=lambda j: containment(keys[i], rawsets[j])) if entries else None
            if best is not None and containment(keys[i], rawsets[best]) >= 0.3:
                events.append({"layer": "extraction", "kind": "entry text garbled/incomplete vs independent extraction", "n": 1}); miss_kinds["garbled"] += 1
            else:
                events.append({"layer": "decision", "kind": "mid-list entry dropped (cleanup/region tagging/segmentation)", "n": 1}); miss_kinds["mid-drop"] += 1
    out["missing"] = dict(miss_kinds)
    # spurious tool entries
    spur = Counter()
    for j, e in enumerate(entries):
        if j in matched_j or any(m_["entry"] == j for m_ in merged):
            continue
        # a fragment of a split entry?
        if any(containment(keys[i], rawsets[j]) >= 0.25 for i in split_i):
            continue
        c = classify_spurious(e["raw"])
        spur[c] += 1
        events.append({"layer": "decision", "kind": "spurious entry: " + c, "n": 1})
    out["spurious"] = dict(spur); out["spurious_total"] = sum(spur.values())
    if b["status"] == "found" and M < 0.5 * tn:
        out["failure"] = "silent wrong accept: status=found with %d/%d truth entries" % (M, tn)
    # ---- field-level + author-level on matched pairs
    fld = Counter(); auth_recs = []
    for i, j, s in pairs:
        t, e = truth_refs[i], entries[j]
        win = mupdf_window(mu, keys[i])
        # title
        tt = fold(t.get("title", ""))
        if tt and len(tt.split()) >= 3:
            fld["title_n"] += 1
            et = fold(e.get("title") or "")
            ok = bool(et) and difflib.SequenceMatcher(None, tt, et).ratio() >= 0.9
            if ok:
                fld["title_ok"] += 1
            else:
                fld["title_bad"] += 1
                rawok = tt in fold(e["raw"])
                if rawok:
                    events.append({"layer": "recognition", "kind": "title field wrong/missing though raw entry contains it", "n": 1}); fld["title_bad_recognition"] += 1
                elif win and tt in fold(win):
                    events.append({"layer": "extraction", "kind": "title text in raw differs from independent extraction", "n": 1}); fld["title_bad_extraction"] += 1
                else:
                    fld["title_bad_source"] += 1
        if t.get("year"):
            fld["year_n"] += 1
            if e.get("year") == t["year"]:
                fld["year_ok"] += 1
            else:
                fld["year_bad"] += 1
                if str(t["year"]) in e["raw"]:
                    events.append({"layer": "recognition", "kind": "year field wrong/missing though raw contains it", "n": 1}); fld["year_bad_recognition"] += 1
                else:
                    fld["year_bad_other"] += 1
        if t.get("doi"):
            fld["doi_n"] += 1
            if (e.get("doi") or "").lower().rstrip(".") == t["doi"].lower().rstrip("."):
                fld["doi_ok"] += 1
            else:
                fld["doi_bad"] += 1
                if t["doi"].lower() in re.sub(r"\s+", "", e["raw"].lower()):
                    events.append({"layer": "recognition", "kind": "DOI in raw but doi field wrong/missing", "n": 1}); fld["doi_bad_recognition"] += 1
                elif win and t["doi"].lower() in re.sub(r"\s+", "", win.lower()):
                    events.append({"layer": "extraction", "kind": "DOI in independent text but not in tool raw", "n": 1}); fld["doi_bad_extraction"] += 1
                else:
                    fld["doi_not_printed"] += 1
        if "�" in e["raw"]:
            fld["ufffd_entries"] += 1
            events.append({"layer": "extraction", "kind": "U+FFFD in raw entry", "n": 1})
        if kind == "jats" and t.get("authors"):
            recs = score_authors(t["authors"], e.get("authors") or [], e["raw"], win)
            for r in recs:
                r["paper"] = pid; r["ref"] = i
            auth_recs += recs
            # author list level
            fld["auth_lists"] += 1
            printed_n = sum(1 for r in recs if r["printed_in_pdf"])
            if all(r.get("ok") or r["layer"] == "source" for r in recs) and len(e.get("authors") or []) <= printed_n + 1:
                fld["auth_list_ok"] += 1
    out["fields"] = dict(fld)
    out["authors"] = auth_recs
    for r in auth_recs:
        if r["layer"] and r["layer"] != "source":
            events.append({"layer": r["layer"], "kind": "author: " + r["outcome"][:60], "n": 1})
    out["events"] = events
    lc = Counter()
    for ev in events:
        lc[ev["layer"]] += ev["n"]
    out["layer_counts"] = dict(lc)
    return out

def main():
    manifest = load_manifest()
    manual = json.load(open(f"{WORK}/manual_counts.json")) if os.path.exists(f"{WORK}/manual_counts.json") else {}
    scores = []
    for pid, item in manifest.items():
        rp = f"{WORK}/results/{pid}.json"
        if not os.path.exists(rp):
            continue
        res = json.load(open(rp))
        truth = load_truth(pid)
        mu = load_mupdf(pid)
        scores.append(score_paper(item, res, truth, mu, manual))
    json.dump(scores, open(f"{WORK}/scores.json", "w"), ensure_ascii=False, indent=1)
    print("scored", len(scores))
    return scores

if __name__ == "__main__":
    main()
