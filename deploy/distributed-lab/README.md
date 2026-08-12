# Rusty Weather isolated distributed release lab

This directory is a destructive-to-its-own-runtime, reproducible release lab for
the distributed Rusty Weather path. It never publishes ports to the host and its
Docker bridge is `internal: true`. The synthetic `11.231.0.0/24` addresses are
used only inside that isolated bridge because production connectors correctly
reject loopback, RFC1918, link-local, and documentation ranges.

The lab exercises real application seams, not HTTP response stubs:

- one authoritative resolver/signing node behind HTTPS;
- two independently signed federated origins behind HTTPS, with Alpha empty so
  resolution must fail over to the replicated Beta generation;
- signed, chunked, resumable `.rws` generation replication into Beta;
- the production Cloudflare R2 gateway Worker under local Wrangler R2, speaking
  HTTPS and enforcing immutable content-addressed writes;
- an exact cold object absent from R2, advertised by one principal and fetched
  by a different principal through coturn;
- the production Cloudflare credential-provider contract, served by a bounded
  lab double at the exact provider hostname;
- a baseline TURN path, plus opt-in deterministic loss, duplication, delay,
  and reordering after the baseline is green;
- packet captures from both client network namespaces and `tshark` assertions
  that no uploader/downloader packet is direct.

## Run

Docker Engine with Compose is the only required runtime. From a clean checkout
on Windows:

```powershell
./deploy/distributed-lab/run.ps1
```

On Linux/macOS:

```sh
./deploy/distributed-lab/run.sh
```

`CONFIG_ONLY=1 ./deploy/distributed-lab/run.sh` or
`./deploy/distributed-lab/run.ps1 -ConfigOnly` provisions deterministic fixture
configuration, validates Compose, checks every external image digest, and stops.
Use `KEEP_RUNNING=1` or `-KeepRunning` to inspect containers after a run. A later
run deliberately replaces only `deploy/distributed-lab/runtime`; `down` removes
lab containers and anonymous/named volumes, while the evidence bind mount stays.

The final gate is `runtime/results/verified.json`. Supporting evidence includes
the replication receipt, federated resolution, R2 immutable-replay rejection,
exact recovered bytes, both client results, settled broker/quota accounting,
sanitized component logs, two pcaps, and `packet-proof.json`.

The baseline and deterministic impairment profiles pass on the frozen source.
The downloader sends an authenticated, session/object-bound ReceiverReady
marker to establish its TURN permission before waiting; payload chunks, ACKs,
completion, and receipt remain bounded and end-to-end authenticated. The final
accounting gate requires two distinct principals, exact upload/download byte
charges, zero active/reserved quota, no pending session, both role credentials
revoked, and the successful-recovery hot-promotion signal.

After a baseline succeeds, exercise deterministic TURN loss, duplication,
delay, and reordering with:

```powershell
$env:LAB_ENABLE_NETEM = '1'
./deploy/distributed-lab/run.ps1
```

```sh
LAB_ENABLE_NETEM=1 ./deploy/distributed-lab/run.sh
```

The impairment capture must show bounded retransmission while still producing
the exact original SHA-256 and zero direct client-to-client packets.

## Security boundary

No direct-connect test exemption exists. The generated origin descriptors use
HTTPS DNS names, every resolved endpoint remains subject to
`PublicInternetOnly`, the TURN route is restricted to `11.231.0.15/32`, and the
relay library still rejects direct candidates. The fake provider emits one UDP
`turn:` URL and never emits STUN, host, or server-reflexive candidates.

An isolated container cannot obtain publicly trusted certificates for internal
names. `Dockerfile.rw-lab` therefore copies the source into an ephemeral build
stage and `apply-lab-ca.sh` changes only four WebPKI root selections in that
container build to the generated lab CA. The script fails unless the production
DNS/global-address and direct-candidate guards are still present. The checkout is
not patched, TLS verification is not disabled, images are labeled lab-only, and
the resulting server image must never be deployed. This is the sole test-only
allowance; production DNS and direct-IP policy are unchanged.

The relay operator can necessarily observe connection metadata. Neither client
receives the other's address in control-plane state, and the result verifier
fails if client-visible evidence contains either client IP, direct-candidate
terminology, or a STUN URL. Payload bytes are end-to-end encrypted and are
accepted only after the origin signature, canonical identity, size limits, and
final SHA-256 all verify.

## Topology and pinned dependencies

| Address | Role |
|---|---|
| `11.231.0.10` | authoritative HTTPS edge |
| `11.231.0.11` | Alpha university HTTPS edge |
| `11.231.0.12` | Beta public-origin HTTPS edge |
| `11.231.0.13` | HTTPS Wrangler/R2 immutable hot store |
| `11.231.0.14` | Cloudflare TURN credential API lab edge |
| `11.231.0.15` | audited coturn relay |
| `11.231.0.21` | uploader principal/network namespace |
| `11.231.0.22` | downloader principal/network namespace |

All third-party Compose images use full manifest digests: Caddy starts with
`4c6e91`, Node with `6f7b03`, coturn with `71c3c9`, Netshoot with `47b907`, and
Alpine with `251091`. The three local images inherit the
digest-pinned Rust, Debian, and Node bases in their Dockerfiles. The run scripts
also inspect the resolved Compose image list and fail if an external tag is not
digest-pinned.

The topology is intentionally a release gate, not a production deployment
template. Real federation descriptors, CA-issued certificates, Cloudflare/R2
credentials, capacity values, retention, and public DNS remain operator-owned
deployment inputs.
