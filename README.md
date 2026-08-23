# Àwárí

**Àwárí** is a niri overlay launcher. Super filters windows, apps, and files. There is no bar and no map.

The 3-second demo is: Super, type, the right row, next vsync. Canonical spec: [`docs/launcher.md`](docs/launcher.md). The old map-shell writeup (then called Reelshell) is [`docs/architecture.md`](docs/architecture.md) (historical).

Yoruba **àwárí**: a finding, a discovery. You type `awari` — Cargo, systemd, and niri binds are ASCII.

## Docs

| Doc | Contents |
|---|---|
| [`docs/launcher.md`](docs/launcher.md) | Canonical product spec |
| [`docs/architecture.md`](docs/architecture.md) | Historical map-shell design |
| [`docs/map.md`](docs/map.md) | Historical map / filmstrip |
| [`docs/surfaces.md`](docs/surfaces.md) | Layer-shell notes (launcher overlay still applies) |
| [`docs/compositor.md`](docs/compositor.md) | niri sockets (spawn / focus / window list) |
| [`docs/performance.md`](docs/performance.md) | Historical budgets; launcher IPC/vsync still apply |

## Budgets

Closed daemon must sleep (`pidstat` **>1%** for 10s fails). RSS well under 100MB including the home file index. IPC → damage p99 **< 2ms**; first pixel **≤ next vsync** after that (not including niri spawn). Reduced-motion: duration 0.

## Stack

Rust + GPUI layer-shell overlay. `niri-ipc = "=26.4.0"`. Entire workspace GPL-3.0-or-later. systemd `--user`, single instance. Files via in-process `fff-search`, not an MCP/CLI spawn.

Linking `niri-ipc` makes the binary a GPL derivative. Plugins are out.

`awari ping` talks to a running daemon. Stale `$XDG_RUNTIME_DIR/awari/ipc.sock`: Ping then unlink on `ECONNREFUSED`. Looping unit: `systemctl --user reset-failed`.

## Status

Launcher daemon + overlay. Bar, map, filmstrip, and HUD services are gone.

### Build

Linux + niri only.

```
# Debian: libwayland-dev libxkbcommon-dev libegl-dev pkg-config
# Fedora: wayland-devel libxkbcommon-devel mesa-libEGL-devel
cargo test
cargo run -p awari
awari ping
```

`contrib/awari.service`. niri: `spawn-at-startup "awari"` and `Mod+D { spawn "awari" "toggle-launcher"; }`.

---

Launcher spec 2026-08-23.
