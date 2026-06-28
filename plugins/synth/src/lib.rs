#[allow(warnings)]
mod bindings;

use std::collections::HashMap;
use std::f32::consts::PI;

use bindings::wasi::frame_buffer::frame_buffer::{Buffer, Device};
use bindings::wasi::graphics_context::graphics_context::Context as GfxContext;
use bindings::wasi::surface::surface::{CreateDesc, Surface};
use bindings::wk::midi::midi::Input;
use bindings::wk::webaudio::audio::{Context as Audio, Gain, Oscillator, OscillatorType};
use bindings::Guest;

const VOLUME: usize = 0;
const WAVE: usize = 1;
const TUNE: usize = 2;

/// Equal-temperament frequency of a MIDI `note`, shifted by `tune` semitones.
fn freq(note: u8, tune: f32) -> f32 {
    440.0 * 2.0f32.powf((note as f32 - 69.0 + tune) / 12.0)
}

/// Map the wave knob (0..3) to an oscillator waveform.
fn osc_type(wave: f32) -> OscillatorType {
    match wave.round() as i32 {
        0 => OscillatorType::Sine,
        1 => OscillatorType::Square,
        2 => OscillatorType::Sawtooth,
        _ => OscillatorType::Triangle,
    }
}

/// One cycle of waveform `idx` at `phase` in 0..1, returning -1..1 (for the
/// on-screen preview).
fn wave_sample(idx: i32, phase: f32) -> f32 {
    match idx {
        0 => (phase * 2.0 * PI).sin(),
        1 => {
            if phase < 0.5 {
                1.0
            } else {
                -1.0
            }
        }
        2 => 2.0 * phase - 1.0,
        _ => {
            if phase < 0.5 {
                4.0 * phase - 1.0
            } else {
                3.0 - 4.0 * phase
            }
        }
    }
}

/// A knob: a value in `[min, max]` with a display colour.
struct Knob {
    value: f32,
    min: f32,
    max: f32,
    color: [u8; 3],
}

impl Knob {
    fn norm(&self) -> f32 {
        (self.value - self.min) / (self.max - self.min)
    }
    fn set_norm(&mut self, n: f32) {
        self.value = self.min + n.clamp(0.0, 1.0) * (self.max - self.min);
    }
}

/// A sounding note: oscillator -> gain -> speakers.
struct Voice {
    osc: Oscillator,
    gain: Gain,
}

/// The synth: a bank of voices keyed by MIDI note, plus three knobs whose values
/// are applied live to every sounding voice.
struct Synth {
    audio: Audio,
    voices: HashMap<u8, Voice>,
    knobs: [Knob; 3],
}

impl Synth {
    fn new() -> Self {
        Synth {
            audio: Audio::new(),
            voices: HashMap::new(),
            knobs: [
                Knob {
                    value: 0.2,
                    min: 0.0,
                    max: 1.0,
                    color: [90, 150, 240],
                },
                Knob {
                    value: 3.0,
                    min: 0.0,
                    max: 3.0,
                    color: [90, 230, 160],
                },
                Knob {
                    value: 0.0,
                    min: -12.0,
                    max: 12.0,
                    color: [240, 170, 80],
                },
            ],
        }
    }

    fn volume(&self) -> f32 {
        self.knobs[VOLUME].value
    }
    fn wave(&self) -> OscillatorType {
        osc_type(self.knobs[WAVE].value)
    }
    fn tune(&self) -> f32 {
        self.knobs[TUNE].value
    }

    fn note_on(&mut self, note: u8) {
        if self.voices.contains_key(&note) {
            return;
        }
        let osc = self.audio.create_oscillator();
        let gain = self.audio.create_gain();
        gain.set_gain(self.volume());
        gain.connect_destination();
        osc.set_type(self.wave());
        osc.set_frequency(freq(note, self.tune()));
        osc.connect(&gain);
        osc.start(0.0);
        self.voices.insert(note, Voice { osc, gain });
    }

    fn note_off(&mut self, note: u8) {
        if let Some(v) = self.voices.remove(&note) {
            v.osc.stop(0.0);
        }
    }

    /// Re-apply the current knob values to every sounding voice.
    fn apply_knobs(&mut self) {
        let vol = self.volume();
        let wave = self.wave();
        let tune = self.tune();
        for (note, v) in &self.voices {
            v.gain.set_gain(vol);
            v.osc.set_type(wave);
            v.osc.set_frequency(freq(*note, tune));
        }
    }
}

/// Knob centres and radius for the current surface size.
fn layout(w: u32, h: u32) -> ([(i32, i32); 3], i32) {
    let r = ((h as f32 * 0.22).min(w as f32 * 0.13)).max(10.0) as i32;
    let cy = (h as f32 * 0.36) as i32;
    let centers = [
        ((w as f32 * 0.2) as i32, cy),
        ((w as f32 * 0.5) as i32, cy),
        ((w as f32 * 0.8) as i32, cy),
    ];
    (centers, r)
}

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

/// Draw a knob: a dark body, a coloured ring, and an indicator pointing from the
/// value (270° sweep, straight up = mid).
fn draw_knob(buf: &mut [u8], w: u32, h: u32, cx: i32, cy: i32, r: i32, k: &Knob) {
    disc(buf, w, h, cx, cy, r, [30, 30, 38]);
    ring(buf, w, h, cx, cy, r, 2, k.color);
    let theta = (k.norm() - 0.5) * 1.5 * PI;
    let len = r as f32 * 0.8;
    let tx = cx + (len * theta.sin()) as i32;
    let ty = cy - (len * theta.cos()) as i32;
    line(buf, w, h, cx, cy, tx, ty, k.color);
    disc(buf, w, h, tx, ty, 2, k.color);
}

struct Component;

impl Guest for Component {
    fn run() {
        let surface = Surface::new(CreateDesc {
            width: Some(380),
            height: Some(200),
        });
        let ctx = GfxContext::new();
        surface.connect_graphics_context(&ctx);
        let device = Device::new();
        device.connect_graphics_context(&ctx);
        let frame = surface.subscribe_frame();
        let input = Input::new();

        let mut synth = Synth::new();
        // Knob drag state (across frames): which knob, and the drag anchor.
        let mut grab: Option<usize> = None;
        let mut start_y = 0.0f32;
        let mut start_norm = 0.0f32;

        loop {
            frame.block();
            let _ = surface.get_frame();
            let w = surface.width().max(1);
            let h = surface.height().max(1);
            let (centers, r) = layout(w, h);

            // Incoming MIDI: note-on (status 0x90, vel>0) / note-off (0x80, or
            // 0x90 vel 0).
            while let Some(msg) = input.receive() {
                if msg.len() >= 3 {
                    let status = msg[0] & 0xF0;
                    let note = msg[1];
                    let vel = msg[2];
                    match status {
                        0x90 if vel > 0 => synth.note_on(note),
                        0x80 | 0x90 => synth.note_off(note),
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
                        start_norm = synth.knobs[i].norm();
                        break;
                    }
                }
            }
            while let Some(ev) = surface.get_pointer_move() {
                if let Some(i) = grab {
                    let n = start_norm + (start_y - ev.y as f32) / 160.0;
                    synth.knobs[i].set_norm(n);
                    synth.apply_knobs();
                }
            }
            while surface.get_pointer_up().is_some() {
                grab = None;
            }

            // Paint the panel: background, knobs, and a preview of the waveform.
            let buffer = Buffer::from_graphics_buffer(ctx.get_current_buffer());
            let mut pixels = vec![0u8; (w * h * 4) as usize];
            for px in pixels.chunks_exact_mut(4) {
                px.copy_from_slice(&[22, 22, 28, 255]);
            }
            for (i, &(cx, cy)) in centers.iter().enumerate() {
                draw_knob(&mut pixels, w, h, cx, cy, r, &synth.knobs[i]);
            }

            // Waveform preview strip along the bottom, in the wave knob colour.
            let wave_idx = synth.knobs[WAVE].value.round() as i32;
            let color = synth.knobs[WAVE].color;
            let strip_h = (h as f32 * 0.2) as i32;
            let mid = h as i32 - strip_h / 2 - 6;
            let amp = (strip_h / 2 - 2).max(1) as f32;
            for x in 0..w as i32 {
                let phase = ((x as f32 / w as f32) * 2.0) % 1.0;
                let s = wave_sample(wave_idx, phase);
                let y = mid - (s * amp) as i32;
                put(&mut pixels, w, h, x, y, color);
                put(&mut pixels, w, h, x, y + 1, color);
            }

            buffer.set(&pixels);
            ctx.present();
        }
    }
}

bindings::export!(Component with_types_in bindings);
