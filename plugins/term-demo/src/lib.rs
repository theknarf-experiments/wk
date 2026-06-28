#[allow(warnings)]
mod bindings;

use std::io::{Read, Write};

use bindings::Guest;

/// Parser state for incoming escape sequences (so arrow keys etc. don't echo).
enum EscState {
    Normal,
    Esc,
    Csi,
}

struct Component;

impl Guest for Component {
    fn run() {
        let mut out = std::io::stdout();
        let stdin = std::io::stdin();

        // Clear the screen, draw a banner and a prompt. `\r\n` because there's
        // no line discipline translating `\n` to a carriage return.
        let _ = write!(
            out,
            "\x1b[2J\x1b[H\
             \x1b[1;38;5;81m  wk terminal \x1b[0m\x1b[38;5;245m— a wasm guest in a VT window\x1b[0m\r\n\
             \x1b[2m  type text · Enter submits · Backspace edits · Ctrl-C clears\x1b[0m\r\n\r\n\
             \x1b[32m> \x1b[0m"
        );
        let _ = out.flush();

        let mut line: Vec<u8> = Vec::new();
        let mut esc = EscState::Normal;
        let mut byte = [0u8; 1];

        loop {
            match stdin.lock().read(&mut byte) {
                Ok(0) | Err(_) => break, // stdin closed: exit cleanly
                Ok(_) => {}
            }
            let b = byte[0];

            // Swallow escape sequences (arrow keys, etc.).
            match esc {
                EscState::Esc => {
                    esc = if b == b'[' {
                        EscState::Csi
                    } else {
                        EscState::Normal
                    };
                    continue;
                }
                EscState::Csi => {
                    // A CSI ends at a byte in the range 0x40..=0x7e.
                    if (0x40..=0x7e).contains(&b) {
                        esc = EscState::Normal;
                    }
                    continue;
                }
                EscState::Normal => {}
            }

            match b {
                0x1b => esc = EscState::Esc,
                b'\r' | b'\n' => {
                    let text = String::from_utf8_lossy(&line).into_owned();
                    let _ = write!(out, "\r\n\x1b[33m  » {text}\x1b[0m\r\n\x1b[32m> \x1b[0m");
                    line.clear();
                }
                0x7f | 0x08 => {
                    if line.pop().is_some() {
                        // Move left, overwrite with a space, move left again.
                        let _ = write!(out, "\x08 \x08");
                    }
                }
                0x03 => {
                    // Ctrl-C: clear the current line.
                    line.clear();
                    let _ = write!(out, "\r\x1b[2K\x1b[32m> \x1b[0m");
                }
                _ => {
                    // Echo printable ASCII; ignore other control bytes.
                    if (0x20..0x7f).contains(&b) {
                        line.push(b);
                        let _ = out.write_all(&[b]);
                    }
                }
            }
            let _ = out.flush();
        }
    }
}

bindings::export!(Component with_types_in bindings);
