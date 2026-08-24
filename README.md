# Àwárí

**Àwárí** is a Wayland overlay launcher. Super filters windows, apps, and files. There is no bar and no map.

The 3-second demo is: Super, type, the right row, next vsync. Canonical spec: [`docs/launcher.md`](docs/launcher.md).

Yoruba **àwárí**: a finding, a discovery. You type `awari` — Cargo, systemd, and niri binds are ASCII (only when niri is the compositor; otherwise any Wayland compositor).

## Docs

| Doc | Contents |
|---|---|
| [`docs/launcher.md`](docs/launcher.md) | Canonical product spec |

Budgets, stack, and status notes live in the launcher spec.

## Budgets

Closed daemon must sleep (`pidstat` **>1%** for 10s fails). RSS well under 100MB including the home file index. IPC → damage p99 **< 2ms**; first pixel **≤ next vsync** after that (not including niri spawn). Reduced-motion: duration 0.

## Stack

Rust + GPUI layer-shell overlay. Entire workspace GPL-3.0-or-later. systemd `--user`, single instance. Files via in-process `fff-search`, not an MCP/CLI spawn.

Linking `niri-ipc` makes the binary a GPL derivative when niri is the compositor. Plugins are out.

`awari ping` talks to a running daemon. Stale `$XDG_RUNTIME_DIR/awari/ipc.sock`: Ping then unlink on `ECONNREFUSED`. Looping unit: `systemctl --user reset-failed`.

## Status

Launcher daemon + overlay. Bar, map, filmstrip, and HUD services are gone.

### Build

Linux + any Wayland compositor (niri, hyprland, mutter, sway detected at runtime).

```
# Debian: libwayland-dev libxkbcommon-dev libegl-dev pkg-config
# Fedora: wayland-devel libxkbcommon-devel mesa-libEGL-devel
cargo test
cargo run -p awari
awari ping
```

`contrib/awari.service`. When niri is the compositor: `spawn-at-startup "awari"` and `Mod+D { spawn "awari" "toggle-launcher"; }`.

---

Launcher spec 2026-08-23.
