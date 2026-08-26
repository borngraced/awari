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
use std::os::unix::io::{FromRawFd, RawFd};
use std::os::unix::process::CommandExt;
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

    let child_log_fd = init_log_pipe(filter);

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

    spawn_reaper(child.clone(), visible.clone(), keep_alive, child_log_fd);

    if keep_alive {
        start_child(&child, keep_alive, child_log_fd);
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
                        start_child(&child, keep_alive, child_log_fd);
                        signal_child(&child, libc::SIGUSR1);
                    }
                } else if visible.load(Ordering::Relaxed) {
                    stop_child(&child);
                } else {
                    start_child(&child, keep_alive, child_log_fd);
                }
            }
            ClientRequest::OpenLauncher => {
                start_child(&child, keep_alive, child_log_fd);
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
    child_log_fd: RawFd,
) {
    thread::Builder::new()
        .name("awari-reap".into())
        .spawn(move || {
            let mut consecutive_failures = 0u32;
            let mut last_start = Instant::now();
            loop {
                {
                    let mut g = child.lock().unwrap();
                    if let Some(c) = g.as_mut() {
                        if matches!(c.try_wait(), Ok(Some(_))) {
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
                                let backoff =
                                    Duration::from_millis(backoff_ms).min(REAPER_MAX_BACKOFF);
                                thread::sleep(backoff);
                                last_start = Instant::now();
                                start_child(&child, keep_alive, child_log_fd);
                            } else {
                                visible.store(false, Ordering::Relaxed);
                            }
                            continue;
                        }
                    } else if keep_alive {
                        // child is None — first spawn never succeeded.
                        // Retry after a short backoff so we don't spin-loop.
                        consecutive_failures += 1;
                        if consecutive_failures >= REAPER_MAX_FAST_FAILS {
                            tracing::error!(
                                failures = consecutive_failures,
                                "gui never started; disabling keep-alive respawn"
                            );
                            return;
                        }
                        let factor = 2u32.saturating_pow(consecutive_failures - 1);
                        let backoff_ms = REAPER_BASE_BACKOFF_MS
                            .checked_mul(u64::from(factor))
                            .unwrap_or(30_000);
                        let backoff = Duration::from_millis(backoff_ms).min(REAPER_MAX_BACKOFF);
                        drop(g);
                        thread::sleep(backoff);
                        last_start = Instant::now();
                        start_child(&child, keep_alive, child_log_fd);
                        continue;
                    }
                }
                thread::sleep(Duration::from_millis(100));
            }
        })
        .map_err(|e| tracing::error!(%e, "reaper thread failed to spawn"))
        .ok();
}

/// Spawn a GUI child if none is running. Holds the child lock across
/// check-and-spawn to prevent two callers from both spawning.
fn start_child(
    child: &Arc<Mutex<Option<Child>>>,
    keep_alive: bool,
    child_log_fd: RawFd,
) {
    let mut guard = child.lock().unwrap();
    if guard.is_some() {
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
    let (out, err) = if child_log_fd >= 0 {
        unsafe {
            let o = libc::dup(child_log_fd);
            let e = libc::dup(child_log_fd);
            if o >= 0 && e >= 0 {
                (Stdio::from_raw_fd(o), Stdio::from_raw_fd(e))
            } else {
                if o >= 0 { libc::close(o); }
                if e >= 0 { libc::close(e); }
                (Stdio::null(), Stdio::null())
            }
        }
    } else {
        (Stdio::null(), Stdio::null())
    };
    cmd.stdout(out).stderr(err);
    // The parent blocks SIGUSR1/SIGUSR2 around the fork so a signal aimed at
    // the daemon can't race the child, but the child *inherits* that blocked
    // mask. A manually launched `awari gui` starts with signals unblocked and
    // works; with them blocked, GPUI/wgpu init fails and the child exits
    // instantly (spawn succeeds, process dies -> no persistent child to
    // signal). Unblock in the child before exec so it matches a normal launch.
    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(|| {
            let mut mask: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut mask);
            libc::sigaddset(&mut mask, libc::SIGUSR1);
            libc::sigaddset(&mut mask, libc::SIGUSR2);
            libc::pthread_sigmask(libc::SIG_UNBLOCK, &mask, std::ptr::null_mut());
            Ok(())
        });
    }
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
    match cmd.spawn() {
        Ok(c) => *guard = Some(c),
        Err(e) => tracing::error!(%e, "spawn gui"),
    }
    #[cfg(unix)]
    unsafe {
        libc::pthread_sigmask(libc::SIG_SETMASK, &saved_mask, std::ptr::null_mut());
    }
}

fn signal_child(child: &Arc<Mutex<Option<Child>>>, sig: i32) {
    let pid = child.lock().unwrap().as_ref().map(|c| c.id());
    if let Some(pid) = pid {
        tracing::debug!(pid, sig, "sending signal to gui");
        unsafe {
            libc::kill(pid as i32, sig);
        }
    } else {
        tracing::warn!(sig, "no gui child to signal");
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

const LOG_CAP: u64 = 1024 * 1024;
const LOG_COMPACT_HEADROOM: u64 = 1024 * 1024;

/// Set up the shared log pipe: the daemon's tracing and the GUI's captured
/// stdout/stderr both write to one pipe that a reader thread drains into
/// `awari.log`. Returns a dup'd write fd for the GUI child's stdout/stderr,
/// or -1 if the pipe can't be created.
///
/// The tracing subscriber writes through `PipeWriter(Arc<Mutex<File>>)` —
/// the mutex serialises concurrent writes so no interleaving is possible.
/// The returned child fd is a separate dup; `start_child` never touches
/// the shared `File`, avoiding the old deadlock where both paths competed
/// for the same lock.
fn init_log_pipe(filter: tracing_subscriber::EnvFilter) -> RawFd {
    let mut fds = [-1i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        tracing_subscriber::fmt().with_env_filter(filter).init();
        return -1;
    }
    let read_fd = fds[0];
    let write_fd = fds[1];
    let path = runtime_dir().join("awari.log");

    // Dup before wrapping the original in Arc<Mutex<File>> — start_child
    // will use this copy; it never touches the shared File.
    let child_fd = unsafe { libc::dup(write_fd) };

    let shared = Arc::new(Mutex::new(unsafe { File::from_raw_fd(write_fd) }));

    match thread::Builder::new()
        .name("awari-log".into())
        .spawn(move || log_reader(read_fd, path))
    {
        Ok(_) => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(PipeWriter(shared))
                .init();
            child_fd
        }
        Err(_) => {
            unsafe {
                libc::close(read_fd);
                libc::close(child_fd);
            }
            tracing_subscriber::fmt().with_env_filter(filter).init();
            -1
        }
    }
}

fn log_reader(read_fd: RawFd, path: PathBuf) {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok();
    let mut buf = [0u8; 8192];
    loop {
        let n = unsafe {
            libc::read(read_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
        };
        match n {
            -1 => break,
            0 => break,
            n => {
                let n = n as usize;
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
        }
    }
    unsafe { libc::close(read_fd); }
}

/// Rewrite `path` to keep at most `cap` bytes, advancing past the first
/// newline so we never truncate a line. Streams the tail out so the whole file
/// is never buffered in memory at once.
fn compact_log(path: &Path, cap: u64) -> std::io::Result<()> {
    let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if len <= cap {
        return Ok(());
    }
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
///
/// Writes are serialised through the mutex, so concurrent callers from
/// different threads never interleave. Tracing lines are well under
/// `PIPE_BUF` (4096 on Linux), so even without the mutex a single
/// `write(2)` would be atomic — but the mutex also protects the
/// `File`'s internal buffer state.
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
