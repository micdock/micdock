#!/usr/bin/env bash
# Point the AUR package at a published release: sets pkgver, resets pkgrel, takes the checksums from the
# release's .sha256 files, then builds, lints and installs the package in a clean Arch container (podman or
# docker) and regenerates .SRCINFO from there.
#
# usage: packaging/arch/update.sh 0.12.0        (after the v0.12.0 release is on GitHub)
#        packaging/arch/update.sh --srcinfo     (only rebuild/test and regenerate .SRCINFO, e.g. after pkgrel++)
set -euo pipefail
cd "$(dirname "$0")"
url=https://github.com/micdock/micdock/releases/download

if [[ ${1:-} != --srcinfo ]]; then
  ver=${1:?usage: update.sh <version> | --srcinfo}
  for a in x86_64 aarch64; do
    sum=$(curl -fsSL "$url/v$ver/micdock-$ver-$a-linux.tar.gz.sha256" | cut -d' ' -f1)
    [[ $sum =~ ^[0-9a-f]{64}$ ]] || { echo "no checksum for $a in release v$ver" >&2; exit 1; }
    sed -i "s/^sha256sums_$a=.*/sha256sums_$a=('$sum')/" PKGBUILD
  done
  sed -i -e "s/^pkgver=.*/pkgver=$ver/" -e "s/^pkgrel=.*/pkgrel=1/" PKGBUILD
fi

engine=$(command -v podman || command -v docker) || { echo "needs podman or docker" >&2; exit 1; }
"$engine" run --rm -v "$PWD:/pkg:Z" docker.io/library/archlinux:base-devel bash -c '
  set -e
  pacman -Syu --noconfirm -q namcap alsa-lib libxkbcommon >/dev/null
  useradd -m build && cp -r /pkg /home/build/p && chown -R build /home/build/p && cd /home/build/p
  su build -c "makepkg --noconfirm -f" | tail -1
  su build -c "makepkg --printsrcinfo" > /pkg/.SRCINFO
  namcap PKGBUILD *.pkg.tar.zst || true
  pacman -U --noconfirm *.pkg.tar.zst >/dev/null
  ugg2wav --version
  ! ldd /usr/bin/micdock | grep "not found"'
echo "PKGBUILD and .SRCINFO are ready; see packaging/arch/README.md to publish."
