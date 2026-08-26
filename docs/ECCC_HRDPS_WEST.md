# ECCC experimental HRDPS-West 1 km contract

Rusty Weather identifies this lane as `hrdps-west`. It is deliberately
separate from the operational continental `hrdps` model.

## Authoritative distribution contract

- Producer and licensing publisher: Environment and Climate Change Canada,
  Meteorological Service of Canada.
- Product documentation:
  <https://eccc-msc.github.io/open-data/msc-data/nwp_hrdps/readme_hrdps-datamart-alpha_en/>
- Anonymous object root:
  `https://dd.alpha.weather.gc.ca/model_hrdps/west/1km/grib2/{HH}/{hhh}/`
- Status: experimental data on ECCC's non-operational DD-Alpha test service.
- Cycles and horizon: 00/12 UTC, hourly f000-f048.
- Retention: only 24 hours of rolling source history.
- Published domain: most of British Columbia and western Alberta.
- Model vertical grid: 62 hybrid/SLEVE levels.
- Published GRIB grid: 1330x1180 rotated latitude/longitude, nominal 1 km,
  filename token `rotated_latlon0.009x0.009`.
- Filename:
  `CMC_hrdps_west_{component}_rotated_latlon0.009x0.009_{YYYYMMDD}T{HH}Z_P{hhh}-00.grib2`.

The HRDPS-West 1.5 technical specification describes a 1350x1200 Arakawa-C
computational grid. That is not the DD-Alpha distribution grid. Live published
objects and the distribution documentation both resolve to 1330x1180, which is
the only grid Rusty Weather advertises for this lane.

Technical specification:
<https://collaboration.cmc.ec.gc.ca/cmc/cmoi/product_guide/docs/tech_specifications/tech_specifications_HRDPS-WEST_1.5.0_e.pdf>

The official changelog records HRDPS-West v1.6.0 on 2026-04-14 for the MSC
high-performance-computing migration. The DD-Alpha distribution document and
live object metadata above remain the acquisition authority:
<https://eccc-msc.github.io/open-data/msc-data/nwp_hrdps/changelog_hrdps-west_en/>.

The ECCC Data Servers End-use Licence v2.1 (September 2022) permits worldwide
royalty-free use, including commercial copying, modification, publication,
and distribution, subject to its terms. Redistributed output must retain:

`Data Source: Environment and Climate Change Canada`

Licence: <https://eccc-msc.github.io/open-data/licence/readme_en/>

## Bounded acquisition and normalization

DD-Alpha publishes one one-message GRIB2 object per variable/level/lead. RWS
expands `rws-pressure` and `rws-surface` into exact allowlisted components; it
does not download an unfiltered model cycle. The live common pressure inventory
for TMP, RH, UGRD, VGRD, and HGT is:

`50, 100, 150, 175, 200, 225, 250, 275, 300, 350, 400, 450, 500, 550, 600, 650, 700, 750, 800, 850, 875, 900, 925, 950, 970, 985, 1000, 1015 hPa`.

The canonical sounding profile selects the schema-requested 100-1000 hPa
subset. Surface acquisition uses the West-specific `TGL`/`SFC` naming dialect,
including `TMP_TGL_2`, paired `UGRD_TGL_10`/`VGRD_TGL_10`, `PRMSL_MSL_0`,
`PRES_SFC_0`, and `HGT_SFC_0`.

The analysis lead is not assumed to have the positive-lead inventory. In the
2026-08-14 capture the f000 directory had 333 objects versus 338 at f001:
accumulated precipitation and surface height are among the roles absent at
f000. The lead-aware planner omits those objects only at f000 instead of
issuing known 404s or fabricating values; f001-f048 request them normally.
The absolute object count is not treated as an invariant, because DD-Alpha is
an experimental service whose field list may be edited without notice; the
planner keys on component identity.

Live U/V objects use GRIB2 grid template 3.1 and set resolution/component flag
`0x08`, declaring vectors relative to the rotated grid. The decoder therefore:

1. requires a matching U/V pair;
2. requires both messages to describe the same normalized grid;
3. requires the grid-relative component flag;
4. derives the grid-i tangent from the normalized live coordinates; and
5. rotates the pair to canonical earth-relative east/north components.

A missing pair, mismatched grid, unsupported topology, or ambiguous component
metadata fails closed. Derived/heavy diagnostics remain disabled until their
entire vector path is independently verified.

## Pinned evidence

The fixture
`crates/rw-ingest/tests/fixtures/hrdps-west.20260814.t00z.f024.inventory.txt`
pins the official 2026-08-14 00z f024 directory (338 GRIB objects), exact
listing hash, representative filename roles, live grid metadata, pressure
inventory, and SHA-256 identities for bounded U, V, speed, and direction
objects. A newly created but empty cycle directory is not complete evidence;
latest-run discovery requires independent surface and pressure sentinels at
the terminal f048 lead and falls back to the newest complete cycle. This avoids
admitting a cycle while DD-Alpha is still publishing its earlier leads.
At the 2026-08-14 capture, both 00z f048 sentinels returned HTTP 200 while the
same-day 12z terminal sentinels returned 404 even though the 12z directory tree
already existed, directly exercising the required fallback behavior.

## Re-verification, 2026-08-25

The 2026-08-14 fixture cycle is long past DD-Alpha's 24-hour retention and
cannot be refetched, so the contract was re-checked against that day's live
12z cycle instead:

| Check | Result |
| --- | --- |
| `.../west/1km/grib2/12/` | all 49 lead directories f000-f048 present |
| f000 listing | 331 GRIB2 objects; `HGT_SFC_0` and `APCP_SFC_0` absent |
| f001 listing | 336 GRIB2 objects; `HGT_SFC_0` and `APCP_SFC_0` present |
| Common pressure inventory | TMP, RH, UGRD, VGRD, HGT and SPFH each at the same 28 levels, 50 through 1015 hPa |
| Surface dialect | `TMP_TGL_2`, `DPT_TGL_2`, `RH_TGL_2`, `UGRD_TGL_10`, `VGRD_TGL_10`, `WIND_TGL_10`, `WDIR_TGL_10`, `GUST_TGL_10`, `PRES_SFC_0`, `PRMSL_MSL_0`, `TCDC_SFC_0` all present |
| Same-day 00z leads | directories present but empty, consistent with the documented 24-hour rolling retention |
| Ranged read of f001 `UGRD_ISBL_0500` | HTTP 206; GRIB2 section 1 originating centre 54 (CMC), reference time 2026-08-25 12 UTC |
| Section 3 of that message | template 3.1 (rotated lat/lon), 1,569,400 points, Ni 1330, Nj 1180, increments 0.00899 degrees, southern rotation pole near 33.443381S, rotation angle 0 |
| Resolution and component flags | `0x38`; the `0x08` bit is set, so U/V are resolved relative to the rotated grid and must be rotated before use |

The published grid is therefore still 1330x1180, not the technical
specification's 1350x1200 computational grid, and the grid-relative vector
contract that the paired rotation path depends on is confirmed live. The
per-lead object count drifted slightly from the 2026-08-14 capture (331/336
versus 333/338), which is expected for an experimental feed and is why no test
asserts an absolute object count.
