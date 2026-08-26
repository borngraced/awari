//! GUI child supervision: spawn, signal, and reap the `awari gui` process.

use std::os::unix::io::{FromRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const KILL_TIMEOUT_MS: u64 = 900;

const REAPER_FAST_FAIL: Duration = Duration::from_secs(2);
const REAPER_MAX_FAST_FAILS: u32 = 5;
const REAPER_BASE_BACKOFF_MS: u64 = 100;
const REAPER_MAX_BACKOFF: Duration = Duration::from_secs(30);

#[cfg(unix)]
pub(crate) fn block_signal(sig: i32) {
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
pub(crate) fn spawn_signal_thread(child: Arc<Mutex<Option<Child>>>) {
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

pub(crate) fn spawn_reaper(
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
pub(crate) fn start_child(
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

pub(crate) fn signal_child(child: &Arc<Mutex<Option<Child>>>, sig: i32) {
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

pub(crate) fn stop_child(child: &Arc<Mutex<Option<Child>>>) {
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
