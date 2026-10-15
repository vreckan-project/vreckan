#!/usr/bin/env bash
# build-base-image.sh — build + publish the vreckan-base-build build/work image.
#
# Builds `Dockerfile.base-build` (the self-contained build stage: Rust 1.99
# toolchain, the -sys build deps, sccache, and a warm crates.io + git
# dependency cache), tags it, and pushes it to the Gitea container registry
# (ANT-203). The result is the stable image operators/devs spin up for vreckan
# work instead of re-assembling the toolchain each time.
#
# Required env:
#   GITEA_REGISTRY_USER     — registry login user (e.g. admin)
#   GITEA_REGISTRY_PASSWORD — registry password (a Gitea token works)
#
# Optional env (defaults shown):
#   GITEA_REGISTRY_HOST     — repo.vreckanproject.com
#   GITEA_REGISTRY_NAMESPACE— vreckan
#   IMAGE_NAME              — vreckan-base-build
#   BASE_TAG                — latest
#   DOCKERFILE              — Dockerfile.base-build
#   PUSH                    — 1 to build+tag+push (default); 0 to build+tag only
#
# The image is tagged under both the requested tag (default `latest`) and a
# date-stamped tag so a pull can be traced back to the build date. Both tags
# are pushed together.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

: "${GITEA_REGISTRY_USER:?GITEA_REGISTRY_USER is required}"
: "${GITEA_REGISTRY_PASSWORD:?GITEA_REGISTRY_PASSWORD is required}"

HOST="${GITEA_REGISTRY_HOST:-repo.vreckanproject.com}"
NAMESPACE="${GITEA_REGISTRY_NAMESPACE:-vreckan}"
IMAGE="${IMAGE_NAME:-vreckan-base-build}"
TAG="${BASE_TAG:-latest}"
DOCKFILE="${DOCKERFILE:-Dockerfile.base-build}"
PUSH="${PUSH:-1}"

REF="${HOST}/${NAMESPACE}/${IMAGE}"
DATE_TAG="$(date -u +%Y%m%d)"

echo "==> Building ${REF}:${TAG} from ${DOCKFILE}"
docker build \
  --file "$DOCKFILE" \
  --tag "${REF}:${TAG}" \
  --tag "${REF}:${DATE_TAG}" \
  .

# Record what was built: the image id and the pinned base digest, for the
# commit/PR that publishes the image.
IMAGE_ID="$(docker image inspect "${REF}:${TAG}" --format '{{.Id}}')"
echo "==> Built ${REF}:${TAG} (${IMAGE_ID})"

if [ "$PUSH" = "1" ]; then
  echo "==> Logging in to ${HOST}"
  # --password-stdin keeps the token out of the process argument list.
  printf '%s' "$GITEA_REGISTRY_PASSWORD" | docker login "$HOST" \
    --username "$GITEA_REGISTRY_USER" --password-stdin
  echo "==> Pushing ${REF}:${TAG} and ${REF}:${DATE_TAG}"
  docker push "${REF}:${TAG}"
  docker push "${REF}:${DATE_TAG}"
  echo "==> Done. Pull with:"
  echo "    docker pull ${REF}:${TAG}"
fi
