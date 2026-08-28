//! The search worker: one thread owns every `FilePicker` (persistent roots plus
//! transient per-directory pickers for path navigation), receives debounced
//! queries, and merges per-root hits into one score-ordered list.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::thread;

use fff_search::{
    ContentCacheBudget, FFFMode, FilePicker, FilePickerOptions, FrecencyTracker,
    FuzzySearchOptions, PaginationArgs, QueryParser, SharedFilePicker, SharedFrecency,
};
use regex::Regex;

use awari_ipc::state_dir;

use super::fzy::subsequence_score_chars;
use super::path::path_query_dir;
use super::regex::{RegexCaches, regex_hint, resolve_regex};
use super::{
    CTRL_POLL, FILE_CACHE_BUDGET, FileHit, FilesOptions, MAX_FILE_RESULTS, PER_ROOT_ROWS,
    QUERY_DEBOUNCE, ROOT_CACHE_BYTES, TRANSIENT_CACHE_BYTES, TRANSIENT_DIR_CAP,
};

/// Open a persistent frecency store for a root. Falls back to an in-memory
/// tracker if the on-disk LMDB can't be created, so ranking still works
/// (without cross-session frequency) rather than erroring.
fn open_frecency(root: &std::path::Path) -> SharedFrecency {
    let frecency = SharedFrecency::default();
    let dir = state_dir().join("frecency");
    let _ = std::fs::create_dir_all(&dir);
    let hash = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        root.hash(&mut h);
        h.finish()
    };
    let path = dir.join(format!("frecency-{hash:016x}"));
    match FrecencyTracker::open(&path) {
        Ok(tracker) => {
            if let Err(e) = frecency.init(tracker) {
                tracing::warn!(?e, "frecency init failed; using in-memory");
            }
            frecency
        }
        Err(e) => {
            tracing::warn!(?e, root = %root.display(), "frecency open failed; using in-memory");
            frecency
        }
    }
}

/// Build the persistent per-root `FilePicker`s. Called once at startup; the
/// indexes are kept warm for the daemon's lifetime (bounded by
/// `ROOT_CACHE_BYTES` and carrying FFF watches + frecency). The matching
/// `SharedFrecency` clones are returned alongside so the daemon can record
/// launcher opens (driving the "frequent" half of frecency ranking).
pub(super) fn build_root_pickers(
    roots: &[PathBuf],
) -> (Vec<SharedFilePicker>, Vec<(PathBuf, SharedFrecency)>) {
    let mut pickers = Vec::new();
    let mut frecencies = Vec::new();
    for root in roots {
        let shared = SharedFilePicker::default();
        let frecency = open_frecency(root);
        let home = super::path::is_home_root(root);
        let res = FilePicker::new_with_shared_state(
            shared.clone(),
            frecency.clone(),
            FilePickerOptions {
                base_path: root.display().to_string(),
                mode: FFFMode::Neovim,
                watch: true,
                enable_home_dir_scanning: home,
                enable_fs_root_scanning: true,
                enable_mmap_cache: false,
                enable_content_indexing: false,
                follow_symlinks: false,
                cache_budget: ContentCacheBudget::from_overrides(0, ROOT_CACHE_BYTES, 0),
            },
        );
        match res {
            Ok(()) => {
                pickers.push(shared);
                frecencies.push((root.clone(), frecency));
            }
            Err(e) => tracing::warn!(%e, root = %root.display(), "file index failed"),
        }
    }
    (pickers, frecencies)
}

pub(super) fn picker_loop(
    pickers: Vec<SharedFilePicker>,
    qrx: Receiver<(u64, String)>,
    rtx: Sender<(u64, Vec<FileHit>)>,
    ctrl: Receiver<()>,
    opts: FilesOptions,
) {
    let mut regex_caches = RegexCaches::default();
    tracing::info!(roots = pickers.len(), "file index started");

    let parser = QueryParser::default();
    let mut transient: HashMap<PathBuf, SharedFilePicker> = HashMap::new();
    let mut transient_order: VecDeque<PathBuf> = VecDeque::with_capacity(TRANSIENT_DIR_CAP);
    let mut prev_raw = String::new();
    loop {
        // Reclaim memory on dismiss by dropping the per-directory scratch
        // indexes only. The root indexes stay warm (bounded by ROOT_CACHE_BYTES,
        // with live FFF watches + frecency), so the next open is fast and we
        // avoid a full filesystem walk here. Multiple queued clear signals just
        // repeat this cheap transient drop.
        while ctrl.try_recv().is_ok() {
            tracing::debug!("clearing transient file caches on dismiss");
            transient.clear();
            transient_order.clear();
            // Drop the compiled regex cache too, so a session's worst pattern
            // doesn't pin its regex engine buffers for the daemon's lifetime.
            regex_caches = RegexCaches::default();
            // Hand freed pages back to the OS so RSS actually falls after a
            // heavy session instead of plateauing at the peak.
            unsafe { libc::malloc_trim(0) };
        }
        // Block for the next query, but wake periodically so a clear signal
        // isn't starved while the launcher is idle.
        let first = match qrx.recv_timeout(CTRL_POLL) {
            Ok(f) => f,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        // Coalesce everything that arrived since `first`. If more than one
        // query is in flight it's a typing burst, so debounce once and take the
        // newest query; a lone query needs no artificial wait.
        let (latest, n) = coalesce(&qrx, first);
        let (seq, raw) = if n > 1 {
            thread::sleep(QUERY_DEBOUNCE);
            coalesce(&qrx, latest).0
        } else {
            latest
        };
        // An empty query is a browse: `search_all` returns frecency-ranked
        // files (fff-search short-circuits to `score_filtered_by_frecency`), so
        // the Files list shows "recent and frequent" without typing.
        // Scratch pickers serve a single query lineage (refining or backspacing
        // within one path). An unrelated query invalidates them, so drop them
        // instead of letting up to 8 stale subtree indexes linger for the rest
        // of the session.
        if !(raw.starts_with(&prev_raw) || prev_raw.starts_with(&raw)) {
            transient.clear();
            transient_order.clear();
        }
        let hits = search_all(
            &pickers,
            &mut transient,
            &mut transient_order,
            &parser,
            &raw,
            &opts,
            &mut regex_caches,
        );
        prev_raw = raw;
        if rtx.send((seq, hits)).is_err() {
            return;
        }
    }
}

fn is_lockfile(path: &std::path::Path) -> bool {
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
    )
}

fn coalesce(qrx: &Receiver<(u64, String)>, first: (u64, String)) -> ((u64, String), usize) {
    let mut latest = first;
    let mut count = 1;
    loop {
        match qrx.try_recv() {
            Ok(next) => {
                latest = next;
                count += 1;
            }
            Err(TryRecvError::Empty) => return (latest, count),
            Err(TryRecvError::Disconnected) => return (latest, count),
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
    regex_caches: &mut RegexCaches,
) -> Vec<FileHit> {
    let (pattern, regex) = resolve_regex(&mut regex_caches.main, raw, opts.regex);
    let fff_query = if regex.is_some() {
        regex_hint(&pattern)
    } else {
        raw.to_string()
    };
    let mut merged: Vec<Vec<(i32, FileHit)>> = pickers
        .iter()
        .map(|shared| search_one(shared, parser, &fff_query, &regex, opts.index_lockfiles))
        .collect();

    if let Some((dir, term)) = path_query_dir(raw) {
        // LRU eviction: keep at most TRANSIENT_DIR_CAP per-directory indexes.
        if let Some(pos) = transient_order.iter().position(|p| *p == dir) {
            transient_order.remove(pos);
        } else if transient_order.len() >= TRANSIENT_DIR_CAP
            && let Some(old) = transient_order.pop_front()
        {
            transient.remove(&old);
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
                    cache_budget: ContentCacheBudget::from_overrides(
                        FILE_CACHE_BUDGET,
                        TRANSIENT_CACHE_BYTES,
                        0,
                    ),
                },
            );
            shared
        });
        let (t_pat, t_re) = resolve_regex(&mut regex_caches.term, &term, opts.regex);
        let t_fff = if t_re.is_some() {
            regex_hint(&t_pat)
        } else {
            term
        };

        merged.push(search_one(
            shared,
            parser,
            &t_fff,
            &t_re,
            opts.index_lockfiles,
        ));
    }

    merge_scored(merged, MAX_FILE_RESULTS)
}

/// Merge per-root scored hits into one globally score-ordered list.
fn merge_scored(merged: Vec<Vec<(i32, FileHit)>>, cap: usize) -> Vec<FileHit> {
    let mut all: Vec<(i32, FileHit)> = merged.into_iter().flatten().collect();
    all.sort_by_key(|a| std::cmp::Reverse(a.0));

    if all.len() > cap {
        all.truncate(cap);
    }

    all.into_iter().map(|(_, h)| h).collect()
}

fn search_one(
    shared: &SharedFilePicker,
    parser: &QueryParser<fff_search::FileSearchConfig>,
    fff_query: &str,
    regex: &Option<Regex>,
    index_lockfiles: bool,
) -> Vec<(i32, FileHit)> {
    let Ok(guard) = shared.read() else {
        return Vec::new();
    };
    let Some(p) = guard.as_ref() else {
        return Vec::new();
    };
    let query = parser.parse(fff_query);
    let needle_chars: Vec<char> = fff_query.to_lowercase().chars().collect();
    let results = p.fuzzy_search(
        &query,
        None,
        FuzzySearchOptions {
            pagination: PaginationArgs {
                offset: 0,
                limit: MAX_FILE_RESULTS,
            },
            ..Default::default()
        },
    );
    let base = p.base_path.clone();

    // Regex mode: FFF only narrows by the hint; the compiled regex is the real
    // filter, matched against the absolute path.
    if let Some(re) = regex {
        return results
            .items
            .iter()
            .map(|item| {
                (
                    0i32,
                    FileHit {
                        path: std::sync::Arc::from(item.absolute_path(p, &base)),
                    },
                )
            })
            .filter(|(_, h)| {
                (index_lockfiles || !is_lockfile(&h.path)) && re.is_match(&h.path.to_string_lossy())
            })
            .take(PER_ROOT_ROWS)
            .collect();
    }

    // Normal mode: subsequence match + score ranking, so a tight match
    // outranks a boundary-rich spray.
    let mut scored: Vec<(i32, FileHit)> = results
        .items
        .iter()
        .map(|item| FileHit {
            path: std::sync::Arc::from(item.absolute_path(p, &base)),
        })
        .filter(|h| index_lockfiles || !is_lockfile(&h.path))
        .filter_map(|h| {
            let p_lc = h.path.to_string_lossy().to_lowercase();
            let name_lc = h
                .path
                .file_name()
                .map(|n| n.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            let s_path = subsequence_score_chars(&needle_chars, &p_lc);
            let s_name = subsequence_score_chars(&needle_chars, &name_lc);
            let score = match (s_path, s_name) {
                (Some(a), Some(b)) => a.max(b),
                (Some(a), None) => a,
                (None, Some(b)) => b,
                (None, None) => return None,
            };
            Some((score, h))
        })
        .collect();

    let k = PER_ROOT_ROWS;
    if scored.len() > k {
        scored.select_nth_unstable_by_key(k, |a| std::cmp::Reverse(a.0));
        scored.truncate(k);
    }

    scored.sort_by_key(|a| std::cmp::Reverse(a.0));
    if scored.len() > k {
        scored.truncate(k);
    }

    scored
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::Arc;

    fn hits(names: &[&str]) -> Vec<(i32, FileHit)> {
        names
            .iter()
            .map(|n| {
                (
                    0i32,
                    FileHit {
                        path: Arc::from(PathBuf::from(n)),
                    },
                )
            })
            .collect()
    }

    #[test]
    fn merge_one_short_root_does_not_hang() {
        let merged = vec![hits(&["a", "b"])];
        let out = merge_scored(merged, 16);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn merge_scored_orders_by_score_across_roots() {
        // A lower-scored hit in an earlier root must not beat a higher-scored
        // hit in a later root (the old round-robin merge did exactly that).
        let merged = vec![
            vec![(
                5,
                FileHit {
                    path: Arc::from(PathBuf::from("a1")),
                },
            )],
            vec![(
                3,
                FileHit {
                    path: Arc::from(PathBuf::from("a2")),
                },
            )],
            vec![(
                1,
                FileHit {
                    path: Arc::from(PathBuf::from("a3")),
                },
            )],
            vec![(
                4,
                FileHit {
                    path: Arc::from(PathBuf::from("b1")),
                },
            )],
        ];
        let out = merge_scored(merged, 4);
        let names: Vec<_> = out.iter().map(|h| h.path.to_str().unwrap()).collect();
        assert_eq!(names, ["a1", "b1", "a2", "a3"]);
    }

    #[test]
    fn merge_without_small_cap_keeps_every_root_hit() {
        let merged = vec![hits(&["a1", "a2"]), hits(&["b1", "b2", "b3"])];
        let out = merge_scored(merged, 200);
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
        assert!(!is_lockfile(Path::new("/p/go.mod")));
        assert!(is_lockfile(Path::new("/p/go.sum")));
    }

    #[test]
    fn transient_path_search_returns_hits() {
        let base = std::env::temp_dir().join(format!("awari_transient_{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("aw_foo.rs"), b"x").unwrap();
        std::fs::write(base.join("bar.txt"), b"y").unwrap();
        let nested = base.join("sub");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("aw_nested.rs"), b"z").unwrap();

        let pickers: Vec<SharedFilePicker> = Vec::new();
        let mut transient: HashMap<PathBuf, SharedFilePicker> = HashMap::new();
        let mut transient_order: VecDeque<PathBuf> = VecDeque::new();
        let parser = QueryParser::<fff_search::FileSearchConfig>::default();
        let mut caches = RegexCaches {
            main: None,
            term: None,
        };
        let opts = FilesOptions {
            index_lockfiles: false,
            regex: false,
        };

        // First call builds the transient picker (async index); second call
        // after the index settles should return hits.
        let _ = search_all(
            &pickers,
            &mut transient,
            &mut transient_order,
            &parser,
            &base.display().to_string(),
            &opts,
            &mut caches,
        );
        std::thread::sleep(std::time::Duration::from_millis(2000));
        let browse = search_all(
            &pickers,
            &mut transient,
            &mut transient_order,
            &parser,
            &base.display().to_string(),
            &opts,
            &mut caches,
        );
        let term_q = format!("{}/aw", base.display());
        let term = search_all(
            &pickers,
            &mut transient,
            &mut transient_order,
            &parser,
            &term_q,
            &opts,
            &mut caches,
        );
        let _ = std::fs::remove_dir_all(&base);
        assert!(
            !browse.is_empty() || !term.is_empty(),
            "transient path search returned nothing for {}",
            base.display()
        );
    }
}
