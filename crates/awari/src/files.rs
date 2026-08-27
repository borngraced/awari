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
    ContentCacheBudget, FFFMode, FilePicker, FilePickerOptions, FrecencyTracker, FuzzySearchOptions,
    PaginationArgs, QueryParser, SharedFilePicker, SharedFrecency,
};

use awari_ipc::state_dir;

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

/// Subsequence fuzzy matcher using the fzy / skim affine-gap Smith–Waterman
/// algorithm. A match must be a subsequence of the
/// candidate. Boundary/capital bonuses are awarded only for the leading char and
/// tight (consecutive) matches, never for scattered gaps — otherwise a
/// boundary-rich haystack could stack bonuses across a meaningless spray.
const FZY_MATCH: i32 = 16;
const FZY_GAP_START: i32 = -3;
const FZY_GAP_EXTEND: i32 = -1;
const FZY_BONUS_HEAD: i32 = FZY_MATCH / 2; // 8: start of word / after hard sep
const FZY_BONUS_BREAK: i32 = FZY_MATCH / 2 + FZY_GAP_EXTEND; // 7: after soft sep
const FZY_BONUS_CAMEL: i32 = FZY_MATCH / 2 + 2 * FZY_GAP_EXTEND; // 6: camelCase
const FZY_BONUS_CONSECUTIVE: i32 = -(FZY_GAP_START + FZY_GAP_EXTEND); // 4
const FZY_FIRST_CHAR_MULT: i32 = 2;
const FZY_CASE_MISMATCH: i32 = FZY_GAP_EXTEND * 2; // -2
const FZY_NEG_INF: i32 = i32::MIN / 2;

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
    frecencies: Vec<(PathBuf, SharedFrecency)>,
}

impl Files {
    /// One worker indexes every root; empty roots disable the source.
    pub fn spawn(roots: Vec<PathBuf>, opts: FilesOptions) -> (Self, Receiver<(u64, Vec<FileHit>)>) {
        let (qtx, qrx) = std::sync::mpsc::channel::<(u64, String)>();
        let (rtx, rrx) = std::sync::mpsc::channel();
        let (ctrl_tx, ctrl_rx) = std::sync::mpsc::channel::<()>();
        let (pickers, frecencies) = build_root_pickers(&roots);
        if !pickers.is_empty() {
            thread::Builder::new()
                .name("awari-files".into())
                .spawn(move || picker_loop(pickers, qrx, rtx, ctrl_rx, opts))
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

    /// Reclaim file-search memory: drops the per-directory scratch indexes
    /// kept during path navigation. Root indexes are bounded and kept warm,
    /// so idle RAM stays near baseline without a re-index walk. Call on dismiss.
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
        if let Some(frec) = best {
            if let Ok(mut g) = frec.write() {
                if let Some(tracker) = g.as_mut() {
                    if let Err(e) = tracker.track_access(path) {
                        tracing::debug!(?e, "frecency track_access failed");
                    }
                }
            }
        }
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

/// Open a persistent frecency store for a root. Falls back to an in-memory
/// tracker if the on-disk LMDB can't be created, so ranking still works
/// (without cross-session frequency) rather than erroring.
fn open_frecency(root: &Path) -> SharedFrecency {
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
fn build_root_pickers(
    roots: &[PathBuf],
) -> (Vec<SharedFilePicker>, Vec<(PathBuf, SharedFrecency)>) {
    let mut pickers = Vec::new();
    let mut frecencies = Vec::new();
    for root in roots {
        let shared = SharedFilePicker::default();
        let frecency = open_frecency(root);
        let home = is_home_root(root);
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

fn picker_loop(
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
        // An empty query is a browse: `search_all` returns frecency-ranked
        // files (fff-search short-circuits to `score_filtered_by_frecency`),
        // so the Files list shows "recent and frequent" without typing.
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

/// Character role classification for the in-place boundary bonus, mirroring the
/// fzy / clangd scheme: the char before and after a match decide how much that
/// match is worth (start of word, after a soft separator, a camelCase bump, or
/// nothing in the middle of a run).
#[derive(Clone, Copy, PartialEq, Eq)]
enum FzyCharType {
    Empty,
    Upper,
    Lower,
    Number,
    HardSep,
    SoftSep,
}

impl FzyCharType {
    fn of(ch: char) -> Self {
        match ch {
            '\0' => FzyCharType::Empty,
            ' ' | '/' | '\\' | '|' | '(' | ')' | '[' | ']' | '{' | '}' => FzyCharType::HardSep,
            '!'..='\'' | '*'..='.' | ':'..='@' | '^'..='`' | '~' => FzyCharType::SoftSep,
            '0'..='9' => FzyCharType::Number,
            'A'..='Z' => FzyCharType::Upper,
            _ => FzyCharType::Lower,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FzyCharRole {
    Head,
    Tail,
    Camel,
    Break,
}

impl FzyCharRole {
    fn of_type(prev: FzyCharType, cur: FzyCharType) -> Self {
        match (prev, cur) {
            (FzyCharType::Empty | FzyCharType::HardSep, _) => FzyCharRole::Head,
            (FzyCharType::SoftSep, _) => FzyCharRole::Break,
            (FzyCharType::Lower | FzyCharType::Number, FzyCharType::Upper) => FzyCharRole::Camel,
            _ => FzyCharRole::Tail,
        }
    }
}

fn fzy_in_place_bonus(prev: FzyCharType, cur: FzyCharType) -> i32 {
    match FzyCharRole::of_type(prev, cur) {
        FzyCharRole::Head => FZY_BONUS_HEAD,
        FzyCharRole::Camel => FZY_BONUS_CAMEL,
        FzyCharRole::Break => FZY_BONUS_BREAK,
        FzyCharRole::Tail => 0,
    }
}

fn fzy_char_eq(c: char, p: char, case_sensitive: bool) -> bool {
    if case_sensitive {
        c == p
    } else {
        c.to_ascii_lowercase() == p.to_ascii_lowercase()
    }
}

/// Best subsequence score of `needle` within `haystack`, or `None` if `needle`
/// is not a subsequence of `haystack`. Case-insensitive unless `needle` contains
    /// an ASCII uppercase letter (smart-case), matching skim behavior.
pub fn subsequence_score(needle: &str, haystack: &str) -> Option<i32> {
    let needle: Vec<char> = needle.chars().collect();
    subsequence_score_chars(&needle, haystack)
}

/// Core of [`subsequence_score`] with the needle precomputed as `&[char]` so
/// the hot file-search path builds it once and reuses it across every hit.
///
/// Implements the fzy / skim affine-gap Smith–Waterman subsequence scorer.
/// Two score matrices are maintained per
/// pattern row: `M` (match ends here) and `P` (gap — char skipped). Gaps use an
/// affine penalty (`GAP_START` once, then `GAP_EXTEND` per extra char), which is
/// what lets a tight match survive one trailing gap while a scattered spray
/// accumulates a penalty per gap.
fn subsequence_score_chars(needle: &[char], haystack: &str) -> Option<i32> {
    let pattern = needle;
    let choice: Vec<char> = haystack.chars().collect();
    if pattern.is_empty() {
        return Some(0);
    }
    if choice.len() < pattern.len() {
        return None;
    }
    let case_sensitive = needle.iter().any(|c| c.is_ascii_uppercase());

    // first_match[i] = earliest choice column where pattern[i] can align. If any
    // pattern char has no candidate, the needle isn't a subsequence at all.
    let mut first_match = vec![0usize; pattern.len()];
    {
        let mut ci = 0usize;
        for (i, &p) in pattern.iter().enumerate() {
            let mut found = None;
            while ci < choice.len() {
                if fzy_char_eq(choice[ci], p, case_sensitive) {
                    found = Some(ci);
                    break;
                }
                ci += 1;
            }
            match found {
                Some(pos) => {
                    first_match[i] = pos;
                    ci = pos + 1;
                }
                None => return None,
            }
        }
    }

    let rows = pattern.len() + 1;
    let cols = choice.len() + 1;
    // Cell state: M score, P score, running consecutive bonus. Indexed by
    // `row * cols + col`. Columns are 1-indexed into `choice` (col 0 is the
    // empty prefix); `choice[col - 1]` is the char for column `col`.
    let mut m = vec![FZY_NEG_INF; rows * cols];
    let mut p = vec![FZY_NEG_INF; rows * cols];
    let mut bonus = vec![0i32; rows * cols];

    // In-place boundary bonus per choice column (1-indexed). The first column is
    // doubled, mirroring skim's preference for the leading pattern char.
    let mut in_bonus = vec![0i32; cols];
    {
        let mut prev_ch = '\0';
        for (j, &c) in choice.iter().enumerate() {
            in_bonus[j + 1] = fzy_in_place_bonus(FzyCharType::of(prev_ch), FzyCharType::of(c));
            prev_ch = c;
        }
        in_bonus[1] *= FZY_FIRST_CHAR_MULT;
    }

    // Row 0: no pattern consumed yet. P[0][j] = GAP_EXTEND (a skipped prefix);
    // M[0][j] stays NEG_INF (a match can't end before the pattern starts).
    for j in 0..cols {
        p[j] = FZY_GAP_EXTEND;
    }
    // Reset the starting cell of every pattern row so the DP never reads stale
    // state from a previous reuse of the buffers.
    for (i, &start) in first_match.iter().enumerate() {
        let idx = (i + 1) * cols + (start + 1);
        m[idx] = FZY_NEG_INF;
        p[idx] = FZY_NEG_INF;
        bonus[idx] = 0;
    }

    for (i, &pch) in pattern.iter().enumerate() {
        let row = i + 1;
        let row_prev = i;
        let row_base = row * cols;
        let row_prev_base = row_prev * cols;
        let to_skip = first_match[i];
        for (j, &c_ch) in choice[to_skip..].iter().enumerate() {
            let col = to_skip + j + 1;
            let col_prev = to_skip + j;
            let idx_cur = row_base + col;
            let idx_last = row_base + col_prev;
            let idx_prev = row_prev_base + col_prev;
            let in_place = in_bonus[col];

            // --- M matrix: best alignment ending in a match at (row, col). ---
            if fzy_char_eq(c_ch, pch, case_sensitive) {
                let prev_match = m[idx_prev];
                let prev_skip = p[idx_prev];
                let prev_bonus = bonus[idx_last];
                let match_val = FZY_MATCH
                    + if !case_sensitive && pch != c_ch {
                        FZY_CASE_MISMATCH
                    } else {
                        0
                    };
                let consecutive = prev_bonus.max(in_place).max(FZY_BONUS_CONSECUTIVE);
                bonus[idx_last] = consecutive;
                let score_match = prev_match + consecutive;
                // Boundary/capital bonuses apply only on the tight (`score_match`)
                // path or for the leading char; a scattered (gapped) transition
                // gets no in-place bonus, so boundary-rich haystacks can't stack
                // bonuses across a meaningless spray.
                let score_skip = if i == 0 {
                    prev_skip + in_place
                } else {
                    prev_skip
                };
                if score_match >= score_skip {
                    m[idx_cur] = score_match + match_val;
                } else {
                    m[idx_cur] = score_skip + match_val;
                }
            } else {
                m[idx_cur] = FZY_NEG_INF;
                bonus[idx_cur] = 0;
            }

            // --- P matrix: best alignment with col skipped (gap). Affine gap. ---
            let gap_match = FZY_GAP_START + FZY_GAP_EXTEND + m[idx_last];
            let gap_skip = FZY_GAP_EXTEND + p[idx_last];
            if gap_match >= gap_skip {
                p[idx_cur] = gap_match;
            } else {
                p[idx_cur] = gap_skip;
            }
        }
    }

    // The score is the best M in the final pattern row, from the last char's
    // first feasible column onward. A valid subsequence always yields a finite
    // value here; a degenerate all-NEG_INF means no real match.
    let last_row = pattern.len();
    let first_col = first_match[pattern.len() - 1] + 1;
    let best = m[last_row * cols + first_col..]
        .iter()
        .copied()
        .max()
        .unwrap_or(FZY_NEG_INF);
    (best > FZY_NEG_INF).then_some(best)
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
    merge_scored(merged, cap)
}

/// Merge per-root scored hits into one globally score-ordered list. Each root's
/// hits are already sorted, but the *global* order must also be by score — a
/// lower-scored hit in an earlier root must not beat a higher-scored hit in a
/// later root (that was the bug in the old round-robin merge).
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
            .map(|item| (0i32, FileHit {
                path: Arc::from(item.absolute_path(p, &base)),
            }))
            .filter(|(_, h)| {
                (index_lockfiles || !is_lockfile(&h.path)) && re.is_match(&h.path.to_string_lossy())
            })
            .take(PER_ROOT_ROWS)
            .collect();
    }

    // Normal mode: subsequence match + score ranking. Boundary/capital
    // bonuses only apply to the leading char and consecutive runs (never to
    // scattered gaps), so a tight match outranks a boundary-rich spray.
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
    if scored.len() > k {
        scored.truncate(k);
    }
    scored
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
    let name = Path::new(term)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(term);
    match name {
        "gnome-terminal" | "kitty" => vec![
            "--".to_string(),
            "sh".to_string(),
            "-c".to_string(),
            script.to_string(),
        ],
        "wezterm" | "wezterm-gui" => vec![
            "start".to_string(),
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

    fn hits(names: &[&str]) -> Vec<(i32, FileHit)> {
        names
            .iter()
            .map(|n| (0i32, FileHit {
                path: Arc::from(PathBuf::from(n)),
            }))
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
            hits(&["a1", "a2", "a3"]),
            hits(&["b1"]),
        ];
        // Re-score so b1 outranks a2/a3: give a1..a3 decreasing, b1 high.
        let merged = vec![
            vec![(5, FileHit { path: Arc::from(PathBuf::from("a1")) })],
            vec![(3, FileHit { path: Arc::from(PathBuf::from("a2")) })],
            vec![(1, FileHit { path: Arc::from(PathBuf::from("a3")) })],
            vec![(4, FileHit { path: Arc::from(PathBuf::from("b1")) })],
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
    fn terminal_argv_matches_emulator() {
        assert_eq!(
            terminal_args("wezterm", "ls"),
            vec!["start", "--", "sh", "-c", "ls"]
        );
        assert_eq!(
            terminal_args("/usr/bin/kitty", "ls"),
            vec!["--", "sh", "-c", "ls"]
        );
        assert_eq!(
            terminal_args("alacritty", "ls"),
            vec!["-e", "sh", "-c", "ls"]
        );
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
    fn transient_path_search_returns_hits() {
        use std::collections::VecDeque;
        let base =
            std::env::temp_dir().join(format!("awari_transient_{}", std::process::id()));
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
        let mut caches = RegexCaches { main: None, term: None };
        let opts = FilesOptions {
            index_lockfiles: false,
            regex: false,
        };

        // First call builds the transient picker (async index); second call after
        // the index settles should return hits.
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
        eprintln!(
            "TRANSIENT browse(base)={} term('aw')={}",
            browse.len(),
            term.len()
        );
        let _ = std::fs::remove_dir_all(&base);
        assert!(
            !browse.is_empty() || !term.is_empty(),
            "transient path search returned nothing for {}",
            base.display()
        );
    }

    #[test]
    fn path_query_dir_resolves_existing_directory() {
        let base =
            std::env::temp_dir().join(format!("awari_pathq_existing_{}", std::process::id()));
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
        // "ab" in "axb" is gapped (one skipped char): the gap penalty must push
        // it below the consecutive "ab" in "abx". An exact match "ab" must beat
        // a leading-gap match "xab" (fzy doesn't penalize a trailing gap, only
        // leading/trailing-in-choice gaps via P, so exact == consecutive).
        let gapped = subsequence_score("ab", "axb").unwrap();
        let consecutive = subsequence_score("ab", "abx").unwrap();
        let exact = subsequence_score("ab", "ab").unwrap();
        let leading_gap = subsequence_score("ab", "xab").unwrap();
        assert!(gapped < consecutive, "{gapped} < {consecutive}");
        assert!(exact > leading_gap, "{exact} > {leading_gap}");
    }

    #[test]
    fn subsequence_gap_penalty_beats_scattered_boundaries() {
        // fzy tolerates single-char gaps (a one-gap scatter scores ≈ a tight
        // match), but a *multi-gap* scatter — the pathological boundary-rich
        // spray — is clearly penalized by the affine gap cost.
        let tight = subsequence_score("golang", "golang").unwrap();
        let single_gap = subsequence_score("golang", "g/o/l/a/n/g").unwrap();
        let multi_gap = subsequence_score("golang", "gzzzzozzzlzzzazzznzzzg").unwrap();
        assert!(tight >= single_gap, "{tight} >= {single_gap}");
        assert!(tight > multi_gap, "{tight} > {multi_gap}");
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

    #[test]
    fn subsequence_repro_golang_boundary_spray() {
        // Concrete repro: query "golang" must rank the real goland tarball (a
        // tight "golan" core + one trailing-grab gap) ABOVE a fully-scattered,
        // boundary-rich spray like `G..o..l..a..n..g`, whose letters all land on
        // capitals/dots. The affine-gap scorer keeps the tight match ahead
        // because the spray pays GAP_START per gap.
        let spray = subsequence_score("golang", "G..o..l..a..n..g").unwrap();
        let goland = subsequence_score("golang", "goland-2026.2.0.1.tar.gz").unwrap();
        assert!(
            goland > spray,
            "goland({goland}) must beat spray({spray})"
        );
    }

    #[test]
    fn subsequence_gapped_match_is_not_filtered() {
        // Regression: a match that needs one large trailing gap (grab the final
        // `g` from `.tar.gz`) must still score as a real match, never None.
        // Our previous linear-gap scorer returned None here and dropped the file.
        assert!(subsequence_score("golang", "goland-2026.2.0.1.tar.gz").is_some());
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
