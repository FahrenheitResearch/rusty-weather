# Hetzner Server Operations — Master Doc

**Audience:** Drew and any agent maintaining the CAFire weather production
server. Read this first; it is self-contained. Written 2026-07-03 against
the live server state — every path, unit, and number below was verified on
the box, not recalled.

Companion docs (external contract, do NOT break what they promise):
`docs/CAFIRE_HANDOFF.md` + `docs/CAFIRE_HANDOFF_ADDENDUM_1.md` — the API
surface CAFire.org builds against.

---

## 0. Hard rules (Drew's standing orders)

1. **Never show `cafire.wxsection.com` to end users.** It is a backend
   hostname. User-facing branding is `cafire.org/weather` and
   "CALIFORNIA WILDFIRE TRACKING".
2. **Never fake fuel data.** If gridMET is lagging, fuels stay absent.
3. **`.rws` / rw-store is the canonical backend** — never resurrect
   WxStore for new work.
4. **Never lower plot quality for speed** without Drew's explicit ok.
5. **The croppable branding strip stays** on all map plots (crop the top
   band off and the plot must still be complete).
6. **Legacy data is frozen until ~2026-07-17** (two-week no-delete promise
   made to CAFire at the 2026-07-03 cutover). See §9.
7. Long-running remote jobs go in **tmux, always** — nohup'd processes get
   reaped when the ssh session scope exits.

---

## 1. What this server is

One Hetzner box serving the fire-weather API that CAFire.org's site
integrates: model-driven maps, meteograms, daily outlook cards, cross
sections, soundings, climatology/anomaly products, fire perimeters,
plus legacy satellite + lightning imagery.

- **Host:** `ubuntu-32gb-nbg1-1` — 16-core EPYC, 30 GB RAM, 600 GB disk,
  20 TB/mo bandwidth (current use ~12 TB/mo).
- **Access:** `ssh root@178.104.59.253` (key auth already set up on
  Drew's Windows machine and on weather-node-1).
- **Public URL:** `https://cafire.wxsection.com` (TLS via the legacy
  stack's Caddy container; Cloudflare in front).
- **Source repo (canonical):** `C:\Users\drew\rusty-fire-weather` on
  Drew's Windows machine, branch `codex/rusty-fire-weather-cafire-ui`.
  A source copy lives at `/opt/rusty-weather/src` on the server (that is
  a deploy artifact, not the source of truth).

Two stacks coexist:

| Stack | Where | Runs as | Purpose today |
|---|---|---|---|
| **rusty-weather** (the product) | `/opt/rusty-weather` | 4 host systemd services | Everything except satellite/lightning |
| **legacy docker stack** | `/opt/cafire-weather-service` | docker compose | Satellite + lightning only (HRRR side retired 2026-07-03), plus the Caddy that fronts *both* stacks |

---

## 2. 60-second health check

```bash
ssh root@178.104.59.253
systemctl is-active rusty-wx-api rusty-wx-pipeline rusty-wx-pipeline-gfs rusty-wx-pipeline-nbm
df -h /                                    # worry below ~40 GB free
curl -s localhost:8788/api/health          # "ok": true
cat /opt/rusty-weather/store/hrrr/latest.json   # run should be ≤ ~2h old
cd /opt/cafire-weather-service && docker compose ps   # api, caddy, satellite-worker, lightning-worker Up
```

Freshness expectations: a new HRRR run lands roughly hourly (NOMADS makes
each cycle available ~50–90 min after its init time, so "latest run is
1–2 h behind wall clock" is normal; **3+ h behind is an incident** — see
§8). GFS refreshes 6-hourly, NBM roughly 6-hourly (synoptic cycles, ~12 h
publication lag is normal for the longest hours).

Journal errors, last 3 h, all lanes:

```bash
journalctl -u rusty-wx-pipeline -u rusty-wx-pipeline-gfs -u rusty-wx-pipeline-nbm \
  --since '3 hours ago' --no-pager --output cat | grep -icE 'error|failed|panic'
```

Occasional transient fetch errors are fine; hundreds of lines or a
repeating identical panic is not.

---

## 3. The four systemd services

All unit files in `/etc/systemd/system/`, all `Restart=always`,
`WantedBy=multi-user.target`. Binaries in `/opt/rusty-weather/bin/`.

| Unit | What it does | Key flags |
|---|---|---|
| `rusty-wx-api` | The HTTP API (`rw_fire_api`) on `0.0.0.0:8788` | `--max-render-jobs 3 --render-timeout-secs 300`, store/out roots, spawns `rw_render` children |
| `rusty-wx-pipeline` | HRRR lane: ingest + fuels + prune + prewarm | `--interval-mins 5 --profile view-volumes`, Nice=10 |
| `rusty-wx-pipeline-gfs` | GFS lane (F0–192 step 6) | `--interval-mins 15 --keep-recent 2`, Nice=12 |
| `rusty-wx-pipeline-nbm` | NBM lane (F6–264 step 6) | `--interval-mins 20 --keep-recent 2`, Nice=13 |

Details that matter:

- **Per-model lock files** at `/opt/rusty-weather/store/.rw-pipeline-lock-{hrrr,gfs,nbm}`.
  A stale lock makes the daemon exit instantly on every start. Each unit's
  `ExecStartPre` removes **its own** lock, so `systemctl restart <unit>`
  is the correct fix — never edit a unit to remove a *different* model's
  lock (that mismatch caused the July 2 crash-loop).
- Only the **HRRR** lane runs fuels import and prewarm; GFS/NBM are
  ingest-only.
- The HRRR lane ingests with `--profile view-volumes`: 2-D + derived
  fields **plus five 3-D isobaric volumes** (temperature/dewpoint/u/v/
  height on 37 levels @ 25 hPa) that power `/api/xsection` and
  `/api/sounding`. Volumes exist for hours ingested **since 2026-07-03
  00Z** — older stored hours 422 on those endpoints by design.
- Each pipeline tick: probe AWS/NOMADS idx for the newest cycle → fetch
  missing hours (`rw_batch`) → fuels once per run (HRRR) → atomically
  update `latest.json` → `rw_prune` → prewarm renders via the API when a
  run completes.

---

## 4. Filesystem layout and disk economics

```
/opt/rusty-weather/
├── bin/        rw_fire_api  rw_pipeline  rw_render  rw_batch  rw_prune
│               rw_fuel_fetch  rw_fuel_import  rw_climo_import  rw_land_mask
├── store/      the .rws model stores (see below)
├── cache/      _raw_fetch/ = raw GRIB downloads (~90-150 GB; capped by hourly cron, see §4)
├── out/        render outputs: job-*/ dirs (disposable cache) + ecape/ (KEEP)
├── src/        source tree from last deploy (build workspace)
└── logs/       one-off build/backfill logs (journald has the real logs)
```

**Store** (`/opt/rusty-weather/store/`):

- `hrrr/` (~57 GB), `gfs/` (~12 GB), `nbm/` (~1.7 GB) — run dirs named
  `YYYYMMDD_HHz/` holding `f###.rws` hour files. HRRR runs also carry a
  `.fuels-imported` flag file once fuel grids are merged in.
- `<model>/latest.json` — the per-model manifest (schema
  `cafire.latest_run.v1`). Three run pointers, each feeding a different
  API alias so every product family resolves to a run that actually has
  what it needs (a run is promoted to a pointer only once it's *ready*
  for that family — see the fix below):
  - `complete_run` → alias **`latest`**: newest fully-ingested run.
    Advances the instant weather ingest finishes → weather maps always
    freshest. Never gated on fuels.
  - `day_run` → alias **`latest-day`**: newest **complete** run covering
    a full UTC day (anomaly / day-window products). Requires full ingest
    so 24–48h windows aren't rendered against missing hours; off-cycle
    F18 runs never qualify, so it rides the extended 00/06/12/18Z runs.
  - `fuel_run` → alias **`fuel-run`**: newest complete run whose
    `.fuels-imported` flag is set (HRRR only). Fuel products (ERC etc.)
    resolve here, so a slow/failed gridMET import never errors a fuel map
    on `latest` and never freezes the weather pointer.
  Also carries `run`, `stored_hours`. Written atomically by the pipeline;
  the API resolves all three aliases **before** computing render-cache
  keys. (Fix `86a1f4d`, 2026-07-03: before this, pointers advanced before
  a run was ready — fuels lagged on `latest`, and `day_run` promoted to a
  still-ingesting extended run — causing recurring `rw_render exit 1`.)
- `rtma_climo/` (~34 GB) — **precious, not reproducible on this server.**
  Two runs: `seasonal_v2026_05_24` (DOY percentile climatology) and
  `exact_v2026_05_24` (all-time records), each with a `land_mask.bin/json`
  sidecar. Source packs live on weather-node-2; a rebuild means a
  multi-hour repack + ~34 GB transfer. Never let a prune script near it
  (`rw_prune` doesn't touch it; keep it that way).

**Disk budget** (2026-07-03: 454 GB used / 123 GB free, 79%):

| Consumer | Size | Behavior |
|---|---|---|
| `cache/_raw_fetch` | ~90–150 GB | Raw GRIBs, re-fetchable dead weight post-ingest. **Capped by an hourly cron** (`/etc/cron.hourly/rw-rawfetch-cache-cap`, deletes files >6 h) — NOT by the pipeline. ⚠️ The daemon's own `rw_prune` does **not** honor the 6 h cache policy (`rw_pipeline` has no `--cache-max-age-hours` flag to pass through; it leaves 6–24 h GRIBs). Without the cron this balloons past 200 GB and fills the disk (happened 2026-07-03, →94%). If the cron is ever removed, the proper fix is to rebuild the pipeline so its internal prune passes the age flag. |
| `store/hrrr` | ~57 GB | Pruned to newest runs + newest long (≥F030) run |
| `store/rtma_climo` | 34 GB | Fixed |
| legacy `data/` | ~180 GB | Shrinks ~123 GB after the §9 cleanup |

Levers if disk gets tight, in order: (1) confirm the `rw-rawfetch-cache-cap`
cron exists and is running — if `_raw_fetch` is past ~150 GB the cron is
missing or broken; reclaim immediately with
`find /opt/rusty-weather/cache/_raw_fetch -type f -mmin +360 -delete`;
(2) execute §9 if past the soak window; (3) HRRR `--keep-recent` 3→2 buys
~15 GB. The July 2 incident (§8) is what happens when this is ignored: at
95% full the ingest daemon dies and the public site serves stale weather.
The 2026-07-03 recurrence (disk hit 94%) was this exact cause — the
daemon's prune silently leaving old GRIBs — and is why the hourly cron now
exists as a backstop.

---

## 5. API surface and request flow

`rw_fire_api` listens on 8788, reachable only via Caddy (ufw allows
8788/8790 from the docker bridge `172.18.0.0/16` only; the internet
cannot hit it directly).

Endpoints (full contract in the CAFire handoff docs): `/lab` (the
reference client — served from the binary, baked in at build time from
`crates/rusty-weather/src/cafire_preview.html`), `POST /api/render`
(async job → files under `/outputs/`), `/api/jobs/*`, `/api/meteogram`,
`/api/daily`, `/api/xsection`, `/api/sounding`, `/api/runs`
(+ `?var=` hour-probe), `/api/vars`, `/api/fires` (WFIGS perimeters),
`/api/ecape/*` (static gallery pushed from node 1), `/api/health`.

- **Render cache** is keyed on the full request body (with run aliases
  resolved first). Consequence: after any deploy that changes plot
  *appearance*, old cached images keep serving until you clear
  `out/job-*` (§7 step 5).
- `temp_units` defaults to °F on surface temperature maps; `"c"` opts out.
- Request body cap 2 MB; render children are killed at 300 s.

**Caddy routing** — lives in the *legacy* stack's
`/opt/cafire-weather-service/Caddyfile`, container `caddy`:

| Path | Backend |
|---|---|
| `/api/*` **except** `/api/v1/*` | rusty-weather API (172.18.0.1:8788) |
| `/lab*`, `/outputs/*` | rusty-weather API |
| `/node/*` | weather-node-1 via reverse SSH tunnel (172.18.0.1:8790) |
| `/api/v1/*` and everything else | legacy `api:8000` (satellite/lightning + old site) |
| `/wxstore/*` | wxstore:8899 — **stopped**, so this 502s; expected |

Two Caddy gotchas, both learned the hard way: (1) the Caddyfile is
bind-mounted — after editing it you **must** `docker compose restart caddy`
(sed/scp create a new inode the container can't see); (2) you almost
never need to edit it — the `/api/*` matcher already routes any *new*
API endpoint to the new stack with zero config change.

---

## 6. The weather-node-1 relationship (heavy compute)

PyroCb/ECAPE/PFT products are too heavy for this box. They render on
**weather-node-1** (Drew's home lab, 24 cores) and reach the public
through this server. Nothing on Hetzner initiates contact with the node —
the node dials **out** (safe pattern; node stays unreachable from the
internet).

On node 1 (`ssh drew@weather-node-1.local`), four tmux sessions:

| tmux | Runs |
|---|---|
| `wxpipe` | `rw_pipeline --profile full` — full-profile HRRR ingest incl. heavy/ECAPE/PFT grids |
| `nodeapi` | `rw_fire_api` on :8791 |
| `nodetunnel` | `ssh -N -R 172.18.0.1:8790:127.0.0.1:8791 root@<hetzner>` in a respawn loop |
| `ecapepush` | `push_ecape.sh` 15-min loop → rsyncs rendered galleries to Hetzner `/opt/rusty-weather/out/ecape/` |

Hetzner-side enablers (already configured): sshd `GatewayPorts
clientspecified`, ufw 8790 from 172.18.0.0/16, Caddy `/node/*` route.

**If `/node/*` 502s:** the tunnel or node API died. Fix on node 1, not
here — check the tmux sessions exist and restart the dead one
(`tmux new-session -d -s nodetunnel '<cmd>'`). Note node IPs drift after
router resets; use the `.local` mDNS name.

---

## 7. Deploying code changes

From the Windows repo (`C:\Users\drew\rusty-fire-weather`). Pattern used
for every deploy so far:

```powershell
# 1. ship the tree (from repo root; git archive respects the index)
git archive --format=tar HEAD | ssh root@178.104.59.253 "tar -x -C /opt/rusty-weather/src"
```

```bash
# 2. build on the server (~1-2 min warm)
cd /opt/rusty-weather/src && cargo build --release --bin rw_fire_api --bin rw_render --bin rw_pipeline --bin rw_batch --bin rw_prune --bin rw_fuel_fetch

# 3. swap binaries — cp over a RUNNING binary fails with ETXTBSY;
#    cp-to-temp + mv is atomic and always works:
for b in rw_fire_api rw_render rw_pipeline rw_batch rw_prune rw_fuel_fetch; do
  cp target/release/$b /opt/rusty-weather/bin/$b.new && mv /opt/rusty-weather/bin/$b.new /opt/rusty-weather/bin/$b
done

# 4. restart what changed
systemctl restart rusty-wx-api          # api / render changes
systemctl restart rusty-wx-pipeline rusty-wx-pipeline-gfs rusty-wx-pipeline-nbm   # pipeline changes

# 5. ONLY if plot appearance changed — clear the render cache, PRESERVING ecape:
find /opt/rusty-weather/out -maxdepth 1 -name 'job-*' -exec rm -rf {} +
```

**Runtime system dependency — fonts (required, or PNG shares render wrong).**
`rw_fire_api` rasterizes the SVG cards to PNG for `/api/daily?format=png`
(the shareable/copyable image) via resvg, which reads fonts from
`/usr/share/fonts`. The cards name `Inter` (daily) and `IBM Plex Mono`
(meteograms); `fonts-dejavu-core` is a glyph fallback. If the box is ever
rebuilt, reinstall these or the shared PNGs fall back to the wrong faces:
```bash
apt-get install -y fonts-inter fonts-ibm-plex fonts-dejavu-core
```
This is a *runtime* dep of the API, not just a build dep — the SVG endpoint
(default, no `format`) doesn't need it, but the PNG one does.

6. Verify public: hit `https://cafire.wxsection.com/api/health`, then one
fresh render through `/lab`. The Lab HTML is compiled into `rw_fire_api`,
so UI changes deploy with the binary — no separate file copy.

The same pattern (git archive → build → tmux-managed restart) deploys to
node 1; exact node process args are recorded in memory/session notes —
capture `ps` output before killing anything there.

---

## 8. Incident runbook (every entry below actually happened)

**Public data is stale (latest run hours behind).** July 2 incident.
Triage order:
1. `df -h /` — at ~95% the ingest daemon crashes mid-run. Free space:
   `find /opt/rusty-weather/cache/_raw_fetch -type f -mmin +360 -delete`
   (the big, always-safe reclaim), then check the `rw-rawfetch-cache-cap`
   cron still exists (§4 — the daemon's own prune doesn't cap `_raw_fetch`,
   so if that cron is gone the cache re-balloons). Then restart the
   pipeline units.
2. `systemctl status rusty-wx-pipeline` — crash-looping? Almost always a
   stale `.rw-pipeline-lock-hrrr` surviving a crash. `systemctl restart`
   clears it via ExecStartPre.
3. `journalctl -u rusty-wx-pipeline --since '2 hours ago'` for the real
   error (NOMADS outages also happen; those self-heal).

**Renders 422 "predates vertical-volume storage."** Not a bug: that hour
was ingested before 2026-07-03 00Z or by a non-volume lane. Clients must
discover hours via `/api/runs?var=temperature_iso`. Rolls over naturally.

**`rw_render` exit 1 on a product.** Usually a heavy/experimental product
requested on an hour whose store lacks the grid (e.g. PFT before the
recipe existed). Check with `/api/runs?var=<product's source var>`.

**Legacy `/health` shows `ok:false`.** Expected since the 2026-07-03
retirement — its checklist includes stopped services (wxstore). The
*component* flags inside the payload are the truth: `satellite_enabled`
/ satellite `ok:true`, lightning `ok:true`. Only escalate if a component
flag goes false. The new stack's health is `/api/health` (separate).

**Satellite or lightning imagery stale.** Legacy stack's job:
`cd /opt/cafire-weather-service && docker compose logs satellite-worker
--since 1h` (or `lightning-worker`), `docker compose restart <service>`.

**Caddy edits don't take effect.** Bind-mount inode problem —
`docker compose restart caddy`.

**A long ssh job died silently.** nohup'd processes are reaped with the
ssh session scope. Rule 7: tmux, always. And watcher scripts must use
`pgrep -x` (exact name) — `pgrep -f` matches the watcher itself.

**`cargo build` or `cp` fails on a live binary.** ETXTBSY — use the
temp+mv swap from §7 (Linux), or stop the service first (Windows exe
lock needs `Stop-Process`).

**Reboot behavior:** everything important self-heals — the four systemd
units are enabled, legacy keepers are `restart: unless-stopped`, retired
legacy services are `restart: no` (deliberately can't come back). After
any reboot just run the §2 check.

---

## 9. Legacy stack: what's kept, what's dead, what to delete on ~Jul 17

`/opt/cafire-weather-service` (docker compose).

**Running, keep forever:** `caddy` (fronts everything), `api` (serves
`/api/v1/*` satellite+lightning), `satellite-worker`, `lightning-worker`.

**Stopped 2026-07-03 02:30Z, restart=no:** `static-worker` (old HRRR
gallery — was independently downloading ~170 GB/day), `pressure-volume`,
`pressure-volume-builder`, `warmer`, `wxstore`. Rollback if something was
missed: `docker compose start <service>` (takes seconds).

⚠️ Never run a bare `docker compose up -d` in that directory — it would
resurrect all five retired services. Always name the service:
`docker compose restart caddy`, `docker compose up -d satellite-worker`.

**Soak cleanup — after ~2026-07-17, and only with Drew's go** (tracked as
task #10; honors the two-week no-delete promise in the CAFire addendum):

```bash
rm -rf /opt/cafire-weather-service/data/cache/hrrr        # ~12 GB (cron already trimmed it from ~80)
rm -rf /opt/cafire-weather-service/data/volume-stores     # ~26 GB
rm -rf /opt/cafire-weather-service/data/wxstore           # ~17 GB
rm /etc/cron.hourly/cafire-legacy-cache-cap               # its target dir is gone
# optional tidy: remove the 5 retired services from docker-compose.yml
```

⚠️ Delete `data/cache/`**`hrrr`** ONLY — NOT the whole `data/cache/` folder.
`data/cache/satellite` (~45 GB) sits right beside it and is **still served**.
Also do **not** touch `data/artifacts` (~65 GB) or `data/glm` — satellite and
lightning live there. Net reclaim is **~55 GB** (not the ~123 GB first
estimated: the hourly cron shrank the legacy HRRR cache from ~80 to ~12 GB).
Until the cleanup runs, the hourly cron
`cafire-legacy-cache-cap` keeps the orphaned legacy HRRR cache from
growing (it caps `data/cache/hrrr` to the 2 newest cycle dirs).

---

## 10. Fuels

The HRRR pipeline runs `rw_fuel_fetch` once per run (gridMET-derived
dead-fuel-moisture grids, LANDFIRE layers): tries fuel-date day−1, falls
back day−2, then day−3 (gridMET publishes with lag — a day-1 miss is
normal, silence across all three days for multiple runs is not). Success
drops `.fuels-imported` in the run dir; fuel products for a run without
the flag correctly refuse rather than fake data (rule 2). One-time
yearly cost: new gridMET annual files (~300 MB) at calendar rollover —
if every fuel fetch starts failing in early January, that's why.

---

## 11. Orientation for a brand-new agent

1. Read this doc, then skim `docs/CAFIRE_HANDOFF.md` +
   `CAFIRE_HANDOFF_ADDENDUM_1.md` (the external promises) and
   `docs/FABLE_FIRE_WEATHER_HANDOFF.md` (project deep background).
2. Run the §2 health check so you know the baseline before changing
   anything.
3. The repo on Drew's machine is the source of truth; the server only
   ever receives `git archive` snapshots. Local workflow gotchas: commit
   messages containing `/`-paths can trip the PowerShell safety hook —
   use `git commit -F <file>`; `git add <dir>` does not add sibling
   files — add files explicitly or use `-A`.
4. Verification culture here: real proof renders + pixel checks over
   eyeballing; tests green before deploy (`cargo test` — two
   *pre-existing* rw-ingest `size_estimate` failures are known and not
   yours); check the public URL after every deploy.
5. **Do not touch the Hetzner server without Drew's explicit approval**
   for anything beyond read-only inspection and the runbook fixes in §8.
