#!/usr/bin/env bash
# Build the macOS binary natively: dist/macos-<arch>/lk-latency.
# Needs Xcode command line tools, rustup, and `brew install pkg-config jpeg-turbo`.
set -euo pipefail
cd "$(dirname "$0")/.."
arch="$(uname -m)"   # arm64 on Apple silicon
# Link libjpeg statically so users don't need Homebrew.
export LIBJPEG_STATIC=1
export LK_LATENCY_COMMIT="$(git rev-parse --short=12 HEAD 2>/dev/null || echo unknown)"
cargo build --locked --release
mkdir -p "dist/macos-$arch"
cp target/release/lk-latency "dist/macos-$arch/lk-latency"
# Only system libraries and frameworks may be linked.
if otool -L "dist/macos-$arch/lk-latency" | tail -n +2 | grep -vE '^\s+(/usr/lib/|/System/Library/)'; then
  echo "error: non-system dynamic library linked (see above)" >&2
  exit 1
fi
"dist/macos-$arch/lk-latency" version
