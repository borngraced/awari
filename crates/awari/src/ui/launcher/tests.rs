use super::*;
use crate::desktop::DesktopApp;
use crate::files::FileHit;
use crate::surfaces::LAUNCHER_NAMESPACE;
use crate::ui::launcher::view::icon_letter;
use std::path::PathBuf;
use std::sync::Arc;

fn source_menu_visible(q_empty: bool, cat: Category, calc: bool) -> bool {
    q_empty && cat == Category::All && !calc
}

fn app(name: &str, app_id: Option<&str>) -> DesktopApp {
    DesktopApp {
        name: name.into(),
        exec: Arc::from(vec![name.to_lowercase()]),
        app_id: app_id.map(Into::into),
        icon: None,
        name_lc: name.to_lowercase(),
        app_id_lc: app_id.map(|s| s.to_lowercase()),
    }
}

#[cfg(test)]
fn filter_rows(params: FilterParams) -> Vec<LauncherRow> {
    let prefix = command_prefix(params.query);
    let calc = crate::math::evaluate(params.query);
    filter_rows_cached(FilterParams {
        prefix,
        calc,
        ..params
    })
}

fn rows(
    q: &str,
    apps: &[DesktopApp],
    windows: &[WindowEntry],
    files: &[FileHit],
    recents: &[String],
) -> Vec<LauncherRow> {
    filter_rows(FilterParams {
        query: q,
        apps,
        windows,
        files,
        recents,
        app_usage: &Default::default(),
        app_icons: &Default::default(),
        category: Category::All,
        file_max: 50,
        total_max: 30,
        cached_app_rows: None,
        cached_win_rows: None,
        prefix: None,
        calc: None,
    })
}

#[test]
fn filter_matches_name_case_insensitive() {
    let apps = vec![app("Firefox", None)];
    let out = rows(
        "fire",
        &apps,
        &[WindowEntry {
            id: 1,
            title: "Terminal".into(),
            app_id: None,
            app_id_lc: None,
        }],
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
fn running_app_and_window_both_show() {
    let apps = vec![app("Firefox", Some("firefox"))];
    let out = rows(
        "",
        &apps,
        &[WindowEntry {
            id: 7,
            title: "Mozilla Firefox".into(),
            app_id: Some("firefox".into()),
            app_id_lc: Some("firefox".into()),
        }],
        &[],
        &[],
    );
    assert!(out.len() >= 2, "app and window both appear, got {}", out.len());
    assert!(matches!(out[0].kind, RowKind::App { .. }), "app is first");
    assert!(
        out.iter().any(|r| matches!(r.kind, RowKind::Window { .. })),
        "window also appears"
    );
}

#[test]
fn app_and_window_survive_file_cap_pressure() {
    let apps = vec![app("Alacritty", Some("alacritty"))];
    let files: Vec<FileHit> = (0..50)
        .map(|i| FileHit {
            path: Arc::from(PathBuf::from(format!("/cfg/alacritty{i}.toml"))),
        })
        .collect();
    let out = rows(
        "alacritty",
        &apps,
        &[WindowEntry {
            id: 7,
            title: "Alacritty — Terminal".into(),
            app_id: Some("alacritty".into()),
            app_id_lc: Some("alacritty".into()),
        }],
        &files,
        &[],
    );
    assert!(matches!(out[0].kind, RowKind::App { .. }), "app is first");
    assert!(
        out.iter().any(|r| matches!(r.kind, RowKind::Window { .. })),
        "window survives file pressure"
    );
    let window_idx = out
        .iter()
        .position(|r| matches!(r.kind, RowKind::Window { .. }))
        .unwrap();
    if let Some(fi) = out.iter().position(|r| matches!(r.kind, RowKind::File { .. })) {
        assert!(window_idx < fi, "window precedes files");
    }
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
        path: Arc::from(PathBuf::from("/tmp/notes.md")),
    }];
    let out = rows(
        "~/not",
        &[app("Notes", None)],
        &[WindowEntry {
            id: 1,
            title: "Editor".into(),
            app_id: None,
            app_id_lc: None,
        }],
        &files,
        &[],
    );
    assert!(matches!(out[0].kind, RowKind::File { .. }));
}

#[test]
fn empty_query_never_dumps_files() {
    let files = vec![FileHit {
        path: Arc::from(PathBuf::from("/tmp/x")),
    }];
    let out = rows("", &[app("Zed", None)], &[], &files, &[]);
    assert!(out.iter().all(|r| !matches!(r.kind, RowKind::File { .. })));
}

#[test]
fn apps_chip_empty_is_uncapped() {
    let apps: Vec<DesktopApp> = (0..30).map(|i| app(&format!("App{i:02}"), None)).collect();
    let out = filter_rows(FilterParams {
        query: "",
        apps: &apps,
        windows: &[],
        files: &[],
        recents: &[],
        app_usage: &Default::default(),
        app_icons: &Default::default(),
        category: Category::Apps,
        file_max: 50,
        total_max: 30,
        cached_app_rows: None,
        cached_win_rows: None,
        prefix: None,
        calc: None,
    });
    assert_eq!(out.len(), 30);
}

#[test]
fn files_chip_returns_every_hit() {
    let files: Vec<FileHit> = (0..40)
        .map(|i| FileHit {
            path: Arc::from(PathBuf::from(format!("/tmp/f{i}"))),
        })
        .collect();
    let out = filter_rows(FilterParams {
        query: "f",
        apps: &[],
        windows: &[],
        files: &files,
        recents: &[],
        app_usage: &Default::default(),
        app_icons: &Default::default(),
        category: Category::Files,
        file_max: 50,
        total_max: 30,
        cached_app_rows: None,
        cached_win_rows: None,
        prefix: None,
        calc: None,
    });
    assert_eq!(out.len(), 40);
}

#[test]
fn files_empty_query_shows_browse() {
    let files: Vec<FileHit> = (0..12)
        .map(|i| FileHit {
            path: Arc::from(PathBuf::from(format!("/tmp/f{i}"))),
        })
        .collect();
    // Empty query in the Files category is a frecency browse: every hit is
    // shown (capped by file_max), not gated behind a typed filter.
    let out = filter_rows(FilterParams {
        query: "",
        apps: &[],
        windows: &[],
        files: &files,
        recents: &[],
        app_usage: &Default::default(),
        app_icons: &Default::default(),
        category: Category::Files,
        file_max: 50,
        total_max: 30,
        cached_app_rows: None,
        cached_win_rows: None,
        prefix: None,
        calc: None,
    });
    assert_eq!(out.len(), 12);
    assert!(out.iter().all(|r| matches!(r.kind, RowKind::File { .. })));
}

#[test]
fn commands_chip_runs_query() {
    let out = filter_rows(FilterParams {
        query: "x",
        apps: &[app("X", None)],
        windows: &[],
        files: &[],
        recents: &[],
        app_usage: &Default::default(),
        app_icons: &Default::default(),
        category: Category::Commands,
        file_max: 50,
        total_max: 30,
        cached_app_rows: None,
        cached_win_rows: None,
        prefix: None,
        calc: None,
    });
    assert_eq!(out.len(), 1);
    assert!(matches!(out[0].kind, RowKind::Command { .. }));
}

#[test]
fn windows_chip_only_windows() {
    let out = filter_rows(FilterParams {
        query: "",
        apps: &[app("Firefox", Some("firefox"))],
        windows: &[WindowEntry {
            id: 7,
            title: "Mozilla Firefox".into(),
            app_id: Some("firefox".into()),
            app_id_lc: Some("firefox".into()),
        }],
        files: &[],
        recents: &[],
        app_usage: &Default::default(),
        app_icons: &Default::default(),
        category: Category::Windows,
        file_max: 50,
        total_max: 30,
        cached_app_rows: None,
        cached_win_rows: None,
        prefix: None,
        calc: None,
    });
    assert_eq!(out.len(), 1);
    assert!(matches!(out[0].kind, RowKind::Window { .. }));
}

#[test]
fn row_cap_holds() {
    let apps: Vec<DesktopApp> = (0..40).map(|i| app(&format!("App{i:02}"), None)).collect();
    let out = rows("app", &apps, &[], &[], &[]);
    assert_eq!(out.len(), 30);
}

#[test]
fn calculator_does_not_spawn_list_row() {
    // A valid arithmetic query must not produce a list row (it surfaces as
    // an inline ghost instead), nor the "run in terminal" fallback.
    let out = filter_rows(FilterParams {
        query: "2 + 2",
        apps: &[],
        windows: &[],
        files: &[],
        recents: &[],
        app_usage: &Default::default(),
        app_icons: &Default::default(),
        category: Category::All,
        file_max: 50,
        total_max: 30,
        cached_app_rows: None,
        cached_win_rows: None,
        prefix: None,
        calc: None,
    });
    assert!(out.is_empty());
}

#[test]
fn calculator_no_row_in_any_category() {
    // Calc is never a per-category list row; an Apps-view arithmetic query
    // yields no injected row either (the "no match" fallback is suppressed).
    let out = filter_rows(FilterParams {
        query: "2 + 2",
        apps: &[],
        windows: &[],
        files: &[],
        recents: &[],
        app_usage: &Default::default(),
        app_icons: &Default::default(),
        category: Category::Apps,
        file_max: 50,
        total_max: 30,
        cached_app_rows: None,
        cached_win_rows: None,
        prefix: None,
        calc: None,
    });
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
    assert!(names.contains(&"alpha.txt"), "{names:?}");
    assert!(names.contains(&"beta.log"), "{names:?}");
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

#[test]
fn reveal_waits_for_query_pause() {
    assert_eq!(
        reveal_action(true, true, false),
        RevealAction::Debounce
    );
    assert_eq!(
        reveal_action(true, false, true),
        RevealAction::ShowNow
    );
    assert_eq!(
        reveal_action(true, false, false),
        RevealAction::Hold
    );
    assert_eq!(
        reveal_action(false, true, false),
        RevealAction::Collapse
    );
    assert!(!wants_results("", Category::All, false));
    assert!(wants_results("a", Category::All, false));
    assert!(wants_results("", Category::Apps, false));
    assert!(!wants_results("2+2", Category::All, true));
}

#[test]
fn source_menu_shows_only_when_empty_and_top_level() {
    // Visible: empty query at the top level, no calculator result.
    assert!(source_menu_visible(true, Category::All, false));
    // Hidden the instant a char is typed.
    assert!(!source_menu_visible(false, Category::All, false));
    // Hidden inside a category browse (even when empty).
    assert!(!source_menu_visible(true, Category::Apps, false));
    assert!(!source_menu_visible(true, Category::Files, false));
    assert!(!source_menu_visible(true, Category::Windows, false));
    // Hidden when a calculator result is showing.
    assert!(!source_menu_visible(true, Category::All, true));
}

#[test]
fn stale_echo_does_not_rewind_query() {
    assert!(stale_query_snapshot(true, "fire", "f"));
    assert!(stale_query_snapshot(true, "fir", "fire"));
    assert!(!stale_query_snapshot(true, "fire", "fire"));
    assert!(!stale_query_snapshot(false, "old", "from-history"));
}

#[test]
fn reveal_delete_is_a_query_change() {
    // Backspace/delete are local edits. Daemon-driven query changes Debounce;
    // emptying the box Collapses.
    assert_eq!(
        reveal_action(true, true, false),
        RevealAction::Debounce
    );
    assert_eq!(
        reveal_action(false, true, false),
        RevealAction::Collapse
    );
    assert_eq!(
        reveal_action(true, false, false),
        RevealAction::Hold
    );
}

#[test]
fn alacritty_appears_when_running() {
    let apps = vec![app("Alacritty", Some("Alacritty"))];
    let windows = vec![WindowEntry {
        id: 1,
        title: "alacritty".into(),
        app_id: Some("Alacritty".into()),
        app_id_lc: Some("alacritty".into()),
    }];
    let rows = filter_rows(FilterParams {
        query: "alacritty",
        apps: &apps,
        windows: &windows,
        files: &[],
        recents: &[],
        app_usage: &Default::default(),
        app_icons: &Default::default(),
        category: Category::All,
        file_max: 30,
        total_max: 30,
        cached_app_rows: None,
        cached_win_rows: None,
        prefix: None,
        calc: None,
    });
    assert!(!rows.is_empty(), "alacritty should appear in results");
    assert!(
        rows[0].label.to_lowercase().contains("alacritty"),
        "expected alacritty first, got: {:?}",
        rows.iter().map(|r| &r.label).collect::<Vec<_>>()
    );
}

#[test]
fn alacritty_appears_when_not_running() {
    let apps = vec![app("Alacritty", Some("Alacritty"))];
    let rows = filter_rows(FilterParams {
        query: "alacritty",
        apps: &apps,
        windows: &[],
        files: &[],
        recents: &[],
        app_usage: &Default::default(),
        app_icons: &Default::default(),
        category: Category::All,
        file_max: 30,
        total_max: 30,
        cached_app_rows: None,
        cached_win_rows: None,
        prefix: None,
        calc: None,
    });
    assert!(!rows.is_empty(), "alacritty should appear in results");
    assert!(
        rows[0].label.to_lowercase().contains("alacritty"),
        "expected alacritty first, got: {:?}",
        rows.iter().map(|r| &r.label).collect::<Vec<_>>()
    );
}
