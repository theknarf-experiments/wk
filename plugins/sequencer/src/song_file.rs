//! The sequencer's document: a Standard MIDI File in the node's own filesystem.
//!
//! Wire a file to the node on the canvas and the sequencer opens it; press
//! Cmd+S and it writes it back. That is the whole of it, and it is the point:
//! the work leaves. A pattern that only exists inside the node is a sketch, and
//! a `.mid` is the one format every other music program reads.
//!
//! The node's per-node options still hold the song as well, so a workspace
//! reopens where it was left without a file wired at all. A file, when there is
//! one, wins at startup — it is the thing the user can see and back up.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use wk_midifile::MidiFile;
use wk_sequence::Song;

/// The file the song is read from and written to.
pub struct SongFile {
    pub path: PathBuf,
    /// The file's timestamp when it was last read or written, so an edit made
    /// outside can be noticed without re-reading every frame.
    stamp: Option<SystemTime>,
}

/// The first MIDI file mounted into this node, if any.
///
/// Only the root is searched, and only one level deep, because that is where a
/// wire puts a file: a `BindMount` of `riff.mid` appears as `/riff.mid`.
pub fn find() -> Option<SongFile> {
    let mut found: Vec<PathBuf> = std::fs::read_dir("/")
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("mid") || e.eq_ignore_ascii_case("midi"))
        })
        .collect();
    // Stable choice when more than one is mounted, rather than whatever order
    // the directory happens to yield.
    found.sort();
    found
        .into_iter()
        .next()
        .map(|path| SongFile { path, stamp: None })
}

impl SongFile {
    /// The name to show in the window.
    pub fn name(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// Read the file into a song of `steps_per_pattern`-long patterns.
    ///
    /// An empty file is not an error: wiring a fresh, zero-length file to the
    /// node is how you say "save here", and it should not read as a failure.
    pub fn load(&mut self, steps_per_pattern: i32) -> Result<Option<Song>, String> {
        let bytes = std::fs::read(&self.path).map_err(|e| format!("read: {e}"))?;
        self.stamp = stamp_of(&self.path);
        if bytes.is_empty() {
            return Ok(None);
        }
        let file = MidiFile::parse(&bytes).map_err(|e| e.to_string())?;
        Ok(Some(Song::from_midi_file(&file, steps_per_pattern)))
    }

    /// Write `song` out, playing `order`'s patterns one after another.
    pub fn save(&mut self, song: &Song, order: &[usize]) -> Result<(), String> {
        let bytes = song.to_midi_file(order).write();
        std::fs::write(&self.path, &bytes).map_err(|e| format!("write: {e}"))?;
        self.stamp = stamp_of(&self.path);
        Ok(())
    }

    /// Has the file changed on disk since it was last read or written?
    ///
    /// The sequencer only acts on this when its own song is unmodified, so
    /// editing the file in another program reloads it here, and an edit in
    /// progress is never thrown away by one.
    pub fn changed_on_disk(&self) -> bool {
        let now = stamp_of(&self.path);
        now.is_some() && now != self.stamp
    }
}

fn stamp_of(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}
