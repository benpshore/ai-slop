"""Parse JATS <ref-list> into structured ground truth."""
import re, xml.etree.ElementTree as ET

def _txt(e):
    return re.sub(r"\s+", " ", "".join(e.itertext())).strip() if e is not None else ""

def parse_refs(xml_text):
    xml_text = re.sub(r"<!DOCTYPE[^>]*>", "", xml_text, count=1)
    root = ET.fromstring(xml_text.encode("utf-8"))
    back = root.find(".//back")
    out = []
    if back is None:
        return out
    for rl in back.iter("ref-list"):
        # skip nested ref-lists already covered by iter (flat)
        for ref in rl.findall("ref"):
            cit = None
            for tag in ("element-citation", "mixed-citation", "citation", "nlm-citation"):
                cit = ref.find(tag)
                if cit is not None:
                    break
            authors = []
            title = ""
            year = None
            doi = None
            source = ""
            if cit is not None:
                for pg in cit.findall("person-group"):
                    t = pg.get("person-group-type", "author")
                    if t not in ("author", ""):
                        continue
                    for n in pg:
                        if n.tag == "name":
                            authors.append({"surname": _txt(n.find("surname")), "given": _txt(n.find("given-names"))})
                        elif n.tag == "string-name":
                            authors.append({"surname": _txt(n.find("surname")), "given": _txt(n.find("given-names")) or _txt(n)})
                        elif n.tag == "collab":
                            authors.append({"surname": _txt(n), "given": "", "collab": True})
                if not authors:
                    for n in cit.findall("string-name"):
                        authors.append({"surname": _txt(n.find("surname")), "given": _txt(n.find("given-names"))})
                    for n in cit.findall("name"):
                        authors.append({"surname": _txt(n.find("surname")), "given": _txt(n.find("given-names"))})
                for tag in ("article-title", "chapter-title", "source", "trans-title"):
                    e = cit.find(tag)
                    if e is not None and tag != "source":
                        title = _txt(e)
                        break
                s = cit.find("source")
                source = _txt(s)
                if not title:
                    title = ""  # book / report: only source exists
                y = cit.find("year")
                if y is not None:
                    m = re.search(r"\d{4}", _txt(y))
                    year = int(m.group()) if m else None
                for pid in cit.findall("pub-id"):
                    if pid.get("pub-id-type") == "doi":
                        doi = _txt(pid).lower()
            out.append({"label": _txt(ref.find("label")), "authors": authors, "title": title, "source": source,
                        "year": year, "doi": doi, "text": _txt(cit if cit is not None else ref)})
    return out
