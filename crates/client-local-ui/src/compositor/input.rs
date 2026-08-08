//! Keyboard mapping: winit key events → terminal stdin bytes and wasi-gfx key
//! codes. Split out of the compositor so the input translation lives on its own.

use super::*;

/// Encode a key press as the bytes a terminal app expects on stdin. `text` is
/// winit's resolved character(s) for the key (handles shift/layout).
pub(super) fn encode_term_key(
    code: KeyCode,
    text: Option<&str>,
    mods: ModifiersState,
) -> Option<Vec<u8>> {
    use KeyCode as C;
    // Ctrl+letter -> control byte (Ctrl-A = 0x01 ... Ctrl-Z = 0x1a).
    if mods.control_key() {
        if let Some(n) = letter_index(code) {
            return Some(vec![n + 1]);
        }
    }
    Some(match code {
        C::Enter | C::NumpadEnter => vec![b'\r'],
        C::Backspace => vec![0x7f],
        C::Tab => vec![b'\t'],
        C::Escape => vec![0x1b],
        C::ArrowUp => vec![0x1b, b'[', b'A'],
        C::ArrowDown => vec![0x1b, b'[', b'B'],
        C::ArrowRight => vec![0x1b, b'[', b'C'],
        C::ArrowLeft => vec![0x1b, b'[', b'D'],
        C::Home => vec![0x1b, b'[', b'H'],
        C::End => vec![0x1b, b'[', b'F'],
        _ => match text {
            Some(t) if !t.is_empty() => t.as_bytes().to_vec(),
            _ => return None,
        },
    })
}

fn letter_index(code: KeyCode) -> Option<u8> {
    use KeyCode as C;
    let n = match code {
        C::KeyA => 0,
        C::KeyB => 1,
        C::KeyC => 2,
        C::KeyD => 3,
        C::KeyE => 4,
        C::KeyF => 5,
        C::KeyG => 6,
        C::KeyH => 7,
        C::KeyI => 8,
        C::KeyJ => 9,
        C::KeyK => 10,
        C::KeyL => 11,
        C::KeyM => 12,
        C::KeyN => 13,
        C::KeyO => 14,
        C::KeyP => 15,
        C::KeyQ => 16,
        C::KeyR => 17,
        C::KeyS => 18,
        C::KeyT => 19,
        C::KeyU => 20,
        C::KeyV => 21,
        C::KeyW => 22,
        C::KeyX => 23,
        C::KeyY => 24,
        C::KeyZ => 25,
        _ => return None,
    };
    Some(n)
}

/// Map a winit physical key to the wasi-gfx W3C `key` code.
fn map_key(code: KeyCode) -> Option<Key> {
    use KeyCode as C;
    Some(match code {
        C::KeyA => Key::KeyA,
        C::KeyB => Key::KeyB,
        C::KeyC => Key::KeyC,
        C::KeyD => Key::KeyD,
        C::KeyE => Key::KeyE,
        C::KeyF => Key::KeyF,
        C::KeyG => Key::KeyG,
        C::KeyH => Key::KeyH,
        C::KeyI => Key::KeyI,
        C::KeyJ => Key::KeyJ,
        C::KeyK => Key::KeyK,
        C::KeyL => Key::KeyL,
        C::KeyM => Key::KeyM,
        C::KeyN => Key::KeyN,
        C::KeyO => Key::KeyO,
        C::KeyP => Key::KeyP,
        C::KeyQ => Key::KeyQ,
        C::KeyR => Key::KeyR,
        C::KeyS => Key::KeyS,
        C::KeyT => Key::KeyT,
        C::KeyU => Key::KeyU,
        C::KeyV => Key::KeyV,
        C::KeyW => Key::KeyW,
        C::KeyX => Key::KeyX,
        C::KeyY => Key::KeyY,
        C::KeyZ => Key::KeyZ,
        C::Digit0 => Key::Digit0,
        C::Digit1 => Key::Digit1,
        C::Digit2 => Key::Digit2,
        C::Digit3 => Key::Digit3,
        C::Digit4 => Key::Digit4,
        C::Digit5 => Key::Digit5,
        C::Digit6 => Key::Digit6,
        C::Digit7 => Key::Digit7,
        C::Digit8 => Key::Digit8,
        C::Digit9 => Key::Digit9,
        C::ArrowUp => Key::ArrowUp,
        C::ArrowDown => Key::ArrowDown,
        C::ArrowLeft => Key::ArrowLeft,
        C::ArrowRight => Key::ArrowRight,
        C::Space => Key::Space,
        C::Enter => Key::Enter,
        C::Tab => Key::Tab,
        C::Escape => Key::Escape,
        C::Backspace => Key::Backspace,
        C::ShiftLeft => Key::ShiftLeft,
        C::ShiftRight => Key::ShiftRight,
        C::ControlLeft => Key::ControlLeft,
        C::ControlRight => Key::ControlRight,
        C::AltLeft => Key::AltLeft,
        C::AltRight => Key::AltRight,
        C::SuperLeft => Key::MetaLeft,
        C::SuperRight => Key::MetaRight,
        _ => return None,
    })
}

pub(super) fn key_event(code: KeyCode, mods: ModifiersState) -> KeyEvent {
    KeyEvent {
        key: map_key(code),
        text: None,
        alt_key: mods.alt_key(),
        ctrl_key: mods.control_key(),
        meta_key: mods.super_key(),
        shift_key: mods.shift_key(),
    }
}
