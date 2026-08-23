# Àwárí — overlay launcher

| Field | Value |
|---|---|
| **Status** | Canonical. Supersedes `docs/architecture.md` (map shell). |
| **Date** | 2026-08-23 |
| **Name** | **Àwárí** (Yoruba *àwárí*: a finding). Binary and crate: `awari`. |
| **Compositor** | niri only (`niri-ipc = "=26.4.0"`) |
| **License** | GPL-3.0-or-later (the `niri-ipc` crate makes the binary a GPL derivative) |

The 3-second demo is: Super, type, the right window or app or file, first pixel on the next vsync. If the demo is a bar, a map, or a sliding desktop, the product is wrong.

## What it is

A long-lived process that owns one Wayland overlay. Closed, the overlay is unmapped and the process sleeps. Open, it has exclusive keyboard, a filter field, and a short ranked list. Super does not start a GUI. niri binds `spawn "awari" "toggle-launcher"`; that argv is a unix-socket client that writes one JSON line to `$XDG_RUNTIME_DIR/awari/ipc.sock` and exits. The overlay is already in the daemon.

This is the only surface. There is no exclusive zone, no HUD, no filmstrip, no OSD, no notification daemon, no tray, no peek. Those were shell. They are out.

GPUI is the right toolkit for this and was the wrong toolkit for a 32px sleeping bar. The frame clock may paint while the overlay is mapped. It must not paint while the overlay is unmapped. Unmap is how Exclusive is released and how idle is won. Opacity-0 mapped scrims are a last resort if remap misses the next frame; measure both, keep one.

Mechanically, gpui has no hide/unmap — closed means the window is destroyed, open rebuilds it. `Stats.launcher_open_to_first_commit_ms` (see `awari dump-stats`) records IPC-open → first render; if its p99 misses the vsync gate, switching to an opacity-0 mapped surface with runtime keyboard release is the only sanctioned change.

## Process

No args, or `awari daemon`, is the daemon. Single instance: bind `ipc.sock` after `Ping`; live `Ok` → exit 1; `ECONNREFUSED` → unlink stale sock and bind. `SO_PEERCRED`, same uid only. systemd `--user`, `Type=simple`, `Restart=on-failure`.

Client argv is only: `toggle-launcher`, `open-launcher`, `close-launcher`, `ping`, `dump-stats`. Filmstrip IPC is gone.

niri is optional at start (apps and files still work; window rows are empty until the command socket exists). Spawn and focus go through `Action::Spawn` / `FocusWindow`, never `SpawnSh` from a desktop file.

## Sources

One overlay, one ranked list, one activate. Four backends. **`fff-search` is only Files.** It does not see `.desktop` files, niri windows, or a command registry. Do not stretch it into the whole launcher.

**Windows.** Ours. Tiles on the active workspace of the output that showed the overlay. Label is title, then app_id, then `#id`. Activate is `FocusWindow`. A running app is a window, not a second spawn, when the query matches that window. Do not invent other-output or other-workspace rows in v1.

Output binding: the overlay opens with no explicit output, so niri attaches it to the focused output at open time; we do not choose. Row scope tracks the single focused workspace via `WorkspaceActivated { focused: true }` from the event stream, falling back to the first active workspace before any event arrives.

Dedup rule: an App row is suppressed iff some visible Window row's `app_id` equals that app's display name or its `StartupWMClass`, case-insensitively.

**Apps.** Ours. Cached `.desktop` entries from the XDG data dirs, user first, first-id wins. Skip `Hidden`, `NoDisplay`, non-`Application`. Parse `Exec` with no shell. Reject `%F` / `%U`. Omit `%f` / `%u` unless this activation picked a file. `Terminal=true` wraps `$TERMINAL -e` or the entry is dropped. `TryExec` must exist. Activate is `Spawn { command: argv }`. Do not concatenate user input onto Exec. `DBusActivatable` may wait; v1 spawn Exec is enough.

**Files.** v1. In-process [`fff-search`](https://crates.io/crates/fff-search) pinned `=0.10.5` (`FilePicker`), MIT, linked into the GPL binary. Not `fff-mcp`, not FFI, not a forked `fd`/`fzf`/`rg`. Defaults-only features: the `zlob` fast path needs a Zig toolchain and is forbidden; vendored libgit2 and LMDB are accepted upstream costs. FFF owns path fuzzy (typo-resistant), frecency, the background watcher, and git status on results. Activate is `xdg-open` on the path (portal later if we must). Directories: same. Content grep is not a launcher row in v1; git `modified` / `untracked` may show in the preview.

Indexing scope is load-bearing. `enable_fs_root_scanning` stays off. **Do not turn on `enable_home_dir_scanning` until RSS is measured.** Default roots: XDG user dirs that exist (`Desktop`, `Documents`, `Downloads`, `Music`, `Pictures`, `Videos`) plus explicit extra paths if we add a config key later. Honour `.gitignore` / `.ignore` inside those trees. A 14k-file tree is ~26MB resident in FFF’s own numbers; `$HOME` as one picker is how we blow the 100MB budget. If settled RSS with the default roots fails the budget, shrink roots, do not keep a bar to hide it.

The picker may still be scanning on first Super; show what it has, do not block open. Frecency is how “top match” is ordered for files. Do not hand-roll a second file scorer.

**Commands.** Ours if the chip exists (fixed registry: compositor verbs we actually implement). FFF does not index them.

Apps and the file picker load after the daemon is up, not on the first presented pixel. Windows stream from niri events.

The old map-shell non-goal “file indexer” is void. That was a sleeping bar. This product is a finder; files are the 3-second demo, not a later plugin.

## Query

One field. No tabs, no provider plugins, no prefix language beyond what the query already looks like.

Empty query: windows on this output, then recent apps — an in-memory activation list keyed by display name (most recent first, then alphabetical). No dump of `$HOME`.

Non-empty: rank across the active chip's sources (All = windows + apps + files + commands). Substring is the floor for the small sets (windows, apps, commands), ranked by our hand-rolled subsequence scorer (`matchq`: word-boundary and contiguity bonuses) so prefix beats scattered. Files go through FFF's matcher so `firfox` and transposed path fragments still hit. Cap the visible list. Kind is a quiet glyph on mixed lists.

Section order is fixed — windows, apps, files, commands — except path-shaped queries flip files to the front. Headers are labels, never selection slots.

If the query is path-shaped (`~`, `/`, `.`, or contains `/`), files win the ranking and FFF constraints (`*.pdf`, `!node_modules/`) are allowed. `../` is a path, not a search operator.

Enter activates the selected row and dismisses. Escape / click on the scrim / `toggle-launcher` while open dismisses without action. Up/down move the selection. Typing resets selection to 0.

No calculator, no clipboard history, no free-form shell, no web search. A **command** is a named verb in our registry, not `sh -c` from the query.

## Surface

Layer `overlay`, namespace `awari:launcher`, `exclusive_zone = 0`, full-output so fullscreen does not cover it. Keyboard Exclusive while mapped; None (by unmap) when closed. Scrim click dismisses; the panel swallows clicks. Input region empty when closed if the surface is somehow still mapped.

Centered panel, one search row, one list. Icons from the freedesktop name or path already on the desktop entry / window app_id; letter tile if missing. No preview pane in v1 (FFF can do it; the overlay should not). Reduced-motion: duration 0.

niri `spawn-at-startup "awari"` and `Mod+D { spawn "awari" "toggle-launcher"; }`. The bind is extra latency we do not control; the budget below starts at IPC read.

## Budgets

Closed daemon: `pidstat` on the process **>1% for 10s** is a fail. RSS well under 100MB after icon warm **and** the FFF index on the default roots has settled. Measure that; do not assume `$HOME` is free. If it fails, shrink roots. The watcher must not wake the GPU while the overlay is unmapped.

Open: IPC read → damage p99 **< 2ms**. First presented pixel **≤ next vsync** after that damage. Do not pack niri spawn + present into 16ms. Do not keep the GPU awake after unmap.

## Config

KDL, `~/.config/awari/config.kdl`. **No** `exec`, scripts, or shell interpolation. Unknown keys warn and ignore. This is the customization surface — not plugins.

**Theme.** Every concept token is a hex override (`#RGB` / `#RRGGBB` / `#RRGGBBAA`). Defaults are the overlay mock (violet accent, `#141416` panel). Keys: `accent`, `accent-dim`, `bg`, `panel`, `raise`, `border`, `text`, `text-dim`, `text-faint`, `scrim`. No CSS `url()`, no named colors, no fetching a theme from the network.

**Files.** `files { roots "~/Documents" "~/code" }`. Empty list = XDG user dirs that exist. `/` is dropped. `~` expands. Listing `$HOME` is allowed and is how you opt into home-wide FFF; the RSS budget still applies after the index settles.

**Sources.** `sources { windows true apps true files true commands true }` — hide a backend without recompiling.

**Motion.** `reduced`, `duration-ms` as before.

Example: `contrib/config.kdl`.

## Kill

Delete, do not hide: bar layer, exclusive zone, map crate usage, filmstrip, browse, HUD chips, menus, audio/network/power services, notifications ownership, OSD, tray/SNI, peek, filmstrip IPC, map docs as if they were current.

Keep: overlay launcher, strict desktop parse, unix IPC, niri adapter for spawn/focus/window list, lock/socket, GPUI layer-shell window.

`docs/architecture.md`, `docs/map.md`, and the map-shaped parts of `docs/surfaces.md` / `docs/performance.md` are historical. This file is the spec.

## Not v1

Other compositors. Plugins. Calculator. Clipboard. Free-form shell. Indexing `/` or unmeasured full-`$HOME`. Content grep as rows. `%F`/`%U` multi-file Exec. RemoteDesktop portal. A settings GUI. A second UI family. Reintroducing the map. Building our own file walker/scorer instead of `fff-search`.
