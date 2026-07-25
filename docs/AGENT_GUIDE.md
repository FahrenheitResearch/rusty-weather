# Rusty Fire Weather — Agent Guide

**The single entry point for ANY agent (Claude, Codex, or other) working on
this project.** Read this before touching anything. Last full update:
**2026-07-06**. If you change how the project works, update this doc (and
`HETZNER_OPS.md` for server-process changes) in the same session — this file
is the crash-durable copy of the project's working knowledge. Copies live in
the repo (`docs/AGENT_GUIDE.md`, pushed to GitHub), on the server
(`/opt/rusty-weather/AGENT_GUIDE.md`), and in Drew's Downloads folder.

---

## 1. What this is

**rusty-fire-weather** is a Rust-first fire-weather processing and plotting
service. It ingests HRRR / GFS / NBM model runs plus gridMET fuels and an
RTMA climatology archive into a custom `.rws` store, derives fire-weather
products, and serves maps, meteograms, daily outlook cards, cross sections,
and soundings through a single HTTP API. It is the live backend for
**CAFire.org's weather page** (California Wildfire Tracking, "CWT").

- Public host: `https://cafire.wxsection.com` behind Cloudflare.
  **Never show `cafire.wxsection.com` to end users or put it on products** —
  user-facing branding is always **cafire.org/weather**.
- "The Lab" = our own product-browser UI at `/lab` (see §5 — it's
  `cafire_preview.html`, compiled into the API binary).
- CAFire's own legacy docker stack still runs on the same box and still
  serves their satellite + lightning pages (§4). Its HRRR/model role was
  retired at the 2026-07-03 cutover.
- One Hetzner server does everything. Four home Ubuntu nodes exist for
  compute/archive side work (§4.4).

## 2. Read next — which doc for what

| Doc (in `docs/`) | What it is |
|---|---|
| **HETZNER_OPS.md** | THE server runbook: health check, systemd lanes, disk economics, deploy pattern, incident playbooks, legacy-stack keep/delete lists, Jul-17 cleanup. Deepest operational truth; a copy lives at `/opt/rusty-weather/HETZNER_OPS.md` on the box. |
| FABLE_FIRE_WEATHER_HANDOFF.md | Deep project background and goals (why `.rws`, why not the old RustWX monolith). |
| CAFIRE_HANDOFF.md + ADDENDUM_1/2 | **External promises to CAFire — treat as contracts.** API reference, integration recipes, switchover playbook, client tuning advice. |
| CAFIRE_DATA_SOURCES.md | Per-product data provenance. **INTERNAL — contains our sources/infra.** |
| CAFIRE_PRODUCT_GUIDE.md | What each plot is + how computed + citations. **Shareable — must NEVER contain data sources or infra.** |
| PFT_SPEC.md | Tory & Kepert 2021 PyroCb Firepower Threshold — full equation set + validation anchors. |
| FORMAT.md / STREAMING.md / DETERMINISM.md | `.rws` store format internals. |
| FABLE_ARCHITECTURE_REVIEW.md, FABLE_BOOT_PROMPT.md, HETZNER_READINESS.md | Historical context; superseded by this guide for orientation. |

## 3. Hard rules (violating these has burned us — every one is load-bearing)

1. **Do NOT touch the Hetzner server without Drew's explicit approval**,
   beyond read-only inspection and the incident-runbook fixes in
   HETZNER_OPS §8. Drew sometimes grants broad discretion for a session;
   that grant does not persist to the next session.
2. **`.rws` / rw-store is the canonical backend. Never WxStore.**
3. **Never fake fuel data.** Fuel products use real gridMET/LANDFIRE grids
   or they don't render.
4. **Never lower plot quality for speed** without explicit approval.
5. **Never commit generated `outputs/`.**
6. **Push after committing.** The whole project sat 96 commits deep with no
   remote until 2026-07-06. Branch: `codex/rusty-fire-weather-cafire-ui` →
   `origin` (github.com/FahrenheitResearch/rusty-weather).
7. **Card SVGs use plain `<text>` only — no `<tspan>`, no `xml:space`.**
   Structural novelty broke Chrome's right-click "Copy Image" on
   2026-07-04. Keep every text element flat.
8. **Deploy via `git archive`, never by scp'ing working files** (Windows
   working tree scp'd once = CRLF drift on the server tree).
9. CAFIRE_PRODUCT_GUIDE stays source-free; CAFIRE_DATA_SOURCES stays
   internal.
10. Verification culture: **tests green + real proof renders you actually
    LOOK at + public-URL check after every deploy** (§7).

## 4. The map — machines & services

### 4.1 Hetzner box
`root@178.104.59.253` — 16-core AMD EPYC-Genoa, 30 GB RAM, ~230 GB disk.
Layout `/opt/rusty-weather/{bin,store,cache,out,src,logs}`.

systemd units (all should be `active`):

| Unit | What |
|---|---|
| `rusty-wx-api` | `rw_fire_api` on `0.0.0.0:8788` — the whole API + Lab |
| `rusty-wx-pipeline` | HRRR refresh daemon (ingest + fuels + prune + prewarm) |
| `rusty-wx-pipeline-gfs` | GFS lane (6-hourly, F384) |
| `rusty-wx-pipeline-nbm` | NBM lane (F264) |

Retention crons live in **`/etc/cron.hourly/`** (root's crontab is EMPTY —
that's normal): `cafire-artifact-retention` (36 h sat/lightning artifacts),
`cafire-cache-cap` (36 h `data/cache/{satellite,hrrr}` — **must survive the
Jul-17 cleanup**, it also caps satellite), `rw-rawfetch-cache-cap` (6 h raw
GRIBs; the pipeline's internal rw_prune does NOT honor cache age — this cron
is the backstop).

### 4.2 CAFire legacy docker stack
`/opt/cafire-weather-service` (docker compose). **Running:** `caddy`
(public edge), `api` (:8000, loopback + compose network — still serves
CAFire's satellite/lightning pages via the Caddy fallback),
`satellite-worker`, `lightning-worker`. **Retired** (stopped, restart=no):
`static-worker`, `pressure-volume`, `pressure-volume-builder`, `warmer`,
`wxstore`. Satellite + lightning stay on this stack permanently.

- `FAST_METEOGRAM_STORE_ENABLED=false` in `.env` since 2026-07-06 (its
  warm thread refetched full F0–48 HRRR ~4×/day for meteograms nothing
  calls; backup `.env.bak-20260706-faststore`).
- The api's own `/health` says `ok:false` — pre-existing; it live-probes
  the retired wxstore container. Jul-17 tidy: `WXSTORE_ENABLED=false`.

### 4.3 Caddy routing (`/opt/cafire-weather-service/Caddyfile`)
- `handle_path /lab*` → `172.18.0.1:8788` — prefix is **stripped**, so
  public `/lab` = rw_fire_api `/` (the Lab page).
- `@lab_api` `/api/*` → `:8788` (wildcard matcher — new `/api/...`
  endpoints need **no** Caddyfile change).
- `/outputs/*` → `:8788`; `handle_path /node/*` → `:8790` (node-1 tunnel,
  paused); fallback `handle` → `api:8000` (legacy stack).

### 4.4 Home nodes (see also HETZNER_OPS)
4 Ubuntu boxes on Drew's LAN, mDNS `.local` names (IPs drift — use mDNS).
**node 1** = ECAPE/PFT heavy-compute node (24c/123 GB) — serving **paused
2026-07-03** (tmux sessions killed; restart recipe in HETZNER_OPS/memory;
public `/node/*` 502s and the static `/api/ecape` gallery is frozen at
20260703_04z — expected). **node 2** = RTMA climatology archive + repack
host (holds the 201 GB CONUS climo pack; wide-west is what's deployed).

## 5. The map — repo

`C:\Users\drew\rusty-fire-weather` on Drew's PC (Windows). Workspace crates:

- `crates/rusty-weather` — the product crate. Binaries: `rw_fire_api` (the
  API), `rw_pipeline` (refresh daemon), `rw_batch` (orchestrated ingest),
  `rw_render` (map renderer child process), `rw_prune`, `rw_fuel_fetch`,
  `rw_climo_import`, `rw_land_mask`.
  Key sources (modules of rw_fire_api via `#[path]`):
  - `src/bin/rw_fire_api.rs` — routes, run-alias resolution, job queue,
    RenderGate.
  - `src/meteogram.rs` — meteograms AND the daily outlook card
    (`render_daily_svg`, `night_key`, wind barbs).
  - `src/xsection.rs`, `src/sounding.rs` — vertical products.
  - `src/svg_raster.rs` — SVG→PNG (`?format=png`), resvg, 2×, max 4
    concurrent (static gate).
  - `src/cafire_preview.html` — **the Lab**, `include_str!`'d into the
    binary: UI changes deploy with the binary, no file copy.
  - `src/fire_site.html` (`/ops`), DEMO_HTML (`/legacy`).
- `crates/rustwx-*` — rendering/products/calc/regrid/io lineage
  (`rustwx-products` has colormaps, gazetteer, derived recipes, windowed
  products; `rustwx-render` is the map rasterizer).
- `crates/rw-ingest`, `rw-store`, `rw-sat` and `vendor/netcrust`,
  `vendor/sharprs` (skew-T engine, CWT-recolored).
- `.rws` dev store lives in a DIFFERENT dir: `C:\Users\drew\rusty-weather\store`.

## 6. How data flows

1. Pipelines probe AWS/NOMADS idx files for the newest cycle, `rw_batch`
   ingests GRIB → `.rws` per hour: `store/hrrr/20260706_12z/f012.rws`.
   HRRR: hourly runs to F18; extended 00/06/12/18z runs to F48 (the daemon
   finishes extended runs across ticks — freshest cycle first, then
   backfill). GFS: 6-hourly to F384. NBM: to F264.
2. `latest.json` per model carries **run pointers**, written atomically:
   - `run` — newest touched (may be mid-ingest)
   - `complete_run` — newest fully-ingested → alias **`latest`**
   - `day_run` — newest run covering ≥20 h of a UTC day (extended runs;
     NOT gated on full ingest) → alias **`latest-day`** — anomaly/day-window
     products resolve here
   - `fuel_run` — newest complete run with fuels imported → alias
     **`fuel-run`** — fuel products resolve here
3. The API renders on demand: maps via `rw_render` child (gated by
   `--max-render-jobs`), cards/meteograms/xsections as SVG in-process,
   soundings as PNG. `?format=png` rasterizes any SVG card server-side.
4. Special case: `/api/daily?run=latest` **falls back to `day_run`** when
   the newest complete hourly run can't fill any 24 h bucket ("no bucket
   has enough samples" — mid-local-day inits). Explicit runs still surface
   the error.
5. Daily **temperature** cards draw the LO as a true **overnight** min
   (evening→dawn window, `night_key()`), positioned BETWEEN the day
   columns. Other variables and step-bucket cards keep in-column HI/LO.
6. **Map rendering shells out to the `rw_render` binary** (rw_fire_api spawns
   it). `store_render.rs` compiles into `rw_render` — rebuild `rw_render`
   (not just `rw_fire_api`) for any render/store change, or the fix won't
   take effect.
7. **GFS/NBM surface weather maps** (Lab Weather + Radar families, 2026-07-06):
   the render path takes `model=`; HRRR is the default (omit `model` for
   cache parity). GFS carries the full surface set + MSLP isobars (coarse
   0.25°); NBM is a 2.5-km surface blend with no MSLP/clouds/reflectivity
   and no pressure-pair derived, so its temp/RH/dewpoint maps render with
   the isobar overlay dropped (overlays are optional in `store_render.rs`;
   the fill is required). Per-model product availability was probed against
   the store, not assumed — the Lab's `MODEL_PRODUCTS` table holds the
   verified sets. Reflectivity/IR and hourly-QPF windows are HRRR-only.

## 7. API quick reference

`GET /` Lab · `/ops` · `/legacy` · `GET /api/health` · `GET /api/runs[?model&var]`
· `GET /api/vars` · `POST /api/render` (async job → `/outputs/...` WebP)
· `GET /api/meteogram?lat&lon&run[&model][&vars=a,b,c][&format=json|png]`
· `GET /api/daily?lat&lon&var[&model][&run][&step=1|3|6][&format=png]`
· `GET /api/xsection?lat0&lon0&lat1&lon1&run[&hour][&field=temperature|rh|wind][&format=json|png]`
· `GET /api/sounding?lat&lon&run&hour` (native PNG) · `GET /api/fires` (WFIGS)
· `GET /api/ecape/...` (frozen static gallery while node 1 is paused).

Gotchas: surface RH variable is **`rh_2m`** (not relative_humidity_2m);
window-product slugs look like **`2m_temp_24_48h_max`** (full list in the
Lab's `FAMS`); run slugs look like `20260706_12z`.

## 8. Deploying (condensed — full detail in HETZNER_OPS §7)

```
# on Drew's PC, from the repo root — COMMIT FIRST, the server only ever
# receives git archive snapshots; then PUSH
git archive --format=tar HEAD | ssh root@178.104.59.253 "tar -x -C /opt/rusty-weather/src"

# on the server (cargo needs the env source in non-interactive ssh)
source $HOME/.cargo/env && cd /opt/rusty-weather/src
cargo build --release --bin rw_fire_api          # ~1–2 min warm
# swap: cp over a running binary = ETXTBSY; cp-to-temp + mv is atomic
cp target/release/rw_fire_api /opt/rusty-weather/bin/rw_fire_api.new
mv /opt/rusty-weather/bin/rw_fire_api.new /opt/rusty-weather/bin/rw_fire_api
systemctl restart rusty-wx-api
```

- Risky change? Stand up a **temp instance on :8799** against the live
  store first (`rw_fire_api --port 8799 --store-root /opt/rusty-weather/store`),
  A/B it, then swap.
- `cargo test` before deploy. Two **pre-existing known failures** in
  rw-ingest `size_estimate` are not yours.
- Runtime dep for `?format=png`: `apt install fonts-inter fonts-ibm-plex
  fonts-dejavu-core`.
- Verify after EVERY deploy: `/api/health`, a fresh render, the public
  URL, and **fetch a proof PNG and actually look at it** (pixel checks
  beat eyeballing for chrome geometry).

## 9. Gotcha ledger (each one cost real time)

**Local / git**
- PowerShell safety hook trips on `/`-paths in inline commit messages —
  use `git commit -F <file>` or commit from bash.
- `git add <dir>` does NOT add a sibling FILE of the same name — add files
  explicitly or `-A`.
- Local `rw_fire_api.exe` running = build fails on file lock
  (`Stop-Process` first).
- Rust raw strings containing `"#` (SVG fills) need `r##"..."##`.

**Server**
- Never a bare `docker compose up -d` in the CAFire dir — resurrects all
  five retired services. Name the service, and for `api` use
  `docker compose up -d --no-deps api` (its `depends_on: pressure-volume`
  is retired). `docker compose restart` does NOT reload `.env`.
- Caddyfile is bind-mounted: editing it in place gives the container a
  stale inode — `docker compose restart caddy` after changes.
- Long remote jobs go in **tmux**, always (ssh session-scope reaping kills
  nohup'd jobs silently).
- Stale `.rw-pipeline-lock-<model>` in the store makes a restarted daemon
  exit instantly — the systemd units' ExecStartPre remove it; manual runs
  must too.
- `crontab -l` being empty is normal; the crons are `/etc/cron.hourly/*`.

**Product**
- No `<tspan>` / `xml:space` in cards (rule 7). Colorbars are
  value-proportional — never log-spaced bins (they collapse the working
  range into one color block). Branding must be croppable (banner strip)
  and say cafire.org/weather only.
- gridMET lags ~1–2 days; "day index N not published yet" in logs is
  benign; fuel import falls back day −1/−2/−3.
- **`rw_fire_api` FORCES projection env on the rw_render child** (~line 730):
  `RUSTWX_PROJECTED_FRAME_SOURCE=requested` and
  `RUSTWX_PROJECTION_VARIANT=<projection_variant_for_bounds()>`. A local repro
  that ignores these exercises a DIFFERENT branch and will disagree with
  production — that cost an hour on the 2026-07-25 CONUS clipping bug. When a
  faithful local probe contradicts the live service, check what env the parent
  sets on the child. Continental boxes (lat_span ≥ 25 or lon_span ≥ 45) MUST get
  `adaptive`: a regional Mercator has one reference latitude and cannot describe
  26° of it, and the mismatched frame gets the raster CLIPPED (CONUS lost Texas,
  the Gulf and Florida). `MapExtent::from_bounds` only ever PADS, never crops.
- **Both labs must be updated together.** `generic_lab.html` is a separate
  hand-maintained page; it silently drifted to 105 slugs while
  `cafire_preview.html` grew to 359, because
  `preview_site_exposes_the_full_catalog` only reads the CAFire page.
  `generic_lab_mirrors_the_lab_map_catalog` now fails the build on drift. The
  generic page's script is wrapped, so `FAMS` is NOT a window global — inspect
  the DOM when driving it with CDP.
- **Place labels declutter in PIXEL space** (`draw_projected_place_labels`
  reserves each drawn rect and tries other quadrants). The upstream place
  SELECTION declutters in kilometres, which cannot know rendered text width —
  that is why continental maps used to pile names on top of each other. Any new
  label overlay must go through the same reservation or it will overprint.
- **`cargo test --workspace` STOPS at the first failing test binary.** Use
  `--no-fail-fast` or you will believe there is only one failure. Known
  pre-existing failures (2026-07-25): 2× `rw-ingest size_estimate` (stale table,
  121 builtin variables vs 116 covered) and
  `ingest_derived_matches_direct_calc_kernels_bit_exactly`.
- If the dev box crashes mid-build, `target/debug` metadata can be corrupted
  (`error[E0786] invalid metadata files for crate ...`). `rm -rf target/debug`
  and rebuild; release binaries already written are fine.

## 10. State snapshot (2026-07-06) & open items

Healthy: all 4 units + 4 containers up; disk ~70 % with all three caps
proven working; HRRR/GFS/NBM lanes current; branch pushed.

Open / parked:
- **Jul-17 soak cleanup** (HETZNER_OPS end of doc): delete
  `data/cache/hrrr`, `volume-stores`, `wxstore` — **KEEP
  `cafire-cache-cap`**; optional `WXSTORE_ENABLED=false` tidy.
- **CAFire client asks** (their side, one-liners): add `&format=png` to
  share/open-image links; fix their zoom crash by zooming the PNG (or
  dropping custom zoom — our Lab has none and doesn't crash).
- Node-1 ECAPE/PFT serving paused — restart recipe in HETZNER_OPS; PFT2
  blocked on unpublished entrainment constant.
- `rw_pipeline` still doesn't pass a cache-age flag to its internal
  rw_prune (hourly cron is the backstop).
- Daily-card wishlist: anomaly-percentile coloring of strip cells.

## 11. Working with Drew

- Explicit approval for anything that changes the server; read-only
  inspection is always fine. When he grants broad discretion, still verify
  everything and report outcomes faithfully — including what you did NOT do.
- **Answer his mid-task messages immediately** and post short status lines
  during long operations; silence has been flagged twice.
- Precision matters: don't guess upstream behavior (AWS mirrors publish
  AFTER NOMADS; byte-range fetches are not faster for us). Retract wrong
  claims plainly.
- He prefers the least-work-for-CAFire option and server-side fixes over
  asking their team to change things.
- Primary Claude sessions keep auto-memory on Drew's machine, but other
  agents can't read it — that's why this doc exists. Keep it current.
