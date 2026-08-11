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
`root@$CAFIRE_BOX` — 16-core AMD EPYC-Genoa, 30 GB RAM, ~230 GB disk.

> The address is deliberately NOT in this repo: it is public on GitHub. Get it
> from your ssh config / password manager and export `CAFIRE_BOX=<host>` before
> running anything below.
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
- **`generic.wxsection.com`** is a second host serving the unbranded lab:
  root rewrites to `/generic`, only `/generic`, `/api/*` and `/outputs/*` are
  exposed, everything else 302s to `/generic` so the CAFire-branded `/`, `/ops`
  and `/legacy` never appear there. Full block + the bind-mount inode trap:
  HETZNER_OPS §5. Note `wxsection.com` itself is NOT this box — it is a
  Cloudflare Tunnel from Drew's PC (`.cloudflared/config.yml`), so never
  repoint the apex here without checking that config.

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
   **Domains:** HRRR is CONUS and NBM is CONUS, but **GFS is GLOBAL** — the
   0.25° file is stored whole (~197 MB/hour, 1440×721), so point products work
   anywhere on Earth (verified: London, Tokyo, Sydney, mid-Pacific, Brasília).
   Nothing subsets it to CONUS.
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
  `[&theme=cafire|slate|midnight|paper|ember|mono][&brand=][&credit=][&footer=][&accent=#rrggbb][&logo=<id>]`
· `POST /api/card-logo` (image bytes or a `data:` URL → `{id}` for `&logo=`)
· `GET /api/xsection?lat0&lon0&lat1&lon1&run[&hour][&field=temperature|rh|wind][&format=json|png]`
· `GET /api/sounding?lat&lon&run&hour[&style=cwt][&layout=<tokens>][&brand=][&note=][&zoom=][&dpi=][&text_scale=][&format=json]`
  Default is the sharppyrs SPC window (BowEcho's), branded `cafire.org/weather`
  unless `brand=` says otherwise; `style=cwt` serves the older house composite.
  `note=` is the poster's context ("why I am posting this"), and it is drawn in
  the **NOTES PANEL**, not on the header line — see the token grammar below for
  where that panel sits. The panel wraps the note with egui's own text layout and
  elides what does not fit with a trailing `…`; ~500 characters fit its default
  cell, and `SOUNDING_HEADER_MAX_CHARS` caps `note=` at 240 before that, so in
  practice a note is never cut. The header line is left to the credit alone,
  because repeating the note there would be the same sentence twice. **A `layout=`
  with no `notes` cell falls back to the old line** (note ahead of the credit) so
  the note cannot vanish silently.
  **That line is fitted, not capped.** sharppyrs draws it right-aligned and
  unclipped, so anything wider than its band runs leftward across the title
  instead of eliding — `sounding_sharppy::fit_header` reserves the credit's
  width, then elides at a word boundary with `…`. Two consequences worth
  knowing: the band is a fraction of the window width, so a bigger `zoom=` costs
  header characters; and `HEADER_FONT_PT`/`HEADER_RIGHT_PAD_PT` mirror literals
  inside sharppyrs, so bumping its rev means checking them.
  **The hodograph draws into a centered SQUARE**, so its axes span equal knots.
  Its kt-per-pixel is one isotropic scalar, and taking it from the WIDTH of a cell
  wider than it is tall left the vertical axis short — it read 120 across and 80
  down before. Squaring the drawn area letterboxed the cell; the alternative,
  stretching to fill, would make isotachs elliptical and misstate every shear
  vector, so it is not on the table. **That letterbox is now the
  location map**: the default `layout=` splits the big upper-right cell at the
  fraction that squares the hodograph's share (409.25 x 409.37 pt on the stock
  board), so the plot lost nothing and the map got the 166 pt that used to be
  background. The trailing `layout=` token is the window in knots; **195 is what
  puts 90 kt as the outermost labeled ring on all four axes**, and only rings
  whose labels fit whole get labeled at all (rings past that still draw,
  unlabeled). Both live upstream in `panels::hodo` — bumping the rev means
  re-checking them, and BowEcho shares the pin.

  **`layout=` token grammar** (`sounding_sharppy::DEFAULT_LAYOUT_TOKENS` has the
  arithmetic; malformed tokens fall back to it rather than erroring). Five
  `|`-separated sections plus an optional sixth, panel tokens comma-separated
  inside a section:

  ```text
  strips(2) | main(1 or 2) | insets(4) | bottom(6) | hodo_zoom_kts [ | g3:<geometry> ]
  ```

  Our default, which is also the string to copy and edit:

  ```text
  layout=speed,advection|hodograph,locationmap|slinky,thetae,srwinds,streamwiseness|convectiveindices,kinematics,notes,severeindices,hidden,hidden|195
  ```

  - Panel tokens: `speed` `advection` `hodograph` `slinky` `thetae` `srwinds`
    `locationmap` `hazardtype` `convectiveindices` `kinematics` `severeindices`
    `indexboard` `ship` `streamwiseness` `stp` `notes` `hidden`. Any token is
    legal in any cell.
  - **main takes ONE or TWO cells.** One (`hodograph`) gives the whole cell to
    that panel — every pre-split string still parses and still lays out
    identically. Two (`hodograph,locationmap`) splits it SIDE BY SIDE, the first
    panel keeping the left share.
  - **bottom takes 6:** slots 0 and 1 are full-height columns, slots 2 and 3
    share the third column vertically (2 on top — that is the notes slot), slots
    4 and 5 are full-height columns again. Hidden slots must be the TRAILING
    ones; hiding a middle one hands its space to the panel sharing its column,
    whose text then scales past its box. A legacy 3-cell bottom section is
    migrated.
  - `hodo_zoom_kts` is a plain decimal, clamped to 80..=500.
  - The optional `g3:` section is six `;`-separated groups:
    `top_height,skew_width,right_main_height` `;` `right_col(3)` `;`
    `inset_col(4)` `;` `bottom_col(5)` `;` `bottom_split` `;` `main_split`.
    `main_split` is the fraction of the main cell's WIDTH the first panel keeps,
    clamped to 0.20..=0.80, default 0.711. **Lower it for a wider map and a
    smaller hodograph; raising it above ~0.7112 does not grow the hodograph** —
    the plot is height-bound past that, so the extra width is letterbox again.
    Legacy `g1:` (4 groups) and `g2:` (5 groups) still parse and leave the main
    cell unsplit. Omitting the section entirely — which our default does — keeps
    every fraction at sharppyrs' own defaults, so upstream board tweaks still
    reach us.
· `GET /api/fires` (WFIGS)
· `GET /api/ecape/...` (frozen static gallery while node 1 is paused).

**Near-surface smoke wears the EPA PM2.5 AQI categories** (`smoke_pm25_native`
and the `smoke_8m_*` windowed maxima). Boundaries are the 2024-revised
breakpoints — `9.0 / 35.4 / 55.4 / 125.4 / 225.4` µg/m³ — and they are EXACT bin
edges, with the official AirNow RGB per band and three darkening steps inside
each. Three traps if you touch `epa_pm25_*` in `plot_design.rs`:

- The renderer picks a bin's color by **numeric position** across the level span,
  not by bin index, so a palette listing one color per band does NOT produce one
  color per band on a nonlinear ladder. The palette is a fine lookup table of the
  step function for that reason.
- Within-band shading **darkens only**, and the factor has a floor near `0.747`:
  darken yellow past that and it becomes olive, which is nearer EPA's orange than
  its own yellow. A test enforces this.
- **Column smoke is deliberately NOT on this scale** — mg/m² through the column
  is not what anyone breathes, and a plume aloft would claim a health category
  the number cannot support.

The categories are a proxy, and say so in user-facing copy: AQI breakpoints are
defined on a 24-hour average, these fields are hourly, and HRRR-Smoke carries
smoke only (no background aerosol), so it reads low where other pollution exists.

Gotchas: surface RH variable is **`rh_2m`** (not relative_humidity_2m);
window-product slugs look like **`2m_temp_24_48h_max`** (full list in the
Lab's `FAMS`); run slugs look like `20260706_12z`.

**RW Store small reads cache decoded 2-D tiles.** Each `HourReader` lazily
retains at most 8 MiB of dense tiles by default, shared by point and window
reads. `read_full_2d` bypasses the cache so day reductions do not retain entire
hours. High-fanout callers can use `open_with_tile_cache_bytes` (including
zero to disable it), and `tile_cache_stats` exposes hits, misses, and memory.
The reported byte count covers cached f32 payloads, not allocator metadata,
outstanding `Arc` handles, or per-thread zstd contexts.

## 8. Deploying (condensed — full detail in HETZNER_OPS §7)

```
# on Drew's PC, from the repo root — COMMIT FIRST, the server only ever
# receives git archive snapshots; then PUSH
git archive --format=tar HEAD | ssh root@$CAFIRE_BOX "tar -x -C /opt/rusty-weather/src"

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
  A `Center` label is the exception to the quadrant search: it means "this
  reading belongs to THIS point", so it draws where it belongs or is dropped —
  never slid inside the frame, which would silently reattach a value to another
  place.
- **Place SELECTION: majors are offered to the declutter first**
  (`major_cities_first`), because the declutter is greedy and first-come. It also
  keeps TWO footprints per candidate: `selection_bounds` (is this place in
  frame?) uses the preset's real footprint, while `overlap_bounds` (are these two
  labels too close?) is a uniform small box for overlay labels. A preset's
  footprint scales with importance — 1.9° for a major, 0.58° for a hamlet — so
  reusing it as a keep-away zone made big cities suppress their neighbours over
  ~500 km while two hamlets 60 km apart both passed. That is what deleted
  Los Angeles, Sacramento, San Diego and Reno from a dense California map.
  `min_center_spacing_km` is the real spacing control; city CROP selection still
  uses the true footprint, where it means what it says.
- **City value labels** (`value_labels: true` → `RUSTWX_VALUE_LABELS=1`) stamp
  the plotted value at each selected place instead of its name. They opt OUT of
  the priority tiers: names are a size/opacity hierarchy, but a bare number has
  no hierarchy to express, so every value renders at one bold size
  (`VALUE_LABEL_SCALE`) with a white halo, centred on its point, no dot.
- **`cargo test --workspace` STOPS at the first failing test binary.** Use
  `--no-fail-fast` or you will believe there is only one failure. Known
  pre-existing failures (2026-07-25): 2× `rw-ingest size_estimate` (stale table,
  121 builtin variables vs 116 covered) and
  `ingest_derived_matches_direct_calc_kernels_bit_exactly`.
- If the dev box crashes mid-build, `target/debug` metadata can be corrupted
  (`error[E0786] invalid metadata files for crate ...`). `rm -rf target/debug`
  and rebuild; release binaries already written are fine.
- **`git archive` deploys can SILENTLY not rebuild.** `git archive` stamps
  extracted files with the COMMIT time. If that is older than the existing
  `target/` fingerprints, cargo decides nothing changed, skips the build, and
  `install` copies a STALE binary — the deploy reports success, the source on the
  box is right, and the running code is old. This burned a full debug cycle on
  2026-07-25 (the run-alias fix "deployed" three times before it took). ALWAYS
  touch the sources after extracting, and verify the binary, not the deploy:

      git archive --format=tar HEAD | ssh $BOX 'tar -xf - -C /opt/rusty-weather/src
        && cd /opt/rusty-weather/src
        && find crates vendor -name "*.rs" -newermt "-1 day" -o -name "*.rs" -exec touch {} +
        && cargo build --release --bin <bin>'

  Then prove the new code is in the running binary before believing it:
  `strings /opt/rusty-weather/bin/<bin> | grep -c "<a string only the new code has>"`.
  Never conclude "deployed" from an exit code alone.
- **ffmpeg is now a SERVER DEPENDENCY** (installed 2026-07-25, 6.1.1 with libx264
  + libvpx-vp9). Loop video export (`/api/loops/<id>/animation.mp4|.webm`) shells
  out to it; without it those endpoints 500 with "is ffmpeg installed?" while GIF
  keeps working. Reinstall it if the box is ever rebuilt: `apt-get install -y ffmpeg`.
  Video is the right default here, not a nicety — measured on a 13-frame CONUS
  loop, MP4 is 336 KB against GIF's 3.6 MB, and GIF's 256 colors visibly band the
  temperature ramps.
- Run pointers in `latest.json` (`day_run`, `complete_run`, `fuel_run`) advance
  as soon as a cycle STARTS ingesting, so they can name a run that does not yet
  hold the hours a product needs — an extended HRRR cycle takes over an hour to
  walk F000 to F048. The API resolves aliases against the requested hour
  (`resolve_latest_run_for_hour`) and falls back to an older run that has it;
  keep that behavior if you touch alias resolution, or every 0-48 h window
  product breaks for an hour after each extended cycle, four times a day.

- **Both lab pages are `include_str!`d into `rw_fire_api`**, so an HTML-only
  change is still a Rust rebuild — editing `generic_lab.html` and reloading the
  browser serves the OLD page from the running binary, silently. Rebuild
  `rw_fire_api` (locally too) before believing a UI change didn't work.
- Outlook-card **theming lives in `CardTheme` (`meteogram.rs`)**, and only the
  card CHROME is themed — paper, text, gridlines, accent, attribution. The data
  colors (absolute temperature ramp, precip blues, the 15/30 mph wind
  thresholds) are deliberately identical in every theme: recolor those and two
  cards of the same forecast stop being comparable. `theme=` picks a palette,
  `brand=`/`credit=`/`footer=` override the attribution, and a PRESENT-but-EMPTY
  value clears a line while an ABSENT one inherits the theme's own — the generic
  lab relies on that distinction to honor its "Branding: None" button.
- **A web map's longitude is unbounded; every stored grid is -180..180.**
  Leaflet's `mouseEventToLatLng` keeps counting past the antimeridian (-186,
  +200, +560), so a point picked after panning missed every grid cell and the
  point products answered "point is outside the model grid" — for GLOBAL GFS,
  which plainly covers it. `wrap_longitude` normalizes at the API boundary
  (daily, meteogram, sounding, xsection) and the labs wrap before filling the
  boxes. Keep both: the server one is the guarantee, the client one keeps the
  displayed numbers sane.
- **Card logos are uploaded, not inlined.** Cards are GETs — that is what makes
  them shareable/downloadable/copyable by URL — so a base64 logo in the query
  string would blow past request-line limits. `POST /api/card-logo` re-encodes
  to PNG, caps the long edge at 512 px, content-addresses it (FNV-1a, a cache
  key and NOT a security boundary) and stores `<out_root>/card-logos/<id>.png`;
  the card then references `&logo=<id>` and embeds it as a `data:` URI so the
  SVG stays one self-contained document. `card_logo_path` is the only thing
  between that query parameter and the filesystem — keep it strict.
- **Place naming is global, from two files.** `us_places_gazetteer.tsv` (Census,
  public domain, ~32k places) plus `world_cities_gazetteer.tsv` (GeoNames
  cities5000, **CC BY 4.0 — keep the attribution in the file header**, ~57k
  cities in 244 countries). GFS is global, so a card can be raised anywhere, and
  with only the US file a London point read "3005 mi ENE of Lubec, ME". The
  second column is a USPS state for US rows and a COUNTRY NAME for the rest —
  never an ISO2 code, because CA/MO/MD/ME/LA/DE/IN/AL/PA/MT/NE/IL/VA all collide
  with state abbreviations and "London, CA" would read as California. Regenerate
  with `tools/make_world_gazetteer.py` (needs the GeoNames zip + countryInfo.txt).
  Known limit, pinned by `a_city_district_can_still_win_over_its_metro`: a
  district of a city is just another populated place in the data, so central
  Brasilia resolves to "Plano Piloto" — separating a district from an
  independent neighbor needs boundary polygons we do not carry.
- **Place naming knows how big a city is** (`CITY_FOOTPRINTS` in places.rs).
  The gazetteer holds ONE Census internal point per place and New York City's is
  in **Brooklyn**, so a Manhattan point used to resolve to "Hoboken, NJ" — 6 km
  away across the Hudson versus 13 km to the city's own point. Cities in that
  table get a footprint radius; a point inside one has its distance discounted
  5×, and `NearestPlace::inside_footprint` tells callers to say "New York, NY"
  rather than "8 mi N of New York, NY". Radii are deliberately UNDER the true
  extent — over-claiming (Newark reading as New York) is worse than falling back
  to the old nearest-point answer.
  **The anchor matters as much as the radius.** A Census internal point only has
  to fall inside the polygon: San Francisco's is 50 km out in the Pacific among
  the Farallon Islands the city-county takes in, so downtown resolved to "Daly
  City, CA" and no radius centered there could have helped. Anchorage's is 33 km
  off, Corpus Christi's 22, New Orleans' 17. Each table row therefore carries its
  own anchor (GeoNames' downtown point), which also REPLACES the row's
  coordinates so distances and bearings refer to the city. Regenerate with
  `tools/make_city_footprints.py`; radii are hand-set in that script and
  `footprint_cities_are_anchored_on_themselves` fails the build if an anchor ever
  resolves to a different place. The world file carries its own footprint column,
  derived from population, so a big city beats its own subdivisions.
- **A locally-shifted HOUR needs a locally-rolled DAY.** Both sounding headers
  print the valid time twice, Z and local. The CWT subtitle built its local half
  by adding the offset to the UTC hour and printing that against the UTC day
  label, so at UTC-7 an 03Z valid time read `Tue 7/28 03Z (20 local)` when 8 pm
  is MONDAY — an off-by-one on a forecast valid time, which a forecaster acts on
  rather than notices. `meteogram::local_valid_label` rolls the date with the hour
  and hands back both, and `sounding::cwt_valid_clause` is the only place that
  clause is composed, so the halves cannot disagree. The sharppyrs title had the
  same bug and the same fix; a third caller should take the helper, not the
  arithmetic.
- **Fast card iteration without a deploy or a full store:** hardlink one stored
  hour into a throwaway store so the card has hours to walk, and point a local
  API at it — no copy, no disk cost.

      $s="C:\rw\store\hrrr\20260725_18z"; $d="C:\rw\preview-store\hrrr\20260725_18z"
      # link grid.rwg, copy run.json, then f000..f047 -> the one real hour
      0..47 | % { New-Item -ItemType HardLink -Path ("$d\f{0:d3}.rws" -f $_) -Target "$s\f006.rws" }
      # run=latest needs a pointer the daemon would normally write
      '{"day_run":"20260725_18z","complete_run":"20260725_18z","run":"20260725_18z"}' > C:\rw\preview-store\hrrr\latest.json
      cargo run --bin rw_fire_api -- --port 8799 --store-root C:/rw/preview-store --out-root C:/rw/cache

  Every column carries the same values (one real hour), so this proves layout,
  palette and plumbing — not the data. Kill the binary before `cargo build` or
  the link step fails with "Access is denied".

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
