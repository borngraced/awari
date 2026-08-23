//! File source: in-process `fff-search` (`FilePicker`) rooted at `$HOME`.
//! One thread owns the picker; the daemon sends queries, results come back
//! tagged with a sequence number so stale answers are dropped.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::thread;
use std::time::Duration;

use fff_search::{
    FilePicker, FilePickerOptions, FFFMode, FuzzySearchOptions, PaginationArgs, QueryParser,
    SharedFilePicker, SharedFrecency,
};

const FILE_ROWS: usize = 8;

#[derive(Clone, Debug, PartialEq)]
pub struct FileHit {
    pub path: PathBuf,
}

/// `~`, `/`, `.`, or any path separator — files win ranking for these.
pub fn is_path_shaped(query: &str) -> bool {
    let q = query.trim();
    q.starts_with('~') || q.starts_with('.') || q.contains('/')
}

pub struct Files {
    tx: Sender<(u64, String)>,
    seq: u64,
}

impl Files {
    pub fn spawn() -> (Self, Receiver<(u64, Vec<FileHit>)>) {
        let (qtx, qrx) = std::sync::mpsc::channel::<(u64, String)>();
        let (rtx, rrx) = std::sync::mpsc::channel();
        thread::Builder::new()
            .name("awari-files".into())
            .spawn(move || picker_loop(qrx, rtx))
            .expect("files thread");
        (Self { tx: qtx, seq: 0 }, rrx)
    }

    /// Fire a query; results arrive on the receiver tagged with this seq.
    pub fn query(&mut self, q: &str) -> u64 {
        self.seq += 1;
        let _ = self.tx.send((self.seq, q.to_string()));
        self.seq
    }
}

fn home_root() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn picker_loop(qrx: Receiver<(u64, String)>, rtx: Sender<(u64, Vec<FileHit>)>) {
    let base = home_root();
    let shared_picker = SharedFilePicker::default();
    let shared_frecency = SharedFrecency::default();

    if let Err(e) = FilePicker::new_with_shared_state(
        shared_picker.clone(),
        shared_frecency,
        FilePickerOptions {
            base_path: base.display().to_string(),
            mode: FFFMode::Neovim,
            watch: true,
            enable_home_dir_scanning: true,
            enable_fs_root_scanning: false,
            enable_mmap_cache: false,
            enable_content_indexing: false,
            follow_symlinks: false,
            cache_budget: None,
        },
    ) {
        tracing::warn!(%e, "fff file index failed to start; file rows disabled");
        return;
    }

    // First open shows whatever is indexed so far; do not block on full scan.
    let _ = shared_picker.wait_for_scan(Duration::from_secs(30));

    let parser = QueryParser::default();
    for (seq, raw) in qrx {
        if raw.trim().is_empty() {
            continue; // empty query: no $HOME dump
        }
        let hits = search(&shared_picker, &parser, &raw);
        if rtx.send((seq, hits)).is_err() {
            return;
        }
    }
}

fn search(
    picker: &SharedFilePicker,
    parser: &QueryParser<fff_search::FileSearchConfig>,
    raw: &str,
) -> Vec<FileHit> {
    let Ok(guard) = picker.read() else {
        return Vec::new();
    };
    let Some(p) = guard.as_ref() else {
        return Vec::new();
    };
    let query = parser.parse(raw);
    let results = p.fuzzy_search(
        &query,
        None,
        FuzzySearchOptions {
            pagination: PaginationArgs {
                offset: 0,
                limit: FILE_ROWS,
            },
            ..Default::default()
        },
    );
    let base = p.base_path.clone();
    results
        .items
        .iter()
        .map(|item| FileHit {
            path: item.absolute_path(p, &base),
        })
        .collect()
}

/// Open via the desktop default handler. No shell.
pub fn activate(path: &Path) {
    match std::process::Command::new("xdg-open")
        .arg(path)
        .spawn()
    {
        Ok(_) => {}
        Err(e) => tracing::warn!(%e, path = %path.display(), "xdg-open failed"),
    }
}
