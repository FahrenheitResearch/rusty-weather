# Deploying the Rusty Weather service

`rw-server` is the headless, self-hosted API for validated Rusty Weather
stores. It defaults to `127.0.0.1:8788`, refuses an unauthenticated public bind,
and does not enable cross-origin access unless exact origins are configured.

## Package contents

Release service archives contain `rw-server`, `rw-scheduler`, both example
configurations, the server JSON Schema, the generated OpenAPI document,
scheduler README, systemd and Windows deployment templates, license notices,
security policy, operations guide, R2 gateway source plus its Node license/SBOM
inventory, and a `SHA256SUMS.txt` manifest covering every file inside the
extracted top-level `rusty-weather-server` directory. Docker files remain
absent because a binary archive is not a Docker build context; use the matching
attested `rusty-weather-source.tar.gz` when an exact source build is required.
Verify the archive's `.sha256` file and review the matching `rw-server`,
`rw-scheduler`, and R2 Worker CycloneDX SBOMs before installing it. The separate
`rusty-weather-r2-gateway.tar.gz` is the already-bundled deployable Worker, not
a replacement for the conventional Rusty Weather HTTPS service.

Verify the downloaded archive before extraction. The release sidecar uses the
standard SHA-256 checksum-file format:

    # Linux
    sha256sum --check rusty-weather-server-linux-x64.tar.gz.sha256

    # macOS
    shasum -a 256 --check rusty-weather-server-macos-apple-silicon.tar.gz.sha256

On Windows, compare the first field in the `.sha256` file with:

    (Get-FileHash -Algorithm SHA256 .\rusty-weather-server-windows-x64.zip).Hash

After extraction, verify `SHA256SUMS.txt` from inside its top-level directory;
it deliberately does not include itself. Tagged public Windows packages are
Authenticode-signed, and tagged macOS packages are signed and notarized. Treat
an absent/invalid signature as a failed release verification, not a warning.
Every tagged release asset also has GitHub's signed build-provenance
attestation binding its digest to the exact repository workflow and commit.
Verify a downloaded asset with `gh attestation verify <file> --repo
FahrenheitResearch/rusty-weather`; a checksum alone detects corruption but does
not establish who built the file.

    # Linux, from the directory containing rusty-weather-server/
    (cd rusty-weather-server && sha256sum --check SHA256SUMS.txt)

    # macOS
    (cd rusty-weather-server && shasum -a 256 --check SHA256SUMS.txt)

On Windows, enter the extracted `rusty-weather-server` directory and use:

    Get-Content .\SHA256SUMS.txt | ForEach-Object {
        if ($_ -notmatch '^([0-9a-f]{64})  (.+)$') { throw "Malformed checksum: $_" }
        $path = $Matches[2].Replace('/', '\')
        $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $path).Hash
        if ($actual -ine $Matches[1]) { throw "Checksum mismatch: $path" }
    }

Then require `Valid` for both executable signatures:

    Get-AuthenticodeSignature .\rw-server.exe, .\rw-scheduler.exe |
        Select-Object Path, Status, SignerCertificate

To build from a trusted checkout instead:

    RW_BUILD_SHA=$(git rev-parse HEAD) cargo build --locked --release \
      -p rw-server --bin rw-server \
      -p rw-scheduler --bin rw-scheduler

On PowerShell, set `$env:RW_BUILD_SHA = git rev-parse HEAD` first. Use a clean
checkout and an exact commit; the explicit stamp is required when `.git` is not
present (for example in a Docker build context) and must never be `unknown`.

Windows binaries use the MSVC runtime. Install the supported Microsoft Visual
C++ Redistributable on a clean host before installing the WinSW services; a
missing `VCRUNTIME140.dll` is a deployment prerequisite failure, not a server
configuration error.

The resulting binaries are `target/release/rw-server` and
`target/release/rw-scheduler` (with `.exe` suffixes on Windows).

## Configure and validate

Copy `config/rusty-weather.example.toml` to a service-owned configuration path
and edit it. `config/rusty-weather.schema.json` describes every accepted field;
unknown TOML fields are rejected. `RW_*` environment variables override file
values; `--config` or `RW_CONFIG` selects the file.

Create separate directories for the immutable/query store and generated
artifacts. Keep the store read-only to the API service. Generate a random API
token with at least 32 bytes of entropy, put one token per line in a regular
file, and make the file readable only by the service identity. Do not put
tokens on the command line or in the TOML file.

Validate before every start:

    rw-server --config /etc/rusty-weather/rusty-weather.toml doctor

Run interactively:

    rw-server --config /etc/rusty-weather/rusty-weather.toml serve

Print the effective non-secret configuration, authoritative configuration
schema, or OpenAPI document:

    rw-server --config /etc/rusty-weather/rusty-weather.toml print-config
    rw-server config-schema
    rw-server openapi

limits.job_result_bytes bounds each content-addressed asynchronous result. It
is paired with limits.job_history_records and limits.job_retention_seconds;
expired terminal job records and objects no longer referenced by any retained
job are removed automatically at startup and before new job admission. It
defaults to 512 MiB and may not exceed 16 GiB; set it below the artifact-volume
quota and reverse-proxy download limits.

Temporal query memory has separate service budgets. `json_grid_values` and
`sync_result_values` keep synchronous JSON responses small;
`temporal_reduction_cells` and `temporal_output_values` bound asynchronous
full-domain reductions before allocation. The defaults cover HRRR's
1799-by-1059 domain for both scalar and vector summaries. Size the two async
limits together with heavy concurrency and available memory; the serialized
artifact must still fit `job_result_bytes`.

### Optional Community Cache Phase 1

Leave `[community].enabled = false` for the normal self-hosted service. Enabling
it requires a service-readable Ed25519 signing-key file containing exactly 32
base64-encoded secret bytes. The private key never belongs in TOML, environment
output, R2, logs, or a client bundle. Distribute the corresponding public key
to clients through a separately authenticated release/configuration path.
Enablement also requires `community.capacity_audit_completed = true`. Set it
only after the target origin host's disk and concurrency audit supplies the
deployment-specific quota values.

Give `community.root` its own real, writable, quota-limited directory; do not
nest it inside `store_root` or expose it as a static directory. Its durable
objects and signed manifests are an expendable cache, not a substitute for the
model store or backups. Capacity values in the example are non-production
parsing examples, not recommendations; replace them with audited values.

`community.hot_store` supports a filesystem test/local provider or an
R2-compatible authenticated HTTPS gateway with content-addressed objects and
signed-manifest blobs plus one strictly bounded renewable request pointer. The
gateway must expose exact GET/PUT semantics at `<base_url>/<bucket>/v1/...` and
`.../v2/...`, enforce TLS, keep its bearer token in `token_file`, and reject
replacement of immutable keys with different bytes. Deterministic keys are:

- `v1/objects/{object_sha256}`
- `v2/manifests/{manifest_sha256}.json`
- `v2/requests/{request_sha256}.json`

The source distribution includes a concrete Cloudflare Worker/R2 binding at
`deploy/cloudflare-r2-gateway`. It implements this exact closed key grammar,
public cacheable GETs for BowEcho, authenticated create-only immutable PUTs,
strict atomic pointer replacement, fixed byte ceilings, and no list/delete
surface. Run its tests and
Wrangler dry-run, install its bearer token as a private file, and use a TLS
custom domain before enabling promotion. BowEcho receives only the public base
URL; it never receives the gateway write token.

Configure `community.origin_base_url` only as an absolute HTTPS URL. It is the
authoritative Hetzner Rusty Weather dynamic resolve/object API and manifest
signer, not a mutable run alias. It is conventional HTTPS and is never reached
through TURN. The Phase 1 order is local CAS, R2, then Hetzner origin; a
successful origin result fills the local CAS and may be promoted to R2. The
Phase 1 server contains no relay or direct-peer transport.

Keep `community.promotion.enabled = false` until the hot gateway and cost
alerts are verified. `community.quotas.promoted_bytes_per_month` is the global
promotion cost ceiling; crossing it pauses promotion without disabling local
cache or origin queries. `RW_COMMUNITY_KILL_SWITCH=true` likewise stops hot
promotion and case publication while retaining signed normal-origin fallback.
Case publication has its own `[community.cases].enabled` gate. Typed artifact
uploads additionally require
`community.cases.artifact_publication_enabled = true`; keep it false until
rights/attribution workflows and publication quotas are verified. Complete
immutable `.rws` generation transfer is owned by the separate, default-off
`[generation_replication]` service and its advanced deployment overlay.
Enabling typed case artifacts does not enable generation replication, and
neither feature exposes an arbitrary-file, private-directory, or raw `wrfout`
upload route. Replication additionally requires its explicit security and
capacity gates, owner principals, signing key, writable same-filesystem
staging/store topology, and `origin_catalog.publication_sources` policy.
Set `community.quotas.maximum_principals` to bound the durable monthly
accounting file. Bound case retention independently with
`community.cases.maximum_cases`, `community.cases.storage_bytes`, and
`community.cases.default_retention_seconds`; expired cases are removed rather
than accumulating indefinitely.

## Docker Compose

The source-checkout example at `deploy/docker/compose.yaml` builds the
multi-stage Dockerfile and runs as numeric UID/GID `65532:65532` with a
read-only root filesystem, no Linux capabilities, a read-only model store, and
a bounded temporary filesystem. Binary service archives do not include this
source-build template. Use it only from the matching trusted checkout unless a
release publishes a documented immutable image digest. The Dockerfile pins its
BuildKit frontend plus builder and runtime base images by multi-platform
digest; update those digests intentionally when applying upstream security
updates.

Follow `deploy/docker/README.md` to create the token and data directories.
In particular, create/chown the bind mounts before Compose starts them:

    sudo install -d -o 65532 -g 65532 -m 0750 \
      deploy/docker/data/store deploy/docker/data/artifacts \
      deploy/docker/data/scheduler-cache deploy/docker/data/scheduler-state

The published port is loopback-only. When a reverse proxy is added, configure
its public hostname and TLS at the proxy. Artifact links are deliberately
relative so the same API response remains valid behind any trusted hostname.
Set body, connection, and upstream timeouts no larger than the service limits;
never use the proxy to bypass API authentication.

## systemd

The hardened unit assumes these paths:

- binary: `/usr/local/bin/rw-server`
- configuration: `/etc/rusty-weather/rusty-weather.toml`
- token file: `/etc/rusty-weather/api-tokens.txt`
- store: `/var/lib/rusty-weather/store`
- artifacts: `/var/lib/rusty-weather/artifacts`
- optional Community cache/control state:
  `/var/lib/rusty-weather/community-cache`
- optional federation health/accounting state:
  `/var/lib/rusty-weather/federation`
- optional generation-replication control state:
  `/var/lib/rusty-weather/generation-replication`

Example installation as root:

    useradd --system --home-dir /var/lib/rusty-weather --shell /usr/sbin/nologin rusty-weather
    install -d -o rusty-weather -g rusty-weather -m 0750 /var/lib/rusty-weather/store
    install -d -o rusty-weather -g rusty-weather -m 0750 /var/lib/rusty-weather/artifacts
    install -d -o root -g rusty-weather -m 0750 /etc/rusty-weather
    install -o root -g root -m 0755 rw-server /usr/local/bin/rw-server
    install -o root -g rusty-weather -m 0640 config/rusty-weather.example.toml /etc/rusty-weather/rusty-weather.toml
    install -o root -g rusty-weather -m 0640 deploy/systemd/rusty-weather.env /etc/default/rusty-weather
    install -o root -g root -m 0644 deploy/systemd/rusty-weather.service /etc/systemd/system/rusty-weather.service

Create `/etc/rusty-weather/api-tokens.txt` separately, owned by
`rusty-weather:rusty-weather` with mode `0600`. The server intentionally
rejects any Unix token file with group or other permission bits.

Create an optional state directory only when its feature is deliberately
enabled, and keep every signing key/provider token as a distinct `0600`
regular file under `/etc/rusty-weather`. The systemd unit grants those bounded
state locations write access while its more-specific `ReadOnlyPaths` rule keeps
the operational model store read-only. Advanced generation replication is the
only mode that also needs a separately reviewed writable-store policy. The
packaged `rusty-weather-generation-replication.conf` is that narrowly scoped
systemd drop-in: it resets the base store's read-only path rule and grants write
access only to the model store and the replication control root. Install it
only after the security/capacity gates and configuration in
[`GENERATION_REPLICATION.md`](GENERATION_REPLICATION.md) have been reviewed:

    install -d -o rusty-weather -g rusty-weather -m 0750 \
      /var/lib/rusty-weather/generation-replication
    install -d -o root -g root -m 0755 \
      /etc/systemd/system/rusty-weather.service.d
    install -o root -g root -m 0644 \
      deploy/systemd/rusty-weather-generation-replication.conf \
      /etc/systemd/system/rusty-weather.service.d/50-generation-replication.conf

Do not install the drop-in on an ordinary operational API node. Its filesystem
permission alone does not enable replication: the feature remains default-off,
requires its distinct signing key and operator gates, and starts kill-switched.

Then validate and start:

    sudo -u rusty-weather /usr/local/bin/rw-server --config /etc/rusty-weather/rusty-weather.toml doctor
    systemctl daemon-reload
    systemctl enable --now rusty-weather
    systemctl status rusty-weather

The unit uses facilities available on current systemd releases. If an older
distribution rejects a sandbox directive, remove only the unsupported
directive after documenting the reduced isolation; do not disable the whole
sandbox section.

## Windows service

Use the template and ACL guidance in `deploy/windows/README.md`. The template
uses the permissively licensed WinSW wrapper, which must be obtained and
verified separately. It runs under `NetworkService`, binds to loopback, sends a
graceful stop signal, and rolls logs. A dedicated managed service account is
preferred in managed environments.

## Public exposure checklist

- Terminate TLS in a maintained reverse proxy or load balancer.
- Keep API tokens enabled and rotate them without publishing them in logs.
- Bind directly to a private interface only when network policy also restricts
  clients; otherwise retain a loopback bind behind the proxy.
- Set exact `cors_origins`; never use a wildcard for credentialed browser use.
- Protect `/metrics` unless the metrics collector is isolated and trusted.
- Keep the store read-only and artifact output on a separate writable volume.
- Monitor readiness, request failures, admission rejections, queue depth,
  latency, free disk space, and upstream ingest freshness.
- Back up and restore-test according to `docs/OPERATIONS.md`.

## Scheduler deployment

`rw-scheduler` is an optional writer; `rw-server` remains useful against stores
populated by other workflows. Copy
`config/rusty-weather-scheduler.example.toml` to a service-owned path, keep its
explicit model allowlist, and retain the default disabled/dry-run retention
until an operator has reviewed candidates. Its store, cache, and state roots
must be absolute, distinct, and non-nested.

The Compose service is opt-in through `--profile scheduler`; see
`deploy/docker/README.md`. It gives only the scheduler a read-write store mount
and keeps cache/state on separate mounts.

For systemd, the hardened scheduler unit assumes:

- binary: `/usr/local/bin/rw-scheduler`
- configuration: `/etc/rusty-weather/rusty-weather-scheduler.toml`
- shared writable store: `/var/lib/rusty-weather/store`
- scheduler state: `/var/lib/rusty-weather/scheduler`
- ingest cache: `/var/cache/rusty-weather/ingest`

Install the binary, configuration, directories, and unit as root:

    install -o root -g root -m 0755 rw-scheduler /usr/local/bin/rw-scheduler
    install -d -o rusty-weather -g rusty-weather -m 0750 /var/lib/rusty-weather/store
    install -d -o rusty-weather -g rusty-weather -m 0750 /var/lib/rusty-weather/scheduler
    install -d -o rusty-weather -g rusty-weather -m 0750 /var/cache/rusty-weather/ingest
    install -o root -g rusty-weather -m 0640 config/rusty-weather-scheduler.example.toml /etc/rusty-weather/rusty-weather-scheduler.toml
    install -o root -g root -m 0644 deploy/systemd/rusty-weather-scheduler.service /etc/systemd/system/rusty-weather-scheduler.service
    sudo -u rusty-weather /usr/local/bin/rw-scheduler --config /etc/rusty-weather/rusty-weather-scheduler.toml plan
    systemctl daemon-reload
    systemctl enable --now rusty-weather-scheduler

The API unit keeps its mount namespace's store read-only even though the shared
`rusty-weather` identity must own the store for scheduler publication. The
scheduler unit uses `SIGINT` and a two-minute stop timeout so cancellation is
observed at ingest stage boundaries. Use `rw-scheduler ... status` to inspect
durable state without provider access.

On Windows, install the separate WinSW template and ACLs described in
`deploy/windows/README.md`. Do not point two scheduler processes at the same
state/store roots.
