//! Overlay launcher daemon. No bar. Process stays alive with no windows.

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;
use std::sync::mpsc::Receiver;

use gpui::{
    point, px, size, App, AppContext, Bounds, Context, Entity, Global, QuitMode,
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
    files_tx: crate::files::Files,
    files_seq: u64,
    file_hits: Vec<FileHit>,
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
        let (files_tx, files_rx) = crate::files::Files::spawn(roots);
        let daemon = Self {
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
            apps: crate::desktop::scan_applications(),
            cfg,
            stats,
            recents: Vec::new(),
            files_tx,
            files_seq: 0,
            file_hits: Vec::new(),
        };
        spawn_niri_pump(cx, inbox);
        spawn_ipc(cx);
        spawn_files_pump(cx, files_rx);
        daemon
    }

    fn apply_niri(&mut self, msgs: Vec<NiriMsg>) {
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
        let windows = if self.cfg.sources.windows {
            self.launcher_windows()
        } else {
            Vec::new()
        };
        let files = if self.cfg.sources.files {
            self.file_hits.as_slice()
        } else {
            &[]
        };
        launcher::filter_rows(
            &self.launcher_query,
            apps,
            &windows,
            files,
            &self.recents,
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
            theme: self.cfg.theme,
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
                l.apply_view(view);
                cx.notify();
            });
        });
    }

    fn ensure_launcher(&mut self, cx: &mut Context<Self>) {
        if self.launcher.is_some() {
            return;
        }
        let shell = cx.entity().downgrade();
        let theme = self.cfg.theme;
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
            LauncherCmd::Key { key, ch } => self.launcher_key(&key, ch.as_deref(), cx),
            LauncherCmd::SetQuery { query } => {
                if self.launcher_query != query {
                    self.launcher_query = query;
                    self.launcher_selected = 0;
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
        // Resident overlay: the window stays alive for the whole session.
        let Some(h) = self.launcher.clone() else {
            return;
        };
        let theme = self.cfg.theme;
        let generation = self.launcher_gen;
        let shell = cx.entity().downgrade();
        cx.defer(move |cx| {
            // Drop stale closes: a newer open must not be hidden by us.
            let current = shell.upgrade().map(|d| d.read(cx).launcher_gen);
            if current != Some(generation) {
                return;
            }
            let _ = h.update(cx, |l, window, cx| {
                l.apply_view(LauncherView::closed(theme));
                l.closing = true;
                window.set_input_region(Some(&[]));
                window.set_keyboard_interactivity(
                    gpui::layer_shell::KeyboardInteractivity::None,
                );
                cx.notify();
            });
        });
    }

    fn launcher_key(&mut self, key: &str, _ch: Option<&str>, cx: &mut Context<Self>) {
        let key = key.to_ascii_lowercase();
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
        if let launcher::RowKind::App { .. } = &kind {
            let name = row.label.clone();
            self.recents.retain(|n| *n != name);
            self.recents.insert(0, name);
            self.recents.truncate(20);
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

    fn apply_ipc(&mut self, req: ClientRequest, cx: &mut Context<Self>) {
        match req {
            ClientRequest::ToggleLauncher => {
                self.set_launcher_open(!self.launcher_open, cx);
            }
            ClientRequest::OpenLauncher => self.set_launcher_open(true, cx),
            ClientRequest::CloseLauncher => self.set_launcher_open(false, cx),
            _ => {}
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
                d.apply_niri(msgs);
                if d.launcher_open {
                    d.sync_launcher(cx);
                }
                cx.notify();
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
                d.apply_ipc(req, cx);
                cx.notify();
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
                    }
                }
                cx.notify();
            });
        }
    })
    .detach();
}
