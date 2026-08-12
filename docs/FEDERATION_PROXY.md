# Authoritative Public-Origin Failover

Status: implemented, default-off authority and one-hop origin data paths. The
isolated verification/transport core is integrated with rw-server routes,
canonical Community signing/staging, durable quotas, and a production-shaped
HTTP consumer test. Live multi-host and packaged deployment gates remain
explicitly unsatisfied below.

This path is conventional server-to-server HTTPS. It is not Community Cache,
TURN, peer discovery, or direct BowEcho-to-origin access. University, lab, and
other public-origin URLs are deliberately public in signed federation
descriptors. Ordinary BowEcho participant addresses never enter this module.

## Delivery position

The authority tries federation only as an explicitly requested public-origin
fallback after its normal operational path:

1. authority local CAS;
2. authority R2/hot immutable object;
3. authority local published-store computation;
4. approved public-origin federation proxy;
5. honest unavailable response.

Federation failure never prevents local/R2/authority operation. Historical
Community Cache lookup remains a separate relay-only path.

## Routes

### Client-facing authority route

`POST /v1/federation/objects/resolve`

- Uses the normal authenticated BowEcho/Rusty principal.
- Rejects any `X-Rusty-Federation-Hop` header.
- Accepts `rw.federation.proxy-resolve.v1`, containing one exact canonical
  `ShareRequest` and an optional approved public `preferred_origin_id`.
- Returns the ordinary `ResolveObjectResponse` contract, authority-signed with
  the current Community object signing key.
- Never returns the selected upstream URL, IP address, DNS answer, bearer
  token, or transport diagnostic. The selected public origin ID is retained
  only for bounded server metrics/audit.

The client retrieves the returned immutable hash from the authority's existing
`GET /v1/community/objects/{sha256}` route. BowEcho never receives credentials
for a third-party origin.

### Origin-only one-hop route

`POST /v1/federation/objects/resolve-local`

- Uses an origin-scoped bearer credential provisioned only on the calling
  authority.
- Requires exactly `X-Rusty-Federation-Hop: 1`.
- Accepts the ordinary exact `ResolveObjectRequest`.
- May consult only that node's local CAS, configured R2, and locally published
  store. It must not call the normal remote origin connector, federation proxy,
  or Community relay.
- Returns an ordinary signed `ResolveObjectResponse`; object bytes remain on
  the same origin's dedicated authenticated
  `GET /v1/federation/objects/{sha256}` route.

The dedicated local-only boundary makes A-to-B and B-to-A configurations
terminate after one upstream attempt. No request may forward or increment a
hop value, and the outer proxy route rejects a hop header on re-entry.

## Candidate selection

The rw-server adapter obtains candidates only from `FederationService`, whose
catalog and descriptors have already passed the operator allowlist and
Ed25519 signature chain. It derives the query capability from the canonical
request, enumerates only advertised products for that model/query, asks the
health service to exclude quarantined origins, and rechecks the exact pressure
levels, response bound, and geographic coverage in the proxy core.

Candidates are ordered by consecutive failures and then public origin ID.
An origin hint only moves an already eligible candidate to the front. Unknown,
expired, revoked, unhealthy, capability-incompatible, self-referential, or
duplicate-root candidates are rejected before network I/O. Attempts are
strictly bounded.

The current `rw.federation.origin.v1` descriptor has no signed assertion for
unauthenticated object delivery. Therefore every data-capable proxy origin
requires a separate server-held data bearer-token file. Unauthenticated public
data access remains disabled until a future signed schema explicitly defines
that assertion and its policy.

## HTTPS and credential boundary

Every upstream call uses a fresh agent and DNS lookup with:

- HTTPS only, WebPKI verification, and the original TLS hostname preserved;
- canonical lowercase DNS roots with no userinfo, query, fragment, encoded
  path segment, redirect, proxy, IP literal, private name, or non-default port;
- no redirect following and rejection of `Location` even on a 2xx response;
- a bounded DNS answer set rejected in full if any address is non-global;
- one pinned approved socket per connection and no idle connection reuse;
- independent DNS/connect/send/receive/global timeouts;
- exact same-origin resolve and object paths;
- bounded status, content type, content length, and streamed body size.

Each origin has a separate credential object bound to the exact signed origin
ID and HTTPS root. Health credentials and data credentials are separate. A
credential is never selected by URL, supplied by the client, forwarded after a
redirect, serialized, logged, or included in an error.
At startup, domain-separated secret digests prove that ordinary API,
local-resolve, every origin data, and every origin health credential value are
pairwise disjoint—even when two different files contain the same value.

## Object acceptance and authority re-signing

An upstream response is accepted only when all of these checks pass:

- response and full manifest bind the exact canonical `ShareRequest` and its
  SHA-256 identity;
- the object manifest is signed by an active, non-revoked object key in that
  origin's currently verified descriptor;
- descriptor, selected signing key, and upstream manifest are unexpired;
- encoded SHA-256, encoded size, decompressed size, compression, typed payload
  schema, and embedded request identity are exact and bounded;
- model, run, snapshot, grid, variables, recipe, query parameters, source
  provenance, and publication grant therefore cannot cross cache identities;
- ECMWF attribution and modification notice rules pass before staging;
- relay delivery is absent from the upstream response.

The authority preserves upstream `created_unix` and caps the new expiry to the
minimum of upstream manifest expiry, descriptor expiry, selected upstream key
expiry, and `created_unix + community.object_manifest_retention_seconds`.
Expired or revoked data can never be revived. The authority signs with the
same current Community signing key and key ID used for locally computed
objects, then stages to local CAS. R2 promotion remains policy- and
popularity-driven; verification success does not make promotion mandatory.

## Required rw-server configuration

The integrated server validates the following settings under
`federation.proxy`:

- `enabled` (default `false`);
- `security_tests_passed` (default `false`, required when enabled);
- `authority_origin_id` and canonical public `authority_https_root`;
- `maximum_attempts` (1 through the federation selection bound);
- DNS, connect, send, receive, and global timeout seconds;
- a durable `accounting_state_file`;
- a separate atomic `control_state_file` and one or more auth-domain
  `operator_principals` when outbound proxying is enabled;
- per-principal monthly download bytes, concurrency, and maximum principals;
- a server-side kill switch independent of catalog discovery;
- optional R2 promotion policy using the existing Community limits.

Each `federation.approved_origins[]` entry additionally needs
`data_bearer_token_file`. This file is required whenever proxy delivery is
enabled and is intentionally distinct from `health_bearer_token_file`.
All configured federation credential files must also contain mutually distinct
values and must not reuse an ordinary BowEcho API token.

Before the first DNS lookup or upstream HTTPS call, the durable quota ledger
atomically acquires concurrency and consumes the worst-case bounded bytes for
every permitted attempt (request, manifest, and object bounds). Failed DNS,
timeout, malformed, verification, and staging attempts are conservatively not
refunded because the origin may already have emitted bytes. Reservations
survive restart; month rollover resets forward only, while clock rollback
fails closed instead of erasing future-dated accounting.

Authenticated configured operators use only these private no-store endpoints:

- `GET /v1/federation/proxy/operator/status` returns `enabled`, runtime
  `kill_switch`, and `persistence_healthy`—no origin, principal, URL, address,
  credential, request, or quota identity;
- `POST /v1/federation/proxy/operator/kill-switch` accepts the closed
  `rw.server.federation-proxy-kill-switch.v1` body with `engaged`.

Engaging stops transport before the atomic control write. Disengaging writes
and fsyncs the durable state before transport reopens. Concurrent updates are
serialized. A failed write leaves the process killed and reports unhealthy
persistence. A persisted engaged state survives restart; the static
`kill_switch = true` setting always re-engages at startup as an additional
safety override. Monitor `rw_federation_proxy_kill_switch` and run `doctor`
after changing the control path or operator set.

The integration must consume, not duplicate, these existing Community values:

- `community.signing_key_file` and `community.signing_key_id`;
- `community.object_manifest_retention_seconds`;
- Community protocol encoded/decompressed/manifest bounds;
- the current Community trusted-key set and local CAS/R2 staging policy.

## Deployment and secrets

Hetzner needs:

- the normal inbound BowEcho API token file;
- the current Community Ed25519 object signing key file;
- the distinct federation catalog Ed25519 signing key file;
- one signed descriptor file per approved public origin;
- pinned public descriptor-signing keys in config;
- one distinct data bearer-token file per approved origin;
- optional distinct health bearer-token files;
- writable durable health, quota, and Community CAS state paths.
- a writable isolated federation control-state path and at least one
  value-free operator principal digest.

Each public origin needs a normal inbound Rusty authentication token matching
only its authority-held data-token file, its own Community object signing key,
an origin descriptor signed by its descriptor key, and a publication-gated
local store. Secret files must be bounded regular files, not symlinks, and be
readable only by the service identity (plus the platform administrator/SYSTEM
where required).

Rotation order is: provision the new public verification key, publish a
descriptor containing both old and new active object keys, rotate the signer,
observe successful verification, then revoke/remove the old key. Emergency
revocation is applied at the authority and prevents selection/staging even if
an old descriptor or object remains cached.

## Release evidence

Automated core tests cover wrong-key failover, exact-request mismatch,
malformed payloads, relay rejection, object-key rotation/revocation, expiry
capping and non-revival, private/mixed DNS, rebinding, redirects, URL tricks,
credential scoping/redaction, deterministic health ordering, self/cyclic
origins, bounded attempts, pre-I/O quota rejection, conservative failure
charging, restart/month/clock behavior, pairwise credential-value isolation,
runtime operator authorization, atomic durable kill-switch restart/failure
behavior, private no-store HTTP control, coarse metrics,
pressure/geography capability checks, and two-origin failover.

The in-process production-shaped HTTP test now proves the origin-only route
uses its separate token, consumes exactly one hop, returns a signed manifest,
and serves the exact verified immutable bytes. Before production enablement,
the deployment must additionally pass:

1. a live two-node test proving `resolve-local` never invokes either node's
   proxy and the outer authority stages/retrieves the object end to end;
2. live token-isolation tests using distinct health/data tokens for two origins;
3. live R2 staging/promotion and R2/origin fallback tests;
4. container tests with hostile DNS, redirect, timeout, and oversized-body
   services;
5. packaged deployment smoke tests with real TLS and service-user file ACLs;
6. dashboards/alerts for coarse origin ID, success/failure, quarantine,
   attempts, bytes, quota rejection, and kill-switch state, with no URL/IP/
   credential diagnostics.

Live multi-host, container-network, packaged-release, and Hetzner deployment
gates are not satisfied by unit tests and must be recorded as unexecuted until
their actual evidence exists.
