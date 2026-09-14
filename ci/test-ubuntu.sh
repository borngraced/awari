#!/usr/bin/env bash
set -euo pipefail
apt-get update -qq
apt-get install -y -qq --no-install-recommends \
  curl ca-certificates git xz-utils pkg-config gcc make \
  zlib1g-dev libwayland-dev libegl-dev libxkbcommon-dev \
  libssl-dev libfontconfig1-dev
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable
. "$HOME/.cargo/env"
export CARGO_TARGET_DIR=/target
cargo build --release -p awari
ls -lh "$CARGO_TARGET_DIR/release/awari"
