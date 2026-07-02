//! The wk compositor: the GUI window client. It composites the surfaces its
//! nodes paint into draggable windows on an infinite canvas and routes input
//! back to the focused node. The whole UI (windows, menu, text) is drawn by
//! hand as 2D quads via `render2d`; windowing/input is winit. The authoritative
//! document lives in the server, reached through a `ServerHandle`.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use winit::platform::pump_events::{EventLoopExtPumpEvents, PumpStatus};
use winit::window::{Window, WindowId};

use crate::host_shell::Gfx;
use crate::render2d::{Quad, Renderer, TextureId};
use crate::text::Fonts;
use wk_protocol::{Command, NodeId, NodeKind, NodePatch, Resource, ResourceRef, Wire};
use wk_server::plugin::{Key, KeyEvent, PointerEvent, ResizeEvent, SharedNode, SharedSurface};
use wk_server::runtime::ServerHandle;
use wk_server::server::{View, FILE_H, FILE_W};
use wk_server::terminal::CellView;

const FRAME: Duration = Duration::from_nanos(1_000_000_000 / 60);
const SCROLL_PAN_SPEED: f32 = 30.0;
/// Fraction of the remaining pan distance covered each frame.
const PAN_SMOOTH: f32 = 0.3;
const ZOOM_STEP: f32 = 1.1;

/// Window title-bar height and border thickness, in canvas pixels.
const TITLE_H: f32 = 22.0;
const BORDER: f32 = 1.0;
/// Top menu bar height, in screen pixels (not zoomed).
const MENU_H: f32 = 26.0;
/// Height of the top workspace-tab bar (shown only with more than one tab).
const TAB_H: f32 = 26.0;
const PAD: f32 = 6.0;

const CLEAR: wgpu::Color = wgpu::Color {
    r: 0.05,
    g: 0.05,
    b: 0.08,
    a: 1.0,
};
const MENU_BG: [f32; 4] = [0.13, 0.13, 0.16, 1.0];
const MENU_HOVER: [f32; 4] = [0.26, 0.28, 0.34, 1.0];
const TITLE: [f32; 4] = [0.18, 0.19, 0.24, 1.0];
const TITLE_FOCUS: [f32; 4] = [0.24, 0.34, 0.52, 1.0];
const BODY: [f32; 4] = [0.10, 0.10, 0.13, 1.0];
const BORDER_COL: [f32; 4] = [0.32, 0.33, 0.38, 1.0];
const TEXT: [f32; 4] = [0.90, 0.90, 0.93, 1.0];
const CLOSE_HOT: [f32; 4] = [0.80, 0.30, 0.30, 1.0];
/// Warning tint (e.g. a HostPort whose localhost port collides with another).
const WARN: [f32; 4] = [0.92, 0.45, 0.40, 1.0];
const TERM_BG: [f32; 4] = [0.063, 0.063, 0.086, 1.0];
/// Body fill in the workspace for a node popped out into its own window (behind
/// the "detached" label).
const DETACHED_BG: [f32; 4] = [0.10, 0.11, 0.14, 1.0];

fn rgba(c: [u8; 3]) -> [f32; 4] {
    [
        c[0] as f32 / 255.0,
        c[1] as f32 / 255.0,
        c[2] as f32 / 255.0,
        1.0,
    ]
}

/// Encode a key press as the bytes a terminal app expects on stdin. `text` is
/// winit's resolved character(s) for the key (handles shift/layout).
fn encode_term_key(code: KeyCode, text: Option<&str>, mods: ModifiersState) -> Option<Vec<u8>> {
    use KeyCode as C;
    // Ctrl+letter -> control byte (Ctrl-A = 0x01 ... Ctrl-Z = 0x1a).
    if mods.control_key() {
        if let Some(n) = letter_index(code) {
            return Some(vec![n + 1]);
        }
    }
    Some(match code {
        C::Enter | C::NumpadEnter => vec![b'\r'],
        C::Backspace => vec![0x7f],
        C::Tab => vec![b'\t'],
        C::Escape => vec![0x1b],
        C::ArrowUp => vec![0x1b, b'[', b'A'],
        C::ArrowDown => vec![0x1b, b'[', b'B'],
        C::ArrowRight => vec![0x1b, b'[', b'C'],
        C::ArrowLeft => vec![0x1b, b'[', b'D'],
        C::Home => vec![0x1b, b'[', b'H'],
        C::End => vec![0x1b, b'[', b'F'],
        _ => match text {
            Some(t) if !t.is_empty() => t.as_bytes().to_vec(),
            _ => return None,
        },
    })
}

fn letter_index(code: KeyCode) -> Option<u8> {
    use KeyCode as C;
    let n = match code {
        C::KeyA => 0,
        C::KeyB => 1,
        C::KeyC => 2,
        C::KeyD => 3,
        C::KeyE => 4,
        C::KeyF => 5,
        C::KeyG => 6,
        C::KeyH => 7,
        C::KeyI => 8,
        C::KeyJ => 9,
        C::KeyK => 10,
        C::KeyL => 11,
        C::KeyM => 12,
        C::KeyN => 13,
        C::KeyO => 14,
        C::KeyP => 15,
        C::KeyQ => 16,
        C::KeyR => 17,
        C::KeyS => 18,
        C::KeyT => 19,
        C::KeyU => 20,
        C::KeyV => 21,
        C::KeyW => 22,
        C::KeyX => 23,
        C::KeyY => 24,
        C::KeyZ => 25,
        _ => return None,
    };
    Some(n)
}

/// Map a winit physical key to the wasi-gfx W3C `key` code.
fn map_key(code: KeyCode) -> Option<Key> {
    use KeyCode as C;
    Some(match code {
        C::KeyA => Key::KeyA,
        C::KeyB => Key::KeyB,
        C::KeyC => Key::KeyC,
        C::KeyD => Key::KeyD,
        C::KeyE => Key::KeyE,
        C::KeyF => Key::KeyF,
        C::KeyG => Key::KeyG,
        C::KeyH => Key::KeyH,
        C::KeyI => Key::KeyI,
        C::KeyJ => Key::KeyJ,
        C::KeyK => Key::KeyK,
        C::KeyL => Key::KeyL,
        C::KeyM => Key::KeyM,
        C::KeyN => Key::KeyN,
        C::KeyO => Key::KeyO,
        C::KeyP => Key::KeyP,
        C::KeyQ => Key::KeyQ,
        C::KeyR => Key::KeyR,
        C::KeyS => Key::KeyS,
        C::KeyT => Key::KeyT,
        C::KeyU => Key::KeyU,
        C::KeyV => Key::KeyV,
        C::KeyW => Key::KeyW,
        C::KeyX => Key::KeyX,
        C::KeyY => Key::KeyY,
        C::KeyZ => Key::KeyZ,
        C::Digit0 => Key::Digit0,
        C::Digit1 => Key::Digit1,
        C::Digit2 => Key::Digit2,
        C::Digit3 => Key::Digit3,
        C::Digit4 => Key::Digit4,
        C::Digit5 => Key::Digit5,
        C::Digit6 => Key::Digit6,
        C::Digit7 => Key::Digit7,
        C::Digit8 => Key::Digit8,
        C::Digit9 => Key::Digit9,
        C::ArrowUp => Key::ArrowUp,
        C::ArrowDown => Key::ArrowDown,
        C::ArrowLeft => Key::ArrowLeft,
        C::ArrowRight => Key::ArrowRight,
        C::Space => Key::Space,
        C::Enter => Key::Enter,
        C::Tab => Key::Tab,
        C::Escape => Key::Escape,
        C::Backspace => Key::Backspace,
        C::ShiftLeft => Key::ShiftLeft,
        C::ShiftRight => Key::ShiftRight,
        C::ControlLeft => Key::ControlLeft,
        C::ControlRight => Key::ControlRight,
        C::AltLeft => Key::AltLeft,
        C::AltRight => Key::AltRight,
        C::SuperLeft => Key::MetaLeft,
        C::SuperRight => Key::MetaRight,
        _ => return None,
    })
}

fn key_event(code: KeyCode, mods: ModifiersState) -> KeyEvent {
    KeyEvent {
        key: map_key(code),
        text: None,
        alt_key: mods.alt_key(),
        ctrl_key: mods.control_key(),
        meta_key: mods.super_key(),
        shift_key: mods.shift_key(),
    }
}

/// The infinite-canvas camera: windows live in canvas space and map to screen
/// space by panning (scroll) and zooming (Cmd/Ctrl + scroll).
#[derive(Clone, Copy)]
struct Camera {
    pan: [f32; 2],
    zoom: f32,
}

impl Camera {
    fn to_screen(self, p: [f32; 2]) -> [f32; 2] {
        [
            self.pan[0] + p[0] * self.zoom,
            self.pan[1] + p[1] * self.zoom,
        ]
    }
    fn to_canvas(self, p: [f32; 2]) -> [f32; 2] {
        [
            (p[0] - self.pan[0]) / self.zoom,
            (p[1] - self.pan[1]) / self.zoom,
        ]
    }
    fn zoom_at(&mut self, factor: f32, focus: [f32; 2]) {
        let anchor = self.to_canvas(focus);
        self.zoom = (self.zoom * factor).clamp(ZOOM_MIN, ZOOM_MAX);
        self.pan = [
            focus[0] - anchor[0] * self.zoom,
            focus[1] - anchor[1] * self.zoom,
        ];
    }
}

/// Zoom limits and the fixed presets offered by the corner zoom menu.
const ZOOM_MIN: f32 = 0.2;
const ZOOM_MAX: f32 = 2.0;
const ZOOM_PRESETS: [f32; 4] = [2.0, 1.5, 1.0, 0.5];

fn ease(current: f32, target: f32) -> f32 {
    let d = target - current;
    if d.abs() < 0.5 {
        target
    } else {
        current + d * PAN_SMOOTH
    }
}

fn contains(r: [f32; 4], p: [f32; 2]) -> bool {
    p[0] >= r[0] && p[0] < r[2] && p[1] >= r[1] && p[1] < r[3]
}

fn intersect(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    [
        a[0].max(b[0]),
        a[1].max(b[1]),
        a[2].min(b[2]),
        a[3].min(b[3]),
    ]
}

fn win_rect(cam: Camera, pos: [f32; 2], size: [f32; 2]) -> [f32; 4] {
    let s = cam.to_screen(pos);
    [
        s[0],
        s[1],
        s[0] + size[0] * cam.zoom,
        s[1] + size[1] * cam.zoom,
    ]
}

fn title_bar(r: [f32; 4], z: f32) -> [f32; 4] {
    [r[0], r[1], r[2], r[1] + TITLE_H * z]
}
/// The close box at the right of a workspace tab rect.
fn tab_close_btn(r: [f32; 4]) -> [f32; 4] {
    let s = (TAB_H - 12.0).max(8.0);
    let x1 = r[2] - 5.0;
    let y0 = (TAB_H - s) * 0.5;
    [x1 - s, y0, x1, y0 + s]
}
fn close_btn(r: [f32; 4], z: f32) -> [f32; 4] {
    let s = (TITLE_H - 8.0) * z;
    let x1 = r[2] - 4.0 * z;
    let y0 = r[1] + 4.0 * z;
    [x1 - s, y0, x1, y0 + s]
}
/// The detach button, just left of the close button. Pops the node out into its
/// own OS window (and, when already detached, reattaches it). Shown on app nodes.
fn detach_btn(r: [f32; 4], z: f32) -> [f32; 4] {
    let cb = close_btn(r, z);
    let w = cb[2] - cb[0];
    let gap = 4.0 * z;
    [cb[0] - w - gap, cb[1], cb[0] - gap, cb[3]]
}
/// The Run/▶ button, just left of the detach button. Shown only on an idle or
/// exited node so it can be (re)started after wiring.
fn run_btn(r: [f32; 4], z: f32) -> [f32; 4] {
    let db = detach_btn(r, z);
    let w = db[2] - db[0];
    let gap = 4.0 * z;
    [db[0] - w - gap, db[1], db[0] - gap, db[3]]
}
/// The editable launch-args bar along the bottom of an idle node's body (a
/// one-line input strip, so it doesn't paint over the node's output above).
fn args_bar(r: [f32; 4], z: f32) -> [f32; 4] {
    let ca = content_rect(r, z);
    let h = (TITLE_H * z).min((ca[3] - ca[1]).max(0.0));
    [ca[0], ca[3] - h, ca[2], ca[3]]
}
fn resize_grip(r: [f32; 4], z: f32) -> [f32; 4] {
    let g = 16.0 * z;
    [r[2] - g, r[3] - g, r[2], r[3]]
}
/// The "−" and "+" port-step buttons on a HostPort node (bottom-right).
fn port_step_btns(r: [f32; 4], z: f32) -> ([f32; 4], [f32; 4]) {
    let s = 14.0 * z;
    let gap = 3.0 * z;
    let y0 = r[3] - s - 4.0 * z;
    let px = r[2] - 4.0 * z;
    let plus = [px - s, y0, px, y0 + s];
    let minus = [px - 2.0 * s - gap, y0, px - s - gap, y0 + s];
    (minus, plus)
}
fn content_rect(r: [f32; 4], z: f32) -> [f32; 4] {
    [
        r[0] + BORDER * z,
        r[1] + TITLE_H * z,
        r[2] - BORDER * z,
        r[3] - BORDER * z,
    ]
}

/// Caches rasterized strings as textures (white glyphs, tinted at draw time).
#[derive(Default)]
struct TextCache {
    map: HashMap<String, (TextureId, f32, f32)>,
}

impl TextCache {
    #[allow(clippy::too_many_arguments)]
    fn draw(
        &mut self,
        quads: &mut Vec<Quad>,
        r: &mut Renderer,
        fonts: &Fonts,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        s: &str,
        x: f32,
        y: f32,
        scale: f32,
        color: [f32; 4],
        clip: [f32; 4],
    ) {
        let (tex, w, h) = match self.map.get(s) {
            Some(e) => *e,
            None => {
                let Some(g) = fonts.rasterize(s) else {
                    return;
                };
                if self.map.len() >= 1024 {
                    for (_, (tex, _, _)) in self.map.drain() {
                        r.remove_texture(tex);
                    }
                }
                let tex = r.create_texture(device, queue, g.width, g.height, &g.rgba);
                let e = (tex, g.width as f32, g.height as f32);
                self.map.insert(s.to_string(), e);
                e
            }
        };
        quads.push(Quad::tex(
            [x, y, x + w * scale, y + h * scale],
            [0.0, 0.0, 1.0, 1.0],
            color,
            tex,
            clip,
        ));
    }
}

enum DragMode {
    Move,
    Resize,
    /// Dragging a connection wire out of a node's port toward another node.
    Connect,
}
struct Drag {
    id: NodeId,
    mode: DragMode,
    grab: [f32; 2],
}

/// An action runnable from the Cmd/Ctrl+K command palette.
#[derive(Clone, Copy)]
enum PaletteCmd {
    /// Launch the dependency at this index in `available`.
    Launch(usize),
    /// Centre the camera on this node.
    GoTo(NodeId),
    AddVirtualFile,
    AddHostFile,
    AddPort,
    AddNetwork,
    AddGateway,
    NewWorkspace,
    CloseWorkspace,
    /// Jump the camera to this zoom factor.
    Zoom(f32),
    Quit,
}

/// Most filtered command-palette rows shown at once.
const PALETTE_MAX: usize = 9;

/// Connection port radius, in canvas pixels.
const PORT_R: f32 = 6.0;
const FILE_BG: [f32; 4] = [0.20, 0.17, 0.10, 1.0];
const FILE_BORDER: [f32; 4] = [0.55, 0.45, 0.25, 1.0];
/// HostMappedFile nodes are tinted (blue/grey) to distinguish disk-backed files
/// from in-memory VirtualFiles.
const HOSTFILE_BG: [f32; 4] = [0.10, 0.14, 0.22, 1.0];
const HOSTFILE_BORDER: [f32; 4] = [0.30, 0.45, 0.65, 1.0];
const PORT_COL: [f32; 4] = [0.70, 0.72, 0.80, 1.0];
/// Input-port (left, target) dot — dimmer than the output port you drag from.
const PORT_IN_COL: [f32; 4] = [0.42, 0.44, 0.52, 1.0];
/// A port lights up when the cursor is over it (hover / valid drop target).
const PORT_HOT: [f32; 4] = [0.55, 0.80, 1.0, 1.0];
/// HostPort node colours and wire (exposes a wasi:http node to localhost).
const HOSTPORT_BG: [f32; 4] = [0.10, 0.18, 0.20, 1.0];
const HOSTPORT_BORDER: [f32; 4] = [0.30, 0.62, 0.66, 1.0];
const HOSTPORT_WIRE: [f32; 4] = [0.40, 0.78, 0.82, 1.0];
const WIRE_COL: [f32; 4] = [0.55, 0.60, 0.72, 1.0];
/// MIDI connection wires get a distinct (teal/green) colour.
const MIDI_WIRE_COL: [f32; 4] = [0.35, 0.78, 0.62, 1.0];
/// Network node colours and membership wire (a virtual network / Docker bridge).
const NET_BG: [f32; 4] = [0.14, 0.12, 0.20, 1.0];
const NET_BORDER: [f32; 4] = [0.50, 0.40, 0.72, 1.0];
const NET_WIRE_COL: [f32; 4] = [0.62, 0.50, 0.86, 1.0];
/// A selected wire is drawn thicker in this highlight colour.
const WIRE_SEL_COL: [f32; 4] = [1.0, 0.85, 0.4, 1.0];

/// The **output** port (a node as a source), on the right edge, vertically
/// centred — drag a wire out of here.
fn port_out(r: [f32; 4]) -> [f32; 2] {
    [r[2], (r[1] + r[3]) * 0.5]
}
/// The **input** port (a node as a target), on the left edge — drop a wire here.
fn port_in(r: [f32; 4]) -> [f32; 2] {
    [r[0], (r[1] + r[3]) * 0.5]
}
/// Draw a node's input (left) and output (right) connection ports as dots. The
/// output is brighter (you drag from it); the input is dimmer (you drop onto it).
/// The port under the cursor `mp` lights up and grows a bit (hover feedback).
fn draw_ports(
    quads: &mut Vec<Quad>,
    circle: TextureId,
    r: [f32; 4],
    zf: f32,
    mp: [f32; 2],
    clip: [f32; 4],
) {
    let pr = PORT_R * zf;
    for (center, base) in [(port_in(r), PORT_IN_COL), (port_out(r), PORT_COL)] {
        let (col, rad) = if near(mp, center, pr + 3.0) {
            (PORT_HOT, pr * 1.4)
        } else {
            (base, pr)
        };
        quads.push(Quad::disc(circle, center, rad, col, clip));
    }
}
fn near(a: [f32; 2], b: [f32; 2], radius: f32) -> bool {
    let (dx, dy) = (a[0] - b[0], a[1] - b[1]);
    dx * dx + dy * dy <= radius * radius
}

fn dist_to_segment(p: [f32; 2], a: [f32; 2], b: [f32; 2]) -> f32 {
    let (abx, aby) = (b[0] - a[0], b[1] - a[1]);
    let (apx, apy) = (p[0] - a[0], p[1] - a[1]);
    let len2 = abx * abx + aby * aby;
    let t = if len2 > 0.0 {
        ((apx * abx + apy * aby) / len2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let (cx, cy) = (a[0] + abx * t, a[1] + aby * t);
    let (dx, dy) = (p[0] - cx, p[1] - cy);
    (dx * dx + dy * dy).sqrt()
}

/// How close (screen px) a click must be to a wire to select it.
const WIRE_PICK: f32 = 6.0;

/// The curved arrow (perfect-arrows) for a connection from output port `a` to
/// input port `b`. Shared by drawing and hit-testing so they agree.
fn connection_arrow(a: [f32; 2], b: [f32; 2], zf: f32) -> crate::arrows::Arrow {
    let opts = crate::arrows::ArrowOptions {
        // End the curve a touch before the input port so the arrowhead sits there.
        pad_end: (6.0 * zf).max(4.0),
        ..Default::default()
    };
    crate::arrows::get_arrow(a[0], a[1], b[0], b[1], &opts)
}

/// Draw a connection as a curved arrow with a head at the target end, so a wire
/// looks smooth and shows its direction (source output -> target input).
#[allow(clippy::too_many_arguments)]
fn draw_connection(
    quads: &mut Vec<Quad>,
    white: TextureId,
    a: [f32; 2],
    b: [f32; 2],
    sel: bool,
    color: [f32; 4],
    zf: f32,
    clip: [f32; 4],
) {
    let th = if sel {
        (3.5 * zf).max(2.5)
    } else {
        (2.0 * zf).max(1.5)
    };
    let arrow = connection_arrow(a, b, zf);
    // The curved shaft, tessellated into short segments.
    let pts = crate::arrows::polyline(&arrow, 24);
    for s in pts.windows(2) {
        quads.push(Quad::line(white, s[0], s[1], th, color, clip));
    }
    // Arrowhead at the end, pointing along the arrival angle.
    let size = (7.0 * zf).max(5.0);
    let end = [arrow.end.0, arrow.end.1];
    let ang = arrow.end_angle;
    let spread = 0.5;
    for wing in [
        ang + std::f32::consts::PI - spread,
        ang + std::f32::consts::PI + spread,
    ] {
        let p = [end[0] + wing.cos() * size, end[1] + wing.sin() * size];
        quads.push(Quad::line(white, end, p, th.max(1.5), color, clip));
    }
}

/// A node popped out into its own OS window. Purely client-local view state:
/// neither the detached flag nor this window's size is ever sent to the server
/// or written to the workspace, so a restart brings every node back into the
/// main window. A node is "detached" iff it has an entry in [`App::detached`].
struct Detached {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    /// The detached window's logical inner size — the node's render target while
    /// detached (replacing its in-workspace content size). Never persisted.
    size: [u32; 2],
    // Per-window input, accumulated from winit events and forwarded to the node
    // each frame (mirrors the main window's input handling).
    mouse: [f32; 2],
    lmb: bool,
    prev_lmb: bool,
    key_events: Vec<(KeyEvent, bool)>,
    term_input: Vec<u8>,
}

struct App {
    /// This client's connection to the independently-running server: send
    /// [`Command`]s, read [`View`] snapshots.
    conn: ServerHandle,
    /// The latest snapshot, filtered to the active tab, refreshed each `frame`.
    view: View,
    /// The workspace (tab) this client is currently viewing. Purely client-side:
    /// all workspaces run on the server; switching tabs never touches it.
    active_ws: NodeId,
    /// All workspace ids (tabs), in order — for the tab bar.
    tabs: Vec<NodeId>,
    /// Localhost ports claimed by more than one HostPort across all workspaces
    /// (they can't all bind); flagged in the UI. Computed from the full view.
    port_conflicts: HashSet<u16>,
    gfx: Option<Gfx>,
    /// Nodes currently popped out into their own OS window, keyed by node id.
    detached: HashMap<NodeId, Detached>,
    /// Detach requests awaiting window creation (needs the `ActiveEventLoop`,
    /// which `frame` doesn't have; drained in `about_to_wait`).
    pending_detach: Vec<NodeId>,

    views: HashMap<u64, (TextureId, u32, u32)>,
    text_cache: TextCache,
    /// VT terminal per non-graphical node, fed from its stdout.
    terminals: HashMap<NodeId, wk_server::terminal::Terminal>,

    cam: Camera,
    pan_target: [f32; 2],
    /// Last known viewport size in screen px (updated each frame), so newly
    /// added nodes can be placed at the centre of the current view.
    viewport: [f32; 2],
    /// This client's stacking order (which node draws/hit-tests on top).
    z: Vec<NodeId>,
    kbd_focus: Option<NodeId>,
    /// When editing an idle node's args: its id and the in-progress text.
    editing_args: Option<(NodeId, String)>,
    drag: Option<Drag>,
    /// The connection wire currently selected (click to select, Delete to remove).
    wire_sel: Option<Wire>,
    /// Set when Delete/Backspace is pressed; consumed in `frame` to drop the
    /// selected wire.
    del_wire: bool,
    zoom_menu_open: bool,
    /// Command palette (Cmd/Ctrl+K) state: open, the typed filter, and the
    /// highlighted row. `palette_run` is set when a command is chosen and
    /// executed in `frame`; `request_exit` quits wk on the next loop.
    palette_open: bool,
    palette_query: String,
    palette_sel: usize,
    /// First visible row (fractional, so trackpad pixel-scroll accumulates).
    palette_scroll: f32,
    palette_run: Option<PaletteCmd>,
    request_exit: bool,

    // Input state, fed by winit events between frames.
    mouse: [f32; 2],
    lmb: bool,
    prev_lmb: bool,
    mods: ModifiersState,
    pan_delta: [f32; 2],
    /// Accumulated zoom multiplier this frame (1.0 = none); fed by Cmd/Ctrl +
    /// scroll and by trackpad pinch.
    zoom_factor: f32,
    zoom_focus: [f32; 2],
    key_events: Vec<(KeyEvent, bool)>,
    /// Keyboard encoded as terminal input bytes for the focused terminal node.
    term_input: Vec<u8>,
}

impl App {
    fn new(conn: ServerHandle) -> Result<Self, String> {
        let full = conn.view();
        let active_ws = full.workspaces.first().copied().unwrap_or_else(NodeId::new);
        let tabs = full.workspaces.clone();
        let view = full.for_workspace(active_ws);
        Ok(App {
            conn,
            view,
            active_ws,
            tabs,
            port_conflicts: HashSet::new(),
            gfx: None,
            detached: HashMap::new(),
            pending_detach: Vec::new(),
            views: HashMap::new(),
            text_cache: TextCache::default(),
            terminals: HashMap::new(),
            cam: Camera {
                pan: [0.0, 0.0],
                zoom: 1.0,
            },
            pan_target: [0.0, 0.0],
            viewport: [1280.0, 800.0],
            z: Vec::new(),
            kbd_focus: None,
            editing_args: None,
            drag: None,
            wire_sel: None,
            del_wire: false,
            zoom_menu_open: false,
            palette_open: false,
            palette_query: String::new(),
            palette_sel: 0,
            palette_scroll: 0.0,
            palette_run: None,
            request_exit: false,
            mouse: [0.0, 0.0],
            lmb: false,
            prev_lmb: false,
            mods: ModifiersState::empty(),
            pan_delta: [0.0, 0.0],
            zoom_factor: 1.0,
            zoom_focus: [0.0, 0.0],
            key_events: Vec::new(),
            term_input: Vec::new(),
        })
    }

    fn rect_of(&self, id: NodeId) -> [f32; 4] {
        win_rect(self.cam, self.view.win_pos[&id], self.view.win_size[&id])
    }

    /// The topmost canvas node (app or file) under `mp`, if any.
    fn topmost_under(&self, mp: [f32; 2]) -> Option<NodeId> {
        self.z
            .iter()
            .rev()
            .copied()
            .find(|&id| contains(self.rect_of(id), mp))
    }

    /// The topmost node whose **output** port (right edge) is under `mp` — where a
    /// wire is dragged out. Ports sit on the node edge, so half the circle is
    /// outside the rect; hit-test the whole disc separately.
    fn output_port_under(&self, mp: [f32; 2], zf: f32) -> Option<NodeId> {
        self.z
            .iter()
            .rev()
            .copied()
            .find(|&id| near(mp, port_out(self.rect_of(id)), PORT_R * zf + 3.0))
    }

    /// The topmost node whose **input** port (left edge) is under `mp` — where a
    /// dragged wire is dropped.
    fn input_port_under(&self, mp: [f32; 2], zf: f32) -> Option<NodeId> {
        self.z
            .iter()
            .rev()
            .copied()
            .find(|&id| near(mp, port_in(self.rect_of(id)), PORT_R * zf + 3.0))
    }

    /// A canvas position that centres a node of `size` in the current view, with
    /// a small cascade (by `n`) so successively added nodes don't fully overlap.
    fn view_center(&self, size: [f32; 2], n: usize) -> [f32; 2] {
        let c = self
            .cam
            .to_canvas([self.viewport[0] * 0.5, self.viewport[1] * 0.5]);
        let step = (n % 8) as f32 * 24.0;
        [c[0] - size[0] * 0.5 + step, c[1] - size[1] * 0.5 + step]
    }

    /// A centred, cascading canvas position for a newly added file node.
    fn next_file_pos(&self) -> [f32; 2] {
        self.view_center([FILE_W, FILE_H], self.view.file_nodes.len())
    }

    /// The live app node with id `id`, if it is an app (not a file) node.
    fn app_node(&self, id: NodeId) -> Option<SharedNode> {
        self.view.app_node(id)
    }

    /// (Re)run an idle or exited node's guest. Commits any in-progress args edit
    /// for this node first, then asks the server to start it.
    fn run_node(&mut self, id: NodeId) {
        if let Some((eid, text)) = self.editing_args.take() {
            if eid == id {
                self.conn.send(Command::Update {
                    id,
                    patch: NodePatch {
                        args: Some(text),
                        ..Default::default()
                    },
                });
            } else {
                self.editing_args = Some((eid, text));
            }
        }
        self.conn.send(Command::Run(id));
    }

    /// Toggle a node between attached and popped-out into its own OS window.
    /// Reattaching just drops the window here (the surface reverts to its
    /// in-workspace size next frame); detaching is deferred to `about_to_wait`,
    /// which has the `ActiveEventLoop` needed to create a window.
    fn toggle_detach(&mut self, id: NodeId) {
        if self.detached.remove(&id).is_none() && !self.pending_detach.contains(&id) {
            self.pending_detach.push(id);
        }
    }

    /// Create OS windows for any queued detach requests. Called from
    /// `about_to_wait` (has the event loop) before rendering.
    fn create_pending_detached(&mut self, el: &ActiveEventLoop) {
        if self.pending_detach.is_empty() {
            return;
        }
        // Resolve each request's initial window size + title before borrowing gfx.
        let reqs: Vec<(NodeId, [u32; 2], String)> = std::mem::take(&mut self.pending_detach)
            .into_iter()
            .filter(|id| !self.detached.contains_key(id))
            .map(|id| {
                let size = self
                    .view
                    .win_size
                    .get(&id)
                    .map(|s| {
                        [
                            (s[0] - 2.0 * BORDER).max(200.0) as u32,
                            (s[1] - TITLE_H - BORDER).max(150.0) as u32,
                        ]
                    })
                    .unwrap_or([480, 360]);
                let title = self
                    .app_node(id)
                    .map(|n| n.name.clone())
                    .unwrap_or_else(|| format!("node {id}"));
                (id, size, title)
            })
            .collect();
        let Some(gfx) = &self.gfx else { return };
        for (id, size, title) in reqs {
            match gfx.create_detached(el, &format!("{title} — wk (detached)"), size) {
                Ok((window, surface, config)) => {
                    let size = Gfx::logical_size(&window);
                    self.detached.insert(
                        id,
                        Detached {
                            window,
                            surface,
                            config,
                            size,
                            mouse: [0.0, 0.0],
                            lmb: false,
                            prev_lmb: false,
                            key_events: Vec::new(),
                            term_input: Vec::new(),
                        },
                    );
                }
                Err(e) => eprintln!("wk: failed to detach node {id}: {e}"),
            }
        }
    }

    /// Handle a winit event addressed to a detached node's window: close (which
    /// reattaches the node), resize (updates the render target), or input (queued
    /// and forwarded to the node in `frame`, like the main window).
    fn detached_window_event(&mut self, wid: WindowId, event: WindowEvent) {
        let Some(node_id) = self
            .detached
            .iter()
            .find(|(_, d)| d.window.id() == wid)
            .map(|(&k, _)| k)
        else {
            return;
        };
        match event {
            // Closing a detached window brings the node back into the workspace.
            WindowEvent::CloseRequested => {
                self.detached.remove(&node_id);
            }
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                if let (Some(gfx), Some(det)) = (self.gfx.as_ref(), self.detached.get_mut(&node_id))
                {
                    let size = Gfx::logical_size(&det.window);
                    det.size = size;
                    gfx.reconfigure(&det.surface, &mut det.config, size);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                if let Some(det) = self.detached.get_mut(&node_id) {
                    let scale = det.window.scale_factor();
                    det.mouse = [(position.x / scale) as f32, (position.y / scale) as f32];
                }
            }
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } => {
                if let Some(det) = self.detached.get_mut(&node_id) {
                    det.lmb = state == ElementState::Pressed;
                }
            }
            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),
            WindowEvent::KeyboardInput { event, .. } => {
                if let PhysicalKey::Code(code) = event.physical_key {
                    let pressed = event.state == ElementState::Pressed;
                    let mods = self.mods;
                    if let Some(det) = self.detached.get_mut(&node_id) {
                        if pressed {
                            if let Some(bytes) = encode_term_key(code, event.text.as_deref(), mods)
                            {
                                det.term_input.extend(bytes);
                            }
                        }
                        det.key_events.push((key_event(code, mods), pressed));
                    }
                }
            }
            _ => {}
        }
    }

    /// The screen-space endpoints of a wire (both nodes must still be placed).
    fn wire_endpoints(&self, w: Wire) -> Option<([f32; 2], [f32; 2])> {
        let (a, b) = match w {
            Wire::File(f, a) => (f, a),
            Wire::Midi(s, d) => (s, d),
            Wire::Serve(h, hp) => (h, hp),
            Wire::Net(app, net) => (app, net),
        };
        if self.view.win_pos.contains_key(&a) && self.view.win_pos.contains_key(&b) {
            // Source's output port (right) to target's input port (left), so the
            // wire flows left-to-right and lines up with the visible dots.
            Some((port_out(self.rect_of(a)), port_in(self.rect_of(b))))
        } else {
            None
        }
    }

    /// The wire (of any kind, either direction) already joining two nodes.
    fn wire_between(&self, a: NodeId, b: NodeId) -> Option<Wire> {
        let s = &self.view;
        let pair = |x: NodeId, y: NodeId| (x == a && y == b) || (x == b && y == a);
        s.connections
            .iter()
            .find(|&&(f, n)| pair(f, n))
            .map(|&(f, n)| Wire::File(f, n))
            .or_else(|| {
                s.midi_links
                    .iter()
                    .find(|&&(x, y)| pair(x, y))
                    .map(|&(x, y)| Wire::Midi(x, y))
            })
            .or_else(|| {
                s.serves
                    .iter()
                    .find(|(&h, &hp)| pair(h, hp))
                    .map(|(&h, &hp)| Wire::Serve(h, hp))
            })
            .or_else(|| {
                s.net_links
                    .iter()
                    .find(|&&(x, y)| pair(x, y))
                    .map(|&(x, y)| Wire::Net(x, y))
            })
    }

    /// The connection wire nearest to `mp` within the pick radius, if any. Picks
    /// against the drawn curve, not the straight chord, so clicks land on the arc.
    fn wire_at(&self, mp: [f32; 2], zf: f32) -> Option<Wire> {
        let s = &self.view;
        let all = s
            .connections
            .iter()
            .map(|&(f, a)| Wire::File(f, a))
            .chain(s.midi_links.iter().map(|&(s, d)| Wire::Midi(s, d)))
            .chain(s.serves.iter().map(|(&h, &hp)| Wire::Serve(h, hp)))
            .chain(s.net_links.iter().map(|&(a, n)| Wire::Net(a, n)));
        let mut best: Option<(f32, Wire)> = None;
        for w in all {
            if let Some((a, b)) = self.wire_endpoints(w) {
                let arrow = connection_arrow(a, b, zf);
                let pts = crate::arrows::polyline(&arrow, 24);
                let d = pts
                    .windows(2)
                    .map(|s| dist_to_segment(mp, s[0], s[1]))
                    .fold(f32::INFINITY, f32::min);
                if d <= WIRE_PICK && best.map(|(bd, _)| d < bd).unwrap_or(true) {
                    best = Some((d, w));
                }
            }
        }
        best.map(|(_, w)| w)
    }

    /// All command-palette entries (label + action) for the current state.
    fn palette_all(&self) -> Vec<(String, PaletteCmd)> {
        let mut v: Vec<(String, PaletteCmd)> = self
            .view
            .available
            .iter()
            .enumerate()
            .map(|(i, dep)| (format!("Add {}", dep.name), PaletteCmd::Launch(i)))
            .collect();
        v.push(("Add Virtual File".into(), PaletteCmd::AddVirtualFile));
        v.push(("Add Host File".into(), PaletteCmd::AddHostFile));
        v.push(("Add Port".into(), PaletteCmd::AddPort));
        v.push(("Add Network".into(), PaletteCmd::AddNetwork));
        v.push(("Add Gateway".into(), PaletteCmd::AddGateway));
        v.push(("New Workspace  (Cmd+T)".into(), PaletteCmd::NewWorkspace));
        if self.tabs.len() > 1 {
            v.push((
                "Close Workspace  (Cmd+W)".into(),
                PaletteCmd::CloseWorkspace,
            ));
        }
        for &z in &ZOOM_PRESETS {
            v.push((format!("Zoom {:.0}%", z * 100.0), PaletteCmd::Zoom(z)));
        }
        // Jump to any node in the active workspace (searchable by name).
        for &id in &self.view.node_ids {
            v.push((
                format!("Go to {}", self.node_label(id)),
                PaletteCmd::GoTo(id),
            ));
        }
        v.push(("Quit wk".into(), PaletteCmd::Quit));
        v
    }

    /// A short human label for a node (for palette search / "go to").
    fn node_label(&self, id: NodeId) -> String {
        if let Some(n) = self.view.app_node(id) {
            n.name.clone()
        } else if let Some(f) = self.view.file_nodes.get(&id) {
            f.name.clone()
        } else if let Some(&p) = self.view.host_ports.get(&id) {
            format!("port :{p}")
        } else if self.view.gateways.contains(&id) {
            "gateway".into()
        } else if self.view.net_nodes.contains(&id) {
            "network".into()
        } else {
            "node".into()
        }
    }

    /// Palette entries matching the current query (case-insensitive substring).
    fn palette_filtered(&self) -> Vec<(String, PaletteCmd)> {
        let q = self.palette_query.to_lowercase();
        self.palette_all()
            .into_iter()
            .filter(|(label, _)| q.is_empty() || label.to_lowercase().contains(&q))
            .collect()
    }

    /// Largest valid scroll offset for `len` filtered rows.
    fn palette_max_scroll(len: usize) -> f32 {
        len.saturating_sub(PALETTE_MAX) as f32
    }

    /// Scroll so the selected row is within the visible window.
    fn palette_scroll_to_sel(&mut self) {
        let top = self.palette_scroll.round() as usize;
        if self.palette_sel < top {
            self.palette_scroll = self.palette_sel as f32;
        } else if self.palette_sel >= top + PALETTE_MAX {
            self.palette_scroll = (self.palette_sel + 1 - PALETTE_MAX) as f32;
        }
    }

    /// Handle a key press while editing an idle node's launch args.
    fn editing_args_key(&mut self, code: KeyCode, text: Option<&str>) {
        match code {
            KeyCode::Escape => self.editing_args = None,
            KeyCode::Enter | KeyCode::NumpadEnter => {
                // Commit the edit and run the node (run_node commits + launches).
                if let Some((id, _)) = self.editing_args {
                    self.run_node(id);
                }
            }
            KeyCode::Backspace => {
                if let Some((_, s)) = self.editing_args.as_mut() {
                    s.pop();
                }
            }
            _ => {
                if let (Some((_, s)), Some(t)) = (self.editing_args.as_mut(), text) {
                    for ch in t.chars().filter(|c| !c.is_control()) {
                        s.push(ch);
                    }
                }
            }
        }
    }

    /// Handle a key press while the command palette is open.
    fn palette_key(&mut self, code: KeyCode, text: Option<&str>) {
        let len = self.palette_filtered().len();
        match code {
            KeyCode::Escape => {
                self.palette_open = false;
                self.palette_query.clear();
            }
            KeyCode::Enter | KeyCode::NumpadEnter => {
                self.palette_run = self
                    .palette_filtered()
                    .get(self.palette_sel)
                    .map(|(_, c)| *c);
                self.palette_open = false;
                self.palette_query.clear();
            }
            KeyCode::ArrowDown => {
                if len > 0 {
                    self.palette_sel = (self.palette_sel + 1).min(len - 1);
                    self.palette_scroll_to_sel();
                }
            }
            KeyCode::ArrowUp => {
                self.palette_sel = self.palette_sel.saturating_sub(1);
                self.palette_scroll_to_sel();
            }
            KeyCode::Backspace => {
                self.palette_query.pop();
                self.palette_sel = 0;
                self.palette_scroll = 0.0;
            }
            _ => {
                if let Some(t) = text {
                    for ch in t.chars().filter(|c| !c.is_control()) {
                        self.palette_query.push(ch);
                    }
                    self.palette_sel = 0;
                    self.palette_scroll = 0.0;
                }
            }
        }
    }

    /// Execute a palette command (from `frame`, where the screen size is known).
    fn run_palette(&mut self, cmd: PaletteCmd, fb: [f32; 2]) {
        let ws = self.active_ws;
        match cmd {
            PaletteCmd::Launch(dep) => {
                let pos = self.view_center([360.0, 260.0], 0);
                self.conn.send(Command::Create(Resource::Node {
                    kind: NodeKind::App { dep },
                    pos,
                    ws,
                }));
            }
            PaletteCmd::GoTo(id) => {
                if let (Some(&pos), Some(&size)) =
                    (self.view.win_pos.get(&id), self.view.win_size.get(&id))
                {
                    let c = [pos[0] + size[0] * 0.5, pos[1] + size[1] * 0.5];
                    let z = self.cam.zoom;
                    self.pan_target = [fb[0] * 0.5 - c[0] * z, fb[1] * 0.5 - c[1] * z];
                }
            }
            PaletteCmd::AddVirtualFile => {
                let pos = self.next_file_pos();
                self.conn.send(Command::Create(Resource::Node {
                    kind: NodeKind::VirtualFile,
                    pos,
                    ws,
                }));
            }
            PaletteCmd::AddHostFile => {
                let pos = self.next_file_pos();
                self.conn.send(Command::Create(Resource::Node {
                    kind: NodeKind::HostFile,
                    pos,
                    ws,
                }));
            }
            PaletteCmd::AddPort => {
                let pos = self.view_center([FILE_W, FILE_H], self.view.host_ports.len());
                self.conn.send(Command::Create(Resource::Node {
                    kind: NodeKind::Port,
                    pos,
                    ws,
                }));
            }
            PaletteCmd::AddNetwork => {
                let pos = self.view_center([FILE_W, FILE_H], self.view.net_nodes.len());
                self.conn.send(Command::Create(Resource::Node {
                    kind: NodeKind::Network,
                    pos,
                    ws,
                }));
            }
            PaletteCmd::AddGateway => {
                let pos = self.view_center([FILE_W, FILE_H], self.view.net_nodes.len());
                self.conn.send(Command::Create(Resource::Node {
                    kind: NodeKind::Gateway,
                    pos,
                    ws,
                }));
            }
            PaletteCmd::NewWorkspace => self.new_workspace(),
            PaletteCmd::CloseWorkspace => self.close_workspace(self.active_ws),
            PaletteCmd::Zoom(z) => {
                self.cam
                    .zoom_at(z / self.cam.zoom, [fb[0] * 0.5, fb[1] * 0.5]);
                self.pan_target = self.cam.pan;
            }
            PaletteCmd::Quit => self.request_exit = true,
        }
    }

    /// Create a new workspace tab and switch this client's view to it. The client
    /// mints the id so it can switch locally; the server just records the tab.
    fn new_workspace(&mut self) {
        let id = NodeId::new();
        self.conn.send(Command::Create(Resource::Workspace { id }));
        self.active_ws = id;
    }

    /// Move to the next (`forward`) or previous open tab, wrapping around.
    fn cycle_tab(&mut self, forward: bool) {
        let n = self.tabs.len();
        if n < 2 {
            return;
        }
        let i = self
            .tabs
            .iter()
            .position(|&id| id == self.active_ws)
            .unwrap_or(0);
        let j = if forward {
            (i + 1) % n
        } else {
            (i + n - 1) % n
        };
        self.active_ws = self.tabs[j];
    }

    /// Delete a workspace and all its nodes. Switches this client to a neighbour
    /// first; never closes the last tab (the server refuses too).
    fn close_workspace(&mut self, id: NodeId) {
        if self.tabs.len() <= 1 {
            return;
        }
        if self.active_ws == id {
            let i = self.tabs.iter().position(|&t| t == id).unwrap_or(0);
            self.active_ws = if i > 0 {
                self.tabs[i - 1]
            } else {
                self.tabs[1]
            };
        }
        self.conn.send(Command::Delete(ResourceRef::Workspace(id)));
        self.tabs.retain(|&t| t != id);
    }

    /// Duplicate the focused node, else the one under the cursor.
    fn duplicate_focused(&mut self) {
        if let Some(id) = self.kbd_focus.or_else(|| self.topmost_under(self.mouse)) {
            self.conn.send(Command::Duplicate(id));
        }
    }

    /// The tab rectangles (one per workspace, in order) and the trailing "+"
    /// button rect. Tabs are labelled by their 1-based position and carry a
    /// close box (see [`tab_close_btn`]).
    fn tab_layout(&self, gfx: &Gfx) -> (Vec<(NodeId, [f32; 4])>, [f32; 4]) {
        let mut rects = Vec::with_capacity(self.tabs.len());
        let mut x = 0.0;
        for (i, &id) in self.tabs.iter().enumerate() {
            let label = gfx.fonts.measure(&format!("{}", i + 1)) as f32;
            let w = label + 2.0 * PAD + (TAB_H - 12.0).max(8.0) + 8.0;
            rects.push((id, [x, 0.0, x + w, TAB_H]));
            x += w;
        }
        let plus_w = gfx.fonts.measure("+") as f32 + 2.0 * PAD;
        (rects, [x, 0.0, x + plus_w, TAB_H])
    }

    /// Panel/query/row rects for the command palette at screen size `fb`.
    fn palette_layout(fb: [f32; 2]) -> (f32, f32, f32, f32) {
        let w = (fb[0] * 0.5).clamp(320.0, 560.0);
        let x = (fb[0] - w) * 0.5;
        let y = (fb[1] * 0.16).max(40.0);
        let row_h = MENU_H + 4.0;
        (x, y, w, row_h)
    }

    /// Draw a terminal cell grid, scaled uniformly to fit `area`, clipped to
    /// `clip`. Shared by the in-workspace node body and its detached window.
    fn draw_term_grid(
        &mut self,
        quads: &mut Vec<Quad>,
        gfx: &mut Gfx,
        cells: &[CellView],
        cursor: Option<(usize, usize)>,
        area: [f32; 4],
        clip: [f32; 4],
    ) {
        let white = gfx.renderer.white;
        let cols = wk_server::terminal::COLS as f32;
        let rows = wk_server::terminal::ROWS as f32;
        let bw = (gfx.fonts.measure("M") as f32).max(1.0);
        let bh = (gfx.fonts.line_height() as f32).max(1.0);
        let scale = ((area[2] - area[0]) / (cols * bw))
            .min((area[3] - area[1]) / (rows * bh))
            .max(0.01);
        let cw = bw * scale;
        let chh = bh * scale;
        quads.push(Quad::solid(white, area, TERM_BG, clip));
        for cell in cells {
            let cx = area[0] + cell.col as f32 * cw;
            let cy = area[1] + cell.row as f32 * chh;
            if let Some(bg) = cell.bg {
                quads.push(Quad::solid(
                    white,
                    [cx, cy, cx + cw, cy + chh],
                    rgba(bg),
                    clip,
                ));
            }
            if cell.ch != ' ' {
                let mut buf = [0u8; 4];
                self.text_cache.draw(
                    quads,
                    &mut gfx.renderer,
                    &gfx.fonts,
                    &gfx.device,
                    &gfx.queue,
                    cell.ch.encode_utf8(&mut buf),
                    cx,
                    cy,
                    scale,
                    rgba(cell.fg),
                    clip,
                );
            }
        }
        if let Some((ccol, crow)) = cursor {
            let cx = area[0] + ccol as f32 * cw;
            let cy = area[1] + crow as f32 * chh;
            quads.push(Quad::solid(
                white,
                [cx, cy, cx + cw, cy + chh],
                [0.85, 0.85, 0.9, 0.45],
                clip,
            ));
        }
    }

    /// Render one detached node into its own window: the node's live content
    /// (graphical surface or terminal grid) filling the window.
    fn render_detached(
        &mut self,
        gfx: &mut Gfx,
        id: NodeId,
        node_surface: &HashMap<NodeId, SharedSurface>,
    ) {
        let Some(size) = self.detached.get(&id).map(|d| d.size) else {
            return;
        };
        let fb = [size[0] as f32, size[1] as f32];
        let full = [0.0, 0.0, fb[0], fb[1]];
        let white = gfx.renderer.white;
        let mut quads: Vec<Quad> = Vec::new();

        let sid = node_surface.get(&id).map(|s| s.lock().unwrap().id);
        if let Some(sid) = sid {
            if let Some(&(tex, _, _)) = self.views.get(&sid) {
                quads.push(Quad::tex(
                    full,
                    [0.0, 0.0, 1.0, 1.0],
                    [1.0, 1.0, 1.0, 1.0],
                    tex,
                    full,
                ));
            }
        } else if let Some((cells, cursor)) =
            self.terminals.get(&id).map(|t| (t.cells(), t.cursor()))
        {
            self.draw_term_grid(&mut quads, gfx, &cells, cursor, full, full);
        } else {
            quads.push(Quad::solid(white, full, DETACHED_BG, full));
        }

        let Some(det) = self.detached.get(&id) else {
            return;
        };
        let frame = match det.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f)
            | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            _ => return,
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = gfx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("detached"),
            });
        {
            let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(CLEAR),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            gfx.renderer
                .draw(&gfx.device, &gfx.queue, &mut rpass, fb, &quads);
        }
        gfx.queue.submit([encoder.finish()]);
        det.window.pre_present_notify();
        frame.present();
    }

    /// One compositor frame: update from input, drive surfaces, render.
    fn frame(&mut self) {
        let Some(mut gfx) = self.gfx.take() else {
            return;
        };
        // Refresh our snapshot of the server for this frame. The server ticks
        // and advances the runtime on its own thread; we only read and send.
        let full = self.conn.view();
        self.tabs = full.workspaces.clone();
        if !self.tabs.contains(&self.active_ws) {
            self.active_ws = self.tabs.first().copied().unwrap_or(self.active_ws);
        }
        // Ports claimed by more than one HostPort (across every workspace, since
        // they all run) can't all bind — flag them.
        let mut port_count: HashMap<u16, u32> = HashMap::new();
        for &p in full.host_ports.values() {
            *port_count.entry(p).or_default() += 1;
        }
        self.port_conflicts = port_count
            .into_iter()
            .filter(|&(_, c)| c > 1)
            .map(|(p, _)| p)
            .collect();
        self.view = full.for_workspace(self.active_ws);

        // Apply pan/zoom (zoom immediate, pan eased).
        if (self.zoom_factor - 1.0).abs() > f32::EPSILON {
            self.cam.zoom_at(self.zoom_factor, self.zoom_focus);
            self.pan_target = self.cam.pan;
        }
        self.pan_target[0] += self.pan_delta[0];
        self.pan_target[1] += self.pan_delta[1];
        self.cam.pan = [
            ease(self.cam.pan[0], self.pan_target[0]),
            ease(self.cam.pan[1], self.pan_target[1]),
        ];
        self.pan_delta = [0.0, 0.0];
        self.zoom_factor = 1.0;

        let mp = self.mouse;
        let lmb = self.lmb;
        let down_edge = lmb && !self.prev_lmb;
        let up_edge = !lmb && self.prev_lmb;
        let zf = self.cam.zoom;
        let fb = [
            gfx.surface_desc.width as f32,
            gfx.surface_desc.height as f32,
        ];
        // Remember the viewport so newly added nodes land in the current view.
        self.viewport = fb;

        // ---- reconcile the stacking order with the server's live node set ----
        // Positions are assigned by the server when a node is created, so here the
        // client only tracks draw order: new nodes go on top, gone ones drop out.
        let nodes: Vec<SharedNode> = self.view.nodes.clone();
        let node_by_id: HashMap<NodeId, SharedNode> =
            nodes.iter().map(|i| (i.id, i.clone())).collect();
        let ids = self.view.node_ids.clone();
        let live: std::collections::HashSet<NodeId> = ids.iter().copied().collect();
        for &id in &ids {
            if !self.z.contains(&id) {
                self.z.push(id);
            }
        }
        self.z.retain(|id| live.contains(id));

        let surfaces: Vec<SharedSurface> = self.view.surfaces.clone();
        let node_surface: HashMap<NodeId, SharedSurface> = surfaces
            .iter()
            .map(|s| (s.lock().unwrap().node_id, s.clone()))
            .collect();

        // ---- feed terminal nodes (those without a surface) ----
        for node in &nodes {
            if node_surface.contains_key(&node.id) {
                continue;
            }
            let bytes = node.term_io.drain_out();
            let term = self
                .terminals
                .entry(node.id)
                .or_insert_with(|| wk_server::terminal::Terminal::new(node.term_io.clone()));
            if !bytes.is_empty() {
                term.feed(&bytes);
            }
        }
        self.terminals
            .retain(|id, _| node_by_id.contains_key(id) && !node_surface.contains_key(id));

        // ---- interaction ----
        let mut to_close: Vec<NodeId> = Vec::new();

        // Corner zoom button (bottom-left) and the preset items stacked above it.
        let zoom_btn_w = gfx.fonts.measure("200%") as f32 + 3.0 * PAD;
        let zoom_btn = [0.0, fb[1] - MENU_H, zoom_btn_w, fb[1]];
        let zoom_item = |i: usize| -> [f32; 4] {
            let top = fb[1] - MENU_H - ZOOM_PRESETS.len() as f32 * MENU_H;
            let y0 = top + i as f32 * MENU_H;
            [0.0, y0, zoom_btn_w, y0 + MENU_H]
        };
        // Corner add/command button (bottom-right) that opens the Cmd/Ctrl+K
        // palette — the single entry point for adding nodes and other commands.
        let menu_btn_w = gfx.fonts.measure("+ Add  (Cmd+K)") as f32 + 2.0 * PAD;
        let menu_btn = [fb[0] - menu_btn_w, fb[1] - MENU_H, fb[0], fb[1]];

        // Continue an in-progress drag (move / resize / connect).
        //
        // The dragged node can vanish mid-drag — undo of its creation, closing
        // its workspace, or switching tabs (which filters it out of the
        // active-workspace view). In that case abandon the drag instead of
        // indexing a now-missing key in `view.win_pos`/`win_size`.
        if let Some(d) = self
            .drag
            .take()
            .filter(|d| self.view.win_pos.contains_key(&d.id))
        {
            match d.mode {
                DragMode::Move if lmb => {
                    let mc = self.cam.to_canvas(mp);
                    let pos = [mc[0] - d.grab[0], mc[1] - d.grab[1]];
                    self.conn.send(Command::Update {
                        id: d.id,
                        patch: NodePatch {
                            pos: Some(pos),
                            ..Default::default()
                        },
                    });
                    self.drag = Some(d);
                }
                DragMode::Resize if lmb => {
                    let p = self.view.win_pos[&d.id];
                    let mc = self.cam.to_canvas(mp);
                    let size = [
                        (mc[0] - p[0]).max(100.0),
                        (mc[1] - p[1]).max(TITLE_H + 40.0),
                    ];
                    self.conn.send(Command::Update {
                        id: d.id,
                        patch: NodePatch {
                            size: Some(size),
                            ..Default::default()
                        },
                    });
                    self.drag = Some(d);
                }
                DragMode::Connect if lmb => self.drag = Some(d),
                // Released: wire to the target node — its input port (left), or
                // anywhere on its body for convenience. Dragging over an existing
                // wire removes it (the client decides create vs delete; the
                // server's create never disconnects).
                DragMode::Connect => {
                    if let Some(target) = self
                        .input_port_under(mp, zf)
                        .or_else(|| self.topmost_under(mp))
                    {
                        if target != d.id {
                            match self.wire_between(d.id, target) {
                                Some(w) => self.conn.send(Command::Delete(ResourceRef::Wire(w))),
                                None => self
                                    .conn
                                    .send(Command::Create(Resource::Wire { a: d.id, b: target })),
                            }
                        }
                    }
                }
                _ => {} // move/resize released: drop the drag
            }
        }

        if down_edge && self.drag.is_none() {
            let mut consumed = false;
            // Any fresh click clears the wire selection; a click that lands on a
            // wire (empty-canvas branch below) re-selects it.
            self.wire_sel = None;
            // The command palette is modal: click a row to run it, click
            // anywhere else to dismiss it.
            if self.palette_open {
                let (px, py, pw, row_h) = Self::palette_layout(fb);
                let filtered = self.palette_filtered();
                let start = (self.palette_scroll.round() as usize).min(filtered.len());
                for (i, (_, cmd)) in filtered.iter().skip(start).take(PALETTE_MAX).enumerate() {
                    let y0 = py + (i as f32 + 1.0) * row_h;
                    if contains([px, y0, px + pw, y0 + row_h], mp) {
                        self.palette_run = Some(*cmd);
                        break;
                    }
                }
                self.palette_open = false;
                self.palette_query.clear();
                consumed = true;
            }
            // Corner zoom menu (drawn on top) takes clicks first.
            if !consumed && self.zoom_menu_open {
                let mut hit = false;
                for (i, &z) in ZOOM_PRESETS.iter().enumerate() {
                    if contains(zoom_item(i), mp) {
                        // Jump to the preset zoom, anchored at the screen centre.
                        self.cam
                            .zoom_at(z / self.cam.zoom, [fb[0] * 0.5, fb[1] * 0.5]);
                        self.pan_target = self.cam.pan;
                        hit = true;
                        break;
                    }
                }
                self.zoom_menu_open = false;
                if hit || contains(zoom_btn, mp) {
                    consumed = true;
                }
            } else if !consumed && contains(zoom_btn, mp) {
                self.zoom_menu_open = true;
                consumed = true;
            }
            if consumed {
                // handled by the zoom menu
            } else if contains(menu_btn, mp) {
                // Open the command palette (same as Cmd/Ctrl+K).
                self.palette_open = true;
                self.palette_query.clear();
                self.palette_sel = 0;
                self.palette_scroll = 0.0;
                consumed = true;
            }
            // Tab bar (top): click a tab to view it, its × to close it, or "+"
            // to open a new one.
            if !consumed && self.tabs.len() > 1 {
                let (rects, plus) = self.tab_layout(&gfx);
                if contains(plus, mp) {
                    self.new_workspace();
                    consumed = true;
                } else if let Some(&(id, r)) = rects.iter().find(|(_, r)| contains(*r, mp)) {
                    if contains(tab_close_btn(r), mp) {
                        self.close_workspace(id);
                    } else {
                        self.active_ws = id;
                    }
                    consumed = true;
                }
            }
            // Dragging a wire out of a node's output port (right edge). Checked
            // before the node-body hit-test so the port's outer half (past the
            // edge) is grabbable too.
            if !consumed {
                if let Some(id) = self.output_port_under(mp, zf) {
                    self.z.retain(|&x| x != id);
                    self.z.push(id);
                    self.drag = Some(Drag {
                        id,
                        mode: DragMode::Connect,
                        grab: [0.0, 0.0],
                    });
                    consumed = true;
                }
            }
            if !consumed {
                if let Some(id) = self.topmost_under(mp) {
                    self.z.retain(|&x| x != id);
                    self.z.push(id);
                    let r = self.rect_of(id);
                    let is_file = self.view.file_nodes.contains_key(&id);
                    let is_port = self.view.host_ports.contains_key(&id);
                    let is_net = self.view.net_nodes.contains(&id);
                    if is_file || is_port || is_net {
                        // Canvas widget nodes (file / HostPort / Network): close,
                        // adjust port (HostPort −/+ buttons), or move.
                        let (minus, plus) = port_step_btns(r, zf);
                        if contains(close_btn(r, zf), mp) {
                            self.conn.send(Command::Delete(ResourceRef::Node(id)));
                        } else if is_port && contains(plus, mp) {
                            self.conn.send(Command::Update {
                                id,
                                patch: NodePatch {
                                    port_delta: Some(1),
                                    ..Default::default()
                                },
                            });
                        } else if is_port && contains(minus, mp) {
                            self.conn.send(Command::Update {
                                id,
                                patch: NodePatch {
                                    port_delta: Some(-1),
                                    ..Default::default()
                                },
                            });
                        } else {
                            let mc = self.cam.to_canvas(mp);
                            let p = self.view.win_pos[&id];
                            self.drag = Some(Drag {
                                id,
                                mode: DragMode::Move,
                                grab: [mc[0] - p[0], mc[1] - p[1]],
                            });
                        }
                    } else {
                        // App node: clicking anywhere activates it.
                        self.kbd_focus = Some(id);
                        let idle = self
                            .app_node(id)
                            .map(|n| !n.running.load(Ordering::Relaxed) && n.is_runnable())
                            .unwrap_or(false);
                        if contains(close_btn(r, zf), mp) {
                            to_close.push(id);
                        } else if contains(detach_btn(r, zf), mp) {
                            self.toggle_detach(id);
                        } else if idle && contains(run_btn(r, zf), mp) {
                            self.run_node(id);
                        } else if contains(resize_grip(r, zf), mp) {
                            self.editing_args = None;
                            self.drag = Some(Drag {
                                id,
                                mode: DragMode::Resize,
                                grab: [0.0, 0.0],
                            });
                        } else if contains(title_bar(r, zf), mp) {
                            self.editing_args = None;
                            let mc = self.cam.to_canvas(mp);
                            let p = self.view.win_pos[&id];
                            self.drag = Some(Drag {
                                id,
                                mode: DragMode::Move,
                                grab: [mc[0] - p[0], mc[1] - p[1]],
                            });
                        } else if idle && contains(args_bar(r, zf), mp) {
                            // Click the args bar of an idle node to edit them.
                            let cur = self
                                .view
                                .node_args
                                .get(&id)
                                .cloned()
                                .unwrap_or_default()
                                .join(" ");
                            self.editing_args = Some((id, cur));
                        }
                    }
                    consumed = true;
                }
            }
            if !consumed {
                // Clicked empty canvas: select a wire under the cursor (so it
                // can be deleted), else unfocus the app.
                self.kbd_focus = None;
                self.editing_args = None;
                self.wire_sel = self.wire_at(mp, zf);
            }
        }

        // Run a command chosen from the palette (executed here so screen size
        // is known for zoom).
        if let Some(cmd) = self.palette_run.take() {
            self.run_palette(cmd, fb);
        }

        // Delete the selected wire on Delete/Backspace.
        if self.del_wire {
            self.del_wire = false;
            if let Some(w) = self.wire_sel.take() {
                self.conn.send(Command::Delete(ResourceRef::Wire(w)));
            }
        }
        // Drop a stale selection (its node was closed/removed).
        if let Some(w) = self.wire_sel {
            if !self.view.wire_exists(w) {
                self.wire_sel = None;
            }
        }

        // Route pointer to the surface under the cursor (not while the modal
        // command palette is open).
        if self.drag.is_none() && !self.palette_open {
            if let Some(&id) = self.z.iter().rev().find(|&&id| {
                contains(
                    win_rect(self.cam, self.view.win_pos[&id], self.view.win_size[&id]),
                    mp,
                )
            }) {
                let r = win_rect(self.cam, self.view.win_pos[&id], self.view.win_size[&id]);
                let ca = content_rect(r, zf);
                if contains(ca, mp) {
                    if let Some(surf) = node_surface.get(&id) {
                        let local = PointerEvent {
                            x: ((mp[0] - ca[0]) / zf) as f64,
                            y: ((mp[1] - ca[1]) / zf) as f64,
                        };
                        let mut s = surf.lock().unwrap();
                        s.pointer_move.push_back(local);
                        if down_edge {
                            s.pointer_down.push_back(local);
                        }
                        if up_edge {
                            s.pointer_up.push_back(local);
                        }
                    }
                }
            }
        }

        // Keyboard to the focused window: a graphical node's surface gets
        // wasi-gfx key events; a terminal node gets the encoded input bytes.
        if let Some(fid) = self.kbd_focus {
            if let Some(surf) = node_surface.get(&fid) {
                let mut s = surf.lock().unwrap();
                for (ev, down) in &self.key_events {
                    if *down {
                        s.key_down.push_back(ev.clone());
                    } else {
                        s.key_up.push_back(ev.clone());
                    }
                }
            } else if !self.term_input.is_empty() {
                if let (Some(term), Some(node)) =
                    (self.terminals.get_mut(&fid), node_by_id.get(&fid))
                {
                    if term.is_raw() {
                        // Raw mode: keystrokes go to the guest verbatim (no echo).
                        node.term_io.feed_in(&self.term_input);
                    } else {
                        term.key_input(&self.term_input, &node.term_io);
                    }
                }
            }
        }
        self.key_events.clear();
        self.term_input.clear();

        // ---- drive surfaces ----
        for shared in &surfaces {
            let (sid, w, h, pixels) = {
                let mut s = shared.lock().unwrap();
                // A detached node renders at its own window's size; an attached
                // one at its in-workspace content size.
                let target = if let Some(det) = self.detached.get(&s.node_id) {
                    Some(det.size)
                } else {
                    self.view.win_size.get(&s.node_id).map(|size| {
                        [
                            (size[0] - 2.0 * BORDER).max(16.0) as u32,
                            (size[1] - TITLE_H - BORDER).max(16.0) as u32,
                        ]
                    })
                };
                if let Some([cw, ch]) = target {
                    if cw != s.width || ch != s.height {
                        s.width = cw;
                        s.height = ch;
                        s.pixels = vec![0; (cw * ch * 4) as usize];
                        s.resize = Some(ResizeEvent {
                            width: cw,
                            height: ch,
                        });
                    }
                }
                let ready = s.pixels.len() == (s.width * s.height * 4) as usize;
                let px = ready.then(|| s.pixels.clone());
                let out = (s.id, s.width, s.height, px);
                s.frame_ready = true;
                s.wake();
                out
            };
            if w == 0 || h == 0 {
                continue;
            }
            let stale = self.views.get(&sid).map(|&(_, vw, vh)| vw != w || vh != h);
            match stale {
                None | Some(true) => {
                    if let Some((old, _, _)) = self.views.remove(&sid) {
                        gfx.renderer.remove_texture(old);
                    }
                    let init = pixels.unwrap_or_else(|| vec![0; (w * h * 4) as usize]);
                    let tex = gfx
                        .renderer
                        .create_texture(&gfx.device, &gfx.queue, w, h, &init);
                    self.views.insert(sid, (tex, w, h));
                }
                Some(false) => {
                    if let Some(px) = &pixels {
                        gfx.renderer
                            .update_texture(&gfx.queue, self.views[&sid].0, w, h, px);
                    }
                }
            }
        }

        // ---- build quads ----
        let white = gfx.renderer.white;
        let full = [0.0, 0.0, fb[0], fb[1]];
        let mut quads: Vec<Quad> = Vec::new();

        // Connection wires, under the nodes: curved arrows from a source's output
        // port to a target's input port. The selected wire is drawn thicker in the
        // highlight colour.
        for &(file_id, app_id) in &self.view.connections {
            if let Some((a, b)) = self.wire_endpoints(Wire::File(file_id, app_id)) {
                let sel = self.wire_sel == Some(Wire::File(file_id, app_id));
                let col = if sel { WIRE_SEL_COL } else { WIRE_COL };
                draw_connection(&mut quads, white, a, b, sel, col, zf, full);
            }
        }
        for &(src, dst) in &self.view.midi_links {
            if let Some((a, b)) = self.wire_endpoints(Wire::Midi(src, dst)) {
                let sel = self.wire_sel == Some(Wire::Midi(src, dst));
                let col = if sel { WIRE_SEL_COL } else { MIDI_WIRE_COL };
                draw_connection(&mut quads, white, a, b, sel, col, zf, full);
            }
        }
        for (&http, &hostport) in &self.view.serves {
            if let Some((a, b)) = self.wire_endpoints(Wire::Serve(http, hostport)) {
                let sel = self.wire_sel == Some(Wire::Serve(http, hostport));
                let col = if sel { WIRE_SEL_COL } else { HOSTPORT_WIRE };
                draw_connection(&mut quads, white, a, b, sel, col, zf, full);
            }
        }
        // Network membership wires (app node — Network node).
        for &(app, net) in &self.view.net_links {
            if let Some((a, b)) = self.wire_endpoints(Wire::Net(app, net)) {
                let sel = self.wire_sel == Some(Wire::Net(app, net));
                let col = if sel { WIRE_SEL_COL } else { NET_WIRE_COL };
                draw_connection(&mut quads, white, a, b, sel, col, zf, full);
            }
        }

        // Clone the draw order so the body can call `&mut self` helpers (e.g.
        // `draw_term_grid`) without holding a borrow of `self.z`.
        let z_order = self.z.clone();
        for &id in &z_order {
            let pos = self.view.win_pos[&id];
            let size = self.view.win_size[&id];
            let r = win_rect(self.cam, pos, size);
            if r[2] < 0.0 || r[0] > fb[0] || r[3] < 0.0 || r[1] > fb[1] {
                continue;
            }
            let clip = intersect(r, full);

            // A file node renders as a small labelled box with a port.
            if let Some(file) = self.view.file_nodes.get(&id) {
                let name = file.name.clone();
                let bytes = file.size;
                let host = file.host_mapped;
                let (border, bg, sub_col) = if host {
                    (HOSTFILE_BORDER, HOSTFILE_BG, [0.55, 0.68, 0.85, 1.0])
                } else {
                    (FILE_BORDER, FILE_BG, [0.65, 0.6, 0.5, 1.0])
                };
                quads.push(Quad::solid(white, r, border, clip));
                let body = [
                    r[0] + BORDER * zf,
                    r[1] + BORDER * zf,
                    r[2] - BORDER * zf,
                    r[3] - BORDER * zf,
                ];
                quads.push(Quad::solid(white, body, bg, clip));
                let lh = gfx.fonts.line_height() as f32;
                self.text_cache.draw(
                    &mut quads,
                    &mut gfx.renderer,
                    &gfx.fonts,
                    &gfx.device,
                    &gfx.queue,
                    &name,
                    r[0] + PAD * zf,
                    r[1] + PAD * zf,
                    zf,
                    TEXT,
                    clip,
                );
                // VirtualFiles show their byte count; HostMappedFiles show the
                // size plus a "disk" marker so they read as backed by a path.
                let sub = if host {
                    format!("{bytes} B · disk")
                } else {
                    format!("{bytes} B")
                };
                self.text_cache.draw(
                    &mut quads,
                    &mut gfx.renderer,
                    &gfx.fonts,
                    &gfx.device,
                    &gfx.queue,
                    &sub,
                    r[0] + PAD * zf,
                    r[1] + (PAD + lh) * zf,
                    zf * 0.85,
                    sub_col,
                    clip,
                );
                let cb = close_btn(r, zf);
                if contains(cb, mp) {
                    quads.push(Quad::solid(white, cb, CLOSE_HOT, clip));
                }
                self.text_cache.draw(
                    &mut quads,
                    &mut gfx.renderer,
                    &gfx.fonts,
                    &gfx.device,
                    &gfx.queue,
                    "x",
                    cb[0] + (cb[2] - cb[0]) * 0.28,
                    cb[1] + (cb[3] - cb[1]) * 0.05,
                    zf * 0.8,
                    TEXT,
                    clip,
                );
                draw_ports(&mut quads, gfx.renderer.circle, r, zf, mp, full);
                continue;
            }

            // A HostPort node: a labelled box exposing a wasi:http node to a
            // localhost port when wired.
            if let Some(&port) = self.view.host_ports.get(&id) {
                let serving = self.view.serves.values().any(|&hp| hp == id);
                let conflict = self.port_conflicts.contains(&port);
                quads.push(Quad::solid(white, r, HOSTPORT_BORDER, clip));
                let body = [
                    r[0] + BORDER * zf,
                    r[1] + BORDER * zf,
                    r[2] - BORDER * zf,
                    r[3] - BORDER * zf,
                ];
                quads.push(Quad::solid(white, body, HOSTPORT_BG, clip));
                let lh = gfx.fonts.line_height() as f32;
                self.text_cache.draw(
                    &mut quads,
                    &mut gfx.renderer,
                    &gfx.fonts,
                    &gfx.device,
                    &gfx.queue,
                    &format!("HostPort :{port}"),
                    r[0] + PAD * zf,
                    r[1] + PAD * zf,
                    zf,
                    if conflict { WARN } else { TEXT },
                    clip,
                );
                let (status, status_col) = if conflict {
                    ("port in use", WARN)
                } else if serving {
                    ("live ●", [0.4, 0.85, 0.5, 1.0])
                } else {
                    ("idle", [0.55, 0.7, 0.72, 1.0])
                };
                self.text_cache.draw(
                    &mut quads,
                    &mut gfx.renderer,
                    &gfx.fonts,
                    &gfx.device,
                    &gfx.queue,
                    status,
                    r[0] + PAD * zf,
                    r[1] + (PAD + lh) * zf,
                    zf * 0.7,
                    status_col,
                    clip,
                );
                let cb = close_btn(r, zf);
                if contains(cb, mp) {
                    quads.push(Quad::solid(white, cb, CLOSE_HOT, clip));
                }
                self.text_cache.draw(
                    &mut quads,
                    &mut gfx.renderer,
                    &gfx.fonts,
                    &gfx.device,
                    &gfx.queue,
                    "x",
                    cb[0] + (cb[2] - cb[0]) * 0.28,
                    cb[1] + (cb[3] - cb[1]) * 0.05,
                    zf * 0.8,
                    TEXT,
                    clip,
                );
                // Port −/+ buttons (also: scroll over the node to change fast).
                let (minus, plus) = port_step_btns(r, zf);
                for (b, label) in [(minus, "-"), (plus, "+")] {
                    quads.push(Quad::solid(
                        white,
                        b,
                        if contains(b, mp) { MENU_HOVER } else { TITLE },
                        clip,
                    ));
                    self.text_cache.draw(
                        &mut quads,
                        &mut gfx.renderer,
                        &gfx.fonts,
                        &gfx.device,
                        &gfx.queue,
                        label,
                        b[0] + (b[2] - b[0]) * 0.3,
                        b[1] + (b[3] - b[1]) * 0.02,
                        zf * 0.8,
                        TEXT,
                        clip,
                    );
                }
                draw_ports(&mut quads, gfx.renderer.circle, r, zf, mp, full);
                continue;
            }

            // A Network node: an isolated virtual network; wired app nodes share
            // it. Shows how many members are on it.
            if self.view.net_nodes.contains(&id) {
                let members = self
                    .view
                    .net_links
                    .iter()
                    .filter(|&&(_, n)| n == id)
                    .count();
                let is_gw = self.view.gateways.contains(&id);
                quads.push(Quad::solid(white, r, NET_BORDER, clip));
                let body = [
                    r[0] + BORDER * zf,
                    r[1] + BORDER * zf,
                    r[2] - BORDER * zf,
                    r[3] - BORDER * zf,
                ];
                quads.push(Quad::solid(white, body, NET_BG, clip));
                let lh = gfx.fonts.line_height() as f32;
                self.text_cache.draw(
                    &mut quads,
                    &mut gfx.renderer,
                    &gfx.fonts,
                    &gfx.device,
                    &gfx.queue,
                    if is_gw { "Gateway" } else { "Network" },
                    r[0] + PAD * zf,
                    r[1] + PAD * zf,
                    zf,
                    TEXT,
                    clip,
                );
                self.text_cache.draw(
                    &mut quads,
                    &mut gfx.renderer,
                    &gfx.fonts,
                    &gfx.device,
                    &gfx.queue,
                    &if is_gw {
                        format!("host • {members}")
                    } else {
                        format!("{members} node(s)")
                    },
                    r[0] + PAD * zf,
                    r[1] + (PAD + lh) * zf,
                    zf * 0.7,
                    [0.72, 0.62, 0.9, 1.0],
                    clip,
                );
                let cb = close_btn(r, zf);
                if contains(cb, mp) {
                    quads.push(Quad::solid(white, cb, CLOSE_HOT, clip));
                }
                self.text_cache.draw(
                    &mut quads,
                    &mut gfx.renderer,
                    &gfx.fonts,
                    &gfx.device,
                    &gfx.queue,
                    "x",
                    cb[0] + (cb[2] - cb[0]) * 0.28,
                    cb[1] + (cb[3] - cb[1]) * 0.05,
                    zf * 0.8,
                    TEXT,
                    clip,
                );
                draw_ports(&mut quads, gfx.renderer.circle, r, zf, mp, full);
                continue;
            }

            let focused = self.kbd_focus == Some(id);
            quads.push(Quad::solid(white, r, BORDER_COL, clip));
            let body = [
                r[0] + BORDER * zf,
                r[1] + BORDER * zf,
                r[2] - BORDER * zf,
                r[3] - BORDER * zf,
            ];
            quads.push(Quad::solid(white, body, BODY, clip));
            let tb = title_bar(r, zf);
            quads.push(Quad::solid(
                white,
                tb,
                if focused { TITLE_FOCUS } else { TITLE },
                clip,
            ));

            let mut node_idle = false;
            let mut node_loading = false;
            if let Some(node) = node_by_id.get(&id) {
                let running = node.running.load(Ordering::Relaxed);
                let loading = node.is_loading();
                node_loading = loading;
                let runnable = node.is_runnable();
                // Idle (offer Run/args) only once compiled and not running.
                node_idle = !loading && !running && runnable;
                let label = if loading {
                    format!("{} (loading…)", node.name)
                } else if running {
                    node.name.clone()
                } else if node.finished.load(Ordering::Relaxed) {
                    format!("{} (exited)", node.name)
                } else if runnable {
                    format!("{} (idle)", node.name)
                } else {
                    node.name.clone()
                };
                let ty = tb[1] + (TITLE_H * zf - gfx.fonts.line_height() as f32 * zf) * 0.5;
                self.text_cache.draw(
                    &mut quads,
                    &mut gfx.renderer,
                    &gfx.fonts,
                    &gfx.device,
                    &gfx.queue,
                    &label,
                    tb[0] + PAD * zf,
                    ty,
                    zf,
                    TEXT,
                    intersect(tb, full),
                );
            }

            let cb = close_btn(r, zf);
            if contains(cb, mp) {
                quads.push(Quad::solid(white, cb, CLOSE_HOT, clip));
            }
            self.text_cache.draw(
                &mut quads,
                &mut gfx.renderer,
                &gfx.fonts,
                &gfx.device,
                &gfx.queue,
                "x",
                cb[0] + (cb[2] - cb[0]) * 0.28,
                cb[1] + (cb[3] - cb[1]) * 0.05,
                zf * 0.8,
                TEXT,
                clip,
            );

            // Detach button: pop the node out into its own OS window (highlighted
            // while detached). Drawn as a small "window" icon.
            let db = detach_btn(r, zf);
            let is_det = self.detached.contains_key(&id);
            let panel = if focused { TITLE_FOCUS } else { TITLE };
            if is_det || contains(db, mp) {
                quads.push(Quad::solid(white, db, TITLE_FOCUS, clip));
            }
            let p = (db[2] - db[0]) * 0.24;
            let outer = [db[0] + p, db[1] + p, db[2] - p, db[3] - p];
            quads.push(Quad::solid(white, outer, TEXT, clip));
            let t = (outer[2] - outer[0]) * 0.2;
            let inner = [outer[0] + t, outer[1] + t * 1.9, outer[2] - t, outer[3] - t];
            quads.push(Quad::solid(
                white,
                inner,
                if is_det || contains(db, mp) {
                    TITLE_FOCUS
                } else {
                    panel
                },
                clip,
            ));

            // Run/▶ button for an idle or exited node (start or re-start it).
            if node_idle {
                let rb = run_btn(r, zf);
                if contains(rb, mp) {
                    quads.push(Quad::solid(white, rb, TITLE_FOCUS, clip));
                }
                self.text_cache.draw(
                    &mut quads,
                    &mut gfx.renderer,
                    &gfx.fonts,
                    &gfx.device,
                    &gfx.queue,
                    ">",
                    rb[0] + (rb[2] - rb[0]) * 0.30,
                    rb[1] + (rb[3] - rb[1]) * 0.05,
                    zf * 0.8,
                    TEXT,
                    clip,
                );
            }

            let ca = content_rect(r, zf);
            let ca_clip = intersect(ca, full);
            // A node still compiling its wasm shows a centered loading message.
            if node_loading {
                let msg = "compiling…";
                let lh = gfx.fonts.line_height() as f32 * zf;
                let w = gfx.fonts.measure(msg) as f32 * zf;
                self.text_cache.draw(
                    &mut quads,
                    &mut gfx.renderer,
                    &gfx.fonts,
                    &gfx.device,
                    &gfx.queue,
                    msg,
                    (ca[0] + ca[2]) * 0.5 - w * 0.5,
                    (ca[1] + ca[3]) * 0.5 - lh * 0.5,
                    zf,
                    PORT_COL,
                    ca_clip,
                );
            }
            if self.detached.contains_key(&id) {
                // Popped out into its own OS window: the live content renders
                // there; here we just show a "detached" placeholder in place.
                quads.push(Quad::solid(white, ca, DETACHED_BG, ca_clip));
                let msg = "detached";
                let lh = gfx.fonts.line_height() as f32 * zf;
                let w = gfx.fonts.measure(msg) as f32 * zf;
                self.text_cache.draw(
                    &mut quads,
                    &mut gfx.renderer,
                    &gfx.fonts,
                    &gfx.device,
                    &gfx.queue,
                    msg,
                    (ca[0] + ca[2]) * 0.5 - w * 0.5,
                    (ca[1] + ca[3]) * 0.5 - lh * 0.5,
                    zf,
                    PORT_COL,
                    ca_clip,
                );
            } else if let Some(sid) = node_surface.get(&id).map(|s| s.lock().unwrap().id) {
                if let Some(&(tex, _, _)) = self.views.get(&sid) {
                    quads.push(Quad::tex(
                        ca,
                        [0.0, 0.0, 1.0, 1.0],
                        [1.0, 1.0, 1.0, 1.0],
                        tex,
                        ca_clip,
                    ));
                }
            } else if let Some((cells, cursor)) =
                self.terminals.get(&id).map(|t| (t.cells(), t.cursor()))
            {
                self.draw_term_grid(&mut quads, &mut gfx, &cells, cursor, ca, ca_clip);
            }

            // Idle node: a one-line, editable launch-args bar along the bottom
            // (so it doesn't cover the node's output/scrollback above).
            if node_idle {
                let editing = matches!(&self.editing_args, Some((eid, _)) if *eid == id);
                let bar = args_bar(r, zf);
                let bar_clip = intersect(bar, full);
                quads.push(Quad::solid(
                    white,
                    bar,
                    if editing { TITLE_FOCUS } else { TITLE },
                    bar_clip,
                ));
                let line = match &self.editing_args {
                    Some((eid, s)) if *eid == id => format!("args: {s}_"),
                    _ => format!(
                        "args: {}  (click to edit, > to run)",
                        self.view
                            .node_args
                            .get(&id)
                            .cloned()
                            .unwrap_or_default()
                            .join(" ")
                    ),
                };
                let lh = gfx.fonts.line_height() as f32 * zf;
                let ty = bar[1] + ((bar[3] - bar[1]) - lh) * 0.5;
                self.text_cache.draw(
                    &mut quads,
                    &mut gfx.renderer,
                    &gfx.fonts,
                    &gfx.device,
                    &gfx.queue,
                    &line,
                    bar[0] + PAD * zf,
                    ty,
                    zf,
                    TEXT,
                    bar_clip,
                );
            }

            draw_ports(&mut quads, gfx.renderer.circle, r, zf, mp, full);
        }

        // The wire being dragged out of an output port toward the cursor — same
        // curved arrow as a finished connection.
        if let Some(d) = &self.drag {
            if matches!(d.mode, DragMode::Connect) {
                let from = port_out(self.rect_of(d.id));
                draw_connection(
                    &mut quads,
                    white,
                    from,
                    mp,
                    false,
                    [0.80, 0.85, 1.0, 1.0],
                    zf,
                    full,
                );
            }
        }

        // Corner add/command button (bottom-right): opens the Cmd/Ctrl+K palette.
        let menu_bg = if contains(menu_btn, mp) {
            MENU_HOVER
        } else {
            MENU_BG
        };
        quads.push(Quad::solid(white, menu_btn, menu_bg, full));
        self.text_cache.draw(
            &mut quads,
            &mut gfx.renderer,
            &gfx.fonts,
            &gfx.device,
            &gfx.queue,
            "+ Add  (Cmd+K)",
            menu_btn[0] + PAD,
            menu_btn[1] + (MENU_H - gfx.fonts.line_height() as f32) * 0.5,
            1.0,
            TEXT,
            full,
        );
        // Corner zoom button + its preset menu (bottom-left). Clicking the button
        // opens the menu; clicking a preset jumps the zoom (handy for 100%).
        let lh = gfx.fonts.line_height() as f32;
        if self.zoom_menu_open {
            for (i, &z) in ZOOM_PRESETS.iter().enumerate() {
                let r = zoom_item(i);
                let bg = if contains(r, mp) {
                    MENU_HOVER
                } else if (z - self.cam.zoom).abs() < 0.001 {
                    TITLE_FOCUS
                } else {
                    MENU_BG
                };
                quads.push(Quad::solid(white, r, bg, full));
                self.text_cache.draw(
                    &mut quads,
                    &mut gfx.renderer,
                    &gfx.fonts,
                    &gfx.device,
                    &gfx.queue,
                    &format!("{:.0}%", z * 100.0),
                    r[0] + PAD,
                    r[1] + (MENU_H - lh) * 0.5,
                    1.0,
                    TEXT,
                    full,
                );
            }
        }
        let zoom_bg = if contains(zoom_btn, mp) || self.zoom_menu_open {
            MENU_HOVER
        } else {
            MENU_BG
        };
        quads.push(Quad::solid(white, zoom_btn, zoom_bg, full));
        self.text_cache.draw(
            &mut quads,
            &mut gfx.renderer,
            &gfx.fonts,
            &gfx.device,
            &gfx.queue,
            &format!("{:.0}%", self.cam.zoom * 100.0),
            zoom_btn[0] + PAD,
            zoom_btn[1] + (MENU_H - lh) * 0.5,
            1.0,
            TEXT,
            full,
        );

        // Top workspace-tab bar — only when the document has more than one tab.
        if self.tabs.len() > 1 {
            let (rects, plus) = self.tab_layout(&gfx);
            quads.push(Quad::solid(white, [0.0, 0.0, fb[0], TAB_H], MENU_BG, full));
            for (i, &(id, r)) in rects.iter().enumerate() {
                let bg = if id == self.active_ws {
                    TITLE_FOCUS
                } else if contains(r, mp) {
                    MENU_HOVER
                } else {
                    MENU_BG
                };
                quads.push(Quad::solid(white, r, bg, full));
                let label = format!("{}", i + 1);
                self.text_cache.draw(
                    &mut quads,
                    &mut gfx.renderer,
                    &gfx.fonts,
                    &gfx.device,
                    &gfx.queue,
                    &label,
                    r[0] + PAD,
                    (TAB_H - lh) * 0.5,
                    1.0,
                    TEXT,
                    full,
                );
                // Close box (×) on the right of the tab.
                let cb = tab_close_btn(r);
                if contains(cb, mp) {
                    quads.push(Quad::solid(white, cb, CLOSE_HOT, full));
                }
                let xw = gfx.fonts.measure("x") as f32 * 0.8;
                self.text_cache.draw(
                    &mut quads,
                    &mut gfx.renderer,
                    &gfx.fonts,
                    &gfx.device,
                    &gfx.queue,
                    "x",
                    (cb[0] + cb[2]) * 0.5 - xw * 0.5,
                    (TAB_H - lh * 0.8) * 0.5,
                    0.8,
                    TEXT,
                    full,
                );
            }
            let pbg = if contains(plus, mp) {
                MENU_HOVER
            } else {
                MENU_BG
            };
            quads.push(Quad::solid(white, plus, pbg, full));
            let pw = gfx.fonts.measure("+") as f32;
            self.text_cache.draw(
                &mut quads,
                &mut gfx.renderer,
                &gfx.fonts,
                &gfx.device,
                &gfx.queue,
                "+",
                (plus[0] + plus[2]) * 0.5 - pw * 0.5,
                (TAB_H - lh) * 0.5,
                1.0,
                TEXT,
                full,
            );
        }

        // Command palette (Cmd/Ctrl+K): dim the canvas, then a centred panel with
        // the typed query and the filtered commands (selected row highlighted).
        if self.palette_open {
            quads.push(Quad::solid(white, full, [0.0, 0.0, 0.0, 0.45], full));
            let (px, py, pw, row_h) = Self::palette_layout(fb);
            let filtered = self.palette_filtered();
            let rows = filtered.len().min(PALETTE_MAX);
            let panel = [px, py, px + pw, py + (rows as f32 + 1.0) * row_h];
            quads.push(Quad::solid(white, panel, BORDER_COL, full));
            let inset = [
                panel[0] + 1.0,
                panel[1] + 1.0,
                panel[2] - 1.0,
                panel[3] - 1.0,
            ];
            quads.push(Quad::solid(white, inset, BODY, full));
            // Query row.
            let q = if self.palette_query.is_empty() {
                "Type a command…".to_string()
            } else {
                self.palette_query.clone()
            };
            let q_col = if self.palette_query.is_empty() {
                [0.5, 0.5, 0.56, 1.0]
            } else {
                TEXT
            };
            self.text_cache.draw(
                &mut quads,
                &mut gfx.renderer,
                &gfx.fonts,
                &gfx.device,
                &gfx.queue,
                &q,
                px + PAD,
                py + (row_h - lh) * 0.5,
                1.0,
                q_col,
                full,
            );
            let start = (self.palette_scroll.round() as usize).min(filtered.len());
            for (i, (label, _)) in filtered.iter().skip(start).take(PALETTE_MAX).enumerate() {
                let row = start + i;
                let y0 = py + (i as f32 + 1.0) * row_h;
                let r = [px, y0, px + pw, y0 + row_h];
                let hot = contains(r, mp);
                if row == self.palette_sel || hot {
                    quads.push(Quad::solid(white, r, TITLE_FOCUS, full));
                }
                self.text_cache.draw(
                    &mut quads,
                    &mut gfx.renderer,
                    &gfx.fonts,
                    &gfx.device,
                    &gfx.queue,
                    label,
                    px + PAD,
                    y0 + (row_h - lh) * 0.5,
                    1.0,
                    TEXT,
                    full,
                );
            }
        }

        // ---- render ----
        let frame = match gfx.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f)
            | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            _ => {
                self.prev_lmb = lmb;
                self.gfx = Some(gfx);
                return;
            }
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = gfx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("frame"),
            });
        {
            let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(CLEAR),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            gfx.renderer
                .draw(&gfx.device, &gfx.queue, &mut rpass, fb, &quads);
        }
        gfx.queue.submit([encoder.finish()]);
        frame.present();

        // ---- quit closed nodes ----
        for id in &to_close {
            // Drop the closed node's rendered surface texture (client-owned).
            if let Some(surf) = node_surface.get(id) {
                let sid = surf.lock().unwrap().id;
                if let Some((tex, _, _)) = self.views.remove(&sid) {
                    gfx.renderer.remove_texture(tex);
                }
            }
            // Server: kill the node and drop all document state referencing it.
            self.conn.send(Command::Delete(ResourceRef::Node(*id)));
            // Client-local cleanup.
            self.terminals.remove(id);
            self.detached.remove(id);
            self.z.retain(|x| x != id);
            if matches!(self.editing_args, Some((eid, _)) if eid == *id) {
                self.editing_args = None;
            }
            if self.kbd_focus == Some(*id) {
                self.kbd_focus = None;
            }
        }

        // ---- detached node windows ----
        // Drop windows for nodes that vanished (closed elsewhere), then forward
        // each window's queued input to its node and render its own window.
        self.detached.retain(|id, _| node_by_id.contains_key(id));
        let det_ids: Vec<NodeId> = self.detached.keys().copied().collect();
        for id in det_ids {
            let (mouse, lmb_d, prev_d, keys, term_in) = {
                let det = self.detached.get_mut(&id).unwrap();
                let out = (
                    det.mouse,
                    det.lmb,
                    det.prev_lmb,
                    std::mem::take(&mut det.key_events),
                    std::mem::take(&mut det.term_input),
                );
                det.prev_lmb = det.lmb;
                out
            };
            // Forward the detached window's input straight to the node — the
            // window's size is the surface size, so coordinates map 1:1.
            if let Some(surf) = node_surface.get(&id) {
                let mut s = surf.lock().unwrap();
                let local = PointerEvent {
                    x: mouse[0] as f64,
                    y: mouse[1] as f64,
                };
                s.pointer_move.push_back(local);
                if lmb_d && !prev_d {
                    s.pointer_down.push_back(local);
                }
                if !lmb_d && prev_d {
                    s.pointer_up.push_back(local);
                }
                for (ev, down) in &keys {
                    if *down {
                        s.key_down.push_back(ev.clone());
                    } else {
                        s.key_up.push_back(ev.clone());
                    }
                }
            } else if !term_in.is_empty() {
                if let (Some(node), Some(term)) = (node_by_id.get(&id), self.terminals.get_mut(&id))
                {
                    if term.is_raw() {
                        node.term_io.feed_in(&term_in);
                    } else {
                        term.key_input(&term_in, &node.term_io);
                    }
                }
            }
            self.render_detached(&mut gfx, id, &node_surface);
        }

        self.prev_lmb = lmb;
        self.gfx = Some(gfx);
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.gfx.is_none() {
            match Gfx::new(event_loop) {
                Ok(gfx) => self.gfx = Some(gfx),
                Err(e) => {
                    eprintln!("failed to create window: {e}");
                    event_loop.exit();
                }
            }
        }
    }

    /// Called each loop iteration once events are drained — we render here so it
    /// runs *inside* winit's handler (set for the whole pump). Rendering in the
    /// outer loop instead left a window where the handler was unset and a
    /// quit/close event would log "no handler was set".
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // A palette "Quit" command asks to exit on the next loop.
        if self.request_exit {
            event_loop.exit();
            return;
        }
        if self.gfx.is_some() {
            self.create_pending_detached(event_loop);
            self.frame();
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        // Route events for a detached node's window to that node, not the canvas.
        let is_main = self
            .gfx
            .as_ref()
            .map(|g| g.window.id() == id)
            .unwrap_or(true);
        if !is_main {
            self.detached_window_event(id, event);
            return;
        }
        let scale = self
            .gfx
            .as_ref()
            .map(|g| g.window.scale_factor())
            .unwrap_or(1.0);
        match event {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                if let Some(gfx) = &mut self.gfx {
                    gfx.resize();
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.mouse = [(position.x / scale) as f32, (position.y / scale) as f32];
            }
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } => self.lmb = state == ElementState::Pressed,
            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x, y),
                    MouseScrollDelta::PixelDelta(p) => (p.x as f32 / 50.0, p.y as f32 / 50.0),
                };
                // While the palette is open, the wheel scrolls its list instead
                // of panning the canvas.
                if self.palette_open {
                    let max = Self::palette_max_scroll(self.palette_filtered().len());
                    self.palette_scroll = (self.palette_scroll - dy).clamp(0.0, max);
                    return;
                }
                // Scrolling over a HostPort node adjusts its port (scroll up =
                // higher), rather than panning the canvas.
                if let Some(id) = self.topmost_under(self.mouse) {
                    if self.view.host_ports.contains_key(&id) {
                        let step = if dy > 0.0 {
                            dy.ceil() as i32
                        } else if dy < 0.0 {
                            dy.floor() as i32
                        } else {
                            0
                        };
                        self.conn.send(Command::Update {
                            id,
                            patch: NodePatch {
                                port_delta: Some(step),
                                ..Default::default()
                            },
                        });
                        return;
                    }
                }
                if self.mods.control_key() || self.mods.super_key() {
                    self.zoom_factor *= ZOOM_STEP.powf(dy);
                    self.zoom_focus = self.mouse;
                } else {
                    self.pan_delta[0] += dx * SCROLL_PAN_SPEED;
                    self.pan_delta[1] += dy * SCROLL_PAN_SPEED;
                }
            }
            // Native trackpad pinch (macOS): delta is the incremental
            // magnification; zoom around the cursor.
            WindowEvent::PinchGesture { delta, .. } if delta.is_finite() => {
                self.zoom_factor *= (1.0 + delta as f32).clamp(0.1, 10.0);
                self.zoom_focus = self.mouse;
            }
            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),
            WindowEvent::KeyboardInput { event, .. } => {
                if let PhysicalKey::Code(code) = event.physical_key {
                    let pressed = event.state == ElementState::Pressed;
                    // Cmd/Ctrl+K toggles the command palette.
                    if pressed
                        && !event.repeat
                        && (self.mods.super_key() || self.mods.control_key())
                        && code == KeyCode::KeyK
                    {
                        self.palette_open = !self.palette_open;
                        self.palette_query.clear();
                        self.palette_sel = 0;
                        self.palette_scroll = 0.0;
                        return;
                    }
                    // Cmd/Ctrl+T opens a new workspace tab.
                    if pressed
                        && !event.repeat
                        && (self.mods.super_key() || self.mods.control_key())
                        && code == KeyCode::KeyT
                    {
                        self.new_workspace();
                        return;
                    }
                    // Ctrl+Tab cycles tabs (Shift to go backwards). Not Cmd+Tab:
                    // macOS reserves that for its app switcher, so it never
                    // reaches the app; Ctrl+Tab is free on every platform.
                    if pressed && !event.repeat && self.mods.control_key() && code == KeyCode::Tab {
                        self.cycle_tab(!self.mods.shift_key());
                        return;
                    }
                    // Cmd/Ctrl+W closes the current workspace tab.
                    if pressed
                        && !event.repeat
                        && (self.mods.super_key() || self.mods.control_key())
                        && code == KeyCode::KeyW
                    {
                        self.close_workspace(self.active_ws);
                        return;
                    }
                    // Cmd/Ctrl+D duplicates the focused / hovered node.
                    if pressed
                        && !event.repeat
                        && (self.mods.super_key() || self.mods.control_key())
                        && code == KeyCode::KeyD
                    {
                        self.duplicate_focused();
                        return;
                    }
                    // Cmd/Ctrl+Z undoes the last mutation.
                    if pressed
                        && (self.mods.super_key() || self.mods.control_key())
                        && code == KeyCode::KeyZ
                    {
                        self.conn.send(Command::Undo);
                        return;
                    }
                    // While the palette is open it captures all keystrokes.
                    if self.palette_open {
                        if pressed {
                            self.palette_key(code, event.text.as_deref());
                        }
                        return;
                    }
                    // While editing a node's args, keystrokes edit that text.
                    if self.editing_args.is_some() {
                        if pressed {
                            self.editing_args_key(code, event.text.as_deref());
                        }
                        return;
                    }
                    // Escape quits wk only when nothing is focused; otherwise it
                    // belongs to the focused app/terminal (vim lives on Escape).
                    if code == KeyCode::Escape && pressed && self.kbd_focus.is_none() {
                        el.exit();
                    }
                    // Delete/Backspace removes the selected wire (when no app is
                    // focused, so a focused terminal still gets Backspace).
                    if pressed
                        && self.wire_sel.is_some()
                        && self.kbd_focus.is_none()
                        && matches!(code, KeyCode::Delete | KeyCode::Backspace)
                    {
                        self.del_wire = true;
                    }
                    if pressed {
                        if let Some(bytes) = encode_term_key(code, event.text.as_deref(), self.mods)
                        {
                            self.term_input.extend(bytes);
                        }
                    }
                    self.key_events.push((key_event(code, self.mods), pressed));
                }
            }
            _ => {}
        }
    }
}

/// The single-player front-end: a wgpu window driven by winit. It owns all the
/// view/input state ([`App`]) and forwards mutations to the server as
/// [`Command`]s over its [`ServerHandle`]. See [`wk_protocol::Client`].
pub struct WindowClient;

impl wk_protocol::Client<ServerHandle> for WindowClient {
    fn run(self: Box<Self>, conn: ServerHandle) -> Result<(), String> {
        let mut event_loop = EventLoop::builder().build().map_err(|e| e.to_string())?;
        let mut app = App::new(conn)?;
        loop {
            // Pump (and render, via `about_to_wait`) with the handler set the
            // whole time, blocking up to a frame for events — this paces ~60fps
            // when idle and leaves no window where a macOS event has no handler
            // to run. A quit calls `ActiveEventLoop::exit()`, so the next pump
            // returns Exit.
            if let PumpStatus::Exit(_) = event_loop.pump_app_events(Some(FRAME), &mut app) {
                break;
            }
        }
        // The server owns persistence; the window closing just detaches this
        // client.
        Ok(())
    }
}
