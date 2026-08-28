use super::*;
use gpui::SharedString;
use std::path::PathBuf;
use std::sync::Arc;

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
