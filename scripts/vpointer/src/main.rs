//! A pointer for the test compositor that can hold its button down.
//!
//! `wlrctl pointer` has `click`, which is a press and a release together, so
//! a harness built on it can click anything and drag nothing. This speaks
//! the same protocol, `zwlr_virtual_pointer_v1`, and keeps the two apart:
//! press, a series of moves with the button held, release. To the client
//! under it that is a real drag, with real serials, which is what a Wayland
//! `start_drag` asks for.
//!
//! ```text
//! vpointer drag X1 Y1 X2 Y2 --extent WxH [--steps N] [--hold-ms N]
//! vpointer move X Y --extent WxH
//! ```
//!
//! Coordinates are in the compositor's layout, in pixels, and `--extent` is
//! the size of that layout: absolute motion is a fraction of it.

use std::time::{Duration, Instant};

use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_pointer, wl_registry};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};

/// `BTN_LEFT` from `linux/input-event-codes.h`.
const BTN_LEFT: u32 = 0x110;

struct State;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrVirtualPointerManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwlrVirtualPointerManagerV1,
        _: <ZwlrVirtualPointerManagerV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrVirtualPointerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwlrVirtualPointerV1,
        _: <ZwlrVirtualPointerV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

struct Pointer {
    conn: Connection,
    ptr: ZwlrVirtualPointerV1,
    extent: (u32, u32),
    started: Instant,
}

impl Pointer {
    fn now(&self) -> u32 {
        self.started.elapsed().as_millis() as u32
    }

    fn at(&self, x: u32, y: u32) {
        let (w, h) = self.extent;
        self.ptr
            .motion_absolute(self.now(), x.min(w), y.min(h), w, h);
        self.ptr.frame();
        let _ = self.conn.flush();
    }

    fn button(&self, state: wl_pointer::ButtonState) {
        self.ptr.button(self.now(), BTN_LEFT, state);
        self.ptr.frame();
        let _ = self.conn.flush();
    }
}

fn usage() -> ! {
    eprintln!(
        "usage: vpointer drag X1 Y1 X2 Y2 --extent WxH [--steps N] [--hold-ms N]\n       vpointer move X Y --extent WxH"
    );
    std::process::exit(2)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut positional = Vec::new();
    let mut extent = None;
    let mut steps = 24u32;
    let mut hold = Duration::from_millis(150);
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--extent" => {
                let value = it.next().unwrap_or_else(|| usage());
                let (w, h) = value.split_once('x').unwrap_or_else(|| usage());
                extent = Some((
                    w.parse().unwrap_or_else(|_| usage()),
                    h.parse().unwrap_or_else(|_| usage()),
                ));
            }
            "--steps" => steps = it.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| usage()),
            "--hold-ms" => {
                hold = Duration::from_millis(
                    it.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| usage()),
                )
            }
            _ => positional.push(arg.clone()),
        }
    }
    let Some(extent) = extent else { usage() };
    let numbers: Vec<u32> = positional[1..]
        .iter()
        .map(|v| v.parse().unwrap_or_else(|_| usage()))
        .collect();

    let conn = Connection::connect_to_env().expect("no Wayland display to talk to");
    let (globals, mut queue) = registry_queue_init::<State>(&conn).expect("registry");
    let qh = queue.handle();
    let manager: ZwlrVirtualPointerManagerV1 = globals
        .bind(&qh, 1..=2, ())
        .expect("this compositor has no zwlr_virtual_pointer_manager_v1");
    let pointer = Pointer {
        ptr: manager.create_virtual_pointer(None, &qh, ()),
        conn: conn.clone(),
        extent,
        started: Instant::now(),
    };
    queue.roundtrip(&mut State).expect("roundtrip");

    match (positional.first().map(String::as_str), numbers.as_slice()) {
        (Some("move"), &[x, y]) => pointer.at(x, y),
        (Some("drag"), &[x1, y1, x2, y2]) => {
            pointer.at(x1, y1);
            std::thread::sleep(hold);
            pointer.button(wl_pointer::ButtonState::Pressed);
            std::thread::sleep(hold);
            // In steps, with time between them: a client tells a drag from a
            // click by movement while the button is down, and some only start
            // one past a threshold, so a single jump can read as a click.
            let lerp = |a: u32, b: u32, t: f64| (a as f64 + (b as f64 - a as f64) * t).round() as u32;
            for step in 1..=steps.max(1) {
                let t = step as f64 / steps.max(1) as f64;
                pointer.at(lerp(x1, x2, t), lerp(y1, y2, t));
                std::thread::sleep(Duration::from_millis(16));
            }
            std::thread::sleep(hold);
            pointer.button(wl_pointer::ButtonState::Released);
            std::thread::sleep(hold);
        }
        _ => usage(),
    }
    queue.roundtrip(&mut State).expect("roundtrip");
    pointer.ptr.destroy();
    let _ = conn.flush();
}
