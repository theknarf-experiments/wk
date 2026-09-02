use super::*;
use wk_midifile::{Event, EventKind, MidiFile};

/// A song with one note per named track, for the scheduler tests: pitch 60 on
/// track 0 at step 0, pitch 64 on track 1 at step 2, in a 4-step pattern.
fn two_track_song() -> Song {
    let mut song = Song::new();
    song.set_bpm(120.0);
    let pattern = &mut song.patterns[0];
    pattern.steps = 4;
    pattern.add_note(0, Note::new(0, 60, 1, 100));
    pattern.add_note(1, Note::new(2, 64, 1, 80));
    song
}

/// The note-ons in `out`, as `(channel, pitch, velocity, time)`.
fn note_ons(out: &[Emit]) -> Vec<(u8, u8, u8, u64)> {
    out.iter()
        .filter(|e| e.data[0] & 0xF0 == 0x90)
        .map(|e| (e.data[0] & 0x0F, e.data[1], e.data[2], e.time))
        .collect()
}

fn note_offs(out: &[Emit]) -> Vec<(u8, u8, u64)> {
    out.iter()
        .filter(|e| e.data[0] & 0xF0 == 0x80)
        .map(|e| (e.data[0] & 0x0F, e.data[1], e.time))
        .collect()
}

// ---- the transport ----

#[test]
fn steps_land_on_the_tempo_to_the_microsecond() {
    // 120 BPM is a sixteenth note every 125 ms exactly.
    let song = two_track_song();
    let mut s = Scheduler::new(120.0);
    let mut out = Vec::new();
    s.start(0, &mut out);
    // Far enough on that the look-ahead window has reached both steps; the
    // instants they carry are absolute, so when they were queued is irrelevant.
    s.pump(500_000, &song, Playback::Pattern(0), &[], &mut out);

    let ons = note_ons(&out);
    let first = ons.iter().find(|&&(_, p, _, _)| p == 60).unwrap();
    let second = ons.iter().find(|&&(_, p, _, _)| p == 64).unwrap();
    assert_eq!(first.3, 0, "step 0 is the instant the transport started");
    assert_eq!(second.3, 250_000, "step 2 is two steps of 125ms later");
}

#[test]
fn a_note_is_released_after_exactly_its_length() {
    let mut song = Song::new();
    song.patterns[0].steps = 8;
    song.patterns[0].add_note(0, Note::new(0, 60, 3, 100));
    let mut s = Scheduler::new(120.0);
    let mut out = Vec::new();
    s.start(0, &mut out);
    s.pump(500_000, &song, Playback::Pattern(0), &[], &mut out);

    let on = note_ons(&out)[0].3;
    let off = note_offs(&out)[0].2;
    assert_eq!(off - on, 3 * 125_000, "three steps at 125ms each");
}

#[test]
fn the_clock_runs_at_twenty_four_pulses_to_the_quarter() {
    let song = Song::new();
    let mut s = Scheduler::new(150.0);
    let mut out = Vec::new();
    s.start(0, &mut out);
    s.pump(1_000_000, &song, Playback::Pattern(0), &[], &mut out);

    let clocks: Vec<u64> = out
        .iter()
        .filter(|e| e.data == vec![0xF8])
        .map(|e| e.time)
        .collect();
    assert!(clocks.len() > 24, "the clock runs through an empty bar");
    // A quarter note at 150 BPM is 400ms and holds 24 pulses.
    assert_eq!(clocks[24] - clocks[0], 400_000);
}

#[test]
fn a_tempo_change_moves_the_future_and_leaves_the_past_alone() {
    // This is what lets the tempo be dragged while the music plays: the next
    // boundary stays where it is, and everything after it follows the new rate.
    let song = two_track_song();
    let mut s = Scheduler::new(120.0);
    let mut out = Vec::new();
    s.start(0, &mut out);
    s.pump(0, &song, Playback::Pattern(0), &[], &mut out);
    let queued: Vec<u64> = out.iter().map(|e| e.time).collect();
    let pivot = s.step_instant(1); // the first boundary not yet sent

    s.set_bpm(240.0);
    assert_eq!(
        s.step_instant(1),
        pivot,
        "the next boundary does not jump when the tempo changes"
    );
    assert_eq!(
        out.iter().map(|e| e.time).collect::<Vec<_>>(),
        queued,
        "nothing already sent is rewritten"
    );

    // From there the steps are half as long.
    let after = s.step_instant(2) - s.step_instant(1);
    assert!((after - 62_500.0).abs() < 0.001, "240 BPM is 62.5ms a step");
}

#[test]
fn stopping_releases_what_is_sounding_after_what_is_queued() {
    // A note-off stamped "now" would land before note-ons already scheduled
    // ahead of the clock, and those notes would sound forever.
    let mut song = Song::new();
    song.patterns[0].steps = 4;
    song.patterns[0].add_note(0, Note::new(0, 60, 4, 100));
    let mut s = Scheduler::new(120.0);
    let mut out = Vec::new();
    s.start(0, &mut out);
    s.pump(0, &song, Playback::Pattern(0), &[], &mut out);
    let latest_on = note_ons(&out).iter().map(|&(_, _, _, t)| t).max().unwrap();

    out.clear();
    s.stop(&mut out);
    let release = note_offs(&out)[0].2;
    assert!(
        release >= latest_on,
        "release at {release} must not precede the last queued note-on at {latest_on}"
    );
    assert!(out.iter().any(|e| e.data == vec![0xFC]), "and it says stop");
}

#[test]
fn each_track_plays_on_its_own_channel() {
    let song = two_track_song();
    let mut s = Scheduler::new(120.0);
    let mut out = Vec::new();
    s.start(0, &mut out);
    s.pump(500_000, &song, Playback::Pattern(0), &[], &mut out);

    let ons = note_ons(&out);
    assert!(ons.contains(&(0, 60, 100, 0)), "track 1 on channel 1");
    assert!(
        ons.iter().any(|&(c, p, v, _)| (c, p, v) == (1, 64, 80)),
        "track 2 on channel 2, at its own velocity: {ons:?}"
    );
}

#[test]
fn a_muted_track_sends_nothing() {
    let mut song = two_track_song();
    song.tracks[1].muted = true;
    let mut s = Scheduler::new(120.0);
    let mut out = Vec::new();
    s.start(0, &mut out);
    s.pump(0, &song, Playback::Pattern(0), &[], &mut out);
    assert!(
        note_ons(&out).iter().all(|&(c, _, _, _)| c == 0),
        "the muted track's channel is silent"
    );
}

#[test]
fn a_pitch_held_live_is_not_doubled_while_recording() {
    let song = two_track_song();
    let mut s = Scheduler::new(120.0);
    let mut out = Vec::new();
    s.start(0, &mut out);
    // The player is holding C4 on channel 1; MIDI thru already sounds it.
    s.pump(0, &song, Playback::Pattern(0), &[(0, 60)], &mut out);
    assert!(
        !note_ons(&out).iter().any(|&(c, p, _, _)| (c, p) == (0, 60)),
        "the held note is left to the thru path"
    );
}

// ---- song mode ----

#[test]
fn the_chain_plays_its_patterns_in_order_and_loops() {
    let mut song = Song::new();
    song.patterns[0] = Pattern::empty(4);
    song.patterns[0].add_note(0, Note::new(0, 60, 1, 100));
    song.patterns.push(Pattern::empty(2));
    song.patterns[1].add_note(0, Note::new(0, 67, 1, 100));
    song.chain = vec![0, 1];

    // Six steps: pattern 0 (four), pattern 1 (two), then round again.
    let seen: Vec<Position> = (0..7)
        .map(|i| locate(&song, Playback::Song, i).unwrap())
        .collect();
    assert_eq!(
        seen[0],
        Position {
            pattern: 0,
            step: 0,
            chain_index: 0
        }
    );
    assert_eq!(
        seen[3],
        Position {
            pattern: 0,
            step: 3,
            chain_index: 0
        }
    );
    assert_eq!(
        seen[4],
        Position {
            pattern: 1,
            step: 0,
            chain_index: 1
        }
    );
    assert_eq!(
        seen[5],
        Position {
            pattern: 1,
            step: 1,
            chain_index: 1
        }
    );
    assert_eq!(seen[6], seen[0], "and the whole chain loops");
}

#[test]
fn a_chain_entry_pointing_nowhere_is_skipped_not_played_as_silence() {
    let mut song = Song::new();
    song.patterns[0] = Pattern::empty(2);
    song.chain = vec![0, 9, 0];
    // Only the two real entries contribute, so the chain is four steps.
    assert_eq!(locate(&song, Playback::Song, 0).unwrap().chain_index, 0);
    assert_eq!(locate(&song, Playback::Song, 2).unwrap().chain_index, 2);
    assert_eq!(locate(&song, Playback::Song, 4).unwrap().chain_index, 0);
}

#[test]
fn an_empty_chain_plays_nothing_rather_than_panicking() {
    let song = Song::new();
    assert_eq!(locate(&song, Playback::Song, 0), None);

    // And the scheduler still runs its clock, so slaved gear keeps time.
    let mut s = Scheduler::new(120.0);
    let mut out = Vec::new();
    s.start(0, &mut out);
    s.pump(200_000, &song, Playback::Song, &[], &mut out);
    assert!(out.iter().any(|e| e.data == vec![0xF8]));
    assert!(note_ons(&out).is_empty());
}

// ---- persistence ----

#[test]
fn a_song_survives_being_saved_and_restored() {
    let mut song = Song::new();
    song.set_bpm(137.0);
    song.tracks[2] = Track {
        channel: 9,
        muted: true,
    };
    song.patterns[0].steps = 12;
    song.patterns[0].add_note(0, Note::new(3, 60, 2, 90));
    song.patterns[0].add_note(2, Note::new(0, 36, 1, 127));
    song.patterns.push(Pattern::empty(8));
    song.patterns[1].add_note(1, Note::new(4, 72, 4, 55));
    song.chain = vec![0, 1, 1];

    assert_eq!(Song::from_options(&song.to_options()), song);
}

#[test]
fn patterns_saved_by_older_versions_still_load() {
    // The very first layout: bare (step, pitch, len) triples at 120 BPM.
    let v0 = Song::from_options(&[5.0, 64.0, 1.0, 9.0, 67.0, 2.0]);
    assert_eq!(v0.patterns[0].track(0).len(), 2);
    assert_eq!(v0.patterns[0].track(0)[0], Note::new(5, 64, 1, 100));
    assert_eq!(v0.bpm, 120.0);

    // The second: tagged, with a tempo, a length and velocities.
    let v1 = Song::from_options(&[-1.0, 1.0, 96.0, 8.0, 0.0, 48.0, 2.0, 77.0]);
    assert_eq!(v1.bpm, 96.0);
    assert_eq!(v1.patterns[0].steps, 8);
    assert_eq!(v1.patterns[0].track(0)[0], Note::new(0, 48, 2, 77));
}

#[test]
fn nonsense_option_values_give_a_usable_song_rather_than_a_panic() {
    for vals in [
        vec![],
        vec![-1.0],
        vec![-1.0, 2.0],
        vec![-1.0, 2.0, 120.0, 99.0, 99.0, 99.0],
        vec![-1.0, 2.0, f32::NAN, 1.0, 1.0, 0.0],
    ] {
        let song = Song::from_options(&vals);
        assert!(!song.patterns.is_empty(), "always at least one pattern");
        assert!((MIN_BPM..=MAX_BPM).contains(&song.bpm) || song.bpm.is_nan());
    }
}

// ---- MIDI files ----

#[test]
fn a_song_survives_a_round_trip_through_a_midi_file() {
    let mut song = Song::new();
    song.set_bpm(140.0);
    song.tracks[0] = Track::new(0);
    song.tracks[1] = Track::new(9);
    song.patterns[0] = Pattern::empty(16);
    song.patterns[0].add_note(0, Note::new(0, 60, 4, 100));
    song.patterns[0].add_note(0, Note::new(8, 64, 2, 80));
    song.patterns[0].add_note(1, Note::new(0, 36, 1, 127));
    song.patterns[0].add_note(1, Note::new(4, 38, 1, 90));

    let file = song.to_midi_file(&[0]);
    let bytes = file.write();
    let back = Song::from_midi_file(&MidiFile::parse(&bytes).expect("parses"), 16);

    assert!(
        (back.bpm - 140.0).abs() < 0.5,
        "tempo survives: {}",
        back.bpm
    );
    assert_eq!(back.tracks[0].channel, 0);
    assert_eq!(back.tracks[1].channel, 9);
    let mut a = song.patterns[0].track(0).to_vec();
    let mut b = back.patterns[0].track(0).to_vec();
    a.sort_by_key(|n| n.step);
    b.sort_by_key(|n| n.step);
    assert_eq!(a, b, "the melody comes back exactly");
    let mut a = song.patterns[0].track(1).to_vec();
    let mut b = back.patterns[0].track(1).to_vec();
    a.sort_by_key(|n| n.step);
    b.sort_by_key(|n| n.step);
    assert_eq!(a, b, "and so does the drum part");
}

#[test]
fn exporting_a_chain_lays_the_patterns_end_to_end() {
    let mut song = Song::new();
    song.patterns[0] = Pattern::empty(4);
    song.patterns[0].add_note(0, Note::new(0, 60, 1, 100));
    song.patterns.push(Pattern::empty(4));
    song.patterns[1].add_note(0, Note::new(0, 67, 1, 100));
    song.chain = vec![0, 1, 0];

    let back = Song::from_midi_file(&song.to_midi_file(&song.chain.clone()), 4);
    assert_eq!(
        back.patterns.len(),
        3,
        "three bars of music, three patterns"
    );
    assert_eq!(back.chain, vec![0, 1, 2]);
    assert_eq!(back.patterns[0].track(0)[0].pitch, 60);
    assert_eq!(back.patterns[1].track(0)[0].pitch, 67);
    assert_eq!(back.patterns[2].track(0)[0].pitch, 60);
}

#[test]
fn a_note_crossing_a_bar_line_is_split_not_cut_short() {
    // Import splits it so the sound keeps its length; the two halves are
    // adjacent, so they sound as one held note.
    let mut song = Song::new();
    song.patterns[0] = Pattern::empty(8);
    song.patterns[0].add_note(0, Note::new(0, 60, 8, 100));
    let file = song.to_midi_file(&[0]);

    let back = Song::from_midi_file(&file, 4);
    assert_eq!(back.patterns.len(), 2);
    assert_eq!(back.patterns[0].track(0)[0], Note::new(0, 60, 4, 100));
    assert_eq!(back.patterns[1].track(0)[0], Note::new(0, 60, 4, 100));
}

#[test]
fn a_file_from_elsewhere_imports_at_its_own_resolution() {
    // 480 ticks per quarter, which is what most DAWs write, and a tempo this
    // sequencer did not choose.
    let mut file = MidiFile::new(480);
    file.tracks.push(vec![
        Event::new(0, EventKind::tempo(400_000)), // 150 BPM
        Event::new(0, EventKind::end_of_track()),
    ]);
    file.tracks.push(vec![
        Event::new(0, EventKind::Midi(vec![0x92, 60, 64])),
        Event::new(480, EventKind::Midi(vec![0x82, 60, 0])), // a quarter note
        Event::new(0, EventKind::Midi(vec![0x92, 67, 64])),
        Event::new(240, EventKind::Midi(vec![0x82, 67, 0])), // an eighth
        Event::new(0, EventKind::end_of_track()),
    ]);

    let song = Song::from_midi_file(&file, 16);
    assert!((song.bpm - 150.0).abs() < 0.5, "tempo: {}", song.bpm);
    assert_eq!(song.tracks[0].channel, 2, "the file's channel is kept");
    let notes = song.patterns[0].track(0);
    assert_eq!(notes[0], Note::new(0, 60, 4, 64), "a quarter is four steps");
    assert_eq!(notes[1], Note::new(4, 67, 2, 64), "an eighth is two");
}

#[test]
fn a_file_with_more_parts_than_there_are_tracks_keeps_the_first_ones() {
    let mut file = MidiFile::new(96);
    let mut events = Vec::new();
    for channel in 0..12u8 {
        events.push(Event::new(
            0,
            EventKind::Midi(vec![0x90 | channel, 60, 100]),
        ));
        events.push(Event::new(24, EventKind::Midi(vec![0x80 | channel, 60, 0])));
    }
    events.push(Event::new(0, EventKind::end_of_track()));
    file.tracks.push(events);

    let song = Song::from_midi_file(&file, 16);
    let played: usize = song.patterns[0]
        .notes
        .iter()
        .filter(|t| !t.is_empty())
        .count();
    assert_eq!(
        played, MAX_TRACKS,
        "as many parts as there are tracks, no panic"
    );
}

#[test]
fn an_empty_file_imports_as_an_empty_song() {
    let song = Song::from_midi_file(&MidiFile::new(96), 16);
    assert_eq!(song.patterns.len(), 1);
    assert!(song.patterns[0].track(0).is_empty());
}

#[test]
fn a_file_whose_notes_are_never_released_still_imports() {
    let mut file = MidiFile::new(96);
    file.tracks.push(vec![
        Event::new(0, EventKind::Midi(vec![0x90, 60, 100])),
        Event::new(96, EventKind::end_of_track()),
    ]);
    let song = Song::from_midi_file(&file, 16);
    assert_eq!(song.patterns[0].track(0).len(), 1, "the stuck note is kept");
}
