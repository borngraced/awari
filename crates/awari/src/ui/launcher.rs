//! Overlay command palette. Full-output scrim; Escape / click outside / Mod+D close.

use gpui::{
    div, img, px, App, Context, FocusHandle, Focusable, InteractiveElement, IntoElement,
    MouseButton, ObjectFit, ParentElement, Render, Styled, StyledImage,
    StatefulInteractiveElement, WeakEntity, Window,
};
use std::path::PathBuf;
use std::time::Instant;

use crate::app::Daemon;
use crate::desktop::DesktopApp;
use crate::files::FileHit;
use crate::surfaces::LAUNCHER_NAMESPACE;
use crate::ui::icon::Icon;
use crate::ui::theme::Theme;

pub const LAUNCHER_W: f32 = 520.0;
/// Window covers the output so clicks outside the palette dismiss it.
pub const LAUNCHER_H: f32 = 1080.0;
const PANEL_TOP: f32 = 96.0;
const ROW_CAP: usize = 10;
/// Max file rows taken from the fff results.
const FILE_ROWS: usize = 8;

/// Commands the overlay may enqueue. Handlers never update Daemon/Launcher
/// synchronously and never destroy this window themselves.
#[derive(Clone)]
pub enum LauncherCmd {
    Dismiss,
    Key { key: String, ch: Option<String> },
    Activate { index: usize },
    /// IPC-open → first rendered frame, in whole milliseconds.
    OpenToRender { ms: u64 },
}

#[derive(Clone)]
pub struct LauncherRow {
    pub kind: RowKind,
    pub label: String,
    /// Icon hint: a `.desktop` `Icon=` value for apps, an app_id for windows.
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
}

impl LauncherView {
    pub fn closed(theme: Theme) -> Self {
        Self {
            open: false,
            query: String::new(),
            selected: 0,
            rows: Vec::new(),
            theme,
        }
    }
}

pub struct Launcher {
    pub shell: WeakEntity<Daemon>,
    view: LauncherView,
    focus: FocusHandle,
    /// Row under the pointer, for hover-revealed kind glyphs.
    hovered: Option<usize>,
    /// Set when the daemon opens the overlay; consumed on first render.
    open_started: Option<Instant>,
}

impl Launcher {
    pub fn new(shell: WeakEntity<Daemon>, theme: Theme, cx: &mut Context<Self>) -> Self {
        Self {
            shell,
            view: LauncherView::closed(theme),
            focus: cx.focus_handle(),
            hovered: None,
            open_started: None,
        }
    }

    pub fn apply_view(&mut self, view: LauncherView) {
        self.view = view;
    }

    pub fn arm_open_timer(&mut self, started: Instant) {
        self.open_started = Some(started);
    }
}

impl Focusable for Launcher {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
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
        keyboard_interactivity: KeyboardInteractivity::Exclusive,
    }
}

pub fn filter_rows(
    query: &str,
    apps: &[DesktopApp],
    windows: &[(u64, String, Option<String>)],
    files: &[FileHit],
    recents: &[String],
) -> Vec<LauncherRow> {
    let q = query.trim().to_lowercase();
    let empty = q.is_empty();

    // Windows: fuzzy-scored titles.
    let mut win_rows: Vec<(i64, LauncherRow)> = windows
        .iter()
        .filter_map(|(id, title, app_id)| {
            let s = if empty {
                1
            } else {
                crate::matchq::score(title, &q)
                    .max(app_id.as_deref().and_then(|a| crate::matchq::score(a, &q)))?
            };
            Some((s, LauncherRow {
                kind: RowKind::Window { id: *id },
                label: title.clone(),
                icon: app_id.clone(),
            }))
        })
        .collect();
    if !empty {
        win_rows.sort_by(|a, b| b.0.cmp(&a.0));
    }

    // Dedup: a running window replaces its app row when identities collide.
    let visible_app_ids: Vec<String> = win_rows
        .iter()
        .filter_map(|(_, r)| r.icon.as_deref().map(|s| s.to_lowercase()))
        .collect();

    let mut app_rows: Vec<(i64, LauncherRow)> = apps
        .iter()
        .filter_map(|app| {
            let ident_hits_window = |probe: Option<&str>| {
                probe
                    .map(|p| visible_app_ids.iter().any(|v| v == p.to_lowercase().as_str()))
                    .unwrap_or(false)
            };
            if ident_hits_window(Some(&app.name)) || ident_hits_window(app.app_id.as_deref()) {
                return None;
            }
            let s = if empty {
                1
            } else {
                let by_name = crate::matchq::score(&app.name, &q);
                let by_id = app.app_id.as_deref().and_then(|a| crate::matchq::score(a, &q));
                match (by_name, by_id) {
                    (Some(a), Some(b)) => Some(a.max(b)),
                    (a, b) => a.or(b),
                }?
            };
            Some((s, LauncherRow {
                kind: RowKind::App {
                    exec: app.exec.clone(),
                },
                label: app.name.clone(),
                icon: app.icon.clone(),
            }))
        })
        .collect();
    if empty {
        // Most recently activated first, then alphabetical.
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
                kind: RowKind::File { path: hit.path.clone() },
                label: hit
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| hit.path.display().to_string()),
                icon: None,
            })
            .collect::<Vec<_>>()
    };

    let mut out: Vec<LauncherRow> = Vec::with_capacity(ROW_CAP);
    fn push_all(out: &mut Vec<LauncherRow>, rows: impl Iterator<Item = LauncherRow>) {
        for r in rows {
            if out.len() >= ROW_CAP {
                return;
            }
            out.push(r);
        }
    }

    if crate::files::is_path_shaped(&q) {
        push_all(&mut out, file_rows(FILE_ROWS).into_iter());
        push_all(&mut out, win_rows.into_iter().map(|(_, r)| r));
        push_all(&mut out, app_rows.into_iter().map(|(_, r)| r));
    } else {
        push_all(&mut out, win_rows.into_iter().map(|(_, r)| r));
        push_all(&mut out, app_rows.into_iter().map(|(_, r)| r));
        if !empty {
            push_all(&mut out, file_rows(FILE_ROWS).into_iter());
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

const ROW_H: f32 = 38.0;
const ICON_TILE: f32 = 22.0;

/// Small keycap chip used in the search row and footer hints.
fn keycap(t: Theme, label: &'static str) -> gpui::Div {
    div()
        .px_1p5()
        .py_0p5()
        .rounded(px(4.))
        .border_1()
        .border_color(t.border())
        .text_color(t.muted())
        .text_xs()
        .child(label)
}

impl Render for Launcher {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.view.open {
            window.set_input_region(Some(&[]));
            return div().id("launcher-root").w_full().h_full();
        }
        window.set_input_region(None);
        self.focus.focus(window, cx);

        // IPC-open → first frame. Consume the timer once; report via daemon.
        if let Some(t0) = self.open_started.take() {
            let ms = u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX);
            post(self, cx, LauncherCmd::OpenToRender { ms });
        }

        let t = self.view.theme;
        let win_w = f32::from(window.bounds().size.width);
        let x = ((win_w - LAUNCHER_W) / 2.0).max(8.0);

        let mut list = div().flex().flex_col();
        if self.view.rows.is_empty() {
            list = list.child(
                div()
                    .px(px(12.))
                    .py(px(10.))
                    .text_color(t.muted())
                    .text_sm()
                    .child("no matches"),
            );
        }
        let mut last_kind_windows: Option<Option<bool>> = None;
        for (i, row) in self.view.rows.iter().enumerate() {
            let kind_windows = match row.kind {
                RowKind::Window { .. } => Some(true),
                RowKind::App { .. } => Some(false),
                RowKind::File { .. } => None,
            };
            if last_kind_windows != Some(kind_windows) {
                let label = match kind_windows {
                    Some(true) => "WINDOWS",
                    Some(false) => "APPLICATIONS",
                    None => "FILES",
                };
                list = list.child(
                    div()
                        .px(px(12.))
                        .pt(px(6.))
                        .pb(px(2.))
                        .text_color(t.faint())
                        .text_xs()
                        .child(label),
                );
                last_kind_windows = Some(kind_windows);
            }

            let selected = i == self.view.selected;
            let hovered = self.hovered == Some(i);
            let letter = icon_letter(Some(&row.label));

            // Icon: resolved freedesktop icon, letter tile as fallback.
            // Files get a plain file glyph instead of a letter tile.
            let icon_slot = match &row.kind {
                RowKind::File { .. } => div()
                    .size(px(ICON_TILE))
                    .flex()
                    .items_center()
                    .justify_center()
                    .flex_none()
                    .rounded(px(5.))
                    .text_color(if selected { t.accent() } else { t.muted() })
                    .child(Icon::File.element(if selected {
                        t.accent()
                    } else {
                        t.muted()
                    })),
                _ => match crate::icons::resolve(row.icon.as_deref().unwrap_or("")) {
                Some(path) => div()
                    .size(px(ICON_TILE))
                    .flex_none()
                    .overflow_hidden()
                    .rounded(px(5.))
                    .child(
                        img(path)
                            .size(px(ICON_TILE))
                            .object_fit(ObjectFit::Contain)
                            .flex_none(),
                    ),
                None => div()
                    .size(px(ICON_TILE))
                    .flex()
                    .items_center()
                    .justify_center()
                    .flex_none()
                    .rounded(px(5.))
                    .bg(t.surface())
                    .text_color(if selected {
                        t.accent()
                    } else {
                        t.muted()
                    })
                    .text_xs()
                    .child(letter),
            }
            };

            // Kind glyph on the right, revealed by hover or selection.
            let kind_icon = match row.kind {
                RowKind::Window { .. } => Icon::AppWindow,
                RowKind::App { .. } => Icon::LayoutGrid,
                RowKind::File { .. } => Icon::File,
            };

            list = list.child(
                div()
                    .id(("launch-row", i))
                    .flex()
                    .items_center()
                    .gap_2()
                    .w_full()
                    .h(px(ROW_H))
                    .px(px(10.))
                    .rounded_md()
                    .border_l_2()
                    .border_color(if selected {
                        t.accent()
                    } else {
                        t.ghost()
                    })
                    .bg(if selected {
                        t.select()
                    } else {
                        t.panel()
                    })
                    .hover(|s| s.bg(t.hover()))
                    .on_hover(cx.listener(move |this, is_in: &bool, _, cx| {
                        this.hovered = if *is_in { Some(i) } else { None };
                        cx.notify();
                    }))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            post(this, cx, LauncherCmd::Activate { index: i });
                        }),
                    )
                    .child(icon_slot)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(t.fg())
                            .text_sm()
                            .child(row.label.clone()),
                    )
                    .child(
                        kind_icon
                            .element(if selected || hovered {
                                t.accent()
                            } else {
                                t.ghost()
                            })
                            .mr_1(),
                    ),
            );
        }

        let q_empty = self.view.query.is_empty();
        let placeholder = "type to filter";

        div()
            .id("launcher-root")
            .track_focus(&self.focus)
            .relative()
            .w_full()
            .h_full()
            .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, _, cx| {
                cx.stop_propagation();
                post(
                    this,
                    cx,
                    LauncherCmd::Key {
                        key: ev.keystroke.key.clone(),
                        ch: ev.keystroke.key_char.clone(),
                    },
                );
            }))
            .child(
                div()
                    .id("launcher-scrim")
                    .absolute()
                    .inset_0()
                    .bg(t.scrim())
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            post(this, cx, LauncherCmd::Dismiss);
                        }),
                    ),
            )
            .child(
                div()
                    .id("launcher-panel")
                    .absolute()
                    .left(px(x))
                    .top(px(PANEL_TOP))
                    .w(px(LAUNCHER_W))
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
                        // Search field.
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_3()
                            .h(px(44.))
                            .bg(t.surface())
                            .border_b_1()
                            .border_color(t.border())
                            .child(Icon::Search.element(t.faint()))
                            .child(if q_empty {
                                div()
                                    .text_color(t.faint())
                                    .text_sm()
                                    .child(placeholder)
                            } else {
                                div().text_color(t.fg()).text_sm().child(
                                    self.view.query.clone(),
                                )
                            })
                            .child(
                                div()
                                    .w(px(2.))
                                    .h(px(15.))
                                    .flex_none()
                                    .rounded(px(1.))
                                    .bg(t.accent()),
                            )
                            .child(div().flex_1())
                            .child(keycap(t, "esc").on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| {
                                    post(this, cx, LauncherCmd::Dismiss);
                                }),
                            )),
                    )
                    .child(div().px(px(4.)).py(px(4.)).child(list))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1p5()
                            .px_3()
                            .h(px(30.))
                            .bg(t.surface())
                            .border_t_1()
                            .border_color(t.border())
                            .text_color(t.faint())
                            .text_xs()
                            .child(keycap(t, "↑"))
                            .child(keycap(t, "↓"))
                            .child("browse")
                            .child(div().mx_0p5())
                            .child(keycap(t, "↵"))
                            .child("open")
                            .child(div().flex_1())
                            .child(keycap(t, "esc"))
                            .child("close"),
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
        }
    }

    #[test]
    fn filter_matches_name_case_insensitive() {
        let apps = vec![app("Firefox", None)];
        let rows = filter_rows("fire", &apps, &[(1, "Terminal".into(), None)], &[], &[]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label, "Firefox");
    }

    #[test]
    fn fuzzy_typo_still_matches() {
        let apps = vec![app("Firefox", None)];
        let rows = filter_rows("firfox", &apps, &[], &[], &[]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label, "Firefox");
    }

    #[test]
    fn running_window_suppresses_app_row() {
        let apps = vec![app("Firefox", Some("firefox"))];
        // Window whose app_id equals the app identity → no duplicate App row.
        let rows = filter_rows(
            "",
            &apps,
            &[(7, "Mozilla Firefox".into(), Some("firefox".into()))],
            &[],
            &[],
        );
        assert_eq!(rows.len(), 1);
        assert!(matches!(rows[0].kind, RowKind::Window { .. }));
    }

    #[test]
    fn empty_query_orders_apps_by_recency() {
        let apps = vec![app("Alpha", None), app("Beta", None), app("Gamma", None)];
        let rows = filter_rows("", &apps, &[], &[], &["Gamma".into()]);
        assert_eq!(rows[0].label, "Gamma");
    }

    #[test]
    fn path_shaped_query_puts_files_first() {
        let files = vec![FileHit { path: "/tmp/notes.md".into() }];
        let rows = filter_rows(
            "~/not",
            &[app("Notes", None)],
            &[(1, "Editor".into(), None)],
            &files,
            &[],
        );
        assert!(matches!(rows[0].kind, RowKind::File { .. }));
    }

    #[test]
    fn empty_query_never_dumps_files() {
        let files = vec![FileHit { path: "/tmp/x".into() }];
        let rows = filter_rows("", &[app("Zed", None)], &[], &files, &[]);
        assert!(rows.iter().all(|r| !matches!(r.kind, RowKind::File { .. })));
    }

    #[test]
    fn row_cap_holds() {
        let apps: Vec<DesktopApp> = (0..30)
            .map(|i| app(&format!("App{i:02}"), None))
            .collect();
        let rows = filter_rows("app", &apps, &[], &[], &[]);
        assert_eq!(rows.len(), ROW_CAP);
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
