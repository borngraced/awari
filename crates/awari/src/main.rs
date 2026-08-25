#[cfg(not(target_os = "linux"))]
compile_error!("awari is Linux-only (Wayland)");

mod app;
mod argv;
mod config;
mod desktop;
mod files;
mod icons;
mod lock;
mod matchq;
mod math;
mod shell;
mod surfaces;
mod ui;

use std::sync::{Arc, Mutex};

use awari_compositor::connect;
use gpui_platform::application;

use crate::lock::Stats;
use crate::surfaces::SurfaceRole;

/// True if `flag` appears anywhere in `args`. Flags may precede or follow the
/// mode arg; scanning all of `args` (argv[0] is never a flag) keeps daemon
/// dispatch and `gui_main` using one consistent flag-detection strategy.
fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let no_keep_alive = has_flag(&args, "--no-keep-alive");
    // The first non-flag arg selects the mode; flags (e.g. --no-keep-alive) are
    // detected independently of position, so `awari --no-keep-alive daemon` and
    // `awari daemon --no-keep-alive` behave identically.
    let cmd = args
        .iter()
        .skip(1)
        .find(|a| !a.starts_with('-'))
        .cloned();
    match cmd.as_deref() {
        Some("gui") => gui_main(&args),
        Some("daemon") => shell::run(no_keep_alive),
        Some(other) => std::process::exit(argv::client_main(other)),
        None => shell::run(no_keep_alive),
    }
}

fn gui_main(args: &[String]) {
    let filter = tracing_subscriber::EnvFilter::try_from_env("AWARI_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let (compositor, inbox, wlr_connected) = connect();
    if !wlr_connected {
        tracing::warn!(
            "compositor did not advertise wlr-foreign-toplevel; window switching disabled (apps/files/commands still work)"
        );
    }

    let cfg = config::load();
    let drop = has_flag(args, "--no-keep-alive");
    tracing::info!(
        ns = SurfaceRole::Launcher.namespace(),
        mode = if drop { "drop" } else { "keep-alive" },
        "gpui launcher"
    );

    let keep_alive = !drop;
    let open = !has_flag(args, "--hidden");

    application().run(move |cx| {
        gpui_base::init(cx);
        app::Daemon::start(
            cx,
            Some(compositor),
            inbox,
            Arc::new(Mutex::new(Stats::default())),
            cfg,
            open,
            keep_alive,
        );
    });
}
