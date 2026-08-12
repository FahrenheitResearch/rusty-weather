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
3. Build and start the service:

       RW_BUILD_SHA=$(git rev-parse HEAD) \
         docker compose -f deploy/docker/compose.yaml up -d --build

   The required build argument is embedded in `/v1/version`, scheduler-written
   stores, and the image's OCI revision label. Build only from a clean checkout
   whose commit is the value supplied; never substitute `unknown`.

4. Check readiness:

       docker compose -f deploy/docker/compose.yaml exec api \
         rw-server --config /etc/rusty-weather/rusty-weather.toml healthcheck

The example publishes only to host loopback. Put a TLS reverse proxy on the
same host when remote clients need access. The container root filesystem is
read-only, all Linux capabilities are removed, the model store is mounted
read-only, and only the artifact directory is writable. Do not commit the
`secrets` or `data` directories.

For a prebuilt image, replace `build` with an immutable image digest. Review
the image's CycloneDX SBOM and checksum before deployment.

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
