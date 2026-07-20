//! Host side of `wk:capture/frames` — screen capture as a canvas capability.
//!
//! A Screen Capture node owns a [`SharedFrameSlot`]: the newest captured frame
//! (RGBA8 + a sequence number), written by whichever capture source is active —
//! the MVP source is the local client's own rendered canvas. Wiring an app to
//! the capture node points the app's [`SharedCaptureSrc`] at that slot (the
//! `sync_captures` reconciler); unwiring clears it. The guest polls
//! `next-frame`, and a per-store sequence cursor makes each new frame arrive
//! exactly once.

use std::sync::{Arc, Mutex};

use wasmtime::component::{HasData, Linker};
use wasmtime::Result;

use crate::plugin::HostState;

wasmtime::component::bindgen!({
    path: "wit-capture",
    world: "capture-host",
    imports: { default: trappable },
    require_store_data_send: true,
});

/// The newest captured frame for one Screen Capture node. `seq` starts at 0
/// (no frame yet) and increments with every capture.
#[derive(Default)]
pub struct FrameSlot {
    pub seq: u64,
    pub width: u32,
    pub height: u32,
    /// Tightly packed RGBA8, `width * height * 4` bytes.
    pub data: Vec<u8>,
}

pub type SharedFrameSlot = Arc<Mutex<FrameSlot>>;

/// A node's view of its granted capture source: `None` until a capture wire
/// points it at a Screen Capture node's slot.
pub type SharedCaptureSrc = Arc<Mutex<Option<SharedFrameSlot>>>;

pub fn new_slot() -> SharedFrameSlot {
    Arc::new(Mutex::new(FrameSlot::default()))
}

pub fn new_src() -> SharedCaptureSrc {
    Arc::new(Mutex::new(None))
}

pub fn add_to_linker(l: &mut Linker<HostState>) -> Result<()> {
    wk::capture::frames::add_to_linker::<_, HasCapture>(l, |s| s)?;
    Ok(())
}

struct HasCapture;
impl HasData for HasCapture {
    type Data<'a> = &'a mut HostState;
}

impl wk::capture::frames::Host for HostState {
    fn next_frame(&mut self) -> Result<Option<wk::capture::frames::Frame>> {
        let Some(slot) = self.capture_src.lock().unwrap().clone() else {
            return Ok(None); // not wired to a Screen Capture node
        };
        let s = slot.lock().unwrap();
        if s.seq == 0 || s.seq == self.capture_seq {
            return Ok(None); // nothing captured yet, or nothing new
        }
        self.capture_seq = s.seq;
        Ok(Some(wk::capture::frames::Frame {
            seq: s.seq,
            width: s.width,
            height: s.height,
            data: s.data.clone(),
        }))
    }
}
