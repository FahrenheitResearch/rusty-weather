# Full run-generation replication

`rw-generation-replication` is the network-neutral, fail-closed engine for the
advanced opt-in replication of a complete Rusty Weather `.rws` generation. It
does not implement peer discovery, relay transport, or automatic publication.
`rw-server` now supplies the bounded authenticated HTTP and operator seams,
while the BowEcho owner-facing Publish workflow remains a release blocker.
This path is independent of operational local/R2/HTTPS delivery and of rare
object recovery through the Community Cache relay.

## Closed object contract

A generation consists only of the exact files named by the signed protocol
manifest:

- one `run.json`;
- one `grid.rwg`; and
- the `.rws` hour files registered in `run.json`.

The contract has no path, directory, URL, raw `wrfout`, or arbitrary-file
field. Every filename is one safe cross-platform store component. Every file
is an ordered sequence of bounded SHA-256 chunks with exact offsets and sizes,
and every reconstructed file has its own exact SHA-256 and size.

Every replication upload is client-authored and therefore denied unless the
owner performs an explicit publication action and confirms redistribution
rights. `PublicProvider` is rejected even when model/provenance labels look
plausible; the authority attests the exact owner-bound generation it validated,
not unverified upstream identity. Use `PrivateWrf`, `PrivateArwen`, or
`UserProvided`, and preserve public NOAA/ECMWF lineage in provenance and
attribution. Owner publication also requires attribution. ECMWF-derived generations retain the
ECMWF CC BY 4.0 attribution and a non-empty modification notice in the signed
manifest. Revocation creates a durable owner-bound rights-withdrawal tombstone;
the same generation id cannot silently be published again.

`retain_until_unix` has one exact meaning: it is the **exclusive expiry of
publication authorization**. The generation is eligible immediately before
that second and ineligible at that second. It is not a minimum-custody promise,
a billing reservation, or permission to keep serving until a later cleanup
job. Operators that want a minimum custody commitment must express and enforce
that separately; it cannot weaken this signed expiry.

## Source and local snapshot identities

`source_snapshot_id` is signed lineage from the publishing machine. Rusty
Weather snapshot identity incorporates the local `run.json` file identity, so
it is intentionally not portable across filesystems. Finalization opens the
installed run through `rw-query` and returns `local_snapshot_id` in
`PublishedRunGeneration`. Consumers use the local id for local cache/query
identity and retain the source id only as provenance. The engine never claims
that the two values are equal.

## Resumable lifecycle

1. `begin` validates the complete protocol manifest, owner binding, rights,
   retention, paths, limits, storage/count/concurrency quotas, and kill switch.
2. `missing_chunks` pages the exact missing SHA-256 objects with a bounded,
   canonical cursor. Existing chunks are rehashed before reuse.
3. `upload_chunk` accepts only a hash and exact byte length already present in
   the manifest. Hash/size validation occurs before admission. The chunk is
   written to a create-new candidate, fsynced, and renamed into its immutable
   content-addressed location.
4. Upload state and content objects survive restart. Finalization is refused
   until every required chunk is present and reverified.
5. Files are reconstructed into a same-filesystem hidden staging directory.
   The engine verifies exact file hashes and sizes, parses and validates
   `run.json`, requires exact model/run/grid/hour filename/storage-slot/valid
   time/source-provenance identity, performs `ValidateDepth::Deep`, and opens a
   `RunSnapshot` before publication.
6. A new run directory is installed with a same-filesystem rename. Its signed
   origin manifest and durable publication state are retained. Finalization
   returns the source and newly computed local snapshot ids.
7. At `retain_until_unix`, any publication lookup or garbage-collection pass
   atomically removes the generation from the authorized set and persists a
   terminal expiry tombstone plus authenticated retirement work before it can
   return bytes or catalog visibility.

## Visibility and replacement atomicity

The engine never overwrites or temporarily removes an existing different run.
Cross-platform replacement of a non-empty directory cannot provide a proven
old-or-new namespace view on every supported Windows filesystem. Therefore:

- an absent destination may be installed by same-filesystem directory rename;
- an already-present byte-exact generation may be adopted idempotently as
  crash recovery; and
- any different, incomplete, invalid, or unprovable destination fails closed
  with `Conflict` and remains untouched.

Directory publication and durable control-state replacement cannot form one
filesystem transaction. The service consequently exposes `authorize_query`,
which permits only a durable published record whose current local snapshot id
still matches. A crash after directory rename but before state commit leaves an
unauthorized exact orphan; restart/finalize can adopt it. Server/catalog wiring
must call this gate and must not expose replication-owned directories via a raw
`StoreCatalog` scan. `PublishedStoreCatalog` does this with one of three explicit
publication-source policies: `scheduler`, `replication`, or `union`. Scheduler
mode requires a fresh scheduler catalog. Replication mode requires a healthy
durable replication authority and exposes only its exact publications. Union
requires both and fails the complete view closed if a replicated model/run
collides with a scheduler publication.

Scheduler retention deletes only directories carrying its authenticated
`.rw-scheduler-owner.json` marker. The scheduler refuses to claim a preexisting
non-empty unowned run, while replication refuses to adopt a scheduler-marked
run. Replicated publications therefore remain outside scheduler active/previous
retention without permitting either process to replace the other authority's
generation.

Expiry is monotonic in durable state. Once the deadline is observed and the
terminal transition commits, a backward wall clock and a process restart still
find only the tombstone; neither can reconstruct publication rights. Excessive
backward clock skew before expiry fails closed. Physical deletion is decoupled
from authorization: contention or a cleanup error leaves authenticated pending
retirement work, never a visible publication, and a later bounded pass retries
it. An inability to commit authenticated state returns an error and does not
authorize the request.

If durable state persistence fails during normal finalization, the just-added
directory is rolled back into staging and removed. A differing existing run is
never used as rollback material because it was never moved. Revocation commits
the tombstone before attempting physical retirement, so a disk cleanup failure
cannot restore publication rights; `run.json` is removed first when retirement
succeeds.

## Durable quotas and abuse controls

The signed persistent state tracks uploads, publications, tombstones, the kill
switch, and UTC-calendar-month upload accounting. Policy bounds:

- owner and global reserved storage;
- owner and global generation count;
- owner and global concurrent uploads;
- upload lifetime and manifest/state sizes; and
- owner and global monthly accepted upload bytes.

Monthly accounting is deliberately conservative. Every manifest-authorized,
hash-valid chunk request is charged before disk admission, including a replay
of an already cached chunk. A later disk failure does not refund it. Invalid
hashes and undeclared objects are rejected before charging. The ledger is
authenticated and atomically persisted, survives restart, and rolls only when
a valid upload enters a new UTC calendar month.

Garbage collection expires unfinished uploads, removes chunks no longer
referenced by an upload or publication, removes abandoned chunk candidates,
deletes orphan signed manifests, and drains terminal publication retirements
within explicit entry, generation, and deletion budgets. Expiring publication
state is bounded by the configured maximum generation count; each physical pass
is bounded separately. Cleanup removes `run.json` first so even an accidental
raw store scan cannot recognize a partially retired generation.
The service is single-process per control root through an OS advisory lock;
final installation also coordinates with the rw-store per-run writer lock.

## Server contract

The protected API provides:

- `GET /v1/community/generation-replication/owner` for this caller's
  replication-domain-separated owner hash;
- `POST /v1/community/generations` for begin/idempotent resume;
- owner-bound status and cursor-bounded missing-chunk reads;
- exact `application/octet-stream` chunk upload with a route-specific body
  limit, empty-body rejection, and hash/size/inventory validation;
- finalize and durable owner revocation; and
- operator-only coarse status, kill-switch, and bounded GC routes.

Every response is private/no-store. Operator status contains aggregate health,
authorized publication counts/bytes, pending-retirement counts/bytes, and
tombstone totals only—never owner IDs, model/run IDs, paths, source URLs, or
credentials. The GC response separately reports expired publications, completed
generation retirements, and retirement work still pending after the bounded
pass. The existing authenticated principal is hashed again under the
`rw-server-generation-replication-owner-v1` domain before it enters a manifest.

Configuration is default-off. Enablement requires API authentication,
`origin_catalog.enabled`, publication source `replication` or `union`, an
isolated durable control root, a distinct Ed25519 signing key, at least one
operator principal, `security_tests_passed = true`, and
`capacity_audit_completed = true`. Keep the durable kill switch engaged until
the audited deployment and recovery suite pass.

The packaged systemd service keeps the ordinary query store read-only. A
replication node must deliberately install
`deploy/systemd/rusty-weather-generation-replication.conf` as
`/etc/systemd/system/rusty-weather.service.d/50-generation-replication.conf`.
That reviewed drop-in resets only the base store path rule and makes the store
and isolated replication control root writable; it does not enable the feature
or relax any protocol, rights, quota, or kill-switch gate. Container operators
use the equivalent `deploy/docker/compose.replication.yaml` overlay.

## Remaining release prerequisites

Do not expose this feature until the direct host-capacity audit supplies final
disk/inode/concurrency/bandwidth/retention values, signing-key custody and
rotation are operational, deployment recovery and multi-node smoke evidence is
recorded, and BowEcho provides a deliberate owner Publish UI with explicit
rights/provenance/attribution confirmation. Normal operational Hetzner/R2
delivery does not use this engine or TURN. Full-generation replication remains
a separate advanced opt-in feature.
