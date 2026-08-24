//! File source: in-process `fff-search` (`FilePicker`), one picker per
//! configured root. One thread owns all pickers; the daemon sends queries,
//! results come back tagged with a sequence number so stale answers drop.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;

use regex::Regex;

use fff_search::{
    ContentCacheBudget, FilePicker, FilePickerOptions, FFFMode, FuzzySearchOptions, PaginationArgs,
    QueryParser, SharedFilePicker, SharedFrecency,
};

/// Hits asked of each FFF picker. High enough to browse; the overlay
/// virtualizes, so this is a search-cost cap, not a paint cap.
const PER_ROOT_ROWS: usize = 200;

/// Cap on the number of distinct path-shaped directories we keep an in-memory
/// index for. Path navigation (`~/dev/`) builds a `SharedFilePicker` per
/// directory; without a cap this map grows for every directory the user ever
/// types, leaking memory for the life of the daemon. Each picker recursively
/// indexes its whole subtree, so keep this small.
const TRANSIENT_DIR_CAP: usize = 8;

/// Max cached query results retained per `FilePicker`. Bounding this caps the
/// per-picker result cache so repeated searches don't grow memory without end.
const FILE_CACHE_BUDGET: usize = 2048;

/// Content-cache byte cap applied to the persistent root indexes. Without a
/// cap fff auto-sizes this from the scanned file count, which can be large for
/// user dirs; bounding it keeps the baseline memory predictable.
const ROOT_CACHE_BYTES: u64 = 64 * 1024 * 1024;

/// Typing-burst debounce before a search runs. Results are async rows, so
/// the added latency hides under continued typing; a keystroke burst costs
/// one search instead of one per key.
const QUERY_DEBOUNCE: Duration = Duration::from_millis(20);

/// How often the worker wakes while idle to check for a cache-clear signal.
/// Small enough that dismiss → reclaim is prompt, large enough to be free.
const CTRL_POLL: Duration = Duration::from_millis(100);

/// Behavior flags for the file source.
pub struct FilesOptions {
    pub index_lockfiles: bool,
    pub regex: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FileHit {
    pub path: PathBuf,
}

/// `~`, `/`, `.`, or any path separator — files win ranking for these.
pub fn is_path_shaped(query: &str) -> bool {
    let q = query.trim();
    q.starts_with('~') || q.starts_with('.') || q.contains('/')
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// If `raw` is a path-shaped query whose leading directory exists on disk,
/// return that directory so results can be pulled directly from it (e.g.
/// `~/dev/` → `~/dev`). Returns `None` otherwise.
fn path_query_dir(raw: &str) -> Option<PathBuf> {
    if !is_path_shaped(raw) {
        return None;
    }
    let trimmed = raw.trim();
    let expanded = if let Some(rest) = trimmed.strip_prefix('~') {
        let home = home_dir()?;
        if rest.is_empty() || rest.starts_with('/') {
            home.join(rest.trim_start_matches('/'))
        } else {
            home.join(rest)
        }
    } else {
        PathBuf::from(trimmed)
    };
    let dir = if expanded.to_string_lossy().ends_with('/') {
        expanded
    } else {
        match expanded.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => expanded,
        }
    };
    dir.is_dir().then_some(dir)
}

pub struct Files {
    tx: Sender<(u64, String)>,
    ctrl: Sender<()>,
    seq: u64,
}

impl Files {
    /// One worker indexes every root; empty roots disable the source.
    pub fn spawn(roots: Vec<PathBuf>, opts: FilesOptions) -> (Self, Receiver<(u64, Vec<FileHit>)>) {
        let (qtx, qrx) = std::sync::mpsc::channel::<(u64, String)>();
        let (rtx, rrx) = std::sync::mpsc::channel();
        let (ctrl_tx, ctrl_rx) = std::sync::mpsc::channel::<()>();
        if !roots.is_empty() {
            thread::Builder::new()
                .name("awari-files".into())
                .spawn(move || picker_loop(roots, qrx, rtx, ctrl_rx, opts))
                .expect("files thread");
        }
        (Self { tx: qtx, ctrl: ctrl_tx, seq: 0 }, rrx)
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

    /// Reclaim file-search memory: drops the per-directory scratch indexes
    /// kept during path navigation. Root indexes are bounded and kept warm,
    /// so idle RAM stays near baseline without a re-index walk. Call on dismiss.
    pub fn clear(&self) {
        let _ = self.ctrl.send(());
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

/// Build the persistent per-root `FilePicker`s. Called once at startup; the
/// indexes are kept warm for the daemon's lifetime (bounded by
/// `ROOT_CACHE_BYTES` and carrying FFF watches + frecency).
fn build_root_pickers(roots: &[PathBuf]) -> Vec<SharedFilePicker> {
    let mut pickers = Vec::new();
    for root in roots {
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
                cache_budget: ContentCacheBudget::from_overrides(0, ROOT_CACHE_BYTES, 0),
            },
        );
        match res {
            Ok(()) => pickers.push(shared),
            Err(e) => tracing::warn!(%e, root = %root.display(), "file index failed"),
        }
    }
    pickers
}

fn picker_loop(
    roots: Vec<PathBuf>,
    qrx: Receiver<(u64, String)>,
    rtx: Sender<(u64, Vec<FileHit>)>,
    ctrl: Receiver<()>,
    opts: FilesOptions,
) {
    let pickers = build_root_pickers(&roots);
    tracing::info!(roots = pickers.len(), "file index started");

    let parser = QueryParser::default();
    let mut transient: HashMap<PathBuf, SharedFilePicker> = HashMap::new();
    let mut transient_order: VecDeque<PathBuf> = VecDeque::with_capacity(TRANSIENT_DIR_CAP);
    loop {
        // Reclaim memory on dismiss by dropping the per-directory scratch
        // indexes only. The root indexes stay warm (bounded by
        // ROOT_CACHE_BYTES, with live FFF watches + frecency), so the next
        // open is fast and we avoid a full filesystem walk here. Multiple
        // queued clear signals just repeat this cheap transient drop.
        while ctrl.try_recv().is_ok() {
            tracing::debug!("clearing transient file caches on dismiss");
            transient.clear();
            transient_order.clear();
        }
        // Block for the next query, but wake periodically so a clear signal
        // isn't starved while the launcher is idle.
        let first = match qrx.recv_timeout(CTRL_POLL) {
            Ok(f) => f,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        // Debounce: let a typing burst land, then answer only the newest
        // queued query (coalesce drains everything that arrived).
        thread::sleep(QUERY_DEBOUNCE);
        let (seq, raw) = coalesce(&qrx, first);
        if raw.trim().is_empty() {
            continue;
        }
        let hits = search_all(&pickers, &mut transient, &mut transient_order, &parser, &raw, &opts);
        if rtx.send((seq, hits)).is_err() {
            return;
        }
    }
}

/// Resolve whether `raw` is a regex query and compile it. The `r:` prefix
/// forces regex mode per-query; otherwise only the `files.regex` config does.
fn resolve_regex(raw: &str, global_regex: bool) -> (String, Option<Regex>) {
    let (pattern, want) = if let Some(p) = raw.strip_prefix("r:") {
        (p.to_string(), true)
    } else if global_regex {
        (raw.to_string(), true)
    } else {
        (raw.to_string(), false)
    };
    if !want {
        return (pattern, None);
    }
    match Regex::new(&pattern) {
        Ok(re) => (pattern, Some(re)),
        Err(e) => {
            tracing::debug!(%e, "regex compile failed; ignoring regex filter");
            (pattern, None)
        }
    }
}

/// A fuzzy-friendly hint derived from a regex pattern (strips metacharacters)
/// so FFF still returns candidate paths for the regex to refine.
fn regex_hint(raw: &str) -> String {
    raw.chars()
        .filter(|c| {
            c.is_alphanumeric() || *c == '/' || *c == '.' || *c == ' ' || *c == '-' || *c == '_'
        })
        .collect()
}

/// Lock files that are usually noise in launcher file search.
fn is_lockfile(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
        return false;
    };
    if name.ends_with(".lock") {
        return true;
    }
    matches!(
        name,
        "Cargo.lock"
            | "package-lock.json"
            | "yarn.lock"
            | "pnpm-lock.yaml"
            | "pnpm-lock.yml"
            | "npm-shrinkwrap.json"
            | "Gemfile.lock"
            | "poetry.lock"
            | "composer.lock"
            | "mix.lock"
            | "flake.lock"
            | "Pipfile.lock"
            | "deno.lock"
            | "bun.lockb"
            | "go.sum"
            | "go.mod"
    )
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
    transient: &mut HashMap<PathBuf, SharedFilePicker>,
    transient_order: &mut VecDeque<PathBuf>,
    parser: &QueryParser<fff_search::FileSearchConfig>,
    raw: &str,
    opts: &FilesOptions,
) -> Vec<FileHit> {
    let (pattern, regex) = resolve_regex(raw, opts.regex);
    let fff_query = if regex.is_some() {
        regex_hint(&pattern)
    } else {
        raw.to_string()
    };
    let mut merged: Vec<Vec<FileHit>> = pickers
        .iter()
        .map(|shared| search_one(shared, parser, &fff_query, &regex, opts.index_lockfiles))
        .collect();
    // When the query points at a real directory (e.g. `~/dev/`), search it
    // directly so path navigation reaches places outside the configured roots.
    // Only the trailing filename segment is fuzzy-matched, not the dir prefix.
    if let Some(dir) = path_query_dir(raw) {
        // LRU eviction: keep at most TRANSIENT_DIR_CAP per-directory indexes.
        if let Some(pos) = transient_order.iter().position(|p| *p == dir) {
            transient_order.remove(pos);
        } else if transient_order.len() >= TRANSIENT_DIR_CAP {
            if let Some(old) = transient_order.pop_front() {
                transient.remove(&old);
            }
        }
        transient_order.push_back(dir.clone());
        let shared = transient.entry(dir.clone()).or_insert_with(|| {
            let shared = SharedFilePicker::default();
            let frecency = SharedFrecency::default();
            let _ = FilePicker::new_with_shared_state(
                shared.clone(),
                frecency,
                FilePickerOptions {
                    base_path: dir.display().to_string(),
                    mode: FFFMode::Neovim,
                    watch: false,
                    enable_home_dir_scanning: false,
                    enable_fs_root_scanning: false,
                    enable_mmap_cache: false,
                    enable_content_indexing: false,
                    follow_symlinks: false,
                    cache_budget: ContentCacheBudget::from_overrides(FILE_CACHE_BUDGET, 0, 0),
                },
            );
            shared
        });
        let term = if raw.trim_end().ends_with('/') {
            String::new()
        } else {
            raw.rsplit('/').next().unwrap_or("").to_string()
        };
        let (t_pat, t_re) = resolve_regex(&term, opts.regex);
        let t_fff = if t_re.is_some() {
            regex_hint(&t_pat)
        } else {
            term
        };
        merged.push(search_one(shared, parser, &t_fff, &t_re, opts.index_lockfiles));
    }
    let cap = PER_ROOT_ROWS.saturating_mul(merged.len().max(1));
    merge_round_robin(&merged, cap)
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
    fff_query: &str,
    regex: &Option<Regex>,
    index_lockfiles: bool,
) -> Vec<FileHit> {
    let Ok(guard) = shared.read() else {
        return Vec::new();
    };
    let Some(p) = guard.as_ref() else {
        return Vec::new();
    };
    let query = parser.parse(fff_query);
    // Lockfile/regex filters run after FFF's pagination, so overscan the
    // page when they will shrink it; otherwise a heavily-filtered query
    // delivers fewer rows than exist past the limit.
    let limit = if regex.is_some() || !index_lockfiles {
        PER_ROOT_ROWS * 4
    } else {
        PER_ROOT_ROWS
    };
    let results = p.fuzzy_search(
        &query,
        None,
        FuzzySearchOptions {
            pagination: PaginationArgs {
                offset: 0,
                limit,
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
        .filter(|h| {
            if !index_lockfiles && is_lockfile(&h.path) {
                return false;
            }
            if let Some(re) = regex
                && !re.is_match(&h.path.to_string_lossy())
            {
                return false;
            }
            true
        })
        .take(PER_ROOT_ROWS)
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

/// Reveal a path in the file manager by opening its parent directory.
pub fn reveal(path: &Path) {
    let parent = path.parent().unwrap_or(path);
    activate(parent);
}

/// Resolve the user's preferred terminal emulator: `$TERMINAL`, then probing
/// `$PATH` directly (no `which` fork) so this is safe to call on the UI
/// thread. The result is cached for the process lifetime.
fn resolve_terminal() -> Option<String> {
    static CACHED: OnceLock<Option<String>> = OnceLock::new();
    CACHED
        .get_or_init(|| {
            if let Ok(term) = std::env::var("TERMINAL") {
                if !term.trim().is_empty() {
                    return Some(term.trim().to_string());
                }
            }
            let Some(paths) = std::env::var("PATH").ok() else {
                return None;
            };
            for candidate in [
                "alacritty",
                "kitty",
                "ghostty",
                "wezterm",
                "foot",
                "gnome-terminal",
                "konsole",
                "st",
            ] {
                for dir in paths.split(':') {
                    if dir.is_empty() {
                        continue;
                    }
                    let p = Path::new(dir).join(candidate);
                    if p.is_file() {
                        return Some(candidate.to_string());
                    }
                }
            }
            None
        })
        .clone()
}

/// Build the args to run `script` via `sh -c` in the given terminal.
fn terminal_args(term: &str, script: &str) -> Vec<String> {
    match term {
        "gnome-terminal" => vec!["--".to_string(), "sh".to_string(), "-c".to_string(), script.to_string()],
        _ => vec!["-e".to_string(), "sh".to_string(), "-c".to_string(), script.to_string()],
    }
}

/// Spawn `script` inside the user's terminal emulator. The child is reaped on
/// a background thread (like `activate`) so the UI thread never blocks.
fn run_script(script: &str) {
    let Some(term) = resolve_terminal() else {
        tracing::warn!("no terminal emulator found; set $TERMINAL");
        return;
    };
    match Command::new(&term)
        .args(terminal_args(&term, script))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => {
            thread::Builder::new()
                .name("awari-terminal".into())
                .spawn(move || {
                    let mut child = child;
                    let _ = child.wait();
                })
                .ok();
        }
        Err(e) => tracing::warn!(%e, term, "failed to spawn terminal"),
    }
}

/// Open a terminal emulator rooted at `dir`.
pub fn run_in_terminal(dir: &Path) {
    let dir = dir.display().to_string();
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "bash".to_string());
    run_script(&format!("cd {dir:?} && exec {shell}"));
}

/// Open a terminal, run `command`, then drop to an interactive shell so the
/// user can inspect the output.
pub fn run_command(command: &str) {
    run_script(&format!("{command} ; exec \"$SHELL\""));
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

    #[test]
    fn merge_without_small_cap_keeps_every_root_hit() {
        let merged = vec![hits(&["a1", "a2"]), hits(&["b1", "b2", "b3"])];
        let out = merge_round_robin(&merged, 200);
        assert_eq!(out.len(), 5);
    }

    #[test]
    fn lockfiles_detected_by_name() {
        assert!(is_lockfile(Path::new("/p/Cargo.lock")));
        assert!(is_lockfile(Path::new("/p/nested/foo.lock")));
        assert!(is_lockfile(Path::new("/p/package-lock.json")));
        assert!(is_lockfile(Path::new("/p/yarn.lock")));
        assert!(!is_lockfile(Path::new("/p/main.rs")));
        assert!(!is_lockfile(Path::new("/p/Cargo.toml")));
        assert!(!is_lockfile(Path::new("/p/flake.nix")));
    }

    #[test]
    fn regex_resolution() {
        // `r:` prefix forces regex and strips the prefix.
        let (pat, re) = resolve_regex("r:foo", false);
        assert_eq!(pat, "foo");
        assert!(re.is_some());
        // No prefix and global off → plain fuzzy (no regex).
        assert!(resolve_regex("foo", false).1.is_none());
        // Global on → regex even without prefix.
        assert!(resolve_regex("foo", true).1.is_some());
        // Invalid pattern → falls back to no regex rather than panicking.
        assert!(resolve_regex("r:[", false).1.is_none());
    }

    #[test]
    fn regex_hint_strips_metacharacters() {
        assert_eq!(regex_hint(r"\.rs$"), ".rs");
        assert_eq!(regex_hint(r"src/.*\.rs"), "src/..rs");
    }

    #[test]
    fn path_query_dir_resolves_existing_directory() {
        let base = std::env::temp_dir().join(format!("awari_pathq_{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let q = format!("{}/", base.display());
        let got = path_query_dir(&q);
        let _ = std::fs::remove_dir_all(&base);
        let got = got.expect("existing directory with trailing slash should resolve");
        assert_eq!(
            got.as_os_str()
                .to_string_lossy()
                .trim_end_matches('/')
                .to_string(),
            base.to_string_lossy().to_string(),
            "resolved dir should match the queried directory"
        );
    }

    #[test]
    fn path_query_dir_ignores_missing_and_plain() {
        assert!(path_query_dir("firefox").is_none());
        assert!(path_query_dir("/nonexistent_awari_xyz_123/").is_none());
    }
}
