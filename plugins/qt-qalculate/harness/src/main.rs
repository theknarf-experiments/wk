//! Run `qalculate-qt.wasm` as a real wk node, headless, and check that it
//! renders a real Qt window AND actually evaluates real expressions in it.
//!
//! WHY THIS EXISTS. "It links" is worth very little for this port, and "it
//! renders" is worth surprisingly little too. The failure this port was built
//! to avoid does not look like a crash and does not look like a blank frame:
//! libqalculate dispatches every timed calculation through pthreads, wasi-libc's
//! `pthread_create` returns ENOTSUP, and the unpatched library therefore
//! answers every single expression with the string `aborted` — in a window
//! that paints perfectly, with a full keypad, a menu bar and a blinking
//! cursor. A pixel histogram would call that a pass. So the deliverable here
//! is not the binary and not a frame; it is a NUMBER that came out of
//! libqalculate and reached the screen.
//!
//! It drives `PluginHost` — the same runtime the daemon uses — pumping one
//! frame per iteration the way the compositor does, exactly as the sibling
//! `qt-torrentfileeditor103` harness does. It is a STANDALONE cargo project
//! rather than another `#[test]` in `crates/wk-server/src/plugin.rs` because
//! that file is shared and other Qt ports are being written against it at the
//! same time.
//!
//! WHAT IT ASSERTS, in order, and each is a different claim:
//!
//!   1. the node opens a wk surface at all (QApplication plus the wk QPA
//!      plugin came up) and reports a non-empty font database;
//!   2. the composited frame is a WIDGET FRAME — thousands of dark pixels
//!      (text, the keypad's button borders, the history frame) against a large
//!      light Fusion background — not a blank buffer;
//!   3. THE NODE COMPUTED. `5 m + 2 ft to cm` arrives the way a workspace
//!      passes it, as the node's `args`, and the history view comes back
//!      reading `560.96 cm`. That single string exercises the parser, the unit
//!      database loaded from the definitions compiled into the binary, GMP and
//!      MPFR, the printer, and — the point — libqalculate's threaded
//!      `Calculator::calculate()` entry point running on wk's inline-thread
//!      patch. Unpatched, this reads `aborted`;
//!   4. the calculator is INTERACTIVE. A genuine `wasi:surface` pointer
//!      press/release focuses the expression editor; a genuine Escape — a key
//!      with NO `text` at all, so it can only act through its key code —
//!      clears it; genuine key events WITH their `text` type `1234*5678` into
//!      it, which reads back as `1234×5678` because ExpressionEdit substitutes
//!      the multiplication sign as you type; the node computes `7006652`; and
//!      a genuine Return commits it, which is asserted through the app's own
//!      expression history rather than through the answer, because qalculate
//!      auto-calculates as you type and the answer is on screen before Return
//!      is pressed.
//!      All digits and one operator, deliberately: no letters means
//!      ExpressionEdit's completion popup cannot appear, and Return therefore
//!      cannot be swallowed by a completion (expressionedit.cpp accepts the
//!      highlighted completion on Return when the popup is up);
//!   5. the answer was DRAWN, not merely computed: the pixels of the band the
//!      history view occupies change between claim 3 and claim 5.
//!
//! Both frames are written out as PPM (`--dump <dir>`), and doc/ carries a PNG
//! of one of them. A histogram proves "not blank", never "the right window".
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

/// The expression handed to the node as `args`, and the answer it must reach.
/// Chosen because it is the shortest thing that fails loudly if any layer is
/// missing: two different units, a unit CONVERSION, a decimal result.
const ARG_EXPRESSION: &str = "5 m + 2 ft to cm";
const ARG_ANSWER: &str = "560.96 cm";

/// The expression the harness TYPES. All digits and one operator, on purpose:
/// see the module comment. It reads back with a MULTIPLICATION SIGN, not the
/// asterisk that was typed: ExpressionEdit substitutes U+00D7 as you type
/// (settings->printops.multiplication_sign), which is itself evidence that the
/// keystrokes went through the app's own input handling and not straight into
/// a plain text buffer.
const TYPED_KEYS: &str = "1234*5678";
const TYPED_EXPRESSION: &str = "1234\u{00d7}5678";
const TYPED_ANSWER: &str = "7006652";

/// Compare answers with every space removed. qalculate DIGIT-GROUPS its
/// output — `7006652` is displayed as `7 006 652`, and the separator is a
/// narrow no-break space, not an ASCII one — and it prefixes the result with
/// `=` or `≈` depending on whether the value is exact. Neither is worth
/// pinning in a test, but the digits are.
fn squash(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

fn write_ppm(path: &Path, w: u32, h: u32, pixels: &[u8]) {
    let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
    ppm.extend(pixels.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]));
    std::fs::write(path, ppm).expect("write frame dump");
    eprintln!("frame dumped to {}", path.display());
}

fn main() {
    let mut wasm = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../qalculate-qt.wasm");
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
        "qalculate-qt",
        id,
        // Positional arguments are the expression to calculate: main.cpp joins
        // them and calls win->calculate() before entering the event loop. This
        // is exactly what a wk workspace's `args` line does.
        &[ARG_EXPRESSION.to_string()],
        surfaces.clone(),
        nodes.clone(),
        Vec::new(),
        Some(wk_server::images::ContainerSetup {
            layers: Vec::new(),
            env: vec![
                ("WK_QALC_SELFTEST".into(), "1".into()),
                // libqalculate writes its preferences and history under
                // getLocalDir(), which is derived from HOME. Unset, the
                // patched getHomeDir() returns "/" and the app would scatter
                // dotfiles at the root of the node's vfs.
                ("HOME".into(), "/root".into()),
            ],
        }),
    )
    .expect("spawn");

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

    // --- narration helpers --------------------------------------------------
    // STATE expr='...' result='...' tops=N popup=N focus='...'
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

    // The font database has to be non-empty or every string renders as
    // nothing at all and every claim below would be about invisible text.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        pump_frame();
        std::thread::sleep(Duration::from_millis(15));
        let log = log_now();
        if let Some(line) = log.lines().find(|l| l.starts_with("QALC platform=")) {
            eprintln!("{line}");
            assert!(
                line.contains("platform=wk"),
                "the node came up on the wrong QPA: {line}"
            );
            let families: i64 = line
                .split_once("families=")
                .and_then(|(_, v)| v.split_whitespace().next())
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            // Qt 6 ships no fonts and a node has no host font directory: with
            // an empty database the app runs and every string renders as
            // nothing at all, which would make every claim below a claim about
            // invisible text.
            assert!(families > 0, "empty QFontDatabase: {line}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the app never narrated its platform; log:\n{}",
            log_now()
        );
    }

    // --- claim 3: THE NODE COMPUTED ----------------------------------------
    // The expression came in as the node's args and was evaluated before the
    // event loop even started. `560.96 cm` is the whole of libqalculate
    // working: parser, unit definitions, GMP/MPFR, printer, and the threaded
    // Calculator::calculate() entry point on wk's inline-thread patch. The
    // unpatched library answers `aborted` here, in a frame that looks fine.
    let deadline = Instant::now() + Duration::from_secs(180);
    let arg_state = loop {
        pump_frame();
        std::thread::sleep(Duration::from_millis(15));
        if let Some(line) = state(&log_now()) {
            let r = field(&line, "result");
            if !r.is_empty() {
                assert_ne!(
                    r, "aborted",
                    "libqalculate returned `aborted` -- this is the pthread_create ENOTSUP \
                     failure that patches/libqalculate-0002-wasi-inline-threads.patch exists \
                     to fix. The window renders perfectly either way; that is the point of \
                     asserting on the string. Line: {line}"
                );
                if squash(&r).contains(&squash(ARG_ANSWER)) {
                    break line;
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "`{ARG_EXPRESSION}` never produced `{ARG_ANSWER}`; log:\n{}",
            log_now()
        );
    };
    eprintln!("{arg_state}");
    eprintln!("the node evaluated `{ARG_EXPRESSION}` -> `{ARG_ANSWER}`");
    {
        let s = surface.lock().unwrap();
        write_ppm(&dump.join("qalc-args.ppm"), s.width, s.height, &s.pixels);
    }

    // --- claim 4: typing into it works -------------------------------------
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
    // One key, pressed and released. `text` is the half that matters: `key`
    // alone says which key moved, never what it types, and a text editor
    // inserts the text or nothing at all.
    let typed = |s: &mut VirtualSurface, key: Key, text: Option<&str>, shift: bool| {
        let ev = KeyEvent {
            key: Some(key),
            text: text.map(str::to_owned),
            alt_key: false,
            ctrl_key: false,
            meta_key: false,
            shift_key: shift,
            repeat: false,
        };
        s.key_down.push_back(ev.clone());
        s.key_up.push_back(ev);
        s.wake();
    };

    let deadline = Instant::now() + Duration::from_secs(120);
    let ee_rect = loop {
        pump_frame();
        std::thread::sleep(Duration::from_millis(15));
        if let Some(r) = rect(&log_now(), "EXPREDIT ") {
            break r;
        }
        assert!(
            Instant::now() < deadline,
            "the app never published the expression editor's rect; log:\n{}",
            log_now()
        );
    };
    let (ex, ey) = (
        (ee_rect.0 + ee_rect.2 / 2) as f64,
        (ee_rect.1 + ee_rect.3 / 2) as f64,
    );
    eprintln!("clicking the expression editor at ({ex}, {ey}) to focus it");

    // Focus first, and prove it: text delivered while nothing is focused goes
    // nowhere and looks exactly like text that never arrived. ONE click per
    // attempt, a second apart, so repeated attempts cannot become a
    // double/triple click that selects the line.
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        click(&mut surface.lock().unwrap(), ex, ey);
        let attempt = Instant::now() + Duration::from_secs(1);
        while Instant::now() < attempt {
            pump_frame();
            std::thread::sleep(Duration::from_millis(15));
        }
        if state(&log_now()).is_some_and(|l| field(&l, "focus") == "ExpressionEdit") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "a real pointer click at ({ex}, {ey}) never focused the expression editor; log:\n{}",
            log_now()
        );
    }
    eprintln!("the expression editor has keyboard focus");

    // The editor still holds the expression that came in as args -- qalculate
    // leaves it there so you can amend it. Escape is the app's own "clear the
    // expression" gesture (expressionedit.cpp's Key_Escape arm), and it is a
    // key with NO text at all, so this doubles as the negative control for the
    // typing below: a keystroke that acts on the widget through its key code
    // alone. Exactly one, and only while the document is non-empty: Escape on
    // an EMPTY document is "close the window" (settings->close_with_esc).
    assert!(
        !state(&log_now())
            .map(|l| field(&l, "expr"))
            .unwrap_or_default()
            .is_empty(),
        "the editor was already empty -- Escape would close the app"
    );
    typed(&mut surface.lock().unwrap(), Key::Escape, None, false);
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        pump_frame();
        std::thread::sleep(Duration::from_millis(15));
        if state(&log_now()).is_some_and(|l| field(&l, "expr").is_empty()) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Escape (a key with no text) never cleared the expression editor; log:\n{}",
            log_now()
        );
    }
    eprintln!("Escape -- a key with no text at all -- cleared the editor");

    // The history view's pixels, so "it was drawn" can be asserted rather than
    // assumed from a string. Taken now, before the second calculation.
    let history_pixels = || -> Vec<u8> {
        let s = surface.lock().unwrap();
        // The bottom two thirds of the surface: the history view fills the
        // area above the keypad and below the toolbar, and its own rect is not
        // published. A whole-frame comparison would be satisfied by a blinking
        // cursor, which is why this is a band and not the frame.
        let y0 = s.height / 4;
        let y1 = s.height / 2;
        let mut out = Vec::new();
        for y in y0..y1 {
            for x in 0..s.width {
                let i = ((y * s.width + x) * 4) as usize;
                out.extend_from_slice(&s.pixels[i..i + 4]);
            }
        }
        out
    };
    let before_pixels = history_pixels();

    // Type it. Digit8 with shift is where `*` lives on the US layout the
    // compositor reports; the QPA inserts `text`, not the key code.
    let keys: Vec<(Key, &str, bool)> = vec![
        (Key::Digit1, "1", false),
        (Key::Digit2, "2", false),
        (Key::Digit3, "3", false),
        (Key::Digit4, "4", false),
        (Key::Digit8, "*", true),
        (Key::Digit5, "5", false),
        (Key::Digit6, "6", false),
        (Key::Digit7, "7", false),
        (Key::Digit8, "8", false),
    ];
    {
        let mut s = surface.lock().unwrap();
        for (k, t, shift) in keys {
            typed(&mut s, k, Some(t), shift);
        }
    }
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        pump_frame();
        std::thread::sleep(Duration::from_millis(15));
        if let Some(l) = state(&log_now()) {
            if field(&l, "expr") == TYPED_EXPRESSION {
                eprintln!("{l}");
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "typed characters never reached the expression editor -- it reads '{}', want \
             '{TYPED_EXPRESSION}'; log:\n{}",
            state(&log_now())
                .map(|l| field(&l, "expr"))
                .unwrap_or_default(),
            log_now()
        );
    }
    eprintln!("real key events typed `{TYPED_KEYS}` into the expression editor (it reads `{TYPED_EXPRESSION}`)");

    // The answer to the TYPED expression. qalculate auto-calculates as you
    // type, so this may well be on screen before Return is pressed -- which is
    // a stronger statement than it looks: the whole parse/evaluate/print/paint
    // pipeline is running once per keystroke inside the frame-paced event
    // dispatcher, not once per Return.
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        pump_frame();
        std::thread::sleep(Duration::from_millis(15));
        if let Some(l) = state(&log_now()) {
            let r = field(&l, "result");
            assert!(
                !r.contains("aborted"),
                "libqalculate returned `aborted` for the typed expression: {l}"
            );
            if squash(&r).contains(TYPED_ANSWER) {
                eprintln!("{l}");
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "`{TYPED_EXPRESSION}` never produced `{TYPED_ANSWER}`; log:\n{}",
            log_now()
        );
    }
    eprintln!("the node evaluated the typed expression -> `{TYPED_ANSWER}`");

    // Return. No `text`: that is what winit reports for it, and the editor has
    // to act on the key code alone. `hist` is the assertion that separates
    // "the live preview updated" from "Return committed it": ExpressionEdit
    // ::addToHistory() only runs on a committed expression.
    let hist_before: i64 = state(&log_now())
        .and_then(|l| l.split_once("hist=").map(|(_, v)| v.to_string()))
        .and_then(|v| v.split_whitespace().next().and_then(|n| n.parse().ok()))
        .unwrap_or(-1);
    typed(&mut surface.lock().unwrap(), Key::Enter, None, false);

    let deadline = Instant::now() + Duration::from_secs(180);
    let typed_state = loop {
        pump_frame();
        std::thread::sleep(Duration::from_millis(15));
        if let Some(l) = state(&log_now()) {
            let hist: i64 = l
                .split_once("hist=")
                .and_then(|(_, v)| v.split_whitespace().next())
                .and_then(|n| n.parse().ok())
                .unwrap_or(-1);
            if hist > hist_before && field(&l, "last") == TYPED_EXPRESSION {
                break l;
            }
        }
        assert!(
            Instant::now() < deadline,
            "Return never committed `{TYPED_EXPRESSION}` to the expression history \
             (hist was {hist_before}); log:\n{}",
            log_now()
        );
    };
    eprintln!("{typed_state}");
    eprintln!("Return committed it to the expression history");

    // --- claim 5: and it was DRAWN -----------------------------------------
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        pump_frame();
        std::thread::sleep(Duration::from_millis(15));
        if history_pixels() != before_pixels {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the history view reported a new answer but its pixels never changed -- a result \
             that is computed and not repainted; log:\n{}",
            log_now()
        );
    }
    eprintln!("the history view repainted with the new answer");

    // A few more frames so everything has settled before the snapshot.
    for _ in 0..20 {
        pump_frame();
        std::thread::sleep(Duration::from_millis(15));
    }
    {
        let s = surface.lock().unwrap();
        write_ppm(&dump.join("qalc-typed.ppm"), s.width, s.height, &s.pixels);
    }

    println!("\n--- node log (tail) ---");
    let log = log_now();
    for line in log.lines().rev().take(6).collect::<Vec<_>>().iter().rev() {
        println!("{line}");
    }
    println!("PASS");

    // Close the surface: the guest traps on its next get-frame and exits.
    {
        let mut s = surface.lock().unwrap();
        s.closed = true;
        s.wake();
    }
    node.kill.store(true, Ordering::Relaxed);
}
