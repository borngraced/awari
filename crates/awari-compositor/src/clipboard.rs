//! Clipboard history watcher built on the `wlr-data-control-unstable-v1`
//! protocol, which most wlroots-family compositors implement. Each new
//! `selection` event is read as `text/plain` and surfaced to the daemon as a
//! `ClipboardEvent`, so the launcher can show a clipboard history section.

use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;

use smithay_client_toolkit::reexports::client as wl;
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{
        Event as DeviceEvent, ZwlrDataControlDeviceV1,
    },
    zwlr_data_control_manager_v1::{Event as ManagerEvent, ZwlrDataControlManagerV1},
    zwlr_data_control_offer_v1::{
        Event as OfferEvent, ZwlrDataControlOfferV1,
    },
};
use wl::protocol::wl_registry::{self, WlRegistry};
use wl::protocol::wl_seat::{self, WlSeat};
use wl::{event_created_child, Dispatch, Proxy, QueueHandle};

/// How many clipboard entries the in-memory watcher will buffer before the
/// daemon drains them. Generous so a burst of copies is not silently dropped.
const CLIPBOARD_BUFFER: usize = 256;

#[derive(Debug)]
pub enum ClipboardEvent {
    /// A new clipboard text was captured (`receive` finished reading).
    Text(String),
    /// The data-control connection died; the watcher has stopped.
    Degraded(String),
}

pub struct ClipboardInbox {
    pending: Mutex<VecDeque<ClipboardEvent>>,
    wake_rx: Mutex<Option<Receiver<()>>>,
    wake_tx: SyncSender<()>,
}

impl ClipboardInbox {
    fn new() -> Arc<Self> {
        let (wake_tx, wake_rx) = mpsc::sync_channel(1);
        Arc::new(Self {
            pending: Mutex::new(VecDeque::new()),
            wake_rx: Mutex::new(Some(wake_rx)),
            wake_tx,
        })
    }

    fn push(&self, event: ClipboardEvent) {
        let mut pending = self.pending.lock().expect("clipboard pending");
        while pending.len() >= CLIPBOARD_BUFFER {
            pending.pop_front();
        }
        pending.push_back(event);
        drop(pending);
        match self.wake_tx.try_send(()) {
            Ok(()) | Err(TrySendError::Full(())) => {}
            Err(TrySendError::Disconnected(())) => {}
        }
    }

    pub fn take_wake(&self) -> Option<Receiver<()>> {
        self.wake_rx.lock().expect("clipboard wake").take()
    }

    pub fn drain(&self) -> Vec<ClipboardEvent> {
        std::mem::take(&mut *self.pending.lock().expect("clipboard pending")).into()
    }
}

/// Spawn a background watcher that follows the clipboard on a dedicated
/// Wayland connection. Returns `None` when the compositor does not advertise
/// `wlr-data-control`.
pub fn spawn_clipboard_watcher() -> Option<Arc<ClipboardInbox>> {
    let conn = wl::Connection::connect_to_env().ok()?;
    let mut queue = conn.new_event_queue::<ClipboardState>();
    let qh = queue.handle();
    let inbox = ClipboardInbox::new();

    let mut state = ClipboardState {
        inbox: inbox.clone(),
        manager: None,
        manager_name: None,
        device: None,
        seat: None,
        _registry: None,
    };
    let registry = conn.display().get_registry(&qh, ());
    state._registry = Some(registry);
    queue.roundtrip(&mut state).ok()?;

    if state.manager.is_none() || state.seat.is_none() {
        tracing::debug!("compositor does not advertise wlr-data-control; clipboard history disabled");
        return None;
    }
    // The manager and seat are bound after the roundtrip above; create the
    // per-seat device now so `selection` events start flowing.
    state.ensure_device(&qh);

    thread::Builder::new()
        .name("awari-clipboard".into())
        .spawn(move || {
            loop {
                match queue.blocking_dispatch(&mut state) {
                    Ok(_) => {}
                    Err(e) => {
                        state.inbox.push(ClipboardEvent::Degraded(e.to_string()));
                        break;
                    }
                }
            }
        })
        .ok()?;

    Some(inbox)
}

struct ClipboardState {
    inbox: Arc<ClipboardInbox>,
    manager: Option<ZwlrDataControlManagerV1>,
    manager_name: Option<u32>,
    device: Option<ZwlrDataControlDeviceV1>,
    seat: Option<WlSeat>,
    _registry: Option<WlRegistry>,
}

impl Dispatch<WlRegistry, ()> for ClipboardState {
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
                if interface == ZwlrDataControlManagerV1::interface().name {
                    let v = version.min(ZwlrDataControlManagerV1::interface().version);
                    let manager = registry.bind::<ZwlrDataControlManagerV1, (), ClipboardState>(
                        name,
                        v,
                        qh,
                        (),
                    );
                    st.manager = Some(manager);
                    st.manager_name = Some(name);
                } else if interface == WlSeat::interface().name {
                    let v = version.min(WlSeat::interface().version);
                    let seat = registry.bind::<WlSeat, (), ClipboardState>(name, v, qh, ());
                    if st.seat.is_none() {
                        st.seat = Some(seat);
                    }
                }
            }
            wl_registry::Event::GlobalRemove { name } if st.manager_name == Some(name) => {
                st.manager.take();
                st.manager_name = None;
                st.device.take();
                st.inbox.push(ClipboardEvent::Degraded(
                    "wlr-data-control global retracted".into(),
                ));
            }
            _ => {}
        }
    }
}

impl ClipboardState {
    /// Create the per-seat data device once both the manager and a seat are
    /// available. `get_data_device` needs a bound `wl_seat`.
    fn ensure_device(&mut self, qh: &QueueHandle<Self>) {
        if self.device.is_none()
            && let Some(manager) = self.manager.as_ref()
            && let Some(seat) = self.seat.as_ref()
        {
            let device = manager.get_data_device(seat, qh, ());
            self.device = Some(device);
        }
    }
}

impl Dispatch<ZwlrDataControlManagerV1, ()> for ClipboardState {
    fn event(
        st: &mut Self,
        _proxy: &ZwlrDataControlManagerV1,
        _event: ManagerEvent,
        _data: &(),
        _conn: &wl::Connection,
        qh: &QueueHandle<Self>,
    ) {
        st.ensure_device(qh);
    }
}

impl Dispatch<WlSeat, ()> for ClipboardState {
    fn event(
        st: &mut Self,
        _proxy: &WlSeat,
        event: wl_seat::Event,
        _data: &(),
        _conn: &wl::Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities { .. } = event {}
        st.ensure_device(qh);
    }
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for ClipboardState {
    fn event(
        st: &mut Self,
        _proxy: &ZwlrDataControlDeviceV1,
        event: DeviceEvent,
        _data: &(),
        _conn: &wl::Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            // Read `text/plain` on a side thread so the dispatch loop never
            // blocks on a slow source.
            DeviceEvent::Selection { id: Some(offer) } => read_offer_text(offer, st.inbox.clone()),
            DeviceEvent::Selection { id: None } | DeviceEvent::PrimarySelection { .. } => {}
            DeviceEvent::Finished => {
                st.inbox.push(ClipboardEvent::Degraded(
                    "wlr-data-control device finished".into(),
                ));
            }
            _ => {}
        }
    }

    event_created_child!(ClipboardState, ZwlrDataControlDeviceV1, [
        // The `data_offer` event introduces a `zwlr_data_control_offer_v1`
        // child that is subsequently referenced by `selection` /
        // `primary_selection`. Without this specialization wayland-client
        // panics on the un-handled child creation.
        0u16 => (ZwlrDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ZwlrDataControlOfferV1, ()> for ClipboardState {
    fn event(
        _st: &mut Self,
        _proxy: &ZwlrDataControlOfferV1,
        _event: OfferEvent,
        _data: &(),
        _conn: &wl::Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

/// Spawn a thread that requests `text/plain` from the offer, reads the pipe,
/// and pushes the resulting text (or ignores the failure) into the inbox.
fn read_offer_text(offer: ZwlrDataControlOfferV1, inbox: Arc<ClipboardInbox>) {
    let mut fds = [0i32; 2];
    // SAFETY: `fds` is a valid two-element buffer; on failure (non-zero return)
    // no valid FD was written so there is nothing to close.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        let e = std::io::Error::last_os_error();
        tracing::warn!(%e, "failed to create clipboard pipe");
        return;
    }
    let (read_fd, write_fd) = (fds[0], fds[1]);

    // `offer.receive` must run on the Wayland dispatch thread.
    let write_fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(write_fd) };
    offer.receive("text/plain".to_string(), write_fd);

    thread::Builder::new()
        .name("awari-clipboard-read".into())
        .spawn(move || {
            use std::io::Read;
            use std::os::fd::FromRawFd;
            let mut buf = Vec::new();
            let mut file = unsafe { std::fs::File::from_raw_fd(read_fd) };
            if file.read_to_end(&mut buf).is_ok()
                && let Ok(text) = String::from_utf8(buf)
                && !text.trim().is_empty()
            {
                inbox.push(ClipboardEvent::Text(text));
            }
        })
        .ok();
}
