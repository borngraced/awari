//! File source: in-process `fff-search` (`FilePicker`), one picker per
//! configured root. One thread owns all pickers; the daemon sends queries,
//! results come back tagged with a sequence number so stale answers drop.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::thread;
use std::time::Duration;

use regex::Regex;

use fff_search::{
    ContentCacheBudget, FFFMode, FilePicker, FilePickerOptions, FuzzySearchOptions, PaginationArgs,
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

/// Byte cap applied to transient per-directory indexes. Pure insurance: these
/// pickers run with content indexing and mmap caching disabled (we never
/// search inside files), so this budget is currently inert. It only becomes
/// meaningful if that ever changes.
const TRANSIENT_CACHE_BYTES: u64 = 2 * 1024 * 1024;

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
    pub path: Arc<Path>,
}

/// `~`, `/`, `.`, or any path separator — files win ranking for these.
pub fn is_path_shaped(query: &str) -> bool {
    let q = query.trim();
    q.starts_with('~') || q.starts_with('.') || q.contains('/')
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// If `raw` is a path-shaped query, return the directory to browse and the
/// fragment to match within it. When the full path is itself an existing
/// directory (e.g. `~/dev` or `~/dev/`), browse its contents; otherwise search
/// its parent for the trailing segment (e.g. `~/dev` where `dev` does not yet
/// exist, or `~/dev/aw`). Returns `None` if neither the path nor its parent is
/// a real directory.
fn path_query_dir(raw: &str) -> Option<(PathBuf, String)> {
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
    if expanded.is_dir() {
        return Some((expanded, String::new()));
    }
    // Otherwise treat it as a partial: search the parent for the trailing
    // segment. Only when the parent exists and isn't the filesystem root
    // (scanning `/` would be pathological).
    let parent = expanded.parent().filter(|p| !p.as_os_str().is_empty())?;
    if !parent.is_dir() || parent.as_os_str() == "/" {
        return None;
    }
    let term = expanded
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    Some((parent.to_path_buf(), term))
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
        (
            Self {
                tx: qtx,
                ctrl: ctrl_tx,
                seq: 0,
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
    let mut regex_caches = RegexCaches::default();
    tracing::info!(roots = pickers.len(), "file index started");

    let parser = QueryParser::default();
    let mut transient: HashMap<PathBuf, SharedFilePicker> = HashMap::new();
    let mut transient_order: VecDeque<PathBuf> = VecDeque::with_capacity(TRANSIENT_DIR_CAP);
    let mut prev_raw = String::new();
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
            // Freed pages otherwise linger in glibc arenas; hand them back so
            // RSS actually falls after a heavy session instead of plateauing
            // at the peak.
            unsafe { libc::malloc_trim(0) };
        }
        // Block for the next query, but wake periodically so a clear signal
        // isn't starved while the launcher is idle.
        let first = match qrx.recv_timeout(CTRL_POLL) {
            Ok(f) => f,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        // Coalesce drains everything that arrived since `first`. If more than
        // one query is in flight it's a typing burst, so debounce once and
        // then take the newest query; a lone query needs no artificial wait.
        let (latest, n) = coalesce(&qrx, first);
        let (seq, raw) = if n > 1 {
            thread::sleep(QUERY_DEBOUNCE);
            coalesce(&qrx, latest).0
        } else {
            latest
        };
        if raw.trim().is_empty() {
            continue;
        }
        // Scratch pickers serve a single query lineage (refining or
        // backspacing within one path). An unrelated query invalidates them,
        // so drop them instead of letting up to 8 stale subtree indexes
        // linger for the rest of the session.
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

/// Resolve whether `raw` is a regex query and compile it, memoizing the last
/// compiled pattern so identical re-queries within a session skip
/// recompilation. The `r:` prefix forces regex mode per-query; otherwise only
/// the `files.regex` config does. The compile otherwise runs on every
/// keystroke in regex mode.
#[derive(Default)]
struct RegexCaches {
    main: Option<(String, Regex)>,
    term: Option<(String, Regex)>,
}

fn resolve_regex(
    cache: &mut Option<(String, Regex)>,
    raw: &str,
    global_regex: bool,
) -> (String, Option<Regex>) {
    let (pattern, want) = if let Some(p) = raw.strip_prefix("r:") {
        (p.to_string(), true)
    } else if global_regex {
        (raw.to_string(), true)
    } else {
        (raw.to_string(), false)
    };
    if !want {
        *cache = None;
        return (pattern, None);
    }
    if let Some((prev, re)) = cache
        && *prev == pattern
    {
        return (pattern, Some(re.clone()));
    }
    match Regex::new(&pattern) {
        Ok(re) => {
            *cache = Some((pattern.clone(), re.clone()));
            (pattern, Some(re))
        }
        Err(e) => {
            tracing::debug!(%e, "regex compile failed; ignoring regex filter");
            *cache = None;
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

/// Subsequence fuzzy matcher. The query must be a *subsequence* of the
/// candidate (every query char present, in order) — this alone excludes the
/// loose fuzzy hits fff produces (`heap` never matches `head_formatter`, which
/// has no `p`; `head` never matches `readme`, which has no `h`). Among true
/// matches, a subsequence score ranks them: consecutive matches, matches at
/// word boundaries (after `/`, `.`, `_`, `-`, …) and camelCase transitions
/// score highest, with small penalties for leading/trailing unmatched chars.
const SCORE_CONSECUTIVE: i32 = 10;
const SCORE_WORD: i32 = 30;
const SCORE_CAPITAL: i32 = 15;
const SCORE_DOT: i32 = 8;
const SCORE_LEAD: i32 = 1;
const SCORE_TRAIL: i32 = 1;

fn is_word_boundary(c: char) -> bool {
    matches!(
        c,
        ':' | '/' | '\\' | '.' | '-' | '_' | ' ' | '`' | '(' | ')' | '[' | ']' | '@' | '#' | '~'
    )
}

/// Best subsequence score of `needle` within `haystack`, or `None` if `needle`
/// is not a subsequence of `haystack`. Case-insensitive.
pub fn subsequence_score(needle: &str, haystack: &str) -> Option<i32> {
    let needle: Vec<char> = needle.chars().collect();
    subsequence_score_chars(&needle, haystack)
}

/// Core of [`subsequence_score`] with the needle precomputed as `&[char]` so
/// the hot file-search path builds it once and reuses it across every hit.
/// The haystack is scanned char-by-char with a one-char lookback instead of
/// materializing a `Vec<char>`, so no per-call heap allocation for it.
fn subsequence_score_chars(needle: &[char], haystack: &str) -> Option<i32> {
    let n = needle.len();
    if n == 0 {
        return Some(0);
    }
    let m = haystack.chars().count();
    if n > m {
        return None;
    }
    // prev[j]: best score matching the first (i-1) needle chars ending at
    // haystack position (j-1), carried forward as a running max. prev_exact[j]
    // is the same score but only when a match ends EXACTLY at j — it is never
    // carried forward, so it records the real last-matched position. The gapped
    // path uses the carried `prev` (best so far); the consecutive path uses
    // `prev_exact` so a carried plateau can't fake adjacency.
    let mut prev = vec![i32::MIN; m + 1];
    let mut prev_exact = vec![i32::MIN; m + 1];
    let mut cur = vec![i32::MIN; m + 1];
    let mut cur_exact = vec![i32::MIN; m + 1];
    for i in 1..=n {
        cur.fill(i32::MIN);
        cur_exact.fill(i32::MIN);
        let mut best_prev_excl = i32::MIN; // max prev[k] for k < j-1 (gapped path)
        let nc = needle[i - 1].to_ascii_lowercase();
        let mut prev_c: Option<char> = None;
        let mut j = 0;
        for c in haystack.chars() {
            j += 1;
            // Skip this haystack char: carry forward the best ending at <= j-1.
            cur[j] = cur[j - 1];
            let hc = c.to_ascii_lowercase();
            if nc == hc {
                let boundary = j == 1 || prev_c.is_some_and(is_word_boundary);
                let bonus = if boundary {
                    SCORE_WORD
                } else if c.is_ascii_uppercase() {
                    SCORE_CAPITAL
                } else {
                    SCORE_DOT
                };
                if i == 1 {
                    let s = bonus - (j as i32 - 1) * SCORE_LEAD;
                    if s > cur[j] {
                        cur[j] = s;
                        cur_exact[j] = s;
                    }
                } else {
                    // Consecutive: previous needle char matched EXACTLY at j-1.
                    if prev_exact[j - 1] != i32::MIN {
                        let s = prev_exact[j - 1] + SCORE_CONSECUTIVE;
                        if s > cur[j] {
                            cur[j] = s;
                            cur_exact[j] = s;
                        }
                    }
                    // Gapped: previous char matched somewhere before j-2.
                    if best_prev_excl != i32::MIN {
                        let s = best_prev_excl + bonus;
                        if s > cur[j] {
                            cur[j] = s;
                            cur_exact[j] = s;
                        }
                    }
                }
            }
            // Expose prev[j-1] to the next column's gapped path.
            if prev[j - 1] != i32::MIN && prev[j - 1] > best_prev_excl {
                best_prev_excl = prev[j - 1];
            }
            prev_c = Some(c);
        }
        // This row becomes prev for the next needle char; reuse the buffers.
        std::mem::swap(&mut prev, &mut cur);
        std::mem::swap(&mut prev_exact, &mut cur_exact);
    }
    // Only a full match (the final row) counts. The real last-matched position
    // is the highest j with an exact match there; penalize the unmatched tail.
    let last_match = prev_exact
        .iter()
        .enumerate()
        .skip(1)
        .filter(|(_, v)| **v != i32::MIN)
        .map(|(j, _)| j as i32)
        .max()
        .unwrap_or(0);
    let best = if last_match == 0 {
        i32::MIN
    } else {
        prev[m] - (m as i32 - last_match) * SCORE_TRAIL
    };
    (best != i32::MIN).then_some(best)
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
    let mut merged: Vec<Vec<FileHit>> = pickers
        .iter()
        .map(|shared| search_one(shared, parser, &fff_query, &regex, opts.index_lockfiles))
        .collect();
    // When the query points at a real directory (e.g. `~/dev/`), search it
    // directly so path navigation reaches places outside the configured roots.
    // Only the trailing filename segment is fuzzy-matched, not the dir prefix.
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
    let q_lc = fff_query.to_lowercase();
    let needle_chars: Vec<char> = q_lc.chars().collect();
    let fff_limit = PER_ROOT_ROWS * 2;
    let results = p.fuzzy_search(
        &query,
        None,
        FuzzySearchOptions {
            pagination: PaginationArgs {
                offset: 0,
                limit: fff_limit,
            },
            ..Default::default()
        },
    );
    let base = p.base_path.clone();

    // Regex mode: FFF only narrows by the hint; the compiled regex is the
    // real filter, matched against the absolute path.
    if let Some(re) = regex {
        return results
            .items
            .iter()
            .map(|item| FileHit {
                path: Arc::from(item.absolute_path(p, &base)),
            })
            .filter(|h| {
                (index_lockfiles || !is_lockfile(&h.path)) && re.is_match(&h.path.to_string_lossy())
            })
            .take(PER_ROOT_ROWS)
            .collect();
    }

    // Normal mode: subsequence match + score ranking.
    let mut scored: Vec<(i32, FileHit)> = results
        .items
        .iter()
        .map(|item| FileHit {
            path: Arc::from(item.absolute_path(p, &base)),
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
    scored
        .into_iter()
        .map(|(_, h)| h)
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
pub(crate) fn resolve_terminal() -> Option<String> {
    static CACHED: OnceLock<Option<String>> = OnceLock::new();
    CACHED
        .get_or_init(|| {
            if let Ok(term) = std::env::var("TERMINAL")
                && !term.trim().is_empty()
            {
                return Some(term.trim().to_string());
            }
            let paths = std::env::var("PATH").ok()?;
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
        "gnome-terminal" => vec![
            "--".to_string(),
            "sh".to_string(),
            "-c".to_string(),
            script.to_string(),
        ],
        _ => vec![
            "-e".to_string(),
            "sh".to_string(),
            "-c".to_string(),
            script.to_string(),
        ],
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
                path: Arc::from(PathBuf::from(n)),
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
        let mut cache = None;
        // `r:` prefix forces regex and strips the prefix.
        let (pat, re) = resolve_regex(&mut cache, "r:foo", false);
        assert_eq!(pat, "foo");
        assert!(re.is_some());
        // No prefix and global off → plain fuzzy (no regex).
        assert!(resolve_regex(&mut cache, "foo", false).1.is_none());
        // Global on → regex even without prefix.
        assert!(resolve_regex(&mut cache, "foo", true).1.is_some());
        // Invalid pattern → falls back to no regex rather than panicking.
        assert!(resolve_regex(&mut cache, "r:[", false).1.is_none());
    }

    #[test]
    fn regex_hint_strips_metacharacters() {
        assert_eq!(regex_hint(r"\.rs$"), ".rs");
        assert_eq!(regex_hint(r"src/.*\.rs"), "src/..rs");
    }

    #[test]
    fn path_query_dir_resolves_existing_directory() {
        let base = std::env::temp_dir().join(format!("awari_pathq_existing_{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        // Trailing slash: browse the directory's contents (empty fragment).
        let got = path_query_dir(&format!("{}/", base.display()));
        // No trailing slash on an existing directory: still browse it.
        let got2 = path_query_dir(&base.display().to_string());
        let _ = std::fs::remove_dir_all(&base);
        let (dir, frag) = got.expect("existing directory with trailing slash should resolve");
        assert_eq!(
            dir.as_os_str()
                .to_string_lossy()
                .trim_end_matches('/')
                .to_string(),
            base.to_string_lossy().to_string(),
            "resolved dir should match the queried directory"
        );
        assert_eq!(frag, "", "browsing an existing dir uses an empty fragment");
        let (dir2, frag2) = got2.expect("existing directory without slash should resolve");
        assert_eq!(dir2, base, "no-slash existing dir resolves to itself");
        assert_eq!(frag2, "", "no-slash existing dir uses an empty fragment");
    }

    #[test]
    fn path_query_dir_resolves_parent_for_partial() {
        let base = std::env::temp_dir().join(format!("awari_pathq_partial_{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let got = path_query_dir(&format!("{}/aw", base.display()));
        let _ = std::fs::remove_dir_all(&base);
        let (dir, frag) = got.expect("partial under an existing dir should resolve");
        assert_eq!(dir, base, "partial searches the parent directory");
        assert_eq!(frag, "aw", "partial matches the trailing segment");
    }

    #[test]
    fn path_query_dir_ignores_missing_and_plain() {
        assert!(path_query_dir("firefox").is_none());
        assert!(path_query_dir("/nonexistent_awari_xyz_123/").is_none());
    }

    #[test]
    fn subsequence_excludes_loose_fuzzy_hits() {
        // The two real complaints: fff's typo/subsequence fuzz returned these
        // for "heap"/"head", but neither is a subsequence, so the matcher drops them.
        assert!(subsequence_score("heap", "head_formatter").is_none());
        assert!(subsequence_score("head", "readme.md").is_none());
        assert!(subsequence_score("head", "readme").is_none());
    }

    #[test]
    fn subsequence_keeps_real_subsequence_matches() {
        assert!(subsequence_score("heap", "heaptrace.rs").is_some());
        assert!(subsequence_score("head", "head.rs").is_some());
        assert!(subsequence_score("head", "src/headless.rs").is_some());
    }

    #[test]
    fn subsequence_consecutive_requires_adjacency() {
        // "ab" in "axb" is gapped (a at 1, x at 2, b at 3): the consecutive
        // bonus must NOT fire across the gap. "ab" in "abx" is truly
        // consecutive (a at 1, b at 2). The gapped match scores below the
        // consecutive one, and an exact (no trailing) match above both.
        let gapped = subsequence_score("ab", "axb").unwrap();
        let consecutive = subsequence_score("ab", "abx").unwrap();
        let exact = subsequence_score("ab", "ab").unwrap();
        assert!(gapped < consecutive, "{gapped} < {consecutive}");
        assert!(consecutive < exact, "{consecutive} < {exact}");
    }

    #[test]
    fn subsequence_ranks_shorter_paths_higher() {
        let bare = subsequence_score("head", "head.rs").unwrap();
        // Same basename deeper in the tree pays leading/trailing penalties.
        let nested = subsequence_score("head", "src/deep/nested/head.rs").unwrap();
        assert!(bare > nested);
        // A late, deeply-prefixed match ranks below a filename-length one.
        let late = subsequence_score("head", "zzz/verylongpath/head.rs").unwrap();
        assert!(bare > late);
    }

    #[test]
    fn subsequence_empty_query_matches_everything() {
        assert_eq!(subsequence_score("", "anything"), Some(0));
    }

    /// Temporary diagnostic: hammer the worker with distinct plain queries and
    /// print RSS at checkpoints. Run with:
    ///   cargo test -p awari --release rss_files -- --ignored --nocapture
    #[test]
    #[ignore]
    fn rss_files_plain_queries() {
        use std::time::Duration;
        fn rss_kb() -> i64 {
            std::fs::read_to_string("/proc/self/statm")
                .ok()
                .and_then(|s| {
                    s.split_whitespace()
                        .nth(1)
                        .and_then(|v| v.parse::<i64>().ok())
                })
                .map(|p| p * 4)
                .unwrap_or(0)
        }
        let root = std::path::Path::new("/tmp/opencode/rssfix");
        for d in ["a", "b", "c", "d"] {
            let dir = root.join(d);
            std::fs::create_dir_all(&dir).unwrap();
            for i in 0..300u32 {
                let _ = std::fs::write(dir.join(format!("file_{d}_{i}.txt")), "x");
            }
        }
        let (mut files, rx) = Files::spawn(
            vec![root.to_path_buf()],
            FilesOptions {
                index_lockfiles: false,
                regex: false,
            },
        );
        let drain = std::thread::spawn(move || for _ in rx {});
        // Let the picker finish its initial walk so the scan itself isn't
        // conflated with per-query growth.
        files.query("warm");
        std::thread::sleep(Duration::from_millis(1500));
        let base = rss_kb();
        eprintln!("base rss = {base} KB");
        for i in 0..2000u64 {
            files.query(&format!("zz{}x{}", i % 977, i));
            if i % 400 == 399 {
                std::thread::sleep(Duration::from_millis(120));
                eprintln!("after {} queries: rss = {} KB", i + 1, rss_kb());
            }
        }
        std::thread::sleep(Duration::from_millis(500));
        let end = rss_kb();
        eprintln!("end rss = {end} KB (delta {} KB)", end - base);
        // Phase 2: cycle MORE distinct path dirs than TRANSIENT_DIR_CAP (8),
        // forcing constant picker eviction + re-walk churn.
        let pbase = rss_kb();
        eprintln!("path-phase base rss = {pbase} KB");
        for i in 0..600u64 {
            let d = i % 16;
            files.query(&format!("/tmp/opencode/rssfix/dir{d}/x{}", i % 13));
            if i % 150 == 149 {
                std::thread::sleep(Duration::from_millis(120));
                eprintln!("after {} path queries: rss = {} KB", i + 1, rss_kb());
            }
        }
        std::thread::sleep(Duration::from_millis(500));
        eprintln!(
            "after path phase: rss = {} KB (delta {} KB)",
            rss_kb(),
            rss_kb() - pbase
        );
        files.clear();
        std::thread::sleep(Duration::from_millis(500));
        eprintln!("after clear: rss = {} KB", rss_kb());
        drop(files);
        drain.join().ok();
    }
}
