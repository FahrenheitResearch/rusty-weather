# BowEcho Community Cache protocol v1

Status: Phase 1 operational delivery plus a separately feature-gated Phase 2
relay control plane. The concrete client TURN transport remains gated until
packet-level relay-only and fallback tests pass.

This document is normative for BowEcho Community Cache and Private Community
Sharing. The words MUST, MUST NOT, SHOULD, and MAY have their RFC 2119 meanings.

Community Cache distributes narrowly typed, immutable weather-query objects.
It is not a file-sharing interface. Direct connectivity between users is
permanently out of scope: relay-mediated peer-assisted transfers come
afterward. A conforming implementation MUST NOT gather, signal, exchange,
display, log, or report another user's IP address. It MUST NOT offer a direct
transport or fall back to one.

The Rust implementation of the wire DTOs, normalization, canonical byte
encoding, SHA-256 identity, Ed25519 signing, verification, and limits is
`rw-community-protocol`. HTTP clients and servers MUST use those shared
functions instead of reproducing canonicalization or signature logic.

## Deployment and delivery order

The reliable origin is a Rusty Weather service on the operator's Hetzner node.
The initial operator policy serves the newest HRRR hourly run, newest complete
extended HRRR run, newest GFS run, and newest NBM surface run. It keeps one
previous generation only while atomically replacing the active generation.
Retention, completeness, cadence, disk limits, and worker concurrency MUST be
derived from model capability metadata and operator configuration. Final
capacity values are intentionally absent pending a direct node audit.

The Phase 1 operational path is strictly:

1. a verified object in the BowEcho local content-addressed cache;
2. a hot immutable object in R2;
3. the authoritative Hetzner/Rusty Weather dynamic HTTPS origin;
4. successful origin output is stored locally and MAY be promoted to R2.

Hetzner is the conventional HTTPS origin and manifest signer. It is never
reached through TURN or any peer transport. Every failure or absence at one
tier MUST continue immediately to the next tier, and an R2 miss or outage MUST
NOT make an otherwise valid origin query fail.

A historical-object path, when Phase 2 is separately enabled and passes
its security gates, is local cache, R2, a relay-only community seed, an
archival HTTPS origin, then unavailable. No peer being online is a cache miss,
never an application error. Frequently relayed objects SHOULD be promoted to
R2 so relays recover rare objects rather than repeatedly carrying hot content.

The relay provider is an operator-selected implementation behind a provider
abstraction. Cloudflare TURN is the intended first provider, configured for
relay-only operation. Pricing, included traffic, and thresholds are mutable
operator inputs and MUST NOT be compiled into this protocol.

## Admitted object categories

`ShareQuery` is a closed, tagged union. Exactly these v1 categories exist:

- `profile`: a pressure profile plus explicitly named nearest-point surface
  samples;
- `point_series`: an exact point time series;
- `native_window`: a native-grid window or tile at an exact storage slot and
  valid time;
- `geographic_window`: a finite fixed-point geographic bbox resolved against
  the exact signed snapshot/grid, returning a versioned minimal native
  envelope with cropped coordinates, projection metadata, cell mask, and
  either surface fields or explicit pressure levels;
- `temporal_grid`: a temporal/diurnal reduction with explicit window,
  semantics, reducer, vertical selection, and missing-data policy;
- `case_artifact`: an artifact deliberately attached to a published case room.

Unknown tags and unknown schema versions MUST fail closed. There is no generic
blob, filename, directory, URL, local path, provider request, or arbitrary-file
variant. Full-run replication is not a v1 category; it is a separate advanced
opt-in feature for a later protocol version.

For `profile`, `pressure_variables` and `surface_variables` are independently
bound into cache identity. The outer `variables` list MUST equal their sorted
union. The standard JSON payload is `ProfileObjectPayload<ProfileResult>` and
MUST include one sorted `SurfaceSample` for every signed surface variable,
using the profile's nearest native-grid point, storage slot, and `valid_unix`.
Normal surface candidates include `temperature_2m`, `dewpoint_2m`, `u_10m`,
`v_10m`, a model's canonical surface-pressure field or documented
approximation, `orography`, and `mslp`; a missing field is represented as a
typed null sample, not silently substituted under the same identity.

`geographic_window` uses the distinct typed payload schema
`rw.community.geographic-window-payload.v1`; it is never decoded as the older
native-index window payload. The signed bbox uses eastward-arc longitude
semantics (west > east crosses the antimeridian), and the object data uses
`rw.query.geographic-window.v1` with a minimal native envelope, exact cropped
coordinates/projection, a cell mask, and unreduced `[level][y][x]` pressure
layout when levels were requested.

## Canonical request identity

The schema is `rw.community.request.v1`. A `ShareRequest` contains and binds:

- exact `model`, immutable `run`, `snapshot_id`, and `grid_hash`;
- sorted, unique variables;
- the complete tagged query and all query parameters;
- exact single-object `valid_unix` or an exact time window, as applicable;
- a recipe identifier, recipe version, and sorted recipe parameters;
- normalized source provider, role, and product provenance;
- the data-origin/publication policy facts.

Coordinates are signed fixed-point integers in degrees times 10^7. They are
not IEEE floating-point values. Pressure levels are unique ascending hPa
integers. Local-day requests bind both the requested civil date/timezone and
the resolved UTC interval. These rules prevent platform-dependent identity and
daylight-saving ambiguity.

`canonical_request_bytes` writes one deterministic binary preimage:

- domain prefix `rw-community-request-identity-v1\0`;
- fixed field order;
- one-byte enum discriminants;
- big-endian fixed-width integers;
- raw UTF-8 strings prefixed by a big-endian u32 byte length;
- lists/maps prefixed by a big-endian u32 count and emitted in normalized
  sorted order.

`request_sha256` is lowercase hex SHA-256 of that preimage. Changing the run,
snapshot, grid, variable split, valid time/window, query, recipe, provenance,
or publication facts MUST change the identity. New semantics require a new
schema and domain prefix; fields MUST NOT be appended invisibly to the v1
preimage.

## Origin-signed object manifest

The schema is `rw.community.object.v1`. `ObjectManifest` is self-describing and
contains:

- the full canonical `ShareRequest` and matching `request_sha256`;
- SHA-256 of the exact encoded object bytes;
- an allowlisted content type;
- `none`, `gzip`, or `zstd` compression;
- exact encoded and decoded sizes;
- required provider attribution records and modification notices;
- creation and expiration Unix timestamps.

The signature block contains `algorithm = ed25519`, an opaque signing-key ID,
and a standard-base64 64-byte signature. The signing preimage is:

1. `rw-community-object-signature-v1\0`;
2. the length-prefixed signing-key ID;
3. the fixed-field canonical object-manifest encoding, including the complete
   canonical request.

The private signing key remains at the trusted origin. BowEcho is provisioned
with one or more pinned Ed25519 verifying keys indexed by key ID. Key rotation
adds a new pinned ID before removing the old one. An unknown key ID, malformed
signature, unsupported algorithm/schema, request-hash mismatch, object-hash
mismatch, or size mismatch is an untrusted cache miss and MUST NOT be rendered,
seeded, or promoted.

Consumers MUST run `verify_signed_object`. It validates schema and bounded
fields, checks the embedded canonical request hash against the expected
request, checks encoded size and SHA-256, verifies Ed25519, and enforces source
notices. Only then may a compressed body enter bounded streaming decode.
`DecodedSizeGuard` MUST account for every decompressed chunk before retention
and MUST match the signed decoded size at completion. An object is published to
the cache atomically only after all checks succeed.

Accepted v1 content types are deliberately narrow:

- `application/json`;
- `application/vnd.apache.arrow.file`;
- `application/vnd.rusty-weather.window+zstd`;
- `image/png` for explicitly published case-room images only.

Object and manifest byte ceilings, decoded ceilings, decompression ratio,
variables, provenance entries, attribution records, artifacts, and chunks are
bounded by `ProtocolLimits`. Deployment policy MAY lower those limits but MUST
NOT raise them without a protocol/security review.

## HTTP contract

All endpoints use authenticated HTTPS. Requests larger than the configured
manifest/request limit are rejected before JSON parsing. JSON decoders MUST
reject unknown fields where the protocol DTO declares a closed structure.
Errors use the service's normal RFC 9457 problem format and MUST omit paths,
peer identifiers, addresses, provider secrets, relay credentials, and raw
signatures.

### `POST /v1/community/objects/resolve`

Input is `ResolveObjectRequest` with schema `rw.community.resolve.v1` and one
canonical `ShareRequest`. The origin normalizes and validates the request,
computes `request_sha256`, and returns `ResolveObjectResponse`.

On a known object, `signed_manifest` contains the verified/persisted
`SignedObjectManifest`. Phase 1 `delivery_order` is an ordered subset of only
`r2_hot_object` and `origin`; it MUST omit `community_relay`. A missing current
object begins ordinary dynamic generation at the authoritative Hetzner HTTPS
origin. It MUST NOT fabricate a peer-only success or involve TURN.

### `GET /v1/community/objects/{sha256}`

Returns the exact immutable encoded bytes whose lowercase SHA-256 is the path
value. The handler accepts only 64 lowercase hexadecimal characters and serves
only objects referenced by a valid signed manifest. It does not accept a
filename or path component. The client verifies the expected manifest, size,
hash, signature, attribution, and bounded decode independently of HTTPS.
Revoked owner publications are tombstoned before their live request mapping is
removed and return unavailable; an object hash cannot be republished through
the artifact endpoint after rights withdrawal.

### `POST /v1/community/artifacts`

Publishes exactly one typed case artifact with schema
`rw.community.case-artifact-publication.v1`. The closed payload union admits
only a plain-text annotation, a bounded scalar table, a fixed-coordinate
point/polyline/polygon overlay, or a bounded PNG/WebP rendered image. It has no
path, filename, directory, URL, HTML, script, raw-file, or process-command
field. Image signatures, encoded bytes, declared dimensions, and decoded pixel
surface are bounded before acceptance.

The canonical `case_artifact` request binds case/artifact/type, model, immutable
run, source snapshot, grid, variables, recipe, provenance, publication policy,
and `publication_owner_principal_sha256`. The latter MUST exactly match the
SHA-256 principal derived from the authenticated bearer token. Every body sent
to this client-authored endpoint MUST use `PrivateWrf`, `PrivateArwen`, or
`UserProvided`, set explicit owner publication, confirm redistribution rights,
and include non-empty attribution/license fields. `PublicProvider` is rejected
even when the client supplies a plausible public model or provider name: the
origin signature attests immutable owner-bound bytes and claims, not an
unverified upstream identity. Underlying NOAA/ECMWF lineage remains bound in
source provenance and attribution. The origin serializes the typed wrapper,
hashes the exact bytes, signs the complete object manifest, atomically stores
bytes and manifest, and writes a durable audit record. Publication also requires
bounded retention and
`community.cases.artifact_publication_enabled = true`.

### `POST /v1/community/artifacts/{sha256}/revoke`

Only the recorded authenticated owner may confirm rights withdrawal. The
server durably writes an owner/object/reason/time tombstone before removing the
live request mapping. A tombstoned artifact is never served or silently
recreated under that object identity.

### `POST /v1/community/cases`

Creates a deliberate publication from a validated `CaseRoomManifest` with
schema `rw.community.case.v1`. It requires an authenticated explicit Publish
action, redistribution-rights confirmation, event/time bounds, title,
retention, model/run/snapshot/grid source attribution, and bounded references
to already signed, unexpired, non-revoked typed artifacts owned by the same
authenticated principal. Every reference must exactly match the signed
case/artifact/type/request/object identity and an exact source entry. Passive
searches never call this endpoint.

The origin assigns or validates the opaque `case_id`, signs the canonical case
manifest with Ed25519, and returns `SignedCaseRoomManifest`. Case mutation is a
new signed generation; existing object bytes remain immutable.

### `GET /v1/community/cases/{case_id}`

Returns a bounded `SignedCaseRoomManifest`. `case_id` is an opaque safe token,
never a path, address, username, or hostname. The client verifies the origin
signature and all referenced object manifests before use.

### `GET /v1/community/cases?after=&limit=`

Returns `rw.community.case-directory.v1`, a private/no-store page of at most
100 complete `SignedCaseRoomManifest` values in strictly increasing opaque
`case_id` order. `after` is the final opaque ID from the preceding page and
`next_after` is present only when more cases remain. The service verifies each
case signature and every live referenced artifact before discovery. Passive
searches, viewers, presence, peer state, and unpublished cases never enter the
directory.

### `POST /v1/community/cases/{case_id}/revoke`

Withdraws a case only when the authenticated principal owns every referenced
artifact. A durable case tombstone is written before the live case manifest is
removed, and the opaque case ID cannot be reused.

## R2 renewable signed-manifest layout

R2 is an optional hot-object tier with deterministic keys:

- `v1/objects/{object_sha256}`
- `v2/manifests/{manifest_sha256}.json`
- `v2/requests/{request_sha256}.json`

The manifest blob is the bounded serialized `SignedObjectManifest`; its key is
the SHA-256 of those exact bytes. The data object is the exact encoded body. A
strict `rw.community.hot-manifest-pointer.v1` record binds one request hash to
one manifest hash and is the only replaceable R2 key. Readers hash the fetched
manifest bytes, then verify the origin signature, full request, object, and
expiry; the pointer is discovery, never a trust root. The origin writes object,
manifest blob, then pointer in that order. This permits signature/key renewal
after expiry without overwriting immutable content or permanently stranding a
request. Legacy `v1/manifests/{request_sha256}.json` is read-only migration
fallback and receives no new writes.

No key contains model names, run names, queries, user IDs, filenames, or paths.
Immutable uploads use create-if-absent/conditional semantics and byte-compare
conflicts. Pointer updates require authenticated strict-schema replacement.
Promotion does not change canonical request or object identity.

Revocable `case_artifact` objects are not promoted to the public hot tier in
this version. They remain behind the authoritative origin's live tombstone
check, so owner rights withdrawal takes effect for every conforming fetch.
R2 case-artifact promotion must remain disabled until a mandatory fresh signed
revocation-status contract is implemented by the origin, gateway, and BowEcho.
Bytes already delivered before withdrawal cannot be clawed back; the UI states
that unavoidable publication limitation before Publish.

## Source and publication policy

NOAA/public-provider objects still require an affirmative operator policy that
redistribution rights are confirmed. The software license does not relicense
data.

Every object whose provenance includes `ecmwf-open-data` MUST carry the ECMWF
source notice, ECMWF link, CC BY 4.0 name/link, terms link, disclaimer, and a
non-empty notice that the source was modified/subset/normalized/derived. The
same records MUST propagate into every case room referencing ECMWF material.
Verification fails closed when they are missing.

`PrivateWrf`, `PrivateArwen`, and `UserProvided` are default-deny. They require
both `explicit_owner_publication = true` and
`redistribution_rights_confirmed = true`. Merely enabling Community Cache,
loading a run, performing a search, rendering a result, or creating a local
cache entry does not grant publication. No private directory or raw `wrfout`
path crosses this contract. ArWen MAY publish a processed object immediately
after its atomic `.rws` commit only following the owner's explicit publication
action and rights confirmation.

All client-authored case artifacts use that same explicit-owner boundary,
including annotations, plots, or tables derived from public model data. They
MUST NOT claim `PublicProvider`; only objects materialized from an
origin-validated immutable run may carry that identity. A future source-handle
contract may add stronger upstream attestation, but free-form client provenance
is never upgraded into it.

Raw `wrfout`, arbitrary files, and complete-run replication have no upload
route. `rw.community.run-generation-publication.v1` is a disabled-by-default
inventory contract for a later, separately reviewed service. It can inventory
only immutable `.rws` generation chunks by opaque ID, ordinal, byte count, and
SHA-256; `publication_enabled = true` currently fails validation.

## Case rooms and privacy

A case room is an explicit publication, not presence telemetry. Its signed
manifest contains title, event bounds, publication and retention times,
model/run/snapshot/grid provenance, source attribution, and immutable artifact
references. It contains no viewer list, passive query, local path, machine
name, network address, or hidden automatic publication state.

The UI calls the feature Community Cache or Private Community Sharing. It is
off by default and shows the enabled categories, disk allowance, upload and
download caps, concurrency, monthly cap, and metered-network behavior before
opt-in. Passive searches remain private.

## Phase 2 relay-only contract

Phase 2 may add relay-mediated peer-assisted delivery only after its fallback,
privacy, abuse, and cost tests pass. The protocol crate intentionally provides
no network implementation. Its `RelayCandidate` can represent only `relay` and
contains opaque relay/ticket IDs plus expiration; unknown fields and candidate
kinds fail deserialization. IDs reject IP literals, dots, colons, slashes, and
backslashes. There is no host, server-reflexive, peer-reflexive, direct, socket,
or endpoint-address field.

The backend issues signed, short-lived credentials scoped to relay, opaque
session and subject, exact object hash, upload/download direction, byte limit,
chunk limit, and validity interval. Maximum lifetime is 15 minutes. Credentials
are single-purpose and MUST be revoked or allowed to expire after transfer.
They do not contain another user's identity or address.

Payload chunks use `EncryptedRelayEnvelope`: opaque session ID, exact object
hash, chunk index/count, plaintext byte count, a unique 24-byte nonce, and
XChaCha20-Poly1305 ciphertext. V1 fixes plaintext chunks at 512 bytes (except
the final chunk) and caps the complete compact JSON/base64 datagram at 1,200
bytes. The worst-case 128-character session/u32-index envelope is tested at
1,150 bytes, so the protocol does not depend on IPv4 or IPv6 fragmentation.
Ephemeral X25519 public keys travel only through the authenticated backend and
are bound, with both signed role credentials, into a signed session transcript.
The relay never receives the derived per-object content key. Envelope headers
are authenticated additional data. Nonces never repeat for a session key.

TURN/UDP reliability is a bounded stop-and-wait protocol. The downloader sends
one `receiver_ready` control datagram through its bound allocation before
waiting for data. It is authenticated with the ACK key using the
downloader-only kind and is bound to the opaque session, exact object, ACK
kind, and index zero. This
send causes the pinned TURN client to create the downloader's lazy receive
permission. It has no receipt requirement: it may be discarded while the
uploader's reverse permission does not yet exist. The uploader's first bounded
data attempt creates that reverse permission. A valid late or duplicate
`receiver_ready` is authenticated, bounded, and ignored while an ACK or final
receipt is awaited; an unsigned, cross-session, cross-object, wrong-kind, or
excessive marker fails closed.

For each encrypted chunk, the downloader sends an end-to-end authenticated ACK
bound to session, object, ACK kind, and exact chunk index. The uploader
encrypts each chunk once and retransmits identical ciphertext only up to the
configured attempt ceiling. A downloader may accept only the immediately
preceding byte-identical duplicate; it authenticates the ciphertext again,
re-ACKs it, and never appends it twice. Future or older chunks fail closed.
After every chunk ACK, the uploader sends a separate
authenticated transfer-complete confirmation. The downloader accepts that
confirmation only after exact byte count and final SHA-256 match, then still
applies the origin-signed manifest, decompression, schema, and source-policy
checks and posts its authenticated broker completion before returning an
authenticated transfer receipt. The uploader succeeds only after that receipt,
then posts its own signed-role completion; it reports success only when the
broker confirms matching exact-byte completion from both roles. A lost final
receipt or missing counterpart completion fails/falls back rather than counting
as a successful recovery.

The initial stop-and-wait implementation admits only profile and point-series
objects up to 64 KiB. At 512 bytes per RTT it is intentionally not presented as
a practical large-object transport. Native/geographic windows, temporal grids,
and case artifacts continue directly to archival HTTPS/unavailable after R2
miss until a bounded authenticated sliding-window/selective-ACK transport and
its loss/duplication/reordering tests pass.

No implementation may add address-bearing signaling or gather direct
candidates. A provider that cannot guarantee relay-only operation is not a
valid provider. There is no silent or user-selectable direct fallback now or in
a future mode.

TURN participants necessarily exchange the relay provider's allocated route so
the provider can create a permission/channel between its own allocations. This
is not a host, server-reflexive, peer-reflexive, or direct candidate. It is
carried only by a closed participant-authenticated transport response, never by
`RelayCandidate`, discovery, UI state, errors, telemetry, or ordinary logs. A
client may register only the address returned by its own TURN allocation. The
backend accepts it only when it is globally routable and inside an exact,
operator-audited relay-provider CIDR allowlist; that allowlist is empty by
default and no provider ranges are compiled as trust. Registration binds the
route to the active signed role credential, authenticated principal, object,
session, and ephemeral key offer. Each participant receives only the other
provider allocation and signed E2E transcript. A host/srflx/direct address is
therefore outside the representable public candidate contract and outside the
admitted transport-route policy.

The relay operator necessarily observes connection metadata such as the
connecting user's IP, time, transfer size, relay/session identifiers, and abuse
signals. Other BowEcho users never receive those addresses. Relays cannot read
end-to-end encrypted payloads. The UI MUST disclose this distinction without
claiming anonymity from the relay operator.

## Quotas, kill switch, and failure behavior

The Phase 2 broker is an independently off-by-default server feature. It
accepts only authenticated exact-hash cold historical operations:

- `POST /v1/community/relay/advertisements` admits an origin-signed immutable
  identity under local opt-in/category/storage/upload/metered policy;
- `POST /v1/community/relay/historical/lookups` requires `historical = true`
  and either durably reserves one caller-specific session or immediately
  returns archival HTTPS/unavailable fallback;
- caller-specific grant polling and exact role/session retrieval never expose
  a seed/session directory or the other participant's grant;
- `/routes` and `/transport` are the sole transport-private provider-allocation
  exchange, authenticated and `Cache-Control: no-store`;
- `/sessions/complete`, `/fail`, and `/revoke` durably commit the terminal
  transition before reporting success; and
- operator status/kill endpoints expose only coarse counters/gates and require
  configured bearer-derived principal digests.

Every mutating admission/reservation/terminal transition atomically replaces
the bounded coordinator snapshot before returning success. TURN username,
password, API token, allocation address, and provider credential are never
persisted. After restart every pre-crash session is failed/revoked, its full
reservation is conservatively charged, and short-lived TURN credentials are
allowed to expire. A corrupt, incompatible, oversized, or configuration-mixed
snapshot fails closed.

The initial provider adapter calls Cloudflare's documented
`/credentials/generate` endpoint with the exact configured TTL and opaque
`customIdentifier`, uses `/credentials/{username}/revoke`, accepts only the
documented 201/204 statuses, strips all STUN URLs, and admits only TURN/TURNS
URLs on the operator host allowlist. Provider API HTTPS preserves
`rtc.live.cloudflare.com` for TLS verification while pinning one freshly
resolved address only after the complete DNS answer set is globally routable.
Redirects and proxy discovery are disabled, all phases have bounded timeouts,
and provider errors are deliberately coarse.

Server policy enforces per-user upload, download, storage, concurrency, and
monthly limits; local policy enforces disk allowance and cache eviction. Seed
upload pauses by default on metered networks. The backend has a global kill
switch plus traffic/cost thresholds that disable new relay work without
breaking local, R2, or origin retrieval. Quota or relay errors are fallback
events, not object corruption and not total request failure.

Objects are immutable and evicted by reference/retention policy. Eviction does
not delete arbitrary paths and never follows object-controlled filenames.
Unknown, malformed, oversized, unsigned, expired, incorrectly attributed, or
tampered objects are rejected and MUST NOT be re-seeded.
