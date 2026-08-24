#!/usr/bin/env sh
# Prepare the patched GPUI tree awari builds against, and wire Cargo.toml.
#
# We carry one small patch (`patches/gpui-set-visible.patch`) that adds
# `Window::set_visible(bool)` for layer-shell surfaces: hide unmaps the
# surface (role destroyed, null buffer committed — compositor releases
# keyboard focus), show remaps it, and the renderer/surface stay alive so
# reopening the launcher costs a frame instead of a full window teardown.
# Neither upstream zed nor gpui-ce has merged this yet (gpui-ce PR #87 open).
#
# What this does:
#   1. Seeds .third_party/zed at our pinned zed rev (cargo checkout cache
#      if present, else blobless clone).
#   2. Applies patches/gpui-set-visible.patch and commits the result with a
#      fixed timestamp (deterministic SHA across machines).
#   3. Rewrites the BEGIN/END-GPUI-PATCH block in the root Cargo.toml with
#      [patch] entries pointing at the local tree via file:// + that SHA.
#
# Re-run any time (idempotent); safe to delete .third_party/.

set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TREE="$ROOT/.third_party/zed"
REV="fd82517a115d97a07835b52f0512b22b38e38ccf"
PATCH="$ROOT/patches/gpui-set-visible.patch"
MANIFEST="$ROOT/Cargo.toml"
FIXED_DATE="2026-08-23T00:00:00Z"

if [ ! -f "$PATCH" ]; then
    echo "error: $PATCH missing" >&2
    exit 1
fi

if [ ! -f "$TREE/crates/gpui/src/platform.rs" ]; then
    rm -rf "$TREE"
    mkdir -p "$TREE"

    CARGO_SRC="$(ls -d "$HOME"/.cargo/git/checkouts/zed-*/*/ 2>/dev/null | while read -r d; do
        if grep -qs 'set_input_region' "$d/crates/gpui_linux/src/linux/wayland/window.rs" 2>/dev/null; then echo "$d"; fi
    done | head -1)"

    if [ -n "$CARGO_SRC" ]; then
        echo "seeding from cargo checkout: $CARGO_SRC"
        cp -R "$CARGO_SRC"/. "$TREE/"
        rm -rf "$TREE"/.git
        git init -q "$TREE"
    else
        echo "fetching zed @ ${REV:0:9} (blobless clone)"
        git init -q "$TREE"
        git -C "$TREE" remote add origin https://github.com/zed-industries/zed.git
        git -C "$TREE" config remote.origin.promisor true
        git -C "$TREE" config remote.origin.partialclonefilter blob:none
        git -C "$TREE" fetch --filter=blob:none origin main
        git -C "$TREE" checkout -q -b work "$REV"
    fi

    git -C "$TREE" add -A
    GIT_AUTHOR_DATE="$FIXED_DATE" GIT_COMMITTER_DATE="$FIXED_DATE" \
        git -C "$TREE" \
        -c user.email=awari@local -c user.name=awari -c commit.gpgsign=false \
        commit -qm "zed $REV baseline" || true
fi

# Deterministic rebuild of the patched branch from the pristine baseline.
if git -C "$TREE" rev-parse -q --verify awari-baseline >/dev/null; then
    git -C "$TREE" reset -q --hard awari-baseline
else
    git -C "$TREE" add -A
    GIT_AUTHOR_DATE="$FIXED_DATE" GIT_COMMITTER_DATE="$FIXED_DATE" \
        git -C "$TREE" commit -qm "zed $REV baseline"
    git -C "$TREE" tag awari-baseline
fi

git -C "$TREE" apply --whitespace=nowarn "$PATCH"
git -C "$TREE" add -A
GIT_AUTHOR_DATE="$FIXED_DATE" GIT_COMMITTER_DATE="$FIXED_DATE" \
    git -C "$TREE" \
    -c user.email=awari@local -c user.name=awari -c commit.gpgsign=false \
    commit -qm "Add Window::set_visible for layer-shell surfaces"
LOCAL_REV="$(git -C "$TREE" rev-parse HEAD)"
echo "patched gpui tree: $LOCAL_REV"

# Rewrite the managed block in the root manifest.
BLOCK_BEGIN="# BEGIN-GPUI-PATCH (managed by scripts/setup-gpui.sh)"
BLOCK_END="# END-GPUI-PATCH"
URL="file://$TREE"

{
    echo "$BLOCK_BEGIN"
    echo '[patch."https://github.com/zed-industries/zed"]'
    # Versions match the locked zed-sourced packages; they disambiguate
    # against shadow crates inside the zed tree (e.g. a 0.0.0 `gpui`).
    for spec in \
        "gpui 0.2.2" \
        "gpui_platform 0.1.0" \
        "gpui_linux 0.1.0" \
        "gpui_wgpu 0.1.0" \
        "gpui_macros 0.1.0" \
        "gpui_apple 0.1.0" \
        "gpui_macos 0.1.0" \
        "gpui_web 0.1.0" \
        "gpui_windows 0.1.0" \
        "collections 0.1.0" \
        "gpui_shared_string 0.1.0" \
        "gpui_util 0.1.0" \
        "sum_tree 0.1.0"; do
        set -- $spec
        echo "$1 = { version = \"=$2\", git = \"$URL\", rev = \"$LOCAL_REV\" }"
    done
    echo "$BLOCK_END"
} > "$ROOT/.gpui-patch-block.tmp"

if grep -qF "$BLOCK_BEGIN" "$MANIFEST"; then
    # Replace everything between the markers.
    awk -v begin="$BLOCK_BEGIN" -v end="$BLOCK_END" -v repl="$(cat "$ROOT/.gpui-patch-block.tmp")" '
        $0 == begin {inblock=1; print repl; next}
        inblock && $0 == end {inblock=0; print; next}
        !inblock && $0 == end {next}
        !inblock {print}
    ' "$MANIFEST" > "$MANIFEST.tmp"
    mv "$MANIFEST.tmp" "$MANIFEST"
else
    cat "$ROOT/.gpui-patch-block.tmp" >> "$MANIFEST"
fi
rm -f "$ROOT/.gpui-patch-block.tmp"

echo "ok: Cargo.toml patched against local gpui ($LOCAL_REV)"
