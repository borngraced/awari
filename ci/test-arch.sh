#!/usr/bin/env bash
set -euo pipefail
pacman-key --init
pacman-key --populate archlinux
pacman -Sy --noconfirm --needed \
  base-devel git curl pkgconf openssl wayland libxkbcommon \
  mesa fontconfig
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable
. "$HOME/.cargo/env"
export CARGO_TARGET_DIR=/target
cargo build --release -p awari
ls -lh "$CARGO_TARGET_DIR/release/awari"
