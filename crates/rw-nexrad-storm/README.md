# rw-nexrad-storm

Pure-Rust-facing, UI- and server-independent decoding contracts for WSR-88D
Level III storm products:

- product 58, Storm Tracking Information (STI/NST): authoritative storm IDs,
  current centroids, past positions, forecast positions, motion, and forecast
  error;
- product 62, Storm Structure (SS/NSS): authoritative point attributes such
  as base/top height, cell-based VIL, and maximum reflectivity.

These products contain points and tracks, not storm polygons. A polygon made
from Level II gates, MRMS, or an algorithm remains derived geometry even when
it is paired with a Level III storm ID. The crate's pairing API retains both
provenances and never relabels derived geometry as an RPG polygon.

## Primary specifications

- [WSR-88D RPG Class 1 User ICD](https://www.roc.noaa.gov/public-documents/icds/2620001AD.pdf), Document 2620001AD, Build 24.0
  (19 August 2025): section 3.3.1 and Figures 3-6, 3-8b, 3-14, and 3-16;
  Tables III, VIII, and IX; Appendix D.
- [WSR-88D RPG Product Specification](https://www.roc.noaa.gov/public-documents/icds/2620003AE.pdf), Document 2620003AE, Build 24.0
  (19 August 2025): section 18 and Appendix C, Formats I and V.

The implementation cites the precise figure/table again at each binary or
semantic boundary. Tests construct spec-shaped messages rather than relying
on filenames or undocumented third-party interpretations.

## Output contract

`decode` accepts either the binary message or the common short WMO/AWIPS
prefix followed by the binary message. Exact packet I/J coordinates remain
signed quarter-kilometre integers. Convenience latitude/longitude is marked
as the crate's spherical radar-centric derivation. The current point receives
the volume timestamp; historical points deliberately receive no invented
timestamp because product 58 does not encode one per point. Forecast times are
only produced when the product's own adaptation table provides the interval.

The AWIPS PIL contains a three-character radar token (for example `TLX`), not
an ICAO prefix. Pass `DecodeOptions::site_hint` when an upstream source knows
the exact four-character site (`KTLX`). Geometry pairing accepts the explicit
three/four-character suffix relationship, requires a time window, and uses a
bounded nearest-centroid association. Its result embeds both the Level III
identity and the untouched Level II geometry provenance.

All offsets, inclusive block lengths, packet lengths, page/line counts, ASCII
fields, bzip2 output sizes, and configured resource limits are checked before
use. Decoding malformed or truncated input returns `DecodeError`; no parser
path intentionally panics on input bytes.
