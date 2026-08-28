//! File source: in-process `fff-search` (`FilePicker`), one picker per
//! configured root. One thread owns all pickers; the daemon sends queries,
//! results come back tagged with a sequence number so stale answers drop.

mod action;
mod fzy;
mod path;
mod picker;
mod regex;

pub use action::{activate, reveal, run_in_terminal};
pub(crate) use action::{resolve_terminal, run_script};
pub use fzy::subsequence_score;
pub use path::is_path_shaped;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use fff_search::SharedFrecency;

/// Hits asked of each FFF picker. High enough to browse; the overlay
/// virtualizes, so this is a search-cost cap, not a paint cap.
const PER_ROOT_ROWS: usize = 200;

/// Hard cap on file-search results returned to the launcher per query. The panel
/// shows only a handful of rows, so anything beyond this is pure search cost
/// (and, for an empty-query browse, index memory) with no visible benefit.
const MAX_FILE_RESULTS: usize = 30;

/// Cap on distinct path-shaped directories kept indexed for navigation
/// (`~/dev/` builds one per-directory picker). Without a cap this map grows for
/// every directory typed, leaking memory for the daemon's lifetime.
const TRANSIENT_DIR_CAP: usize = 8;

/// Max cached query results retained per `FilePicker`, bounding per-picker
/// result-cache memory across repeated searches.
const FILE_CACHE_BUDGET: usize = 2048;

/// Byte cap for transient per-directory indexes. Inert today (transient pickers
/// run with content indexing and mmap caching disabled) unless that changes.
const TRANSIENT_CACHE_BYTES: u64 = 2 * 1024 * 1024;

/// Content-cache byte cap for the persistent root indexes. Without a cap fff
/// auto-sizes this from the scanned count; bounding it keeps baseline memory
/// predictable.
const ROOT_CACHE_BYTES: u64 = 64 * 1024 * 1024;

/// Typing-burst debounce before a search runs. Results are async rows, so the
/// latency hides under continued typing; a burst costs one search.
const QUERY_DEBOUNCE: Duration = Duration::from_millis(20);

/// How often the worker wakes while idle to check for a cache-clear signal.
const CTRL_POLL: Duration = Duration::from_millis(100);

/// Behavior flags for the file source.
pub struct FilesOptions {
    pub index_lockfiles: bool,
    pub regex: bool,
    /// Toggles applied to every fff-search picker (see `config::FffConfig`).
    pub fff: crate::config::FffConfig,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FileHit {
    pub path: Arc<Path>,
}

pub struct Files {
    tx: Sender<(u64, String)>,
    ctrl: Sender<()>,
    seq: u64,
    frecencies: Vec<(PathBuf, SharedFrecency)>,
}

impl Files {
    /// One worker indexes every root; empty roots disable the source.
    pub fn spawn(roots: Vec<PathBuf>, opts: FilesOptions) -> (Self, Receiver<(u64, Vec<FileHit>)>) {
        let (qtx, qrx) = std::sync::mpsc::channel::<(u64, String)>();
        let (rtx, rrx) = std::sync::mpsc::channel();
        let (ctrl_tx, ctrl_rx) = std::sync::mpsc::channel::<()>();
        let (pickers, frecencies) = picker::build_root_pickers(&roots, opts.fff);
        if !pickers.is_empty() {
            std::thread::Builder::new()
                .name("awari-files".into())
                .spawn(move || picker::picker_loop(pickers, qrx, rtx, ctrl_rx, opts))
                .expect("files thread");
        }
        (
            Self {
                tx: qtx,
                ctrl: ctrl_tx,
                seq: 0,
                frecencies,
            },
            rrx,
        )
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

    /// Reclaim file-search memory: drops the per-directory scratch indexes kept
    /// during path navigation. Root indexes are bounded and kept warm, so idle
    /// RAM stays near baseline without a re-index walk. Call on dismiss.
    pub fn clear(&self) {
        let _ = self.ctrl.send(());
    }

    /// Record that a file was opened through the launcher, feeding the
    /// "frequent" half of frecency ranking. Maps the path to its owning root
    /// (most specific match) and writes the access into that root's shared
    /// frecency store — the same one the picker reads when scoring.
    pub fn record_open(&self, path: &Path) {
        let mut best: Option<&SharedFrecency> = None;
        let mut best_len = 0;
        for (root, frec) in &self.frecencies {
            if let Ok(rest) = path.strip_prefix(root)
                && !rest.as_os_str().is_empty()
            {
                let l = root.as_os_str().len();
                if l > best_len {
                    best_len = l;
                    best = Some(frec);
                }
            }
        }
        if let Some(frec) = best
            && let Ok(mut g) = frec.write()
            && let Some(tracker) = g.as_mut()
            && let Err(e) = tracker.track_access(path)
        {
            tracing::debug!(?e, "frecency track_access failed");
        }
    }
}