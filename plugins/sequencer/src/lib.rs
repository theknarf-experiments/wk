// A step sequencer / piano roll, as a wk:clap plugin. No `run` loop: the host
// drives it by calling `process` from the audio path (the transport clock counts
// samples there, plays notes out, and records incoming ones onto the playhead)
// and `gui-render` per frame (it paints the roll and handles editing). Note I/O
// rides wk:clap's event model; the pattern persists via wk:options — a wk:clap
// node freely using another wk interface.
#[allow(warnings)]
mod bindings;

use std::cell::RefCell;
use std::collections::HashMap;

use bindings::exports::wk::clap::plugins::{
    AudioBuffer, AudioPortInfo, Descriptor, Event, Guest, GuestPlugin, NotePortInfo, ParamInfo,
    Plugin, ProcessResult, ProcessStatus, Supported, Transport as WitTransport,
};
use bindings::wasi::frame_buffer::frame_buffer::{Buffer, Device};
use bindings::wasi::graphics_context::graphics_context::Context as GfxContext;
use bindings::wasi::surface::surface::{CreateDesc, Key, Surface};
use bindings::wk::clap::types::{Midi, NoteDialects};

/// Steps per loop (one bar of sixteenth notes).
const STEPS: i32 = 16;
/// Pitch rows, low to high, starting at `LOW` (C3 = MIDI 48) — two octaves + C.
const ROWS: i32 = 25;
const LOW: i32 = 48;
/// Fixed tempo. At 120 BPM, sixteenth notes fire eight times a second.
const BPM: f32 = 120.0;

/// Height of the top strip holding the Play / Record buttons.
const CTRL_H: f32 = 30.0;
/// Button box size and left inset.
const BTN_W: f32 = 40.0;
const BTN_H: f32 = 22.0;
const BTN_Y: f32 = 4.0;
const BTN_GAP: f32 = 10.0;

/// Left/right/top padding around the grid.
const PAD: f32 = 6.0;
/// Pixels at a note's right edge that grab the resize handle.
const RESIZE_PX: f32 = 7.0;

/// Is MIDI `note` a black key (used to tint the piano-roll rows)?
fn is_black(note: i32) -> bool {
    matches!(note.rem_euclid(12), 1 | 3 | 6 | 8 | 10)
}

/// A raw 3-byte MIDI message as a wk:clap event (port 0, no sample offset).
fn midi_ev(status: u8, note: u8, vel: u8) -> Event {
    Event::Midi(Midi {
        time: 0,
        port_index: 0,
        data: (status, note, vel),
    })
}

#[derive(PartialEq, Clone, Copy)]
enum Transport {
    Stopped,
    Playing,
    Recording,
}

/// A note in the roll: a `step` start column, a `pitch`, and a `len` in steps
/// (>= 1). Notes never wrap past the bar (`step + len <= STEPS`).
#[derive(Clone, Copy)]
struct Note {
    step: i32,
    pitch: i32,
    len: i32,
}

impl Note {
    /// Is this note sounding at step `p`?
    fn covers(&self, p: i32) -> bool {
        self.step <= p && p < self.step + self.len
    }
}

/// A mouse edit in progress: dragging a note's body to move it, or its right
/// edge to resize it. Offsets are captured at grab time (`step`/`pitch` of the
/// note minus the cursor's), so the note tracks the cursor without snapping.
enum Drag {
    None,
    Move { idx: usize, doff: i32, poff: i32 },
    Resize { idx: usize },
}

/// The sequencer's mutable state, behind the resource's single `RefCell`.
/// `gui-render` and `process` never run concurrently (the host serializes a
/// node's calls), so a `RefCell` is enough.
struct Seq {
    // Lazily created on the first `gui-render` (the host composites the surface).
    surface: Option<Surface>,
    ctx: Option<GfxContext>,
    device: Option<Device>,
    px: Vec<u8>,
    drag: Drag,
    loaded: bool,

    /// Audio sample rate, from `activate` — the transport's time base.
    sample_rate: f32,
    notes: Vec<Note>,
    transport: Transport,
    /// Current step (0..STEPS); the playhead column.
    playhead: i32,
    /// Samples elapsed in the current step.
    acc: f32,
    /// True on the first tick after starting, so we fire the current column
    /// without first advancing off it.
    restart: bool,
    /// Pitches currently sounding from playback (so they can be turned off).
    sounding: Vec<i32>,
    /// Notes being recorded right now: incoming pitch -> index in `notes`.
    pending: HashMap<i32, usize>,
    /// The selected note (index into `notes`), for drag/resize/delete.
    selected: Option<usize>,
    /// The pattern changed and should be re-persisted.
    dirty: bool,
    /// Notes to emit on the next `process` block (playback + thru + silence).
    out: Vec<Event>,
}

impl Seq {
    fn new() -> Self {
        Seq {
            surface: None,
            ctx: None,
            device: None,
            px: Vec::new(),
            drag: Drag::None,
            loaded: false,
            sample_rate: 48_000.0,
            notes: Vec::new(),
            transport: Transport::Stopped,
            playhead: 0,
            acc: 0.0,
            restart: true,
            sounding: Vec::new(),
            pending: HashMap::new(),
            selected: None,
            dirty: false,
            out: Vec::new(),
        }
    }

    /// Sixteenth-note steps per second at the fixed tempo.
    fn steps_per_sec() -> f32 {
        BPM / 60.0 * 4.0
    }

    /// Samples between step boundaries at the current sample rate.
    fn samples_per_step(&self) -> f32 {
        (self.sample_rate / Self::steps_per_sec()).max(1.0)
    }

    /// Add a note (clamping its length inside the bar) and return its index.
    fn add_note(&mut self, step: i32, pitch: i32, len: i32) -> usize {
        let len = len.clamp(1, STEPS - step);
        self.notes.push(Note { step, pitch, len });
        self.dirty = true;
        self.notes.len() - 1
    }

    /// The topmost note under `(step, pitch)`, if any.
    fn note_at(&self, step: i32, pitch: i32) -> Option<usize> {
        (0..self.notes.len())
            .rev()
            .find(|&i| self.notes[i].pitch == pitch && self.notes[i].covers(step))
    }

    /// Remove the selected note; cancels any in-progress recording.
    fn delete_selected(&mut self) {
        if let Some(i) = self.selected.take() {
            if i < self.notes.len() {
                self.notes.remove(i);
                self.dirty = true;
            }
            self.pending.clear();
        }
    }

    fn silence(&mut self) {
        for pitch in self.sounding.drain(..) {
            self.out.push(midi_ev(0x80, pitch as u8, 0));
        }
    }

    fn start(&mut self, mode: Transport) {
        // Toggle: pressing the active transport button stops it.
        if self.transport == mode {
            self.stop();
            return;
        }
        self.transport = mode;
        self.playhead = 0;
        self.acc = 0.0;
        self.restart = true;
    }

    fn stop(&mut self) {
        self.transport = Transport::Stopped;
        self.pending.clear();
        self.silence();
    }

    /// Advance the transport by one audio block of `frames` samples, stepping the
    /// playhead on each sample boundary crossed and firing that column.
    fn advance(&mut self, frames: u32) {
        if self.transport == Transport::Stopped {
            return;
        }
        if self.restart {
            self.restart = false;
            self.acc = 0.0;
            self.on_step();
            return;
        }
        let step_samples = self.samples_per_step();
        self.acc += frames as f32;
        while self.acc >= step_samples {
            self.acc -= step_samples;
            self.playhead = (self.playhead + 1) % STEPS;
            self.on_step();
        }
    }

    /// At a step boundary: grow any notes still being recorded, then reconcile
    /// the sounding set with the notes that should play at the new playhead.
    fn on_step(&mut self) {
        let p = self.playhead;
        // At the loop point, finalise anything still being recorded.
        if p == 0 {
            self.pending.clear();
        }
        // Grow held recorded notes so they reach the current step.
        if self.transport == Transport::Recording {
            for &idx in self.pending.values() {
                let n = &mut self.notes[idx];
                if p >= n.step + n.len {
                    n.len = p - n.step + 1;
                    self.dirty = true;
                }
            }
        }

        // Pitches that should sound now. While recording, skip pitches being
        // played live (the MIDI thru already sounds them), so they don't
        // double-trigger.
        let recording = self.transport == Transport::Recording;
        let live: Vec<i32> = self.pending.keys().copied().collect();
        let mut want: Vec<i32> = Vec::new();
        for n in &self.notes {
            if n.covers(p) && !(recording && live.contains(&n.pitch)) && !want.contains(&n.pitch) {
                want.push(n.pitch);
            }
        }

        let offs: Vec<i32> = self
            .sounding
            .iter()
            .copied()
            .filter(|p| !want.contains(p))
            .collect();
        for pitch in offs {
            self.out.push(midi_ev(0x80, pitch as u8, 0));
        }
        self.sounding.retain(|p| want.contains(p));
        for pitch in want {
            if !self.sounding.contains(&pitch) {
                self.out.push(midi_ev(0x90, pitch as u8, 100));
                self.sounding.push(pitch);
            }
        }
    }

    /// Record capture for one incoming message: while recording, open a note on
    /// note-on and close it on note-off. (Thru is handled by the caller.)
    fn record(&mut self, msg: (u8, u8, u8)) {
        if self.transport != Transport::Recording {
            return;
        }
        let (status, d1, d2) = msg;
        let pitch = d1 as i32;
        let on = status & 0xF0 == 0x90 && d2 > 0;
        let off = status & 0xF0 == 0x80 || (status & 0xF0 == 0x90 && d2 == 0);
        if on && (LOW..LOW + ROWS).contains(&pitch) && !self.pending.contains_key(&pitch) {
            let idx = self.add_note(self.playhead, pitch, 1);
            self.pending.insert(pitch, idx);
        } else if off {
            self.pending.remove(&pitch);
        }
    }

    /// Restore a saved pattern: a flat list of (step, pitch, len) triples.
    fn load(&mut self, vals: &[f32]) {
        for t in vals.chunks_exact(3) {
            let (step, pitch, len) = (t[0] as i32, t[1] as i32, t[2] as i32);
            if (0..STEPS).contains(&step)
                && (LOW..LOW + ROWS).contains(&pitch)
                && len >= 1
                && step + len <= STEPS
            {
                self.notes.push(Note { step, pitch, len });
            }
        }
    }

    /// The pattern as a flat list of (step, pitch, len) triples, to persist.
    fn options(&self) -> Vec<f32> {
        let mut v = Vec::with_capacity(self.notes.len() * 3);
        for n in &self.notes {
            v.push(n.step as f32);
            v.push(n.pitch as f32);
            v.push(n.len as f32);
        }
        v
    }
}

// ---- pixel drawing ----

fn put(buf: &mut [u8], w: u32, h: u32, x: i32, y: i32, c: [u8; 3]) {
    if x < 0 || y < 0 || x >= w as i32 || y >= h as i32 {
        return;
    }
    let i = ((y as u32 * w + x as u32) * 4) as usize;
    buf[i] = c[0];
    buf[i + 1] = c[1];
    buf[i + 2] = c[2];
    buf[i + 3] = 255;
}

fn fill_rect(buf: &mut [u8], w: u32, h: u32, x0: i32, y0: i32, x1: i32, y1: i32, c: [u8; 3]) {
    for y in y0..y1 {
        for x in x0..x1 {
            put(buf, w, h, x, y, c);
        }
    }
}

/// A 1px outline of the box `x0,y0..x1,y1`.
fn stroke_rect(buf: &mut [u8], w: u32, h: u32, x0: i32, y0: i32, x1: i32, y1: i32, c: [u8; 3]) {
    for x in x0..x1 {
        put(buf, w, h, x, y0, c);
        put(buf, w, h, x, y1 - 1, c);
    }
    for y in y0..y1 {
        put(buf, w, h, x0, y, c);
        put(buf, w, h, x1 - 1, y, c);
    }
}

fn disc(buf: &mut [u8], w: u32, h: u32, cx: i32, cy: i32, r: i32, c: [u8; 3]) {
    for dy in -r..=r {
        for dx in -r..=r {
            if dx * dx + dy * dy <= r * r {
                put(buf, w, h, cx + dx, cy + dy, c);
            }
        }
    }
}

/// A right-pointing "play" triangle filling the box `x0,y0..x1,y1`.
fn play_icon(buf: &mut [u8], w: u32, h: u32, x0: i32, y0: i32, x1: i32, y1: i32, c: [u8; 3]) {
    let bw = (x1 - x0) as f32;
    let bh = (y1 - y0) as f32;
    for y in y0..y1 {
        // Distance from the vertical centre folds the triangle to a point.
        let t = ((y - y0) as f32 / bh - 0.5).abs() * 2.0;
        let right = x0 as f32 + bw * (1.0 - t);
        for x in x0..(right as i32) {
            put(buf, w, h, x, y, c);
        }
    }
}

/// A wk:clap plugin instance.
struct Sequencer {
    st: RefCell<Seq>,
}

impl Sequencer {
    fn new() -> Self {
        Sequencer {
            st: RefCell::new(Seq::new()),
        }
    }
}

impl GuestPlugin for Sequencer {
    fn init(&self) -> bool {
        // Restore the saved pattern once, at startup.
        let mut st = self.st.borrow_mut();
        if !st.loaded {
            let saved = bindings::wk::options::options::load();
            st.load(&saved);
            st.loaded = true;
        }
        true
    }
    fn activate(&self, sample_rate: f64, _min_frames: u32, _max_frames: u32) -> bool {
        self.st.borrow_mut().sample_rate = sample_rate as f32;
        true
    }
    fn deactivate(&self) {}
    fn start_processing(&self) -> bool {
        true
    }
    fn stop_processing(&self) {}
    fn reset(&self) {}
    fn on_main_thread(&self) {}

    /// Record incoming MIDI (thru + capture), advance the transport, and emit the
    /// notes queued since the last block.
    fn process(
        &self,
        _steady_time: i64,
        frames: u32,
        _transport: Option<WitTransport>,
        in_events: Vec<Event>,
        _audio_in: Vec<AudioBuffer>,
    ) -> ProcessResult {
        let mut st = self.st.borrow_mut();
        // Incoming MIDI: capture onto the playhead (if recording) + pass through.
        for ev in in_events {
            if let Event::Midi(m) = &ev {
                st.record(m.data);
            }
            st.out.push(ev);
        }
        // Advance the transport clock, firing playback notes.
        st.advance(frames);
        let out = std::mem::take(&mut st.out);
        ProcessResult {
            status: ProcessStatus::Continue,
            audio_out: Vec::new(),
            out_events: out,
        }
    }

    fn features(&self) -> Supported {
        Supported::NOTE_PORTS
    }

    // ---- params (none) ----
    fn param_count(&self) -> u32 {
        0
    }
    fn param_info_at(&self, _index: u32) -> Option<ParamInfo> {
        None
    }
    fn param_get(&self, _id: u32) -> Option<f64> {
        None
    }
    fn param_value_to_text(&self, _id: u32, _value: f64) -> Option<String> {
        None
    }
    fn param_text_to_value(&self, _id: u32, _text: String) -> Option<f64> {
        None
    }
    fn params_flush(&self, _in_events: Vec<Event>) -> Vec<Event> {
        Vec::new()
    }

    // ---- audio ports (none) ----
    fn audio_port_count(&self, _is_input: bool) -> u32 {
        0
    }
    fn audio_port_info_at(&self, _index: u32, _is_input: bool) -> Option<AudioPortInfo> {
        None
    }

    // ---- note ports (in + out) ----
    fn note_port_count(&self, _is_input: bool) -> u32 {
        1
    }
    fn note_port_info_at(&self, index: u32, is_input: bool) -> Option<NotePortInfo> {
        if index != 0 {
            return None;
        }
        Some(NotePortInfo {
            id: 0,
            name: if is_input { "In".into() } else { "Out".into() },
            supported_dialects: NoteDialects::MIDI,
            preferred_dialect: NoteDialects::MIDI,
        })
    }

    // ---- state (the pattern persists via wk:options instead) ----
    fn state_save(&self) -> Option<Vec<u8>> {
        Some(Vec::new())
    }
    fn state_load(&self, _data: Vec<u8>) -> bool {
        true
    }

    // ---- wk GUI ----
    fn has_gui(&self) -> bool {
        true
    }

    /// Paint the roll and handle editing / transport buttons.
    fn gui_render(&self) {
        let mut st = self.st.borrow_mut();
        if st.surface.is_none() {
            let surface = Surface::new(CreateDesc {
                width: Some(560),
                height: Some(320),
            });
            let ctx = GfxContext::new();
            surface.connect_graphics_context(&ctx);
            let device = Device::new();
            device.connect_graphics_context(&ctx);
            st.surface = Some(surface);
            st.ctx = Some(ctx);
            st.device = Some(device);
        }
        let (w, h) = {
            let surface = st.surface.as_ref().unwrap();
            let _ = surface.get_frame();
            (surface.width().max(1), surface.height().max(1))
        };
        let wf = w as f32;
        let hf = h as f32;

        // Grid geometry (below the control strip).
        let gx0 = PAD;
        let gy0 = CTRL_H + PAD;
        let gw = (wf - 2.0 * PAD).max(1.0);
        let gh = (hf - gy0 - PAD).max(1.0);
        let cell_w = gw / STEPS as f32;
        let cell_h = gh / ROWS as f32;

        // Pixel -> (step, pitch). Step floors to a column; pitch counts rows from
        // the top (highest pitch). Values may fall outside the grid; callers clamp.
        let to_step = |px: f32| ((px - gx0) / cell_w).floor() as i32;
        let to_pitch = |py: f32| {
            let sr = ((py - gy0) / cell_h).floor() as i32;
            LOW + (ROWS - 1 - sr)
        };

        // Button hit-boxes on the control strip.
        let play_x0 = PAD;
        let rec_x0 = PAD + BTN_W + BTN_GAP;
        let in_box =
            |px: f32, py: f32, x0: f32| px >= x0 && px < x0 + BTN_W && py >= BTN_Y && py < BTN_Y + BTN_H;

        while let Some(ev) = st.surface.as_ref().unwrap().get_pointer_down() {
            let (px, py) = (ev.x as f32, ev.y as f32);
            if py < CTRL_H {
                if in_box(px, py, play_x0) {
                    st.start(Transport::Playing);
                } else if in_box(px, py, rec_x0) {
                    st.start(Transport::Recording);
                }
                continue;
            }
            let step = to_step(px);
            let pitch = to_pitch(py);
            if !(0..STEPS).contains(&step) || !(LOW..LOW + ROWS).contains(&pitch) {
                continue;
            }
            match st.note_at(step, pitch) {
                Some(idx) => {
                    st.selected = Some(idx);
                    let n = st.notes[idx];
                    let right_px = gx0 + (n.step + n.len) as f32 * cell_w;
                    st.drag = if px >= right_px - RESIZE_PX {
                        Drag::Resize { idx }
                    } else {
                        Drag::Move {
                            idx,
                            doff: n.step - step,
                            poff: n.pitch - pitch,
                        }
                    };
                }
                None => {
                    // Empty space: draw a new one-step note and grab its edge so a
                    // horizontal drag sets its length.
                    let idx = st.add_note(step, pitch, 1);
                    st.selected = Some(idx);
                    st.drag = Drag::Resize { idx };
                }
            }
        }

        while let Some(ev) = st.surface.as_ref().unwrap().get_pointer_move() {
            let (px, py) = (ev.x as f32, ev.y as f32);
            match st.drag {
                Drag::Move { idx, doff, poff } if idx < st.notes.len() => {
                    let len = st.notes[idx].len;
                    let step = (to_step(px) + doff).clamp(0, STEPS - len);
                    let pitch = (to_pitch(py) + poff).clamp(LOW, LOW + ROWS - 1);
                    let n = &mut st.notes[idx];
                    if n.step != step || n.pitch != pitch {
                        n.step = step;
                        n.pitch = pitch;
                        st.dirty = true;
                    }
                }
                Drag::Resize { idx } if idx < st.notes.len() => {
                    let start = st.notes[idx].step;
                    let len = (to_step(px) - start + 1).clamp(1, STEPS - start);
                    let n = &mut st.notes[idx];
                    if n.len != len {
                        n.len = len;
                        st.dirty = true;
                    }
                }
                _ => {}
            }
        }

        while st.surface.as_ref().unwrap().get_pointer_up().is_some() {
            st.drag = Drag::None;
        }

        // Keyboard: Space toggles play, R toggles record, Backspace/Delete removes
        // the selected note.
        while let Some(ev) = st.surface.as_ref().unwrap().get_key_down() {
            match ev.key {
                Some(Key::Space) => st.start(Transport::Playing),
                Some(Key::KeyR) => st.start(Transport::Recording),
                Some(Key::Backspace) | Some(Key::Delete) => {
                    st.delete_selected();
                    st.drag = Drag::None;
                }
                _ => {}
            }
        }
        while st.surface.as_ref().unwrap().get_key_up().is_some() {}

        if st.dirty {
            bindings::wk::options::options::store(&st.options());
            st.dirty = false;
        }

        // ---- paint ----
        let n = (w * h * 4) as usize;
        st.px.clear();
        st.px.resize(n, 0);
        for p in st.px.chunks_exact_mut(4) {
            p.copy_from_slice(&[20, 20, 26, 255]);
        }

        let running = st.transport != Transport::Stopped;
        let playhead = st.playhead;

        // Row backgrounds (tinted by black/white key).
        for sr in 0..ROWS {
            let pitch = LOW + (ROWS - 1 - sr);
            let y0 = (gy0 + sr as f32 * cell_h) as i32;
            let y1 = (gy0 + (sr + 1) as f32 * cell_h) as i32;
            let c = if is_black(pitch) {
                [30, 30, 40]
            } else {
                [44, 44, 56]
            };
            fill_rect(&mut st.px, w, h, gx0 as i32, y0, (gx0 + gw) as i32, y1, c);
        }

        // Playhead column highlight.
        if running {
            let x0 = (gx0 + playhead as f32 * cell_w) as i32;
            let x1 = (gx0 + (playhead + 1) as f32 * cell_w) as i32;
            fill_rect(&mut st.px, w, h, x0, gy0 as i32, x1, (gy0 + gh) as i32, [56, 58, 70]);
        }

        // Beat divider lines every four steps.
        for step in (0..=STEPS).step_by(4) {
            let x = (gx0 + step as f32 * cell_w) as i32;
            fill_rect(&mut st.px, w, h, x, gy0 as i32, x + 1, (gy0 + gh) as i32, [70, 70, 84]);
        }

        // Notes.
        let notes = st.notes.clone();
        let selected = st.selected;
        for (i, n) in notes.iter().enumerate() {
            let sr = ROWS - 1 - (n.pitch - LOW);
            if !(0..ROWS).contains(&sr) {
                continue;
            }
            let x0 = (gx0 + n.step as f32 * cell_w) as i32 + 1;
            let x1 = (gx0 + (n.step + n.len) as f32 * cell_w) as i32 - 1;
            let y0 = (gy0 + sr as f32 * cell_h) as i32 + 1;
            let y1 = (gy0 + (sr + 1) as f32 * cell_h) as i32 - 1;
            let color = if running && n.covers(playhead) {
                [160, 255, 200]
            } else {
                [90, 200, 250]
            };
            fill_rect(&mut st.px, w, h, x0, y0, x1, y1, color);
            if selected == Some(i) {
                stroke_rect(&mut st.px, w, h, x0 - 1, y0 - 1, x1 + 1, y1 + 1, [240, 245, 255]);
            }
        }

        // Playhead line on top.
        if running {
            let x = (gx0 + playhead as f32 * cell_w) as i32;
            fill_rect(&mut st.px, w, h, x, gy0 as i32, x + 2, (gy0 + gh) as i32, [230, 235, 120]);
        }

        // Control strip.
        fill_rect(&mut st.px, w, h, 0, 0, w as i32, CTRL_H as i32, [30, 30, 38]);

        // Play button: box + green triangle (bright when playing).
        let playing = st.transport == Transport::Playing;
        fill_rect(
            &mut st.px,
            w,
            h,
            play_x0 as i32,
            BTN_Y as i32,
            (play_x0 + BTN_W) as i32,
            (BTN_Y + BTN_H) as i32,
            [46, 48, 58],
        );
        let pc = if playing { [110, 240, 150] } else { [80, 150, 100] };
        play_icon(
            &mut st.px,
            w,
            h,
            (play_x0 + 10.0) as i32,
            (BTN_Y + 4.0) as i32,
            (play_x0 + BTN_W - 8.0) as i32,
            (BTN_Y + BTN_H - 4.0) as i32,
            pc,
        );

        // Record button: box + red disc (bright when recording).
        let recording = st.transport == Transport::Recording;
        fill_rect(
            &mut st.px,
            w,
            h,
            rec_x0 as i32,
            BTN_Y as i32,
            (rec_x0 + BTN_W) as i32,
            (BTN_Y + BTN_H) as i32,
            [46, 48, 58],
        );
        let rc = if recording { [255, 90, 90] } else { [150, 70, 70] };
        disc(
            &mut st.px,
            w,
            h,
            (rec_x0 + BTN_W / 2.0) as i32,
            (BTN_Y + BTN_H / 2.0) as i32,
            (BTN_H / 2.0 - 4.0) as i32,
            rc,
        );

        let ctx = st.ctx.as_ref().unwrap();
        let buffer = Buffer::from_graphics_buffer(ctx.get_current_buffer());
        buffer.set(&st.px);
        ctx.present();
    }
}

struct Component;

impl Guest for Component {
    type Plugin = Sequencer;

    fn count() -> u32 {
        1
    }

    fn get(index: u32) -> Option<Descriptor> {
        if index != 0 {
            return None;
        }
        Some(Descriptor {
            id: "wk.sequencer".into(),
            name: "Sequencer".into(),
            vendor: "wk".into(),
            version: "1.0.0".into(),
            features: vec!["note-effect".into()],
        })
    }

    fn create(plugin_id: String) -> Option<Plugin> {
        if plugin_id != "wk.sequencer" {
            return None;
        }
        Some(Plugin::new(Sequencer::new()))
    }
}

bindings::export!(Component with_types_in bindings);
