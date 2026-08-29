//! Fast-path client argv. No wgpu.

use awari_ipc::{ClientReply, ClientRequest};

pub fn client_main(arg: &str) -> i32 {
    #[cfg(feature = "probe")]
    match arg {
        "probe-files" => {
            // Manual file-worker latency/RSS probe (see files/probe.rs).
            // Only built with `--features probe`.
            crate::files::probe::files();
            return 0;
        }
        "probe-typing" => {
            crate::files::probe::typing();
            return 0;
        }
        _ => {}
    }
    let req = match arg {
        "ping" => ClientRequest::Ping,
        "toggle-launcher" => ClientRequest::ToggleLauncher,
        "open-launcher" => ClientRequest::OpenLauncher,
        "close-launcher" => ClientRequest::CloseLauncher,
        "restart" => ClientRequest::Restart,
        _ => {
            eprintln!("unknown command: {arg}");
            return 2;
        }
    };
    if matches!(req, ClientRequest::Restart) {
        return restart_client();
    }
    match awari_ipc::send(&awari_ipc::socket_path(), &req) {
        Ok(ClientReply::Ok) => 0,
        Ok(ClientReply::Err(e)) => {
            eprintln!("{e}");
            1
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

/// `awari restart`: talk to the daemon when it is up; when it is not running
/// (drop mode exhausted, or never started), boot a fresh daemon instead, so
/// restart behaves like a systemd stop-then-start on a stopped unit.
fn restart_client() -> i32 {
    match awari_ipc::send(&awari_ipc::socket_path(), &ClientRequest::Restart) {
        Ok(ClientReply::Ok) => 0,
        Ok(ClientReply::Err(e)) => {
            eprintln!("{e}");
            1
        }
        Err(_) => {
            // `send` failed either because no daemon is up, or because a
            // running daemon rejected the frame (e.g. an installed binary that
            // predates the `restart` command). A live daemon answers `Ping`,
            // so distinguish the two before booting a replacement — one that
            // hits another daemon's flock already held binds nothing (this
            // branch never sets RESTART_ENV) and dies, so a silent no-op.
            if awari_ipc::ping_live().is_ok() {
                eprintln!(
                    "awari: a daemon is running but did not accept `restart` \
                     (older installed binary?); reinstall awari and restart manually"
                );
                return 1;
            }
            let Some(exe) = std::env::current_exe().ok() else {
                eprintln!("awari: cannot resolve executable");
                return 1;
            };
            // Reconstruct this invocation with the `restart` subcommand replaced
            // by `daemon`, preserving leading flags: `awari --no-keep-alive
            // restart` must boot a drop-mode daemon, not a hardcoded keep-alive
            // one.
            let mut cmd = std::process::Command::new(exe);
            let mut daemon_arg = false;
            for arg in std::env::args_os().skip(1) {
                if !daemon_arg && arg == "restart" {
                    cmd.arg("daemon");
                    daemon_arg = true;
                } else {
                    cmd.arg(arg);
                }
            }
            if !daemon_arg {
                cmd.arg("daemon");
            }
            match cmd
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
            {
                Ok(_) => 0,
                Err(e) => {
                    eprintln!("awari: failed to start daemon: {e}");
                    1
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_command_is_2() {
        assert_eq!(client_main("nope"), 2);
    }
}
