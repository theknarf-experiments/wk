#[allow(warnings)]
mod bindings;

use std::collections::HashMap;

use bindings::Guest;
use bindings::wasi::frame_buffer::frame_buffer::{Buffer, Device};
use bindings::wasi::graphics_context::graphics_context::Context as GfxContext;
use bindings::wasi::surface::surface::{CreateDesc, Key, Surface};
use bindings::wk::midi::midi::{Input, Output, now};

mod font;
use font::{text, text_width};

/// Pitch rows shown at once, and the lowest of them. The window scrolls by an
/// octave at a time; the notes themselves may sit anywhere in the MIDI range.
const ROWS: i32 = 25;
const LOWEST: i32 = 0;
const HIGHEST: i32 = 127;

/// Tempo limits. Wide enough for a ballad and for drum and bass.
const MIN_BPM: f32 = 20.0;
const MAX_BPM: f32 = 300.0;

/// Pattern length limits, in sixteenth-note steps: a single step up to four bars.
const MIN_STEPS: i32 = 1;
const MAX_STEPS: i32 = 64;

/// How far ahead of the clock the sequencer keeps music queued, in microseconds.
///
/// Nothing downstream can be precise about an event it has not been given yet:
/// a synth needs it before it can place the note on its audio clock, and a
/// hardware port needs it before the driver can time the byte out. So the
/// sequencer runs ahead of itself. This is long enough to survive a slow frame
/// and short enough that a tempo change or an edit takes effect almost at once,
/// since it only ever invalidates music already queued inside this window.
const LOOKAHEAD_US: f64 = 60_000.0;

/// MIDI clock pulses per quarter note — 24, fixed by the MIDI specification.
/// The sequencer is the clock master for whatever it is wired to, so an
/// arpeggiator or an external drum machine can lock to its tempo.
const PPQ: i32 = 24;
/// Clock pulses per sixteenth-note step.
const PULSES_PER_STEP: i32 = PPQ / 4;

/// A safety valve on the scheduler: at most this many steps are queued in one
/// pass, so an absurd tempo or a long stall cannot spin the loop.
const MAX_STEPS_PER_PUMP: i32 = 256;

/// The velocity a note drawn with the mouse gets. Mezzo-forte, the middle of
/// the range in practice.
const DEFAULT_VEL: u8 = 100;

/// Layout. The window is a stack: transport bar, ruler, roll, velocity lane,
/// with a piano-key gutter down the left of the roll.
const CTRL_H: f32 = 28.0;
const RULER_H: f32 = 13.0;
const GUTTER_W: f32 = 30.0;
const VEL_H: f32 = 44.0;
const PAD: f32 = 6.0;

/// Button box metrics on the transport bar.
const BTN_W: f32 = 34.0;
const BTN_H: f32 = 20.0;
const BTN_Y: f32 = 4.0;
const BTN_GAP: f32 = 8.0;

/// Pixels at a note's right edge that grab the resize handle.
const RESIZE_PX: f32 = 7.0;

/// Vertical pixels of drag that span a field's whole range.
const DRAG_SPAN: f32 = 200.0;

/// The saved-options layout is tagged so an older saved pattern still loads.
/// A step is never negative, so no pattern written by the first version of this
/// node can begin with this value.
const SAVE_TAG: f32 = -1.0;
const SAVE_VERSION: f32 = 1.0;

/// Is MIDI `note` a black key (used to tint the piano-roll rows)?
fn is_black(note: i32) -> bool {
    matches!(note.rem_euclid(12), 1 | 3 | 6 | 8 | 10)
}

/// The name of MIDI `note`, e.g. `C4` or `F#3`, with middle C as C4.
fn note_name(note: i32) -> String {
    const NAMES: [&str; 12] = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    ];
    format!(
        "{}{}",
        NAMES[note.rem_euclid(12) as usize],
        note.div_euclid(12) - 1
    )
}

/// Microseconds per sixteenth-note step at `bpm`.
fn step_micros(bpm: f32) -> f64 {
    60_000_000.0 / bpm as f64 / 4.0
}

#[derive(PartialEq, Clone, Copy)]
enum Transport {
    Stopped,
    Playing,
    Recording,
}

/// A note in the roll: a `step` start column, a `pitch`, a `len` in steps
/// (at least 1), and the `vel` it sounds at.
#[derive(Clone, Copy, PartialEq)]
struct Note {
    step: i32,
    pitch: i32,
    len: i32,
    vel: u8,
}

impl Note {
    /// Is this note sounding at pattern position `p` of a `steps`-long pattern?
    /// A note left hanging over the loop point by a shortened pattern is cut
    /// off at the end rather than sounding into the next cycle.
    fn covers(&self, p: i32, steps: i32) -> bool {
        self.step <= p && p < (self.step + self.len).min(steps)
    }
}

/// A mouse edit in progress. Offsets are captured at grab time so a note tracks
/// the cursor without snapping to it.
enum Drag {
    None,
    Move {
        idx: usize,
        doff: i32,
        poff: i32,
    },
    Resize {
        idx: usize,
    },
    /// Painting velocities in the lane under the cursor.
    Velocity,
    /// Turning the tempo or the pattern length by dragging vertically.
    Tempo {
        start_y: f32,
        start: f32,
    },
    Length {
        start_y: f32,
        start: f32,
    },
}

/// The sequencer.
///
/// The transport is driven by wk's shared MIDI clock, not by the frame rate.
/// Each frame it works out which step boundaries fall inside the look-ahead
/// window and sends their notes stamped with the instant they belong to, so the
/// music keeps the clock's time rather than the display's.
struct Seq {
    out: Output,
    input: Input,
    notes: Vec<Note>,
    /// Pattern length in sixteenth-note steps.
    steps: i32,
    bpm: f32,

    transport: Transport,
    /// Microseconds per step at the current tempo.
    step_us: f64,
    /// The instant of absolute step 0 under the current tempo, in microseconds
    /// on the shared clock. Fractional, so the tempo never rounds into drift.
    origin: f64,
    /// The first absolute step whose events have not been sent yet. Everything
    /// before it is already queued downstream.
    next_step: i64,
    /// Pitches whose note-on has been sent and note-off has not.
    scheduled_on: Vec<i32>,

    /// Notes being recorded right now: incoming pitch -> its index in `notes`
    /// and the absolute step its note-on was quantised to.
    pending: HashMap<i32, (usize, i64)>,

    /// The lowest pitch row on screen.
    low: i32,
    /// The selected note (index into `notes`), for drag/resize/delete.
    selected: Option<usize>,
    /// Pattern states to step back to, and the ones stepped back from.
    undo: Vec<Vec<Note>>,
    redo: Vec<Vec<Note>>,
    /// The pattern or its settings changed and should be re-persisted.
    dirty: bool,
}

impl Seq {
    fn new() -> Self {
        Seq {
            out: Output::new(),
            input: Input::new(),
            notes: Vec::new(),
            steps: 16,
            bpm: 120.0,
            transport: Transport::Stopped,
            step_us: step_micros(120.0),
            origin: 0.0,
            next_step: 0,
            scheduled_on: Vec::new(),
            pending: HashMap::new(),
            low: 48,
            selected: None,
            undo: Vec::new(),
            redo: Vec::new(),
            dirty: false,
        }
    }

    // ---- editing ----

    /// Record the pattern as it is now, so the next edit can be stepped back.
    /// Called once before a change begins, not once per pixel of a drag.
    fn checkpoint(&mut self) {
        self.undo.push(self.notes.clone());
        if self.undo.len() > 128 {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    fn undo(&mut self) {
        if let Some(prev) = self.undo.pop() {
            self.redo.push(std::mem::replace(&mut self.notes, prev));
            self.after_pattern_change();
        }
    }

    fn redo(&mut self) {
        if let Some(next) = self.redo.pop() {
            self.undo.push(std::mem::replace(&mut self.notes, next));
            self.after_pattern_change();
        }
    }

    /// Selection and in-flight recording both index into `notes`, so neither
    /// survives a wholesale replacement of it.
    fn after_pattern_change(&mut self) {
        self.selected = None;
        self.pending.clear();
        self.dirty = true;
    }

    /// Add a note (clamping its length inside the pattern) and return its index.
    fn add_note(&mut self, step: i32, pitch: i32, len: i32, vel: u8) -> usize {
        let len = len.clamp(1, (self.steps - step).max(1));
        self.notes.push(Note {
            step,
            pitch,
            len,
            vel,
        });
        self.dirty = true;
        self.notes.len() - 1
    }

    /// The topmost note under `(step, pitch)`, if any.
    fn note_at(&self, step: i32, pitch: i32) -> Option<usize> {
        (0..self.notes.len())
            .rev()
            .find(|&i| self.notes[i].pitch == pitch && self.notes[i].covers(step, self.steps))
    }

    /// Remove the selected note.
    fn delete_selected(&mut self) {
        if let Some(i) = self.selected.take() {
            if i < self.notes.len() {
                self.checkpoint();
                self.notes.remove(i);
                self.dirty = true;
            }
            self.pending.clear();
        }
    }

    // ---- settings ----

    fn set_bpm(&mut self, bpm: f32) {
        let bpm = bpm.clamp(MIN_BPM, MAX_BPM);
        if (bpm - self.bpm).abs() < 0.005 {
            return;
        }
        // Re-anchor the clock on the next boundary still to be scheduled, so it
        // stays exactly where it is and the new rate applies from there on.
        // Nothing already sent is invalidated and no boundary is sent twice.
        let pivot = self.step_instant(self.next_step);
        self.bpm = bpm;
        self.step_us = step_micros(bpm);
        self.origin = pivot - self.next_step as f64 * self.step_us;
        self.dirty = true;
    }

    fn set_steps(&mut self, steps: i32) {
        let steps = steps.clamp(MIN_STEPS, MAX_STEPS);
        if steps != self.steps {
            self.steps = steps;
            self.dirty = true;
        }
    }

    // ---- transport ----

    /// The instant absolute step `i` falls on.
    fn step_instant(&self, i: i64) -> f64 {
        self.origin + i as f64 * self.step_us
    }

    /// The pattern position the playhead is on at `now`.
    fn playhead(&self, now: u64) -> i32 {
        if self.transport == Transport::Stopped {
            return 0;
        }
        let f = ((now as f64 - self.origin) / self.step_us).floor() as i64;
        f.rem_euclid(self.steps as i64) as i32
    }

    fn start(&mut self, mode: Transport, now: u64) {
        // Pressing the button of the running mode stops; pressing the other one
        // switches mode without disturbing the clock.
        if self.transport == mode {
            self.stop();
            return;
        }
        let from_stopped = self.transport == Transport::Stopped;
        self.transport = mode;
        if from_stopped {
            self.origin = now as f64;
            self.next_step = 0;
            self.scheduled_on.clear();
            self.out.send_at(&[0xFA], now); // MIDI start
        }
    }

    fn stop(&mut self) {
        if self.transport == Transport::Stopped {
            return;
        }
        // Release at the end of what is already queued. Sending note-offs for
        // "now" would put them before note-ons already scheduled ahead of the
        // clock, and those notes would sound forever.
        let at = self.step_instant(self.next_step).max(0.0) as u64;
        for pitch in std::mem::take(&mut self.scheduled_on) {
            self.out.send_at(&[0x80, pitch as u8, 0], at);
        }
        self.out.send_at(&[0xFC], at); // MIDI stop
        self.transport = Transport::Stopped;
        self.pending.clear();
    }

    /// Queue every step boundary that falls inside the look-ahead window.
    fn pump(&mut self, now: u64) {
        if self.transport == Transport::Stopped {
            return;
        }
        let horizon = now as f64 + LOOKAHEAD_US;
        let mut budget = MAX_STEPS_PER_PUMP;
        while self.step_instant(self.next_step) < horizon && budget > 0 {
            self.schedule_step(self.next_step);
            self.next_step += 1;
            budget -= 1;
        }
    }

    /// Send everything that happens on absolute step `i`, stamped for the
    /// instant that step falls on.
    fn schedule_step(&mut self, i: i64) {
        let t = self.step_instant(i);
        let p = i.rem_euclid(self.steps as i64) as i32;

        // The clock runs whether or not there are notes, so anything slaved to
        // this sequencer keeps time through an empty bar.
        for k in 0..PULSES_PER_STEP {
            let tick = t + k as f64 * self.step_us / PULSES_PER_STEP as f64;
            self.out.send_at(&[0xF8], tick.max(0.0) as u64);
        }

        // What should be sounding on this step. While recording, a pitch being
        // played live is left alone: MIDI thru is already sounding it, and
        // re-triggering it here would double the note.
        let live: Vec<i32> = self.pending.keys().copied().collect();
        let recording = self.transport == Transport::Recording;
        let mut want: Vec<(i32, u8)> = Vec::new();
        for n in &self.notes {
            if n.covers(p, self.steps)
                && !(recording && live.contains(&n.pitch))
                && !want.iter().any(|&(pitch, _)| pitch == n.pitch)
            {
                want.push((n.pitch, n.vel));
            }
        }

        let at = t.max(0.0) as u64;
        let offs: Vec<i32> = self
            .scheduled_on
            .iter()
            .copied()
            .filter(|p| !want.iter().any(|&(pitch, _)| pitch == *p))
            .collect();
        for pitch in offs {
            self.out.send_at(&[0x80, pitch as u8, 0], at);
        }
        self.scheduled_on
            .retain(|p| want.iter().any(|&(pitch, _)| pitch == *p));
        for (pitch, vel) in want {
            if !self.scheduled_on.contains(&pitch) {
                self.out.send_at(&[0x90, pitch as u8, vel.max(1)], at);
                self.scheduled_on.push(pitch);
            }
        }
    }

    /// The absolute step an instant is nearest to.
    fn step_of(&self, instant: u64) -> i64 {
        ((instant as f64 - self.origin) / self.step_us).round() as i64
    }

    /// Drain incoming MIDI: while recording, open a note on note-on and close it
    /// on note-off, both placed by the instant the message carries rather than
    /// by the frame it was drained on. Every message is passed through to the
    /// output, so the node is a MIDI thru as well (that is what lets you hear
    /// what you are playing while it records).
    fn pump_input(&mut self) {
        while let Some(ev) = self.input.receive_event() {
            let msg = &ev.data;
            if self.transport == Transport::Recording && msg.len() >= 3 {
                let status = msg[0] & 0xF0;
                let pitch = msg[1] as i32;
                let vel = msg[2];
                let on = status == 0x90 && vel > 0;
                let off = status == 0x80 || (status == 0x90 && vel == 0);
                let when = if ev.time == 0 { now() } else { ev.time };
                if on && (LOWEST..=HIGHEST).contains(&pitch) && !self.pending.contains_key(&pitch) {
                    let abs = self.step_of(when);
                    let p = abs.rem_euclid(self.steps as i64) as i32;
                    if self.pending.is_empty() {
                        self.checkpoint();
                    }
                    let idx = self.add_note(p, pitch, 1, vel);
                    self.pending.insert(pitch, (idx, abs));
                } else if let Some((idx, abs_on)) =
                    off.then(|| self.pending.remove(&pitch)).flatten()
                {
                    // Length comes from when the key was actually released, so
                    // a held note records as held.
                    let held = (self.step_of(when) - abs_on).max(1) as i32;
                    if let Some(n) = self.notes.get_mut(idx) {
                        n.len = held.clamp(1, (self.steps - n.step).max(1));
                        self.dirty = true;
                    }
                }
            }
            self.out.send_at(msg, ev.time);
        }
    }

    // ---- persistence ----

    /// Restore saved settings and pattern.
    ///
    /// A tagged list carries the tempo, the pattern length and four values per
    /// note. An untagged one is a pattern saved before velocity and tempo
    /// existed: bare `(step, pitch, len)` triples, which still load.
    fn load(&mut self, vals: &[f32]) {
        let (body, tagged) = match vals {
            [tag, version, bpm, steps, rest @ ..]
                if *tag == SAVE_TAG && *version <= SAVE_VERSION =>
            {
                self.set_steps(*steps as i32);
                self.bpm = bpm.clamp(MIN_BPM, MAX_BPM);
                self.step_us = step_micros(self.bpm);
                (rest, true)
            }
            _ => (vals, false),
        };
        let stride = if tagged { 4 } else { 3 };
        for t in body.chunks_exact(stride) {
            let (step, pitch, len) = (t[0] as i32, t[1] as i32, t[2] as i32);
            let vel = if tagged {
                (t[3] as i32).clamp(1, 127) as u8
            } else {
                DEFAULT_VEL
            };
            if (0..self.steps).contains(&step)
                && (LOWEST..=HIGHEST).contains(&pitch)
                && len >= 1
                && step + len <= self.steps
            {
                self.notes.push(Note {
                    step,
                    pitch,
                    len,
                    vel,
                });
            }
        }
        // Show the octave the pattern is actually in.
        if let Some(min) = self.notes.iter().map(|n| n.pitch).min() {
            self.low = (min - 2).clamp(LOWEST, HIGHEST - ROWS + 1);
        }
    }

    /// Settings and pattern, flattened for the host to persist.
    fn options(&self) -> Vec<f32> {
        let mut v = vec![SAVE_TAG, SAVE_VERSION, self.bpm, self.steps as f32];
        for n in &self.notes {
            v.extend_from_slice(&[n.step as f32, n.pitch as f32, n.len as f32, n.vel as f32]);
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

/// Scale a colour by `f`, so a note's velocity reads as its brightness.
fn dim(c: [u8; 3], f: f32) -> [u8; 3] {
    let f = f.clamp(0.0, 1.0);
    [
        (c[0] as f32 * f) as u8,
        (c[1] as f32 * f) as u8,
        (c[2] as f32 * f) as u8,
    ]
}

// ---- geometry ----

/// Where everything sits for the current surface size. Input and painting both
/// read this, so a click always lands on what was drawn.
struct Layout {
    /// The piano roll, right of the key gutter and below the ruler.
    gx0: f32,
    gy0: f32,
    gw: f32,
    gh: f32,
    cell_w: f32,
    cell_h: f32,
    /// The velocity lane along the bottom.
    vel_y0: f32,
    vel_y1: f32,
    /// Transport-bar hit boxes.
    play_x: f32,
    rec_x: f32,
    bpm_x: f32,
    len_x: f32,
    field_w: f32,
}

const FIELD_W: f32 = 62.0;

fn layout(w: u32, h: u32, steps: i32) -> Layout {
    let (wf, hf) = (w as f32, h as f32);
    let vel_y1 = hf - PAD;
    let vel_y0 = (vel_y1 - VEL_H).max(CTRL_H + RULER_H + 20.0);
    let gy0 = CTRL_H + RULER_H;
    let gx0 = GUTTER_W;
    let gw = (wf - GUTTER_W - PAD).max(1.0);
    let gh = (vel_y0 - gy0 - 4.0).max(1.0);
    let play_x = PAD;
    let rec_x = play_x + BTN_W + BTN_GAP;
    let bpm_x = rec_x + BTN_W + BTN_GAP * 2.0;
    let len_x = bpm_x + FIELD_W + BTN_GAP;
    Layout {
        gx0,
        gy0,
        gw,
        gh,
        cell_w: gw / steps as f32,
        cell_h: gh / ROWS as f32,
        vel_y0,
        vel_y1,
        play_x,
        rec_x,
        bpm_x,
        len_x,
        field_w: FIELD_W,
    }
}

impl Layout {
    /// The step column a pixel x falls in. May be outside the pattern.
    fn to_step(&self, px: f32) -> i32 {
        ((px - self.gx0) / self.cell_w).floor() as i32
    }

    /// The pitch a pixel y falls on, given the lowest row on screen.
    fn to_pitch(&self, py: f32, low: i32) -> i32 {
        let row = ((py - self.gy0) / self.cell_h).floor() as i32;
        low + (ROWS - 1 - row)
    }

    fn in_box(&self, px: f32, py: f32, x0: f32, wide: f32) -> bool {
        px >= x0 && px < x0 + wide && py >= BTN_Y && py < BTN_Y + BTN_H
    }
}

struct Component;

impl Guest for Component {
    fn run() {
        let surface = Surface::new(CreateDesc {
            width: Some(720),
            height: Some(420),
        });
        let ctx = GfxContext::new();
        surface.connect_graphics_context(&ctx);
        let device = Device::new();
        device.connect_graphics_context(&ctx);
        let frame = surface.subscribe_frame();

        let mut seq = Seq::new();
        seq.load(&bindings::wk::options::options::load());
        let mut drag = Drag::None;

        loop {
            frame.block();
            let _ = surface.get_frame();
            let w = surface.width().max(1);
            let h = surface.height().max(1);
            let lay = layout(w, h, seq.steps);
            let t = now();

            // ---- pointer ----
            while let Some(ev) = surface.get_pointer_down() {
                let (px, py) = (ev.x as f32, ev.y as f32);
                if py < CTRL_H {
                    if lay.in_box(px, py, lay.play_x, BTN_W) {
                        seq.start(Transport::Playing, t);
                    } else if lay.in_box(px, py, lay.rec_x, BTN_W) {
                        seq.start(Transport::Recording, t);
                    } else if lay.in_box(px, py, lay.bpm_x, lay.field_w) {
                        drag = Drag::Tempo {
                            start_y: py,
                            start: seq.bpm,
                        };
                    } else if lay.in_box(px, py, lay.len_x, lay.field_w) {
                        drag = Drag::Length {
                            start_y: py,
                            start: seq.steps as f32,
                        };
                    }
                    continue;
                }
                if py >= lay.vel_y0 {
                    // The velocity lane: drag across it to shape the pattern's
                    // dynamics, the way a piano roll has done for thirty years.
                    seq.checkpoint();
                    paint_velocity(&mut seq, &lay, px, py);
                    drag = Drag::Velocity;
                    continue;
                }
                if px < lay.gx0 {
                    continue; // the key gutter is a label, not a control
                }
                let step = lay.to_step(px);
                let pitch = lay.to_pitch(py, seq.low);
                if !(0..seq.steps).contains(&step) || !(LOWEST..=HIGHEST).contains(&pitch) {
                    continue;
                }
                match seq.note_at(step, pitch) {
                    Some(idx) => {
                        seq.selected = Some(idx);
                        seq.checkpoint();
                        let n = seq.notes[idx];
                        let right_px = lay.gx0 + (n.step + n.len) as f32 * lay.cell_w;
                        drag = if px >= right_px - RESIZE_PX {
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
                        // Empty space: draw a new one-step note and grab its
                        // edge, so a horizontal drag sets its length.
                        seq.checkpoint();
                        let idx = seq.add_note(step, pitch, 1, DEFAULT_VEL);
                        seq.selected = Some(idx);
                        drag = Drag::Resize { idx };
                    }
                }
            }

            while let Some(ev) = surface.get_pointer_move() {
                let (px, py) = (ev.x as f32, ev.y as f32);
                match drag {
                    Drag::Move { idx, doff, poff } if idx < seq.notes.len() => {
                        let len = seq.notes[idx].len;
                        let step = (lay.to_step(px) + doff).clamp(0, (seq.steps - len).max(0));
                        let pitch = (lay.to_pitch(py, seq.low) + poff).clamp(LOWEST, HIGHEST);
                        let n = &mut seq.notes[idx];
                        if n.step != step || n.pitch != pitch {
                            n.step = step;
                            n.pitch = pitch;
                            seq.dirty = true;
                        }
                    }
                    Drag::Resize { idx } if idx < seq.notes.len() => {
                        let start = seq.notes[idx].step;
                        let len = (lay.to_step(px) - start + 1).clamp(1, seq.steps - start);
                        let n = &mut seq.notes[idx];
                        if n.len != len {
                            n.len = len;
                            seq.dirty = true;
                        }
                    }
                    Drag::Velocity => paint_velocity(&mut seq, &lay, px, py),
                    Drag::Tempo { start_y, start } => {
                        seq.set_bpm(start + (start_y - py) / DRAG_SPAN * (MAX_BPM - MIN_BPM));
                    }
                    Drag::Length { start_y, start } => {
                        let span = (MAX_STEPS - MIN_STEPS) as f32;
                        seq.set_steps((start + (start_y - py) / DRAG_SPAN * span).round() as i32);
                    }
                    _ => {}
                }
            }

            while surface.get_pointer_up().is_some() {
                drag = Drag::None;
            }

            // ---- keyboard ----
            while let Some(ev) = surface.get_key_down() {
                let cmd = ev.meta_key || ev.ctrl_key;
                match ev.key {
                    Some(Key::Space) => seq.start(Transport::Playing, t),
                    Some(Key::KeyR) => seq.start(Transport::Recording, t),
                    Some(Key::KeyZ) if cmd && ev.shift_key => seq.redo(),
                    Some(Key::KeyZ) if cmd => seq.undo(),
                    Some(Key::Backspace) | Some(Key::Delete) => {
                        seq.delete_selected();
                        drag = Drag::None;
                    }
                    // Scroll the pitch window an octave at a time.
                    Some(Key::ArrowUp) => seq.low = (seq.low + 12).min(HIGHEST - ROWS + 1),
                    Some(Key::ArrowDown) => seq.low = (seq.low - 12).max(LOWEST),
                    // Nudge the selected note along the bar.
                    Some(Key::ArrowLeft) | Some(Key::ArrowRight) => {
                        let delta = if ev.key == Some(Key::ArrowLeft) {
                            -1
                        } else {
                            1
                        };
                        if let Some(i) = seq.selected.filter(|&i| i < seq.notes.len()) {
                            seq.checkpoint();
                            let len = seq.notes[i].len;
                            let n = &mut seq.notes[i];
                            n.step = (n.step + delta).clamp(0, (seq.steps - len).max(0));
                            seq.dirty = true;
                        }
                    }
                    _ => {}
                }
            }
            while surface.get_key_up().is_some() {}

            // Incoming MIDI (record capture + thru), then queue the music that
            // falls inside the look-ahead window.
            seq.pump_input();
            seq.pump(t);

            if seq.dirty {
                bindings::wk::options::options::store(&seq.options());
                seq.dirty = false;
            }

            paint(&surface, &ctx, &seq, &lay, t);
        }
    }
}

/// Set the velocity of the notes starting in the column under the cursor, from
/// how high in the lane it is.
fn paint_velocity(seq: &mut Seq, lay: &Layout, px: f32, py: f32) {
    let step = lay.to_step(px);
    if !(0..seq.steps).contains(&step) {
        return;
    }
    let f = ((lay.vel_y1 - py) / (lay.vel_y1 - lay.vel_y0)).clamp(0.0, 1.0);
    let vel = (f * 126.0) as u8 + 1;
    for n in seq.notes.iter_mut().filter(|n| n.step == step) {
        if n.vel != vel {
            n.vel = vel;
            seq.dirty = true;
        }
    }
}

/// Paint a frame.
fn paint(surface: &Surface, ctx: &GfxContext, seq: &Seq, lay: &Layout, t: u64) {
    let w = surface.width().max(1);
    let h = surface.height().max(1);
    let buffer = Buffer::from_graphics_buffer(ctx.get_current_buffer());
    let mut px = vec![0u8; (w * h * 4) as usize];
    for p in px.chunks_exact_mut(4) {
        p.copy_from_slice(&[20, 20, 26, 255]);
    }

    let running = seq.transport != Transport::Stopped;
    let playhead = seq.playhead(t);
    let (gx0, gy0, gw, gh) = (lay.gx0, lay.gy0, lay.gw, lay.gh);

    // Row backgrounds, tinted by black/white key.
    for row in 0..ROWS {
        let pitch = seq.low + (ROWS - 1 - row);
        let y0 = (gy0 + row as f32 * lay.cell_h) as i32;
        let y1 = (gy0 + (row + 1) as f32 * lay.cell_h) as i32;
        let c = if is_black(pitch) {
            [30, 30, 40]
        } else {
            [44, 44, 56]
        };
        fill_rect(&mut px, w, h, gx0 as i32, y0, (gx0 + gw) as i32, y1, c);

        // The key gutter: a piano keyboard turned on its side, named so you can
        // see which octave you are looking at.
        let key = if is_black(pitch) {
            [26, 26, 32]
        } else {
            [200, 200, 210]
        };
        fill_rect(&mut px, w, h, 0, y0, gx0 as i32 - 1, y1, key);
        if lay.cell_h >= 9.0 {
            let label = note_name(pitch);
            let col = if is_black(pitch) {
                [170, 170, 180]
            } else {
                [40, 40, 48]
            };
            text(&mut px, w, h, 2, y0 + 1, &label, 1, col);
        }
    }

    // Playhead column highlight.
    if running {
        let x0 = (gx0 + playhead as f32 * lay.cell_w) as i32;
        let x1 = (gx0 + (playhead + 1) as f32 * lay.cell_w) as i32;
        fill_rect(
            &mut px,
            w,
            h,
            x0,
            gy0 as i32,
            x1,
            (gy0 + gh) as i32,
            [56, 58, 70],
        );
    }

    // Beat dividers, and the ruler that numbers the beats.
    fill_rect(
        &mut px,
        w,
        h,
        0,
        CTRL_H as i32,
        w as i32,
        (CTRL_H + RULER_H) as i32,
        [26, 26, 34],
    );
    for step in (0..=seq.steps).step_by(4) {
        let x = (gx0 + step as f32 * lay.cell_w) as i32;
        fill_rect(
            &mut px,
            w,
            h,
            x,
            gy0 as i32,
            x + 1,
            (gy0 + gh) as i32,
            [70, 70, 84],
        );
        if step < seq.steps {
            // Beats numbered from one, the way a musician counts them.
            let label = format!("{}", step / 4 + 1);
            text(
                &mut px,
                w,
                h,
                x + 2,
                CTRL_H as i32 + 3,
                &label,
                1,
                [130, 130, 150],
            );
        }
    }

    // Notes. Brightness carries velocity, so the dynamics are visible in the
    // roll and not only in the lane below it.
    for (i, n) in seq.notes.iter().enumerate() {
        let row = ROWS - 1 - (n.pitch - seq.low);
        if !(0..ROWS).contains(&row) || n.step >= seq.steps {
            continue;
        }
        let end = (n.step + n.len).min(seq.steps);
        let x0 = (gx0 + n.step as f32 * lay.cell_w) as i32 + 1;
        let x1 = (gx0 + end as f32 * lay.cell_w) as i32 - 1;
        let y0 = (gy0 + row as f32 * lay.cell_h) as i32 + 1;
        let y1 = (gy0 + (row + 1) as f32 * lay.cell_h) as i32 - 1;
        let base = if running && n.covers(playhead, seq.steps) {
            [160, 255, 200]
        } else {
            [90, 200, 250]
        };
        // Never fully dark: a quiet note still has to be visible to be edited.
        let shade = dim(base, 0.4 + 0.6 * (n.vel as f32 / 127.0));
        fill_rect(&mut px, w, h, x0, y0, x1, y1, shade);
        if seq.selected == Some(i) {
            stroke_rect(
                &mut px,
                w,
                h,
                x0 - 1,
                y0 - 1,
                x1 + 1,
                y1 + 1,
                [240, 245, 255],
            );
        }
    }

    // Playhead line on top.
    if running {
        let x = (gx0 + playhead as f32 * lay.cell_w) as i32;
        fill_rect(
            &mut px,
            w,
            h,
            x,
            gy0 as i32,
            x + 2,
            (gy0 + gh) as i32,
            [230, 235, 120],
        );
    }

    // ---- velocity lane ----
    fill_rect(
        &mut px,
        w,
        h,
        0,
        lay.vel_y0 as i32,
        w as i32,
        lay.vel_y1 as i32,
        [26, 26, 34],
    );
    text(
        &mut px,
        w,
        h,
        3,
        lay.vel_y0 as i32 + 2,
        "VEL",
        1,
        [110, 110, 130],
    );
    let lane_h = lay.vel_y1 - lay.vel_y0;
    for n in seq.notes.iter().filter(|n| n.step < seq.steps) {
        let x0 = (gx0 + n.step as f32 * lay.cell_w) as i32 + 1;
        let x1 = (gx0 + (n.step + 1) as f32 * lay.cell_w) as i32 - 1;
        let bar = lane_h * (n.vel as f32 / 127.0);
        let y0 = (lay.vel_y1 - bar) as i32;
        fill_rect(
            &mut px,
            w,
            h,
            x0,
            y0,
            x1.max(x0 + 1),
            lay.vel_y1 as i32,
            [90, 160, 210],
        );
    }

    // ---- transport bar ----
    fill_rect(&mut px, w, h, 0, 0, w as i32, CTRL_H as i32, [30, 30, 38]);

    let playing = seq.transport == Transport::Playing;
    fill_rect(
        &mut px,
        w,
        h,
        lay.play_x as i32,
        BTN_Y as i32,
        (lay.play_x + BTN_W) as i32,
        (BTN_Y + BTN_H) as i32,
        [46, 48, 58],
    );
    play_icon(
        &mut px,
        w,
        h,
        (lay.play_x + 9.0) as i32,
        (BTN_Y + 4.0) as i32,
        (lay.play_x + BTN_W - 7.0) as i32,
        (BTN_Y + BTN_H - 4.0) as i32,
        if playing {
            [110, 240, 150]
        } else {
            [80, 150, 100]
        },
    );

    let recording = seq.transport == Transport::Recording;
    fill_rect(
        &mut px,
        w,
        h,
        lay.rec_x as i32,
        BTN_Y as i32,
        (lay.rec_x + BTN_W) as i32,
        (BTN_Y + BTN_H) as i32,
        [46, 48, 58],
    );
    disc(
        &mut px,
        w,
        h,
        (lay.rec_x + BTN_W / 2.0) as i32,
        (BTN_Y + BTN_H / 2.0) as i32,
        (BTN_H / 2.0 - 4.0) as i32,
        if recording {
            [255, 90, 90]
        } else {
            [150, 70, 70]
        },
    );

    // Tempo and pattern length, each a field you drag vertically to set.
    for (x, label, value) in [
        (lay.bpm_x, "BPM", format!("{}", seq.bpm.round() as i32)),
        (lay.len_x, "LEN", format!("{}", seq.steps)),
    ] {
        fill_rect(
            &mut px,
            w,
            h,
            x as i32,
            BTN_Y as i32,
            (x + lay.field_w) as i32,
            (BTN_Y + BTN_H) as i32,
            [46, 48, 58],
        );
        text(
            &mut px,
            w,
            h,
            x as i32 + 4,
            BTN_Y as i32 + 7,
            label,
            1,
            [120, 122, 140],
        );
        let vw = text_width(&value, 2);
        text(
            &mut px,
            w,
            h,
            (x + lay.field_w) as i32 - 4 - vw,
            BTN_Y as i32 + 3,
            &value,
            2,
            [225, 230, 245],
        );
    }

    buffer.set(&px);
    ctx.present();
}

bindings::export!(Component with_types_in bindings);
