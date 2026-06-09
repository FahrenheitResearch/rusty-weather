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

Next: unified store (rw-store), then the serve daemon, then the web UI — see
docs/superpowers/specs/2026-06-09-rusty-weather-design.md.

## Layout

- `crates/` — ported rustwx crates (names kept for diffability) + the `rusty-weather` bin crate
- `vendor/` — vendored deps (sharprs, metrust, grib-core, wx-*, ecape-rs)
- `assets/basemap/` — Natural Earth + US county shapefiles
