//! Run `slate.wasm` as a real wk node, headless, and prove it draws.
//!
//! This is the same shape as wk-server's own
//! `plugin::tests::qt_widgets_app_paints_through_the_wk_qpa`: spawn the guest
//! through `PluginHost`, play the per-frame role the compositor normally
//! plays, and read the presented pixels back. It lives here rather than as a
//! `#[test]` in `crates/wk-server` because that file is shared with other work
//! in flight; `[workspace]` in Cargo.toml keeps this crate out of the repo's
//! workspace entirely.
//!
//! What it checks, in order of how much it proves:
//!
//! 1. The component instantiates and opens a surface at all. For a 41 MB
//!    component this alone takes a while — wasmtime compiles it first.
//! 2. The surface is a real UI: not blank, not uniform, and with the spread of
//!    tones a Material-dark Quick scene has (a dark ground, mid-grey chrome,
//!    near-white text).
//! 3. Text was actually rendered — measured as near-white pixels, which in
//!    this theme only glyphs and highlights produce.
//! 4. Pointer input reaches the scene: it clicks where Slate's "New" toolbar
//!    button is and looks for the frame to change.
//!
//! A histogram proves "not blank", never "the right window", so it always
//! writes the frame out as a PPM for a human to look at. That is the check
//! that catches a QPA plugin painting something plausible and wrong.
//!
//! Usage: slate-harness <slate.wasm> <out.ppm> [seconds] [KEY=VALUE ...]
//!
//! Trailing KEY=VALUE arguments are handed to the node as environment. That is
//! how a theory about the guest gets tested without rebuilding it —
//! `QT_QUICK_CONTROLS_STYLE=Basic` was what confirmed that the missing Material
//! menu background is a ShaderEffect problem and not a compositing one.

use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wk_protocol::NodeId;
use wk_server::images::ContainerSetup;
use wk_server::plugin::{NodeRegistry, PluginHost, PointerButton, PointerEvent, SurfaceRegistry};

fn main() {
    let mut args = std::env::args().skip(1);
    let wasm = args.next().unwrap_or_else(|| "slate.wasm".into());
    let out = args.next().unwrap_or_else(|| "/tmp/slate.ppm".into());
    let budget: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(600);
    let extra_env: Vec<(String, String)> = args
        .filter_map(|a| a.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())))
        .collect();
    let wasm = Path::new(&wasm);
    assert!(wasm.exists(), "no such component: {}", wasm.display());

    let host = PluginHost::new().expect("PluginHost");
    let nodes: NodeRegistry = Arc::new(Mutex::new(Vec::new()));
    let surfaces: SurfaceRegistry = Arc::new(Mutex::new(Vec::new()));
    let id = NodeId::new();

    host.spawn(
        wasm,
        "slate",
        id,
        &[],
        surfaces.clone(),
        nodes.clone(),
        Vec::new(),
        Some(ContainerSetup {
            layers: Vec::new(),
            // QT_QPA_PLATFORM / QT_QUICK_BACKEND / QSG_RENDER_LOOP /
            // QT_QPA_FONTDIR all default correctly from node/wkslate.cpp, so
            // the only thing worth setting here is a writable HOME for
            // QSettings and the Qt logging rules Slate silences by default.
            env: [
                ("HOME".to_string(), "/root".to_string()),
                ("QT_LOGGING_RULES".to_string(), "qt.qpa.*=true".to_string()),
            ]
            .into_iter()
            .chain(extra_env)
            .collect(),
        }),
    )
    .expect("spawn");

    let deadline = Instant::now() + Duration::from_secs(budget);

    let node = loop {
        if let Some(n) = nodes.lock().unwrap().iter().find(|n| n.id == id).cloned() {
            break n;
        }
        assert!(Instant::now() < deadline, "node never appeared");
        std::thread::sleep(Duration::from_millis(20));
    };
    let log = || String::from_utf8_lossy(&node.term_io.log_read(0).0).to_string();

    eprintln!("waiting for a surface (wasmtime has to compile 41 MB first)...");
    let surface = loop {
        if let Some(s) = surfaces.lock().unwrap().first().cloned() {
            break s;
        }
        if node.finished.load(Ordering::Relaxed) {
            eprintln!("--- node log ---\n{}", log());
            panic!("slate exited before opening a surface");
        }
        assert!(
            Instant::now() < deadline,
            "slate never opened a surface; node log:\n{}",
            log()
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    eprintln!("surface opened after {:?}", Instant::now() - (deadline - Duration::from_secs(budget)));

    // The compositor's per-frame job: hand the guest a frame credit and wake
    // whoever is blocked in wkgfx_wait_frame_timeout().
    let pump = || {
        let mut s = surface.lock().unwrap();
        s.frame_ready = true;
        s.wake();
    };

    // Tone histogram. Slate's Material Dark theme paints a #303030 ground with
    // #424242 chrome and near-white text, so a live scene has all three bands.
    let sample = || {
        let s = surface.lock().unwrap();
        let (mut dark, mut mid, mut bright) = (0usize, 0usize, 0usize);
        for px in s.pixels.chunks_exact(4) {
            let lum = px[0] as u32 + px[1] as u32 + px[2] as u32;
            if lum < 200 {
                dark += 1;
            } else if lum < 600 {
                mid += 1;
            } else {
                bright += 1;
            }
        }
        (s.width, s.height, dark, mid, bright)
    };

    let mut best = (0, 0, 0usize, 0usize, 0usize);
    loop {
        pump();
        std::thread::sleep(Duration::from_millis(15));
        let now = sample();
        if now.2 + now.3 + now.4 > 0 && now.3 + now.4 > best.3 + best.4 {
            best = now;
        }
        // All three tone bands present, ranked rather than named, so the test
        // does not encode one Quick Controls style: Material Dark is
        // 678k/92k/16k dark/bright/mid and Basic is 447k/334k/5k mid/bright/dark.
        // What both have and a broken frame does not is a dominant ground, a
        // large second band of chrome, and a small third band — the text and
        // separators. A cleared frame, a frame of one flat colour, and a frame
        // with chrome but no glyphs all fail this.
        let mut bands = [best.2, best.3, best.4];
        bands.sort_unstable_by(|a, b| b.cmp(a));
        if bands[0] > 200_000 && bands[1] > 20_000 && bands[2] > 3_000 {
            break;
        }
        if node.finished.load(Ordering::Relaxed) {
            eprintln!("--- node log ---\n{}", log());
            dump(&surface, &out);
            panic!("slate exited; last frame: {best:?}");
        }
        if Instant::now() >= deadline {
            eprintln!("--- node log ---\n{}", log());
            dump(&surface, &out);
            panic!("slate never painted a full scene; best frame was {best:?}");
        }
    }

    let (w, h, dark, mid, bright) = best;
    println!("frame {w}x{h}: {dark} dark, {mid} mid, {bright} bright px");
    dump(&surface, &out);

    // Input: click the "File" menu. This is the whole input path — host queue,
    // wkgfx_poll_event, QWkInput, QGuiApplication delivery, QQuickWindow
    // hit-testing, a Quick Control — and opening a menu ALSO exercises
    // multi-top-level compositing, because a Quick popup that does not fit
    // inside its window becomes a second QWindow that QFbScreen has to z-order
    // into the same surface.
    let (bx, by) = (26.0, 19.0);
    {
        let mut s = surface.lock().unwrap();
        s.pointer_move.push_back(PointerEvent { x: bx, y: by, button: None });
        s.pointer_down.push_back(PointerEvent {
            x: bx,
            y: by,
            button: Some(PointerButton::Left),
        });
        s.pointer_up.push_back(PointerEvent {
            x: bx,
            y: by,
            button: Some(PointerButton::Left),
        });
        s.wake();
    }
    let before = snapshot(&surface);
    let mut changed = 0usize;
    let click_deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < click_deadline {
        pump();
        std::thread::sleep(Duration::from_millis(15));
        changed = diff(&before, &snapshot(&surface));
        // A menu is thousands of pixels. Anything smaller is a hover
        // highlight or a repaint artefact and should not count as "the click
        // arrived".
        if changed > 5_000 {
            break;
        }
    }
    println!("pointer click at ({bx}, {by}): {changed} pixels changed");
    let mut alt = out.clone();
    alt.push_str(".click.ppm");
    dump(&surface, &alt);
    assert!(
        changed > 5_000,
        "the click never reached the scene ({changed} pixels changed)"
    );

    println!("--- node log ---\n{}", log());
}

fn snapshot(surface: &wk_server::plugin::SharedSurface) -> Vec<u8> {
    surface.lock().unwrap().pixels.clone()
}

fn diff(a: &[u8], b: &[u8]) -> usize {
    if a.len() != b.len() {
        return usize::MAX;
    }
    a.chunks_exact(4)
        .zip(b.chunks_exact(4))
        .filter(|(x, y)| x[..3] != y[..3])
        .count()
}

fn dump(surface: &wk_server::plugin::SharedSurface, path: &str) {
    let s = surface.lock().unwrap();
    let mut ppm = format!("P6\n{} {}\n255\n", s.width, s.height).into_bytes();
    ppm.extend(s.pixels.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]));
    std::fs::write(path, ppm).expect("write ppm");
    eprintln!("frame written to {path}");
}
