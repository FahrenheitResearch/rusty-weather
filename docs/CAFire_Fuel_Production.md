# Rusty Fire Weather CAFire Production Notes

This branch keeps `.rws` / `rw-store` as the canonical data format and renders
CAFire maps with the RustWX-style plotting stack.

## Binaries

- `rw_batch`: ingest model hours into `.rws` stores.
- `rw_fuel_fetch`: download/cache public fuel datasets, process daily layers,
  and import them into existing `.rws` hours.
- `rw_fuel_import`: import/regrid fuel layers into existing `.rws` hours.
- `rw_cafire`: one-command CAFire ingest -> fuel fetch/import -> domain render.
- `rw_render`: render one stored hour/domain/product set.
- `rw_fire_api`: local/web draw-a-box render API.

## One-Command CAFire Run

```powershell
target\release\rw_cafire.exe `
  --date 20260629 --cycle 3 --hours 0-48 `
  --products cafire-with-fuels `
  --fuel-provider gridmet `
  --fuel-date 2026-06-29 `
  --fuel-cache-dir C:\rw\cache\fuel `
  --fuel-method bilinear `
  --store-root C:\rw\store `
  --out-dir C:\rw\cafire_out
```

`rw_fuel_fetch` currently supports gridMET daily NetCDF inputs for KBDI,
ERC, Burning Index, 1h/10h/100h/1000h dead fuel moisture, and daily precip
fuel context. It writes same-grid `.rws` variables using the native fuel
product slugs, so `rw_render --products cafire-with-fuels` can render fuel
products without another data path.

Manual fuel layers are still supported for LANDFIRE or other provider files:

```powershell
target\release\rw_fuel_import.exe `
  --store-root C:\rw\store `
  --model hrrr --run 20260629_03z --hours 0-48 `
  --layer landfire_fuel_model=C:\fuel\landfire_model.nc:fuel_model `
  --layer landfire_fuel_loading=C:\fuel\landfire_loading.nc:fuel_loading `
  --lat-var lat --lon-var lon `
  --method nearest --overwrite
```

Fuel-aware render products skip with clear messages if the needed fuel grids
are absent. Today that means LANDFIRE products render once the static LANDFIRE
layers are supplied or a LANDFIRE downloader is added.

## Fuel Fetch Only

```powershell
target\release\rw_fuel_fetch.exe `
  --store-root C:\rw\store `
  --model hrrr --run 20260629_03z --hours 0-48 `
  --date 2026-06-29 `
  --cache-dir C:\rw\cache\fuel `
  --kbdi-spinup-days 180 `
  --kbdi-annual-rain-in 20 `
  --method bilinear
```

The fetch manifest records every downloaded file, cache hit, source variable,
daily slice index, regrid timing, per-layer stats, and rewritten `.rws` hours.

## Draw-A-Box API

```powershell
target\release\rw_fire_api.exe `
  --host 127.0.0.1 --port 8788 `
  --store-root C:\rw\store `
  --out-root C:\rw\api_jobs `
  --rw-render target\release\rw_render.exe `
  --max-render-jobs 2
```

`--max-render-jobs` bounds simultaneous `rw_render` child processes. Extra
requests queue in memory and `/api/health` reports `{active, waiting,
max_active}`.

For Hetzner, run this behind nginx/Caddy with TLS and keep `--max-render-jobs`
sized to the server. Start at `2` for a small node, then tune from load-test
results.

## Load Test

```powershell
python scripts\fire_api_load_test.py `
  --api http://127.0.0.1:8788 `
  --scenario preview-core `
  --requests 40 `
  --concurrency 10 `
  --out-dir C:\rw\load_tests
```

The harness writes CSV samples and a JSON summary with client latency,
API wall time, renderer wall time, throughput, byte totals, and failures.
