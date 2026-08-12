# Rusty Weather public-origin federation v1

Status: implemented signed discovery, bounded active health monitoring,
deterministic selection, and default-off authoritative data failover. Full-run
replication remains a separate advanced contract.

Public-origin federation is conventional HTTPS discovery for institutions and
operators who deliberately publish a Rusty Weather service address. It is not
Community Cache discovery. Ordinary BowEcho users never appear in the catalog,
and the DTOs contain no client address, relay identifier, ICE candidate, STUN
state, socket, or arbitrary callback URL.

## Signed contract

`rw-community-protocol` owns the closed DTOs, fixed-field canonical binary
encoding, Ed25519 signing, verification, parsing limits, and URL policy:

- `rw.federation.origin.v1` binds an origin ID and display name; canonical
  public HTTPS root; same-origin health path; descriptor and immutable-object
  Ed25519 key IDs/public keys/validity intervals; model, product, query, and
  pressure-level capabilities; fixed-point geographic rectangles; retention;
  API schema and build versions; issue/expiry times; attribution,
  acceptable-use, and privacy links; explicit replication consent and limits;
  and request/concurrency/egress quotas.
- `rw.federation.catalog.v1` binds a catalog ID, generation/expiry interval,
  and a canonical ordered set of the complete origin-signed descriptors. The
  catalog is separately signed by the federation operator.

The signing key ID is part of each domain-separated signing preimage. Unknown
algorithms, schemas, fields, key IDs, signatures, or noncanonical collection
orders fail closed. Descriptor and catalog bytes, lifetimes, origins, keys,
models, products, query capabilities, coverage areas, and pressure levels all
have hard upper bounds.

## Operator approval, rotation, and revocation

There is intentionally no registration, update, or callback HTTP endpoint.
Enabling federation requires all of the following in server configuration:

1. a permission-restricted catalog signing-key file;
2. one bounded origin-signed descriptor file per origin;
3. an out-of-band allowlist binding every origin ID to exact trusted
   descriptor-signing key IDs and Ed25519 public keys;
4. optional revoked-origin and revoked-key sets.

The descriptor set and allowlist IDs must match exactly at startup. This makes
self-signed, but unapproved, registration impossible. For key rotation, an
operator first verifies and allowlists the new key out of band. An origin may
then publish both old and new key intervals and sign with the new key. The old
key can be revoked without invalidating a descriptor signed by the approved new
key. Revocation wins over both the descriptor and allowlist.

## URL and SSRF policy

Origin roots use a deliberately narrow grammar: lowercase public DNS names,
standard `https://`, no IP literals, nonstandard ports, credentials, fragments,
queries, path, backslashes, special-use/local suffixes, or ambiguous numeric
hosts. Health is a bounded relative `/v1/...` path, so a descriptor cannot
redirect a health probe to another host. Policy links use the same public-host
rules and bounded paths.

Static syntax checks cannot defeat DNS rebinding by themselves. The separately
feature-gated server health monitor therefore creates a fresh client for every
probe, resolves the signed hostname immediately before connecting, rejects the
entire answer set if it is empty, oversized, or contains any non-global address,
and gives the connector exactly one pinned socket address. The original signed
hostname remains in the HTTPS URI for TLS SNI and certificate validation.
Redirects are disabled, response bodies are bounded, and resolve, connect,
request-send, response-receive, and whole-call deadlines are independently
bounded. A new resolution and policy check occurs for every new probe; pooled
connections cannot bypass it.

Active monitoring is off by default even when catalog federation is enabled.
Enabling it requires a durable health-state path. Each approved origin may
optionally name a permission-restricted bearer-token file for its same-origin
health path; otherwise the monitor performs a public HTTPS GET. Tokens are
never accepted inline, exposed by an API, or placed in logs.

## Delivery and failover roles

Current operational BowEcho data remains:

1. verified local cache;
2. R2/CDN immutable hot object when materialized;
3. the authoritative Rusty Weather HTTPS origin for a dynamic miss.

Federated public origins are an explicit authority-mediated failover option,
not a peer tier inserted into current operational access. BowEcho may inspect
the signed public catalog, but sends an exact request only to the authoritative
`POST /v1/federation/objects/resolve` endpoint. The authority selects an
approved origin and uses its own origin-scoped credential over conventional
HTTPS; BowEcho never receives that credential or connects directly.

The server filters exact capability, requested bounds, response-size quota,
and (when required) replication opt-in. Repeated health failures temporarily
quarantine an origin; healthy observations reset it; ordering and failover are
deterministic and bounded. The dedicated upstream `resolve-local` route can
consult only that origin's CAS, R2, and published store, so cyclic federation
cannot recurse. A failed public origin advances to the next candidate or an
honest unavailable result. It never falls through to an ordinary user's
address. The exact proxy, verification, credential, and deployment contract is
documented in `FEDERATION_PROXY.md`.

Community Cache remains a separate cold immutable-object mechanism for
ordinary opted-in users. Those transfers are encrypted and relay-mediated, and
another user never sees a participant address. Publishing a WRF-compatible run
does not automatically create a public federated origin or grant replication:
both require explicit rights confirmation and the separate operator approval
flow.

## Health, quarantine, and operator visibility

Probe success clears a prior failure and quarantine immediately. Consecutive
failures reach an operator-configured threshold and quarantine the origin for a
bounded interval; the monitor continues probing quarantined origins so recovery
does not wait for the interval to expire. Health state is atomically persisted
without IP addresses, endpoint URLs, credentials, or raw transport errors, so
restart cannot silently erase quarantine.

Authenticated `GET /v1/federation/health` returns only origin ids, coarse
healthy/degraded/quarantined/unknown states, counters, and timestamps. OpenMetrics
exports only aggregate unlabeled federation counts and probe totals. Application
logs contain the public origin id and coarse outcomes, never a resolved address,
health URL, token, or low-level error containing one.
