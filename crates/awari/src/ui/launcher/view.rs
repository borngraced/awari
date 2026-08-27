use gpui::prelude::*;
use gpui::{
    AnimationExt, AnyElement, App, Context, FocusHandle, Focusable, FontWeight, HighlightStyle,
    InteractiveElement, IntoElement, MouseButton, ObjectFit, ParentElement, Pixels, Render,
    ScrollStrategy, SharedString, SpringAnimation, Styled, StyledImage, StyledText, Task,
    UniformListScrollHandle, WeakEntity, Window, div, img, px, uniform_list,
};
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use super::scoring::*;
use super::types::*;
use super::{
    GRID_COLS, GRID_ROW_H, ICON_GRID, ICON_LIST, LAUNCHER_W, NO_MATCH_H,
    RESULTS_DEBOUNCE, ROW_H, SCALE_MIN, SEARCH_H, SOURCE_LIST_H, motion_spring,
};
use crate::app::Daemon;
use crate::surfaces::LAUNCHER_NAMESPACE;
use crate::ui::icon::Icon;
use crate::ui::theme::Theme;

#[derive(Clone)]
pub struct LauncherView {
    pub open: bool,
    pub query: String,
    pub selected: usize,
    pub rows: Arc<[LauncherRow]>,
    pub theme: Theme,
    pub category: Category,
    pub files_enabled: bool,
    pub windows_enabled: bool,
    /// A valid calculator result for the current query, if any. When set the
    /// launcher shows it as an inline ghost (` = <result>`) instead of a list
    /// row, and Enter copies it / Tab accepts it as the new input.
    pub calc: Option<String>,
    /// Panel position offset from default center-top position.
    pub panel_offset_x: f32,
    pub panel_offset_y: f32,
    /// Open/close and height spring settle time (`motion.duration-ms`).
    pub motion_ms: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RevealAction {
    Collapse,
    Debounce,
    ShowNow,
    Hold,
}

pub(crate) fn wants_results(query: &str, category: Category, has_calc: bool) -> bool {
    !has_calc && (!query.trim().is_empty() || category != Category::All)
}

/// The empty-state source menu (Apps / Files / Windows) is visible only at the
/// top level with no query and no calculator result. Visibility tracks the
/// query instantly: shown the moment it empties, hidden the instant a char is
/// typed — there is no debounce on the hide path.
pub(crate) fn source_menu_visible(query_empty: bool, category: Category, has_calc: bool) -> bool {
    query_empty && category == Category::All && !has_calc
}

/// Results stay hidden while the query is changing. A pause reveals them
/// (and grows the panel) once. Category clicks show immediately.
pub(crate) fn reveal_action(
    wants: bool,
    daemon_query: bool,
    category_changed: bool,
) -> RevealAction {
    if !wants {
        RevealAction::Collapse
    } else if daemon_query {
        RevealAction::Debounce
    } else if category_changed {
        RevealAction::ShowNow
    } else {
        RevealAction::Hold
    }
}

/// A snapshot whose query does not match the live buffer is an in-flight
/// echo from an earlier keystroke and must not rewind the input.
pub(crate) fn stale_query_snapshot(local_dirty: bool, local_q: &str, snap_q: &str) -> bool {
    local_dirty && local_q != snap_q
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
            windows_enabled: true,
            calc: None,
            panel_offset_x: 0.0,
            panel_offset_y: 0.0,
            motion_ms: 140,
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
    open_started: Option<Instant>,
    pub(crate) closing: bool,
    /// Hovered source-list row (empty-state menu), tracked separately from the
    /// keyboard selection so mouse hover can highlight without stealing focus.
    hovered_source: Option<usize>,
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
    /// Drag state: (origin_x, origin_y) in screen coords when drag started.
    dragging: Option<(f32, f32)>,
    /// View-side panel position offset (avoids IPC round-trip on every mouse move).
    panel_offset_x: f32,
    panel_offset_y: f32,
    /// Results (and panel growth) are shown only after typing pauses, or
    /// immediately on a category click.
    fit_expanded: bool,
    /// Task that reveals results after `RESULTS_DEBOUNCE` of quiet typing.
    height_debounce: Option<Task<()>>,
    /// Invalidates in-flight debounce tasks. Dropping the Task does not
    /// cancel the executor timer, so a generation check is required.
    reveal_gen: u64,
    /// True after a local edit until a daemon snapshot with the same query
    /// arrives. Stale snapshots must not overwrite the input buffer.
    local_query_dirty: bool,
    /// Bumped each open so open/close springs remount from rest instead of
    /// continuing a previous close's height.
    open_gen: u64,
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
            open_started: None,
            closing: false,
            hovered_source: None,
            last_select: None,
            last_input_open: None,
            focused: false,
            blink_gen: Arc::new(AtomicU64::new(0)),
            blink_running: false,
            action_menu: None,
            accepted: None,
            dragging: None,
            panel_offset_x: 0.0,
            panel_offset_y: 0.0,
            fit_expanded: false,
            height_debounce: None,
            reveal_gen: 0,
            local_query_dirty: false,
            open_gen: 0,
        }
    }

    pub fn apply_view(&mut self, incoming: LauncherView, cx: &mut Context<Self>) {
        let was_open = self.view.open;
        let opening = incoming.open && !was_open;

        if opening {
            self.cursor = 0;
            self.local_query_dirty = false;
            self.accepted = None;
            self.view = incoming;
            self.last_select = Some(self.view.selected);
            self.open_gen = self.open_gen.wrapping_add(1);
            self.hide_results();
            self.panel_offset_x = self.view.panel_offset_x;
            self.panel_offset_y = self.view.panel_offset_y;
            self.closing = false;
            self.ensure_blink(cx);
            cx.notify();
            return;
        }

        if stale_query_snapshot(self.local_query_dirty, &self.view.query, &incoming.query) {
            self.view.open = incoming.open;
            self.view.theme = incoming.theme;
            self.view.files_enabled = incoming.files_enabled;
            self.view.windows_enabled = incoming.windows_enabled;
            self.view.panel_offset_x = incoming.panel_offset_x;
            self.view.panel_offset_y = incoming.panel_offset_y;
            self.view.motion_ms = incoming.motion_ms;
            self.apply_open_flags(incoming.open, was_open, cx);
            cx.notify();
            return;
        }

        let daemon_query = incoming.query != self.view.query;
        let category_changed = incoming.category != self.view.category;

        if daemon_query {
            self.view.query = incoming.query;
            self.cursor = self.view.query.len();
            self.accepted = None;
            self.local_query_dirty = false;
            self.action_menu = None;
        } else {
            self.local_query_dirty = false;
        }

        if category_changed {
            self.action_menu = None;
        } else if self.view.selected != incoming.selected {
            self.action_menu = None;
        }

        self.view.open = incoming.open;
        self.view.rows = incoming.rows;
        self.view.selected = incoming.selected;
        self.view.theme = incoming.theme;
        self.view.category = incoming.category;
        self.view.files_enabled = incoming.files_enabled;
        self.view.windows_enabled = incoming.windows_enabled;
        self.view.calc = incoming.calc;
        self.view.panel_offset_x = incoming.panel_offset_x;
        self.view.panel_offset_y = incoming.panel_offset_y;
        self.view.motion_ms = incoming.motion_ms;
        self.last_select = Some(self.view.selected);

        // Keep the highlighted row in view. `scroll_to_item` is the uniform
        // list's own deferred-scroll primitive: it no-ops when the row is
        // already visible and otherwise glides it into view, without the
        // hand-rolled offset/spring bookkeeping that could drift out of sync
        // with the actual laid-out items (e.g. after a file-results refresh
        // re-lays out the list).
        if self.view.open && !self.view.rows.is_empty() {
            let browsing_grid =
                self.view.category == Category::Apps && self.view.query.trim().is_empty();
            let ix = if browsing_grid {
                self.view.selected / GRID_COLS
            } else {
                self.view.selected
            };
            self.scroll.scroll_to_item(ix, ScrollStrategy::Nearest);
        }

        let wants = wants_results(
            &self.view.query,
            self.view.category,
            self.view.calc.is_some(),
        );
        match reveal_action(wants, daemon_query, category_changed) {
            RevealAction::Collapse => self.hide_results(),
            RevealAction::Debounce => self.schedule_results_reveal(cx),
            RevealAction::ShowNow => self.show_results_now(),
            RevealAction::Hold => {
                if wants && self.view.open && self.height_debounce.is_none() {
                    self.schedule_results_reveal(cx);
                }
            }
        }

        self.apply_open_flags(self.view.open, was_open, cx);
        cx.notify();
    }

    fn apply_open_flags(&mut self, open: bool, was_open: bool, cx: &mut Context<Self>) {
        if open {
            self.closing = false;
            if !was_open {
                self.panel_offset_x = self.view.panel_offset_x;
                self.panel_offset_y = self.view.panel_offset_y;
            }
            self.ensure_blink(cx);
        } else {
            self.cursor = 0;
            self.caret_on = true;
            self.stop_blink();
        }
    }

    /// Hide results and, after a typing pause, reveal them at the fitted height.
    /// Called from the key handler because the query is applied locally before
    /// the daemon round-trip, so `apply_view` often sees no query change.
    fn on_query_typed(&mut self, cx: &mut Context<Self>) {
        self.local_query_dirty = true;
        self.view.calc = crate::math::evaluate(&self.view.query);
        if wants_results(
            &self.view.query,
            self.view.category,
            self.view.calc.is_some(),
        ) {
            self.schedule_results_reveal(cx);
        } else {
            self.hide_results();
        }
    }

    fn hide_results(&mut self) {
        self.reveal_gen = self.reveal_gen.wrapping_add(1);
        self.height_debounce.take();
        self.fit_expanded = false;
    }

    fn show_results_now(&mut self) {
        self.reveal_gen = self.reveal_gen.wrapping_add(1);
        self.height_debounce.take();
        self.fit_expanded = true;
    }

    fn schedule_results_reveal(&mut self, cx: &mut Context<Self>) {
        // Re-arm the reveal timer without collapsing the panel. The height must
        // stay fixed while the query is changing; the visible row count is a
        // fixed capacity derived from `max_panel_h`, so it never needs to grow
        // here — only the live result count (clamped to that capacity) matters.
        self.reveal_gen = self.reveal_gen.wrapping_add(1);
        self.height_debounce.take();
        let ticket = self.reveal_gen;
        let debounce = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(RESULTS_DEBOUNCE).await;
            let _ = this.update(cx, |l, cx| {
                if l.reveal_gen != ticket {
                    return;
                }
                if wants_results(&l.view.query, l.view.category, l.view.calc.is_some()) {
                    l.fit_expanded = true;
                }
                cx.notify();
            });
        });
        self.height_debounce = Some(debounce);
    }

    /// Begin the close animation without discarding the current content.
    ///
    /// Flipping only `view.open` makes the panel/height springs retarget to
    /// their closed state, so the overlay fades and slides out, while the
    /// query, rows, theme and category stay intact through the fade. The old
    /// path replaced the whole view with `closed`, blanking the panel and
    /// collapsing its height on the first frame so nothing visibly animated.
    pub(crate) fn begin_close(&mut self, cx: &mut Context<Self>) {
        self.view.open = false;
        self.closing = true;
        self.action_menu = None;
        self.accepted = None;
        self.stop_blink();
        cx.notify();
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
                self.cursor = c;
                return None;
            }
            "arrowright" | "right" => {
                c = self.view.query[c..]
                    .char_indices()
                    .nth(1)
                    .map(|(i, _)| c + i)
                    .unwrap_or(self.view.query.len());
                self.cursor = c;
                return None;
            }
            "home" => {
                self.cursor = 0;
                return None;
            }
            "end" => {
                self.cursor = self.view.query.len();
                return None;
            }
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
                .child(div().text_color(t.muted()).child("Awari search"))
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
            } else if self.fit_expanded {
                self.view
                    .rows
                    .first()
                    .and_then(|r| ghost_suffix(q, &r.label))
            } else {
                None
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

fn first_result_icon(row: &LauncherRow, t: &Theme) -> gpui::Div {
    let size = 22.0;
    let tile = div()
        .size(px(size))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.))
        .bg(t.surface());

    match &row.kind {
        RowKind::File { .. } => tile.child(Icon::File.element_px(t.muted(), size * 0.58)),
        _ => match &row.resolved_icon {
            Some(path) => tile.overflow_hidden().child(
                img(path.clone())
                    .size(px(size))
                    .object_fit(ObjectFit::Contain)
                    .flex_none(),
            ),
            None => tile
                .text_color(t.muted())
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

    fn tile(&self, i: usize, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let Some(row) = self.view.rows.get(i) else {
            return div()
                .id(("launch-tile-empty", i))
                .flex_1()
                .min_w_0()
                .flex_none()
                .into_any_element();
        };
        let selected = i == self.view.selected;
        let base = if selected { t.select() } else { t.ghost() };

        div()
            .id(("launch-tile", i))
            .flex()
            .flex_col()
            .items_center()
            .gap_3()
            .h(px(GRID_ROW_H))
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .py(px(5.))
            .px(px(8.))
            .rounded(px(10.))
            .bg(base)
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
            .into_any_element()
    }

    fn list_row(&self, i: usize, t: &Theme, q: &[char], cx: &mut Context<Self>) -> AnyElement {
        let Some(row) = self.view.rows.get(i) else {
            return div().id(("launch-row-empty", i)).into_any_element();
        };
        let selected = i == self.view.selected;
        let base = if selected { t.select() } else { t.ghost() };

        div()
            .id(("launch-row", i))
            .flex()
            .items_center()
            .gap(px(14.))
            .w_full()
            .min_w_0()
            .h(px(ROW_H))
            .overflow_hidden()
            .px(px(12.))
            .py(px(11.))
            .rounded(px(9.))
            .bg(base)
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
            .into_any_element()
    }

    /// Empty-state menu: a fixed, overlay-style list of the three sources
    /// (Apps / Files / Windows). Shown only when the query is empty and we are
    /// at the top level (`Category::All`). Highlights the keyboard-selected
    /// row (`view.selected`) or the mouse-hovered row; clicking a row enters
    /// that category's browse view.
    fn source_list_el(&self, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.entity();
        let cats = [Category::Apps, Category::Files, Category::Windows];
        let subtitles = [
            "Browse installed apps",
            "Recent and frequent",
            "Currently open windows",
        ];
        let sel = self.view.selected;
        let hover = self.hovered_source;
        div().id("source-list").flex_col().gap(px(2.)).children(
            cats.iter()
                .enumerate()
                .map(|(i, &c)| {
                    let this = this.clone();
                    let selected = sel == i || hover == Some(i);
                    div()
                        .id(("source-row", i as u64))
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .h(px(38.))
                        .px(px(10.))
                        .py(px(10.))
                        .rounded(px(8.))
                        .when(selected, |el| el.bg(t.ghost()))
                        .on_hover(move |h: &bool, _window, cx: &mut App| {
                            this.update(cx, |l, cx| {
                                if *h {
                                    l.hovered_source = Some(i);
                                } else if l.hovered_source == Some(i) {
                                    l.hovered_source = None;
                                }
                                cx.notify();
                            });
                        })
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, _, cx| {
                                post(this, cx, LauncherCmd::SetCategory { category: c });
                            }),
                        )
                        .child(
                            div()
                                .w(px(18.))
                                .flex_none()
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(c.icon().element_px(t.muted(), 17.0)),
                        )
                        .child(div().text_size(px(14.)).text_color(t.fg()).child(c.label()))
                        .child(div().flex_1())
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(t.muted())
                                .child(subtitles[i]),
                        )
                })
                .collect::<Vec<_>>(),
        )
    }
}

impl Render for Launcher {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let animating = self.view.open || self.closing;
        let open = self.view.open;

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
        if let Some(size) = t.font_size {
            window.set_rem_size(px(size as f32));
        }
        let font_family = t.font.clone();
        let win_w = f32::from(window.bounds().size.width);
        let panel_w = LAUNCHER_W.min(win_w * 0.92).max(280.0);
        let q_empty = self.view.query.trim().is_empty();
        let cat = self.view.category;
        let browsing_grid = cat == Category::Apps && q_empty;
        // Empty-state source menu: only at the top level with no query. It is
        // an overlay below the search bar and must never resize/reposition the
        // input box or the window — visibility tracks the query instantly
        // (shown the moment it empties, hidden the instant a char is typed).
        let source_list = source_menu_visible(q_empty, cat, self.view.calc.is_some());
        let expanded = self.fit_expanded;
        let max_panel_h: f32 = 500.0;
        let search_bar_h: f32 = SEARCH_H;
        let chips_h: f32 = if expanded { 36.0 } else { 0.0 };
        // launch-results contributes p(8)+p(8)+mb(8) = 24px of vertical space
        // around the list that the panel height must include.
        let results_pad: f32 = 24.0;

        // Visible row count is a fixed *capacity* derived from the panel height,
        // not a snapshot of the current result count. This is what prevents both
        // failure modes: too few rows (capacity smaller than the live count →
        // forced scroll) and the empty gap (capacity larger than the live count
        // → panel reserves space for rows that don't exist). The live count only
        // ever *reduces* the visible rows below capacity, never grows them.
        //
        // The list viewport is sized to EXACTLY `fit_rows * ROW_H` (no extra
        // breath): with one result the panel hugs that single row, and at
        // capacity the last row's bottom aligns with the viewport bottom so it
        // is never clipped. The trailing padding around the list is the
        // `results_pad` already accounted for in `panel_h`.
        let list_area = max_panel_h - search_bar_h - chips_h - results_pad;
        let list_capacity = (list_area / ROW_H).max(1.0) as usize;
        let grid_capacity_items =
            ((list_area / GRID_ROW_H).max(1.0) as usize * GRID_COLS).min(self.view.rows.len());
        let fit_rows = if browsing_grid {
            grid_capacity_items
        } else {
            list_capacity.min(self.view.rows.len())
        };
        let results_h = if source_list {
            SOURCE_LIST_H
        } else if browsing_grid {
            let grid_rows = fit_rows.div_ceil(GRID_COLS);
            (grid_rows as f32 * GRID_ROW_H).min(list_area)
        } else if fit_rows == 0 {
            NO_MATCH_H
        } else {
            (fit_rows as f32 * ROW_H).min(list_area)
        };
        let panel_h = if source_list {
            // Fixed height for the overlay menu; the input box stays put.
            // The +2 absorbs the panel's 1px borders so nothing clips.
            (search_bar_h + 6.0 + results_h + 6.0 + 2.0).clamp(search_bar_h, max_panel_h)
        } else if expanded {
            (search_bar_h + chips_h + results_pad + results_h).clamp(search_bar_h, max_panel_h)
        } else {
            search_bar_h
        };
        let motion = motion_spring(self.view.motion_ms);
        let open_gen = self.open_gen;

        let mut results = div()
            .id("launch-results")
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .min_h_0();
        if source_list {
            results = results
                .p(px(6.))
                .mb(px(6.))
                .w_full()
                .child(self.source_list_el(&t, cx));
        } else {
            results = results.p(px(8.)).mb(px(8.)).w_full();
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
                                        div().flex().flex_row().gap(px(10.)).w_full();
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
                let list_h = (fit_rows as f32 * ROW_H).min(list_area);

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
                    .h(px(list_h)),
                );
            }
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
                    SpringAnimation::new(motion).to(1.0).from(0.0),
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
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _, _, cx| cx.stop_propagation()),
            )
            .child(results)
            .when_some(action_menu_el, |el, menu| el.child(menu));

        let search_focus = self.focus_handle.clone();
        let offset_x = self.panel_offset_x;
        let offset_y = self.panel_offset_y;
        div()
            .id("launcher-root")
            .track_focus(&search_focus)
            .relative()
            .flex()
            .justify_center()
            .w_full()
            .h_full()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if this.dragging.is_none() {
                        post(this, cx, LauncherCmd::Dismiss);
                    }
                }),
            )
            .on_mouse_move(cx.listener(|this, ev: &gpui::MouseMoveEvent, _, cx| {
                if let Some((ox, oy)) = this.dragging.take() {
                    let x: f32 = ev.position.x.into();
                    let y: f32 = ev.position.y.into();
                    this.panel_offset_x += x - ox;
                    this.panel_offset_y += y - oy;
                    this.dragging = Some((x, y));
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if this.dragging.take().is_some() {
                        post(
                            this,
                            cx,
                            LauncherCmd::SavePosition {
                                x: this.panel_offset_x,
                                y: this.panel_offset_y,
                            },
                        );
                    }
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if this.dragging.take().is_some() {
                        post(
                            this,
                            cx,
                            LauncherCmd::SavePosition {
                                x: this.panel_offset_x,
                                y: this.panel_offset_y,
                            },
                        );
                    }
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
                        this.on_query_typed(cx);
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
                            this.on_query_typed(cx);
                            post(this, cx, LauncherCmd::SetQuery { query: completed });
                        }
                        Some(TabOutcome::Row(completion)) => {
                            this.cursor = completion.len();
                            this.accepted = None;
                            this.view.query = completion.clone();
                            this.on_query_typed(cx);
                            post(this, cx, LauncherCmd::SetQuery { query: completion });
                        }
                        None => {}
                    }
                    return;
                }
                if let Some(query) = this.edit(&ev.keystroke) {
                    cx.stop_propagation();
                    this.on_query_typed(cx);
                    post(this, cx, LauncherCmd::SetQuery { query });
                }
                cx.notify();
            }))
            .child(
                div()
                    .id("launcher-panel-wrap")
                    .relative()
                    .flex_none()
                    .mt(px(80.0 + offset_y))
                    .ml(px(offset_x))
                    .with_spring(
                        ("launcher-panel-h", open_gen),
                        SpringAnimation::new(motion).to(panel_h).from(SEARCH_H),
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
                                    .id("search-bar")
                                    .flex()
                                    .flex_none()
                                    .items_center()
                                    .gap(px(10.))
                                    .px(px(16.))
                                    .py(px(12.))
                                    .cursor_grab()
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|this, ev: &gpui::MouseDownEvent, _, cx| {
                                            cx.stop_propagation();
                                            let x: f32 = ev.position.x.into();
                                            let y: f32 = ev.position.y.into();
                                            this.dragging = Some((x, y));
                                            cx.notify();
                                        }),
                                    )
                                    .on_mouse_up(
                                        MouseButton::Left,
                                        cx.listener(|this, _, _, cx| {
                                            if this.dragging.take().is_some() {
                                                post(
                                                    this,
                                                    cx,
                                                    LauncherCmd::SavePosition {
                                                        x: this.panel_offset_x,
                                                        y: this.panel_offset_y,
                                                    },
                                                );
                                            }
                                        }),
                                    )
                                    .on_mouse_up_out(
                                        MouseButton::Left,
                                        cx.listener(|this, _, _, cx| {
                                            if this.dragging.take().is_some() {
                                                post(
                                                    this,
                                                    cx,
                                                    LauncherCmd::SavePosition {
                                                        x: this.panel_offset_x,
                                                        y: this.panel_offset_y,
                                                    },
                                                );
                                            }
                                        }),
                                    )
                                    .child(Icon::Search.element_px(t.muted(), 25.0))
                                    .child(
                                        div()
                                            .id("query-wrap")
                                            .flex_1()
                                            .min_w_0()
                                            .flex()
                                            .flex_row()
                                            .items_center()
                                            .when_some(t.font.clone(), |el, f| el.font_family(f))
                                            .text_size(px(24.))
                                            .line_height(px(24.))
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(t.fg())
                                            .overflow_hidden()
                                            .child(self.query_element())
                                            .child(div().flex_1().min_w_0())
                                            .when(expanded, |el| {
                                                el.when_some(self.view.rows.first(), |el, row| {
                                                    el.child(first_result_icon(row, &t))
                                                })
                                            }),
                                    ),
                            )
                            .when(expanded || source_list, |el| el.child(results_body))
                            .with_spring(
                                ("launcher-panel", open_gen),
                                SpringAnimation::new(motion).to(target).from(0.0),
                                move |el, p| {
                                    el.opacity(p)
                                        .scale(SCALE_MIN + p * (1.0 - SCALE_MIN))
                                        .mt(px((p - 1.0) * 12.0))
                                },
                            ),
                    ),
            )
    }
}
