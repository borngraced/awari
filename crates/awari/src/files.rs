//! File source: in-process `fff-search` (`FilePicker`), one picker per
//! configured root. One thread owns all pickers; the daemon sends queries,
//! results come back tagged with a sequence number so stale answers drop.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::thread;

use regex::Regex;

use fff_search::{
    FilePicker, FilePickerOptions, FFFMode, FuzzySearchOptions, PaginationArgs, QueryParser,
    SharedFilePicker, SharedFrecency,
};

/// Hits asked of each FFF picker. High enough to browse; the overlay
/// virtualizes, so this is a search-cost cap, not a paint cap.
const PER_ROOT_ROWS: usize = 200;

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
    seq: u64,
}

impl Files {
    /// One worker indexes every root; empty roots disable the source.
    pub fn spawn(roots: Vec<PathBuf>, opts: FilesOptions) -> (Self, Receiver<(u64, Vec<FileHit>)>) {
        let (qtx, qrx) = std::sync::mpsc::channel::<(u64, String)>();
        let (rtx, rrx) = std::sync::mpsc::channel();
        if !roots.is_empty() {
            thread::Builder::new()
                .name("awari-files".into())
                .spawn(move || picker_loop(roots, qrx, rtx, opts))
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

fn picker_loop(
    roots: Vec<PathBuf>,
    qrx: Receiver<(u64, String)>,
    rtx: Sender<(u64, Vec<FileHit>)>,
    opts: FilesOptions,
) {
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
    let mut transient: HashMap<PathBuf, SharedFilePicker> = HashMap::new();
    while let Ok(first) = qrx.recv() {
        let (seq, raw) = coalesce(&qrx, first);
        if raw.trim().is_empty() {
            continue;
        }
        let hits = search_all(&pickers, &mut transient, &parser, &raw, &opts);
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
                    cache_budget: None,
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
