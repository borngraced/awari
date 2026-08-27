#[cfg(not(target_os = "linux"))]
compile_error!("awari is Linux-only (Wayland)");

mod app;
mod argv;
mod child;
mod config;
mod desktop;
mod files;
mod icons;
mod lock;
mod log;
mod matchq;
mod math;
mod shell;
mod surfaces;
mod ui;

use std::sync::{Arc, Mutex};

use awari_compositor::{Backend, connect};
use gpui_platform::application;

use crate::app::{GpuMode, StartState};
use crate::lock::Stats;
use crate::surfaces::SurfaceRole;

/// True if `flag` appears anywhere in `args`. Flags may precede or follow the
/// mode arg; scanning all of `args` (argv[0] is never a flag) keeps daemon
/// dispatch and `gui_main` using one consistent flag-detection strategy.
fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

fn main() {
    let args: Vec<String> = std::env::args_os()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let daemon_mode = if has_flag(&args, "--no-keep-alive") {
        GpuMode::Drop
    } else {
        GpuMode::KeepAlive
    };
    // The first non-flag arg selects the mode; flags (e.g. --no-keep-alive) are
    // detected independently of position, so `awari --no-keep-alive daemon` and
    // `awari daemon --no-keep-alive` behave identically.
    let cmd = args.iter().skip(1).find(|a| !a.starts_with('-')).cloned();
    match cmd.as_deref() {
        Some("gui") => gui_main(&args),
        Some("daemon") => shell::run(daemon_mode),
        Some(other) => std::process::exit(argv::client_main(other)),
        None => shell::run(daemon_mode),
    }
}

fn gui_main(args: &[String]) {
    let filter = tracing_subscriber::EnvFilter::try_from_env("AWARI_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let (backend, inbox) = connect();
    if matches!(backend, Backend::Noop) {
        tracing::warn!(
            "compositor did not advertise wlr-foreign-toplevel; window switching disabled (apps/files/commands still work)"
        );
    }

    let cfg = config::load();
    let drop_gpu = has_flag(args, "--no-keep-alive");
    tracing::info!(
        ns = SurfaceRole::Launcher.namespace(),
        mode = if drop_gpu { "drop" } else { "keep-alive" },
        "gpui launcher"
    );

    let gpu_mode = if drop_gpu {
        GpuMode::Drop
    } else {
        GpuMode::KeepAlive
    };
    let start_state = if has_flag(args, "--hidden") {
        StartState::Hidden
    } else {
        StartState::Open
    };

    application().run(move |cx| {
        // gpui_base::init(cx);
        eprintln!("[boot] pre-gpui rss={}MiB", app::boot_rss_mib());
        app::Daemon::start(
            cx,
            match backend {
                Backend::Wlr(c) => Some(c),
                Backend::Noop => None,
            },
            inbox,
            Arc::new(Mutex::new(Stats::default())),
            cfg,
            start_state,
            gpu_mode,
        );
    });
}
