# Watching a running simulation's output

Rusty Weather can follow a WRF run that is still in progress. Point the
**Live watch** window at the folder the model is writing, choose the domain
you want to see, and every finished `wrfout` frame is validated and imported
into the store as it appears, instead of waiting for the whole forecast and
importing it as one batch afterwards.

## Current scope

Rusty Weather does not launch, configure, or preprocess the run it watches.

- There is **no preprocessing frontend** in this repository. Producing
  `wrfinput_dNN`/`wrfbdy_d01` from a source dataset — the WPS-style
  source-to-initialization step — is a separate effort and is not included.
- There is **no forecast-runtime integration**. Nothing here starts a model,
  binds a GPU, resolves a Python/CUDA environment, or ships a runtime bundle.
- Nothing in the app stubs, shims, or simulates those missing pieces. There is
  no disabled "Run simulation" button and no stage that reports a result it did
  not obtain.

You start the model yourself, outside the app, by whatever means you normally
would. Rusty Weather is a reader of the directory it produces.

That also means the watch is **producer-independent** by construction. Stock
WRF and any other model that writes WRF-shaped output are equally valid; the
watch has no notion of which one wrote the file, and asks the file itself.

## What the watch requires of the output

- One time record per file: `frames_per_outfile = 1`. A growing multi-time
  file is not safe to import incrementally, and is rejected with that reason.
  Such a file can still be opened normally once the run has finished.
- Files named `wrfout_dNN_...`, up to four directory levels below the watched
  root. The configured domain token (`d01` … `d99`) selects exactly one domain;
  other domains in the same folder are ignored, not merged.
- A readable model initialization time and a valid time on the configured
  output cadence. A frame off the cadence is rejected rather than rounded into
  a neighbouring slot.

## How a file is decided to be finished

A file that is still being written is a valid NetCDF prefix at best. The watch
uses two independent kinds of evidence.

**The stability window.** `rw-sim`'s `StableWrfoutWatcher` returns a path only
after its size and mtime have held still across several polls and the last
write is older than a minimum age. If the file later changes it becomes
eligible again, so a producer that rewinds or rewrites a frame is handled
deterministically instead of pinning a stale one.

**A completion attribute, if the producer publishes one.** Stock WRF does not,
so this is optional and unset by default. A producer that writes an integer
"this file is closed" WRF global attribute can have it honoured: name the
attribute in the watch settings and choose a mode.

| Mode | Behaviour |
| --- | --- |
| `Auto` (default) | Honour the named attribute when it is present and require it to equal 1; accept the file on the stability window alone when it is absent. |
| `Stock WRF` | Ignore any completion attribute; the stability window and the metadata readback are the whole contract. |
| `Require completion attribute` | The named attribute must be present and equal 1. A file without it is never accepted. |

No attribute name is hardcoded, because no particular model runner is assumed.
Selecting the strict mode without naming an attribute is refused at start.

## The case contract

The first accepted frame fixes the case identity: the canonical watched root,
the domain token, the model initialization time, the grid shape and spacing,
a SHA-256 of the `XLAT`/`XLONG` coordinate grid, and a normalized projection
fingerprint (Lambert conformal, polar stereographic, Mercator, unrotated
lat/lon, or none). Every later frame must reproduce that identity exactly.

A frame that fails is classified before it is discarded:

- **transient** — not readable yet. The publication is withdrawn back to the
  stability window, and the file must satisfy it again from scratch.
- **rejected** — structurally wrong for this case. The file is quarantined
  until its signature changes, and the reason stays visible in the status row.

Two frames may never claim the same cadence slot, and a path that was already
published may not change its physical time. Both are rejected outright.

## What happens to an accepted frame

Accepted frames queue in front of the app's single WRF processor, which runs
them one at a time under the chosen product profile (`Quick look + soundings`
or `Full diagnostics`). Each frame is appended to a stable per-case run through
rw-store's atomic hour writer — the run name is derived from the case digest,
the initialization time, and the processing profile, so re-watching the same
case with the same profile continues the same run rather than forking it.

Queued work is keyed by `(canonical path, size, mtime)`, with Windows
extended-length, UNC, and case aliases normalized to one identity. A genuine
rewrite is therefore imported again, while a path that merely reappears in a
later scan is not imported twice.

With **Follow each newly processed valid time** enabled, the viewer advances to
each newly imported frame, but only forward in physical time — a late-arriving
earlier frame is imported without yanking the display backwards. Stopping the
watch cancels the queue; the frame already being processed is allowed to
finish, and a new session cannot start until it does.
