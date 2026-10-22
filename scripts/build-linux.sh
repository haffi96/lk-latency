#!/usr/bin/env bash
# Build the Linux binary in the pinned build container: dist/linux-<arch>/lk-latency.
# Usage: scripts/build-linux.sh [amd64|arm64]   (default: this machine's arch)
set -euo pipefail
cd "$(dirname "$0")/.."
arch="${1:-$(uname -m | sed 's/x86_64/amd64/; s/aarch64/arm64/')}"
commit="$(git rev-parse --short=12 HEAD 2>/dev/null || echo unknown)"
docker buildx build --platform "linux/$arch" --target binary \
  --build-arg LK_LATENCY_COMMIT="$commit" \
  --output "type=local,dest=dist/linux-$arch" .
ls -l "dist/linux-$arch/lk-latency"
