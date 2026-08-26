//! Run `kcalc.wasm` as a real wk node, headless, and check that KDE — not just
//! Qt — actually came up and did arithmetic.
//!
//! WHY THIS EXISTS. "Fourteen KDE frameworks linked" is a weak claim. Almost
//! every patch in `../patches/` removes a code path (a dlopen, a fork, a
//! timezone lookup), and the interesting question is not whether the remains
//! compile but whether what is left still *is* KDE: does a KXmlGuiWindow open,
//! does KConfig read its settings, does KLocalizedString return strings, do
//! keystrokes reach the app, and does KNumber — i.e. GMP, MPFR and MPC, three
//! autotools cross-builds nothing else in this port exercises — compute?
//!
//! It plays exactly the role wk's own `crates/wk-server` tests play for
//! gfx-smoke and qt-smoke: it drives `PluginHost` — the same runtime the
//! daemon uses — pumps one frame per iteration the way the compositor does,
//! and reads back the pixels the guest presented.
//!
//! WHAT IT ASSERTS, in order, and each is a different claim:
//!
//!   1. the node opens a wk surface at all — QApplication came up, the wk QPA
//!      plugin was found by Q_IMPORT_PLUGIN, and the static-plugin path
//!      survived `patches/kcoreaddons-0002-no-qlibrary`;
//!   2. the composited frame is a WIDGET FRAME — thousands of dark pixels
//!      (the digit buttons' text and borders) against a large light Fusion
//!      background — not a blank buffer. This is where a font database that
//!      came up empty would show;
//!   3. a VISIBLE top-level window (`tops=1`), which means KCalculator's
//!      constructor ran to completion: KXmlGuiWindow, KActionCollection,
//!      `setupGUI()` reading `:/kxmlgui5/kcalc/kcalcui.rc` out of qrc and
//!      KXMLGUIFactory building the menubar from it, KConfigSkeleton loading
//!      `kcalc_settings`, KLocalizedString on every label. Any one of those
//!      aborting leaves a surface but never gets here;
//!   4. a real `wasi:surface` key event reaches the app: pressing `8` puts an
//!      8 in the expression line. KCalc binds its keypad with QShortcut/
//!      setShortcut, so this exercises the QPA's key path AND Qt's shortcut
//!      matching, not just a focused line edit;
//!   5. ARITHMETIC. `8 / 2` must show `4`, and `1 / 8` must show `0.125`. The
//!      first goes through KNumber's integer path (GMP), the second cannot
//!      stay an integer and lands in the float path (MPFR). The expected echo
//!      is `8÷2` with U+00F7 — KCalc renders the DIVISION SIGN, not the slash
//!      that was typed, so asserting on it also proves the token went through
//!      KCalc's own tokenizer rather than landing in a QLineEdit verbatim.
//!      This is the assertion that makes the whole port mean something: it
//!      cannot pass unless KCalc's parser, KNumber and both bignum libraries
//!      are genuinely working inside the component.
//!
//! Claim 5 is the one a pixel histogram can never make. No arrangement of
//! pixels distinguishes 42 from 43 without OCR, which is why
//! `patches/kcalc-0004-selftest` has the app print what it is showing.
//!
//! The frame is written out as PPM (`--dump <dir>`) so a human can look; a
//! histogram proves "not blank", never "the right window".
//!
//! Usage: cargo run -- [--wasm PATH] [--dump DIR]

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wk_protocol::NodeId;
use wk_server::plugin::{Key, KeyEvent, NodeRegistry, PluginHost, SurfaceRegistry, VirtualSurface};

fn write_ppm(path: &Path, w: u32, h: u32, pixels: &[u8]) {
    let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
    ppm.extend(pixels.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]));
    std::fs::write(path, ppm).expect("write frame dump");
    eprintln!("frame dumped to {}", path.display());
}

fn main() {
    let mut wasm = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../kcalc.wasm");
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
        "kcalc",
        id,
        &[],
        surfaces.clone(),
        nodes.clone(),
        Vec::new(),
        Some(wk_server::images::ContainerSetup {
            layers: Vec::new(),
            // HOME and the XDG dirs: KConfig writes kcalcrc through
            // QStandardPaths, and KLocalizedString/KIconLoader read
            // XDG_DATA_DIRS. Qt's UNIX QStandardPaths backend is the active
            // one in this port (see plugins/qt/wasip2.cmake), so it reads
            // exactly these variables and falls back to paths under "/" that
            // a node's vfs may not have.
            env: vec![
                ("WK_KCALC_SELFTEST".into(), "1".into()),
                ("HOME".into(), "/root".into()),
                ("XDG_CONFIG_HOME".into(), "/root/.config".into()),
                ("XDG_DATA_HOME".into(), "/root/.local/share".into()),
                ("XDG_CACHE_HOME".into(), "/root/.cache".into()),
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
    // The node appears immediately; the guest only starts once the background
    // compile of the 25 MB component finishes, so seeding the vfs now is
    // comfortably ahead of main().
    {
        let mut fs = node.fs.lock().unwrap();
        fs.ensure_dir_path("root");
        fs.ensure_dir_path("root/.config");
        fs.ensure_dir_path("root/.local");
        fs.ensure_dir_path("root/.local/share");
        fs.ensure_dir_path("root/.cache");
    }

    let log_now = || String::from_utf8_lossy(&node.term_io.log_read(0).0).to_string();

    // --- claim 1: a wk surface opens ---------------------------------------
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
    {
        let s = surface.lock().unwrap();
        write_ppm(&dump.join("kcalc-main.ppm"), s.width, s.height, &s.pixels);
    }

    // STATE input='...' display='...' tops=N  (see patches/kcalc-0004-selftest)
    //
    // TWO displays, because they are different widgets answering different
    // questions: `input` is the KCalcInputDisplay QLineEdit holding the
    // expression being typed, `display` is the KCalcDisplay holding the
    // RESULT, which stays empty until `=`. A harness watching only `display`
    // sees nothing while keys arrive perfectly — which reads exactly like keys
    // not arriving, and did, on the first run of this file.
    let field = |log: &str, key: &str| -> Option<String> {
        let line = log.lines().rfind(|l| l.starts_with("STATE input='"))?;
        line.split_once(&format!("{key}='"))
            .and_then(|(_, rest)| rest.split_once('\''))
            .map(|(v, _)| v.to_string())
    };

    // --- claim 3: KCalculator's constructor ran all the way through --------
    let deadline = Instant::now() + Duration::from_secs(180);
    let tops = loop {
        pump_frame();
        std::thread::sleep(Duration::from_millis(15));
        let log = log_now();
        if let Some(line) = log.lines().rfind(|l| l.starts_with("STATE input='")) {
            // tops=1 is the assertion, not just "a STATE line appeared": the
            // KXmlGuiWindow has to be VISIBLE, which means setupGUI() parsed
            // kcalcui.rc out of qrc and KXMLGUIFactory built the menubar.
            if line.contains("tops=1") {
                break line.to_string();
            }
        }
        assert!(
            Instant::now() < deadline,
            "KCalc never showed a top-level window -- KXmlGuiWindow/setupGUI/\
             KConfig did not finish; log:\n{}",
            log_now()
        );
    };
    eprintln!("KXmlGuiWindow visible: {tops}");

    // One key, pressed and released, with BOTH halves: `key` says which
    // physical key moved, `text` says what it types. KCalc binds its keypad
    // with QShortcut/setShortcut, which matches on the resolved key — so the
    // text is what makes `/` arrive as Qt::Key_Slash rather than as a bare
    // scancode.
    let typed = |s: &mut VirtualSurface, key: Key, text: &str| {
        let ev = KeyEvent {
            key: Some(key),
            text: Some(text.to_owned()),
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

    // Wait until SOME narrated state matched `want`, pumping frames.
    //
    // Scanning the whole log rather than only the latest line, because KCalc
    // evaluates as you type and then MOVES the result: `8 ÷ 2` shows
    // display='4' the moment the 2 arrives, and pressing Enter commits it, so
    // the next state is input='4' display=''. A check that only ever looked at
    // the most recent line would see the committed state and conclude the
    // division never happened — which is exactly what the first version of
    // this file did.
    let seen = |want: &str, what: &str| {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            pump_frame();
            std::thread::sleep(Duration::from_millis(15));
            let log = log_now();
            if log.lines().any(|l| l.starts_with("STATE ") && l.contains(want)) {
                eprintln!("{what}: saw {want}");
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{what}: never saw {want}; last state was {:?}; log:\n{log}",
                log.lines().rfind(|l| l.starts_with("STATE "))
            );
        }
    };
    // ...and for the states that must be CURRENT rather than merely reached.
    let settle = |which: &str, want: &str, what: &str| {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            pump_frame();
            std::thread::sleep(Duration::from_millis(15));
            let log = log_now();
            let got = field(&log, which).unwrap_or_default();
            if got == want {
                eprintln!("{what}: {which} = '{got}'");
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{what}: {which} is '{got}', expected '{want}'; log:\n{log}"
            );
        }
    };

    // --- claim 4: a real key event reaches the app -------------------------
    // Into the INPUT display: KCalc binds its keypad with QShortcut/
    // setShortcut, so this exercises the QPA's key path and Qt's shortcut
    // matching, and the digit lands in the expression line.
    typed(&mut surface.lock().unwrap(), Key::Digit8, "8");
    settle("input", "8", "pressing 8");

    // --- claim 5: arithmetic, through KNumber and the bignums --------------
    // 8 / 2 = 4 -- KNumber's integer path, i.e. GMP. The expected input string
    // is `8÷2`, with U+00F7: KCalc echoes the DIVISION SIGN, not the slash
    // that was typed, so asserting on it also proves the token went through
    // KCalc's own tokenizer rather than landing in a QLineEdit verbatim.
    {
        let mut s = surface.lock().unwrap();
        typed(&mut s, Key::Slash, "/");
        typed(&mut s, Key::Digit2, "2");
    }
    seen("input='8\u{f7}2' display='4'", "8 / 2 evaluated (GMP integer path)");

    // Enter commits: the result becomes the new expression and the result
    // display clears. Asserting this is what proves the `=` binding fired —
    // pbEqual's shortcut is Qt::Key_Enter, with a second QShortcut on
    // Qt::Key_Return (kcalc.cpp:393-395).
    typed(&mut surface.lock().unwrap(), Key::Enter, "\r");
    settle("input", "4", "Enter committed the result");

    // Delete is KCalc's All Clear (kcalc.cpp:308), so the next expression
    // starts from an empty line instead of appending to the 4.
    typed(&mut surface.lock().unwrap(), Key::Delete, "");
    settle("input", "", "Delete cleared");

    // 1 / 8 = 0.125 -- cannot stay an integer, so this is the float path and
    // therefore MPFR. Two different bignum libraries, two different answers,
    // and the second one also proves the decimal separator survived a locale
    // with no system locale behind it.
    {
        let mut s = surface.lock().unwrap();
        typed(&mut s, Key::Digit1, "1");
        typed(&mut s, Key::Slash, "/");
        typed(&mut s, Key::Digit8, "8");
    }
    seen("input='1\u{f7}8' display='0.125'", "1 / 8 evaluated (MPFR float path)");

    // The display's text change asks for a repaint via QWidget::update(), which
    // the QPA coalesces and delivers on the NEXT host frame (see
    // qwkcompositor.h). `seen()` returns as soon as the app NARRATES the new
    // value, which is strictly earlier than the repaint that shows it. Pumping
    // here is defensive: it closes that window so the dump and the assertion
    // below cannot race the compositor.
    for _ in 0..10 {
        pump_frame();
        std::thread::sleep(Duration::from_millis(15));
    }
    {
        let s = surface.lock().unwrap();
        write_ppm(&dump.join("kcalc-result.ppm"), s.width, s.height, &s.pixels);
    }
    // --- claim 6: the RESULT IS ON SCREEN ---------------------------------
    // Claim 5 asserts what the app SAYS it is showing. That is the strong
    // assertion, but it is blind to a display that renders nothing -- and that
    // is not hypothetical. The first build of this port narrated `0.125` while
    // both display rectangles dumped pure white, and nothing in the suite
    // noticed, because every assertion read the narration. The cause was a
    // STALE ARTIFACT: kcalc.wasm had been linked against an older libqwk.a,
    // and rebuilding the QPA and relinking fixed it with no source change --
    // the same trap that left plugins/qt-slate… unrebuilt and made
    // torrent-file-editor fail to relink. Nothing in the tree prevents that
    // recurring, so assert on PIXELS as well as on narration.
    {
        let s = surface.lock().unwrap();
        let dark = |y0: u32, y1: u32| -> u32 {
            let mut n = 0;
            for y in y0..y1.min(s.height) {
                for x in 0..s.width {
                    let o = ((y * s.width + x) * 4) as usize;
                    if s.pixels[o] < 128 && s.pixels[o + 1] < 128 && s.pixels[o + 2] < 128 {
                        n += 1;
                    }
                }
            }
            n
        };
        // The expression line and the result display, in that order down the
        // window. Glyphs are near-black on white here, so a blank rectangle is
        // exactly zero and any rendered text is hundreds of pixels.
        let expr = dark(30, 80);
        let result = dark(85, 200);
        eprintln!("display pixels: expression={expr} dark, result={result} dark");
        assert!(
            expr > 20 && result > 100,
            "KCalc narrated the right answer but did not PAINT it: \
             expression={expr} dark px, result={result} dark px (blank is 0). \
             Either the repaint never reached the surface or the display is \
             drawing in the background colour."
        );
    }

    eprintln!("\nKDE Frameworks 6 runs in wk: KCalc opened a KXmlGuiWindow, took real");
    eprintln!("key events through Qt's shortcut machinery, and evaluated two");
    eprintln!("expressions through KNumber/GMP/MPFR inside a wasm32-wasip2 component.");
}
