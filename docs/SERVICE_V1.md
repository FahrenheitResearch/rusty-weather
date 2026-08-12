# Rusty Weather service v1

Rusty Weather is a permissive, self-hosted weather data engine for public
forecast models and private WRF-compatible runs. Its primary differentiator is
native exact-time temporal and diurnal analytics over stored model fields.

The service is intentionally brand-neutral. Domain-specific applications can
embed the query library or build their own presentation layer over the same
versioned API.

## Components

- `rw-store`: atomic, validated, mmap-oriented hourly/exact-time storage.
- `rw-ingest`: provider acquisition, extraction, derivation, and publication.
- `rw-scheduler`: optional durable cycle discovery, bounded ingest execution,
  restart recovery, and scheduler-owned retention.
- `rw-query`: transport-independent catalog, point/profile/window, and
  temporal/spatial analytics.
- `rw-server`: bounded HTTP host, authentication, observability, jobs, and
  immutable artifacts.

## Stable HTTP surface

- `GET /v1/health/live`
- `GET /v1/health/ready`
- `GET /v1/version`
- `GET /v1/models`
- `GET /v1/models/{model}/runs`
- `GET /v1/models/{model}/runs/{run}`
- `GET /v1/models/{model}/runs/{run}/variables`
- `GET /v1/point`
- `POST /v1/points`
- `POST /v1/profile`
- `POST /v1/window`
- `POST /v1/analytics/spatial-series`
- `POST /v1/analytics/temporal-grid`
- `POST /v1/jobs/temporal-grid`
- `GET /v1/jobs/{id}`
- `DELETE /v1/jobs/{id}`
- `GET /v1/artifacts/{hash}/{file}`
- `GET /v1/openapi.json`
- `GET /metrics`

Every request names an explicit run. The durable scheduler exposes safe
latest/latest-available/latest-covering selection primitives, but the v1 HTTP
surface does not silently resolve mutable aliases. Applications should resolve
a run from the catalog once, then use that immutable run ID for the query.

## Safe defaults

- listen on `127.0.0.1`;
- reject unauthenticated non-loopback binds unless an explicit unsafe override
  is supplied;
- disable CORS until exact origins are configured;
- load secrets from environment variables or token files, never CLI values;
- mount stores read-only;
- bound request bytes, variables, points, cells, output values, concurrency,
  job count, cache size, and deadlines;
- keep filesystem paths and subprocess output out of public responses;
- use RFC 9457 `application/problem+json` errors with stable codes and request
  IDs;
- stop accepting HTTP work through Axum's graceful shutdown path. Asynchronous
  jobs have per-job cancellation tokens, and `DELETE /v1/jobs/{id}` requests
  cooperative cancellation at tile/timestep checkpoints. Work is not forcibly
  preempted between checkpoints; durable queued/running records left by process
  termination are marked interrupted during the next startup recovery.

## Capability reporting

Catalog presence, remote fetch support, ingest support, stored availability,
query support, rendering, temporal semantics, and verification level are
reported independently. A catalog entry is not advertised as operationally
verified merely because a URL template exists.

Local valid stores are queryable even when their model identifier is not built
into the registry. This keeps private WRF, ArWen, and other compatible output
first-class.
