# Docker deployment

Run commands from the repository root unless noted otherwise. These files are
source-checkout deployment helpers: binary service archives intentionally omit
them because they do not contain the Docker build context. Build only from the
matching trusted source checkout unless a release explicitly publishes and
documents an immutable image digest. The Dockerfile's BuildKit frontend,
builder, and runtime base images are pinned by multi-platform digest; review
and deliberately update those pins when taking upstream security updates.

1. Create the bind-mount directories. The container runs as numeric UID/GID
   `65532:65532`, so the API artifact directory and scheduler store/cache/state
   directories must be writable by that identity:

       sudo install -d -o 65532 -g 65532 -m 0750 \
         deploy/docker/data/store deploy/docker/data/artifacts \
         deploy/docker/data/community-cache deploy/docker/data/federation \
         deploy/docker/data/scheduler-cache deploy/docker/data/scheduler-state
       install -d -m 0700 deploy/docker/secrets

   Do not rely on Compose-created root-owned `0755` directories: `rw-server`
   must create its job database and artifacts at startup, and `rw-scheduler`
   must publish to the store and persist cache/state.
2. Put one random API token of at least 32 bytes per line in
   `deploy/docker/secrets/api-tokens.txt` and set its host permissions to
   owner-read only where the platform supports Unix modes. Compose initially
   mounts this secret read-only; the non-root entrypoint copies it with mode
   `0600` into the container's private `/tmp` tmpfs because the server rejects
   group/world-readable token files.

   Advanced features use the same copy-to-tmpfs boundary because Compose file
   secrets normally arrive with mode `0444`, while Rusty Weather deliberately
   rejects private keys or bearer tokens with group/world permission bits.
   A site-local Compose override can mount the source secret and set the
   corresponding source environment variable below. The entrypoint copies it
   to the fixed private target with mode `0600` before the server starts:

   | Source environment variable | Private target / server override |
   | --- | --- |
   | `RW_DOCKER_COMMUNITY_SIGNING_KEY_SOURCE` | `RW_COMMUNITY_SIGNING_KEY_FILE` |
   | `RW_DOCKER_GENERATION_REPLICATION_SIGNING_KEY_SOURCE` | `RW_GENERATION_REPLICATION_SIGNING_KEY_FILE` |
   | `RW_DOCKER_FEDERATION_SIGNING_KEY_SOURCE` | `RW_FEDERATION_SIGNING_KEY_FILE` |
   | `RW_DOCKER_COMMUNITY_RELAY_SIGNING_KEY_SOURCE` | `/tmp/rusty-weather-community-relay-signing.key` |
   | `RW_DOCKER_CLOUDFLARE_TURN_API_TOKEN_SOURCE` | `/tmp/rusty-weather-cloudflare-turn-api.token` |
   | `RW_DOCKER_COMMUNITY_ORIGIN_TOKEN_SOURCE` | `/tmp/rusty-weather-community-origin.token` |
   | `RW_DOCKER_R2_GATEWAY_TOKEN_SOURCE` | `/tmp/rusty-weather-r2-gateway.token` |
   | `RW_DOCKER_SECRET_DIRECTORY_SOURCE` | each regular non-symlink file under `/tmp/rusty-weather-secrets/` |

   For fixed-target rows, point the external TOML at that exact `/tmp` path.
   The directory form supports a bounded operator-managed set of federation
   origin credentials and descriptor keys without adding secret values to the
   Compose environment. It rejects an empty directory and skips symlinks;
   reference each copied basename explicitly from the external TOML.
   Never put a private key or bearer token directly in TOML or an environment
   variable; only the non-secret source file path belongs in the environment.
3. Build and start the service:

       RW_BUILD_SHA=$(git rev-parse HEAD) \
         docker compose -f deploy/docker/compose.yaml up -d --build

   The required build argument is embedded in `/v1/version`, scheduler-written
   stores, and the image's OCI revision label. Build only from a clean checkout
   whose commit is the value supplied; never substitute `unknown`.

   The safe checked-in example is mounted by default. For an audited
   deployment, copy it outside the checkout, keep the copy readable only by
   its operator group, use container paths for its state and secret files,
   and set `RW_SERVER_CONFIG_PATH` to its absolute host path:

       RW_BUILD_SHA=$(git rev-parse HEAD) \
         RW_SERVER_CONFIG_PATH=/absolute/path/rusty-weather.toml \
         docker compose -f deploy/docker/compose.yaml up -d --build

   Do not edit the checked-in example in place and do not place private keys
   or bearer tokens in TOML. Mount additional secret files read-only through a
   site-local Compose override, then reference their container paths from the
   external configuration. Run `rw-server doctor` inside the final composed
   service before exposing it.

4. Check readiness:

       docker compose -f deploy/docker/compose.yaml exec api \
         rw-server --config /etc/rusty-weather/rusty-weather.toml healthcheck

The example publishes only to host loopback. Put a TLS reverse proxy on the
same host when remote clients need access. The container root filesystem is
read-only, all Linux capabilities are removed, the model store is mounted
read-only, and only the artifact, Community Cache, and federation-state
directories are writable. Community Cache and federation remain disabled in
the shipped configuration; their dedicated mounts avoid making the entire
container filesystem or model store writable when an operator later enables
an audited configuration. Do not commit the `secrets` or `data` directories.

Full-generation replication is deliberately not enabled by this base Compose
file. It must write an atomically staged run into the model store, whereas the
ordinary API needs only a read-only store. Use the separately reviewed
replication deployment overlay and its capacity/rights controls when that
advanced feature is enabled; never change the base API store mount to
read-write merely to make a configuration error disappear.

### Advanced generation-replication overlay

[`compose.replication.yaml`](compose.replication.yaml) is the only checked-in
Compose layer that changes the API model store to read-write. It also provides
a separate durable control root and copies the replication signing key from a
Compose secret into tmpfs with mode `0600`. The overlay remains disabled,
kill-switched, and capacity/security-unapproved by default.

Create the additional paths without broadening permissions on the base mounts:

    sudo install -d -o 65532 -g 65532 -m 0750 \
      deploy/docker/data/generation-replication
    install -m 0600 /secure/source/generation-replication-ed25519.key \
      deploy/docker/secrets/generation-replication-ed25519.key

Before enablement, obtain the authenticated operator-principal SHA-256 from
the server's documented auth tooling, complete the direct disk/inode/network
capacity audit, run the exact release security/recovery suite, choose
`replication` for a replication-only university/lab origin or `union` for a
Hetzner node that also serves scheduler lanes, and keep the kill switch engaged
until doctor/readiness pass. A replication-only origin still sets
`RW_ORIGIN_CATALOG_ENABLED=true`; its publication source is `replication`, so
it does not require a scheduler catalog and exposes only engine-authorized
generations.

The deliberate enablement command is shaped as follows; use audited values and
never paste the private key itself into the environment:

    RW_BUILD_SHA=$(git rev-parse HEAD) \
      RW_SERVER_CONFIG_PATH=/absolute/path/rusty-weather.toml \
      RW_ORIGIN_CATALOG_ENABLED=true \
      RW_ORIGIN_CATALOG_PUBLICATION_SOURCES=replication \
      RW_GENERATION_REPLICATION_ENABLED=true \
      RW_GENERATION_REPLICATION_SECURITY_TESTS_PASSED=true \
      RW_GENERATION_REPLICATION_CAPACITY_AUDIT_COMPLETED=true \
      RW_GENERATION_REPLICATION_KILL_SWITCH=true \
      RW_GENERATION_REPLICATION_OPERATOR_PRINCIPALS=<auth-principal-sha256> \
      docker compose -f deploy/docker/compose.yaml \
        -f deploy/docker/compose.replication.yaml up -d --build

After startup, run `rw-server doctor`, inspect the authenticated coarse
replication/origin status, then change the kill switch through the protected
operator route. Do not restart with an environment-level false kill switch as
a substitute for that reviewed transition. Owner revocation remains explicit;
the signed `retain_until_unix` deadline is an exclusive authorization expiry,
so the engine durably tombstones and queues automatic retirement at that exact
instant. Operator GC remains available for bounded retry/recovery of terminal
storage, and opening or querying a run never publishes it.

For a prebuilt image, replace `build` with an immutable image digest. Review
the image's CycloneDX SBOM and checksum before deployment.

Release CI validates the unmodified production image independently of the
private-CA distributed lab image. Operators can reproduce that hardened smoke
from the matching clean source checkout with:

    RW_BUILD_SHA=$(git rev-parse HEAD) \
      bash deploy/docker/smoke-production-candidate.sh

Set `RW_SERVICE_ARCHIVE` to a downloaded Linux x64 service archive to verify
its inner package contract in the same run. Verify the archive's published
outer checksum before invoking the script. Tagged releases include an attested
deterministic source archive, so the Docker build context can be matched
exactly to the embedded image revision.

## Optional scheduler profile

The `scheduler` service is behind the explicit Compose profile named
`scheduler`; the normal `up` command starts only the API. The scheduler image
shares the store with the API but mounts it read-write, while its ingest cache
and durable state use separate bind mounts. The API retains its read-only store
mount.

The checked-in scheduler example is safe to inspect because retention is both
disabled and dry-run. For production, copy it outside the checkout, edit the
model allowlist and capacity limits, and provide its absolute host path through
`RW_SCHEDULER_CONFIG_PATH`.

The first-step `install -d` command creates the store, cache, and state
directories with the required ownership. Do not make the API token or
configuration writable by UID/GID `65532:65532`.

Start both services explicitly:

    RW_BUILD_SHA=$(git rev-parse HEAD) \
      RW_SCHEDULER_CONFIG_PATH=/absolute/path/scheduler.toml \
      docker compose -f deploy/docker/compose.yaml --profile scheduler up -d --build

Inspect durable state without contacting providers:

    docker compose -f deploy/docker/compose.yaml --profile scheduler exec scheduler \
      /usr/local/bin/rw-scheduler --config /etc/rusty-weather/rusty-weather-scheduler.toml status

Compose sends `SIGINT` and allows two minutes for cooperative ingest shutdown.
Enable destructive retention only after reviewing a dry-run plan and confirming
that no non-scheduler writer owns the candidate run IDs.
