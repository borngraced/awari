#!/usr/bin/env bash
set -euo pipefail

dir=${1:?usage: package-arch.sh <staging-dir> [version]}
ver=${2:-0.1.0}
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
case "$dir" in
  /*) : ;;
  *) dir="$(cd "$dir" && pwd)" ;;
esac

if [ "$(id -u)" = 0 ]; then
  # makepkg ships with pacman/base, but fakeroot + debugedit do not.
  pacman -Sy --noconfirm --needed base-devel fakeroot debugedit >/dev/null
fi

cd "$dir"
# makepkg refuses to run as root; the CI container runs as root.
if [ "$(id -u)" = 0 ]; then
  useradd -m build 2>/dev/null || true
  chown -R build:build "$dir"
  chmod a+r awari awari.service config.kdl LICENSE
else
  :
fi

sed -e "s/@VERSION@/$ver/g" "$repo/packaging/PKGBUILD" > PKGBUILD

if [ "$(id -u)" != 0 ]; then
makepkg -f --skipinteg --nocheck --nodeps
else
  chown build:build PKGBUILD
  su build -s /bin/bash -c "cd '$dir' && makepkg -f --skipinteg --nocheck --nodeps"
fi

echo "built $PWD/awari-${ver}-*.pkg.tar.zst"