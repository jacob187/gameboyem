#!/usr/bin/env bash
# Run the Linux CI steps locally in Docker. Usage: scripts/docker-linux-test.sh [amd64|arm64]
# Builds into target/docker-<arch> so it never clobbers the host target dir.
set -euo pipefail
ARCH="${1:-$(uname -m | sed 's/x86_64/amd64/;s/aarch64/arm64/')}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
docker run --rm --platform "linux/$ARCH" \
  -v "$ROOT":/src -w /src \
  -v "gb-cargo-registry-$ARCH":/usr/local/cargo/registry \
  -e CARGO_TARGET_DIR="/src/target/docker-$ARCH" \
  -e CI=true \
  rust:latest bash -c '
    set -euo pipefail
    apt-get update -qq && apt-get install -y -qq pkg-config libx11-dev libxkbcommon-dev libwayland-dev libasound2-dev libudev-dev >/dev/null
    rustup component add rustfmt clippy >/dev/null 2>&1
    uname -m
    cargo fmt --all --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace
    cargo run -p gb-test --release -- --fail-on-mismatch | tail -3
  '
