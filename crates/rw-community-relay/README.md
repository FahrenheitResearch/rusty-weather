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

## Required server integration (not yet wired)

The active `rw-server` routes/configuration are being changed by other work, so
this crate deliberately stops at a clean injected boundary. Integration must:

1. Add an off-by-default relay configuration containing the three independent
   enable/security-test/capacity-audit gates, quota values, cost threshold,
   promotion threshold, relay signing key reference, and Cloudflare secret
   reference. No provider price is compiled into the protocol.
2. Implement `RelayProvider` in the backend. Keep the Cloudflare long-lived key
   server-side, request credentials with a TTL no greater than fifteen minutes,
   pass the response through `CloudflareTurnAdapter`, use only an opaque
   per-session alias as `customIdentifier`, and revoke both generated usernames
   when a session closes.
3. Atomically persist the coordinator snapshot after every mutation. A newly
   issued session snapshot MUST reach durable storage before either participant
   receives a grant; terminal/revocation state MUST persist before success is
   reported. If persistence fails, return the normal archival fallback and do
   not deliver the grant.
4. Authenticate every control request. Availability accepts an exact signed
   manifest; lookup accepts only its 64-character lowercase object hash. Do not
   expose a seed list, passive query telemetry, account identity, socket,
   provider response, or the combined two-participant grant.
5. Exchange only ephemeral public keys and the signed session binding. Deliver
   each participant its own candidate, credential, and TURN access secret. A
   transport route allowlist must contain current, independently audited relay
   allocation CIDRs; no provider ranges are trusted by default or compiled in.
6. On the BowEcho side, invoke this path only after a historical local/R2 miss.
   Any miss or failure immediately continues to archival HTTPS. Current data
   remains local/R2/Hetzner and never invokes the coordinator.
7. After decryption, run the existing complete `verify_signed_object`, bounded
   decompression, typed schema, and attribution verification before committing
   bytes to the local cache or rendering them.

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
