# syntax=docker/dockerfile:1.7@sha256:a57df69d0ea827fb7266491f2813635de6f17269be881f696fbfdf2d83dda33e

FROM rust:1.92-bookworm@sha256:e90e846de4124376164ddfbaab4b0774c7bdeef5e738866295e5a90a34a307a2 AS builder

ARG RW_BUILD_SHA
RUN test -n "${RW_BUILD_SHA}" && test "${RW_BUILD_SHA}" != unknown
ENV RW_BUILD_SHA=${RW_BUILD_SHA}
WORKDIR /src
COPY . .
RUN cargo install --locked --version 0.8.4 cargo-about \
    && cargo about generate --locked --workspace --all-features --fail --output-file /tmp/THIRD_PARTY_LICENSES.html about.hbs \
    && test -s /tmp/THIRD_PARTY_LICENSES.html \
    && cargo build --locked --release \
        -p rw-server --bin rw-server \
        -p rw-scheduler --bin rw-scheduler \
    && target/release/rw-server config-schema > /tmp/rusty-weather.schema.json \
    && target/release/rw-server openapi > /tmp/rusty-weather.openapi.json

FROM debian:bookworm-slim@sha256:abd67ffcfa541b485a3dff59865ab629aa048a6c613e639d36e7456b0b229241 AS runtime

ARG RW_BUILD_SHA
ARG RW_UID=65532
ARG RW_GID=65532

RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid "${RW_GID}" rusty-weather \
    && useradd --uid "${RW_UID}" --gid "${RW_GID}" \
        --home-dir /var/lib/rusty-weather --no-create-home \
        --shell /usr/sbin/nologin rusty-weather \
    && install -d -o "${RW_UID}" -g "${RW_GID}" -m 0750 \
        /var/lib/rusty-weather/store /var/lib/rusty-weather/artifacts \
        /var/lib/rusty-weather/community-cache /var/lib/rusty-weather/federation \
        /var/lib/rusty-weather/scheduler /var/cache/rusty-weather/ingest \
        /var/cache/rusty-weather/server \
    && install -d -o root -g "${RW_GID}" -m 0750 /etc/rusty-weather

COPY --from=builder /src/target/release/rw-server /usr/local/bin/rw-server
COPY --from=builder /src/target/release/rw-scheduler /usr/local/bin/rw-scheduler
COPY --chown=root:rusty-weather --chmod=0440 \
    config/rusty-weather.example.toml /etc/rusty-weather/rusty-weather.toml
COPY --chown=root:rusty-weather --chmod=0440 \
    config/rusty-weather-scheduler.example.toml /etc/rusty-weather/rusty-weather-scheduler.toml
COPY --from=builder --chmod=0444 \
    /tmp/rusty-weather.schema.json /usr/share/doc/rusty-weather/rusty-weather.schema.json
COPY --from=builder --chmod=0444 \
    /tmp/rusty-weather.openapi.json /usr/share/doc/rusty-weather/openapi.json
COPY --chmod=0444 crates/rw-scheduler/README.md \
    /usr/share/doc/rusty-weather/RW_SCHEDULER.md
COPY --chmod=0444 README.md SECURITY.md LICENSE THIRD_PARTY_NOTICES.md \
    /usr/share/doc/rusty-weather/
COPY --chmod=0444 docs/DEPLOYMENT.md docs/OPERATIONS.md docs/SERVICE_V1.md \
    docs/DATA_SOURCES.md docs/MODEL_SUPPORT.md docs/REDUCTIONS.md \
    docs/COMMUNITY_CACHE_PROTOCOL.md docs/COMMUNITY_CACHE_THREAT_MODEL.md \
    docs/FEDERATION.md docs/FEDERATION_PROXY.md docs/ORIGIN_CATALOG.md docs/GENERATION_REPLICATION.md \
    docs/DISTRIBUTED_RELEASE_GATES.md \
    /usr/share/doc/rusty-weather/
COPY --from=builder --chmod=0444 /tmp/THIRD_PARTY_LICENSES.html \
    /usr/share/doc/rusty-weather/THIRD_PARTY_LICENSES.html
COPY --chmod=0444 crates/rustwx-render/assets/fonts/SourceSans3-LICENSE.md \
    /usr/share/doc/rusty-weather/third-party-licenses/SourceSans3-LICENSE.md
COPY --chmod=0444 vendor/ecape-rs/LICENSE \
    /usr/share/doc/rusty-weather/third-party-licenses/ecape-rs-LICENSE
COPY --chmod=0444 vendor/sharprs/LICENSE \
    /usr/share/doc/rusty-weather/third-party-licenses/sharprs-LICENSE
COPY --chmod=0444 vendor/sharprs/PROVENANCE.md \
    /usr/share/doc/rusty-weather/third-party-licenses/sharprs-PROVENANCE.md
COPY --chmod=0444 vendor/netcrust/LICENSE-MIT \
    /usr/share/doc/rusty-weather/third-party-licenses/netcrust-LICENSE-MIT
COPY --chmod=0444 vendor/netcrust/LICENSE-APACHE \
    /usr/share/doc/rusty-weather/third-party-licenses/netcrust-LICENSE-APACHE
COPY --chmod=0444 vendor/wrf-rust/LICENSE \
    /usr/share/doc/rusty-weather/third-party-licenses/wrf-rust-LICENSE
COPY --chmod=0444 vendor/wrf-rust/PROVENANCE.md \
    /usr/share/doc/rusty-weather/third-party-licenses/wrf-rust-PROVENANCE.md
COPY --chmod=0555 deploy/docker/docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh

WORKDIR /var/lib/rusty-weather
USER ${RW_UID}:${RW_GID}
LABEL org.opencontainers.image.source="https://github.com/FahrenheitResearch/rusty-weather" \
      org.opencontainers.image.revision="${RW_BUILD_SHA}" \
      org.opencontainers.image.licenses="MIT AND BSD-3-Clause"
EXPOSE 8788
STOPSIGNAL SIGTERM

HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 \
    CMD ["/usr/local/bin/rw-server", "--config", "/etc/rusty-weather/rusty-weather.toml", "healthcheck", "--timeout-seconds", "4"]

ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]
CMD ["--config", "/etc/rusty-weather/rusty-weather.toml", "serve"]
