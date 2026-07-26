"""Emit the Rust CITY_FOOTPRINTS table: (name, state, radius_km, anchor lat, lon).

Radii are mine (about 0.9*sqrt(land_area/pi), deliberately conservative).
ANCHORS come from GeoNames, not from the Census file, because the Census
"internal point" only has to fall inside the polygon: San Francisco's is 50 km
offshore among the Farallon Islands, so a radius around it would not contain the
city at all. GeoNames' point is the populated-place location -- downtown -- which
is what "am I in this city" should measure from.
"""
import math

# (Census/gazetteer name, USPS state, footprint radius km)
CITIES = [
    ("New York", "NY", 14.0), ("Los Angeles", "CA", 18.0), ("Chicago", "IL", 13.0),
    ("Houston", "TX", 21.0), ("Phoenix", "AZ", 19.0), ("Philadelphia", "PA", 10.0),
    ("San Antonio", "TX", 18.0), ("San Diego", "CA", 15.0), ("Dallas", "TX", 15.0),
    ("San Jose", "CA", 11.0), ("Austin", "TX", 15.0), ("Jacksonville", "FL", 22.0),
    ("Fort Worth", "TX", 15.0), ("Columbus", "OH", 12.0), ("Indianapolis", "IN", 16.0),
    ("Charlotte", "NC", 14.0), ("San Francisco", "CA", 6.0), ("Seattle", "WA", 8.0),
    ("Denver", "CO", 10.0), ("Oklahoma City", "OK", 20.0), ("Nashville-Davidson", "TN", 19.0),
    ("Washington", "DC", 8.0), ("El Paso", "TX", 13.0), ("Las Vegas", "NV", 10.0),
    ("Boston", "MA", 6.0), ("Detroit", "MI", 10.0), ("Portland", "OR", 10.0),
    ("Louisville", "KY", 15.0), ("Memphis", "TN", 14.0), ("Baltimore", "MD", 8.0),
    ("Milwaukee", "WI", 8.0), ("Albuquerque", "NM", 11.0), ("Tucson", "AZ", 13.0),
    ("Fresno", "CA", 9.0), ("Sacramento", "CA", 8.0), ("Kansas City", "MO", 15.0),
    ("Mesa", "AZ", 11.0), ("Atlanta", "GA", 10.0), ("Omaha", "NE", 9.0),
    ("Colorado Springs", "CO", 11.0), ("Raleigh", "NC", 10.0), ("Virginia Beach", "VA", 13.0),
    ("Long Beach", "CA", 8.0), ("Miami", "FL", 5.0), ("Oakland", "CA", 7.0),
    ("Minneapolis", "MN", 6.0), ("Tulsa", "OK", 11.0), ("Bakersfield", "CA", 11.0),
    ("Wichita", "KS", 10.0), ("Arlington", "TX", 8.0), ("Aurora", "CO", 12.0),
    ("Tampa", "FL", 11.0), ("New Orleans", "LA", 11.0), ("Cleveland", "OH", 7.0),
    ("Anaheim", "CA", 8.0), ("Urban Honolulu", "HI", 8.0),
    ("Riverside", "CA", 9.0), ("Santa Ana", "CA", 6.0), ("Corpus Christi", "TX", 11.0),
    ("Lexington-Fayette", "KY", 17.0), ("Stockton", "CA", 8.0), ("St. Paul", "MN", 6.0),
    ("Cincinnati", "OH", 7.0), ("Pittsburgh", "PA", 6.0), ("Anchorage", "AK", 25.0),
    ("Greensboro", "NC", 11.0), ("Toledo", "OH", 9.0), ("Newark", "NJ", 5.0),
    ("Buffalo", "NY", 6.0), ("Chandler", "AZ", 9.0), ("Reno", "NV", 9.0),
    ("Boise City", "ID", 9.0), ("Spokane", "WA", 8.0), ("Richmond", "VA", 8.0),
    ("Baton Rouge", "LA", 10.0), ("Des Moines", "IA", 9.0), ("Tacoma", "WA", 8.0),
    ("Birmingham", "AL", 14.0), ("Rochester", "NY", 6.0), ("Salt Lake City", "UT", 9.0),
    ("Huntsville", "AL", 14.0), ("Chattanooga", "TN", 11.0), ("Knoxville", "TN", 10.0),
    ("Mobile", "AL", 11.0), ("Savannah", "GA", 9.0), ("Charleston", "SC", 10.0),
    ("Shreveport", "LA", 11.0), ("Lincoln", "NE", 9.0), ("Sioux Falls", "SD", 9.0),
    ("Little Rock", "AR", 12.0), ("Orlando", "FL", 9.0), ("St. Louis", "MO", 7.0),
    ("Jackson", "MS", 12.0), ("Augusta-Richmond County", "GA", 15.0),
    ("Columbus", "GA", 14.0), ("Chesapeake", "VA", 17.0), ("Norfolk", "VA", 8.0),
    ("Newport News", "VA", 9.0), ("Suffolk", "VA", 19.0), ("Lubbock", "TX", 9.0),
    ("Laredo", "TX", 10.0), ("Fort Wayne", "IN", 10.0), ("Madison", "WI", 9.0),
    ("Fayetteville", "NC", 11.0), ("Salem", "OR", 8.0), ("Eugene", "OR", 8.0),
    ("Springfield", "MO", 9.0), ("Fort Collins", "CO", 9.0), ("Provo", "UT", 7.0),
    ("Billings", "MT", 8.0), ("Cheyenne", "WY", 7.0), ("Fargo", "ND", 9.0),
    ("Bismarck", "ND", 7.0), ("Missoula", "MT", 7.0), ("Bend", "OR", 7.0),
    ("Redding", "CA", 8.0), ("Chico", "CA", 7.0), ("Medford", "OR", 6.0),
    ("Flagstaff", "AZ", 8.0), ("Santa Fe", "NM", 8.0), ("Grand Junction", "CO", 6.0),
]

US_TSV = "C:/Users/drew/rusty-fire-weather/crates/rustwx-products/src/places/us_places_gazetteer.tsv"


def km(lat1, lon1, lat2, lon2):
    r = 6371.0088
    p1, p2 = math.radians(lat1), math.radians(lat2)
    a = (math.sin((p2 - p1) / 2) ** 2
         + math.cos(p1) * math.cos(p2) * math.sin(math.radians(lon2 - lon1) / 2) ** 2)
    return 2 * r * math.asin(math.sqrt(a))


gaz = {}
with open(US_TSV, encoding="utf-8") as handle:
    for line in handle:
        if line.startswith("#") or not line.strip():
            continue
        name, state, lat, lon = line.rstrip("\n").split("\t")
        gaz.setdefault((name, state), []).append((float(lat), float(lon)))

geo = {}
with open("geonames/cities5000.txt", encoding="utf-8") as handle:
    for line in handle:
        f = line.rstrip("\n").split("\t")
        if len(f) < 15 or f[6] != "P" or f[8] != "US":
            continue
        key = (f[2], f[10])
        pop = int(f[14] or 0)
        if key not in geo or pop > geo[key][2]:
            geo[key] = (float(f[4]), float(f[5]), pop)

# GeoNames spells a few consolidated names differently from the Census file.
GEO_ALIAS = {
    ("Nashville-Davidson", "TN"): ("Nashville", "TN"),
    ("Lexington-Fayette", "KY"): ("Lexington", "KY"),
    ("Augusta-Richmond County", "GA"): ("Augusta", "GA"),
    ("Boise City", "ID"): ("Boise", "ID"),
    ("Urban Honolulu", "HI"): ("Honolulu", "HI"),
    ("St. Paul", "MN"): ("Saint Paul", "MN"),
    ("New York", "NY"): ("New York City", "NY"),
}

out, problems = [], []
for name, state, radius in CITIES:
    rows = gaz.get((name, state))
    if not rows:
        problems.append(f"NOT IN GAZETTEER: {name}, {state}")
        continue
    anchor = geo.get(GEO_ALIAS.get((name, state), (name, state)))
    if not anchor:
        problems.append(f"no GeoNames anchor: {name}, {state}")
        continue
    lat, lon, pop = anchor
    # Sanity: the anchor must plausibly belong to the same city.
    nearest = min(km(lat, lon, r[0], r[1]) for r in rows)
    if nearest > 60.0:
        problems.append(f"anchor {nearest:.0f} km from every gazetteer row: {name}, {state}")
        continue
    out.append((state, name, radius, lat, lon, nearest, pop))

out.sort(key=lambda r: (r[0], r[1]))
print(f"// {len(out)} cities")
for state, name, radius, lat, lon, off, pop in out:
    print(f'    ("{name}", "{state}", {radius:.1f}, {lat:.4f}, {lon:.4f}),'
          f'  // anchor {off:.1f} km from the Census point, pop {pop}')
print()
for problem in problems:
    print("PROBLEM:", problem)
