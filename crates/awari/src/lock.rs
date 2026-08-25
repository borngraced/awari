//! Stale-safe single-instance bind (architecture PR2).

use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};
use std::thread;

use awari_ipc::{ClientReply, ClientRequest, SOCKET_NAME};

pub struct IpcServer {
    pub listener: UnixListener,
}

#[derive(Default)]
pub struct Stats {
    pub launcher_open_to_first_commit_ms: Option<u64>,
}

pub fn acquire() -> Result<IpcServer, String> {
    let dir = awari_ipc::runtime_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    let path = dir.join(SOCKET_NAME);
    match UnixStream::connect(&path) {
        Ok(stream) => match ping_stream(stream) {
            Ok(ClientReply::Ok) => {
                return Err("already running (live daemon on ipc.sock)".into());
            }
            Ok(_) | Err(_) => {
                let _ = std::fs::remove_file(&path);
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
            let _ = std::fs::remove_file(&path);
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            tracing::warn!(%e, "connect ipc.sock");
            let _ = std::fs::remove_file(&path);
        }
    }
    let listener = UnixListener::bind(&path).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(IpcServer { listener })
}

fn ping_stream(mut stream: UnixStream) -> Result<ClientReply, String> {
    let mut line = serde_json::to_string(&ClientRequest::Ping).map_err(|e| e.to_string())?;
    line.push('\n');
    stream
        .write_all(line.as_bytes())
        .map_err(|e| e.to_string())?;
    stream.flush().map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream);
    let mut reply = String::new();
    reader.read_line(&mut reply).map_err(|e| e.to_string())?;
    serde_json::from_str(reply.trim()).map_err(|e| e.to_string())
}

fn peer_uid(stream: &UnixStream) -> Option<u32> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let fd = stream.as_raw_fd();
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    if rc == 0 { Some(cred.uid) } else { None }
}

static IPC_RX: Mutex<Option<std::sync::mpsc::Receiver<ClientRequest>>> = Mutex::new(None);

pub fn take_ipc_rx() -> Option<std::sync::mpsc::Receiver<ClientRequest>> {
    IPC_RX.lock().unwrap().take()
}

pub fn spawn_accept(listener: UnixListener, stats: Arc<Mutex<Stats>>) {
    let (tx, rx) = std::sync::mpsc::channel();
    *IPC_RX.lock().unwrap() = Some(rx);
    thread::Builder::new()
        .name("awari-ipc".into())
        .spawn(move || {
            let self_uid = unsafe { libc::getuid() };
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                if peer_uid(&stream) != Some(self_uid) {
                    tracing::warn!("ipc peer uid mismatch");
                    continue;
                }
                if let Err(e) = handle_client(stream, &stats, &tx) {
                    tracing::debug!(%e, "ipc client");
                }
            }
        })
        .expect("ipc accept thread");
}

fn handle_client(
    stream: UnixStream,
    stats: &Arc<Mutex<Stats>>,
    cmds: &std::sync::mpsc::Sender<ClientRequest>,
) -> Result<(), String> {
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    reader.read_line(&mut line).map_err(|e| e.to_string())?;
    let req: ClientRequest = serde_json::from_str(line.trim()).map_err(|e| e.to_string())?;
    let reply = match req {
        ClientRequest::Ping => ClientReply::Ok,
        ClientRequest::DumpStats => {
            let s = stats.lock().expect("stats");
            ClientReply::Stats {
                launcher_open_to_first_commit_ms: s.launcher_open_to_first_commit_ms,
                rss_bytes: rss_bytes(),
            }
        }
        ClientRequest::ToggleLauncher
        | ClientRequest::OpenLauncher
        | ClientRequest::CloseLauncher
        | ClientRequest::LauncherShown
        | ClientRequest::LauncherHidden => {
            let _ = cmds.send(req);
            ClientReply::Ok
        }
    };
    let mut out = serde_json::to_string(&reply).map_err(|e| e.to_string())?;
    out.push('\n');
    let mut stream = reader.into_inner();
    stream
        .write_all(out.as_bytes())
        .map_err(|e| e.to_string())?;
    stream.flush().map_err(|e| e.to_string())?;
    Ok(())
}

fn rss_bytes() -> u64 {
    if let Ok(s) = std::fs::read_to_string("/proc/self/statm") {
        if let Some(pages) = s.split_whitespace().nth(1) {
            if let Ok(n) = pages.parse::<u64>() {
                return n * 4096;
            }
        }
    }
    0
}
