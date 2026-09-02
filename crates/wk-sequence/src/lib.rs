//! The music a step sequencer holds, and the arithmetic that plays it.
//!
//! Split out of the sequencer plugin so it can be tested where tests run. The
//! plugin is a wasm component: it draws, takes clicks, and talks to wk's MIDI
//! and filesystem. Everything that decides *what note sounds when* is here,
//! with no wasm, no drawing and no I/O, which is why the timing can be asserted
//! to the microsecond rather than listened to and hoped about.
//!
//! The shape is the one a step sequencer has had since hardware ones: a
//! [`Song`] owns a fixed set of [`Track`]s (each with a MIDI channel and a mute)
//! and a bank of [`Pattern`]s (a bar of notes for every track), and a chain
//! saying which pattern plays after which.

pub mod smf;

/// How many tracks a song has. Eight is the number a hardware groovebox
/// settled on: enough for a kit plus a few parts, few enough to select by eye.
pub const MAX_TRACKS: usize = 8;
/// How many patterns a song can hold.
pub const MAX_PATTERNS: usize = 16;
/// How many entries a song chain can hold.
pub const MAX_CHAIN: usize = 64;

/// Pattern length limits, in sixteenth-note steps: one step to four bars.
pub const MIN_STEPS: i32 = 1;
pub const MAX_STEPS: i32 = 64;

/// Tempo limits. Wide enough for a ballad and for drum and bass.
pub const MIN_BPM: f32 = 20.0;
pub const MAX_BPM: f32 = 300.0;

/// The lowest and highest MIDI note.
pub const LOWEST: i32 = 0;
pub const HIGHEST: i32 = 127;

/// MIDI clock pulses per quarter note, fixed by the specification.
pub const PPQ: i32 = 24;
/// Clock pulses per sixteenth-note step.
pub const PULSES_PER_STEP: i32 = PPQ / 4;

/// How far ahead of the clock events are queued, in microseconds.
///
/// Nothing downstream can be precise about an event it has not been given yet:
/// a synth needs it before it can place the note on its audio clock, and a
/// hardware port needs it before the driver can time the byte out. Long enough
/// to survive a slow frame, short enough that an edit or a tempo change takes
/// effect almost at once, since it only invalidates music inside this window.
pub const LOOKAHEAD_US: f64 = 60_000.0;

/// A cap on how many steps one pass may queue, so an extreme tempo or a long
/// stall cannot spin the loop.
const MAX_STEPS_PER_PUMP: i32 = 256;

/// Microseconds per sixteenth-note step at `bpm`.
pub fn step_micros(bpm: f32) -> f64 {
    60_000_000.0 / bpm.clamp(MIN_BPM, MAX_BPM) as f64 / 4.0
}

/// A note in a pattern: a `step` start column, a `pitch`, a `len` in steps (at
/// least 1), and the velocity it sounds at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Note {
    pub step: i32,
    pub pitch: i32,
    pub len: i32,
    pub vel: u8,
}

impl Note {
    pub fn new(step: i32, pitch: i32, len: i32, vel: u8) -> Self {
        Note {
            step,
            pitch,
            len,
            vel,
        }
    }

    /// Is this note sounding at position `p` of a `steps`-long pattern? A note
    /// left hanging over the end by a shortened pattern is cut off there rather
    /// than sounding into the next cycle.
    pub fn covers(&self, p: i32, steps: i32) -> bool {
        self.step <= p && p < (self.step + self.len).min(steps)
    }

    /// Where the note stops, clamped to the pattern.
    pub fn end(&self, steps: i32) -> i32 {
        (self.step + self.len).min(steps)
    }
}

/// One of the song's parts: which MIDI channel it plays on, and whether it is
/// silenced. Tracks belong to the song, not to a pattern, so muting a part
/// mutes it everywhere and a synth stays wired to the same channel throughout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Track {
    /// MIDI channel, 0 to 15.
    pub channel: u8,
    pub muted: bool,
}

impl Track {
    pub fn new(channel: u8) -> Self {
        Track {
            channel: channel.min(15),
            muted: false,
        }
    }
}

/// A bar: how long it is, and what every track plays in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pattern {
    pub steps: i32,
    /// Notes per track, indexed the same way as [`Song::tracks`].
    pub notes: Vec<Vec<Note>>,
}

impl Pattern {
    pub fn empty(steps: i32) -> Self {
        Pattern {
            steps: steps.clamp(MIN_STEPS, MAX_STEPS),
            notes: vec![Vec::new(); MAX_TRACKS],
        }
    }

    /// The notes of `track`, or nothing for a track index out of range.
    pub fn track(&self, track: usize) -> &[Note] {
        self.notes.get(track).map_or(&[], |v| v.as_slice())
    }

    /// The topmost note of `track` under `(step, pitch)`.
    pub fn note_at(&self, track: usize, step: i32, pitch: i32) -> Option<usize> {
        let notes = self.notes.get(track)?;
        (0..notes.len())
            .rev()
            .find(|&i| notes[i].pitch == pitch && notes[i].covers(step, self.steps))
    }

    /// Add a note to `track`, clamping it inside the pattern, and return its
    /// index.
    pub fn add_note(&mut self, track: usize, mut note: Note) -> Option<usize> {
        let steps = self.steps;
        let notes = self.notes.get_mut(track)?;
        note.step = note.step.clamp(0, steps - 1);
        note.pitch = note.pitch.clamp(LOWEST, HIGHEST);
        note.len = note.len.clamp(1, (steps - note.step).max(1));
        note.vel = note.vel.max(1);
        notes.push(note);
        Some(notes.len() - 1)
    }
}

/// Everything the sequencer holds: the tempo, the parts, the bank of patterns
/// and the order they play in.
#[derive(Clone, Debug, PartialEq)]
pub struct Song {
    pub bpm: f32,
    pub tracks: Vec<Track>,
    pub patterns: Vec<Pattern>,
    /// Pattern indices, in the order song mode plays them. Empty means there is
    /// no song yet, only patterns.
    pub chain: Vec<usize>,
}

impl Default for Song {
    fn default() -> Self {
        Song::new()
    }
}

impl Song {
    /// One sixteen-step pattern, eight tracks each on its own MIDI channel.
    ///
    /// Channel per track by default because that is what makes several tracks
    /// useful straight away: wire each synth to its own channel and the parts
    /// are already separate.
    pub fn new() -> Self {
        Song {
            bpm: 120.0,
            tracks: (0..MAX_TRACKS).map(|i| Track::new(i as u8)).collect(),
            patterns: vec![Pattern::empty(16)],
            chain: Vec::new(),
        }
    }

    pub fn set_bpm(&mut self, bpm: f32) {
        self.bpm = bpm.clamp(MIN_BPM, MAX_BPM);
    }

    pub fn pattern(&self, index: usize) -> Option<&Pattern> {
        self.patterns.get(index)
    }

    pub fn pattern_mut(&mut self, index: usize) -> Option<&mut Pattern> {
        self.patterns.get_mut(index)
    }

    /// Add an empty pattern the same length as `like`, returning its index.
    pub fn add_pattern(&mut self, like: usize) -> Option<usize> {
        if self.patterns.len() >= MAX_PATTERNS {
            return None;
        }
        let steps = self.patterns.get(like).map_or(16, |p| p.steps);
        self.patterns.push(Pattern::empty(steps));
        Some(self.patterns.len() - 1)
    }

    /// Copy a pattern, returning the new one's index. How a variation gets
    /// made: duplicate the bar, then change one note in it.
    pub fn clone_pattern(&mut self, index: usize) -> Option<usize> {
        if self.patterns.len() >= MAX_PATTERNS {
            return None;
        }
        let copy = self.patterns.get(index)?.clone();
        self.patterns.push(copy);
        Some(self.patterns.len() - 1)
    }

    /// The MIDI channel `track` plays on.
    pub fn channel(&self, track: usize) -> u8 {
        self.tracks.get(track).map_or(0, |t| t.channel)
    }

    /// Is any track soloed by everything else being muted? Used only for the
    /// obvious display question; muting is the real mechanism.
    pub fn muted(&self, track: usize) -> bool {
        self.tracks.get(track).is_some_and(|t| t.muted)
    }
}

/// What the transport is playing: one pattern on repeat, or the chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Playback {
    /// Loop this pattern. What you want while writing a bar.
    Pattern(usize),
    /// Play the chain, looping the whole thing.
    Song,
}

/// Where the transport is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Position {
    /// The pattern being played.
    pub pattern: usize,
    /// The step within it.
    pub step: i32,
    /// Which entry of [`Song::chain`] this is, in song mode.
    pub chain_index: usize,
}

/// A message to send, and the instant it belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Emit {
    pub data: Vec<u8>,
    /// Microseconds on wk's shared MIDI clock.
    pub time: u64,
}

/// Which pattern and step an absolute step number lands on.
///
/// Returns nothing when there is nothing to play: a pattern index that does not
/// exist, or song mode with an empty chain.
pub fn locate(song: &Song, play: Playback, abs: i64) -> Option<Position> {
    match play {
        Playback::Pattern(index) => {
            let pattern = song.patterns.get(index)?;
            Some(Position {
                pattern: index,
                step: abs.rem_euclid(pattern.steps as i64) as i32,
                chain_index: 0,
            })
        }
        Playback::Song => {
            // Skip chain entries pointing at patterns that no longer exist,
            // keeping each surviving entry's position in the chain so the UI can
            // highlight the right one.
            let entries: Vec<(usize, usize)> = song
                .chain
                .iter()
                .enumerate()
                .filter(|(_, &p)| p < song.patterns.len())
                .map(|(ci, &p)| (ci, p))
                .collect();
            let total: i64 = entries
                .iter()
                .map(|&(_, p)| song.patterns[p].steps as i64)
                .sum();
            if total <= 0 {
                return None;
            }
            let mut pos = abs.rem_euclid(total);
            for (chain_index, pattern) in entries {
                let len = song.patterns[pattern].steps as i64;
                if pos < len {
                    return Some(Position {
                        pattern,
                        step: pos as i32,
                        chain_index,
                    });
                }
                pos -= len;
            }
            None
        }
    }
}

/// The transport: a clock, and the memory of what it has already sent.
///
/// It runs *ahead* of the clock. Each pass queues every step boundary falling
/// inside [`LOOKAHEAD_US`], stamped with the instant that boundary lands on, so
/// the music keeps the clock's time rather than the caller's frame rate.
#[derive(Clone, Debug)]
pub struct Scheduler {
    /// Microseconds per step at the current tempo.
    step_us: f64,
    /// The instant of absolute step 0, in microseconds. Fractional, so the
    /// tempo never rounds into drift.
    origin: f64,
    /// The first absolute step not yet sent.
    next_step: i64,
    running: bool,
    /// `(channel, pitch)` pairs whose note-on has been sent and note-off has not.
    on: Vec<(u8, u8)>,
}

impl Scheduler {
    pub fn new(bpm: f32) -> Self {
        Scheduler {
            step_us: step_micros(bpm),
            origin: 0.0,
            next_step: 0,
            running: false,
            on: Vec::new(),
        }
    }

    pub fn running(&self) -> bool {
        self.running
    }

    /// The instant absolute step `i` falls on.
    pub fn step_instant(&self, i: i64) -> f64 {
        self.origin + i as f64 * self.step_us
    }

    /// The absolute step an instant is nearest to. Recording uses this to place
    /// a played note by when it was played.
    pub fn step_of(&self, instant: u64) -> i64 {
        ((instant as f64 - self.origin) / self.step_us).round() as i64
    }

    /// The absolute step the clock is on right now.
    pub fn abs_step(&self, now: u64) -> i64 {
        ((now as f64 - self.origin) / self.step_us).floor() as i64
    }

    /// Where the transport is, for display and for recording.
    pub fn position(&self, now: u64, song: &Song, play: Playback) -> Option<Position> {
        self.running
            .then(|| locate(song, play, self.abs_step(now)))
            .flatten()
    }

    /// Change tempo without disturbing the beat.
    ///
    /// The clock is re-anchored on the next boundary still to be scheduled, so
    /// that boundary stays exactly where it is and the new rate applies from
    /// there. Nothing already sent is invalidated and no boundary goes out
    /// twice, which is what lets the tempo be dragged while the music plays.
    pub fn set_bpm(&mut self, bpm: f32) {
        let pivot = self.step_instant(self.next_step);
        self.step_us = step_micros(bpm);
        self.origin = pivot - self.next_step as f64 * self.step_us;
    }

    /// Start from the top, announcing it so anything slaved to this clock runs.
    pub fn start(&mut self, now: u64, out: &mut Vec<Emit>) {
        self.origin = now as f64;
        self.next_step = 0;
        self.on.clear();
        self.running = true;
        out.push(Emit {
            data: vec![0xFA],
            time: now,
        });
    }

    /// Stop, releasing everything.
    ///
    /// The releases are stamped for the end of what is already queued, not for
    /// now: a note-off stamped "now" would land *before* note-ons already
    /// scheduled ahead of the clock, and those notes would sound forever.
    pub fn stop(&mut self, out: &mut Vec<Emit>) {
        if !self.running {
            return;
        }
        let at = self.step_instant(self.next_step).max(0.0) as u64;
        for (channel, pitch) in std::mem::take(&mut self.on) {
            out.push(Emit {
                data: vec![0x80 | channel, pitch, 0],
                time: at,
            });
        }
        out.push(Emit {
            data: vec![0xFC],
            time: at,
        });
        self.running = false;
    }

    /// Queue every step boundary inside the look-ahead window.
    ///
    /// `suppress` names `(channel, pitch)` pairs the caller is sounding live —
    /// the keys being held down while recording, which MIDI thru is already
    /// playing. Re-triggering those here would double the note.
    pub fn pump(
        &mut self,
        now: u64,
        song: &Song,
        play: Playback,
        suppress: &[(u8, u8)],
        out: &mut Vec<Emit>,
    ) {
        if !self.running {
            return;
        }
        let horizon = now as f64 + LOOKAHEAD_US;
        let mut budget = MAX_STEPS_PER_PUMP;
        while self.step_instant(self.next_step) < horizon && budget > 0 {
            self.schedule_step(self.next_step, song, play, suppress, out);
            self.next_step += 1;
            budget -= 1;
        }
    }

    fn schedule_step(
        &mut self,
        abs: i64,
        song: &Song,
        play: Playback,
        suppress: &[(u8, u8)],
        out: &mut Vec<Emit>,
    ) {
        let t = self.step_instant(abs);

        // The clock runs whether or not there are notes, so anything slaved to
        // this sequencer keeps time through an empty bar.
        for k in 0..PULSES_PER_STEP {
            let tick = t + k as f64 * self.step_us / PULSES_PER_STEP as f64;
            out.push(Emit {
                data: vec![0xF8],
                time: tick.max(0.0) as u64,
            });
        }

        // What should be sounding on this step, across every unmuted track.
        let mut want: Vec<(u8, u8, u8)> = Vec::new();
        if let Some(pos) = locate(song, play, abs) {
            let pattern = &song.patterns[pos.pattern];
            for (index, track) in song.tracks.iter().enumerate() {
                if track.muted {
                    continue;
                }
                for note in pattern.track(index) {
                    if !note.covers(pos.step, pattern.steps) {
                        continue;
                    }
                    let key = (track.channel, note.pitch as u8);
                    if suppress.contains(&key) || want.iter().any(|&(c, p, _)| (c, p) == key) {
                        continue;
                    }
                    want.push((key.0, key.1, note.vel.max(1)));
                }
            }
        }

        let at = t.max(0.0) as u64;
        let offs: Vec<(u8, u8)> = self
            .on
            .iter()
            .copied()
            .filter(|key| !want.iter().any(|&(c, p, _)| (c, p) == *key))
            .collect();
        for (channel, pitch) in offs {
            out.push(Emit {
                data: vec![0x80 | channel, pitch, 0],
                time: at,
            });
        }
        self.on
            .retain(|key| want.iter().any(|&(c, p, _)| (c, p) == *key));
        for (channel, pitch, vel) in want {
            if !self.on.contains(&(channel, pitch)) {
                out.push(Emit {
                    data: vec![0x90 | channel, pitch, vel],
                    time: at,
                });
                self.on.push((channel, pitch));
            }
        }
    }
}

// ---- persistence ----

/// The saved layout is tagged so an older saved pattern still loads. A step is
/// never negative, so nothing written by the first version can begin with this.
const SAVE_TAG: f32 = -1.0;
/// Version 1 was one track and one pattern, with a tempo. Version 2 added
/// tracks, a pattern bank and a chain.
const SAVE_VERSION: f32 = 2.0;

impl Song {
    /// Restore a song from the host's per-node option values.
    ///
    /// Three layouts load: the current one, the single-pattern one that came
    /// before it, and the very first, which was bare `(step, pitch, len)`
    /// triples at a fixed tempo. Someone's saved bar is not worth breaking.
    pub fn from_options(vals: &[f32]) -> Self {
        match vals {
            [tag, version, rest @ ..] if *tag == SAVE_TAG && *version >= 2.0 => {
                Self::from_options_v2(rest).unwrap_or_else(Song::new)
            }
            [tag, version, bpm, steps, rest @ ..] if *tag == SAVE_TAG && *version >= 1.0 => {
                Self::from_flat_notes(*bpm, *steps as i32, rest, 4)
            }
            _ => Self::from_flat_notes(120.0, 16, vals, 3),
        }
    }

    /// The one-track layouts: a flat run of notes, three or four values each.
    fn from_flat_notes(bpm: f32, steps: i32, body: &[f32], stride: usize) -> Song {
        let mut song = Song::new();
        song.set_bpm(bpm);
        let pattern = &mut song.patterns[0];
        pattern.steps = steps.clamp(MIN_STEPS, MAX_STEPS);
        for t in body.chunks_exact(stride) {
            let vel = if stride == 4 {
                (t[3] as i32).clamp(1, 127) as u8
            } else {
                100
            };
            pattern.add_note(0, Note::new(t[0] as i32, t[1] as i32, t[2] as i32, vel));
        }
        song
    }

    fn from_options_v2(body: &[f32]) -> Option<Song> {
        let mut r = body.iter().copied();
        let mut next = || r.next();
        let bpm = next()?;
        let n_tracks = (next()? as usize).min(MAX_TRACKS);
        let n_patterns = (next()? as usize).min(MAX_PATTERNS);
        let n_chain = (next()? as usize).min(MAX_CHAIN);

        let mut song = Song::new();
        song.set_bpm(bpm);
        for i in 0..n_tracks {
            let channel = next()? as u8;
            let muted = next()? != 0.0;
            song.tracks[i] = Track {
                channel: channel.min(15),
                muted,
            };
        }
        song.chain = (0..n_chain)
            .filter_map(|_| next().map(|v| v as usize))
            .collect();

        song.patterns.clear();
        for _ in 0..n_patterns.max(1) {
            let steps = (next()? as i32).clamp(MIN_STEPS, MAX_STEPS);
            let count = next()? as usize;
            let mut pattern = Pattern::empty(steps);
            for _ in 0..count {
                let track = (next()? as usize).min(MAX_TRACKS - 1);
                let step = next()? as i32;
                let pitch = next()? as i32;
                let len = next()? as i32;
                let vel = (next()? as i32).clamp(1, 127) as u8;
                pattern.add_note(track, Note::new(step, pitch, len, vel));
            }
            song.patterns.push(pattern);
        }
        if song.patterns.is_empty() {
            song.patterns.push(Pattern::empty(16));
        }
        // A chain entry pointing past the bank would silently skip a bar.
        song.chain.retain(|&i| i < song.patterns.len());
        Some(song)
    }

    /// Flatten the song for the host to persist.
    pub fn to_options(&self) -> Vec<f32> {
        let mut v = vec![
            SAVE_TAG,
            SAVE_VERSION,
            self.bpm,
            self.tracks.len() as f32,
            self.patterns.len() as f32,
            self.chain.len() as f32,
        ];
        for track in &self.tracks {
            v.push(track.channel as f32);
            v.push(if track.muted { 1.0 } else { 0.0 });
        }
        for &index in &self.chain {
            v.push(index as f32);
        }
        for pattern in &self.patterns {
            v.push(pattern.steps as f32);
            let count: usize = pattern.notes.iter().map(|t| t.len()).sum();
            v.push(count as f32);
            for (track, notes) in pattern.notes.iter().enumerate() {
                for n in notes {
                    v.extend_from_slice(&[
                        track as f32,
                        n.step as f32,
                        n.pitch as f32,
                        n.len as f32,
                        n.vel as f32,
                    ]);
                }
            }
        }
        v
    }
}

#[cfg(test)]
mod tests;
