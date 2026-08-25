#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH= cd -- "${script_dir}/../.." && pwd)
build_sha=${RW_BUILD_SHA:?set RW_BUILD_SHA to the exact 40-character source commit}
image=${RW_CANDIDATE_IMAGE:-rusty-weather:production-candidate}
service_archive=${RW_SERVICE_ARCHIVE:-}

case "${build_sha}" in
  *[!0-9a-f]*|'')
    printf '%s\n' 'RW_BUILD_SHA must be a lowercase hexadecimal source commit.' >&2
    exit 1
    ;;
esac
if [ "${#build_sha}" -ne 40 ]; then
  printf '%s\n' 'RW_BUILD_SHA must contain exactly 40 hexadecimal characters.' >&2
  exit 1
fi
if [ "$(git -C "${repo_root}" rev-parse HEAD)" != "${build_sha}" ]; then
  printf '%s\n' 'RW_BUILD_SHA does not match the checked-out source commit.' >&2
  exit 1
fi
if ! git -C "${repo_root}" diff --quiet -- \
  || ! git -C "${repo_root}" diff --cached --quiet --
then
  printf '%s\n' 'Production-candidate smoke rejects tracked or staged source changes.' >&2
  exit 1
fi

runtime_dir=$(mktemp -d)
candidate_source="${runtime_dir}/source"
container="rw-production-candidate-$$"
store_volume="${container}-store"
artifact_volume="${container}-artifacts"
community_volume="${container}-community"
federation_volume="${container}-federation"
cache_volume="${container}-cache"

cleanup() {
  docker rm -f "${container}" >/dev/null 2>&1 || true
  for volume in \
    "${store_volume}" "${artifact_volume}" \
    "${community_volume}" "${federation_volume}" \
    "${cache_volume}"
  do
    docker volume rm "${volume}" >/dev/null 2>&1 || true
  done
  rm -rf "${runtime_dir}"
}
trap cleanup EXIT INT TERM

printf '%s\n' 'production-candidate-smoke-token-0123456789abcdef' \
  > "${runtime_dir}/api-tokens.txt"
chmod 0444 "${runtime_dir}/api-tokens.txt"
mkdir "${candidate_source}"
git -C "${repo_root}" archive --format=tar "${build_sha}" \
  > "${runtime_dir}/source.tar"
test "$(git -C "${repo_root}" get-tar-commit-id \
  < "${runtime_dir}/source.tar")" = "${build_sha}"
tar -xf "${runtime_dir}/source.tar" -C "${candidate_source}"
rm "${runtime_dir}/source.tar"

docker build \
  --file "${candidate_source}/Dockerfile" \
  --build-arg "RW_BUILD_SHA=${build_sha}" \
  --tag "${image}" \
  "${candidate_source}"

revision=$(docker image inspect --format '{{ index .Config.Labels "org.opencontainers.image.revision" }}' "${image}")
test "${revision}" = "${build_sha}"
test "$(docker image inspect --format '{{ .Config.User }}' "${image}")" = '65532:65532'

for volume in \
  "${store_volume}" "${artifact_volume}" \
  "${community_volume}" "${federation_volume}" \
  "${cache_volume}"
do
  docker volume create "${volume}" >/dev/null
done

docker run --detach --name "${container}" \
  --network none \
  --read-only \
  --user 65532:65532 \
  --cap-drop ALL \
  --security-opt no-new-privileges:true \
  --pids-limit 256 \
  --tmpfs /tmp:rw,noexec,nosuid,size=64m,mode=1777 \
  --env RW_LISTEN=0.0.0.0:8788 \
  --env RW_STORE_ROOT=/var/lib/rusty-weather/store \
  --env RW_ARTIFACT_ROOT=/var/lib/rusty-weather/artifacts \
  --env RW_CACHE_ROOT=/var/cache/rusty-weather/server \
  --env RW_DOCKER_TOKEN_SOURCE=/run/secrets/rw_api_tokens \
  --mount "type=bind,src=${candidate_source}/config/rusty-weather.example.toml,dst=/etc/rusty-weather/rusty-weather.toml,readonly" \
  --mount "type=bind,src=${runtime_dir}/api-tokens.txt,dst=/run/secrets/rw_api_tokens,readonly" \
  --mount "type=volume,src=${store_volume},dst=/var/lib/rusty-weather/store,readonly" \
  --mount "type=volume,src=${artifact_volume},dst=/var/lib/rusty-weather/artifacts" \
  --mount "type=volume,src=${community_volume},dst=/var/lib/rusty-weather/community-cache" \
  --mount "type=volume,src=${federation_volume},dst=/var/lib/rusty-weather/federation" \
  --mount "type=volume,src=${cache_volume},dst=/var/cache/rusty-weather/server" \
  "${image}" >/dev/null

attempt=0
health=starting
while [ "${attempt}" -lt 60 ]; do
  health=$(docker inspect --format '{{ if .State.Health }}{{ .State.Health.Status }}{{ else }}missing{{ end }}' "${container}")
  [ "${health}" = healthy ] && break
  [ "${health}" = unhealthy ] && break
  attempt=$((attempt + 1))
  sleep 1
done
if [ "${health}" != healthy ]; then
  docker logs "${container}" >&2
  printf 'production candidate health status: %s\n' "${health}" >&2
  exit 1
fi

test "$(docker inspect --format '{{ .HostConfig.ReadonlyRootfs }}' "${container}")" = true
test "$(docker inspect --format '{{ .HostConfig.Privileged }}' "${container}")" = false
test "$(docker inspect --format '{{ .HostConfig.PidsLimit }}' "${container}")" = 256
docker inspect --format '{{ json .HostConfig.CapDrop }}' "${container}" | grep -q '"ALL"'
docker inspect --format '{{ json .HostConfig.SecurityOpt }}' "${container}" | grep -q 'no-new-privileges'
test "$(docker exec "${container}" stat -c '%a:%u:%g' /tmp/rusty-weather-api-tokens)" = '600:65532:65532'
docker exec "${container}" sh -c 'test ! -w /usr/share/doc/rusty-weather'
docker exec "${container}" sh -c 'touch /var/lib/rusty-weather/artifacts/.candidate-smoke && rm /var/lib/rusty-weather/artifacts/.candidate-smoke'
docker exec "${container}" /usr/local/bin/rw-server \
  --config /etc/rusty-weather/rusty-weather.toml \
  healthcheck --timeout-seconds 4
docker stop --time 15 "${container}" >/dev/null

if [ -n "${service_archive}" ]; then
  archive=$(CDPATH= cd -- "$(dirname -- "${service_archive}")" && pwd)/$(basename -- "${service_archive}")
  test -f "${archive}"
  mkdir "${runtime_dir}/archive"
  tar -xzf "${archive}" -C "${runtime_dir}/archive"
  package="${runtime_dir}/archive/rusty-weather-server"
  test -d "${package}"
  (
    cd "${package}"
    sha256sum --check SHA256SUMS.txt
  )
  test -f "${package}/deploy/systemd/rusty-weather-generation-replication.conf"
  test -f "${package}/deploy/cloudflare-r2-gateway/THIRD_PARTY_LICENSES-node.md"
  test -f "${package}/deploy/cloudflare-r2-gateway/rusty-weather-r2-gateway.cdx.json"
  test ! -d "${package}/deploy/cloudflare-r2-gateway/node_modules"
  "${package}/rw-server" --version
  "${package}/rw-scheduler" --version
  python3 - "${package}/BUILD-MANIFEST.json" "${build_sha}" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as handle:
    manifest = json.load(handle)
if manifest.get("source_commit") != sys.argv[2]:
    raise SystemExit("service archive build manifest does not match RW_BUILD_SHA")
PY
fi

printf '%s\n' 'Unmodified production image and packaged service candidate passed.'
