//! Files dropped from the file manager.
//!
//! Slint 1.19's winit backend doesn't deliver external drops, and winit 0.30 has
//! no Wayland drag-and-drop at all, so:
//!
//! * **Wayland**: we attach to the window's own `wl_display` with a private event
//!   queue, register a `wl_data_device` per seat and implement the receiving side of
//!   the drag-and-drop protocol (accept `text/uri-list`, read it through a pipe on drop).
//!   KWin, Mutter and wlroots send drag events to every data device a client owns, so
//!   this works alongside winit and Slint's clipboard.
//! * **X11**: winit already implements XDND and reports `HoveredFile`/`DroppedFile`.

use std::collections::HashMap;
use std::io::Read;
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::path::PathBuf;

use wayland_client::backend::ObjectId;
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::wl_data_device::{self, WlDataDevice};
use wayland_client::protocol::wl_data_device_manager::{DndAction, WlDataDeviceManager};
use wayland_client::protocol::wl_data_offer::{self, WlDataOffer};
use wayland_client::protocol::{wl_registry, wl_seat::WlSeat};
use wayland_client::{event_created_child, Connection, Dispatch, Proxy, QueueHandle};

#[derive(Debug, Clone)]
pub enum DndEvent {
    /// Files are being dragged over the window (true) or left it (false).
    Hover(bool),
    Drop(Vec<PathBuf>),
}

pub type Sink = Box<dyn Fn(DndEvent) + Send>;

const URI_LIST: &str = "text/uri-list";

/// Turn a `text/uri-list` payload into local paths (comments and remote URIs skipped).
pub fn parse_uri_list(text: &str) -> Vec<PathBuf> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.strip_prefix("file://"))
        // file://host/path: drop the (usually empty or "localhost") host part
        .map(|rest| match rest.find('/') {
            Some(i) => &rest[i..],
            None => rest,
        })
        .map(|p| PathBuf::from(percent_decode(p)))
        .collect()
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ------------------------------------------------------------------ Wayland

struct State {
    conn: Connection,
    /// MIME types announced for each offer
    mimes: HashMap<ObjectId, Vec<String>>,
    /// offer of the drag currently over our window, if it carries files
    current: Option<WlDataOffer>,
    sink: Sink,
}

/// Start listening for drops on an existing Wayland connection.
///
/// # Safety
/// `display` must be the live `wl_display*` of the window's connection and must
/// outlive the listener thread (true for the lifetime of the app window).
pub unsafe fn start_wayland(display: *mut std::ffi::c_void, sink: Sink) -> Result<(), String> {
    let backend = unsafe { wayland_backend::sys::client::Backend::from_foreign_display(display.cast()) };
    let conn = Connection::from_backend(backend);
    let (globals, mut queue) = registry_queue_init::<State>(&conn).map_err(|e| format!("wayland registry: {e}"))?;
    let qh = queue.handle();
    let ddm: WlDataDeviceManager = globals.bind(&qh, 1..=3, ()).map_err(|e| format!("no data device manager: {e}"))?;
    let seats: Vec<WlSeat> = globals
        .contents()
        .clone_list()
        .into_iter()
        .filter(|g| g.interface == WlSeat::interface().name)
        .map(|g| globals.registry().bind::<WlSeat, _, State>(g.name, g.version.min(5), &qh, ()))
        .collect();
    if seats.is_empty() {
        return Err("no wayland seat".into());
    }
    for seat in &seats {
        ddm.get_data_device(seat, &qh, ());
    }
    conn.flush().map_err(|e| e.to_string())?;
    let mut state = State { conn: conn.clone(), mimes: HashMap::new(), current: None, sink };
    std::thread::Builder::new()
        .name("coda-dnd".into())
        .spawn(move || {
            let _keep = (ddm, seats);
            while queue.blocking_dispatch(&mut state).is_ok() {}
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<WlSeat, ()> for State {
    fn event(_: &mut Self, _: &WlSeat, _: <WlSeat as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<WlDataDeviceManager, ()> for State {
    fn event(_: &mut Self, _: &WlDataDeviceManager, _: <WlDataDeviceManager as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WlDataOffer, ()> for State {
    fn event(state: &mut Self, offer: &WlDataOffer, event: wl_data_offer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_data_offer::Event::Offer { mime_type } = event {
            state.mimes.entry(offer.id()).or_default().push(mime_type);
        }
    }
}

impl Dispatch<WlDataDevice, ()> for State {
    fn event(state: &mut Self, _: &WlDataDevice, event: wl_data_device::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            wl_data_device::Event::Enter { serial, id, .. } => {
                let Some(offer) = id else { return };
                let has_files = state.mimes.get(&offer.id()).is_some_and(|m| m.iter().any(|x| x == URI_LIST));
                if has_files {
                    offer.accept(serial, Some(URI_LIST.into()));
                    if offer.version() >= 3 {
                        offer.set_actions(DndAction::Copy | DndAction::Move, DndAction::Copy);
                    }
                    state.current = Some(offer);
                    (state.sink)(DndEvent::Hover(true));
                } else {
                    offer.accept(serial, None);
                }
            }
            wl_data_device::Event::Leave => {
                if let Some(o) = state.current.take() {
                    state.mimes.remove(&o.id());
                    o.destroy();
                    (state.sink)(DndEvent::Hover(false));
                }
            }
            wl_data_device::Event::Drop => {
                let Some(offer) = state.current.take() else { return };
                state.mimes.remove(&offer.id());
                let paths = receive(&state.conn, &offer);
                if offer.version() >= 3 {
                    offer.finish();
                }
                offer.destroy();
                let _ = state.conn.flush();
                (state.sink)(DndEvent::Hover(false));
                if !paths.is_empty() {
                    (state.sink)(DndEvent::Drop(paths));
                }
            }
            wl_data_device::Event::Selection { id } => {
                // clipboard offers are Slint's business; release ours
                if let Some(o) = id {
                    state.mimes.remove(&o.id());
                    o.destroy();
                }
            }
            _ => {}
        }
    }

    event_created_child!(State, WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (WlDataOffer, ()),
    ]);
}

/// Ask the source for its uri-list and read it through a pipe.
fn receive(conn: &Connection, offer: &WlDataOffer) -> Vec<PathBuf> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return vec![];
    }
    let (read_end, write_end) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    offer.receive(URI_LIST.into(), write_end.as_fd());
    if conn.flush().is_err() {
        return vec![];
    }
    drop(write_end); // the source holds its own copy now; EOF arrives when it closes it
    let mut text = String::new();
    let _ = std::fs::File::from(read_end).read_to_string(&mut text);
    parse_uri_list(&text)
}

// ------------------------------------------------------------------ X11 (winit)

/// Collects winit's per-file events into one drop.
#[derive(Default)]
pub struct X11Collector {
    pending: Vec<PathBuf>,
}

impl X11Collector {
    pub fn push(&mut self, p: PathBuf) {
        self.pending.push(p);
    }
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }
    pub fn take(&mut self) -> Vec<PathBuf> {
        std::mem::take(&mut self.pending)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_uri_lists() {
        let t = "# comment\r\nfile:///home/me/Music/My%20Album/01%20-%20Song.flac\r\nfile://localhost/tmp/a%23b.mp3\r\nhttps://example.com/x.mp3\r\n";
        assert_eq!(
            parse_uri_list(t),
            vec![PathBuf::from("/home/me/Music/My Album/01 - Song.flac"), PathBuf::from("/tmp/a#b.mp3")]
        );
        assert_eq!(parse_uri_list("file:///caf%C3%A9.flac"), vec![PathBuf::from("/café.flac")]);
    }
}
