//! Daemon unix protocol (not niri-ipc). Newline-delimited JSON.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Mutex, Once};

use serde::{Deserialize, Serialize};

pub const SOCKET_NAME: &str = "ipc.sock";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ClientRequest {
    ToggleLauncher,
    OpenLauncher,
    CloseLauncher,
    /// Sent by the GUI to the daemon when the overlay actually becomes visible
    /// (e.g. opened via the Open signal, or at startup in open mode).
    LauncherShown,
    /// Sent by the GUI to the daemon when the overlay actually becomes hidden
    /// (e.g. dismissed via Escape/click inside the GUI). Keeps the daemon's
    /// `visible` flag truthful even when the close was not daemon-initiated.
    LauncherHidden,
    /// Stop the GUI and re-exec the daemon so a fresh `config.kdl` is loaded.
    Restart,
    Ping,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ClientReply {
    Ok,
    Err(String),
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

/// Durable state (history, usage, panel position). Survives reboot.
pub fn state_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from(".local/state"))
        .join("awari")
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

static NOTIFY_PUMP: Once = Once::new();
static NOTIFY_TX: Mutex<Option<mpsc::Sender<ClientRequest>>> = Mutex::new(None);

/// One request, one connection, read the reply. The daemon's accept loop is
/// one-shot per accept, so a kept-alive stream's second write is never read.
fn notify_once(line: &str) -> Result<(), IpcError> {
    let mut stream = UnixStream::connect(socket_path())?;
    stream.write_all(line.as_bytes())?;
    stream.flush()?;
    let mut reader = BufReader::new(stream);
    let mut reply = String::new();
    reader.read_line(&mut reply)?;
    if reply.is_empty() {
        return Err(IpcError::Empty);
    }
    let _ = serde_json::from_str::<ClientReply>(reply.trim())?;
    Ok(())
}

/// Fire-and-forget status ping from the GUI back to the daemon (e.g. when the
/// overlay is actually shown or hidden by an in-GUI action such as Escape).
/// Writes are serialized through one pump thread so rapid show/hide pings
/// can't be reordered on the socket and desync the daemon's `visible` flag.
/// Each ping is retried once on a fresh connection so a dead peer cannot drop
/// `LauncherHidden` and invert the next toggle.
pub fn notify(req: ClientRequest) {
    NOTIFY_PUMP.call_once(|| {
        let (tx, rx) = mpsc::channel::<ClientRequest>();
        *NOTIFY_TX.lock().unwrap() = Some(tx);
        std::thread::Builder::new()
            .name("awari-notify".into())
            .spawn(move || {
                for req in rx {
                    let mut line = match serde_json::to_string(&req) {
                        Ok(l) => l,
                        Err(_) => continue,
                    };
                    line.push('\n');
                    if notify_once(&line).is_err() {
                        let _ = notify_once(&line);
                    }
                }
            })
            .expect("notify pump thread");
    });
    if let Some(tx) = NOTIFY_TX.lock().unwrap().clone() {
        let _ = tx.send(req);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_launcher() {
        for req in [
            ClientRequest::ToggleLauncher,
            ClientRequest::OpenLauncher,
            ClientRequest::CloseLauncher,
            ClientRequest::LauncherShown,
            ClientRequest::LauncherHidden,
            ClientRequest::Restart,
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
