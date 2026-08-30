use crate::config::Config;
use crate::desktop::{DesktopApp, scan_applications};
use crate::files::{FileHit, Files, FilesOptions};
use crate::ui::launcher::{self, Launcher, LauncherCmd, LauncherView};

use awari_compositor::{
    Compositor, CompositorCommand, CompositorInbox, CompositorMsg, spawn_detached,
};
use awari_ipc::{ClientRequest, notify};
use gpui::layer_shell::LayerShellNotSupportedError;
use gpui::{
    App, AppContext, Bounds, ClipboardItem, Context, DisplayId, Entity, Global, QuitMode, Task,
    WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind, WindowOptions, point, px,
    size,
};
use std::collections::HashMap;
use std::fs;
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::thread;
use std::time::Duration;

const LAUNCHER_CLOSE_GRACE_MS: u64 = 200;

/// Whether the GPU overlay process stays in memory between dismisses
/// (`KeepAlive`) or exits on dismiss to free the GPU process (`Drop`).
pub enum GpuMode {
    KeepAlive,
    Drop,
}

/// Whether the launcher opens immediately (`Open`) or starts hidden and waits
/// for the first toggle (`Hidden`).
pub enum StartState {
    Open,
    Hidden,
}

pub struct Daemon {
    /// Compositor backend (wlr-foreign-toplevel). `None` when the compositor
    /// doesn't advertise foreign-toplevel; apps/files/commands still work.
    compositor: Option<Arc<dyn Compositor>>,
    launcher: Option<WindowHandle<Launcher>>,
    pending_close: Option<Task<()>>,
    /// When true the GUI stays in memory (hidden) between dismisses for instant
    /// re-opens; when false it quits on dismiss to free the GPU process.
    keep_alive: bool,
    quit_after_close: bool,
    launcher_display: Option<DisplayId>,
    launcher_open: bool,
    launcher_query: String,
    launcher_selected: usize,
    launcher_category: launcher::Category,
    /// Bumped on every open/close; deferred window updates from a previous
    /// generation are dropped so a stale hide cannot clobber a fresh open.
    launcher_gen: u64,
    apps: Vec<DesktopApp>,
    app_icons: HashMap<String, String>,
    cfg: Config,
    /// Desktop names by last activation, most recent first.
    recents: Vec<String>,
    query_history: Vec<String>,
    history_cursor: Option<usize>,
    history_live: Option<String>,
    app_usage: HashMap<String, u64>,
    files_tx: Option<crate::files::Files>,
    files_seq: u64,
    file_hits_gen: u64,
    file_hits: Vec<FileHit>,
    windows_list: Vec<launcher::WindowEntry>,
    /// Bumped whenever the app or window list changes, so cached launcher
    /// rows can be invalidated without re-filtering on every highlight move.
    source_gen: u64,
    /// Last computed rows, reused across highlight moves (Select / arrows)
    /// and other non-query changes to avoid re-scoring on every keystroke.
    last_rows: Option<Arc<[launcher::LauncherRow]>>,
    last_rows_key: Option<RowsKey>,
    /// Pre-scored app/window rows, keyed by `(query, source_gen, category)`.
    /// Reused on the re-render that fires when file results arrive so the
    /// expensive `matchq` scoring + sort doesn't run twice per keystroke.
    appwin_cache: Option<AppWinCache>,
    /// Panel position offset from default center-top position.
    panel_offset_x: f32,
    panel_offset_y: f32,
}

/// Holds the daemon entity so GPUI does not drop it with no windows open.
struct Keep(#[allow(dead_code)] Entity<Daemon>);
impl Global for Keep {}

/// Cache key for `last_rows`: a row set is reusable when none of these inputs
/// (which affect ranking) have changed, so a highlight move doesn't re-score.
struct RowsKey {
    query: String,
    category: launcher::Category,
    files_seq: u64,
    file_hits_gen: u64,
    source_gen: u64,
}

/// Pre-scored app/window rows, reused on the re-render that fires when file
/// results arrive so `matchq` scoring + sort doesn't run twice per keystroke.
struct AppWinCache {
    query: String,
    category: launcher::Category,
    source_gen: u64,
    app_rows: Arc<[launcher::LauncherRow]>,
    win_rows: Arc<[launcher::LauncherRow]>,
}

impl Daemon {
    pub fn start(
        cx: &mut App,
        compositor: Option<Arc<dyn Compositor + 'static>>,
        inbox: Arc<CompositorInbox>,
        cfg: Config,
        start_state: StartState,
        gpu_mode: GpuMode,
    ) {
        cx.set_quit_mode(QuitMode::Explicit);

        let daemon = cx.new(|cx| Self::new(cx, compositor, inbox, cfg));
        daemon.update(cx, |d, cx| {
            d.keep_alive = matches!(gpu_mode, GpuMode::KeepAlive);

            if matches!(start_state, StartState::Open) {
                d.set_launcher_open(true, cx);
            }
        });

        #[cfg(unix)]
        {
            let (tx, mut rx) = futures::channel::mpsc::unbounded::<Signal>();
            install_signal_handlers(tx);

            let entity = daemon.downgrade();
            cx.spawn(async move |cx| {
                use futures::StreamExt;

                while let Some(sig) = rx.next().await {
                    tracing::debug!(?sig, "signal received in gui");
                    if let Some(d) = entity.upgrade() {
                        d.update(cx, |d, cx| match sig {
                            Signal::Open => {
                                tracing::debug!(
                                    launcher_open = d.launcher_open,
                                    "handling Signal::Open"
                                );
                                if !d.launcher_open {
                                    d.set_launcher_open(true, cx);
                                }
                            }
                            Signal::Close => d.dismiss_launcher(cx),
                            Signal::Quit => {
                                d.quit_after_close = true;
                                d.dismiss_launcher(cx);
                            }
                        });
                    } else {
                        tracing::warn!("daemon entity dropped, ignoring signal");
                    }
                }
            })
            .detach();
        }
        cx.set_global(Keep(daemon));
    }

    fn new(
        cx: &mut Context<Self>,
        compositor: Option<Arc<dyn Compositor>>,
        inbox: Arc<CompositorInbox>,
        cfg: Config,
    ) -> Self {
        let (files_tx, files_rx) = if cfg.sources.files {
            let (tx, rx) = Files::spawn(
                cfg.files.resolved_roots(),
                FilesOptions {
                    index_lockfiles: cfg.files.index_lockfiles,
                    regex: cfg.files.regex,
                    fff: cfg.fff,
                },
            );
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };
        let (apps_tx, apps_rx) = std::sync::mpsc::channel::<Vec<DesktopApp>>();
        let mut daemon = Self {
            compositor,
            launcher: None,
            pending_close: None,
            keep_alive: true,
            quit_after_close: false,
            launcher_display: None,
            launcher_open: false,
            launcher_query: String::new(),
            launcher_selected: 0,
            launcher_category: launcher::Category::All,
            launcher_gen: 0,
            apps: Vec::new(),
            app_icons: HashMap::new(),
            cfg,
            recents: Vec::new(),
            query_history: Vec::new(),
            history_cursor: None,
            history_live: None,
            app_usage: HashMap::new(),
            files_tx,
            files_seq: 0,
            file_hits_gen: 0,
            file_hits: Vec::new(),
            windows_list: Vec::new(),
            source_gen: 0,
            last_rows: None,
            last_rows_key: None,
            appwin_cache: None,
            panel_offset_x: 0.0,
            panel_offset_y: 0.0,
        };

        spawn_compositor_pump(cx, inbox);
        if let Some(files_rx) = files_rx {
            spawn_files_pump(cx, files_rx);
        }

        spawn_apps_pump(cx, apps_rx);

        // Scan `.desktop` files off the bootstrap path so the overlay prewarm
        // (wgpu device, shaders, fonts) runs first; apps swap in when ready.
        thread::Builder::new()
            .name("awari-apps-scan".into())
            .spawn(move || {
                let _ = apps_tx.send(scan_applications());
            })
            .ok();

        daemon.load_history();
        daemon.load_usage();
        daemon.load_position();
        daemon
    }

    fn apply_compositor(&mut self, msgs: Vec<CompositorMsg>) -> bool {
        let changed = !msgs.is_empty();
        let mut windows_changed = false;
        for msg in msgs {
            match msg {
                CompositorMsg::Changed => windows_changed = true,
                CompositorMsg::Degraded(e) => tracing::warn!(%e, "compositor degraded"),
            }
        }

        // Only rebuild the window list when the backend reports a change (or on
        // the first batch, when it's still empty).
        if windows_changed || self.windows_list.is_empty() {
            self.refresh_windows();
        }
        changed
    }

    /// Rebuild the cached window list from the compositor backend. Cheap enough
    /// to run on a change batch, and far cheaper than rebuilding it on every
    /// keystroke inside `filtered_rows`.
    fn refresh_windows(&mut self) {
        let new = self.launcher_windows();
        if new != self.windows_list {
            self.windows_list = new;
            self.source_gen += 1;
        }
    }

    fn launcher_windows(&self) -> Vec<launcher::WindowEntry> {
        match &self.compositor {
            Some(c) => c
                .windows()
                .into_iter()
                .map(|t| {
                    let app_id_lc = t.app_id.as_deref().map(|s| s.to_lowercase());
                    launcher::WindowEntry {
                        id: t.id,
                        title: t.title.unwrap_or_else(|| format!("#{}", t.id)),
                        app_id: t.app_id,
                        app_id_lc,
                    }
                })
                .collect(),
            None => Vec::new(),
        }
    }

    fn filtered_rows(
        &self,
        cached_app: Option<&[launcher::LauncherRow]>,
        cached_win: Option<&[launcher::LauncherRow]>,
        prefix: Option<&str>,
        calc: Option<String>,
    ) -> Vec<launcher::LauncherRow> {
        let apps = self.apps.as_slice();
        let empty_windows: &[launcher::WindowEntry] = &[];
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
        launcher::filter_rows_cached(launcher::FilterParams {
            query: &self.launcher_query,
            apps,
            windows,
            files,
            recents: &self.recents,
            app_usage: &self.app_usage,
            app_icons: &self.app_icons,
            category: self.launcher_category,
            file_max: self.cfg.files.max_results,
            total_max: self.cfg.max_results,
            cached_app_rows: cached_app,
            cached_win_rows: cached_win,
            prefix,
            calc,
        })
    }

    fn sync_launcher(&mut self, cx: &mut Context<Self>) {
        let Some(h) = self.launcher else {
            return;
        };

        let q = self.launcher_query.trim();
        let prefix = launcher::command_prefix(q);
        let calc = crate::math::evaluate(q);
        // Reuse the last rows when nothing that affects ranking changed, so a
        // highlight move (Select / arrow) doesn't re-score every app/window.
        let rows = if self.last_rows_key.as_ref().is_some_and(|k| {
            k.query == self.launcher_query
                && k.category == self.launcher_category
                && k.files_seq == self.files_seq
                && k.file_hits_gen == self.file_hits_gen
                && k.source_gen == self.source_gen
        }) {
            self.last_rows.clone().unwrap()
        } else {
            let use_cache = !q.is_empty()
                && prefix.is_none()
                && self.launcher_category != launcher::Category::Commands
                && calc.is_none();
            // Reuse pre-scored app/window rows when the query + source list are
            // unchanged. This skips the expensive `matchq` scoring on the
            // re-render that fires when file results arrive.
            let (cached_app, cached_win) = if use_cache {
                match &self.appwin_cache {
                    Some(c)
                        if c.query == q
                            && c.category == self.launcher_category
                            && c.source_gen == self.source_gen =>
                    {
                        (Some(c.app_rows.clone()), Some(c.win_rows.clone()))
                    }
                    _ => {
                        let (a, w) = launcher::score_app_window(
                            q,
                            &self.apps,
                            &self.windows_list,
                            &self.recents,
                            &self.app_usage,
                            &self.app_icons,
                            self.launcher_category,
                        );
                        let a_arc: Arc<[launcher::LauncherRow]> = Arc::from(a);
                        let w_arc: Arc<[launcher::LauncherRow]> = Arc::from(w);
                        self.appwin_cache = Some(AppWinCache {
                            query: q.to_string(),
                            category: self.launcher_category,
                            source_gen: self.source_gen,
                            app_rows: a_arc.clone(),
                            win_rows: w_arc.clone(),
                        });
                        (Some(a_arc), Some(w_arc))
                    }
                }
            } else {
                self.appwin_cache = None;
                (None, None)
            };
            let rows_vec = self.filtered_rows(
                cached_app.as_deref(),
                cached_win.as_deref(),
                prefix,
                calc.clone(),
            );
            let rows: Arc<[launcher::LauncherRow]> = Arc::from(rows_vec);
            self.last_rows = Some(rows.clone());
            self.last_rows_key = Some(RowsKey {
                query: self.launcher_query.clone(),
                category: self.launcher_category,
                files_seq: self.files_seq,
                file_hits_gen: self.file_hits_gen,
                source_gen: self.source_gen,
            });
            rows
        };

        let source_active = self.launcher_query.trim().is_empty()
            && self.launcher_category == launcher::Category::All;
        if !source_active && self.launcher_selected >= rows.len() {
            self.launcher_selected = rows.len().saturating_sub(1);
        }

        let view = LauncherView {
            open: self.launcher_open,
            query: self.launcher_query.clone(),
            selected: self.launcher_selected,
            rows,
            theme: self.cfg.theme.clone(),
            category: self.launcher_category,
            files_enabled: self.cfg.sources.files,
            windows_enabled: self.cfg.sources.windows,
            calc: calc.clone(),
            panel_offset_x: self.panel_offset_x,
            panel_offset_y: self.panel_offset_y,
            motion_ms: self.cfg.motion.duration_ms,
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
        let reduce_motion = self.cfg.motion.reduced;
        let bounds = Bounds {
            origin: point(px(0.), px(0.)),
            size: size(px(1920.), px(launcher::LAUNCHER_H)),
        };
        let result = cx.open_window(
            WindowOptions {
                titlebar: None,
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                app_id: Some("awari".into()),
                window_background: WindowBackgroundAppearance::Transparent,
                kind: WindowKind::LayerShell(launcher::layer_opts()),
                display_id: self.launcher_display,
                ..Default::default()
            },
            |window, cx| {
                cx.set_reduce_motion(reduce_motion);
                cx.new(|cx| Launcher::new(shell.clone(), theme.clone(), window, cx))
            },
        );

        // Compositors without `wlr-layer-shell` (e.g. GNOME/Mutter) can't host
        // the overlay; fall back to a regular window so apps/files/commands
        // still work. Window switching is unavailable there regardless.
        let result = match result {
            Ok(handle) => Ok(handle),
            Err(e) if e.downcast_ref::<LayerShellNotSupportedError>().is_some() => cx.open_window(
                WindowOptions {
                    titlebar: None,
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    app_id: Some("awari".into()),
                    window_background: WindowBackgroundAppearance::Transparent,
                    kind: WindowKind::Normal,
                    display_id: self.launcher_display,
                    ..Default::default()
                },
                |window, cx| {
                    cx.set_reduce_motion(reduce_motion);
                    cx.new(|cx| Launcher::new(shell.clone(), theme.clone(), window, cx))
                },
            ),
            Err(e) => Err(e),
        };
        match result {
            Ok(handle) => self.launcher = Some(handle),
            Err(e) => tracing::warn!(%e, "launcher overlay failed to open"),
        }
    }

    /// Pick the `DisplayId` the launcher should appear on: the monitor of the
    /// focused (activated) toplevel when we can determine it, otherwise the
    /// primary display, otherwise `None` (compositor's default).
    fn launcher_target_display(&self, cx: &App) -> Option<DisplayId> {
        let origin = match self.compositor.as_ref().and_then(|c| c.focused_output()) {
            Some(rect) => {
                let scale = rect.scale.max(1) as f32;
                (rect.x as f32 * scale, rect.y as f32 * scale)
            }
            None => return cx.primary_display().map(|d| d.id()),
        };
        let (fx, fy) = origin;
        let displays = cx.displays();
        if displays.is_empty() {
            return cx.primary_display().map(|d| d.id());
        }
        let best = displays
            .iter()
            .min_by_key(|d| {
                let o = d.bounds().origin;
                let dx = o.x.as_f32() - fx;
                let dy = o.y.as_f32() - fy;
                (dx * dx + dy * dy) as i64
            })
            .expect("displays non-empty");
        Some(best.id())
    }

    fn ensure_launcher_display(&mut self, cx: &mut Context<Self>) {
        let desired = self.launcher_target_display(cx);
        if self.launcher.is_some() && self.launcher_display == desired {
            return;
        }
        if let Some(handle) = self.launcher.take() {
            let _ = handle.update(cx, |_, window, _| window.remove_window());
        }
        self.launcher_display = desired;
        self.ensure_launcher(cx);
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
                    self.refresh_file_hits();
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
                    self.history_cursor = None;
                    self.history_live = None;
                    self.refresh_file_hits();
                    self.sync_launcher(cx);
                }
            }
            LauncherCmd::SavePosition { x, y } => {
                self.panel_offset_x = x;
                self.panel_offset_y = y;
                self.save_position();
            }
        }
    }

    fn set_launcher_open(&mut self, open: bool, cx: &mut Context<Self>) {
        tracing::debug!(
            open,
            launcher_open = self.launcher_open,
            "set_launcher_open"
        );

        // Bumped on every transition; deferred window work from a previous
        // generation is dropped so a stale hide cannot clobber a fresh open
        // (destroy-on-close masked this by killing the handle instead).
        self.launcher_gen += 1;
        if open {
            self.launcher_open = true;

            notify(ClientRequest::LauncherShown);

            // Cancel any in-flight teardown so a reopen during the fade reuses
            // the live surface instead of removing it out from under us.
            self.pending_close = None;
            self.launcher_query.clear();
            self.launcher_selected = 0;
            self.launcher_category = launcher::Category::All;
            self.file_hits.clear();
            self.file_hits_gen += 1;

            if let Some(ft) = &mut self.files_tx {
                self.files_seq = ft.invalidate();
            }

            self.ensure_launcher_display(cx);
            if let Some(h) = self.launcher {
                let generation = self.launcher_gen;
                let shell = cx.entity().downgrade();
                cx.defer(move |cx| {
                    let current = shell.upgrade().map(|d| d.read(cx).launcher_gen);
                    if current != Some(generation) {
                        return;
                    }

                    let _ = h.update(cx, |l, window, _| {
                        l.closing = false;

                        window.clear_sprite_atlas();
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
        let cfg_motion_ms = self.cfg.motion.duration_ms as u64;
        self.save_position();
        self.launcher_open = false;

        notify(ClientRequest::LauncherHidden);

        // Return file-search RAM to a near-baseline "sleeping" footprint
        if let Some(ft) = &self.files_tx {
            ft.clear();
        }

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

        let Some(h) = self.launcher else {
            if !self.keep_alive || self.quit_after_close {
                cx.quit();
            }
            return;
        };
        let generation = self.launcher_gen;
        let shell = cx.entity().downgrade();

        cx.defer(move |cx| {
            let current = shell.upgrade().map(|d| d.read(cx).launcher_gen);
            if current != Some(generation) {
                return;
            }

            let _ = h.update(cx, |l, window, cx| {
                l.begin_close(cx);
                l.clear_icon_cache(cx);
                l.clear_text_cache(cx);
                window.set_input_region(Some(&[]));
                window.set_keyboard_interactivity(gpui::layer_shell::KeyboardInteractivity::None);
                cx.notify();
            });
        });

        let close_task = cx.spawn(async move |this, cx| {
            let grace_ms = if cfg_motion_ms == 0 {
                50
            } else {
                LAUNCHER_CLOSE_GRACE_MS.max(cfg_motion_ms)
            };

            cx.background_executor()
                .timer(Duration::from_millis(grace_ms))
                .await;

            if let Some(daemon) = this.upgrade() {
                daemon.update(cx, |d, cx| {
                    if d.launcher_open {
                        return;
                    }
                    if !d.keep_alive || d.quit_after_close {
                        cx.quit();
                    } else if let Some(h) = d.launcher {
                        let _ = h.update(cx, |l, window, cx| {
                            l.closing = false;
                            window.clear_sprite_atlas();

                            // Paint one final empty/transparent frame so the
                            // compositor stops showing the last launcher frame
                            // (the ghost left after an accept), then drop the
                            // GPU surface only after that frame is presented.
                            //
                            // on_next_frame closures run at the START of the
                            // frame they book, before draw(), so a single hop
                            // would release the surface first and the empty
                            // frame's draw would be absorbed as an idle redraw.
                            // Two hops present the transparent frame (notify +
                            // frame N draw), then release on frame N+1.
                            cx.notify();
                            window.on_next_frame(|window, _app| {
                                window.on_next_frame(|window, _app| {
                                    window.release_gpu_for_idle();
                                });
                            });
                        });

                        // Idle memory reclaim: drop the launcher's candidate
                        // caches and release the hit-buffer capacity so each
                        // session starts from a clean slate instead of carrying
                        // the previous session's peak. They rebuild on the next
                        // open (an empty-query browse re-filters, which is cheap).
                        d.last_rows = None;
                        d.appwin_cache = None;
                        d.file_hits.clear();
                        d.file_hits.shrink_to_fit();

                        unsafe { libc::malloc_trim(0) };
                    }
                });
            }
        });
        self.pending_close = Some(close_task);
    }

    fn launcher_key(&mut self, key: &str, _ch: Option<&str>, shift: bool, cx: &mut Context<Self>) {
        let key = key.to_ascii_lowercase();

        if shift && matches!(key.as_str(), "up" | "arrowup" | "down" | "arrowdown") {
            self.history_step(key == "down" || key == "arrowdown", cx);
            return;
        }

        let source_active = self.launcher_query.trim().is_empty()
            && self.launcher_category == launcher::Category::All;
        match key.as_str() {
            "escape" | "esc" => self.dismiss_launcher(cx),
            "enter" | "return" => {
                if source_active {
                    let category = match self.launcher_selected {
                        1 => launcher::Category::Files,
                        2 => launcher::Category::Windows,
                        _ => launcher::Category::Apps,
                    };
                    self.launcher_category = category;
                    self.launcher_selected = 0;
                    self.sync_launcher(cx);
                } else {
                    self.activate_launcher_row(cx);
                }
            }
            "up" | "arrowup" => {
                if source_active {
                    self.launcher_selected = self.launcher_selected.saturating_sub(1).min(2);
                } else {
                    self.launcher_selected = self.launcher_selected.saturating_sub(1);
                }
                self.sync_launcher(cx);
            }
            "down" | "arrowdown" => {
                if source_active {
                    self.launcher_selected = (self.launcher_selected + 1).min(2);
                } else {
                    self.launcher_selected = self.launcher_selected.saturating_add(1);
                }
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

    fn read_state(name: &str) -> Option<String> {
        let primary = awari_ipc::state_dir().join(name);
        fs::read_to_string(&primary)
            .ok()
            .or_else(|| fs::read_to_string(awari_ipc::runtime_dir().join(name)).ok())
    }

    fn write_state(name: &str, body: &str) {
        let dir = awari_ipc::state_dir();
        let _ = fs::create_dir_all(&dir);
        let _ = fs::write(dir.join(name), body);
    }

    /// Load past queries from `$XDG_STATE_HOME/awari/history`.
    fn load_history(&mut self) {
        if let Some(s) = Self::read_state("history") {
            self.query_history = s
                .lines()
                .map(str::to_string)
                .filter(|l| !l.trim().is_empty())
                .collect();
        }
    }

    fn save_history(&self) {
        Self::write_state("history", &self.query_history.join("\n"));
    }

    fn load_usage(&mut self) {
        if let Some(s) = Self::read_state("usage") {
            for line in s.lines() {
                if let Some((name, cnt)) = line.split_once('\t')
                    && let Ok(n) = cnt.parse::<u64>()
                {
                    self.app_usage.insert(name.to_string(), n);
                }
            }
        }
    }

    fn save_usage(&self) {
        let body: Vec<String> = self
            .app_usage
            .iter()
            .map(|(k, v)| format!("{}\t{}", k, v))
            .collect();
        Self::write_state("usage", &body.join("\n"));
    }

    fn load_position(&mut self) {
        if let Some(s) = Self::read_state("position") {
            let parts: Vec<&str> = s.split_whitespace().collect();
            if parts.len() == 2
                && let (Ok(x), Ok(y)) = (parts[0].parse::<f32>(), parts[1].parse::<f32>())
            {
                self.panel_offset_x = x;
                self.panel_offset_y = y;
            }
        }
    }

    fn save_position(&self) {
        Self::write_state(
            "position",
            &format!("{} {}", self.panel_offset_x, self.panel_offset_y),
        );
    }

    fn refresh_file_hits(&mut self) {
        self.file_hits.clear();
        self.file_hits_gen += 1;
        if let Some(ft) = &mut self.files_tx {
            if !self.launcher_query.trim().is_empty() {
                self.files_seq = ft.query(&self.launcher_query);
            } else if self.launcher_category == launcher::Category::Files {
                self.files_seq = ft.query("");
            } else {
                self.files_seq = ft.invalidate();
            }
        }
    }

    fn activate_launcher_row(&mut self, cx: &mut Context<Self>) {
        let q = self.launcher_query.trim();

        if let Some(result) = crate::math::evaluate(q) {
            cx.write_to_clipboard(ClipboardItem::new_string(result));
            self.dismiss_launcher(cx);
            return;
        }
        let prefix = launcher::command_prefix(q);
        let rows = self.filtered_rows(None, None, prefix, None);
        let Some(row) = rows.get(self.launcher_selected) else {
            return;
        };
        let kind = row.kind.clone();

        let default = kind
            .actions()
            .into_iter()
            .next()
            .unwrap_or(launcher::RowAction::Open);
        self.run_row_action(kind, default, cx);
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
            launcher::RowAction::Open | launcher::RowAction::Run => self.activate_kind(kind, cx),
            launcher::RowAction::ShowInFolder => {
                if let launcher::RowKind::File { path } = &kind {
                    crate::files::reveal(path);
                }
                self.dismiss_launcher(cx);
            }
            launcher::RowAction::CopyPath => {
                let text = match &kind {
                    launcher::RowKind::File { path } => path.display().to_string(),
                    launcher::RowKind::App { exec, .. } => exec.join(" "),
                    launcher::RowKind::Window { .. } => String::new(),
                    launcher::RowKind::Command { command } => command.clone(),
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
        // Record recents + usage from the app name carried on the kind, so we
        // don't have to re-score every row here (which also races async
        // updates that may have changed the selection since the menu opened).
        if let launcher::RowKind::App { name, .. } = &kind {
            self.recents.retain(|n| n.as_str() != name.as_ref());
            self.recents.insert(0, name.to_string());
            self.recents.truncate(20);
            *self.app_usage.entry(name.to_string()).or_insert(0) += 1;
            self.save_usage();
        }

        if let launcher::RowKind::Command { command } = &kind {
            crate::files::run_script(&format!("{command} ; exec \"$SHELL\""));
            self.dismiss_launcher(cx);

            return;
        }

        self.dismiss_launcher(cx);

        if let launcher::RowKind::File { path } = kind {
            if let Some(files) = &self.files_tx {
                files.record_open(&path);
            }

            crate::files::activate(&path);
            return;
        }

        // App spawns inline: `spawn_detached` is a detached fork (~ms) that
        // never touches this surface or the shell state, so deferring it only
        // adds a composite cycle to Enter -> launch.
        //
        // Window focus MUST stay deferred (behind `dismiss_launcher`'s own
        // deferred teardown above): the overlay's Exclusive keyboard grab is
        // only released by that deferred close (set_input_region([]) +
        // set_keyboard_interactivity(None), registered before this point).
        // FocusWindow running in the same synchronous frame would hit a
        // surface still holding the seat — a wlroots-style compositor keeps
        // routing keys to the overlay and the focused window never receives
        // them (or the focus request is ignored). FIFO defer order guarantees
        // the grab frees first, as the original behavior did.
        match kind {
            launcher::RowKind::App { exec, .. } => {
                if let Err(e) = spawn_detached(&exec) {
                    tracing::warn!(%e, "failed to launch app");
                }
            }
            launcher::RowKind::Window { id } => {
                let compositor = self.compositor.clone();
                cx.defer(move |_cx| {
                    let Some(compositor) = compositor else {
                        return;
                    };
                    if let Err(e) = compositor.apply(CompositorCommand::FocusWindow { id }) {
                        tracing::warn!(%e, "failed to focus window");
                    }
                });
            }
            launcher::RowKind::File { .. } => unreachable!("handled above"),
            launcher::RowKind::Command { .. } => {}
        }
    }
}

fn spawn_compositor_pump(cx: &mut Context<Daemon>, inbox: Arc<CompositorInbox>) {
    let (tx, mut rx) = futures::channel::mpsc::unbounded();
    let wake = inbox.take_wake();

    thread::Builder::new()
        .name("awari-compositor-pump".into())
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
                let changed = d.apply_compositor(msgs);

                if changed && d.launcher_open {
                    d.sync_launcher(cx);
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
                    d.file_hits_gen += 1;

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
                let mut icons: HashMap<String, String> = HashMap::new();
                for a in &apps {
                    if let Some(v) = a.icon.as_ref() {
                        let v = v.to_string();
                        if let Some(k) = a.app_id_lc.as_ref() {
                            icons.entry(k.clone()).or_insert(v.clone());
                        }
                        icons.entry(a.name_lc.clone()).or_insert(v);
                    }
                }

                d.app_icons = icons;
                d.apps = apps;
                d.source_gen += 1;

                if d.launcher_open {
                    d.sync_launcher(cx);
                }
            });
        }
    })
    .detach();
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug)]
enum Signal {
    Open,
    Close,
    Quit,
}

#[cfg(unix)]
static SIGNAL_WRITE_FD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

#[cfg(unix)]
extern "C" fn on_signal(sig: i32) {
    let token = match sig {
        libc::SIGUSR1 => b'O',
        libc::SIGUSR2 => b'C',
        _ => b'T',
    };
    let fd = SIGNAL_WRITE_FD.load(std::sync::atomic::Ordering::Relaxed);
    if fd >= 0 {
        unsafe {
            libc::write(fd, &token as *const u8 as *const libc::c_void, 1);
        }
    }
}

#[cfg(unix)]
/// Map UNIX signals onto launcher intents via a self-pipe: the handler only
/// writes a byte (it must not touch GPUI state), a reader thread forwards the
/// token, and the foreground task runs the matching path — `Open` shows a
/// hidden in-memory overlay, `Close` dismisses (hide when kept alive, quit when
/// dropped), `Quit` dismisses and forces a quit.
fn install_signal_handlers(tx: futures::channel::mpsc::UnboundedSender<Signal>) {
    use std::os::unix::io::FromRawFd;
    let mut fds = [0i32; 2];

    unsafe {
        if libc::pipe(fds.as_mut_ptr()) != 0 {
            return;
        }
    }

    let (read_fd, write_fd) = (fds[0], fds[1]);
    SIGNAL_WRITE_FD.store(write_fd, std::sync::atomic::Ordering::Relaxed);
    let _ = std::thread::Builder::new()
        .name("awari-sig".into())
        .spawn(move || {
            use std::io::Read;
            let mut reader = unsafe { std::fs::File::from_raw_fd(read_fd) };
            let mut buf = [0u8; 1];

            while reader.read(&mut buf).is_ok() {
                tracing::debug!(byte = buf[0], "self-pipe byte received");
                let sig = match buf[0] {
                    b'O' => Signal::Open,
                    b'C' => Signal::Close,
                    _ => Signal::Quit,
                };

                if tx.unbounded_send(sig).is_err() {
                    break;
                }
            }
        });

    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_signal as *const () as usize;
        sa.sa_flags = libc::SA_RESTART;
        libc::sigaction(libc::SIGUSR1, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGUSR2, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
        let mut mask: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut mask);
        libc::sigaddset(&mut mask, libc::SIGUSR1);
        libc::sigaddset(&mut mask, libc::SIGUSR2);
        libc::pthread_sigmask(libc::SIG_UNBLOCK, &mask, std::ptr::null_mut());
    }
}
