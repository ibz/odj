#!/usr/bin/env bash
# Builds odj-git-<version>-<arch>.pkg.tar.zst inside an Arch Linux container,
# from the current checkout pinned to HEAD (not the GitHub clone that
# `makepkg -si` uses for end users), so CI packages exactly what's checked
# out, including an open PR's commits.
#
#   packaging/arch/build.sh
#
# The package lands in packaging/arch/dist/.
set -euo pipefail

repo=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
out="$repo/packaging/arch/dist"
cache="odj-arch-cache"

mkdir -p "$out"
docker run --rm \
  -v "$repo:/src:ro" \
  -v "$out:/out" \
  -v "$cache:/cache" \
  -e SHA="$(git -C "$repo" rev-parse HEAD)" \
  -e HOST_IDS="$(id -u):$(id -g)" \
  archlinux:base-devel bash /src/packaging/arch/docker-build.sh
ls "$out"/*.pkg.tar.*
