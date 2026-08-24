//! Overlay launcher daemon. No bar. Process stays alive with no windows.

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;
use std::sync::mpsc::Receiver;
use std::collections::HashMap;
use std::fs;

use gpui::{
    point, px, size, App, AppContext, Bounds, ClipboardItem, Context, Entity, Global, QuitMode,
    WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind, WindowOptions,
};
use niri_ipc::state::{EventStreamState, EventStreamStatePart};
use awari_compositor::{Compositor, CompositorCommand, NiriHandle, NiriInbox, NiriMsg};
use awari_ipc::ClientRequest;

use crate::config::Config;
use crate::desktop::DesktopApp;
use crate::files::FileHit;
use crate::lock::Stats;
use crate::ui::launcher::{self, Launcher, LauncherCmd, LauncherView};

/// Holds the daemon entity so GPUI does not drop it with no windows open.
struct Keep(#[allow(dead_code)] Entity<Daemon>);
impl Global for Keep {}

pub struct Daemon {
    niri: Option<Arc<NiriHandle>>,
    state: EventStreamState,
    output_name: String,
    /// Workspace id from the last focused `WorkspaceActivated`.
    focused_ws: Option<u64>,
    launcher: Option<WindowHandle<Launcher>>,
    launcher_open: bool,
    launcher_query: String,
    launcher_selected: usize,
    launcher_category: launcher::Category,
    /// Bumped on every open/close; deferred window updates from a previous
    /// generation are dropped so a stale hide cannot clobber a fresh open.
    launcher_gen: u64,
    apps: Vec<DesktopApp>,
    cfg: Config,
    stats: Arc<Mutex<Stats>>,
    /// Desktop names by last activation, most recent first.
    recents: Vec<String>,
    /// Past launcher queries, most recent first (Shift+Up/Down recall).
    query_history: Vec<String>,
    /// Position within `query_history` while recalling, or `None` for the
    /// live (currently typed) query.
    history_cursor: Option<usize>,
    /// Query that was on screen before we entered history recall, restored
    /// when the user steps back past the newest entry.
    history_live: Option<String>,
    /// Launch counts per app name; boosts repeated picks in ranking.
    app_usage: HashMap<String, u64>,
    files_tx: crate::files::Files,
    files_seq: u64,
    file_hits: Vec<FileHit>,
    /// Cached window list, rebuilt only when niri reports a change (not on
    /// every keystroke), so `filtered_rows` can borrow it without re-cloning
    /// every title/app_id per character typed.
    windows_list: Vec<(u64, String, Option<String>)>,
}

impl Daemon {
    pub fn start(
        cx: &mut App,
        niri: Option<Arc<NiriHandle>>,
        stats: Arc<Mutex<Stats>>,
        cfg: Config,
    ) {
        cx.set_quit_mode(QuitMode::Explicit);
        let daemon = cx.new(|cx| Self::new(cx, niri, stats, cfg));
        // Prewarm: build the overlay now (wgpu device, shaders, fonts) so
        // the first Super press costs a frame instead of full stack init.
        // The null-buffer hide is queued before any configure roundtrip
        // completes, so the surface never maps and never grabs the keyboard.
        // Overlay builds here once; it stays mapped-but-empty (transparent,
        // keyboard None, no input region) so wgpu/fonts warm at boot.
        daemon.update(cx, |d, cx| d.ensure_launcher(cx));
        cx.set_global(Keep(daemon));
    }

    fn new(
        cx: &mut Context<Self>,
        niri: Option<Arc<NiriHandle>>,
        stats: Arc<Mutex<Stats>>,
        cfg: Config,
    ) -> Self {
        let inbox = NiriInbox::start();
        let roots = if cfg.sources.files {
            cfg.files.resolved_roots()
        } else {
            Vec::new()
        };
        let (files_tx, files_rx) = crate::files::Files::spawn(
            roots,
            crate::files::FilesOptions {
                index_lockfiles: cfg.files.index_lockfiles,
                regex: cfg.files.regex,
            },
        );
        let (apps_tx, apps_rx) = std::sync::mpsc::channel::<Vec<DesktopApp>>();
        let mut daemon = Self {
            niri,
            state: EventStreamState::default(),
            output_name: String::new(),
            focused_ws: None,
            launcher: None,
            launcher_open: false,
            launcher_query: String::new(),
            launcher_selected: 0,
            launcher_category: launcher::Category::All,
            launcher_gen: 0,
            apps: Vec::new(),
            cfg,
            stats,
            recents: Vec::new(),
            query_history: Vec::new(),
            history_cursor: None,
            history_live: None,
            app_usage: HashMap::new(),
            files_tx,
            files_seq: 0,
            file_hits: Vec::new(),
            windows_list: Vec::new(),
        };
        spawn_niri_pump(cx, inbox);
        spawn_ipc(cx);
        spawn_files_pump(cx, files_rx);
        spawn_apps_pump(cx, apps_rx);
        // Scan `.desktop` files off the bootstrap path so the overlay prewarm
        // (wgpu device, shaders, fonts) runs first; apps swap in when ready.
        thread::Builder::new()
            .name("awari-apps-scan".into())
            .spawn(move || {
                let _ = apps_tx.send(crate::desktop::scan_applications());
            })
            .ok();
        daemon.load_history();
        daemon.load_usage();
        daemon
    }

    fn apply_niri(&mut self, msgs: Vec<NiriMsg>) -> bool {
        let changed = !msgs.is_empty();
        for msg in msgs {
            match msg {
                NiriMsg::Event(ev) => {
                    if let niri_ipc::Event::WorkspaceActivated { id, focused: true } = &ev {
                        self.focused_ws = Some(*id);
                        if let Some(w) = self.state.workspaces.workspaces.values().find(|w| w.id == *id)
                            && let Some(o) = w.output.clone()
                        {
                            self.output_name = o;
                        }
                    }
                    let _ = self.state.apply(ev);
                }
                NiriMsg::Outputs(outs) => {
                    if self.output_name.is_empty() {
                        if let Some(name) = self
                            .state
                            .workspaces
                            .workspaces
                            .values()
                            .find(|w| w.is_active)
                            .and_then(|w| w.output.clone())
                        {
                            self.output_name = name;
                        } else if let Some((name, _)) = outs.iter().find(|(_, o)| o.logical.is_some())
                        {
                            self.output_name = name.clone();
                        }
                    }
                }
                NiriMsg::Degraded(e) => tracing::warn!(%e, "niri degraded"),
                NiriMsg::Version(v) => {
                    tracing::info!(niri = %v, pin = awari_compositor::NIRI_IPC_PIN);
                }
            }
        }
        if self.output_name.is_empty() {
            if let Some(name) = self
                .state
                .workspaces
                .workspaces
                .values()
                .find(|w| w.is_active)
                .and_then(|w| w.output.clone())
            {
                self.output_name = name;
            }
        }
        self.refresh_windows();
        changed
    }

    /// Rebuild the cached window list from the current niri state. Cheap enough
    /// to run on every niri batch, and far cheaper than rebuilding it on every
    /// keystroke inside `filtered_rows`.
    fn refresh_windows(&mut self) {
        self.windows_list = self.launcher_windows();
    }

    fn launcher_windows(&self) -> Vec<(u64, String, Option<String>)> {
        let ws_id = match self.focused_ws {
            Some(id) => Some(id),
            None => self
                .state
                .workspaces
                .workspaces
                .values()
                .find(|w| {
                    w.is_active
                        && (self.output_name.is_empty()
                            || w.output.as_deref() == Some(self.output_name.as_str()))
                })
                .map(|w| w.id),
        };
        self.state
            .windows
            .windows
            .values()
            .filter(|w| ws_id.is_some() && w.workspace_id == ws_id)
            .map(|w| {
                (
                    w.id,
                    w.title
                        .clone()
                        .or_else(|| w.app_id.clone())
                        .unwrap_or_else(|| format!("#{}", w.id)),
                    w.app_id.clone(),
                )
            })
            .collect()
    }

    fn filtered_rows(&self) -> Vec<launcher::LauncherRow> {
        let apps = if self.cfg.sources.apps {
            self.apps.as_slice()
        } else {
            &[]
        };
        let empty_windows: &[(u64, String, Option<String>)] = &[];
        let windows = if self.cfg.sources.windows {
            self.windows_list.as_slice()
        } else {
            empty_windows
        };
        let files = if self.cfg.sources.files {
            self.file_hits.as_slice()
        } else {
            &[]
        };
        launcher::filter_rows(
            &self.launcher_query,
            apps,
            windows,
            files,
            &self.recents,
            &self.app_usage,
            self.launcher_category,
        )
    }

    fn sync_launcher(&mut self, cx: &mut Context<Self>) {
        let Some(h) = self.launcher else {
            return;
        };
        let rows = self.filtered_rows();
        if self.launcher_selected >= rows.len() {
            self.launcher_selected = rows.len().saturating_sub(1);
        }
        let view = LauncherView {
            open: self.launcher_open,
            query: self.launcher_query.clone(),
            selected: self.launcher_selected,
            rows,
            theme: self.cfg.theme.clone(),
            category: self.launcher_category,
        };
        let generation = self.launcher_gen;
        let shell = cx.entity().downgrade();
        cx.defer(move |cx| {
            // A stale view (from before a toggle) must not overwrite the
            // current one.
            let current = shell.upgrade().map(|d| d.read(cx).launcher_gen);
            if current != Some(generation) {
                return;
            }
            let _ = h.update(cx, |l, _, cx| {
                l.apply_view(view, cx);
                cx.notify();
            });
        });
    }

    fn ensure_launcher(&mut self, cx: &mut Context<Self>) {
        if self.launcher.is_some() {
            return;
        }
        let shell = cx.entity().downgrade();
        let theme = self.cfg.theme.clone();
        let bounds = Bounds {
            origin: point(px(0.), px(0.)),
            size: size(px(1920.), px(launcher::LAUNCHER_H)),
        };
        match cx.open_window(
            WindowOptions {
                titlebar: None,
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                app_id: Some("awari".into()),
                window_background: WindowBackgroundAppearance::Transparent,
                kind: WindowKind::LayerShell(launcher::layer_opts()),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| Launcher::new(shell, theme, window, cx)),
        ) {
            Ok(handle) => self.launcher = Some(handle),
            Err(e) => tracing::warn!(%e, "launcher overlay failed to open"),
        }
    }

    pub(crate) fn apply_launcher_cmd(&mut self, cmd: LauncherCmd, cx: &mut Context<Self>) {
        match cmd {
            LauncherCmd::Dismiss => self.dismiss_launcher(cx),
            LauncherCmd::Activate { index } => {
                self.launcher_selected = index;
                self.activate_launcher_row(cx);
            }
            LauncherCmd::Select { index } => {
                if self.launcher_selected != index {
                    self.launcher_selected = index;
                    self.sync_launcher(cx);
                }
            }
            LauncherCmd::SetCategory { category } => {
                if self.launcher_category != category {
                    self.launcher_category = category;
                    self.launcher_selected = 0;
                    self.sync_launcher(cx);
                }
            }
            LauncherCmd::Key { key, ch, shift } => {
                self.launcher_key(&key, ch.as_deref(), shift, cx)
            }
            LauncherCmd::SetQuery { query } => {
                if self.launcher_query != query {
                    self.launcher_query = query;
                    self.launcher_selected = 0;
                    // Typing returns to the live query, leaving history recall.
                    self.history_cursor = None;
                    self.history_live = None;
                    self.refresh_file_hits();
                    self.sync_launcher(cx);
                }
            }
            LauncherCmd::OpenToRender { ms } => {
                let mut s = self.stats.lock().expect("stats");
                s.launcher_open_to_first_commit_ms = Some(ms);
                tracing::info!(ms, "launcher open → first render");
            }
        }
    }

    fn set_launcher_open(&mut self, open: bool, cx: &mut Context<Self>) {
        // Bumped on every transition; deferred window work from a previous
        // generation is dropped so a stale hide cannot clobber a fresh open
        // (destroy-on-close masked this by killing the handle instead).
        self.launcher_gen += 1;
        if open {
            self.launcher_open = true;
            self.launcher_query.clear();
            self.launcher_selected = 0;
            self.launcher_category = launcher::Category::All;
            self.file_hits.clear();
            self.files_seq = self.files_tx.invalidate();
            let started = Instant::now();
            self.ensure_launcher(cx);
            if let Some(h) = self.launcher.clone() {
                let generation = self.launcher_gen;
                let shell = cx.entity().downgrade();
                cx.defer(move |cx| {
                    let current = shell.upgrade().map(|d| d.read(cx).launcher_gen);
                    if current != Some(generation) {
                        return;
                    }
                    let _ = h.update(cx, |l, window, _| {
                        l.arm_open_timer(started);
                        window.set_keyboard_interactivity(
                            gpui::layer_shell::KeyboardInteractivity::Exclusive,
                        );
                    });
                });
            }
            self.sync_launcher(cx);
        } else {
            self.dismiss_launcher(cx);
        }
    }

    fn dismiss_launcher(&mut self, cx: &mut Context<Self>) {
        self.launcher_open = false;
        // Return file-search RAM to a near-baseline "sleeping" footprint:
        // drop the per-directory scratch indexes and rebuild the root
        // indexes from scratch. The next open re-indexes on demand.
        self.files_tx.clear();
        // Remember the query just used so Shift+Up can recall it next time.
        let q = self.launcher_query.trim().to_string();
        if !q.is_empty() {
            self.query_history.retain(|h| *h != q);
            self.query_history.insert(0, q);
            self.query_history.truncate(50);
            self.save_history();
        }
        self.history_cursor = None;
        self.history_live = None;
        // Resident overlay: the window stays alive for the whole session.
        let Some(h) = self.launcher.clone() else {
            return;
        };
        let theme = self.cfg.theme.clone();
        let generation = self.launcher_gen;
        let shell = cx.entity().downgrade();
        cx.defer(move |cx| {
            // Drop stale closes: a newer open must not be hidden by us.
            let current = shell.upgrade().map(|d| d.read(cx).launcher_gen);
            if current != Some(generation) {
                return;
            }
            let _ = h.update(cx, |l, window, cx| {
                l.apply_view(LauncherView::closed(theme), cx);
                l.closing = true;
                window.set_input_region(Some(&[]));
                window.set_keyboard_interactivity(
                    gpui::layer_shell::KeyboardInteractivity::None,
                );
                cx.notify();
            });
        });
    }

    fn launcher_key(
        &mut self,
        key: &str,
        _ch: Option<&str>,
        shift: bool,
        cx: &mut Context<Self>,
    ) {
        let key = key.to_ascii_lowercase();
        // Shift+Up/Down recall past queries without disturbing the live one.
        if shift && matches!(key.as_str(), "up" | "arrowup" | "down" | "arrowdown") {
            self.history_step(key == "down" || key == "arrowdown", cx);
            return;
        }
        match key.as_str() {
            "escape" | "esc" => self.dismiss_launcher(cx),
            "enter" | "return" => self.activate_launcher_row(cx),
            "up" | "arrowup" => {
                self.launcher_selected = self.launcher_selected.saturating_sub(1);
                self.sync_launcher(cx);
            }
            "down" | "arrowdown" => {
                self.launcher_selected = self.launcher_selected.saturating_add(1);
                self.sync_launcher(cx);
            }
            _ => {}
        }
    }

    /// Step through `query_history`. `newer` = Shift+Down (toward the live
    /// query), `older` = Shift+Up (deeper into history, most-recent-first).
    fn history_step(&mut self, newer: bool, cx: &mut Context<Self>) {
        if self.query_history.is_empty() {
            return;
        }
        let len = self.query_history.len();
        let next = match self.history_cursor {
            None => {
                if newer {
                    None
                } else {
                    self.history_live = Some(self.launcher_query.clone());
                    Some(0)
                }
            }
            Some(i) => {
                if newer {
                    if i == 0 {
                        let live = self.history_live.take().unwrap_or_default();
                        self.history_cursor = None;
                        self.launcher_query = live;
                        self.launcher_selected = 0;
                        self.refresh_file_hits();
                        self.sync_launcher(cx);
                        return;
                    }
                    Some(i - 1)
                } else if i + 1 < len {
                    Some(i + 1)
                } else {
                    Some(len - 1)
                }
            }
        };
        self.history_cursor = next;
        if let Some(idx) = next {
            self.launcher_query = self.query_history[idx].clone();
        }
        self.launcher_selected = 0;
        self.refresh_file_hits();
        self.sync_launcher(cx);
    }

    /// Load past queries from `$XDG_RUNTIME_DIR/.awari/history`.
    fn load_history(&mut self) {
        let path = awari_ipc::runtime_dir().join("history");
        if let Ok(s) = fs::read_to_string(&path) {
            self.query_history = s
                .lines()
                .map(str::to_string)
                .filter(|l| !l.trim().is_empty())
                .collect();
        }
    }

    /// Persist `query_history` to `history` (newline-separated).
    fn save_history(&self) {
        let dir = awari_ipc::runtime_dir();
        let _ = fs::create_dir_all(&dir);
        let _ = fs::write(dir.join("history"), self.query_history.join("\n"));
    }

    /// Load app launch counts from `$XDG_RUNTIME_DIR/.awari/usage`.
    fn load_usage(&mut self) {
        let path = awari_ipc::runtime_dir().join("usage");
        if let Ok(s) = fs::read_to_string(&path) {
            for line in s.lines() {
                if let Some((name, cnt)) = line.split_once('\t') {
                    if let Ok(n) = cnt.parse::<u64>() {
                        self.app_usage.insert(name.to_string(), n);
                    }
                }
            }
        }
    }

    /// Persist `app_usage` to `usage` as `name\tcount` lines.
    fn save_usage(&self) {
        let dir = awari_ipc::runtime_dir();
        let _ = fs::create_dir_all(&dir);
        let body: Vec<String> = self
            .app_usage
            .iter()
            .map(|(k, v)| format!("{}\t{}", k, v))
            .collect();
        let _ = fs::write(dir.join("usage"), body.join("\n"));
    }

    fn refresh_file_hits(&mut self) {
        self.file_hits.clear();
        if self.cfg.sources.files && !self.launcher_query.trim().is_empty() {
            self.files_seq = self.files_tx.query(&self.launcher_query);
        } else {
            self.files_seq = self.files_tx.invalidate();
        }
    }

    fn activate_launcher_row(&mut self, cx: &mut Context<Self>) {
        let rows = self.filtered_rows();
        let Some(row) = rows.get(self.launcher_selected) else {
            return;
        };
        let kind = row.kind.clone();
        self.run_row_action(kind, launcher::RowAction::Open, cx);
    }

    /// Perform `action` on `kind`. Index 0 of `RowKind::actions` is `Open`,
    /// which reuses the original activation path; the others are auxiliary
    /// (reveal, copy path, run in terminal).
    pub fn run_row_action(
        &mut self,
        kind: launcher::RowKind,
        action: launcher::RowAction,
        cx: &mut Context<Self>,
    ) {
        match action {
            launcher::RowAction::Open => self.activate_kind(kind, cx),
            launcher::RowAction::ShowInFolder => {
                if let launcher::RowKind::File { path } = &kind {
                    crate::files::reveal(path);
                }
                self.dismiss_launcher(cx);
            }
            launcher::RowAction::CopyPath => {
                let text = match &kind {
                    launcher::RowKind::File { path } => path.display().to_string(),
                    launcher::RowKind::App { exec } => exec.join(" "),
                    launcher::RowKind::Window { .. } => String::new(),
                };
                if !text.is_empty() {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
                self.dismiss_launcher(cx);
            }
            launcher::RowAction::RunInTerminal => {
                if let launcher::RowKind::File { path } = &kind {
                    let dir = path.parent().unwrap_or(path);
                    crate::files::run_in_terminal(dir);
                }
                self.dismiss_launcher(cx);
            }
        }
    }

    fn activate_kind(&mut self, kind: launcher::RowKind, cx: &mut Context<Self>) {
        if let launcher::RowKind::App { .. } = &kind {
            let rows = self.filtered_rows();
            if let Some(row) = rows.get(self.launcher_selected) {
                let name = row.label.clone();
                self.recents.retain(|n| *n != name);
                self.recents.insert(0, name.clone());
                self.recents.truncate(20);
                // Track launch frequency to bias ranking toward used apps.
                *self.app_usage.entry(name).or_insert(0) += 1;
                self.save_usage();
            }
        }
        self.dismiss_launcher(cx);
        match kind {
            launcher::RowKind::File { path } => {
                crate::files::activate(&path);
                return;
            }
            _ => {}
        }
        let niri = self.niri.clone();
        cx.defer(move |_cx| {
            let Some(niri) = niri else {
                return;
            };
            match kind {
                launcher::RowKind::App { exec } => {
                    let _ = niri.apply(CompositorCommand::Spawn { command: exec });
                }
                launcher::RowKind::Window { id } => {
                    let _ = niri.apply(CompositorCommand::FocusWindow { id });
                }
                launcher::RowKind::File { .. } => unreachable!("handled above"),
            }
        });
    }

    fn apply_ipc(&mut self, req: ClientRequest, cx: &mut Context<Self>) -> bool {
        match req {
            ClientRequest::ToggleLauncher => {
                self.set_launcher_open(!self.launcher_open, cx);
                true
            }
            ClientRequest::OpenLauncher => {
                self.set_launcher_open(true, cx);
                true
            }
            ClientRequest::CloseLauncher => {
                self.set_launcher_open(false, cx);
                true
            }
            _ => false,
        }
    }
}

fn spawn_niri_pump(cx: &mut Context<Daemon>, inbox: Arc<NiriInbox>) {
    let (tx, mut rx) = futures::channel::mpsc::unbounded();
    let wake = inbox.take_wake();
    thread::Builder::new()
        .name("awari-niri-pump".into())
        .spawn(move || {
            let Some(wake) = wake else { return };
            while wake.recv().is_ok() {
                let _ = tx.unbounded_send(inbox.drain());
            }
        })
        .ok();
    cx.spawn(async move |this, cx| {
        use futures::StreamExt;
        while let Some(msgs) = rx.next().await {
            let _ = this.update(cx, |d, cx| {
                let changed = d.apply_niri(msgs);
                if changed && d.launcher_open {
                    d.sync_launcher(cx);
                    cx.notify();
                }
            });
        }
    })
    .detach();
}

fn spawn_ipc(cx: &mut Context<Daemon>) {
    let Some(std_rx) = crate::lock::take_ipc_rx() else {
        return;
    };
    let (tx, mut rx) = futures::channel::mpsc::unbounded();
    thread::Builder::new()
        .name("awari-ipc-pump".into())
        .spawn(move || {
            while let Ok(req) = std_rx.recv() {
                let _ = tx.unbounded_send(req);
            }
        })
        .ok();
    cx.spawn(async move |this, cx| {
        use futures::StreamExt;
        while let Some(req) = rx.next().await {
            let _ = this.update(cx, |d, cx| {
                let changed = d.apply_ipc(req, cx);
                if changed {
                    cx.notify();
                }
            });
        }
    })
    .detach();
}

fn spawn_files_pump(cx: &mut Context<Daemon>, rx: Receiver<(u64, Vec<FileHit>)>) {
    let (tx, mut fut_rx) = futures::channel::mpsc::unbounded();
    thread::Builder::new()
        .name("awari-files-pump".into())
        .spawn(move || {
            for (seq, hits) in rx {
                if tx.unbounded_send((seq, hits)).is_err() {
                    return;
                }
            }
        })
        .ok();
    cx.spawn(async move |this, cx| {
        use futures::StreamExt;
        while let Some((seq, hits)) = fut_rx.next().await {
            let _ = this.update(cx, |d, cx| {
                // Drop answers that no longer match the newest query.
                if seq == d.files_seq {
                    d.file_hits = hits;
                    if d.launcher_open {
                        d.sync_launcher(cx);
                        cx.notify();
                    }
                }
            });
        }
    })
    .detach();
}

/// Streams the `.desktop` scan result into the daemon once it's ready, then
/// swaps `apps` and refreshes the open launcher. Mirrors the files pump shape.
fn spawn_apps_pump(cx: &mut Context<Daemon>, rx: Receiver<Vec<DesktopApp>>) {
    let (tx, mut fut_rx) = futures::channel::mpsc::unbounded();
    thread::Builder::new()
        .name("awari-apps-pump".into())
        .spawn(move || {
            if let Ok(apps) = rx.recv() {
                let _ = tx.unbounded_send(apps);
            }
        })
        .ok();
    cx.spawn(async move |this, cx| {
        use futures::StreamExt;
        while let Some(apps) = fut_rx.next().await {
            let _ = this.update(cx, |d, cx| {
                d.apps = apps;
                if d.launcher_open {
                    d.sync_launcher(cx);
                }
            });
        }
    })
    .detach();
}
