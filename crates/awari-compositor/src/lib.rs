//! Generic Wayland compositor backend for awari.
//!
//! Window listing uses the de-facto cross-compositor protocol
//! `wlr-foreign-toplevel-management-unstable-v1`, which is implemented by the
//! wlroots family (sway, hyprland, river, wayfire, labwc, …) and, in recent
//! releases, by niri as well. awari no longer depends on any single compositor
//! at runtime — it works on any compositor that advertises the foreign-toplevel
//! global. When the global is absent, the launcher still works for apps / files
//! / commands; only the window-switching rows are empty.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;

use smithay_client_toolkit::reexports::client as wl;
use wl::protocol::wl_registry::{self, WlRegistry};
use wl::protocol::wl_seat::{self, WlSeat};
use wl::backend::ObjectId;
use wl::{Dispatch, QueueHandle, Proxy};
use wl::event_created_child;
use wayland_protocols_wlr::foreign_toplevel::v1::client::{
    zwlr_foreign_toplevel_handle_v1::{Event as HandleEvent, ZwlrForeignToplevelHandleV1},
    zwlr_foreign_toplevel_manager_v1::{Event as ManagerEvent, ZwlrForeignToplevelManagerV1},
};

/// Value of the `activated` flag in the foreign-toplevel `state` array. The
/// `state` event carries an array of enum values
/// (maximized=0, minimized=1, activated=2, fullscreen=3); the scanner exposes
/// it as raw bytes.
const STATE_ACTIVATED: u8 = 2;

#[derive(Clone, Debug)]
pub enum CompositorCommand {
    FocusWindow { id: u64 },
    Spawn { command: Vec<String> },
}

#[derive(Debug, thiserror::Error)]
pub enum CompositorError {
    #[error("wayland: {0}")]
    Wayland(String),
    #[error("not connected")]
    NotConnected,
}

/// A single open toplevel window, normalized across compositors.
pub struct Toplevel {
    pub id: u64,
    pub title: Option<String>,
    pub app_id: Option<String>,
}

pub trait Compositor: Send + Sync {
    fn apply(&self, cmd: CompositorCommand) -> Result<(), CompositorError>;
    /// All open toplevel windows (no workspace filtering).
    fn windows(&self) -> Vec<Toplevel>;
}

/// Coalesced compositor events: many protocol messages, one UI wake.
pub enum CompositorMsg {
    Changed,
    Degraded(String),
}

pub struct CompositorInbox {
    pending: Mutex<Vec<CompositorMsg>>,
    wake_rx: Mutex<Option<Receiver<()>>>,
    wake_tx: SyncSender<()>,
}

impl CompositorInbox {
    fn new() -> Arc<Self> {
        let (wake_tx, wake_rx) = mpsc::sync_channel(1);
        Arc::new(Self {
            pending: Mutex::new(Vec::new()),
            wake_rx: Mutex::new(Some(wake_rx)),
            wake_tx,
        })
    }

    fn push(&self, msg: CompositorMsg) {
        self.pending.lock().expect("inbox pending").push(msg);
        match self.wake_tx.try_send(()) {
            Ok(()) | Err(TrySendError::Full(())) => {}
            Err(TrySendError::Disconnected(())) => {}
        }
    }

    pub fn take_wake(&self) -> Option<Receiver<()>> {
        self.wake_rx.lock().expect("inbox wake").take()
    }

    pub fn drain(&self) -> Vec<CompositorMsg> {
        std::mem::take(&mut *self.pending.lock().expect("inbox pending"))
    }
}

/// Connect to the running compositor. Returns a no-op backend (empty windows)
/// when the foreign-toplevel global is unavailable, so the launcher still
/// launches apps/files/commands.
pub fn connect() -> (Arc<dyn Compositor>, Arc<CompositorInbox>) {
    match connect_wlr() {
        Some(pair) => pair,
        None => (Arc::new(NoopCompositor), CompositorInbox::new()),
    }
}

/// Spawn `command` (argv form) detached: a helper thread reaps the child so it
/// never lingers as a zombie. stdio is nulled because awari runs headless.
/// Returns false if the program could not be launched.
fn spawn_detached(command: &[String]) -> bool {
    if command.is_empty() {
        return false;
    }
    match std::process::Command::new(&command[0])
        .args(&command[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            thread::Builder::new()
                .name("awari-spawn".into())
                .spawn(move || {
                    let _ = child.wait();
                })
                .ok();
            true
        }
        Err(e) => {
            tracing::warn!(%e, cmd = ?command.first(), "failed to spawn");
            false
        }
    }
}

struct WlrInner {
    toplevels: HashMap<ObjectId, ToplevelInfo>,
    next_id: u64,
    dirty: bool,
}

struct ToplevelInfo {
    id: u64,
    handle: ZwlrForeignToplevelHandleV1,
    title: Option<String>,
    app_id: Option<String>,
    activated: bool,
}

struct WlrState {
    inner: Arc<Mutex<WlrInner>>,
    inbox: Arc<CompositorInbox>,
    manager: Option<ZwlrForeignToplevelManagerV1>,
    seat: Option<WlSeat>,
    _registry: Option<WlRegistry>,
}

impl WlrState {
    fn mark_dirty(&self) {
        let mut g = self.inner.lock().expect("wlr inner");
        if !g.dirty {
            g.dirty = true;
            self.inbox.push(CompositorMsg::Changed);
        }
    }
}

struct WlrBackend {
    inner: Arc<Mutex<WlrInner>>,
    cmd_tx: Sender<CompositorCommand>,
}

impl Compositor for WlrBackend {
    fn apply(&self, cmd: CompositorCommand) -> Result<(), CompositorError> {
        match cmd {
            CompositorCommand::Spawn { command } => {
                if spawn_detached(&command) {
                    Ok(())
                } else {
                    Err(CompositorError::NotConnected)
                }
            }
            CompositorCommand::FocusWindow { id } => {
                let _ = self.cmd_tx.send(CompositorCommand::FocusWindow { id });
                Ok(())
            }
        }
    }

    fn windows(&self) -> Vec<Toplevel> {
        let g = self.inner.lock().expect("wlr inner");
        let mut out: Vec<(bool, Toplevel)> = g
            .toplevels
            .values()
            .map(|t| {
                (
                    t.activated,
                    Toplevel {
                        id: t.id,
                        title: t.title.clone(),
                        app_id: t.app_id.clone(),
                    },
                )
            })
            .collect();
        // Focused window first, then stable by id so unfocused rows don't reshuffle.
        out.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.id.cmp(&b.1.id)));
        out.into_iter().map(|(_, t)| t).collect()
    }
}

struct NoopCompositor;

impl Compositor for NoopCompositor {
    fn apply(&self, cmd: CompositorCommand) -> Result<(), CompositorError> {
        match cmd {
            CompositorCommand::Spawn { command } => {
                if spawn_detached(&command) {
                    Ok(())
                } else {
                    Err(CompositorError::NotConnected)
                }
            }
            CompositorCommand::FocusWindow { .. } => Ok(()),
        }
    }
    fn windows(&self) -> Vec<Toplevel> {
        Vec::new()
    }
}

fn connect_wlr() -> Option<(Arc<dyn Compositor>, Arc<CompositorInbox>)> {
    let conn = wl::Connection::connect_to_env().ok()?;
    let mut queue = conn.new_event_queue::<WlrState>();
    let qh = queue.handle();
    let inner = Arc::new(Mutex::new(WlrInner {
        toplevels: HashMap::new(),
        next_id: 1,
        dirty: false,
    }));
    let inbox = CompositorInbox::new();
    let mut state = WlrState {
        inner: inner.clone(),
        inbox: inbox.clone(),
        manager: None,
        seat: None,
        _registry: None,
    };
    let registry = conn.display().get_registry(&qh, ());
    state._registry = Some(registry);
    queue.roundtrip(&mut state).ok()?;
    if state.manager.is_none() {
        tracing::warn!(
            "compositor does not advertise wlr-foreign-toplevel; window list disabled"
        );
        return None;
    }
    inbox.push(CompositorMsg::Changed);

    let (cmd_tx, cmd_rx) = mpsc::channel::<CompositorCommand>();
    let inbox_thread = inbox.clone();
    thread::Builder::new()
        .name("awari-wlr".into())
        .spawn(move || loop {
            match queue.blocking_dispatch(&mut state) {
                Ok(_) => {
                    while let Ok(cmd) = cmd_rx.try_recv() {
                        handle_command(&mut state, &queue, cmd);
                    }
                    state.inner.lock().expect("wlr inner").dirty = false;
                }
                Err(e) => {
                    inbox_thread.push(CompositorMsg::Degraded(e.to_string()));
                    break;
                }
            }
        })
        .ok()?;
    Some((Arc::new(WlrBackend { inner, cmd_tx }), inbox))
}

fn handle_command(state: &mut WlrState, _queue: &wl::EventQueue<WlrState>, cmd: CompositorCommand) {
    if let CompositorCommand::FocusWindow { id } = cmd {
        let (handle, seat) = {
            let g = state.inner.lock().expect("wlr inner");
            let handle = g
                .toplevels
                .values()
                .find(|t| t.id == id)
                .map(|t| t.handle.clone());
            (handle, state.seat.clone())
        };
        if let (Some(h), Some(seat)) = (handle, seat) {
            h.activate(&seat);
        } else {
            tracing::warn!("cannot focus window: no seat or handle");
        }
    }
}

impl Dispatch<WlRegistry, ()> for WlrState {
    fn event(
        st: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &wl::Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            if interface == ZwlrForeignToplevelManagerV1::interface().name {
                let v = version.min(ZwlrForeignToplevelManagerV1::interface().version);
                let manager =
                    registry.bind::<ZwlrForeignToplevelManagerV1, (), WlrState>(name, v, qh, ());
                st.manager = Some(manager);
            } else if interface == WlSeat::interface().name {
                let v = version.min(WlSeat::interface().version);
                let seat = registry.bind::<WlSeat, (), WlrState>(name, v, qh, ());
                if st.seat.is_none() {
                    st.seat = Some(seat);
                }
            }
        }
    }
}

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for WlrState {
    fn event(
        st: &mut Self,
        _proxy: &ZwlrForeignToplevelManagerV1,
        event: ManagerEvent,
        _data: &(),
        _conn: &wl::Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ManagerEvent::Toplevel { toplevel } => {
                let mut g = st.inner.lock().expect("wlr inner");
                let id = g.next_id;
                g.next_id += 1;
                g.toplevels.insert(
                    toplevel.id(),
                    ToplevelInfo {
                        id,
                        handle: toplevel.clone(),
                        title: None,
                        app_id: None,
                        activated: false,
                    },
                );
                drop(g);
                st.mark_dirty();
            }
            ManagerEvent::Finished => {
                st.inner.lock().expect("wlr inner").toplevels.clear();
                st.mark_dirty();
            }
            _ => {}
        }
    }

    event_created_child!(WlrState, ZwlrForeignToplevelManagerV1, [
        0u16 => (ZwlrForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<WlSeat, ()> for WlrState {
    fn event(
        _st: &mut Self,
        _proxy: &WlSeat,
        _event: wl_seat::Event,
        _data: &(),
        _conn: &wl::Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrForeignToplevelHandleV1, ()> for WlrState {
    fn event(
        st: &mut Self,
        proxy: &ZwlrForeignToplevelHandleV1,
        event: HandleEvent,
        _data: &(),
        _conn: &wl::Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let oid = proxy.id();
        match event {
            HandleEvent::Title { title } => {
                if let Some(t) = st.inner.lock().expect("wlr inner").toplevels.get_mut(&oid) {
                    t.title = Some(title);
                }
                st.mark_dirty();
            }
            HandleEvent::AppId { app_id } => {
                if let Some(t) = st.inner.lock().expect("wlr inner").toplevels.get_mut(&oid) {
                    t.app_id = Some(app_id);
                }
                st.mark_dirty();
            }
            HandleEvent::State { state: ws } => {
                let activated = ws.contains(&STATE_ACTIVATED);
                if let Some(t) = st.inner.lock().expect("wlr inner").toplevels.get_mut(&oid) {
                    t.activated = activated;
                }
                st.mark_dirty();
            }
            HandleEvent::Closed => {
                st.inner.lock().expect("wlr inner").toplevels.remove(&oid);
                proxy.destroy();
                st.mark_dirty();
            }
            _ => {}
        }
    }
}
