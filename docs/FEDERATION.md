# Rusty Weather public-origin federation v1

Status: implemented discovery and selection foundation. Network replication is
not enabled by this contract.

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

Static syntax checks cannot defeat DNS rebinding by themselves. Any component
that later opens a connection MUST resolve the hostname immediately before the
request, reject every non-global answer, pin the selected address for that TLS
connection, retain the signed hostname for certificate/SNI validation, disable
redirects, and repeat the check for every new connection. The current server
selection seam performs no descriptor-directed network request.

## Delivery and failover roles

Current operational BowEcho data remains:

1. verified local cache;
2. R2/CDN immutable hot object when materialized;
3. the authoritative Rusty Weather HTTPS origin for a dynamic miss.

Federated public origins are a later archival/failover option, not a peer tier
inserted into current operational access. A client verifies the catalog and
descriptor chains, matches exact model/product/query/geographic capability and
quota, then uses conventional HTTPS. Hosted R2 and the current authoritative
origin retain priority unless the product policy explicitly selects archival
federation.

The server exposes a bounded selection seam used by future clients/operators.
It filters exact capability, requested bounds, response-size quota, and (when
required) replication opt-in. Repeated health failures temporarily quarantine
an origin; healthy observations reset it; ordering is deterministic and output
count is capped. A failed public origin advances to the next selected origin or
an honest unavailable result. It never falls through to an ordinary user's
address.

Community Cache remains a separate cold immutable-object mechanism for
ordinary opted-in users. Those transfers are encrypted and relay-mediated, and
another user never sees a participant address. Publishing a WRF-compatible run
does not automatically create a public federated origin or grant replication:
both require explicit rights confirmation and the separate operator approval
flow.
