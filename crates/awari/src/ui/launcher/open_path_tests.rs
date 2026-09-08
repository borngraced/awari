use super::*;
use gpui::SharedString;
use std::path::PathBuf;
use std::sync::Arc;

#[test]
fn read_dir_matching_matches_fragment_in_name() {
    let dir = std::env::temp_dir().join(format!("awari_rdm_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    std::fs::write(dir.join("notes.md"), b"").unwrap();
    std::fs::write(dir.join("no.txt"), b"").unwrap();
    std::fs::write(dir.join("zzz"), b"").unwrap();

    let names: Vec<String> = read_dir_matching(&dir, "no")
        .unwrap()
        .into_iter()
        .map(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert!(names.iter().any(|n| n == "notes.md"), "{names:?}");
    assert!(names.iter().any(|n| n == "no.txt"), "{names:?}");
    assert!(!names.iter().any(|n| n == "zzz"), "{names:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

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
        subtitle: build_subtitle(&kind),
        kind,
        label: SharedString::from(label),
        resolved_icon: None,
    }
}

#[test]
fn tab_inline_completes_top_row() {
    let rows = vec![lrow(
        RowKind::App {
            name: "GoLand".into(),
            comment: None,
            exec: Arc::from(vec![]),
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
fn tab_falls_back_to_selected_file_path() {
    let rows = vec![
        lrow(
            RowKind::File {
                path: Arc::from(PathBuf::from("/tmp/a b.txt")),
            },
            "a b.txt",
        ),
        lrow(
            RowKind::App {
                name: "Zed".into(),
                comment: None,
                exec: Arc::from(vec![]),
            },
            "Zed",
        ),
    ];
    match tab_completion("ze", &rows, 1) {
        Some(TabOutcome::Inline { completed, .. }) => assert_eq!(completed, "Zed"),
        other => panic!("expected Inline on selected app, got {other:?}"),
    }
    match tab_completion("a", &rows, 0) {
        Some(TabOutcome::Inline { completed, .. }) => assert_eq!(completed, "a b.txt"),
        other => panic!("expected Inline on selected file, got {other:?}"),
    }
    assert!(matches!(
        tab_completion("zzz", &rows, 1),
        Some(TabOutcome::Row(_))
    ));
    assert!(tab_completion("zzz", &rows, 9).is_none());
}

#[test]
fn app_subtitle_prefers_comment_over_exec() {
    let with_comment = RowKind::App {
        name: "Zed".into(),
        comment: Some("The editor for what you'll build".into()),
        exec: Arc::from(vec!["zed".into()]),
    };
    assert_eq!(
        build_subtitle(&with_comment).map(|s| s.to_string()),
        Some("The editor for what you'll build".to_string())
    );
    let without_comment = RowKind::App {
        name: "Zed".into(),
        comment: None,
        exec: Arc::from(vec!["zed".into(), "--foreground".into()]),
    };
    assert_eq!(
        build_subtitle(&without_comment).map(|s| s.to_string()),
        Some("zed --foreground".to_string())
    );
}
