#!/usr/bin/env bash
# Runs inside archlinux:base-devel, invoked by build.sh. Builds the PKGBUILD
# against the mounted checkout pinned to $SHA, instead of the GitHub clone
# that `makepkg -si` uses for end users, so it packages exactly what's
# checked out (including an open PR's commits).
set -euo pipefail

# rubberband/alsa-lib/libpipewire/yt-dlp/tmux aren't needed to *build* odj,
# but makepkg checks runtime `depends` are satisfied before packaging, so
# they still need to be present in this container.
#
# -Syu, not -Sy: Arch doesn't support partial upgrades, and the
# archlinux:base-devel image is a snapshot of whatever the repos looked
# like when it was last pushed. A bare -Sy refreshes the package lists to
# today's repos without upgrading the already-installed base packages,
# so a new install can end up wanting a glibc/gcc-libs newer than what's
# on disk — pacman refuses with "could not satisfy dependencies" rather
# than risk it. -u upgrades the base image's own packages first.
pacman -Syu --noconfirm --needed base-devel git rust clang rubberband alsa-lib libpipewire yt-dlp tmux >/dev/null

useradd -m builder
cp -r /src /home/builder/odj-src
chown -R builder:builder /home/builder/odj-src /cache

pkgbuild=/home/builder/odj-src/packaging/arch/PKGBUILD
sed -i "s|^source=.*|source=(\"\$_pkgname::git+file:///home/builder/odj-src#commit=$SHA\")|" "$pkgbuild"

su builder -c '
  set -euo pipefail
  export CARGO_HOME=/cache/cargo CARGO_TARGET_DIR=/cache/target
  cd /home/builder/odj-src/packaging/arch
  makepkg --noconfirm
'

cp /home/builder/odj-src/packaging/arch/*.pkg.tar.* /out/
chown "$HOST_IDS" /out/*.pkg.tar.*
