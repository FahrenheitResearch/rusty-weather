# RTMA Climatology Pack + Anomaly Serving Plan

Date: 2026-07-01. Status: pack v1 generating on node 2.

## Pack (produced by tools/rw-climo-pack on node 2)

Source: FireWxAtlas RTMA `seasonal_15day` climatology (±7-day DOY window,
2019–2026, Feb 29 omitted, formula manifest `v2026_05_24`), CONUS 2.5 km
zarr, 525 GB. The pack crops to the CAFire West (lon −126.2..−103.3, lat
31.4..47.0 → grid rows 216..1034 × cols 114..1012 = 818×898, 22.3% of
cells) and quantizes to i16.

- `pack_manifest.json` — schema `cafire.rtma_climo_pack.v1`: crop indices,
  per-slice `scale/offset/min/max/nan_count`, source provenance.
- `<window>/<product>/<stat>.i16bin` — `[365, 818, 898]` i16 LE, DOY-major.
  NaN sentinel −32768. Counts (`sample_count`) stored exactly (scale 1).
- `latitude_deg.f32bin` / `longitude_deg.f32bin` — cropped coordinate grids
  so the importer regrids without touching the atlas.
- Product-windows v1 (10): `utc_00_23` × {min_rh_2m_pct, max_vpd_2m_kpa,
  max_wind_10m_ms, max_gust_10m_ms, max_surface_hdw_wind,
  max_surface_hdw_gust, hours_joint_rh15_gust25, hours_joint_rh20_gust25,
  hours_joint_rh20_wind20} plus `utc_12_06_next_day` × min_rh_2m_pct
  (overnight recovery window).
- Stats (9): p05 p10 p25 p50 p75 p90 p95 p99 + sample_count.
- Verification: tool self-verifies random slices against the source zarr
  (bound: half-quantum + f32 rounding); independently cross-checked with a
  numpy decoder (max err = scale/2 exactly, NaN masks identical).
- Size: ~48 GB (fits Hetzner alongside minimal model history).

## Serving side (all Rust, .rws-native)

1. `rw_climo_import` (next): pack → `.rws` climatology store using the
   existing rw-store writer + `rustwx-regrid` (same lane fuel imports use):
   regrid each slice bilinearly onto the HRRR CONUS grid cropped to the
   pack bounds, write `store/rtma_climo/<pack_version>/f{doy:03}.rws`
   (forecast_hour ⇒ DOY, 1..365) with variables named
   `<window>__<product>__<stat>`. Grid defined by a cropped `grid.rwg`
   (HRRR-subgrid). ~33 GB served.
2. Anomaly product lane in rusty-weather: for a HRRR forecast hour, compute
   the daily-window forecast field (e.g. max VPD over the local day),
   look up the valid-date DOY slice, and render:
   - `*_percentile_rank` — piecewise-linear CDF rank between stored
     percentile anchors (clamped <p05 / >p99 bands labeled explicitly);
   - `*_vs_p95` exceedance and "top-N% for this date" categorical maps;
   - overnight RH recovery quality vs the `utc_12_06_next_day` store.
3. Naming discipline (per node-2 RTMA product contract): climatology
   context products say `surface_hdw_*`; never label the RTMA proxy as
   true HDWI. Titles carry "vs 2019–2026 ±7-day climatology (n≈105)".
4. Percentile palettes: diverging around p50 with hard emphasis bands at
   p90/p95/p99 (the IMET signal); sample_count drives a low-confidence
   hatch/mask where n < 60.

## Decisions recorded 2026-07-01

- All new components Rust (ETL, importer, serving). Python only for
  independent verification.
- ECAPE stays off the operational lane (nodes 3/4 later). No HREF. RRFS
  deferred. Hetzner keeps minimal model history (e.g. one 48 h + one 18 h
  HRRR run) to hold the climo store comfortably. No Synoptic/MesoWest;
  FEMS keyless CSV endpoints only.
