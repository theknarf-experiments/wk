//! Run `torrent-file-editor.wasm` as a real wk node, headless, and check that
//! it renders a real Qt window with a real .torrent loaded in it.
//!
//! WHY THIS EXISTS. "It links" is worth very little for this port: three of
//! the seven qtbase patches fix bugs that only appear when a real executable
//! is linked, and a fourth only at runtime. So the deliverable is not the
//! binary, it is a run. This harness plays exactly the role wk's own
//! `crates/wk-server` tests play for gfx-smoke and qt-smoke: it drives
//! `PluginHost` — the same runtime the daemon uses — pumps one frame per
//! iteration the way the compositor does, and reads back the pixels the guest
//! presented.
//!
//! It is a STANDALONE cargo project rather than another `#[test]` in
//! `crates/wk-server/src/plugin.rs` because that file is shared and other Qt
//! ports are being written against it at the same time.
//!
//! WHAT IT ASSERTS, in order, and each is a different claim:
//!
//!   1. the node opens a wk surface at all (QApplication + the wk QPA plugin
//!      came up, and the font database is not empty);
//!   2. the composited frame is a WIDGET FRAME — thousands of dark pixels
//!      (text, borders, the tree view's grid) against a large light Fusion
//!      background — not a blank buffer and not a gradient;
//!   3. the .torrent that arrived over the node's filesystem actually PARSED:
//!      the app reports the torrent's name, its info hash, its total size and
//!      its file count, all of which come out of BencodeModel. A pixel
//!      histogram cannot tell a loaded torrent from an empty form;
//!   4. the app is EDITABLE, not just readable: a genuine `wasi:surface`
//!      pointer press/release focuses the Name field and real key events TYPE
//!      INTO IT — the field's own text changes, the pixels inside its rect
//!      change with it, and a Backspace (a key with no text at all) takes the
//!      last character back;
//!   5. multi-window: the same kind of click aimed at the About button's
//!      published rect opens the About dialog, which the app sees as a second
//!      visible top-level — so this also exercises multi-window compositing
//!      and a nested QDialog::exec() inside the frame-paced event dispatcher.
//!
//! Claim 4 is the one this app existed without. The compositor built every
//! `wasi:surface` key event with `text: none` (a literal `None` in
//! `crates/client-local-ui/src/compositor/input.rs`), and the Qt QPA has
//! nothing else to put in `QKeyEvent::text()`, so `QWidgetLineControl`
//! inserted nothing: every QLineEdit in a Qt wk node was effectively
//! read-only and torrent-file-editor was a torrent *viewer*. The check below
//! is deliberately the user-visible claim — the string in the Name field —
//! and not "a KeyEvent carried a non-empty string", which is the same
//! sentence one layer too high to be worth anything.
//!
//! All three frames are written out as PPM (`--dump <dir>`): a histogram
//! proves "not blank", never "the right window".
//!
//! Usage: cargo run -- [--wasm PATH] [--dump DIR]

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wk_protocol::NodeId;
use wk_server::plugin::{
    Key, KeyEvent, NodeRegistry, PluginHost, PointerButton, PointerEvent, SurfaceRegistry,
    VirtualSurface,
};

/// The .torrent this run feeds the node. Prefers `../test/demo.torrent` — the
/// fixture the example workspace bind-mounts — so the committed file is the
/// one actually proven to parse, and falls back to the identical bytes built
/// in code below when it is missing.
fn torrent_bytes() -> Vec<u8> {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../test/demo.torrent");
    match std::fs::read(&fixture) {
        Ok(b) => {
            eprintln!("using fixture {}", fixture.display());
            b
        }
        Err(_) => sample_torrent(),
    }
}

/// A small but genuinely valid multi-file .torrent, built here so the harness
/// can run with no fixture on disk and so the values it asserts on are known.
fn sample_torrent() -> Vec<u8> {
    fn s(out: &mut Vec<u8>, b: &[u8]) {
        out.extend_from_slice(format!("{}:", b.len()).as_bytes());
        out.extend_from_slice(b);
    }
    fn i(out: &mut Vec<u8>, n: i64) {
        out.extend_from_slice(format!("i{n}e").as_bytes());
    }
    // Bencode dictionary keys must be in lexicographic order or a strict
    // parser rejects the file.
    let mut t = Vec::new();
    t.push(b'd');
    s(&mut t, b"announce");
    s(&mut t, b"http://tracker.invalid/announce");
    s(&mut t, b"created by");
    s(&mut t, b"wk qt port harness");
    s(&mut t, b"creation date");
    i(&mut t, 1_700_000_000);
    s(&mut t, b"encoding");
    s(&mut t, b"UTF-8");
    s(&mut t, b"info");
    {
        t.push(b'd');
        s(&mut t, b"files");
        {
            t.push(b'l');
            for (len, path) in [(1_048_576i64, "hello.txt"), (2_097_152, "world.bin")] {
                t.push(b'd');
                s(&mut t, b"length");
                i(&mut t, len);
                s(&mut t, b"path");
                t.push(b'l');
                s(&mut t, path.as_bytes());
                t.push(b'e');
                t.push(b'e');
            }
            t.push(b'e');
        }
        s(&mut t, b"name");
        s(&mut t, b"wk-qt-demo");
        s(&mut t, b"piece length");
        i(&mut t, 262_144);
        s(&mut t, b"pieces");
        // 12 pieces' worth of 20-byte SHA1 placeholders. Nothing verifies
        // them; they only have to be a multiple of 20 bytes long.
        let pieces: Vec<u8> = (0..12 * 20).map(|n| (n * 7 % 251) as u8).collect();
        s(&mut t, &pieces);
        t.push(b'e');
    }
    t.push(b'e');
    t
}

fn write_ppm(path: &Path, w: u32, h: u32, pixels: &[u8]) {
    let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
    ppm.extend(pixels.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]));
    std::fs::write(path, ppm).expect("write frame dump");
    eprintln!("frame dumped to {}", path.display());
}

fn main() {
    let mut wasm = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../torrent-file-editor.wasm");
    let mut dump = PathBuf::from("/tmp");
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--wasm" => wasm = PathBuf::from(args.next().expect("--wasm PATH")),
            "--dump" => dump = PathBuf::from(args.next().expect("--dump DIR")),
            other => panic!("unknown argument {other}"),
        }
    }
    assert!(
        wasm.exists(),
        "no component at {} -- run ./build.sh",
        wasm.display()
    );

    let host = PluginHost::new().expect("host");
    let nodes: NodeRegistry = Arc::new(Mutex::new(Vec::new()));
    let surfaces: SurfaceRegistry = Arc::new(Mutex::new(Vec::new()));
    let id = NodeId::new();
    host.spawn(
        &wasm,
        "torrent-file-editor",
        id,
        // argv[1]: main.cpp opens a single filename argument if it exists.
        // This is exactly what a wk workspace's `args` line does, and the
        // path is where a BindMount would land it.
        &["/data/example.torrent".to_string()],
        surfaces.clone(),
        nodes.clone(),
        Vec::new(),
        Some(wk_server::images::ContainerSetup {
            layers: Vec::new(),
            env: vec![("WK_TFE_SELFTEST".into(), "1".into())],
        }),
    )
    .expect("spawn");

    // The node appears immediately; the guest only starts once the background
    // compile of the 22 MB component finishes, so seeding the file now is
    // comfortably ahead of main().
    let node = {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(n) = nodes.lock().unwrap().iter().find(|n| n.id == id).cloned() {
                break n;
            }
            assert!(Instant::now() < deadline, "node never appeared");
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    {
        let torrent = torrent_bytes();
        eprintln!("seeding /data/example.torrent ({} bytes)", torrent.len());
        let mut fs = node.fs.lock().unwrap();
        fs.ensure_dir_path("data");
        fs.put_file_at("data/example.torrent", torrent);
    }

    let log_now = || String::from_utf8_lossy(&node.term_io.log_read(0).0).to_string();

    let surface = {
        let deadline = Instant::now() + Duration::from_secs(600);
        loop {
            if let Some(s) = surfaces.lock().unwrap().first().cloned() {
                break s;
            }
            assert!(
                !node.finished.load(Ordering::Relaxed),
                "the node exited before opening a surface; log:\n{}",
                log_now()
            );
            assert!(
                Instant::now() < deadline,
                "no surface after 10 minutes; log:\n{}",
                log_now()
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

    // --- claim 2: a widget frame, not a blank buffer -----------------------
    let deadline = Instant::now() + Duration::from_secs(300);
    let (dark, light) = loop {
        pump_frame();
        std::thread::sleep(Duration::from_millis(15));
        {
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
            if dark > 1000 && light > 10_000 {
                break (dark, light);
            }
        }
        assert!(
            Instant::now() < deadline,
            "never painted a widget frame; log:\n{}",
            log_now()
        );
    };
    eprintln!("frame: {dark} dark px, {light} light px");

    // --- claim 3: the .torrent parsed --------------------------------------
    // STATE title='...' name='...' hash='...' size='...' files=N tops=N focus='...'
    let state = |log: &str| -> Option<String> {
        log.lines()
            .rfind(|l| l.starts_with("STATE "))
            .map(|s| s.to_string())
    };
    let field = |line: &str, key: &str| -> String {
        line.split_once(&format!("{key}='"))
            .and_then(|(_, rest)| rest.split_once('\''))
            .map(|(v, _)| v.to_string())
            .unwrap_or_default()
    };
    let deadline = Instant::now() + Duration::from_secs(120);
    let state_line = loop {
        pump_frame();
        std::thread::sleep(Duration::from_millis(15));
        let log = log_now();
        if let Some(line) = state(&log) {
            if !field(&line, "name").is_empty() {
                break line;
            }
        }
        assert!(
            Instant::now() < deadline,
            "the torrent never showed up in the UI; log:\n{}",
            log_now()
        );
    };
    eprintln!("{state_line}");
    assert_eq!(
        field(&state_line, "name"),
        "wk-qt-demo",
        "torrent name in the UI"
    );
    assert!(
        field(&state_line, "hash").len() == 40,
        "info hash should be 40 hex chars, got '{}'",
        field(&state_line, "hash")
    );
    assert!(
        state_line.contains("files=2"),
        "both files should be listed: {state_line}"
    );

    {
        let s = surface.lock().unwrap();
        write_ppm(&dump.join("tfe-main.ppm"), s.width, s.height, &s.pixels);
    }

    // A published widget rect, in surface coordinates. The LAST such line,
    // never the first: the app republishes on a repeating timer and the wk QPA
    // forces the first top-level to the full surface a few frames in, so an
    // early reading is the pre-resize geometry and aiming at it misses.
    let rect = |log: &str, tag: &str| -> Option<(i64, i64, i64, i64)> {
        let line = log.lines().rfind(|l| l.starts_with(tag))?;
        let n: Vec<i64> = line[tag.len()..]
            .split_whitespace()
            .filter_map(|t| t.parse().ok())
            .collect();
        (n.len() == 4).then(|| (n[0], n[1], n[2], n[3]))
    };

    // --- claim 4: the Name field is EDITABLE ------------------------------
    // Before the About click, not after: About is modal, and a modal dialog
    // takes the keyboard focus this whole section rests on.
    let click = |s: &mut VirtualSurface, x: f64, y: f64| {
        s.pointer_move
            .push_back(PointerEvent { x, y, button: None });
        s.pointer_down.push_back(PointerEvent {
            x,
            y,
            button: Some(PointerButton::Left),
        });
        s.pointer_up.push_back(PointerEvent {
            x,
            y,
            button: Some(PointerButton::Left),
        });
        s.wake();
    };
    // One key, pressed and released. `text` is the half that was missing:
    // `key` alone says which key moved, never what it types, and a QLineEdit
    // inserts the text or nothing at all. Backspace is sent with text=None,
    // which is what winit reports for it — so this also checks that a
    // textless key still acts on the field instead of being ignored.
    let typed = |s: &mut VirtualSurface, key: Key, text: Option<&str>| {
        let ev = KeyEvent {
            key: Some(key),
            text: text.map(str::to_owned),
            alt_key: false,
            ctrl_key: false,
            meta_key: false,
            shift_key: false,
            repeat: false,
        };
        s.key_down.push_back(ev.clone());
        s.key_up.push_back(ev);
        s.wake();
    };
    // The Name field's pixels, so "the widget repainted" can be asserted and
    // not merely assumed from the string.
    let field_pixels = |r: (i64, i64, i64, i64)| -> Vec<u8> {
        let s = surface.lock().unwrap();
        let mut out = Vec::new();
        for y in r.1.max(0)..(r.1 + r.3).min(s.height as i64) {
            for x in r.0.max(0)..(r.0 + r.2).min(s.width as i64) {
                let i = ((y as u32 * s.width + x as u32) * 4) as usize;
                out.extend_from_slice(&s.pixels[i..i + 4]);
            }
        }
        out
    };

    let deadline = Instant::now() + Duration::from_secs(120);
    let name_rect = loop {
        pump_frame();
        std::thread::sleep(Duration::from_millis(15));
        if let Some(r) = rect(&log_now(), "LENAME ") {
            break r;
        }
        assert!(
            Instant::now() < deadline,
            "the app never published the Name field's rect; log:\n{}",
            log_now()
        );
    };
    let before_text = field(&state_line, "name");
    let before_pixels = field_pixels(name_rect);
    let (nx, ny) = (
        (name_rect.0 + name_rect.2 / 2) as f64,
        (name_rect.1 + name_rect.3 / 2) as f64,
    );
    eprintln!("clicking the Name field at ({nx}, {ny}) to focus it");

    // Focus first, and prove it: text delivered while nothing is focused goes
    // nowhere and looks exactly like text that never arrived. Without this
    // step a failure below could not be read.
    //
    // ONE click per attempt, a second apart — not a click every frame, which is
    // what the About check below can afford to do. Clicks that land inside
    // QApplication::doubleClickInterval() at the same spot are a double and
    // then a TRIPLE click, and a triple click in a QLineEdit selects the whole
    // line: the field then reads 'xyz' rather than 'wk-qt-demoxyz' and the
    // assertion can no longer tell insertion from replacement. Which is the
    // more interesting claim — a cursor sitting where the user clicked.
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        click(&mut surface.lock().unwrap(), nx, ny);
        let attempt = Instant::now() + Duration::from_secs(1);
        while Instant::now() < attempt {
            pump_frame();
            std::thread::sleep(Duration::from_millis(15));
        }
        if state(&log_now()).is_some_and(|l| field(&l, "focus") == "leName") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "a real pointer click at ({nx}, {ny}) never focused the Name field; log:\n{}",
            log_now()
        );
    }
    eprintln!("the Name field has keyboard focus");

    // First, the OLD event shape: a perfectly good `key` and `text: none`,
    // which is what every key event the compositor built used to look like.
    // This is the negative control, and it belongs here rather than in a
    // comment — without it, "the field says wk-qt-demoxyz" is equally
    // consistent with Qt reconstructing the letter from the key code, and the
    // test would not actually be about `text` at all. It must type NOTHING.
    typed(&mut surface.lock().unwrap(), Key::KeyX, None);
    let until = Instant::now() + Duration::from_secs(3);
    while Instant::now() < until {
        pump_frame();
        std::thread::sleep(Duration::from_millis(15));
        let now = state(&log_now())
            .map(|l| field(&l, "name"))
            .unwrap_or_default();
        assert_eq!(
            now, before_text,
            "a key event with text: none typed into the Name field. The whole \
             defect was that `text` never arrived, so if the key code alone can \
             produce a character this test proves nothing about the fix."
        );
    }
    eprintln!("a key with no text typed nothing, as it must");

    // Now the same three keys WITH their text. Three ordinary letters, then a
    // Backspace that must remove exactly the last of them.
    {
        let mut s = surface.lock().unwrap();
        typed(&mut s, Key::KeyX, Some("x"));
        typed(&mut s, Key::KeyY, Some("y"));
        typed(&mut s, Key::KeyZ, Some("z"));
    }
    // The exact string, not `contains("xyz")`. The click landed past the end of
    // a short name in a wide field, so Qt put the cursor at the end and the
    // three characters must APPEND: an equality here is what distinguishes
    // "text was inserted at the cursor" from "the field was replaced wholesale"
    // (which is what a stray selection, or a QLineEdit reset by the model,
    // would look like).
    let want = format!("{before_text}xyz");
    let deadline = Instant::now() + Duration::from_secs(120);
    let typed_line = loop {
        pump_frame();
        std::thread::sleep(Duration::from_millis(15));
        if let Some(l) = state(&log_now()) {
            if field(&l, "name") == want {
                break l;
            }
        }
        assert!(
            Instant::now() < deadline,
            "typed characters never reached the Name field -- it reads '{}', want '{want}'. \
             An unchanged '{before_text}' is the bug the fix was for: the compositor built \
             every key event with text: none, so the QLineEdit had nothing to insert. log:\n{}",
            state(&log_now())
                .map(|l| field(&l, "name"))
                .unwrap_or_default(),
            log_now()
        );
    };
    eprintln!("{typed_line}");

    // The edit reached the app's DATA, not only its widget: renaming the
    // torrent re-encodes the info dict, so BencodeModel hands back a different
    // info hash and the window title gains its modified marker. A QLineEdit
    // that swallowed the text and told nobody would pass everything above.
    assert_ne!(
        field(&typed_line, "hash"),
        field(&state_line, "hash"),
        "renaming the torrent must re-hash its info dict"
    );
    assert!(
        field(&typed_line, "title").starts_with('*'),
        "the app should mark the document modified: {typed_line}"
    );

    // The characters were also DRAWN. The string above comes from
    // leName->text(), which a broken paint path would report just as happily;
    // these are the pixels a user looks at.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        pump_frame();
        std::thread::sleep(Duration::from_millis(15));
        if field_pixels(name_rect) != before_pixels {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the Name field's text changed but its pixels never did -- typed text \
             that is not repainted; log:\n{}",
            log_now()
        );
    }
    eprintln!("the Name field repainted with the typed text");
    {
        let s = surface.lock().unwrap();
        write_ppm(&dump.join("tfe-typed.ppm"), s.width, s.height, &s.pixels);
    }

    // Backspace: a key with no text at all still has to act on the field, and
    // has to take back exactly one character. This is the other half of the
    // pair — `key` doing the work `text` cannot.
    typed(&mut surface.lock().unwrap(), Key::Backspace, None);
    let after_bs = format!("{before_text}xy");
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        pump_frame();
        std::thread::sleep(Duration::from_millis(15));
        if state(&log_now()).is_some_and(|l| field(&l, "name") == after_bs) {
            eprintln!("Backspace took the 'z' back: name='{after_bs}'");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Backspace never reached the Name field -- it reads '{}', want '{after_bs}'; log:\n{}",
            state(&log_now())
                .map(|l| field(&l, "name"))
                .unwrap_or_default(),
            log_now()
        );
    }

    // --- claim 5: real input opens the About dialog ------------------------
    let (bx, by, bw, bh) =
        rect(&log_now(), "BTNABOUT ").expect("the app never published the About rect");
    let (cx, cy) = ((bx + bw / 2) as f64, (by + bh / 2) as f64);
    eprintln!("clicking the About button at ({cx}, {cy})");

    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        {
            let mut s = surface.lock().unwrap();
            if s.pointer_down.is_empty() && s.pointer_up.is_empty() {
                s.pointer_move.push_back(PointerEvent {
                    x: cx,
                    y: cy,
                    button: None,
                });
                s.pointer_down.push_back(PointerEvent {
                    x: cx,
                    y: cy,
                    button: Some(PointerButton::Left),
                });
                s.pointer_up.push_back(PointerEvent {
                    x: cx,
                    y: cy,
                    button: Some(PointerButton::Left),
                });
            }
            s.frame_ready = true;
            s.wake();
        }
        std::thread::sleep(Duration::from_millis(15));
        let log = log_now();
        if let Some(line) = state(&log) {
            if line.contains("tops=2") {
                eprintln!("{line}");
                eprintln!("the About dialog is up: a SECOND top-level window is composited");
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "the About dialog never opened; log:\n{}",
            log_now()
        );
    }

    // A few more frames so the dialog is fully painted before the snapshot.
    for _ in 0..20 {
        pump_frame();
        std::thread::sleep(Duration::from_millis(15));
    }
    {
        let s = surface.lock().unwrap();
        write_ppm(&dump.join("tfe-about.ppm"), s.width, s.height, &s.pixels);
    }

    println!("\n--- node log ---\n{}", log_now());
    println!("PASS");

    // Close the surface: the guest traps on its next get-frame and exits.
    {
        let mut s = surface.lock().unwrap();
        s.closed = true;
        s.wake();
    }
    node.kill.store(true, Ordering::Relaxed);
}
