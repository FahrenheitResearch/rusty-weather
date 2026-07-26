"""Convert GeoNames cities5000 into the 4-column gazetteer TSV the renderer reads.

Same shape as us_places_gazetteer.tsv: name<TAB>region<TAB>lat<TAB>lon, where the
second column is the COUNTRY NAME for international rows (the US file uses the
USPS state). Country names rather than ISO2 codes because half the two-letter
codes collide with state abbreviations -- "London, CA" would read as California
when it means Ontario, and MO/MD/ME/LA/DE/IN/AL/PA/MT/NE/IL/VA all collide too.

US rows are dropped: the Census file already covers the US far more densely
(32k places vs ~3.5k here) and two spellings of the same city would make labels
inconsistent.
"""
import sys

SRC = "geonames/cities5000.txt"
OUT = "world_cities_gazetteer.tsv"

# Feature codes worth naming a forecast point after. Excluded on purpose:
# PPLX (a section of a city -- "Shinjuku" where "Tokyo" is the useful answer),
# and everything historical/abandoned/destroyed, which would name a place that
# is not there any more.
KEEP = {
    "PPL", "PPLA", "PPLA2", "PPLA3", "PPLA4", "PPLA5", "PPLC", "PPLG",
    "PPLL", "PPLS", "PPLF", "STLMT",
}

# ISO2 -> country name, from GeoNames' own country table.
countries = {}
with open("countryInfo.txt", encoding="utf-8") as handle:
    for line in handle:
        if line.startswith("#"):
            continue
        parts = line.rstrip("\n").split("\t")
        if len(parts) > 4 and parts[0]:
            countries[parts[0]] = parts[4].strip()

rows = {}
skipped_us = 0
skipped_code = 0
skipped_name = 0
with open(SRC, encoding="utf-8") as handle:
    for line in handle:
        parts = line.rstrip("\n").split("\t")
        if len(parts) < 15:
            continue
        name, ascii_name = parts[1], parts[2]
        lat, lon = parts[4], parts[5]
        feature_class, feature_code, country = parts[6], parts[7], parts[8]
        if feature_class != "P":
            continue
        if country == "US":
            skipped_us += 1
            continue
        if feature_code not in KEEP:
            skipped_code += 1
            continue
        # The card's fonts cover Latin; an ASCII name never renders as tofu.
        label = "".join(c for c in (ascii_name or name) if 32 <= ord(c) < 127).strip()
        label = " ".join(label.split())
        region = countries.get(country, country)
        if not label or not region:
            skipped_name += 1
            continue
        try:
            lat_f, lon_f = float(lat), float(lon)
            pop = int(parts[14] or 0)
        except ValueError:
            continue
        # Footprint radius for big cities only, so a city beats its own
        # subdivisions and the suburbs ringing it: without this a point by
        # Notre-Dame resolved to "Paris 04 Hotel-de-Ville" (an arrondissement,
        # stored as an ordinary populated place with its own population) rather
        # than to Paris. Small towns stay points, exactly like small US towns.
        # Population is a crude proxy for extent, so the radius is clamped hard;
        # it only has to beat a subdivision of the same city.
        if pop >= 250_000:
            radius = min(12.0, max(3.0, 1.6 * (pop / 1e5) ** 0.5))
        else:
            radius = 0.0
        key = (label, region, round(lat_f, 4), round(lon_f, 4))
        # Keep the largest radius when a city appears more than once.
        rows[key] = max(rows.get(key, 0.0), round(radius, 1))

records = sorted(rows.items(), key=lambda kv: (kv[0][1], kv[0][0], kv[0][2], kv[0][3]))
with open(OUT, "w", encoding="utf-8", newline="\n") as out:
    out.write("# World cities gazetteer: name<TAB>country<TAB>lat<TAB>lon (4dp)\n")
    out.write("# Source: GeoNames cities5000 (populated places, population >= 5000):\n")
    out.write("#   https://download.geonames.org/export/dump/cities5000.zip\n")
    out.write("# This work is based on data from GeoNames (https://www.geonames.org/),\n")
    out.write("# licensed under CC BY 4.0 (https://creativecommons.org/licenses/by/4.0/).\n")
    out.write("# US rows are dropped: us_places_gazetteer.tsv covers the US far more\n")
    out.write("# densely (all Census places), and two spellings of one city would make\n")
    out.write("# labels inconsistent. ASCII names (GeoNames `asciiname`) so no glyph is\n")
    out.write("# missing from the card fonts. Sections of cities (PPLX) and\n")
    out.write("# historical/abandoned places are excluded.\n")
    out.write("# Regenerate: see docs/AGENT_GUIDE.md gotcha ledger.\n")
    for (label, country, lat_f, lon_f), radius in records:
        out.write(f"{label}\t{country}\t{lat_f:.4f}\t{lon_f:.4f}\t{radius:.1f}\n")

print(f"wrote {len(records)} rows to {OUT}")
print(f"skipped: {skipped_us} US, {skipped_code} by feature code, {skipped_name} unnamed")
countries = len({k[1] for k, _ in records})
print(f"countries/territories: {countries}")
for probe in ("London", "Tokyo", "Sydney", "Paris", "Brasilia", "Reykjavik", "Ushuaia"):
    hits = [(k, v) for k, v in records if k[0] == probe]
    print(f"  {probe}: {hits[:2]}")
