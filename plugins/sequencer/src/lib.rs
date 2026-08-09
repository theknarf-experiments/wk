#[allow(warnings)]
mod bindings;

use bindings::Guest;
use bindings::wasi::frame_buffer::frame_buffer::{Buffer, Device};
use bindings::wasi::graphics_context::graphics_context::Context as GfxContext;
use bindings::wasi::surface::surface::{CreateDesc, Key, Surface};
use bindings::wk::midi::midi::{Input, Output};

/// The compositor signals roughly this many frames per second; the transport
/// clock counts frames, so this sets the time base for the tempo.
const FPS: f32 = 60.0;
/// Steps per loop (one bar of sixteenth notes).
const STEPS: usize = 16;
/// Pitch rows, low to high, starting at `LOW` (C3 = MIDI 48) — two octaves + C.
const ROWS: usize = 25;
const LOW: u8 = 48;
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

/// Is MIDI `note` a black key (used to tint the piano-roll rows)?
fn is_black(note: u8) -> bool {
    matches!(note % 12, 1 | 3 | 6 | 8 | 10)
}

#[derive(PartialEq, Clone, Copy)]
enum Transport {
    Stopped,
    Playing,
    Recording,
}

/// The sequencer: a pitch × step grid, a frame-driven transport that walks the
/// columns emitting MIDI, and (while recording) a capture path that quantises
/// incoming notes onto the current step. Persists the pattern via wk:options.
struct Seq {
    out: Output,
    input: Input,
    /// `grid[row][step]` — row 0 is the lowest pitch (`LOW`).
    grid: [[bool; STEPS]; ROWS],
    transport: Transport,
    /// Current step (0..STEPS); the playhead column.
    playhead: usize,
    /// Frames elapsed in the current step.
    frame: f32,
    /// True on the first tick after starting, so we fire the current column
    /// without first advancing off it.
    restart: bool,
    /// Notes currently sounding from playback (so they can be turned off).
    sounding: Vec<u8>,
    /// The pattern changed and should be re-persisted.
    dirty: bool,
}

impl Seq {
    fn new() -> Self {
        Seq {
            out: Output::new(),
            input: Input::new(),
            grid: [[false; STEPS]; ROWS],
            transport: Transport::Stopped,
            playhead: 0,
            frame: 0.0,
            restart: true,
            sounding: Vec::new(),
            dirty: false,
        }
    }

    /// Sixteenth-note steps per second at the fixed tempo.
    fn steps_per_sec() -> f32 {
        BPM / 60.0 * 4.0
    }

    /// MIDI note number of grid row `r`.
    fn row_note(r: usize) -> u8 {
        LOW + r as u8
    }

    fn toggle_cell(&mut self, row: usize, step: usize) {
        if row < ROWS && step < STEPS {
            self.grid[row][step] = !self.grid[row][step];
            self.dirty = true;
        }
    }

    /// Turn off every note currently sounding from playback.
    fn silence(&mut self) {
        for n in self.sounding.drain(..) {
            self.out.send(&[0x80, n, 0]);
        }
    }

    /// Sound every active cell in the playhead column.
    fn fire_column(&mut self) {
        for r in 0..ROWS {
            if self.grid[r][self.playhead] {
                let note = Self::row_note(r);
                self.out.send(&[0x90, note, 100]);
                self.sounding.push(note);
            }
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
        self.frame = 0.0;
        self.restart = true;
    }

    fn stop(&mut self) {
        self.transport = Transport::Stopped;
        self.silence();
    }

    /// Advance the transport one frame, emitting MIDI as steps fire.
    fn tick(&mut self) {
        if self.transport == Transport::Stopped {
            return;
        }
        let step_frames = (FPS / Self::steps_per_sec()).max(1.0);
        if self.restart {
            self.silence();
            self.fire_column();
            self.restart = false;
            self.frame = 0.0;
        } else if self.frame >= step_frames {
            self.silence();
            self.playhead = (self.playhead + 1) % STEPS;
            self.fire_column();
            self.frame = 0.0;
        }
        self.frame += 1.0;
    }

    /// Drain incoming MIDI: while recording, quantise note-ons onto the current
    /// step; always pass every message through to the output (MIDI thru).
    fn pump_input(&mut self) {
        while let Some(msg) = self.input.receive() {
            if self.transport == Transport::Recording && msg.len() >= 3 {
                let status = msg[0] & 0xF0;
                let note = msg[1];
                if status == 0x90
                    && msg[2] > 0
                    && note >= LOW
                    && (note as usize) < LOW as usize + ROWS
                {
                    let row = (note - LOW) as usize;
                    if !self.grid[row][self.playhead] {
                        self.grid[row][self.playhead] = true;
                        self.dirty = true;
                    }
                }
            }
            self.out.send(&msg);
        }
    }

    /// Restore a saved pattern: a flat list of (step, note) pairs.
    fn load(&mut self, vals: &[f32]) {
        for pair in vals.chunks_exact(2) {
            let step = pair[0] as i32;
            let note = pair[1] as i32;
            if (0..STEPS as i32).contains(&step)
                && note >= LOW as i32
                && note < LOW as i32 + ROWS as i32
            {
                self.grid[(note - LOW as i32) as usize][step as usize] = true;
            }
        }
    }

    /// The pattern as a flat list of (step, note) pairs, to persist.
    fn options(&self) -> Vec<f32> {
        let mut v = Vec::new();
        for r in 0..ROWS {
            for s in 0..STEPS {
                if self.grid[r][s] {
                    v.push(s as f32);
                    v.push(Self::row_note(r) as f32);
                }
            }
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
        // Fraction from top (0) to bottom (1), folded to a distance from the
        // vertical centre; the triangle's right edge narrows with that distance.
        let t = ((y - y0) as f32 / bh - 0.5).abs() * 2.0;
        let right = x0 as f32 + bw * (1.0 - t);
        for x in x0..(right as i32) {
            put(buf, w, h, x, y, c);
        }
    }
}

struct Component;

impl Guest for Component {
    fn run() {
        let surface = Surface::new(CreateDesc {
            width: Some(560),
            height: Some(320),
        });
        let ctx = GfxContext::new();
        surface.connect_graphics_context(&ctx);
        let device = Device::new();
        device.connect_graphics_context(&ctx);
        let frame = surface.subscribe_frame();

        let mut seq = Seq::new();
        seq.load(&bindings::wk::options::options::load());

        loop {
            frame.block();
            let _ = surface.get_frame();
            let w = surface.width().max(1);
            let h = surface.height().max(1);
            let wf = w as f32;
            let hf = h as f32;

            // Grid geometry (below the control strip).
            let gx0 = PAD;
            let gy0 = CTRL_H + PAD;
            let gw = (wf - 2.0 * PAD).max(1.0);
            let gh = (hf - gy0 - PAD).max(1.0);
            let cell_w = gw / STEPS as f32;
            let cell_h = gh / ROWS as f32;

            // Button hit-boxes on the control strip.
            let play_x0 = PAD;
            let rec_x0 = PAD + BTN_W + BTN_GAP;
            let in_box = |px: f32, py: f32, x0: f32| {
                px >= x0 && px < x0 + BTN_W && py >= BTN_Y && py < BTN_Y + BTN_H
            };

            // Mouse: buttons on the strip, otherwise toggle a grid cell.
            while let Some(ev) = surface.get_pointer_down() {
                let (px, py) = (ev.x as f32, ev.y as f32);
                if py < CTRL_H {
                    if in_box(px, py, play_x0) {
                        seq.start(Transport::Playing);
                    } else if in_box(px, py, rec_x0) {
                        seq.start(Transport::Recording);
                    }
                    continue;
                }
                let col = ((px - gx0) / cell_w) as i32;
                let screen_row = ((py - gy0) / cell_h) as i32;
                if col >= 0
                    && (col as usize) < STEPS
                    && screen_row >= 0
                    && (screen_row as usize) < ROWS
                {
                    // Top of the grid is the highest pitch.
                    let row = ROWS - 1 - screen_row as usize;
                    seq.toggle_cell(row, col as usize);
                }
            }
            while surface.get_pointer_up().is_some() {}
            while surface.get_pointer_move().is_some() {}

            // Keyboard shortcuts: Space toggles play, R toggles record.
            while let Some(ev) = surface.get_key_down() {
                match ev.key {
                    Some(Key::Space) => seq.start(Transport::Playing),
                    Some(Key::KeyR) => seq.start(Transport::Recording),
                    _ => {}
                }
            }
            while surface.get_key_up().is_some() {}

            // Incoming MIDI (record capture + thru), then advance the transport.
            seq.pump_input();
            seq.tick();

            if seq.dirty {
                bindings::wk::options::options::store(&seq.options());
                seq.dirty = false;
            }

            // ---- paint ----
            let buffer = Buffer::from_graphics_buffer(ctx.get_current_buffer());
            let mut px = vec![0u8; (w * h * 4) as usize];
            for p in px.chunks_exact_mut(4) {
                p.copy_from_slice(&[20, 20, 26, 255]);
            }

            // Control strip.
            fill_rect(&mut px, w, h, 0, 0, w as i32, CTRL_H as i32, [30, 30, 38]);

            // Play button: box + green triangle (bright when playing).
            let playing = seq.transport == Transport::Playing;
            fill_rect(
                &mut px,
                w,
                h,
                play_x0 as i32,
                BTN_Y as i32,
                (play_x0 + BTN_W) as i32,
                (BTN_Y + BTN_H) as i32,
                [46, 48, 58],
            );
            let pc = if playing {
                [110, 240, 150]
            } else {
                [80, 150, 100]
            };
            play_icon(
                &mut px,
                w,
                h,
                (play_x0 + 10.0) as i32,
                (BTN_Y + 4.0) as i32,
                (play_x0 + BTN_W - 8.0) as i32,
                (BTN_Y + BTN_H - 4.0) as i32,
                pc,
            );

            // Record button: box + red disc (bright when recording).
            let recording = seq.transport == Transport::Recording;
            fill_rect(
                &mut px,
                w,
                h,
                rec_x0 as i32,
                BTN_Y as i32,
                (rec_x0 + BTN_W) as i32,
                (BTN_Y + BTN_H) as i32,
                [46, 48, 58],
            );
            let rc = if recording {
                [255, 90, 90]
            } else {
                [150, 70, 70]
            };
            disc(
                &mut px,
                w,
                h,
                (rec_x0 + BTN_W / 2.0) as i32,
                (BTN_Y + BTN_H / 2.0) as i32,
                (BTN_H / 2.0 - 4.0) as i32,
                rc,
            );

            // Piano roll. Rows top-to-bottom are high-to-low pitch.
            let running = seq.transport != Transport::Stopped;
            for screen_row in 0..ROWS {
                let row = ROWS - 1 - screen_row;
                let note = Seq::row_note(row);
                let y0 = (gy0 + screen_row as f32 * cell_h) as i32;
                let y1 = (gy0 + (screen_row + 1) as f32 * cell_h) as i32 - 1;
                for step in 0..STEPS {
                    let x0 = (gx0 + step as f32 * cell_w) as i32;
                    let x1 = (gx0 + (step + 1) as f32 * cell_w) as i32 - 1;
                    let on_playhead = running && step == seq.playhead;
                    let color = if seq.grid[row][step] {
                        if on_playhead {
                            [180, 255, 210]
                        } else {
                            [90, 200, 250]
                        }
                    } else if on_playhead {
                        [60, 62, 74]
                    } else if is_black(note) {
                        [30, 30, 40]
                    } else {
                        [44, 44, 56]
                    };
                    fill_rect(&mut px, w, h, x0, y0, x1, y1, color);
                }
            }

            // Beat divider lines every four steps.
            for step in (0..=STEPS).step_by(4) {
                let x = (gx0 + step as f32 * cell_w) as i32;
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
            }

            buffer.set(&px);
            ctx.present();
        }
    }
}

bindings::export!(Component with_types_in bindings);
