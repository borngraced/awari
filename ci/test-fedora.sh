#!/usr/bin/env bash
set -euo pipefail
dnf install -y git curl xz gcc make pkgconf-pkg-config \
  openssl-devel zlib-devel wayland-devel mesa-libEGL-devel \
  libxkbcommon-devel fontconfig-devel
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable
. "$HOME/.cargo/env"
export CARGO_TARGET_DIR=/target
cargo build --release -p awari
ls -lh "$CARGO_TARGET_DIR/release/awari"
