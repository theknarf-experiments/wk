//! A multi-track step sequencer as a wk node.
//!
//! The music lives in [`wk_sequence`]: the song, the transport arithmetic and
//! the MIDI-file conversion are all in workspace crates, tested natively. What
//! is here is the window — layout, painting, clicks and keys — plus the two
//! things only a node can do: talk to wk's MIDI ports, and read and write the
//! file wired to it on the canvas.

#[allow(warnings)]
mod bindings;

mod paint;
mod song_file;

use std::collections::HashMap;

use bindings::Guest;
use bindings::wasi::frame_buffer::frame_buffer::Device;
use bindings::wasi::graphics_context::graphics_context::Context as GfxContext;
use bindings::wasi::surface::surface::{CreateDesc, Key, Surface};
use bindings::wk::midi::midi::{Input, Output, now};

use song_file::SongFile;
use wk_sequence::{
    Emit, MAX_BPM, MAX_CHAIN, MAX_PATTERNS, MAX_STEPS, MAX_TRACKS, MIN_BPM, MIN_STEPS, Note,
    Pattern, Playback, Scheduler, Song,
};

/// Pitch rows shown at once. The window scrolls by an octave; the notes
/// themselves may sit anywhere in the MIDI range.
pub const ROWS: i32 = 25;
const LOWEST: i32 = 0;
const HIGHEST: i32 = 127;

/// The velocity a note drawn with the mouse gets: mezzo-forte, the middle of
/// the range in practice.
const DEFAULT_VEL: u8 = 100;

/// Vertical pixels of drag that span a field's whole range.
const DRAG_SPAN: f32 = 200.0;

/// Pixels at a note's right edge that grab the resize handle.
const RESIZE_PX: f32 = 7.0;

/// How long a message stays in the transport bar.
const STATUS_FRAMES: u32 = 180;

#[derive(PartialEq, Clone, Copy)]
enum Transport {
    Stopped,
    Playing,
    Recording,
}

/// A mouse edit in progress. Offsets are captured at grab time so a note tracks
/// the cursor without snapping to it.
enum Drag {
    None,
    Move {
        idx: usize,
        doff: i32,
        poff: i32,
    },
    Resize {
        idx: usize,
    },
    /// Painting velocities in the lane under the cursor.
    Velocity,
    /// Turning a transport-bar field by dragging vertically.
    Field {
        which: Field,
        start_y: f32,
        start: f32,
    },
}

#[derive(Clone, Copy, PartialEq)]
enum Field {
    Tempo,
    Length,
    Channel,
}

/// The sequencer node.
struct App {
    out: Output,
    input: Input,
    song: Song,
    sched: Scheduler,
    transport: Transport,
    /// The pattern being edited, and looped when not in song mode.
    pattern: usize,
    /// The track being edited. Its notes are solid in the roll; the others ghost.
    track: usize,
    /// Play the chain rather than the one pattern.
    song_mode: bool,
    /// The lowest pitch row on screen.
    low: i32,
    /// The selected note, as an index into the current pattern's current track.
    selected: Option<usize>,
    /// Songs to step back to, and the ones stepped back from.
    undo: Vec<Song>,
    redo: Vec<Song>,
    /// Notes being recorded: incoming pitch -> the pattern and note it opened,
    /// and the absolute step it started on.
    pending: HashMap<i32, (usize, usize, i64)>,
    /// The song changed and should be re-persisted through wk:options.
    dirty: bool,
    /// The song differs from what the file on disk holds.
    unsaved: bool,
    file: Option<SongFile>,
    status: String,
    status_left: u32,
    /// Whether shift is held. Pointer events carry no modifiers, so the state
    /// is kept from the key events, which do — that is what lets a shift-click
    /// mean something different from a click.
    shift: bool,
}

impl App {
    fn new() -> Self {
        App {
            out: Output::new(),
            input: Input::new(),
            song: Song::new(),
            sched: Scheduler::new(120.0),
            transport: Transport::Stopped,
            pattern: 0,
            track: 0,
            song_mode: false,
            low: 48,
            selected: None,
            undo: Vec::new(),
            redo: Vec::new(),
            pending: HashMap::new(),
            dirty: false,
            unsaved: false,
            file: None,
            status: String::new(),
            status_left: 0,
            shift: false,
        }
    }

    fn say(&mut self, message: impl Into<String>) {
        self.status = message.into();
        self.status_left = STATUS_FRAMES;
    }

    /// What the transport is playing.
    fn playback(&self) -> Playback {
        if self.song_mode {
            Playback::Song
        } else {
            Playback::Pattern(self.pattern)
        }
    }

    /// The pattern being edited.
    fn pattern(&self) -> &Pattern {
        &self.song.patterns[self.pattern.min(self.song.patterns.len() - 1)]
    }

    fn steps(&self) -> i32 {
        self.pattern().steps
    }

    // ---- editing ----

    /// Record the song as it is, so the next edit can be stepped back. Called
    /// once before a change begins, not once per pixel of a drag.
    fn checkpoint(&mut self) {
        self.undo.push(self.song.clone());
        if self.undo.len() > 64 {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    fn undo(&mut self) {
        if let Some(previous) = self.undo.pop() {
            self.redo.push(std::mem::replace(&mut self.song, previous));
            self.after_replace();
            self.say("undo");
        }
    }

    fn redo(&mut self) {
        if let Some(next) = self.redo.pop() {
            self.undo.push(std::mem::replace(&mut self.song, next));
            self.after_replace();
            self.say("redo");
        }
    }

    /// Selection and in-flight recording index into the song, so neither
    /// survives a wholesale replacement of it.
    fn after_replace(&mut self) {
        self.selected = None;
        self.pending.clear();
        self.pattern = self.pattern.min(self.song.patterns.len() - 1);
        self.track = self.track.min(MAX_TRACKS - 1);
        self.sched.set_bpm(self.song.bpm);
        self.dirty = true;
        self.unsaved = true;
    }

    /// Note that the music changed: it needs persisting, and it no longer
    /// matches the file.
    fn touched(&mut self) {
        self.dirty = true;
        self.unsaved = true;
    }

    fn delete_selected(&mut self) {
        let Some(index) = self.selected.take() else {
            return;
        };
        self.checkpoint();
        let (pattern, track) = (self.pattern, self.track);
        if let Some(notes) = self.song.patterns[pattern]
            .notes
            .get_mut(track)
            .filter(|n| index < n.len())
        {
            notes.remove(index);
        }
        self.pending.clear();
        self.touched();
    }

    // ---- transport ----

    fn start(&mut self, mode: Transport, clock: u64) {
        // Pressing the button of the running mode stops; pressing the other
        // switches mode without disturbing the clock.
        if self.transport == mode {
            self.stop();
            return;
        }
        let from_stopped = self.transport == Transport::Stopped;
        self.transport = mode;
        if from_stopped {
            let mut out = Vec::new();
            self.sched.start(clock, &mut out);
            self.emit(out);
        }
    }

    fn stop(&mut self) {
        if self.transport == Transport::Stopped {
            return;
        }
        let mut out = Vec::new();
        self.sched.stop(&mut out);
        self.emit(out);
        self.transport = Transport::Stopped;
        self.pending.clear();
    }

    fn emit(&self, events: Vec<Emit>) {
        for event in events {
            self.out.send_at(&event.data, event.time);
        }
    }

    /// Queue the music falling inside the look-ahead window.
    fn pump(&mut self, clock: u64) {
        // Pitches the player is holding down: MIDI thru is already sounding
        // them, so the scheduler must not trigger them a second time.
        let channel = self.song.channel(self.track);
        let suppress: Vec<(u8, u8)> = self
            .pending
            .keys()
            .map(|&pitch| (channel, pitch as u8))
            .collect();
        let mut out = Vec::new();
        let play = self.playback();
        self.sched
            .pump(clock, &self.song, play, &suppress, &mut out);
        self.emit(out);
    }

    /// Drain incoming MIDI: while recording, open a note on note-on and close it
    /// on note-off, both placed by the instant the message carries rather than
    /// by the frame it was drained on. Every message is passed through to the
    /// output, which is what lets you hear what you are playing.
    fn pump_input(&mut self) {
        while let Some(event) = self.input.receive_event() {
            let msg = &event.data;
            if self.transport == Transport::Recording && msg.len() >= 3 {
                let status = msg[0] & 0xF0;
                let pitch = msg[1] as i32;
                let vel = msg[2];
                let when = if event.time == 0 { now() } else { event.time };
                if status == 0x90 && vel > 0 {
                    self.record_on(pitch, vel, when);
                } else if status == 0x80 || (status == 0x90 && vel == 0) {
                    self.record_off(pitch, when);
                }
            }
            self.out.send_at(msg, event.time);
        }
    }

    fn record_on(&mut self, pitch: i32, vel: u8, when: u64) {
        if !(LOWEST..=HIGHEST).contains(&pitch) || self.pending.contains_key(&pitch) {
            return;
        }
        let abs = self.sched.step_of(when);
        // In song mode the note belongs to whichever pattern is playing.
        let Some(position) = wk_sequence::locate(&self.song, self.playback(), abs) else {
            return;
        };
        if self.pending.is_empty() {
            self.checkpoint();
        }
        let track = self.track;
        let note = Note::new(position.step, pitch, 1, vel);
        if let Some(index) = self.song.patterns[position.pattern].add_note(track, note) {
            self.pending.insert(pitch, (position.pattern, index, abs));
            self.touched();
        }
    }

    fn record_off(&mut self, pitch: i32, when: u64) {
        let Some((pattern, index, started)) = self.pending.remove(&pitch) else {
            return;
        };
        // Length comes from when the key was actually released, so a held note
        // records as held.
        let held = (self.sched.step_of(when) - started).max(1) as i32;
        let steps = self.song.patterns[pattern].steps;
        if let Some(note) = self.song.patterns[pattern]
            .notes
            .get_mut(self.track)
            .and_then(|t| t.get_mut(index))
        {
            note.len = held.clamp(1, (steps - note.step).max(1));
            self.dirty = true;
            self.unsaved = true;
        }
    }

    // ---- the file ----

    /// Look for a MIDI file wired to this node, and open it.
    ///
    /// Called at startup and then periodically, because a file can be wired to
    /// a node that is already running — noticing it is the same courtesy the
    /// canvas extends everywhere else. A file that turns up while there is
    /// unsaved work here is adopted as the place to save *to* rather than read
    /// from: nobody wants their bar replaced by a wire.
    fn open_file(&mut self) {
        let Some(mut file) = song_file::find() else {
            return;
        };
        let name = file.name();
        if self.unsaved {
            self.say(format!("{name} wired — Cmd+S writes to it"));
            self.file = Some(file);
            return;
        }
        match file.load(self.steps()) {
            Ok(Some(song)) => {
                self.song = song;
                self.sched.set_bpm(self.song.bpm);
                self.pattern = 0;
                self.track = 0;
                self.song_mode = !self.song.chain.is_empty();
                self.selected = None;
                self.focus_on_the_music();
                self.say(format!("opened {name}"));
                self.dirty = true;
                self.unsaved = false;
            }
            // An empty file is a place to save to, not a failure.
            Ok(None) => self.say(format!("{name} is empty — Cmd+S writes to it")),
            Err(e) => self.say(format!("{name}: {e}")),
        }
        self.file = Some(file);
    }

    /// Write the song to the wired file.
    fn save_file(&mut self) {
        let order = self.export_order();
        let Some(mut file) = self.file.take() else {
            self.say("no MIDI file wired to this node");
            return;
        };
        let name = file.name();
        match file.save(&self.song, &order) {
            Ok(()) => {
                self.unsaved = false;
                self.say(format!("saved {name}"));
            }
            Err(e) => self.say(format!("{name}: {e}")),
        }
        self.file = Some(file);
    }

    /// What to persist through `wk:options`.
    ///
    /// Nothing, when a MIDI file is the document. The file already holds the
    /// song and wins at startup, so writing it here too would bloat every
    /// workspace file with a copy of the music and leave two sources of truth
    /// to drift apart. A track mute is the one thing a MIDI file cannot carry;
    /// losing it on reload is a smaller cost than that divergence.
    fn options(&self) -> Vec<f32> {
        match self.file {
            Some(_) => Vec::new(),
            None => self.song.to_options(),
        }
    }

    /// Which patterns an export lays end to end: the chain in song mode, or the
    /// pattern being looped. Exporting what you hear surprises nobody.
    fn export_order(&self) -> Vec<usize> {
        if self.song_mode && !self.song.chain.is_empty() {
            self.song.chain.clone()
        } else {
            vec![self.pattern]
        }
    }

    /// Pick the file up again if it changed under us — but only when there is
    /// nothing here that would be lost by doing so.
    fn reload_if_changed(&mut self) {
        if self.unsaved || self.transport != Transport::Stopped {
            return;
        }
        let Some(file) = &self.file else { return };
        if !file.changed_on_disk() {
            return;
        }
        let mut file = self.file.take().expect("just checked");
        let name = file.name();
        if let Ok(Some(song)) = file.load(self.steps()) {
            self.song = song;
            self.sched.set_bpm(self.song.bpm);
            self.after_replace();
            self.unsaved = false;
            self.say(format!("reloaded {name}"));
        }
        self.file = Some(file);
    }

    /// Scroll the pitch window to the track being edited, if none of its notes
    /// are on screen.
    ///
    /// Selecting a part and finding an empty grid — because the bass is two
    /// octaves below what the window happens to show — reads as "this track is
    /// empty", which is the wrong answer to a question nobody asked.
    fn focus_on_the_music(&mut self) {
        let mut pitches: Vec<i32> = self
            .song
            .patterns
            .iter()
            .flat_map(|p| p.track(self.track))
            .map(|n| n.pitch)
            .collect();
        if pitches.is_empty() {
            // Nothing on this track: fall back to wherever the song sits, so a
            // fresh track opens in the same register as the rest of it.
            pitches = self
                .song
                .patterns
                .iter()
                .flat_map(|p| p.notes.iter().flatten())
                .map(|n| n.pitch)
                .collect();
        }
        let (Some(&low), Some(&high)) = (pitches.iter().min(), pitches.iter().max()) else {
            return;
        };
        // Only leave the view alone when the whole part is already on screen.
        // "One note visible" is not good enough: a bass with its top note in
        // range and the rest below it looks like an almost-empty track.
        if low >= self.low && high < self.low + ROWS {
            return;
        }
        // Centre the part's range in the window as best it fits.
        let middle = (low + high) / 2;
        self.low = (middle - ROWS / 2).clamp(LOWEST, HIGHEST - ROWS + 1);
    }

    // ---- settings ----

    fn set_bpm(&mut self, bpm: f32) {
        let bpm = bpm.clamp(MIN_BPM, MAX_BPM);
        if (bpm - self.song.bpm).abs() < 0.005 {
            return;
        }
        self.song.set_bpm(bpm);
        self.sched.set_bpm(bpm);
        self.touched();
    }

    fn set_steps(&mut self, steps: i32) {
        let steps = steps.clamp(MIN_STEPS, MAX_STEPS);
        let pattern = self.pattern;
        if self.song.patterns[pattern].steps != steps {
            self.song.patterns[pattern].steps = steps;
            self.touched();
        }
    }

    fn set_channel(&mut self, channel: i32) {
        let channel = channel.clamp(0, 15) as u8;
        if self.song.tracks[self.track].channel != channel {
            self.song.tracks[self.track].channel = channel;
            self.touched();
        }
    }

    /// Select a pattern, creating it if the slot is the next empty one.
    fn select_pattern(&mut self, index: usize) {
        if index < self.song.patterns.len() {
            self.pattern = index;
            self.selected = None;
        } else if index == self.song.patterns.len() && index < MAX_PATTERNS {
            self.checkpoint();
            if let Some(new) = self.song.add_pattern(self.pattern) {
                self.pattern = new;
                self.selected = None;
                self.touched();
            }
        }
    }

    fn append_to_chain(&mut self, index: usize) {
        if index >= self.song.patterns.len() || self.song.chain.len() >= MAX_CHAIN {
            return;
        }
        self.checkpoint();
        self.song.chain.push(index);
        self.touched();
        self.say(format!("pattern {} added to the song", index + 1));
    }

    fn remove_from_chain(&mut self, at: usize) {
        if at >= self.song.chain.len() {
            return;
        }
        self.checkpoint();
        self.song.chain.remove(at);
        if self.song.chain.is_empty() {
            self.song_mode = false;
        }
        self.touched();
    }
}

// ---- the window ----

use bindings::wasi::frame_buffer::frame_buffer::Buffer;
use paint::{BTN_W, CTRL_H, FIELD_W, Layout, TRACK_H, layout};

struct Component;

impl Guest for Component {
    fn run() {
        let surface = Surface::new(CreateDesc {
            width: Some(820),
            height: Some(560),
        });
        let ctx = GfxContext::new();
        surface.connect_graphics_context(&ctx);
        let device = Device::new();
        device.connect_graphics_context(&ctx);
        let frame = surface.subscribe_frame();

        let mut app = App::new();
        app.song = Song::from_options(&bindings::wk::options::options::load());
        app.sched = Scheduler::new(app.song.bpm);
        app.song_mode = !app.song.chain.is_empty();
        app.focus_on_the_music();
        // A wired MIDI file is the document, so it wins over the saved options.
        app.open_file();

        let mut drag = Drag::None;
        // Checking the file's timestamp every frame would be wasteful; twice a
        // second is faster than anyone can switch windows.
        let mut until_file_check = 0u32;

        loop {
            frame.block();
            let _ = surface.get_frame();
            let w = surface.width().max(1);
            let h = surface.height().max(1);
            let lay = layout(w, h, app.steps());
            let clock = now();

            // Keys first: a shift held down this frame has to be known before
            // the click that it modifies is read.
            keyboard(&mut app, &surface, &mut drag, clock);
            pointer(&mut app, &surface, &lay, &mut drag, clock);

            // Incoming MIDI (record capture and thru), then queue the music
            // falling inside the look-ahead window.
            app.pump_input();
            app.pump(clock);

            if until_file_check == 0 {
                if app.file.is_none() {
                    // A file may be wired to the node after it starts, and the
                    // node it is wired to should open it.
                    app.open_file();
                } else {
                    app.reload_if_changed();
                }
                until_file_check = 30;
            }
            until_file_check -= 1;

            if app.dirty {
                bindings::wk::options::options::store(&app.options());
                app.dirty = false;
            }
            if app.status_left > 0 {
                app.status_left -= 1;
                if app.status_left == 0 {
                    app.status.clear();
                }
            }

            let at = paint::playing_at(&app, clock);
            let pixels = paint::paint(&app, &lay, w, h, at);
            Buffer::from_graphics_buffer(ctx.get_current_buffer()).set(&pixels);
            ctx.present();
        }
    }
}

fn pointer(app: &mut App, surface: &Surface, lay: &Layout, drag: &mut Drag, clock: u64) {
    while let Some(ev) = surface.get_pointer_down() {
        let (px, py) = (ev.x as f32, ev.y as f32);
        let shift = app.shift;

        if py < CTRL_H {
            transport_click(app, lay, drag, px, py, clock);
        } else if py < CTRL_H + TRACK_H {
            if let Some(index) = lay.to_track(px) {
                // Shift picks the mute, because muting is the thing you do
                // *without* wanting to move the edit cursor.
                if shift {
                    app.checkpoint();
                    app.song.tracks[index].muted = !app.song.tracks[index].muted;
                    app.touched();
                } else {
                    app.track = index;
                    app.selected = None;
                    app.focus_on_the_music();
                }
            }
        } else if py >= lay.chain_y {
            if let Some(slot) = lay.to_slot(px, app.song.chain.len()) {
                app.remove_from_chain(slot);
            }
        } else if py >= lay.pat_y {
            if let Some(index) = lay.to_slot(px, MAX_PATTERNS) {
                if shift {
                    app.append_to_chain(index);
                } else {
                    app.select_pattern(index);
                }
            }
        } else if py >= lay.vel_y0 {
            // The velocity lane: drag across it to shape the dynamics, the way
            // a piano roll has done for thirty years.
            app.checkpoint();
            paint_velocity(app, lay, px, py);
            *drag = Drag::Velocity;
        } else if px >= lay.gx0 {
            roll_click(app, lay, drag, px, py);
        }
    }

    while let Some(ev) = surface.get_pointer_move() {
        let (px, py) = (ev.x as f32, ev.y as f32);
        let (pattern, track) = (app.pattern, app.track);
        let steps = app.steps();
        match *drag {
            Drag::Move { idx, doff, poff } => {
                let step = (lay.to_step(px) + doff).max(0);
                let pitch = (lay.to_pitch(py, app.low) + poff).clamp(LOWEST, HIGHEST);
                if let Some(note) = app.song.patterns[pattern]
                    .notes
                    .get_mut(track)
                    .and_then(|t| t.get_mut(idx))
                {
                    let step = step.min((steps - note.len).max(0));
                    if note.step != step || note.pitch != pitch {
                        note.step = step;
                        note.pitch = pitch;
                        app.touched();
                    }
                }
            }
            Drag::Resize { idx } => {
                if let Some(note) = app.song.patterns[pattern]
                    .notes
                    .get_mut(track)
                    .and_then(|t| t.get_mut(idx))
                {
                    let len = (lay.to_step(px) - note.step + 1).clamp(1, steps - note.step);
                    if note.len != len {
                        note.len = len;
                        app.touched();
                    }
                }
            }
            Drag::Velocity => paint_velocity(app, lay, px, py),
            Drag::Field {
                which,
                start_y,
                start,
            } => {
                let travel = (start_y - py) / DRAG_SPAN;
                match which {
                    Field::Tempo => app.set_bpm(start + travel * (MAX_BPM - MIN_BPM)),
                    Field::Length => {
                        let span = (MAX_STEPS - MIN_STEPS) as f32;
                        app.set_steps((start + travel * span).round() as i32);
                    }
                    Field::Channel => app.set_channel((start + travel * 16.0).round() as i32),
                }
            }
            Drag::None => {}
        }
    }

    while surface.get_pointer_up().is_some() {
        *drag = Drag::None;
    }
}

fn transport_click(app: &mut App, lay: &Layout, drag: &mut Drag, px: f32, py: f32, clock: u64) {
    if lay.in_button(px, py, lay.play_x, BTN_W) {
        app.start(Transport::Playing, clock);
    } else if lay.in_button(px, py, lay.rec_x, BTN_W) {
        app.start(Transport::Recording, clock);
    } else if lay.in_button(px, py, lay.bpm_x, FIELD_W) {
        *drag = Drag::Field {
            which: Field::Tempo,
            start_y: py,
            start: app.song.bpm,
        };
    } else if lay.in_button(px, py, lay.len_x, FIELD_W) {
        *drag = Drag::Field {
            which: Field::Length,
            start_y: py,
            start: app.steps() as f32,
        };
    } else if lay.in_button(px, py, lay.chan_x, FIELD_W) {
        *drag = Drag::Field {
            which: Field::Channel,
            start_y: py,
            start: app.song.channel(app.track) as f32,
        };
    } else if lay.in_button(px, py, lay.song_x, FIELD_W) {
        app.song_mode = !app.song_mode;
        if app.song_mode && app.song.chain.is_empty() {
            app.song_mode = false;
            app.say("the song is empty — shift-click a pattern to add it");
        }
    }
}

fn roll_click(app: &mut App, lay: &Layout, drag: &mut Drag, px: f32, py: f32) {
    let step = lay.to_step(px);
    let pitch = lay.to_pitch(py, app.low);
    let steps = app.steps();
    if !(0..steps).contains(&step) || !(LOWEST..=HIGHEST).contains(&pitch) {
        return;
    }
    let (pattern, track) = (app.pattern, app.track);
    match app.song.patterns[pattern].note_at(track, step, pitch) {
        Some(idx) => {
            app.selected = Some(idx);
            app.checkpoint();
            let note = app.song.patterns[pattern].track(track)[idx];
            let right = lay.gx0 + (note.step + note.len) as f32 * lay.cell_w;
            *drag = if px >= right - RESIZE_PX {
                Drag::Resize { idx }
            } else {
                Drag::Move {
                    idx,
                    doff: note.step - step,
                    poff: note.pitch - pitch,
                }
            };
        }
        None => {
            // Empty space: draw a one-step note and grab its edge, so a
            // horizontal drag sets its length.
            app.checkpoint();
            let note = Note::new(step, pitch, 1, DEFAULT_VEL);
            if let Some(idx) = app.song.patterns[pattern].add_note(track, note) {
                app.selected = Some(idx);
                *drag = Drag::Resize { idx };
                app.touched();
            }
        }
    }
}

fn keyboard(app: &mut App, surface: &Surface, drag: &mut Drag, clock: u64) {
    // The wheel scrolls the pitch window, a semitone at a time.
    while let Some(ev) = surface.get_pointer_scroll() {
        if ev.delta_y != 0.0 {
            let delta = if ev.delta_y > 0.0 { 1 } else { -1 };
            app.low = (app.low + delta).clamp(LOWEST, HIGHEST - ROWS + 1);
        }
    }
    while let Some(ev) = surface.get_key_down() {
        let cmd = ev.meta_key || ev.ctrl_key;
        app.shift = ev.shift_key;
        match ev.key {
            Some(Key::Space) => app.start(Transport::Playing, clock),
            Some(Key::KeyR) if !cmd => app.start(Transport::Recording, clock),
            Some(Key::KeyS) if cmd => app.save_file(),
            Some(Key::KeyZ) if cmd && ev.shift_key => app.redo(),
            Some(Key::KeyZ) if cmd => app.undo(),
            Some(Key::KeyD) if cmd => {
                // Duplicate the bar: how a variation gets made.
                app.checkpoint();
                if let Some(new) = app.song.clone_pattern(app.pattern) {
                    app.pattern = new;
                    app.selected = None;
                    app.touched();
                    app.say(format!("pattern {} duplicated", new + 1));
                }
            }
            Some(Key::Backspace) | Some(Key::Delete) => {
                app.delete_selected();
                *drag = Drag::None;
            }
            // Scroll the pitch window an octave at a time.
            Some(Key::ArrowUp) => app.low = (app.low + 12).min(HIGHEST - ROWS + 1),
            Some(Key::ArrowDown) => app.low = (app.low - 12).max(LOWEST),
            Some(Key::ArrowLeft) | Some(Key::ArrowRight) => {
                let delta = if ev.key == Some(Key::ArrowLeft) {
                    -1
                } else {
                    1
                };
                nudge_selected(app, delta);
            }
            // Number keys pick a track, the way a groovebox does.
            Some(key) => {
                if let Some(index) = track_key(key) {
                    app.track = index;
                    app.selected = None;
                    app.focus_on_the_music();
                }
            }
            None => {}
        }
    }
    while let Some(ev) = surface.get_key_up() {
        app.shift = ev.shift_key;
    }
}

/// The track a number key selects.
fn track_key(key: Key) -> Option<usize> {
    Some(match key {
        Key::Digit1 => 0,
        Key::Digit2 => 1,
        Key::Digit3 => 2,
        Key::Digit4 => 3,
        Key::Digit5 => 4,
        Key::Digit6 => 5,
        Key::Digit7 => 6,
        Key::Digit8 => 7,
        _ => return None,
    })
}

fn nudge_selected(app: &mut App, delta: i32) {
    let Some(index) = app.selected.filter(|_| true) else {
        return;
    };
    let (pattern, track) = (app.pattern, app.track);
    let steps = app.steps();
    app.checkpoint();
    if let Some(note) = app.song.patterns[pattern]
        .notes
        .get_mut(track)
        .and_then(|t| t.get_mut(index))
    {
        note.step = (note.step + delta).clamp(0, (steps - note.len).max(0));
        app.touched();
    } else {
        app.undo.pop(); // nothing changed, so do not leave an empty undo step
    }
}

/// Set the velocity of the notes starting in the column under the cursor, from
/// how high in the lane it is.
fn paint_velocity(app: &mut App, lay: &Layout, px: f32, py: f32) {
    let step = lay.to_step(px);
    let steps = app.steps();
    if !(0..steps).contains(&step) {
        return;
    }
    let f = ((lay.vel_y1 - py) / (lay.vel_y1 - lay.vel_y0)).clamp(0.0, 1.0);
    let vel = (f * 126.0) as u8 + 1;
    let (pattern, track) = (app.pattern, app.track);
    let mut changed = false;
    if let Some(notes) = app.song.patterns[pattern].notes.get_mut(track) {
        for note in notes.iter_mut().filter(|n| n.step == step) {
            if note.vel != vel {
                note.vel = vel;
                changed = true;
            }
        }
    }
    if changed {
        app.touched();
    }
}

bindings::export!(Component with_types_in bindings);
