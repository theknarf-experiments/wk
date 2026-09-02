//! Turning a [`Song`] into a Standard MIDI File and back.
//!
//! This is the door out of the sequencer. Notes drawn here open in a notation
//! program, a DAW, or a hardware sequencer; a part written anywhere else comes
//! in the same way. Without it a pattern only exists inside wk, which is not
//! what a musician means by having written something.

use wk_midifile::{Event, EventKind, MidiFile};

use crate::{Note, Pattern, Song, Track, MAX_BPM, MAX_PATTERNS, MAX_STEPS, MAX_TRACKS, MIN_BPM};

/// Ticks per quarter note in the files this writes. Divisible by 4 (so a
/// sixteenth-note step is a whole number of ticks) and by 24 (so a MIDI clock
/// pulse is too), and small enough to stay readable.
pub const TICKS_PER_QUARTER: u16 = 96;
/// Ticks in one sixteenth-note step.
const TICKS_PER_STEP: u32 = TICKS_PER_QUARTER as u32 / 4;

impl Song {
    /// Write the song out as a Standard MIDI File, playing `order`'s patterns
    /// one after another.
    ///
    /// `order` is what the sequencer would play: the chain in song mode, or the
    /// single pattern being looped. Exporting what you hear rather than the
    /// whole bank is the behaviour that does not surprise anyone.
    ///
    /// Format 1: a tempo track, then one track per part, each on its own
    /// channel — the shape every other tool expects to be handed.
    pub fn to_midi_file(&self, order: &[usize]) -> MidiFile {
        let mut file = MidiFile::new(TICKS_PER_QUARTER);
        let micros_per_quarter = (60_000_000.0 / self.bpm as f64) as u32;
        file.tracks.push(vec![
            Event::new(0, EventKind::track_name("wk sequencer")),
            Event::new(0, EventKind::tempo(micros_per_quarter)),
            Event::new(0, EventKind::time_signature(4, 2)),
            Event::new(0, EventKind::end_of_track()),
        ]);

        // Where each pattern in the order starts, in steps.
        let mut starts = Vec::with_capacity(order.len());
        let mut cursor = 0i64;
        for &index in order {
            starts.push(cursor);
            cursor += self.patterns.get(index).map_or(0, |p| p.steps as i64);
        }
        let total_steps = cursor;

        for (index, track) in self.tracks.iter().enumerate() {
            // (tick, is-note-on, message)
            let mut timed: Vec<(u32, bool, Vec<u8>)> = Vec::new();
            for (slot, &pattern_index) in order.iter().enumerate() {
                let Some(pattern) = self.patterns.get(pattern_index) else {
                    continue;
                };
                let base = starts[slot];
                for note in pattern.track(index) {
                    let on = (base + note.step as i64) as u32 * TICKS_PER_STEP;
                    let off = (base + note.end(pattern.steps) as i64) as u32 * TICKS_PER_STEP;
                    timed.push((
                        on,
                        true,
                        vec![0x90 | track.channel, note.pitch as u8, note.vel],
                    ));
                    timed.push((off, false, vec![0x80 | track.channel, note.pitch as u8, 0]));
                }
            }
            if timed.is_empty() {
                continue; // a silent part is not worth a track
            }
            // Note-offs before note-ons at the same tick, so a repeated note
            // releases before it retriggers instead of being cut short.
            timed.sort_by_key(|&(tick, is_on, _)| (tick, is_on));

            let mut events = vec![Event::new(
                0,
                EventKind::track_name(&format!("track {} (ch {})", index + 1, track.channel + 1)),
            )];
            let mut previous = 0u32;
            for (tick, _, msg) in timed {
                events.push(Event::new(tick - previous, EventKind::Midi(msg)));
                previous = tick;
            }
            // Hold the track open to the end of the song, so a part that stops
            // early does not shorten the file for everyone else.
            let end = total_steps as u32 * TICKS_PER_STEP;
            events.push(Event::new(
                end.saturating_sub(previous),
                EventKind::end_of_track(),
            ));
            file.tracks.push(events);
        }
        file
    }

    /// Read a Standard MIDI File into a song of `steps_per_pattern`-long
    /// patterns, chained in order.
    ///
    /// Timing is quantised to the sequencer's sixteenth-note grid, because that
    /// is the only grid it has; a performance recorded off the grid will be
    /// pulled onto it. Each MIDI channel present becomes a track, in the order
    /// the channels first appear, up to [`MAX_TRACKS`]. A note crossing a
    /// pattern boundary is split at it rather than cut short, so the music
    /// still sounds the same length.
    pub fn from_midi_file(file: &MidiFile, steps_per_pattern: i32) -> Song {
        let steps_per_pattern = steps_per_pattern.clamp(1, MAX_STEPS);
        let ppq = file.ppq.max(1) as f64;
        let mut song = Song::new();
        song.set_bpm(((60_000_000.0 / file.tempo().max(1) as f64) as f32).clamp(MIN_BPM, MAX_BPM));

        // Channel -> track index, in the order channels are first heard.
        let mut channels: Vec<u8> = Vec::new();
        let mut notes: Vec<Imported> = Vec::new();
        let mut open: Vec<Opening> = Vec::new();

        for events in &file.tracks {
            let mut tick: u64 = 0;
            for event in events {
                tick += event.delta as u64;
                let EventKind::Midi(msg) = &event.kind else {
                    continue;
                };
                if msg.len() < 3 {
                    continue;
                }
                let (status, pitch, vel) = (msg[0] & 0xF0, msg[1], msg[2]);
                let is_on = status == 0x90 && vel > 0;
                let is_off = status == 0x80 || (status == 0x90 && vel == 0);
                if !is_on && !is_off {
                    continue;
                }
                let channel = msg[0] & 0x0F;
                let Some(track) = track_for(&mut channels, channel) else {
                    continue; // more parts than this sequencer has tracks
                };
                // Quarter notes are `ppq` ticks and hold four steps.
                let step = (tick as f64 / ppq * 4.0).round() as i64;
                if is_on {
                    // A second note-on for a pitch already down ends the first.
                    close(&mut open, &mut notes, track, pitch, step);
                    open.push(Opening {
                        track,
                        pitch,
                        start: step,
                        vel,
                    });
                } else {
                    close(&mut open, &mut notes, track, pitch, step);
                }
            }
            // A track that ends without releasing everything: give the stragglers
            // a step each rather than dropping them.
            let step = (tick as f64 / ppq * 4.0).round() as i64;
            let stuck: Vec<(usize, u8)> = open.iter().map(|o| (o.track, o.pitch)).collect();
            for (track, pitch) in stuck {
                close(&mut open, &mut notes, track, pitch, step);
            }
        }

        if notes.is_empty() {
            return song;
        }
        for (index, &channel) in channels.iter().enumerate() {
            song.tracks[index] = Track::new(channel);
        }

        let end = notes
            .iter()
            .map(|n| n.start + n.len as i64)
            .max()
            .unwrap_or(0);
        let wanted = ((end + steps_per_pattern as i64 - 1) / steps_per_pattern as i64).max(1);
        let count = wanted.min(MAX_PATTERNS as i64) as usize;
        song.patterns = (0..count)
            .map(|_| Pattern::empty(steps_per_pattern))
            .collect();
        song.chain = (0..count).collect();

        for note in notes {
            // Split across pattern boundaries so the note keeps its length.
            let mut at = note.start;
            let mut left = note.len as i64;
            while left > 0 {
                let index = (at / steps_per_pattern as i64) as usize;
                if index >= count {
                    break;
                }
                let local = (at % steps_per_pattern as i64) as i32;
                let piece = left.min(steps_per_pattern as i64 - local as i64);
                song.patterns[index].add_note(
                    note.track,
                    Note::new(local, note.pitch, piece as i32, note.vel),
                );
                at += piece;
                left -= piece;
            }
        }
        song
    }
}

/// The track a channel maps to, adding it if this is the first time it is heard
/// and there is room.
fn track_for(channels: &mut Vec<u8>, channel: u8) -> Option<usize> {
    if let Some(i) = channels.iter().position(|&c| c == channel) {
        return Some(i);
    }
    if channels.len() >= MAX_TRACKS {
        return None;
    }
    channels.push(channel);
    Some(channels.len() - 1)
}

/// A note whose note-on has been seen and whose release has not.
struct Opening {
    track: usize,
    pitch: u8,
    /// The step it started on, counted from the beginning of the file.
    start: i64,
    vel: u8,
}

/// A finished note, positioned in the file rather than in a pattern.
struct Imported {
    track: usize,
    start: i64,
    pitch: i32,
    len: i32,
    vel: u8,
}

/// Close the open note for `(track, pitch)` at `step`, recording it. A
/// zero-length note becomes one step, since the grid has nothing shorter.
fn close(open: &mut Vec<Opening>, notes: &mut Vec<Imported>, track: usize, pitch: u8, step: i64) {
    let Some(at) = open
        .iter()
        .position(|o| o.track == track && o.pitch == pitch)
    else {
        return;
    };
    let started = open.remove(at);
    notes.push(Imported {
        track,
        start: started.start,
        pitch: pitch as i32,
        len: (step - started.start).max(1) as i32,
        vel: started.vel,
    });
}
