#[allow(warnings)]
mod bindings;

use bindings::Guest;
use bindings::wasi::frame_buffer::frame_buffer::{Buffer, Device};
use bindings::wasi::graphics_context::graphics_context::Context as GfxContext;
use bindings::wasi::surface::surface::{CreateDesc, Key, Surface};
use bindings::wk::midi::midi::{Input, Output};

/// Two octaves of keys. White-key semitone offsets (C D E F G A B, twice, plus
/// the closing C); the black keys sit on the white-key boundaries.
const WHITE: [usize; 15] = [0, 2, 4, 5, 7, 9, 11, 12, 14, 16, 17, 19, 21, 23, 24];
/// Black keys: (white-boundary the key straddles, semitone). None on B–C.
const BLACK: [(f32, usize); 10] = [
    (1.0, 1),
    (2.0, 3),
    (4.0, 6),
    (5.0, 8),
    (6.0, 10),
    (8.0, 13),
    (9.0, 15),
    (11.0, 18),
    (12.0, 20),
    (13.0, 22),
];
/// Total keys drawn (two octaves + the closing C).
const NKEYS: usize = 25;
/// Number of white keys.
const NW: usize = 15;

/// Height of the top strip holding the octave-shift buttons.
const CTRL_H: f32 = 24.0;
/// Width of each octave button (left = down, right = up).
const BTN_W: f32 = 48.0;

/// MIDI note number of key `i` above the keyboard's base note.
fn midi_note(base: i32, i: usize) -> u8 {
    (base + i as i32).clamp(0, 127) as u8
}

/// Computer-keyboard piano mapping (FL-Studio style): the home row plays the
/// lower displayed octave's white keys, the row above its black keys. The
/// on-screen buttons shift which octaves the whole keyboard covers.
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

/// A two-octave MIDI keyboard, shiftable by the octave buttons. Ref-counts local
/// presses (mouse + keyboard) so overlapping presses don't send a spurious
/// note-off, emits note-on/off relative to a base MIDI note, and receives MIDI on
/// its input port — lighting up incoming notes and passing them through.
struct Keyboard {
    out: Output,
    input: Input,
    held: [u32; NKEYS],
    ext: [bool; NKEYS],
    /// MIDI note of key 0 (C of the lower displayed octave). Default C4 = 60.
    base: i32,
}

impl Keyboard {
    fn new() -> Self {
        Keyboard {
            out: Output::new(),
            input: Input::new(),
            held: [0; NKEYS],
            ext: [false; NKEYS],
            base: 60,
        }
    }

    fn press(&mut self, note: usize) {
        self.held[note] += 1;
        if self.held[note] == 1 {
            self.out.send(&[0x90, midi_note(self.base, note), 100]);
        }
    }

    fn release(&mut self, note: usize) {
        if self.held[note] == 0 {
            return;
        }
        self.held[note] -= 1;
        if self.held[note] == 0 {
            self.out.send(&[0x80, midi_note(self.base, note), 0]);
        }
    }

    fn active(&self, note: usize) -> bool {
        self.held[note] > 0
    }

    /// Release every held key at the current pitch (so nothing sticks across an
    /// octave shift).
    fn all_off(&mut self) {
        for note in 0..NKEYS {
            if self.held[note] > 0 {
                self.out.send(&[0x80, midi_note(self.base, note), 0]);
                self.held[note] = 0;
            }
        }
    }

    /// Shift the whole keyboard by `delta` octaves, clamped so MIDI stays valid.
    fn shift_octave(&mut self, delta: i32) {
        let new_base = (self.base + delta * 12).clamp(24, 96);
        if new_base != self.base {
            self.all_off();
            self.base = new_base;
        }
    }

    /// Drain MIDI arriving on the input port: light up note-ons within the
    /// displayed range, and pass every message through to the output (MIDI thru).
    ///
    /// Pass-through keeps each message's instant. The piano usually sits in the
    /// middle of a chain — a hardware keyboard on one side, a sequencer
    /// recording on the other — and re-stamping here would throw away when the
    /// key was actually struck and replace it with when this node woke up.
    fn pump_input(&mut self) {
        while let Some(ev) = self.input.receive_event() {
            let msg = &ev.data;
            if msg.len() >= 3 {
                let status = msg[0] & 0xF0;
                let note = msg[1] as i32 - self.base;
                if (0..NKEYS as i32).contains(&note) {
                    let n = note as usize;
                    if status == 0x90 && msg[2] > 0 {
                        self.ext[n] = true;
                    } else if status == 0x80 || (status == 0x90 && msg[2] == 0) {
                        self.ext[n] = false;
                    }
                }
                // All sound off / all notes off: the host sends these when a
                // MIDI cable is unplugged, so clear the lit keys too.
                if status == 0xB0 && matches!(msg[1], 120 | 123) {
                    self.ext = [false; NKEYS];
                }
            }
            self.out.send_at(msg, ev.time);
        }
    }
}

/// Which note is under the cursor in the keyboard area (`y` measured from the
/// top of the keys, i.e. below the control strip).
fn hit_test(x: f32, y: f32, w: f32, kb_h: f32) -> usize {
    let white_w = w / NW as f32;
    let black_h = kb_h * 0.55;
    let black_w = white_w * 0.6;
    if y < black_h {
        for &(mult, note) in &BLACK {
            let cx = mult * white_w;
            if x >= cx - black_w / 2.0 && x < cx + black_w / 2.0 {
                return note;
            }
        }
    }
    let wi = ((x / white_w) as usize).min(NW - 1);
    WHITE[wi]
}

struct Component;

impl Guest for Component {
    fn run() {
        let surface = Surface::new(CreateDesc {
            width: Some(660),
            height: Some(220),
        });
        let ctx = GfxContext::new();
        surface.connect_graphics_context(&ctx);
        let device = Device::new();
        device.connect_graphics_context(&ctx);
        let frame = surface.subscribe_frame();

        let mut keyboard = Keyboard::new();
        // Keyboard de-bounce (the host re-sends key-down while a key is held).
        let mut key_held = [false; NKEYS];
        let mut mouse_note: Option<usize> = None;

        loop {
            frame.block();
            let _ = surface.get_frame();
            let w = surface.width().max(1);
            let h = surface.height().max(1);
            let wf = w as f32;
            let hf = h as f32;
            let kb_h = (hf - CTRL_H).max(1.0);

            // Mouse: the top strip holds the octave buttons; below it, the keys.
            while let Some(ev) = surface.get_pointer_down() {
                let (px, py) = (ev.x as f32, ev.y as f32);
                if py < CTRL_H {
                    if px < BTN_W {
                        keyboard.shift_octave(-1);
                    } else if px >= wf - BTN_W {
                        keyboard.shift_octave(1);
                    }
                    // A click on the strip shouldn't leave a key mouse-held.
                    if let Some(prev) = mouse_note.take() {
                        keyboard.release(prev);
                    }
                    continue;
                }
                let note = hit_test(px, py - CTRL_H, wf, kb_h);
                if mouse_note != Some(note) {
                    if let Some(prev) = mouse_note.take() {
                        keyboard.release(prev);
                    }
                    keyboard.press(note);
                    mouse_note = Some(note);
                }
            }
            while surface.get_pointer_up().is_some() {
                if let Some(note) = mouse_note.take() {
                    keyboard.release(note);
                }
            }
            while surface.get_pointer_move().is_some() {}

            // Keyboard: held-set de-bounces auto-repeat into one note on/off.
            while let Some(ev) = surface.get_key_down() {
                if let Some(note) = ev.key.and_then(key_to_note) {
                    if !key_held[note] {
                        key_held[note] = true;
                        keyboard.press(note);
                    }
                }
            }
            while let Some(ev) = surface.get_key_up() {
                if let Some(note) = ev.key.and_then(key_to_note) {
                    if key_held[note] {
                        key_held[note] = false;
                        keyboard.release(note);
                    }
                }
            }

            // MIDI in from a wired source (e.g. a hardware keyboard): highlight +
            // pass through.
            keyboard.pump_input();

            // Paint the control strip and the two-octave keyboard.
            let buffer = Buffer::from_graphics_buffer(ctx.get_current_buffer());
            let mut active = [false; NKEYS];
            let mut ext = [false; NKEYS];
            for n in 0..NKEYS {
                active[n] = keyboard.active(n);
                ext[n] = keyboard.ext[n];
            }
            let mut pixels = vec![0u8; (w * h * 4) as usize];
            let white_w = wf / NW as f32;
            let black_h = kb_h * 0.55;
            let black_w = white_w * 0.6;
            // Octave-button glyph geometry.
            let cy = CTRL_H / 2.0;
            let dcx = BTN_W / 2.0; // down (−) button centre
            let ucx = wf - BTN_W / 2.0; // up (+) button centre
            let gr = 7.0; // glyph half-extent
            for y in 0..h {
                for x in 0..w {
                    let i = ((y * w + x) * 4) as usize;
                    let (fx, fy) = (x as f32, y as f32);

                    let (r, g, b) = if fy < CTRL_H {
                        // Control strip: octave-down button (left), up (right).
                        let on_down = fx < BTN_W;
                        let on_up = fx >= wf - BTN_W;
                        let minus = on_down && (fy - cy).abs() < 1.6 && (fx - dcx).abs() < gr;
                        let plus = on_up
                            && (((fy - cy).abs() < 1.6 && (fx - ucx).abs() < gr)
                                || ((fx - ucx).abs() < 1.6 && (fy - cy).abs() < gr));
                        if minus || plus {
                            (210, 215, 225)
                        } else if on_down || on_up {
                            (46, 48, 58)
                        } else {
                            (28, 28, 34)
                        }
                    } else {
                        // Keyboard area. Green = an upstream (hardware) note;
                        // blue = a local mouse/keyboard press.
                        let ky = fy - CTRL_H;
                        let mut black = None;
                        if ky < black_h {
                            for &(mult, note) in &BLACK {
                                let cx = mult * white_w;
                                if fx >= cx - black_w / 2.0 && fx < cx + black_w / 2.0 {
                                    black = Some(note);
                                    break;
                                }
                            }
                        }
                        if let Some(note) = black {
                            if ext[note] {
                                (110, 210, 140)
                            } else if active[note] {
                                (110, 140, 210)
                            } else {
                                (16, 16, 22)
                            }
                        } else {
                            let wi = ((fx / white_w) as usize).min(NW - 1);
                            let note = WHITE[wi];
                            let edge = (fx % white_w) < 1.5 || (fx % white_w) > white_w - 1.5;
                            if edge {
                                (60, 60, 70)
                            } else if ext[note] {
                                (150, 255, 180)
                            } else if active[note] {
                                (150, 190, 255)
                            } else {
                                (242, 242, 246)
                            }
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
