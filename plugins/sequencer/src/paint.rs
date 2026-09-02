//! Where everything sits, and how it is drawn.
//!
//! [`Layout`] is computed once a frame and read by both the input code and the
//! painting code, so a click always lands on what was drawn.

use wk_pixelfont::REGULAR as FONT;

/// Draw `s` at `(x, y)`. A thin wrapper so the call sites stay short.
#[allow(clippy::too_many_arguments)]
fn text(buf: &mut [u8], w: u32, h: u32, x: i32, y: i32, s: &str, scale: i32, color: [u8; 3]) {
    FONT.draw(buf, w, h, x, y, s, scale, color);
}

fn text_width(s: &str, scale: i32) -> i32 {
    FONT.measure(s, scale)
}
use crate::{App, ROWS, Transport};
use wk_sequence::{MAX_PATTERNS, MAX_TRACKS, Playback, Position};

// Strip heights, top to bottom.
pub const CTRL_H: f32 = 28.0;
pub const TRACK_H: f32 = 22.0;
pub const RULER_H: f32 = 13.0;
pub const VEL_H: f32 = 40.0;
pub const PAT_H: f32 = 22.0;
pub const CHAIN_H: f32 = 22.0;
/// The piano-key gutter down the left of the roll.
pub const GUTTER_W: f32 = 30.0;
pub const PAD: f32 = 6.0;

// Transport-bar metrics.
pub const BTN_W: f32 = 34.0;
pub const BTN_H: f32 = 20.0;
pub const BTN_Y: f32 = 4.0;
const BTN_GAP: f32 = 8.0;
pub const FIELD_W: f32 = 58.0;

const BG: [u8; 3] = [20, 20, 26];
const STRIP: [u8; 3] = [30, 30, 38];
const SUNK: [u8; 3] = [26, 26, 34];
const CELL: [u8; 3] = [46, 48, 58];
const CELL_ON: [u8; 3] = [72, 96, 120];
const LABEL: [u8; 3] = [120, 122, 140];
const VALUE: [u8; 3] = [225, 230, 245];

pub struct Layout {
    /// The piano roll, right of the gutter and below the ruler.
    pub gx0: f32,
    pub gy0: f32,
    pub gw: f32,
    pub gh: f32,
    pub cell_w: f32,
    pub cell_h: f32,
    /// The track strip.
    pub track_y: f32,
    pub track_w: f32,
    /// The velocity lane.
    pub vel_y0: f32,
    pub vel_y1: f32,
    /// The pattern bank and the song chain.
    pub pat_y: f32,
    pub chain_y: f32,
    pub slot_w: f32,
    /// Transport-bar hit boxes.
    pub play_x: f32,
    pub rec_x: f32,
    pub bpm_x: f32,
    pub len_x: f32,
    pub chan_x: f32,
    pub song_x: f32,
}

/// The x where the labelled strips' cells begin, leaving room for their caption.
const STRIP_X: f32 = 34.0;

pub fn layout(w: u32, h: u32, steps: i32) -> Layout {
    let (wf, hf) = (w as f32, h as f32);
    let track_y = CTRL_H;
    let gy0 = track_y + TRACK_H + RULER_H;
    let chain_y = hf - PAD - CHAIN_H;
    let pat_y = chain_y - PAT_H - 2.0;
    let vel_y1 = pat_y - 4.0;
    let vel_y0 = (vel_y1 - VEL_H).max(gy0 + 20.0);
    let gh = (vel_y0 - gy0 - 4.0).max(1.0);
    let gw = (wf - GUTTER_W - PAD).max(1.0);

    let play_x = PAD;
    let rec_x = play_x + BTN_W + BTN_GAP;
    let bpm_x = rec_x + BTN_W + BTN_GAP * 2.0;
    let len_x = bpm_x + FIELD_W + BTN_GAP;
    let chan_x = len_x + FIELD_W + BTN_GAP;
    let song_x = chan_x + FIELD_W + BTN_GAP;

    Layout {
        gx0: GUTTER_W,
        gy0,
        gw,
        gh,
        cell_w: gw / steps.max(1) as f32,
        cell_h: gh / ROWS as f32,
        track_y,
        track_w: ((wf - STRIP_X - PAD) / MAX_TRACKS as f32).max(8.0),
        vel_y0,
        vel_y1,
        pat_y,
        chain_y,
        slot_w: ((wf - STRIP_X - PAD) / MAX_PATTERNS as f32).max(8.0),
        play_x,
        rec_x,
        bpm_x,
        len_x,
        chan_x,
        song_x,
    }
}

impl Layout {
    /// The step column a pixel x falls in. May be outside the pattern.
    pub fn to_step(&self, px: f32) -> i32 {
        ((px - self.gx0) / self.cell_w).floor() as i32
    }

    /// The pitch a pixel y falls on, given the lowest row on screen.
    pub fn to_pitch(&self, py: f32, low: i32) -> i32 {
        let row = ((py - self.gy0) / self.cell_h).floor() as i32;
        low + (ROWS - 1 - row)
    }

    /// Is `(px, py)` inside a transport-bar box starting at `x0`?
    pub fn in_button(&self, px: f32, py: f32, x0: f32, wide: f32) -> bool {
        px >= x0 && px < x0 + wide && (BTN_Y..BTN_Y + BTN_H).contains(&py)
    }

    /// The track cell under `px`, if any.
    pub fn to_track(&self, px: f32) -> Option<usize> {
        self.cell_index(px, self.track_w, MAX_TRACKS)
    }

    /// The pattern or chain slot under `px`, if any.
    pub fn to_slot(&self, px: f32, count: usize) -> Option<usize> {
        self.cell_index(px, self.slot_w, count)
    }

    fn cell_index(&self, px: f32, wide: f32, count: usize) -> Option<usize> {
        if px < STRIP_X {
            return None;
        }
        let index = ((px - STRIP_X) / wide).floor() as usize;
        (index < count).then_some(index)
    }

    fn cell_x(&self, index: usize, wide: f32) -> f32 {
        STRIP_X + index as f32 * wide
    }
}

// ---- pixel primitives ----

fn put(buf: &mut [u8], w: u32, h: u32, x: i32, y: i32, c: [u8; 3]) {
    if x < 0 || y < 0 || x >= w as i32 || y >= h as i32 {
        return;
    }
    let i = ((y as u32 * w + x as u32) * 4) as usize;
    buf[i] = c[0];
    buf[i + 1] = c[1];
    buf[i + 2] = c[2];
    buf[i + 3] = 255;
}

fn fill(buf: &mut [u8], w: u32, h: u32, x0: i32, y0: i32, x1: i32, y1: i32, c: [u8; 3]) {
    for y in y0..y1 {
        for x in x0..x1 {
            put(buf, w, h, x, y, c);
        }
    }
}

fn stroke(buf: &mut [u8], w: u32, h: u32, x0: i32, y0: i32, x1: i32, y1: i32, c: [u8; 3]) {
    for x in x0..x1 {
        put(buf, w, h, x, y0, c);
        put(buf, w, h, x, y1 - 1, c);
    }
    for y in y0..y1 {
        put(buf, w, h, x0, y, c);
        put(buf, w, h, x1 - 1, y, c);
    }
}

fn disc(buf: &mut [u8], w: u32, h: u32, cx: i32, cy: i32, r: i32, c: [u8; 3]) {
    for dy in -r..=r {
        for dx in -r..=r {
            if dx * dx + dy * dy <= r * r {
                put(buf, w, h, cx + dx, cy + dy, c);
            }
        }
    }
}

/// A right-pointing "play" triangle filling the box `x0,y0..x1,y1`.
fn play_icon(buf: &mut [u8], w: u32, h: u32, x0: i32, y0: i32, x1: i32, y1: i32, c: [u8; 3]) {
    let (bw, bh) = ((x1 - x0) as f32, (y1 - y0) as f32);
    for y in y0..y1 {
        // Distance from the vertical centre folds the triangle to a point.
        let t = ((y - y0) as f32 / bh - 0.5).abs() * 2.0;
        let right = x0 as f32 + bw * (1.0 - t);
        for x in x0..(right as i32) {
            put(buf, w, h, x, y, c);
        }
    }
}

/// Scale a colour, so a note's velocity reads as its brightness.
fn dim(c: [u8; 3], f: f32) -> [u8; 3] {
    let f = f.clamp(0.0, 1.0);
    [
        (c[0] as f32 * f) as u8,
        (c[1] as f32 * f) as u8,
        (c[2] as f32 * f) as u8,
    ]
}

/// Is MIDI `note` a black key?
fn is_black(note: i32) -> bool {
    matches!(note.rem_euclid(12), 1 | 3 | 6 | 8 | 10)
}

/// The name of MIDI `note`, e.g. `C4` or `F#3`, with middle C as C4.
fn note_name(note: i32) -> String {
    const NAMES: [&str; 12] = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    ];
    format!(
        "{}{}",
        NAMES[note.rem_euclid(12) as usize],
        note.div_euclid(12) - 1
    )
}

// ---- the frame ----

pub fn paint(app: &App, lay: &Layout, w: u32, h: u32, at: Option<Position>) -> Vec<u8> {
    let mut px = vec![0u8; (w * h * 4) as usize];
    for p in px.chunks_exact_mut(4) {
        p.copy_from_slice(&[BG[0], BG[1], BG[2], 255]);
    }
    let buf = &mut px;

    // The pattern under the playhead, which in song mode may not be the one
    // being edited. The roll always shows the edited pattern; the playhead is
    // only drawn when they are the same, because a line pointing at a bar you
    // are not looking at is a lie.
    let editing = app.pattern();
    let steps = editing.steps;
    let playhead = at.filter(|p| p.pattern == app.pattern).map(|p| p.step);

    roll(buf, w, h, app, lay, playhead);
    velocity_lane(buf, w, h, app, lay);
    track_strip(buf, w, h, app, lay);
    pattern_strip(buf, w, h, app, lay, at);
    chain_strip(buf, w, h, app, lay, at);
    transport_bar(buf, w, h, app, lay, steps);
    px
}

fn roll(buf: &mut [u8], w: u32, h: u32, app: &App, lay: &Layout, playhead: Option<i32>) {
    let pattern = app.pattern();
    let steps = pattern.steps;
    let (gx0, gy0, gw, gh) = (lay.gx0, lay.gy0, lay.gw, lay.gh);

    // Rows, and the piano keyboard down the side that says which is which.
    for row in 0..ROWS {
        let pitch = app.low + (ROWS - 1 - row);
        let y0 = (gy0 + row as f32 * lay.cell_h) as i32;
        let y1 = (gy0 + (row + 1) as f32 * lay.cell_h) as i32;
        let tint = if is_black(pitch) {
            [30, 30, 40]
        } else {
            [44, 44, 56]
        };
        fill(buf, w, h, gx0 as i32, y0, (gx0 + gw) as i32, y1, tint);

        let key = if is_black(pitch) {
            [26, 26, 32]
        } else {
            [200, 200, 210]
        };
        fill(buf, w, h, 0, y0, gx0 as i32 - 1, y1, key);
        if lay.cell_h >= 9.0 {
            let colour = if is_black(pitch) {
                [170, 170, 180]
            } else {
                [40, 40, 48]
            };
            text(buf, w, h, 2, y0 + 1, &note_name(pitch), 1, colour);
        }
    }

    if let Some(step) = playhead {
        let x0 = (gx0 + step as f32 * lay.cell_w) as i32;
        let x1 = (gx0 + (step + 1) as f32 * lay.cell_w) as i32;
        fill(
            buf,
            w,
            h,
            x0,
            gy0 as i32,
            x1,
            (gy0 + gh) as i32,
            [56, 58, 70],
        );
    }

    // Beat dividers, and the ruler that numbers the beats.
    let ruler_y = lay.gy0 - RULER_H;
    fill(buf, w, h, 0, ruler_y as i32, w as i32, lay.gy0 as i32, SUNK);
    for step in (0..=steps).step_by(4) {
        let x = (gx0 + step as f32 * lay.cell_w) as i32;
        fill(
            buf,
            w,
            h,
            x,
            gy0 as i32,
            x + 1,
            (gy0 + gh) as i32,
            [70, 70, 84],
        );
        if step < steps {
            // Beats numbered from one, the way a musician counts them.
            let label = format!("{}", step / 4 + 1);
            text(
                buf,
                w,
                h,
                x + 2,
                ruler_y as i32 + 3,
                &label,
                1,
                [130, 130, 150],
            );
        }
    }

    // Other tracks first, ghosted, then the one being edited on top. Seeing the
    // rest of the arrangement behind what you are working on is the difference
    // between writing a part and writing a bar.
    for track in 0..MAX_TRACKS {
        if track == app.track {
            continue;
        }
        for note in pattern.track(track) {
            note_block(buf, w, h, app, lay, note, steps, [58, 62, 78], false);
        }
    }
    for (index, note) in pattern.track(app.track).iter().enumerate() {
        let sounding = playhead.is_some_and(|p| note.covers(p, steps));
        let base = if sounding {
            [160, 255, 200]
        } else {
            [90, 200, 250]
        };
        // Never fully dark: a quiet note still has to be visible to be edited.
        let shade = dim(base, 0.4 + 0.6 * (note.vel as f32 / 127.0));
        note_block(
            buf,
            w,
            h,
            app,
            lay,
            note,
            steps,
            shade,
            app.selected == Some(index),
        );
    }

    if let Some(step) = playhead {
        let x = (gx0 + step as f32 * lay.cell_w) as i32;
        fill(
            buf,
            w,
            h,
            x,
            gy0 as i32,
            x + 2,
            (gy0 + gh) as i32,
            [230, 235, 120],
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn note_block(
    buf: &mut [u8],
    w: u32,
    h: u32,
    app: &App,
    lay: &Layout,
    note: &wk_sequence::Note,
    steps: i32,
    colour: [u8; 3],
    selected: bool,
) {
    let row = ROWS - 1 - (note.pitch - app.low);
    if !(0..ROWS).contains(&row) || note.step >= steps {
        return;
    }
    let x0 = (lay.gx0 + note.step as f32 * lay.cell_w) as i32 + 1;
    let x1 = (lay.gx0 + note.end(steps) as f32 * lay.cell_w) as i32 - 1;
    let y0 = (lay.gy0 + row as f32 * lay.cell_h) as i32 + 1;
    let y1 = (lay.gy0 + (row + 1) as f32 * lay.cell_h) as i32 - 1;
    fill(buf, w, h, x0, y0, x1.max(x0 + 1), y1, colour);
    if selected {
        stroke(buf, w, h, x0 - 1, y0 - 1, x1 + 1, y1 + 1, [240, 245, 255]);
    }
}

fn velocity_lane(buf: &mut [u8], w: u32, h: u32, app: &App, lay: &Layout) {
    fill(
        buf,
        w,
        h,
        0,
        lay.vel_y0 as i32,
        w as i32,
        lay.vel_y1 as i32,
        SUNK,
    );
    text(buf, w, h, 3, lay.vel_y0 as i32 + 2, "VEL", 1, LABEL);
    let pattern = app.pattern();
    let lane = lay.vel_y1 - lay.vel_y0;
    for note in pattern
        .track(app.track)
        .iter()
        .filter(|n| n.step < pattern.steps)
    {
        let x0 = (lay.gx0 + note.step as f32 * lay.cell_w) as i32 + 1;
        let x1 = (lay.gx0 + (note.step + 1) as f32 * lay.cell_w) as i32 - 1;
        let bar = lane * (note.vel as f32 / 127.0);
        let y0 = (lay.vel_y1 - bar) as i32;
        fill(
            buf,
            w,
            h,
            x0,
            y0,
            x1.max(x0 + 1),
            lay.vel_y1 as i32,
            [90, 160, 210],
        );
    }
}

fn track_strip(buf: &mut [u8], w: u32, h: u32, app: &App, lay: &Layout) {
    let (y0, y1) = (lay.track_y as i32, (lay.track_y + TRACK_H) as i32);
    fill(buf, w, h, 0, y0, w as i32, y1, STRIP);
    text(buf, w, h, 4, y0 + 7, "TRK", 1, LABEL);
    for index in 0..MAX_TRACKS {
        let track = app.song.tracks[index];
        let x0 = lay.cell_x(index, lay.track_w) as i32;
        let x1 = (lay.cell_x(index, lay.track_w) + lay.track_w - 2.0) as i32;
        let selected = index == app.track;
        let has_notes = !app.pattern().track(index).is_empty();
        let bg = match (selected, track.muted) {
            (true, _) => CELL_ON,
            (false, true) => [38, 30, 30],
            (false, false) => CELL,
        };
        fill(buf, w, h, x0, y0 + 2, x1, y1 - 2, bg);
        // A track with something in it reads brighter, so an arrangement can be
        // taken in at a glance.
        let ink = match (track.muted, has_notes) {
            (true, _) => [150, 90, 90],
            (false, true) => VALUE,
            (false, false) => [130, 132, 150],
        };
        let label = format!("{}", index + 1);
        text(buf, w, h, x0 + 4, y0 + 4, &label, 2, ink);
        let channel = format!("C{}", track.channel + 1);
        let cw = text_width(&channel, 1);
        text(buf, w, h, x1 - 3 - cw, y0 + 9, &channel, 1, dim(ink, 0.8));
    }
}

fn pattern_strip(buf: &mut [u8], w: u32, h: u32, app: &App, lay: &Layout, at: Option<Position>) {
    let (y0, y1) = (lay.pat_y as i32, (lay.pat_y + PAT_H) as i32);
    fill(buf, w, h, 0, y0, w as i32, y1, STRIP);
    text(buf, w, h, 4, y0 + 7, "PAT", 1, LABEL);
    let count = app.song.patterns.len();
    for index in 0..MAX_PATTERNS {
        let x0 = lay.cell_x(index, lay.slot_w) as i32;
        let x1 = (lay.cell_x(index, lay.slot_w) + lay.slot_w - 2.0) as i32;
        let exists = index < count;
        // One slot past the end is the "add another" button.
        let addable = index == count;
        if !exists && !addable {
            continue;
        }
        let playing = at.is_some_and(|p| p.pattern == index);
        let bg = if index == app.pattern {
            CELL_ON
        } else if playing {
            [46, 70, 56]
        } else {
            CELL
        };
        fill(buf, w, h, x0, y0 + 2, x1, y1 - 2, bg);
        let label = if addable {
            "+".to_string()
        } else {
            format!("{}", index + 1)
        };
        let ink = if addable { LABEL } else { VALUE };
        let tw = text_width(&label, 1);
        text(
            buf,
            w,
            h,
            x0 + ((x1 - x0 - tw) / 2).max(1),
            y0 + 7,
            &label,
            1,
            ink,
        );
    }
}

fn chain_strip(buf: &mut [u8], w: u32, h: u32, app: &App, lay: &Layout, at: Option<Position>) {
    let (y0, y1) = (lay.chain_y as i32, (lay.chain_y + CHAIN_H) as i32);
    fill(buf, w, h, 0, y0, w as i32, y1, SUNK);
    let caption_ink = if app.song_mode {
        [140, 220, 160]
    } else {
        LABEL
    };
    text(buf, w, h, 4, y0 + 7, "SNG", 1, caption_ink);
    if app.song.chain.is_empty() {
        text(
            buf,
            w,
            h,
            STRIP_X as i32,
            y0 + 7,
            "SHIFT-CLICK A PATTERN",
            1,
            [80, 82, 96],
        );
        return;
    }
    for (slot, &pattern) in app.song.chain.iter().enumerate() {
        let x0 = lay.cell_x(slot, lay.slot_w) as i32;
        let x1 = (lay.cell_x(slot, lay.slot_w) + lay.slot_w - 2.0) as i32;
        if x0 >= w as i32 {
            break;
        }
        let here = app.song_mode && at.is_some_and(|p| p.chain_index == slot);
        let bg = if here { [70, 110, 84] } else { CELL };
        fill(buf, w, h, x0, y0 + 2, x1, y1 - 2, bg);
        let label = format!("{}", pattern + 1);
        let tw = text_width(&label, 1);
        text(
            buf,
            w,
            h,
            x0 + ((x1 - x0 - tw) / 2).max(1),
            y0 + 7,
            &label,
            1,
            VALUE,
        );
    }
}

fn transport_bar(buf: &mut [u8], w: u32, h: u32, app: &App, lay: &Layout, steps: i32) {
    fill(buf, w, h, 0, 0, w as i32, CTRL_H as i32, STRIP);

    let playing = app.transport == Transport::Playing;
    fill(
        buf,
        w,
        h,
        lay.play_x as i32,
        BTN_Y as i32,
        (lay.play_x + BTN_W) as i32,
        (BTN_Y + BTN_H) as i32,
        CELL,
    );
    play_icon(
        buf,
        w,
        h,
        (lay.play_x + 9.0) as i32,
        (BTN_Y + 4.0) as i32,
        (lay.play_x + BTN_W - 7.0) as i32,
        (BTN_Y + BTN_H - 4.0) as i32,
        if playing {
            [110, 240, 150]
        } else {
            [80, 150, 100]
        },
    );

    let recording = app.transport == Transport::Recording;
    fill(
        buf,
        w,
        h,
        lay.rec_x as i32,
        BTN_Y as i32,
        (lay.rec_x + BTN_W) as i32,
        (BTN_Y + BTN_H) as i32,
        CELL,
    );
    disc(
        buf,
        w,
        h,
        (lay.rec_x + BTN_W / 2.0) as i32,
        (BTN_Y + BTN_H / 2.0) as i32,
        (BTN_H / 2.0 - 4.0) as i32,
        if recording {
            [255, 90, 90]
        } else {
            [150, 70, 70]
        },
    );

    for (x, label, value) in [
        (lay.bpm_x, "BPM", format!("{}", app.song.bpm.round() as i32)),
        (lay.len_x, "LEN", format!("{steps}")),
        (
            lay.chan_x,
            "CH",
            format!("{}", app.song.channel(app.track) + 1),
        ),
    ] {
        field(buf, w, h, x, label, &value);
    }

    // The song-mode toggle.
    let on = app.song_mode;
    fill(
        buf,
        w,
        h,
        lay.song_x as i32,
        BTN_Y as i32,
        (lay.song_x + FIELD_W) as i32,
        (BTN_Y + BTN_H) as i32,
        if on { [56, 96, 68] } else { CELL },
    );
    let ink = if on { [180, 245, 200] } else { LABEL };
    text(
        buf,
        w,
        h,
        lay.song_x as i32 + 8,
        BTN_Y as i32 + 7,
        "SONG",
        1,
        ink,
    );

    // Status, right-aligned: what file is open, whether it needs saving, and
    // whatever the last action had to say.
    let mut note = if app.status.is_empty() {
        match &app.file {
            Some(f) => f.name(),
            None => String::new(),
        }
    } else {
        app.status.clone()
    };
    if app.unsaved && app.file.is_some() && app.status.is_empty() {
        note.push('*');
    }
    let note = note.to_uppercase();
    let tw = text_width(&note, 1);
    let x = w as i32 - PAD as i32 - tw;
    if x > (lay.song_x + FIELD_W) as i32 {
        text(buf, w, h, x, BTN_Y as i32 + 7, &note, 1, [140, 145, 165]);
    }
}

fn field(buf: &mut [u8], w: u32, h: u32, x: f32, label: &str, value: &str) {
    fill(
        buf,
        w,
        h,
        x as i32,
        BTN_Y as i32,
        (x + FIELD_W) as i32,
        (BTN_Y + BTN_H) as i32,
        CELL,
    );
    text(buf, w, h, x as i32 + 4, BTN_Y as i32 + 7, label, 1, LABEL);
    let vw = text_width(value, 2);
    text(
        buf,
        w,
        h,
        (x + FIELD_W) as i32 - 4 - vw,
        BTN_Y as i32 + 3,
        value,
        2,
        VALUE,
    );
}

/// Which pattern the transport is on, for the strips to highlight.
pub fn playing_at(app: &App, clock: u64) -> Option<Position> {
    if app.transport == Transport::Stopped {
        return None;
    }
    let play = if app.song_mode {
        Playback::Song
    } else {
        Playback::Pattern(app.pattern)
    };
    app.sched.position(clock, &app.song, play)
}
