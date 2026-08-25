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

use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use awari_ipc::{ClientRequest, runtime_dir};
use crate::app::GpuMode;
use crate::config;
use crate::lock;

const KILL_TIMEOUT_MS: u64 = 900;

#[cfg(unix)]
static CHILD_PID: AtomicI32 = AtomicI32::new(-1);

/// `mode` selects keep-alive (`GpuMode::KeepAlive`) or drop (`GpuMode::Drop`)
/// for the launcher GUI.
pub fn run(mode: GpuMode) {
    let filter = tracing_subscriber::EnvFilter::try_from_env("AWARI_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

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
    install_daemon_sigterm();

    let child = Arc::new(Mutex::new(None::<Child>));
    let visible = Arc::new(AtomicBool::new(false));

    spawn_reaper(child.clone(), visible.clone(), keep_alive);

    // Keep-alive mode pre-spawns a hidden, warm GUI so the first open is instant.
    if keep_alive {
        start_child(&child, keep_alive);
    }

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
                            start_child(&child, keep_alive);
                        }
                        signal_child(&child, libc::SIGUSR1);
                    }
                } else if visible.load(Ordering::Relaxed) {
                    stop_child(&child);
                } else {
                    start_child(&child, keep_alive);
                }
            }
            ClientRequest::OpenLauncher => {
                if child.lock().unwrap().is_none() {
                    start_child(&child, keep_alive);
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
extern "C" fn on_daemon_sigterm(_sig: i32) {
    let pid = CHILD_PID.load(Ordering::Relaxed);
    if pid > 0 {
        unsafe {
            libc::kill(pid, libc::SIGTERM);
            // Best-effort: give the GUI a moment to exit so it isn't orphaned
            // mid-teardown. Non-blocking poll so a wedged GUI can't pin the
            // daemon in the handler (SIGTERM is blocked while we're here);
            // bound it to ~500 ms, then leave regardless. Both `waitpid` and
            // `nanosleep` are async-signal-safe.
            let mut status: i32 = 0;
            let pause = libc::timespec {
                tv_sec: 0,
                tv_nsec: 10_000_000,
            };
            for _ in 0..50 {
                if libc::waitpid(pid, &mut status, libc::WNOHANG) > 0 {
                    break;
                }
                libc::nanosleep(&pause, std::ptr::null_mut());
            }
        }
    }
    // `_exit` (not `process::exit`) skips atexit handlers and stdio flushing,
    // which are not async-signal-safe to run from a signal handler.
    unsafe { libc::_exit(0) };
}

#[cfg(unix)]
fn install_daemon_sigterm() {
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_daemon_sigterm as *const () as usize;
        sa.sa_flags = libc::SA_RESTART;
        libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
    }
}

const REAPER_FAST_FAIL: Duration = Duration::from_secs(2);
const REAPER_MAX_FAST_FAILS: u32 = 5;
const REAPER_BASE_BACKOFF_MS: u64 = 100;
const REAPER_MAX_BACKOFF: Duration = Duration::from_secs(30);

fn spawn_reaper(child: Arc<Mutex<Option<Child>>>, visible: Arc<AtomicBool>, keep_alive: bool) {
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
                                start_child(&child, keep_alive);
                            } else {
                                visible.store(false, Ordering::Relaxed);
                            }
                            continue;
                        }
                    }
                }
                thread::sleep(Duration::from_millis(100));
            }
        })
        .map_err(|e| tracing::error!(%e, "reaper thread failed to spawn"))
        .ok();
}

fn start_child(child: &Arc<Mutex<Option<Child>>>, keep_alive: bool) {
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
        // Default GUI mode is keep-alive; `--hidden` starts it pre-warmed but
        // not shown. We don't pass `--keep-alive` (the GUI only parses
        // `--no-keep-alive` to flip to drop mode), so the flag stays meaningful.
        cmd.arg("--hidden");
    } else {
        cmd.arg("--no-keep-alive").arg("--open");
    }
    // Keep the GUI's logs out of the daemon's stdio: append to a file under
    // the runtime dir (with the rest of our state), falling back to /dev/null
    // if it can't be opened.
    let log_path = runtime_dir().join("gui.log");
    let (out, err) = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        Ok(f) => {
            let err = f.try_clone().map(Stdio::from).unwrap_or_else(|_| Stdio::null());
            (Stdio::from(f), err)
        }
        Err(_) => (Stdio::null(), Stdio::null()),
    };
    cmd.stdout(out).stderr(err);
    // Block show/hide signals in this thread before spawning so the GUI child
    // inherits the mask. The GUI unblocks them only after installing its
    // handlers, which closes the race where a signal sent right after spawn
    // (daemon toggled during the GUI's boot window) would hit the default
    // terminate disposition and kill the child before GPUI installs handlers.
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
    let result = cmd.spawn();
    #[cfg(unix)]
    unsafe {
        libc::pthread_sigmask(libc::SIG_SETMASK, &saved_mask, std::ptr::null_mut());
    }
    match result {
        Ok(c) => {
            #[cfg(unix)]
            CHILD_PID.store(c.id() as i32, Ordering::Relaxed);
            *child.lock().unwrap() = Some(c);
        }
        Err(e) => {
            tracing::error!(%e, "spawn gui");
        }
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
            if alive {
                if let Some(mut c) = guard.take() {
                    drop(guard);
                    let _ = c.kill();
                    let _ = c.wait();
                }
            }
        })
        .map_err(|e| tracing::error!(%e, "kill-watchdog thread failed to spawn"))
        .ok();
}
