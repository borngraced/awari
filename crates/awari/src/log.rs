//! Shared log pipe: drains the daemon's tracing and the GUI child's captured
//! stdout/stderr into `awari.log`.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::io::{FromRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;

use awari_ipc::runtime_dir;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;

const LOG_CAP: u64 = 1024 * 1024;
const LOG_COMPACT_HEADROOM: u64 = 1024 * 1024;

/// Set up the shared log pipe: the daemon's tracing and the GUI's captured
/// stdout/stderr both write to one pipe that a reader thread drains into
/// `awari.log`. Returns a dup'd write fd for the GUI child's stdout/stderr,
/// or -1 if the pipe can't be created.
///
/// The tracing subscriber writes through `PipeWriter(Arc<Mutex<File>>)` —
/// the mutex serialises concurrent writes so no interleaving is possible.
/// The returned child fd is a separate dup; `start_child` never touches
/// the shared `File`, avoiding the old deadlock where both paths competed
/// for the same lock.
pub(crate) fn init_log_pipe(filter: EnvFilter) -> RawFd {
    let mut fds = [-1i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        tracing_subscriber::fmt().with_env_filter(filter).init();
        return -1;
    }
    let read_fd = fds[0];
    let write_fd = fds[1];
    let path = runtime_dir().join("awari.log");

    // Dup before wrapping the original in Arc<Mutex<File>> — start_child
    // will use this copy; it never touches the shared File.
    let child_fd = unsafe { libc::dup(write_fd) };

    let shared = Arc::new(Mutex::new(unsafe { File::from_raw_fd(write_fd) }));

    match thread::Builder::new()
        .name("awari-log".into())
        .spawn(move || log_reader(read_fd, path))
    {
        Ok(_) => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(PipeWriter(shared))
                .init();
            child_fd
        }
        Err(_) => {
            unsafe {
                libc::close(read_fd);
                libc::close(child_fd);
            }
            tracing_subscriber::fmt().with_env_filter(filter).init();
            -1
        }
    }
}

fn log_reader(read_fd: RawFd, path: PathBuf) {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok();
    let mut buf = [0u8; 8192];
    loop {
        let n = unsafe { libc::read(read_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        match n {
            -1 => break,
            0 => break,
            n => {
                let n = n as usize;
                if file
                    .as_mut()
                    .map(|f| {
                        f.metadata()
                            .map(|m| m.len() > LOG_CAP + LOG_COMPACT_HEADROOM)
                            .unwrap_or(false)
                    })
                    .unwrap_or(false)
                    && compact_log(&path, LOG_CAP).is_ok()
                {
                    file = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&path)
                        .ok();
                }
                if let Some(f) = file.as_mut() {
                    let _ = f.write_all(&buf[..n]);
                    let _ = f.flush();
                }
            }
        }
    }
    unsafe {
        libc::close(read_fd);
    }
}

/// Rewrite `path` to keep at most `cap` bytes, advancing past the first
/// newline so we never truncate a line. Streams the tail out so the whole file
/// is never buffered in memory at once.
fn compact_log(path: &Path, cap: u64) -> std::io::Result<()> {
    let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if len <= cap {
        return Ok(());
    }
    let excess = (len - cap) as usize;
    let mut file = File::open(path)?;
    let mut buf = [0u8; 8192];
    let mut pos = 0usize;
    let mut start = excess;
    while pos < excess + 1 {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        for (i, &b) in buf[..n].iter().enumerate() {
            if pos + i >= excess && b == b'\n' {
                start = pos + i + 1;
                break;
            }
        }
        if start > excess {
            break;
        }
        pos += n;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut src = File::open(path)?;
        src.seek(SeekFrom::Start(start as u64))?;
        let mut dst = File::create(&tmp)?;
        std::io::copy(&mut src, &mut dst)?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// `MakeWriter` that fans tracing events into the shared log pipe.
///
/// Writes are serialised through the mutex, so concurrent callers from
/// different threads never interleave. Tracing lines are well under
/// `PIPE_BUF` (4096 on Linux), so even without the mutex a single
/// `write(2)` would be atomic — but the mutex also protects the
/// `File`'s internal buffer state.
struct PipeWriter(Arc<Mutex<File>>);

impl Write for PipeWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.lock().unwrap().flush()
    }
}

impl<'writer> MakeWriter<'writer> for PipeWriter {
    type Writer = PipeWriter;
    fn make_writer(&'writer self) -> PipeWriter {
        PipeWriter(Arc::clone(&self.0))
    }
}
