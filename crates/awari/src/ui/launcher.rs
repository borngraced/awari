//! Overlay finder: search, chips, list/grid. Clean, text-focused layout.

use gpui::prelude::*;
use gpui::{
    AnimationExt, AnyElement, App, Context, FocusHandle, Focusable, FontWeight, HighlightStyle,
    Image, ImageFormat, InteractiveElement, IntoElement, MouseButton, ObjectFit, ParentElement,
    Render, Rgba, ScrollStrategy, SpringAnimation, SpringConfig, Styled, StyledImage, StyledText,
    UniformListScrollHandle, WeakEntity, Window, div, img, px, uniform_list, Point, Pixels,
};
use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::app::Daemon;
use crate::desktop::DesktopApp;
use crate::files::FileHit;
use crate::surfaces::LAUNCHER_NAMESPACE;
use crate::ui::icon::Icon;
use crate::ui::theme::Theme;

pub const LAUNCHER_W: f32 = 600.0;
pub const LAUNCHER_H: f32 = 1080.0;
const PANEL_H: f32 = 560.0;
const GRID_COLS: usize = 4;
const SLIDE: f32 = 22.0;
const PANEL_SPRING: SpringConfig = SpringConfig::new(380.0, 30.0, 1.0);
const HEIGHT_SPRING: SpringConfig = SpringConfig::new(380.0, 30.0, 1.0);
const SEARCH_H: f32 = 68.0;
const ITEM_HOVER_SPRING: SpringConfig = SpringConfig::new(420.0, 34.0, 1.0);

fn mix(a: &Rgba, b: &Rgba, t: f32) -> Rgba {
    let t = t.clamp(0.0, 1.0);
    Rgba {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: a.a + (b.a - a.a) * t,
    }
}
const ICON_LIST: f32 = 30.0;
const ICON_GRID: f32 = 50.0;
const AWARI_MARK: &[u8] = include_bytes!("../../assets/icons/awari_mark.svg");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum Category {
    All,
    Apps,
    Files,
    Commands,
}

impl Category {
    fn icon(self) -> Icon {
        match self {
            Self::All => Icon::LayoutGrid,
            Self::Apps => Icon::AppWindow,
            Self::Files => Icon::File,
            Self::Commands => Icon::Command,
        }
    }

    fn all() -> [Category; 3] {
        [Self::Apps, Self::Files, Self::Commands]
    }
}

#[derive(Clone)]
pub enum LauncherCmd {
    Dismiss,
    Key {
        key: String,
        ch: Option<String>,
        shift: bool,
    },
    SetQuery {
        query: String,
    },
    Activate {
        index: usize,
    },
    Select {
        index: usize,
    },
    SetCategory {
        category: Category,
    },
    OpenToRender {
        ms: u64,
    },
}

#[derive(Clone)]
pub struct LauncherRow {
    pub kind: RowKind,
    pub label: String,
    pub resolved_icon: Option<PathBuf>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RowAction {
    Open,
    ShowInFolder,
    CopyPath,
    RunInTerminal,
    Run,
    CopyResult,
}

impl RowAction {
    pub fn label(&self) -> &'static str {
        match self {
            RowAction::Open => "Open",
            RowAction::ShowInFolder => "Show in Folder",
            RowAction::CopyPath => "Copy Path",
            RowAction::RunInTerminal => "Run in Terminal",
            RowAction::Run => "Run",
            RowAction::CopyResult => "Copy Result",
        }
    }
}

#[derive(Clone)]
pub enum RowKind {
    App {
        name: String,
        exec: Vec<String>,
    },
    Window {
        id: u64,
    },
    File {
        path: PathBuf,
    },
    /// A shell command to run in a terminal (from `>` command mode or the
    /// no-match fallback).
    Command {
        command: String,
    },
    /// A calculator result; `Enter` copies the value to the clipboard.
    Calc {
        result: String,
    },
}

impl RowKind {
    /// Actions available for this kind, in display order. Index 0 is the
    /// default action performed by `Enter`.
    pub fn actions(&self) -> Vec<RowAction> {
        match self {
            RowKind::File { .. } => vec![
                RowAction::Open,
                RowAction::ShowInFolder,
                RowAction::CopyPath,
                RowAction::RunInTerminal,
            ],
            RowKind::App { .. } => vec![RowAction::Open, RowAction::CopyPath],
            RowKind::Window { .. } => vec![RowAction::Open],
            RowKind::Command { .. } => vec![RowAction::Run, RowAction::CopyPath],
            RowKind::Calc { .. } => vec![RowAction::CopyResult],
        }
    }
}

#[derive(Clone)]
pub struct LauncherView {
    pub open: bool,
    pub query: String,
    pub selected: usize,
    pub rows: Vec<LauncherRow>,
    pub theme: Theme,
    pub category: Category,
    /// Whether the Files source is enabled; gates the Files category chip.
    pub files_enabled: bool,
}

fn action_menu_top(row_top: Pixels, item_h: Pixels, menu_h: Pixels, viewport: Pixels) -> Pixels {
    let gap = px(6.);
    if row_top + item_h + gap + menu_h <= viewport {
        row_top + item_h + gap
    } else if row_top - gap - menu_h >= px(0.) {
        row_top - gap - menu_h
    } else {
        px(0.)
    }
}

impl LauncherView {
    pub fn closed(theme: Theme) -> Self {
        Self {
            open: false,
            query: String::new(),
            selected: 0,
            rows: Vec::new(),
            theme,
            category: Category::All,
            files_enabled: true,
        }
    }
}

struct ActionMenu {
    actions: Vec<RowAction>,
    index: usize,
}

pub struct Launcher {
    pub shell: WeakEntity<Daemon>,
    view: LauncherView,
    cursor: usize,
    caret_on: bool,
    focus_handle: FocusHandle,
    scroll: UniformListScrollHandle,
    scrolled_to: Option<usize>,
    scroll_target: Option<Pixels>,
    scroll_anim_gen: u64,
    open_started: Option<Instant>,
    pub(crate) closing: bool,
    hovered: Option<usize>,
    hovered_chip: Option<Category>,
    /// Last row index we posted a `Select` for, so pointer-move spam over the
    /// same row doesn't round-trip through the daemon.
    last_select: Option<usize>,
    /// Whether we last set the Wayland input region for open vs closed; we only
    /// re-issue `set_input_region` on a transition, not every animation frame.
    last_input_open: Option<bool>,
    /// Whether keyboard focus is currently held; `focus_search` is only called
    /// once per open session instead of on every rendered frame.
    focused: bool,
    /// Caret-timer generation. Bumped on close/reopen so a tick parked in the
    /// background executor can tell it is stale and exit: the closed daemon
    /// must have no periodic wakeups at all.
    blink_gen: Arc<AtomicU64>,
    blink_running: bool,
    /// Open action menu for the selected row. `Alt+Enter` opens it, arrows
    /// move the highlight, `Enter` runs the highlighted action, `Esc` closes.
    action_menu: Option<ActionMenu>,
    /// Accepted inline completion: the query it applies to plus the byte
    /// offset where the completed suffix starts. Rendered accent while the
    /// cursor sits at the end and the query is unchanged; any edit or query
    /// swap clears it.
    accepted: Option<(String, usize)>,
}

impl Launcher {
    pub fn new(
        shell: WeakEntity<Daemon>,
        theme: Theme,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // No caret timer here: it is spawned on open (ensure_blink) and torn
        // down on close so the idle daemon never wakes periodically.
        Self {
            shell,
            view: LauncherView::closed(theme),
            cursor: 0,
            caret_on: true,
            focus_handle: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            scrolled_to: None,
            scroll_target: None,
            scroll_anim_gen: 0,
            open_started: None,
            closing: false,
            hovered: None,
            hovered_chip: None,
            last_select: None,
            last_input_open: None,
            focused: false,
            blink_gen: Arc::new(AtomicU64::new(0)),
            blink_running: false,
            action_menu: None,
            accepted: None,
        }
    }

    pub fn apply_view(&mut self, view: LauncherView, cx: &mut Context<Self>) {
        if self.view.query != view.query || self.view.category != view.category {
            self.scrolled_to = None;
            self.scroll_target = None;
            self.scroll_anim_gen = self.scroll_anim_gen.wrapping_add(1);
        }
        if self.view.query != view.query || self.view.selected != view.selected {
            self.action_menu = None;
        }
        if self.view.query != view.query {
            self.accepted = None;
        }
        self.view = view;
        self.last_select = Some(self.view.selected);
        if self.view.open {
            self.closing = false;
            self.ensure_blink(cx);
        } else {
            self.cursor = 0;
            self.caret_on = true;
            self.stop_blink();
        }
    }

    /// Run the caret timer only while the overlay is open. The task parks in
    /// a 530ms timer; a generation counter invalidates any tick that fires
    /// after close or reopen, so exactly one ticker exists while open and
    /// zero while closed.
    fn ensure_blink(&mut self, cx: &mut Context<Self>) {
        if self.blink_running {
            return;
        }
        self.blink_running = true;
        let gen_id = self.blink_gen.fetch_add(1, Ordering::Relaxed) + 1;
        let flag = self.blink_gen.clone();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(530))
                    .await;
                if flag.load(Ordering::Relaxed) != gen_id {
                    break;
                }
                let Ok(()) = this.update(cx, |this, cx| {
                    if this.view.open && flag.load(Ordering::Relaxed) == gen_id {
                        this.caret_on = !this.caret_on;
                        cx.notify();
                    } else {
                        this.stop_blink();
                    }
                }) else {
                    break;
                };
            }
        })
        .detach();
    }

    fn stop_blink(&mut self) {
        self.blink_running = false;
        self.blink_gen.fetch_add(1, Ordering::Relaxed);
    }

    pub fn arm_open_timer(&mut self, started: Instant) {
        self.open_started = Some(started);
    }

    /// Apply a keystroke to the query/cursor; returns the new query when it changed.
    fn edit(&mut self, k: &gpui::Keystroke) -> Option<String> {
        let key = k.key.to_ascii_lowercase();
        self.accepted = None;
        let mut q = self.view.query.clone();
        let mut c = self.cursor.min(q.len());
        match key.as_str() {
            "backspace" => {
                if c == 0 {
                    return None;
                }
                let prev = q[..c]
                    .char_indices()
                    .next_back()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                q.replace_range(prev..c, "");
                c = prev;
            }
            "delete" => {
                if c >= q.len() {
                    return None;
                }
                let next = q[c..]
                    .char_indices()
                    .nth(1)
                    .map(|(i, _)| c + i)
                    .unwrap_or(q.len());
                q.replace_range(c..next, "");
            }
            "arrowleft" | "left" => {
                c = q[..c]
                    .char_indices()
                    .next_back()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
            }
            "arrowright" | "right" => {
                c = q[c..]
                    .char_indices()
                    .nth(1)
                    .map(|(i, _)| c + i)
                    .unwrap_or(q.len());
            }
            "home" => c = 0,
            "end" => c = q.len(),
            _ => {
                let Some(ch) = &k.key_char else { return None };
                if ch.is_empty() || ch.chars().any(|b| b.is_control()) {
                    return None;
                }
                q.insert_str(c, ch);
                c += ch.len();
            }
        }
        self.view.query = q.clone();
        self.cursor = c;
        Some(q)
    }

    fn query_element(&self) -> AnyElement {
        let t = self.view.theme.clone();
        let q = &self.view.query;
        let c = self.cursor.min(q.len());
        let caret = div()
            .id("caret")
            .w(px(2.))
            .h(px(20.))
            .rounded(px(1.))
            .bg(t.accent())
            .when(!self.caret_on, |el| el.opacity(0.0));
        if q.is_empty() {
            return div()
                .flex()
                .flex_nowrap()
                .items_center()
                .flex_none()
                .child(caret)
                .child(
                    div()
                        .text_color(t.muted())
                        .child("Search apps, files, and commands"),
                )
                .into_any_element();
        }
        let token_len = command_token_len(q);
        let (prefix, suffix) = q.split_at(c);
        let p_split = prefix.len().min(token_len);
        let (p_accent, p_norm) = prefix.split_at(p_split);
        // Accepted-completion highlight wins over command-mode coloring only
        // when they can't both apply (command mode never ghosts).
        let accepted_off = self
            .accepted
            .as_ref()
            .filter(|(aq, _)| aq == q && c == q.len() && token_len == 0)
            .map(|(_, off)| *off);
        let s_accent_len = if token_len > 0 {
            token_len.saturating_sub(c)
        } else if let Some(off) = accepted_off {
            (c - off).min(suffix.len())
        } else {
            0
        };
        let (s_accent, s_norm) = suffix.split_at(s_accent_len);
        // Ghost preview hides once its own accept is on screen. Calculator
        // results are shown verbatim, so they must not spawn a ghost suffix.
        let ghost = if token_len == 0 && c == q.len() && accepted_off.is_none() {
            self.view.rows.first().and_then(|r| {
                if matches!(r.kind, RowKind::Calc { .. }) {
                    None
                } else {
                    ghost_suffix(q, &r.label)
                }
            })
        } else {
            None
        };
        div()
            .flex()
            .flex_nowrap()
            .items_center()
            .flex_none()
            .when(!p_accent.is_empty(), |el| {
                el.child(div().text_color(t.accent()).child(p_accent.to_string()))
            })
            .when(!p_norm.is_empty(), |el| {
                el.child(div().child(p_norm.to_string()))
            })
            .child(caret)
            .when(!s_accent.is_empty(), |el| {
                el.child(div().text_color(t.accent()).child(s_accent.to_string()))
            })
            .when(!s_norm.is_empty(), |el| {
                el.child(div().child(s_norm.to_string()))
            })
            .when_some(ghost, |el, g| {
                el.child(div().text_color(t.faint()).child(g))
            })
            .into_any_element()
    }
}

impl Focusable for Launcher {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

fn post(this: &Launcher, cx: &mut App, cmd: LauncherCmd) {
    let shell = this.shell.clone();
    cx.defer(move |cx| {
        if let Some(s) = shell.upgrade() {
            s.update(cx, |s, cx| s.apply_launcher_cmd(cmd, cx));
        }
    });
}

pub fn layer_opts() -> gpui::layer_shell::LayerShellOptions {
    use gpui::layer_shell::{Anchor, KeyboardInteractivity, Layer, LayerShellOptions};
    LayerShellOptions {
        namespace: LAUNCHER_NAMESPACE.into(),
        layer: Layer::Overlay,
        anchor: Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
        exclusive_zone: Some(px(0.)),
        exclusive_edge: None,
        margin: None,
        keyboard_interactivity: KeyboardInteractivity::None,
    }
}

fn row_category(r: &LauncherRow) -> Category {
    match r.kind {
        RowKind::App { .. } => Category::Apps,
        RowKind::File { .. } => Category::Files,
        // Windows have no dedicated tab; treat them as neutral (no highlight).
        RowKind::Window { .. } => Category::All,
        RowKind::Command { .. } => Category::Commands,
        RowKind::Calc { .. } => Category::All,
    }
}

fn push_capped(
    out: &mut Vec<LauncherRow>,
    cap: Option<usize>,
    rows: impl IntoIterator<Item = LauncherRow>,
) {
    for r in rows {
        if cap.is_some_and(|c| out.len() >= c) {
            return;
        }
        out.push(r);
    }
}

/// Detect a command prefix at the start of `q`. `r:` switches file search to
/// regex mode (handled in the files source), while `>` and `o:` are inline
/// command modes that replace the result list here. Returned as the literal
/// token so callers color it and branch on it from one source of truth.
pub fn command_prefix(q: &str) -> Option<&'static str> {
    if q.starts_with("r:") {
        Some("r:")
    } else if q.starts_with("o:") {
        Some("o:")
    } else if q.starts_with('>') {
        Some(">")
    } else {
        None
    }
}

fn command_token_len(q: &str) -> usize {
    command_prefix(q).map_or(0, |p| p.len())
}

/// Inline autocomplete candidate: the untyped remainder of `top_label` when
/// the live query is a case-insensitive prefix of it. `None` when there is
/// nothing to complete (empty query, no prefix match, or label == query).
fn ghost_suffix(query: &str, top_label: &str) -> Option<String> {
    if query.is_empty() {
        return None;
    }
    let mut qi = query.chars();
    let mut matched = 0usize;
    for (off, lc) in top_label.char_indices() {
        match qi.next() {
            None => {
                let rest = &top_label[matched..];
                return if rest.is_empty() { None } else { Some(rest.to_string()) };
            }
            Some(qc) => {
                let hit = qc == lc
                    || (qc.is_ascii() && lc.is_ascii() && qc.eq_ignore_ascii_case(&lc))
                    || qc.to_lowercase().eq(lc.to_lowercase());
                if !hit {
                    return None;
                }
                matched = off + lc.len_utf8();
            }
        }
    }
    None
}

/// What a Tab keypress should do, decided purely from view state.
#[derive(Debug)]
enum TabOutcome {
    /// Inline ghost accept: `completed` is the full query, `accepted_off` the
    /// byte offset where the accent-highlighted suffix starts.
    Inline {
        completed: String,
        accepted_off: usize,
    },
    /// Legacy row completion (selected row's path / label / command / result).
    Row(String),
}

fn tab_completion(query: &str, rows: &[LauncherRow], selected: usize) -> Option<TabOutcome> {
    if let Some(r) = rows.first() {
        // Calc results never ghost (the result text is not a completion).
        if !query.is_empty()
            && command_prefix(query).is_none()
            && !matches!(r.kind, RowKind::Calc { .. })
        {
            if ghost_suffix(query, &r.label).is_some() {
                return Some(TabOutcome::Inline {
                    accepted_off: query.len(),
                    completed: r.label.clone(),
                });
            }
        }
    }
    rows.get(selected).and_then(|row| {
        let completion = match &row.kind {
            RowKind::File { path } => path.display().to_string(),
            RowKind::App { .. } | RowKind::Window { .. } => row.label.clone(),
            RowKind::Command { command } => command.clone(),
            RowKind::Calc { result } => result.clone(),
        };
        (!completion.is_empty()).then_some(TabOutcome::Row(completion))
    })
}

fn expand_open_path(raw: &str) -> Option<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if let Some(rest) = trimmed.strip_prefix('~') {
        let home = home?;
        if rest.is_empty() || rest.starts_with('/') {
            Some(home.join(rest.trim_start_matches('/')))
        } else {
            Some(home.join(rest))
        }
    } else if trimmed.starts_with('/') {
        Some(PathBuf::from(trimmed))
    } else {
        home.map(|h| h.join(trimmed))
    }
}

/// Build the result list for an `o:` (open path) query. Lists real entries
/// under the typed path via `read_dir` (so it works outside the configured
/// fff roots), and shows a direct "Open <path>" row only when that exact path
/// exists. Never returns an optimistic row for a nonexistent path.
fn open_path_rows(arg: &str, file_max: usize) -> Vec<LauncherRow> {
    let Some(base) = expand_open_path(arg) else {
        return Vec::new();
    };
    let mut rows: Vec<LauncherRow> = Vec::new();
    // Direct "Open <path>" row, only when the typed path already exists.
    if base.exists() {
        rows.push(open_file_row(&base, true));
    }
    // Real results: entries under the parent dir matching the last segment.
    // When the typed path is itself an existing directory, list its contents
    // (empty fragment) rather than searching the dir for its own name.
    let parent = base
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| base.clone());
    let frag = if base.is_dir() {
        String::new()
    } else {
        base.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    let search_dir = if base.is_dir() { &base } else { &parent };
    if let Some(entries) = read_dir_matching(search_dir, &frag) {
        for p in entries {
            if rows.len() >= file_max {
                break;
            }
            if p == base {
                continue; // already shown as the direct row
            }
            rows.push(open_file_row(&p, false));
        }
    }
    rows
}

fn open_file_row(p: &Path, is_direct: bool) -> LauncherRow {
    let label = if is_direct {
        format!("Open “{}”", p.display())
    } else {
        p.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| p.display().to_string())
    };
    LauncherRow {
        kind: RowKind::File {
            path: p.to_path_buf(),
        },
        label,
        resolved_icon: None,
    }
}

/// List entries of `dir`, filtered by `frag` (subsequence match, case-insensitive)
/// and sorted best-match first. Returns `None` if `dir` isn't a readable directory.
fn read_dir_matching(dir: &Path, frag: &str) -> Option<Vec<PathBuf>> {
    let rd = std::fs::read_dir(dir).ok()?;
    let frag_lc = frag.to_lowercase();
    let mut scored: Vec<(i32, PathBuf)> = Vec::new();
    for entry in rd.flatten() {
        let p = entry.path();
        let name = match p.file_name() {
            Some(n) => n.to_string_lossy().into_owned(),
            None => continue,
        };
        let name_lc = name.to_lowercase();
        let score = if frag_lc.is_empty() {
            Some(0)
        } else {
            crate::files::subsequence_score(&name_lc, &frag_lc)
        };
        if let Some(s) = score {
            scored.push((s, p));
        }
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    Some(scored.into_iter().map(|(_, p)| p).collect())
}

/// Score and rank the app and window rows for `query`. This is the expensive
/// part of filtering (`matchq` over every app/window plus a sort); it depends
/// only on the query, the app/window lists, recents, and usage — never on file
/// hits. The Daemon caches the result by `(query, source_gen, category)` so the
/// re-render that fires when file search returns can reuse it instead of
/// re-scoring the whole list.
pub fn score_app_window(
    query: &str,
    apps: &[DesktopApp],
    windows: &[(u64, String, Option<String>, Option<String>)],
    recents: &[String],
    app_usage: &HashMap<String, u64>,
    app_icons: &HashMap<String, String>,
    category: Category,
) -> (Vec<LauncherRow>, Vec<LauncherRow>) {
    let q = query.trim();
    let empty = q.is_empty();
    let apps_only = category == Category::Apps;
    let files_only = category == Category::Files;

    let mut win_scored: Vec<(i64, usize)> = if files_only || apps_only {
        Vec::new()
    } else {
        windows
            .iter()
            .enumerate()
            .filter_map(|(ix, (_, title, app_id, _))| {
                let s = if empty {
                    1
                } else {
                    crate::matchq::score(title, q)
                        .max(app_id.as_deref().and_then(|a| crate::matchq::score(a, q)))?
                };
                Some((s, ix))
            })
            .collect()
    };
    if !empty {
        win_scored.sort_by(|a, b| b.0.cmp(&a.0));
    }

    let visible_app_ids: Vec<&str> = win_scored
        .iter()
        .filter_map(|&(_, ix)| windows[ix].3.as_deref())
        .collect();

    let mut app_scored: Vec<(i64, &DesktopApp)> = if files_only {
        Vec::new()
    } else {
        apps.iter()
            .filter_map(|app| {
                if !apps_only {
                    let ident_hits_window =
                        |probe: &str| visible_app_ids.iter().any(|v| *v == probe);
                    if ident_hits_window(&app.name_lc)
                        || ident_hits_window(app.app_id_lc.as_deref().unwrap_or(""))
                    {
                        return None;
                    }
                }
                let s = if empty {
                    1
                } else {
                    let by_name = crate::matchq::score(&app.name, q);
                    let by_id = app
                        .app_id
                        .as_deref()
                        .and_then(|a| crate::matchq::score(a, q));
                    let base = match (by_name, by_id) {
                        o @ (Some(_), Some(_)) => o.0,
                        (a, b) => a.or(b),
                    }?;
                    // Boost repeatedly-launched apps so muscle-memory picks
                    // stay near the top without overriding a strong match.
                    let usage = app_usage.get(&app.name).copied().unwrap_or(0);
                    base + (usage.saturating_sub(1) as i64) * 5
                };
                Some((s, app))
            })
            .collect()
    };
    if empty {
        // Precompute recent positions once instead of scanning `recents`
        // per comparator call.
        let recent_pos: HashMap<&str, usize> = recents
            .iter()
            .enumerate()
            .map(|(i, n)| (n.as_str(), i))
            .collect();
        app_scored.sort_by(|a, b| {
            let ra = recent_pos.get(a.1.name.as_str()).copied();
            let rb = recent_pos.get(b.1.name.as_str()).copied();
            let r = ra.unwrap_or(usize::MAX).cmp(&rb.unwrap_or(usize::MAX));
            if r != std::cmp::Ordering::Equal {
                return r;
            }
            // Recent ties broken by launch frequency (most-used first).
            let ua = app_usage.get(&a.1.name).copied().unwrap_or(0);
            let ub = app_usage.get(&b.1.name).copied().unwrap_or(0);
            ub.cmp(&ua).then_with(|| a.1.name.cmp(&b.1.name))
        });
    } else {
        app_scored.sort_by(|a, b| b.0.cmp(&a.0));
    }

    let win_row = |ix: usize| -> LauncherRow {
        let (id, title, app_id, _) = &windows[ix];
        let resolved_icon = app_id.as_deref().and_then(|raw| {
            let lc = raw.to_lowercase();
            let name = app_icons.get(&lc).map(|s| s.as_str()).unwrap_or(raw);
            crate::icons::resolve(name)
        });
        LauncherRow {
            kind: RowKind::Window { id: *id },
            label: title.clone(),
            resolved_icon,
        }
    };
    let app_row = |app: &DesktopApp| -> LauncherRow {
        LauncherRow {
            kind: RowKind::App {
                name: app.name.clone(),
                exec: app.exec.clone(),
            },
            label: app.name.clone(),
            resolved_icon: app.icon.as_deref().and_then(crate::icons::resolve),
        }
    };

    let app_rows: Vec<LauncherRow> = app_scored.into_iter().map(|(_, a)| app_row(a)).collect();
    let win_rows: Vec<LauncherRow> = win_scored.into_iter().map(|(_, ix)| win_row(ix)).collect();
    (app_rows, win_rows)
}

/// Thin wrapper kept for tests: scores app/window rows inline (no cache).
#[cfg(test)]
pub fn filter_rows(
    query: &str,
    apps: &[DesktopApp],
    windows: &[(u64, String, Option<String>, Option<String>)],
    files: &[FileHit],
    recents: &[String],
    app_usage: &HashMap<String, u64>,
    app_icons: &HashMap<String, String>,
    category: Category,
    file_max: usize,
    total_max: usize,
) -> Vec<LauncherRow> {
    filter_rows_cached(
        query, apps, windows, files, recents, app_usage, app_icons, category, file_max, total_max,
        None, None,
    )
}

/// Like [`filter_rows`], but reuses pre-scored app/window rows when the caller
/// supplies them (the Daemon caches these by `query` + `source_gen`). This is
/// the expensive part — `matchq` scoring + a sort over every app/window — so
/// skipping it on the re-render that fires when file results arrive avoids a
/// redundant full re-score on every keystroke.
pub fn filter_rows_cached(
    query: &str,
    apps: &[DesktopApp],
    windows: &[(u64, String, Option<String>, Option<String>)],
    files: &[FileHit],
    recents: &[String],
    app_usage: &HashMap<String, u64>,
    app_icons: &HashMap<String, String>,
    category: Category,
    file_max: usize,
    total_max: usize,
    cached_app_rows: Option<&[LauncherRow]>,
    cached_win_rows: Option<&[LauncherRow]>,
) -> Vec<LauncherRow> {
    let q = query.trim();
    // Inline command modes replace the result list. `r:` falls through to the
    // normal path (it only flips file search to regex mode, handled below).
    if let Some(prefix) = command_prefix(q) {
        match prefix {
            ">" => {
                let cmd = q.strip_prefix('>').unwrap().trim();
                return if cmd.is_empty() {
                    Vec::new()
                } else {
                    vec![LauncherRow {
                        kind: RowKind::Command {
                            command: cmd.to_string(),
                        },
                        label: format!("Run “{}” in terminal", cmd),
                        resolved_icon: None,
                    }]
                };
            }
            "o:" => {
                return open_path_rows(q.strip_prefix("o:").unwrap(), file_max);
            }
            _ => {}
        }
    }
    // Calculator mode: a query that parses as arithmetic shows its result as
    // the sole row. Only in the All view, so category tabs still behave.
    if category == Category::All {
        if let Some(result) = crate::math::evaluate(q) {
            return vec![LauncherRow {
                kind: RowKind::Calc {
                    result: result.clone(),
                },
                label: format!("{} = {}", q.trim(), result),
                resolved_icon: None,
            }];
        }
    }
    if category == Category::Commands {
        return Vec::new();
    }
    // Score folds case internally; keep the raw trimmed query.
    let empty = q.is_empty();
    let apps_only = category == Category::Apps;
    let files_only = category == Category::Files;
    let ranked_cap = if apps_only || files_only {
        None
    } else {
        Some(total_max)
    };

    // App/window scoring (`matchq` over every app/window + a sort) is the
    // expensive part. Reuse the caller-supplied cached rows when available;
    // otherwise score now. The Daemon caches these by `query` + `source_gen`,
    // so the re-render that fires when file results arrive reuses them instead
    // of re-scoring the whole app/window list.
    let (app_rows, win_rows): (Vec<LauncherRow>, Vec<LauncherRow>) =
        match (cached_app_rows, cached_win_rows) {
            (Some(a), Some(w)) if !empty => (a.to_vec(), w.to_vec()),
            _ => score_app_window(q, apps, windows, recents, app_usage, app_icons, category),
        };

    let file_row = |hit: &FileHit| -> LauncherRow {
        LauncherRow {
            kind: RowKind::File {
                path: hit.path.clone(),
            },
            label: hit
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| hit.path.display().to_string()),
            resolved_icon: None,
        }
    };

    let mut out: Vec<LauncherRow> = Vec::new();
    if files_only {
        if !empty {
            push_capped(&mut out, Some(file_max), files.iter().map(file_row));
        }
        return out;
    }
    if apps_only {
        push_capped(&mut out, ranked_cap, app_rows.into_iter());
        return out;
    }

    if crate::files::is_path_shaped(q) {
        // Explicit path navigation: files first, then apps, then windows.
        push_capped(
            &mut out,
            ranked_cap,
            files.iter().take(file_max).map(file_row),
        );
        push_capped(&mut out, ranked_cap, app_rows.into_iter());
        push_capped(&mut out, ranked_cap, win_rows.into_iter());
    } else {
        // Apps are the primary action: rank above files and windows.
        push_capped(&mut out, ranked_cap, app_rows.into_iter());
        if !empty {
            push_capped(
                &mut out,
                ranked_cap,
                files.iter().take(file_max).map(file_row),
            );
        }
        push_capped(&mut out, ranked_cap, win_rows.into_iter());
    }
    // Fallback: nothing matched a non-path query -> offer to run it as a
    // shell command, mirroring the `>` command-mode trigger.
    if out.is_empty() && !empty && !crate::files::is_path_shaped(q) {
        out.push(LauncherRow {
            kind: RowKind::Command {
                command: q.to_string(),
            },
            label: format!("Run “{}” in terminal", q),
            resolved_icon: None,
        });
    }
    out
}

fn icon_letter(app_id: Option<&str>) -> String {
    let Some(app) = app_id else {
        return "#".into();
    };
    let last = app.rsplit('.').next().unwrap_or(app);
    if let Some(c) = last.chars().find(|c| c.is_ascii_alphabetic()) {
        return c.to_ascii_uppercase().to_string();
    }
    if let Some(c) = last.chars().find(|c| c.is_ascii_digit()) {
        return c.to_string();
    }
    last.chars()
        .next()
        .map(|c| c.to_string())
        .unwrap_or_else(|| "#".into())
}

fn icon_slot(row: &LauncherRow, selected: bool, t: &Theme, size: f32, radius: f32) -> gpui::Div {
    let tile = div()
        .size(px(size))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(radius))
        .bg(if selected { t.hover() } else { t.surface() });
    match &row.kind {
        RowKind::File { .. } => tile
            .child(Icon::File.element_px(if selected { t.fg() } else { t.muted() }, size * 0.58)),
        RowKind::Calc { .. } => tile.child(
            Icon::Command.element_px(if selected { t.fg() } else { t.muted() }, size * 0.58),
        ),
        _ => match &row.resolved_icon {
            Some(path) => tile.overflow_hidden().child(
                img(path.clone())
                    .size(px(size))
                    .object_fit(ObjectFit::Contain)
                    .flex_none(),
            ),
            None => tile
                .text_color(if selected { t.fg() } else { t.muted() })
                .text_xs()
                .child(icon_letter(Some(&row.label))),
        },
    }
}

fn highlighted_name(label: &str, query: &str, t: &Theme) -> StyledText {
    let q: Vec<char> = query.trim().to_lowercase().chars().collect();
    let accent = HighlightStyle {
        color: Some(t.accent().into()),
        ..Default::default()
    };
    let mut ranges: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
    let mut qi = 0usize;
    let mut start = 0usize;
    for c in label.chars() {
        let len = c.len_utf8();
        let hit = qi < q.len() && c.to_lowercase().eq(q[qi].to_lowercase());
        if hit {
            let end = start + len;
            if let Some(last) = ranges.last_mut() {
                if last.0.end == start {
                    last.0 = last.0.start..end;
                } else {
                    ranges.push((start..end, accent));
                }
            } else {
                ranges.push((start..end, accent));
            }
            qi += 1;
        }
        start += len;
    }
    StyledText::new(label.to_string()).with_highlights(ranges)
}

/// Secondary line shown under a list item: the file path, the launch command,
/// or a short kind label.
fn row_subtitle(row: &LauncherRow) -> String {
    match &row.kind {
        RowKind::File { path } => path.display().to_string(),
        RowKind::App { exec, .. } => exec.join(" "),
        RowKind::Window { .. } => "Window".into(),
        RowKind::Command { command } => command.clone(),
        // The label already shows "expr = result"; a subtitle would repeat it.
        RowKind::Calc { .. } => String::new(),
    }
}

impl Launcher {
    fn focus_search(&self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle, cx);
    }

    fn keep_selected_visible(&mut self, grid: bool, cx: &mut Context<Self>) {
        let sel = self.view.selected;
        if self.view.rows.is_empty() {
            return;
        }
        if self.scrolled_to == Some(sel) {
            return;
        }
        self.scrolled_to = Some(sel);

        let (item_h, viewport, max_off, cur) = {
            let state = self.scroll.0.borrow();
            let handle = &state.base_handle;
            let Some(item_h) = handle.bounds_for_item(0).map(|b| b.size.height) else {
                drop(state);
                let ix = if grid { sel / GRID_COLS } else { sel };
                self.scroll.scroll_to_item(ix, ScrollStrategy::Nearest);
                return;
            };
            (
                item_h,
                handle.bounds().size.height,
                handle.max_offset().y,
                handle.offset().y,
            )
        };

        let ix = if grid { sel / GRID_COLS } else { sel };
        let item_top = item_h * (ix as f32);
        let item_visible_top = item_top + cur;
        let item_visible_bottom = item_visible_top + item_h;
        let mut target = if item_visible_top < px(0.0) {
            cur - item_visible_top
        } else if item_visible_bottom > viewport {
            cur - (item_visible_bottom - viewport)
        } else {
            return;
        };
        if target > px(0.0) {
            target = px(0.0);
        }
        if target < max_off {
            target = max_off;
        }
        if (target - cur).abs() < px(0.5) {
            return;
        }

        self.scroll_target = Some(target);
        let anim_gen = self.scroll_anim_gen.wrapping_add(1);
        self.scroll_anim_gen = anim_gen;
        cx.spawn(async move |this, cx| {
            loop {
                let current = this
                    .update(cx, |l, _| l.scroll.0.borrow().base_handle.offset().y)
                    .unwrap_or(px(0.0));
                let target_y = this
                    .update(cx, |l, _| l.scroll_target)
                    .unwrap_or(None)
                    .unwrap_or(current);
                if this
                    .update(cx, |l, _| l.scroll_anim_gen != anim_gen)
                    .unwrap_or(true)
                {
                    break;
                }
                let next = current + (target_y - current) * 0.35;
                if (target_y - next).abs() < px(0.5) {
                    let _ = this.update(cx, |l, cx| {
                        l.scroll
                            .0
                            .borrow()
                            .base_handle
                            .set_offset(Point::new(px(0.0), target_y));
                        l.scroll_target = None;
                        cx.notify();
                    });
                    break;
                }
                let _ = this.update(cx, |l, cx| {
                    l.scroll
                        .0
                        .borrow()
                        .base_handle
                        .set_offset(Point::new(px(0.0), next));
                    cx.notify();
                });
                let _ = cx.background_executor().timer(Duration::from_millis(16)).await;
            }
        })
        .detach();
    }

    fn tile(&self, i: usize, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let this = cx.entity();
        let Some(row) = self.view.rows.get(i) else {
            return div()
                .id(("launch-tile-empty", i))
                .flex_1()
                .min_w_0()
                .flex_none()
                .into_any_element();
        };
        let selected = i == self.view.selected;
        let hv = if self.hovered == Some(i) { 1.0f32 } else { 0.0 };
        let base = if selected { t.select() } else { t.ghost() };
        let hover_col = t.select();
        div()
            .id(("launch-tile", i))
            .flex()
            .flex_col()
            .items_center()
            .gap_3()
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .py(px(16.))
            .px(px(8.))
            .rounded(px(10.))
            .bg(base)
            .on_hover(move |h: &bool, _window, cx: &mut App| {
                this.update(cx, |l, cx| {
                    if *h {
                        l.hovered = Some(i);
                    } else if l.hovered == Some(i) {
                        l.hovered = None;
                    }
                    cx.notify();
                });
            })
            .cursor_pointer()
            .on_mouse_move(cx.listener(move |this, _, _, cx| {
                if this.last_select != Some(i) {
                    this.last_select = Some(i);
                    post(this, cx, LauncherCmd::Select { index: i });
                }
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    post(this, cx, LauncherCmd::Activate { index: i });
                }),
            )
            .child(icon_slot(row, selected, t, ICON_GRID, 12.0))
            .child(
                div()
                    .w_full()
                    .text_size(px(13.))
                    .font_weight(FontWeight::BOLD)
                    .text_color(if selected { t.fg() } else { t.muted() })
                    .text_center()
                    .truncate()
                    .child(row.label.clone()),
            )
            .with_spring(
                ("launch-tile-hover", i as u64),
                SpringAnimation::new(ITEM_HOVER_SPRING).to(hv).from(0.0),
                move |el, v| el.bg(mix(&base, &hover_col, v)),
            )
            .into_any_element()
    }

    fn list_row(&self, i: usize, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let this = cx.entity();
        let q = &self.view.query;
        let Some(row) = self.view.rows.get(i) else {
            return div().id(("launch-row-empty", i)).into_any_element();
        };
        let selected = i == self.view.selected;
        let hv = if self.hovered == Some(i) { 1.0f32 } else { 0.0 };
        let base = if selected { t.select() } else { t.ghost() };
        let hover_col = t.select();
        div()
            .id(("launch-row", i))
            .flex()
            .items_center()
            .gap(px(14.))
            .w_full()
            .min_w_0()
            .overflow_hidden()
            .px(px(12.))
            .py(px(11.))
            .rounded(px(9.))
            .bg(base)
            .on_hover(move |h: &bool, _window, cx: &mut App| {
                this.update(cx, |l, cx| {
                    if *h {
                        l.hovered = Some(i);
                    } else if l.hovered == Some(i) {
                        l.hovered = None;
                    }
                    cx.notify();
                });
            })
            .cursor_pointer()
            .on_mouse_move(cx.listener(move |this, _, _, cx| {
                if this.last_select != Some(i) {
                    this.last_select = Some(i);
                    post(this, cx, LauncherCmd::Select { index: i });
                }
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    post(this, cx, LauncherCmd::Activate { index: i });
                }),
            )
            .child(icon_slot(row, selected, t, ICON_LIST, 8.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex_col()
                    .gap(px(2.))
                    .overflow_hidden()
                    .child(
                        div()
                            .text_size(px(16.))
                            .font_weight(FontWeight::BOLD)
                            .text_color(if selected { t.fg() } else { t.muted() })
                            .truncate()
                            .child(highlighted_name(&row.label, q, t)),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(if selected { t.muted() } else { t.faint() })
                            .truncate()
                            .child(row_subtitle(row)),
                    ),
            )
            .with_spring(
                ("launch-row-hover", i as u64),
                SpringAnimation::new(ITEM_HOVER_SPRING).to(hv).from(0.0),
                move |el, v| el.bg(mix(&base, &hover_col, v)),
            )
            .into_any_element()
    }
}

impl Render for Launcher {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let animating = self.view.open || self.closing;
        let open = self.view.open;
        // set_input_region / focus only change on open-close transitions;
        // skip the no-op churn on every animation frame.
        if self.last_input_open != Some(open) {
            window.set_input_region(if open { None } else { Some(&[]) });
            self.last_input_open = Some(open);
        }
        if !animating {
            self.focused = false;
            return div().id("launcher-root").w_full().h_full();
        }
        if open && !self.focused {
            self.focus_search(window, cx);
            self.focused = true;
        } else if !open {
            self.focused = false;
        }
        let target = if open { 1.0f32 } else { 0.0 };

        if let Some(t0) = self.open_started.take() {
            let ms = u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX);
            post(self, cx, LauncherCmd::OpenToRender { ms });
        }

        let t = self.view.theme.clone();
        // Font customization: a rem override scales every text_* token at
        // once; the family refines the root div so all children inherit.
        if let Some(size) = t.font_size {
            window.set_rem_size(px(size as f32));
        }
        let font_family = t.font.clone();
        let win_w = f32::from(window.bounds().size.width);
        let panel_w = LAUNCHER_W.min(win_w * 0.92).max(280.0);
        let q_empty = self.view.query.trim().is_empty();
        let cat = self.view.category;
        let active_cat = if cat != Category::All {
            cat
        } else if q_empty {
            Category::All
        } else {
            self.view
                .rows
                .get(self.view.selected)
                .or_else(|| self.view.rows.first())
                .map(row_category)
                .unwrap_or(Category::All)
        };
        let browsing_grid = cat == Category::Apps && q_empty;
        let show_results = !q_empty || cat != Category::All;
        let compact = q_empty && cat == Category::All;
        let panel_h = if compact { SEARCH_H } else { PANEL_H };
        self.keep_selected_visible(browsing_grid, cx);

        let mut results = div()
            .id("launch-results")
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .p(px(8.));
        results = results.w_full();
        if self.view.rows.is_empty() {
            results = results.child(
                div()
                    .px(px(12.))
                    .py(px(28.))
                    .text_size(px(15.))
                    .font_weight(FontWeight::BOLD)
                    .text_color(t.faint())
                    .child("no matches"),
            );
        } else if browsing_grid {
            let n = self.view.rows.len().div_ceil(GRID_COLS);
            let t_grid = t.clone();
            results = results.child(
                uniform_list(
                    "launch-grid",
                    n,
                    cx.processor(move |this, range: Range<usize>, _, cx| {
                        range
                            .map(|row_i| {
                                let mut row =
                                    div().flex().flex_row().gap(px(10.)).p(px(8.)).w_full();
                                for col in 0..GRID_COLS {
                                    let i = row_i * GRID_COLS + col;
                                    row = row.child(this.tile(i, &t_grid, cx));
                                }
                                row
                            })
                            .collect()
                    }),
                )
                .track_scroll(&self.scroll)
                .flex_1()
                .h_full(),
            );
        } else {
            let n = self.view.rows.len();
            let t_list = t.clone();
            results = results.child(
                uniform_list(
                    "launch-list",
                    n,
                    cx.processor(move |this, range: Range<usize>, _, cx| {
                        range.map(|i| this.list_row(i, &t_list, cx)).collect()
                    }),
                )
                .track_scroll(&self.scroll)
                .flex_1()
                .h_full(),
            );
        }

        let mut cat_icons = div().flex().flex_none().items_center().gap(px(14.));
        let this = cx.entity();
        let files_enabled = self.view.files_enabled;
        for c in Category::all() {
            if c == Category::Files && !files_enabled {
                continue;
            }
            let active = active_cat == c;
            let cc = c;
            let this = this.clone();
            let icon_col = if active { t.accent() } else { t.muted() };
            cat_icons = cat_icons.child(
                div()
                    .id(("cat-icon", c as u64))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .on_hover(move |h: &bool, _window, cx: &mut App| {
                        this.update(cx, |l, cx| {
                            if *h {
                                l.hovered_chip = Some(cc);
                            } else if l.hovered_chip == Some(cc) {
                                l.hovered_chip = None;
                            }
                            cx.notify();
                        });
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            let next = if this.view.category == cc {
                                Category::All
                            } else {
                                cc
                            };
                            post(this, cx, LauncherCmd::SetCategory { category: next });
                        }),
                    )
                    .child(c.icon().element_px(icon_col, 20.0)),
            );
        }

        let action_menu_el = self.action_menu.as_ref().map(|menu| {
            let index = menu.index;
            // Anchor the menu to the selected row: open below it, or above when
            // there isn't room, so it never covers the highlighted item.
            let (item_h, scroll_y, viewport_h) = {
                let s = self.scroll.0.borrow();
                let item_h = s
                    .base_handle
                    .bounds_for_item(0)
                    .map(|b| b.size.height)
                    .unwrap_or(px(56.));
                let scroll_y = s.base_handle.offset().y;
                let viewport_h = s.base_handle.bounds().size.height;
                (item_h, scroll_y, viewport_h)
            };
            let visual_ix = if browsing_grid {
                self.view.selected / GRID_COLS
            } else {
                self.view.selected
            };
            let row_top = item_h * (visual_ix as f32) + scroll_y; // selected row's top, relative to the list
            let menu_h = px(36.) * (menu.actions.len() as f32) + px(12.);
            let top = action_menu_top(row_top, item_h, menu_h, viewport_h);
            div()
                .absolute()
                .left(px(20.))
                .right(px(20.))
                .top(top)
                .flex_col()
                .gap(px(2.))
                .p(px(6.))
                .bg(t.panel())
                .border_1()
                .border_color(t.border())
                .rounded(px(10.))
                .shadow_lg()
                .children(menu.actions.iter().enumerate().map(|(i, a)| {
                    let selected = i == index;
                    div()
                        .id(("action", i as u64))
                        .flex()
                        .items_center()
                        .justify_between()
                        .px(px(12.))
                        .py(px(8.))
                        .rounded(px(6.))
                        .bg(if selected { t.select() } else { t.ghost() })
                        .text_color(t.fg())
                        .child(div().child(a.label()))
                        .child(div().child(if i == 0 { "↵" } else { "" }))
                }))
        });

        let results_body = div()
            .relative()
            .flex()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .border_t_1()
            .border_color(t.border())
            .child(results)
            .when_some(action_menu_el, |el, menu| el.child(menu));

        let search_focus = self.focus_handle.clone();
        div()
            .id("launcher-root")
            .track_focus(&search_focus)
            .relative()
            .flex()
            .items_center()
            .justify_center()
            .w_full()
            .h_full()
            .when_some(font_family, |root, family| root.font_family(family))
            .capture_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, _, cx| {
                let key = ev.keystroke.key.to_ascii_lowercase();
                if this.action_menu.is_some() {
                    cx.stop_propagation();
                    match key.as_str() {
                        "escape" | "esc" => {
                            this.action_menu = None;
                            cx.notify();
                        }
                        "up" | "arrowup" => {
                            if let Some(m) = this.action_menu.as_mut() {
                                m.index = m.index.saturating_sub(1);
                            }
                            cx.notify();
                        }
                        "down" | "arrowdown" => {
                            if let Some(m) = this.action_menu.as_mut() {
                                if !m.actions.is_empty() {
                                    m.index = (m.index + 1).min(m.actions.len() - 1);
                                }
                            }
                            cx.notify();
                        }
                        "enter" | "return" => {
                            let menu = this.action_menu.take();
                            if let Some(menu) = menu {
                                if let Some(row) = this.view.rows.get(this.view.selected) {
                                    let kind = row.kind.clone();
                                    if let Some(action) = menu.actions.get(menu.index).copied() {
                                        let shell = this.shell.clone();
                                        cx.defer(move |cx| {
                                            let _ = shell.update(cx, |d, cx| {
                                                d.run_row_action(kind, action, cx);
                                            });
                                        });
                                    }
                                }
                            }
                            cx.notify();
                        }
                        _ => {}
                    }
                    return;
                }
                if matches!(
                    key.as_str(),
                    "escape" | "esc" | "enter" | "return" | "up" | "arrowup" | "down" | "arrowdown"
                ) {
                    if ev.keystroke.modifiers.alt && matches!(key.as_str(), "enter" | "return") {
                        cx.stop_propagation();
                        if let Some(row) = this.view.rows.get(this.view.selected) {
                            let actions = row.kind.actions();
                            if !actions.is_empty() {
                                this.action_menu = Some(ActionMenu { actions, index: 0 });
                                cx.notify();
                            }
                        }
                        return;
                    }
                    cx.stop_propagation();
                    post(
                        this,
                        cx,
                        LauncherCmd::Key {
                            key: ev.keystroke.key.clone(),
                            ch: ev.keystroke.key_char.clone(),
                            shift: ev.keystroke.modifiers.shift,
                        },
                    );
                    return;
                }
                if key == "tab" {
                    cx.stop_propagation();
                    match tab_completion(&this.view.query, &this.view.rows, this.view.selected) {
                        Some(TabOutcome::Inline {
                            completed,
                            accepted_off,
                        }) => {
                            this.cursor = completed.len();
                            this.accepted = Some((completed.clone(), accepted_off));
                            this.view.query = completed.clone();
                            post(this, cx, LauncherCmd::SetQuery { query: completed });
                        }
                        Some(TabOutcome::Row(completion)) => {
                            this.cursor = completion.len();
                            this.accepted = None;
                            post(this, cx, LauncherCmd::SetQuery { query: completion });
                        }
                        None => {}
                    }
                    return;
                }
                if let Some(query) = this.edit(&ev.keystroke) {
                    cx.stop_propagation();
                    post(this, cx, LauncherCmd::SetQuery { query });
                }
                cx.notify();
            }))
            .child(
                div()
                    .id("launcher-scrim")
                    .absolute()
                    .inset_0()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            post(this, cx, LauncherCmd::Dismiss);
                        }),
                    ),
            )
            .child(
                div()
                    .id("launcher-panel-wrap")
                    .relative()
                    .flex_none()
                    .with_spring(
                        "launcher-panel-h",
                        SpringAnimation::new(HEIGHT_SPRING).to(panel_h).from(0.0),
                        |el, h| el.h(px(h)),
                    )
                    .child(
                        div()
                            .id("launcher-panel")
                            .relative()
                            .w(px(panel_w))
                            .max_w_full()
                            .h_full()
                            .flex()
                            .flex_col()
                            .overflow_hidden()
                            .rounded(px(16.))
                            .border_1()
                            .border_color(t.border())
                            .bg(t.panel())
                            .text_color(t.fg())
                            .shadow_lg()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|_, _, _, cx| cx.stop_propagation()),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_none()
                                    .items_center()
                                    .gap(px(12.))
                                    .px(px(20.))
                                    .py(px(16.))
                                    .child(
                                        div().h(px(32.)).flex().items_center().flex_none().child(
                                            img(Arc::new(Image::from_bytes(
                                                ImageFormat::Svg,
                                                AWARI_MARK.to_vec(),
                                            )))
                                            .size(px(32.))
                                            .flex_none(),
                                        ),
                                    )
                                    .child(
                                        div()
                                            .id("query-wrap")
                                            .flex_1()
                                            .min_w_0()
                                            .flex()
                                            .flex_row()
                                            .items_center()
                                            .mt(px(2.))
                                            .when_some(t.font.clone(), |el, f| el.font_family(f))
                                            .text_size(px(24.))
                                            .line_height(px(24.))
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(t.fg())
                                            .overflow_hidden()
                                            .child(self.query_element())
                                            .child(div().flex_1().min_w_0())
                                            .child(cat_icons),
                                    ),
                            )
                            .when(show_results, |el| el.child(results_body))
                            .with_spring(
                                "launcher-panel",
                                SpringAnimation::new(PANEL_SPRING).to(target).from(0.0),
                                |el, v| el.mt(px((1.0 - v) * SLIDE)).opacity(v),
                            ),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(name: &str, app_id: Option<&str>) -> DesktopApp {
        DesktopApp {
            name: name.into(),
            exec: vec![name.to_lowercase()],
            app_id: app_id.map(Into::into),
            icon: None,
            name_lc: name.to_lowercase(),
            app_id_lc: app_id.map(|s| s.to_lowercase()),
        }
    }

    fn rows(
        q: &str,
        apps: &[DesktopApp],
        windows: &[(u64, String, Option<String>, Option<String>)],
        files: &[FileHit],
        recents: &[String],
    ) -> Vec<LauncherRow> {
        filter_rows(
            q,
            apps,
            windows,
            files,
            recents,
            &Default::default(),
            &Default::default(),
            Category::All,
            50,
            30,
        )
    }

    #[test]
    fn filter_matches_name_case_insensitive() {
        let apps = vec![app("Firefox", None)];
        let out = rows(
            "fire",
            &apps,
            &[(1, "Terminal".into(), None, None)],
            &[],
            &[],
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].label, "Firefox");
    }

    #[test]
    fn fuzzy_typo_still_matches() {
        let apps = vec![app("Firefox", None)];
        let out = rows("firfox", &apps, &[], &[], &[]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].label, "Firefox");
    }

    #[test]
    fn running_window_suppresses_app_row() {
        let apps = vec![app("Firefox", Some("firefox"))];
        let out = rows(
            "",
            &apps,
            &[(
                7,
                "Mozilla Firefox".into(),
                Some("firefox".into()),
                Some("firefox".into()),
            )],
            &[],
            &[],
        );
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0].kind, RowKind::Window { .. }));
    }

    #[test]
    fn empty_query_orders_apps_by_recency() {
        let apps = vec![app("Alpha", None), app("Beta", None), app("Gamma", None)];
        let out = rows("", &apps, &[], &[], &["Gamma".into()]);
        assert_eq!(out[0].label, "Gamma");
    }

    #[test]
    fn path_shaped_query_puts_files_first() {
        let files = vec![FileHit {
            path: "/tmp/notes.md".into(),
        }];
        let out = rows(
            "~/not",
            &[app("Notes", None)],
            &[(1, "Editor".into(), None, None)],
            &files,
            &[],
        );
        assert!(matches!(out[0].kind, RowKind::File { .. }));
    }

    #[test]
    fn empty_query_never_dumps_files() {
        let files = vec![FileHit {
            path: "/tmp/x".into(),
        }];
        let out = rows("", &[app("Zed", None)], &[], &files, &[]);
        assert!(out.iter().all(|r| !matches!(r.kind, RowKind::File { .. })));
    }

    #[test]
    fn apps_chip_empty_is_uncapped() {
        let apps: Vec<DesktopApp> = (0..30).map(|i| app(&format!("App{i:02}"), None)).collect();
        let out = filter_rows(
            "",
            &apps,
            &[],
            &[],
            &[],
            &Default::default(),
            &Default::default(),
            Category::Apps,
            50,
            30,
        );
        assert_eq!(out.len(), 30);
    }

    #[test]
    fn files_chip_returns_every_hit() {
        let files: Vec<FileHit> = (0..40)
            .map(|i| FileHit {
                path: format!("/tmp/f{i}").into(),
            })
            .collect();
        let out = filter_rows(
            "f",
            &[],
            &[],
            &files,
            &[],
            &Default::default(),
            &Default::default(),
            Category::Files,
            50,
            30,
        );
        assert_eq!(out.len(), 40);
    }

    #[test]
    fn commands_chip_is_empty() {
        let out = filter_rows(
            "x",
            &[app("X", None)],
            &[],
            &[],
            &[],
            &Default::default(),
            &Default::default(),
            Category::Commands,
            50,
            30,
        );
        assert!(out.is_empty());
    }

    #[test]
    fn row_cap_holds() {
        let apps: Vec<DesktopApp> = (0..40).map(|i| app(&format!("App{i:02}"), None)).collect();
        let out = rows("app", &apps, &[], &[], &[]);
        assert_eq!(out.len(), 30);
    }

    #[test]
    fn calculator_shows_result_in_all_view() {
        let out = filter_rows(
            "2 + 2",
            &[],
            &[],
            &[],
            &[],
            &Default::default(),
            &Default::default(),
            Category::All,
            50,
            30,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].label, "2 + 2 = 4");
        assert!(matches!(out[0].kind, RowKind::Calc { .. }));
    }

    #[test]
    fn calculator_only_in_all_view() {
        let out = filter_rows(
            "2 + 2",
            &[],
            &[],
            &[],
            &[],
            &Default::default(),
            &Default::default(),
            Category::Apps,
            50,
            30,
        );
        assert!(out.is_empty());
    }

    #[test]
    fn open_path_lists_real_entries() {
        let dir = std::env::temp_dir().join(format!("awari_o_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("alpha.txt"), b"").unwrap();
        std::fs::write(dir.join("beta.log"), b"").unwrap();
        let out = open_path_rows(&dir.to_string_lossy(), 50);
        let names: Vec<&str> = out.iter().map(|r| r.label.as_str()).collect();
        assert!(names.iter().any(|n| *n == "alpha.txt"), "{names:?}");
        assert!(names.iter().any(|n| *n == "beta.log"), "{names:?}");
        // An existing directory also offers a direct "Open <dir>" row.
        assert!(
            out.iter()
                .any(|r| r.label == format!("Open “{}”", dir.display()))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_path_direct_row_only_when_exists() {
        let dir = std::env::temp_dir().join(format!("awari_o2_{}", std::process::id()));
        let arg = format!("{}/missing.txt", dir.display());
        let out = open_path_rows(&arg, 50);
        // Nonexistent path with no readable parent -> no optimistic row.
        assert!(out.is_empty());
    }

    #[test]
    fn launcher_exclusive_zone_is_zero() {
        let opts = layer_opts();
        assert_eq!(f32::from(opts.exclusive_zone.unwrap()), 0.0);
        assert_eq!(opts.namespace, LAUNCHER_NAMESPACE);
    }

    #[test]
    fn icon_letter_falls_back_to_digit_then_hash() {
        assert_eq!(icon_letter(Some("org.example.firefox")), "F");
        assert_eq!(icon_letter(Some("1234")), "1");
        assert_eq!(icon_letter(None), "#");
    }
}

#[cfg(test)]
mod open_path_tests {
    use super::*;

    #[test]
    fn expand_resolves_relative_and_absolute() {
        unsafe { std::env::set_var("HOME", "/home/tester") };
        assert_eq!(
            expand_open_path("~/docs"),
            Some(PathBuf::from("/home/tester/docs"))
        );
        assert_eq!(
            expand_open_path("/abs/path"),
            Some(PathBuf::from("/abs/path"))
        );
        // Bare name resolves relative to $HOME.
        assert_eq!(
            expand_open_path("notes.txt"),
            Some(PathBuf::from("/home/tester/notes.txt"))
        );
        assert!(expand_open_path("   ").is_none());
    }

    #[test]
    fn command_token_lengths() {
        assert_eq!(command_token_len("r:foo"), 2);
        assert_eq!(command_token_len("o:foo"), 2);
        assert_eq!(command_token_len(">foo"), 1);
        assert_eq!(command_token_len("plain"), 0);
    }

    #[test]
    fn ghost_completes_case_insensitively() {
        assert_eq!(ghost_suffix("Go", "GoLand").as_deref(), Some("Land"));
        assert_eq!(ghost_suffix("gO", "GoLand").as_deref(), Some("Land"));
        assert_eq!(ghost_suffix("goland", "GoLand").as_deref(), None);
        assert_eq!(ghost_suffix("golands", "GoLand"), None);
        assert_eq!(ghost_suffix("", "GoLand"), None);
        assert_eq!(ghost_suffix("zz", "GoLand"), None);
    }

    #[test]
    fn ghost_respects_char_boundaries() {
        assert_eq!(ghost_suffix("é", "Émigré").as_deref(), Some("migré"));
        // Multi-byte query that fully consumes the label → nothing to add.
        assert_eq!(ghost_suffix("émigré", "Émigré"), None);
    }

    fn lrow(kind: RowKind, label: &str) -> LauncherRow {
        LauncherRow {
            kind,
            label: label.into(),
            resolved_icon: None,
        }
    }

    #[test]
    fn tab_inline_completes_top_row() {
        let rows = vec![lrow(
            RowKind::App {
                name: "GoLand".into(),
                exec: vec![],
            },
            "GoLand",
        )];
        match tab_completion("go", &rows, 0) {
            Some(TabOutcome::Inline {
                completed,
                accepted_off,
            }) => {
                assert_eq!(completed, "GoLand");
                assert_eq!(accepted_off, 2);
            }
            other => panic!("expected Inline, got {other:?}"),
        }
        assert!(tab_completion("", &rows, 0).is_some());
    }

    #[test]
    fn tab_command_mode_skips_ghost() {
        let rows = vec![lrow(
            RowKind::Command {
                command: "foo --bar".into(),
            },
            "foo",
        )];
        match tab_completion(">fo", &rows, 0) {
            Some(TabOutcome::Row(c)) => assert_eq!(c, "foo --bar"),
            other => panic!("expected Row, got {other:?}"),
        }
    }

    #[test]
    fn tab_calc_row_never_ghosts() {
        let rows = vec![lrow(
            RowKind::Calc {
                result: "42".into(),
            },
            "1+1 = 42",
        )];
        match tab_completion("1+", &rows, 0) {
            Some(TabOutcome::Inline { .. }) => panic!("calc must not ghost"),
            Some(TabOutcome::Row(c)) => assert_eq!(c, "42"),
            None => panic!("expected selected-row completion"),
        }
    }

    #[test]
    fn tab_falls_back_to_selected_file_path() {
        let rows = vec![
            lrow(
                RowKind::File {
                    path: PathBuf::from("/tmp/a b.txt"),
                },
                "a b.txt",
            ),
            lrow(
                RowKind::App {
                    name: "Zed".into(),
                    exec: vec![],
                },
                "Zed",
            ),
        ];
        match tab_completion("zed", &rows, 0) {
            Some(TabOutcome::Row(p)) => assert_eq!(p, "/tmp/a b.txt"),
            other => panic!("expected File path, got {other:?}"),
        }
        assert!(matches!(
            tab_completion("zzz", &rows, 1),
            Some(TabOutcome::Row(_))
        ));
        assert!(tab_completion("zzz", &rows, 9).is_none());
    }
}
