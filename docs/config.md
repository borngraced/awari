# Keybindings & Configuration

Àwárí runs as a background daemon. You toggle the overlay from a compositor
keybind, not from inside the app.

## Opening the launcher

Set a key in your compositor (Sway, Hyprland, and so on) to run one of these:

```sh
awari toggle-launcher   # show or hide the overlay
awari open-launcher     # show it without toggling
```

## Navigation

| Key | Action |
| --- | --- |
| `↑` / `↓` (or `ArrowUp` / `ArrowDown`) | Move the selection |
| `Enter` / `Return` | Activate the selected row (its default action) |
| `Esc` | Dismiss the launcher |
| `Tab` | Autocomplete the query with the selected result's full value (file path, app name, or command) |
| `←` / `→` | Move the text cursor within the input |
| `Home` / `End` | Jump the cursor to the start or end of the input |
| `Backspace` / `Delete` | Edit the input |
| `Shift` + `↑` / `↓` | Recall previous queries from history without disturbing the live query |

## Actions via Alt + Enter

Press `Alt+Enter` to open an action menu for the selected row. The actions
depend on the row type:

- File: Open, Show in Folder, Copy Path, Run in Terminal
- App: Open, Copy Path
- Window: Open (focus)
- Command: Run, Copy Path

Inside the menu, `↑` / `↓` choose an action, `Enter` applies it, and `Esc`
closes the menu.

## Categories

The chips All, Apps, Files, Commands, and Windows are clickable and filter
results. The Commands tab treats the query as a shell command (the `>` prefix
is optional there). Windows lists open toplevels only.

## Query prefixes

These are recognized by their leading token, which shows up in the accent
color:

- `>` runs the rest as a shell command in a terminal.
- `o:<path>` opens a path with the default handler. `~` and `/` are absolute. A
  bare name resolves relative to `$HOME`. Unlike a plain path query, `o:` lists
  the real entries under what you typed (it reads the filesystem directly, so it
  works outside your configured search roots) and shows a direct
  "Open `<path>`" row only when that exact path already exists. A path that does
  not exist produces no optimistic row, so you pick from actual files and
  directories. This works for both files and directories. The latter opens in
  your file manager.
- `r:<regex>` treats the file query as a regular expression matched against each
  candidate's full path.
- Any query that looks like a path (it starts with `~`, `/`, or `.`, or it
  contains `/`) switches into path navigation, and file-finder constraints such
  as `*.pdf` and `!node_modules/` apply.

## Configuration

The config file lives at `~/.config/awari/config.kdl` (KDL format). Unknown keys
are ignored, and there is no `exec` support. This is configuration only. Colors
are written as hex (`#rrggbb` or `#rrggbbaa`), and durations are in
milliseconds.

### files

```kdl
files {
    roots "~/projects" "/data/docs"   // dirs the file index searches
    index-lockfiles false             // show Cargo.lock, *.lock, and so on (default: hidden)
    regex false                       // treat every file query as regex
    max-results 50                    // max file rows shown
}
```

- `roots` sets the directories to index. Empty means the existing XDG user dirs.
  `/` is dropped, and `~` expands to your home directory.
- `index-lockfiles` (default `false`) shows lock files like `Cargo.lock`,
  `package-lock.json`, and `*.lock` when `true`. They are hidden by default.
- `regex` (default `false`) treats every file query as a regular expression. The
  `r:` prefix forces regex on a single query no matter what this setting says.
- `max-results` (default `50`) controls how many file rows appear.

### fff

Toggles for the fff-search file indexers. The search root itself is always the
configured `files.roots`, and home-directory scanning turns on automatically
when a root is `$HOME`, so neither is configurable here.

```kdl
fff {
    watch true             // background file watcher
    fs-root-scanning true  // allow indexing the filesystem root
    mmap-cache false       // pre-populate mmap caches for top-frecency files
    content-indexing false // build a content index for content-aware filtering
    follow-symlinks false  // index through symbolic links
}
```

- `watch` (default `true`) spawns the background watcher that keeps the index
  up to date.
- `fs-root-scanning` (default `true`) permits `/` as a scan root.
- `mmap-cache` (default `false`) and `content-indexing` (default `false`)
  trade memory for faster subsequent filtering.
- `follow-symlinks` (default `false`) indexes through symbolic links.

The transient per-directory picker used for path navigation never spawns a
watcher and never broadens scanning beyond the typed directory; it honors
`mmap-cache`, `content-indexing`, and `follow-symlinks`.

### sources

```kdl
sources {
    windows true
    files true
}
```

Toggle the window and file sources independently. Apps are always indexed and
cannot be turned off.

### top-level

```kdl
max-results 30   // max total rows in the All view (apps + files + windows)
```

This caps the combined result list in the default All view. The Files tab uses
`files.max-results` instead.

### keep-alive

```kdl
keep-alive true   // default: keep the GPU overlay in memory between uses
```

Controls what happens to the GPU overlay after you dismiss the launcher:

- `keep-alive true` (default): the overlay stays in memory, hidden, between
  dismisses. Re-opens are instant, at the cost of holding the GPU process in
  memory while idle.
- `keep-alive false`: the overlay process exits on dismiss, leaving only the
  tiny GPU-free shell. The next open rebuilds the interface (a cold start).

The daemon flag `--no-keep-alive` forces drop mode regardless of this setting.
`awari gui` inherits the mode from how the daemon launched it
(`gui --no-keep-alive` for drop, `gui --hidden` for keep-alive).

### motion

```kdl
motion {
    reduced false     // disable the open/close animation
    duration-ms 140   // open/close and panel-height spring settle (0-1000; 0 snaps)
}
```

### theme

```kdl
theme {
    name "awari"              // load a built-in preset
    font "JetBrains Mono"
    font-size 13              // 8-64
    accent    "#b4a0ff"
    bg        "#141416"
    panel     "#1b1b1e"
    text      "#f2eef9"
    text-dim  "#8c899b"
    border    "#2b2b30"
}
```

- `name` loads a preset, and any token you set below overrides it.
- `font` and `font-size` set the UI typeface (8 to 64). The bundled JetBrains
  Mono is the default; set any installed family to switch, or `font "default"`
  to restore GPUI's system UI font. Fonts are embedded in the binary, so no
  font is required on the host.
- Available presets: `awari` (the default), `ash`, `ember`, `verdant`, `paper`,
  `mono`, `nord`, `tokyonight`, `catppuccin`, `gruvbox`.
- Color tokens: `accent`, `accent-dim` (alias `select`), `bg`, `panel`, `raise`
  (alias `hover` / `surface`), `border`, `text` (alias `fg`), `text-dim` (alias
  `muted`), `text-faint` (alias `faint`), `scrim`.

### Example

```kdl
files {
    roots "~/code"
    max-results 80
}
fff {
    watch true
}
sources {
    windows true
    files true
}
max-results 40
motion {
    reduced false
    duration-ms 120
}
theme {
    name "catppuccin"
    accent "#cba6f7"
}
```
