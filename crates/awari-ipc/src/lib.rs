//! Daemon unix protocol (not niri-ipc). Newline-delimited JSON.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const SOCKET_NAME: &str = "ipc.sock";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ClientRequest {
    ToggleLauncher,
    OpenLauncher,
    CloseLauncher,
    Ping,
    DumpStats,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ClientReply {
    Ok,
    Err(String),
    Stats {
        launcher_open_to_first_commit_ms: Option<u64>,
        rss_bytes: u64,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("empty reply")]
    Empty,
}

pub fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".awari")
}

pub fn socket_path() -> PathBuf {
    runtime_dir().join(SOCKET_NAME)
}

pub fn send(path: &Path, req: &ClientRequest) -> Result<ClientReply, IpcError> {
    let mut stream = UnixStream::connect(path)?;
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    stream.flush()?;
    let mut reader = BufReader::new(stream);
    let mut reply = String::new();
    reader.read_line(&mut reply)?;
    if reply.is_empty() {
        return Err(IpcError::Empty);
    }
    Ok(serde_json::from_str(reply.trim())?)
}

/// Client argv: ping a live daemon. Does not unlink.
pub fn ping_live() -> Result<ClientReply, IpcError> {
    send(&socket_path(), &ClientRequest::Ping)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_launcher_and_stats() {
        for req in [
            ClientRequest::ToggleLauncher,
            ClientRequest::OpenLauncher,
            ClientRequest::CloseLauncher,
            ClientRequest::DumpStats,
        ] {
            let s = serde_json::to_string(&req).unwrap();
            assert_eq!(serde_json::from_str::<ClientRequest>(&s).unwrap(), req);
        }
    }

    #[test]
    fn roundtrip_ping() {
        let s = serde_json::to_string(&ClientRequest::Ping).unwrap();
        assert_eq!(
            serde_json::from_str::<ClientRequest>(&s).unwrap(),
            ClientRequest::Ping
        );
        let r = serde_json::to_string(&ClientReply::Ok).unwrap();
        assert!(matches!(
            serde_json::from_str::<ClientReply>(&r).unwrap(),
            ClientReply::Ok
        ));
    }
}
