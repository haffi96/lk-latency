#!/usr/bin/env bash
# Package every dist/<platform>/lk-latency as lk-latency-<version>-<platform>.tar.gz + SHA256SUMS.
set -euo pipefail
cd "$(dirname "$0")/.."
version="${1:?usage: scripts/package.sh <version>}"
cd dist
rm -f ./*.tar.gz SHA256SUMS
for dir in */; do
  platform="${dir%/}"
  [ -f "$platform/lk-latency" ] || continue
  cp ../README.md "$platform/"
  tar -czf "lk-latency-$version-$platform.tar.gz" -C "$platform" lk-latency README.md
done
sha256sum ./*.tar.gz */lk-latency | tee SHA256SUMS
