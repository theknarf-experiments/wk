// A small polyphonic synth, as a wk:clap plugin. No `run` loop and no host Web
// Audio graph: it synthesises its own samples in `process` (two detuned
// oscillators -> resonant TPT lowpass -> AR envelope, per voice) from the notes
// arriving on its wk:clap note input, and paints its knob panel in `gui-render`.
// Knob settings persist via wk:options — a wk:clap node freely using another wk
// interface. Combines wasi-gfx (knob UI) with wk:clap audio + note ports.
#[allow(warnings)]
mod bindings;

use std::collections::HashMap;
use std::f32::consts::PI;

use bindings::exports::wk::clap::plugins::{
    AudioBuffer, AudioPortInfo, Descriptor, Event, Guest, GuestPlugin, NotePortInfo, ParamInfo,
    Plugin, ProcessResult, ProcessStatus, Supported, Transport,
};
use bindings::wasi::frame_buffer::frame_buffer::{Buffer, Device};
use bindings::wasi::graphics_context::graphics_context::Context as GfxContext;
use bindings::wasi::surface::surface::{CreateDesc, Surface};
use bindings::wk::clap::types::{AudioPortFlags, NoteDialects};

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

/// One cycle of waveform `idx` at `phase` in 0..1, returning -1..1 (used both for
/// synthesis and for the mini waveform drawn on the wave knob).
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
#[derive(Clone, Copy)]
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

#[derive(PartialEq, Clone, Copy)]
enum Stage {
    Attack,
    Sustain,
    Release,
}

/// A sounding note: two detuned oscillators summed into a resonant lowpass (TPT
/// state-variable filter), shaped by an AR envelope. Phases and filter state
/// integrate per sample; the voice is reaped when its release ramp reaches zero.
struct Voice {
    note: u8,
    phase_a: f32,
    phase_b: f32,
    env: f32,
    stage: Stage,
    // TPT SVF integrator state.
    ic1: f32,
    ic2: f32,
}

impl Voice {
    fn new(note: u8) -> Self {
        Voice {
            note,
            phase_a: 0.0,
            phase_b: 0.0,
            env: 0.0,
            stage: Stage::Attack,
            ic1: 0.0,
            ic2: 0.0,
        }
    }
}

/// The synth: a bank of voices keyed by MIDI note, plus knobs whose values are
/// read live each block by the DSP.
struct Synth {
    // Lazily created on the first `gui-render` (the host composites the surface).
    surface: Option<Surface>,
    ctx: Option<GfxContext>,
    device: Option<Device>,
    px: Vec<u8>,
    loaded: bool,
    // Knob drag state (across frames): which knob, plus the drag anchor.
    grab: Option<usize>,
    start_y: f32,
    start_norm: f32,

    sample_rate: f32,
    voices: HashMap<u8, Voice>,
    knobs: [Knob; NUM_KNOBS],
}

impl Synth {
    fn new() -> Self {
        Synth {
            surface: None,
            ctx: None,
            device: None,
            px: Vec::new(),
            loaded: false,
            grab: None,
            start_y: 0.0,
            start_norm: 0.0,
            sample_rate: 48_000.0,
            voices: HashMap::new(),
            knobs: [
                Knob { label: "VOL", value: 0.3, min: 0.0, max: 1.0, log: false, color: [90, 150, 240] },
                Knob { label: "WAVE", value: 2.0, min: 0.0, max: 3.0, log: false, color: [90, 230, 160] },
                Knob { label: "TUNE", value: 0.0, min: -12.0, max: 12.0, log: false, color: [240, 170, 80] },
                Knob { label: "CUT", value: 1800.0, min: 80.0, max: 10000.0, log: true, color: [200, 130, 240] },
                Knob { label: "RES", value: 3.0, min: 0.1, max: 18.0, log: false, color: [240, 110, 140] },
                Knob { label: "ATK", value: 0.02, min: 0.003, max: 1.5, log: true, color: [110, 210, 230] },
                Knob { label: "REL", value: 0.35, min: 0.02, max: 2.5, log: true, color: [230, 210, 100] },
            ],
        }
    }

    fn note_on(&mut self, note: u8) {
        // Retrigger a still-sounding voice instead of stacking a new one.
        let v = self.voices.entry(note).or_insert_with(|| Voice::new(note));
        v.stage = Stage::Attack;
    }

    fn note_off(&mut self, note: u8) {
        if let Some(v) = self.voices.get_mut(&note) {
            v.stage = Stage::Release;
        }
    }

    /// Apply host-saved option values (knob settings) if they match our layout.
    fn apply_options(&mut self, vals: &[f32]) {
        if vals.len() == NUM_KNOBS {
            for (k, &v) in self.knobs.iter_mut().zip(vals) {
                k.value = v.clamp(k.min, k.max);
            }
        }
    }

    /// The current knob values, in knob order, to persist via the host.
    fn options(&self) -> Vec<f32> {
        self.knobs.iter().map(|k| k.value).collect()
    }

    /// Synthesise `frames` samples into a mono buffer (the caller mirrors it to
    /// both stereo channels).
    fn render(&mut self, frames: usize) -> Vec<f32> {
        let sr = self.sample_rate;
        let vol = self.knobs[VOL].value;
        let wave = self.knobs[WAVE].value.round() as i32;
        let tune = self.knobs[TUNE].value;
        let atk = (sr * self.knobs[ATK].value).max(1.0);
        let rel = (sr * self.knobs[REL].value).max(1.0);
        // TPT state-variable filter coefficients (shared cutoff/res across voices).
        let fc = self.knobs[CUT].value.min(sr * 0.45);
        let g = (PI * fc / sr).tan();
        // Map resonance (Q ~0.1..18) to the SVF damping term k = 1/Q.
        let k = (1.0 / self.knobs[RES].value).clamp(0.05, 2.0);
        let a1 = 1.0 / (1.0 + g * (g + k));
        let a2 = g * a1;
        let a3 = g * a2;
        // Per-voice oscillator detune factors (±UNISON_CENTS).
        let det = 2.0f32.powf(UNISON_CENTS / 1200.0);

        let mut out = vec![0.0f32; frames];
        for s in out.iter_mut() {
            let mut mix = 0.0f32;
            for v in self.voices.values_mut() {
                match v.stage {
                    Stage::Attack => {
                        v.env += 1.0 / atk;
                        if v.env >= 1.0 {
                            v.env = 1.0;
                            v.stage = Stage::Sustain;
                        }
                    }
                    Stage::Release => {
                        v.env -= 1.0 / rel;
                        if v.env <= 0.0 {
                            v.env = 0.0;
                            continue;
                        }
                    }
                    Stage::Sustain => {}
                }
                let f = freq(v.note, tune);
                let osc = 0.5 * (wave_sample(wave, v.phase_a) + wave_sample(wave, v.phase_b));
                v.phase_a += f / det / sr;
                if v.phase_a >= 1.0 {
                    v.phase_a -= 1.0;
                }
                v.phase_b += f * det / sr;
                if v.phase_b >= 1.0 {
                    v.phase_b -= 1.0;
                }
                // TPT SVF lowpass.
                let v3 = osc - v.ic2;
                let v1 = a1 * v.ic1 + a2 * v3;
                let v2 = v.ic2 + a2 * v.ic1 + a3 * v3;
                v.ic1 = 2.0 * v1 - v.ic1;
                v.ic2 = 2.0 * v2 - v.ic2;
                mix += v2 * v.env;
            }
            let mut y = mix * vol;
            if !y.is_finite() {
                y = 0.0;
            }
            *s = y.clamp(-1.0, 1.0);
        }
        // Reap finished-release voices.
        self.voices
            .retain(|_, v| !(v.stage == Stage::Release && v.env <= 0.0));
        out
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

/// A wk:clap plugin instance.
struct SynthPlugin {
    st: std::cell::RefCell<Synth>,
}

impl SynthPlugin {
    fn new() -> Self {
        SynthPlugin {
            st: std::cell::RefCell::new(Synth::new()),
        }
    }
}

impl GuestPlugin for SynthPlugin {
    fn init(&self) -> bool {
        let mut st = self.st.borrow_mut();
        if !st.loaded {
            let saved = bindings::wk::options::options::load();
            st.apply_options(&saved);
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
    fn reset(&self) {
        self.st.borrow_mut().voices.clear();
    }
    fn on_main_thread(&self) {}

    /// Trigger/release voices from incoming MIDI, then synthesise a stereo block.
    fn process(
        &self,
        _steady_time: i64,
        frames: u32,
        _transport: Option<Transport>,
        in_events: Vec<Event>,
        _audio_in: Vec<AudioBuffer>,
    ) -> ProcessResult {
        let mut st = self.st.borrow_mut();
        for ev in &in_events {
            if let Event::Midi(m) = ev {
                let (status, note, vel) = m.data;
                match status & 0xF0 {
                    0x90 if vel > 0 => st.note_on(note),
                    0x80 | 0x90 => st.note_off(note),
                    _ => {}
                }
            }
        }
        let mono = st.render(frames as usize);
        // One stereo output port: both channels carry the mono mix.
        ProcessResult {
            status: ProcessStatus::Continue,
            audio_out: vec![vec![mono.clone(), mono]],
            out_events: Vec::new(),
        }
    }

    fn features(&self) -> Supported {
        Supported::AUDIO_PORTS | Supported::NOTE_PORTS
    }

    // ---- params (none exposed; the GUI knobs drive the DSP directly) ----
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

    // ---- audio ports (one stereo output) ----
    fn audio_port_count(&self, is_input: bool) -> u32 {
        if is_input { 0 } else { 1 }
    }
    fn audio_port_info_at(&self, index: u32, is_input: bool) -> Option<AudioPortInfo> {
        if is_input || index != 0 {
            return None;
        }
        Some(AudioPortInfo {
            id: 0,
            name: "Out".into(),
            channel_count: 2,
            flag_set: AudioPortFlags::IS_MAIN,
            port_type: "stereo".into(),
        })
    }

    // ---- note ports (one input) ----
    fn note_port_count(&self, is_input: bool) -> u32 {
        if is_input { 1 } else { 0 }
    }
    fn note_port_info_at(&self, index: u32, is_input: bool) -> Option<NotePortInfo> {
        if !is_input || index != 0 {
            return None;
        }
        Some(NotePortInfo {
            id: 0,
            name: "In".into(),
            supported_dialects: NoteDialects::MIDI,
            preferred_dialect: NoteDialects::MIDI,
        })
    }

    // ---- state (knobs persist via wk:options instead) ----
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

    /// Paint the knob panel and handle knob drags.
    fn gui_render(&self) {
        let mut st = self.st.borrow_mut();
        if st.surface.is_none() {
            let surface = Surface::new(CreateDesc {
                width: Some(420),
                height: Some(240),
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
        let (centers, r) = layout(w, h);

        // Mouse: grab a knob on press, turn it by dragging vertically.
        while let Some(ev) = st.surface.as_ref().unwrap().get_pointer_down() {
            let (mx, my) = (ev.x as f32, ev.y as f32);
            for (i, &(cx, cy)) in centers.iter().enumerate() {
                let (dx, dy) = (mx - cx as f32, my - cy as f32);
                if dx * dx + dy * dy <= ((r + 6) as f32).powi(2) {
                    st.grab = Some(i);
                    st.start_y = my;
                    st.start_norm = st.knobs[i].norm();
                    break;
                }
            }
        }
        while let Some(ev) = st.surface.as_ref().unwrap().get_pointer_move() {
            if let Some(i) = st.grab {
                let n = st.start_norm + (st.start_y - ev.y as f32) / 160.0;
                st.knobs[i].set_norm(n);
            }
        }
        let mut released = false;
        while st.surface.as_ref().unwrap().get_pointer_up().is_some() {
            st.grab = None;
            released = true;
        }
        // Persist knob settings when a drag ends (the host saves them per node).
        if released {
            bindings::wk::options::options::store(&st.options());
        }

        // Paint the panel: background, then each knob with its label.
        let n = (w * h * 4) as usize;
        st.px.clear();
        st.px.resize(n, 0);
        for px in st.px.chunks_exact_mut(4) {
            px.copy_from_slice(&[22, 22, 28, 255]);
        }
        for (i, &(cx, cy)) in centers.iter().enumerate() {
            let k = st.knobs[i];
            draw_knob(&mut st.px, w, h, cx, cy, r, &k);
        }

        // A mini view of the current waveform across the WAVE knob body.
        let (wcx, wcy) = centers[WAVE];
        let wave_idx = st.knobs[WAVE].value.round() as i32;
        let span = (r as f32 * 0.6) as i32;
        let amp = r as f32 * 0.32;
        let mut prev: Option<(i32, i32)> = None;
        for dx in -span..=span {
            let phase = ((dx + span) as f32 / (2 * span) as f32) * 2.0 % 1.0;
            let y = wcy - (wave_sample(wave_idx, phase) * amp) as i32;
            let x = wcx + dx;
            if let Some((px, py)) = prev {
                line(&mut st.px, w, h, px, py, x, y, [225, 225, 230]);
            }
            prev = Some((x, y));
        }

        let ctx = st.ctx.as_ref().unwrap();
        let buffer = Buffer::from_graphics_buffer(ctx.get_current_buffer());
        buffer.set(&st.px);
        ctx.present();
    }
}

struct Component;

impl Guest for Component {
    type Plugin = SynthPlugin;

    fn count() -> u32 {
        1
    }

    fn get(index: u32) -> Option<Descriptor> {
        if index != 0 {
            return None;
        }
        Some(Descriptor {
            id: "wk.synth".into(),
            name: "Synth".into(),
            vendor: "wk".into(),
            version: "1.0.0".into(),
            features: vec!["instrument".into(), "synthesizer".into(), "stereo".into()],
        })
    }

    fn create(plugin_id: String) -> Option<Plugin> {
        if plugin_id != "wk.synth" {
            return None;
        }
        Some(Plugin::new(SynthPlugin::new()))
    }
}

bindings::export!(Component with_types_in bindings);
