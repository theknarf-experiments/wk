//! A MIDI 1.0 byte-stream decoder.
//!
//! Bytes arriving from a hardware port are a *stream*, not a message: one
//! CoreMIDI packet may hold several messages, one message may be split across
//! packets, a device may omit repeated status bytes (running status), and a
//! system real-time byte may be injected in the middle of any other message
//! (including inside a system-exclusive dump). Handing such a blob to a plugin
//! as if it were one message loses everything after the first three bytes.
//!
//! [`Decoder`] turns the stream back into whole messages. One decoder lives per
//! source and is fed every byte that source produces, in order, because running
//! status and a partially received message carry across packet boundaries.

/// One complete MIDI message: a status byte plus its data bytes, or a whole
/// system-exclusive message from `f0` through `f7`.
pub type Message = Vec<u8>;

/// How many data bytes follow `status`. Only meaningful for a status byte
/// (`>= 0x80`) that is not a system real-time byte.
fn data_len(status: u8) -> usize {
    match status {
        // Channel voice messages, by the high nibble.
        0x80..=0xBF => 2, // note off, note on, poly key pressure, control change
        0xC0..=0xDF => 1, // program change, channel pressure
        0xE0..=0xEF => 2, // pitch bend
        // System common.
        0xF1 => 1, // MIDI time-code quarter frame
        0xF2 => 2, // song position pointer
        0xF3 => 1, // song select
        _ => 0,    // f4/f5 undefined, f6 tune request
    }
}

/// A system-exclusive dump longer than this is abandoned rather than buffered
/// forever, so a device that never sends its terminating `f7` cannot grow the
/// decoder without bound. Comfortably larger than a real patch dump.
const SYSEX_LIMIT: usize = 65536;

/// Incremental MIDI stream decoder. Feed it bytes with [`Decoder::feed`]; it
/// yields the complete messages those bytes finished.
#[derive(Default)]
pub struct Decoder {
    /// The status byte awaiting data, and how many data bytes it still wants in
    /// total. For a channel message this doubles as the running-status latch:
    /// it is re-armed after each complete message, so a bare data byte starts
    /// another message with the same status.
    pending: Option<(u8, usize)>,
    /// Data bytes received for `pending` so far.
    data: Vec<u8>,
    /// The system-exclusive message being accumulated, if one is in progress.
    sysex: Option<Vec<u8>>,
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed raw bytes, returning every message they completed.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Message> {
        let mut out = Vec::new();
        for &b in bytes {
            self.byte(b, &mut out);
        }
        out
    }

    fn byte(&mut self, b: u8, out: &mut Vec<Message>) {
        match b {
            // System real-time. A single byte that may appear anywhere,
            // including between the status and data bytes of another message or
            // inside a sysex dump, and which disturbs no other state.
            0xF8..=0xFF => out.push(vec![b]),

            // End of system-exclusive. Terminates a dump in progress and is
            // part of it; on its own it means nothing.
            0xF7 => {
                if let Some(mut dump) = self.sysex.take() {
                    dump.push(b);
                    out.push(dump);
                }
                self.pending = None;
            }

            // Start of system-exclusive. Abandons any message in progress.
            0xF0 => {
                self.pending = None;
                self.data.clear();
                self.sysex = Some(vec![b]);
            }

            // Any other status byte. It ends a sysex dump without a terminator
            // (the dump is discarded, per the spec's "any status byte cancels")
            // and abandons an incomplete message.
            0x80..=0xF6 => {
                self.sysex = None;
                self.data.clear();
                let want = data_len(b);
                if want == 0 {
                    // Tune request, or an undefined system-common byte we pass
                    // through as-is. Either way running status is cancelled.
                    out.push(vec![b]);
                    self.pending = None;
                } else {
                    self.pending = Some((b, want));
                }
            }

            // A data byte.
            _ => {
                if let Some(dump) = self.sysex.as_mut() {
                    if dump.len() < SYSEX_LIMIT {
                        dump.push(b);
                    } else {
                        // Runaway dump: give up on it rather than buffer more.
                        self.sysex = None;
                    }
                    return;
                }
                let Some((status, want)) = self.pending else {
                    // A data byte with no status to attach it to (the stream
                    // was joined mid-message). Nothing sensible to do with it.
                    return;
                };
                self.data.push(b);
                if self.data.len() < want {
                    return;
                }
                let mut msg = Vec::with_capacity(1 + want);
                msg.push(status);
                msg.append(&mut self.data);
                out.push(msg);
                // Channel messages latch: a following data byte starts another
                // message with the same status. System common messages do not.
                self.pending = (status < 0xF0).then_some((status, want));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(bytes: &[u8]) -> Vec<Message> {
        Decoder::new().feed(bytes)
    }

    #[test]
    fn splits_a_packet_holding_several_messages() {
        // The bug this decoder exists to fix: a keyboard sending two note-ons
        // in one packet used to arrive as a single 6-byte "message", and every
        // plugin read only the first three bytes.
        let msgs = decode(&[0x90, 60, 100, 0x90, 64, 100, 0x80, 60, 0]);
        assert_eq!(
            msgs,
            vec![vec![0x90, 60, 100], vec![0x90, 64, 100], vec![0x80, 60, 0]]
        );
    }

    #[test]
    fn running_status_reuses_the_last_status_byte() {
        // A chord sent the way most keyboards send one: the status byte once,
        // then bare note/velocity pairs.
        let msgs = decode(&[0x90, 60, 100, 64, 100, 67, 100]);
        assert_eq!(
            msgs,
            vec![
                vec![0x90, 60, 100],
                vec![0x90, 64, 100],
                vec![0x90, 67, 100]
            ]
        );
    }

    #[test]
    fn a_message_split_across_feeds_is_reassembled() {
        let mut d = Decoder::new();
        assert!(d.feed(&[0x90]).is_empty(), "status alone completes nothing");
        assert!(d.feed(&[60]).is_empty(), "one data byte is not enough");
        assert_eq!(d.feed(&[100]), vec![vec![0x90, 60, 100]]);
        // Running status still holds across the boundary.
        assert_eq!(d.feed(&[62]), Vec::<Message>::new());
        assert_eq!(d.feed(&[100]), vec![vec![0x90, 62, 100]]);
    }

    #[test]
    fn message_lengths_follow_the_status_byte() {
        // Program change and channel pressure take one data byte, not two.
        assert_eq!(
            decode(&[0xC0, 5, 0xD0, 64, 0xE0, 0, 64]),
            vec![vec![0xC0, 5], vec![0xD0, 64], vec![0xE0, 0, 64]]
        );
    }

    #[test]
    fn realtime_bytes_pass_through_without_disturbing_a_message() {
        // A clock byte lands between the status and its data. Both survive, and
        // the clock is not mistaken for part of the note.
        let msgs = decode(&[0x90, 0xF8, 60, 0xF8, 100]);
        assert_eq!(msgs, vec![vec![0xF8], vec![0xF8], vec![0x90, 60, 100]]);
    }

    #[test]
    fn realtime_bytes_do_not_break_running_status() {
        let msgs = decode(&[0x90, 60, 100, 0xFE, 64, 100]);
        assert_eq!(
            msgs,
            vec![vec![0x90, 60, 100], vec![0xFE], vec![0x90, 64, 100]]
        );
    }

    #[test]
    fn sysex_is_one_message_even_when_split_and_interrupted() {
        let mut d = Decoder::new();
        assert!(d.feed(&[0xF0, 0x7E, 0x00]).is_empty());
        // A clock byte inside the dump is its own message; the dump continues.
        assert_eq!(d.feed(&[0xF8]), vec![vec![0xF8]]);
        assert_eq!(
            d.feed(&[0x06, 0x01, 0xF7]),
            vec![vec![0xF0, 0x7E, 0x00, 0x06, 0x01, 0xF7]]
        );
    }

    #[test]
    fn a_status_byte_cancels_an_unterminated_sysex() {
        let msgs = decode(&[0xF0, 0x7E, 0x00, 0x90, 60, 100]);
        assert_eq!(msgs, vec![vec![0x90, 60, 100]], "the dump is discarded");
    }

    #[test]
    fn system_common_cancels_running_status() {
        // After a song-position message the bare data bytes have no status to
        // attach to, so they are dropped rather than read as notes.
        let msgs = decode(&[0x90, 60, 100, 0xF2, 0, 0, 62, 100]);
        assert_eq!(msgs, vec![vec![0x90, 60, 100], vec![0xF2, 0, 0]]);
    }

    #[test]
    fn tune_request_is_a_complete_message_on_its_own() {
        assert_eq!(decode(&[0xF6]), vec![vec![0xF6]]);
    }

    #[test]
    fn orphan_data_bytes_are_ignored() {
        // Joining a stream mid-message must not synthesise notes.
        assert_eq!(decode(&[60, 100, 64]), Vec::<Message>::new());
    }

    #[test]
    fn a_new_status_abandons_an_incomplete_message() {
        let msgs = decode(&[0x90, 60, 0x80, 62, 0]);
        assert_eq!(msgs, vec![vec![0x80, 62, 0]]);
    }

    #[test]
    fn a_runaway_sysex_is_abandoned_rather_than_buffered() {
        let mut d = Decoder::new();
        d.feed(&[0xF0]);
        d.feed(&vec![0x7Fu8; SYSEX_LIMIT * 2]);
        // The dump was dropped, so its terminator completes nothing, and the
        // decoder still works afterwards.
        assert!(d.feed(&[0xF7]).is_empty());
        assert_eq!(d.feed(&[0x90, 60, 100]), vec![vec![0x90, 60, 100]]);
    }
}
