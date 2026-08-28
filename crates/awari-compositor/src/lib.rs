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
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;

use smithay_client_toolkit::reexports::client as wl;
use wayland_protocols_wlr::foreign_toplevel::v1::client::{
    zwlr_foreign_toplevel_handle_v1::{Event as HandleEvent, ZwlrForeignToplevelHandleV1},
    zwlr_foreign_toplevel_manager_v1::{Event as ManagerEvent, ZwlrForeignToplevelManagerV1},
};
use wl::backend::ObjectId;
use wl::event_created_child;
use wl::protocol::wl_output::{self, WlOutput};
use wl::protocol::wl_registry::{self, WlRegistry};
use wl::protocol::wl_seat::{self, WlSeat};
use wl::{Dispatch, Proxy, QueueHandle};

/// Value of the `activated` flag in the foreign-toplevel `state` array. The
/// `state` event carries an array of enum values
/// (maximized=0, minimized=1, activated=2, fullscreen=3); the scanner exposes
/// it as raw bytes.
const STATE_ACTIVATED: u8 = 2;

#[derive(Clone, Debug)]
pub enum CompositorCommand {
    FocusWindow { id: u64 },
}

#[derive(Debug, thiserror::Error)]
pub enum CompositorError {
    #[error("wayland: {0}")]
    Wayland(String),
    #[error("not connected")]
    NotConnected,
    #[error("spawn failed: {0}")]
    Spawn(String),
}

/// A single open toplevel window, normalized across compositors.
#[derive(Clone, Debug)]
pub struct Toplevel {
    pub id: u64,
    pub title: Option<String>,
    pub app_id: Option<String>,
}

/// Logical geometry of an output plus its scale factor. Enough for a client
/// to place a surface on the same monitor as a given toplevel without needing
/// to correlate `wl_output` identities across separate Wayland connections.
#[derive(Clone, Copy, Debug, Default)]
pub struct OutputRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    /// `wl_output` scale factor (>=1). The caller converts logical coordinates
    /// to physical pixels by multiplying by this when matching against its own
    /// display list.
    pub scale: i32,
}

pub trait Compositor: Send + Sync {
    fn apply(&self, cmd: CompositorCommand) -> Result<(), CompositorError>;
    /// All open toplevel windows (no workspace filtering).
    fn windows(&self) -> Vec<Toplevel>;
    /// Geometry of the output currently holding the focused (activated)
    /// toplevel, if any. Lets the launcher appear on the same monitor as the
    /// window the user is working in. `None` when there is no focus
    /// information (no compositor, or no activated toplevel).
    fn focused_output(&self) -> Option<OutputRect>;
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

/// Which compositor backend was bound at `connect()` time.
pub enum Backend {
    /// `wlr-foreign-toplevel` was available; window switching works.
    Wlr(Arc<dyn Compositor>),
    /// No foreign-toplevel global: apps / files / commands still work, but the
    /// window-switching rows are empty.
    Noop,
}

/// Connect to the running compositor. Returns the backend and its inbox. The
/// backend is `Backend::Noop` when the foreign-toplevel global is absent.
pub fn connect() -> (Backend, Arc<CompositorInbox>) {
    match connect_wlr() {
        Some((backend, inbox)) => (Backend::Wlr(backend), inbox),
        None => (Backend::Noop, CompositorInbox::new()),
    }
}

/// Spawn `command` (argv form) detached: a helper thread reaps the child so it
/// never lingers as a zombie. stdio is nulled because awari runs headless.
/// App launching is independent of the window-listing backend, so this is
/// exposed directly rather than routed through `Compositor::apply`.
pub fn spawn_detached(command: &[String]) -> Result<(), CompositorError> {
    if command.is_empty() {
        return Err(CompositorError::Spawn("empty command".into()));
    }
    std::process::Command::new(&command[0])
        .args(&command[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|mut child| {
            thread::Builder::new()
                .name("awari-spawn".into())
                .spawn(move || {
                    let _ = child.wait();
                })
                .ok();
        })
        .map_err(|e| CompositorError::Spawn(e.to_string()))
}

struct WlrInner {
    toplevels: HashMap<ObjectId, ToplevelInfo>,
    /// Outputs we've bound, keyed by their `wl_output` object id. Geometry is
    /// filled in as `wl_output` events arrive.
    outputs: HashMap<ObjectId, OutputInfo>,
    next_id: u64,
    dirty: bool,
    seat: Option<WlSeat>,
}

/// Accumulated `wl_output` geometry. Each field arrives in a separate event;
/// we assemble them as they come.
#[derive(Default)]
struct OutputInfo {
    geometry: Option<(i32, i32)>,
    mode: Option<(i32, i32)>,
    scale: i32,
}

struct ToplevelInfo {
    id: u64,
    handle: ZwlrForeignToplevelHandleV1,
    title: Option<String>,
    app_id: Option<String>,
    activated: bool,
    /// `wl_output` object ids the toplevel is currently shown on.
    outputs: Vec<ObjectId>,
}

struct WlrState {
    inner: Arc<Mutex<WlrInner>>,
    inbox: Arc<CompositorInbox>,
    manager: Option<ZwlrForeignToplevelManagerV1>,
    manager_name: Option<u32>,
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
    conn: Arc<wl::Connection>,
}

impl Compositor for WlrBackend {
    fn apply(&self, cmd: CompositorCommand) -> Result<(), CompositorError> {
        match cmd {
            CompositorCommand::FocusWindow { id } => {
                // Sent synchronously: the request just enqueues on the shared
                // connection, so it can't stall behind a blocked blocking_dispatch.
                let (handle, seat) = {
                    let g = self.inner.lock().expect("wlr inner");
                    let handle = g
                        .toplevels
                        .values()
                        .find(|t| t.id == id)
                        .map(|t| t.handle.clone());
                    (handle, g.seat.clone())
                };
                match (handle, seat) {
                    (Some(h), Some(seat)) => {
                        h.activate(&seat);
                        if let Err(e) = self.conn.flush() {
                            tracing::warn!(%e, "failed to flush focus request");
                        }
                        Ok(())
                    }
                    _ => Err(CompositorError::NotConnected),
                }
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

        out.sort_by_key(|(activated, t)| (std::cmp::Reverse(*activated), t.id));
        out.into_iter().map(|(_, t)| t).collect()
    }

    fn focused_output(&self) -> Option<OutputRect> {
        let g = self.inner.lock().expect("wlr inner");
        let toplevel = g.toplevels.values().find(|t| t.activated)?;
        let out_id = toplevel.outputs.first()?;
        let info = g.outputs.get(out_id)?;
        let (x, y) = info.geometry?;
        let (width, height) = info.mode?;

        Some(OutputRect {
            x,
            y,
            width,
            height,
            scale: info.scale.max(1),
        })
    }
}

fn connect_wlr() -> Option<(Arc<dyn Compositor>, Arc<CompositorInbox>)> {
    let conn = wl::Connection::connect_to_env().ok()?;
    let mut queue = conn.new_event_queue::<WlrState>();
    let qh = queue.handle();
    let inner = Arc::new(Mutex::new(WlrInner {
        toplevels: HashMap::new(),
        outputs: HashMap::new(),
        next_id: 1,
        dirty: false,
        seat: None,
    }));
    let inbox = CompositorInbox::new();
    let mut state = WlrState {
        inner: inner.clone(),
        inbox: inbox.clone(),
        manager: None,
        manager_name: None,
        _registry: None,
    };
    let registry = conn.display().get_registry(&qh, ());
    state._registry = Some(registry);
    queue.roundtrip(&mut state).ok()?;

    if state.manager.is_none() {
        tracing::warn!("compositor does not advertise wlr-foreign-toplevel; window list disabled");
        return None;
    }
    inbox.push(CompositorMsg::Changed);

    let conn = Arc::new(conn);
    let inbox_thread = inbox.clone();

    thread::Builder::new()
        .name("awari-wlr".into())
        .spawn(move || {
            loop {
                match queue.blocking_dispatch(&mut state) {
                    Ok(_) => {
                        state.inner.lock().expect("wlr inner").dirty = false;
                    }
                    Err(e) => {
                        // Connection died: clear the stale snapshot so the UI stops
                        // offering dead window rows, then stop the pump.
                        state.inner.lock().expect("wlr inner").toplevels.clear();
                        inbox_thread.push(CompositorMsg::Changed);
                        inbox_thread.push(CompositorMsg::Degraded(e.to_string()));
                        break;
                    }
                }
            }
        })
        .ok()?;

    Some((Arc::new(WlrBackend { inner, conn }), inbox))
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
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                if interface == ZwlrForeignToplevelManagerV1::interface().name {
                    let v = version.min(ZwlrForeignToplevelManagerV1::interface().version);
                    let manager = registry.bind::<ZwlrForeignToplevelManagerV1, (), WlrState>(
                        name,
                        v,
                        qh,
                        (),
                    );
                    st.manager = Some(manager);
                    st.manager_name = Some(name);
                } else if interface == WlSeat::interface().name {
                    let v = version.min(WlSeat::interface().version);
                    let seat = registry.bind::<WlSeat, (), WlrState>(name, v, qh, ());
                    let mut g = st.inner.lock().expect("wlr inner");

                    if g.seat.is_none() {
                        g.seat = Some(seat);
                    }
                } else if interface == WlOutput::interface().name {
                    let v = version.min(WlOutput::interface().version);
                    let output = registry.bind::<WlOutput, (), WlrState>(name, v, qh, ());

                    st.inner
                        .lock()
                        .expect("wlr inner")
                        .outputs
                        .insert(output.id(), OutputInfo::default());
                }
            }
            wl_registry::Event::GlobalRemove { name } if st.manager_name == Some(name) => {
                st.manager_name = None;
                st.inner.lock().expect("wlr inner").toplevels.clear();
                st.inbox.push(CompositorMsg::Changed);
                st.inbox.push(CompositorMsg::Degraded(
                    "foreign-toplevel global retracted".into(),
                ));
            }
            _ => {}
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
                        outputs: Vec::new(),
                    },
                );
                drop(g);
                st.mark_dirty();
            }
            ManagerEvent::Finished => {
                let handles: Vec<_> = st
                    .inner
                    .lock()
                    .expect("wlr inner")
                    .toplevels
                    .drain()
                    .map(|(_, t)| t.handle)
                    .collect();
                for h in handles {
                    h.destroy();
                }
                // The manager has no destroy request; dropping it releases the
                // proxy.
                st.manager.take();
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

impl Dispatch<WlOutput, ()> for WlrState {
    fn event(
        _st: &mut Self,
        proxy: &WlOutput,
        event: wl_output::Event,
        _data: &(),
        _conn: &wl::Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let oid = proxy.id();
        let mut g = _st.inner.lock().expect("wlr inner");
        let info = g.outputs.entry(oid).or_default();
        match event {
            wl_output::Event::Geometry { x, y, .. } => {
                info.geometry = Some((x, y));
            }
            wl_output::Event::Mode {
                flags,
                width,
                height,
                ..
            } => {
                let bits = match flags {
                    wl::WEnum::Value(m) => u32::from(m),
                    wl::WEnum::Unknown(v) => v,
                };
                if bits & 1 != 0 {
                    info.mode = Some((width, height));
                }
            }
            wl_output::Event::Scale { factor } => {
                info.scale = factor;
            }
            _ => {}
        }
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
                let activated = ws.as_chunks::<4>().0.iter().any(|c| {
                    u32::from_le_bytes([c[0], c[1], c[2], c[3]]) == STATE_ACTIVATED as u32
                });
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
            HandleEvent::OutputEnter { output } => {
                let out_id = output.id();
                let mut g = st.inner.lock().expect("wlr inner");
                if let Some(t) = g.toplevels.get_mut(&oid)
                    && !t.outputs.contains(&out_id)
                {
                    t.outputs.push(out_id);
                }
                drop(g);
                st.mark_dirty();
            }
            HandleEvent::OutputLeave { output } => {
                let out_id = output.id();
                let mut g = st.inner.lock().expect("wlr inner");
                if let Some(t) = g.toplevels.get_mut(&oid) {
                    t.outputs.retain(|o| o != &out_id);
                }
                drop(g);
                st.mark_dirty();
            }
            _ => {}
        }
    }
}
