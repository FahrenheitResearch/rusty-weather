#!/bin/sh
set -eu

if [ -n "${RW_DOCKER_TOKEN_SOURCE:-}" ]; then
    token_target=/tmp/rusty-weather-api-tokens
    umask 077
    cp "${RW_DOCKER_TOKEN_SOURCE}" "${token_target}"
    chmod 0600 "${token_target}"
    export RW_API_TOKEN_FILE="${token_target}"
fi

exec /usr/local/bin/rw-server "$@"
