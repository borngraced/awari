# Keybindings

Àwárí runs as a resident daemon; the overlay is toggled by a compositor
keybind, not an in-app shortcut.

## Opening

Bind a key in your compositor (Sway, Hyprland, …) to run:

```sh
awari toggle-launcher   # show / hide the overlay
awari open-launcher     # show only (no toggle)
```

## Navigation

| Key | Action |
| --- | --- |
| `↑` / `↓` (or `ArrowUp` / `ArrowDown`) | Move the selection |
| `Enter` / `Return` | Activate the selected row (its default action) |
| `Esc` | Dismiss the launcher |
| `Tab` | Autocomplete the query with the selected result's full value (file path / app name / command) |
| `←` / `→` | Move the text cursor within the input |
| `Home` / `End` | Jump the cursor to the start / end of the input |
| `Backspace` / `Delete` | Edit the input |
| `Shift` + `↑` / `↓` | Recall previous queries from history without disturbing the live query |

## Actions — `Alt` + `Enter`

`Alt+Enter` opens an action menu for the selected row. The available actions
depend on the row type:

- **File**: Open · Show in Folder · Copy Path · Run in Terminal
- **App**: Open · Copy Path
- **Window**: Open (focus)
- **Command**: Run · Copy Path

Inside the menu: `↑` / `↓` choose an action, `Enter` applies it, `Esc` closes.

## Categories

The chips — **All · Apps · Files · Commands · Windows** — are clickable to
filter results (Windows has no dedicated ranking; its rows surface under All).
The **Commands** tab is only populated by the `>` command mode.

## Query prefixes

These are detected by their leading token (rendered in the accent color):

- **`>`** — run the rest as a shell command in a terminal.
- **`o:<path>`** — open a path with the default handler. `~` and `/` are
  absolute; a bare name resolves relative to `$HOME`. Unlike a plain path
  query, `o:` lists the *real* entries under what you've typed (via the
  filesystem, so it works outside the configured search roots) and shows a
  direct **Open `<path>`** row only when that exact path already exists. A
  nonexistent path produces no optimistic row — you pick from actual files and
  directories. Works for files and directories (the latter opens in the file
  manager).
- **`r:<regex>`** — treat the file query as a regular expression, matched
  against each candidate's full path.
- A query that looks like a path (starts with `~`, `/`, or `.`, or contains
  `/`) enters path navigation; fff constraints such as `*.pdf` and
  `!node_modules/` apply.

## Configuration

The config file is `~/.config/awari/config.kdl` (KDL). Unknown keys are
ignored, and there is no `exec` — configuration only. Colors are hex
(`#rrggbb` or `#rrggbbaa`); durations are milliseconds.

### `files`

```kdl
files {
    roots "~/projects" "/data/docs"   // dirs fff-search indexes
    index-lockfiles false             // show Cargo.lock, *.lock, … (default: hidden)
    regex false                       // treat every file query as regex
    max-results 50                    // max file rows shown
}
```

- `roots` — directories to index. Empty = the existing XDG user dirs. `/` is
  dropped. `~` expands to your home directory.
- `index-lockfiles` (default `false`) — when `true`, lock files
  (`Cargo.lock`, `package-lock.json`, `*.lock`, …) appear in results; by
  default they are hidden.
- `regex` (default `false`) — treat every file query as a regular expression.
  The `r:` prefix forces regex per-query regardless of this setting.
- `max-results` (default `50`) — how many file rows to display.

### `sources`

```kdl
sources {
    windows true
    apps true
    files true
}
```

Toggle each result source independently.

### top-level

```kdl
max-results 30   // max total rows in the All view (apps + files + windows)
```

Caps the combined result list in the default All view. The Files tab is capped
by `files.max-results` instead.

### `motion`

```kdl
motion {
    reduced false     // disable the open/close animation
    duration-ms 140   // animation duration (clamped to 0–1000)
}
```

### `theme`

```kdl
theme {
    name "gruvbox"            // load a built-in preset
    font "JetBrains Mono"
    font-size 13              // 8–64
    accent    "#8b7bf0"
    bg        "#0b0b0c"
    panel     "#141416"
    text      "#eceaf0"
    text-dim  "#8b8994"
    border    "#ffffff12"
}
```

- `name` loads a preset; explicit tokens below override it.
- `font` / `font-size` set the UI type (8–64).
- Color tokens: `accent`, `accent-dim` (alias `select`), `bg`, `panel`,
  `raise` (alias `hover` / `surface`), `border`, `text` (alias `fg`),
  `text-dim` (alias `muted`), `text-faint` (alias `faint`), `scrim`.

### Example

```kdl
files {
    roots "~/code"
    max-results 80
}
sources {
    windows true
    apps true
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

