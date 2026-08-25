    use super::*;
    use crate::desktop::DesktopApp;
    use crate::files::FileHit;
    use crate::surfaces::LAUNCHER_NAMESPACE;
    use crate::ui::launcher::view::icon_letter;
    use std::path::PathBuf;
    use std::sync::Arc;

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
    fn running_window_suppresses_app_row() {
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
    fn commands_chip_is_empty() {
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
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].label, "2 + 2 = 4");
        assert!(matches!(out[0].kind, RowKind::Calc { .. }));
    }

    #[test]
    fn calculator_only_in_all_view() {
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
