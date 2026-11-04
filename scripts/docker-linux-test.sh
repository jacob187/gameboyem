#!/usr/bin/env bash
# Run the Linux CI steps locally in Docker. Usage: scripts/docker-linux-test.sh [amd64|arm64]
# Builds into target/docker-<arch> so it never clobbers the host target dir.
#
# Needs roms/test and roms/corpus on the host (scripts/fetch-test-roms.sh,
# scripts/fetch-homebrew-corpus.sh). CI=true makes a missing ROM a failure, so
# the homebrew cases are built in the container with the Linux GBDK (downloaded
# into the gitignored tools/gbdk-linux/, never tools/gbdk, which holds the host
# build) into target/docker-<arch>/roms/homebrew; the host roms/homebrew is not
# touched. GBDK_VERSION overrides the release (default: the one CI uses).
set -euo pipefail
ARCH="${1:-$(uname -m | sed 's/x86_64/amd64/;s/aarch64/arm64/')}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
GBDK_VERSION="${GBDK_VERSION:-4.5.0}"
case "$ARCH" in
  amd64) GBDK_ARCHIVE=gbdk-linux64.tar.gz ;;
  arm64) GBDK_ARCHIVE=gbdk-linux-arm64.tar.gz ;;
  *) echo "unsupported arch: $ARCH" >&2; exit 2 ;;
esac
docker run --rm --platform "linux/$ARCH" \
  -v "$ROOT":/src -w /src \
  -v "gb-cargo-registry-$ARCH":/usr/local/cargo/registry \
  -e CARGO_TARGET_DIR="/src/target/docker-$ARCH" \
  -e CI=true \
  -e GBDK_VERSION="$GBDK_VERSION" -e GBDK_ARCHIVE="$GBDK_ARCHIVE" -e ARCH="$ARCH" \
  rust:latest bash -c '
    set -euo pipefail
    apt-get update -qq && apt-get install -y -qq pkg-config libx11-dev libxkbcommon-dev libwayland-dev libasound2-dev libudev-dev >/dev/null
    rustup component add rustfmt clippy >/dev/null 2>&1
    uname -m
    GBDK=/src/tools/gbdk-linux
    if [ ! -x "$GBDK/bin/lcc" ]; then
      mkdir -p "$GBDK"
      curl -fsSL "https://github.com/gbdk-2020/gbdk-2020/releases/download/$GBDK_VERSION/$GBDK_ARCHIVE" | tar -xz -C "$GBDK" --strip-components=1
    fi
    ROMS=/src/target/docker-$ARCH/roms
    mkdir -p "$ROMS/homebrew"
    ln -sfn /src/roms/test "$ROMS/test"
    ln -sfn /src/roms/corpus "$ROMS/corpus"
    make -C homebrew GBDK_HOME="$GBDK/" OUT="$ROMS/homebrew" BUILD="/src/target/docker-$ARCH/homebrew-build" >/dev/null
    export GB_ROMS_DIR="$ROMS"
    cargo fmt --all --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace
    cargo run -p gb-test --release -- --fail-on-mismatch | tail -3
  '
