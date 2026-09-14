#!/usr/bin/env bash
set -euo pipefail

# Build a binary .deb from release artifacts and upload it to a Launchpad PPA.
# Requires ~/.dput.cf and an SSH key registered on Launchpad (workflow writes both).
ver=${1:?usage: publish-ppa.sh <version>}
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
dist=${DIST_DIR:-"$repo/dist"}
series=${DISTRIBUTION:-noble}
ppa=${PPA_NAME:-borngraced/awari}
pkgver="${ver}-0~ppa1~${series}"

bash "$repo/ci/package-ubuntu.sh" "$dist/awari" "$pkgver"

cd "$dist/awari"
deb="awari_${pkgver}_amd64.deb"
size=$(stat -c %s "$deb")
md5=$(md5sum "$deb" | cut -d' ' -f1)
sha1=$(sha1sum "$deb" | cut -d' ' -f1)
sha256=$(sha256sum "$deb" | cut -d' ' -f1)
date=$(date -Ru)
target="ppa:${ppa}"
incoming="${ppa#*/}"

cat > "awari_${pkgver}_amd64.changes" <<EOF
Format: 1.8
Date: $date
Source: awari
Binary: awari
Architecture: amd64
Version: $pkgver
Distribution: $series
Urgency: medium
Maintainer: Samuel Onoja Taiwo <awari@sot.dev>
Changed-By: Samuel Onoja Taiwo <awari@sot.dev>
Description:
 awari - A fast Wayland launcher for apps, files and windows
Changes:
 awari ($pkgver) $series; urgency=medium
 .
   CI release of awari $ver
Files:
 $md5 $size utils optional $deb
Checksums-Sha1:
 $sha1 $size $deb
Checksums-Sha256:
 $sha256 $size $deb
EOF

dput "$target" "awari_${pkgver}_amd64.changes"