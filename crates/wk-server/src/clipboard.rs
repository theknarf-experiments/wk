//! Host side of `wk:clipboard/clipboard` — the system clipboard as a canvas
//! capability.
//!
//! A Clipboard node owns a [`SharedBoard`]: the text the host clipboard last
//! held, plus an outbox of text a guest wants put there. The board is pumped
//! by whoever actually owns a window-system connection — for the local client
//! that is the winit thread and `arboard`, in `client-local-ui`'s
//! `pump_clipboard`. Nothing in wk-server touches a platform clipboard API,
//! and a Clipboard node with no pump is simply an empty one.
//!
//! Wiring an app to a Clipboard node points the app's [`SharedClipSrc`] at
//! that board (the `sync_clipboard` reconciler) and sets its two permits;
//! unwiring clears them. This mirrors `capture.rs` exactly, with one
//! deliberate difference: capture has ONE permit and clipboard has TWO, so a
//! token can grant copy-out (`write`) without granting the ability to read
//! whatever the user last copied anywhere on their machine (`read`). See
//! `wit-clipboard/world.wit` for why that split is not optional.
//!
//! The read side is a published value, never a request/response round trip:
//! `get` reads a mutex the pump refreshes. That is what lets Qt call it from
//! inside the synchronous `QPlatformClipboard::mimeData()`. The write side is
//! an outbox for the same reason in reverse — `arboard::Clipboard` may only be
//! touched by the thread that owns it, and that is never a guest thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use wasmtime::component::{HasData, Linker};
use wasmtime::Result;

use crate::plugin::HostState;

wasmtime::component::bindgen!({
    path: "wit-clipboard",
    world: "clipboard-host",
    imports: { default: trappable },
    require_store_data_send: true,
});

/// The biggest string that crosses the boundary in either direction, 4 MiB.
///
/// A text editor node can select-all and copy a whole document, and both
/// directions land in host memory that outlives the call: the outbox holds a
/// guest's `set` until the client's next event-loop pass, and `text` holds
/// whatever the host clipboard had until it changes. Neither should be an
/// unbounded allocation a sandbox controls, so an oversized `set` is dropped
/// (with a log line, since silently losing a copy is otherwise baffling) and
/// an oversized host clipboard is simply not published.
pub const MAX_TEXT: usize = 4 * 1024 * 1024;

/// One Clipboard node's board: what the host clipboard holds, and what a guest
/// has asked to put there.
#[derive(Default)]
pub struct Board {
    /// Whether a real host clipboard is behind this board at all. False on a
    /// machine where `arboard::Clipboard::new()` failed (no display server),
    /// and in every headless test that never installs a pump — the UI shows
    /// this so "nothing pastes" has a visible cause.
    pub present: bool,
    /// 0 until the pump has observed the host clipboard once; then increments
    /// on every OBSERVED CHANGE, never on a re-read of the same text. This is
    /// wk's entire clipboard change-notification mechanism, because `arboard`
    /// offers none — no `changeCount`, no watcher.
    pub seq: u64,
    /// The text the host clipboard held when the pump last looked.
    pub text: String,
    /// Text a guest asked to put on the host clipboard, waiting for the pump.
    /// Last write wins: a node that copies twice before the client gets a turn
    /// meant the second one.
    pub outbox: Option<String>,
}

pub type SharedBoard = Arc<Mutex<Board>>;

/// A node's view of its granted clipboard: `None` until a clipboard wire (and
/// a token that allows at least one of read/write) points it at a Clipboard
/// node's board.
pub type SharedClipSrc = Arc<Mutex<Option<SharedBoard>>>;

/// Whether this node may read / may write, refreshed from its capability token
/// every tick so attenuation revokes live. Two of them, not one: `read` and
/// `write` are separately grantable actions on the same wire, the way `file`
/// splits (server.rs `sync_files`) rather than the way `midi` splits by
/// direction — an app↔Clipboard wire has no meaningful direction.
pub type ClipPermit = Arc<AtomicBool>;

pub fn new_board() -> SharedBoard {
    Arc::new(Mutex::new(Board::default()))
}

pub fn new_src() -> SharedClipSrc {
    Arc::new(Mutex::new(None))
}

pub fn new_permit() -> ClipPermit {
    Arc::new(AtomicBool::new(false))
}

pub fn add_to_linker(l: &mut Linker<HostState>) -> Result<()> {
    wk::clipboard::clipboard::add_to_linker::<_, HasClipboard>(l, |s| s)?;
    Ok(())
}

struct HasClipboard;
impl HasData for HasClipboard {
    type Data<'a> = &'a mut HostState;
}

impl wk::clipboard::clipboard::Host for HostState {
    fn get(&mut self) -> Result<Option<wk::clipboard::clipboard::Snapshot>> {
        if !self.clip_read.load(Ordering::Relaxed) {
            // Log the first denied read per node and then go quiet. The guest
            // is told nothing (that is the point of conflating deny with
            // empty), but the person running wk must be able to see in the
            // logs that a node tried and was refused — otherwise "my app
            // cannot paste" has no diagnosis anywhere in the system.
            if !self.clip_denied_logged {
                self.clip_denied_logged = true;
                eprintln!(
                    "wk: node {} asked to read the clipboard and was denied \
                     (no Clipboard wire, or its token forbids clipboard/read)",
                    self.node_id
                );
            }
            return Ok(None);
        }
        let Some(board) = self.clip_src.lock().unwrap().clone() else {
            return Ok(None); // permitted, but not wired to a Clipboard node
        };
        let b = board.lock().unwrap();
        if b.seq == 0 || b.text.len() > MAX_TEXT {
            return Ok(None); // nothing observed yet, or too big to hand over
        }
        Ok(Some(wk::clipboard::clipboard::Snapshot {
            seq: b.seq,
            text: b.text.clone(),
        }))
    }

    fn set(&mut self, text: String) -> Result<()> {
        if !self.clip_write.load(Ordering::Relaxed) {
            return Ok(()); // silently dropped, like wk:midi's send
        }
        let Some(board) = self.clip_src.lock().unwrap().clone() else {
            return Ok(());
        };
        if text.len() > MAX_TEXT {
            eprintln!(
                "wk: node {} tried to copy {} bytes; dropped (limit {MAX_TEXT})",
                self.node_id,
                text.len()
            );
            return Ok(());
        }
        board.lock().unwrap().outbox = Some(text);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The board is the whole contract between the host's pump and a guest:
    /// `seq` only moves on an observed CHANGE, which is what makes "is the
    /// clipboard still mine?" answerable from a number.
    #[test]
    fn seq_advances_only_when_the_text_changes() {
        let board = new_board();
        let mut b = board.lock().unwrap();
        // What a pump does: compare, then publish.
        let observe = |t: &str, b: &mut Board| {
            if b.seq == 0 || b.text != t {
                b.seq += 1;
                b.text = t.to_string();
            }
        };
        observe("hello", &mut b);
        assert_eq!(b.seq, 1);
        observe("hello", &mut b);
        assert_eq!(b.seq, 1, "re-reading the same text is not a change");
        observe("world", &mut b);
        assert_eq!(b.seq, 2);
        // An empty clipboard is a real state, distinct from "never observed".
        observe("", &mut b);
        assert_eq!(b.seq, 3);
        assert_eq!(b.text, "");
    }
}
