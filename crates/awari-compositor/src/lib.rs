//! Dual-socket niri adapter. Event stream is write-once.

use std::sync::mpsc::{self, Receiver, Sender, TrySendError};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use niri_ipc::socket::Socket;
use niri_ipc::{Action, Event, Request, Response};

pub const NIRI_IPC_PIN: &str = "26.4.0";

#[derive(Clone, Debug)]
pub enum CompositorCommand {
    FocusWindow { id: u64 },
    Spawn { command: Vec<String> },
}

#[derive(Debug, thiserror::Error)]
pub enum CompositorError {
    #[error("niri ipc: {0}")]
    Ipc(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("not connected")]
    NotConnected,
}

pub trait Compositor: Send + Sync {
    fn apply(&self, cmd: CompositorCommand) -> Result<(), CompositorError>;
}

pub struct NiriHandle {
    commands: Mutex<Option<Socket>>,
}

impl NiriHandle {
    pub fn connect_commands() -> Result<Self, CompositorError> {
        let socket = Socket::connect().map_err(|e| CompositorError::Ipc(e.to_string()))?;
        Ok(Self {
            commands: Mutex::new(Some(socket)),
        })
    }

    fn send_action(&self, action: Action) -> Result<(), CompositorError> {
        let mut guard = self.commands.lock().expect("command mutex");
        let sock = guard.as_mut().ok_or(CompositorError::NotConnected)?;
        let reply = sock
            .send(Request::Action(action))
            .map_err(|e| CompositorError::Ipc(e.to_string()))?;
        match reply {
            Ok(_) => Ok(()),
            Err(msg) => Err(CompositorError::Ipc(msg)),
        }
    }
}

impl Compositor for NiriHandle {
    fn apply(&self, cmd: CompositorCommand) -> Result<(), CompositorError> {
        match cmd {
            CompositorCommand::FocusWindow { id } => self.send_action(Action::FocusWindow { id }),
            CompositorCommand::Spawn { command } => self.send_action(Action::Spawn { command }),
        }
    }
}

#[derive(Clone, Debug)]
pub enum NiriMsg {
    Event(Event),
    Outputs(std::collections::HashMap<String, niri_ipc::Output>),
    Degraded(String),
    Version(String),
}

/// Coalesced niri events: many compositor messages, one UI wake.
pub struct NiriInbox {
    pending: Mutex<Vec<NiriMsg>>,
    wake_rx: Mutex<Option<Receiver<()>>>,
}

impl NiriInbox {
    pub fn start() -> std::sync::Arc<Self> {
        let (wake_tx, wake_rx) = mpsc::sync_channel(1);
        let inbox = std::sync::Arc::new(Self {
            pending: Mutex::new(Vec::new()),
            wake_rx: Mutex::new(Some(wake_rx)),
        });
        let (tx, rx) = mpsc::channel();
        spawn_event_thread(tx);
        let inbox_fwd = inbox.clone();
        thread::Builder::new()
            .name("awari-niri-coalesce".into())
            .spawn(move || {
                while let Ok(msg) = rx.recv() {
                    inbox_fwd.pending.lock().expect("niri pending").push(msg);
                    match wake_tx.try_send(()) {
                        Ok(()) | Err(TrySendError::Full(())) => {}
                        Err(TrySendError::Disconnected(())) => break,
                    }
                }
            })
            .expect("niri coalesce thread");
        inbox
    }

    pub fn take_wake(&self) -> Option<Receiver<()>> {
        self.wake_rx.lock().expect("niri wake").take()
    }

    pub fn drain(&self) -> Vec<NiriMsg> {
        std::mem::take(&mut *self.pending.lock().expect("niri pending"))
    }
}

fn spawn_event_thread(tx: Sender<NiriMsg>) {
    thread::Builder::new()
        .name("awari-niri-events".into())
        .spawn(move || event_loop(tx))
        .expect("spawn niri event thread");
}

fn event_loop(tx: Sender<NiriMsg>) {
    let mut backoff = Duration::from_millis(200);
    loop {
        match run_stream(&tx) {
            Ok(()) => backoff = Duration::from_millis(200),
            Err(e) => {
                let _ = tx.send(NiriMsg::Degraded(e.to_string()));
                thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(5));
            }
        }
    }
}

fn run_stream(tx: &Sender<NiriMsg>) -> Result<(), CompositorError> {
    let mut sock = Socket::connect().map_err(|e| CompositorError::Ipc(e.to_string()))?;

    if let Ok(Ok(Response::Version(v))) = sock.send(Request::Version) {
        tracing::info!(crate = NIRI_IPC_PIN, niri = %v, "niri version vs pin");
        let _ = tx.send(NiriMsg::Version(v));
    }

    if let Ok(Ok(Response::Outputs(outs))) = sock.send(Request::Outputs) {
        let _ = tx.send(NiriMsg::Outputs(outs));
    }

    sock.send(Request::EventStream)
        .map_err(|e| CompositorError::Ipc(e.to_string()))?
        .map_err(CompositorError::Ipc)?;

    let mut next = sock.read_events();
    loop {
        let event = next().map_err(|e| CompositorError::Ipc(e.to_string()))?;
        if matches!(event, Event::ConfigLoaded { .. }) {
            if let Ok(mut cmd) = Socket::connect() {
                if let Ok(Ok(Response::Outputs(outs))) = cmd.send(Request::Outputs) {
                    let _ = tx.send(NiriMsg::Outputs(outs));
                }
            }
        }
        let _ = tx.send(NiriMsg::Event(event));
    }
}
