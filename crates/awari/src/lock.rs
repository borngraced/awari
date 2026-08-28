//! Stale-safe single-instance bind (architecture PR2).

use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::thread;

use thiserror::Error;

use awari_ipc::{ClientReply, ClientRequest, SOCKET_NAME};

pub struct IpcServer {
    pub listener: UnixListener,
    _lock: std::fs::File,
}

#[derive(Debug, Error)]
pub enum LockError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("already running")]
    AlreadyRunning,
}

pub fn acquire() -> Result<IpcServer, LockError> {
    let dir = awari_ipc::runtime_dir();
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    let lock_path = dir.join("daemon.lock");
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)?;
    #[cfg(unix)]
    unsafe {
        libc::fcntl(lock_file.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
    }
    let rc = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EAGAIN) => {
                return Err(LockError::AlreadyRunning);
            }
            _ => return Err(LockError::Io(err)),
        }
    }
    let path = dir.join(SOCKET_NAME);
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(IpcServer {
        listener,
        _lock: lock_file,
    })
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

pub fn spawn_accept(
    listener: UnixListener,
) -> std::sync::mpsc::Receiver<ClientRequest> {
    let (tx, rx) = std::sync::mpsc::channel();
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
                if let Err(e) = handle_client(stream, &tx) {
                    tracing::debug!(%e, "ipc client");
                }
            }
        })
        .expect("ipc accept thread");
    rx
}

fn handle_client(
    stream: UnixStream,
    cmds: &std::sync::mpsc::Sender<ClientRequest>,
) -> Result<(), LockError> {
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let req: ClientRequest = serde_json::from_str(line.trim())?;
    let reply = match req {
        ClientRequest::Ping => ClientReply::Ok,
        ClientRequest::ToggleLauncher
        | ClientRequest::OpenLauncher
        | ClientRequest::CloseLauncher
        | ClientRequest::LauncherShown
        | ClientRequest::LauncherHidden => {
            let _ = cmds.send(req);
            ClientReply::Ok
        }
    };
    let mut out = serde_json::to_string(&reply)?;
    out.push('\n');
    let mut stream = reader.into_inner();
    stream.write_all(out.as_bytes())?;
    stream.flush()?;
    Ok(())
}
