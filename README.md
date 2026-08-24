# Àwárí

Àwárí is a launcher for Wayland. Hit Super, type, and you get a ranked list of
the windows, apps, and files you're after. The overlay stays resident, so the
result lands on the next frame instead of after some program boots.

The name is Yoruba for "a finding, a discovery".

## Motivation

Most launchers are slow in ways you learn to live with. They fork a search tool
on every keystroke (fd, fzf, rg) and pay for it in process startups and parsing.
Or they're a web view that spins up a runtime just to draw a text field. Files
tend to be an afterthought stuck onto an app menu.

Àwárí goes the other way. File search runs inside the daemon through fff-search,
pinned and in-process, so there's no subprocess per character. The overlay is
already up, so pressing Super costs nothing: a socket message opens it and takes
the keyboard. Windows, apps, and files all go into one ranking, so you get
whichever you meant. And because it's only a launcher, it stays small: a sleeping
process when closed and a bounded one while open.

## Usage

Super opens it. Typing narrows the list and keeps the top row selected; Enter
activates it and closes. Escape or a click on the background dismisses. Up and
down move the selection.

When a query looks like a path (starts with ~, /, or ., or contains /), files
rank first and fff constraints like `*.pdf` and `!node_modules/` apply. `../` is
a path, not an operator.

## How it stays fast

- File search is in-process via fff-search (0.10.5), not a spawned fd/fzf/rg.
- The overlay is one long-lived layer-shell window. Opening is one redraw plus a
  keyboard-interactivity request, not a compositor spawn.
- fff does fuzzy path matching, frecency, and git-status tagging. Our own
  `matchq` ranks windows and apps without allocating per keystroke.
- IPC read to damage is under 2 ms p99, and the first pixel lands within a
  vsync after that. Reduced-motion sets the duration to zero.

## Features

- One list covering windows (focus), apps (XDG `.desktop`), and files.
- Empty query shows windows on the focused output, then recent apps.
- Dynamic path search: `~/dev/reelshell/src/` lists its contents as you type.
- Regex file filter with the `r:` prefix.
- Lockfiles like `Cargo.lock` and `package-lock.json` are skipped by default.
- Theme is KDL with hex color tokens. No CSS, no remote fetches.

## Memory

The closed daemon should be idle; sustained CPU above about 1% is a bug. Memory
is bounded too. Per-directory file indexes are capped with LRU eviction and each
picker's cache is finite, so browsing through many folders doesn't pile up
indexes.

## Build

Linux with a Wayland compositor (niri, hyprland, mutter, sway detected at
runtime).

```sh
# Debian/Ubuntu
sudo apt install libwayland-dev libxkbcommon-dev libegl-dev pkg-config
# Fedora
sudo dnf install wayland-devel libxkbcommon-devel mesa-libEGL-devel

cargo test
cargo run -p awari
awari ping
```

Run as a user service (`contrib/awari.service`). With niri:
`spawn-at-startup "awari"` and `Mod+D { spawn "awari" "toggle-launcher"; }`.

## Configuration

KDL at `~/.config/awari/config.kdl`. No exec, scripts, or shell interpolation.

```kdl
theme {
  accent "#b4a0ff"
  panel  "#141416"
  text   "#f2f2f5"
}
files {
  roots "~/Documents" "~/code"
}
sources {
  windows true
  apps   true
  files  true
}
```

See `contrib/config.kdl` for the full token list (theme, files, sources,
motion).

## Status

Launcher daemon and overlay only. The bar, map, filmstrip, HUD, and tray
services are gone.

Rust on a GPUI layer-shell. GPL-3.0-or-later; linking `niri-ipc` makes the
binary a GPL derivative under niri. No plugins.

Client commands: `toggle-launcher`, `open-launcher`, `close-launcher`, `ping`,
`dump-stats`. `awari ping` talks to a running daemon over
`$XDG_RUNTIME_DIR/awari/ipc.sock`.
