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
use winit::event::{
    DeviceEvent, DeviceId, ElementState, MouseButton, MouseScrollDelta, WindowEvent,
};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use winit::platform::pump_events::{EventLoopExtPumpEvents, PumpStatus};
use winit::window::CursorGrabMode;
use winit::window::{Window, WindowId};

use crate::host_shell::Gfx;
use crate::render2d::{Quad, Renderer, TextureId};
use crate::render3d::{MeshDraw, MeshGpu, Quad3, Renderer3d};
use crate::text::Fonts;
use wk_protocol::{
    BoundaryWire, Command, NodeId, NodeKind, NodePatch, PortDir, PortKind, Resource, ResourceRef,
    ViewMode, Wire,
};
use wk_server::plugin::{
    Key, KeyEvent, PointerButton, PointerEvent, ResizeEvent, ScrollEvent, SharedNode, SharedSurface,
};
use wk_server::runtime::ServerHandle;
use wk_server::scene::RayEvent;
use wk_server::server::{View, FILE_H, FILE_W, NOTE_H, NOTE_W};
use wk_server::terminal::CellView;

mod camera;
use camera::*;
mod camera3d;
use camera3d::*;
mod geometry;
use geometry::*;
mod input;
use input::{encode_term_key, key_event, KeyText};
mod modals;
use modals::*;
mod palette;
use palette::*;
mod ports;
use ports::*;
mod term_raster;
use term_raster::TermRaster;
mod text_cache;
use text_cache::TextCache;

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
/// The 3D view's ground plane.
const GROUND_COL: [f32; 4] = [0.085, 0.085, 0.115, 1.0];
/// World-space height of a panel's floating name label (also its lift above
/// the panel's top edge).
const LABEL_H: f32 = 0.05;
/// The 3D world's light: xyz = direction toward the light, w = ambient floor.
const WORLD_LIGHT: [f32; 4] = [0.35, 0.85, 0.4, 0.45];
/// Connection-port disc radius in the 3D view, world units.
const PORT3D_R: f32 = 0.028;
/// Rasterization size for 3D text: world-space quads are metres tall on
/// screen, so they need a much larger glyph size than the 2D UI's `FONT_PX`.
const FONT3D_PX: f32 = 48.0;
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

enum DragMode {
    Move,
    Resize,
    /// Dragging a connection wire out of one of a node's typed output ports
    /// toward a compatible input port on another node. The whole [`Port`],
    /// not just its kind: an instance can wear two output ports of one kind,
    /// and which one the drag left decides which boundary wire it authors.
    Connect(Port),
}
struct Drag {
    id: NodeId,
    mode: DragMode,
    grab: [f32; 2],
}

const FILE_BG: [f32; 4] = [0.20, 0.17, 0.10, 1.0];
const FILE_BORDER: [f32; 4] = [0.55, 0.45, 0.25, 1.0];
/// BindMount nodes are tinted (blue/grey) to distinguish disk-backed binds
/// from in-memory Volumes.
const HOSTFILE_BG: [f32; 4] = [0.10, 0.14, 0.22, 1.0];
const HOSTFILE_BORDER: [f32; 4] = [0.30, 0.45, 0.65, 1.0];
/// Screen Capture nodes: recording-red chrome (a capability, like Network).
const CAPTURE_BG: [f32; 4] = [0.22, 0.10, 0.12, 1.0];
const CAPTURE_BORDER: [f32; 4] = [0.80, 0.30, 0.35, 1.0];
/// Clipboard nodes: amber chrome. A capability like Capture and Api, and the
/// colour is meant to read as a warning: this node hands whatever the user
/// last copied ANYWHERE to whatever app is wired to it.
const CLIPBOARD_BG: [f32; 4] = [0.22, 0.16, 0.06, 1.0];
const CLIPBOARD_BORDER: [f32; 4] = [0.90, 0.68, 0.25, 1.0];
/// Api nodes: bright-cyan chrome (wk's own client API as a capability).
const API_BG: [f32; 4] = [0.08, 0.16, 0.22, 1.0];
const API_BORDER: [f32; 4] = [0.30, 0.75, 0.90, 1.0];
const MIDI_BG: [f32; 4] = [0.10, 0.20, 0.13, 1.0];
const MIDI_BORDER: [f32; 4] = [0.35, 0.75, 0.45, 1.0];
/// HostService nodes: teal chrome (a host service published into a Network —
/// the reverse of a HostPort).
const HOSTSVC_BG: [f32; 4] = [0.07, 0.19, 0.18, 1.0];
const HOSTSVC_BORDER: [f32; 4] = [0.25, 0.80, 0.70, 1.0];
/// `group` nodes: violet chrome. An instance is a whole workspace standing in
/// one node, so it gets a colour no single capability owns.
const GROUP_BG: [f32; 4] = [0.14, 0.10, 0.22, 1.0];
const GROUP_BORDER: [f32; 4] = [0.62, 0.48, 0.92, 1.0];

/// Note nodes: a warm yellow sticky, dark text, for annotations.
const NOTE_BG: [f32; 4] = [0.93, 0.86, 0.42, 1.0];
const NOTE_BORDER: [f32; 4] = [0.72, 0.62, 0.18, 1.0];
const NOTE_TEXT: [f32; 4] = [0.14, 0.12, 0.05, 1.0];
/// A slightly deeper yellow drag strip along a note's top edge.
const NOTE_GRIP: [f32; 4] = [0.86, 0.76, 0.30, 1.0];
/// Height (canvas units) of a note's top drag strip; below it, the body edits.
const NOTE_GRAB: f32 = 16.0;
/// Muted grey for overlay text (a node's "compiling…" / "detached" message).
/// Ports are coloured by their [`PortKind`], not this.
const MUTED_TEXT: [f32; 4] = [0.70, 0.72, 0.80, 1.0];
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

/// What varies between the small "widget" nodes (file / HostPort / Network /
/// uplink) when drawing their shared chrome — see [`App::draw_widget`].
struct WidgetChrome<'a> {
    /// The node id, so shared chrome can draw its typed ports.
    id: NodeId,
    r: [f32; 4],
    border: [f32; 4],
    bg: [f32; 4],
    title: &'a str,
    title_col: [f32; 4],
    status: &'a str,
    status_col: [f32; 4],
    /// Text scale of the status line relative to the title.
    status_scale: f32,
    /// Draw the copy-ticket button (uplink nodes — see [`ticket_btn`]).
    copy_ticket: bool,
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
    // Right/middle buttons: nothing in a detached window uses them for canvas
    // interactions, so they go to the node with their button identity.
    rmb: bool,
    prev_rmb: bool,
    mmb: bool,
    prev_mmb: bool,
    /// Wheel events (pointer position + line deltas) queued for the node; only
    /// delivered if its surface subscribed to scroll, dropped otherwise.
    scroll: Vec<ScrollEvent>,
    key_events: Vec<(KeyEvent, bool)>,
    term_input: Vec<u8>,
}

/// A wk:scene entity's GPU-side cache: its meshes, the textures created for
/// them (to free on removal), and a local-space bounding sphere for ray tests.
struct EntityGpu {
    meshes: Vec<MeshGpu>,
    owned_tex: Vec<TextureId>,
    bound: ([f32; 3], f32),
}

/// Frames a freshly created workspace stays "pending" — long enough for the
/// server thread to apply the Create and publish it (a frame or two in
/// practice), short enough that a refused create doesn't strand the client on
/// a tab that will never exist.
const PENDING_WS_FRAMES: u8 = 60;

/// Which tab the client should be on once the server's view arrives, and what
/// remains of a pending create.
///
/// A workspace this client just minted isn't in `tabs` yet — the server
/// applies commands on its own thread — so a plain "is `active` still there?"
/// check would drag the view back to the first tab and undo the switch. That
/// is the Cmd+T race: the palette set `active` late enough in the frame to
/// usually survive it, a keystroke didn't. Hold the switch while the create is
/// pending; give up if it never lands, or if something else changed tabs.
fn reconcile_active_ws(
    tabs: &[NodeId],
    active: NodeId,
    pending: Option<(NodeId, u8)>,
) -> (NodeId, Option<(NodeId, u8)>) {
    let pending = match pending {
        // It landed: the switch stands on its own from here.
        Some((id, _)) if tabs.contains(&id) => None,
        // Still waiting, and still where we put the user: keep holding.
        Some((id, left)) if id == active && left > 0 => Some((id, left - 1)),
        // Gave up, or the user moved on.
        _ => None,
    };
    if !tabs.contains(&active) && pending.is_none() {
        return (tabs.first().copied().unwrap_or(active), None);
    }
    (active, pending)
}

/// Whether a node's flat 2D panel is drawn in the 3D world. Hidden only when
/// the document asks for it (`panel3d #false`) *and* the node has a `wk:scene`
/// body to stand in its place — a node whose guest died (or whose objects a
/// token muted) would otherwise vanish from the world with nothing to click.
fn shows_panel3d(hidden: &HashSet<NodeId>, bodied: &HashSet<NodeId>, id: NodeId) -> bool {
    !hidden.contains(&id) || !bodied.contains(&id)
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
    /// The 3D workspace view (toggled from the palette): its fly camera, the
    /// canvas point mapped to straight-ahead, and the lazily-built renderer.
    mode_3d: bool,
    /// The `wk view` sequence this client has already applied.
    view_mode_seq: u64,
    /// A workspace this client minted and switched to, not yet in the server's
    /// view: `(id, frames left to wait)`. See [`reconcile_active_ws`].
    pending_ws: Option<(NodeId, u8)>,
    /// Fly mode: free 6-DoF flight. Off = walk (grounded at eye height).
    fly3d: bool,
    cam3d: Camera3d,
    cyl_anchor: [f32; 2],
    renderer3d: Option<Renderer3d>,
    /// Terminal grids rasterized as textures for 3D panels, keyed by node.
    term_views: HashMap<NodeId, (TextureId, u32, u32)>,
    term_raster: TermRaster,
    /// High-res font + its own string cache for world-space (3D) text.
    fonts3d: Fonts,
    text_cache3d: TextCache,
    /// GPU meshes for wk:scene entities, keyed by the content hash of their
    /// GLB (loaded on first sight; empty = failed, don't retry). Hashing the
    /// geometry rather than the entity id means a world node that restarts —
    /// or ten nodes showing the same model — upload once between them.
    entity_meshes: HashMap<u64, EntityGpu>,
    /// A panel move in progress: the node, the grab distance along the cursor
    /// ray (scroll pushes/pulls it), and the world-space offset from the grab
    /// point to the panel centre.
    drag3d: Option<(NodeId, f32, [f32; 3])>,
    /// A wire drag in progress in 3D, from a node's typed out-port.
    wire3d: Option<(NodeId, Port)>,
    /// Whether the cursor is currently grabbed+hidden for look mode.
    look_captured: bool,
    /// Last known viewport size in screen px (updated each frame), so newly
    /// added nodes can be placed at the centre of the current view.
    viewport: [f32; 2],
    /// This client's stacking order (which node draws/hit-tests on top).
    z: Vec<NodeId>,
    kbd_focus: Option<NodeId>,
    /// When editing an idle node's args: its id and the in-progress text.
    editing_args: Option<(NodeId, String)>,
    /// When editing a note's text: its id and the in-progress text.
    editing_note: Option<(NodeId, String)>,
    /// When editing a bind wire's mount path (via its wire label): the wire's
    /// `(source, app)` pair and the in-progress text.
    editing_mount: Option<((NodeId, NodeId), String)>,
    /// Frame counter for throttling canvas readback (Screen Capture nodes).
    capture_tick: u64,
    /// When the clipboard pump last read the host clipboard. `arboard` has no
    /// change notification, so "did it change?" is a poll and a diff — and on
    /// X11 each poll is a synchronous round trip to whichever process owns the
    /// selection, so it is throttled rather than run every pass.
    clip_polled: Option<std::time::Instant>,
    /// When inspecting a node's virtual filesystem (a modal overlay).
    inspect: Option<Inspector>,
    /// Background-fetched listings/previews for inspector paths that cross a
    /// provider mount (the render thread never blocks on a guest).
    browse: ProviderBrowse,
    /// OS file drag-and-drop: whether a drag is hovering the window, and how
    /// many files of the current drop have landed (staggers their nodes).
    drop_hovering: bool,
    drop_stagger: u32,
    /// Palette "Go headless": drop every window but keep pumping the event
    /// loop — the server (and every node) runs on, with no client drawn.
    /// Leaving the loop instead would beachball on macOS: a parked main
    /// thread stops servicing the app's runloop, and even the window's own
    /// close needs events pumped.
    request_headless: bool,
    headless: bool,
    /// The host's Ctrl-C flag: checked each pass; exits gracefully.
    interrupt: Arc<std::sync::atomic::AtomicBool>,
    /// When viewing a node's output log (a modal overlay). Mutually exclusive
    /// with `inspect` — one panel at a time.
    logs: Option<LogView>,
    /// Largest useful log scroll, recomputed each frame the log panel draws (it
    /// needs the font/width to wrap), so the wheel handler can clamp against it.
    log_max_scroll: f32,
    /// System clipboard, for pasting into the args/ticket field and the
    /// command palette. `None` if the platform has no clipboard.
    clipboard: Option<arboard::Clipboard>,
    /// The uplink whose ticket was just copied, and when — the widget shows a
    /// short confirmation, because a clipboard write is otherwise invisible.
    ticket_copied: Option<(NodeId, std::time::Instant)>,
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
    /// Right button + accumulated drag (mouse look) and scroll travel, and the
    /// currently held keys (WASD flight). Look mode only reads `rmb` in the 3D
    /// view; on the 2D canvas the right button is free, so its edges
    /// (`prev_rmb`) route right-clicks to the surface under the cursor.
    rmb: bool,
    prev_rmb: bool,
    /// Middle button: no canvas interaction uses it anywhere, so it always
    /// routes to the surface under the cursor.
    mmb: bool,
    prev_mmb: bool,
    look_delta: [f32; 2],
    fly_scroll: f32,
    keys_down: HashSet<KeyCode>,
    /// The character each held key produced, so its release can carry the same
    /// text its press did. Shared by the main and detached windows: winit routes
    /// a press and its release to whichever window had focus, and a code is a
    /// code either way.
    key_text: KeyText,
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
    fn new(
        conn: ServerHandle,
        interrupt: Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<Self, String> {
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
            mode_3d: false,
            view_mode_seq: 0,
            pending_ws: None,
            fly3d: false,
            cam3d: Camera3d::new(),
            cyl_anchor: [0.0, 0.0],
            renderer3d: None,
            term_views: HashMap::new(),
            term_raster: TermRaster::default(),
            fonts3d: Fonts::new(FONT3D_PX)?,
            text_cache3d: TextCache::default(),
            entity_meshes: HashMap::new(),
            drag3d: None,
            wire3d: None,
            look_captured: false,
            viewport: [1280.0, 800.0],
            z: Vec::new(),
            kbd_focus: None,
            editing_args: None,
            editing_note: None,
            editing_mount: None,
            capture_tick: 0,
            clip_polled: None,
            inspect: None,
            browse: ProviderBrowse::new(),
            drop_hovering: false,
            drop_stagger: 0,
            request_headless: false,
            headless: false,
            interrupt,
            logs: None,
            log_max_scroll: 0.0,
            clipboard: arboard::Clipboard::new().ok(),
            ticket_copied: None,
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
            rmb: false,
            prev_rmb: false,
            mmb: false,
            prev_mmb: false,
            look_delta: [0.0, 0.0],
            fly_scroll: 0.0,
            keys_down: HashSet::new(),
            key_text: KeyText::default(),
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

    /// The typed connection ports a node exposes, derived from its kind. An app
    /// can mount volumes (bind in), send/receive MIDI, serve to a HostPort, join
    /// a Network, and receive capture frames; the small widget nodes each expose
    /// the single port their kind participates in.
    /// A port's `slot` is its index here, stamped on in one pass at the end so
    /// the hit-test, the drag and the wire lookup all name the same dot even
    /// when a node wears two ports of one kind and direction.
    fn node_ports(&self, id: NodeId) -> Vec<Port> {
        let mut ports = self.node_ports_unslotted(id);
        for (i, p) in ports.iter_mut().enumerate() {
            p.slot = i;
        }
        ports
    }

    fn node_ports_unslotted(&self, id: NodeId) -> Vec<Port> {
        use PortDir::{In, Out};
        use PortKind::{Api, Bind, Capture, Clipboard, Midi, Net, Serve};
        let v = &self.view;
        let one = |kind, dir| vec![port(kind, dir)];
        if v.notes.contains_key(&id) {
            Vec::new()
        } else if v.file_nodes.contains_key(&id) {
            one(Bind, Out) // a volume/bind mounts into apps
        } else if v.host_ports.contains_key(&id) {
            one(Serve, In) // apps serve to a HostPort
        } else if v.net_nodes.contains(&id) {
            one(Net, In) // members join a Network
        } else if v.routers.contains(&id)
            || v.uplinks.contains_key(&id)
            || v.host_services.contains_key(&id)
        {
            // A router joins Networks the way an uplink does — and unlike any
            // other member, it may hold several at once.
            one(Net, Out)
        } else if v.capture_feeds.contains_key(&id) {
            one(Capture, Out) // a Capture node grants apps
        } else if v.clipboard_boards.contains_key(&id) {
            one(Clipboard, Out) // a Clipboard node grants apps
        } else if v.api_nodes.contains(&id) {
            one(Api, Out) // an Api node grants apps
        } else if let Some(p) = v.boundary_ports.get(&id) {
            // A boundary port stands in for the node on the far side, so its
            // dot faces inward: an *in*-port feeds this workspace's nodes (Out),
            // an *out*-port is fed by them (In).
            one(p.kind, if p.dir == PortDir::In { Out } else { In })
        } else if let Some(g) = v.groups.get(&id) {
            // An instance wears the definition's own boundary ports, facing
            // outward: what the definition calls an in-port is an input here.
            g.ports.iter().map(|p| port(p.kind, p.dir)).collect()
        } else if v.midi_ins.contains_key(&id) {
            one(Midi, Out) // a hardware MIDI input drives apps
        } else {
            // An app node. Every app can mount a volume (Bind); the other ports
            // appear only on apps whose component actually imports the matching
            // capability: MIDI (`wk:midi`), Capture (`wk:capture`), Network
            // (`wasi:sockets`), Serve (a `wasi:http` server, or a networked
            // node whose port a HostPort can forward), and a Bind *output* on
            // fs providers (`wk:fs/provider` — the app's served tree mounts
            // into other apps like a volume). While the component is still
            // compiling these all read false, so the ports appear once it's
            // ready.
            let node = v.app_node(id);
            let midi = node.as_ref().is_some_and(|n| n.imports_midi());
            let net = node.as_ref().is_some_and(|n| n.imports_net());
            let capture = node.as_ref().is_some_and(|n| n.imports_capture());
            let clipboard = node.as_ref().is_some_and(|n| n.imports_clipboard());
            let provides_fs = v.fs_providers.contains(&id);
            // The API is reached over the app's virtual network, so the port
            // appears on any app that can speak to a network at all.
            let api = node.as_ref().is_some_and(|n| n.imports_net());
            let serve = node
                .as_ref()
                .is_some_and(|n| n.http_path().is_some() || n.imports_net());
            let mut ports = vec![port(Bind, In)];
            if midi {
                ports.push(port(Midi, In));
            }
            if capture {
                ports.push(port(Capture, In));
            }
            if clipboard {
                ports.push(port(Clipboard, In));
            }
            if api {
                ports.push(port(Api, In));
            }
            if provides_fs {
                ports.push(port(Bind, Out));
            }
            if midi {
                ports.push(port(Midi, Out));
            }
            if serve {
                ports.push(port(Serve, Out));
            }
            if net {
                ports.push(port(Net, Out));
            }
            ports
        }
    }

    /// The screen anchor of a node's port of a given kind + direction, falling
    /// back to the edge centre if the node has no such port.
    fn port_pos(&self, id: NodeId, kind: PortKind, dir: PortDir) -> [f32; 2] {
        let r = self.rect_of(id);
        let ports = self.node_ports(id);
        let anchors = port_anchors(r, &ports);
        ports
            .iter()
            .zip(&anchors)
            .find(|(p, _)| p.kind == kind && p.dir == dir)
            .map(|(_, &a)| a)
            .unwrap_or_else(|| {
                let x = if dir == PortDir::Out { r[2] } else { r[0] };
                [x, (r[1] + r[3]) * 0.5]
            })
    }

    /// The screen anchor of one particular port of a node (by slot), if the
    /// node is placed and has that many ports.
    fn port_slot_pos(&self, id: NodeId, slot: usize) -> Option<[f32; 2]> {
        if !self.view.win_pos.contains_key(&id) {
            return None;
        }
        let ports = self.node_ports(id);
        port_anchors(self.rect_of(id), &ports).get(slot).copied()
    }

    /// The topmost node + port of direction `dir` whose dot is under `mp`. Ports
    /// sit on the node edge (half the disc outside the rect), so they're
    /// hit-tested against the whole disc.
    fn port_under(&self, mp: [f32; 2], zf: f32, dir: PortDir) -> Option<(NodeId, Port)> {
        for &id in self.z.iter().rev() {
            let r = self.rect_of(id);
            let ports = self.node_ports(id);
            let anchors = port_anchors(r, &ports);
            for (p, a) in ports.iter().zip(&anchors) {
                if p.dir == dir && near(mp, *a, PORT_R * zf + 3.0) {
                    return Some((id, *p));
                }
            }
        }
        None
    }

    /// Finish a wire drag from one node's port to another's: connect, or
    /// disconnect if that pair is already joined (the client decides create vs
    /// delete; the server's create never disconnects). An instance at either
    /// end makes it a boundary wire instead — one gesture, one command, one
    /// undo step, whatever the expansion then makes of it.
    fn finish_wire_drag(&mut self, src: (NodeId, Port), dst: (NodeId, Port)) {
        if let Some(bw) = boundary_wire_for(&self.view, src, dst) {
            self.conn.send(if boundary_authored(&self.view, &bw) {
                Command::Delete(ResourceRef::Boundary(bw))
            } else {
                Command::Create(Resource::Boundary(bw))
            });
            return;
        }
        match self.wire_between(src.0, dst.0) {
            Some(w) => self.conn.send(Command::Delete(ResourceRef::Wire(w))),
            None => self
                .conn
                .send(Command::Create(Resource::Wire { a: src.0, b: dst.0 })),
        }
    }

    /// Draw a node's typed connection ports as coloured dots — inputs down the
    /// left edge, outputs down the right — each in its wire kind's colour, lit
    /// when hovered (inputs a touch dimmer, as drop targets).
    fn draw_typed_ports(
        &self,
        quads: &mut Vec<Quad>,
        circle: TextureId,
        id: NodeId,
        zf: f32,
        mp: [f32; 2],
        clip: [f32; 4],
    ) {
        let r = self.rect_of(id);
        let ports = self.node_ports(id);
        let anchors = port_anchors(r, &ports);
        let pr = PORT_R * zf;
        for (p, a) in ports.iter().zip(anchors) {
            let (col, rad) = if near(mp, a, pr + 3.0) {
                (PORT_HOT, pr * 1.4)
            } else if p.dir == PortDir::In {
                let c = port_color(p.kind);
                ([c[0] * 0.7, c[1] * 0.7, c[2] * 0.7, 1.0], pr)
            } else {
                (port_color(p.kind), pr)
            };
            quads.push(Quad::disc(circle, a, rad, col, clip));
        }
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

    /// Current clipboard text (single line — tickets/args paste as one line),
    /// or `None` if there's no clipboard or it holds no text.
    fn clipboard_text(&mut self) -> Option<String> {
        let text = self.clipboard.as_mut()?.get_text().ok()?;
        Some(text.trim().to_string())
    }

    /// Move text between the HOST's system clipboard and every Clipboard
    /// node's board. The only place in wk that touches a platform clipboard.
    ///
    /// Out first, then in. A guest's `set` lands in the board's `outbox` and
    /// is drained here; on success the board's own `text`/`seq` are updated to
    /// match, so the guest's own copy does NOT read back to it a moment later
    /// as "somebody else changed the clipboard" (which is what would break
    /// QWkClipboard's `ownsMode`, and with it Qt's "is this selection still
    /// mine?" bookkeeping).
    ///
    /// The read half is a poll and a diff because `arboard` has no change
    /// notification at all — there is no `changeCount`, no watcher — so a
    /// `wasi:io/poll` pollable in the WIT would be a promise the host cannot
    /// keep. Throttled to `CLIP_POLL`: on X11 every `get_text` is a
    /// synchronous round trip to whichever process owns the selection, and a
    /// wk instance polling that at frame rate is a bad neighbour. The
    /// compromise costs a paste that lands within the throttle window a stale
    /// read; reading on demand from inside the guest's `get` is not available,
    /// because `arboard::Clipboard` belongs to this thread.
    ///
    /// Every `arboard` failure is a no-op, never a panic and never a
    /// guest-visible trap — the same `.ok()`-swallowing the ticket-copy path
    /// above uses. An empty clipboard, an image where text was asked for, and
    /// another X11 client holding the selection are all normal.
    fn pump_clipboard(&mut self) {
        // Only boards with a wired app, the way the capture pump only reads
        // the canvas back for a Capture node someone is actually using. A
        // Clipboard node sitting unwired on the canvas should not make wk poll
        // the window system forever, and nothing could read the result anyway.
        let present = self.clipboard.is_some();
        let boards: Vec<_> = self
            .view
            .clipboard_boards
            .iter()
            .filter(|(id, _)| self.view.clipboard_links.iter().any(|(_, c)| c == *id))
            .map(|(_, b)| b.clone())
            .collect();
        if boards.is_empty() {
            return;
        }

        // Out: drain what guests asked to copy.
        for board in &boards {
            let outgoing = {
                let mut b = board.lock().unwrap();
                b.present = present;
                b.outbox.take()
            };
            let Some(text) = outgoing else { continue };
            let ok = self
                .clipboard
                .as_mut()
                .is_some_and(|c| c.set_text(text.clone()).is_ok());
            if ok {
                // Publish it as the current value ourselves. Without this the
                // next poll sees "the clipboard changed" and bumps seq, and
                // the node that just copied concludes it lost ownership.
                let mut b = board.lock().unwrap();
                if b.text != text {
                    b.seq += 1;
                    b.text = text;
                }
                // The poll below would only re-read what we just wrote.
                self.clip_polled = Some(std::time::Instant::now());
            }
        }

        // In: has anything (this node, another app, another machine's
        // synced clipboard) changed what the host holds?
        let due = self
            .clip_polled
            .is_none_or(|t| t.elapsed() >= Self::CLIP_POLL);
        if !due {
            return;
        }
        self.clip_polled = Some(std::time::Instant::now());
        let Some(text) = self.clipboard.as_mut().and_then(|c| c.get_text().ok()) else {
            return; // empty, an image, occupied, or no clipboard at all
        };
        for board in &boards {
            let mut b = board.lock().unwrap();
            // seq moves only on an actual change, so a guest can tell "still
            // mine" from "someone else copied". seq == 0 means never observed,
            // so the first read always publishes — including an empty string,
            // which is a real clipboard state.
            if b.seq == 0 || b.text != text {
                b.seq += 1;
                b.text.clone_from(&text);
            }
        }
    }

    /// How often the host clipboard is re-read. Fast enough that a Cmd+V in a
    /// node usually sees what the user copied a moment ago; slow enough not to
    /// hammer an X11 selection owner. A `Focused(true)` on the window forces a
    /// read regardless, which covers the common "copy elsewhere, switch to wk,
    /// paste" sequence.
    const CLIP_POLL: std::time::Duration = std::time::Duration::from_millis(250);

    /// What a Clipboard node currently GRANTS, as the label its widget shows.
    ///
    /// This is half the security argument for having a Clipboard node at all:
    /// a user must be able to see at a glance which app can read what they
    /// copied. It reports the app's live permits (which the server's
    /// `sync_clipboard` refreshes from its capability token every tick), not
    /// the mere existence of the wire — so `wk token attenuate` shows up here
    /// as "write only" or "denied" within a tick.
    fn clipboard_grant(&self, clip: NodeId) -> (String, [f32; 4]) {
        let Some(&(app, _)) = self.view.clipboard_links.iter().find(|&&(_, c)| c == clip) else {
            return (
                "wire an app to grant it".to_string(),
                [0.55, 0.7, 0.72, 1.0],
            );
        };
        let Some(node) = self.view.app_node(app) else {
            return ("wired".to_string(), [0.8, 0.65, 0.5, 1.0]);
        };
        use std::sync::atomic::Ordering::Relaxed;
        let (r, w) = (node.clip_read.load(Relaxed), node.clip_write.load(Relaxed));
        let present = self
            .view
            .clipboard_boards
            .get(&clip)
            .is_some_and(|b| b.lock().unwrap().present);
        if !present {
            // No `arboard` on this machine (or wk is running headless on a
            // box with no display server). Say so rather than implying a
            // working bridge — otherwise "paste does nothing" has no cause.
            return ("no host clipboard".to_string(), [0.8, 0.5, 0.45, 1.0]);
        }
        match (r, w) {
            (true, true) => ("● read + write".to_string(), [0.95, 0.75, 0.35, 1.0]),
            (true, false) => ("● read only".to_string(), [0.9, 0.7, 0.4, 1.0]),
            (false, true) => ("● write only".to_string(), [0.75, 0.7, 0.5, 1.0]),
            // Wired but the token says no: the wire is drawn, the grant is not.
            (false, false) => ("denied by token".to_string(), [0.8, 0.5, 0.45, 1.0]),
        }
    }

    /// Put uplink `id`'s own ticket on the clipboard, and flag the widget to
    /// confirm it. A failed/absent clipboard leaves no confirmation, which is
    /// the honest signal that nothing was copied.
    fn copy_ticket(&mut self, id: NodeId) {
        let Some(ticket) = self.view.uplinks.get(&id).map(|u| u.ticket.clone()) else {
            return;
        };
        let ok = self
            .clipboard
            .as_mut()
            .is_some_and(|c| c.set_text(ticket).is_ok());
        if ok {
            self.ticket_copied = Some((id, std::time::Instant::now()));
        }
    }

    /// How long the "ticket copied" confirmation stays on an uplink widget.
    const COPIED_FOR: std::time::Duration = std::time::Duration::from_secs(2);

    /// Whether uplink `id` should currently show its copy confirmation.
    fn just_copied(&self, id: NodeId) -> bool {
        matches!(self.ticket_copied, Some((cid, at)) if cid == id && at.elapsed() < Self::COPIED_FOR)
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
                            rmb: false,
                            prev_rmb: false,
                            mmb: false,
                            prev_mmb: false,
                            scroll: Vec::new(),
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
            WindowEvent::MouseInput { state, button, .. } => {
                if let Some(det) = self.detached.get_mut(&node_id) {
                    let held = state == ElementState::Pressed;
                    match button {
                        MouseButton::Left => det.lmb = held,
                        MouseButton::Right => det.rmb = held,
                        MouseButton::Middle => det.mmb = held,
                        _ => {}
                    }
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                if let Some(det) = self.detached.get_mut(&node_id) {
                    let (dx, dy) = match delta {
                        MouseScrollDelta::LineDelta(x, y) => (x, y),
                        MouseScrollDelta::PixelDelta(p) => (p.x as f32 / 50.0, p.y as f32 / 50.0),
                    };
                    det.scroll.push(ScrollEvent {
                        x: det.mouse[0] as f64,
                        y: det.mouse[1] as f64,
                        delta_x: dx as f64,
                        delta_y: dy as f64,
                    });
                }
            }
            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),
            WindowEvent::KeyboardInput { event, .. } => {
                if let PhysicalKey::Code(code) = event.physical_key {
                    let pressed = event.state == ElementState::Pressed;
                    let mods = self.mods;
                    // Resolved before borrowing the detached window — and
                    // unconditionally, even for a window that has since been
                    // dropped: the memo tracks which keys are down, so it must
                    // see every event this window gets (see `KeyText`).
                    let text = self.key_text.resolve(code, event.text.as_ref(), pressed);
                    if let Some(det) = self.detached.get_mut(&node_id) {
                        if pressed {
                            if let Some(bytes) = encode_term_key(code, event.text.as_deref(), mods)
                            {
                                det.term_input.extend(bytes);
                            }
                        }
                        det.key_events
                            .push((key_event(code, text, mods, event.repeat), pressed));
                    }
                }
            }
            _ => {}
        }
    }

    /// The screen-space endpoints of a wire (both nodes must still be placed):
    /// the source's typed **output** port to the target's typed **input** port
    /// of the wire's kind, so each wire leaves and lands on its matching dot.
    /// `(src, dst)` is the flow direction, which for Capture is the reverse of
    /// the variant's `(app, cap)` tuple (frames flow cap → app).
    fn wire_endpoints(&self, w: Wire) -> Option<([f32; 2], [f32; 2])> {
        let (src, dst, kind) = match w {
            Wire::Bind(vol, app) => (vol, app, PortKind::Bind),
            Wire::Midi(s, d) => (s, d, PortKind::Midi),
            Wire::Serve(http, hp) => (http, hp, PortKind::Serve),
            Wire::Net(app, net) => (app, net, PortKind::Net),
            Wire::Capture(app, cap) => (cap, app, PortKind::Capture),
            Wire::Clipboard(app, clip) => (clip, app, PortKind::Clipboard),
            Wire::Api(app, api) => (api, app, PortKind::Api),
        };
        if self.view.win_pos.contains_key(&src) && self.view.win_pos.contains_key(&dst) {
            Some((
                self.port_pos(src, kind, PortDir::Out),
                self.port_pos(dst, kind, PortDir::In),
            ))
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
            .map(|&(f, n)| Wire::Bind(f, n))
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
            .or_else(|| {
                s.capture_links
                    .iter()
                    .find(|&&(x, y)| pair(x, y))
                    .map(|&(x, y)| Wire::Capture(x, y))
            })
            .or_else(|| {
                s.clipboard_links
                    .iter()
                    .find(|&&(x, y)| pair(x, y))
                    .map(|&(x, y)| Wire::Clipboard(x, y))
            })
            .or_else(|| {
                s.api_links
                    .iter()
                    .find(|&&(x, y)| pair(x, y))
                    .map(|&(x, y)| Wire::Api(x, y))
            })
    }

    /// The connection wire nearest to `mp` within the pick radius, if any. Picks
    /// against the drawn curve, not the straight chord, so clicks land on the arc.
    fn wire_at(&self, mp: [f32; 2], zf: f32) -> Option<Wire> {
        let s = &self.view;
        let all = s
            .connections
            .iter()
            .map(|&(f, a)| Wire::Bind(f, a))
            .chain(s.midi_links.iter().map(|&(s, d)| Wire::Midi(s, d)))
            .chain(s.capture_links.iter().map(|&(a, c)| Wire::Capture(a, c)))
            .chain(
                s.clipboard_links
                    .iter()
                    .map(|&(a, c)| Wire::Clipboard(a, c)),
            )
            .chain(s.api_links.iter().map(|&(a, n)| Wire::Api(a, n)))
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

    /// The selected bind wire's mount-path label text — the in-progress edit
    /// (with a caret) when that wire's path is being edited.
    fn mount_label_text(&self, src: NodeId, app: NodeId) -> String {
        match &self.editing_mount {
            Some((w, s)) if *w == (src, app) => format!("mount: {s}\u{2588}"),
            _ => format!("mount: {}", mount_path(&self.view, src, app)),
        }
    }

    /// The screen rect of a bind wire's mount-path label, centred on the
    /// wire's midpoint. Clicking it edits the path.
    fn mount_label_rect(&self, fonts: &Fonts, src: NodeId, app: NodeId) -> Option<[f32; 4]> {
        let (a, b) = self.wire_endpoints(Wire::Bind(src, app))?;
        let zf = self.cam.zoom;
        let arrow = connection_arrow(a, b, zf);
        // The quadratic bezier at t = 0.5.
        let mid = [
            0.25 * (arrow.start.0 + arrow.end.0) + 0.5 * arrow.control.0,
            0.25 * (arrow.start.1 + arrow.end.1) + 0.5 * arrow.control.1,
        ];
        let w = fonts.measure(&self.mount_label_text(src, app)) as f32 + 2.0 * PAD;
        let h = fonts.line_height() as f32 + PAD;
        Some([
            mid[0] - w * 0.5,
            mid[1] - h * 0.5,
            mid[0] + w * 0.5,
            mid[1] + h * 0.5,
        ])
    }

    /// Handle a key press while editing a bind wire's mount path. Enter commits
    /// (a blank path resets to the default); Escape cancels.
    fn editing_mount_key(&mut self, code: KeyCode, text: Option<&str>) {
        match code {
            KeyCode::Escape => self.editing_mount = None,
            KeyCode::Enter | KeyCode::NumpadEnter => {
                if let Some(((src, app), path)) = self.editing_mount.take() {
                    self.conn.send(Command::SetMount {
                        volume: src,
                        app,
                        path: path.trim().to_string(),
                    });
                }
            }
            KeyCode::Backspace => {
                if let Some((_, s)) = self.editing_mount.as_mut() {
                    s.pop();
                }
            }
            _ => {
                if let (Some((_, s)), Some(t)) = (self.editing_mount.as_mut(), text) {
                    for ch in t.chars().filter(|c| !c.is_control()) {
                        s.push(ch);
                    }
                }
            }
        }
    }

    /// All command-palette entries (label + action) for the current state.
    fn palette_all(&self) -> Vec<PaletteRow> {
        let d = |s: &str| Some(s.to_string());
        let mut v: Vec<PaletteRow> = self
            .view
            .available
            .iter()
            .enumerate()
            .map(|(i, dep)| {
                PaletteRow::new(
                    format!("Add {}", dep.name),
                    dep.description.clone(),
                    PaletteCmd::Launch(i),
                )
            })
            .collect();
        v.push(PaletteRow::new(
            "Add Virtual File",
            d("an in-memory shared file"),
            PaletteCmd::AddVolume,
        ));
        v.push(PaletteRow::new(
            "Add Host File",
            d("a disk-backed file"),
            PaletteCmd::AddBindMount,
        ));
        v.push(PaletteRow::new(
            "Add Port",
            d("publish a node on a localhost port"),
            PaletteCmd::AddPort,
        ));
        v.push(PaletteRow::new(
            "Add Network",
            d("an isolated virtual network"),
            PaletteCmd::AddNetwork,
        ));
        v.push(PaletteRow::new(
            "Add Gateway",
            d("a network whose members get host access"),
            PaletteCmd::AddGateway,
        ));
        v.push(PaletteRow::new(
            "Add Router",
            d("bridge two networks — their members reach each other, each node stays on its own"),
            PaletteCmd::AddRouter,
        ));
        v.push(PaletteRow::new(
            "Add Iroh Uplink",
            d("extend a network to a remote peer"),
            PaletteCmd::AddIroh,
        ));
        v.push(PaletteRow::new(
            "Add Veilid Uplink",
            d("extend a network over onion-routed Veilid"),
            PaletteCmd::AddVeilid,
        ));
        v.push(PaletteRow::new(
            "Add Note",
            d("a yellow sticky note for annotations"),
            PaletteCmd::AddNote,
        ));
        v.push(PaletteRow::new(
            "Add Screen Capture",
            d("grants wired apps the captured canvas (frames)"),
            PaletteCmd::AddCapture,
        ));
        v.push(PaletteRow::new(
            "Add Clipboard",
            d("grants wired apps the HOST's system clipboard (read/write)"),
            PaletteCmd::AddClipboard,
        ));
        v.push(PaletteRow::new(
            "Add API",
            d("wired apps can drive wk over their virtual network"),
            PaletteCmd::AddApi,
        ));
        v.push(PaletteRow::new(
            "Add MIDI In",
            d("a hardware MIDI input device — wire it to a synth or the piano"),
            PaletteCmd::AddMidiIn,
        ));
        v.push(PaletteRow::new(
            "Add Host Service",
            d("publish a host TCP service into a Network (the reverse of a HostPort)"),
            PaletteCmd::AddHostService,
        ));
        v.push(PaletteRow::new(
            "New Workspace  (Cmd+T)",
            None,
            PaletteCmd::NewWorkspace,
        ));
        if self.tabs.len() > 1 {
            v.push(PaletteRow::new(
                "Close Workspace  (Cmd+W)",
                None,
                PaletteCmd::CloseWorkspace,
            ));
        }
        for &z in &ZOOM_PRESETS {
            v.push(PaletteRow::new(
                format!("Zoom {:.0}%", z * 100.0),
                None,
                PaletteCmd::Zoom(z),
            ));
        }
        if self.mode_3d {
            v.push(PaletteRow::new(
                if self.fly3d { "Walk Mode" } else { "Fly Mode" },
                d(if self.fly3d {
                    "back on the ground (also F)"
                } else {
                    "free 6-DoF flight, Q/E down/up (also F)"
                }),
                PaletteCmd::ToggleFly,
            ));
            // The focused node — clicking a wk:scene object focuses its node,
            // the same gesture that focuses a panel — can drop its flat panel
            // and stand as the 3D object alone. Offered only for a node that
            // has such an object, since it is what would be left.
            if let Some(id) = self.kbd_focus.filter(|id| self.scene_nodes().contains(id)) {
                let hidden = self.view.hidden_panel3d.contains(&id);
                let who = self.node_label(id);
                v.push(PaletteRow::new(
                    if hidden { "Show Panel" } else { "Hide Panel" },
                    d(&if hidden {
                        format!("{who} — draw its 2D card again, beside its 3D object")
                    } else {
                        format!("{who} — leave only its 3D object, remembered in the file")
                    }),
                    PaletteCmd::TogglePanel3d(id),
                ));
            }
        }
        v.push(PaletteRow::new(
            if self.mode_3d { "2D View" } else { "3D View" },
            d(if self.mode_3d {
                "back to the flat canvas (also Esc)"
            } else {
                "walk the workspace — WASD/QE move, right-drag look, Esc exits"
            }),
            PaletteCmd::View3d,
        ));
        // Jump to any node in the active workspace (searchable by name).
        for &id in &self.view.node_ids {
            v.push(PaletteRow::new(
                format!("Go to {}", self.node_label(id)),
                None,
                PaletteCmd::GoTo(id),
            ));
        }
        v.push(PaletteRow::new(
            "Go headless (close UI, keep nodes running)",
            None,
            PaletteCmd::Headless,
        ));
        v.push(PaletteRow::new("Quit wk", None, PaletteCmd::Quit));
        v
    }

    /// A short human label for a node (for palette search / "go to").
    fn node_label(&self, id: NodeId) -> String {
        if let Some(label) = self.view.node_labels.get(&id) {
            // What the server says to call it: a chosen name, else the type.
            // Not the node's `name`, which for an unnamed node is a generated
            // handle — useful for dialling it, useless on a card.
            label.clone()
        } else if let Some(n) = self.view.app_node(id) {
            n.name.clone()
        } else if let Some(f) = self.view.file_nodes.get(&id) {
            f.name.clone()
        } else if let Some(&p) = self.view.host_ports.get(&id) {
            format!("port :{p}")
        } else if self.view.gateways.contains(&id) {
            "gateway".into()
        } else if self.view.net_nodes.contains(&id) {
            "network".into()
        } else if self.view.routers.contains(&id) {
            "router".into()
        } else if let Some(u) = self.view.uplinks.get(&id) {
            u.kind.label().to_lowercase()
        } else if self.view.midi_ins.contains_key(&id) {
            "midi in".into()
        } else if let Some(svc) = self.view.host_services.get(&id) {
            svc.name.clone()
        } else if let Some(p) = self.view.boundary_ports.get(&id) {
            format!("{} {}", port_label(p.dir), p.name)
        } else if let Some(g) = self.view.groups.get(&id) {
            g.definition.clone()
        } else {
            "node".into()
        }
    }

    /// Nodes that own at least one live `wk:scene` entity — the ones with a 3D
    /// body of their own, and so the only ones whose flat panel can be hidden.
    fn scene_nodes(&self) -> HashSet<NodeId> {
        self.view
            .scene_entities
            .iter()
            .map(|e| e.lock().unwrap().node_id)
            .collect()
    }

    /// Palette entries matching the current query, best match first (see
    /// [`palette_rank`]). The sort is stable, so rows that match equally well
    /// keep the deliberate order [`Self::palette_all`] built them in.
    fn palette_filtered(&self) -> Vec<PaletteRow> {
        let mut rows: Vec<(u8, PaletteRow)> = self
            .palette_all()
            .into_iter()
            .filter_map(|r| {
                palette_rank(&r.label, r.desc.as_deref(), &self.palette_query).map(|s| (s, r))
            })
            .collect();
        rows.sort_by_key(|(score, _)| *score);
        rows.into_iter().map(|(_, r)| r).collect()
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

    /// Send the in-progress note edit to the server and stop editing.
    fn commit_note(&mut self) {
        if let Some((id, text)) = self.editing_note.take() {
            self.conn.send(Command::Update {
                id,
                patch: NodePatch {
                    text: Some(text),
                    ..Default::default()
                },
            });
        }
    }

    /// Handle a key press while editing a note's text. Enter inserts a newline
    /// (notes are multi-line); Escape commits and stops editing.
    fn editing_note_key(&mut self, code: KeyCode, text: Option<&str>) {
        match code {
            KeyCode::Escape => self.commit_note(),
            KeyCode::Enter | KeyCode::NumpadEnter => {
                if let Some((_, s)) = self.editing_note.as_mut() {
                    s.push('\n');
                }
            }
            KeyCode::Backspace => {
                if let Some((_, s)) = self.editing_note.as_mut() {
                    s.pop();
                }
            }
            _ => {
                if let (Some((_, s)), Some(t)) = (self.editing_note.as_mut(), text) {
                    for ch in t.chars().filter(|c| *c == '\n' || !c.is_control()) {
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
                self.palette_run = self.palette_filtered().get(self.palette_sel).map(|r| r.cmd);
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
                    if self.mode_3d {
                        // Aim the fly camera at the node's panel on the cylinder.
                        let theta = (c[0] - self.cyl_anchor[0]) / (PX_PER_M * CYL_R);
                        let target = [
                            CYL_R * theta.sin(),
                            -(c[1] - self.cyl_anchor[1]) / PX_PER_M,
                            -CYL_R * theta.cos(),
                        ];
                        let v = sub3(target, self.cam3d.pos);
                        self.cam3d.yaw = v[0].atan2(-v[2]);
                        let hd = (v[0] * v[0] + v[2] * v[2]).sqrt().max(1e-6);
                        self.cam3d.pitch = (v[1] / hd).atan().clamp(-1.5, 1.5);
                    } else {
                        let z = self.cam.zoom;
                        self.pan_target = [fb[0] * 0.5 - c[0] * z, fb[1] * 0.5 - c[1] * z];
                    }
                }
            }
            PaletteCmd::AddVolume => {
                let pos = self.next_file_pos();
                self.conn.send(Command::Create(Resource::Node {
                    kind: NodeKind::Volume,
                    pos,
                    ws,
                }));
            }
            PaletteCmd::AddBindMount => {
                let pos = self.next_file_pos();
                self.conn.send(Command::Create(Resource::Node {
                    kind: NodeKind::BindMount,
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
            PaletteCmd::AddRouter => {
                let pos = self.view_center([FILE_W, FILE_H], self.view.routers.len());
                self.conn.send(Command::Create(Resource::Node {
                    kind: NodeKind::Router,
                    pos,
                    ws: self.active_ws,
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
            PaletteCmd::AddIroh => {
                let pos = self.view_center([FILE_W, FILE_H], self.view.uplinks.len());
                self.conn.send(Command::Create(Resource::Node {
                    kind: NodeKind::Iroh,
                    pos,
                    ws,
                }));
            }
            PaletteCmd::AddVeilid => {
                let pos = self.view_center([FILE_W, FILE_H], self.view.uplinks.len());
                self.conn.send(Command::Create(Resource::Node {
                    kind: NodeKind::Veilid,
                    pos,
                    ws,
                }));
            }
            PaletteCmd::AddNote => {
                let pos = self.view_center([NOTE_W, NOTE_H], self.view.notes.len());
                self.conn.send(Command::Create(Resource::Node {
                    kind: NodeKind::Note,
                    pos,
                    ws,
                }));
            }
            PaletteCmd::AddCapture => {
                let pos = self.view_center([FILE_W, FILE_H], self.view.capture_feeds.len());
                self.conn.send(Command::Create(Resource::Node {
                    kind: NodeKind::Capture,
                    pos,
                    ws,
                }));
            }
            PaletteCmd::AddClipboard => {
                let pos = self.view_center([FILE_W, FILE_H], self.view.clipboard_boards.len());
                self.conn.send(Command::Create(Resource::Node {
                    kind: NodeKind::Clipboard,
                    pos,
                    ws,
                }));
            }
            PaletteCmd::AddApi => {
                let pos = self.view_center([FILE_W, FILE_H], self.view.api_nodes.len());
                self.conn.send(Command::Create(Resource::Node {
                    kind: NodeKind::Api,
                    pos,
                    ws,
                }));
            }
            PaletteCmd::AddMidiIn => {
                let pos = self.view_center([FILE_W, FILE_H], self.view.midi_ins.len());
                self.conn.send(Command::Create(Resource::Node {
                    kind: NodeKind::MidiIn,
                    pos,
                    ws,
                }));
            }
            PaletteCmd::AddHostService => {
                let pos = self.view_center([FILE_W, FILE_H], self.view.host_services.len());
                self.conn.send(Command::Create(Resource::Node {
                    kind: NodeKind::HostService,
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
            PaletteCmd::ToggleFly => self.fly3d = !self.fly3d,
            PaletteCmd::TogglePanel3d(id) => {
                let show = self.view.hidden_panel3d.contains(&id);
                self.conn.send(Command::Update {
                    id,
                    patch: NodePatch {
                        panel3d: Some(show),
                        ..Default::default()
                    },
                });
            }
            PaletteCmd::View3d => self.set_mode_3d(!self.mode_3d, fb),
            PaletteCmd::Quit => self.request_exit = true,
            PaletteCmd::Headless => self.request_headless = true,
        }
    }

    /// Enter or leave the 3D world. Shared by the palette's "3D View" and the
    /// `wk view` command, so both land in exactly the same place.
    fn set_mode_3d(&mut self, want: bool, fb: [f32; 2]) {
        if want == self.mode_3d {
            return;
        }
        if want {
            // Anchor the cylinder on the centre of the current view so the
            // nodes you were looking at appear straight ahead.
            self.cyl_anchor = self.cam.to_canvas([fb[0] * 0.5, fb[1] * 0.5]);
            self.cam3d = Camera3d::new();
        }
        self.mode_3d = want;
    }

    /// Apply a `wk view` request once, when its sequence advances past the one
    /// this client last saw. A client that attaches later inherits the
    /// sequence without being yanked by a request made before it existed.
    fn apply_view_request(&mut self, (seq, mode): (u64, ViewMode), fb: [f32; 2]) {
        if seq == self.view_mode_seq {
            return;
        }
        self.view_mode_seq = seq;
        self.set_mode_3d(mode.wants_3d(self.mode_3d), fb);
    }

    /// Create a new workspace tab and switch this client's view to it. The client
    /// mints the id so it can switch locally; the server just records the tab.
    fn new_workspace(&mut self) {
        let id = NodeId::new();
        self.conn.send(Command::Create(Resource::Workspace { id }));
        self.active_ws = id;
        // The server hasn't applied the Create yet; hold this switch until its
        // view catches up (see `reconcile_active_ws`).
        self.pending_ws = Some((id, PENDING_WS_FRAMES));
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

    /// What a tab is called: the workspace's name, else its 1-based position.
    /// Drawing and hit-testing both go through this — a tab is only as wide as
    /// its label, so the two must agree on the label or clicks land on the
    /// wrong tab.
    fn tab_label(&self, i: usize, id: NodeId) -> String {
        match self.view.workspace_names.get(&id).map(|n| n.trim()) {
            Some(n) if !n.is_empty() => n.to_string(),
            _ => format!("{}", i + 1),
        }
    }

    /// The tab rectangles (one per workspace, in order) and the trailing "+"
    /// button rect. Tabs are labelled by [`Self::tab_label`] and carry a close
    /// box (see [`tab_close_btn`]).
    fn tab_layout(&self, gfx: &Gfx) -> (Vec<(NodeId, [f32; 4])>, [f32; 4]) {
        let mut rects = Vec::with_capacity(self.tabs.len());
        let mut x = 0.0;
        for (i, &id) in self.tabs.iter().enumerate() {
            let label = gfx.fonts.measure(&self.tab_label(i, id)) as f32;
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

    /// The inspector's interactive regions for the current node's listing of
    /// `n_entries` rows — see [`inspect_geom`] (this just supplies the modal's
    /// current `dir`/`scroll`).
    fn inspect_regions(&self, fb: [f32; 2], n_entries: usize) -> InspectRegions {
        let insp = self.inspect.as_ref();
        let has_up = insp.is_some_and(|i| !i.dir.is_empty());
        let scroll = insp.map_or(0, |i| i.scroll.floor().max(0.0) as usize);
        inspect_geom(fb, n_entries, has_up, scroll)
    }

    /// Draw the shared chrome of a small "widget" node (file / HostPort /
    /// Network / uplink): the bordered box, a title and a status line, the
    /// hover-lit close button, and the wiring ports. Kind-specific extras (a
    /// HostPort's −/+ buttons) draw on top afterwards.
    #[allow(clippy::too_many_arguments)]
    fn draw_widget(
        &mut self,
        quads: &mut Vec<Quad>,
        gfx: &mut Gfx,
        white: TextureId,
        zf: f32,
        mp: [f32; 2],
        clip: [f32; 4],
        full: [f32; 4],
        w: WidgetChrome,
    ) {
        quads.push(Quad::solid(white, w.r, w.border, clip));
        let body = [
            w.r[0] + BORDER * zf,
            w.r[1] + BORDER * zf,
            w.r[2] - BORDER * zf,
            w.r[3] - BORDER * zf,
        ];
        quads.push(Quad::solid(white, body, w.bg, clip));
        let lh = gfx.fonts.line_height() as f32;
        self.text_cache.draw(
            quads,
            &mut gfx.renderer,
            &gfx.fonts,
            &gfx.device,
            &gfx.queue,
            w.title,
            w.r[0] + PAD * zf,
            w.r[1] + PAD * zf,
            zf,
            w.title_col,
            clip,
        );
        self.text_cache.draw(
            quads,
            &mut gfx.renderer,
            &gfx.fonts,
            &gfx.device,
            &gfx.queue,
            w.status,
            w.r[0] + PAD * zf,
            w.r[1] + (PAD + lh) * zf,
            zf * w.status_scale,
            w.status_col,
            clip,
        );
        let cb = close_btn(w.r, zf);
        if contains(cb, mp) {
            quads.push(Quad::solid(white, cb, CLOSE_HOT, clip));
        }
        self.text_cache.draw(
            quads,
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
        if w.copy_ticket {
            let tb = ticket_btn(w.r, zf);
            if contains(tb, mp) {
                quads.push(Quad::solid(white, tb, CLOSE_HOT, clip));
            }
            self.text_cache.draw(
                quads,
                &mut gfx.renderer,
                &gfx.fonts,
                &gfx.device,
                &gfx.queue,
                "c",
                tb[0] + (tb[2] - tb[0]) * 0.28,
                tb[1] + (tb[3] - tb[1]) * 0.05,
                zf * 0.8,
                TEXT,
                clip,
            );
        }
        self.draw_typed_ports(quads, gfx.renderer.circle, w.id, zf, mp, full);
    }

    /// Draw a yellow sticky note: word-wrapped text on a warm panel, with a close
    /// button but no ports (a note wires to nothing). `editing` appends a caret.
    #[allow(clippy::too_many_arguments)]
    fn draw_note(
        &mut self,
        quads: &mut Vec<Quad>,
        gfx: &mut Gfx,
        white: TextureId,
        zf: f32,
        r: [f32; 4],
        clip: [f32; 4],
        mp: [f32; 2],
        text: &str,
        editing: bool,
    ) {
        quads.push(Quad::solid(white, r, NOTE_BORDER, clip));
        let body = [
            r[0] + BORDER * zf,
            r[1] + BORDER * zf,
            r[2] - BORDER * zf,
            r[3] - BORDER * zf,
        ];
        quads.push(Quad::solid(white, body, NOTE_BG, clip));
        // A deeper-yellow top strip: the drag handle (the body below edits).
        let grip = [body[0], body[1], body[2], r[1] + NOTE_GRAB * zf];
        quads.push(Quad::solid(white, grip, NOTE_GRIP, clip));

        let lh = gfx.fonts.line_height() as f32;
        let pad = PAD * zf;
        let max_units = (((r[2] - r[0]) - 2.0 * pad) / zf).max(1.0);
        let shown = if editing {
            format!("{text}\u{2588}")
        } else {
            text.to_string()
        };
        // Word-wrap (honoring explicit newlines) up front, so measuring the font
        // doesn't overlap the mutable renderer borrow used to draw.
        let mut lines: Vec<String> = Vec::new();
        for para in shown.split('\n') {
            let mut cur = String::new();
            for word in para.split(' ') {
                let cand = if cur.is_empty() {
                    word.to_string()
                } else {
                    format!("{cur} {word}")
                };
                if cur.is_empty() || (gfx.fonts.measure(&cand) as f32) <= max_units {
                    cur = cand;
                } else {
                    lines.push(std::mem::take(&mut cur));
                    cur = word.to_string();
                }
            }
            lines.push(cur);
        }
        let mut y = r[1] + NOTE_GRAB * zf + pad * 0.5;
        for line in &lines {
            if y + lh * zf > r[3] {
                break; // clip overflow to the note's height
            }
            self.text_cache.draw(
                quads,
                &mut gfx.renderer,
                &gfx.fonts,
                &gfx.device,
                &gfx.queue,
                line,
                r[0] + pad,
                y,
                zf,
                NOTE_TEXT,
                clip,
            );
            y += lh * zf;
        }

        let cb = close_btn(r, zf);
        if contains(cb, mp) {
            quads.push(Quad::solid(white, cb, CLOSE_HOT, clip));
        }
        self.text_cache.draw(
            quads,
            &mut gfx.renderer,
            &gfx.fonts,
            &gfx.device,
            &gfx.queue,
            "x",
            cb[0] + (cb[2] - cb[0]) * 0.28,
            cb[1] + (cb[3] - cb[1]) * 0.05,
            zf * 0.8,
            NOTE_TEXT,
            clip,
        );
    }

    /// Draw a terminal cell grid, scaled uniformly to fit `area`, clipped to
    /// `clip`. Shared by the in-workspace node body and its detached window.
    #[allow(clippy::too_many_arguments)]
    fn draw_term_grid(
        &mut self,
        quads: &mut Vec<Quad>,
        gfx: &mut Gfx,
        cells: &[CellView],
        cursor: Option<(usize, usize)>,
        area: [f32; 4],
        clip: [f32; 4],
        grid: (u16, u16),
    ) {
        let white = gfx.renderer.white;
        let cols = grid.0 as f32;
        let rows = grid.1 as f32;
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
        } else if self.terminals.contains_key(&id) {
            // Detached window: no camera zoom, so the grid is the window size
            // divided by the cell metrics.
            let bw = (gfx.fonts.measure("M") as f32).max(1.0);
            let bh = (gfx.fonts.line_height() as f32).max(1.0);
            let cols = ((fb[0] / bw).floor() as i32).clamp(1, 500) as u16;
            let rows = ((fb[1] / bh).floor() as i32).clamp(1, 300) as u16;
            if let Some(t) = self.terminals.get_mut(&id) {
                t.resize(cols, rows);
            }
            let (cells, cursor) = self
                .terminals
                .get(&id)
                .map(|t| (t.cells(), t.cursor()))
                .unwrap();
            self.draw_term_grid(&mut quads, gfx, &cells, cursor, full, full, (cols, rows));
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
    /// Advance every surface one compositor frame: resize it to its render
    /// target (in-workspace content or detached window), take its pixels into
    /// its GPU texture, and signal the guest to produce the next frame.
    fn drive_surfaces(&mut self, gfx: &mut Gfx, surfaces: &[SharedSurface]) {
        for shared in surfaces {
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
    }

    /// One frame of the 3D view: fly the camera, drive surfaces as usual, lay
    /// the workspace out as panels on a cylinder around the origin, route the
    /// mouse (drag panels back into canvas positions, or pointer input into
    /// surfaces), and render with depth.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    fn frame_3d(
        &mut self,
        gfx: &mut Gfx,
        surfaces: &[SharedSurface],
        node_surface: &HashMap<NodeId, SharedSurface>,
        node_by_id: &HashMap<NodeId, SharedNode>,
        fb: [f32; 2],
        mp: [f32; 2],
        lmb: bool,
        down_edge: bool,
        up_edge: bool,
    ) {
        // Fly camera. Holding the right button is "look mode": mouse look plus
        // WASD/QE flight (Shift sprints). Released, the keyboard belongs to
        // the active (focused) node instead. The wheel always flies the gaze.
        self.cam3d
            .look(std::mem::replace(&mut self.look_delta, [0.0, 0.0]));
        let fly = std::mem::take(&mut self.fly_scroll);
        // While a panel is grabbed the wheel pushes/pulls it instead.
        let cam_fly = if self.drag3d.is_some() { 0.0 } else { fly };
        let no_keys = HashSet::new();
        let fly_keys = if self.rmb && !self.palette_open {
            &self.keys_down
        } else {
            &no_keys
        };
        self.cam3d.advance(
            fly_keys,
            self.mods.shift_key(),
            cam_fly,
            !self.fly3d,
            1.0 / 60.0,
        );
        // Keyboard → the active node, exactly like the 2D canvas: a graphical
        // node's surface gets wasi-gfx key events, a terminal node the encoded
        // bytes. (window_event only queues these when a node is focused and
        // the camera isn't in look mode.)
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
            } else if !self.term_input.is_empty() && !self.view.attached.contains(&fid) {
                if let (Some(term), Some(node)) =
                    (self.terminals.get_mut(&fid), node_by_id.get(&fid))
                {
                    if term.is_raw() {
                        node.term_io.feed_in(&self.term_input);
                    } else {
                        term.key_input(&self.term_input, &node.term_io);
                    }
                }
            }
        }
        self.key_events.clear();
        self.term_input.clear();

        self.drive_surfaces(gfx, surfaces);

        // The 3D renderer must exist before anything uploads meshes — the
        // wk:scene entity loader runs below and caches its result, so a
        // missing renderer on the first frame would cache entities as empty.
        if self.renderer3d.is_none() {
            self.renderer3d = Some(Renderer3d::new(
                &gfx.device,
                gfx.surface_desc.format,
                gfx.renderer.texture_layout(),
            ));
        }

        // Drop terminal textures whose node vanished.
        let stale_terms: Vec<NodeId> = self
            .term_views
            .keys()
            .copied()
            .filter(|id| !self.terminals.contains_key(id))
            .collect();
        for id in stale_terms {
            if let Some((tex, _, _)) = self.term_views.remove(&id) {
                gfx.renderer.remove_texture(tex);
            }
        }

        // Wrap the canvas onto a cylinder: canvas x becomes arc length, canvas
        // y height, so the 2D arrangement survives the trip into space. The
        // anchor (view centre at toggle time) faces the camera's start pose.
        enum Body {
            Tex(TextureId),
            Fill([f32; 4]),
        }
        struct Line {
            tex: TextureId,
            w: f32,
            h: f32,
            color: [f32; 4],
        }
        struct Panel {
            id: NodeId,
            center: [f32; 3],
            right: [f32; 3],
            normal: [f32; 3],
            w: f32,
            h: f32,
            border: [f32; 4],
            body: Body,
            /// Content pixel size when the panel routes pointer input.
            surface_px: Option<(u32, u32)>,
            /// Floating name label above the panel (app nodes).
            label: Option<Line>,
            /// Text drawn on the card, stacked from the top (widgets, notes).
            lines: Vec<Line>,
            /// Card text alignment: notes read left-aligned, widgets centred.
            left_text: bool,
            /// Whether a plain click anywhere on the card starts a move drag
            /// (widgets/terminals/notes); surface panels drag by their label
            /// or Cmd/Ctrl+drag so clicks stay app input.
            body_drag: bool,
            /// Typed connection ports in panel-local coords (port, lu, lv):
            /// inputs on the left edge, outputs on the right, like 2D.
            ports: Vec<(Port, f32, f32)>,
        }
        /// Widget-card chrome: border, bg, title, title colour, status,
        /// status colour — the 3D analogue of `WidgetChrome`.
        struct Chrome([f32; 4], [f32; 4], String, [f32; 4], String, [f32; 4]);
        /// A string as a world-space text line `lh` tall, shrunk to `max_w`.
        /// Rasterized with the high-res 3D font so metre-wide text stays crisp.
        fn mk_line(
            tc: &mut TextCache,
            fonts: &Fonts,
            gfx: &mut Gfx,
            s: &str,
            color: [f32; 4],
            lh: f32,
            max_w: f32,
        ) -> Option<Line> {
            let (tex, tw, th) = tc.get(&mut gfx.renderer, fonts, &gfx.device, &gfx.queue, s)?;
            let mut h = lh;
            let mut w = tw / th * lh;
            if w > max_w {
                h *= max_w / w;
                w = max_w;
            }
            Some(Line { tex, w, h, color })
        }

        let ids = self.view.node_ids.clone();
        let bodied = self.scene_nodes();
        let mut panels: Vec<Panel> = Vec::new();
        // Each node's world pose (origin + yaw), the parent frame for any
        // wk:scene entities it owns.
        let mut poses: HashMap<NodeId, ([f32; 3], f32)> = HashMap::new();
        for id in ids {
            let (Some(&pos), Some(&size)) =
                (self.view.win_pos.get(&id), self.view.win_size.get(&id))
            else {
                continue;
            };
            // A free 3D pose ([x, y, z, yaw], world-absolute) wins; nodes
            // without one sit on the layout cylinder, each workspace's cluster
            // rotated into its own sector (the active tab straight ahead).
            let (center, right, normal) = if let Some(&[x, y, z, yaw]) = self.view.pos3d.get(&id) {
                let (sy, cy) = yaw.sin_cos();
                let normal = [sy, 0.0, cy];
                ([x, y, z], [cy, 0.0, -sy], normal)
            } else {
                let n_ws = self.tabs.len().max(1) as f32;
                let ws_idx = self
                    .view
                    .node_ws
                    .get(&id)
                    .and_then(|w| self.tabs.iter().position(|t| t == w))
                    .unwrap_or(0) as f32;
                let active_idx = self
                    .tabs
                    .iter()
                    .position(|t| *t == self.active_ws)
                    .unwrap_or(0) as f32;
                let sector = (ws_idx - active_idx) * (std::f32::consts::TAU / n_ws);
                let cx = pos[0] + size[0] * 0.5 - self.cyl_anchor[0];
                let cy = pos[1] + size[1] * 0.5 - self.cyl_anchor[1];
                let theta = cx / (PX_PER_M * CYL_R) + sector;
                let (s, c) = theta.sin_cos();
                (
                    [CYL_R * s, -cy / PX_PER_M, -CYL_R * c],
                    [c, 0.0, s],
                    [-s, 0.0, c],
                )
            };
            poses.insert(id, (center, normal[0].atan2(normal[2])));
            // A stripped node renders as its wk:scene objects alone — the pose
            // above still places them, and pressing one focuses the node
            // exactly as its panel would have.
            if !shows_panel3d(&self.view.hidden_panel3d, &bodied, id) {
                continue;
            }
            let cw = size[0] / PX_PER_M;
            let max_w = cw * 0.92;

            // A graphical node: its live surface at the surface's aspect.
            if let Some(view) = node_surface.get(&id).and_then(|surf| {
                let sid = surf.lock().unwrap().id;
                self.views.get(&sid).copied()
            }) {
                let (tex, pw, ph) = view;
                let label = mk_line(
                    &mut self.text_cache3d,
                    &self.fonts3d,
                    gfx,
                    &self
                        .view
                        .app_node(id)
                        .map(|n| n.name.clone())
                        .unwrap_or_default(),
                    TEXT,
                    LABEL_H,
                    2.0,
                );
                panels.push(Panel {
                    id,
                    center,
                    right,
                    normal,
                    w: pw as f32 / PX_PER_M,
                    h: ph as f32 / PX_PER_M,
                    border: BORDER_COL,
                    body: Body::Tex(tex),
                    surface_px: Some((pw, ph)),
                    label,
                    lines: Vec::new(),
                    left_text: false,
                    body_drag: false,
                    ports: Vec::new(),
                });
                continue;
            }

            // A terminal node: its grid rasterized into a texture. The grid is
            // sized from the node's canvas rect exactly like the 2D view at
            // zoom 1, so toggling views doesn't reflow the terminal.
            if self.terminals.contains_key(&id) {
                let bw = (gfx.fonts.measure("M") as f32).max(1.0);
                let bh = (gfx.fonts.line_height() as f32).max(1.0);
                let cols = (((size[0] - 2.0 * BORDER) / bw).floor() as i32).clamp(1, 500) as u16;
                let rows =
                    (((size[1] - TITLE_H - BORDER) / bh).floor() as i32).clamp(1, 300) as u16;
                let (cells, cursor) = {
                    let t = self.terminals.get_mut(&id).unwrap();
                    t.resize(cols, rows);
                    (t.cells(), t.cursor())
                };
                let (tw, th, px) =
                    self.term_raster
                        .rasterize(&gfx.fonts, &cells, cursor, (cols, rows));
                let tex = match self.term_views.get(&id) {
                    Some(&(tex, w, h)) if w == tw && h == th => {
                        gfx.renderer.update_texture(&gfx.queue, tex, tw, th, &px);
                        tex
                    }
                    old => {
                        if let Some(&(tex, _, _)) = old {
                            gfx.renderer.remove_texture(tex);
                        }
                        let tex = gfx
                            .renderer
                            .create_texture(&gfx.device, &gfx.queue, tw, th, &px);
                        self.term_views.insert(id, (tex, tw, th));
                        tex
                    }
                };
                let name = self.node_label(id);
                let label = mk_line(
                    &mut self.text_cache3d,
                    &self.fonts3d,
                    gfx,
                    &name,
                    TEXT,
                    LABEL_H,
                    2.0,
                );
                panels.push(Panel {
                    id,
                    center,
                    right,
                    normal,
                    w: cw,
                    h: cw * th as f32 / tw as f32,
                    border: BORDER_COL,
                    body: Body::Tex(tex),
                    surface_px: None,
                    label,
                    lines: Vec::new(),
                    left_text: false,
                    body_drag: true,
                    ports: Vec::new(),
                });
                continue;
            }

            // A note: the yellow annotation card with its text.
            if let Some(text) = self.view.notes.get(&id).cloned() {
                let lines = text
                    .split('\n')
                    .filter_map(|l| {
                        mk_line(
                            &mut self.text_cache3d,
                            &self.fonts3d,
                            gfx,
                            l,
                            NOTE_TEXT,
                            0.042,
                            max_w,
                        )
                    })
                    .collect();
                panels.push(Panel {
                    id,
                    center,
                    right,
                    normal,
                    w: cw,
                    h: size[1] / PX_PER_M,
                    border: NOTE_BORDER,
                    body: Body::Fill(NOTE_BG),
                    surface_px: None,
                    label: None,
                    lines,
                    left_text: true,
                    body_drag: true,
                    ports: Vec::new(),
                });
                continue;
            }

            // The widget nodes (files/ports/networks/…): kind colours plus the
            // same title/status the 2D chrome shows.
            let widget: Option<Chrome> = if let Some(file) = self.view.file_nodes.get(&id) {
                let (border, bg, status, status_col) = if file.host_mapped {
                    let kind = if file.is_dir { "dir" } else { "file" };
                    (
                        HOSTFILE_BORDER,
                        HOSTFILE_BG,
                        format!("{} B · {kind}", file.size),
                        [0.55, 0.68, 0.85, 1.0],
                    )
                } else {
                    let tail = if file.persist { " · persist" } else { "" };
                    (
                        FILE_BORDER,
                        FILE_BG,
                        format!("{} B{tail}", file.size),
                        [0.65, 0.6, 0.5, 1.0],
                    )
                };
                Some(Chrome(
                    border,
                    bg,
                    file.name.clone(),
                    TEXT,
                    status,
                    status_col,
                ))
            } else if let Some(&port) = self.view.host_ports.get(&id) {
                let serving = self.view.serves.values().any(|&hp| hp == id);
                let (state, state_col) = if self.port_conflicts.contains(&port) {
                    ("port in use".to_string(), WARN)
                } else if serving {
                    ("live ●".to_string(), [0.4, 0.85, 0.5, 1.0])
                } else {
                    ("idle".to_string(), [0.55, 0.7, 0.72, 1.0])
                };
                Some(Chrome(
                    HOSTPORT_BORDER,
                    HOSTPORT_BG,
                    state,
                    state_col,
                    format!(":{port}"),
                    TEXT,
                ))
            } else if self.view.net_nodes.contains(&id) {
                let members = self
                    .view
                    .net_links
                    .iter()
                    .filter(|&&(_, n)| n == id)
                    .count();
                let is_gw = self.view.gateways.contains(&id);
                Some(Chrome(
                    NET_BORDER,
                    NET_BG,
                    if is_gw { "Gateway" } else { "Network" }.to_string(),
                    TEXT,
                    if is_gw {
                        format!("host • {members}")
                    } else {
                        format!("{members} node(s)")
                    },
                    [0.72, 0.62, 0.9, 1.0],
                ))
            } else if self.view.routers.contains(&id) {
                let bridged = self
                    .view
                    .net_links
                    .iter()
                    .filter(|&&(m, _)| m == id)
                    .count();
                Some(Chrome(
                    NET_BORDER,
                    NET_BG,
                    "Router".to_string(),
                    TEXT,
                    if bridged < 2 {
                        // Its own status line says why it is doing nothing:
                        // one network is not a bridge.
                        format!("{bridged}/2 networks")
                    } else {
                        format!("bridging {bridged}")
                    },
                    if bridged < 2 {
                        [0.55, 0.7, 0.72, 1.0]
                    } else {
                        [0.72, 0.62, 0.9, 1.0]
                    },
                ))
            } else if let Some(meta) = self.view.uplinks.get(&id) {
                let (status, status_col) = if meta.peers > 0 {
                    (format!("● {} peer(s)", meta.peers), [0.4, 0.85, 0.5, 1.0])
                } else if self.view.node_args.get(&id).is_some_and(|a| !a.is_empty()) {
                    ("dialing…".to_string(), [0.72, 0.62, 0.9, 1.0])
                } else {
                    ("add peer in 2D".to_string(), [0.55, 0.7, 0.72, 1.0])
                };
                Some(Chrome(
                    NET_BORDER,
                    NET_BG,
                    meta.kind.label().to_string(),
                    TEXT,
                    status,
                    status_col,
                ))
            } else if let Some(feed) = self.view.capture_feeds.get(&id) {
                let wired = self.view.capture_links.iter().any(|&(_, c)| c == id);
                let live = feed.lock().unwrap().seq > 0;
                let (status, status_col) = if wired && live {
                    ("● recording", [0.95, 0.45, 0.5, 1.0])
                } else if wired {
                    ("waiting for frames", [0.8, 0.65, 0.5, 1.0])
                } else {
                    ("wire an app", [0.55, 0.7, 0.72, 1.0])
                };
                Some(Chrome(
                    CAPTURE_BORDER,
                    CAPTURE_BG,
                    "screen capture".to_string(),
                    TEXT,
                    status.to_string(),
                    status_col,
                ))
            } else if self.view.clipboard_boards.contains_key(&id) {
                let (status, status_col) = self.clipboard_grant(id);
                Some(Chrome(
                    CLIPBOARD_BORDER,
                    CLIPBOARD_BG,
                    "clipboard".to_string(),
                    TEXT,
                    status,
                    status_col,
                ))
            } else if self.view.api_nodes.contains(&id) {
                let wired = self.view.api_links.iter().any(|&(_, n)| n == id);
                let (status, status_col) = if wired {
                    ("● wired", [0.5, 0.85, 0.9, 1.0])
                } else {
                    ("wire an app", [0.55, 0.7, 0.72, 1.0])
                };
                Some(Chrome(
                    API_BORDER,
                    API_BG,
                    "wk api".to_string(),
                    TEXT,
                    status.to_string(),
                    status_col,
                ))
            } else if let Some(p) = self.view.boundary_ports.get(&id) {
                let col = port_color(p.kind);
                Some(Chrome(
                    col,
                    [col[0] * 0.22, col[1] * 0.22, col[2] * 0.22, 1.0],
                    p.name.clone(),
                    TEXT,
                    format!("{} {}", port_label(p.dir), p.kind.as_str()),
                    col,
                ))
            } else if let Some(g) = self.view.groups.get(&id) {
                Some(Chrome(
                    GROUP_BORDER,
                    GROUP_BG,
                    g.definition.clone(),
                    TEXT,
                    group_status(g),
                    GROUP_BORDER,
                ))
            } else if let Some(device) = self.view.midi_ins.get(&id) {
                let (status, status_col) = if device.is_empty() {
                    ("no device".to_string(), [0.8, 0.65, 0.5, 1.0])
                } else {
                    (device.clone(), [0.5, 0.85, 0.6, 1.0])
                };
                Some(Chrome(
                    MIDI_BORDER,
                    MIDI_BG,
                    "MIDI in".to_string(),
                    TEXT,
                    status,
                    status_col,
                ))
            } else if let Some(svc) = self.view.host_services.get(&id) {
                let wired = self.view.net_links.iter().any(|&(s, _)| s == id);
                let status_col = if wired {
                    [0.45, 0.85, 0.75, 1.0]
                } else {
                    [0.55, 0.7, 0.68, 1.0]
                };
                Some(Chrome(
                    HOSTSVC_BORDER,
                    HOSTSVC_BG,
                    svc.name.clone(),
                    TEXT,
                    format!("→ {}", svc.target),
                    status_col,
                ))
            } else {
                None
            };

            if let Some(Chrome(border, bg, title, title_col, status, status_col)) = widget {
                let lines = [
                    mk_line(
                        &mut self.text_cache3d,
                        &self.fonts3d,
                        gfx,
                        &title,
                        title_col,
                        0.05,
                        max_w,
                    ),
                    mk_line(
                        &mut self.text_cache3d,
                        &self.fonts3d,
                        gfx,
                        &status,
                        status_col,
                        0.04,
                        max_w,
                    ),
                ]
                .into_iter()
                .flatten()
                .collect();
                panels.push(Panel {
                    id,
                    center,
                    right,
                    normal,
                    w: cw,
                    h: size[1] / PX_PER_M,
                    border,
                    body: Body::Fill(bg),
                    surface_px: None,
                    label: None,
                    lines,
                    left_text: false,
                    body_drag: true,
                    ports: Vec::new(),
                });
                continue;
            }

            // An app node that is neither rendering nor a terminal yet (idle /
            // compiling): a plain dark card with its name.
            let name = self.node_label(id);
            let label = mk_line(
                &mut self.text_cache3d,
                &self.fonts3d,
                gfx,
                &name,
                TEXT,
                LABEL_H,
                2.0,
            );
            panels.push(Panel {
                id,
                center,
                right,
                normal,
                w: cw,
                h: size[1] / PX_PER_M,
                border: BORDER_COL,
                body: Body::Fill(BODY),
                surface_px: None,
                label,
                lines: Vec::new(),
                left_text: false,
                body_drag: true,
                ports: Vec::new(),
            });
        }

        // Typed connection ports on each panel's edges: inputs down the left,
        // outputs down the right, matching the 2D slot layout (canvas y down →
        // world y up).
        for p in &mut panels {
            let ports = self.node_ports(p.id);
            let ins: Vec<usize> = (0..ports.len())
                .filter(|&i| ports[i].dir == PortDir::In)
                .collect();
            let outs: Vec<usize> = (0..ports.len())
                .filter(|&i| ports[i].dir == PortDir::Out)
                .collect();
            let slot =
                |k: usize, n: usize, h: f32| h * 0.5 - h * (k as f32 + 1.0) / (n as f32 + 1.0);
            for (k, &pi) in ins.iter().enumerate() {
                p.ports
                    .push((ports[pi], -p.w * 0.5, slot(k, ins.len(), p.h)));
            }
            for (k, &pi) in outs.iter().enumerate() {
                p.ports
                    .push((ports[pi], p.w * 0.5, slot(k, outs.len(), p.h)));
            }
        }

        // ---- wk:scene entities: plugin-owned 3D objects riding their node ----
        struct Ent {
            /// Content hash of the entity's GLB — the key into `entity_meshes`.
            glb_hash: u64,
            /// Scenery is drawn but never picked (see `wk:scene`'s set-scenery).
            scenery: bool,
            model: [[f32; 4]; 4],
            /// World-space bounding sphere.
            center: [f32; 3],
            radius: f32,
            shared: wk_server::scene::SharedEntity,
            node: NodeId,
        }
        let scene_entities = self.view.scene_entities.clone();
        let mut ents: Vec<Ent> = Vec::new();
        let mut live_ents: HashSet<u64> = HashSet::new();
        for shared in &scene_entities {
            let (id, node_id, epos, eyaw, escale, scenery, hash, glb) = {
                let e = shared.lock().unwrap();
                (
                    e.id,
                    e.node_id,
                    e.pos,
                    e.yaw,
                    e.scale,
                    e.scenery,
                    e.glb_hash,
                    e.glb.clone(),
                )
            };
            live_ents.insert(hash);
            // Load the entity's GLB into GPU meshes on first sight of this
            // geometry. (Only once the renderer exists — caching an empty
            // result here is permanent, by design, for genuinely broken GLBs.)
            if !self.entity_meshes.contains_key(&hash) && self.renderer3d.is_some() {
                let gpu = match crate::gltf_scene::load_bytes(&glb) {
                    Ok(cpu) => {
                        let r3d = self.renderer3d.as_ref();
                        let mut meshes = Vec::new();
                        let mut owned_tex = Vec::new();
                        let (mut c, mut n) = ([0.0f64; 3], 0usize);
                        let mut r2 = 0.0f32;
                        if let Some(r3d) = r3d {
                            for m in &cpu {
                                let tex = match &m.texture {
                                    Some((w, h, px)) => {
                                        let t = gfx.renderer.create_texture(
                                            &gfx.device,
                                            &gfx.queue,
                                            *w,
                                            *h,
                                            px,
                                        );
                                        owned_tex.push(t);
                                        t
                                    }
                                    None => gfx.renderer.white,
                                };
                                meshes.push(r3d.upload_mesh(&gfx.device, m, tex));
                                for p in &m.positions {
                                    for k in 0..3 {
                                        c[k] += p[k] as f64;
                                    }
                                    n += 1;
                                }
                            }
                        }
                        let center = if n > 0 {
                            [
                                (c[0] / n as f64) as f32,
                                (c[1] / n as f64) as f32,
                                (c[2] / n as f64) as f32,
                            ]
                        } else {
                            [0.0; 3]
                        };
                        for m in &cpu {
                            for p in &m.positions {
                                let v = sub3(*p, center);
                                r2 = r2.max(dot3(v, v));
                            }
                        }
                        EntityGpu {
                            meshes,
                            owned_tex,
                            bound: (center, r2.sqrt().max(0.05)),
                        }
                    }
                    Err(e) => {
                        eprintln!("scene entity {id}: {e}");
                        EntityGpu {
                            meshes: Vec::new(),
                            owned_tex: Vec::new(),
                            bound: ([0.0; 3], 0.0),
                        }
                    }
                };
                self.entity_meshes.insert(hash, gpu);
            }
            let Some(&(origin, nyaw)) = poses.get(&node_id) else {
                continue;
            };
            let model = mat_mul(
                mat_mul(mat_translate(origin), mat_rot_y(nyaw)),
                mat_mul(
                    mat_mul(mat_translate(epos), mat_rot_y(eyaw)),
                    mat_scale(escale),
                ),
            );
            let cache = &self.entity_meshes[&hash];
            ents.push(Ent {
                glb_hash: hash,
                scenery,
                model,
                center: transform_point3(model, cache.bound.0),
                radius: cache.bound.1 * escale.max(0.01),
                shared: shared.clone(),
                node: node_id,
            });
        }
        // Free GPU meshes for entities that vanished.
        let stale: Vec<u64> = self
            .entity_meshes
            .keys()
            .copied()
            .filter(|hash| !live_ents.contains(hash))
            .collect();
        for hash in stale {
            if let Some(gpu) = self.entity_meshes.remove(&hash) {
                for t in gpu.owned_tex {
                    gfx.renderer.remove_texture(t);
                }
            }
        }

        // ---- mouse: palette, wire/move drags, else pointer into surfaces ----
        let (o, d) = self.cam3d.pixel_ray(mp, fb);
        let chord = self.mods.super_key() || self.mods.control_key();
        // Nearest panel hit under the cursor: a connection port (checked
        // first, and reaching slightly past the card edge), the card body, or
        // its floating label.
        #[derive(Clone, Copy, PartialEq)]
        enum Zone {
            Body,
            Label,
            Port(usize),
        }
        let mut best: Option<(f32, usize, f32, f32, Zone)> = None;
        if !self.rmb {
            for (i, p) in panels.iter().enumerate() {
                let denom = dot3(d, p.normal);
                if denom.abs() < 1e-6 {
                    continue;
                }
                let t = dot3(sub3(p.center, o), p.normal) / denom;
                if t <= 0.05 {
                    continue;
                }
                let hit = [o[0] + d[0] * t, o[1] + d[1] * t, o[2] + d[2] * t];
                let lu = dot3(sub3(hit, p.center), p.right);
                let lv = hit[1] - p.center[1];
                let port = p.ports.iter().position(|&(_, pu, pv)| {
                    let (du, dv) = (lu - pu, lv - pv);
                    (du * du + dv * dv).sqrt() <= PORT3D_R * 1.8
                });
                let zone = if let Some(pi) = port {
                    Some(Zone::Port(pi))
                } else if lu.abs() <= p.w * 0.5 && lv.abs() <= p.h * 0.5 {
                    Some(Zone::Body)
                } else if p.label.as_ref().is_some_and(|l| {
                    lu.abs() <= l.w * 0.5 && (lv - (p.h * 0.5 + LABEL_H)).abs() <= l.h * 0.5
                }) {
                    Some(Zone::Label)
                } else {
                    None
                };
                if let Some(z) = zone {
                    if best.is_none_or(|b| t < b.0) {
                        best = Some((t, i, lu, lv, z));
                    }
                }
            }
        }

        // Entities claim the cursor when their bounding sphere is the nearest
        // hit: the guest gets hover/press/release, the panel hit is dropped,
        // and pressing focuses the owning node.
        let mut ent_claimed = false;
        if !self.rmb && !self.palette_open && self.drag3d.is_none() && self.wire3d.is_none() {
            let mut ent_hit: Option<(f32, usize)> = None;
            for (i, e) in ents.iter().enumerate() {
                if e.scenery {
                    continue; // you walk through the world, you don't click it
                }
                if let Some(t) = ray_sphere(o, d, e.center, e.radius) {
                    if t > 0.05 && ent_hit.is_none_or(|b| t < b.0) {
                        ent_hit = Some((t, i));
                    }
                }
            }
            if let Some((te, i)) = ent_hit {
                if best.is_none_or(|b| te < b.0) {
                    ent_claimed = true;
                    best = None;
                    let e = &ents[i];
                    if down_edge && chord {
                        // Cmd/Ctrl+drag carries the object: grab its node,
                        // same convention as dragging a surface panel.
                        self.kbd_focus = Some(e.node);
                        if let Some(&(origin, _)) = poses.get(&e.node) {
                            let hit = [o[0] + d[0] * te, o[1] + d[1] * te, o[2] + d[2] * te];
                            self.drag3d = Some((e.node, te, sub3(origin, hit)));
                        }
                    } else {
                        let mut st = e.shared.lock().unwrap();
                        st.push_event(RayEvent::Hover);
                        if down_edge {
                            st.push_event(RayEvent::Press);
                            drop(st);
                            self.kbd_focus = Some(e.node);
                        } else if up_edge {
                            st.push_event(RayEvent::Release);
                        }
                    }
                }
            }
        }

        if self.palette_open {
            // The palette is modal: click a row to run it, click anywhere else
            // to dismiss it (same as 2D).
            if down_edge {
                let (px, py, pw, row_h) = Self::palette_layout(fb);
                let filtered = self.palette_filtered();
                let start = (self.palette_scroll.round() as usize).min(filtered.len());
                for (i, r) in filtered.iter().skip(start).take(PALETTE_MAX).enumerate() {
                    let y0 = py + (i as f32 + 1.0) * row_h;
                    if contains([px, y0, px + pw, y0 + row_h], mp) {
                        self.palette_run = Some(r.cmd);
                        break;
                    }
                }
                self.palette_open = false;
                self.palette_query.clear();
            }
        } else if let Some((src, from)) = self.wire3d {
            // A wire drag: on release, connect to a node that accepts the kind
            // (its matching input port, or anywhere on such a node); dropping
            // on an already-wired pair removes the wire — 2D's toggle
            // semantics exactly.
            if !lmb {
                self.wire3d = None;
                if let Some((_, i, _, _, zone)) = best {
                    let p = &panels[i];
                    let fits = |&(port, _, _): &(Port, f32, f32)| {
                        port.kind == from.kind && port.dir == PortDir::In
                    };
                    let target = match zone {
                        Zone::Port(pi) => Some(p.ports[pi]).filter(fits),
                        _ => p.ports.iter().copied().find(fits),
                    };
                    if let Some((port, _, _)) = target {
                        if p.id != src {
                            let dst = (p.id, port);
                            self.finish_wire_drag((src, from), dst);
                        }
                    }
                }
            }
        } else if let Some((id, dist, off)) = self.drag3d {
            // The grabbed node rides the cursor ray at its grab distance
            // (scroll pushes/pulls), turning to face you. Its free 3D pose is
            // written to the server — promoting it off the layout cylinder
            // and into the world for good.
            if !lmb {
                self.drag3d = None;
            } else {
                let dist = (dist + fly * 0.35).clamp(0.4, 60.0);
                self.drag3d = Some((id, dist, off));
                let c = [
                    o[0] + d[0] * dist + off[0],
                    o[1] + d[1] * dist + off[1],
                    o[2] + d[2] * dist + off[2],
                ];
                let yaw = (o[0] - c[0]).atan2(o[2] - c[2]);
                self.conn.send(Command::Update {
                    id,
                    patch: NodePatch {
                        pos3d: Some([c[0], c[1], c[2], yaw]),
                        ..Default::default()
                    },
                });
            }
        } else if !self.rmb {
            if let Some((t, i, lu, lv, zone)) = best {
                let p = &panels[i];
                match zone {
                    Zone::Port(pi) => {
                        // Dragging out of an out-port starts a wire.
                        let (port, _, _) = p.ports[pi];
                        if down_edge && port.dir == PortDir::Out {
                            self.wire3d = Some((p.id, port));
                        }
                    }
                    zone => {
                        let on_label = zone == Zone::Label;
                        let grab_here = down_edge && (on_label || p.body_drag || chord);
                        if grab_here {
                            // Grabbing a node also makes it the active node —
                            // the one the keyboard goes to.
                            self.kbd_focus = Some(p.id);
                            let hit = [o[0] + d[0] * t, o[1] + d[1] * t, o[2] + d[2] * t];
                            self.drag3d = Some((p.id, t, sub3(p.center, hit)));
                        } else if !on_label && !chord {
                            if let (Some((pw, ph)), Some(surf)) =
                                (p.surface_px, node_surface.get(&p.id))
                            {
                                let at = |button| PointerEvent {
                                    x: ((lu + p.w * 0.5) / p.w * pw as f32) as f64,
                                    y: ((p.h * 0.5 - lv) / p.h * ph as f32) as f64,
                                    button,
                                };
                                let mut s = surf.lock().unwrap();
                                s.pointer_move.push_back(at(None));
                                if down_edge {
                                    s.pointer_down.push_back(at(Some(PointerButton::Left)));
                                }
                                if up_edge {
                                    s.pointer_up.push_back(at(Some(PointerButton::Left)));
                                }
                                // Middle button is free in 3D too (the right
                                // button is look mode — the canvas keeps it).
                                if self.mmb && !self.prev_mmb {
                                    s.pointer_down.push_back(at(Some(PointerButton::Middle)));
                                }
                                if !self.mmb && self.prev_mmb {
                                    s.pointer_up.push_back(at(Some(PointerButton::Middle)));
                                }
                            }
                        }
                    }
                }
            } else if down_edge && !ent_claimed {
                // A click on empty space clears the active node (so Escape can
                // then exit the 3D view, and a focused vim keeps its Escape).
                self.kbd_focus = None;
            }
        }
        // The port under the cursor lights up (hover / drop target). Captured
        // by id + index so the panel sort below can't invalidate it.
        let hot_port: Option<(NodeId, usize)> = best.and_then(|(_, i, _, _, z)| match z {
            Zone::Port(pi) => Some((panels[i].id, pi)),
            _ => None,
        });
        // Run a command chosen from the palette.
        if let Some(cmd) = self.palette_run.take() {
            self.run_palette(cmd, fb);
        }

        // ---- build the world ----
        // A world is just a node's scenery now (see `wk:scene`'s set-scenery):
        // if any is loaded, this workspace has a place to stand in.
        let world_loaded = ents
            .iter()
            .any(|e| e.scenery && !self.entity_meshes[&e.glb_hash].meshes.is_empty());

        let eye = self.cam3d.pos;
        let white = gfx.renderer.white;
        let circle = gfx.renderer.circle;
        let scale3 = |v: [f32; 3], k: f32| [v[0] * k, v[1] * k, v[2] * k];
        // Panels draw far-to-near, each in its own back-to-front order
        // (backing, body, text, label). Sorting whole panels — never
        // individual quads — keeps a card's translucent text from writing
        // depth before the card body draws and punching a hole through it.
        let d2 = |p: [f32; 3]| {
            let v = sub3(p, eye);
            dot3(v, v)
        };
        panels.sort_by(|a, b| {
            d2(b.center)
                .partial_cmp(&d2(a.center))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut quads3: Vec<Quad3> = Vec::new();
        // Without a world scene, a ground plane one eye-height below the
        // camera start gives some bearings.
        if !world_loaded {
            quads3.push(Quad3::spanned(
                [0.0, -1.6, 0.0],
                [60.0, 0.0, 0.0],
                [0.0, 0.0, -60.0],
                [0.0, 0.0, 1.0, 1.0],
                GROUND_COL,
                white,
            ));
        }
        // Connection wires, anchored on the panels' typed ports (source's
        // out-port to target's in-port; centre as the fallback), in the wire
        // kind's colour.
        let idx: HashMap<NodeId, usize> =
            panels.iter().enumerate().map(|(i, p)| (p.id, i)).collect();
        let port_at = |p: &Panel, pu: f32, pv: f32| -> [f32; 3] {
            [
                p.center[0] + p.right[0] * pu,
                p.center[1] + pv,
                p.center[2] + p.right[2] * pu,
            ]
        };
        // One particular port of a panel, by slot — how an instance's dots are
        // told apart when the definition declares two of one kind.
        let port_slot_world = |p: &Panel, slot: usize| -> Option<[f32; 3]> {
            p.ports
                .iter()
                .find(|&&(port, _, _)| port.slot == slot)
                .map(|&(_, pu, pv)| port_at(p, pu, pv))
        };
        let port_world = |p: &Panel, kind: PortKind, dir: PortDir| -> [f32; 3] {
            match p
                .ports
                .iter()
                .find(|&&(port, _, _)| port.kind == kind && port.dir == dir)
            {
                Some(&(_, pu, pv)) => [
                    p.center[0] + p.right[0] * pu,
                    p.center[1] + pv,
                    p.center[2] + p.right[2] * pu,
                ],
                None => p.center,
            }
        };
        let mut links: Vec<(NodeId, NodeId, PortKind)> = Vec::new();
        for &(f, a) in &self.view.connections {
            links.push((f, a, PortKind::Bind));
        }
        for &(s, d) in &self.view.midi_links {
            links.push((s, d, PortKind::Midi));
        }
        for (&http, &hp) in &self.view.serves {
            links.push((http, hp, PortKind::Serve));
        }
        for &(app, net) in &self.view.net_links {
            links.push((app, net, PortKind::Net));
        }
        for &(app, cap) in &self.view.capture_links {
            links.push((app, cap, PortKind::Capture));
        }
        for &(app, clip) in &self.view.clipboard_links {
            links.push((app, clip, PortKind::Clipboard));
        }
        for &(app, api) in &self.view.api_links {
            links.push((app, api, PortKind::Api));
        }
        for (a, b, kind) in links {
            if let (Some(&ia), Some(&ib)) = (idx.get(&a), idx.get(&b)) {
                quads3.push(Quad3::ribbon(
                    white,
                    port_world(&panels[ia], kind, PortDir::Out),
                    port_world(&panels[ib], kind, PortDir::In),
                    eye,
                    0.012,
                    port_color(kind),
                ));
            }
        }
        // Boundary wires, as on the flat canvas: the live wire they stand for
        // ends inside the instance, where this room cannot see it.
        for (&gid, g) in &self.view.groups {
            for (dir, wires) in [(PortDir::In, &g.in_wires), (PortDir::Out, &g.out_wires)] {
                for (name, node) in wires {
                    let Some(slot) = g.ports.iter().position(|p| p.dir == dir && p.name == *name)
                    else {
                        continue;
                    };
                    let kind = g.ports[slot].kind;
                    let (Some(&ig), Some(&inode)) = (idx.get(&gid), idx.get(node)) else {
                        continue;
                    };
                    let Some(gp) = port_slot_world(&panels[ig], slot) else {
                        continue;
                    };
                    let far = port_world(&panels[inode], kind, dir.opposite());
                    let (from, to) = match dir {
                        PortDir::In => (far, gp),
                        PortDir::Out => (gp, far),
                    };
                    quads3.push(Quad3::ribbon(white, from, to, eye, 0.012, port_color(kind)));
                }
            }
        }
        // The wire being dragged: from its source port to the cursor (the
        // hovered panel's depth, or a fixed reach into the room).
        if let Some((src, sport)) = self.wire3d {
            if let Some(sp) = panels.iter().find(|p| p.id == src) {
                let kind = sport.kind;
                let from = port_slot_world(sp, sport.slot)
                    .unwrap_or_else(|| port_world(sp, kind, PortDir::Out));
                let reach = best.map(|(t, ..)| t).unwrap_or(2.5);
                let to = [
                    o[0] + d[0] * reach,
                    o[1] + d[1] * reach,
                    o[2] + d[2] * reach,
                ];
                quads3.push(Quad3::ribbon(white, from, to, eye, 0.012, port_color(kind)));
            }
        }
        // Panels: a backing plate (highlighted while dragged), the content
        // (live texture or a kind-coloured fill), stacked card text, and a
        // floating name label above app nodes.
        for p in &panels {
            let uv = [0.0, 0.0, 1.0, 1.0];
            let back = [
                p.center[0] - p.normal[0] * 0.005,
                p.center[1],
                p.center[2] - p.normal[2] * 0.005,
            ];
            let dragged = self.drag3d.is_some_and(|(id, _, _)| id == p.id);
            let focused = self.kbd_focus == Some(p.id);
            quads3.push(Quad3::spanned(
                back,
                scale3(p.right, p.w * 0.5 + 0.015),
                [0.0, p.h * 0.5 + 0.015, 0.0],
                uv,
                if dragged {
                    WIRE_SEL_COL
                } else if focused {
                    TITLE_FOCUS
                } else {
                    p.border
                },
                white,
            ));
            match p.body {
                Body::Tex(t) => quads3.push(Quad3::spanned(
                    p.center,
                    scale3(p.right, p.w * 0.5),
                    [0.0, p.h * 0.5, 0.0],
                    uv,
                    [1.0, 1.0, 1.0, 1.0],
                    t,
                )),
                Body::Fill(col) => quads3.push(Quad3::spanned(
                    p.center,
                    scale3(p.right, p.w * 0.5),
                    [0.0, p.h * 0.5, 0.0],
                    uv,
                    col,
                    white,
                )),
            }
            // Card text, stacked from the top, floated just off the body so it
            // never z-fights it.
            let mut ty = p.h * 0.5 - 0.045;
            for l in &p.lines {
                let tx = if p.left_text {
                    -p.w * 0.5 + 0.02 + l.w * 0.5
                } else {
                    0.0
                };
                if ty - l.h * 0.5 < -p.h * 0.5 {
                    break; // out of card
                }
                let lc = [
                    p.center[0] + p.normal[0] * 0.003 + p.right[0] * tx,
                    p.center[1] + ty,
                    p.center[2] + p.normal[2] * 0.003 + p.right[2] * tx,
                ];
                quads3.push(Quad3::spanned(
                    lc,
                    scale3(p.right, l.w * 0.5),
                    [0.0, l.h * 0.5, 0.0],
                    uv,
                    l.color,
                    l.tex,
                ));
                ty -= l.h + 0.012;
            }
            if let Some(l) = &p.label {
                let lc = [p.center[0], p.center[1] + p.h * 0.5 + LABEL_H, p.center[2]];
                quads3.push(Quad3::spanned(
                    lc,
                    scale3(p.right, l.w * 0.5),
                    [0.0, l.h * 0.5, 0.0],
                    uv,
                    l.color,
                    l.tex,
                ));
            }
            // Typed ports as discs riding the panel edges (inputs dimmer, the
            // hovered one lit and enlarged — hover is also the drop target).
            for (pi, &(port, pu, pv)) in p.ports.iter().enumerate() {
                let c = port_color(port.kind);
                let (col, r) = if hot_port == Some((p.id, pi)) {
                    (PORT_HOT, PORT3D_R * 1.4)
                } else if port.dir == PortDir::In {
                    ([c[0] * 0.7, c[1] * 0.7, c[2] * 0.7, 1.0], PORT3D_R)
                } else {
                    (c, PORT3D_R)
                };
                let pc = [
                    p.center[0] + p.right[0] * pu + p.normal[0] * 0.004,
                    p.center[1] + pv,
                    p.center[2] + p.right[2] * pu + p.normal[2] * 0.004,
                ];
                quads3.push(Quad3::spanned(
                    pc,
                    scale3(p.right, r),
                    [0.0, r, 0.0],
                    uv,
                    col,
                    circle,
                ));
            }
        }

        // ---- render: a depth pass for the world, then a 2D HUD pass ----
        let vp = self.cam3d.view_proj(fb[0] / fb[1].max(1.0));
        let mut all_meshes: Vec<&MeshGpu> = Vec::new();
        let mut world_draws: Vec<MeshDraw> = Vec::new();
        for e in &ents {
            if let Some(gpu) = self.entity_meshes.get(&e.glb_hash) {
                for m in &gpu.meshes {
                    all_meshes.push(m);
                    world_draws.push(MeshDraw {
                        mesh: all_meshes.len() - 1,
                        model: e.model,
                        color: [1.0, 1.0, 1.0, 1.0],
                    });
                }
            }
        }
        let renderer3d = self.renderer3d.as_mut().unwrap();
        let depth =
            renderer3d.depth_view(&gfx.device, gfx.surface_desc.width, gfx.surface_desc.height);
        let frame = match gfx.surface.get_current_texture() {
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
                label: Some("frame3d"),
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
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &depth,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            renderer3d.draw_world(
                &gfx.device,
                &gfx.queue,
                &mut rpass,
                &gfx.renderer,
                vp,
                WORLD_LIGHT,
                &all_meshes,
                &world_draws,
                &quads3,
            );
        }
        // HUD: the exit hint, drawn flat over the world.
        let mut hud: Vec<Quad> = Vec::new();
        self.text_cache.draw(
            &mut hud,
            &mut gfx.renderer,
            &gfx.fonts,
            &gfx.device,
            &gfx.queue,
            "3D — hold right: look + WASD walk (F: fly) · drag card/label: move + focus · click empty: unfocus · Esc exits",
            PAD,
            fb[1] - MENU_H + PAD,
            1.0,
            MUTED_TEXT,
            [0.0, 0.0, fb[0], fb[1]],
        );
        // Who has the keyboard right now.
        if let Some(fid) = self.kbd_focus {
            let who = format!("keyboard → {}", self.node_label(fid));
            self.text_cache.draw(
                &mut hud,
                &mut gfx.renderer,
                &gfx.fonts,
                &gfx.device,
                &gfx.queue,
                &who,
                PAD,
                PAD,
                1.0,
                TEXT,
                [0.0, 0.0, fb[0], fb[1]],
            );
        }
        self.draw_palette(&mut hud, gfx, fb, mp);
        {
            let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            gfx.renderer
                .draw(&gfx.device, &gfx.queue, &mut rpass, fb, &hud);
        }
        gfx.queue.submit([encoder.finish()]);
        frame.present();
    }

    /// Draw the command palette overlay (dim, centred panel, typed query,
    /// filtered rows) — shared by the 2D canvas and the 3D view's HUD pass.
    fn draw_palette(&mut self, quads: &mut Vec<Quad>, gfx: &mut Gfx, fb: [f32; 2], mp: [f32; 2]) {
        if !self.palette_open {
            return;
        }
        let white = gfx.renderer.white;
        let full = [0.0, 0.0, fb[0], fb[1]];
        let lh = gfx.fonts.line_height() as f32;
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
            quads,
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
        for (i, prow) in filtered.iter().skip(start).take(PALETTE_MAX).enumerate() {
            let row = start + i;
            let y0 = py + (i as f32 + 1.0) * row_h;
            let r = [px, y0, px + pw, y0 + row_h];
            let hot = contains(r, mp);
            if row == self.palette_sel || hot {
                quads.push(Quad::solid(white, r, TITLE_FOCUS, full));
            }
            self.text_cache.draw(
                quads,
                &mut gfx.renderer,
                &gfx.fonts,
                &gfx.device,
                &gfx.queue,
                &prow.label,
                px + PAD,
                y0 + (row_h - lh) * 0.5,
                1.0,
                TEXT,
                full,
            );
            // The description, dimmer, after the label — clipped to the row
            // so a long one can't spill out of the panel.
            if let Some(desc) = &prow.desc {
                let dx = px + PAD + gfx.fonts.measure(&prow.label) as f32 + 14.0;
                self.text_cache.draw(
                    quads,
                    &mut gfx.renderer,
                    &gfx.fonts,
                    &gfx.device,
                    &gfx.queue,
                    desc,
                    dx,
                    y0 + (row_h - lh * 0.85) * 0.5,
                    0.85,
                    [0.5, 0.54, 0.6, 1.0],
                    [r[0], r[1], r[2] - PAD, r[3]],
                );
            }
        }
    }

    /// Keep the cursor grab in sync with look mode (3D + right button held):
    /// grabbed and invisible while looking, normal otherwise. Reconciled every
    /// loop tick so every exit path (button release, Esc, palette toggle to
    /// 2D) releases it.
    fn sync_look_capture(&mut self) {
        let want = self.mode_3d && self.rmb;
        if want == self.look_captured {
            return;
        }
        let Some(gfx) = &self.gfx else {
            return;
        };
        if want {
            // macOS supports Locked; Windows/X11 only Confined — try both.
            let _ = gfx
                .window
                .set_cursor_grab(CursorGrabMode::Locked)
                .or_else(|_| gfx.window.set_cursor_grab(CursorGrabMode::Confined));
        } else {
            let _ = gfx.window.set_cursor_grab(CursorGrabMode::None);
        }
        gfx.window.set_cursor_visible(!want);
        self.look_captured = want;
    }

    fn frame(&mut self) {
        let Some(mut gfx) = self.gfx.take() else {
            return;
        };
        // Refresh our snapshot of the server for this frame. The server ticks
        // and advances the runtime on its own thread; we only read and send.
        let full = self.conn.view();
        self.tabs = full.workspaces.clone();
        (self.active_ws, self.pending_ws) =
            reconcile_active_ws(&self.tabs, self.active_ws, self.pending_ws);
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
        // The live set across *every* workspace — client-local per-node state
        // (terminals, detached windows, surface textures) is keyed by node/
        // surface, not by tab, so it must be reconciled against all workspaces.
        // Reconciling against the active-tab-only view would tear down a
        // detached window or drop a terminal's scrollback on every tab switch.
        let all_nodes: Vec<SharedNode> = full.nodes.clone();
        let all_node_ids: std::collections::HashSet<NodeId> =
            all_nodes.iter().map(|n| n.id).collect();
        let all_surface_ids: std::collections::HashSet<u64> =
            full.surfaces.iter().map(|s| s.lock().unwrap().id).collect();
        // Free GPU textures for surfaces that vanished by any path (undo, node
        // exit, workspace close) — not just the close-button path.
        let stale_views: Vec<u64> = self
            .views
            .keys()
            .copied()
            .filter(|sid| !all_surface_ids.contains(sid))
            .collect();
        for sid in stale_views {
            if let Some((tex, _, _)) = self.views.remove(&sid) {
                gfx.renderer.remove_texture(tex);
            }
        }

        // A `wk view` switch is applied before the slice below is picked, so
        // it takes effect on this very frame rather than the next one.
        self.apply_view_request(
            full.view_mode,
            [
                gfx.surface_desc.width as f32,
                gfx.surface_desc.height as f32,
            ],
        );

        // In 3D the whole document is one world — every workspace's nodes are
        // present (each workspace is just a cluster); 2D keeps per-tab views.
        self.view = if self.mode_3d {
            full.clone()
        } else {
            full.for_workspace(self.active_ws)
        };

        // Drop keyboard/edit state pointing at a node that's no longer visible
        // (deleted, or in another tab) so keystrokes — including a paste — can't
        // route into a dead or off-screen editor.
        if let Some((id, _)) = &self.editing_args {
            if !self.view.win_pos.contains_key(id) {
                self.editing_args = None;
            }
        }
        if let Some((id, _)) = &self.editing_note {
            if !self.view.win_pos.contains_key(id) {
                self.editing_note = None;
            }
        }
        if let Some((pair, _)) = &self.editing_mount {
            if !self.view.connections.contains(pair) {
                self.editing_mount = None;
            }
        }
        if self
            .kbd_focus
            .is_some_and(|id| !self.view.win_pos.contains_key(&id))
        {
            self.kbd_focus = None;
        }

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
        // Right/middle button edges: unused by the 2D canvas (the right button
        // only drives look mode in 3D), so they route to the hovered surface.
        let rmb_down = self.rmb && !self.prev_rmb;
        let rmb_up = !self.rmb && self.prev_rmb;
        let mmb_down = self.mmb && !self.prev_mmb;
        let mmb_up = !self.mmb && self.prev_mmb;
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
        // Keyed over *all* workspaces so detached windows / terminals in
        // another tab still resolve their node; the active-tab render is gated
        // by `z` (below), so this doesn't draw off-tab nodes.
        let node_by_id: HashMap<NodeId, SharedNode> =
            all_nodes.iter().map(|i| (i.id, i.clone())).collect();
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
        // Feed *every* workspace's terminals, not just the active tab's, so a
        // node's output while you're on another tab keeps draining into its
        // scrollback instead of buffering and replaying in one gulp on return.
        for node in &all_nodes {
            if node_surface.contains_key(&node.id) {
                continue;
            }
            // A CLI client attached to this node owns its terminal I/O; the UI
            // treats it as detached and doesn't drain (or later feed) it.
            if self.view.attached.contains(&node.id) {
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
            .retain(|id, _| all_node_ids.contains(id) && !node_surface.contains_key(id));

        // ---- 3D view ----
        // The palette's "3D View" walks the same workspace as a world of
        // panels; all the 2D canvas interaction below is bypassed while it's
        // active (the camera owns the mouse and keyboard, Esc returns).
        if self.mode_3d {
            self.frame_3d(
                &mut gfx,
                &surfaces,
                &node_surface,
                &node_by_id,
                fb,
                mp,
                lmb,
                down_edge,
                up_edge,
            );
            self.prev_lmb = lmb;
            self.prev_rmb = self.rmb;
            self.prev_mmb = self.mmb;
            self.gfx = Some(gfx);
            return;
        }

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
                DragMode::Connect(_) if lmb => self.drag = Some(d),
                // Released: wire to a target that accepts this kind — its matching
                // input port, or anywhere on a node that has one, for convenience.
                DragMode::Connect(from) => {
                    let target = self
                        .port_under(mp, zf, PortDir::In)
                        .filter(|&(_, p)| p.kind == from.kind)
                        .or_else(|| {
                            let t = self.topmost_under(mp)?;
                            let p = self
                                .node_ports(t)
                                .into_iter()
                                .find(|p| p.kind == from.kind && p.dir == PortDir::In)?;
                            Some((t, p))
                        });
                    if let Some(target) = target.filter(|&(t, _)| t != d.id) {
                        self.finish_wire_drag((d.id, from), target);
                    }
                }
                _ => {} // move/resize released: drop the drag
            }
        }

        if down_edge && self.drag.is_none() {
            let mut consumed = false;
            // A click anywhere but the currently-edited note's body commits that
            // note's text (a click on it keeps editing).
            if let Some((eid, _)) = self.editing_note.clone() {
                let er = self.rect_of(eid);
                let same = self.topmost_under(mp) == Some(eid)
                    && !contains(close_btn(er, zf), mp)
                    && mp[1] >= er[1] + NOTE_GRAB * zf;
                if !same {
                    self.commit_note();
                }
            }
            // A click on the selected bind wire's mount-path label starts (or
            // continues) editing the in-app mount path; any other fresh click
            // clears the wire selection — a click that lands on a wire
            // (empty-canvas branch below) re-selects it.
            let mount_label_hit = match self.wire_sel {
                Some(Wire::Bind(f, a)) => self
                    .mount_label_rect(&gfx.fonts, f, a)
                    .filter(|r| contains(*r, mp))
                    .map(|_| (f, a)),
                _ => None,
            };
            if let Some((f, a)) = mount_label_hit {
                if self.editing_mount.as_ref().map(|(w, _)| *w) != Some((f, a)) {
                    self.editing_mount = Some(((f, a), mount_path(&self.view, f, a)));
                }
                consumed = true;
            } else {
                self.editing_mount = None;
                self.wire_sel = None;
            }
            // The filesystem inspector is modal: navigate on a row click,
            // dismiss on the close box or a click outside the panel.
            if let Some(insp) = &self.inspect {
                let entries = node_by_id
                    .get(&insp.node)
                    .map(|n| inspect_listing(&self.browse, n, &insp.dir))
                    .unwrap_or_default();
                let (panel, close, up, rows, _preview) = self.inspect_regions(fb, entries.len());
                if contains(close, mp) || !contains(panel, mp) {
                    self.inspect = None;
                } else if up.is_some_and(|r| contains(r, mp)) {
                    if let Some(i) = self.inspect.as_mut() {
                        i.go_up();
                    }
                } else if let Some(&(_, idx)) = rows.iter().find(|(r, _)| contains(*r, mp)) {
                    let entry = entries[idx].clone();
                    if let Some(i) = self.inspect.as_mut() {
                        if entry.is_dir {
                            i.dir = i.child_path(&entry.name);
                            i.file = None;
                            i.scroll = 0.0;
                        } else {
                            i.file = Some(entry.name.clone());
                        }
                    }
                }
                consumed = true;
            }
            // The output-log panel is modal too: dismiss on its close box or a
            // click outside it (there's nothing to click inside).
            if let Some(lv) = &self.logs {
                let (x, y, w, h, row_h) = inspect_layout(fb);
                let panel = [x, y, x + w, y + h];
                let close = {
                    let s = row_h - 8.0;
                    [x + w - s - 6.0, y + 4.0, x + w - 6.0, y + 4.0 + s]
                };
                let _ = lv;
                if contains(close, mp) || !contains(panel, mp) {
                    self.logs = None;
                }
                consumed = true;
            }
            // The command palette is modal: click a row to run it, click
            // anywhere else to dismiss it.
            if !consumed && self.palette_open {
                let (px, py, pw, row_h) = Self::palette_layout(fb);
                let filtered = self.palette_filtered();
                let start = (self.palette_scroll.round() as usize).min(filtered.len());
                for (i, r) in filtered.iter().skip(start).take(PALETTE_MAX).enumerate() {
                    let y0 = py + (i as f32 + 1.0) * row_h;
                    if contains([px, y0, px + pw, y0 + row_h], mp) {
                        self.palette_run = Some(r.cmd);
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
            // Dragging a wire out of a node's typed output port (right edge).
            // Checked before the node-body hit-test so the port's outer half
            // (past the edge) is grabbable too.
            if !consumed {
                if let Some((id, p)) = self.port_under(mp, zf, PortDir::Out) {
                    self.z.retain(|&x| x != id);
                    self.z.push(id);
                    self.drag = Some(Drag {
                        id,
                        mode: DragMode::Connect(p),
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
                    let is_uplink = self.view.uplinks.contains_key(&id);
                    let is_note = self.view.notes.contains_key(&id);
                    let is_capture = self.view.capture_feeds.contains_key(&id);
                    let is_api = self.view.api_nodes.contains(&id);
                    let is_clipboard = self.view.clipboard_boards.contains_key(&id);
                    let is_boundary = self.view.boundary_ports.contains_key(&id);
                    let is_group = self.view.groups.contains_key(&id);
                    if is_note {
                        // Note: close (top-right), drag from the top strip, or
                        // click the body to edit the text.
                        if contains(close_btn(r, zf), mp) {
                            self.conn.send(Command::Delete(ResourceRef::Node(id)));
                        } else if mp[1] < r[1] + NOTE_GRAB * zf {
                            let mc = self.cam.to_canvas(mp);
                            let p = self.view.win_pos[&id];
                            self.drag = Some(Drag {
                                id,
                                mode: DragMode::Move,
                                grab: [mc[0] - p[0], mc[1] - p[1]],
                            });
                        } else if !matches!(&self.editing_note, Some((eid, _)) if *eid == id) {
                            // Start editing (a click on an already-editing note
                            // keeps its in-progress buffer).
                            let cur = self.view.notes.get(&id).cloned().unwrap_or_default();
                            self.editing_note = Some((id, cur));
                        }
                    } else if is_file
                        || is_port
                        || is_net
                        || is_uplink
                        || is_capture
                        || is_api
                        || is_clipboard
                        || is_boundary
                        || is_group
                    {
                        // Canvas widget nodes (file / HostPort / Network / Iroh):
                        // close, adjust port (HostPort −/+ buttons), edit the
                        // peer ticket (Iroh, lower half), or move.
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
                        } else if is_uplink && contains(ticket_btn(r, zf), mp) {
                            // Copy this uplink's own ticket. It is ~200 opaque
                            // characters, so the clipboard is the only sane
                            // way to move it to the other side.
                            self.copy_ticket(id);
                        } else if is_uplink && mp[1] > (r[1] + r[3]) * 0.5 {
                            // Click the status line to type/paste the remote
                            // ticket; Enter dials it.
                            let cur = self
                                .view
                                .node_args
                                .get(&id)
                                .cloned()
                                .unwrap_or_default()
                                .join(" ");
                            self.editing_args = Some((id, cur));
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
                        } else if contains(files_btn(r, zf), mp) {
                            // Open (or toggle) the node's filesystem inspector.
                            self.logs = None; // one modal panel at a time
                            self.inspect = match self.inspect.take() {
                                Some(insp) if insp.node == id => None,
                                _ => Some(Inspector {
                                    node: id,
                                    dir: String::new(),
                                    file: None,
                                    scroll: 0.0,
                                }),
                            };
                        } else if contains(logs_btn(r, zf), mp) {
                            // Open (or toggle) the node's output-log panel.
                            self.inspect = None; // one modal panel at a time
                            self.logs = match self.logs.take() {
                                Some(lv) if lv.node == id => None,
                                _ => Some(LogView {
                                    node: id,
                                    scroll: 0.0,
                                }),
                            };
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
                        let at = |button| PointerEvent {
                            x: ((mp[0] - ca[0]) / zf) as f64,
                            y: ((mp[1] - ca[1]) / zf) as f64,
                            button,
                        };
                        let mut s = surf.lock().unwrap();
                        s.pointer_move.push_back(at(None));
                        for (btn, down, up) in [
                            (PointerButton::Left, down_edge, up_edge),
                            (PointerButton::Right, rmb_down, rmb_up),
                            (PointerButton::Middle, mmb_down, mmb_up),
                        ] {
                            if down {
                                s.pointer_down.push_back(at(Some(btn)));
                            }
                            if up {
                                s.pointer_up.push_back(at(Some(btn)));
                            }
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
            } else if !self.term_input.is_empty() && !self.view.attached.contains(&fid) {
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
        self.drive_surfaces(&mut gfx, &surfaces);

        // ---- build quads ----
        let white = gfx.renderer.white;
        let full = [0.0, 0.0, fb[0], fb[1]];
        let mut quads: Vec<Quad> = Vec::new();

        // Connection wires, under the nodes: curved arrows from a source's output
        // port to a target's input port. The selected wire is drawn thicker in the
        // highlight colour.
        for &(file_id, app_id) in &self.view.connections {
            if let Some((a, b)) = self.wire_endpoints(Wire::Bind(file_id, app_id)) {
                let sel = self.wire_sel == Some(Wire::Bind(file_id, app_id));
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
        for &(app, cap) in &self.view.capture_links {
            if let Some((a, b)) = self.wire_endpoints(Wire::Capture(app, cap)) {
                let sel = self.wire_sel == Some(Wire::Capture(app, cap));
                let col = CAPTURE_BORDER;
                draw_connection(&mut quads, white, a, b, sel, col, zf, full);
            }
        }
        for &(app, clip) in &self.view.clipboard_links {
            if let Some((a, b)) = self.wire_endpoints(Wire::Clipboard(app, clip)) {
                let sel = self.wire_sel == Some(Wire::Clipboard(app, clip));
                let col = CLIPBOARD_BORDER;
                draw_connection(&mut quads, white, a, b, sel, col, zf, full);
            }
        }
        for &(app, api) in &self.view.api_links {
            if let Some((a, b)) = self.wire_endpoints(Wire::Api(app, api)) {
                let sel = self.wire_sel == Some(Wire::Api(app, api));
                let col = API_BORDER;
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

        // Boundary wires: a neighbour joined to one of an instance's ports.
        // The live wire this becomes lands on a node *inside* the instance, so
        // it is not on this canvas at all — without drawing the authored line
        // the tab would show an instance connected to nothing.
        for (&gid, g) in &self.view.groups {
            for (dir, wires) in [(PortDir::In, &g.in_wires), (PortDir::Out, &g.out_wires)] {
                for (name, node) in wires {
                    let Some(slot) = g.ports.iter().position(|p| p.dir == dir && p.name == *name)
                    else {
                        continue; // a port the definition no longer declares
                    };
                    let kind = g.ports[slot].kind;
                    let (Some(gp), true) = (
                        self.port_slot_pos(gid, slot),
                        self.view.win_pos.contains_key(node),
                    ) else {
                        continue;
                    };
                    let (a, b) = match dir {
                        PortDir::In => (self.port_pos(*node, kind, PortDir::Out), gp),
                        PortDir::Out => (gp, self.port_pos(*node, kind, PortDir::In)),
                    };
                    draw_connection(&mut quads, white, a, b, false, port_color(kind), zf, full);
                }
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

            // A volume node renders as a small labelled box with a port.
            // Volumes show their byte count; BindMounts show the
            // size plus a "disk" marker so they read as backed by a path.
            if let Some(file) = self.view.file_nodes.get(&id) {
                let name = file.name.clone();
                let (border, bg, status, status_col) = if file.host_mapped {
                    // A bind: a folder mirrors a tree, a file maps one path.
                    let kind = if file.is_dir { "dir" } else { "disk" };
                    (
                        HOSTFILE_BORDER,
                        HOSTFILE_BG,
                        format!("{} B · {kind}", file.size),
                        [0.55, 0.68, 0.85, 1.0],
                    )
                } else {
                    // A named volume: mark it when persistence is on.
                    let tail = if file.persist { " · persist" } else { "" };
                    (
                        FILE_BORDER,
                        FILE_BG,
                        format!("{} B{tail}", file.size),
                        [0.65, 0.6, 0.5, 1.0],
                    )
                };
                self.draw_widget(
                    &mut quads,
                    &mut gfx,
                    white,
                    zf,
                    mp,
                    clip,
                    full,
                    WidgetChrome {
                        id,
                        r,
                        border,
                        bg,
                        title: &name,
                        title_col: TEXT,
                        status: &status,
                        status_col,
                        status_scale: 0.85,
                        copy_ticket: false,
                    },
                );
                continue;
            }

            // A HostPort node: a labelled box exposing a wasi:http node to a
            // localhost port when wired.
            if let Some(&port) = self.view.host_ports.get(&id) {
                let serving = self.view.serves.values().any(|&hp| hp == id);
                let conflict = self.port_conflicts.contains(&port);
                // The live/idle state is the compact, colour-coded title; the
                // port (and any host→container map) is the roomy status line,
                // clear of the close button.
                let (state, state_col) = if conflict {
                    ("port in use", WARN)
                } else if serving {
                    ("live ●", [0.4, 0.85, 0.5, 1.0])
                } else {
                    ("idle", [0.55, 0.7, 0.72, 1.0])
                };
                let container = self
                    .view
                    .serves
                    .iter()
                    .find(|(_, &hp)| hp == id)
                    .and_then(|(&served, _)| self.view.serve_ports.get(&(served, id)).copied());
                let port_label = match container {
                    Some(c) if c != port => format!(":{port}→{c}"),
                    _ => format!(":{port}"),
                };
                self.draw_widget(
                    &mut quads,
                    &mut gfx,
                    white,
                    zf,
                    mp,
                    clip,
                    full,
                    WidgetChrome {
                        id,
                        r,
                        border: HOSTPORT_BORDER,
                        bg: HOSTPORT_BG,
                        title: state,
                        title_col: state_col,
                        status: &port_label,
                        status_col: TEXT,
                        status_scale: 0.85,
                        copy_ticket: false,
                    },
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
                let status = if is_gw {
                    format!("host • {members}")
                } else {
                    format!("{members} node(s)")
                };
                self.draw_widget(
                    &mut quads,
                    &mut gfx,
                    white,
                    zf,
                    mp,
                    clip,
                    full,
                    WidgetChrome {
                        id,
                        r,
                        border: NET_BORDER,
                        bg: NET_BG,
                        title: if is_gw { "Gateway" } else { "Network" },
                        title_col: TEXT,
                        status: &status,
                        status_col: [0.72, 0.62, 0.9, 1.0],
                        status_scale: 0.7,
                        copy_ticket: false,
                    },
                );
                continue;
            }

            // An uplink node (Iroh or Veilid): extends the Network it's wired
            // to onto a remote fabric. The status line doubles as the peer
            // ticket field (click, paste/type, Enter dials).
            if let Some(meta) = self.view.uplinks.get(&id).cloned() {
                let editing = matches!(&self.editing_args, Some((eid, _)) if *eid == id);
                let (status, status_col) = if editing {
                    // Show the tail of the in-progress ticket with a caret.
                    let text = match &self.editing_args {
                        Some((_, s)) => s.as_str(),
                        None => "",
                    };
                    let tail: String = text
                        .chars()
                        .rev()
                        .take(14)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect();
                    (format!("{tail}_"), TEXT)
                } else if self.just_copied(id) {
                    ("ticket copied".into(), [0.4, 0.85, 0.5, 1.0])
                } else if meta.peers > 0 {
                    (format!("● {} peer(s)", meta.peers), [0.4, 0.85, 0.5, 1.0])
                } else if self.view.node_args.get(&id).is_some_and(|a| !a.is_empty()) {
                    ("dialing…".into(), [0.72, 0.62, 0.9, 1.0])
                } else {
                    ("c = copy ticket".into(), [0.55, 0.7, 0.72, 1.0])
                };
                self.draw_widget(
                    &mut quads,
                    &mut gfx,
                    white,
                    zf,
                    mp,
                    clip,
                    full,
                    WidgetChrome {
                        id,
                        r,
                        border: NET_BORDER,
                        bg: NET_BG,
                        title: meta.kind.label(),
                        title_col: TEXT,
                        status: &status,
                        status_col,
                        status_scale: 0.7,
                        copy_ticket: true,
                    },
                );
                continue;
            }

            // A Screen Capture node: a capability widget (like Network) whose
            // status shows whether it's granting frames and capturing.
            if let Some(feed) = self.view.capture_feeds.get(&id) {
                let wired = self.view.capture_links.iter().any(|&(_, c)| c == id);
                let live = feed.lock().unwrap().seq > 0;
                let (status, status_col) = if wired && live {
                    ("● recording", [0.95, 0.45, 0.5, 1.0])
                } else if wired {
                    ("wired — waiting for frames", [0.8, 0.65, 0.5, 1.0])
                } else {
                    ("wire an app to grant capture", [0.55, 0.7, 0.72, 1.0])
                };
                self.draw_widget(
                    &mut quads,
                    &mut gfx,
                    white,
                    zf,
                    mp,
                    clip,
                    full,
                    WidgetChrome {
                        id,
                        r,
                        border: CAPTURE_BORDER,
                        bg: CAPTURE_BG,
                        title: "screen capture",
                        title_col: TEXT,
                        status,
                        status_col,
                        status_scale: 0.7,
                        copy_ticket: false,
                    },
                );
                continue;
            }

            // A Clipboard node: a capability widget whose status is the GRANT,
            // not the wire — see `clipboard_grant`.
            if self.view.clipboard_boards.contains_key(&id) {
                let (status, status_col) = self.clipboard_grant(id);
                self.draw_widget(
                    &mut quads,
                    &mut gfx,
                    white,
                    zf,
                    mp,
                    clip,
                    full,
                    WidgetChrome {
                        id,
                        r,
                        border: CLIPBOARD_BORDER,
                        bg: CLIPBOARD_BG,
                        title: "clipboard",
                        title_col: TEXT,
                        status: &status,
                        status_col,
                        status_scale: 0.7,
                        copy_ticket: false,
                    },
                );
                continue;
            }

            // A wk API node: a capability widget (like Network) whose status
            // shows whether an app is wired to drive wk.
            if self.view.api_nodes.contains(&id) {
                let wired = self.view.api_links.iter().any(|&(_, n)| n == id);
                let (status, status_col) = if wired {
                    ("● wired", [0.5, 0.85, 0.9, 1.0])
                } else {
                    ("wire an app to grant API access", [0.55, 0.7, 0.72, 1.0])
                };
                self.draw_widget(
                    &mut quads,
                    &mut gfx,
                    white,
                    zf,
                    mp,
                    clip,
                    full,
                    WidgetChrome {
                        id,
                        r,
                        border: API_BORDER,
                        bg: API_BG,
                        title: "wk api",
                        title_col: TEXT,
                        status,
                        status_col,
                        status_scale: 0.7,
                        copy_ticket: false,
                    },
                );
                continue;
            }

            // A hardware MIDI input node: a capability widget whose status shows
            // the connected device (or that it's waiting for one).
            if let Some(device) = self.view.midi_ins.get(&id).cloned() {
                let (status, status_col): (String, [f32; 4]) = if device.is_empty() {
                    (
                        "no MIDI device — plug one in".to_string(),
                        [0.8, 0.65, 0.5, 1.0],
                    )
                } else {
                    (device, [0.5, 0.85, 0.6, 1.0])
                };
                self.draw_widget(
                    &mut quads,
                    &mut gfx,
                    white,
                    zf,
                    mp,
                    clip,
                    full,
                    WidgetChrome {
                        id,
                        r,
                        border: MIDI_BORDER,
                        bg: MIDI_BG,
                        title: "MIDI in",
                        title_col: TEXT,
                        status: &status,
                        status_col,
                        status_scale: 0.7,
                        copy_ticket: false,
                    },
                );
                continue;
            }

            // A HostService node: a capability widget whose status shows the
            // host target its fabric name bridges to.
            if let Some(svc) = self.view.host_services.get(&id).cloned() {
                let wired = self.view.net_links.iter().any(|&(s, _)| s == id);
                let status = format!("→ {}", svc.target);
                let status_col = if wired {
                    [0.45, 0.85, 0.75, 1.0]
                } else {
                    [0.55, 0.7, 0.68, 1.0]
                };
                self.draw_widget(
                    &mut quads,
                    &mut gfx,
                    white,
                    zf,
                    mp,
                    clip,
                    full,
                    WidgetChrome {
                        id,
                        r,
                        border: HOSTSVC_BORDER,
                        bg: HOSTSVC_BG,
                        title: &svc.name,
                        title_col: TEXT,
                        status: &status,
                        status_col,
                        status_scale: 0.7,
                        copy_ticket: false,
                    },
                );
                continue;
            }

            // A boundary port: the smallest widget there is — its name, and
            // what may cross it, in that connection kind's own colour.
            if let Some(p) = self.view.boundary_ports.get(&id).cloned() {
                let col = port_color(p.kind);
                let status = format!("{} {}", port_label(p.dir), p.kind.as_str());
                self.draw_widget(
                    &mut quads,
                    &mut gfx,
                    white,
                    zf,
                    mp,
                    clip,
                    full,
                    WidgetChrome {
                        id,
                        r,
                        border: col,
                        // A dark wash of the kind's colour, so the port reads
                        // as belonging to its wire without shouting.
                        bg: [col[0] * 0.22, col[1] * 0.22, col[2] * 0.22, 1.0],
                        title: &p.name,
                        title_col: TEXT,
                        status: &status,
                        status_col: col,
                        status_scale: 0.7,
                        copy_ticket: false,
                    },
                );
                continue;
            }

            // A `group`: one instance of another workspace. It runs nothing
            // itself, so the widget is the definition's name and the size of
            // what it stands for; its edges are the definition's own ports.
            if let Some(g) = self.view.groups.get(&id).cloned() {
                let status = group_status(&g);
                self.draw_widget(
                    &mut quads,
                    &mut gfx,
                    white,
                    zf,
                    mp,
                    clip,
                    full,
                    WidgetChrome {
                        id,
                        r,
                        border: GROUP_BORDER,
                        bg: GROUP_BG,
                        title: &g.definition,
                        title_col: TEXT,
                        status: &status,
                        status_col: GROUP_BORDER,
                        status_scale: 0.7,
                        copy_ticket: false,
                    },
                );
                continue;
            }

            // A note: a yellow annotation panel (no ports, no title bar).
            if let Some(note_text) = self.view.notes.get(&id) {
                let (text, editing) = match &self.editing_note {
                    Some((eid, buf)) if *eid == id => (buf.clone(), true),
                    _ => (note_text.clone(), false),
                };
                self.draw_note(&mut quads, &mut gfx, white, zf, r, clip, mp, &text, editing);
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
                    format!("{} (compiling…)", node.name)
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

            // Files button: opens the node's virtual-filesystem inspector.
            // Drawn as a small "document" icon (a box with a couple of lines).
            let fbn = files_btn(r, zf);
            let open = matches!(&self.inspect, Some(i) if i.node == id);
            if open || contains(fbn, mp) {
                quads.push(Quad::solid(white, fbn, TITLE_FOCUS, clip));
            }
            {
                let pad = (fbn[2] - fbn[0]) * 0.26;
                let doc = [fbn[0] + pad, fbn[1] + pad, fbn[2] - pad, fbn[3] - pad];
                quads.push(Quad::solid(white, doc, TEXT, clip));
                // Two "text" lines inside the document.
                let lw = (doc[2] - doc[0]) * 0.6;
                let lh = (doc[3] - doc[1]) * 0.13;
                let lc = if open || contains(fbn, mp) {
                    TITLE_FOCUS
                } else {
                    panel
                };
                for k in 0..2 {
                    let ly = doc[1] + (doc[3] - doc[1]) * (0.32 + 0.28 * k as f32);
                    quads.push(Quad::solid(
                        white,
                        [doc[0] + lh, ly, doc[0] + lh + lw, ly + lh],
                        lc,
                        clip,
                    ));
                }
            }

            // Logs button: opens the node's output-log panel. Drawn as a few
            // left-aligned "log lines" of varying length.
            let lbn = logs_btn(r, zf);
            let logs_open = matches!(&self.logs, Some(l) if l.node == id);
            if logs_open || contains(lbn, mp) {
                quads.push(Quad::solid(white, lbn, TITLE_FOCUS, clip));
            }
            {
                let pad = (lbn[2] - lbn[0]) * 0.28;
                let inner = [lbn[0] + pad, lbn[1] + pad, lbn[2] - pad, lbn[3] - pad];
                let lh = (inner[3] - inner[1]) * 0.14;
                let full_w = inner[2] - inner[0];
                // Three rows of "text", the middle one shorter.
                for (k, frac) in [1.0_f32, 0.6, 0.85].into_iter().enumerate() {
                    let ly = inner[1] + (inner[3] - inner[1]) * (0.12 + 0.38 * k as f32);
                    quads.push(Quad::solid(
                        white,
                        [inner[0], ly, inner[0] + full_w * frac, ly + lh],
                        TEXT,
                        clip,
                    ));
                }
            }

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
                    MUTED_TEXT,
                    ca_clip,
                );
            }
            if self.detached.contains_key(&id) || self.view.attached.contains(&id) {
                // Popped out into its own OS window, or attached by a CLI client
                // (`docker attach`) — either way the live content isn't ours to
                // render here. Crucially we must NOT resize an attached node's
                // terminal below: the CLI owns its size, and re-deriving it from
                // this canvas rect each frame would clobber the client's resize.
                let remote = self.view.attached.contains(&id) && !self.detached.contains_key(&id);
                quads.push(Quad::solid(white, ca, DETACHED_BG, ca_clip));
                let msg = if remote {
                    "detached (remote)"
                } else {
                    "detached"
                };
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
                    MUTED_TEXT,
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
            } else if self.terminals.contains_key(&id) {
                // Size the grid to the node's canvas rect, independent of zoom:
                // divide by the on-screen cell size (base metric × zoom) so a
                // bigger node shows more cells while zooming just scales them.
                let bw = (gfx.fonts.measure("M") as f32).max(1.0);
                let bh = (gfx.fonts.line_height() as f32).max(1.0);
                let cols = (((ca[2] - ca[0]) / (bw * zf)).floor() as i32).clamp(1, 500) as u16;
                let rows = (((ca[3] - ca[1]) / (bh * zf)).floor() as i32).clamp(1, 300) as u16;
                if let Some(t) = self.terminals.get_mut(&id) {
                    t.resize(cols, rows);
                }
                let (cells, cursor) = self
                    .terminals
                    .get(&id)
                    .map(|t| (t.cells(), t.cursor()))
                    .unwrap();
                self.draw_term_grid(
                    &mut quads,
                    &mut gfx,
                    &cells,
                    cursor,
                    ca,
                    ca_clip,
                    (cols, rows),
                );
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

            self.draw_typed_ports(&mut quads, gfx.renderer.circle, id, zf, mp, full);
        }

        // The selected bind wire's mount-path label, at the wire's midpoint:
        // where the source (volume or fs-provider app) mounts inside the app.
        // Click it to edit — Enter commits (blank resets to the default),
        // Escape cancels.
        if let Some(Wire::Bind(f, a)) = self.wire_sel {
            if let Some(r) = self.mount_label_rect(&gfx.fonts, f, a) {
                quads.push(Quad::solid(white, r, BORDER_COL, full));
                let inset = [r[0] + 1.0, r[1] + 1.0, r[2] - 1.0, r[3] - 1.0];
                quads.push(Quad::solid(white, inset, MENU_BG, full));
                let label = self.mount_label_text(f, a);
                let lh = gfx.fonts.line_height() as f32;
                self.text_cache.draw(
                    &mut quads,
                    &mut gfx.renderer,
                    &gfx.fonts,
                    &gfx.device,
                    &gfx.queue,
                    &label,
                    r[0] + PAD,
                    r[1] + (r[3] - r[1] - lh) * 0.5,
                    1.0,
                    TEXT,
                    full,
                );
            }
        }

        // The wire being dragged out of a typed output port toward the cursor —
        // same curved arrow as a finished connection, in that kind's colour.
        if let Some(d) = &self.drag {
            if let DragMode::Connect(p) = d.mode {
                let from = self
                    .port_slot_pos(d.id, p.slot)
                    .unwrap_or_else(|| self.port_pos(d.id, p.kind, PortDir::Out));
                draw_connection(
                    &mut quads,
                    white,
                    from,
                    mp,
                    false,
                    port_color(p.kind),
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
                let label = self.tab_label(i, id);
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
        self.draw_palette(&mut quads, &mut gfx, fb, mp);

        // Filesystem inspector (Files button): dim the canvas, then a centred
        // panel listing the node's live virtual filesystem with a file preview.
        if let Some(insp) = &self.inspect {
            let node = node_by_id.get(&insp.node);
            let node_name = node.map(|n| n.name.clone()).unwrap_or_default();
            let entries = node
                .map(|n| inspect_listing(&self.browse, n, &insp.dir))
                .unwrap_or_default();
            let (panel, close, up, rows, preview) = self.inspect_regions(fb, entries.len());
            let (_, _, _, _, row_h) = inspect_layout(fb);
            let dim = [0.55, 0.58, 0.64, 1.0];

            quads.push(Quad::solid(white, full, [0.0, 0.0, 0.0, 0.45], full));
            quads.push(Quad::solid(white, panel, BORDER_COL, full));
            let inset = [
                panel[0] + 1.0,
                panel[1] + 1.0,
                panel[2] - 1.0,
                panel[3] - 1.0,
            ];
            quads.push(Quad::solid(white, inset, BODY, full));

            // Title row: node name + current directory path (+ the image's
            // layer count for a container node).
            let n_layers = node_by_id
                .get(&insp.node)
                .map(|n| n.layers.len())
                .unwrap_or(0);
            let title = if n_layers > 0 {
                format!(
                    "files: {node_name}  ·  image: {n_layers} layer{}    /{}",
                    if n_layers == 1 { "" } else { "s" },
                    insp.dir.trim_start_matches('/')
                )
            } else {
                format!(
                    "files: {node_name}    /{}",
                    insp.dir.trim_start_matches('/')
                )
            };
            self.text_cache.draw(
                &mut quads,
                &mut gfx.renderer,
                &gfx.fonts,
                &gfx.device,
                &gfx.queue,
                &title,
                panel[0] + PAD,
                panel[1] + (row_h - lh) * 0.5,
                1.0,
                TEXT,
                [panel[0], panel[1], close[0] - 4.0, panel[1] + row_h],
            );
            if contains(close, mp) {
                quads.push(Quad::solid(white, close, TITLE_FOCUS, full));
            }
            self.text_cache.draw(
                &mut quads,
                &mut gfx.renderer,
                &gfx.fonts,
                &gfx.device,
                &gfx.queue,
                "x",
                close[0] + (close[2] - close[0]) * 0.28,
                close[1] + (close[3] - close[1]) * 0.02,
                0.8,
                TEXT,
                full,
            );

            // The ".." (parent) row.
            if let Some(ur) = up {
                if contains(ur, mp) {
                    quads.push(Quad::solid(white, ur, TITLE_FOCUS, full));
                }
                self.text_cache.draw(
                    &mut quads,
                    &mut gfx.renderer,
                    &gfx.fonts,
                    &gfx.device,
                    &gfx.queue,
                    "../",
                    ur[0] + PAD,
                    ur[1] + (row_h - lh) * 0.5,
                    1.0,
                    dim,
                    ur,
                );
            }

            // Directory entries: dirs (blue, trailing "/") then files (+ size).
            for &(rr, idx) in &rows {
                let e = &entries[idx];
                let selected = insp.file.as_deref() == Some(e.name.as_str());
                if contains(rr, mp) || selected {
                    quads.push(Quad::solid(white, rr, TITLE_FOCUS, full));
                }
                let label = if e.is_dir {
                    format!("{}/", e.name)
                } else {
                    e.name.clone()
                };
                let col = if e.is_dir {
                    [0.6, 0.78, 0.95, 1.0]
                } else {
                    TEXT
                };
                self.text_cache.draw(
                    &mut quads,
                    &mut gfx.renderer,
                    &gfx.fonts,
                    &gfx.device,
                    &gfx.queue,
                    &label,
                    rr[0] + PAD,
                    rr[1] + (row_h - lh) * 0.5,
                    1.0,
                    col,
                    [rr[0], rr[1], rr[2] - PAD, rr[3]],
                );
                use wk_server::vfs::PathKind;
                if e.is_dir {
                    // A directory entry with a `Mounted` origin is a provider
                    // mount point (an fs-provider app's served tree). Name the
                    // provider — the wires + mount paths already say which one
                    // lands here, so no guest is asked.
                    if e.origin == PathKind::Mounted {
                        let tag =
                            provider_serving(&self.view, insp.node, &insp.child_path(&e.name))
                                .map(|n| format!("served by {n}"))
                                .unwrap_or_else(|| "mount".to_string());
                        let tw = gfx.fonts.measure(&tag) as f32 * 0.85;
                        self.text_cache.draw(
                            &mut quads,
                            &mut gfx.renderer,
                            &gfx.fonts,
                            &gfx.device,
                            &gfx.queue,
                            &tag,
                            rr[2] - PAD - tw,
                            rr[1] + (row_h - lh * 0.85) * 0.5,
                            0.85,
                            [0.72, 0.58, 0.85, 1.0],
                            rr,
                        );
                    }
                } else {
                    // Provenance badge: canvas mounts always; layer/edited
                    // distinctions only for container nodes (a plain node's
                    // files are all "written" — no signal in saying so).
                    let badge: Option<(&str, [f32; 4])> = match e.origin {
                        PathKind::Mounted => Some(("mount", [0.72, 0.58, 0.85, 1.0])),
                        PathKind::LayerFile if n_layers > 0 => {
                            Some(("layer", [0.5, 0.65, 0.85, 1.0]))
                        }
                        PathKind::PrivateFile if n_layers > 0 => {
                            Some(("edited", [0.85, 0.72, 0.45, 1.0]))
                        }
                        _ => None,
                    };
                    let sz = human_size(e.size);
                    let sw = gfx.fonts.measure(&sz) as f32;
                    self.text_cache.draw(
                        &mut quads,
                        &mut gfx.renderer,
                        &gfx.fonts,
                        &gfx.device,
                        &gfx.queue,
                        &sz,
                        rr[2] - PAD - sw,
                        rr[1] + (row_h - lh) * 0.5,
                        1.0,
                        dim,
                        rr,
                    );
                    if let Some((tag, col)) = badge {
                        let tw = gfx.fonts.measure(tag) as f32 * 0.85;
                        self.text_cache.draw(
                            &mut quads,
                            &mut gfx.renderer,
                            &gfx.fonts,
                            &gfx.device,
                            &gfx.queue,
                            tag,
                            rr[2] - PAD - sw - 12.0 - tw,
                            rr[1] + (row_h - lh * 0.85) * 0.5,
                            0.85,
                            col,
                            rr,
                        );
                    }
                }
            }
            // Overflowing listing: a thin scrollbar along the right edge shows
            // where the visible window sits (and that there's more to reach).
            let has_up = !insp.dir.is_empty();
            let visible = inspect_rows_fit(fb).saturating_sub(has_up as usize).max(1);
            if entries.len() > visible {
                let list_top = panel[1] + row_h + if has_up { row_h } else { 0.0 };
                let list_bottom = preview[1];
                let track_h = (list_bottom - list_top).max(1.0);
                let frac_pos = insp.scroll.floor().max(0.0) / entries.len() as f32;
                let frac_len = visible as f32 / entries.len() as f32;
                let ty = list_top + track_h * frac_pos;
                let th = (track_h * frac_len).max(12.0);
                quads.push(Quad::solid(
                    white,
                    [panel[2] - 6.0, list_top, panel[2] - 3.0, list_bottom],
                    [0.16, 0.17, 0.21, 1.0],
                    full,
                ));
                quads.push(Quad::solid(
                    white,
                    [
                        panel[2] - 6.0,
                        ty,
                        panel[2] - 3.0,
                        (ty + th).min(list_bottom),
                    ],
                    [0.45, 0.48, 0.56, 1.0],
                    full,
                ));
            }
            if entries.is_empty() {
                self.text_cache.draw(
                    &mut quads,
                    &mut gfx.renderer,
                    &gfx.fonts,
                    &gfx.device,
                    &gfx.queue,
                    "(empty directory)",
                    panel[0] + PAD,
                    panel[1] + row_h * 1.4,
                    1.0,
                    dim,
                    full,
                );
            }

            // Preview strip: a separator line then the selected file's text.
            quads.push(Quad::solid(
                white,
                [preview[0], preview[1], preview[2], preview[1] + 1.0],
                BORDER_COL,
                full,
            ));
            match &insp.file {
                Some(fname) => {
                    let path = insp.child_path(fname);
                    let bytes = node
                        .and_then(|n| inspect_preview(&self.browse, n, &path, INSPECT_PREVIEW_CAP))
                        .unwrap_or_default();
                    let header = format!("{fname}  ({})", human_size(bytes.len()));
                    self.text_cache.draw(
                        &mut quads,
                        &mut gfx.renderer,
                        &gfx.fonts,
                        &gfx.device,
                        &gfx.queue,
                        &header,
                        preview[0] + PAD,
                        preview[1] + 4.0,
                        0.85,
                        dim,
                        preview,
                    );
                    let text = String::from_utf8_lossy(&bytes);
                    let top = preview[1] + 4.0 + lh;
                    let max_lines = ((preview[3] - top - 4.0) / lh).floor() as usize;
                    for (li, line) in text.lines().take(max_lines).enumerate() {
                        // Sanitize control bytes so they don't corrupt the grid.
                        let clean: String = line
                            .chars()
                            .map(|c| {
                                if c == '\t' {
                                    ' '
                                } else if c.is_control() {
                                    '\u{fffd}'
                                } else {
                                    c
                                }
                            })
                            .take(400)
                            .collect();
                        self.text_cache.draw(
                            &mut quads,
                            &mut gfx.renderer,
                            &gfx.fonts,
                            &gfx.device,
                            &gfx.queue,
                            &clean,
                            preview[0] + PAD,
                            top + li as f32 * lh,
                            0.9,
                            TEXT,
                            preview,
                        );
                    }
                }
                None => {
                    self.text_cache.draw(
                        &mut quads,
                        &mut gfx.renderer,
                        &gfx.fonts,
                        &gfx.device,
                        &gfx.queue,
                        "select a file to preview",
                        preview[0] + PAD,
                        preview[1] + 6.0,
                        0.9,
                        dim,
                        preview,
                    );
                }
            }
        }

        // Output-log panel (Logs button): dim the canvas, then a centred panel
        // showing the node's captured stdout/stderr scrollback, tailed.
        if let Some((log_node, scroll)) = self.logs.as_ref().map(|l| (l.node, l.scroll.max(0.0))) {
            let node = node_by_id.get(&log_node);
            let node_name = node.map(|n| n.name.clone()).unwrap_or_default();
            let bytes = node.map(|n| n.term_io.log_read(0).0).unwrap_or_default();
            let dim = [0.55, 0.58, 0.64, 1.0];
            let (x, y, w, h, row_h) = inspect_layout(fb);
            let panel = [x, y, x + w, y + h];
            let close = {
                let s = row_h - 8.0;
                [x + w - s - 6.0, y + 4.0, x + w - 6.0, y + 4.0 + s]
            };

            quads.push(Quad::solid(white, full, [0.0, 0.0, 0.0, 0.45], full));
            quads.push(Quad::solid(white, panel, BORDER_COL, full));
            let inset = [
                panel[0] + 1.0,
                panel[1] + 1.0,
                panel[2] - 1.0,
                panel[3] - 1.0,
            ];
            quads.push(Quad::solid(white, inset, BODY, full));

            // Wrap the log to the panel width (monospace estimate from a sample).
            let char_w = (gfx.fonts.measure("mmmmmmmmmm") as f32 / 10.0).max(1.0);
            let cols = (((w - 2.0 * PAD) / char_w).floor() as usize).max(1);
            let lines = log_lines(&bytes, cols);
            let list_top = panel[1] + row_h;
            let visible = (((panel[3] - list_top - PAD) / lh).floor() as usize).max(1);
            let max_scroll = lines.len().saturating_sub(visible);
            self.log_max_scroll = max_scroll as f32;
            let scroll_i = (scroll.floor() as usize).min(max_scroll);
            let end = lines.len().saturating_sub(scroll_i);
            let start = end.saturating_sub(visible);

            // Title row: node name + line count (and a "tailing" hint at bottom).
            let title = if scroll_i == 0 {
                format!("logs: {node_name}    {} lines", lines.len())
            } else {
                format!(
                    "logs: {node_name}    {} lines  ·  +{scroll_i} up",
                    lines.len()
                )
            };
            self.text_cache.draw(
                &mut quads,
                &mut gfx.renderer,
                &gfx.fonts,
                &gfx.device,
                &gfx.queue,
                &title,
                panel[0] + PAD,
                panel[1] + (row_h - lh) * 0.5,
                1.0,
                TEXT,
                [panel[0], panel[1], close[0] - 4.0, panel[1] + row_h],
            );
            if contains(close, mp) {
                quads.push(Quad::solid(white, close, TITLE_FOCUS, full));
            }
            self.text_cache.draw(
                &mut quads,
                &mut gfx.renderer,
                &gfx.fonts,
                &gfx.device,
                &gfx.queue,
                "x",
                close[0] + (close[2] - close[0]) * 0.28,
                close[1] + (close[3] - close[1]) * 0.02,
                0.8,
                TEXT,
                full,
            );

            if lines.is_empty() {
                self.text_cache.draw(
                    &mut quads,
                    &mut gfx.renderer,
                    &gfx.fonts,
                    &gfx.device,
                    &gfx.queue,
                    "(no output yet)",
                    panel[0] + PAD,
                    list_top + 4.0,
                    1.0,
                    dim,
                    full,
                );
            }
            for (i, line) in lines[start..end].iter().enumerate() {
                self.text_cache.draw(
                    &mut quads,
                    &mut gfx.renderer,
                    &gfx.fonts,
                    &gfx.device,
                    &gfx.queue,
                    line,
                    panel[0] + PAD,
                    list_top + i as f32 * lh,
                    1.0,
                    TEXT,
                    [panel[0], list_top, panel[2] - PAD, panel[3]],
                );
            }
            // Scrollbar: where the visible window sits within the whole log.
            if lines.len() > visible {
                let list_bottom = panel[3] - PAD;
                let track_h = (list_bottom - list_top).max(1.0);
                let frac_pos = start as f32 / lines.len() as f32;
                let frac_len = visible as f32 / lines.len() as f32;
                let ty = list_top + track_h * frac_pos;
                let th = (track_h * frac_len).max(12.0);
                quads.push(Quad::solid(
                    white,
                    [panel[2] - 6.0, list_top, panel[2] - 3.0, list_bottom],
                    [0.16, 0.17, 0.21, 1.0],
                    full,
                ));
                quads.push(Quad::solid(
                    white,
                    [
                        panel[2] - 6.0,
                        ty,
                        panel[2] - 3.0,
                        (ty + th).min(list_bottom),
                    ],
                    [0.45, 0.48, 0.56, 1.0],
                    full,
                ));
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
        // Screen Capture: when a capture node has a wired app, read the canvas
        // back (throttled to ~every 6th frame) and publish it to the node's
        // frame slot for guests to poll via wk:capture.
        self.capture_tick = self.capture_tick.wrapping_add(1);
        let feeds: Vec<_> = if self.capture_tick.is_multiple_of(6) {
            self.view
                .capture_feeds
                .iter()
                .filter(|(id, _)| self.view.capture_links.iter().any(|(_, c)| c == *id))
                .map(|(_, f)| f.clone())
                .collect()
        } else {
            Vec::new()
        };
        let staging = if feeds.is_empty() {
            None
        } else {
            let (w, h) = (gfx.surface_desc.width, gfx.surface_desc.height);
            let bpr = (w * 4).div_ceil(256) * 256;
            let buf = gfx.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("canvas capture"),
                size: (bpr * h) as u64,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            encoder.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo {
                    texture: &frame.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyBufferInfo {
                    buffer: &buf,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(bpr),
                        rows_per_image: Some(h),
                    },
                },
                wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
            );
            Some((buf, bpr, w, h))
        };
        gfx.queue.submit([encoder.finish()]);
        if let Some((buf, bpr, w, h)) = staging {
            let slice = buf.slice(..);
            let (tx, rx) = std::sync::mpsc::channel();
            slice.map_async(wgpu::MapMode::Read, move |r| {
                let _ = tx.send(r);
            });
            let _ = gfx.device.poll(wgpu::PollType::wait_indefinitely());
            if matches!(rx.try_recv(), Ok(Ok(()))) {
                let data = slice.get_mapped_range();
                // Tightly pack, converting the surface's BGRA to RGBA.
                let bgra = matches!(
                    gfx.surface_desc.format,
                    wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
                );
                let mut pixels = vec![0u8; (w * h * 4) as usize];
                for row in 0..h as usize {
                    let src = &data[row * bpr as usize..row * bpr as usize + (w * 4) as usize];
                    let dst = &mut pixels[row * (w * 4) as usize..(row + 1) * (w * 4) as usize];
                    dst.copy_from_slice(src);
                    if bgra {
                        for px in dst.chunks_exact_mut(4) {
                            px.swap(0, 2);
                        }
                    }
                }
                drop(data);
                buf.unmap();
                for feed in &feeds {
                    let mut slot = feed.lock().unwrap();
                    slot.seq = slot.seq.wrapping_add(1).max(1);
                    slot.width = w;
                    slot.height = h;
                    slot.data.clone_from(&pixels);
                }
            }
        }
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
        // Drop windows only for nodes that vanished from *every* workspace
        // (closed elsewhere) — a node detached in another tab keeps its window.
        self.detached.retain(|id, _| all_node_ids.contains(id));
        let det_ids: Vec<NodeId> = self.detached.keys().copied().collect();
        for id in det_ids {
            let (mouse, buttons, keys, term_in, scroll) = {
                let det = self.detached.get_mut(&id).unwrap();
                let out = (
                    det.mouse,
                    [
                        (PointerButton::Left, det.lmb, det.prev_lmb),
                        (PointerButton::Right, det.rmb, det.prev_rmb),
                        (PointerButton::Middle, det.mmb, det.prev_mmb),
                    ],
                    std::mem::take(&mut det.key_events),
                    std::mem::take(&mut det.term_input),
                    std::mem::take(&mut det.scroll),
                );
                det.prev_lmb = det.lmb;
                det.prev_rmb = det.rmb;
                det.prev_mmb = det.mmb;
                out
            };
            // Forward the detached window's input straight to the node — the
            // window's size is the surface size, so coordinates map 1:1.
            if let Some(surf) = node_surface.get(&id) {
                let mut s = surf.lock().unwrap();
                let at = |button| PointerEvent {
                    x: mouse[0] as f64,
                    y: mouse[1] as f64,
                    button,
                };
                s.pointer_move.push_back(at(None));
                for (btn, held, prev) in buttons {
                    if held && !prev {
                        s.pointer_down.push_back(at(Some(btn)));
                    }
                    if !held && prev {
                        s.pointer_up.push_back(at(Some(btn)));
                    }
                }
                // Wheel events reach the node only if it asked for them.
                if s.wants_scroll {
                    s.pointer_scroll.extend(scroll);
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
        self.prev_rmb = self.rmb;
        self.prev_mmb = self.mmb;
        self.gfx = Some(gfx);
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.gfx.is_none() && !self.headless {
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
    fn device_event(&mut self, _el: &ActiveEventLoop, _id: DeviceId, event: DeviceEvent) {
        // Raw (unaccelerated-position) mouse deltas: the only usable look
        // input while the cursor is grabbed and frozen in 3D look mode.
        if let DeviceEvent::MouseMotion { delta } = event {
            if self.mode_3d && self.rmb {
                self.look_delta[0] += delta.0 as f32;
                self.look_delta[1] += delta.1 as f32;
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // The host's Ctrl-C: leave the loop gracefully (the caller shuts the
        // server down, which persists). The only exit from headless mode.
        if self.interrupt.load(std::sync::atomic::Ordering::Relaxed) {
            event_loop.exit();
            return;
        }
        // A palette "Quit" command asks to exit on the next loop.
        if self.request_exit {
            event_loop.exit();
            return;
        }
        // Palette "Go headless": drop every window and keep pumping — the
        // server and its nodes run on. Never leave the loop: on macOS a
        // parked main thread stops servicing the app runloop (beachball),
        // and window teardown itself needs events pumped.
        if self.request_headless && !self.headless {
            self.request_headless = false;
            self.headless = true;
            self.detached.clear();
            self.gfx = None;
            #[cfg(target_os = "macos")]
            {
                use winit::platform::macos::ActiveEventLoopExtMacOS;
                event_loop.hide_application();
            }
            eprintln!(
                "wk: headless — nodes keep running; `wk ps`/`logs`/`attach` still work; Ctrl-C stops and saves"
            );
            return;
        }
        // The clipboard pump runs ABOVE the headless guard on purpose: it is
        // the only host-facing capability here that does not need a window.
        // `arboard` needs a display CONNECTION, which survives "Go headless"
        // — so a node wired to a Clipboard node keeps copying and pasting
        // after the canvas is gone. Putting it in `frame()` beside the capture
        // pump would freeze it the moment the user went headless.
        self.pump_clipboard();
        if self.headless {
            return; // nothing to draw, nobody watching
        }
        if self.gfx.is_some() {
            self.sync_look_capture();
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
            // Coming back to wk is exactly when the host clipboard is most
            // likely to have changed behind our back ("copy in a browser,
            // switch to wk, paste"), so force the throttled poll to fire on
            // the next pass instead of waiting out its interval.
            WindowEvent::Focused(true) => self.clip_polled = None,
            WindowEvent::CursorMoved { position, .. } => {
                // (3D look mode reads raw `DeviceEvent::MouseMotion` deltas —
                // the cursor is grabbed and frozen while it's held.)
                self.mouse = [(position.x / scale) as f32, (position.y / scale) as f32];
            }
            // OS file drag-and-drop: each dropped path becomes a BindMount
            // node already pointed at it — one undoable create, no
            // create-then-type-the-path dance. Placed at the cursor's canvas
            // spot in 2D (winit reports no drop coordinates, so this is the
            // last position the window saw — usually where the drag entered);
            // the 3D view uses the palette's usual staggered slot. A
            // multi-file drop staggers so nodes don't stack.
            WindowEvent::HoveredFile(_) if !self.drop_hovering => {
                self.drop_hovering = true;
                self.drop_stagger = 0;
            }
            WindowEvent::HoveredFile(_) => {}
            WindowEvent::HoveredFileCancelled => self.drop_hovering = false,
            WindowEvent::DroppedFile(path) => {
                self.drop_hovering = false;
                let n = self.drop_stagger as f32;
                self.drop_stagger += 1;
                let pos = if self.mode_3d {
                    self.next_file_pos()
                } else {
                    let c = self.cam.to_canvas(self.mouse);
                    [
                        c[0] - FILE_W * 0.5 + n * 28.0,
                        c[1] - FILE_H * 0.5 + n * 28.0,
                    ]
                };
                self.conn.send(Command::Create(Resource::HostMount {
                    path: path.to_string_lossy().into_owned(),
                    pos,
                    ws: self.active_ws,
                }));
            }
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } => self.lmb = state == ElementState::Pressed,
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Right,
                ..
            } => self.rmb = state == ElementState::Pressed,
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Middle,
                ..
            } => self.mmb = state == ElementState::Pressed,
            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x, y),
                    MouseScrollDelta::PixelDelta(p) => (p.x as f32 / 50.0, p.y as f32 / 50.0),
                };
                // In the 3D view the wheel flies the camera along its gaze
                // (or scrolls the palette list while that's open).
                if self.mode_3d {
                    if self.palette_open {
                        let max = Self::palette_max_scroll(self.palette_filtered().len());
                        self.palette_scroll = (self.palette_scroll - dy).clamp(0.0, max);
                    } else {
                        self.fly_scroll += dy;
                    }
                    return;
                }
                // While the palette is open, the wheel scrolls its list instead
                // of panning the canvas.
                if self.palette_open {
                    let max = Self::palette_max_scroll(self.palette_filtered().len());
                    self.palette_scroll = (self.palette_scroll - dy).clamp(0.0, max);
                    return;
                }
                // The (modal) log panel: the wheel scrolls its scrollback (up =
                // older), clamped by the max computed when it last drew.
                if self.logs.is_some() {
                    let max = self.log_max_scroll;
                    if let Some(lv) = &mut self.logs {
                        lv.scroll = (lv.scroll + dy).clamp(0.0, max);
                    }
                    return;
                }
                // Likewise the (modal) file inspector: the wheel scrolls its
                // listing, clamped so the tail stays reachable but no further.
                if let Some(insp) = &self.inspect {
                    let (node, dir) = (insp.node, insp.dir.clone());
                    let len = self
                        .app_node(node)
                        .and_then(|n| n.fs.lock().unwrap().list_dir(&dir).map(|v| v.len()))
                        .unwrap_or(0);
                    let max = inspect_max_scroll(self.viewport, len, !dir.is_empty()) as f32;
                    if let Some(insp) = &mut self.inspect {
                        insp.scroll = (insp.scroll - dy).clamp(0.0, max);
                    }
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
                // Over the content of a surface that subscribed to scroll (and
                // with no zoom modifier held), the wheel belongs to the guest:
                // deliver a surface-local scroll event instead of panning.
                // Surfaces that never subscribed (paint, piano, …) keep the
                // canvas panning over them exactly as before.
                let zooming = self.mods.control_key() || self.mods.super_key();
                if !zooming && self.drag.is_none() {
                    if let Some(id) = self.topmost_under(self.mouse) {
                        let ca = content_rect(self.rect_of(id), self.cam.zoom);
                        if contains(ca, self.mouse) {
                            if let Some(surf) = self
                                .view
                                .surfaces
                                .iter()
                                .find(|s| s.lock().unwrap().node_id == id)
                            {
                                let mut s = surf.lock().unwrap();
                                if s.wants_scroll {
                                    let zf = self.cam.zoom;
                                    s.pointer_scroll.push_back(ScrollEvent {
                                        x: ((self.mouse[0] - ca[0]) / zf) as f64,
                                        y: ((self.mouse[1] - ca[1]) / zf) as f64,
                                        delta_x: dx as f64,
                                        delta_y: dy as f64,
                                    });
                                    return;
                                }
                            }
                        }
                    }
                }
                if zooming {
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
                    // Held-key state for the 3D fly camera (maintained in both
                    // modes so a key released after leaving 3D isn't stuck).
                    if pressed {
                        self.keys_down.insert(code);
                    } else {
                        self.keys_down.remove(&code);
                    }
                    // The character this key types, resolved here for the same
                    // reason `keys_down` is maintained here: it is held-key
                    // state, and nearly every branch below can swallow the
                    // event and `return` (the palette, the modals, wk's own
                    // Cmd chords). A memo updated only on the paths that reach
                    // a guest goes stale — a release eaten by the palette
                    // leaves its press text behind for some later keystroke to
                    // replay. Resolving up front costs one hash lookup and
                    // keeps the memo honest.
                    let text = self.key_text.resolve(code, event.text.as_ref(), pressed);
                    // The 3D view owns the keyboard: WASD/QE fly (read from
                    // `keys_down` each frame). Cmd/Ctrl+K still opens the
                    // palette, which then captures keys (incl. paste); Escape
                    // returns to the canvas.
                    if self.mode_3d {
                        let app_chord = self.mods.super_key() || self.mods.control_key();
                        if pressed && !event.repeat && app_chord && code == KeyCode::KeyK {
                            self.palette_open = !self.palette_open;
                            self.palette_query.clear();
                            self.palette_sel = 0;
                            self.palette_scroll = 0.0;
                            return;
                        }
                        if self.palette_open {
                            if pressed && app_chord && code == KeyCode::KeyV {
                                if let Some(text) = self.clipboard_text() {
                                    self.palette_query.push_str(&text);
                                    self.palette_sel = 0;
                                    self.palette_scroll = 0.0;
                                }
                            } else if pressed {
                                self.palette_key(code, event.text.as_deref());
                            }
                            return;
                        }
                        // With an active node and the camera not in look mode
                        // (right button), keys queue for that node — including
                        // Escape (vim lives on it). Unfocus by clicking empty
                        // space; Escape exits 3D only when nothing is focused.
                        if self.kbd_focus.is_some() && !self.rmb {
                            if pressed {
                                if let Some(bytes) =
                                    encode_term_key(code, event.text.as_deref(), self.mods)
                                {
                                    self.term_input.extend(bytes);
                                }
                            }
                            self.key_events
                                .push((key_event(code, text, self.mods, event.repeat), pressed));
                            return;
                        }
                        if pressed && !event.repeat && code == KeyCode::KeyF {
                            self.fly3d = !self.fly3d;
                            return;
                        }
                        if pressed && code == KeyCode::Escape {
                            self.mode_3d = false;
                        }
                        return;
                    }
                    // Cmd/Ctrl+K toggles the command palette.
                    // An "app chord" is Cmd (macOS) always, or Ctrl only when no
                    // app/terminal is focused — so a focused terminal keeps its
                    // control keys (Ctrl+C/D/K/W/Z go to the shell, not to wk's
                    // duplicate/palette/close-tab/undo). Use Cmd, or click empty
                    // canvas to unfocus, to reach these while a terminal is up.
                    let app_chord = self.mods.super_key()
                        || (self.mods.control_key() && self.kbd_focus.is_none());
                    let paste = pressed
                        && (self.mods.super_key() || self.mods.control_key())
                        && code == KeyCode::KeyV;
                    // Paste into whichever text field is capturing input.
                    if paste
                        && (self.palette_open
                            || self.editing_args.is_some()
                            || self.editing_note.is_some())
                    {
                        if let Some(text) = self.clipboard_text() {
                            if self.palette_open {
                                self.palette_query.push_str(&text);
                                self.palette_sel = 0;
                                self.palette_scroll = 0.0;
                            } else if let Some((_, s)) = self.editing_args.as_mut() {
                                s.push_str(&text);
                            } else if let Some((_, s)) = self.editing_note.as_mut() {
                                s.push_str(&text);
                            }
                        }
                        return;
                    }
                    if pressed && !event.repeat && app_chord && code == KeyCode::KeyK {
                        self.palette_open = !self.palette_open;
                        self.palette_query.clear();
                        self.palette_sel = 0;
                        self.palette_scroll = 0.0;
                        return;
                    }
                    // Cmd/Ctrl+T opens a new workspace tab.
                    if pressed && !event.repeat && app_chord && code == KeyCode::KeyT {
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
                    if pressed && !event.repeat && app_chord && code == KeyCode::KeyW {
                        self.close_workspace(self.active_ws);
                        return;
                    }
                    // Cmd/Ctrl+D duplicates the focused / hovered node.
                    if pressed && !event.repeat && app_chord && code == KeyCode::KeyD {
                        self.duplicate_focused();
                        return;
                    }
                    // Cmd/Ctrl+Z undoes the last mutation.
                    if pressed && app_chord && code == KeyCode::KeyZ {
                        self.conn.send(Command::Undo);
                        return;
                    }
                    // The filesystem inspector is modal: Escape backs out of a
                    // file preview, then closes the inspector.
                    if self.inspect.is_some() {
                        if pressed && code == KeyCode::Escape {
                            match self.inspect.as_mut().and_then(|i| i.file.take()) {
                                Some(_) => {} // was previewing → back to listing
                                None => self.inspect = None,
                            }
                        }
                        return;
                    }
                    // The log panel is modal: Escape closes it.
                    if self.logs.is_some() {
                        if pressed && code == KeyCode::Escape {
                            self.logs = None;
                        }
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
                    // While editing a note, keystrokes edit its text.
                    if self.editing_note.is_some() {
                        if pressed {
                            self.editing_note_key(code, event.text.as_deref());
                        }
                        return;
                    }
                    // While editing a bind wire's mount path, keystrokes edit
                    // that text (so Backspace edits rather than deleting the
                    // selected wire).
                    if self.editing_mount.is_some() {
                        if pressed {
                            self.editing_mount_key(code, event.text.as_deref());
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
                    self.key_events
                        .push((key_event(code, text, self.mods, event.repeat), pressed));
                }
            }
            _ => {}
        }
    }
}

/// The single-player front-end: a wgpu window driven by winit. It owns all the
/// view/input state ([`App`]) and forwards mutations to the server as
/// [`Command`]s over its [`ServerHandle`]. See [`wk_protocol::Client`].
pub struct WindowClient {
    /// Set (by the host's Ctrl-C handler) to interrupt the loop gracefully —
    /// the only way out of a headless (windowless) session.
    pub interrupt: Arc<std::sync::atomic::AtomicBool>,
}

impl wk_protocol::Client<ServerHandle> for WindowClient {
    fn run(self: Box<Self>, conn: ServerHandle) -> Result<(), String> {
        let mut event_loop = EventLoop::builder().build().map_err(|e| e.to_string())?;
        let mut app = App::new(conn, self.interrupt)?;
        loop {
            // Pump (and render, via `about_to_wait`) with the handler set the
            // whole time, blocking up to a frame for events — this paces ~60fps
            // when idle and leaves no window where a macOS event has no handler
            // to run. A quit calls `ActiveEventLoop::exit()`, so the next pump
            // returns Exit.
            // Headless needs no frame pacing — just stay responsive to the
            // OS and to Ctrl-C.
            let wait = if app.headless {
                Duration::from_millis(250)
            } else {
                FRAME
            };
            if let PumpStatus::Exit(_) = event_loop.pump_app_events(Some(wait), &mut app) {
                break;
            }
        }
        // The server owns persistence; the window closing (or the headless
        // loop interrupting) just detaches this client.
        Ok(())
    }
}

/// The boundary wire a drag from `src`'s port to `dst`'s port would author, if
/// either end is an instance.
///
/// An instance has no wireable node of its own: its dots are the *definition's*
/// boundary ports, and what joins one to a neighbour is a line in the group's
/// block naming that port. Dropping on an instance's input dot writes
/// `in "<port>" "<the other node>"`; dragging out of one of its output dots
/// writes `out`. Which port is decided by the dot's slot, not its kind — a
/// definition may declare two `midi` in-ports, and they are different edges.
///
/// Two instances give `None`: a boundary wire names one port and one node, so
/// it has no way to say which port of the far group it meant.
fn boundary_wire_for(v: &View, src: (NodeId, Port), dst: (NodeId, Port)) -> Option<BoundaryWire> {
    let name = |(id, p): (NodeId, Port)| -> Option<String> {
        Some(v.groups.get(&id)?.ports.get(p.slot)?.name.clone())
    };
    match (v.groups.contains_key(&src.0), v.groups.contains_key(&dst.0)) {
        (false, true) => Some(BoundaryWire {
            group: dst.0,
            dir: PortDir::In,
            port: name(dst)?,
            node: src.0,
        }),
        (true, false) => Some(BoundaryWire {
            group: src.0,
            dir: PortDir::Out,
            port: name(src)?,
            node: dst.0,
        }),
        _ => None,
    }
}

/// Whether a group's block already holds this exact line — a second drag over
/// the same pair takes it away again, as it does for an ordinary wire.
fn boundary_authored(v: &View, bw: &BoundaryWire) -> bool {
    v.groups.get(&bw.group).is_some_and(|g| {
        let wires = match bw.dir {
            PortDir::In => &g.in_wires,
            PortDir::Out => &g.out_wires,
        };
        wires.iter().any(|(p, n)| *p == bw.port && *n == bw.node)
    })
}

/// The in-app path a bind wire mounts at: the per-connection override if set,
/// else the source's own name — a file node's name, or an fs-provider app's
/// (mirrors the server's `mount_path_for`).
fn mount_path(v: &View, src: NodeId, app: NodeId) -> String {
    v.mount_paths.get(&(src, app)).cloned().unwrap_or_else(|| {
        v.file_nodes
            .get(&src)
            .map(|f| f.name.clone())
            .or_else(|| v.app_node(src).map(|n| n.name.clone()))
            .unwrap_or_default()
    })
}

/// The name of the fs-provider app whose served tree is mounted at `path`
/// inside `node`, if any — derived from the graph alone (the provider bind
/// wires into `node` and their mount paths), never by asking the guest.
fn provider_serving(v: &View, node: NodeId, path: &str) -> Option<String> {
    let want = path.trim_start_matches('/');
    v.connections.iter().find_map(|&(src, dst)| {
        (dst == node
            && v.fs_providers.contains(&src)
            && mount_path(v, src, dst).trim_start_matches('/') == want)
            .then(|| v.app_node(src).map(|n| n.name.clone()))
            .flatten()
    })
}

#[cfg(test)]
mod inspect_tests {
    use super::*;

    /// A new tab is switched to even though the server's view hasn't caught
    /// up yet — the Cmd+T race.
    #[test]
    fn a_freshly_created_workspace_keeps_the_switch_until_it_lands() {
        let old = NodeId::from_u128(1);
        let new = NodeId::from_u128(2);
        let tabs = vec![old];

        // Frame 1: created and switched to; the server hasn't published it.
        let (active, pending) = reconcile_active_ws(&tabs, new, Some((new, PENDING_WS_FRAMES)));
        assert_eq!(active, new, "the switch holds");
        assert_eq!(pending, Some((new, PENDING_WS_FRAMES - 1)), "still waiting");

        // Frame 2: it lands. The switch stands on its own now.
        let (active, pending) = reconcile_active_ws(&[old, new], active, pending);
        assert_eq!((active, pending), (new, None));
    }

    /// A create the server never applies must not strand the client on a tab
    /// that doesn't exist.
    #[test]
    fn a_create_that_never_lands_falls_back() {
        let old = NodeId::from_u128(1);
        let ghost = NodeId::from_u128(2);
        let (mut active, mut pending) = (ghost, Some((ghost, 2)));
        for _ in 0..3 {
            (active, pending) = reconcile_active_ws(&[old], active, pending);
        }
        assert_eq!((active, pending), (old, None), "back to a real tab");
    }

    /// The pre-existing behaviour, unchanged: an active tab that goes away
    /// (closed, or deleted by another client) falls back to the first.
    #[test]
    fn a_vanished_tab_still_falls_back_at_once() {
        let a = NodeId::from_u128(1);
        let gone = NodeId::from_u128(9);
        assert_eq!(reconcile_active_ws(&[a], gone, None), (a, None));
        // ...and switching away from a pending tab drops the wait with it.
        let other = NodeId::from_u128(3);
        assert_eq!(
            reconcile_active_ws(&[a, other], other, Some((gone, 5))),
            (other, None)
        );
    }

    /// Port slots are evenly spaced with margins: one sits at the middle, and
    /// N stay strictly inside the node's vertical span in order.
    #[test]
    fn port_slots_are_spaced_and_inside_the_node() {
        let r = [10.0, 100.0, 60.0, 200.0]; // height 100, spans y ∈ [100, 200]
        assert_eq!(port_slots_y(r, 1), vec![150.0], "one port centres");
        let two = port_slots_y(r, 2);
        assert!((two[0] - 133.333).abs() < 0.01 && (two[1] - 166.666).abs() < 0.01);
        // Always inside the node and monotonically increasing.
        let five = port_slots_y(r, 5);
        assert!(five.windows(2).all(|w| w[0] < w[1]));
        assert!(five.iter().all(|&y| y > r[1] && y < r[3]));
    }

    /// Typed ports anchor on the correct edge: inputs on the left (x = r[0]),
    /// outputs on the right (x = r[2]), each in `ports` order.
    #[test]
    fn port_anchors_split_left_and_right_by_direction() {
        let r = [10.0, 100.0, 60.0, 200.0];
        let ports = vec![
            port(PortKind::Bind, PortDir::In),
            port(PortKind::Midi, PortDir::Out),
            port(PortKind::Midi, PortDir::In),
        ];
        let a = port_anchors(r, &ports);
        assert_eq!(a[0][0], r[0], "bind-in on the left edge");
        assert_eq!(a[2][0], r[0], "midi-in on the left edge");
        assert_eq!(a[1][0], r[2], "midi-out on the right edge");
        // The two left-edge inputs share the left edge but sit at distinct heights.
        assert_ne!(a[0][1], a[2][1]);
        // The single output centres vertically.
        assert!((a[1][1] - 150.0).abs() < 0.01);
    }

    /// Hiding a node's 3D panel takes effect only for a node that has a
    /// `wk:scene` body to stand in its place.
    #[test]
    fn a_panel_hides_only_when_something_else_can_be_seen() {
        let (totem, plain) = (NodeId::new(), NodeId::new());
        let hidden: HashSet<NodeId> = [totem, plain].into_iter().collect();
        let bodied: HashSet<NodeId> = [totem].into_iter().collect();
        assert!(
            !shows_panel3d(&hidden, &bodied, totem),
            "asked to hide, and has an object to show instead"
        );
        assert!(
            shows_panel3d(&hidden, &bodied, plain),
            "asked to hide, but hiding would leave nothing at all"
        );
        // A node that never asked keeps its panel either way.
        let none = HashSet::new();
        assert!(shows_panel3d(&none, &bodied, totem));
        assert!(shows_panel3d(&none, &none, plain));
    }

    /// A registry stub for an app node (no wasm), enough for name lookups.
    fn stub_node(id: NodeId, name: &str) -> SharedNode {
        use std::sync::atomic::AtomicBool;
        Arc::new(wk_server::plugin::Node {
            id,
            name: name.to_string(),
            term_io: wk_server::terminal::TermIo::new(),
            fs: wk_server::vfs::new_fs(),
            midi_in: wk_server::midi::new_inbox(),
            options: wk_server::options::new_options(Vec::new()),
            finished: Arc::new(AtomicBool::new(false)),
            running: Arc::new(AtomicBool::new(false)),
            kill: Arc::new(AtomicBool::new(false)),
            setup: std::sync::OnceLock::new(),
            env: Vec::new(),
            layers: Vec::new(),
            capture_src: wk_server::capture::new_src(),
            clip_src: wk_server::clipboard::new_src(),
            clip_read: wk_server::clipboard::new_permit(),
            clip_write: wk_server::clipboard::new_permit(),
            exec_permit: wk_server::exec::new_permit(true),
            fs_serve: wk_server::vfs::ProviderConn::new(),
        })
    }

    /// A bind wire's mount path defaults to its source's name — a file node's
    /// or an fs-provider app's — with the per-connection override winning; the
    /// provider serving a mount point resolves through those same paths.
    #[test]
    fn mount_paths_and_provider_badges_resolve_from_the_view() {
        let (vol, srv, app) = (NodeId::new(), NodeId::new(), NodeId::new());
        let mut v = View::default();
        v.file_nodes.insert(
            vol,
            wk_server::server::FileMeta {
                name: "data".into(),
                size: 0,
                host_mapped: false,
                is_dir: false,
                persist: false,
            },
        );
        v.nodes.push(stub_node(srv, "srv"));
        v.fs_providers.insert(srv);
        v.connections.push((vol, app));
        v.connections.push((srv, app));

        // Defaults: the source's own name.
        assert_eq!(mount_path(&v, vol, app), "data");
        assert_eq!(mount_path(&v, srv, app), "srv");
        // A provider mount at its default path names the serving node…
        assert_eq!(provider_serving(&v, app, "/srv").as_deref(), Some("srv"));
        // …a plain volume mount doesn't (no provider serves it).
        assert_eq!(provider_serving(&v, app, "/data"), None);

        // An override moves the mount; lookups follow it (slash-agnostic).
        v.mount_paths.insert((srv, app), "/mnt/shared".into());
        assert_eq!(mount_path(&v, srv, app), "/mnt/shared");
        assert_eq!(
            provider_serving(&v, app, "/mnt/shared").as_deref(),
            Some("srv")
        );
        assert_eq!(provider_serving(&v, app, "/srv"), None);
    }

    /// Dragging onto (or out of) an instance's port disc authors a boundary
    /// wire naming *that* port — which is why a port's slot is its identity: a
    /// definition may declare two ports of one kind, and a hit-test that knew
    /// only "a midi input" could not say which of them the drag landed on.
    #[test]
    fn a_drag_touching_an_instance_names_the_port_it_landed_on() {
        use wk_server::server::{BoundaryPort, GroupInfo};
        let (inst, other, piano) = (NodeId::new(), NodeId::new(), NodeId::new());
        let midi_in = |name: &str| BoundaryPort {
            name: name.to_string(),
            dir: PortDir::In,
            kind: PortKind::Midi,
        };
        let mut v = View::default();
        v.groups.insert(
            inst,
            GroupInfo {
                definition: "voice".into(),
                // Two in-ports of one kind: distinguishable only by slot.
                ports: vec![
                    midi_in("notes"),
                    midi_in("clock"),
                    BoundaryPort {
                        name: "audio".into(),
                        dir: PortDir::Out,
                        kind: PortKind::Midi,
                    },
                ],
                in_wires: vec![("clock".to_string(), piano)],
                out_wires: Vec::new(),
                nodes: 2,
            },
        );
        let p = |slot| Port {
            kind: PortKind::Midi,
            dir: PortDir::In,
            slot,
        };
        // Dropping on the second dot wires the second port, not the first.
        let bw = boundary_wire_for(&v, (piano, p(0)), (inst, p(1))).expect("a boundary wire");
        assert_eq!(
            (bw.group, bw.dir, &*bw.port, bw.node),
            (inst, PortDir::In, "clock", piano)
        );
        // ...and that line is already written, so the drag is a disconnect.
        assert!(boundary_authored(&v, &bw));
        let fresh = boundary_wire_for(&v, (piano, p(0)), (inst, p(0))).expect("a boundary wire");
        assert_eq!(&*fresh.port, "notes");
        assert!(!boundary_authored(&v, &fresh));

        // Dragging *out* of an instance is its out-port reaching a neighbour.
        let out = Port {
            kind: PortKind::Midi,
            dir: PortDir::Out,
            slot: 2,
        };
        let bw = boundary_wire_for(&v, (inst, out), (other, p(0))).expect("a boundary wire");
        assert_eq!(
            (bw.group, bw.dir, &*bw.port, bw.node),
            (inst, PortDir::Out, "audio", other)
        );

        // Two ordinary nodes are an ordinary wire, and two instances are
        // nothing: the line could not say which port of the far group it meant.
        assert!(boundary_wire_for(&v, (piano, p(0)), (other, p(0))).is_none());
        v.groups.insert(other, v.groups[&inst].clone());
        assert!(boundary_wire_for(&v, (inst, out), (other, p(0))).is_none());
    }

    /// The log panel strips ANSI escapes, normalises control bytes, and hard-
    /// wraps long lines to the column width.
    #[test]
    fn log_lines_strips_ansi_normalises_and_wraps() {
        // SGR colour codes around text are removed; \r\n collapses to one break.
        let raw = b"\x1b[32mhello\x1b[0m world\r\nsecond\tline";
        assert_eq!(
            log_lines(raw, 80),
            vec!["hello world".to_string(), "second line".to_string()]
        );
        // Hard wrap at the column width.
        assert_eq!(
            log_lines(b"abcdef", 3),
            vec!["abc".to_string(), "def".to_string()]
        );
        // A trailing newline doesn't add a spurious blank line; a blank line in
        // the middle is preserved.
        assert_eq!(
            log_lines(b"a\n\nb\n", 80),
            vec!["a".to_string(), String::new(), "b".to_string()]
        );
        // Empty input yields no lines (the panel shows its own placeholder).
        assert!(log_lines(b"", 80).is_empty());
    }

    /// Scrolling offsets which entries are visible, is clamped so the last
    /// entry lands in the last row at max scroll, and the ".." slot costs one
    /// row of capacity.
    #[test]
    fn inspect_scrolling_clamps_and_offsets_rows() {
        let fb = [1440.0, 900.0];
        let (_, _, _, rows7, _) = inspect_geom(fb, 100, false, 7);
        assert_eq!(rows7[0].1, 7, "first visible entry follows the scroll");
        let (_, _, _, rows0, _) = inspect_geom(fb, 100, false, 0);
        assert_eq!(rows7.len(), rows0.len(), "capacity independent of scroll");

        let max = inspect_max_scroll(fb, 100, false);
        assert!(max > 0, "100 entries overflow the panel");
        let (_, _, _, rows_max, _) = inspect_geom(fb, 100, false, max);
        assert_eq!(rows_max.last().unwrap().1, 99, "last entry visible at max");
        assert_eq!(rows_max[0].1, max, "no overscroll past the tail");

        assert_eq!(
            inspect_max_scroll(fb, 3, false),
            0,
            "few entries: no scroll"
        );
        assert_eq!(
            inspect_max_scroll(fb, 100, true),
            max + 1,
            "the .. row costs one slot of capacity"
        );
    }

    /// The inspector's row geometry is well-formed: every region sits inside the
    /// panel, entry rows are stacked in order below the title (and below the
    /// ".." row when present), don't overlap the preview strip, and `scroll`
    /// offsets which entry the first visible row maps to.
    #[test]
    fn inspect_geom_is_well_formed() {
        let fb = [1440.0, 900.0];
        let within = |r: [f32; 4], outer: [f32; 4]| {
            r[0] >= outer[0] - 0.5
                && r[1] >= outer[1] - 0.5
                && r[2] <= outer[2] + 0.5
                && r[3] <= outer[3] + 0.5
        };

        // Root directory (no ".."), plenty of entries.
        let (panel, close, up, rows, preview) = inspect_geom(fb, 100, false, 0);
        assert!(up.is_none(), "root has no parent row");
        assert!(within(close, panel));
        assert!(within(preview, panel));
        assert!(!rows.is_empty());
        // Rows are inside the panel, above the preview, stacked, and map 1:1 to
        // entry indices from 0.
        let mut prev_bottom = f32::MIN;
        for (i, &(r, idx)) in rows.iter().enumerate() {
            assert_eq!(idx, i, "row {i} maps to entry {i}");
            assert!(within(r, panel));
            assert!(r[3] <= preview[1] + 0.5, "row sits above the preview strip");
            assert!(r[1] >= prev_bottom - 0.5, "rows don't overlap");
            prev_bottom = r[3];
        }

        // A subdirectory: the ".." row appears and takes the first slot, so one
        // fewer entry row fits.
        let (_, _, up2, rows2, _) = inspect_geom(fb, 100, true, 0);
        let up2 = up2.expect("subdirectory has a parent row");
        assert!(up2[1] < rows2[0].0[1], "\"..\" is above the first entry");
        assert_eq!(rows.len(), rows2.len() + 1);

        // Scrolling offsets the first visible entry index.
        let (_, _, _, rows3, _) = inspect_geom(fb, 100, false, 5);
        assert_eq!(rows3[0].1, 5);

        // A short listing shows only as many rows as there are entries.
        let (_, _, _, rows4, _) = inspect_geom(fb, 2, false, 0);
        assert_eq!(rows4.len(), 2);
    }
}
