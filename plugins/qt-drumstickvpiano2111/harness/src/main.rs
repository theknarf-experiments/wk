//! Run `drumstick-vpiano.wasm` as a real wk node and prove that it exchanges
//! MIDI with ANOTHER wk node over the fabric.
//!
//! WHY THIS EXISTS. "A Qt window appeared" is the cheap half of this port and
//! plugins/qt-torrentfileeditor103 already established it. The claim here is
//! the wiring: a genuine `wasi:surface` pointer event lands on a piano key
//! inside a QGraphicsView, drumstick turns it into a note, the wk MIDI backend
//! this port adds pushes the raw bytes through `wk:midi/midi`, the host router
//! carries them across a canvas connection, and a SEPARATE node — the real
//! plugins/fluidsynth SoundFont synth — receives that same note. Then the same
//! thing backwards.
//!
//! It is a STANDALONE cargo project rather than another `#[test]` in
//! `crates/wk-server/src/plugin.rs` because that file is shared and other Qt
//! ports are being written against it at the same time.
//!
//! WHAT IT ASSERTS, in order, and each is a different claim:
//!
//!   1. the node opens a wk surface at all — QApplication came up on the wk
//!      QPA and the font database is not empty;
//!   2. BackendManager found the two STATIC wk MIDI plugins. `in='wk'
//!      out='wk'` is the whole static-plugin chain (QT_STATICPLUGIN, the
//!      Q_IMPORT_PLUGIN names, moc over the backend headers) in one line, and
//!      naming them stops a wrong backend from passing;
//!   3. the composited frame is a WIDGET frame — a QGraphicsView full of
//!      piano keys, which is a Qt subsystem no previous wk Qt port had
//!      exercised;
//!   4. OUT: a real pointer press on a real key produces a note, and THE SAME
//!      note number and velocity turn up in the FLUIDSYNTH node's terminal.
//!      Tying the two logs together by value is what makes this a test of the
//!      fabric rather than of two coincidences. The release does the same for
//!      note-off;
//!   5. the NEGATIVE CONTROL for claim 4: with the MIDI route disconnected,
//!      the piano still emits the note and fluidsynth never sees it. Without
//!      this, claim 4 would be equally consistent with the two nodes being
//!      joined by something other than the wire under test;
//!   6. IN: MIDI injected from a phantom source (exactly how the CoreMIDI host
//!      node injects a USB keyboard) is decoded by drumstick's parser, reaches
//!      the application as a note-on, and REPAINTS the key — the pixels in the
//!      key's rect change. This is the half that needs the 1 ms poll pump, so
//!      it is also the test that the pump runs at all.
//!
//! Frames are written out as PPM (`--dump <dir>`): a histogram proves "not
//! blank", never "the right window".
//!
//! Usage: cargo run -- [--wasm PATH] [--synth PATH] [--sf2 PATH] [--dump DIR]

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wk_protocol::NodeId;
use wk_server::plugin::{
    NodeRegistry, PluginHost, PointerButton, PointerEvent, SurfaceRegistry, VirtualSurface,
};

fn write_ppm(path: &Path, w: u32, h: u32, pixels: &[u8]) {
    let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
    ppm.extend(pixels.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]));
    std::fs::write(path, ppm).expect("write frame dump");
    eprintln!("frame dumped to {}", path.display());
}

/// The LAST line with this tag, parsed as N whitespace-separated integers.
/// Last and never first: the app republishes its rects on a repeating timer
/// and the wk QPA forces the first top-level to the full surface a few frames
/// in, so an early reading is the pre-resize geometry and aiming at it misses.
fn last_ints(log: &str, tag: &str) -> Option<Vec<i64>> {
    let line = log.lines().rfind(|l| l.starts_with(tag))?;
    Some(
        line[tag.len()..]
            .split_whitespace()
            .filter_map(|t| t.parse().ok())
            .collect(),
    )
}

fn main() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut wasm = manifest.join("../drumstick-vpiano.wasm");
    let mut synth = manifest.join("../../fluidsynth/fluidsynth.wasm");
    let mut sf2 = manifest.join("../../fluidsynth/soundfont.sf2");
    let mut dump = PathBuf::from("/tmp");
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--wasm" => wasm = PathBuf::from(args.next().expect("--wasm PATH")),
            "--synth" => synth = PathBuf::from(args.next().expect("--synth PATH")),
            "--sf2" => sf2 = PathBuf::from(args.next().expect("--sf2 PATH")),
            "--dump" => dump = PathBuf::from(args.next().expect("--dump DIR")),
            other => panic!("unknown argument {other}"),
        }
    }
    assert!(
        wasm.exists(),
        "no component at {} -- run ./build.sh",
        wasm.display()
    );
    assert!(
        synth.exists() && sf2.exists(),
        "this harness needs the fluidsynth node on the other end of the wire.\n  \
         build it with: cd plugins/fluidsynth && ./build.sh\n  \
         (missing {} or {})",
        synth.display(),
        sf2.display()
    );
    // tfe's harness forgets this and --dump into a fresh checkout panics.
    std::fs::create_dir_all(&dump).expect("create --dump dir");

    let host = PluginHost::new().expect("host");
    let nodes: NodeRegistry = Arc::new(Mutex::new(Vec::new()));
    let surfaces: SurfaceRegistry = Arc::new(Mutex::new(Vec::new()));

    // --- node 1: the synth, on the far end of the wire ---------------------
    let synth_id = NodeId::new();
    host.spawn(
        &synth,
        "fluidsynth",
        synth_id,
        &["--dry-run".to_string(), "/soundfont.sf2".to_string()],
        Arc::new(Mutex::new(Vec::new())),
        nodes.clone(),
        Vec::new(),
        None,
    )
    .expect("spawn fluidsynth");

    // --- node 2: the piano -------------------------------------------------
    let piano_id = NodeId::new();
    host.spawn(
        &wasm,
        "drumstick-vpiano",
        piano_id,
        &[],
        surfaces.clone(),
        nodes.clone(),
        Vec::new(),
        Some(wk_server::images::ContainerSetup {
            layers: Vec::new(),
            env: vec![
                ("WK_VPIANO_SELFTEST".into(), "1".into()),
                // QSettings/QStandardPaths want somewhere to point; the same
                // thing Dockerfile does. Without it VPiano::readSettings still
                // works, but every QStandardPaths call warns.
                ("HOME".into(), "/root".into()),
            ],
        }),
    )
    .expect("spawn vpiano");

    let find = |id: NodeId| {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(n) = nodes.lock().unwrap().iter().find(|n| n.id == id).cloned() {
                break n;
            }
            assert!(Instant::now() < deadline, "node never appeared");
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    let synth_node = find(synth_id);
    let piano_node = find(piano_id);

    // Nodes register synchronously; the guest only starts once the background
    // compile finishes, so seeding now is comfortably ahead of main().
    synth_node
        .fs
        .lock()
        .unwrap()
        .put_file_at("soundfont.sf2", std::fs::read(&sf2).expect("read sf2"));

    // --- the wires ---------------------------------------------------------
    // Exactly what the server does for a canvas "midi" connection: a router
    // entry from the source node into the destination's inbox. Messages queue
    // there even before a guest opens its input port, so wiring early cannot
    // race either boot.
    //
    //   piano -> synth    the OUT direction (claim 4)
    //   kbd   -> piano    the IN direction (claim 6). NodeId::nil() stands in
    //                     for a hardware MidiIn node, which is how
    //                     crates/wk-server's own fluidsynth test injects.
    let kbd = NodeId::nil();
    {
        let midi = host.midi();
        let mut r = midi.lock().unwrap();
        r.connect(piano_id, synth_id, synth_node.midi_in.clone());
        r.connect(kbd, piano_id, piano_node.midi_in.clone());
    }

    let piano_log = || String::from_utf8_lossy(&piano_node.term_io.log_read(0).0).into_owned();
    let synth_log = || String::from_utf8_lossy(&synth_node.term_io.log_read(0).0).into_owned();

    // --- claim 1: a surface -------------------------------------------------
    let surface = {
        let deadline = Instant::now() + Duration::from_secs(600);
        loop {
            if let Some(s) = surfaces.lock().unwrap().first().cloned() {
                break s;
            }
            assert!(
                !piano_node.finished.load(Ordering::Relaxed),
                "the node exited before opening a surface; log:\n{}",
                piano_log()
            );
            assert!(
                Instant::now() < deadline,
                "no surface after 10 minutes; log:\n{}",
                piano_log()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    eprintln!("surface opened");

    // One frame credit per iteration, and wake the guest's parked pollable:
    // precisely what the server does per compositor frame.
    let pump_frame = || {
        let mut s = surface.lock().unwrap();
        s.frame_ready = true;
        s.wake();
    };
    let pump_for = |d: Duration| {
        let until = Instant::now() + d;
        while Instant::now() < until {
            pump_frame();
            std::thread::sleep(Duration::from_millis(15));
        }
    };
    // Spin frames until `pred` holds, then return. Every wait in this harness
    // goes through here so that a guest is never starved of frames while we
    // are waiting on it — with no threads in the guest, a frame IS its
    // scheduling quantum.
    let until = |what: &str, secs: u64, pred: &dyn Fn() -> bool| {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            pump_frame();
            std::thread::sleep(Duration::from_millis(15));
            if pred() {
                return;
            }
            assert!(
                !piano_node.finished.load(Ordering::Relaxed),
                "the piano node exited while waiting for {what}; log:\n{}",
                piano_log()
            );
            assert!(
                !synth_node.finished.load(Ordering::Relaxed),
                "the synth node exited while waiting for {what}; synth log:\n{}",
                synth_log()
            );
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}.\n--- piano log ---\n{}\n--- synth log ---\n{}",
                piano_log(),
                synth_log()
            );
        }
    };

    // The synth has to be up before the OUT assertion means anything: a note
    // arriving before its SoundFont is parsed would queue in the inbox, which
    // is correct behaviour but makes a timeout unreadable.
    until("the SoundFont to load", 300, &|| {
        synth_log().contains("soundfont loaded: /soundfont.sf2")
    });
    eprintln!("synth ready");

    // --- claim 2: the static MIDI backends were found ------------------------
    // If BackendManager had found nothing, VPiano::initialize() would already
    // have called qFatal and the node would be gone — so the check above for
    // `finished` is half of this claim, and the names are the other half.
    until("the piano to report its MIDI backends", 300, &|| {
        piano_log().contains("in='wk' out='wk'")
    });
    let state = piano_log()
        .lines()
        .rfind(|l| l.starts_with("STATE "))
        .unwrap()
        .to_string();
    eprintln!("{state}");

    // --- claim 3: a widget frame, not a blank buffer ------------------------
    // A piano keyboard is mostly white with thin dark borders and dark black
    // keys, so the thresholds differ from a form-heavy app's.
    let histogram = || {
        let s = surface.lock().unwrap();
        let mut dark = 0usize;
        let mut light = 0usize;
        for px in s.pixels.chunks_exact(4) {
            let lum = px[0] as u32 + px[1] as u32 + px[2] as u32;
            if lum < 200 {
                dark += 1;
            } else if lum > 500 {
                light += 1;
            }
        }
        (dark, light)
    };
    until("a painted keyboard", 300, &|| {
        let (dark, light) = histogram();
        dark > 5_000 && light > 20_000
    });
    let (dark, light) = histogram();
    eprintln!("frame: {dark} dark px, {light} light px");
    {
        let s = surface.lock().unwrap();
        write_ppm(&dump.join("vpiano-main.ppm"), s.width, s.height, &s.pixels);
    }

    // --- claim 4: OUT, piano key -> fabric -> synth --------------------------
    until("the piano to publish its keyboard rect", 120, &|| {
        last_ints(&piano_log(), "KEYBD ").is_some_and(|v| v.len() == 4 && v[2] > 0)
    });
    let k = last_ints(&piano_log(), "KEYBD ").unwrap();
    let (kx, ky, kw, kh) = (k[0], k[1], k[2], k[3]);
    eprintln!("keyboard rect: {kx},{ky} {kw}x{kh}");

    // WHERE THE KEYS ACTUALLY ARE, found in the pixels rather than assumed.
    // PianoKeybd is a QGraphicsView and its resizeEvent does
    // `fitInView(sceneRect, Qt::KeepAspectRatio)`, so an 88-key keyboard in a
    // 1006x702 widget is drawn as a THIN HORIZONTAL STRIP (~46 px tall here)
    // centred in a much taller expanse of background. Aiming at a fraction of
    // the widget rect therefore misses the keyboard entirely and produces no
    // note and no error — which is exactly the failure this reads its way out
    // of. The strip is the only part of the rect with dark pixels in it: white
    // keys are drawn on a near-white background, but their borders and the
    // black keys are not.
    let key_band = || -> (i64, i64) {
        let s = surface.lock().unwrap();
        let dark_row = |y: i64| {
            let mut n = 0;
            let mut x = kx.max(0);
            while x < (kx + kw).min(s.width as i64) {
                let i = ((y as u32 * s.width + x as u32) * 4) as usize;
                let lum = s.pixels[i] as u32 + s.pixels[i + 1] as u32 + s.pixels[i + 2] as u32;
                if lum < 200 {
                    n += 1;
                }
                x += 2;
            }
            n > 4
        };
        let rows: Vec<i64> = (ky.max(0)..(ky + kh).min(s.height as i64))
            .filter(|y| dark_row(*y))
            .collect();
        (*rows.first().unwrap_or(&ky), *rows.last().unwrap_or(&ky))
    };
    let (band_top, band_bottom) = key_band();
    assert!(
        band_bottom - band_top > 8,
        "could not find the drawn keyboard inside {kx},{ky} {kw}x{kh} -- rows \
         {band_top}..{band_bottom} carry dark pixels, which is too thin to be a keyboard"
    );
    eprintln!("keys drawn in rows {band_top}..{band_bottom}");

    // Black keys occupy only the upper part of the strip, so the bottom tenth
    // is white keys all the way across and a click there cannot fall in a gap.
    // The x is a fraction of the width rather than a note number: WHICH note
    // it is does not matter, because the claim is that the SAME note reaches
    // the synth, and the app reports which one it sent.
    let key_at = |frac: f64| -> (f64, f64) {
        let h = (band_bottom - band_top) as f64;
        (
            (kx as f64) + (kw as f64) * frac,
            (band_bottom as f64) - h * 0.1,
        )
    };
    let press = |s: &mut VirtualSurface, x: f64, y: f64| {
        s.pointer_move
            .push_back(PointerEvent { x, y, button: None });
        s.pointer_down.push_back(PointerEvent {
            x,
            y,
            button: Some(PointerButton::Left),
        });
        s.wake();
    };
    let release = |s: &mut VirtualSurface, x: f64, y: f64| {
        s.pointer_up.push_back(PointerEvent {
            x,
            y,
            button: Some(PointerButton::Left),
        });
        s.wake();
    };
    // `SENT noteon ch=C note=N vel=V` -> (C, N, V)
    let sent = |kind: &str| -> Option<(i64, i64, i64)> {
        let log = piano_log();
        let line = log
            .lines()
            .rfind(|l| l.starts_with(&format!("SENT {kind} ")))?;
        let n: Vec<i64> = line
            .split_whitespace()
            .filter_map(|t| t.split_once('=').and_then(|(_, v)| v.parse().ok()))
            .collect();
        (n.len() == 3).then(|| (n[0], n[1], n[2]))
    };

    let (px, py) = key_at(0.45);
    eprintln!("pressing a white key at ({px}, {py})");
    press(&mut surface.lock().unwrap(), px, py);
    until("the piano to emit a note-on", 120, &|| sent("noteon").is_some());
    let (chan, note, vel) = sent("noteon").unwrap();
    eprintln!("the piano sent note-on ch={chan} note={note} vel={vel}");
    assert!(
        (0..128).contains(&note) && vel > 0,
        "implausible note-on: ch={chan} note={note} vel={vel}"
    );

    // THE CLAIM. Not "a note-on arrived" — THIS note-on, by value, in a
    // different node's terminal, having crossed wk:midi and the host router.
    let want_on = format!("note-on ch={chan} key={note} vel={vel}");
    until(&format!("the synth to receive {want_on:?}"), 120, &|| {
        synth_log().contains(&want_on)
    });
    eprintln!("the SYNTH node logged: {want_on}");

    release(&mut surface.lock().unwrap(), px, py);
    until("the piano to emit a note-off", 120, &|| {
        sent("noteoff").is_some()
    });
    let (ochan, onote, _) = sent("noteoff").unwrap();
    assert_eq!(
        (ochan, onote),
        (chan, note),
        "the release must take back the note the press produced"
    );
    let want_off = format!("note-off ch={chan} key={note}");
    until(&format!("the synth to receive {want_off:?}"), 120, &|| {
        synth_log().contains(&want_off)
    });
    eprintln!("the SYNTH node logged: {want_off}");
    {
        let s = surface.lock().unwrap();
        write_ppm(&dump.join("vpiano-played.ppm"), s.width, s.height, &s.pixels);
    }

    // --- claim 5: the negative control ---------------------------------------
    // Cut the wire and play a DIFFERENT key. The piano must still emit — it
    // has no idea whether anything is listening — and the synth must never
    // hear it. Without this, claim 4 proves only that two logs agree.
    host.midi().lock().unwrap().disconnect(piano_id, synth_id);
    let (cx, cy) = key_at(0.62);
    eprintln!("wire cut; pressing another key at ({cx}, {cy})");
    press(&mut surface.lock().unwrap(), cx, cy);
    until("the piano to emit a second note-on", 120, &|| {
        sent("noteon").is_some_and(|(_, n, _)| n != note)
    });
    let (chan2, note2, vel2) = sent("noteon").unwrap();
    eprintln!("the piano sent note-on ch={chan2} note={note2} vel={vel2} into a cut wire");
    // Give it as long as the positive case took to arrive, and then some.
    pump_for(Duration::from_secs(3));
    let orphan = format!("note-on ch={chan2} key={note2} vel={vel2}");
    assert!(
        !synth_log().contains(&orphan),
        "the synth heard {orphan:?} with the MIDI route DISCONNECTED -- so the \
         positive result above was not carried by the wire under test.\n--- synth log ---\n{}",
        synth_log()
    );
    eprintln!("...and the synth heard nothing, as it must");
    release(&mut surface.lock().unwrap(), cx, cy);
    pump_for(Duration::from_millis(300));

    // --- claim 6: IN, injected MIDI -> the app, and the key repaints ---------
    // The pixels of the middle-C key's neighbourhood, before and after. The
    // whole keyboard rect is used rather than a computed key rect: which pixel
    // column note 60 occupies depends on the scene's layout, and "the keyboard
    // repainted while exactly one note was on" is the honest claim.
    let keyboard_pixels = || -> Vec<u8> {
        let s = surface.lock().unwrap();
        let mut out = Vec::new();
        for y in ky.max(0)..(ky + kh).min(s.height as i64) {
            for x in kx.max(0)..(kx + kw).min(s.width as i64) {
                let i = ((y as u32 * s.width + x as u32) * 4) as usize;
                out.extend_from_slice(&s.pixels[i..i + 4]);
            }
        }
        out
    };
    // Settle first: the note just released is still fading out of the widget.
    pump_for(Duration::from_millis(500));
    let before = keyboard_pixels();

    eprintln!("injecting note-on 90 3c 64 from a phantom MIDI source");
    host.midi()
        .lock()
        .unwrap()
        .send_from(kbd, &vec![0x90, 60, 100]);

    // The app SAW it: drumstick's parser decoded the raw bytes the 1 ms pump
    // drained out of this node's inbox, and delivered them as a note-on.
    until("the piano to receive the injected note", 120, &|| {
        piano_log().contains("RECV noteon ch=0 note=60 vel=100")
    });
    eprintln!("the piano logged: RECV noteon ch=0 note=60 vel=100");

    // ...and DREW it. A backend that swallowed the note and told nobody would
    // pass everything above.
    until("the middle-C key to light up", 60, &|| {
        keyboard_pixels() != before
    });
    eprintln!("the keyboard repainted with the incoming note held down");
    {
        let s = surface.lock().unwrap();
        write_ppm(&dump.join("vpiano-lit.ppm"), s.width, s.height, &s.pixels);
    }

    host.midi()
        .lock()
        .unwrap()
        .send_from(kbd, &vec![0x80, 60, 0]);
    until("the piano to receive the note-off", 60, &|| {
        piano_log().contains("RECV noteoff ch=0 note=60")
    });
    eprintln!("the piano logged: RECV noteoff ch=0 note=60");

    println!("\n--- piano log ---\n{}", piano_log());
    println!("--- synth log ---\n{}", synth_log());
    println!("PASS");

    // Close the surface: the guest traps on its next get-frame and exits.
    {
        let mut s = surface.lock().unwrap();
        s.closed = true;
        s.wake();
    }
    piano_node.kill.store(true, Ordering::Relaxed);
    synth_node.kill.store(true, Ordering::Relaxed);
}
