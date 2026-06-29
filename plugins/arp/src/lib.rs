#[allow(warnings)]
mod bindings;

use std::collections::BTreeSet;
use std::f32::consts::PI;

use bindings::wasi::frame_buffer::frame_buffer::{Buffer, Device};
use bindings::wasi::graphics_context::graphics_context::Context as GfxContext;
use bindings::wasi::surface::surface::{CreateDesc, Surface};
use bindings::wk::midi::midi::{Input, Output};
use bindings::Guest;

/// The compositor signals roughly this many frames per second; the arp clock
/// counts frames, so this sets the time base for the rate knob.
const FPS: f32 = 60.0;

/// Lowest note shown in the on-screen strip (C4 = MIDI 60) and how many
/// semitones it spans (two octaves).
const STRIP_LOW: u8 = 60;
const STRIP_LEN: usize = 25;

// Knob indices.
const RATE: usize = 0;
const GATE: usize = 1;
const MODE: usize = 2;
const OCT: usize = 3;
const NUM_KNOBS: usize = 4;

/// Is MIDI `note` a black key (used only to tint the strip)?
fn is_black(note: u8) -> bool {
    matches!(note % 12, 1 | 3 | 6 | 8 | 10)
}

fn mode_name(mode: i32) -> &'static str {
    match mode {
        0 => "UP",
        1 => "DN",
        _ => "UPDN",
    }
}

/// A knob: a value in `[min, max]` (linear, or logarithmic for the rate) with a
/// label and colour. (Same control as the synth's knobs.)
struct Knob {
    label: &'static str,
    value: f32,
    min: f32,
    max: f32,
    log: bool,
    color: [u8; 3],
}

impl Knob {
    fn norm(&self) -> f32 {
        if self.log {
            (self.value.ln() - self.min.ln()) / (self.max.ln() - self.min.ln())
        } else {
            (self.value - self.min) / (self.max - self.min)
        }
    }
    fn set_norm(&mut self, n: f32) {
        let n = n.clamp(0.0, 1.0);
        self.value = if self.log {
            (self.min.ln() + n * (self.max.ln() - self.min.ln())).exp()
        } else {
            self.min + n * (self.max - self.min)
        };
    }
}

/// The arpeggiator: a set of held input notes, the derived step sequence, and a
/// frame clock that emits one note at a time out of the MIDI output port.
struct Arp {
    out: Output,
    /// Currently-held input notes (ascending — `BTreeSet` keeps them sorted).
    held: BTreeSet<u8>,
    /// The ascending note sequence to step through (held notes × octave span).
    seq: Vec<u8>,
    /// Position in `seq`, and the up/down direction for the UPDN mode.
    idx: usize,
    dir: i32,
    /// Frames elapsed in the current step.
    frame: f32,
    /// The output note currently sounding (so it can be turned off).
    sounding: Option<u8>,
    /// True until the first step after the held set became non-empty, so the
    /// arp starts on the bottom note rather than skipping it.
    restart: bool,
    knobs: [Knob; NUM_KNOBS],
}

impl Arp {
    fn new() -> Self {
        Arp {
            out: Output::new(),
            held: BTreeSet::new(),
            seq: Vec::new(),
            idx: 0,
            dir: 1,
            frame: 0.0,
            sounding: None,
            restart: true,
            knobs: [
                Knob {
                    label: "RATE",
                    value: 8.0,
                    min: 1.0,
                    max: 16.0,
                    log: true,
                    color: [240, 170, 80],
                },
                Knob {
                    label: "GATE",
                    value: 0.6,
                    min: 0.1,
                    max: 1.0,
                    log: false,
                    color: [90, 230, 160],
                },
                Knob {
                    label: "MODE",
                    value: 0.0,
                    min: 0.0,
                    max: 2.0,
                    log: false,
                    color: [90, 150, 240],
                },
                Knob {
                    label: "OCT",
                    value: 1.0,
                    min: 1.0,
                    max: 3.0,
                    log: false,
                    color: [200, 130, 240],
                },
            ],
        }
    }

    fn note_on(&mut self, note: u8) {
        self.held.insert(note);
    }
    fn note_off(&mut self, note: u8) {
        self.held.remove(&note);
    }

    /// Rebuild the ascending step sequence from the held notes and octave span.
    fn rebuild_seq(&mut self) {
        let oct = (self.knobs[OCT].value.round() as i32).max(1);
        self.seq.clear();
        for o in 0..oct {
            for &n in &self.held {
                let v = n as i32 + 12 * o;
                if v <= 127 {
                    self.seq.push(v as u8);
                }
            }
        }
        if !self.seq.is_empty() && self.idx >= self.seq.len() {
            self.idx = self.seq.len() - 1;
        }
    }

    /// Advance one frame of the arp clock, sending MIDI as steps fire.
    fn tick(&mut self) {
        if self.held.is_empty() {
            // All keys released: silence the held output note and reset.
            if let Some(n) = self.sounding.take() {
                self.out.send(&[0x80, n, 0]);
            }
            self.frame = 0.0;
            self.restart = true;
            self.dir = 1;
            return;
        }

        let rate = self.knobs[RATE].value;
        let step_frames = (FPS / rate).max(1.0);
        let gate = self.knobs[GATE].value;

        // Gate: end the current note partway through the step (staccato).
        if self.sounding.is_some() && self.frame >= step_frames * gate {
            let n = self.sounding.take().unwrap();
            self.out.send(&[0x80, n, 0]);
        }

        // Step boundary: move to the next note and play it.
        if self.restart || self.frame >= step_frames {
            if let Some(n) = self.sounding.take() {
                self.out.send(&[0x80, n, 0]);
            }
            self.fire_step();
            self.frame = 0.0;
        }
        self.frame += 1.0;
    }

    /// Pick the next note in the pattern and send its note-on.
    fn fire_step(&mut self) {
        if self.seq.is_empty() {
            return;
        }
        let len = self.seq.len();
        let mode = self.knobs[MODE].value.round() as i32;
        if self.restart {
            self.idx = if mode == 1 { len - 1 } else { 0 };
            self.dir = 1;
            self.restart = false;
        } else if len == 1 {
            self.idx = 0;
        } else {
            match mode {
                0 => self.idx = (self.idx + 1) % len, // up
                1 => self.idx = (self.idx + len - 1) % len, // down
                _ => {
                    // Up/down bounce: reverse at each end.
                    let mut i = self.idx as i32 + self.dir;
                    if i >= len as i32 - 1 {
                        i = len as i32 - 1;
                        self.dir = -1;
                    } else if i <= 0 {
                        i = 0;
                        self.dir = 1;
                    }
                    self.idx = i.max(0) as usize;
                }
            }
        }
        let note = self.seq[self.idx.min(len - 1)];
        self.out.send(&[0x90, note, 100]);
        self.sounding = Some(note);
    }
}

// ---- pixel drawing (shared style with the synth panel) ----

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

fn disc(buf: &mut [u8], w: u32, h: u32, cx: i32, cy: i32, r: i32, c: [u8; 3]) {
    for dy in -r..=r {
        for dx in -r..=r {
            if dx * dx + dy * dy <= r * r {
                put(buf, w, h, cx + dx, cy + dy, c);
            }
        }
    }
}

fn ring(buf: &mut [u8], w: u32, h: u32, cx: i32, cy: i32, r: i32, thick: i32, c: [u8; 3]) {
    let outer = r * r;
    let inner = (r - thick) * (r - thick);
    for dy in -r..=r {
        for dx in -r..=r {
            let d = dx * dx + dy * dy;
            if d <= outer && d >= inner {
                put(buf, w, h, cx + dx, cy + dy, c);
            }
        }
    }
}

fn line(buf: &mut [u8], w: u32, h: u32, mut x0: i32, mut y0: i32, x1: i32, y1: i32, c: [u8; 3]) {
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    loop {
        put(buf, w, h, x0, y0, c);
        if x0 == x1 && y0 == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x0 += sx;
        }
        if e2 <= dx {
            err += dx;
            y0 += sy;
        }
    }
}

/// A 3x5 bitmap glyph: 5 rows, each holding 3 bits (4=left, 2=mid, 1=right).
fn glyph(c: char) -> [u8; 5] {
    match c {
        'A' => [0b010, 0b101, 0b111, 0b101, 0b101],
        'C' => [0b111, 0b100, 0b100, 0b100, 0b111],
        'D' => [0b110, 0b101, 0b101, 0b101, 0b110],
        'E' => [0b111, 0b100, 0b111, 0b100, 0b111],
        'G' => [0b111, 0b100, 0b101, 0b101, 0b111],
        'M' => [0b101, 0b111, 0b111, 0b101, 0b101],
        'N' => [0b101, 0b111, 0b111, 0b111, 0b101],
        'O' => [0b111, 0b101, 0b101, 0b101, 0b111],
        'P' => [0b110, 0b101, 0b110, 0b100, 0b100],
        'R' => [0b110, 0b101, 0b110, 0b101, 0b101],
        'T' => [0b111, 0b010, 0b010, 0b010, 0b010],
        'U' => [0b101, 0b101, 0b101, 0b101, 0b111],
        _ => [0; 5],
    }
}

/// Draw `s` (uppercase) with its top-left at `x,y`, each cell `scale` px.
fn text(buf: &mut [u8], w: u32, h: u32, x: i32, y: i32, s: &str, scale: i32, c: [u8; 3]) {
    let mut cx = x;
    for ch in s.chars() {
        let g = glyph(ch);
        for (row, bits) in g.iter().enumerate() {
            for col in 0..3 {
                if bits & (1 << (2 - col)) != 0 {
                    for sy in 0..scale {
                        for sx in 0..scale {
                            put(buf, w, h, cx + col * scale + sx, y + row as i32 * scale + sy, c);
                        }
                    }
                }
            }
        }
        cx += 4 * scale; // 3 px glyph + 1 px gap
    }
}

/// Pixel width of `s` at `scale`.
fn text_w(s: &str, scale: i32) -> i32 {
    (s.chars().count() as i32 * 4 - 1) * scale
}

/// Draw a knob: dark body, coloured ring, an indicator from the value, and a
/// centred label below.
fn draw_knob(buf: &mut [u8], w: u32, h: u32, cx: i32, cy: i32, r: i32, k: &Knob) {
    disc(buf, w, h, cx, cy, r, [30, 30, 38]);
    ring(buf, w, h, cx, cy, r, 2, k.color);
    let theta = (k.norm() - 0.5) * 1.5 * PI;
    let len = r as f32 * 0.8;
    let tx = cx + (len * theta.sin()) as i32;
    let ty = cy - (len * theta.cos()) as i32;
    line(buf, w, h, cx, cy, tx, ty, k.color);
    disc(buf, w, h, tx, ty, 2, k.color);

    let scale = (r / 9).max(1);
    let lx = cx - text_w(k.label, scale) / 2;
    text(buf, w, h, lx, cy + r + 3, k.label, scale, [190, 190, 200]);
}

/// Knob centres and radius for the current surface size (one row of four).
fn layout(w: u32, h: u32) -> ([(i32, i32); NUM_KNOBS], i32) {
    let cw = w as f32 / NUM_KNOBS as f32;
    let r = ((cw * 0.28).min(h as f32 * 0.22)).max(8.0) as i32;
    let cy = (h as f32 * 0.72) as i32;
    let mut centers = [(0, 0); NUM_KNOBS];
    for (i, c) in centers.iter_mut().enumerate() {
        *c = ((cw * (i as f32 + 0.5)) as i32, cy);
    }
    (centers, r)
}

struct Component;

impl Guest for Component {
    fn run() {
        let surface = Surface::new(CreateDesc {
            width: Some(420),
            height: Some(190),
        });
        let ctx = GfxContext::new();
        surface.connect_graphics_context(&ctx);
        let device = Device::new();
        device.connect_graphics_context(&ctx);
        let frame = surface.subscribe_frame();
        let input = Input::new();

        let mut arp = Arp::new();
        // Knob drag state across frames: which knob, and the drag anchor.
        let mut grab: Option<usize> = None;
        let mut start_y = 0.0f32;
        let mut start_norm = 0.0f32;

        loop {
            frame.block();
            let _ = surface.get_frame();
            let w = surface.width().max(1);
            let h = surface.height().max(1);
            let (centers, r) = layout(w, h);

            // Incoming MIDI: note-on (0x90, vel>0) / note-off (0x80 or 0x90 v0).
            while let Some(msg) = input.receive() {
                if msg.len() >= 3 {
                    let status = msg[0] & 0xF0;
                    let note = msg[1];
                    let vel = msg[2];
                    match status {
                        0x90 if vel > 0 => arp.note_on(note),
                        0x80 | 0x90 => arp.note_off(note),
                        _ => {}
                    }
                }
            }

            // Mouse: grab a knob on press, turn it by dragging vertically.
            while let Some(ev) = surface.get_pointer_down() {
                let (mx, my) = (ev.x as f32, ev.y as f32);
                for (i, &(cx, cy)) in centers.iter().enumerate() {
                    let (dx, dy) = (mx - cx as f32, my - cy as f32);
                    if dx * dx + dy * dy <= ((r + 6) as f32).powi(2) {
                        grab = Some(i);
                        start_y = my;
                        start_norm = arp.knobs[i].norm();
                        break;
                    }
                }
            }
            while let Some(ev) = surface.get_pointer_move() {
                if let Some(i) = grab {
                    let n = start_norm + (start_y - ev.y as f32) / 160.0;
                    arp.knobs[i].set_norm(n);
                }
            }
            while surface.get_pointer_up().is_some() {
                grab = None;
            }
            // Drain key events (the arp is mouse-driven) so they don't pile up.
            while surface.get_key_down().is_some() {}
            while surface.get_key_up().is_some() {}

            // Advance the arpeggiator one frame (emits MIDI as steps fire).
            arp.rebuild_seq();
            arp.tick();

            // ---- paint ----
            let buffer = Buffer::from_graphics_buffer(ctx.get_current_buffer());
            let mut px = vec![0u8; (w * h * 4) as usize];
            for p in px.chunks_exact_mut(4) {
                p.copy_from_slice(&[20, 20, 26, 255]);
            }

            // Header: "ARP" and the current mode word.
            let mode = arp.knobs[MODE].value.round() as i32;
            text(&mut px, w, h, 6, 5, "ARP", 2, [220, 220, 230]);
            let mw = mode_name(mode);
            text(
                &mut px,
                w,
                h,
                w as i32 - text_w(mw, 2) - 6,
                5,
                mw,
                2,
                [150, 170, 220],
            );

            // Note strip: two octaves from C4. Held notes light up; the note
            // currently sounding is the bright playhead.
            let pad = 6i32;
            let sy0 = 24i32;
            let sy1 = (h as f32 * 0.46) as i32;
            let sw = (w as i32 - 2 * pad).max(STRIP_LEN as i32);
            let cell = sw / STRIP_LEN as i32;
            let play_cell = arp
                .sounding
                .map(|n| (n.saturating_sub(STRIP_LOW)) as i32)
                .filter(|&c| (0..STRIP_LEN as i32).contains(&c));
            for c in 0..STRIP_LEN as i32 {
                let note = STRIP_LOW + c as u8;
                let x0 = pad + c * cell;
                let x1 = x0 + cell - 1;
                let color = if play_cell == Some(c) {
                    [250, 180, 70]
                } else if arp.held.contains(&note) {
                    [70, 120, 210]
                } else if is_black(note) {
                    [30, 30, 40]
                } else {
                    [52, 52, 64]
                };
                fill_rect(&mut px, w, h, x0, sy0, x1, sy1, color);
            }

            // Knobs.
            for (i, &(cx, cy)) in centers.iter().enumerate() {
                draw_knob(&mut px, w, h, cx, cy, r, &arp.knobs[i]);
            }

            buffer.set(&px);
            ctx.present();
        }
    }
}

bindings::export!(Component with_types_in bindings);
