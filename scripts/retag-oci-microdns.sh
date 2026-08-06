#!/usr/bin/env bash
# Retag the microdns linux/arm64 OCI artifact from :main to a release tag and
# latest-release. Injects .microdns.version ELF section.
# Usage: retag-oci-microdns.sh <release-tag>   e.g. v0.1.0
set -euo pipefail

RELEASE_TAG="${1:?usage: $0 <release-tag>}"
IMAGE="${MICRODNS_OCI_IMAGE:-ghcr.io/dcc-bigfred/microdns-linux-arm64}"
BIN_MEDIA_TYPE="application/vnd.dcc-bigfred.microdns.linux.arm64.v1"
TAG_COMMIT="${GITHUB_SHA:?GITHUB_SHA required (tag commit)}"

tmpdir="$(mktemp -d)"
cleanup() { rm -rf "${tmpdir}"; }
trap cleanup EXIT

echo "Pulling ${IMAGE}:main…"
oras pull "${IMAGE}:main" -o "${tmpdir}"

find_layer() {
  local want="$1"
  if [[ -f "${tmpdir}/${want}" ]]; then
    echo "${want}"
    return 0
  fi
  mapfile -t files < <(find "${tmpdir}" -type f \
    ! -name 'manifest.json' ! -name 'config.json' \
    -name "${want}" -printf '%f\n')
  if [[ ${#files[@]} -eq 1 ]]; then
    echo "${files[0]}"
    return 0
  fi
  return 1
}

BIN_NAME="$(find_layer microdns-linux-arm64)" || true
if [[ -z "${BIN_NAME}" ]]; then
  echo "error: expected microdns-linux-arm64 in OCI artifact, found:" >&2
  find "${tmpdir}" -type f >&2
  exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
TAG_COMMIT_SHORT="${TAG_COMMIT:0:7}"
"${SCRIPT_DIR}/inject-elf-version.sh" "${tmpdir}/${BIN_NAME}" "${RELEASE_TAG}" "${TAG_COMMIT_SHORT}"

annotate=(
  --annotation "org.opencontainers.image.source=${GITHUB_SERVER_URL:-https://github.com}/${GITHUB_REPOSITORY:-dcc-bigfred/microdns}"
  --annotation "org.opencontainers.image.revision=${TAG_COMMIT}"
  --annotation "org.opencontainers.image.version=${RELEASE_TAG}"
  --annotation "org.opencontainers.image.title=microdns"
)

echo "Publishing ${IMAGE}:${RELEASE_TAG} and :latest-release"
echo "  microdns: $(wc -c < "${tmpdir}/${BIN_NAME}") bytes"
(
  cd "${tmpdir}"
  oras push "${IMAGE}:${RELEASE_TAG}" "${BIN_NAME}:${BIN_MEDIA_TYPE}" "${annotate[@]}"
  oras push "${IMAGE}:latest-release" "${BIN_NAME}:${BIN_MEDIA_TYPE}" "${annotate[@]}"
)
