<p align="center">
  <img src="crates/awari/assets/awari-icon.svg" width="120" alt="Àwárí logo">
</p>

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

- One unified list for **windows** (focus), **apps** (XDG `.desktop`), and **files**. Apps are always indexed; the files and windows sources can each be toggled off.
- Empty query shows recent apps first, then open windows (no file dump).
- Fuzzy matching across apps and windows, with frecency-ranked files.
- Query prefixes:
  - `>` runs the rest as a shell command in a terminal.
  - `o:<path>` opens a path with the default handler. It lists the real entries as you type and accepts `~` or paths relative to `$HOME`.
  - `r:<regex>` filters files by regular expression.
- Path-shaped queries (starting with `~`, `/`, or `.`, or containing `/`) enter directory navigation; constraints like `*.pdf` and `!node_modules/` apply.
- Calculator: a query that is arithmetic shows the result inline, and Enter copies it.
- `Tab` autocompletes with the selected result's full value. `Shift`+`Up`/`Down` recalls previous queries.
- `Alt`+`Enter` opens a per-row action menu: Open, Show in Folder, Copy Path, Run in Terminal, Run.
- Clickable category chips: All, Apps, Files, Commands, Windows.
- Theming is KDL with hex color tokens (no CSS, no remote fetches). There are nine built-in presets, per-token overrides, and aliases (`select`, `hover`/`surface`, `fg`, `muted`, `faint`). Font and size are configurable too.
- Lockfiles like `Cargo.lock` and `package-lock.json` are skipped by default.
- Monitor-aware placement: the overlay opens on the focused output.

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

Run `awari` as a resident user service so the overlay is always one key away:

```sh
systemctl --user enable --now ~/.config/systemd/user/awari.service
```

(See `contrib/awari.service` for the unit.)

Then bind a key in your compositor to toggle the overlay.

**niri** (`~/.config/niri/config.kdl`):

```kdl
spawn-at-startup "awari"
binds {
    Mod+D { spawn "awari" "toggle-launcher"; }
}
```

**Hyprland** (`~/.config/hypr/hyprland.conf`):

```ini
bind = SUPER, D, exec, awari toggle-launcher
```

## Configuration

KDL at [`~/.config/awari/config.kdl`](docs/config.md). Unknown keys are ignored. No exec,
scripts, or shell interpolation. Every block and token is optional — anything
omitted keeps its default. The complete, copy-pasteable file is
`contrib/config.kdl`.

```kdl
theme {
  name "catppuccin"            // preset: classic · catppuccin (default) · gruvbox
                              //          gruvbox-light · tokyo-night · nord
  // font "Inter"             // system family; "default"/"" = GPUI system UI font
  // font-size 14             // px, clamped to 8..=64
  accent      "#cba6f7"       // = select
  accent-dim  "#cba6f733"
  bg          "#11111b"
  panel       "#1e1e2e"
  raise       "#313244"       // = hover = surface
  border      "#45475a"
  text        "#cdd6f4"       // = fg
  text-dim    "#a6adc8"       // = muted
  text-faint  "#9399b2"       // = faint
  scrim       "#08080ce6"
}

files {
  roots "~/Documents" "~/Downloads" "~/code"   // omit/empty = XDG user dirs
  index_lockfiles false                        // show Cargo.lock, *.lock, …
  regex           false                        // file queries as regex (r: prefix forces it)
}

sources {
  windows true
  files   true
}
// apps is always indexed and cannot be disabled

motion {
  reduced     false            // disable animations
  duration-ms 140              // open/close length, clamped to 0..=1000
}
```

Colors are hex: `#RGB`, `#RRGGBB`, or `#RRGGBBAA`. In place of the canonical
token names you can also use these aliases:

| Alias | Sets |
| --- | --- |
| `select` | `accent-dim` |
| `hover`, `surface` | `raise` |
| `fg` | `text` |
| `muted` | `text-dim` |
| `faint` | `text-faint` |



## Status

Rust on a GPUI layer-shell. GPL-3.0-or-later; linking `niri-ipc` makes the
binary a GPL derivative under niri. No plugins.

Client commands: `toggle-launcher`, `open-launcher`, `close-launcher`,
`ping`, `dump-stats`. `awari ping` talks to a running daemon over
`$XDG_RUNTIME_DIR/awari/ipc.sock`.
