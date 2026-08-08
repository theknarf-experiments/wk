//! `wk attach <node>` — stream a running terminal node's I/O over the socket,
//! the way `docker attach` streams a container's. The node keeps running; the
//! UI treats it as detached while the CLI owns it.
//!
//! Interactive (a tty on stdin): the local terminal goes raw, keystrokes stream
//! to the node, its output streams back, and **Ctrl-P Ctrl-Q** detaches without
//! stopping it. Non-interactive (piped): input is forwarded until EOF and output
//! is printed — so `echo cmd | wk attach <repl>` works in a script.

use std::io::{BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use wk_protocol::ipc::{read_msg, write_msg, ClientMsg, ServerMsg};

use crate::cli;

/// Restores the terminal's original mode on drop (RAII), so a panic or early
/// return can't leave the shell in raw mode.
struct RawMode(libc::termios);

impl RawMode {
    /// Put stdin into raw mode, returning a guard that restores it. `None` if
    /// stdin isn't a terminal.
    fn enable() -> Option<RawMode> {
        // SAFETY: standard termios get/set on fd 0; the returned guard restores
        // the saved state on drop.
        unsafe {
            if libc::isatty(0) != 1 {
                return None;
            }
            let mut orig: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut orig) != 0 {
                return None;
            }
            let mut raw = orig;
            libc::cfmakeraw(&mut raw);
            libc::tcsetattr(0, libc::TCSANOW, &raw);
            Some(RawMode(orig))
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, &self.0);
        }
    }
}

/// The local terminal size (cols, rows), if stdin is a tty.
fn term_size() -> Option<(u16, u16)> {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(0, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
            Some((ws.ws_col, ws.ws_row))
        } else {
            None
        }
    }
}

/// `wk attach <ref>`: attach to a terminal node.
pub fn attach(workspace: &Path, node_ref: &str) -> Result<(), String> {
    let mut stream = cli::connect(workspace)?;
    let snap = cli::get_snapshot(&mut stream)?;
    let node = cli::resolve(&snap, node_ref)?;
    if !node.terminal {
        return Err(format!(
            "{} is not a terminal node (only wasi:cli nodes can be attached)",
            if node.name.is_empty() {
                node_ref
            } else {
                &node.name
            }
        ));
    }
    let (id, name) = (node.id, node.name.clone());

    write_msg(&mut stream, &ClientMsg::Attach { node: id }).map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
    match read_msg::<_, ServerMsg>(&mut reader).map_err(|e| e.to_string())? {
        Some(ServerMsg::Attached { .. }) => {}
        Some(ServerMsg::Error(e)) => return Err(e),
        other => return Err(format!("unexpected reply: {other:?}")),
    }

    let raw = RawMode::enable();
    let interactive = raw.is_some();
    if interactive {
        eprintln!("wk: attached to {name} — detach with Ctrl-P Ctrl-Q\r");
    }

    let done = Arc::new(AtomicBool::new(false));

    // Follow the local terminal size: send it now (the node likely started at a
    // different default), and again whenever the window is resized. A poll loop
    // keeps this dependency-free; ~150 ms latency on a resize is imperceptible.
    let resizer = interactive.then(|| {
        let mut wstream = stream.try_clone();
        let done = done.clone();
        std::thread::spawn(move || {
            let Ok(wstream) = &mut wstream else { return };
            let mut last: Option<(u16, u16)> = None;
            while !done.load(Ordering::Relaxed) {
                let sz = term_size();
                if sz != last {
                    if let Some((cols, rows)) = sz {
                        if write_msg(wstream, &ClientMsg::Resize { cols, rows }).is_err() {
                            break;
                        }
                    }
                    last = sz;
                }
                std::thread::sleep(std::time::Duration::from_millis(150));
            }
        })
    });

    // Forward local stdin to the node on its own thread; the main thread pumps
    // the node's output to stdout. `done` ends both when either side finishes.
    let input = {
        let mut wstream = stream.try_clone().map_err(|e| e.to_string())?;
        let done = done.clone();
        std::thread::spawn(move || forward_stdin(&mut wstream, interactive, &done))
    };

    // Piped (non-interactive): the node never sees our stdin EOF (detach doesn't
    // close its stdin), so read its output until it goes idle, then exit.
    if !interactive {
        let _ = reader
            .get_ref()
            .set_read_timeout(Some(std::time::Duration::from_millis(1200)));
    }

    let mut stdout = std::io::stdout();
    while !done.load(Ordering::Relaxed) {
        match read_msg::<_, ServerMsg>(&mut reader) {
            Ok(Some(ServerMsg::Term(bytes))) => {
                if stdout.write_all(&bytes).is_err() || stdout.flush().is_err() {
                    break;
                }
            }
            // The node exited, or we detached.
            Ok(Some(ServerMsg::Detached)) | Ok(None) => break,
            Ok(Some(_)) => {}
            Err(_) => break,
        }
    }
    done.store(true, Ordering::Relaxed);
    // Best-effort detach so the server hands the node back to the UI.
    let _ = write_msg(&mut stream, &ClientMsg::Detach);
    drop(raw); // restore the terminal before the parting message
    let _ = input.join();
    if let Some(r) = resizer {
        let _ = r.join();
    }
    if interactive {
        eprintln!("\r\nwk: detached from {name}");
    }
    Ok(())
}

/// Read local stdin and forward it as [`ClientMsg::Input`]. In interactive mode,
/// the docker detach sequence **Ctrl-P Ctrl-Q** ends the attach instead of being
/// sent. Stops when `done` is set, on EOF, or on a write error.
fn forward_stdin(stream: &mut UnixStream, interactive: bool, done: &AtomicBool) {
    const CTRL_P: u8 = 0x10;
    const CTRL_Q: u8 = 0x11;
    let mut stdin = std::io::stdin();
    let mut buf = [0u8; 1024];
    let mut armed = false; // saw Ctrl-P, waiting for Ctrl-Q
    while !done.load(Ordering::Relaxed) {
        let n = match stdin.read(&mut buf) {
            Ok(0) | Err(_) => break, // EOF or error
            Ok(n) => n,
        };
        let mut out: Vec<u8> = Vec::with_capacity(n);
        for &b in &buf[..n] {
            if interactive {
                if armed && b == CTRL_Q {
                    // Detach: tell the server so it replies `Detached`, which
                    // wakes the output loop (it's blocked reading the socket and
                    // wouldn't notice a local flag while the node is idle).
                    let _ = write_msg(stream, &ClientMsg::Detach);
                    done.store(true, Ordering::Relaxed);
                    break;
                }
                if b == CTRL_P {
                    armed = true;
                    continue; // hold it back until we know the next byte
                }
                if armed {
                    out.push(CTRL_P); // a lone Ctrl-P, not a detach
                    armed = false;
                }
            }
            out.push(b);
        }
        if !out.is_empty() && write_msg(stream, &ClientMsg::Input(out)).is_err() {
            break;
        }
        if done.load(Ordering::Relaxed) {
            break;
        }
    }
    // EOF/error on stdin: stop forwarding, but leave `done` alone so the output
    // loop keeps draining (interactive stays until detach/exit; piped until idle).
}
