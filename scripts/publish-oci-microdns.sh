#!/usr/bin/env bash
# Publish microdns linux/arm64 OCI artifact to GHCR (ORAS).
# Intended for CI on push to main/master only.
# Usage: publish-oci-microdns.sh <microdns-linux-arm64>
#
# Tags: main, sha-<7>
set -euo pipefail

BIN="${1:?usage: $0 <microdns-linux-arm64>}"
IMAGE="${MICRODNS_OCI_IMAGE:-ghcr.io/dcc-bigfred/microdns-linux-arm64}"
BIN_MEDIA_TYPE="application/vnd.dcc-bigfred.microdns.linux.arm64.v1"

if [[ ! -f "${BIN}" ]]; then
  echo "error: binary not found: ${BIN}" >&2
  exit 1
fi

BRANCH="${GITHUB_REF_NAME:?GITHUB_REF_NAME required}"
if [[ "${BRANCH}" != "master" && "${BRANCH}" != "main" ]]; then
  echo "error: OCI publish is only allowed from master/main (got ${BRANCH})" >&2
  exit 1
fi

SHA_TAG="sha-${GITHUB_SHA::7}"

tmpdir="$(mktemp -d)"
cleanup() { rm -rf "${tmpdir}"; }
trap cleanup EXIT

cp -f "${BIN}" "${tmpdir}/microdns-linux-arm64"
chmod 755 "${tmpdir}/microdns-linux-arm64"

annotate=(
  --annotation "org.opencontainers.image.source=${GITHUB_SERVER_URL}/${GITHUB_REPOSITORY}"
  --annotation "org.opencontainers.image.revision=${GITHUB_SHA}"
  --annotation "org.opencontainers.image.title=microdns"
)

echo "Publishing ${IMAGE}:main and :${SHA_TAG}"
echo "  microdns: $(wc -c < "${tmpdir}/microdns-linux-arm64") bytes"
(
  cd "${tmpdir}"
  oras push "${IMAGE}:main" "microdns-linux-arm64:${BIN_MEDIA_TYPE}" "${annotate[@]}"
  oras push "${IMAGE}:${SHA_TAG}" "microdns-linux-arm64:${BIN_MEDIA_TYPE}" "${annotate[@]}"
)
