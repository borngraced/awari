use gpui::SharedString;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use crate::desktop::DesktopApp;
use crate::files::FileHit;
use crate::ui::icon::Icon;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum Category {
    All,
    Apps,
    Files,
    Commands,
    Windows,
}

impl Category {
    pub(crate) fn icon(self) -> Icon {
        match self {
            Self::All => Icon::LayoutGrid,
            Self::Apps => Icon::AppWindow,
            Self::Files => Icon::File,
            Self::Commands => Icon::Command,
            Self::Windows => Icon::Search,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Apps => "Apps",
            Self::Files => "Files",
            Self::Commands => "Commands",
            Self::Windows => "Windows",
        }
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
    CopyClipboard {
        text: String,
    },
    SavePosition {
        x: f32,
        y: f32,
    },
}
#[derive(Clone)]
pub struct LauncherRow {
    pub kind: RowKind,
    pub label: SharedString,
    pub resolved_icon: Option<Arc<Path>>,
    pub subtitle: Option<SharedString>,
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RowAction {
    Open,
    ShowInFolder,
    CopyPath,
    RunInTerminal,
    Run,
}

impl RowAction {
    pub fn label(&self) -> &'static str {
        match self {
            RowAction::Open => "Open",
            RowAction::ShowInFolder => "Show in Folder",
            RowAction::CopyPath => "Copy Path",
            RowAction::RunInTerminal => "Run in Terminal",
            RowAction::Run => "Run",
        }
    }
}
#[derive(Clone)]
pub enum RowKind {
    App {
        name: SharedString,
        exec: Arc<[String]>,
    },
    Window {
        id: u64,
    },
    File {
        path: Arc<Path>,
    },
    /// A shell command to run in a terminal (from `>` command mode or the
    /// no-match fallback).
    Command {
        command: String,
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
        }
    }
}
#[derive(Clone, Debug, PartialEq)]
pub struct WindowEntry {
    pub id: u64,
    pub title: String,
    pub app_id: Option<String>,
    pub app_id_lc: Option<String>,
}

/// Like [`filter_rows`], but reuses pre-scored app/window rows when the caller
/// supplies them (the Daemon caches these by `query` + `source_gen`). This is
/// the expensive part — `matchq` scoring + a sort over every app/window — so
/// skipping it on the re-render that fires when file results arrive avoids a
/// redundant full re-score on every keystroke.
/// Arguments to [`filter_rows_cached`], grouped so the function isn't a
/// 14-parameter wall.
pub struct FilterParams<'a> {
    pub query: &'a str,
    pub apps: &'a [DesktopApp],
    pub windows: &'a [WindowEntry],
    pub files: &'a [FileHit],
    pub recents: &'a [String],
    pub app_usage: &'a HashMap<String, u64>,
    pub app_icons: &'a HashMap<String, String>,
    pub category: Category,
    pub file_max: usize,
    pub total_max: usize,
    pub cached_app_rows: Option<&'a [LauncherRow]>,
    pub cached_win_rows: Option<&'a [LauncherRow]>,
    pub prefix: Option<&'a str>,
    pub calc: Option<String>,
}
