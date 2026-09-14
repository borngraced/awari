#!/usr/bin/env bash
set -euo pipefail

# Build an SRPM from prebuilt release artifacts and submit it to a Copr project.
# Requires ~/.config/copr (written by the workflow from secrets) and copr-cli.
ver=${1:?usage: publish-copr.sh <version> [copr-project]}
copr_project=${2:-awari}
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
dist=${DIST_DIR:-"$repo/dist"}

topdir="$(mktemp -d)"
trap 'rm -rf "$topdir"' EXIT
mkdir -p "$topdir"/{BUILD,BUILDROOT,RPMS,SOURCES,SPECS,SRPMS}

cp "$dist/awari/awari" "$topdir/SOURCES/awari"
cp "$dist/awari/awari.service" "$topdir/SOURCES/awari.service"
cp "$dist/awari/config.kdl" "$topdir/SOURCES/config.kdl"
cp "$dist/awari/LICENSE" "$topdir/SOURCES/LICENSE"
sed -e "s/@VERSION@/$ver/g" "$repo/packaging/awari.spec" > "$topdir/SPECS/awari.spec"

rpmbuild -bs --define "_topdir $topdir" "$topdir/SPECS/awari.spec"
copr-cli build "$copr_project" "$topdir"/SRPMS/awari-*.src.rpm