<p align="center">
  <img src="crates/awari/assets/awari-icon.svg" width="120" alt="Àwárí logo">
</p>

# Àwárí

Àwárí is a blazingly fast launcher for Wayland (toggle it in ~2.78 ms). Hit Super, type, and get a ranked list of the
windows, apps, and files you're after, powered by
[fff-search](https://github.com/dmtrKovalenko/fff), an in-process,
frecency-ranked index, not a subprocess spawned per keystroke. Àwárí stays
resident as a tiny, GPU-free shell and drives a GPU overlay on demand, so opening
costs a fraction of a second rather than a full app boot.

The name is Yoruba for "a finding, a discovery".

![Àwárí launcher overview](docs/overview.png)

## Screenshots

<table>
  <tr>
    <td><img src="docs/screenshots/desktop-awari.png" width="380" alt="Àwárí on a desktop"></td>
    <td><img src="docs/screenshots/desktop-awari-goland.png" width="380" alt="Àwárí over GoLand"></td>
  </tr>
  <tr>
    <td><img src="docs/screenshots/desktop-awari-rv32.png" width="380" alt="Àwárí desktop view"></td>
    <td><img src="docs/screenshots/desktop-paper-rv32.png" width="380" alt="Àwárí with the Paper theme"></td>
  </tr>
</table>

## More demos

### Themes

<table>
  <tr>
    <td align="center"><img src="docs/screenshots/theme-verdant.png" width="360"><br><sub>Verdant</sub></td>
    <td align="center"><img src="docs/screenshots/theme-ember.png" width="360"><br><sub>Ember</sub></td>
  </tr>
  <tr>
    <td align="center"><img src="docs/screenshots/theme-paper.png" width="360"><br><sub>Paper</sub></td>
    <td align="center"><img src="docs/screenshots/theme-tokyonight.png" width="360"><br><sub>Tokyo Night</sub></td>
  </tr>
</table>

### Views

<table>
  <tr>
    <td align="center"><img src="docs/screenshots/view-paper-apps.png" width="360"><br><sub>Apps category</sub></td>
    <td align="center"><img src="docs/screenshots/view-paper-alt-enter.png" width="360"><br><sub>Alt+Enter action menu</sub></td>
  </tr>
</table>

## Motivation

Most launchers are slow in ways you learn to live with. They fork a search tool
on every keystroke (fd, fzf, rg) and pay for it in process startups and parsing.
Or they're a web view that spins up a runtime just to draw a text field. Files
tend to be an afterthought stuck onto an app menu.

Àwárí goes the other way. File search runs in-process through fff-search, pinned
and ranked, so there's no subprocess per character. A lightweight, GPU-free shell
stays resident and drives the overlay on demand; pressing Super sends a socket
message that opens it and takes the keyboard within a fraction of a second.
Windows, apps, and files all go into one ranking, so you get whichever you meant.
By default the overlay is kept alive (hidden) between uses for instant re-opens,
holding ~tens of MB while idle; with `keep-alive = false` (or the daemon flag
`--no-keep-alive`) it exits on dismiss so only the tiny shell stays up, at the
cost of a ~100 ms rebuild on the next open.

## Usage

Bind a key in your compositor to `awari toggle-launcher` (for example
Super+Space), then press it to open the overlay. Typing filters the list and the
top match stays selected. `Enter` activates the selected result and closes;
`Escape` or a click on the background dismisses without selecting. `Up` and
`Down` move the selection.

`Tab` completes the query: it ghosts in the selected match, or fills the box
with the selected row's full value. `Shift`+`Up`/`Down` recalls previous
queries.

`Alt`+`Enter` opens a per-row action menu. The items depend on the result:
Open, Show in Folder, Copy Path, Run in Terminal, and Run.

Query modes:
- A path-shaped query (starting with `~`, `/`, or `.`, or containing `/`)
  browses the filesystem, files first, then apps and windows. Constraints like
  `*.pdf` and `!node_modules/` apply, and `../` moves up a directory.
- `o:<path>` opens a path and lists its real entries as you type.
- `r:<regex>` filters files by regular expression.
- `> <command>` runs the rest as a shell command in a terminal.
- An arithmetic query shows its result; `Enter` copies it.

Category chips (All, Apps, Files, Commands, Windows) narrow the source.

## How it stays fast

- File search is in-process via fff-search (0.10.5), not a spawned fd/fzf/rg.
- A lightweight, GPU-free shell stays resident and drives a layer-shell overlay
  on demand; once open, typing is one redraw plus a keyboard-interactivity
  request, not a new spawn.
- fff does fuzzy path matching, frecency, and git-status tagging. Our own
  `matchq` ranks windows and apps without allocating per keystroke.
- IPC read to damage is under 2 ms p99. The first paint after a cold open is
  bounded by process and GPU init (sub-150 ms in practice); reduced-motion sets
  the animation duration to zero.

## Features

- **Lightweight**: A tiny, GPU-free background daemon stays resident at just a few MB. By default, the heavy GPU overlay is kept alive but hidden between uses for instant re-opens (~19 ms), holding roughly 77 MB while idle. If you prefer to save every megabyte of RAM, set `keep-alive = false` (or run with `--no-keep-alive`) to completely tear down the GPU stack on dismiss, dropping your idle footprint to just 8.5 MB.
- **On-demand**: searches run in-process via fff-search and rank by frecency, with no per-keystroke subprocess overhead. The toggle command returns in ~2.78 ms, so the overlay opens in a fraction of a second when cold and instantly when kept alive.
- **One suggestion, not a list**: inline ghost-text completion; the full alternates list only shows on ↓.
- **Wayland-native**: one binary for any Wayland compositor. The overlay uses `wlr-layer-shell` (niri, Hyprland, sway, and other wlroots-family compositors like river and labwc); on compositors without it, like GNOME/Mutter, it falls back to a normal window for apps, files, and commands.
- **Unified results**: windows (focus), apps (XDG `.desktop`), and files in one fuzzy-, frecency-ranked list. Apps are always indexed; files and windows can be toggled off.
- **Empty query**: shows recent apps first, then open windows (no file dump).
- **Query prefixes**: `>` runs a shell command in a terminal, `o:<path>` opens a path and lists its real entries as you type (`~` and paths relative to `$HOME` both work), `r:<regex>` filters files by regular expression.
- **Path navigation**: path-shaped queries (starting with `~`, `/`, `.`, or containing `/`) browse the filesystem; constraints like `*.pdf` and `!node_modules/` apply.
- **Calculator**: an arithmetic query shows the result inline, and Enter copies it.
- **Smart completion**: `Tab` autocompletes with the selected result's full value; `Shift`+`↑`/`↓` recalls previous queries.
- **Action menu**: `Alt`+`Enter` opens a per-row menu: Open, Show in Folder, Copy Path, Run in Terminal, Run.
- **Category chips**: clickable All, Apps, Files, Commands, Windows.
- **Theming**: KDL with hex color tokens (no CSS, no remote fetches); nine built-in presets, per-token overrides, and aliases (`select`, `hover`/`surface`, `fg`, `muted`, `faint`); configurable font and size.
- **Lockfiles hidden**: `Cargo.lock`, `package-lock.json`, and `*.lock` are skipped by default.
- **Monitor-aware**: the overlay opens on the focused output.

## Memory

The resident shell is idle when the overlay is closed (a few MB, the GPU process
having exited); sustained CPU above about 1% is a bug. Memory
is bounded too. Per-directory file indexes are capped with LRU eviction and each
picker's cache is finite, so browsing through many folders doesn't pile up
indexes.

## Build

Linux with a Wayland compositor. Best on niri, Hyprland, and sway, plus other
wlroots-family compositors (river, labwc, and others) that provide
`wlr-layer-shell`;
on GNOME/Mutter it falls back to a normal window for apps, files, and commands.

```sh
# Debian/Ubuntu
sudo apt install libwayland-dev libxkbcommon-dev libegl-dev pkg-config
# Fedora
sudo dnf install wayland-devel libxkbcommon-devel mesa-libEGL-devel

cargo test
cargo run -p awari
awari ping
```

## Install

`awari` is not published to crates.io, because it depends on a vendored, patched
GPUI committed under `.third_party/zed` (a local `[patch]` can't be published).
Install it from the git repo instead:

```sh
cargo install --git https://github.com/borngraced/awari awari
```

This compiles GPUI from source, so it needs the Wayland/NixKB/EGL dev libraries
listed in [Build](#build). The result is a single `awari` binary that also runs
the GPU overlay (it re-executes itself as `awari gui`), so the one install is
complete. Then start it as a resident service (below) and bind a key.

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
scripts, or shell interpolation. Every block and token is optional, so anything
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
