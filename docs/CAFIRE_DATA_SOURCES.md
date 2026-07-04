# CAFire Data Provenance Reference

*Where every CAFire.org product's data comes from.*

This is the single source-of-truth for anyone asking "where does this number come from?" — whether you are integrating CAFire products into another site or just want to understand a map like ERC or a fire-weather composite. For each product we name the **upstream dataset**, the **specific variable or formula**, and the **update cadence**. Products fall into two broad buckets: **native fields** read straight from a public dataset (a weather model, gridMET, LANDFIRE, etc.) and **derived products** we compute on our own compute nodes from those inputs. Both are documented here. Start with the "Data sources at a glance" table below — it answers most questions in one row.

---

## Data sources at a glance

| Dataset | What it is | Who publishes it | Public access / attribution | Update cadence | Product families it feeds |
|---|---|---|---|---|---|
| **HRRR** (High-Resolution Rapid Refresh, 3 km CONUS) | Rapid-refresh 3 km CONUS weather forecast model; our primary fire-weather model and base grid | NOAA / NCEP (operational) | GRIB2 from NOMADS (`nomads.ncep.noaa.gov`), fallback AWS Open Data (`s3://noaa-hrrr-bdp-pds`), also Google Cloud & Azure | Hourly cycles 00–23Z; a new run lands ~50–90 min after its init time | Standard surface maps; Day-window extremes; Upper-air & CAPE; ECAPE/PFT; the forecast side of the RTMA anomaly suite; point products |
| **GFS** (Global Forecast System, 0.25°) | Global atmospheric forecast; our medium-range / daily-outlook lane | NOAA / NCEP (operational) | GRIB2 from NOMADS, fallback AWS Open Data (`s3://noaa-gfs-bdp-pds`), also Google Cloud | 6-hourly (00/06/12/18Z); ~3.5–5 h publication lag | Point products (medium-range meteogram / daily cards) |
| **NBM** (National Blend of Models, core CONUS) | NWS statistical multi-model blend (post-processed, not a single dynamical model); our long-range (~day-11) lane | NOAA / NWS (NCEP "blend" product) | GRIB2 from NOMADS, fallback AWS Open Data (`s3://noaa-nbm-grib2-pds`) | Ingested 6-hourly (00/06/12/18Z); long-range outlook to ~day 11 | Point products (long-range meteogram / daily cards) |
| **gridMET** | Daily gridded surface meteorology and NFDRS fire-danger grids for CONUS | Climatology Lab / University of Idaho (Northwest Knowledge Network) | NetCDF from `https://www.northwestknowledge.net/metdata/data`; cite Abatzoglou 2013 (doi:10.1002/joc.3413) | Daily, with ~1–2 day publication lag | Fuels (ERC, Burning Index, dead-fuel moisture, precip context; inputs to KBDI and all fuel composites) |
| **LANDFIRE** | Static surface fuel model and fuel-loading layers (what fuel is on the ground) | USGS / USFS (LANDFIRE program) | `https://landfire.gov`; supplied to CAFire manually as a versioned static layer (no automated download) | Static / versioned — re-imported only when a new LANDFIRE release is supplied | Fuels (fuel model, fuel loading) |
| **Our RTMA climatology archive (2019–2026)** | A frozen climatology built from NOAA's RTMA 2.5 km real-time surface analyses, 2019–2026, used as the "what's normal" baseline for anomaly ranking | Built by CAFire from NOAA/NCEP RTMA analyses | Underlying analyses: NOAA/NCEP RTMA (De Pondeca et al. 2011). Plots are labeled "RTMA 2.5 km analysis climatology \| 2019–2026 archive" | Static (frozen archive; formula manifest v2026_05_24) | RTMA anomaly suite & climatology browser; climatology reference bands on point products |
| **WFIGS current interagency perimeters** | Live active-fire perimeter polygons (national) | Wildland Fire Interagency Geospatial Services (WFIGS) | Public, key-free ArcGIS FeatureServer: `services3.arcgis.com/T4QMspbfLg3qTGWY/.../WFIGS_Interagency_Perimeters_Current/FeatureServer/0` | Updates continuously as incidents report *(external characterization)*; CAFire serves from a 10-minute cache | Fire perimeters (`GET /api/fires`) |
| **US Census Gazetteer (places, national)** | List of ~32,000 US incorporated places and census-designated places with coordinates | US Census Bureau | Public domain: `https://www2.census.gov/geo/docs/maps-data/data/gazetteer/2025_Gazetteer/2025_Gaz_place_national.zip` (2025 vintage) | Static asset; annual Census release | Point products (nearest-community labels on every meteogram, daily card, cross-section, sounding) |

---

## Forecast & analysis models (the raw model feeds)

These are the raw weather-model GRIB feeds CAFire ingests. Almost every map on the site is built on top of HRRR; GFS and NBM extend the point products further out in time.

| Product | What it shows | Data source | Detail (variable / method / attribution) | Cadence |
|---|---|---|---|---|
| **HRRR 3 km** | 3 km CONUS rapid-refresh forecast; the base grid for nearly every product | NOAA HRRR (NCEP) | Two files per hour: surface `wrfsfc` + pressure `wrfprs`. Source priority: NOMADS → AWS (`noaa-hrrr-bdp-pds`) → Google Cloud → Azure. Forecast length F0–F48 on 00/06/12/18Z, F0–F18 off-cycle | Hourly cycles 00–23Z; new run ~50–90 min after init |
| **GFS 0.25°** | Global forecast; medium-range / daily-outlook lane | NOAA GFS (NCEP) | One file per hour, product `pgrb2.0p25`. Source priority: NOMADS → AWS (`noaa-gfs-bdp-pds`) → Google Cloud → NCEI archive (note: the NCEI fallback serves the 0.5° grid-004 archive, not the 0.25° operational product). Forecast length F0–F192 (step 6) | 6-hourly (00/06/12/18Z); ~3.5–5 h lag |
| **NBM core CONUS** | NWS statistical blend; long-range (~day-11) lane | NOAA/NWS NBM (NCEP "blend") | One file per hour, product `core/co` (CONUS). Source priority: NOMADS → AWS (`noaa-nbm-grib2-pds`). Forecast length F6–F264 (step 6, ~day 11). A post-processed multi-model blend, not a single dynamical model | Ingested 6-hourly (00/06/12/18Z) |

---

## Standard surface weather maps

Everyday surface maps. Most are read straight from the HRRR surface file (`wrfsfc`) with no math; a handful are fire-weather indices we compute from those same surface fields.

| Product | What it shows | Data source | Detail (variable / method / attribution) | Cadence |
|---|---|---|---|---|
| 2 m Temperature | Air temperature at 2 m (also under 10 m wind combos) | HRRR (native) | GRIB `TMP:2 m above ground`, straight decode | Hourly |
| 2 m Dewpoint | Dewpoint at 2 m | HRRR (native) | GRIB `DPT:2 m above ground`, straight decode | Hourly |
| 2 m Relative Humidity | RH at 2 m (also RH + 10 m wind combo) | HRRR (native) | GRIB `RH:2 m above ground`, straight decode | Hourly |
| 10 m Wind | 10 m wind barbs over surface fills | HRRR (native) | GRIB `UGRD`/`VGRD:10 m above ground`; speed = √(u²+v²). No standalone wind-speed fill | Hourly |
| 10 m Wind Gusts | Surface wind-gust magnitude | HRRR (native) | GRIB `GUST` (surface / 10 m) / `WSPD10MAX` | Hourly |
| Visibility | Surface horizontal visibility | HRRR (native) | GRIB `VIS:surface` | Hourly |
| Near-Surface Smoke (PM2.5, 8 m) | Near-surface smoke mass density | HRRR native volume (`wrfnat`) — HRRR only | GRIB `MASSDEN:8 m above ground`. Verified/wired for HRRR only (RRFS explicitly blocked) | Hourly |
| Column-Integrated Smoke | Smoke integrated over the whole column | HRRR native volume (`wrfnat`) — HRRR only | GRIB `COLMD:entire atmosphere` | Hourly |
| 2 m Vapor Pressure Deficit (VPD) | Atmospheric moisture demand at 2 m (hPa) | **Derived** from HRRR surface fields | VPD = saturation vapor pressure(T₂ₘ) − vapor pressure(Td₂ₘ), clamped ≥ 0. Not produced on NBM/RTMA/URMA/HREF/REFS | Hourly |
| Hot-Dry-Windy Index (HDW) | Fire-weather index: 2 m moisture deficit × 10 m wind (hPa·m/s) | **Derived** from HRRR surface fields | HDW = VPD × wind speed. A surface proxy, not the canonical lowest-500 m-max HDW | Hourly |
| Fire Weather Composite | Public 0–100 fire-weather danger blend | **Derived** from HRRR surface fields | clamp(0.5 × Fosberg FWI + 0.5 × capped HDW, 0–100) | Hourly |

---

## Day-window extremes

HRRR forecast fields folded over fixed windows — the warmest/coldest/driest value each grid cell reaches over the next 0–24 h, 24–48 h, or 0–48 h, plus trailing-window precipitation. All read from HRRR surface files; "range" products are max − min and QPF windows are sums of hourly precipitation, so those are marked derived. The 24 h / 48 h windows require an extended (00/06/12/18Z) HRRR cycle that reaches F24/F48.

| Product (slug) | What it shows | Data source | Detail (variable / method) | Cadence |
|---|---|---|---|---|
| `2m_temp_0_24h_max` / `_24_48h_max` / `_0_48h_max` | Warmest 2 m temperature over the window | HRRR (native) | `TMP:2 m`, pointwise max over the window's forecast hours | HRRR extended cycles |
| `2m_temp_0_24h_min` / `_24_48h_min` / `_0_48h_min` | Coldest 2 m temperature | HRRR (native) | `TMP:2 m`, pointwise min | HRRR extended cycles |
| `2m_temp_0_24h_range` / `_24_48h_range` / `_0_48h_range` | Diurnal temperature range | **Derived** from HRRR | max − min of hourly `TMP:2 m` | HRRR extended cycles |
| `2m_rh_0_24h_max` / `_24_48h_max` / `_0_48h_max` | Highest 2 m RH | HRRR (native) | `RH:2 m`, pointwise max (clamped 0–100 %) | HRRR extended cycles |
| `2m_rh_0_24h_min` / `_24_48h_min` / `_0_48h_min` | Lowest 2 m RH (driest moment) | HRRR (native) | `RH:2 m`, pointwise min | HRRR extended cycles |
| `2m_rh_0_24h_range` / `_24_48h_range` / `_0_48h_range` | 2 m RH range | **Derived** from HRRR | max − min of hourly `RH:2 m` | HRRR extended cycles |
| `2m_dewpoint_0_24h_max` / `_24_48h_max` / `_0_48h_max` | Highest 2 m dewpoint | HRRR (native) | `DPT:2 m`, pointwise max | HRRR extended cycles |
| `2m_dewpoint_0_24h_min` / `_24_48h_min` / `_0_48h_min` | Lowest 2 m dewpoint | HRRR (native) | `DPT:2 m`, pointwise min | HRRR extended cycles |
| `2m_dewpoint_0_24h_range` / `_24_48h_range` / `_0_48h_range` | 2 m dewpoint range | **Derived** from HRRR | max − min of hourly `DPT:2 m` | HRRR extended cycles |
| `2m_vpd_0_24h_max` / `_24_48h_max` / `_0_48h_max` | Peak 2 m VPD (driest demand) | **Derived** from HRRR | Per-hour VPD = eₛ(T)·(1 − RH/100) from `TMP:2 m` + `RH:2 m` (dewpoint fallback), pointwise max (hPa) | HRRR extended cycles |
| `2m_vpd_0_24h_min` / `_24_48h_min` / `_0_48h_min` | Lowest 2 m VPD (most humid) | **Derived** from HRRR | Per-hour VPD, pointwise min | HRRR extended cycles |
| `2m_vpd_0_24h_range` / `_24_48h_range` / `_0_48h_range` | 2 m VPD range | **Derived** from HRRR | max − min of per-hour VPD | HRRR extended cycles |
| `10m_wind_0_24h_max` / `_24_48h_max` / `_0_48h_max` | Peak 10 m wind speed | HRRR (native) | `WIND:10 m` native hourly max, pointwise max across the window (m/s → kt). Wind has max-only variants in this family | HRRR extended cycles |
| `qpf_1h` | Precipitation over the trailing 1 h | HRRR (native) | `APCP:surface` 1 h bucket (mm → in) | HRRR hourly (F ≥ 1) |
| `qpf_6h` | Precipitation over the trailing 6 h | HRRR (native / summed) | `APCP:surface`; direct native 6 h bucket where available, otherwise sum of six hourly buckets | HRRR hourly (F ≥ 6) |
| `qpf_24h` | Precipitation over the trailing 24 h | HRRR (native / summed) | `APCP:surface`; direct native 24 h bucket where available, otherwise sum of 24 hourly buckets | HRRR extended cycles (F ≥ 24) |

---

## Fuels (ERC / NFDRS / LANDFIRE / weather × fuel composites)

The fuel-dryness lane. The core NFDRS grids (ERC, Burning Index, dead-fuel moisture) come straight from **gridMET**. KBDI is computed locally from gridMET temperature and precipitation. LANDFIRE layers are static and supplied manually. The composites blend gridMET fuels with HRRR-derived weather.

| Product | What it shows | Data source | Detail (variable / method / attribution) | Cadence |
|---|---|---|---|---|
| Energy Release Component (ERC) | NFDRS fuel-dryness / available-energy index | gridMET (NFDRS-G) | NetCDF `energy_release_component-g` (file `erc_{year}.nc`), regridded onto the model grid | Daily (~1–2 day lag) |
| Burning Index | NFDRS flame-length / difficulty-of-control index | gridMET (NFDRS-G) | NetCDF `burning_index_g` (file `bi_{year}.nc`) | Daily (~1–2 day lag) |
| KBDI (Keetch-Byram Drought Index) | Cumulative soil/duff drought index (0–800) | **Derived** locally from gridMET | Keetch-Byram formula from gridMET `air_temperature` (`tmmx`) + `precipitation_amount` (`pr`); 180-day spin-up; annual-rainfall parameter is a scalar (default 20 in), not yet gridded. Not a gridMET-native product | Daily (recomputed with spin-up) |
| 1 h / 10 h / 100 h / 1000 h Dead Fuel Moisture | Modeled dead-fuel moisture content (%) | gridMET (NFDRS) | NetCDF `dead_fuel_moisture_1hr`/`10hr`/`100hr`/`1000hr` (files `fm1`/`fm10`/`fm100`/`fm1000`) | Daily (~1–2 day lag) |
| Daily Precip Fuel Context | Daily precipitation (in) as fuel-wetting context | gridMET | NetCDF `precipitation_amount` (`pr_{year}.nc`), mm → in | Daily (~1–2 day lag) |
| LANDFIRE Fuel Model | Static surface fuel-model classes | LANDFIRE (USGS/USFS) | Manually imported static layer (no automated downloader); regridded to the model grid | Static / versioned |
| LANDFIRE Fuel Loading | Static fuel loading (tons/ac) | LANDFIRE (USGS/USFS) | Manually imported static layer | Static / versioned |
| Fuel Receptiveness | 0–100 blended fuel-dryness score | **Derived** from gridMET fuels | Unweighted mean of normalized ERC, KBDI, Burning Index and 1/10/100/1000 h dead-fuel moisture (drier = higher); averages whatever fuel grids are present | Daily |
| Fire Potential Composite | Weather + fuel fire potential (0–100) | **Derived**: HRRR weather × gridMET fuels | 0.58 × Fire Weather Composite (HRRR-derived) + 0.42 × Fuel Receptiveness (gridMET) | Per HRRR hour, gated by daily fuels |
| HDW × Fuel Receptiveness | Geometric mean of HDW and fuel receptiveness | **Derived**: HRRR HDW × gridMET fuels | √(normalized HDW/100 × receptiveness/100) × 100 | Per HRRR hour, gated by daily fuels |
| VPD × Fuel Receptiveness | Geometric mean of VPD and fuel receptiveness | **Derived**: HRRR VPD × gridMET fuels | √(normalized VPD/100 × receptiveness/100) × 100 | Per HRRR hour, gated by daily fuels |
| ERC × HDW Composite | Geometric mean of ERC (fuels) and HDW (weather) | **Derived**: gridMET ERC × HRRR HDW | √(normalized ERC/100 × normalized HDW/100) × 100 | Per HRRR hour, gated by daily ERC |

---

## RTMA anomaly suite & climatology browser

These maps answer "how unusual is this?" They take the HRRR forecast for the day and rank it against **our RTMA climatology archive (2019–2026)** — a frozen climatology built from NOAA's RTMA 2.5 km surface analyses. Each cell is scored as a percentile against either the same time of year (a ±7-day day-of-year window, ~105 samples) or the whole record (all analyzed days). "Dryness" and "recovery" products invert the scale so the dangerous end reads high. Plots are labeled *"RTMA 2.5 km analysis climatology \| 2019–2026 archive."*

| Product | What it shows | Data source | Detail (variable / method) | Cadence |
|---|---|---|---|---|
| Day-Max VPD Percentile | Where day-max VPD falls in the 2019–2026 distribution for this day of year | HRRR forecast ranked vs RTMA archive | HRRR day-max VPD (Magnus/Bolton from T₂ₘ + Td₂ₘ) vs archive `max_vpd_2m_kpa`, percentiles p05–p99 | Per HRRR run |
| Day-Min RH Dryness Percentile | How extreme day-min RH is vs normal (driest = 99) | HRRR forecast ranked vs RTMA archive | HRRR day-min RH vs archive `min_rh_2m_pct`, dryness-inverted rank | Per HRRR run |
| Day-Max Wind Percentile | Where day-max sustained 10 m wind ranks | HRRR forecast ranked vs RTMA archive | HRRR day-max √(u²+v²) vs archive `max_wind_10m_ms` | Per HRRR run |
| Day-Max Gust Percentile | Where day-max 10 m gust ranks | HRRR forecast ranked vs RTMA archive | HRRR day-max gust vs archive `max_gust_10m_ms` | Per HRRR run |
| Day-Max Surface HDW (Wind) Percentile | Where day-max surface HDW proxy (VPD × sustained wind) ranks | HRRR forecast ranked vs RTMA archive | vs archive `max_surface_hdw_wind`. Surface proxy, labeled surface_hdw — not true HDWI | Per HRRR run |
| Day-Max Surface HDW (Gust) Percentile | Where day-max surface HDW proxy (VPD × gust) ranks | HRRR forecast ranked vs RTMA archive | vs archive `max_surface_hdw_gust`. Surface proxy | Per HRRR run |
| Hours RH ≤ 15 % & Gust ≥ 25 mph Percentile | How the count of joint-threshold hours over the day ranks | HRRR forecast ranked vs RTMA archive | vs archive `hours_joint_rh15_gust25`; where climatology is zero, renders as no-signal | Per HRRR run |
| Hours RH ≤ 20 % & Gust ≥ 25 mph Percentile | Same, RH ≤ 20 % + gust ≥ 25 mph | HRRR forecast ranked vs RTMA archive | vs archive `hours_joint_rh20_gust25` | Per HRRR run |
| Hours RH ≤ 20 % & Wind ≥ 20 mph Percentile | Same, RH ≤ 20 % + sustained wind ≥ 20 mph | HRRR forecast ranked vs RTMA archive | vs archive `hours_joint_rh20_wind20` | Per HRRR run |
| Overnight RH Recovery Percentile (12Z–06Z) | How the overnight minimum RH ranks — poor recovery reads extreme | HRRR forecast ranked vs RTMA archive | HRRR min RH over the 12Z→06Z window vs archive `min_rh_2m_pct` in that window, dryness-inverted; needs ≥16 of 19 hours | Per HRRR run |
| Surface Fire Weather Potential (weather-only, 0–100) | Weighted blend of six ingredient percentiles | **Derived** from the ranked HRRR fold | Weighted mean of per-cell percentiles: min-RH .22, max-VPD .22, max-wind .16, max-gust .16, HDW-wind .12, HDW-gust .12; needs ≥4 ingredients. Weather-only by design | Per HRRR run |
| *vs-record* siblings of every product above | Same fold ranked against the entire 2019–2026 record (not just this time of year) | HRRR forecast ranked vs RTMA all-period archive | Identical fold and math; only the reference distribution differs (all analyzed days) | Per HRRR run |
| Climatology reference maps (`climo_ref` browser) | Browse the stored archive itself — e.g. "what p95 VPD normally looks like on Jul 15." No forecast involved | RTMA archive (analysis archive) | Renders a stored anchor grid (p05–p99, plus record-only max) in native units. Day-of-year target uses the seasonal ±7-day archive; "record" target uses the all-period archive | Static (frozen 2019–2026 archive) |

---

## Upper-air maps & CAPE severe-weather composites (HRRR)

Standard (non-entraining) upper-air and severe-weather products. The isobaric height/temperature/RH/vorticity maps are native HRRR pressure-level fields; the CAPE/CIN, LCL, helicity, shear and composite indices are computed on our nodes from HRRR profiles.

| Product | What it shows | Data source | Detail (variable / method) | Cadence |
|---|---|---|---|---|
| 850 / 700 / 500 / 300 / 250 / 200 mb Height + Winds | Geopotential-height fill + contours with wind barbs | HRRR pressure file (native) | GRIB `HGT:{level} mb` fill/contours, `UGRD`/`VGRD:{level} mb` barbs | Hourly, to F48 |
| Isobaric Temperature / Height / Winds (per level) | Temperature fill + height contours + barbs | HRRR pressure file (native) | GRIB `TMP:{level} mb` fill | Hourly, to F48 |
| Isobaric RH / Height / Winds (per level) | RH fill + height contours + barbs | HRRR pressure file (native) | GRIB `RH:{level} mb` fill | Hourly, to F48 |
| Isobaric Absolute Vorticity / Height / Winds (per level) | Absolute-vorticity fill + height contours + barbs | HRRR pressure file (native) | GRIB `ABSV:{level} mb` fill | Hourly, to F48 |
| SBCAPE / MLCAPE / MUCAPE | Surface-based / mixed-layer / most-unstable CAPE (non-entraining) | **Derived** from HRRR profiles | Non-entraining parcel lift from HRRR isobaric T/moisture/height + surface fields. Computed, not the native HRRR CAPE plane | Hourly, to F48 |
| SBCIN / MLCIN / MUCIN | Corresponding convective inhibition | **Derived** from HRRR profiles | CIN output of the same parcel lifts | Hourly, to F48 |
| LCL height (`sblcl`) | Lifting condensation level (AGL) | **Derived** from HRRR profiles | Output of the surface-based non-entraining parcel lift | Hourly, to F48 |
| 0–1 km / 0–3 km Storm-Relative Helicity (`srh_0_1km`, `srh_0_3km`) | Storm-relative helicity | **Derived** from HRRR profiles | Computed over native isobaric winds + AGL heights | Hourly, to F48 |
| 0–1 km / 0–6 km Bulk Shear | Bulk wind-difference magnitude | **Derived** from HRRR profiles | Computed over native isobaric winds + AGL heights | Hourly, to F48 |
| Significant Tornado Parameter, fixed-layer (`stp_fixed`) | Fixed-layer STP | **Derived** from HRRR profiles | (SBCAPE/1500) × LCL-term × (0–1 km SRH/150) × shear-term | Hourly, to F48 |
| Supercell Composite Parameter (fixed-layer proxy, `scp_mu_0_3km_0_6km_proxy`) | Supercell composite (experimental fixed-depth proxy) | **Derived** from HRRR profiles | MUCAPE × 0–3 km SRH × 0–6 km shear. The effective-layer SCP is not derived from HRRR profiles and is not served in this lane | Hourly, to F48 |
| Energy-Helicity Index (`ehi_0_1km`, `ehi_0_3km`) | Energy-helicity index | **Derived** from HRRR profiles | CAPE × SRH / 160000, using standard non-entraining CAPE | Hourly, to F48 |

> **Note on products not served here:** LFC and Equilibrium Level are computed internally but are not currently rendered as standalone products. The *effective-layer* STP that ships is the ECAPE-weighted version (see the ECAPE family below), not a non-entraining effective-layer product.

---

## ECAPE suite & PyroCb Firepower Threshold (computed on our compute nodes from HRRR 3-D volumes)

The heavy lane: entraining CAPE (ECAPE, Peters et al.) and the PyroCb Firepower Threshold (PFT, Tory & Kepert 2021), both solved per-column on our compute node from full HRRR 3-D isobaric volumes plus surface fields. The primary ECAPE fields and PFT diagnostics are genuine per-column solves; the ratio and composite products are formula combinations of already-computed grids.

| Product | What it shows | Data source | Detail (variable / method / attribution) | Cadence |
|---|---|---|---|---|
| SB / ML / MU ECAPE | Surface-based / mixed-layer / most-unstable entraining CAPE (J/kg) | Computed on node from HRRR isobaric T/q/u/v + surface | Peters et al. entraining CAPE via the vendored ecape-rs kernel; Bunkers right-moving storm motion; automatic entrainment rate | Hourly |
| SB NCAPE | Surface-based normalized (buoyancy-integral) CAPE (J/kg) | Computed on node from HRRR | Peters et al. buoyancy integral over LFC→EL, a by-product of the SB entraining parcel path | Hourly |
| SB / ML ECAPE CIN | Convective inhibition from the entraining parcel path (J/kg) | Computed on node from HRRR | CIN diagnostic of the SB / ML entraining parcel ascent | Hourly |
| SB / ML / MU ECAPE ÷ derived-CAPE ratio | Entraining CAPE relative to our non-entraining CAPE | **Derived** from HRRR | Elementwise ECAPE / computed-CAPE, same parcel; denominator gated ≥ 100 J/kg | Hourly |
| SB / ML / MU ECAPE ÷ native-CAPE ratio | Entraining CAPE relative to HRRR's own model CAPE | **Derived** from HRRR | Numerator = node-computed ECAPE; denominator = HRRR native SB/ML/MU CAPE GRIB plane. Emitted only when the native plane is present | Hourly |
| ECAPE-weighted SCP (`ecape_scp`, experimental) | Supercell composite using MU ECAPE | **Derived** from HRRR | MU ECAPE + 0–3 km SRH + 0–6 km shear | Hourly |
| ECAPE-weighted EHI 0–1 km / 0–3 km (experimental) | Energy-helicity index using SB ECAPE | **Derived** from HRRR | SB ECAPE + 0–1 km / 0–3 km SRH | Hourly |
| ECAPE-weighted STP (`ecape_stp`, experimental) | Effective-form STP using ML ECAPE/ECIN | **Derived** from HRRR | ML ECAPE + ML ECIN + classic ML-parcel LCL + 0–1 km SRH + 0–6 km shear | Hourly |
| PyroCb Firepower Threshold (`pft_gw`) | Minimum fire heat flux (GW) needed for a pyroCb (lower = more favorable) | Computed on node from HRRR | Tory & Kepert 2021 (PFT1) Eq. 25 per-column solve; no-path columns flagged with a sentinel | Hourly |
| PFT free-convection height (`pft_zfc`) | Height AGL of the marginal free-convection level (m) | Computed on node from HRRR | Component of the Tory & Kepert 2021 PFT solution | Hourly |
| PFT required plume warming (`pft_dtheta_fc`) | Fire-induced potential-temperature excess needed at free convection (K) | Computed on node from HRRR | Component of the PFT solution | Hourly |
| PFT mixed-layer wind (`pft_uml`) | Mean 0→z_fc wind speed used in the firepower formula (m/s) | Computed on node from HRRR | Component of the PFT solution | Hourly |

---

## Fire perimeters (`GET /api/fires`)

| Product | What it shows | Data source | Detail (variable / method / attribution) | Cadence |
|---|---|---|---|---|
| Active fire perimeters | Live active-fire perimeter polygons (largest outer ring, decimated) with incident name, GIS acreage, point-of-origin state, and last-updated time | WFIGS current interagency perimeters (public, key-free ArcGIS FeatureServer) | Server-side filter: fires > 300 acres, national (no state filter), ordered by size, capped at 60, delivered as GeoJSON (WGS84). We keep only the single largest outer ring per fire and decimate it toward ~240 points. Fields: name, acres, point-of-origin state, updated time, ring | Served from a 10-minute cache; last-good body served stale on upstream error. Underlying WFIGS feed updates continuously *(external characterization)* |

---

## Point products (meteogram, daily cards, cross-sections, soundings)

Point tools sample the model store at a single grid cell. They pull nearest-community labels from the **US Census Gazetteer** and, on meteograms, draw "what's normal here today" bands from **our RTMA climatology archive**.

| Product | What it shows | Data source | Detail (variable / method / attribution) | Cadence |
|---|---|---|---|---|
| Point Meteogram | Multi-panel time series (T/Td, RH, VPD/HDW, wind + gust, precip, ERC / 10 h fuel, smoke) at one point across a run | Model store (HRRR / GFS / NBM), nearest cell | Samples stored fields (temperature_2m, dewpoint_2m, rh_2m, u/v_10m, wind_gust_10m, erc, dead_fuel_moisture_10h, kbdi, smoke_8m, apcp run-total) at the nearest cell. Default model HRRR | Per run; native hour spacing (1 h HRRR, 6 h GFS/NBM) |
| Meteogram derived series (VPD / HDW / hourly precip) | VPD, dual-axis surface HDW (sustained + gust), per-step precip | **Derived** from the sampled model fields | VPD = (eₛ(T) − eₛ(Td)) in kPa; HDW = VPD × wind (two series, sustained and gust); precip = run-total differences ÷ 25.4 | Per sampled hour |
| Meteogram climatology bands | Dashed "normal here today" reference lines, keyed by day of year | RTMA archive (analysis archive) | Per-variable percentiles from the seasonal archive: VPD p50/p90/p99, gust p90/p99, min-RH p50/p10; alignment gated on the grid hash | Static seasonal climatology |
| Daily Outlook Card | Shareable card: one column per local day, HI/LO strips, wind-barb row, precip row, for any stored 2-D variable | Model store (HRRR / GFS / NBM), nearest cell | Samples the requested variable + 10 m winds + precip run-totals. Default model HRRR | Per run; buckets to local days |
| Vertical Cross Section | Vertical slice along an A→B line: temperature / RH / wind fill + barbs, terrain silhouette, log-p axis | Model store 3-D isobaric volumes (HRRR) | Native `temperature_iso`, `u_iso`, `v_iso` fill; RH fill is **derived** via Magnus (100·eₛ(Td)/eₛ(T)); terrain from surface pressure — a mixed native + derived product | Per run; volumes present only for hours ingested since 2026-07-03 |
| Point Sounding (skew-T) | Full skew-T with barbs, hodograph, ECAPE block, locator map, parameter table | Model store 3-D isobaric volumes + surface fields (HRRR) | Native `temperature_iso`, `dewpoint_iso`, `u_iso`, `v_iso`, `height_iso` at the nearest cell, with a 2 m/10 m surface floor prepended | Per run; requires volume-carrying hours |
| Sounding indices (CAPE / ECAPE / shear / lapse / PWAT) | SBCAPE/CIN, ML/MU CAPE, ECAPE/NCAPE, shear, SRH, lapse rates, PWAT, DCAPE, freezing/wet-bulb-zero levels | **Derived** from the assembled model profile | Computed via the vendored sharprs machinery; single-column ECAPE/NCAPE via ecape-rs (entrainment-adjusted, flagged experimental) | Per rendered hour |
| Nearest-community labels | The nearest-town readout on every point product (e.g. "12 mi NE of Ukiah, CA") | US Census Gazetteer (public domain) | 2025 places file (~32,000 incorporated + census-designated places, 50 states + DC), baked in; nearest place by distance/bearing | Static asset; annual Census release |

---

## Attributions

- **HRRR, GFS** — NOAA / National Centers for Environmental Prediction (NCEP). Operational model output, public domain, obtained via NOMADS and the NOAA Open Data Dissemination program (AWS / Google Cloud / Azure).
- **NBM (National Blend of Models)** — NOAA / National Weather Service, produced at NCEP. Public domain; obtained via NOMADS and NOAA Open Data (AWS).
- **gridMET** — Climatology Lab, University of Idaho (Northwest Knowledge Network). Suggested citation: Abatzoglou, J.T. (2013), "Development of gridded surface meteorological data for ecological applications and modelling," *International Journal of Climatology*, 33: 121–131 doi:10.1002/joc.3413. Data: `https://www.northwestknowledge.net/metdata/data`.
- **LANDFIRE** — U.S. Geological Survey and U.S. Forest Service, LANDFIRE program (`https://landfire.gov`). Suggested credit: "LANDFIRE, U.S. Department of the Interior, U.S. Geological Survey and U.S. Department of Agriculture, U.S. Forest Service," noting the specific LANDFIRE release version used at import.
- **RTMA** (the source analyses behind our RTMA climatology archive, 2019–2026) — NOAA / NCEP Real-Time Mesoscale Analysis (De Pondeca et al. 2011, *Wea. Forecasting* 26, 593–612, doi:10.1175/WAF-D-10-05037.1). Public domain. Derived climatology plots are labeled "RTMA 2.5 km analysis climatology \| 2019–2026 archive."
- **WFIGS interagency perimeters** — Wildland Fire Interagency Geospatial Services (WFIGS), an interagency effort of the National Interagency Fire Center and partners. Public, key-free ArcGIS service. Suggested credit: "Fire perimeters: WFIGS Current Interagency Perimeters."
- **US Census Gazetteer** — U.S. Census Bureau, 2025 Gazetteer Files (places, national). Public domain.

---

*This document describes data provenance only. It is not a service-level agreement; cadences describe typical behavior and can be affected by upstream availability.*
