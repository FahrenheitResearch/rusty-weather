# Distributed product release gates

This document defines the evidence required before a distributed BowEcho and
Rusty Weather build may be described as complete or production-ready. Passing
crate unit tests is necessary, but is not sufficient.

## Delivery invariants

Operational queries use exactly this order:

1. BowEcho's verified local immutable-object cache.
2. The configured R2-compatible hot-object store.
3. The authoritative Rusty Weather HTTPS origin, including an approved
   server-side federated-origin fallback when configured.

Operational delivery never creates a TURN allocation and never asks a
community client for data.

Cold historical queries use this order:

1. BowEcho's verified local immutable-object cache.
2. A signed historical or case-room object in R2.
3. An exact-hash Community Cache lookup through encrypted TURN relay-only
   transport.
4. An archival HTTPS origin that still retains the object.
5. An honest unavailable result.

Every transition must preserve the canonical signed request identity. A miss,
timeout, quota rejection, invalid signature, or unavailable peer must fall
forward without silently changing model, run, grid, time, variables, recipe,
or geographic/vertical selection.

## Required automated evidence

The release candidate must pass, from a clean committed checkout:

- workspace formatting, locked metadata, all-target compilation, all tests,
  generated-contract drift, dependency policy, and no-dependency clippy with
  warnings denied;
- protocol golden vectors for canonical request, object, case, federation,
  relay-session, and full-generation replication signatures;
- malformed JSON, unknown schema/version, expired signature, wrong key,
  decompression bomb, oversized body, path traversal, symlink, and hash
  substitution rejection;
- exact cache-identity separation across model, run, snapshot, grid, valid
  time, variable/product, pressure level, recipe, native window, and
  geographic bounding box;
- private WRF and ArWen default denial plus explicit owner publication and
  redistribution-rights enforcement;
- ECMWF attribution and modification notices on every object, case, and
  replicated generation derived from ECMWF material;
- persistent per-principal and global byte, object, generation, concurrency,
  and calendar-month accounting across restart and month rollover;
- kill-switch, retention, tombstone, revocation, cache eviction, interrupted
  upload, idempotent retry, and crash-recovery behavior;
- origin-catalog active-plus-previous visibility and fail-closed behavior for
  stale, missing, corrupt, or independently replaced generations;
- federated-origin key rotation, revocation, quarantine, rebinding, redirect,
  private-address, wrong-object, and two-origin failover behavior;
- relay packet loss, duplicate, reorder, replay, tamper, wrong-session,
  wrong-object, expiry, quota exhaustion, provider outage, and final-hash
  behavior with bounded retries;
- proof that operational requests never enter relay code and a relay failure
  never blocks an available R2 or HTTPS result.
- immutable R2 gateway tests and a Wrangler dry-run proving public bounded
  GET, authenticated create-only PUT, precondition conflict, closed key
  grammar, and exact-byte preservation;
- a reproducible CycloneDX inventory and complete locked Node build-tool
  license bundle for the R2 Worker; and
- locked format, check, test, and dependency-policy gates for the isolated lab
  controller's separate Cargo workspace.

## Required isolated-network lab

The release lab contains separate processes or containers for:

- the authoritative Hetzner-equivalent Rusty Weather service;
- two approved public-origin services with different signing keys;
- an R2-compatible immutable-object service;
- a TURN server pinned by immutable image digest;
- an opted-in uploader and downloader using different API principals; and
- a passive packet observer outside the application processes.

The lab must perform at least one operational sounding, point series, native
window, geographic pressure field, temporal/diurnal product, typed ensemble
field, published case artifact, cold relay recovery, and complete-generation
replication. Returned payloads are compared to the authoritative local query,
including provenance and attribution.

The packet observer must prove that each ordinary client sends model-data
packets only to the configured HTTPS authority/R2 endpoints and its configured
TURN server. It must observe no UDP or TCP flow between the two client
addresses. Application-visible state, logs, metrics, errors, crash reports,
case manifests, and exported settings must contain no peer address, host ICE
candidate, or server-reflexive candidate. The lab policy that permits private
container addresses is test-only and must be impossible to activate in a
production binary or production configuration.

The release workflow preserves the successful lab's verified result, packet
proof, packet captures, and sanitized logs in the checksummed and attested
`rusty-weather-distributed-lab-evidence.tar.gz`. This archive is protocol
evidence, not a production image: the lab's private CA adaptation remains
explicitly labeled lab-only.

The TURN test must inject deterministic loss, duplication, and reordering. A
successful result requires authenticated acknowledgements, bounded
retransmission, exact final SHA-256 verification, and normal server completion
accounting. Exhausted retries must produce the configured archival fallback or
an honest unavailable result.

## Deployment and package evidence

The exact candidate images and archives must then pass:

- read-only-root, non-root UID/GID, capability-drop, no-new-privileges,
  bounded writable-volume, secret-permission, health, readiness, and graceful
  shutdown checks;
- scheduler publication of the audited HRRR hourly, complete extended HRRR,
  GFS, and NBM surface lanes with active-plus-one-previous retention;
- API consumption of that publication catalog without a raw-store bypass;
- restart recovery for Community Cache, relay accounting, federation health,
  jobs, case tombstones, and generation replication;
- internal and outer checksums, CycloneDX SBOMs, complete third-party license
  text, generated schema/OpenAPI equality, and embedded source SHA/version;
- required Windows Authenticode and macOS signing/notarization checks; and
- a BowEcho packaged-binary smoke covering the Radar and Model workflows, not
  only a library or development build.

The lab image never satisfies the production-candidate gate. CI and release
separately build the unmodified root `Dockerfile` with the exact source SHA and
run `deploy/docker/smoke-production-candidate.sh`. That gate proves non-root,
read-only-root, capability-drop, no-new-privileges, bounded PIDs, private
secret-copy permissions, readiness, writable-volume boundaries, graceful
shutdown, and the extracted service archive's internal checksums and embedded
source SHA. Tagged releases also publish an attested deterministic Git source
archive and the already-bundled R2 Worker with its own checksums, SBOM, and
Node license inventory.

## Manual workflow acceptance

The final visual and usability pass must use the packaged BowEcho binary. It
must confirm the compact Radar controls, product and color-table access,
newest/live versus loop/history behavior, exact tilt selection and low-tilt
auto-follow, RegionGlobal and explicit RIFT dealiasing, and the plot-first Model
workspace. Model acceptance includes arbitrary-domain map drawing, soundings,
point series, native and pressure-level windows, temporal/diurnal products,
typed ensemble products, case-room browsing/publication, source attribution,
failure messages, cancellation, and preservation of the last verified result.

No release is complete while a required path is represented only by a disabled
button, a mock transport, an unexecuted workflow, or an undocumented operator
assumption.
