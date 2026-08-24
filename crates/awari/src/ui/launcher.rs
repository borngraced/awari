//! Overlay finder: search, chips, list/grid. Clean, text-focused layout.

use gpui::{
    div, img, px, AnyElement, AnimationExt, App, Context, FocusHandle,
    Focusable, FontWeight, Rgba, InteractiveElement, IntoElement, MouseButton, ObjectFit,
    ParentElement, Render, ScrollStrategy, SpringAnimation, SpringConfig, Styled, StyledImage,
    UniformListScrollHandle, WeakEntity, Window, uniform_list,
};
use gpui::prelude::*;
use std::ops::Range;
use std::path::PathBuf;
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
const ROW_CAP: usize = 10;
const FILE_ROWS: usize = 8;
const GRID_COLS: usize = 4;
const SLIDE: f32 = 22.0;
const PANEL_SPRING: SpringConfig = SpringConfig::new(380.0, 30.0, 1.0);
const HEIGHT_SPRING: SpringConfig = SpringConfig::new(380.0, 30.0, 1.0);
const SEARCH_H: f32 = 68.0;
const ITEM_HOVER_SPRING: SpringConfig = SpringConfig::new(420.0, 34.0, 1.0);
const CHIP_HOVER_SPRING: SpringConfig = SpringConfig::new(360.0, 30.0, 1.0);

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
    Key { key: String, ch: Option<String> },
    SetQuery { query: String },
    Activate { index: usize },
    Select { index: usize },
    SetCategory { category: Category },
    OpenToRender { ms: u64 },
}

#[derive(Clone)]
pub struct LauncherRow {
    pub kind: RowKind,
    pub label: String,
    pub icon: Option<String>,
}

#[derive(Clone)]
pub enum RowKind {
    App { exec: Vec<String> },
    Window { id: u64 },
    File { path: PathBuf },
}

#[derive(Clone)]
pub struct LauncherView {
    pub open: bool,
    pub query: String,
    pub selected: usize,
    pub rows: Vec<LauncherRow>,
    pub theme: Theme,
    pub category: Category,
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
        }
    }
}

pub struct Launcher {
    pub shell: WeakEntity<Daemon>,
    view: LauncherView,
    cursor: usize,
    caret_on: bool,
    focus_handle: FocusHandle,
    scroll: UniformListScrollHandle,
    scrolled_to: Option<usize>,
    open_started: Option<Instant>,
    pub(crate) closing: bool,
    hovered: Option<usize>,
    hovered_chip: Option<Category>,
}

impl Launcher {
    pub fn new(
        shell: WeakEntity<Daemon>,
        theme: Theme,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let blink = cx.entity();
        cx.spawn(async move |_, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(530))
                    .await;
                let _ = blink.update(cx, |this, cx| {
                    if this.view.open {
                        this.caret_on = !this.caret_on;
                        cx.notify();
                    }
                });
            }
        })
        .detach();
        Self {
            shell,
            view: LauncherView::closed(theme),
            cursor: 0,
            caret_on: true,
            focus_handle: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            scrolled_to: None,
            open_started: None,
            closing: false,
            hovered: None,
            hovered_chip: None,
        }
    }

    pub fn apply_view(&mut self, view: LauncherView) {
        if self.view.query != view.query || self.view.category != view.category {
            self.scrolled_to = None;
        }
        self.view = view;
        if self.view.open {
            self.closing = false;
        } else {
            self.cursor = 0;
            self.caret_on = true;
        }
    }

    pub fn arm_open_timer(&mut self, started: Instant) {
        self.open_started = Some(started);
    }

    /// Apply a keystroke to the query/cursor; returns the new query when it changed.
    fn edit(&mut self, k: &gpui::Keystroke) -> Option<String> {
        let mut q = self.view.query.clone();
        let mut c = self.cursor.min(q.len());
        match k.key.as_str() {
            "backspace" => {
                if c == 0 {
                    return None;
                }
                let prev = q[..c].char_indices().next_back().map(|(i, _)| i).unwrap_or(0);
                q.replace_range(prev..c, "");
                c = prev;
            }
            "delete" => {
                if c >= q.len() {
                    return None;
                }
                let next = q[c..].char_indices().nth(1).map(|(i, _)| c + i).unwrap_or(q.len());
                q.replace_range(c..next, "");
            }
            "arrowleft" => {
                c = q[..c].char_indices().next_back().map(|(i, _)| i).unwrap_or(0);
            }
            "arrowright" => {
                c = q[c..].char_indices().nth(1).map(|(i, _)| c + i).unwrap_or(q.len());
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
        let (prefix, suffix) = q.split_at(c);
        div()
            .flex()
            .flex_nowrap()
            .items_center()
            .flex_none()
            .child(div().child(prefix.to_string()))
            .child(caret)
            .child(div().child(suffix.to_string()))
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
    }
}

pub fn filter_rows(
    query: &str,
    apps: &[DesktopApp],
    windows: &[(u64, String, Option<String>)],
    files: &[FileHit],
    recents: &[String],
    category: Category,
) -> Vec<LauncherRow> {
    if category == Category::Commands {
        return Vec::new();
    }
    let q = query.trim().to_lowercase();
    let empty = q.is_empty();
    let apps_only = category == Category::Apps;
    let files_only = category == Category::Files;
    let ranked_cap = if apps_only || files_only {
        None
    } else {
        Some(ROW_CAP)
    };

    let mut win_rows: Vec<(i64, LauncherRow)> = if files_only || apps_only {
        Vec::new()
    } else {
        windows
            .iter()
            .filter_map(|(id, title, app_id)| {
                let s = if empty {
                    1
                } else {
                    crate::matchq::score(title, &q)
                        .max(app_id.as_deref().and_then(|a| crate::matchq::score(a, &q)))?
                };
                Some((
                    s,
                    LauncherRow {
                        kind: RowKind::Window { id: *id },
                        label: title.clone(),
                        icon: app_id.clone(),
                    },
                ))
            })
            .collect()
    };
    if !empty {
        win_rows.sort_by(|a, b| b.0.cmp(&a.0));
    }

    let visible_app_ids: Vec<String> = win_rows
        .iter()
        .filter_map(|(_, r)| r.icon.as_deref().map(|s| s.to_lowercase()))
        .collect();

    let mut app_rows: Vec<(i64, LauncherRow)> = if files_only {
        Vec::new()
    } else {
        apps.iter()
            .filter_map(|app| {
                if !apps_only {
                    let ident_hits_window = |probe: Option<&str>| {
                        probe
                            .map(|p| {
                                visible_app_ids
                                    .iter()
                                    .any(|v| v == p.to_lowercase().as_str())
                            })
                            .unwrap_or(false)
                    };
                    if ident_hits_window(Some(&app.name))
                        || ident_hits_window(app.app_id.as_deref())
                    {
                        return None;
                    }
                }
                let s = if empty {
                    1
                } else {
                    let by_name = crate::matchq::score(&app.name, &q);
                    let by_id = app
                        .app_id
                        .as_deref()
                        .and_then(|a| crate::matchq::score(a, &q));
                    match (by_name, by_id) {
                        (Some(a), Some(b)) => Some(a.max(b)),
                        (a, b) => a.or(b),
                    }?
                };
                Some((
                    s,
                    LauncherRow {
                        kind: RowKind::App {
                            exec: app.exec.clone(),
                        },
                        label: app.name.clone(),
                        icon: app.icon.clone(),
                    },
                ))
            })
            .collect()
    };
    if empty {
        app_rows.sort_by(|a, b| {
            let ra = recents.iter().position(|n| *n == a.1.label);
            let rb = recents.iter().position(|n| *n == b.1.label);
            ra.unwrap_or(usize::MAX)
                .cmp(&rb.unwrap_or(usize::MAX))
                .then_with(|| a.1.label.cmp(&b.1.label))
        });
    } else {
        app_rows.sort_by(|a, b| b.0.cmp(&a.0));
    }

    let file_rows = |take: usize| {
        files
            .iter()
            .take(take)
            .map(|hit| LauncherRow {
                kind: RowKind::File {
                    path: hit.path.clone(),
                },
                label: hit
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| hit.path.display().to_string()),
                icon: None,
            })
            .collect::<Vec<_>>()
    };

    let mut out: Vec<LauncherRow> = Vec::new();
    let push = |out: &mut Vec<LauncherRow>, rows: Vec<LauncherRow>| {
        for r in rows {
            if ranked_cap.is_some_and(|c| out.len() >= c) {
                return;
            }
            out.push(r);
        }
    };

    if files_only {
        if !empty {
            push(&mut out, file_rows(files.len()));
        }
        return out;
    }
    if apps_only {
        push(
            &mut out,
            app_rows.into_iter().map(|(_, r)| r).collect(),
        );
        return out;
    }

    if crate::files::is_path_shaped(&q) {
        push(&mut out, file_rows(FILE_ROWS));
        push(&mut out, win_rows.into_iter().map(|(_, r)| r).collect());
        push(&mut out, app_rows.into_iter().map(|(_, r)| r).collect());
    } else {
        push(&mut out, win_rows.into_iter().map(|(_, r)| r).collect());
        push(&mut out, app_rows.into_iter().map(|(_, r)| r).collect());
        if !empty {
            push(&mut out, file_rows(FILE_ROWS));
        }
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

fn icon_slot(row: &LauncherRow, selected: bool, t: Theme, size: f32, radius: f32) -> gpui::Div {
    let tile = div()
        .size(px(size))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(radius))
        .bg(if selected {
            t.hover()
        } else {
            t.surface()
        });
    match &row.kind {
        RowKind::File { .. } => tile.child(Icon::File.element_px(
            if selected { t.fg() } else { t.muted() },
            size * 0.58,
        )),
        _ => match crate::icons::resolve(row.icon.as_deref().unwrap_or("")) {
            Some(path) => tile.overflow_hidden().child(
                img(path)
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

fn highlighted_name(label: &str, query: &str, selected: bool, t: Theme, size: f32) -> gpui::Div {
    let base = if selected { t.fg() } else { t.muted() };
    let q: Vec<char> = query.trim().to_lowercase().chars().collect();
    if q.is_empty() {
        return div()
            .text_size(px(size))
            .font_weight(FontWeight::BOLD)
            .text_color(base)
            .truncate()
            .child(label.to_string());
    }
    let mut row = div().flex().flex_row().min_w_0().overflow_hidden();
    let mut qi = 0usize;
    for c in label.chars() {
        let hit = qi < q.len() && c.to_lowercase().eq(q[qi].to_lowercase());
        if hit {
            qi += 1;
        }
        let mut span = div()
            .text_size(px(size))
            .font_weight(FontWeight::BOLD)
            .child(c.to_string());
        if hit {
            span = span.text_color(t.accent());
        } else {
            span = span.text_color(base);
        }
        row = row.child(span);
    }
    row
}

/// Secondary line shown under a list item: the file path, the launch command,
/// or a short kind label.
fn row_subtitle(row: &LauncherRow) -> String {
    match &row.kind {
        RowKind::File { path } => path.display().to_string(),
        RowKind::App { exec } => exec.join(" "),
        RowKind::Window { .. } => "Window".into(),
    }
}

impl Launcher {
    fn focus_search(&self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle, cx);
    }

    fn keep_selected_visible(&mut self, grid: bool) {
        let sel = self.view.selected;
        if self.scrolled_to == Some(sel) {
            return;
        }
        self.scrolled_to = Some(sel);
        let ix = if grid {
            sel / GRID_COLS
        } else {
            sel
        };
        self.scroll.scroll_to_item(ix, ScrollStrategy::Nearest);
    }

    fn tile(&self, i: usize, cx: &mut Context<Self>) -> AnyElement {
        let t = self.view.theme.clone();
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
                post(this, cx, LauncherCmd::Select { index: i });
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    post(this, cx, LauncherCmd::Activate { index: i });
                }),
            )
            .child(icon_slot(row, selected, t.clone(), ICON_GRID, 12.0))
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

    fn list_row(&self, i: usize, cx: &mut Context<Self>) -> AnyElement {
        let t = self.view.theme.clone();
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
                post(this, cx, LauncherCmd::Select { index: i });
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    post(this, cx, LauncherCmd::Activate { index: i });
                }),
            )
            .child(icon_slot(row, selected, t.clone(), ICON_LIST, 8.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex_col()
                    .gap(px(2.))
                    .overflow_hidden()
                    .child(highlighted_name(&row.label, q, selected, t.clone(), 16.0))
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
        if !animating {
            window.set_input_region(Some(&[]));
            return div().id("launcher-root").w_full().h_full();
        }
        window.set_input_region(if self.view.open { None } else { Some(&[]) });
        let target = if self.view.open { 1.0f32 } else { 0.0 };
        self.focus_search(window, cx);

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
        self.keep_selected_visible(browsing_grid);

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
            results = results.child(
                uniform_list(
                    "launch-grid",
                    n,
                    cx.processor(move |this, range: Range<usize>, _, cx| {
                        range
                            .map(|row_i| {
                                let mut row = div()
                                    .flex()
                                    .flex_row()
                                    .gap(px(10.))
                                    .p(px(8.))
                                    .w_full();
                                for col in 0..GRID_COLS {
                                    let i = row_i * GRID_COLS + col;
                                    row = row.child(this.tile(i, cx));
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
            results = results.child(
                uniform_list(
                    "launch-list",
                    n,
                    cx.processor(|this, range: Range<usize>, _, cx| {
                        range.map(|i| this.list_row(i, cx)).collect()
                    }),
                )
                .track_scroll(&self.scroll)
                .flex_1()
                .h_full(),
            );
        }

        let mut cat_icons = div()
            .absolute()
            .top(px(-18.0))
            .left_0()
            .right_0()
            .flex()
            .flex_none()
            .justify_center()
            .gap(px(14.))
            .with_spring(
                "cat-icons-fade",
                SpringAnimation::new(PANEL_SPRING).to(target).from(0.0),
                |el, v| el.opacity(v),
            );
        let this = cx.entity();
        for c in Category::all() {
            let active = active_cat == c;
            let cc = c;
            let this = this.clone();
            let chip_hv = if self.hovered_chip == Some(c) { 1.0f32 } else { 0.0 };
            let base_col = if active { t.select() } else { t.surface() };
            let hover_col = if active {
                mix(&t.accent(), &t.bg(), 0.5)
            } else {
                t.border()
            };
            let icon_col = if active { t.accent() } else { t.muted() };
            cat_icons = cat_icons.child(
                div()
                    .id(("cat-icon", c as u64))
                    .size(px(36.))
                    .rounded(px(18.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(base_col)
                    .border_1()
                    .border_color(if active { t.accent() } else { t.ghost() })
                    .shadow_sm()
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
                    .child(c.icon().element_px(icon_col, 18.0))
                    .with_spring(
                        ("cat-icon-hover", c as u64),
                        SpringAnimation::new(CHIP_HOVER_SPRING).to(chip_hv).from(0.0),
                        move |el, v| el.bg(mix(&base_col, &hover_col, v)),
                    ),
            );
        }

        let results_body = div()
            .flex()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .border_t_1()
            .border_color(t.border())
            .child(results);

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
                if matches!(
                    key.as_str(),
                    "escape" | "esc" | "enter" | "return" | "up" | "arrowup" | "down" | "arrowdown"
                ) {
                    cx.stop_propagation();
                    post(
                        this,
                        cx,
                        LauncherCmd::Key {
                            key: ev.keystroke.key.clone(),
                            ch: ev.keystroke.key_char.clone(),
                        },
                    );
                    return;
                }
                if let Some(query) = this.edit(&ev.keystroke) {
                    cx.stop_propagation();
                    post(this, cx, LauncherCmd::SetQuery { query });
                }
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
                                div()
                                    .h(px(24.))
                                    .flex()
                                    .items_center()
                                    .flex_none()
                                    .child(Icon::Search.element_px(
                                        if q_empty { t.faint() } else { t.accent() },
                                        20.0,
                                    )),
                            )
                            .child(
                                div()
                                    .id("query-wrap")
                                    .flex_1()
                                    .min_w_0()
                                    .mt(px(2.))
                                    .when_some(t.font.clone(), |el, f| el.font_family(f))
                                    .text_size(px(24.))
                                    .line_height(px(24.))
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(t.fg())
                                    .overflow_hidden()
                                    .child(self.query_element()),
                            )
                            )
                            .when(show_results, |el| el.child(results_body))
                            .with_spring(
                                "launcher-panel",
                                SpringAnimation::new(PANEL_SPRING).to(target).from(0.0),
                                |el, v| el.mt(px((1.0 - v) * SLIDE)).opacity(v),
                            ),
                    )
                    .child(cat_icons),
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
        }
    }

    fn rows(
        q: &str,
        apps: &[DesktopApp],
        windows: &[(u64, String, Option<String>)],
        files: &[FileHit],
        recents: &[String],
    ) -> Vec<LauncherRow> {
        filter_rows(q, apps, windows, files, recents, Category::All)
    }

    #[test]
    fn filter_matches_name_case_insensitive() {
        let apps = vec![app("Firefox", None)];
        let out = rows("fire", &apps, &[(1, "Terminal".into(), None)], &[], &[]);
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
            &[(7, "Mozilla Firefox".into(), Some("firefox".into()))],
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
            &[(1, "Editor".into(), None)],
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
        let out = filter_rows("", &apps, &[], &[], &[], Category::Apps);
        assert_eq!(out.len(), 30);
    }

    #[test]
    fn files_chip_returns_every_hit() {
        let files: Vec<FileHit> = (0..40)
            .map(|i| FileHit {
                path: format!("/tmp/f{i}").into(),
            })
            .collect();
        let out = filter_rows("f", &[], &[], &files, &[], Category::Files);
        assert_eq!(out.len(), 40);
    }

    #[test]
    fn commands_chip_is_empty() {
        let out = filter_rows("x", &[app("X", None)], &[], &[], &[], Category::Commands);
        assert!(out.is_empty());
    }

    #[test]
    fn row_cap_holds() {
        let apps: Vec<DesktopApp> = (0..30).map(|i| app(&format!("App{i:02}"), None)).collect();
        let out = rows("app", &apps, &[], &[], &[]);
        assert_eq!(out.len(), ROW_CAP);
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
