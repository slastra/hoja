//! The chip under the cursor while a drag is outside the window.
//!
//! gpui draws a drag preview inside its own window and, when the pointer leaves,
//! hands the gesture to the compositor with `None` for
//! `wl_data_device.start_drag`'s icon argument. So a drag that leaves carries
//! nothing. That argument cannot be filled from outside gpui in the obvious way,
//! because the icon must be a `wl_surface` on the same connection and
//! `start_drag` needs the serial of the button press that began the implicit
//! grab, which gpui keeps to itself.
//!
//! Both are reachable anyway, and this crate is the proof:
//!
//! - `Backend::from_foreign_display` attaches a second backend, with its own
//!   event queue, to the `wl_display` gpui already owns. The compositor still
//!   sees one client, which is what makes the rest legal.
//! - `ObjectId::from_ptr` rebuilds gpui's `wl_surface` as a proxy we can name as
//!   the drag's origin.
//! - Binding `wl_seat` a second time gives us our own `wl_pointer`, and our own
//!   serials. Measured on Hyprland: one physical press is delivered to each
//!   pointer resource with a *different* serial, and `start_drag` with ours is
//!   accepted. So do not try to guess or borrow gpui's serial; witness the press.
//!
//! Requests are made from whichever thread calls `start`; only event dispatch
//! needs a thread of its own, which is why there is no command channel here.

use std::ffi::c_void;
use std::io::Write;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use wayland_backend::sys::client::{Backend, ObjectId};
use wayland_client::protocol::{
    wl_buffer::WlBuffer, wl_compositor::WlCompositor, wl_data_device, wl_data_device::WlDataDevice,
    wl_data_device_manager::WlDataDeviceManager, wl_data_offer::WlDataOffer, wl_data_source,
    wl_data_source::WlDataSource, wl_pointer, wl_pointer::WlPointer, wl_registry,
    wl_registry::WlRegistry, wl_seat, wl_seat::WlSeat, wl_shm, wl_shm::WlShm,
    wl_shm_pool::WlShmPool, wl_surface::WlSurface,
};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum, delegate_noop};

/// `BTN_LEFT` from `linux/input-event-codes.h`.
const BTN_LEFT: u32 = 272;

/// What the compositor should draw under the cursor.
///
/// `bgra` is **premultiplied** BGRA, which is byte-for-byte `Argb8888` on a
/// little-endian machine, so the pixels are copied and never converted.
///
/// Premultiplied is not optional and not what every rasteriser gives you: gpui's
/// `SvgRenderer`, for one, divides the alpha back out. Hand straight alpha to a
/// compositor and the anti-aliased edges fringe, which reads as a broken corner
/// radius rather than as a colour bug.
pub struct Chip {
    pub bgra: Vec<u8>,
    /// Buffer dimensions, in device pixels.
    pub width: i32,
    pub height: i32,
    /// Buffer scale. `SvgRenderer` renders at `SMOOTH_SVG_SCALE_FACTOR`, so a
    /// chip that came from it is 2.
    pub scale: i32,
    /// Where the chip sits relative to the pointer, in surface-local pixels.
    pub hotspot: (i32, i32),
}

/// What the drag hands to a target that reads it.
pub enum Payload {
    /// Files that exist now. Any reader gets them, at any point in the drag.
    Files(Vec<String>),
    /// Files that do not exist yet, and are made the first time anybody but
    /// the source's own window asks for them.
    ///
    /// The **first** reader is given `first` and `make` does not run. The drag
    /// begins with the pointer over the window it was picked up in, so the
    /// compositor's first `enter` goes there, and gpui reads every offer once
    /// on entry — before anything else can. So the first read is always gpui
    /// looking at its own drag, and every read after it is somebody who means
    /// it: a target peeking during motion to decide what to do, a target
    /// reading at the drop, or gpui again after the drag has been out and
    /// come back. `make` runs once, at the first of those, and its result is
    /// what all of them get.
    ///
    /// That is what keeps an archive member cheap to pick up: a drag that is
    /// cancelled, or dropped straight back inside hoja, is read exactly once
    /// and nothing is extracted.
    ///
    /// `first` must not be empty. gpui destroys an offer whose list is empty,
    /// which the compositor takes as the target refusing — and with the
    /// source's own window the only target at that instant, the whole gesture
    /// is cancelled. A real, existing file is the safe thing to name.
    ///
    /// Not keyed on `DndDropPerformed`, though that looks like the obvious
    /// moment: file managers ask for the list during motion and act on what
    /// they were told, so a placeholder served until the drop is the thing
    /// that gets dropped.
    Lazy {
        first: Vec<String>,
        make: Box<dyn FnOnce() -> Vec<String> + Send>,
    },
}

/// How a drag ended. Every variant means the gesture is over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragEvent {
    /// Released over something that accepted the offer.
    Performed,
    /// The target finished reading. Follows `Performed`.
    Finished,
    /// Released over nothing, or refused.
    Cancelled,
}

/// A live attachment to gpui's Wayland connection.
///
/// Dropping this leaves the connection alone: the backend is a guest and does
/// not close a display it did not open.
pub struct DragSource {
    conn: Connection,
    qh: QueueHandle<Inner>,
    compositor: WlCompositor,
    shm: WlShm,
    device: WlDataDevice,
    manager: WlDataDeviceManager,
    /// gpui's surface, rebuilt on our backend. The drag's origin.
    origin: WlSurface,
    /// The serial of the most recent left-button press our pointer saw.
    serial: Arc<AtomicU32>,
    /// Counts drags. Every event names the drag it belongs to, so one that
    /// arrives late cannot be taken for news about the drag after it.
    generation: AtomicU64,
    events: Mutex<mpsc::Receiver<(u64, DragEvent)>>,
    /// Held so the icon outlives `start_drag`; replaced on the next drag.
    live: Mutex<Option<Live>>,
}

struct Live {
    source: WlDataSource,
    icon: WlSurface,
    buffer: WlBuffer,
    pool: WlShmPool,
}

/// What a data source carries with it, so its handler needs no shared state.
struct SourceData {
    generation: u64,
    payload: Mutex<PayloadState>,
}

enum PayloadState {
    Ready(Vec<u8>),
    /// Nobody has read yet. The next reader is the source's own window.
    Unread {
        first: Vec<u8>,
        make: Box<dyn FnOnce() -> Vec<String> + Send>,
    },
    /// The own-window read has happened. The next reader gets the real thing.
    Pending(Box<dyn FnOnce() -> Vec<String> + Send>),
}

impl SourceData {
    fn new(generation: u64, payload: Payload) -> Self {
        let payload = match payload {
            Payload::Files(uris) => PayloadState::Ready(uri_list(&uris)),
            Payload::Lazy { first, make } => PayloadState::Unread {
                first: uri_list(&first),
                make,
            },
        };
        Self {
            generation,
            payload: Mutex::new(payload),
        }
    }

    /// The bytes to hand a reader right now.
    fn bytes(&self) -> Vec<u8> {
        let mut state = self.payload.lock().unwrap();
        // Take the state out so the closure can be moved rather than borrowed.
        let taken = std::mem::replace(&mut *state, PayloadState::Ready(Vec::new()));
        let (next, bytes) = match taken {
            PayloadState::Ready(bytes) => (PayloadState::Ready(bytes.clone()), bytes),
            PayloadState::Unread { first, make } => (PayloadState::Pending(make), first),
            PayloadState::Pending(make) => {
                let bytes = uri_list(&make());
                (PayloadState::Ready(bytes.clone()), bytes)
            }
        };
        *state = next;
        bytes
    }
}

/// `text/uri-list`: one URI per line, CRLF terminated.
fn uri_list(uris: &[String]) -> Vec<u8> {
    let mut list = String::new();
    for uri in uris {
        list.push_str(uri);
        list.push_str("\r\n");
    }
    list.into_bytes()
}

impl DragSource {
    /// Attach to a display and surface that belong to somebody else.
    ///
    /// `None` when this is not a Wayland session, or the compositor does not
    /// offer what a drag needs. The caller must fall back to letting gpui
    /// promote the drag itself in that case, or dragging out stops working.
    ///
    /// # Safety
    ///
    /// `display` must be a live `*mut wl_display` and `surface` a live
    /// `*mut wl_proxy` for a `wl_surface` on that display, both outliving the
    /// returned `DragSource`. In practice they come from gpui's
    /// `HasDisplayHandle`/`HasWindowHandle`, and the window outlives this.
    pub unsafe fn attach(display: *mut c_void, surface: *mut c_void) -> Option<Self> {
        if display.is_null() || surface.is_null() {
            return None;
        }
        // Guest mode: this backend will not close the connection when dropped.
        let backend = unsafe { Backend::from_foreign_display(display.cast()) };
        let conn = Connection::from_backend(backend);
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();

        let (tx, rx) = mpsc::channel();
        let serial = Arc::new(AtomicU32::new(0));
        let mut inner = Inner {
            serial: serial.clone(),
            events: Some(tx),
            ..Default::default()
        };

        let registry = conn.display().get_registry(&qh, ());
        queue.roundtrip(&mut inner).ok()?;

        // Our own seat, so we witness presses and get serials of our own.
        let (name, version) = inner.seat_global?;
        let seat: WlSeat = registry.bind(name, version, &qh, ());
        queue.roundtrip(&mut inner).ok()?;

        let compositor = inner.compositor.clone()?;
        let shm = inner.shm.clone()?;
        let manager = inner.manager.clone()?;
        let device = manager.get_data_device(&seat, &qh, ());

        // gpui's surface, named on our backend.
        let origin = unsafe {
            let id = ObjectId::from_ptr(WlSurface::interface(), surface.cast()).ok()?;
            WlSurface::from_id(&conn, id).ok()?
        };

        queue.roundtrip(&mut inner).ok()?;

        std::thread::Builder::new()
            .name("hoja-drag".into())
            .spawn(move || while queue.blocking_dispatch(&mut inner).is_ok() {})
            .ok()?;

        Some(Self {
            conn,
            qh,
            compositor,
            shm,
            device,
            manager,
            origin,
            serial,
            generation: AtomicU64::new(0),
            events: Mutex::new(rx),
            live: Mutex::new(None),
        })
    }

    /// Begin a drag carrying `payload`, with `chip` under the cursor.
    ///
    /// Returns false when no press has been seen yet, which means there is no
    /// grab to start a drag from.
    pub fn start(&self, chip: Chip, payload: Payload) -> bool {
        let serial = self.serial.load(Ordering::Relaxed);
        if serial == 0 {
            return false;
        }

        // Whatever the last drag left behind is over: a new press has begun a
        // new gesture. Its source may still owe a `Finished`, which will now
        // arrive under the old generation and be ignored.
        self.retire();
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;

        let Some((pool, buffer)) = self.buffer(&chip) else {
            return false;
        };
        let icon = self.compositor.create_surface(&self.qh, ());
        let source = self
            .manager
            .create_data_source(&self.qh, SourceData::new(generation, payload));
        for mime in MIMES {
            source.offer((*mime).into());
        }
        source.set_actions(
            wayland_client::protocol::wl_data_device_manager::DndAction::Copy
                | wayland_client::protocol::wl_data_device_manager::DndAction::Move,
        );

        self.device
            .start_drag(Some(&source), &self.origin, Some(&icon), serial);

        // Only now does the surface have the dnd_icon role, and only a commit
        // made after that maps it. Committing first compiles, runs, and shows
        // nothing at all.
        // A buffer whose size is not a multiple of its scale is a protocol
        // error, and a protocol error ends the connection: gpui's as well as
        // ours, since they share it. A chip drawn at the wrong scale is a
        // blemish, so that is the price paid instead.
        if chip.scale > 1 && chip.width % chip.scale == 0 && chip.height % chip.scale == 0 {
            icon.set_buffer_scale(chip.scale);
        }
        icon.attach(Some(&buffer), chip.hotspot.0, chip.hotspot.1);
        icon.damage_buffer(0, 0, chip.width, chip.height);
        icon.commit();

        // The requests above were made on the caller's thread; nothing will send
        // them until somebody flushes.
        let _ = self.conn.flush();

        *self.live.lock().unwrap() = Some(Live {
            source,
            icon,
            buffer,
            pool,
        });
        true
    }

    /// The next drag-end event, if one has arrived. Never blocks.
    ///
    /// Any event means the gesture is over, so the caller should clear whatever
    /// drag state it holds of its own.
    pub fn poll(&self) -> Option<DragEvent> {
        let current = self.generation.load(Ordering::Relaxed);
        let events = self.events.lock().unwrap();
        loop {
            let (generation, event) = events.try_recv().ok()?;
            if generation != current {
                // A drop yields `Performed` and then `Finished`, and a caller
                // that stops listening at the first leaves the second behind.
                // Read at the start of the next drag it would tear that one
                // down, which is what made every other drag fail.
                continue;
            }
            if matches!(event, DragEvent::Finished | DragEvent::Cancelled) {
                drop(events);
                self.retire();
            }
            return Some(event);
        }
    }

    /// Tear down the icon and source once the compositor is done with them.
    fn retire(&self) {
        if let Some(live) = self.live.lock().unwrap().take() {
            live.source.destroy();
            live.buffer.destroy();
            live.pool.destroy();
            live.icon.destroy();
            let _ = self.conn.flush();
        }
    }

    /// An shm buffer holding the chip.
    ///
    /// The file is unlinked the instant it exists: the fd keeps the pages alive,
    /// so a crash mid-drag leaves nothing behind in `/dev/shm`.
    fn buffer(&self, chip: &Chip) -> Option<(WlShmPool, WlBuffer)> {
        let want = (chip.width as usize)
            .checked_mul(chip.height as usize)?
            .checked_mul(4)?;
        if chip.width <= 0 || chip.height <= 0 || chip.bgra.len() < want {
            return None;
        }
        let path = format!("/dev/shm/hoja-drag-{}", std::process::id());
        let mut file = std::fs::File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .ok()?;
        let _ = std::fs::remove_file(&path);
        file.write_all(&chip.bgra[..want]).ok()?;
        let pool = self
            .shm
            .create_pool(std::os::fd::AsFd::as_fd(&file), want as i32, &self.qh, ());
        let buffer = pool.create_buffer(
            0,
            chip.width,
            chip.height,
            chip.width * 4,
            wl_shm::Format::Argb8888,
            &self.qh,
            (),
        );
        Some((pool, buffer))
    }
}

/// What we advertise. `text/uri-list` is what file managers read; the two
/// gnome-flavoured names are what several toolkits ask for instead.
const MIMES: &[&str] = &[
    "text/uri-list",
    "x-special/gnome-copied-files",
    "text/plain;charset=utf-8",
];

/// Everything the dispatch thread owns. Requests are made elsewhere.
#[derive(Default)]
struct Inner {
    compositor: Option<WlCompositor>,
    shm: Option<WlShm>,
    manager: Option<WlDataDeviceManager>,
    seat_global: Option<(u32, u32)>,
    pointer: Option<WlPointer>,
    serial: Arc<AtomicU32>,
    events: Option<mpsc::Sender<(u64, DragEvent)>>,
}

impl Inner {
    fn emit(&self, generation: u64, event: DragEvent) {
        if let Some(tx) = &self.events {
            let _ = tx.send((generation, event));
        }
    }
}

impl Dispatch<WlRegistry, ()> for Inner {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
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
        match &interface[..] {
            // Version 4 on purpose: from 5, a non-zero offset in `attach` is a
            // protocol error and the hotspot has to move to `wl_surface.offset`.
            "wl_compositor" => state.compositor = Some(registry.bind(name, version.min(4), qh, ())),
            "wl_shm" => state.shm = Some(registry.bind(name, 1, qh, ())),
            "wl_data_device_manager" => {
                state.manager = Some(registry.bind(name, version.min(3), qh, ()))
            }
            "wl_seat" => state.seat_global = Some((name, version.min(7))),
            _ => {}
        }
    }
}

impl Dispatch<WlSeat, ()> for Inner {
    fn event(
        state: &mut Self,
        seat: &WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
            && caps.contains(wl_seat::Capability::Pointer)
            && state.pointer.is_none()
        {
            state.pointer = Some(seat.get_pointer(qh, ()));
        }
    }
}

impl Dispatch<WlPointer, ()> for Inner {
    fn event(
        state: &mut Self,
        _: &WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // The only reason this pointer exists. gpui's own serial is private, and
        // a serial from a resource we do not own would not validate.
        if let wl_pointer::Event::Button {
            serial,
            button: BTN_LEFT,
            state: WEnum::Value(wl_pointer::ButtonState::Pressed),
            ..
        } = event
        {
            state.serial.store(serial, Ordering::Relaxed);
        }
    }
}

impl Dispatch<WlDataSource, SourceData> for Inner {
    fn event(
        state: &mut Self,
        _: &WlDataSource,
        event: wl_data_source::Event,
        data: &SourceData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            // The target is reading. Hand it the list and close, or it waits
            // forever on a pipe nobody is writing to. The list travels with the
            // source, so a target still reading an old drag gets that drag's
            // files and not the next one's.
            //
            // An `OnDrop` payload does its work here, on this thread, with the
            // target waiting on the other end of the pipe. That is the deal:
            // the files exist by the time anybody is told where they are.
            wl_data_source::Event::Send { fd, .. } => {
                let bytes = data.bytes();
                let mut file = std::fs::File::from(fd);
                let _ = file.write_all(&bytes);
            }
            wl_data_source::Event::DndDropPerformed => {
                state.emit(data.generation, DragEvent::Performed)
            }
            wl_data_source::Event::DndFinished => state.emit(data.generation, DragEvent::Finished),
            wl_data_source::Event::Cancelled => state.emit(data.generation, DragEvent::Cancelled),
            _ => {}
        }
    }
}

impl Dispatch<WlDataDevice, ()> for Inner {
    fn event(
        _: &mut Self,
        _: &WlDataDevice,
        _: wl_data_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }

    // The compositor hands over a data offer as soon as the device exists, and a
    // new-id event with no child specified panics the queue.
    wayland_client::event_created_child!(Inner, WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (WlDataOffer, ()),
    ]);
}

delegate_noop!(Inner: ignore WlCompositor);
delegate_noop!(Inner: ignore WlSurface);
delegate_noop!(Inner: ignore WlShm);
delegate_noop!(Inner: ignore WlShmPool);
delegate_noop!(Inner: ignore WlBuffer);
delegate_noop!(Inner: ignore WlDataDeviceManager);
delegate_noop!(Inner: ignore WlDataOffer);

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn a_uri_list_is_crlf_terminated_and_empty_when_empty() {
        assert_eq!(uri_list(&[]), b"");
        assert_eq!(
            uri_list(&["file:///a".into(), "file:///b".into()]),
            b"file:///a\r\nfile:///b\r\n"
        );
    }

    #[test]
    fn files_are_handed_to_every_reader() {
        let data = SourceData::new(1, Payload::Files(vec!["file:///x".into()]));
        assert_eq!(data.bytes(), b"file:///x\r\n");
        assert_eq!(data.bytes(), b"file:///x\r\n");
    }

    #[test]
    fn a_lazy_payload_placates_the_first_reader_and_is_made_once_for_the_rest() {
        let runs = Arc::new(AtomicUsize::new(0));
        let counted = runs.clone();
        let data = SourceData::new(
            1,
            Payload::Lazy {
                first: vec!["file:///the.zip".into()],
                make: Box::new(move || {
                    counted.fetch_add(1, Ordering::Relaxed);
                    vec!["file:///made".into()]
                }),
            },
        );

        // The first read is the source's own window, on entry. It must cost
        // nothing and must not come back empty, or gpui refuses the offer.
        assert_eq!(data.bytes(), b"file:///the.zip\r\n");
        assert_eq!(runs.load(Ordering::Relaxed), 0);

        // The second is somebody who means it — a target peeking during
        // motion, say — and gets the real thing.
        assert_eq!(data.bytes(), b"file:///made\r\n");
        // And so does everybody after, from one extraction.
        assert_eq!(data.bytes(), b"file:///made\r\n");
        assert_eq!(runs.load(Ordering::Relaxed), 1);
    }
}
