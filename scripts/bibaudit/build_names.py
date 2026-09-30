"""Author-name stratum: PMC OA papers by first-author-affiliation country, >= 10 references each (structured JATS names)."""
import os, sys
sys.path.insert(0, os.path.dirname(__file__))
import build_general as G

def q(country_terms):
    aff = " OR ".join(f'"{c}"[Affiliation]' for c in country_terms)
    return f'({aff}) AND "open access"[filter] AND 2018:2026[pdat]'

NAMES = [  # stratum, n, field, note, kw
    ("name-china", 5, "name stratum: Chinese", "names: Chinese (pinyin)", {"term": q(["China"])}),
    ("name-korea", 4, "name stratum: Korean", "names: Korean", {"term": q(["Republic of Korea", "South Korea"])}),
    ("name-japan", 4, "name stratum: Japanese", "names: Japanese", {"term": q(["Japan"])}),
    ("name-vietnam", 3, "name stratum: Vietnamese", "names: Vietnamese (diacritics)", {"term": q(["Vietnam", "Viet Nam"])}),
    ("name-hungary", 3, "name stratum: Hungarian", "names: Hungarian (double-acute, family-first)", {"term": q(["Hungary"])}),
    ("name-spain", 3, "name stratum: Spanish", "names: Spanish double surnames", {"term": q(["Spain"])}),
    ("name-brazil", 3, "name stratum: Portuguese", "names: Portuguese/Brazilian double surnames", {"term": q(["Brazil", "Portugal"])}),
    ("name-nl-de", 4, "name stratum: Dutch/German particles", "names: van/von/de/der/ter", {"term": q(["Netherlands", "Germany"])}),
    ("name-italy", 2, "name stratum: Italian particles", "names: di/de/da/dal", {"term": q(["Italy"])}),
    ("name-arabic", 4, "name stratum: Arabic/Persian/Turkish", "names: al-/el-/bin/abu; Persian; Turkish dotless i", {"term": q(["Egypt", "Saudi Arabia", "Iran", "Iraq", "Turkey", "Pakistan"])}),
    ("name-slavic", 4, "name stratum: Slavic/Cyrillic transliteration", "names: Slavic transliterations", {"term": q(["Russia", "Poland", "Czech Republic", "Serbia", "Ukraine"])}),
    ("name-nordic", 4, "name stratum: Nordic", "names: o-slash, a-ring, ae, Icelandic", {"term": q(["Sweden", "Denmark", "Norway", "Finland"])}),
]
if __name__ == "__main__":
    G.run_pmc(strata=NAMES, need_refs=10)
