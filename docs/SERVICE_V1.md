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
- `rw-query`: transport-independent catalog, point/profile/native and
  geographic window, and
  temporal/spatial analytics.
- `rw-server`: bounded HTTP host, authentication, observability, jobs, and
  immutable artifacts.

## Stable HTTP surface

- `GET /v1/health/live`
- `GET /v1/health/ready`
- `GET /v1/version`
- `GET /v1/models`
- `GET /v1/models/{model}/runs`
- `GET /v1/models/{model}/runs/latest`
- `GET /v1/models/{model}/runs/{run}`
- `GET /v1/models/{model}/runs/{run}/variables`
- `GET /v1/point`
- `POST /v1/points`
- `POST /v1/profile`
- `POST /v1/window`
- `POST /v1/geographic-window`
- `POST /v1/analytics/spatial-series`
- `POST /v1/analytics/temporal-grid`
- `POST /v1/jobs/temporal-grid`
- `GET /v1/jobs/{id}`
- `DELETE /v1/jobs/{id}`
- `GET /v1/artifacts/{hash}/{file}`
- `POST /v1/community/objects/resolve` (feature-gated Phase 1)
- `GET /v1/community/objects/{sha256}` (feature-gated Phase 1)
- `POST /v1/community/artifacts` (typed, owner-bound explicit publication)
- `POST /v1/community/artifacts/{sha256}/revoke`
- `POST /v1/community/cases` (explicit publication; separately gated)
- `GET /v1/community/cases/{case_id}`
- `POST /v1/community/cases/{case_id}/revoke`
- `GET /v1/federation/origins` (feature-gated signed public-origin catalog)
- `GET /v1/federation/origins/{origin_id}` (feature-gated signed descriptor)
- `GET /v1/openapi.json`
- `GET /metrics`

Every data request names an explicit run. The one explicit mutable pointer,
`GET /v1/models/{model}/runs/latest`, selects only from the authenticated
visible catalog by physical cycle origin, uses the canonical run ID only as a
deterministic tie-break, and is returned with `Cache-Control: no-store,
private`. Applications should resolve that pointer once, then bind subsequent
requests to the returned immutable run and snapshot identities. No data query
silently resolves an alias.

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

Stored `surface2d` and `pressure3d` capabilities advertise
`geographic_window=true`. A geographic request must bind the exact catalogued
snapshot and grid hashes. Its versioned response contains a minimal native
rectangular envelope, cropped lat/lon arrays, exact projection metadata, and a
mask that prevents curvilinear envelope cells outside the requested bbox from
being plotted as selected data. Antimeridian-crossing eastward arcs and
explicit, unreduced pressure levels are first-class.

Local valid stores are queryable even when their model identifier is not built
into the registry. This keeps private WRF, ArWen, and other compatible output
first-class.

## Community Cache Phase 1

Community Cache is disabled by default and adds no peer transport. It admits
only the closed query-object categories in `rw-community-protocol`: sounding
profiles with exact-time surface anchors, point series, native surface or
selected-pressure windows, geographically selected surface or pressure
windows, temporal/diurnal grids, and deliberately published case artifacts.
There is no arbitrary-file, directory, raw-WRF, or full-run endpoint.

Case artifacts are created only through a second off-by-default gate and a
closed typed payload union: annotation, scalar table, fixed-coordinate overlay,
or bounded PNG/WebP image. The authenticated principal must match the owner
hash embedded in canonical request identity. Every client-authored artifact is
an explicit `PrivateWrf`, `PrivateArwen`, or `UserProvided` owner publication;
the endpoint rejects `PublicProvider` claims rather than origin-signing
client-asserted upstream identity. Confirmed rights, attribution/license fields,
source snapshot/grid identity, and bounded retention are mandatory. Durable
audit records and rights-withdrawal tombstones prevent
another principal from publishing/revoking on the owner's behalf or reviving a
withdrawn identity. Complete `.rws` generation inventory exists only as a
disabled protocol contract; no raw file/wrfout/full-run HTTP upload exists.

Every accepted object is SHA-256 content-addressed and described by an
origin-signed, self-contained `rw.community.object.v1` manifest binding the
model, immutable run and snapshot, grid, variables, exact valid time/window,
recipe, source provenance, sizes, compression, and attribution. The server
checks the same contract before accepting an R2-compatible hot object or HTTPS
origin response. Local CAS publication is atomic and bounded by byte/object
limits with least-recently-used eviction.

Phase 1 resolution is strictly local CAS, optional R2-compatible hot storage,
then the authoritative Hetzner Rusty Weather dynamic HTTPS origin. Hetzner
signs manifests and is never reached through TURN. A hot-store miss or outage
is not fatal. Successful origin results are stored locally and popular objects
may be promoted under a hit window and global
monthly promotion ceiling. The global kill switch disables promotion and case
publication while preserving a signed normal-origin response. Final origin
disk/concurrency/retention values remain conservative configuration inputs
until the deployment node is audited.

The wire contract and threat model are in `COMMUNITY_CACHE_PROTOCOL.md` and
`COMMUNITY_CACHE_THREAT_MODEL.md`. Relay-mediated peer-assisted transfers come
afterward; direct connectivity is permanently out of scope. Phase 1 contains
no STUN, direct ICE, candidate gathering, peer-address exchange, or direct
fallback code.

## Public-origin federation

Federation is a distinct conventional-HTTPS path for opt-in universities,
labs, and public service nodes whose origin addresses are intentionally public.
It never publishes an ordinary BowEcho or Community Cache client's address.
The authenticated federation endpoints return an authority-signed catalog of
origin-signed `rw.federation.origin.v1` descriptors. Each descriptor binds its
HTTPS root, Ed25519 descriptor/object keys, exact model/product/query
capabilities, geographic coverage, retention, build/schema versions, expiry,
same-origin health path, policy links, replication consent, and quotas.

Origins cannot self-register. The service reads bounded signed descriptor
files provisioned by the operator and requires an exact out-of-band origin-ID
and descriptor-key allowlist. Revoked origins and keys fail closed. The
transport-neutral verifier rejects unknown schemas/fields, unsafe or local URL
forms, untrusted keys, expired material, duplicate identities, and malicious
capability counts. See `FEDERATION.md` for the complete trust and selection
contract.
