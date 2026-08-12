# Rusty Weather service operations

## Health and diagnostics

`GET /v1/health/live` answers when the process can serve HTTP. `GET
/v1/health/ready` additionally checks the configured store and should gate
traffic. Both are deliberately unauthenticated and return no secret paths.

Use the built-in probe from the same network namespace as the service:

    rw-server --config /etc/rusty-weather/rusty-weather.toml healthcheck --timeout-seconds 5

For HTTP monitoring:

    curl --fail --silent --show-error http://127.0.0.1:8788/v1/health/ready

Run `doctor` after configuration, credential, permission, binary, or store
changes. It validates configuration safety, token loading, directory isolation,
and the visible store catalog. `GET /metrics` exposes bounded OpenMetrics data;
it requires a bearer token when `auth.protect_metrics = true`.

## Authentication and token rotation

Token files contain one token per line; blank lines and lines beginning with
`#` are ignored. Tokens must contain at least 32 bytes and no control
characters. On Unix, the service rejects files readable or writable by group
or other users.

To rotate without an unauthenticated interval:

1. Add the new token while retaining the old token.
2. Atomically replace the token file with the same owner and permissions.
3. Restart the service and verify a request with the new token.
4. Remove the old token, atomically replace the file again, and restart.

Token contents are hashed in memory. They are still bearer credentials: keep
them out of command histories, URLs, diagnostics, and source control.

## Backup

Back up these classes separately:

1. Configuration and service templates. Store secrets in a secrets manager or
   encrypted backup, not beside public artifacts.
2. The model store, including every `run.json`, `grid.rwg`, and `.rws` file.
3. Artifact outputs that cannot be cheaply regenerated.
4. The exact release archive, checksum, SBOM, and source revision used.

Completed run directories are immutable from the query service's perspective,
but an ingest publisher may be writing a staging tree or atomically publishing
a manifest. Use a filesystem snapshot, stop the writer, or copy only runs that
have a complete manifest. The simplest conservative procedure is:

1. stop ingest/scheduler writers;
2. stop `rw-server` or remove it from traffic;
3. take a same-filesystem or storage-provider snapshot;
4. restart the service, then the writers;
5. validate a sample of restored runs with `rws validate --deep`.

Backups are not complete until a restore into a separate directory passes
`doctor`, deep store validation, and representative API queries. Do not assume
an object-store sync is transactionally consistent across a run directory.

## Upgrade and rollback

1. Read release notes and format/API compatibility notes.
2. Download the platform service archive, `.sha256`, and CycloneDX SBOM over an
   authenticated channel. Verify all three before extraction.
3. Back up configuration, secrets, and store manifests.
4. Extract into a new versioned directory; do not overwrite the running binary.
5. Run the new binary's `doctor` against the production configuration.
6. Stop the old service, switch the binary path or symlink, and start the new
   service.
7. Require readiness and representative authenticated queries before restoring
   normal traffic.
8. Retain the previous binary and configuration until the observation window
   passes.

If validation fails, stop the new service and restore the previous binary and
configuration. Do not downgrade or rewrite store data unless the relevant
format documentation explicitly permits it. Preserve the failing logs and
request IDs for diagnosis.

## Capacity and retention

Keep the store and artifacts on separate quotas. Alert before either filesystem
fills; atomic publication still needs room for a complete replacement. Bound
each asynchronous artifact with limits.job_result_bytes, bound durable history
with limits.job_history_records and limits.job_retention_seconds, size reader/response
caches within the service's memory budget, and keep heavy query concurrency low
enough that admission remains responsive. Tighten public request limits before
adding process memory limits.

Retention should delete only complete, unreferenced run directories and
immutable artifacts. Coordinate deletions with the ingest/scheduler and active
queries; never remove a broad store root recursively from an interpolated or
unresolved path.

## Incident checklist

- Capture the UTC time, version, request ID, readiness result, and bounded log
  excerpt.
- Check disk space, file permissions, manifest validity, queue/admission
  metrics, and upstream freshness.
- If data may be corrupt, remove the instance from traffic and preserve the
  store for investigation before attempting repair.
- Rotate tokens after suspected disclosure and invalidate reverse-proxy caches.
- Report security issues using `SECURITY.md`; do not open a public issue with
  credentials, private URLs, or user data.

## Community Cache operations

Community Cache is off by default. Before enabling it, verify the signing-key
file permissions, pinned public key, separate cache filesystem quota,
R2-compatible gateway token, HTTPS origin fallback, and all configured monthly
thresholds. Never enable case publication merely to enable response caching.
The server rejects `community.enabled = true` until
`community.capacity_audit_completed = true` records that the origin-host disk
and concurrency values came from the completed capacity audit.
The Phase 1 origin is the authoritative Hetzner Rusty Weather HTTPS service and
manifest signer; it is never a TURN destination. Operational lookup order is
local CAS, R2, then Hetzner dynamic origin, followed by optional R2 promotion.

During a cost, abuse, privacy, or hot-provider incident set
`RW_COMMUNITY_KILL_SWITCH=true` and restart the service. This stops case
publication and hot-object promotion. It intentionally does not disable signed
normal HTTPS origin resolution, so BowEcho remains useful when community assist
is unavailable. Disable the whole feature with `RW_COMMUNITY_ENABLED=false`
only when the Community Cache HTTP surface itself must be closed.

Monitor the separate `community.root` filesystem and the configured per-token
download/upload/concurrency quotas. The CAS evicts least-recently-used request
identities within its byte/object limits; immutable objects referenced by
multiple requests are removed only after the last local reference is evicted.
The global `promoted_bytes_per_month` ceiling pauses further hot promotion but
does not turn an otherwise successful query into an error.
Monthly transfer and promotion accounting is atomically persisted in
`community.root/accounting.json` and survives service restarts. Case manifests
are separately bounded by `community.cases.maximum_cases` and
`community.cases.storage_bytes`; expired cases are removed during startup and
on case access/publication. Requested retention cannot exceed
`community.cases.default_retention_seconds`.

Treat any signature, embedded-request hash, object hash, decoded-size,
decompression-ratio, schema, expiry, or ECMWF-notice failure as untrusted
content. Preserve the bounded request ID and coarse failure code; do not log
object bodies, signatures, bearer tokens, private source paths, or future relay
metadata. A hot-store failure should fall through to origin. Repeated hot-store
failure is an operator alert, not a reason to weaken validation.

Private WRF and ArWen content is non-shareable by default. A case/object may be
published only after the owner deliberately supplies both explicit-publication
and redistribution-rights confirmations. Passive searches, local cache fills,
and opening a run are never publication events. Relay-mediated peer-assisted
transfers are a later phase; direct-IP sharing is not a recovery mode.

## Scheduler operations

The scheduler is a separate optional writer. These commands are safe for
routine inspection:

    rw-scheduler --config /etc/rusty-weather/rusty-weather-scheduler.toml plan
    rw-scheduler --config /etc/rusty-weather/rusty-weather-scheduler.toml status

`plan` is registry/clock based and deliberately does not claim that a cycle is
available upstream. `status` reads durable local job state without network
access. Use `run-once` for supervised one-shot ingest and `daemon` for the
service loop. Capture the JSON reports in the service journal and alert on
repeated retries, stale succeeded cycles, queue saturation, and the configured
free-space reserve gate.

Stop the scheduler before a store snapshot, migration, or binary downgrade, and
allow its cooperative shutdown timeout to expire before forcing termination.
Interrupted running jobs are recovered from durable state on restart. The API
may continue serving already published immutable runs while the scheduler is
stopped.

Scheduler retention considers only scheduler-owned run IDs and revalidates the
root, ownership marker, path, and writer lock immediately before mutation. It
is disabled and dry-run by default. To preview candidates, set
`retention.enabled = true` while leaving `retention.dry_run = true`, execute
`run-once`, and inspect its JSON report. Never set `dry_run = false` merely to
reclaim space during an incident: first protect externally referenced runs and
confirm that no second writer or mutable alias can still reach the candidates.
