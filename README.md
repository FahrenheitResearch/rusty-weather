# rusty-weather

A self-contained weather model viewer: fetch HRRR (more models coming),
store hours in a fast-access format, and view map plots, instant soundings,
and live GOES satellite loops in a native egui app. Full Rust, MIT licensed.
Extracted and rebuilt from the rustwx fast path.

Design: docs/superpowers/specs/2026-06-09-rusty-weather-design.md

## Using crates from this repo as dependencies

The embeddable pieces (`rw-ui` panels, `rw-store`, `rw-ingest`, `rw-sat`) work
as git dependencies:

```toml
[dependencies]
rw-ui = { git = "https://github.com/FahrenheitResearch/rusty-weather" }
rw-sat = { git = "https://github.com/FahrenheitResearch/rusty-weather" }
```

**FOOTGUN — `hdf5-reader` patch required by rw-sat consumers.** `rw-sat`'s
NetCDF stack (`netcrust`) depends on `hdf5-reader = "0.3"` from crates.io,
which this repo patches to a vendored (and bug-fixed) copy. **`[patch]`
sections do not propagate to dependent workspaces**, so any project depending
on `rw-sat` must add to its OWN workspace `Cargo.toml`:

```toml
[patch.crates-io]
hdf5-reader = { git = "https://github.com/FahrenheitResearch/rusty-weather" }
```

Without it you get the crates.io `hdf5-reader`, which (among other things)
fails GOES-19 CMIP files with a checksum mismatch on the `x` variable.

## rw-store

Format spec: docs/FORMAT.md

Each forecast hour is a self-contained `.rws` file: 256×256 spatial tiles of 2D surface fields, zstd-1 compressed f32, with true windowed reads so a regional plot decodes only the intersecting tile set. Pressure-level volumes are stored as 16×16-column 3D chunks (all levels contiguous per column), affine-i16 quantized then zstd-1, so a point sounding mmaps the file, binary-searches the index, and decodes 1–4 small chunks for instant bilinear profiles across all levels. Per-run provenance lives in `grid.rwg` (lat/lon arrays + projection, sha256-hashed for grid-identity checks) and `run.json` (model, cycle, hours present, schema id `rw-store.run.v1`, build-hash from `git rev-parse` compiled in at build time).

    cargo run --release -p rusty-weather --bin rw_ingest -- --model hrrr --date YYYYMMDD --cycle 0 --hours 0-6 --store-root store --verify
    cargo run --release -p rusty-weather --bin rw_bench -- --run YYYYMMDD_00z

Measured (HRRR 20260608_00z, 11 vars / 37 levels, 409 MB/hour): ingest ~6 s/hour warm-cache (2.6 s extract + 1.6 s encode); sounding warm 0.19 ms; full 2D read ~3.6 ms.

The hour file lands above the spec's ~250 MB ballpark because five full 37-level
volumes (45-102 MB each after i16 quantization + zstd-1) buy the instant
soundings; trimming stored levels or variables is the lever if disk ever matters.
locate() measured 75.8 us against an informational 50 us hope — moot in practice,
the entire warm sounding is 0.19 ms.

### Inspecting and exporting stores (`rws`)

`rws` fronts the whole format: list, inspect, validate, diff, and export to NetCDF3.

    rws ls store/hrrr                            # walk run.json manifests: model/run/hours/vars/sizes
    rws dump store/hrrr/20260611_04z/f000.rws --var temperature_iso   # header/meta; per-var index records
    rws validate store/hrrr/20260611_04z --deep  # conformance gate (decompresses every chunk)
    rws diff a.rws b.rws                          # structural compare (writer.build masked); exit 0 = same
    rws export store/hrrr/20260611_04z/f000.rws -o f000_subset.nc --vars temperature_2m,dewpoint_2m,temperature_iso

`validate --deep` is the conformance gate: it parses every header/meta/index, decompresses each chunk, and cross-checks raw lengths, stats, and flags — a clean run means the store matches `docs/FORMAT.md`. `export` writes a dependency-free NetCDF3 (CDF-2) file any scientist can open; 2D fields round-trip bit-exact, and 3D pressure values carry an `rw_quantization` attribute disclosing the lossy affine-i16 step applied at ingest. The output reads natively in xarray via the SciPy backend:

    import xarray as xr
    ds = xr.open_dataset("f000_subset.nc")   # scipy backend reads NetCDF3 natively
    print(ds["temperature_2m"].sel(y=500, x=900).values)

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

## Plan 3 — every product from the store

One `rw_batch` invocation fetches, ingests, computes, and renders the full HRRR
product suite for a range of forecast hours in a pipelined std::thread pipeline:

    cargo run --release -p rusty-weather --bin rw_batch -- \
        --model hrrr --date 20260608 --cycle 0 --hours 4-6 \
        --no-heavy --products all \
        --store-root store --cache-dir out/cache --out-dir out/rw_batch

Add `--heavy` to include the 16 ECAPE-class products (CAPE triplet, effective STP,
VTP, etc.); omit it for the ~81-product/hour fast path. Add `--full-throttle` for
dedicated nodes (default is polite: below-normal priority, `cores-2` rayon pool).

### Product count

| Lane          | Count | Notes                                                          |
|---------------|-------|----------------------------------------------------------------|
| Direct        |    52 | GRIB fields → render, no computation                          |
| Derived       |    29 | Precomputed at ingest (CAPE/CIN triplets, shear, SRH, …)       |
| Heavy         |    16 | ECAPE-class; gated by `--heavy`; CPU-saturating                |
| Windowed      |    49 | Cross-hour accumulations (QPF, UH max, wind max, temp range, …)|
| **Total**     | **146**| (5 windowed realized in a 3-hour store; 44 blocked structurally)|

### Pixel-parity result

95/97 direct+derived+heavy products are **byte-identical** between the GRIB
render lane (smoke_direct/smoke_derived) and the store path (rw_render):

- 52/52 direct — byte-identical after fixing a store codec bug (the f32
  "lossless" tile shortcut for near-constant planes now uses exact-constancy only,
  not an absolute epsilon; regression-pinned).
- 43/45 derived — byte-identical including all 16 heavy. The two diffs are
  `temperature_advection_700mb` and `temperature_advection_850mb` (max channel
  delta 9): `estimate_grid_spacing_m` averages over the compute domain, so the
  GRIB lane (crops before compute) uses midwest spacing while the store grid was
  computed full-CONUS — same behavior as the existing `hrrr_non_ecape_hour` lane.

### Benchmark (20260608 00z f004–f006, midwest, warm cache, polite 30-thread pool)

**Run 1 — no heavy, `--products all` (the primary number):**

| Hour | fetch | extract | thermo | derived | heavy | encode | render |
|------|------:|--------:|-------:|--------:|------:|-------:|-------:|
| f004 | 1500  | 3180    | 3723   | 7179    | 0     | 2335   | 12538  |
| f005 | 1724  | 3818    | 3295   | 7084    | 0     | 2358   | 12292  |
| f006 | 2208  | 3782    | 2984   | 7302    | 0     | 2138   | 8459   |

Totals (ms): fetch 5432 | extract 10780 | thermo 10002 | derived 21565 |
heavy 0 | encode 6831 | render 33289 | windowed 368  
**TOTAL WALL: 59.8 s | process CPU 801.9 s | 248 products rendered**  
Gate ≤ 90 s warm → **PASSED at 59.8 s**. Old node baseline: ~75 s for ~80
products × 3 hours; rw_batch renders 81 × 3 + windowed in 59.8 s on the polite
pool. Serial stage sum 88.3 s → pipelining recovered 28.5 s (32%).

**Run 2 — with heavy (`--heavy`):**

| Hour | fetch | extract | thermo | derived |  heavy | encode | render |
|------|------:|--------:|-------:|--------:|-------:|-------:|-------:|
| f004 | 1530  | 2616    | 2974   | 6984    | 84160  | 2479   | 14430  |
| f005 | 1601  | 3817    | 3159   | 7335    | 83826  | 2148   | 14147  |
| f006 | 1748  | 3753    | 3256   | 7189    | 82701  | 2253   | 8587   |

Totals (ms): fetch 4879 | extract 10186 | thermo 9389 | derived 21508 |
heavy 250687 | encode 6880 | render 37164 | windowed 374  
**TOTAL WALL: 309.4 s | process CPU 7530.4 s | 296 products rendered**  
ECAPE-dominated: 250.7 s of the 309.4 s wall (~82 s/hour) is the ECAPE triplet
(`calc_ecape_parcel` runs two full ascents per parcel type — 6 ascents/column,
~9 billion integration steps per CONUS hour). Pipelining cannot hide CPU-saturating
work; the cost is the vendored `ecape-rs` physics kernel.

**Next:** egui/eframe UI integration (`rw-ui` library-first crate) + multi-model
validation (GFS, RRFS-A, REFS, NBM, RAP) — see
docs/superpowers/specs/2026-06-09-rusty-weather-design.md.

## Layout

- `crates/` — ported rustwx crates (names kept for diffability) + the `rusty-weather` bin crate
- `vendor/` — vendored deps (sharprs, metrust, grib-core, wx-*, ecape-rs)
- `assets/basemap/` — Natural Earth + US county shapefiles
