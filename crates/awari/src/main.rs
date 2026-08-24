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
mod surfaces;
mod ui;

use std::sync::{Arc, Mutex};

use gpui_platform::application;
use awari_compositor::connect;

use crate::lock::Stats;
use crate::surfaces::SurfaceRole;

fn main() {
    if let Some(cmd) = std::env::args().nth(1) {
        if cmd != "daemon" {
            std::process::exit(argv::client_main(&cmd));
        }
    }

    let filter = tracing_subscriber::EnvFilter::try_from_env("AWARI_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let server = match lock::acquire() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("awari: {e}");
            std::process::exit(1);
        }
    };
    let stats = Arc::new(Mutex::new(Stats::default()));
    lock::spawn_accept(server.listener, stats.clone());

    let (compositor, inbox, wlr_connected) = connect();
    if !wlr_connected {
        tracing::warn!(
            "compositor did not advertise wlr-foreign-toplevel; window switching disabled (apps/files/commands still work)"
        );
    }

    let cfg = config::load();
    tracing::info!(ns = SurfaceRole::Launcher.namespace(), "gpui launcher daemon");

    application().run(move |cx| {
        gpui_base::init(cx);
        app::Daemon::start(cx, Some(compositor), inbox, stats.clone(), cfg);
    });
}
