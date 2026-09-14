#!/usr/bin/env bash
set -euo pipefail

dir=${1:?usage: package-ubuntu.sh <staging-dir> [version]}
ver=${2:-0.1.0}

cd "$dir"
pkg="awari_${ver}_amd64"
rm -rf "$pkg"
mkdir -p "$pkg/DEBIAN" \
  "$pkg/usr/bin" \
  "$pkg/usr/lib/systemd/user" \
  "$pkg/usr/share/awari" \
  "$pkg/usr/share/doc/awari"

install -m755 awari "$pkg/usr/bin/awari"
install -m644 awari.service "$pkg/usr/lib/systemd/user/awari.service"
install -m644 config.kdl "$pkg/usr/share/awari/config.kdl"
install -m644 LICENSE "$pkg/usr/share/doc/awari/copyright"

cat > "$pkg/DEBIAN/control" <<EOF
Package: awari
Version: ${ver}
Section: utils
Priority: optional
Architecture: amd64
Maintainer: Samuel Onoja Taiwo <awari@sot.dev>
Depends: libwayland-client0, libegl1, libfontconfig1, libxkbcommon0
Homepage: https://github.com/borngraced/awari
Description: A fast Wayland launcher for apps, files and windows
 Awari is a Wayland launcher built on GPUI and fff search. A GPU-free daemon
 ranks windows, apps and files in one fuzzy list, with in-process search so
 there is no per-keystroke subprocess.
EOF

dpkg-deb --build --root-owner-group "$pkg" >/dev/null
echo "built $PWD/$pkg.deb"