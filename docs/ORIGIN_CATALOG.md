# Bounded Hetzner origin publication

Rusty Weather can make its conventional HTTPS API expose only generations
authorized by a selected publication source. This gate is separate from
federation and Community Cache, and it is off by default. Hetzner uses the
fresh scheduler catalog. A replication-only university/lab origin may instead
serve only durable engine-authorized generations without running the operational
HRRR/GFS/NBM scheduler. `union` explicitly requires both authorities.

The scheduler atomically writes `.rw-origin-catalog.json` directly under the
configured `server.store_root`. The path is not configurable independently,
which prevents the server and scheduler from silently naming different stores.
The initial contract contains four lanes: newest queryable HRRR, newest complete
extended HRRR, newest GFS, and newest complete surface NBM. Each lane may name
an active generation and exactly one older rollback generation.

When `origin_catalog.enabled = true`, the server:

- rejects a symlinked store root or catalog and any non-regular catalog file;
- reads at most 1 MiB and accepts only the closed
  `rw-scheduler.origin-catalog.v1` schema;
- enforces a nonzero configured publication age of at most 24 hours;
- reopens every referenced immutable rw-store snapshot and verifies its model,
  run, cycle origin, exact valid-time inventory, grid, hours, and variables;
- atomically replaces its in-memory allow-set only after the complete new
  document passes validation; and
- applies that allow-set to model/run listings and every direct query snapshot,
  so an unlisted run is indistinguishable from a nonexistent run; and
- resolves `GET /v1/models/{model}/latest-run` only within that same allow-set,
  ordered by physical cycle origin with a deterministic run-ID tie-break and a
  private, no-store response.

`origin_catalog.publication_sources` selects the authority:

- `scheduler`: a fresh scheduler active/previous view is mandatory;
- `replication`: a healthy replication engine is mandatory, and only exact
  engine-authorized durable publications are visible; or
- `union`: both are mandatory, with model/run namespace collisions failing the
  entire publication view closed so a replicated run cannot shadow a scheduler
  lane.

None of these modes broad-scans the raw rw-store. In replication mode, stray,
crash-orphaned, revoked, tombstoned, or snapshot-mismatched directories remain
query-invisible. Scheduler-owned directories and replicated publications have
separate ownership/retention state and cannot be adopted by the other authority.

The small catalog document is polled on the configured bounded cadence. Every
poll deeply revalidates the complete retained set before an atomic view swap,
so deletion or corruption cannot leave a stale listing even when the catalog
bytes did not change. Every actual data query also reopens and validates its
selected snapshot.

## Startup and failure behavior

An enabled server with no catalog starts in `pending`, returns no published
models through a generic service-unavailable response, denies direct run
resolution, and fails readiness. It never falls back to scanning all
directories. After the first valid catalog has been accepted,
a missing, stale, malformed, replaced-with-a-symlink, or storage-inconsistent
catalog changes the state to `unavailable`, clears the complete published view,
and fails readiness. Restoring a valid scheduler document recovers without a
server restart.

The latest-run pointer follows the same failure boundary: an empty, stale,
unavailable, or internally inconsistent publication view cannot fall back to a
raw store scan or reveal an unlisted run. Clients retain the returned run and
snapshot identities for all subsequent immutable queries.

The authenticated origin-catalog status response is deliberately coarse. It
reports only enabled/ready state, timestamps, and aggregate model/run counts;
it never exposes filesystem paths, run IDs, model IDs, peer data, or validation
errors.

Example configuration:

```toml
[origin_catalog]
enabled = true
publication_sources = "scheduler"
refresh_seconds = 5
max_age_seconds = 7200
```

For `scheduler` or `union`, enable this only when the scheduler's host-capacity
audit is complete and both processes use the same real `store_root`. The
scheduler remains responsible for retaining active and previous operational
generations. Replication mode separately requires the advanced replication
security/capacity gates and its isolated durable control root. The server does
not delete or repair store data while serving queries.
