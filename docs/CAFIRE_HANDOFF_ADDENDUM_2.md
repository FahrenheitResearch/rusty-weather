# CAFire Weather — Addendum 2: Integration Tuning (July 2026)

**Companion to `CAFIRE_HANDOFF.md` + `CAFIRE_HANDOFF_ADDENDUM_1.md`** — give all
three to your coding agent. Same conventions: build against `{BASE}`, never show
the backend hostname to users, and `{BASE}/lab` is the working reference client.

**Nothing here is breaking.** The API contract is unchanged. These are
client-side refinements from a review of the live integration, in three
buckets: **(A)** product improvements users will see, **(B)** being a good
neighbor to a shared single-box backend, **(C)** cheap cache-sharing + cleanup.

First, a few things **we** changed on our side that unlock or ease the items
below:

- **`/outputs/*.webp` now serve `Cache-Control: public, max-age=31536000,
  immutable`.** Rendered frames are job-id-keyed and never change, so once a
  frame loads it's held by the browser and the CDN. This is what makes smooth,
  no-refetch loops possible (see A2).
- **GFS now runs to 16 days (F384)**, not 8 (see A3).
- Surface temperature maps default to **°F**; daily cards and meteograms now
  **auto-mark the extended range (~day 8+) as lower-confidence**; a **`fuel-run`
  alias** exists so fuel products never 404 during the daily gridMET import.

---

## A. Product improvements (user-facing)

### A1. Default the map hour to F0 (current), not F6
The reference Lab defaults its hour ladder to the 7th entry, which lands on
**F6** for hourly HRRR — so users open to a 6-hour forecast instead of current
conditions. Default the map/meteogram hour selector to **F0** for HRRR and GFS.
*Exception:* NBM has no F0–F5 (its earliest step is F6), so F6 is correct for
NBM only.

### A2. Make loops replay from cache instead of re-rendering every frame
If the loop issues a fresh `POST /api/render` (+ `/api/jobs` poll) for each hour
on every cycle, it will feel like it reloads on each pass even when those are
cache hits — the per-frame round-trip is the lag. Instead:
1. Resolve each frame's image URL once (a prefetch pass over the hours is fine —
   it warms our shared cache too).
2. Keep the list of resolved `/outputs/...webp` URLs.
3. On loop/step, just swap the `<img>` to the already-resolved URL. With the new
   immutable cache headers those images are already in the browser cache, so
   replay is instant and touches neither our API nor the network.

### A3. Surface the 16-day GFS (and ~11-day NBM) outlooks
GFS now serves to **F384**. If your GFS outlook is still framed/capped at 8 days
(F192), extend it. Deterministic skill past ~day 7–10 is low — if you render our
daily cards/meteograms you get an automatic **"EXTENDED RANGE — LOWER
CONFIDENCE"** marker past day 8; if you build your own graphics, mark the far
end similarly so day 14 doesn't read as confident as day 2.

---

## B. Be a good neighbor (shared single-box backend)

Our backend is **one server** that also serves satellite + lightning. It's
comfortably handling current traffic, but a few client patterns would amplify
load during an incident or with many idle tabs. All are small changes.

### B1. Back off failing background pollers
The status/health, runs, and fires pollers run on fixed timers. On error they
should **back off exponentially (with jitter)** and resume normal cadence on the
first success — otherwise a brief outage gets hammered at full rate and every
client stampedes together on recovery. Also: a status dot doesn't need a
6-second poll — **30–60 s is plenty**.

### B2. Pause polling when the tab is hidden
Add a `visibilitychange` handler that pauses (or greatly slows) all pollers and
**auto-stops loop/play mode when `document.hidden`**. Without it, abandoned open
tabs poll and re-render indefinitely, so load scales with *cumulative tabs*
instead of *active users*.

### B3. Don't prefetch on cache-busting requests
The neighbor-hour prefetch is great for shared **presets** — it warms a cache
everyone benefits from. But for requests that are unique per user — a **drawn
box, a per-fire domain, °C, or non-default label density** — each prefetched
neighbor is a fresh cold render against a small render pool. Skip the prefetch
(or reduce it to ±1) whenever the current request isn't a shared preset.

### B4. Cap and cancel render polling
Give the job-poll loop a sane interval (**~700 ms**), a **max duration/attempt
cap**, and **abort superseded polls** (`AbortController`) so abandoned renders
stop client-side. Debounce rapid controls (arrow-key hour stepping, segmented
toggles) ~150–250 ms and cancel superseded in-flight renders rather than
queuing them.

---

## C. Cheap cache-sharing + cleanup

### C1. Round point coordinates
For meteogram/daily point requests, round lat/lon to **3–4 decimals** (~100 m,
no visible difference — it's what the cross-section/sounding endpoints already
do). Sub-pixel click drift otherwise produces a unique render per pixel;
rounding collapses them into shared cache/edge hits.

### C2. Hide the PyroCb / ECAPE tab for now
Those products proxy to a heavy-compute node that is **currently offline**, so
they return **502** for live users. Hide the tab until we tell you it's back
online. (Re-enabling it is on our side — just don't advertise it meanwhile.)

### C3. Cycle-gate the extended day-window products
The **24–48 h and 0–48 h** window products only exist on the **extended cycles
(00/06/12/18Z)**. On an off-cycle run the picker can offer them and get a `422`.
Gate those options to 00/06/12/18Z runs — or, generally, drive any hour/product
picker from `GET /api/runs?var=<source variable>`, which lists only the
runs/hours that actually carry it (as the Lab already does for the vertical
products). That makes "offered but errors" unreachable.

---

## Priorities, if you only do a few

1. **A1** (F0 default) and **A2** (loop caching) — the two most visible wins.
2. **B1** + **B2** (poller backoff + tab-visibility) — the two that most protect
   the shared backend for everyone.
3. **A3** (surface the 16-day GFS) — you already have the data; it's just UI.

Everything else is polish. None of it changes any endpoint, alias, or response
shape — it's all how the client calls what's already there.
