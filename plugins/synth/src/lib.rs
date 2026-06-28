#[allow(warnings)]
mod bindings;

use std::collections::HashMap;
use std::f32::consts::PI;

use bindings::wasi::frame_buffer::frame_buffer::{Buffer, Device};
use bindings::wasi::graphics_context::graphics_context::Context as GfxContext;
use bindings::wasi::surface::surface::{CreateDesc, Surface};
use bindings::wk::midi::midi::Input;
use bindings::wk::webaudio::audio::{
    BiquadFilter, Context as Audio, FilterType, Gain, Oscillator, OscillatorType,
};
use bindings::Guest;

// Knob indices.
const VOL: usize = 0;
const WAVE: usize = 1;
const TUNE: usize = 2;
const CUT: usize = 3;
const RES: usize = 4;
const ATK: usize = 5;
const REL: usize = 6;
const NUM_KNOBS: usize = 7;

/// Unison spread of the two oscillators per voice, in cents (fixed).
const UNISON_CENTS: f32 = 7.0;

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
/// mini waveform drawn on the wave knob).
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

/// A knob: a value in `[min, max]` (mapped linearly, or logarithmically for
/// frequency/time controls) with a label and colour.
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

/// A sounding note: two detuned oscillators -> low-pass filter -> envelope gain
/// -> speakers. `release_end` marks when the release ramp finishes so the voice
/// can be reaped.
struct Voice {
    osc_a: Oscillator,
    osc_b: Oscillator,
    filter: BiquadFilter,
    gain: Gain,
    release_end: Option<f64>,
}

/// The synth: a bank of voices keyed by MIDI note, plus knobs whose values are
/// applied live to every sounding voice.
struct Synth {
    audio: Audio,
    voices: HashMap<u8, Voice>,
    knobs: [Knob; NUM_KNOBS],
}

impl Synth {
    fn new() -> Self {
        Synth {
            audio: Audio::new(),
            voices: HashMap::new(),
            knobs: [
                Knob {
                    label: "VOL",
                    value: 0.3,
                    min: 0.0,
                    max: 1.0,
                    log: false,
                    color: [90, 150, 240],
                },
                Knob {
                    label: "WAVE",
                    value: 2.0,
                    min: 0.0,
                    max: 3.0,
                    log: false,
                    color: [90, 230, 160],
                },
                Knob {
                    label: "TUNE",
                    value: 0.0,
                    min: -12.0,
                    max: 12.0,
                    log: false,
                    color: [240, 170, 80],
                },
                Knob {
                    label: "CUT",
                    value: 1800.0,
                    min: 80.0,
                    max: 10000.0,
                    log: true,
                    color: [200, 130, 240],
                },
                Knob {
                    label: "RES",
                    value: 3.0,
                    min: 0.1,
                    max: 18.0,
                    log: false,
                    color: [240, 110, 140],
                },
                Knob {
                    label: "ATK",
                    value: 0.02,
                    min: 0.003,
                    max: 1.5,
                    log: true,
                    color: [110, 210, 230],
                },
                Knob {
                    label: "REL",
                    value: 0.35,
                    min: 0.02,
                    max: 2.5,
                    log: true,
                    color: [230, 210, 100],
                },
            ],
        }
    }

    fn note_on(&mut self, note: u8) {
        let peak = self.knobs[VOL].value;
        let attack = self.knobs[ATK].value;
        if let Some(v) = self.voices.get_mut(&note) {
            // Retrigger a still-releasing voice instead of stacking a new one.
            v.release_end = None;
            v.gain.ramp_to(peak, attack);
            return;
        }

        let osc_a = self.audio.create_oscillator();
        let osc_b = self.audio.create_oscillator();
        let filter = self.audio.create_biquad_filter();
        let gain = self.audio.create_gain();

        filter.set_type(FilterType::Lowpass);
        filter.set_frequency(self.knobs[CUT].value);
        filter.set_q(self.knobs[RES].value);
        filter.connect(&gain);
        gain.set_gain(0.0);
        gain.connect_destination();

        let f = freq(note, self.knobs[TUNE].value);
        let wave = osc_type(self.knobs[WAVE].value);
        for (osc, sign) in [(&osc_a, -1.0f32), (&osc_b, 1.0)] {
            osc.set_type(wave);
            osc.set_frequency(f);
            osc.set_detune(sign * UNISON_CENTS);
            osc.connect_filter(&filter);
            osc.start(0.0);
        }
        // Attack: ramp from silence to the volume peak.
        gain.ramp_to(peak, attack);

        self.voices.insert(
            note,
            Voice {
                osc_a,
                osc_b,
                filter,
                gain,
                release_end: None,
            },
        );
    }

    fn note_off(&mut self, note: u8) {
        let release = self.knobs[REL].value;
        let end = self.audio.current_time() + release as f64;
        if let Some(v) = self.voices.get_mut(&note) {
            v.gain.ramp_to(0.0, release);
            v.release_end = Some(end);
        }
    }

    /// Reap voices whose release ramp has finished.
    fn reap(&mut self) {
        let now = self.audio.current_time();
        self.voices.retain(|_, v| match v.release_end {
            Some(end) if now >= end => {
                v.osc_a.stop(0.0);
                v.osc_b.stop(0.0);
                false
            }
            _ => true,
        });
    }

    /// Re-apply live-tweakable knob values to every sounding voice.
    fn apply_knobs(&mut self) {
        let wave = osc_type(self.knobs[WAVE].value);
        let tune = self.knobs[TUNE].value;
        let cut = self.knobs[CUT].value;
        let res = self.knobs[RES].value;
        let vol = self.knobs[VOL].value;
        for (note, v) in &self.voices {
            let f = freq(*note, tune);
            v.osc_a.set_type(wave);
            v.osc_b.set_type(wave);
            v.osc_a.set_frequency(f);
            v.osc_b.set_frequency(f);
            v.filter.set_frequency(cut);
            v.filter.set_q(res);
            // Volume tracks the sustaining level; don't fight an active release.
            if v.release_end.is_none() {
                v.gain.set_gain(vol);
            }
        }
    }
}

/// Knob centres and radius for the current surface size (two rows: 4 then 3).
fn layout(w: u32, h: u32) -> ([(i32, i32); NUM_KNOBS], i32) {
    let cw = w as f32 / 4.0;
    let r = ((cw * 0.3).min(h as f32 * 0.16)).max(8.0) as i32;
    let row_y = [h as f32 * 0.32, h as f32 * 0.72];
    let mut centers = [(0, 0); NUM_KNOBS];
    for (i, c) in centers.iter_mut().enumerate() {
        let col = (i % 4) as f32;
        let row = i / 4;
        *c = ((cw * (col + 0.5)) as i32, row_y[row] as i32);
    }
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

/// A 3x5 bitmap glyph: 5 rows, each holding 3 bits (4=left, 2=mid, 1=right).
fn glyph(c: char) -> [u8; 5] {
    match c {
        'A' => [0b010, 0b101, 0b111, 0b101, 0b101],
        'C' => [0b111, 0b100, 0b100, 0b100, 0b111],
        'E' => [0b111, 0b100, 0b111, 0b100, 0b111],
        'K' => [0b101, 0b101, 0b110, 0b101, 0b101],
        'L' => [0b100, 0b100, 0b100, 0b100, 0b111],
        'N' => [0b101, 0b111, 0b111, 0b111, 0b101],
        'O' => [0b111, 0b101, 0b101, 0b101, 0b111],
        'R' => [0b110, 0b101, 0b110, 0b101, 0b101],
        'S' => [0b111, 0b100, 0b111, 0b001, 0b111],
        'T' => [0b111, 0b010, 0b010, 0b010, 0b010],
        'U' => [0b101, 0b101, 0b101, 0b101, 0b111],
        'V' => [0b101, 0b101, 0b101, 0b101, 0b010],
        'W' => [0b101, 0b101, 0b101, 0b111, 0b101],
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
                            put(
                                buf,
                                w,
                                h,
                                cx + col * scale + sx,
                                y + row as i32 * scale + sy,
                                c,
                            );
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

/// Draw a knob: dark body, coloured ring, an indicator from the value (270°
/// sweep, straight up = mid), and a centred label below.
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

struct Component;

impl Guest for Component {
    fn run() {
        let surface = Surface::new(CreateDesc {
            width: Some(420),
            height: Some(240),
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

            synth.reap();

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

            // Paint the panel: background, then each knob with its label.
            let buffer = Buffer::from_graphics_buffer(ctx.get_current_buffer());
            let mut pixels = vec![0u8; (w * h * 4) as usize];
            for px in pixels.chunks_exact_mut(4) {
                px.copy_from_slice(&[22, 22, 28, 255]);
            }
            for (i, &(cx, cy)) in centers.iter().enumerate() {
                draw_knob(&mut pixels, w, h, cx, cy, r, &synth.knobs[i]);
            }

            // A mini view of the current waveform across the WAVE knob body.
            let (wcx, wcy) = centers[WAVE];
            let wave_idx = synth.knobs[WAVE].value.round() as i32;
            let span = (r as f32 * 0.6) as i32;
            let amp = r as f32 * 0.32;
            let mut prev: Option<(i32, i32)> = None;
            for dx in -span..=span {
                let phase = ((dx + span) as f32 / (2 * span) as f32) * 2.0 % 1.0;
                let y = wcy - (wave_sample(wave_idx, phase) * amp) as i32;
                let x = wcx + dx;
                if let Some((px, py)) = prev {
                    line(&mut pixels, w, h, px, py, x, y, [225, 225, 230]);
                }
                prev = Some((x, y));
            }

            buffer.set(&pixels);
            ctx.present();
        }
    }
}

bindings::export!(Component with_types_in bindings);
