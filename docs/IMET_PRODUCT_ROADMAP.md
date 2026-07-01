# IMET-Focused Product Roadmap for CAFire.org

Date: 2026-07-01. Built from an 18-agent research pass (IMET workflow/pain
points, key-free data verification with live endpoint checks, competing-tool
landscape, fire-science product design) plus the platform inventory. Evidence
URLs live in the workflow transcript; headline facts were adversarially
verified against live endpoints on 2026-07-01.

## Why the thesis works (verified pain points)

- IMETs juggle a dozen fragmented tools (spot page, WFAS, SPC, GACC PDFs, HDW
  site, BlueSky, FEMS, EGP-behind-login). Consolidation is the documented
  unmet need; NOAA's own Fire Weather Testbed exists because of it.
- The NWS actively pushes users OFF spot forecasts toward self-service
  planning tools (NWSI 10-401 discourages unnecessary requests) — tools that
  barely exist. A perimeter-framed, point-meteogram self-service page fills a
  gap the NWS itself is creating.
- The only operational percentile-context product (hdwindex.fs2c.usda.gov) is
  0.5° GEFS, daily-max only, and its authors concede it cannot resolve
  terrain. Our RTMA 2.5 km ±7-day climatology (2019-2026, complete on node 2)
  is two orders of magnitude finer. This is the single biggest differentiator.
- IMETs work 14-16 h days on satellite bandwidth; pre-rendered, fast, mobile-
  legible graphics (exactly our static-render architecture) beat GIS portals.
- Legacy indices are institutionally embedded but scientifically deprecated
  (NWCG recommends dropping Haines, calls LAL duplicative): speak both
  vocabularies — lead with HDW/VPD, still publish Haines/C-Haines/LAL.

## Verified key-free data (live-checked 2026-07-01)

| Source | Access | Key | Notes |
|---|---|---|---|
| HRRR (incl. smoke, **native levels wrfnat**, **15-min subhourly wrfsubh**) | s3://noaa-hrrr-bdp-pds, NOMADS | none | already ingesting sfc/prs subset |
| RAP (13 km, native levels) | s3://noaa-rap-pds | none | profile lane companion |
| **NBM v5.0** (2.5 km, operational Apr 2026) | s3://noaa-nbm-grib2-pds | none | **dedicated fire-wx elements incl. probability of critical RH**; day 8+ horizon |
| **HREF ensemble** | nomads .../com/href/prod (corrected path) | none | prob/mean/spread/ffri to 48 h |
| **RRFS/REFS** — operational **Aug 31, 2026** | s3://noaa-rrfs-pds (prototype, current daily) | none | be ready day one; HRRR's successor |
| GEFS + GEFS-Aerosols smoke | s3://noaa-gefs-pds | none | multi-day smoke outlook |
| RTMA/URMA (+ 15-min RTMA-RU on NOMADS) | s3://noaa-rtma-pds | none | archive deeper than 2019 |
| MRMS (1 km radar/QPE/lightning) | s3://noaa-mrms-pds | none | outflow nowcasting, wetting-rain |
| Stage IV QPE (RFC-QC'd) | nomads .../com/pcpanl/prod | none | wetting-rain verification |
| api.weather.gov | /alerts (RFW live-verified), /zones/fire (geometry per-zone), gridpoints | none | overlay Red Flag polygons everywhere |
| **FEMS (WIMS replacement)** RAWS obs + NFDRS + live fuel moisture | fems.fs2c.usda.gov `/api/climatology/download-weather` and `download-nfdr` CSV/JSON — **keyless, verified incl. multi-year pulls** | none (CSV); official GraphQL API needs free FAMAuth acct + "FEMS API" role (Mar 2026 policy) | full period-of-record archive |
| GOES ABI + GLM | anonymous AWS (already ingesting via rw-sat / rw-glm) | none | FDC hot spots + lightning density |
| NASA FIRMS VIIRS/MODIS detections | free registration key | free-reg | hot-spot overlays |
| gridMET / LANDFIRE / WFAS / Drought Monitor | public | none | already using gridMET/LANDFIRE |
| ⚠ Synoptic/MesoWest | — | **no general free tier since spring 2025** (.edu-only Open Access) | avoid; FEMS + api.weather.gov + IEM cover obs |

## Product tiers (ranked)

### Tier 1 — build from data already in hand

1. **Percentile-anchored anomaly suite** — HRRR forecast max VPD / min RH /
   max wind / gust / surface HDW rendered as "percentile vs this date,
   2019-2026" using the node-2 RTMA DOY climatology (~20 GB Wide-West pack).
   Maps + perimeter domains + county tables (region-summary machinery
   exists). Nobody else has this at 2.5 km.
2. **Overnight RH recovery quality vs climatology** — the 12Z-06Z window
   store exists; quantifies the ambiguous "poor/fair" adjectives.
3. **Critical-threshold timing/duration/alignment maps** — hours and first-
   crossing time of RH<15% + wind≥20/25 mph (SPC/RFW definitions), with the
   joint-hours climatology for context. The burn-period timing brief.
4. **True profile HDWI + Haines/C-Haines** — pressure-level fields are in the
   store; publish `profile_hdwi_*` alongside `surface_hdw_*` per the naming
   contract; keep legacy Haines for IAP compatibility.
5. **Perimeter "pressure on the line"** — spread-aligned wind sectors on the
   perimeter (wind·outward-normal + upslope alignment from the DEM), hourly.
   Uses the just-built perimeter framing/overlay.
6. **GLM lightning density + holdover risk** — GLM flash store × 100/1000-h
   dead fuel moisture × precip since strike = "where should recon look."
7. **Incident meteogram / self-service spot-planning page** — point
   time-series machinery exists; fills the NWS self-service gap.
8. **Mixing height / transport wind / ventilation category + inversion
   breakdown timing** — needs HPBL + more levels added to ingest (free, same
   HRRR files).

### Tier 2 — new free feeds, high value

9. **NBM fire-weather probabilities** (prob. of critical RH, day 3-8) — the
   planning horizon HRRR lacks.
10. **HREF exceedance probabilities** ("70% chance gusts >35 mph 2-6 PM").
11. **RRFS readiness** before Aug 31, 2026 operational date.
12. **RAWS/NFDRS via FEMS keyless CSV** — station overlays + a
    forecast-vs-obs-vs-climatology verification dashboard (relieves the
    0400-0600 scramble; closes the trust loop).
13. **GOES fire detection + fire-temperature RGB loops** on incident domains
    (rw-sat already ingests ABI).
14. **HRRR subhourly (15-min) burn-period winds** + MRMS/GLM convective
    outflow nowcast cues.
15. **Dry-thunderstorm risk grids** (CAPE family + QPF + HRRR LTNG).
16. **Multi-day smoke outlook** (GEFS-Aerosols beyond HRRR-Smoke's 48 h).
17. **Red Flag Warning / fire-zone overlays** from api.weather.gov on every
    product.

### Tier 3 — bigger bets

18. Hourly fine dead-fuel-moisture nowcast (anchored to gridMET daily).
19. PyroCb/plume-potential composite (PFT + C-Haines + ECAPE machinery).
20. Santa Ana / Diablo / Sundowner regime detection with severity ranked
    against the RTMA archive.
21. Wind-shift/frontal timing maps (the classic fatality-scenario brief).
22. Fosberg FWI + Goodrick modification (hourly index vocabulary).

## Strategy notes

- Naming discipline is a trust feature for this audience: `surface_hdw_*` vs
  `profile_hdwi_*`, "weather-only" qualifiers — the node-2 product contract
  already encodes this; keep it on the website.
- Every product should render in three frames: CONUS/West context, CA/Wide
  West, and perimeter-framed incident domain.
- Design for the ATMU: pre-rendered WebP, small payloads, no login, works on
  a phone in a fire camp.
- Obs strategy: FEMS (keyless CSV) + api.weather.gov + IEM. Do not build on
  Synoptic/MesoWest (monetized 2025).
