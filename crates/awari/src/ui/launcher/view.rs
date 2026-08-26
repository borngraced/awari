use gpui::prelude::*;
use gpui::{
    AnimationExt, AnyElement, App, Context, FocusHandle, Focusable, FontWeight, HighlightStyle,
    Image, ImageFormat, InteractiveElement, IntoElement, MouseButton, ObjectFit, ParentElement,
    Pixels, Point, Render, Rgba, ScrollStrategy, SharedString, SpringAnimation, Styled,
    StyledImage, StyledText, UniformListScrollHandle, WeakEntity, Window, div, img, px,
    uniform_list,
};
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use super::scoring::*;
use super::types::*;
use super::{
    AWARI_MARK, GRID_COLS, HEIGHT_SPRING, ICON_GRID, ICON_LIST, ITEM_HOVER_SPRING, LAUNCHER_W,
    PANEL_H, PANEL_SPRING, SEARCH_H, SLIDE,
};
use crate::app::Daemon;
use crate::surfaces::LAUNCHER_NAMESPACE;
use crate::ui::icon::Icon;
use crate::ui::theme::Theme;

fn mix(a: &Rgba, b: &Rgba, t: f32) -> Rgba {
    let t = t.clamp(0.0, 1.0);
    Rgba {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: a.a + (b.a - a.a) * t,
    }
}
#[derive(Clone)]
pub struct LauncherView {
    pub open: bool,
    pub query: String,
    pub selected: usize,
    pub rows: Arc<[LauncherRow]>,
    pub theme: Theme,
    pub category: Category,
    pub files_enabled: bool,
    /// A valid calculator result for the current query, if any. When set the
    /// launcher shows it as an inline ghost (` = <result>`) instead of a list
    /// row, and Enter copies it / Tab accepts it as the new input.
    pub calc: Option<String>,
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
            rows: Arc::new([]),
            theme,
            category: Category::All,
            files_enabled: true,
            calc: None,
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
        let mut c = self.cursor.min(self.view.query.len());
        match key.as_str() {
            "backspace" => {
                if c == 0 {
                    return None;
                }
                let prev = self.view.query[..c]
                    .char_indices()
                    .next_back()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                self.view.query.replace_range(prev..c, "");
                c = prev;
            }
            "delete" => {
                if c >= self.view.query.len() {
                    return None;
                }
                let next = self.view.query[c..]
                    .char_indices()
                    .nth(1)
                    .map(|(i, _)| c + i)
                    .unwrap_or(self.view.query.len());
                self.view.query.replace_range(c..next, "");
            }
            "arrowleft" | "left" => {
                c = self.view.query[..c]
                    .char_indices()
                    .next_back()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
            }
            "arrowright" | "right" => {
                c = self.view.query[c..]
                    .char_indices()
                    .nth(1)
                    .map(|(i, _)| c + i)
                    .unwrap_or(self.view.query.len());
            }
            "home" => c = 0,
            "end" => c = self.view.query.len(),
            _ => {
                let Some(ch) = &k.key_char else { return None };
                if ch.is_empty() || ch.chars().any(|b| b.is_control()) {
                    return None;
                }
                self.view.query.insert_str(c, ch);
                c += ch.len();
            }
        }
        self.cursor = c;
        Some(self.view.query.clone())
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
        // Ghost preview hides once its own accept is on screen. A valid
        // calculator result ghosts as ` = <result>` appended to the typed
        // query (the "what my input becomes" model); otherwise the top row's
        // completion suffix is used.
        let ghost = if token_len == 0 && c == q.len() && accepted_off.is_none() {
            if let Some(result) = &self.view.calc {
                Some(format!(" = {}", result))
            } else {
                self.view.rows.first().and_then(|r| ghost_suffix(q, &r.label))
            }
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

pub(crate) fn icon_letter(app_id: Option<&str>) -> String {
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

fn highlighted_name(label: &str, q: &[char], t: &Theme) -> StyledText {
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
    StyledText::new(label).with_highlights(ranges)
}

pub(crate) fn build_subtitle(kind: &RowKind) -> Option<SharedString> {
    match kind {
        RowKind::File { path } => Some(SharedString::from(path.display().to_string())),
        RowKind::App { exec, .. } => Some(SharedString::from(exec.join(" "))),
        RowKind::Window { .. } => Some(SharedString::from("Window")),
        RowKind::Command { command } => Some(SharedString::from(command.clone())),
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
                let _ = cx
                    .background_executor()
                    .timer(Duration::from_millis(16))
                    .await;
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

    fn list_row(&self, i: usize, t: &Theme, q: &[char], cx: &mut Context<Self>) -> AnyElement {
        let this = cx.entity();
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
                            .child(row.subtitle.clone().unwrap_or_default()),
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
        let show_results = (!q_empty || cat != Category::All) && self.view.calc.is_none();
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
            let q_chars: Vec<char> = self.view.query.trim().to_lowercase().chars().collect();
            results = results.child(
                uniform_list(
                    "launch-list",
                    n,
                    cx.processor(move |this, range: Range<usize>, _, cx| {
                        range
                            .map(|i| this.list_row(i, &t_list, &q_chars, cx))
                            .collect()
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
                .with_spring(
                    "action-menu",
                    SpringAnimation::new(PANEL_SPRING).to(1.0).from(0.0),
                    |el, v| el.opacity(v).mt(px((1.0 - v) * 6.0)),
                )
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
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    post(this, cx, LauncherCmd::Dismiss);
                }),
            )
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
                            if let Some(m) = this.action_menu.as_mut()
                                && !m.actions.is_empty()
                            {
                                m.index = (m.index + 1).min(m.actions.len() - 1);
                            }
                            cx.notify();
                        }
                        "enter" | "return" => {
                            let menu = this.action_menu.take();
                            if let Some(menu) = menu
                                && let Some(row) = this.view.rows.get(this.view.selected)
                            {
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
                    // A valid calculator result: Tab accepts it as the new
                    // input (so the user can keep computing, e.g. `4 * 3`).
                    // Only when the top slot is active — if the user has
                    // arrowed into the list, Tab completes the selected row.
                    if this.view.selected == 0
                        && let Some(result) = this.view.calc.clone()
                    {
                        this.cursor = result.len();
                        this.accepted = None;
                        this.view.query = result.clone();
                        post(this, cx, LauncherCmd::SetQuery { query: result });
                        return;
                    }
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
