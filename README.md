# rusty-weather

A self-contained weather model viewer: fetch HRRR / GFS / RRFS-A / REFS / NBM / RAP,
store hours in a fast-access format, and serve map plots + instant soundings on a
local webpage. Full Rust. Extracted from the rustwx fast path.

Design: docs/superpowers/specs/2026-06-09-rusty-weather-design.md

## Status

Extraction complete (Plan 1). The workspace builds and renders live HRRR plots:

    cargo run --release -p rusty-weather --bin smoke_direct -- --model hrrr --date YYYYMMDD --cycle 0 --forecast-hour 6 --region midwest --all-supported --out-dir out

Measured baseline (HRRR f006, midwest, cold cache, 2026-06-09): 72s wall —
~59s NOMADS fetch (1.35 GB across nat/prs/sfc), 6s field extraction,
~0.5s/plot render across 52 products. Warm cache: 4 derived plots in 16s.

### Plan 2 (rw-store) measured baselines

Read-path benchmark (HRRR 20260608_00z f006: 1799x1059 grid, 6 surface 2D vars
+ 5 pressure volumes x 37 levels, 409.4 MB hour file; median of 5 samples after
1 warmup, 2026-06-09):

    cargo run --release -p rusty-weather --bin rw_bench -- --run 20260608_00z

    metric                                              median           min           max
    open (HourReader)                                  0.75 ms       0.68 ms       0.82 ms
    grid_open (GridFile)                              19.99 ms      19.75 ms      21.80 ms
    locator_build (cold)                               1.95 ms       1.74 ms       2.05 ms
    locate_warm (per call, 1000x/iter)                 75.8 us       75.6 us       76.7 us
    read_full_2d temperature_2m                        3.59 ms       3.51 ms       4.14 ms
    read_full_2d dewpoint_2m                           3.64 ms       3.50 ms       4.16 ms
    read_full_2d u_10m                                 3.52 ms       3.40 ms       4.00 ms
    read_full_2d v_10m                                 3.61 ms       3.40 ms       3.65 ms
    read_full_2d composite_reflectivity                3.40 ms       3.16 ms       3.76 ms
    read_full_2d mslp                                  3.60 ms       3.41 ms       3.95 ms
    window_quarter temperature_2m (899x529)            1.42 ms       1.35 ms       1.43 ms
    window_64 temperature_2m (64x64 mid-grid)          0.57 ms       0.57 ms       0.68 ms
    sounding_cold (open+build+locate+5 profiles)      23.21 ms      22.77 ms      24.05 ms
    sounding_warm (locate+5 profiles)                  0.19 ms       0.18 ms       0.22 ms

Gates: sounding_warm 0.19 ms vs 100 ms hard / 25 ms expected — PASS;
read_full_2d worst 3.64 ms vs 150 ms per var — PASS; window_quarter 1.42 ms vs
0.35 x full + 0.5 ms overhead = 1.76 ms — PASS; locator_build 1.95 ms vs 50 ms
informational target. Even the worst-case first click (sounding_cold: grid
open + locator build + hour open + 5 profiles, dominated by the grid file's
sha256 + coordinate decompress) is 23 ms.

Next: unified store (rw-store), then the serve daemon, then the web UI — see
docs/superpowers/specs/2026-06-09-rusty-weather-design.md.

## Layout

- `crates/` — ported rustwx crates (names kept for diffability) + the `rusty-weather` bin crate
- `vendor/` — vendored deps (sharprs, metrust, grib-core, wx-*, ecape-rs)
- `assets/basemap/` — Natural Earth + US county shapefiles
