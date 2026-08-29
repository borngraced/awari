//! GPU-free shell daemon.
//!
//! Owns the single-instance IPC socket and manages the launcher GUI. Two modes,
//! decided by `keep_alive` (config `keep-alive` / `true` default, or the daemon
//! flag `--no-keep-alive`):
//! - Keep alive (default): a single `awari gui` is pre-spawned hidden and kept
//!   warm; toggles send SIGUSR1 (show) / SIGUSR2 (hide). Re-opens are instant
//!   but the GPU process stays in memory while idle.
//! - Drop: `awari gui` is spawned on open and quits on dismiss, so idle is a
//!   tiny GPU-free shell; re-open rebuilds the interface.

use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::app::GpuMode;
use crate::child::{
    block_signal, signal_child, spawn_reaper, spawn_signal_thread, start_child, stop_child,
};
use crate::config;
use crate::lock;
use crate::log::init_log_pipe;
use awari_ipc::ClientRequest;
use tracing_subscriber::EnvFilter;

/// Set on a daemon re-spawned by `awari restart`, so the fresh process retries
/// the single-instance lock while the old one is still releasing it instead of
/// failing fast with "already running".
const RESTART_ENV: &str = "AWARI_RESTART";
/// Upper bound on restart lock retry: 50 × 100 ms = 5 s of waiting for the
/// predecessor to release the flock before declaring failure.
const RESTART_LOCK_RETRY_TRIES: u32 = 50;

/// `mode` selects keep-alive (`GpuMode::KeepAlive`) or drop (`GpuMode::Drop`)
/// for the launcher GUI.
pub fn run(mode: GpuMode) {
    let filter = EnvFilter::try_from_env("AWARI_LOG").unwrap_or_else(|_| EnvFilter::new("info"));

    let child_log_fd = init_log_pipe(filter);

    let cfg = config::load();
    let keep_alive = matches!(mode, GpuMode::KeepAlive) && cfg.keep_alive;

    // The replacement spawned by `awari restart` can outlive our exit momentarily;
    // give it a five-second window to grab the flock before declaring "already running".
    let server = match acquire_with_retry() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("awari: {e}");
            std::process::exit(1);
        }
    };
    let ipc_rx = lock::spawn_accept(server.listener);

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
        restart = std::env::var_os(RESTART_ENV).is_some(),
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
            ClientRequest::Restart => {
                // Reply first (accept thread), then swap the process: kill the
                // GUI, re-exec ourselves detached, and exit to release the flock.
                let child = child.clone();
                std::thread::Builder::new()
                    .name("awari-restart".into())
                    .spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(150));
                        restart_daemon(&child);
                    })
                    .expect("restart thread");
            }
            ClientRequest::Ping => {}
        }
    }
}

/// `lock::acquire` with a bounded retry window, active only on a daemon that
/// was just re-spawned by `awari restart` (its predecessor has not exited yet
/// and still holds the single-instance flock).
/// Acquire the flock. Under [`RESTART_ENV`] the predecessor still holds the
/// lock while it exits, so retry — but bounded by [`RESTART_LOCK_RETRY_TRIES`]
/// × 100 ms. An unbounded wait would spin forever holding nothing if the
/// predecessor wedged (deadlocked in exit, stuck on a signal).
fn acquire_with_retry() -> Result<lock::IpcServer, lock::LockError> {
    let mut tries = if std::env::var_os(RESTART_ENV).is_some() {
        RESTART_LOCK_RETRY_TRIES
    } else {
        0
    };
    loop {
        match lock::acquire() {
            Ok(server) => return Ok(server),
            Err(lock::LockError::AlreadyRunning) if tries > 0 => {
                tries -= 1;
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => return Err(e),
        }
    }
}

/// Stop the GUI child, spawn a clean copy of this daemon (same argv, plus the
/// [`RESTART_ENV`] marker so it retries the lock), then exit. The new process
/// re-reads `config.kdl` in both the shell and the GUI it spawns, so config
/// edits land without restarting the compositor session.
///
/// Failure-safe ordering: the replacement is spawned *before* the old GUI is
/// torn down. If the exe can't be resolved or the spawn fails, the daemon
/// keeps running untouched (GUI alive, lock held, still accepting IPC), so a
/// failed restart can never leave a GUI-less daemon or no daemon at all.
fn restart_daemon(child: &Arc<Mutex<Option<Child>>>) {
    let Ok(exe) = std::env::current_exe() else {
        tracing::error!("restart: resolve current exe; keeping this daemon running");
        return;
    };
    let mut cmd = std::process::Command::new(exe);
    for arg in std::env::args_os().skip(1) {
        cmd.arg(arg);
    }
    if let Err(e) = cmd
        .env(RESTART_ENV, "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        tracing::error!(%e, "restart: spawn replacement failed; keeping this daemon running");
        return;
    }
    tracing::info!("awari restart: stopping gui and exiting daemon");
    stop_child(child);
    std::process::exit(0);
}
