//! Fast-path client argv. No wgpu.

use awari_ipc::{ClientReply, ClientRequest};

pub fn client_main(arg: &str) -> i32 {
    let req = match arg {
        "ping" => ClientRequest::Ping,
        "dump-stats" => ClientRequest::DumpStats,
        "toggle-launcher" => ClientRequest::ToggleLauncher,
        "open-launcher" => ClientRequest::OpenLauncher,
        "close-launcher" => ClientRequest::CloseLauncher,
        _ => {
            eprintln!("unknown command: {arg}");
            return 2;
        }
    };
    match awari_ipc::send(&awari_ipc::socket_path(), &req) {
        Ok(ClientReply::Ok) => 0,
        Ok(ClientReply::Err(e)) => {
            eprintln!("{e}");
            1
        }
        Ok(other) => {
            println!("{}", serde_json::to_string(&other).unwrap());
            0
        }
        Err(e) => {
            eprintln!("{e}");
            1
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
