#!/usr/bin/env bash
# Builds odj_<version>_<arch>.deb inside a Debian or Ubuntu container, so it
# links against that release's libraries. Build once per release you target:
#
#   packaging/debian/build.sh                 # debian:trixie
#   packaging/debian/build.sh ubuntu:24.04
#
# The .deb lands in packaging/debian/dist/<image>/.
set -euo pipefail

image=${1:-debian:trixie}
repo=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
out="$repo/packaging/debian/dist/${image//:/-}"
crate_version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$repo/Cargo.toml" | head -1)
deb_version="$crate_version+r$(git -C "$repo" rev-list --count HEAD).$(git -C "$repo" rev-parse --short=7 HEAD)-1"
cache="odj-deb-cache-${image//[:\/]/-}"

mkdir -p "$out"
docker run --rm \
  -v "$repo:/src:ro" \
  -v "$out:/out" \
  -v "$cache:/cache" \
  -e CARGO_HOME=/cache/cargo \
  -e RUSTUP_HOME=/cache/rustup \
  -e CARGO_TARGET_DIR=/cache/target \
  -e DEB_VERSION="$deb_version" \
  -e HOST_IDS="$(id -u):$(id -g)" \
  "$image" bash -euo pipefail -c '
    export DEBIAN_FRONTEND=noninteractive PATH=/cache/cargo/bin:$PATH
    apt-get update -qq
    apt-get install -y -qq --no-install-recommends \
      ca-certificates curl build-essential pkg-config libclang-dev \
      librubberband-dev libasound2-dev libpipewire-0.3-dev dpkg-dev >/dev/null
    command -v rustup >/dev/null ||
      curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --no-modify-path
    rustup -q toolchain install stable --profile minimal
    command -v cargo-deb >/dev/null || cargo install --locked cargo-deb
    cd /src
    cargo deb --locked -p odj-player --deb-version "$DEB_VERSION"
    cp /cache/target/debian/odj_"$DEB_VERSION"_*.deb /out/
    chown "$HOST_IDS" /out/*.deb
  '
ls "$out"/*.deb
