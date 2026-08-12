# BowEcho Community Cache threat model

Status: required security baseline before any relay transport implementation.

## Security properties

Community Cache must preserve these properties even when community seeds, the
object store, the network, and submitted object bytes are hostile:

1. Another BowEcho user never learns a user's IP address or network endpoint.
2. There is no direct user-to-user connectivity and no direct fallback.
3. Only origin-signed, content-addressed, correctly scoped objects are used.
4. A cache identity cannot mix models, runs, snapshots, grids, variables,
   valid times, query parameters, recipes, or provenance.
5. Compressed/malformed content cannot cause unbounded allocation or decode.
6. Private WRF, ArWen, and user-provided data cannot be published implicitly.
7. Required provider attribution and modification notices survive every hop.
8. Phase 1 R2 failure degrades to the authoritative Hetzner HTTPS origin;
   future historical relay failure degrades to its archival HTTPS origin.
9. Quotas, metered-network pause, eviction, and the global kill switch bound
   user and operator cost.
10. Passive searches and presence are private; case rooms are deliberate
    publications.

## Trust boundaries

Trusted for integrity:

- the audited Rusty Weather origin signing process and protected Ed25519 key;
- BowEcho's pinned origin verifying keys and shared protocol implementation;
- the local process after operating-system compromise is excluded.

Not trusted for object integrity or confidentiality:

- community seed users and their machines;
- R2/CDN storage and caches;
- relay providers and relay network paths;
- DNS, proxies, and intermediate networks;
- serialized manifests, case rooms, compressed objects, and cache directories
  before complete validation.

Trusted only for availability/routing policy:

- the rendezvous/backend issuing scoped relay credentials;
- operator quota and hot-object promotion services.

The relay operator sees necessary connection metadata, including each
connecting user's IP, timestamps, byte counts, and abuse signals. This is an
explicit privacy disclosure, not an anonymity claim. A community seed or
downloader does not receive another user's address. End-to-end encryption keeps
the payload confidential from the relay; the signed public weather object may
already be public by policy, but the relay still has no decryption key.

## Adversaries and attacks

### Malicious seed

A seed may send different bytes, truncate or replay content, lie about size,
send an object from another run, or withhold data. The client verifies the
expected canonical request hash, origin Ed25519 signature, encoded size, and
SHA-256 before decode. Decode is bounded and completion must equal signed
decoded size. Failure discards the temporary object, penalizes the opaque
session, and immediately falls back. A seed cannot mint a trusted manifest.

### Malicious or compromised object store

R2 may return stale, missing, swapped, or altered manifests/objects. Both are
untrusted until origin-signature, request identity, hash, size, expiration,
schema, and source-policy checks pass. Deterministic keys are not authorization.
An existing immutable key is never overwritten with different content.

### Cache-key confusion

An attacker may try to reuse output across runs, grids, valid times, variables,
vertical levels, temporal semantics, or recipe revisions. `ShareRequest`
canonical identity includes all of those facts plus snapshot and provenance.
Fixed-point coordinates and resolved UTC windows avoid numeric/timezone
ambiguity. The signed manifest embeds the complete request and matching hash.
Tests mutate each identity component and require a different request hash or a
verification failure.

### Signature/key attacks

Unknown key IDs, malformed base64, non-32-byte public keys, malformed
signatures, unknown algorithms, and modified signed fields fail closed. Key
IDs are included in the signing preimage to prevent substitution. Rotation is
an explicit trusted-key-set update. Private signing keys never ship to clients,
R2, seeds, or relays. TLS does not replace object signature verification.

### Decompression and parser attacks

Manifest/request bytes are capped before parsing. Closed DTOs reject unknown
versions and fields. Identifier, list, map, coordinate, time, object, chunk,
and text sizes are bounded. Content types and compression codecs are
allowlisted. The decoder streams through `DecodedSizeGuard` and aborts before
retaining bytes beyond the signed decoded size, configured maximum, or ratio.
Decoded application schemas are validated before cache publication or render.

### Network-address disclosure

Address disclosure can arise through signaling candidates, URLs, logs, UI,
errors, telemetry, peer identifiers, or direct socket observations. The wire
contract exposes only opaque relay/session/ticket identifiers. Relay IDs reject
address-shaped text; DTOs have no address/host/port fields and reject unknown
fields. Candidate kinds other than `relay` cannot deserialize. Public errors
and logs must use request IDs and coarse failure codes, never remote candidates
or another user's connection metadata.

There is no address-bearing discovery, direct candidate gathering, direct ICE,
STUN, direct QUIC/TCP/UDP, LAN discovery, or fallback. Any future dependency
capable of direct connectivity must be configured so those paths are absent,
then tested at the serialized API boundary and with packet-level integration
tests. If relay-only enforcement is uncertain, Community Cache remains
feature-disabled.

### Relay compromise and eavesdropping

A relay may inspect metadata, drop/reorder/replay chunks, or alter ciphertext.
Short-lived credentials restrict exact object, direction, bytes, chunks,
session, subject, relay, and time. End-to-end authenticated encryption detects
alteration; chunk index/count, session, object hash, and plaintext size are
authenticated. Replay/nonces are tracked per session. Missing/corrupt chunks
abort and fall back. The relay never receives an end-to-end content key.

The control backend may relay ephemeral public keys but cannot derive an
X25519 shared secret. A later Phase 2 transcript must bind both ephemeral keys,
session ID, object hash, credential identities, cipher suite, and protocol
version before deriving a per-object key. Long-term identity keys must not be
used directly as payload-encryption keys.

### Credential theft and abuse

Relay credentials expire within 15 minutes, are direction/object/session
scoped, and carry byte/chunk limits. Tokens are treated as secrets: never in
URLs, UI, analytics, or normal logs. Backend per-user upload, download,
storage, concurrency, and monthly limits apply before issuance and during
transfer. Abuse controls may revoke a session or globally disable new relay
credentials without affecting origin retrieval.

### Cost exhaustion and denial of service

Attackers may request expensive products, churn cache keys, exhaust disk, or
force repeated relay traffic. Existing Rusty Weather query budgets remain in
force. Community limits constrain manifest/body/decode size and concurrency.
Local caches have bounded atomic eviction. Server quotas, monthly/cost
thresholds, a global kill switch, and metered-network defaults bound traffic.
Popularity promotion moves recurring objects to R2. No peer availability is
required. In the future historical path, relay failure proceeds immediately to
the archival HTTPS origin rather than retrying or attempting direct transport.

Provider pricing is deliberately not a protocol constant. Operators update
thresholds after capacity and provider audits.

### Private-run exfiltration

Local/private WRF, ArWen, and user-provided runs default to
`explicit_owner_publication = false` and
`redistribution_rights_confirmed = false`. Both facts must become true through
an explicit owner publication action. Enabling the feature, caching, querying,
opening a run, rendering, or joining a case room does not change them. The
closed object union cannot carry raw files or private directories. Publication
policy is included in signed identity, so a private result cannot be relabeled
as public under the same request hash.

### Licensing/attribution loss

Software licensing does not relicense model data. Source provenance is part of
request identity. ECMWF-derived objects and case rooms fail verification
without the ECMWF source/link, CC BY 4.0 link, terms/disclaimer, and a non-empty
modification notice. Notices are signed and copied on promotion/relay; an
intermediate cannot strip them without invalidating the signature.

### Case-room privacy

An attacker may infer interests from a public case room. Publication therefore
requires a titled, bounded, retained, explicit Publish action. Passive searches,
current presence, cache misses, local history, and viewer lists never enter a
case manifest. Case IDs are opaque. UI must make the public scope and retention
clear before submission and must allow no implicit conversion from a search to
a case.

## Required verification gates

Phase 1 must remain feature-gated until automated tests prove:

- golden canonical request bytes and Ed25519 signatures remain stable;
- reordered set-like input normalizes identically, while run, snapshot, grid,
  variables, valid time, query, and recipe changes do not collide;
- signature, request hash, object hash, size, expiry, schema, and content-type
  tampering fail closed;
- bounded decode rejects a decompression bomb before unbounded retention;
- private WRF and ArWen requests fail without explicit publication and rights;
- ECMWF notices are mandatory on objects and case manifests;
- cache writes are atomic, immutable, bounded, and evict correctly;
- R2 failure falls back to origin and no missing hot object is fatal;
- server quotas and kill switch prevent admission without breaking origin
  operation.

Before Phase 2 can be enabled, tests must additionally prove:

- only relay candidates are representable/serializable;
- host, server-reflexive, peer-reflexive, direct, address-bearing, and unknown
  candidates are rejected;
- no app-visible DTO, UI state, error, or normal log reveals a peer IP;
- relay credentials expire, enforce object/direction/byte/chunk scope, and are
  rejected after revocation;
- encrypted envelopes reject nonce reuse, replay, reordering, tampering,
  wrong-session data, and oversized chunks;
- relay outage in the future historical path immediately proceeds to the
  archival HTTPS origin;
- upload/download/storage/concurrency/monthly quotas, metered-network pause,
  global kill switch, cost thresholds, and hot-object promotion work;
- packet-level tests observe client-to-relay traffic only and no direct
  user-to-user connection attempt.

Any failure keeps relay-mediated sharing disabled. Completed radar/model RC
improvements do not depend on this feature gate.

## Residual risks and non-goals

- The relay and access ISP know the connecting user's IP; the product does not
  claim to hide it from network operators.
- Timing and object popularity can reveal coarse interest to the operator.
- A compromised client OS can expose local caches, keys, and user actions.
- An authorized user can redistribute already decrypted public output outside
  BowEcho; protocol controls cannot prevent screenshots or re-publication.
- Availability of origin, R2, or a relay is not guaranteed, though ordered
  fallback prevents peer availability from becoming a dependency.
- Full-run replication is outside v1 and requires a separate threat review,
  explicit advanced opt-in, and stronger storage/cost controls.
