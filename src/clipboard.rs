//! Wayland clipboard support for MuReader.
//!
//! This is deliberately a copy-only data-control client. It owns one Wayland
//! connection because a client must keep its connection alive while it owns
//! the host selection. A small dedicated thread drives the connection and
//! answers host paste requests; the UI thread only sends text to copy.

use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use wayland_client::backend::ObjectData;
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::{self, ExtDataControlManagerV1},
    ext_data_control_offer_v1::ExtDataControlOfferV1,
    ext_data_control_source_v1::{self, ExtDataControlSourceV1},
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::{self, ZwlrDataControlManagerV1},
    zwlr_data_control_offer_v1::ZwlrDataControlOfferV1,
    zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
};
use winit::event_loop::EventLoopProxy;

use crate::UserEvent;

pub enum ClipboardNotice {
    Copied(usize),
    Error(String),
}

enum Command {
    Copy(String),
    Quit,
}

pub struct ClipboardHandle {
    tx: Sender<Command>,
    wake: UnixStream,
    join: Option<JoinHandle<()>>,
}

impl ClipboardHandle {
    pub fn spawn(proxy: EventLoopProxy<UserEvent>) -> Result<Self, String> {
        let (tx, rx) = channel();
        let (wake_read, wake) =
            UnixStream::pair().map_err(|e| format!("clipboard wake pipe: {e}"))?;
        wake_read
            .set_nonblocking(true)
            .map_err(|e| format!("clipboard wake pipe: {e}"))?;
        let join = thread::Builder::new()
            .name("mureader-clipboard".to_string())
            .spawn(move || run(rx, wake_read, proxy))
            .map_err(|e| format!("spawn clipboard worker: {e}"))?;
        Ok(Self {
            tx,
            wake,
            join: Some(join),
        })
    }

    pub fn copy(&self, text: String) -> bool {
        if self.tx.send(Command::Copy(text)).is_err() {
            return false;
        }
        let mut wake = &self.wake;
        wake.write_all(&[1]).is_ok()
    }
}

impl Drop for ClipboardHandle {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Quit);
        let mut wake = &self.wake;
        let _ = wake.write_all(&[1]);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn run(rx: Receiver<Command>, wake: UnixStream, proxy: EventLoopProxy<UserEvent>) {
    let mut backend = match ClipboardProxy::new() {
        Some(backend) => backend,
        None => {
            let _ = proxy.send_event(UserEvent::Clipboard(ClipboardNotice::Error(
                "clipboard unavailable (host has no data-control protocol)".to_string(),
            )));
            return;
        }
    };
    let clipboard_fd = backend.fd().as_raw_fd();
    let wake_fd = wake.as_raw_fd();
    let mut text = String::new();

    loop {
        let mut fds = [
            libc::pollfd {
                fd: clipboard_fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wake_fd,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, -1) };
        if ready < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            let _ = proxy.send_event(UserEvent::Clipboard(ClipboardNotice::Error(format!(
                "clipboard poll: {err}"
            ))));
            return;
        }

        if fds[1].revents != 0 {
            drain_wake(&wake);
            loop {
                match rx.try_recv() {
                    Ok(Command::Copy(new_text)) => {
                        text = new_text;
                        if !backend.set_selection() {
                            let _ = proxy.send_event(UserEvent::Clipboard(ClipboardNotice::Error(
                                "clipboard selection failed".to_string(),
                            )));
                            return;
                        }
                        let _ = proxy.send_event(UserEvent::Clipboard(ClipboardNotice::Copied(
                            text.chars().count(),
                        )));
                    }
                    Ok(Command::Quit) => return,
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
                }
            }
        }

        if fds[0].revents != 0 {
            if fds[0].revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                let _ = proxy.send_event(UserEvent::Clipboard(ClipboardNotice::Error(
                    "clipboard connection closed".to_string(),
                )));
                return;
            }
            backend.dispatch();
            for fd in backend.take_send_requests() {
                let mut file = std::fs::File::from(fd);
                let _ = file.write_all(text.as_bytes());
            }
        }
    }
}

fn drain_wake(wake: &UnixStream) {
    let mut buf = [0u8; 64];
    loop {
        match (&*wake).read(&mut buf) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(_) => break,
        }
    }
}

enum Manager {
    Ext(ExtDataControlManagerV1),
    Wlr(ZwlrDataControlManagerV1),
}

impl Manager {
    fn create_source(&self, qh: &QueueHandle<ClipboardState>) -> Source {
        match self {
            Self::Ext(manager) => Source::Ext(manager.create_data_source(qh, ())),
            Self::Wlr(manager) => Source::Wlr(manager.create_data_source(qh, ())),
        }
    }

    fn get_device(&self, seat: &wl_seat::WlSeat, qh: &QueueHandle<ClipboardState>) -> Device {
        match self {
            Self::Ext(manager) => Device::Ext(manager.get_data_device(seat, qh, ())),
            Self::Wlr(manager) => Device::Wlr(manager.get_data_device(seat, qh, ())),
        }
    }
}

enum Device {
    Ext(ExtDataControlDeviceV1),
    Wlr(ZwlrDataControlDeviceV1),
}

impl Device {
    fn set_selection(&self, source: &Source) {
        match (self, source) {
            (Self::Ext(device), Source::Ext(source)) => device.set_selection(Some(source)),
            (Self::Wlr(device), Source::Wlr(source)) => device.set_selection(Some(source)),
            _ => unreachable!("data-control protocol mismatch"),
        }
    }
}

enum Source {
    Ext(ExtDataControlSourceV1),
    Wlr(ZwlrDataControlSourceV1),
}

impl Source {
    fn offer(&self, mime_type: &str) {
        match self {
            Self::Ext(source) => source.offer(mime_type.to_string()),
            Self::Wlr(source) => source.offer(mime_type.to_string()),
        }
    }

    fn destroy(&self) {
        match self {
            Self::Ext(source) => source.destroy(),
            Self::Wlr(source) => source.destroy(),
        }
    }
}

struct ClipboardState {
    manager: Option<Manager>,
    device: Option<Device>,
    seat: Option<wl_seat::WlSeat>,
    source: Option<Source>,
    send_requests: Vec<OwnedFd>,
}

pub struct ClipboardProxy {
    connection: Connection,
    queue: EventQueue<ClipboardState>,
    state: ClipboardState,
}

impl ClipboardProxy {
    pub fn new() -> Option<Self> {
        let connection = Connection::connect_to_env().ok()?;
        let mut queue = connection.new_event_queue::<ClipboardState>();
        let qh = queue.handle();
        let _registry = connection.display().get_registry(&qh, ());
        let mut state = ClipboardState {
            manager: None,
            device: None,
            seat: None,
            source: None,
            send_requests: Vec::new(),
        };
        queue.roundtrip(&mut state).ok()?;
        state.device = Some(
            state
                .manager
                .as_ref()?
                .get_device(state.seat.as_ref()?, &qh),
        );
        queue.roundtrip(&mut state).ok()?;
        Some(Self {
            connection,
            queue,
            state,
        })
    }

    pub fn fd(&self) -> BorrowedFd<'_> {
        self.connection.as_fd()
    }

    pub fn dispatch(&mut self) {
        if let Some(guard) = self.queue.prepare_read() {
            let _ = guard.read();
        }
        let _ = self.queue.dispatch_pending(&mut self.state);
    }

    pub fn take_send_requests(&mut self) -> Vec<OwnedFd> {
        std::mem::take(&mut self.state.send_requests)
    }

    pub fn set_selection(&mut self) -> bool {
        let (Some(manager), Some(device)) =
            (self.state.manager.as_ref(), self.state.device.as_ref())
        else {
            return false;
        };
        let source = manager.create_source(&self.queue.handle());
        source.offer("text/plain;charset=utf-8");
        source.offer("text/plain");
        device.set_selection(&source);
        if let Some(old) = self.state.source.replace(source) {
            old.destroy();
        }
        self.connection.flush().is_ok()
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for ClipboardState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        else {
            return;
        };
        match interface.as_str() {
            "ext_data_control_manager_v1" if state.manager.is_none() => {
                state.manager = Some(Manager::Ext(registry.bind(name, version.min(1), qh, ())));
            }
            "zwlr_data_control_manager_v1" if state.manager.is_none() => {
                state.manager = Some(Manager::Wlr(registry.bind(name, version.min(3), qh, ())));
            }
            "wl_seat" if state.seat.is_none() => {
                state.seat = Some(registry.bind(name, version.min(1), qh, ()));
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for ClipboardState {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

macro_rules! dispatch_manager {
    ($proxy:ty, $event:ty) => {
        impl Dispatch<$proxy, ()> for ClipboardState {
            fn event(
                _: &mut Self,
                _: &$proxy,
                _: $event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
            }
        }
    };
}

impl Dispatch<ExtDataControlDeviceV1, ()> for ClipboardState {
    fn event(
        state: &mut Self,
        _: &ExtDataControlDeviceV1,
        event: ext_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if matches!(event, ext_data_control_device_v1::Event::Finished) {
            state.device = None;
        }
    }

    fn event_created_child(opcode: u16, qh: &QueueHandle<Self>) -> Arc<dyn ObjectData> {
        assert_eq!(opcode, 0, "unexpected data-control child");
        qh.make_data::<ExtDataControlOfferV1, ()>(())
    }
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for ClipboardState {
    fn event(
        state: &mut Self,
        _: &ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if matches!(event, zwlr_data_control_device_v1::Event::Finished) {
            state.device = None;
        }
    }

    fn event_created_child(opcode: u16, qh: &QueueHandle<Self>) -> Arc<dyn ObjectData> {
        assert_eq!(opcode, 0, "unexpected data-control child");
        qh.make_data::<ZwlrDataControlOfferV1, ()>(())
    }
}

macro_rules! dispatch_offer {
    ($offer:ty) => {
        impl Dispatch<$offer, ()> for ClipboardState {
            fn event(
                _: &mut Self,
                _: &$offer,
                _: <$offer as Proxy>::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
            }
        }
    };
}

dispatch_manager!(ExtDataControlManagerV1, ext_data_control_manager_v1::Event);
dispatch_manager!(
    ZwlrDataControlManagerV1,
    zwlr_data_control_manager_v1::Event
);
dispatch_offer!(ExtDataControlOfferV1);
dispatch_offer!(ZwlrDataControlOfferV1);

impl Dispatch<ExtDataControlSourceV1, ()> for ClipboardState {
    fn event(
        state: &mut Self,
        _: &ExtDataControlSourceV1,
        event: ext_data_control_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_source_v1::Event::Send { fd, .. } => state.send_requests.push(fd),
            ext_data_control_source_v1::Event::Cancelled => state.source = None,
            _ => {}
        }
    }
}

impl Dispatch<ZwlrDataControlSourceV1, ()> for ClipboardState {
    fn event(
        state: &mut Self,
        _: &ZwlrDataControlSourceV1,
        event: zwlr_data_control_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_source_v1::Event::Send { fd, .. } => state.send_requests.push(fd),
            zwlr_data_control_source_v1::Event::Cancelled => state.source = None,
            _ => {}
        }
    }
}
