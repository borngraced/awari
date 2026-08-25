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
use std::time::Duration;

use awari_ipc::ClientRequest;
use crate::config;
use crate::lock;

const KILL_TIMEOUT_MS: u64 = 900;

#[cfg(unix)]
static CHILD_PID: AtomicI32 = AtomicI32::new(-1);

/// `no_keep_alive` is true when the daemon was started with `--no-keep-alive`.
pub fn run(no_keep_alive: bool) {
    let filter = tracing_subscriber::EnvFilter::try_from_env("AWARI_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let cfg = config::load();
    let keep_alive = !no_keep_alive && cfg.keep_alive;

    let server = match lock::acquire() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("awari: {e}");
            std::process::exit(1);
        }
    };
    let stats = Arc::new(Mutex::new(lock::Stats::default()));
    lock::spawn_accept(server.listener, stats.clone());

    #[cfg(unix)]
    install_daemon_sigterm();

    let child = Arc::new(Mutex::new(None::<Child>));
    let visible = Arc::new(AtomicBool::new(false));
    let ipc_rx = match lock::take_ipc_rx() {
        Some(rx) => rx,
        None => {
            tracing::error!("ipc receiver unavailable");
            std::process::exit(1);
        }
    };

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
            ClientRequest::ToggleLauncher => {
                if keep_alive {
                    if visible.load(Ordering::SeqCst) {
                        signal_child(&child, libc::SIGUSR2);
                        visible.store(false, Ordering::SeqCst);
                    } else {
                        if child.lock().unwrap().is_none() {
                            start_child(&child, keep_alive);
                        }
                        signal_child(&child, libc::SIGUSR1);
                        visible.store(true, Ordering::SeqCst);
                    }
                } else if visible.load(Ordering::SeqCst) {
                    stop_child(&child);
                    visible.store(false, Ordering::SeqCst);
                } else {
                    start_child(&child, keep_alive);
                    visible.store(true, Ordering::SeqCst);
                }
            }
            ClientRequest::OpenLauncher => {
                if child.lock().unwrap().is_none() {
                    start_child(&child, keep_alive);
                }
                if keep_alive {
                    signal_child(&child, libc::SIGUSR1);
                }
                visible.store(true, Ordering::SeqCst);
            }
            ClientRequest::CloseLauncher => {
                if keep_alive {
                    signal_child(&child, libc::SIGUSR2);
                    visible.store(false, Ordering::SeqCst);
                } else if visible.load(Ordering::SeqCst) {
                    stop_child(&child);
                    visible.store(false, Ordering::SeqCst);
                }
            }
            // The GUI reports its real visibility so an in-GUI dismiss (Escape,
            // background click) keeps the daemon's `visible` flag truthful.
            ClientRequest::LauncherShown => visible.store(true, Ordering::SeqCst),
            ClientRequest::LauncherHidden => visible.store(false, Ordering::SeqCst),
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
            // Wait for the GUI to actually exit before we do, so it isn't
            // orphaned mid-teardown. `waitpid` is async-signal-safe.
            let mut status: i32 = 0;
            libc::waitpid(pid, &mut status, 0);
        }
    }
    std::process::exit(0);
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

fn spawn_reaper(child: Arc<Mutex<Option<Child>>>, visible: Arc<AtomicBool>, keep_alive: bool) {
    thread::Builder::new()
        .name("awari-reap".into())
        .spawn(move || loop {
            {
                let mut g = child.lock().unwrap();
                if let Some(c) = g.as_mut() {
                    if c.try_wait().ok().flatten().is_some() {
                        *g = None;
                        drop(g);
                        if keep_alive {
                            // Keep-alive: maintain a warm GUI, respawn hidden.
                            start_child(&child, keep_alive);
                        } else {
                            visible.store(false, Ordering::SeqCst);
                        }
                    }
                }
            }
            thread::sleep(Duration::from_millis(100));
        })
        .ok();
}

fn start_child(child: &Arc<Mutex<Option<Child>>>, keep_alive: bool) {
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
    // Keep the GUI's logs out of the daemon's stdio: append to a file, falling
    // back to /dev/null if it can't be opened.
    let (out, err) = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/awari-gui.log")
    {
        Ok(f) => {
            let err = f.try_clone().map(Stdio::from).unwrap_or_else(|_| Stdio::null());
            (Stdio::from(f), err)
        }
        Err(_) => (Stdio::null(), Stdio::null()),
    };
    cmd.stdout(out).stderr(err);
    match cmd.spawn() {
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
    let mut c = match child.lock().unwrap().take() {
        Some(c) => c,
        None => return,
    };
    #[cfg(unix)]
    unsafe {
        libc::kill(c.id() as i32, libc::SIGTERM);
    }
    thread::Builder::new()
        .name("awari-kill-watchdog".into())
        .spawn(move || {
            thread::sleep(Duration::from_millis(KILL_TIMEOUT_MS));
            if c.try_wait().ok().flatten().is_none() {
                let _ = c.kill();
            }
            // Reap only this child. The shared slot was already taken (freed so
            // a new spawn can reuse it) and `visible` is owned by the caller;
            // writing either here would clobber a child spawned during the
            // 900 ms window. The reaper handles whichever child is in the slot.
            let _ = c.wait();
        })
        .ok();
}
