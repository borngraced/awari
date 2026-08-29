//! Manual latency/RSS probes for the file-search worker, compiled only with
//! `--features probe` and driven by `awari probe-files` / `awari probe-typing`.
//! They read the developer's real config and replicate `picker_loop` internals
//! (coalesce, debounce, lineage, `search_all`), so they are profiling tools,
//! not tests — kept out of the `#[test]` tree and the default build.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use fff_search::{FileSearchConfig, QueryParser, SharedFilePicker};

use super::picker::{build_root_pickers, coalesce, search_all, search_one};
use super::regex::RegexCaches;
use super::{CTRL_POLL, FileHit, FilesOptions, MAX_FILE_RESULTS, PER_ROOT_ROWS, QUERY_DEBOUNCE};

type QueryMsg = (u64, String);
type PaintMsg = (u64, Vec<FileHit>, Instant);

fn proc_rss_kb() -> u64 {
    let raw = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    raw.lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|v| v.trim().trim_end_matches(" kB").parse::<u64>().ok())
        .unwrap_or(0)
}

/// Benchmarks the real picker wiring (`search_all`, same code the daemon runs
/// per keystroke, minus the 20 ms typing debounce) against the user's actual
/// configured roots. Reports p50/p99 so the fff cost structure is visible
/// before any optimization.
pub fn files() {
    let cfg = crate::config::load();
    let roots = cfg.files.resolved_roots();
    eprintln!(
        "probe: roots={} {:?}, lockfiles={} regex={}",
        roots.len(),
        roots,
        cfg.files.index_lockfiles,
        cfg.files.regex
    );
    let (pickers, _) = build_root_pickers(&roots, cfg.fff);
    let mut transient: HashMap<PathBuf, SharedFilePicker> = HashMap::new();
    let mut transient_order: VecDeque<PathBuf> = VecDeque::new();
    let parser = QueryParser::<FileSearchConfig>::default();
    let mut caches = RegexCaches {
        main: None,
        term: None,
    };
    let opts = FilesOptions {
        index_lockfiles: cfg.files.index_lockfiles,
        regex: cfg.files.regex,
        fff: cfg.fff,
    };

    eprintln!("RSS after build_root_pickers={}MiB", proc_rss_kb() / 1024);
    for (i, shared) in pickers.iter().enumerate() {
        let total = shared
            .read()
            .ok()
            .and_then(|g| g.as_ref().map(|p| p.live_file_count()))
            .unwrap_or(0);
        eprintln!("  scan root[{i}] {} files", total);
    }

    let rss_before = proc_rss_kb();
    let mut rss_peak = rss_before;

    // Let the async root scans settle before timing (warm index).
    for _ in 0..6 {
        let _ = search_all(
            &pickers,
            &mut transient,
            &mut transient_order,
            &parser,
            "",
            &opts,
            &mut caches,
        );
        thread::sleep(Duration::from_millis(250));
    }

    let battery: &[&str] = &[
        "",
        "main",
        "awari",
        "config",
        "src/app",
        "r:\\.rs$",
        "a",
    ];
    for q in battery {
        let rss_q = proc_rss_kb();
        for _ in 0..4 {
            let _ = search_all(
                &pickers,
                &mut transient,
                &mut transient_order,
                &parser,
                q,
                &opts,
                &mut caches,
            );
        }
        let rss_q1 = proc_rss_kb();
        let _ = search_all(
            &pickers,
            &mut transient,
            &mut transient_order,
            &parser,
            q,
            &opts,
            &mut caches,
        );
        let rss_q2 = proc_rss_kb();
        eprintln!(
            "  battery {q:>12}: rss before={}MiB  after-warm={}MiB  after-first={}MiB (transients={})",
            rss_q / 1024,
            rss_q1 / 1024,
            rss_q2 / 1024,
            transient.len()
        );
        let mut ts: Vec<Duration> = Vec::with_capacity(70);
        for _ in 0..70 {
            let t0 = Instant::now();
            let hits = search_all(
                &pickers,
                &mut transient,
                &mut transient_order,
                &parser,
                q,
                &opts,
                &mut caches,
            );
            ts.push(t0.elapsed());
            let now_rss = proc_rss_kb();
            if now_rss > rss_peak {
                rss_peak = now_rss;
            }
            assert!(hits.len() <= MAX_FILE_RESULTS + PER_ROOT_ROWS);
        }
        ts.sort();
        let p50 = ts[ts.len() / 2];
        let p99 = ts[(ts.len() * 99) / 100];
        eprintln!(
            "{q:>12}: p50={p50:.3?}  p99={p99:.3?}  max={:.3?}",
            ts[ts.len() - 1]
        );
    }

    // Per-root cost attribution: total index size per picker + how a fixed
    // multi-char query scales with candidate count.
    eprintln!("\nper-root attribution (query \"main\", 40 iters):");
    for (i, shared) in pickers.iter().enumerate() {
        let total = shared
            .read()
            .ok()
            .and_then(|g| g.as_ref().map(|p| p.live_file_count()))
            .unwrap_or(0);
        let mut tr: Vec<Duration> = Vec::with_capacity(45);
        for _ in 0..3 {
            let _ = search_one(shared, &parser, "main", &None, opts.index_lockfiles);
        }
        for _ in 0..40 {
            let t0 = Instant::now();
            let hits = search_one(shared, &parser, "main", &None, opts.index_lockfiles);
            tr.push(t0.elapsed());
            assert!(hits.len() <= PER_ROOT_ROWS);
        }
        tr.sort();
        eprintln!(
            "  {:>7} files  root[{i}]={}  p50={:.3?}",
            total,
            cfg.files.resolved_roots()[i].display(),
            tr[tr.len() / 2]
        );
    }

    let rss_after = proc_rss_kb();
    eprintln!(
        "\nsearch worker RSS: before={rss_before}KiB  peak-during-query={rss_peak}KiB  after={rss_after}KiB  (peak delta over baseline: {}MiB)",
        (rss_peak.saturating_sub(rss_before)) / 1024
    );
    eprintln!("transient pickers held: {}", transient.len());
    for (dir, shared) in transient.iter() {
        let total = shared
            .read()
            .ok()
            .and_then(|g| g.as_ref().map(|p| p.live_file_count()))
            .unwrap_or(0);
        eprintln!("  transient {dir:?}: {total} files");
    }
}

/// Simulates real typing through the actual debounced worker path
/// (coalesce -> QUERY_DEBOUNCE on bursts -> lineage -> `search_all` ->
/// async paint), reporting per-burst "last key to paint" latency for both a
/// slow typist (every key becomes its own search) and a fast typist (bursts
/// coalesce into one search after the debounce). This is the latency a user
/// actually perceives; the fixed battery in `files()` measures raw search
/// cost only.
pub fn typing() {
    let cfg = crate::config::load();
    let roots = cfg.files.resolved_roots();
    let (pickers, _) = build_root_pickers(&roots, cfg.fff);
    let parser = QueryParser::<FileSearchConfig>::default();
    let opts = FilesOptions {
        index_lockfiles: cfg.files.index_lockfiles,
        regex: cfg.files.regex,
        fff: cfg.fff,
    };
    let mut caches = RegexCaches::default();

    // Warm the async root scans before timing.
    let mut transient: HashMap<PathBuf, SharedFilePicker> = HashMap::new();
    let mut transient_order: VecDeque<PathBuf> = VecDeque::new();
    for _ in 0..6 {
        let _ = search_all(
            &pickers,
            &mut transient,
            &mut transient_order,
            &parser,
            "",
            &opts,
            &mut caches,
        );
        thread::sleep(Duration::from_millis(250));
    }

    // Worker replica of `picker_loop`: coalesce -> debounce-on-burst ->
    // lineage -> search_all -> paint. Same QUERY_DEBOUNCE/CTRL_POLL rhythm.
    let (qtx, qrx): (Sender<QueryMsg>, Receiver<QueryMsg>) = mpsc::channel();
    let (rtx, rrx): (Sender<PaintMsg>, Receiver<PaintMsg>) = mpsc::channel();
    let worker_pickers = Arc::new(pickers.into_iter().collect::<Vec<_>>());
    let parser_w = QueryParser::<FileSearchConfig>::default();
    let worker = thread::spawn(move || {
        let mut prev_raw = String::new();
        let mut wcaches = RegexCaches::default();
        let mut wtransient: HashMap<PathBuf, SharedFilePicker> = HashMap::new();
        let mut wtransient_order: VecDeque<PathBuf> = VecDeque::new();
        loop {
            let first = match qrx.recv_timeout(CTRL_POLL) {
                Ok(f) => f,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            };
            let ((seq, raw), n) = coalesce(&qrx, first);
            let (seq, raw) = if n > 1 {
                thread::sleep(QUERY_DEBOUNCE);
                coalesce(&qrx, (seq, raw)).0
            } else {
                (seq, raw)
            };
            if !(raw.starts_with(&prev_raw) || prev_raw.starts_with(&raw)) {
                wtransient.clear();
                wtransient_order.clear();
            }
            prev_raw = raw.clone();
            let hits = search_all(
                &worker_pickers,
                &mut wtransient,
                &mut wtransient_order,
                &parser_w,
                &raw,
                &opts,
                &mut wcaches,
            );
            tracing::debug!(seq, %raw, hits = hits.len(), "typed-search-paint");
            if rtx.send((seq, hits, Instant::now())).is_err() {
                break;
            }
        }
    });

    eprintln!("\ntyping simulation through real debounced worker path:");
    let phrases: &[&str] = &["search files", "open app tabs"];
    for phrase in phrases {
        for (cadence_ms, label) in [(15u64, "fast 15ms/key"), (130u64, "slow 130ms/key")] {
            let mut prev = String::new();
            let mut key_send_t: Vec<Instant> = Vec::new();
            let t0 = Instant::now();
            for (ci, ch) in phrase.chars().enumerate() {
                let jitter = if cadence_ms < 20 { 0 } else { (ci % 2) as u64 * 7 };
                thread::sleep(Duration::from_millis(cadence_ms + jitter));
                prev.push(ch);
                key_send_t.push(Instant::now());
                qtx.send(((ci + 1) as u64, prev.clone())).unwrap();
            }
            // Wait for every in-flight key to paint, then attribute each paint
            // to the last key it covers: real "key -> results" latency.
            let mut searches_ran = 0u64;
            let mut gaps: Vec<Duration> = Vec::new();
            while let Ok((paint_seq, _, painted_at)) = rrx.recv_timeout(Duration::from_secs(3)) {
                searches_ran += 1;
                let key_idx = (paint_seq as usize).saturating_sub(1);
                if key_idx < key_send_t.len() {
                    gaps.push(painted_at.saturating_duration_since(key_send_t[key_idx]));
                }
                if paint_seq as usize >= phrase.chars().count() {
                    break;
                }
            }
            let wall = t0.elapsed();
            gaps.sort();
            let p50 = if gaps.is_empty() { Duration::ZERO } else { gaps[gaps.len() / 2] };
            let p90 = if gaps.is_empty() { Duration::ZERO } else { gaps[(gaps.len() * 9) / 10] };
            let last = gaps.last().copied().unwrap_or_default();
            eprintln!(
                "  {phrase:>14} {label}: keys={:>2} searches={searches_ran} key->paint p50={p50:.3?} p90={p90:.3?} max={last:.3?} wall={wall:.3?}",
                phrase.chars().count()
            );
        }
    }

    drop(qtx);
    let _ = worker.join();
}