//! Standard MIDI Files: the format every other music tool can read.
//!
//! A sequencer whose patterns only exist inside itself is a toy, however good
//! it sounds — the work has to be able to leave. This crate reads and writes
//! SMF, which is what "leave" means in practice.
//!
//! It is deliberately close to the bytes: a file is a list of tracks, a track
//! is a list of `(delta, event)` pairs, and meta events keep their raw payload.
//! Nothing is interpreted that does not have to be, so a file read and written
//! again comes back the same, including the events this program has no opinion
//! about. Interpreting the events into music is [`wk-sequence`]'s job.
//!
//! [`wk-sequence`]: https://docs.rs/wk-sequence

use std::fmt;

/// Meta event type numbers, from the SMF specification.
pub mod meta {
    /// Track name, as text.
    pub const TRACK_NAME: u8 = 0x03;
    /// End of track. Every track ends with one, and it carries no data.
    pub const END_OF_TRACK: u8 = 0x2F;
    /// Tempo: three bytes, microseconds per quarter note.
    pub const TEMPO: u8 = 0x51;
    /// Time signature: numerator, denominator as a power of two, MIDI clocks
    /// per metronome click, and 32nd notes per quarter.
    pub const TIME_SIGNATURE: u8 = 0x58;
}

/// What an event is. Channel messages keep their raw bytes because that is what
/// the rest of wk speaks; meta and system-exclusive events keep their payload
/// so a file survives a round trip even where this crate has no opinion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EventKind {
    /// A channel voice or channel mode message: a status byte and its data.
    Midi(Vec<u8>),
    /// A meta event: its type number and its payload.
    Meta { kind: u8, data: Vec<u8> },
    /// A system-exclusive dump, payload only (no leading `f0`).
    Sysex(Vec<u8>),
}

impl EventKind {
    /// A tempo event, from microseconds per quarter note.
    pub fn tempo(micros_per_quarter: u32) -> Self {
        let m = micros_per_quarter.min(0x00FF_FFFF);
        EventKind::Meta {
            kind: meta::TEMPO,
            data: vec![(m >> 16) as u8, (m >> 8) as u8, m as u8],
        }
    }

    /// Microseconds per quarter note, if this is a tempo event.
    pub fn as_tempo(&self) -> Option<u32> {
        match self {
            EventKind::Meta { kind, data } if *kind == meta::TEMPO && data.len() == 3 => {
                Some(u32::from(data[0]) << 16 | u32::from(data[1]) << 8 | u32::from(data[2]))
            }
            _ => None,
        }
    }

    pub fn track_name(name: &str) -> Self {
        EventKind::Meta {
            kind: meta::TRACK_NAME,
            data: name.as_bytes().to_vec(),
        }
    }

    /// The track's name, if this is a track-name event.
    pub fn as_track_name(&self) -> Option<String> {
        match self {
            EventKind::Meta { kind, data } if *kind == meta::TRACK_NAME => {
                Some(String::from_utf8_lossy(data).into_owned())
            }
            _ => None,
        }
    }

    /// A 4/4 time signature with the conventional clock settings.
    pub fn time_signature(numerator: u8, denominator_pow2: u8) -> Self {
        EventKind::Meta {
            kind: meta::TIME_SIGNATURE,
            data: vec![numerator, denominator_pow2, 24, 8],
        }
    }

    pub fn end_of_track() -> Self {
        EventKind::Meta {
            kind: meta::END_OF_TRACK,
            data: Vec::new(),
        }
    }

    fn is_end_of_track(&self) -> bool {
        matches!(self, EventKind::Meta { kind, .. } if *kind == meta::END_OF_TRACK)
    }
}

/// One event and how long after the previous one on its track it happens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    /// Ticks since the previous event on this track.
    pub delta: u32,
    pub kind: EventKind,
}

impl Event {
    pub fn new(delta: u32, kind: EventKind) -> Self {
        Event { delta, kind }
    }
}

/// A Standard MIDI File.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MidiFile {
    /// Ticks per quarter note. The whole file's time base.
    pub ppq: u16,
    pub tracks: Vec<Vec<Event>>,
}

/// Why a file could not be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The file does not start with an `MThd` header chunk.
    NotAMidiFile,
    /// The file ends in the middle of something.
    Truncated,
    /// Timing is expressed in SMPTE frames rather than ticks per quarter note.
    /// Rare, and silently mistiming it would be worse than refusing it.
    SmpteTiming,
    /// A track's bytes do not decode as events.
    BadTrack(usize),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotAMidiFile => write!(f, "not a MIDI file (no MThd header)"),
            Error::Truncated => write!(f, "the file ends mid-way through"),
            Error::SmpteTiming => write!(f, "SMPTE timecode timing is not supported"),
            Error::BadTrack(i) => write!(f, "track {i} does not decode"),
        }
    }
}

impl std::error::Error for Error {}

impl MidiFile {
    /// An empty file at `ppq` ticks per quarter note.
    pub fn new(ppq: u16) -> Self {
        MidiFile {
            ppq: ppq.max(1),
            tracks: Vec::new(),
        }
    }

    /// Read a Standard MIDI File.
    ///
    /// Chunks other than `MTrk` are skipped, as the specification requires, so
    /// a file carrying a vendor's own chunk still opens.
    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        let mut r = Reader::new(bytes);
        if r.take(4).ok_or(Error::Truncated)? != b"MThd" {
            return Err(Error::NotAMidiFile);
        }
        let header_len = r.u32().ok_or(Error::Truncated)? as usize;
        let header = r.take(header_len).ok_or(Error::Truncated)?;
        if header.len() < 6 {
            return Err(Error::Truncated);
        }
        let division = i16::from_be_bytes([header[4], header[5]]);
        if division <= 0 {
            // A negative division is SMPTE: frames per second in the high byte,
            // ticks per frame in the low. A zero one is meaningless.
            return Err(Error::SmpteTiming);
        }

        let mut tracks = Vec::new();
        while let Some(tag) = r.take(4) {
            let len = r.u32().ok_or(Error::Truncated)? as usize;
            let body = r.take(len).ok_or(Error::Truncated)?;
            if tag == b"MTrk" {
                let index = tracks.len();
                tracks.push(parse_track(body).ok_or(Error::BadTrack(index))?);
            }
        }
        Ok(MidiFile {
            ppq: division as u16,
            tracks,
        })
    }

    /// Write the file out. Always format 1 (one tempo map, several parallel
    /// tracks), which is what a multi-track sequencer means, and what every
    /// other tool expects to be handed.
    pub fn write(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"MThd");
        out.extend_from_slice(&6u32.to_be_bytes());
        out.extend_from_slice(&1u16.to_be_bytes()); // format 1
        out.extend_from_slice(&(self.tracks.len() as u16).to_be_bytes());
        out.extend_from_slice(&self.ppq.max(1).to_be_bytes());
        for track in &self.tracks {
            let body = write_track(track);
            out.extend_from_slice(b"MTrk");
            out.extend_from_slice(&(body.len() as u32).to_be_bytes());
            out.extend_from_slice(&body);
        }
        out
    }

    /// The file's tempo in microseconds per quarter note: the first tempo event
    /// in any track, or the MIDI default of 120 BPM if none says otherwise.
    pub fn tempo(&self) -> u32 {
        self.tracks
            .iter()
            .flatten()
            .find_map(|e| e.kind.as_tempo())
            .unwrap_or(500_000)
    }
}

/// How many data bytes follow a channel status byte.
fn channel_data_len(status: u8) -> usize {
    match status & 0xF0 {
        0xC0 | 0xD0 => 1,
        _ => 2,
    }
}

fn parse_track(body: &[u8]) -> Option<Vec<Event>> {
    let mut r = Reader::new(body);
    let mut events = Vec::new();
    let mut running: Option<u8> = None;
    while !r.done() {
        let delta = r.vlq()?;
        let byte = r.u8()?;
        let kind = match byte {
            0xFF => {
                running = None;
                let kind = r.u8()?;
                let len = r.vlq()? as usize;
                let data = r.take(len)?.to_vec();
                EventKind::Meta { kind, data }
            }
            0xF0 | 0xF7 => {
                running = None;
                let len = r.vlq()? as usize;
                EventKind::Sysex(r.take(len)?.to_vec())
            }
            status if status >= 0x80 => {
                running = Some(status);
                let mut msg = vec![status];
                msg.extend_from_slice(r.take(channel_data_len(status))?);
                EventKind::Midi(msg)
            }
            data => {
                // Running status: this byte is the first data byte of another
                // message with the status that came before.
                let status = running?;
                let mut msg = vec![status, data];
                msg.extend_from_slice(r.take(channel_data_len(status) - 1)?);
                EventKind::Midi(msg)
            }
        };
        let end = kind.is_end_of_track();
        events.push(Event { delta, kind });
        if end {
            break;
        }
    }
    Some(events)
}

fn write_track(events: &[Event]) -> Vec<u8> {
    let mut out = Vec::new();
    for event in events {
        write_vlq(&mut out, event.delta);
        match &event.kind {
            // Written without running status. It costs a byte per event and
            // buys a file that is trivial to read back and to diff.
            EventKind::Midi(msg) => out.extend_from_slice(msg),
            EventKind::Meta { kind, data } => {
                out.push(0xFF);
                out.push(*kind);
                write_vlq(&mut out, data.len() as u32);
                out.extend_from_slice(data);
            }
            EventKind::Sysex(data) => {
                out.push(0xF0);
                write_vlq(&mut out, data.len() as u32);
                out.extend_from_slice(data);
            }
        }
    }
    // Every track must end with one, whatever the caller handed us.
    if !events.last().is_some_and(|e| e.kind.is_end_of_track()) {
        write_vlq(&mut out, 0);
        out.extend_from_slice(&[0xFF, meta::END_OF_TRACK, 0]);
    }
    out
}

/// SMF's variable-length quantity: seven bits per byte, high bit set on every
/// byte but the last.
fn write_vlq(out: &mut Vec<u8>, mut value: u32) {
    let mut buf = [0u8; 5];
    let mut n = 0;
    loop {
        buf[n] = (value & 0x7F) as u8;
        n += 1;
        value >>= 7;
        if value == 0 {
            break;
        }
    }
    for i in (0..n).rev() {
        out.push(buf[i] | if i > 0 { 0x80 } else { 0 });
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Reader { bytes, pos: 0 }
    }

    fn done(&self) -> bool {
        self.pos >= self.bytes.len()
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let slice = self.bytes.get(self.pos..end)?;
        self.pos = end;
        Some(slice)
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    fn u32(&mut self) -> Option<u32> {
        let b = self.take(4)?;
        Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn vlq(&mut self) -> Option<u32> {
        let mut value: u32 = 0;
        // Five bytes would overflow 32 bits; the format never needs more than
        // four, so a longer run is a corrupt file rather than a big number.
        for _ in 0..4 {
            let byte = self.u8()?;
            value = (value << 7) | u32::from(byte & 0x7F);
            if byte & 0x80 == 0 {
                return Some(value);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note_on(channel: u8, note: u8, vel: u8) -> EventKind {
        EventKind::Midi(vec![0x90 | channel, note, vel])
    }

    fn note_off(channel: u8, note: u8) -> EventKind {
        EventKind::Midi(vec![0x80 | channel, note, 0])
    }

    #[test]
    fn a_file_survives_being_written_and_read_back() {
        let file = MidiFile {
            ppq: 96,
            tracks: vec![
                vec![
                    Event::new(0, EventKind::track_name("tempo")),
                    Event::new(0, EventKind::tempo(500_000)),
                    Event::new(0, EventKind::time_signature(4, 2)),
                    Event::new(0, EventKind::end_of_track()),
                ],
                vec![
                    Event::new(0, EventKind::track_name("bass")),
                    Event::new(0, note_on(0, 36, 100)),
                    Event::new(24, note_off(0, 36)),
                    Event::new(72, note_on(0, 38, 80)),
                    Event::new(24, note_off(0, 38)),
                    Event::new(0, EventKind::end_of_track()),
                ],
            ],
        };
        let bytes = file.write();
        assert_eq!(&bytes[0..4], b"MThd");
        assert_eq!(MidiFile::parse(&bytes), Ok(file));
    }

    #[test]
    fn running_status_from_another_program_decodes() {
        // Most sequencers write running status; a reader that cannot take it
        // cannot open their files. Two note-ons sharing one status byte.
        let mut track = Vec::new();
        write_vlq(&mut track, 0);
        track.extend_from_slice(&[0x90, 60, 100]);
        write_vlq(&mut track, 24);
        track.extend_from_slice(&[64, 100]); // running status
        write_vlq(&mut track, 0);
        track.extend_from_slice(&[0xFF, meta::END_OF_TRACK, 0]);

        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"MThd");
        bytes.extend_from_slice(&6u32.to_be_bytes());
        bytes.extend_from_slice(&1u16.to_be_bytes());
        bytes.extend_from_slice(&1u16.to_be_bytes());
        bytes.extend_from_slice(&96u16.to_be_bytes());
        bytes.extend_from_slice(b"MTrk");
        bytes.extend_from_slice(&(track.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&track);

        let file = MidiFile::parse(&bytes).expect("parses");
        assert_eq!(
            file.tracks[0][0..2],
            [
                Event::new(0, note_on(0, 60, 100)),
                Event::new(24, note_on(0, 64, 100)),
            ]
        );
    }

    #[test]
    fn variable_length_quantities_round_trip_at_the_boundaries() {
        for value in [0, 1, 127, 128, 255, 8192, 0x0F_FF_FF, 0x0FFF_FFFF] {
            let mut buf = Vec::new();
            write_vlq(&mut buf, value);
            assert_eq!(Reader::new(&buf).vlq(), Some(value), "vlq {value}");
        }
    }

    #[test]
    fn a_long_delta_survives_the_round_trip() {
        // Multi-byte deltas are where a hand-rolled encoder usually breaks.
        let file = MidiFile {
            ppq: 480,
            tracks: vec![vec![
                Event::new(0, note_on(1, 60, 64)),
                Event::new(100_000, note_off(1, 60)),
                Event::new(0, EventKind::end_of_track()),
            ]],
        };
        assert_eq!(MidiFile::parse(&file.write()), Ok(file));
    }

    #[test]
    fn tempo_reads_back_as_microseconds_per_quarter() {
        // 140 BPM.
        let micros = (60_000_000.0f64 / 140.0) as u32;
        let file = MidiFile {
            ppq: 96,
            tracks: vec![vec![
                Event::new(0, EventKind::tempo(micros)),
                Event::new(0, EventKind::end_of_track()),
            ]],
        };
        assert_eq!(MidiFile::parse(&file.write()).unwrap().tempo(), micros);
    }

    #[test]
    fn a_file_with_no_tempo_reads_as_the_midi_default() {
        let file = MidiFile {
            ppq: 96,
            tracks: vec![vec![Event::new(0, EventKind::end_of_track())]],
        };
        assert_eq!(file.tempo(), 500_000, "120 BPM, per the specification");
    }

    #[test]
    fn chunks_this_crate_does_not_know_are_skipped() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"MThd");
        bytes.extend_from_slice(&6u32.to_be_bytes());
        bytes.extend_from_slice(&1u16.to_be_bytes());
        bytes.extend_from_slice(&1u16.to_be_bytes());
        bytes.extend_from_slice(&96u16.to_be_bytes());
        // A vendor chunk between the header and the track.
        bytes.extend_from_slice(b"XFIH");
        bytes.extend_from_slice(&3u32.to_be_bytes());
        bytes.extend_from_slice(&[1, 2, 3]);
        let track = write_track(&[Event::new(0, note_on(0, 60, 1))]);
        bytes.extend_from_slice(b"MTrk");
        bytes.extend_from_slice(&(track.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&track);

        let file = MidiFile::parse(&bytes).expect("the unknown chunk is skipped");
        assert_eq!(file.tracks.len(), 1);
        assert_eq!(file.tracks[0][0].kind, note_on(0, 60, 1));
    }

    #[test]
    fn an_end_of_track_is_written_even_if_the_caller_forgot() {
        let body = write_track(&[Event::new(0, note_on(0, 60, 1))]);
        assert_eq!(&body[body.len() - 3..], &[0xFF, meta::END_OF_TRACK, 0]);
    }

    #[test]
    fn rubbish_is_refused_rather_than_guessed_at() {
        assert_eq!(
            MidiFile::parse(b"not a midi file"),
            Err(Error::NotAMidiFile)
        );
        assert_eq!(MidiFile::parse(b"MThd"), Err(Error::Truncated));

        // SMPTE timing: refused, because mistiming it silently is worse.
        let mut smpte = Vec::new();
        smpte.extend_from_slice(b"MThd");
        smpte.extend_from_slice(&6u32.to_be_bytes());
        smpte.extend_from_slice(&0u16.to_be_bytes());
        smpte.extend_from_slice(&1u16.to_be_bytes());
        smpte.extend_from_slice(&(-25i16).to_be_bytes());
        assert_eq!(MidiFile::parse(&smpte), Err(Error::SmpteTiming));
    }

    #[test]
    fn a_truncated_track_does_not_panic() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"MThd");
        bytes.extend_from_slice(&6u32.to_be_bytes());
        bytes.extend_from_slice(&1u16.to_be_bytes());
        bytes.extend_from_slice(&1u16.to_be_bytes());
        bytes.extend_from_slice(&96u16.to_be_bytes());
        bytes.extend_from_slice(b"MTrk");
        bytes.extend_from_slice(&10u32.to_be_bytes());
        bytes.extend_from_slice(&[0x00, 0x90, 0x3C]); // fewer bytes than declared
        assert_eq!(MidiFile::parse(&bytes), Err(Error::Truncated));
    }
}
