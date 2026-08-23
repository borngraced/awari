//! Overlay command palette. Full-output scrim; Escape / click outside / Mod+D close.

use gpui::{
    div, img, px, App, Context, FocusHandle, Focusable, InteractiveElement, IntoElement,
    MouseButton, ObjectFit, ParentElement, Render, Styled, StyledImage,
    StatefulInteractiveElement, WeakEntity, Window,
};

use crate::app::Daemon;
use crate::config::Config;
use crate::desktop::DesktopApp;
use crate::surfaces::LAUNCHER_NAMESPACE;
use crate::ui::icon::Icon;
use crate::ui::theme::Theme;

pub const LAUNCHER_W: f32 = 520.0;
/// Window covers the output so clicks outside the palette dismiss it.
pub const LAUNCHER_H: f32 = 1080.0;
const PANEL_TOP: f32 = 96.0;
const ROW_CAP: usize = 10;

/// Commands the overlay may enqueue. Handlers never update Daemon/Launcher
/// synchronously and never destroy this window themselves.
#[derive(Clone)]
pub enum LauncherCmd {
    Dismiss,
    Key { key: String, ch: Option<String> },
    Activate { index: usize },
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
}

#[derive(Clone)]
pub struct LauncherView {
    pub open: bool,
    pub query: String,
    pub selected: usize,
    pub rows: Vec<LauncherRow>,
    #[allow(dead_code)]
    pub cfg: Config,
}

impl LauncherView {
    pub fn closed(cfg: Config) -> Self {
        Self {
            open: false,
            query: String::new(),
            selected: 0,
            rows: Vec::new(),
            cfg,
        }
    }
}

pub struct Launcher {
    pub shell: WeakEntity<Daemon>,
    view: LauncherView,
    focus: FocusHandle,
    /// Row under the pointer, for hover-revealed kind glyphs.
    hovered: Option<usize>,
}

impl Launcher {
    pub fn new(shell: WeakEntity<Daemon>, cfg: Config, cx: &mut Context<Self>) -> Self {
        Self {
            shell,
            view: LauncherView::closed(cfg),
            focus: cx.focus_handle(),
            hovered: None,
        }
    }

    pub fn apply_view(&mut self, view: LauncherView) {
        self.view = view;
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

pub fn layer_opts(_cfg: Config) -> gpui::layer_shell::LayerShellOptions {
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
) -> Vec<LauncherRow> {
    let q = query.to_lowercase();
    let mut rows = Vec::new();
    for (id, title, app_id) in windows {
        if q.is_empty() || title.to_lowercase().contains(&q) {
            rows.push(LauncherRow {
                kind: RowKind::Window { id: *id },
                label: title.clone(),
                icon: app_id.clone(),
            });
        }
    }
    for app in apps {
        if q.is_empty() || app.name.to_lowercase().contains(&q) {
            rows.push(LauncherRow {
                kind: RowKind::App {
                    exec: app.exec.clone(),
                },
                label: app.name.clone(),
                icon: app.icon.clone(),
            });
        }
    }
    rows.truncate(ROW_CAP);
    rows
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

        let t = self.view.cfg.theme;
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
        let mut last_kind_windows: Option<bool> = None;
        for (i, row) in self.view.rows.iter().enumerate() {
            // Group header when the row kind changes (windows first, then apps).
            let kind_windows = matches!(row.kind, RowKind::Window { .. });
            if last_kind_windows != Some(kind_windows) {
                list = list.child(
                    div()
                        .px(px(12.))
                        .pt(px(6.))
                        .pb(px(2.))
                        .text_color(t.muted())
                        .text_xs()
                        .child(if kind_windows { "WINDOWS" } else { "APPLICATIONS" }),
                );
                last_kind_windows = Some(kind_windows);
            }

            let selected = i == self.view.selected;
            let hovered = self.hovered == Some(i);
            let letter = icon_letter(Some(&row.label));

            // Icon: resolved freedesktop icon, letter tile as fallback.
            let icon_slot = match crate::icons::resolve(row.icon.as_deref().unwrap_or("")) {
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
            };

            // Kind glyph on the right, revealed by hover or selection.
            let kind_icon = match row.kind {
                RowKind::Window { .. } => Icon::AppWindow,
                RowKind::App { .. } => Icon::LayoutGrid,
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

    #[test]
    fn filter_matches_name_case_insensitive() {
        let apps = vec![DesktopApp {
            name: "Firefox".into(),
            exec: vec!["firefox".into()],
            app_id: None,
            icon: Some("firefox".into()),
        }];
        let rows = filter_rows("fire", &apps, &[(1, "Terminal".into(), None)]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label, "Firefox");
    }

    #[test]
    fn launcher_exclusive_zone_is_zero() {
        let opts = layer_opts(crate::config::Config::default());
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
