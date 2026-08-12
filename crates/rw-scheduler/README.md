# rw-scheduler

`rw-scheduler` is the brand-neutral operational host for keeping an `rw-store`
populated from the model ingest registry. It provides durable job state,
restart recovery, bounded model/hour concurrency, exact-time completeness
checks, retry backoff, disk-reserve admission, and scheduler-owned retention.

## Configuration

Configuration is TOML by default (`.json` selects JSON). Roots must be
absolute, distinct, and non-nested. Models are always an explicit allowlist;
the sole token `all_ready` expands from `rw_ingest::model_ingest_capabilities`
at runtime.

```toml
store_root = "/var/lib/rw/store"
cache_root = "/var/cache/rw/ingest"
state_root = "/var/lib/rw/scheduler"
models = ["hrrr", "gfs", "rtma"] # or exactly ["all_ready"]
profile = "auto"
# Enable only when cache_root has an external size/age retention policy.
use_cache = false
verify = true
rollback_days = 2
poll_seconds = 300
# Hard budget per model; each metadata-only HEAD/range probe is bounded too.
discovery_timeout_seconds = 30
discovery_probe_timeout_seconds = 6
max_concurrent_jobs = 2
max_concurrent_hours = 2
max_queued_jobs = 128
free_space_reserve_bytes = 10737418240

[model_profiles]
hrrr = "view"

[retry]
max_attempts = 5
initial_backoff_seconds = 60
max_backoff_seconds = 3600
jitter_percent = 20

[retention]
enabled = false
dry_run = true
keep_latest_per_model = 3
```

The download cache is disabled by default because the scheduler does not yet
prune it. Enabling `use_cache` can reduce retry and restart downloads, but an
operator must independently bound `cache_root`; otherwise a long-running
daemon can eventually consume its free-space reserve and stop admitting work.

`profile = "auto"` selects the narrow `analysis` pack for analysis feeds, the
complete direct-field `surface` pack for surface-only forecasts, disables
derived/heavy diagnostics for products whose typed capability forbids them,
and otherwise selects `view`. Explicit incompatible overrides fail config
validation before any network or store work.

## Commands

```text
rw-scheduler --config /etc/rw/scheduler.toml plan
rw-scheduler --config /etc/rw/scheduler.toml discover
rw-scheduler --config /etc/rw/scheduler.toml run-once
rw-scheduler --config /etc/rw/scheduler.toml daemon
rw-scheduler --config /etc/rw/scheduler.toml status
```

`plan` is offline: it reports the newest registry cycle not later than the
current UTC clock without claiming it exists upstream. `discover` is a
metadata-only preflight: it uses bounded HEAD or one-byte range requests and
does not create roots, state, cache, or store payloads. `run-once` and `daemon`
use the same bounded provider discovery, including configured rollback, pin the
selected provider in durable state, and resume existing jobs by exact valid timestamp.
Ctrl-C stops admission and is observed by ingest at its normal stage
boundaries. An interrupted attempt returns to `queued` without consuming the
retry budget.

Only one scheduler may operate a `state_root` at a time. A process-scoped OS
lease rejects a second daemon/run-once invocation before discovery or state
mutation; the lease is released automatically if the process exits or dies.

Retention is disabled and dry-run by default. It derives candidates only from
durable scheduler records, requires a matching ownership marker, revalidates
paths, and obtains the run writer lock immediately before mutation. On Windows,
the OS can deny renaming a directory while its lock file is open; in that case
the executable removes payloads while holding the lock and leaves a tiny
non-queryable ownership shell marked `.rw-scheduler-purged.json`. Applied
retention also removes the matching terminal scheduler-state record, so bounded
run retention does not leave state files accumulating indefinitely.

## Intentional v1 boundaries

- An in-flight provider availability HTTP probe is not cancelled immediately;
  it returns at the configured per-probe timeout (six seconds by default),
  after which shutdown is observed before another probe starts.
- The executable has no service-alias state source. The retention library can
  protect caller-supplied aliases, while the executable protects active jobs
  and the configured newest-run count.
- Provider integration tests are deliberately absent. Scheduler tests use an
  injected local discovery implementation and never contact the network.
