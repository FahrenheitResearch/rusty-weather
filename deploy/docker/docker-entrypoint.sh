#!/bin/sh
set -eu

install_private_file() {
    source_path="$1"
    target_path="$2"
    if [ -z "${source_path}" ]; then
        return
    fi
    umask 077
    cp "${source_path}" "${target_path}"
    chmod 0600 "${target_path}"
}

if [ -n "${RW_DOCKER_TOKEN_SOURCE:-}" ]; then
    token_target=/tmp/rusty-weather-api-tokens
    install_private_file "${RW_DOCKER_TOKEN_SOURCE}" "${token_target}"
    export RW_API_TOKEN_FILE="${token_target}"
fi

if [ -n "${RW_DOCKER_COMMUNITY_SIGNING_KEY_SOURCE:-}" ]; then
    key_target=/tmp/rusty-weather-community-signing.key
    install_private_file "${RW_DOCKER_COMMUNITY_SIGNING_KEY_SOURCE}" "${key_target}"
    export RW_COMMUNITY_SIGNING_KEY_FILE="${key_target}"
fi

if [ -n "${RW_DOCKER_GENERATION_REPLICATION_SIGNING_KEY_SOURCE:-}" ]; then
    key_target=/tmp/rusty-weather-generation-replication-signing.key
    install_private_file \
        "${RW_DOCKER_GENERATION_REPLICATION_SIGNING_KEY_SOURCE}" \
        "${key_target}"
    export RW_GENERATION_REPLICATION_SIGNING_KEY_FILE="${key_target}"
fi

# These fixed tmpfs targets are referenced by the advanced external TOML.
# They remain useful even when a release does not expose an environment
# override for the corresponding nested configuration field.
install_private_file \
    "${RW_DOCKER_COMMUNITY_RELAY_SIGNING_KEY_SOURCE:-}" \
    /tmp/rusty-weather-community-relay-signing.key
install_private_file \
    "${RW_DOCKER_CLOUDFLARE_TURN_API_TOKEN_SOURCE:-}" \
    /tmp/rusty-weather-cloudflare-turn-api.token
install_private_file \
    "${RW_DOCKER_COMMUNITY_ORIGIN_TOKEN_SOURCE:-}" \
    /tmp/rusty-weather-community-origin.token
install_private_file \
    "${RW_DOCKER_R2_GATEWAY_TOKEN_SOURCE:-}" \
    /tmp/rusty-weather-r2-gateway.token

if [ -n "${RW_DOCKER_SECRET_DIRECTORY_SOURCE:-}" ]; then
    secret_directory_target=/tmp/rusty-weather-secrets
    install -d -m 0700 "${secret_directory_target}"
    found_secret=false
    for source_path in "${RW_DOCKER_SECRET_DIRECTORY_SOURCE}"/*; do
        if [ ! -f "${source_path}" ] || [ -L "${source_path}" ]; then
            continue
        fi
        found_secret=true
        secret_name=$(basename "${source_path}")
        install_private_file \
            "${source_path}" \
            "${secret_directory_target}/${secret_name}"
    done
    if [ "${found_secret}" != true ]; then
        echo "RW_DOCKER_SECRET_DIRECTORY_SOURCE contains no regular secret files" >&2
        exit 1
    fi
fi

if [ -n "${RW_DOCKER_FEDERATION_SIGNING_KEY_SOURCE:-}" ]; then
    key_target=/tmp/rusty-weather-federation-signing.key
    install_private_file "${RW_DOCKER_FEDERATION_SIGNING_KEY_SOURCE}" "${key_target}"
    export RW_FEDERATION_SIGNING_KEY_FILE="${key_target}"
fi

exec /usr/local/bin/rw-server "$@"
