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

/// `mode` selects keep-alive (`GpuMode::KeepAlive`) or drop (`GpuMode::Drop`)
/// for the launcher GUI.
pub fn run(mode: GpuMode) {
    let filter = EnvFilter::try_from_env("AWARI_LOG").unwrap_or_else(|_| EnvFilter::new("info"));

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
            ClientRequest::Ping => {}
        }
    }
}
