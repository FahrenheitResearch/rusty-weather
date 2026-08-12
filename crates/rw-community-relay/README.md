# `rw-community-relay`

Security and control-plane foundation for BowEcho's opt-in **Community Cache**.
This crate is relay-only by construction. It contains no ICE agent, STUN
client, direct socket, LAN discovery, public seed listing, ordinary origin
delivery, or R2 delivery.

The two delivery paths remain separate:

- operational: local cache -> R2 -> authoritative Hetzner HTTPS origin;
- cold historical: local cache -> R2 -> Community Cache relay -> archival
  HTTPS origin -> honestly unavailable.

Hetzner and R2 never pass through this crate or a TURN allocation.

## Implemented seams

- `CloudflareTurnAdapter` parses the official `iceServers` response, removes
  every `stun:`/`stuns:` URL, rejects every scheme other than `turn:` and
  `turns:`, restricts endpoints to an operator allowlist, and derives the
  revocation handle from Cloudflare's generated username. Provider secrets and
  endpoints are not serializable and use redacted `Debug` output.
- `RelayCoordinator` admits only exact SHA-256 identities from an origin-signed
  Community Cache object manifest. The closed protocol union prevents generic
  files and full-run advertisement. Private WRF, ArWen, and user-provided data
  remain rejected unless the origin-signed request contains both explicit
  owner publication and confirmed redistribution rights.
- Each participant receives a distinct signed credential scoped to one opaque
  session, exact object hash, upload or download direction, byte limit, chunk
  limit, relay alias, and at most fifteen minutes.
- Rendezvous grants are opaque and non-serializable as a pair. Public
  candidates have only the protocol's `relay` kind and opaque IDs; actual
  account identities stay in a redacted backend-only dispatch handle.
- Ephemeral X25519 keys are bound to both signed credential fingerprints, the
  exact object, session, cipher suite, and expiry in an Ed25519-signed session
  transcript. HKDF-SHA-256 derives a per-object XChaCha20-Poly1305 key.
- Each envelope authenticates the session, object hash, chunk index/count,
  plaintext size, and unique 24-byte nonce as additional data. The receiver
  rejects tampering, nonce reuse, replay, reordering, wrong session/hash,
  oversized chunks, incomplete objects, and a final SHA-256 mismatch.
- Admission reserves per-user upload/download/concurrency quota and global
  monthly/cost allowance. Advertisements enforce storage allowance. Metered
  seeding is paused unless explicitly enabled. Because TURN payloads bypass
  the backend, every issued session commits its complete signed byte
  reservation on success, failure, expiry, or kill-switch revocation; a
  client-reported zero never lowers the cost ledger. A durable integration may
  reconcile that conservative charge only from authoritative provider
  analytics. The kill switch revokes pending credentials without touching
  HTTPS origins.
- Successful repeat recovery emits a typed R2 promotion signal. Relay misses,
  provider failure, policy denial, and quota/cost stops produce an immediate
  archival-HTTPS-or-unavailable decision with no provider error text.
- `RelayOnlyTurnClient` is a concrete async adapter around `turn` 0.17.2. It
  uses no ICE agent, passes an empty STUN server, exposes no Binding Request,
  pins a validated public DNS answer, and wraps the base socket so every
  initial request, retry, refresh, and data indication can go only to that
  exact operator TURN server. An injected packet-event sink proves forbidden
  destinations fail before network I/O. DNS, client startup, allocation, and
  shutdown are explicitly time-bounded so transport stalls immediately enter
  the normal archival-HTTPS-or-unavailable fallback path.
- After route binding and E2E key derivation, the downloader sends one
  authenticated `receiver_ready` marker through its bound allocation before
  receiving. This creates `turn`'s lazy downloader permission even when the
  reverse allocation discards the marker. The uploader's first bounded data
  attempt creates its reverse permission; a valid late/duplicate readiness
  marker is bounded and ignored, while tampered, wrong-session, wrong-object,
  wrong-kind, or out-of-order markers fail closed.
- The dependency's raw allocated `Conn` is crate-private. Product/application
  code cannot call `send_to` with an arbitrary address and create permissions
  or channel binds. `RelayRouteRegistry` now admits a participant's own TURN
  allocation route only when it is globally routable and falls within an
  explicit operator-audited provider CIDR allowlist (empty by default). The
  registration is bound to the authenticated subject, active role credential,
  exact object/session, and ephemeral key offer. Each participant can retrieve
  only the other provider allocation plus the signed E2E transcript through a
  closed transport-private response; routes have redacted `Debug` and no
  general serialization contract.
- `RelayCoordinator::export_persistence_json` and
  `restore_persistence_json` provide a bounded, versioned durability seam for
  advertisements, per-principal/month usage, reservations, sessions,
  revocations, kill state, and promotion counters. Provider credentials, TURN
  usernames/passwords, signing secrets, hosts, and addresses are structurally
  absent. Pre-crash sessions are never resumed: restore charges the full
  reservation and locally revokes both credential fingerprints.
- `HistoricalRelayClient` is the closed participant orchestrator. It can only
  advertise a previously verified, unexpired signed object or request the
  exact signed hash of a cold historical object. It performs participant-only
  broker polling, TURN allocation registration, signed peer-route binding,
  bounded authenticated retransmission, receiver-side hash confirmation, and
  both-role completion accounting. Initial product use is deliberately capped
  to signed profile and point-series objects no larger than 64 KiB; larger
  objects fall through to archival HTTPS or honest unavailability until a
  separately tested throughput policy exists.

## Transport support boundary

The audited `turn` 0.17.2 client is UDP-only. It hard-codes UDP allocation and
accepts a packet-oriented `Conn`; it has no TURN-over-TCP or TURN-over-TLS
client. The concrete adapter therefore supports only an explicit
`turn:...?transport=udp` endpoint (including Cloudflare UDP port 3478). It
rejects `turn:...?transport=tcp` and `turns:` instead of downgrading them.

The Cloudflare adapter accepts only its documented service matrix: TURN/UDP
on 3478 or 53, TURN/TCP on 3478 or 80, and TURNS/TCP on 5349 or 443. Custom
Cloudflare host allowlists retain that matrix; another provider requires its
own adapter with an explicit port policy. The concrete client currently picks
only TURN/UDP and prefers 3478.

Cloudflare TURN/TLS on 5349/443 needs a separate client with hostname
verification, WebPKI trust roots, and the same destination/source pinning. That
support must remain disabled until the implementation and packet test exist;
the current crate never pretends UDP is TLS.

## Server integration status

`rw-server` now owns the default-off authenticated broker integration. It has
three independent enable/security-test/capacity-audit gates, a separate kill
switch, durable candidate-first coordinator snapshots, per-principal and
global quota/cost ceilings, popularity signals, an operator principal
allowlist, a relay signing-key reference, and a server-only Cloudflare API
token reference. No provider price is compiled into the protocol.

The backend requests credentials with a TTL no greater than fifteen minutes,
uses only an opaque session alias as Cloudflare's `customIdentifier`, and
revokes generated usernames at terminal state. Every control request is
authenticated. A seed advertises one exact signed manifest; a cold requester
supplies only its exact lowercase SHA-256. The broker exposes neither a seed
list nor passive query telemetry, account identity, raw provider response, or
a combined two-participant grant.

Coordinator state is atomically persisted before a grant becomes observable,
and terminal/revocation state is persisted before success is reported. A
durability failure activates the safe relay kill path and returns the normal
archival fallback. Each participant receives only its own credential and TURN
secret, then the opposite provider allocation and signed E2E transcript after
both independently register inside the operator-audited provider CIDR
allowlist. No provider range is trusted by default or compiled in.

The remaining release-gated seam is BowEcho integration: it must invoke
`HistoricalRelayClient` only after a historical local/R2 miss, continue
immediately to archival HTTPS on any failure, and run the full signature,
hash, decompression, typed-schema, expiry, and attribution checks before
caching or rendering. Operational requests remain local/R2/Hetzner (with
approved server-side federation) and cannot enter the relay broker.

## Mandatory transport gate

The concrete client now enforces one pinned TURN/UDP destination at its socket
boundary, but a real multi-process deployment gate is still required. The
feature must remain disabled until an integration test with operating-system
packet capture proves both clients contact only configured TURN endpoints and
never attempt STUN, host/server-reflexive/peer-reflexive gathering, direct
user-to-user UDP/TCP, QUIC, or LAN discovery. If a chosen transport library
cannot make those paths absent (not merely deprioritized), it is not acceptable.

Cloudflare's standard response includes STUN URLs, which is why passing it
directly into a WebRTC client is forbidden. See the official
[credential documentation](https://developers.cloudflare.com/realtime/turn/generate-credentials/).
