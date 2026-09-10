#!/usr/bin/env bash
# bench/verify-bare-docker.sh — build the dev docker container and run
# bench/verify-bare-overlay.sh inside Linux's kernel FUSE in Docker.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

IMAGE_NAME="${EPOCH_DOCKER_IMAGE:-epoch-fuse}"

echo "▸ Building Docker dev container image ($IMAGE_NAME)..."
docker build -t "$IMAGE_NAME" .

echo "▸ Preparing host cargo cache directories..."
mkdir -p "${HOME}/.cargo/registry" "${HOME}/.cargo/git"

echo "▸ Running bare Epoch-FS verification inside Docker ($IMAGE_NAME)..."
docker run --rm \
  --device /dev/fuse \
  --cap-add SYS_ADMIN \
  --security-opt apparmor=unconfined \
  -v "${HOME}/.cargo/registry:/usr/local/cargo/registry" \
  -v "${HOME}/.cargo/git:/usr/local/cargo/git" \
  -v "${REPO_ROOT}/bench/verify-bare-overlay.sh:/usr/local/bin/verify-bare-overlay.sh:ro" \
  -e GITHUB_REPOSITORY="${GITHUB_REPOSITORY:-poucet/epoch}" \
  -e GITHUB_TOKEN="${GITHUB_TOKEN:-}" \
  "$IMAGE_NAME" \
  /usr/local/bin/verify-bare-overlay.sh
