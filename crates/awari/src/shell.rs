//! GPU-free shell daemon.
//!
//! Owns the single-instance IPC socket and manages the launcher GUI. Two modes,
//! decided by `keep_alive` (config `keep-alive` / `true` default, or the daemon
//! flag `--no-keep-alive`):
//! - Keep alive (default): a single `awari gui` is pre-spawned hidden and kept
//!   warm; toggles send SIGUSR1 (show) / SIGUSR2 (hide). Re-opens are instant
//!   but the GPU process stays in memory at ~tens of MB.
//! - Drop: `awari gui` is spawned on open and quits on dismiss, so idle is a
//!   few-MB shell; re-open rebuilds the interface (~100 ms).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::io::{FromRawFd, IntoRawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::app::GpuMode;
use crate::config;
use crate::lock;
use awari_ipc::{ClientRequest, runtime_dir};
use tracing_subscriber::fmt::MakeWriter;

const KILL_TIMEOUT_MS: u64 = 900;

/// `mode` selects keep-alive (`GpuMode::KeepAlive`) or drop (`GpuMode::Drop`)
/// for the launcher GUI.
pub fn run(mode: GpuMode) {
    let filter = tracing_subscriber::EnvFilter::try_from_env("AWARI_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    // One shared `awari.log`. The daemon's own tracing and the GUI's captured
    // stdout/stderr both flow through a pipe into a single reader thread that
    // writes (and caps) the file, so there is exactly one writer.
    let log_tx = init_log_pipe(filter);

    let cfg = config::load();
    let keep_alive = matches!(mode, GpuMode::KeepAlive) && cfg.keep_alive;

    let server = match lock::acquire() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("awari: {e}");
            std::process::exit(1);
        }
    };
    let stats = Arc::new(Mutex::new(lock::Stats::default()));
    let ipc_rx = lock::spawn_accept(server.listener, stats.clone());

    #[cfg(unix)]
    block_signal(libc::SIGTERM);

    let child = Arc::new(Mutex::new(None::<Child>));
    let visible = Arc::new(AtomicBool::new(false));

    spawn_reaper(child.clone(), visible.clone(), keep_alive, log_tx.clone());

    // Keep-alive mode pre-spawns a hidden, warm GUI so the first open is instant.
    if keep_alive {
        start_child(&child, keep_alive, &log_tx);
    }

    #[cfg(unix)]
    spawn_signal_thread(child.clone());

    tracing::info!(
        mode = if keep_alive { "keep-alive" } else { "drop" },
        "awari shell daemon (gpu-free)"
    );

    for req in ipc_rx {
        match req {
            // The GUI is the source of truth for visibility: it reports
            // LauncherShown / LauncherHidden after every real state change, so
            // the daemon only reads `visible` to decide show vs hide and never
            // writes it optimistically. Toggle is therefore fire-and-forget.
            ClientRequest::ToggleLauncher => {
                if keep_alive {
                    if visible.load(Ordering::Relaxed) {
                        signal_child(&child, libc::SIGUSR2);
                    } else {
                        if child.lock().unwrap().is_none() {
                            start_child(&child, keep_alive, &log_tx);
                        }
                        signal_child(&child, libc::SIGUSR1);
                    }
                } else if visible.load(Ordering::Relaxed) {
                    stop_child(&child);
                } else {
                    start_child(&child, keep_alive, &log_tx);
                }
            }
            ClientRequest::OpenLauncher => {
                if child.lock().unwrap().is_none() {
                    start_child(&child, keep_alive, &log_tx);
                }
                if keep_alive {
                    signal_child(&child, libc::SIGUSR1);
                }
            }
            ClientRequest::CloseLauncher => {
                if keep_alive {
                    signal_child(&child, libc::SIGUSR2);
                } else if visible.load(Ordering::Relaxed) {
                    stop_child(&child);
                }
            }
            // The GUI reports its real visibility so an in-GUI dismiss (Escape,
            // background click) keeps the daemon's `visible` flag truthful.
            ClientRequest::LauncherShown => visible.store(true, Ordering::Relaxed),
            ClientRequest::LauncherHidden => visible.store(false, Ordering::Relaxed),
            _ => {}
        }
    }
}

#[cfg(unix)]
fn block_signal(sig: i32) {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, sig);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
    }
}

/// SIGTERM is blocked process-wide and consumed synchronously here via sigwait,
/// so shutdown can take the child lock (unsafe from a real signal handler) to
/// kill the GUI. Because start_child holds that lock across spawn+store, the
/// child read here always sees the just-forked GUI — never a stale pid that
/// would let it slip through and become orphaned.
#[cfg(unix)]
fn spawn_signal_thread(child: Arc<Mutex<Option<Child>>>) {
    thread::Builder::new()
        .name("awari-signal".into())
        .spawn(move || {
            let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
            unsafe {
                libc::sigemptyset(&mut set);
                libc::sigaddset(&mut set, libc::SIGTERM);
            }
            let mut sig: i32 = 0;
            loop {
                if unsafe { libc::sigwait(&set, &mut sig) } == 0 && sig == libc::SIGTERM {
                    if let Some(c) = child.lock().unwrap().as_ref() {
                        unsafe { libc::kill(c.id() as i32, libc::SIGTERM) };
                    }
                    thread::sleep(Duration::from_millis(200));
                    unsafe { libc::_exit(0) };
                }
            }
        })
        .expect("signal thread");
}

const REAPER_FAST_FAIL: Duration = Duration::from_secs(2);
const REAPER_MAX_FAST_FAILS: u32 = 5;
const REAPER_BASE_BACKOFF_MS: u64 = 100;
const REAPER_MAX_BACKOFF: Duration = Duration::from_secs(30);

fn spawn_reaper(
    child: Arc<Mutex<Option<Child>>>,
    visible: Arc<AtomicBool>,
    keep_alive: bool,
    log_tx: Option<Arc<Mutex<File>>>,
) {
    thread::Builder::new()
        .name("awari-reap".into())
        .spawn(move || {
            let mut consecutive_failures = 0u32;
            let mut last_start = Instant::now();
            loop {
                {
                    let mut g = child.lock().unwrap();
                    if let Some(c) = g.as_mut()
                        && matches!(c.try_wait(), Ok(Some(_)))
                    {
                        let ran_for = last_start.elapsed();
                        *g = None;
                        drop(g);
                        if keep_alive {
                            consecutive_failures = if ran_for < REAPER_FAST_FAIL {
                                consecutive_failures + 1
                            } else {
                                0
                            };
                            if consecutive_failures >= REAPER_MAX_FAST_FAILS {
                                tracing::error!(
                                    failures = consecutive_failures,
                                    "gui crashed on startup repeatedly; \
                                         disabling keep-alive respawn"
                                );
                                return;
                            }
                            let factor = 2u32.saturating_pow(consecutive_failures);
                            let backoff_ms = REAPER_BASE_BACKOFF_MS
                                .checked_mul(u64::from(factor))
                                .unwrap_or(30_000);
                            let backoff = Duration::from_millis(backoff_ms).min(REAPER_MAX_BACKOFF);
                            thread::sleep(backoff);
                            last_start = Instant::now();
                            start_child(&child, keep_alive, &log_tx);
                        } else {
                            visible.store(false, Ordering::Relaxed);
                        }
                        continue;
                    }
                }
                thread::sleep(Duration::from_millis(100));
            }
        })
        .map_err(|e| tracing::error!(%e, "reaper thread failed to spawn"))
        .ok();
}

fn start_child(
    child: &Arc<Mutex<Option<Child>>>,
    keep_alive: bool,
    log_tx: &Option<Arc<Mutex<File>>>,
) {
    if child.lock().unwrap().is_some() {
        return;
    }
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            tracing::error!(%e, "resolve current exe");
            return;
        }
    };
    let mut cmd = Command::new(exe);
    cmd.arg("gui");
    if keep_alive {
        cmd.arg("--hidden");
    } else {
        cmd.arg("--no-keep-alive");
    }
    let (out, err) = match log_tx {
        Some(tx) => match (
            tx.lock().unwrap().try_clone(),
            tx.lock().unwrap().try_clone(),
        ) {
            (Ok(o), Ok(e)) => (Stdio::from(o), Stdio::from(e)),
            _ => (Stdio::null(), Stdio::null()),
        },
        None => (Stdio::null(), Stdio::null()),
    };
    cmd.stdout(out).stderr(err);
    #[cfg(unix)]
    let mut saved_mask: libc::sigset_t = unsafe { std::mem::zeroed() };
    #[cfg(unix)]
    unsafe {
        let mut mask: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut mask);
        libc::sigaddset(&mut mask, libc::SIGUSR1);
        libc::sigaddset(&mut mask, libc::SIGUSR2);
        libc::pthread_sigmask(libc::SIG_BLOCK, &mask, &mut saved_mask);
    }
    {
        let mut guard = child.lock().unwrap();
        match cmd.spawn() {
            Ok(c) => *guard = Some(c),
            Err(e) => tracing::error!(%e, "spawn gui"),
        }
    }
    #[cfg(unix)]
    unsafe {
        libc::pthread_sigmask(libc::SIG_SETMASK, &saved_mask, std::ptr::null_mut());
    }
}

fn signal_child(child: &Arc<Mutex<Option<Child>>>, sig: i32) {
    let pid = child.lock().unwrap().as_ref().map(|c| c.id());
    if let Some(pid) = pid {
        unsafe {
            libc::kill(pid as i32, sig);
        }
    }
}

fn stop_child(child: &Arc<Mutex<Option<Child>>>) {
    let pid = child.lock().unwrap().as_ref().map(|c| c.id());
    let Some(pid) = pid else {
        return;
    };
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
    let slot = child.clone();
    thread::Builder::new()
        .name("awari-kill-watchdog".into())
        .spawn(move || {
            thread::sleep(Duration::from_millis(KILL_TIMEOUT_MS));
            let mut guard = slot.lock().unwrap();
            let alive = guard
                .as_mut()
                .map(|c| matches!(c.try_wait(), Ok(None)))
                .unwrap_or(false);
            if alive && let Some(mut c) = guard.take() {
                drop(guard);
                let _ = c.kill();
                let _ = c.wait();
            }
        })
        .map_err(|e| tracing::error!(%e, "kill-watchdog thread failed to spawn"))
        .ok();
}

/// Max size of `awari.log`. When exceeded the reader rewrites the file keeping
/// only the most recent `LOG_CAP` bytes (trimmed to a line boundary).
const LOG_CAP: u64 = 1024 * 1024;
/// Re-compact only once the file grows this far past `LOG_CAP`, so compaction
/// (a full tail rewrite) runs every ~`LOG_COMPACT_HEADROOM` bytes of new log
/// instead of on every write chunk once the log is full.
const LOG_COMPACT_HEADROOM: u64 = 1024 * 1024;

/// Set up the shared log pipe: the daemon's tracing and the GUI's captured
/// stdout/stderr both write to one pipe that a reader thread drains into
/// `awari.log`. Returns the pipe write end so `start_child` can redirect the
/// GUI child to it; `None` (and stdout logging) if the pipe can't be created.
fn init_log_pipe(filter: tracing_subscriber::EnvFilter) -> Option<Arc<Mutex<File>>> {
    let (rx, tx) = match UnixStream::pair() {
        Ok(p) => p,
        // No pipe: log to stdout so we still capture the daemon.
        Err(_) => {
            tracing_subscriber::fmt().with_env_filter(filter).init();
            return None;
        }
    };
    let tx = Arc::new(Mutex::new(unsafe { File::from_raw_fd(tx.into_raw_fd()) }));
    let path = runtime_dir().join("awari.log");
    let reader_tx = tx.clone();
    // If the reader can't spawn, don't route through the pipe or writes would
    // block once its buffer fills — fall back to stdout logging.
    match thread::Builder::new()
        .name("awari-log".into())
        .spawn(move || log_reader(rx, path))
    {
        Ok(_) => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(PipeWriter(tx))
                .init();
            Some(reader_tx)
        }
        Err(_) => {
            tracing_subscriber::fmt().with_env_filter(filter).init();
            None
        }
    }
}

/// Drains the log pipe into `awari.log`, capping size by dropping the oldest
/// lines. If the file can't be opened it discards input so writes never block.
fn log_reader(mut rx: UnixStream, path: PathBuf) {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok();
    let mut buf = [0u8; 8192];
    loop {
        match rx.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if file
                    .as_mut()
                    .map(|f| {
                        f.metadata()
                            .map(|m| m.len() > LOG_CAP + LOG_COMPACT_HEADROOM)
                            .unwrap_or(false)
                    })
                    .unwrap_or(false)
                    && compact_log(&path, LOG_CAP).is_ok()
                {
                    file = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&path)
                        .ok();
                }
                if let Some(f) = file.as_mut() {
                    let _ = f.write_all(&buf[..n]);
                    let _ = f.flush();
                }
            }
            Err(_) => break,
        }
    }
}

/// Rewrite `path` to keep at most `cap` bytes, advancing past the first
/// newline so we never truncate a line. Streams the tail out so the whole file
/// is never buffered in memory at once.
fn compact_log(path: &Path, cap: u64) -> std::io::Result<()> {
    let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if len <= cap {
        return Ok(());
    }
    // Scan in chunks to find the first newline at or after `excess` without
    // loading the file into memory.
    let excess = (len - cap) as usize;
    let mut file = File::open(path)?;
    let mut buf = [0u8; 8192];
    let mut pos = 0usize;
    let mut start = excess;
    while pos < excess + 1 {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        for (i, &b) in buf[..n].iter().enumerate() {
            if pos + i >= excess && b == b'\n' {
                start = pos + i + 1;
                break;
            }
        }
        if start > excess {
            break;
        }
        pos += n;
    }
    // Stream the tail [start..] into a temp file, then swap it in atomically.
    let tmp = path.with_extension("tmp");
    {
        let mut src = File::open(path)?;
        src.seek(SeekFrom::Start(start as u64))?;
        let mut dst = File::create(&tmp)?;
        std::io::copy(&mut src, &mut dst)?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// `MakeWriter` that fans tracing events into the shared log pipe.
struct PipeWriter(Arc<Mutex<File>>);

impl Write for PipeWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.lock().unwrap().flush()
    }
}

impl<'writer> MakeWriter<'writer> for PipeWriter {
    type Writer = PipeWriter;
    fn make_writer(&'writer self) -> PipeWriter {
        PipeWriter(Arc::clone(&self.0))
    }
}
