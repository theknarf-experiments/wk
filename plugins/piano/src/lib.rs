#[allow(warnings)]
mod bindings;

use bindings::wasi::frame_buffer::frame_buffer::{Buffer, Device};
use bindings::wasi::graphics_context::graphics_context::Context as GfxContext;
use bindings::wasi::surface::surface::{CreateDesc, Key, Surface};
use bindings::wk::webaudio::audio::{Context as Audio, Gain, Oscillator, OscillatorType};
use bindings::Guest;

/// The 13 chromatic notes from C to C (one octave). White keys are the natural
/// notes; the rest are the black keys.
const WHITE: [usize; 8] = [0, 2, 4, 5, 7, 9, 11, 12];
/// Black keys: (white-boundary the key sits on, semitone). They sit between
/// white keys C-D, D-E, F-G, G-A, A-B.
const BLACK: [(f32, usize); 5] = [(1.0, 1), (2.0, 3), (4.0, 6), (5.0, 8), (6.0, 10)];

/// Equal-temperament frequency of semitone `i` above C4 (MIDI 60).
fn freq(i: usize) -> f32 {
    let midi = 60 + i as i32;
    440.0 * 2.0f32.powf((midi as f32 - 69.0) / 12.0)
}

/// Computer-keyboard piano mapping (FL-Studio style): the home row plays the
/// white keys, the row above the black keys.
fn key_to_note(k: Key) -> Option<usize> {
    Some(match k {
        Key::KeyA => 0,
        Key::KeyW => 1,
        Key::KeyS => 2,
        Key::KeyE => 3,
        Key::KeyD => 4,
        Key::KeyF => 5,
        Key::KeyT => 6,
        Key::KeyG => 7,
        Key::KeyY => 8,
        Key::KeyH => 9,
        Key::KeyU => 10,
        Key::KeyJ => 11,
        Key::KeyK => 12,
        _ => return None,
    })
}

/// A sounding note: oscillator -> gain -> speakers.
struct Voice {
    osc: Oscillator,
    _gain: Gain,
}

/// The synth: ref-counts how many inputs (mouse + keyboard) hold each note so
/// overlapping presses don't cut each other off.
struct Synth {
    audio: Audio,
    held: [u32; 13],
    voices: [Option<Voice>; 13],
}

impl Synth {
    fn new() -> Self {
        Synth {
            audio: Audio::new(),
            held: [0; 13],
            voices: core::array::from_fn(|_| None),
        }
    }

    fn press(&mut self, note: usize) {
        self.held[note] += 1;
        if self.held[note] == 1 {
            let osc = self.audio.create_oscillator();
            let gain = self.audio.create_gain();
            gain.set_gain(0.15);
            gain.connect_destination();
            osc.set_type(OscillatorType::Triangle);
            osc.set_frequency(freq(note));
            osc.connect(&gain);
            osc.start(0.0);
            self.voices[note] = Some(Voice { osc, _gain: gain });
        }
    }

    fn release(&mut self, note: usize) {
        if self.held[note] == 0 {
            return;
        }
        self.held[note] -= 1;
        if self.held[note] == 0 {
            if let Some(v) = self.voices[note].take() {
                v.osc.stop(0.0);
            }
        }
    }
}

/// Which note is under the cursor, given the surface size.
fn hit_test(x: f32, y: f32, w: f32, h: f32) -> usize {
    let white_w = w / 8.0;
    let black_h = h * 0.55;
    let black_w = white_w * 0.6;
    if y < black_h {
        for &(mult, note) in &BLACK {
            let cx = mult * white_w;
            if x >= cx - black_w / 2.0 && x < cx + black_w / 2.0 {
                return note;
            }
        }
    }
    let wi = ((x / white_w) as usize).min(7);
    WHITE[wi]
}

struct Component;

impl Guest for Component {
    fn run() {
        let surface = Surface::new(CreateDesc {
            width: Some(560),
            height: Some(200),
        });
        let ctx = GfxContext::new();
        surface.connect_graphics_context(&ctx);
        let device = Device::new();
        device.connect_graphics_context(&ctx);
        let frame = surface.subscribe_frame();

        let mut synth = Synth::new();
        // Keyboard de-bounce (the host re-sends key-down while a key is held).
        let mut key_held = [false; 13];
        let mut mouse_note: Option<usize> = None;

        loop {
            frame.block();
            let _ = surface.get_frame();
            let w = surface.width().max(1);
            let h = surface.height().max(1);

            // Mouse: press the key under the cursor on down, release on up.
            while let Some(ev) = surface.get_pointer_down() {
                let note = hit_test(ev.x as f32, ev.y as f32, w as f32, h as f32);
                if mouse_note != Some(note) {
                    if let Some(prev) = mouse_note.take() {
                        synth.release(prev);
                    }
                    synth.press(note);
                    mouse_note = Some(note);
                }
            }
            while surface.get_pointer_up().is_some() {
                if let Some(note) = mouse_note.take() {
                    synth.release(note);
                }
            }
            while surface.get_pointer_move().is_some() {}

            // Keyboard: held-set de-bounces auto-repeat into one note on/off.
            while let Some(ev) = surface.get_key_down() {
                if let Some(note) = ev.key.and_then(key_to_note) {
                    if !key_held[note] {
                        key_held[note] = true;
                        synth.press(note);
                    }
                }
            }
            while let Some(ev) = surface.get_key_up() {
                if let Some(note) = ev.key.and_then(key_to_note) {
                    if key_held[note] {
                        key_held[note] = false;
                        synth.release(note);
                    }
                }
            }

            // Paint the keyboard.
            let buffer = Buffer::from_graphics_buffer(ctx.get_current_buffer());
            let mut active = [false; 13];
            for (n, a) in active.iter_mut().enumerate() {
                *a = synth.voices[n].is_some();
            }
            let mut pixels = vec![0u8; (w * h * 4) as usize];
            let white_w = w as f32 / 8.0;
            let black_h = h as f32 * 0.55;
            let black_w = white_w * 0.6;
            for y in 0..h {
                for x in 0..w {
                    let i = ((y * w + x) * 4) as usize;
                    let (fx, fy) = (x as f32, y as f32);

                    // Black key?
                    let mut black = None;
                    if fy < black_h {
                        for &(mult, note) in &BLACK {
                            let cx = mult * white_w;
                            if fx >= cx - black_w / 2.0 && fx < cx + black_w / 2.0 {
                                black = Some(note);
                                break;
                            }
                        }
                    }

                    let (r, g, b) = if let Some(note) = black {
                        if active[note] {
                            (110, 140, 210)
                        } else {
                            (16, 16, 22)
                        }
                    } else {
                        let wi = ((fx / white_w) as usize).min(7);
                        let note = WHITE[wi];
                        let edge = (fx % white_w) < 1.5 || (fx % white_w) > white_w - 1.5;
                        if edge {
                            (60, 60, 70)
                        } else if active[note] {
                            (150, 190, 255)
                        } else {
                            (242, 242, 246)
                        }
                    };
                    pixels[i] = r;
                    pixels[i + 1] = g;
                    pixels[i + 2] = b;
                    pixels[i + 3] = 255;
                }
            }
            buffer.set(&pixels);
            ctx.present();
        }
    }
}

bindings::export!(Component with_types_in bindings);
