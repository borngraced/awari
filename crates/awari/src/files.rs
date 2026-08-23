//! File source: in-process `fff-search` (`FilePicker`), one picker per
//! configured root. One thread owns all pickers; the daemon sends queries,
//! results come back tagged with a sequence number so stale answers drop.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::thread;

use fff_search::{
    FilePicker, FilePickerOptions, FFFMode, FuzzySearchOptions, PaginationArgs, QueryParser,
    SharedFilePicker, SharedFrecency,
};

const PER_ROOT_ROWS: usize = 8;

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
    /// One worker indexes every root; empty roots disable the source.
    pub fn spawn(roots: Vec<PathBuf>) -> (Self, Receiver<(u64, Vec<FileHit>)>) {
        let (qtx, qrx) = std::sync::mpsc::channel::<(u64, String)>();
        let (rtx, rrx) = std::sync::mpsc::channel();
        if !roots.is_empty() {
            thread::Builder::new()
                .name("awari-files".into())
                .spawn(move || picker_loop(roots, qrx, rtx))
                .expect("files thread");
        }
        (Self { tx: qtx, seq: 0 }, rrx)
    }

    /// Fire a query; results arrive on the receiver tagged with this seq.
    pub fn query(&mut self, q: &str) -> u64 {
        self.seq += 1;
        let _ = self.tx.send((self.seq, q.to_string()));
        self.seq
    }

    /// Drop in-flight answers without searching.
    pub fn invalidate(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }
}

fn is_home_root(root: &Path) -> bool {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return false;
    };
    if root == home {
        return true;
    }
    match (root.canonicalize(), home.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

fn picker_loop(roots: Vec<PathBuf>, qrx: Receiver<(u64, String)>, rtx: Sender<(u64, Vec<FileHit>)>) {
    let mut pickers: Vec<SharedFilePicker> = Vec::new();
    for root in &roots {
        let shared = SharedFilePicker::default();
        let frecency = SharedFrecency::default();
        let home = is_home_root(root);
        let res = FilePicker::new_with_shared_state(
            shared.clone(),
            frecency,
            FilePickerOptions {
                base_path: root.display().to_string(),
                mode: FFFMode::Neovim,
                watch: true,
                enable_home_dir_scanning: home,
                enable_fs_root_scanning: false,
                enable_mmap_cache: false,
                enable_content_indexing: false,
                follow_symlinks: false,
                cache_budget: None,
            },
        );
        match res {
            Ok(()) => pickers.push(shared),
            Err(e) => tracing::warn!(%e, root = %root.display(), "file index failed"),
        }
    }
    tracing::info!(roots = pickers.len(), "file index started");

    let parser = QueryParser::default();
    while let Ok(first) = qrx.recv() {
        let (seq, raw) = coalesce(&qrx, first);
        if raw.trim().is_empty() {
            continue;
        }
        let hits = search_all(&pickers, &parser, &raw);
        if rtx.send((seq, hits)).is_err() {
            return;
        }
    }
}

fn coalesce(qrx: &Receiver<(u64, String)>, first: (u64, String)) -> (u64, String) {
    let mut latest = first;
    loop {
        match qrx.try_recv() {
            Ok(next) => latest = next,
            Err(TryRecvError::Empty) => return latest,
            Err(TryRecvError::Disconnected) => return latest,
        }
    }
}

fn search_all(
    pickers: &[SharedFilePicker],
    parser: &QueryParser<fff_search::FileSearchConfig>,
    raw: &str,
) -> Vec<FileHit> {
    let merged: Vec<Vec<FileHit>> = pickers
        .iter()
        .map(|shared| search_one(shared, parser, raw))
        .collect();
    merge_round_robin(&merged, PER_ROOT_ROWS.saturating_mul(2))
}

fn merge_round_robin(merged: &[Vec<FileHit>], cap: usize) -> Vec<FileHit> {
    let mut out = Vec::new();
    let mut cursors = vec![0usize; merged.len()];
    loop {
        if out.len() >= cap {
            break;
        }
        let mut progressed = false;
        for (i, m) in merged.iter().enumerate() {
            if out.len() >= cap {
                break;
            }
            if cursors[i] < m.len() {
                out.push(m[cursors[i]].clone());
                cursors[i] += 1;
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
    }
    out
}

fn search_one(
    shared: &SharedFilePicker,
    parser: &QueryParser<fff_search::FileSearchConfig>,
    raw: &str,
) -> Vec<FileHit> {
    let Ok(guard) = shared.read() else {
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
                limit: PER_ROOT_ROWS,
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
    match Command::new("xdg-open")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => {
            thread::Builder::new()
                .name("awari-xdg-open".into())
                .spawn(move || {
                    let mut child = child;
                    let _ = child.wait();
                })
                .ok();
        }
        Err(e) => tracing::warn!(%e, path = %path.display(), "xdg-open failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hits(names: &[&str]) -> Vec<FileHit> {
        names
            .iter()
            .map(|n| FileHit {
                path: PathBuf::from(n),
            })
            .collect()
    }

    #[test]
    fn merge_one_short_root_does_not_hang() {
        let merged = vec![hits(&["a", "b"])];
        let out = merge_round_robin(&merged, 16);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn merge_round_robin_interleaves_and_caps() {
        let merged = vec![hits(&["a1", "a2", "a3"]), hits(&["b1"])];
        let out = merge_round_robin(&merged, 4);
        let names: Vec<_> = out.iter().map(|h| h.path.to_str().unwrap()).collect();
        assert_eq!(names, ["a1", "b1", "a2", "a3"]);
    }
}
