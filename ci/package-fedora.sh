#!/usr/bin/env bash
set -euo pipefail

dir=${1:?usage: package-fedora.sh <staging-dir> [version]}
ver=${2:-0.1.0}
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if ! command -v rpmbuild >/dev/null; then
  dnf install -y rpm-build >/dev/null
fi

cd "$dir"
topdir="$HOME/rpmbuild"
rm -rf "$topdir"
mkdir -p "$topdir"/{BUILD,BUILDROOT,RPMS,SOURCES,SPECS,SRPMS}

cp awari awari.service config.kdl LICENSE "$topdir/SOURCES/"
sed -e "s/@VERSION@/$ver/g" "$repo/packaging/awari.spec" > "$topdir/SPECS/awari.spec"

rpmbuild -bb --define "_topdir $topdir" "$topdir/SPECS/awari.spec" >/dev/null
cp "$topdir"/RPMS/x86_64/*.rpm .
echo "built $PWD/awari-${ver}-*.x86_64.rpm"