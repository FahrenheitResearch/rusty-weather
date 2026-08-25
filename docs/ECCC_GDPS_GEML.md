# ECCC GDPS-GEML acquisition contract

`gdps-geml` is ECCC's experimental global AI-emulator feed. Rusty Weather
keeps that status visible; it is not presented as operational GDPS.

## Provider and transport

- Product documentation: <https://eccc-msc.github.io/open-data/msc-data/nwp_gdps/readme_gdps-geml-datamart_en/>
- Live Datamart root: <https://dd.weather.gc.ca/today/model_gdps-geml/25km/>
- Licence: [ECCC Data Servers End-use Licence 2.1](https://eccc-msc.github.io/open-data/licence/readme_en/)
- Required notice: `Data Source: Environment and Climate Change Canada`

The live object URL is:

```text
https://dd.weather.gc.ca/today/model_gdps-geml/25km/{HH}/{hhh}/{YYYYMMDD}T{HH}Z_MSC_GDPS-GEML_{component}_LatLon0.25_PT{hhh}H.grib2
```

The live path is authoritative: it does not contain the extra `grib2/lat_lon`
segments shown in one documentation layout example. Datamart publishes no
companion byte index for this feed. Every object is one simple-packed GRIB2
message, so acquisition is bounded by an exact component allowlist and the
component cache rather than arbitrary path input or whole-directory download.

## Cadence and inventory

- Cycles: 00z and 12z.
- Leads: f000 through f240 at six-hour steps (41 leads).
- Grid: GRIB template 3.0 regular latitude/longitude, 1440x721, 0.25 degrees,
  first point 90N 0E, last point 90S 359.75E, scan mode 0.
- Packing: GRIB data-representation template 5.0 (simple packing).
- Surface components: `AirTemp_AGL-2m`, `WindU_AGL-10m`,
  `WindV_AGL-10m`, and `Pressure_MSL`.
- Pressure levels: 50, 100, 150, 200, 250, 300, 400, 500, 600, 700, 850,
  925, and 1000 hPa.
- Pressure families: `AirTemp`, `SpecificHumidity`, `WindU`, `WindV`,
  `Geopotential`, and `VerticalVelocity`.

The complete native profile is therefore exactly 4 + 6x13 = 82 objects per
forecast lead. The scheduler uses the sounding-shaped profile so all 82 roles
are acquired without requesting unavailable render or derived inputs.

## Normalization and limitations

- Temperature, U, V, and MSLP map directly to canonical fields.
- Specific humidity is converted to canonical dewpoint. Values outside
  `[0,1)` kg/kg become NaN; negative emulator output is not silently clamped.
- WMO geopotential (m2/s2) is divided by standard gravity and stored as
  canonical geopotential height (`height_iso`).
- Pressure vertical velocity (WMO 0/2/8) is preserved as
  `vertical_velocity_iso` in Pa/s.
- No precipitation, surface pressure, orography, 2 m humidity/dewpoint,
  clouds, visibility, or gust objects are published in the observed contract.
  Derived and heavy diagnostics therefore fail closed.
- The legacy direct-GRIB plot path remains blocked because it cannot assemble
  per-field component bundles. Normalized RWS query is the supported path.

Scheduler retention uses the same configured per-model age/run/disk policy as
other ready remote models; there is no hidden GEML-specific archive promise.
Datamart's `today` transport is short-lived, so discovery and ingest must not
be treated as historical reprocessing coverage.

## Pinned evidence

The fixture
`crates/rw-ingest/tests/fixtures/gdps-geml.20260814.t00z.f006.inventory.txt`
pins the official f006 listing, all 82 exact filenames, representative object
hashes, independent decoded-value hashes, grid metadata, and the emulator's
out-of-range specific-humidity behavior.

The same 2026-08-14 00z f006 lead passed a live `--profile sounding --verify`
ingest with four bit-exact 2-D fields and six 13-level volumes (temperature,
dewpoint synthesized from specific humidity, U, V, height, and omega). Deep
RWS validation then passed with no warnings at 10 variables, 24,912 chunks,
and 166,368,311 compressed payload bytes.
