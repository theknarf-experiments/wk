//! Host side of the wk plugin system: wk implements the standard wasi-gfx
//! interfaces (`wasi:surface`, `wasi:graphics-context`, `wasi:frame-buffer`)
//! over a *virtual surface* and drives a guest's `run` loop. Each guest runs on
//! its own thread with its own wasmtime `Store`; the host signals one frame at a
//! time and reads back the pixels the guest paints.

use std::collections::VecDeque;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context as TaskContext, Poll, Waker};

use wasmtime::component::{Component, HasSelf, Linker, Resource, ResourceTable};
use wasmtime::{Config, Engine, Result, Store, UpdateDeadline};
use wasmtime_wasi::p2::{subscribe, DynPollable, Pollable};
use wasmtime_wasi::{async_trait, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

wasmtime::component::bindgen!({
    path: "wit",
    world: "compositor",
    imports: { default: trappable },
    exports: { default: async },
    with: {
        "wasi:io/poll.pollable": wasmtime_wasi::p2::DynPollable,
        "wasi:surface/surface.surface": SurfaceState,
        "wasi:graphics-context/graphics-context.context": ContextState,
        "wasi:graphics-context/graphics-context.abstract-buffer": AbstractBufferState,
        "wasi:frame-buffer/frame-buffer.device": DeviceState,
        "wasi:frame-buffer/frame-buffer.buffer": BufferState,
    },
});

use wasi::surface::surface::{CreateDesc, FrameEvent};
pub use wasi::surface::surface::{
    Key, KeyEvent, PointerButton, PointerEvent, ResizeEvent, ScrollEvent,
};
use wk_protocol::NodeId;

pub struct VirtualSurface {
    pub id: u64,
    pub node_id: NodeId,
    pub width: u32,
    pub height: u32,
    /// Latest painted RGBA8 pixels (`width * height * 4`).
    pub pixels: Vec<u8>,
    /// Set by the server once per frame; consumed by the frame pollable.
    pub frame_ready: bool,
    /// Set by the server to close this instance: the guest traps on its next
    /// `get_frame` and its thread exits.
    pub closed: bool,
    pub resize: Option<ResizeEvent>,
    pub pointer_move: VecDeque<PointerEvent>,
    pub pointer_down: VecDeque<PointerEvent>,
    pub pointer_up: VecDeque<PointerEvent>,
    pub pointer_scroll: VecDeque<ScrollEvent>,
    /// Set once the guest first subscribes to scroll events. The compositor
    /// reads it to decide wheel routing: scroll goes to a surface that asked
    /// for it, and keeps panning the canvas over one that never did.
    pub wants_scroll: bool,
    pub key_down: VecDeque<KeyEvent>,
    pub key_up: VecDeque<KeyEvent>,
    /// Wakers parked on this surface's pollables; woken when state changes.
    wakers: Vec<Waker>,
}

static NEXT_SURFACE_ID: AtomicU64 = AtomicU64::new(0);

/// Largest surface edge a guest may request. Caps the RGBA8 backing buffer at
/// `MAX_SURFACE_EDGE² * 4` (~256 MB at 8192) and, crucially, keeps
/// `width * height * 4` from overflowing when computed — a guest asking for
/// 65536×65536 would otherwise wrap to a too-small buffer.
const MAX_SURFACE_EDGE: u32 = 8192;

/// Clamp a requested surface size and return `(width, height, byte_len)` for its
/// RGBA8 buffer, computed without overflow.
fn surface_dims(width: u32, height: u32) -> (u32, u32, usize) {
    let w = width.clamp(1, MAX_SURFACE_EDGE);
    let h = height.clamp(1, MAX_SURFACE_EDGE);
    (w, h, w as usize * h as usize * 4)
}

/// Error a closed surface returns to unwind and end its guest cleanly. The
/// driver recognises it and exits the guest thread without logging an error.
#[derive(Debug)]
struct SurfaceClosed;

impl std::fmt::Display for SurfaceClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "surface closed")
    }
}

impl std::error::Error for SurfaceClosed {}

impl VirtualSurface {
    fn new(node_id: NodeId, width: u32, height: u32) -> Self {
        let (width, height, bytes) = surface_dims(width, height);
        Self {
            id: NEXT_SURFACE_ID.fetch_add(1, Ordering::Relaxed),
            node_id,
            width,
            height,
            pixels: vec![0; bytes],
            frame_ready: false,
            closed: false,
            resize: None,
            pointer_move: VecDeque::new(),
            pointer_down: VecDeque::new(),
            pointer_up: VecDeque::new(),
            pointer_scroll: VecDeque::new(),
            wants_scroll: false,
            key_down: VecDeque::new(),
            key_up: VecDeque::new(),
            wakers: Vec::new(),
        }
    }

    pub fn wake(&mut self) {
        for w in self.wakers.drain(..) {
            w.wake();
        }
    }
}

pub type SharedSurface = Arc<Mutex<VirtualSurface>>;
pub type SurfaceRegistry = Arc<Mutex<Vec<SharedSurface>>>;

/// A launched plugin instance.
pub struct Node {
    /// Stable id, persisted in the workspace so connections can refer to this
    /// node across restarts.
    pub id: NodeId,
    pub name: String,
    pub term_io: crate::terminal::SharedTermIo,
    /// This node's in-memory filesystem, so the server can mount connected file
    /// nodes into it.
    pub fs: crate::vfs::SharedFs,
    /// This node's MIDI input queue, so the server can wire a MIDI source's
    /// output to it.
    pub midi_in: crate::midi::SharedInbox,
    /// This node's option values (e.g. knob settings) reported by the guest, so
    /// the server can persist them to the workspace and seed them on restore.
    pub options: crate::options::SharedOptions,
    /// Set by the guest thread when its `run` returns (it exited on its own).
    pub finished: Arc<AtomicBool>,
    /// True while a guest thread is live. A networked node is created idle
    /// (`false`) and run on demand; it flips back to `false` when the guest
    /// exits.
    pub running: Arc<AtomicBool>,
    /// Kill switch: set by the server to stop a still-running node.
    pub kill: Arc<AtomicBool>,
    /// The compiled component and its wiring, filled in by the background compile
    /// thread. `None` while the node is still compiling.
    pub setup: OnceLock<NodeSetup>,
    /// Environment for the guest (a container image's ENV), applied on run.
    pub env: Vec<(String, String)>,
    /// The container image's layer digests mounted into `fs` (empty for a
    /// plain wasm node) — the file inspector shows the count and badges
    /// layer-backed entries.
    pub layers: Vec<String>,
    /// The Screen Capture frame slot granted to this node by a capture wire
    /// (`None` while unwired). Set by the server's capture reconciler.
    pub capture_src: crate::capture::SharedCaptureSrc,
    /// The host clipboard board granted to this node by a clipboard wire
    /// (`None` while unwired), and whether its capability token currently
    /// allows reading / writing it. All three are refreshed by the server's
    /// `sync_clipboard` every tick, so attenuating a token revokes live.
    pub clip_src: crate::clipboard::SharedClipSrc,
    pub clip_read: crate::clipboard::ClipPermit,
    pub clip_write: crate::clipboard::ClipPermit,
    /// Whether this node may run programs via `wk:exec`, refreshed from its
    /// capability token each tick so attenuation revokes it live.
    pub exec_permit: crate::exec::ExecPermit,
    /// This node's `wk:fs` conduit: the queue its serve loop (if it imports
    /// `wk:fs/provider`) pulls from, and consumers with this node mounted push
    /// into. Exists on every node so a consumer can be wired before the
    /// provider runs — calls fail fast (EIO) until a serve loop attaches.
    pub fs_serve: Arc<wk_vfs::ProviderConn>,
}

/// A node's compiled component plus how to run and wire it — published once the
/// background compile finishes.
pub struct NodeSetup {
    /// This node's network stack on the fabric (`Some` if it imports
    /// wasi:sockets), so the server can move it between virtual networks.
    pub net_stack: Option<wk_fabric::netstack::SharedStack>,
    /// Set if this is a `wasi:http` server (exports `incoming-handler`): the
    /// component path to serve when wired to a HostPort. Such nodes aren't run.
    pub http_path: Option<std::path::PathBuf>,
    /// Present for a runnable node (not an http server): the compiled component
    /// and how to instantiate it, reused across runs.
    pub run: Option<RunInfo>,
    /// Whether the component imports `wk:midi` — the UI only draws MIDI ports on
    /// nodes that actually transport MIDI.
    pub midi: bool,
    /// Whether the component imports `wasi:sockets` — the UI only draws a Network
    /// port on nodes that actually do networking.
    pub net: bool,
    /// Whether the component imports `wk:capture` — the UI only draws a Capture
    /// port on nodes that actually consume captured frames.
    pub capture: bool,
    /// Whether the component imports `wk:clipboard` — the UI only draws a
    /// Clipboard port on nodes that actually copy and paste.
    pub clipboard: bool,
    /// Whether the component imports `wk:fs/provider` — it serves a filesystem,
    /// so other nodes may mount it (the UI offers it as a mount source).
    pub fs_provider: bool,
}

/// What [`PluginHost::run_node`] needs to (re)start a node's guest, reused across
/// runs so re-running never recompiles.
pub struct RunInfo {
    component: Component,
    is_command: bool,
    surfaces: SurfaceRegistry,
}

impl Node {
    pub fn is_loading(&self) -> bool {
        self.setup.get().is_none()
    }
    pub fn net_stack(&self) -> Option<wk_fabric::netstack::SharedStack> {
        self.setup.get().and_then(|s| s.net_stack.clone())
    }
    pub fn http_path(&self) -> Option<std::path::PathBuf> {
        self.setup.get().and_then(|s| s.http_path.clone())
    }
    /// Whether this node transports MIDI (imports `wk:midi`). `false` until the
    /// component has finished compiling.
    pub fn imports_midi(&self) -> bool {
        self.setup.get().is_some_and(|s| s.midi)
    }
    /// Whether this node does networking (imports `wasi:sockets`). `false` until
    /// the component has finished compiling.
    pub fn imports_net(&self) -> bool {
        self.setup.get().is_some_and(|s| s.net)
    }
    /// Whether this node consumes captured canvas frames (imports `wk:capture`).
    /// `false` until the component has finished compiling.
    pub fn imports_capture(&self) -> bool {
        self.setup.get().is_some_and(|s| s.capture)
    }
    /// Whether this node copies and pastes (imports `wk:clipboard`). `false`
    /// until the component has finished compiling.
    pub fn imports_clipboard(&self) -> bool {
        self.setup.get().is_some_and(|s| s.clipboard)
    }
    /// Whether this node serves a filesystem (imports `wk:fs/provider`), so
    /// other nodes may mount it. `false` until the component has compiled.
    pub fn serves_fs(&self) -> bool {
        self.setup.get().is_some_and(|s| s.fs_provider)
    }
    pub fn is_runnable(&self) -> bool {
        self.setup.get().is_some_and(|s| s.run.is_some())
    }
    /// A `wasi:cli/command` guest (a terminal app), as opposed to a graphical or
    /// http node — the nodes a CLI client can `attach` to.
    pub fn is_command(&self) -> bool {
        self.setup
            .get()
            .and_then(|s| s.run.as_ref())
            .is_some_and(|r| r.is_command)
    }
}

pub type SharedNode = Arc<Node>;
pub type NodeRegistry = Arc<Mutex<Vec<SharedNode>>>;

pub struct SurfaceState {
    shared: SharedSurface,
}
pub struct ContextState {
    connected: Option<SharedSurface>,
}
pub struct AbstractBufferState {
    shared: SharedSurface,
}
pub struct DeviceState {
    connected: Option<SharedSurface>,
}
pub struct BufferState {
    shared: SharedSurface,
}

#[derive(Clone, Copy)]
enum PollKind {
    Frame,
    Resize,
    PointerMove,
    PointerDown,
    PointerUp,
    PointerScroll,
    KeyDown,
    KeyUp,
}

struct SurfacePollable {
    shared: SharedSurface,
    kind: PollKind,
}

#[async_trait]
impl Pollable for SurfacePollable {
    async fn ready(&mut self) {
        WaitCondition {
            shared: self.shared.clone(),
            kind: self.kind,
        }
        .await
    }
}

/// Future that resolves when its surface condition holds, parking a waker
/// otherwise. The `Frame` condition is one-shot: it consumes `frame_ready`.
struct WaitCondition {
    shared: SharedSurface,
    kind: PollKind,
}

impl Future for WaitCondition {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<()> {
        let mut s = self.shared.lock().unwrap();
        let ready = match self.kind {
            // A closed surface wakes the frame poll so the guest proceeds to
            // `get_frame`, which then traps and ends the guest thread.
            PollKind::Frame => s.frame_ready || s.closed,
            PollKind::Resize => s.resize.is_some(),
            PollKind::PointerMove => !s.pointer_move.is_empty(),
            PollKind::PointerDown => !s.pointer_down.is_empty(),
            PollKind::PointerUp => !s.pointer_up.is_empty(),
            PollKind::PointerScroll => !s.pointer_scroll.is_empty(),
            PollKind::KeyDown => !s.key_down.is_empty(),
            PollKind::KeyUp => !s.key_up.is_empty(),
        };
        if ready {
            if let PollKind::Frame = self.kind {
                s.frame_ready = false;
            }
            Poll::Ready(())
        } else {
            s.wakers.push(cx.waker().clone());
            Poll::Pending
        }
    }
}

pub struct HostState {
    ctx: WasiCtx,
    table: ResourceTable,
    registry: SurfaceRegistry,
    /// The instance this store belongs to; tags the surfaces it creates and the
    /// MIDI it sends.
    pub(crate) node_id: NodeId,
    pub(crate) fs: crate::vfs::SharedFs,
    /// This node's terminal stdio; backs `wk:tty/control` so the guest's
    /// `termios` shim can set the line-discipline mode the client reads.
    pub(crate) term_io: crate::terminal::SharedTermIo,
    /// The capture frame slot granted by a capture wire (shared with the
    /// node; `None` while unwired) + the last frame sequence this store saw.
    pub(crate) capture_src: crate::capture::SharedCaptureSrc,
    pub(crate) capture_seq: u64,
    /// The host clipboard board granted by a clipboard wire (shared with the
    /// node; `None` while unwired), plus the two permits its capability token
    /// grants. Read and write are separate: a token may allow copying OUT of
    /// a node without letting it see what the user copied anywhere else.
    pub(crate) clip_src: crate::clipboard::SharedClipSrc,
    pub(crate) clip_read: crate::clipboard::ClipPermit,
    pub(crate) clip_write: crate::clipboard::ClipPermit,
    /// Whether this store has already logged a denied clipboard read. The
    /// guest is told nothing, but the first refusal per node belongs in the
    /// host log or "my app cannot paste" has no diagnosis anywhere.
    pub(crate) clip_denied_logged: bool,
    /// What this store needs to serve `wk:exec` (run another program from the
    /// node's filesystem). `None` for contexts that may not exec at all —
    /// build steps and children, which keeps `RUN` hermetic.
    pub(crate) exec: Option<crate::exec::ExecCtx>,
    pub(crate) midi_in: crate::midi::SharedInbox,
    pub(crate) midi_router: crate::midi::Router,
    /// Where this node's wk:scene entities register (shared, host-global).
    pub(crate) scene_reg: crate::scene::SceneRegistry,
    pub(crate) options: crate::options::SharedOptions,
    /// This node's network context (smoltcp stack on the fabric) — `Some` only
    /// for nodes that import wasi:sockets. Backs wk's own wasi:sockets impl.
    pub(crate) net: Option<crate::sockets::NetCtx>,
    /// What this store needs to serve `wk:fs` (be a filesystem other nodes
    /// mount). `None` for contexts that don't serve — exec children, build
    /// steps, per-request http stores.
    pub(crate) fs_serve: Option<crate::fsprov::FsServeCtx>,
    /// This store's RNG, backing the standard `wasi:random` interface (needed by
    /// e.g. a guest's `HashMap`).
    random_ctx: wasmtime_wasi::random::WasiRandomCtx,
    /// This store's `wasi:http` context (outbound requests, and serving when a
    /// node exports `wasi:http/incoming-handler`).
    http_ctx: wasmtime_wasi_http::WasiHttpCtx,
    /// Gates outbound `wasi:http` behind the node's host access (see
    /// [`GatedHttpHooks`]).
    http_hooks: GatedHttpHooks,
    gpu: Arc<wgpu_core::global::Global>,
}

impl wasmtime_wasi_http::p2::WasiHttpView for HostState {
    fn http(&mut self) -> wasmtime_wasi_http::p2::WasiHttpCtxView<'_> {
        wasmtime_wasi_http::p2::WasiHttpCtxView {
            ctx: &mut self.http_ctx,
            table: &mut self.table,
            hooks: &mut self.http_hooks,
        }
    }
}

impl wasmtime_wasi_http::p3::WasiHttpView for HostState {
    fn http(&mut self) -> wasmtime_wasi_http::p3::WasiHttpCtxView<'_> {
        wasmtime_wasi_http::p3::WasiHttpCtxView {
            ctx: &mut self.http_ctx,
            table: &mut self.table,
            hooks: &mut self.http_hooks,
        }
    }
}

/// Gates a store's **outbound** `wasi:http` requests behind the same host-access
/// check as raw sockets ([`crate::sockets`]): a guest reaches the real host
/// network only when its node is wired to a Gateway (which sets `host_access`
/// on the node's fabric stack). A node with no fabric stack — a pure-http node,
/// or a per-request serve store — is denied. Without this, `wasi:http/
/// outgoing-handler` dialed straight over the host OS, a hole around the whole
/// fabric+Gateway sandbox that let an "isolated" node reach arbitrary hosts.
struct GatedHttpHooks {
    /// The node's fabric stack, if it has one; `host_access` is read live so
    /// wiring/unwiring a Gateway takes effect between requests.
    stack: Option<wk_fabric::netstack::SharedStack>,
}

impl GatedHttpHooks {
    fn host_allowed(&self) -> bool {
        self.stack
            .as_ref()
            .is_some_and(|s| s.lock().unwrap().host_access)
    }
}

impl wasmtime_wasi_http::p2::WasiHttpHooks for GatedHttpHooks {
    fn send_request(
        &mut self,
        request: hyper::Request<wasmtime_wasi_http::p2::body::HyperOutgoingBody>,
        config: wasmtime_wasi_http::p2::types::OutgoingRequestConfig,
    ) -> wasmtime_wasi_http::p2::HttpResult<wasmtime_wasi_http::p2::types::HostFutureIncomingResponse>
    {
        use wasmtime_wasi_http::p2::bindings::http::types::ErrorCode;
        if !self.host_allowed() {
            return Err(ErrorCode::HttpRequestDenied.into());
        }
        Ok(wasmtime_wasi_http::p2::default_send_request(
            request, config,
        ))
    }
}

impl wasmtime_wasi_http::p3::WasiHttpHooks for GatedHttpHooks {
    fn send_request(
        &mut self,
        request: hyper::Request<
            http_body_util::combinators::UnsyncBoxBody<
                hyper::body::Bytes,
                wasmtime_wasi_http::p3::bindings::http::types::ErrorCode,
            >,
        >,
        options: Option<wasmtime_wasi_http::p3::RequestOptions>,
        fut: Box<
            dyn std::future::Future<
                    Output = std::result::Result<
                        (),
                        wasmtime_wasi_http::p3::bindings::http::types::ErrorCode,
                    >,
                > + Send,
        >,
    ) -> Box<
        dyn std::future::Future<
                Output = std::result::Result<
                    (
                        hyper::Response<
                            http_body_util::combinators::UnsyncBoxBody<
                                hyper::body::Bytes,
                                wasmtime_wasi_http::p3::bindings::http::types::ErrorCode,
                            >,
                        >,
                        Box<
                            dyn std::future::Future<
                                    Output = std::result::Result<
                                        (),
                                        wasmtime_wasi_http::p3::bindings::http::types::ErrorCode,
                                    >,
                                > + Send,
                        >,
                    ),
                    wasmtime_wasi::TrappableError<
                        wasmtime_wasi_http::p3::bindings::http::types::ErrorCode,
                    >,
                >,
            > + Send,
    > {
        use wasmtime_wasi_http::p3::bindings::http::types::ErrorCode;
        let _ = fut;
        if !self.host_allowed() {
            return Box::new(async move { Err(ErrorCode::HttpRequestDenied.into()) });
        }
        Box::new(async move {
            use http_body_util::BodyExt;
            let (res, io) = wasmtime_wasi_http::p3::default_send_request(request, options).await?;
            Ok((
                res.map(BodyExt::boxed_unsync),
                Box::new(io) as Box<dyn std::future::Future<Output = _> + Send>,
            ))
        })
    }
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

impl wk_vfs::VfsView for HostState {
    fn fs(&mut self) -> crate::vfs::SharedFs {
        self.fs.clone()
    }
}

impl wasmtime_wasi_io::IoView for HostState {
    fn table(&mut self) -> &mut ResourceTable {
        &mut self.table
    }
}

/// A `MainThreadSpawner` that runs the closure in place: wk does not create
/// wgpu surfaces on a dedicated UI thread (we render offscreen), so no thread
/// hop is needed.
struct InPlaceSpawner;

impl wasi_webgpu_wasmtime::MainThreadSpawner for InPlaceSpawner {
    async fn spawn<F, T>(&self, f: F) -> T
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        f()
    }
}

impl wasi_webgpu_wasmtime::WasiWebGpuView for HostState {
    fn instance(&self) -> Arc<wgpu_core::global::Global> {
        Arc::clone(&self.gpu)
    }

    fn ui_thread_spawner(&self) -> Box<impl wasi_webgpu_wasmtime::MainThreadSpawner + 'static> {
        Box::new(InPlaceSpawner)
    }
}

fn new_gpu_instance() -> Arc<wgpu_core::global::Global> {
    Arc::new(wgpu_core::global::Global::new(
        "wk-webgpu",
        wgpu_types::InstanceDescriptor {
            backends: wgpu_types::Backends::all(),
            flags: wgpu_types::InstanceFlags::from_build_config(),
            backend_options: Default::default(),
            memory_budget_thresholds: Default::default(),
            display: None,
        },
        None,
    ))
}

impl HostState {
    fn surface_shared(&mut self, res: &Resource<SurfaceState>) -> Result<SharedSurface> {
        Ok(self.table.get(res)?.shared.clone())
    }

    fn subscribe_kind(
        &mut self,
        res: &Resource<SurfaceState>,
        kind: PollKind,
    ) -> Result<Resource<DynPollable>> {
        let shared = self.surface_shared(res)?;
        let p = self.table.push(SurfacePollable { shared, kind })?;
        subscribe(&mut self.table, p)
    }
}

impl wasi::surface::surface::Host for HostState {}
impl wasi::graphics_context::graphics_context::Host for HostState {}
impl wasi::frame_buffer::frame_buffer::Host for HostState {}

impl wasi::surface::surface::HostSurface for HostState {
    fn new(&mut self, desc: CreateDesc) -> Result<Resource<SurfaceState>> {
        let width = desc.width.unwrap_or(256);
        let height = desc.height.unwrap_or(256);
        let shared = Arc::new(Mutex::new(VirtualSurface::new(self.node_id, width, height)));
        self.registry.lock().unwrap().push(shared.clone());
        Ok(self.table.push(SurfaceState { shared })?)
    }

    fn connect_graphics_context(
        &mut self,
        self_: Resource<SurfaceState>,
        context: Resource<ContextState>,
    ) -> Result<()> {
        let shared = self.surface_shared(&self_)?;
        self.table.get_mut(&context)?.connected = Some(shared);
        Ok(())
    }

    fn height(&mut self, self_: Resource<SurfaceState>) -> Result<u32> {
        Ok(self.surface_shared(&self_)?.lock().unwrap().height)
    }

    fn width(&mut self, self_: Resource<SurfaceState>) -> Result<u32> {
        Ok(self.surface_shared(&self_)?.lock().unwrap().width)
    }

    fn request_set_size(
        &mut self,
        self_: Resource<SurfaceState>,
        height: Option<u32>,
        width: Option<u32>,
    ) -> Result<()> {
        let shared = self.surface_shared(&self_)?;
        let mut s = shared.lock().unwrap();
        let (w, h, bytes) = surface_dims(width.unwrap_or(s.width), height.unwrap_or(s.height));
        s.width = w;
        s.height = h;
        s.pixels = vec![0; bytes];
        Ok(())
    }

    fn subscribe_resize(&mut self, self_: Resource<SurfaceState>) -> Result<Resource<DynPollable>> {
        self.subscribe_kind(&self_, PollKind::Resize)
    }
    fn get_resize(&mut self, self_: Resource<SurfaceState>) -> Result<Option<ResizeEvent>> {
        Ok(self.surface_shared(&self_)?.lock().unwrap().resize.take())
    }

    fn subscribe_frame(&mut self, self_: Resource<SurfaceState>) -> Result<Resource<DynPollable>> {
        self.subscribe_kind(&self_, PollKind::Frame)
    }
    fn get_frame(&mut self, self_: Resource<SurfaceState>) -> Result<Option<FrameEvent>> {
        if self.surface_shared(&self_)?.lock().unwrap().closed {
            // Server closed this surface: trap to unwind and end the guest.
            return Err(wasmtime::Error::new(SurfaceClosed));
        }
        Ok(Some(FrameEvent { nothing: false }))
    }

    fn subscribe_pointer_up(
        &mut self,
        self_: Resource<SurfaceState>,
    ) -> Result<Resource<DynPollable>> {
        self.subscribe_kind(&self_, PollKind::PointerUp)
    }
    fn get_pointer_up(&mut self, self_: Resource<SurfaceState>) -> Result<Option<PointerEvent>> {
        Ok(self
            .surface_shared(&self_)?
            .lock()
            .unwrap()
            .pointer_up
            .pop_front())
    }

    fn subscribe_pointer_down(
        &mut self,
        self_: Resource<SurfaceState>,
    ) -> Result<Resource<DynPollable>> {
        self.subscribe_kind(&self_, PollKind::PointerDown)
    }
    fn get_pointer_down(&mut self, self_: Resource<SurfaceState>) -> Result<Option<PointerEvent>> {
        Ok(self
            .surface_shared(&self_)?
            .lock()
            .unwrap()
            .pointer_down
            .pop_front())
    }

    fn subscribe_pointer_move(
        &mut self,
        self_: Resource<SurfaceState>,
    ) -> Result<Resource<DynPollable>> {
        self.subscribe_kind(&self_, PollKind::PointerMove)
    }
    fn get_pointer_move(&mut self, self_: Resource<SurfaceState>) -> Result<Option<PointerEvent>> {
        Ok(self
            .surface_shared(&self_)?
            .lock()
            .unwrap()
            .pointer_move
            .pop_front())
    }

    fn subscribe_pointer_scroll(
        &mut self,
        self_: Resource<SurfaceState>,
    ) -> Result<Resource<DynPollable>> {
        // Subscribing is the guest's declaration that it consumes scroll —
        // the compositor routes the wheel to this surface from here on.
        self.surface_shared(&self_)?.lock().unwrap().wants_scroll = true;
        self.subscribe_kind(&self_, PollKind::PointerScroll)
    }
    fn get_pointer_scroll(&mut self, self_: Resource<SurfaceState>) -> Result<Option<ScrollEvent>> {
        Ok(self
            .surface_shared(&self_)?
            .lock()
            .unwrap()
            .pointer_scroll
            .pop_front())
    }

    fn subscribe_key_up(&mut self, self_: Resource<SurfaceState>) -> Result<Resource<DynPollable>> {
        self.subscribe_kind(&self_, PollKind::KeyUp)
    }
    fn get_key_up(&mut self, self_: Resource<SurfaceState>) -> Result<Option<KeyEvent>> {
        Ok(self
            .surface_shared(&self_)?
            .lock()
            .unwrap()
            .key_up
            .pop_front())
    }

    fn subscribe_key_down(
        &mut self,
        self_: Resource<SurfaceState>,
    ) -> Result<Resource<DynPollable>> {
        self.subscribe_kind(&self_, PollKind::KeyDown)
    }
    fn get_key_down(&mut self, self_: Resource<SurfaceState>) -> Result<Option<KeyEvent>> {
        Ok(self
            .surface_shared(&self_)?
            .lock()
            .unwrap()
            .key_down
            .pop_front())
    }

    fn drop(&mut self, rep: Resource<SurfaceState>) -> Result<()> {
        // Remove the surface from the shared registry the client iterates every
        // frame — otherwise a guest that creates surfaces in a loop grows it
        // (and leaks the client's GPU texture) without bound until node close.
        let shared = self.table.get(&rep)?.shared.clone();
        {
            let mut g = shared.lock().unwrap();
            g.closed = true;
            g.wake();
        }
        self.registry
            .lock()
            .unwrap()
            .retain(|s| !Arc::ptr_eq(s, &shared));
        self.table.delete(rep)?;
        Ok(())
    }
}

impl wasi::graphics_context::graphics_context::HostContext for HostState {
    fn new(&mut self) -> Result<Resource<ContextState>> {
        Ok(self.table.push(ContextState { connected: None })?)
    }

    fn get_current_buffer(
        &mut self,
        self_: Resource<ContextState>,
    ) -> Result<Resource<AbstractBufferState>> {
        let shared = self
            .table
            .get(&self_)?
            .connected
            .clone()
            .expect("graphics-context not connected to a surface");
        Ok(self.table.push(AbstractBufferState { shared })?)
    }

    fn present(&mut self, _self_: Resource<ContextState>) -> Result<()> {
        // Decoupled compositing: the pixels were already written via the
        // frame-buffer; the server reads the latest buffer each frame.
        Ok(())
    }

    fn drop(&mut self, rep: Resource<ContextState>) -> Result<()> {
        self.table.delete(rep)?;
        Ok(())
    }
}

impl wasi::graphics_context::graphics_context::HostAbstractBuffer for HostState {
    fn drop(&mut self, rep: Resource<AbstractBufferState>) -> Result<()> {
        self.table.delete(rep)?;
        Ok(())
    }
}

impl wasi::frame_buffer::frame_buffer::HostDevice for HostState {
    fn new(&mut self) -> Result<Resource<DeviceState>> {
        Ok(self.table.push(DeviceState { connected: None })?)
    }

    fn connect_graphics_context(
        &mut self,
        self_: Resource<DeviceState>,
        context: Resource<ContextState>,
    ) -> Result<()> {
        let shared = self.table.get(&context)?.connected.clone();
        self.table.get_mut(&self_)?.connected = shared;
        Ok(())
    }

    fn drop(&mut self, rep: Resource<DeviceState>) -> Result<()> {
        self.table.delete(rep)?;
        Ok(())
    }
}

impl wasi::frame_buffer::frame_buffer::HostBuffer for HostState {
    fn from_graphics_buffer(
        &mut self,
        buffer: Resource<AbstractBufferState>,
    ) -> Result<Resource<BufferState>> {
        let shared = self.table.get(&buffer)?.shared.clone();
        self.table.delete(buffer)?;
        Ok(self.table.push(BufferState { shared })?)
    }

    fn get(&mut self, self_: Resource<BufferState>) -> Result<Vec<u8>> {
        Ok(self
            .table
            .get(&self_)?
            .shared
            .lock()
            .unwrap()
            .pixels
            .clone())
    }

    fn set(&mut self, self_: Resource<BufferState>, val: Vec<u8>) -> Result<()> {
        let shared = self.table.get(&self_)?.shared.clone();
        shared.lock().unwrap().pixels = val;
        Ok(())
    }

    fn drop(&mut self, rep: Resource<BufferState>) -> Result<()> {
        self.table.delete(rep)?;
        Ok(())
    }
}

/// Whether a component is a standard `wasi:cli/command` (exports `wasi:cli/run`)
/// rather than a wk-world guest (which exports a bare `run`).
fn component_is_command(component: &Component, engine: &Engine) -> bool {
    component
        .component_type()
        .exports(engine)
        .any(|(name, _)| name == "wasi:cli/run" || name.starts_with("wasi:cli/run@"))
}

/// Whether a component is a *wasip3* command (exports `wasi:cli/run@0.3.x`):
/// it must be instantiated against the 0.3 world and driven through the
/// component-model-async `run_concurrent` machinery, not the 0.2 `call_run`.
fn component_is_p3_command(component: &Component, engine: &Engine) -> bool {
    component
        .component_type()
        .exports(engine)
        .any(|(name, _)| name.starts_with("wasi:cli/run@0.3"))
}

/// Run a `wasi:cli/run` command component to completion — either WASI
/// generation, chosen by the version the component exports. Returns the
/// guest's exit code (`exit(n)` and a failed `run` both count as status, not
/// host errors); `Err` is a real trap or instantiation failure.
async fn run_command(
    store: &mut Store<HostState>,
    component: &Component,
    linker: &Linker<HostState>,
) -> Result<i32> {
    let engine = store.engine().clone();
    let outcome = if component_is_p3_command(component, &engine) {
        let command =
            wasmtime_wasi::p3::bindings::Command::instantiate_async(&mut *store, component, linker)
                .await?;
        store
            .run_concurrent(async move |acc| command.wasi_cli_run().call_run(acc).await)
            .await?
    } else {
        let command =
            wasmtime_wasi::p2::bindings::Command::instantiate_async(&mut *store, component, linker)
                .await?;
        command.wasi_cli_run().call_run(&mut *store).await
    };
    match outcome {
        Ok(Ok(())) => Ok(0),
        Ok(Err(())) => Ok(1),
        Err(e) => match e.downcast_ref::<wasmtime_wasi::I32Exit>() {
            // exit(n) is how a CLI reports status, not a failure.
            Some(wasmtime_wasi::I32Exit(code)) => Ok(*code),
            None => Err(e),
        },
    }
}

/// Whether a component imports `wasi:sockets` — i.e. it does networking and so
/// needs a NIC on the fabric.
fn component_imports_sockets(component: &Component, engine: &Engine) -> bool {
    component
        .component_type()
        .imports(engine)
        .any(|(name, _)| name.starts_with("wasi:sockets/"))
}

/// Whether a component imports `wk:midi` — i.e. it sends and/or receives MIDI,
/// so the UI should offer it MIDI ports.
fn component_imports_midi(component: &Component, engine: &Engine) -> bool {
    component
        .component_type()
        .imports(engine)
        .any(|(name, _)| name == "wk:midi/midi" || name.starts_with("wk:midi/"))
}

/// Whether a component imports `wk:capture` — i.e. it consumes captured canvas
/// frames, so the UI should offer it a Capture port.
fn component_imports_capture(component: &Component, engine: &Engine) -> bool {
    component
        .component_type()
        .imports(engine)
        .any(|(name, _)| name.starts_with("wk:capture/"))
}

/// Whether a component imports `wk:clipboard` — i.e. it copies and pastes, so
/// the UI should offer it a Clipboard port. Importing is not permission: the
/// port is only an affordance for drawing the wire that grants it.
fn component_imports_clipboard(component: &Component, engine: &Engine) -> bool {
    component
        .component_type()
        .imports(engine)
        .any(|(name, _)| name.starts_with("wk:clipboard/"))
}

/// Whether a component imports `wk:fs/provider` — i.e. it serves a filesystem
/// other nodes can mount, so the server offers it as a mount source.
fn component_imports_fs_provider(component: &Component, engine: &Engine) -> bool {
    component
        .component_type()
        .imports(engine)
        .any(|(name, _)| name.starts_with("wk:fs/"))
}

/// Whether a component is a `wasi:http` server (exports `incoming-handler`).
fn component_is_proxy(component: &Component, engine: &Engine) -> bool {
    component
        .component_type()
        .exports(engine)
        .any(|(name, _)| name.starts_with("wasi:http/incoming-handler"))
}

/// Add the standard `wasi:random` interfaces, backed by this store's own RNG.
/// (We replicate wasmtime-wasi's linker setup without its filesystem, so its
/// `add_to_linker_async` — which would also add the cap-std fs — can't be used;
/// its random accessor reads a private `WasiCtx` field, so we carry our own.)
fn add_random(l: &mut Linker<HostState>) -> Result<()> {
    use wasmtime_wasi::p2::bindings::random;
    use wasmtime_wasi::random::WasiRandom;
    random::random::add_to_linker::<_, WasiRandom>(l, |s: &mut HostState| &mut s.random_ctx)?;
    random::insecure::add_to_linker::<_, WasiRandom>(l, |s| &mut s.random_ctx)?;
    random::insecure_seed::add_to_linker::<_, WasiRandom>(l, |s| &mut s.random_ctx)?;
    Ok(())
}

/// Build-time `RUN` execution: run a wasi:cli command component against the
/// build's rootfs (its writes become the RUN's layer). stdout/stderr pass
/// through to wk's own, like `docker build` streaming a step's output.
impl PluginHost {
    /// Run a wasm component to completion, sharing `fs`, with its stdio
    /// captured — the engine behind `wk:exec` (and the same machinery a
    /// Dockerfile `RUN` step uses, minus the inherited stdio).
    ///
    /// The child is deliberately impoverished: it gets the caller's
    /// filesystem and nothing else — no surfaces, no MIDI, no capture, no
    /// network, and no `wk:exec` of its own beyond `depth`. It therefore can't
    /// reach anything its parent couldn't.
    pub(crate) fn run_program(
        &self,
        wasm: &[u8],
        argv: &[String],
        env: &[(String, String)],
        fs: &crate::vfs::SharedFs,
        stdin: Vec<u8>,
        depth: u32,
    ) -> std::result::Result<crate::exec::Output, String> {
        self.spawn_program(
            wasm,
            argv,
            env,
            fs,
            Stdin::Bytes(stdin),
            Sink::Capture,
            Sink::Capture,
            depth,
        )?
        .wait()
    }

    /// Start a program and *don't* wait for it.
    ///
    /// This is what a pipeline needs and [`run_program`](Self::run_program)
    /// cannot give: with `run`, the producer must finish before the consumer
    /// starts, because the consumer's stdin is the producer's collected
    /// output. Here both children are live at once and the bytes move through
    /// a [`Pipe`](crate::execpipe::Pipe) as they are written, so
    /// `seq 1 100000 | head -1` finishes early and `yes | head` doesn't buffer
    /// the universe.
    ///
    /// The child is impoverished exactly as in `run_program`: the caller's
    /// filesystem and nothing else.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn spawn_program(
        &self,
        wasm: &[u8],
        argv: &[String],
        env: &[(String, String)],
        fs: &crate::vfs::SharedFs,
        stdin: Stdin,
        stdout: Sink,
        stderr: Sink,
        depth: u32,
    ) -> std::result::Result<Child, String> {
        use wasmtime_wasi::p2::pipe::{MemoryInputPipe, MemoryOutputPipe};

        let component = Component::new(&self.engine, wasm)
            .map_err(|e| format!("{}: not a runnable component: {e:#}", argv[0]))?;
        let linker = self
            .build_linker()
            .map_err(|e| format!("link program: {e:#}"))?;

        // Only a captured sink has bytes to hand back at `wait`; a piped one
        // has already delivered them to whoever is reading the other end.
        let mut b = WasiCtxBuilder::new();
        b.args(argv);
        match stdin {
            Stdin::Empty => b.stdin(MemoryInputPipe::new(Vec::new())),
            Stdin::Bytes(bytes) => b.stdin(MemoryInputPipe::new(bytes)),
            Stdin::Pipe(r) => b.stdin(r),
        };
        let out = match stdout {
            Sink::Capture => {
                let p = MemoryOutputPipe::new(crate::exec::MAX_OUTPUT);
                b.stdout(p.clone());
                Some(p)
            }
            Sink::Pipe(w) => {
                b.stdout(w);
                None
            }
        };
        let err = match stderr {
            Sink::Capture => {
                let p = MemoryOutputPipe::new(crate::exec::MAX_OUTPUT);
                b.stderr(p.clone());
                Some(p)
            }
            Sink::Pipe(w) => {
                b.stderr(w);
                None
            }
        };
        for (k, v) in env {
            b.env(k, v);
        }
        let state = HostState {
            ctx: b.build(),
            table: ResourceTable::new(),
            registry: Arc::new(Mutex::new(Vec::new())),
            node_id: NodeId::nil(),
            fs: fs.clone(),
            term_io: crate::terminal::TermIo::new(),
            capture_src: crate::capture::new_src(),
            capture_seq: 0,
            // No clipboard. Only a NODE wired to a Clipboard node on the
            // canvas gets one, and this store is not a node — it is a build
            // step, an http request, an exec'd child or a bare surface probe.
            // Both permits stay false, so `get` returns none and `set` drops.
            clip_src: crate::clipboard::new_src(),
            clip_read: crate::clipboard::new_permit(),
            clip_write: crate::clipboard::new_permit(),
            clip_denied_logged: false,
            // Children may nest further, up to exec::MAX_DEPTH.
            exec: Some(crate::exec::ExecCtx {
                host: Arc::new(self.clone()),
                depth,
                permit: crate::exec::new_permit(true),
            }),
            midi_in: crate::midi::new_inbox(),
            midi_router: self.midi.clone(),
            scene_reg: crate::scene::new_registry(),
            options: crate::options::new_options(Vec::new()),
            net: None,
            fs_serve: None,
            random_ctx: wasmtime_wasi::random::WasiRandomCtx::default(),
            http_ctx: wasmtime_wasi_http::WasiHttpCtx::new(),
            http_hooks: GatedHttpHooks { stack: None },
            gpu: Arc::clone(&self.gpu),
        };
        let engine = self.engine.clone();
        let name = argv[0].clone();
        // The caller is *already* inside a tokio runtime (the guest's own
        // async host call), so the child gets its own thread: nesting
        // `block_on` in a running runtime panics. Handing back the join handle
        // rather than joining here is the whole difference between `run` and
        // `spawn` — and it is why the two ends of a pipe get polled from two
        // different runtimes, which `execpipe` is built for.
        let join = std::thread::Builder::new()
            .name("wk-exec-child".into())
            .spawn(move || {
                let mut store = Store::new(&engine, state);
                // Epochs kill runaway *nodes*; a child inherits that budget
                // rather than being cut off by a tick meant for its parent.
                store.set_epoch_deadline(1);
                store.epoch_deadline_callback(|_| Ok(wasmtime::UpdateDeadline::Continue(1)));
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_time()
                    .build()
                    .map_err(|e| format!("tokio runtime: {e}"))?;
                rt.block_on(async move {
                    run_command(&mut store, &component, &linker)
                        .await
                        .map_err(|e| format!("run: {e:#}"))
                })
            })
            .map_err(|e| format!("spawn child thread: {e}"))?;
        Ok(Child {
            join,
            out,
            err,
            name,
        })
    }
}

/// Where a spawned child's stdin comes from.
pub enum Stdin {
    /// Immediate end-of-file.
    Empty,
    /// These bytes, then end-of-file.
    Bytes(Vec<u8>),
    /// The reading end of a pipe — whatever the other end writes, as it is
    /// written.
    Pipe(crate::execpipe::PipeReader),
}

/// Where a spawned child's stdout or stderr goes.
pub enum Sink {
    /// Collected in memory and returned by [`Child::wait`].
    Capture,
    /// The writing end of a pipe. Dropped when the child exits, which is what
    /// gives the reader its end-of-file.
    Pipe(crate::execpipe::PipeWriter),
}

/// A running program. Dropping it detaches; [`wait`](Self::wait) collects.
pub struct Child {
    join: std::thread::JoinHandle<std::result::Result<i32, String>>,
    out: Option<wasmtime_wasi::p2::pipe::MemoryOutputPipe>,
    err: Option<wasmtime_wasi::p2::pipe::MemoryOutputPipe>,
    name: String,
}

impl Child {
    /// Block until it exits, then report its status and any captured output.
    pub fn wait(self) -> std::result::Result<crate::exec::Output, String> {
        let status = self
            .join
            .join()
            .map_err(|_| format!("{}: child panicked", self.name))?;
        // Read the captures after the join: the child's writers are gone by
        // then, so this sees everything it wrote even if it trapped.
        let stdout = self.out.map(|p| p.contents().to_vec()).unwrap_or_default();
        let stderr = self.err.map(|p| p.contents().to_vec()).unwrap_or_default();
        match status {
            Ok(exit_code) => Ok(crate::exec::Output {
                exit_code,
                stdout,
                stderr,
            }),
            Err(e) => Err(format!("{}: {e}", self.name)),
        }
    }
}

impl HostState {
    /// This store's exec context, if it may run programs.
    pub(crate) fn exec_ctx(&self) -> Option<&crate::exec::ExecCtx> {
        self.exec.as_ref()
    }

    /// This store's filesystem (the node's vfs) — a child shares it.
    pub(crate) fn fs(&self) -> crate::vfs::SharedFs {
        self.fs.clone()
    }
}

impl crate::images::BuildRunner for PluginHost {
    fn run(
        &self,
        wasm: &[u8],
        argv: &[String],
        env: &[(String, String)],
        fs: &crate::vfs::SharedFs,
    ) -> std::result::Result<(), String> {
        let component =
            Component::new(&self.engine, wasm).map_err(|e| format!("compile RUN target: {e:#}"))?;
        let linker = self
            .build_linker()
            .map_err(|e| format!("link RUN step: {e:#}"))?;
        let mut b = WasiCtxBuilder::new();
        b.inherit_stdout().inherit_stderr().args(argv);
        for (k, v) in env {
            b.env(k, v);
        }
        let state = HostState {
            ctx: b.build(),
            table: ResourceTable::new(),
            registry: Arc::new(Mutex::new(Vec::new())),
            node_id: NodeId::nil(),
            fs: fs.clone(),
            term_io: crate::terminal::TermIo::new(),
            capture_src: crate::capture::new_src(),
            capture_seq: 0,
            // No clipboard. Only a NODE wired to a Clipboard node on the
            // canvas gets one, and this store is not a node — it is a build
            // step, an http request, an exec'd child or a bare surface probe.
            // Both permits stay false, so `get` returns none and `set` drops.
            clip_src: crate::clipboard::new_src(),
            clip_read: crate::clipboard::new_permit(),
            clip_write: crate::clipboard::new_permit(),
            clip_denied_logged: false,
            // A RUN step may spawn programs from the image it is building.
            // That is the point of RUN — `RUN ["/bin/bash.wasm", "-c", "mkdir
            // -p /etc && cp a b"]` is a shell running real commands — and it
            // grants no authority the step doesn't already have: the child is
            // read out of the same filesystem and runs against that same
            // filesystem, which the step can already write to directly. It is
            // the same reasoning the node-level rule encodes, that running a
            // program out of your own fs is not escalation.
            exec: Some(crate::exec::ExecCtx {
                host: Arc::new(self.clone()),
                depth: 0,
                permit: crate::exec::new_permit(true),
            }),
            midi_in: crate::midi::new_inbox(),
            midi_router: self.midi.clone(),
            // A build step registers no visible entities; a throwaway registry.
            scene_reg: crate::scene::new_registry(),
            options: crate::options::new_options(Vec::new()),
            net: None,
            fs_serve: None,
            random_ctx: wasmtime_wasi::random::WasiRandomCtx::default(),
            http_ctx: wasmtime_wasi_http::WasiHttpCtx::new(),
            // No fabric stack at build time: outbound http is denied, keeping
            // builds hermetic.
            http_hooks: GatedHttpHooks { stack: None },
            gpu: Arc::clone(&self.gpu),
        };
        let mut store = Store::new(&self.engine, state);
        // The engine runs with epoch interruption (for killing runaway nodes);
        // a build step just keeps going — nothing increments epochs during a
        // CLI build, and a live server's ticks shouldn't abort it either.
        store.set_epoch_deadline(1);
        store.epoch_deadline_callback(|_| Ok(wasmtime::UpdateDeadline::Continue(1)));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .map_err(|e| format!("tokio runtime: {e}"))?;
        rt.block_on(async move {
            match run_command(&mut store, &component, &linker).await {
                Ok(0) => Ok(()),
                Ok(code) => Err(format!("RUN step exited with status {code}")),
                Err(e) => Err(format!("RUN step trapped: {e:#}")),
            }
        })
    }
}

/// Owns the wasmtime engine and spawns plugin clients on their own threads.
#[derive(Clone)]
pub struct PluginHost {
    engine: Engine,
    gpu: Arc<wgpu_core::global::Global>,
    midi: crate::midi::Router,
    scene: crate::scene::SceneRegistry,
    hub: Arc<wk_fabric::netstack::NetHub>,
}

/// Point `config` at the Pulley interpreter when built with the `pulley`
/// feature; otherwise leave it on the host's native backend.
///
/// Pulley is a portable bytecode: compiling to it produces *data*, so the
/// runtime never needs executable pages. That is the whole point — a platform
/// can forbid mapping memory executable (iOS does) and guests still run. The
/// bill is speed, and it varies with how compute-bound a guest is: guests
/// paced by frames or I/O are close to native, while tight compute (crypto, an
/// interpreter's own loop) lands roughly 10-20x slower.
///
/// The whole test suite doubles as the Pulley suite — `cargo test --features
/// pulley` runs every plugin test through this backend.
fn set_compile_target(config: &mut Config) -> Result<()> {
    #[cfg(feature = "pulley")]
    {
        // Pulley's ISA is pointer-width and endianness specific, and the
        // bytecode is produced for whatever the *host* will interpret it on.
        let target = match (
            cfg!(target_pointer_width = "64"),
            cfg!(target_endian = "big"),
        ) {
            (true, false) => "pulley64",
            (true, true) => "pulley64be",
            (false, false) => "pulley32",
            (false, true) => "pulley32be",
        };
        config.target(target)?;
    }
    let _ = config;
    Ok(())
}

/// Per-memory virtual reservation for a constrained host, in MiB.
///
/// wasmtime's default reserves 4GiB of address space per linear memory (plus a
/// 32MiB guard) so bounds checks fold into the guard region. A desktop shrugs
/// that off — the pages are never touched — but it is charged against a
/// process's address space, and wk runs a memory per node: three nodes already
/// reserve 12GiB. Somewhere small enough for a phone, large enough that the
/// guests that matter (DOOM, CPython, NetSurf) don't spend their lives being
/// relocated.
const SMALL_MEMORY_MIB: u64 = 64;

/// Bound how much address space each guest reserves.
///
/// `WK_MEMORY_RESERVATION_MIB` overrides on any build (`0` disables the
/// reservation entirely, so a memory is allocated at its real size and moved
/// when it grows); otherwise a `pulley` build — the shape that targets a
/// phone — takes [`SMALL_MEMORY_MIB`] and everything else keeps wasmtime's
/// defaults, which are the right trade on a desktop.
///
/// Guests are unaffected in behaviour either way: a reservation smaller than a
/// guest's initial memory is simply ignored, and growth past it relocates
/// rather than failing.
fn set_memory_limits(config: &mut Config) {
    let mib = match std::env::var("WK_MEMORY_RESERVATION_MIB")
        .ok()
        .map(|v| v.trim().parse::<u64>())
    {
        Some(Ok(mib)) => Some(mib),
        Some(Err(_)) => {
            eprintln!("wk: ignoring non-numeric WK_MEMORY_RESERVATION_MIB");
            None
        }
        None => cfg!(feature = "pulley").then_some(SMALL_MEMORY_MIB),
    };
    let Some(mib) = mib else { return };
    config.memory_reservation(mib * (1 << 20));
    // Room to grow into before a memory has to be relocated, and a guard small
    // enough to not reintroduce the reservation through the back door.
    config.memory_reservation_for_growth(mib.min(16) * (1 << 20));
    config.memory_guard_size(64 * 1024);
    // Growth must be allowed to relocate, since there is no longer a large
    // reservation to grow into.
    config.memory_may_move(true);
}

impl PluginHost {
    pub fn new() -> Result<Self> {
        let mut config = Config::new();
        set_compile_target(&mut config)?;
        set_memory_limits(&mut config);
        config.wasm_component_model(true);
        // The WebAssembly exception-handling proposal (new `exnref` model), so
        // guests that use setjmp/longjmp run: wasi-sdk lowers setjmp to wasm EH,
        // and with LTO + `-mllvm -wasm-use-legacy-eh=false` it emits the exnref
        // form cranelift supports. This unlocks interpreters (Lua) and the whole
        // error-recovery class of recompiled C/C++.
        config.wasm_exceptions(true);
        // Component-model native async (`stream`/`future`/async funcs), the
        // substrate of every WASI 0.3 interface — required to *instantiate* a
        // wasip3 guest against the 0.3 imports linked below.
        config.wasm_component_model_async(true);
        // Lets the server stop a runaway node: increment_epoch() each frame
        // trips the per-store deadline callback, which traps on `kill`.
        config.epoch_interruption(true);
        // Persist compiled code to an on-disk cache so a plugin is only
        // compiled once ever — subsequent launches load the cached artifact (a
        // debug sqlite drops from ~3s to milliseconds). The cache key covers
        // the compiler settings, so native and Pulley artifacts never mix.
        // Best-effort: if the cache can't be set up, we compile every launch.
        match wasmtime::Cache::from_file(None) {
            Ok(cache) => {
                config.cache(Some(cache));
            }
            Err(e) => eprintln!("wk: compile cache unavailable, compiling fresh: {e}"),
        }
        Ok(Self {
            engine: Engine::new(&config)?,
            gpu: new_gpu_instance(),
            midi: crate::midi::new_router(),
            scene: crate::scene::new_registry(),
            hub: wk_fabric::netstack::NetHub::new(),
        })
    }

    /// Every live wk:scene entity, snapshot for the client view.
    pub fn scene_entities(&self) -> Vec<crate::scene::SharedEntity> {
        self.scene.lock().unwrap().clone()
    }

    /// The fabric hub (for host-side fabric endpoints like the API listener).
    pub(crate) fn hub(&self) -> Arc<wk_fabric::netstack::NetHub> {
        self.hub.clone()
    }

    /// The live scene registry itself (tests seed entities through this).
    #[cfg(test)]
    pub(crate) fn scene_registry(&self) -> crate::scene::SceneRegistry {
        self.scene.clone()
    }

    /// The shared MIDI router, so the server can wire MIDI connections.
    pub fn midi(&self) -> crate::midi::Router {
        self.midi.clone()
    }

    pub fn detach_net(&self, stack: &wk_fabric::netstack::SharedStack) {
        self.hub.detach(stack);
    }

    /// Advance the epoch so every running node re-checks its kill switch.
    pub fn tick_epoch(&self) {
        self.engine.increment_epoch();
    }

    fn build_linker(&self) -> Result<Linker<HostState>> {
        let mut linker: Linker<HostState> = Linker::new(&self.engine);
        // Provide every wasmtime-wasi interface except its filesystem, then our
        // own in-memory filesystem in its place.
        crate::vfs::add_wasi_except_fs(&mut linker)?;
        add_random(&mut linker)?;
        // wk's own wasi:sockets over the userspace network fabric (smoltcp), so
        // networked guests' BSD sockets are routed by wk, not the host OS.
        crate::sockets::add_to_linker(&mut linker)?;
        // WASI 0.3 (`@0.3.0`) interfaces — cli, clocks, random, sockets from
        // wasmtime-wasi, built on the Component Model's native async (no
        // `wasi:io`). Added alongside the 0.2 set above (different version
        // namespaces, no clash) so a guest compiled against either WASI
        // generation runs. p3 in wasmtime-wasi is still experimental; it
        // reuses our existing `WasiCtx` (`HostState: WasiView`), so it's
        // purely additive. wasmtime's own 0.3 *filesystem* is deliberately
        // NOT added — wk's in-memory vfs provides `wasi:filesystem@0.3.0`
        // below, so a 0.3 guest sees the same layers/mounts/devices/provider
        // mounts as a 0.2 guest (previously 0.3 guests saw an empty fs).
        // wasmtime's own 0.3 *sockets* are likewise NOT added — wk's own
        // `wasi:sockets@0.3.0` below rides the same fabric as the 0.2 impl
        // (smoltcp stacks, hub routing, Gateway-gated host access), so a 0.3
        // guest sees its node's virtual network, not the host OS (previously
        // 0.3 sockets were wasmtime's deny-all host-OS impl).
        wasmtime_wasi::p3::cli::add_to_linker(&mut linker)?;
        wasmtime_wasi::p3::clocks::add_to_linker(&mut linker)?;
        wasmtime_wasi::p3::random::add_to_linker(&mut linker)?;
        crate::sockets_p3::add_to_linker(&mut linker)?;
        crate::vfs::p3::add_to_linker(&mut linker)?;
        // Only the wasi:http interfaces (outgoing-handler + types); the rest of
        // the wasi world is already linked above.
        wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker)?;
        // WASI 0.3 http (`@0.3.0` client + types), alongside the 0.2 http above.
        wasmtime_wasi_http::p3::add_to_linker(&mut linker)?;
        crate::vfs::add_to_linker(&mut linker)?;
        crate::audio::add_to_linker(&mut linker)?;
        crate::midi::add_to_linker(&mut linker)?;
        crate::scene::add_to_linker(&mut linker)?;
        // wk:exec — running another program from the node's own filesystem.
        crate::exec::add_to_linker(&mut linker)?;
        // wk:fs — a node serving a filesystem other nodes mount (wk's FUSE).
        crate::fsprov::add_to_linker(&mut linker)?;
        crate::options::add_to_linker(&mut linker)?;
        crate::tty::add_to_linker(&mut linker)?;
        crate::capture::add_to_linker(&mut linker)?;
        // wk:clipboard — the HOST's system clipboard, gated on a wire to a
        // Clipboard node plus two separately-attenuable token actions.
        crate::clipboard::add_to_linker(&mut linker)?;
        wasi::surface::surface::add_to_linker::<_, HasSelf<_>>(&mut linker, |s| s)?;
        wasi::graphics_context::graphics_context::add_to_linker::<_, HasSelf<_>>(
            &mut linker,
            |s| s,
        )?;
        wasi::frame_buffer::frame_buffer::add_to_linker::<_, HasSelf<_>>(&mut linker, |s| s)?;
        wasi_webgpu_wasmtime::add_to_linker(&mut linker)?;
        Ok(linker)
    }

    /// Serve a `wasi:http/incoming-handler` component on `127.0.0.1:port`,
    /// dispatching each request to a fresh isolated store. `term_io` receives the
    /// guest's stdout/stderr (the HostPort/node case); `None` inherits stdio (the
    /// throwaway CLI case). Binds the port synchronously (so a bind failure is
    /// reported to the caller, not swallowed on a background thread); the server
    /// then runs until `kill` is set.
    pub fn serve(
        &self,
        path: &Path,
        port: u16,
        term_io: Option<crate::terminal::SharedTermIo>,
        kill: Arc<AtomicBool>,
    ) -> Result<()> {
        // Bind before spawning so a port conflict is an error here — otherwise
        // `start_serve` would record a server that never actually bound and
        // never retry it.
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        let listener = std::net::TcpListener::bind(addr)
            .map_err(|e| wasmtime::Error::msg(format!("bind {addr}: {e}")))?;
        let component = Component::from_file(&self.engine, path)?;
        let linker = self.build_linker()?;
        let pre =
            wasmtime_wasi_http::p2::bindings::ProxyPre::new(linker.instantiate_pre(&component)?)?;
        // One isolated container filesystem shared across this server's requests.
        let fs = crate::vfs::new_fs();
        let midi_in = crate::midi::new_inbox();
        let midi = self.midi.clone();
        let gpu = self.gpu.clone();
        let make_state = move || HostState {
            // A wasi:http handler is request-scoped; running programs is a
            // node-lifetime capability, so it doesn't get one.
            exec: None,
            ctx: {
                let mut b = WasiCtxBuilder::new();
                b.arg("http");
                match &term_io {
                    Some(io) => {
                        b.stdout(crate::terminal::stdout(io))
                            .stderr(crate::terminal::stdout(io));
                    }
                    None => {
                        b.inherit_stdout().inherit_stderr();
                    }
                }
                b.build()
            },
            table: ResourceTable::new(),
            registry: Arc::new(Mutex::new(Vec::new())),
            node_id: NodeId::nil(),
            fs: fs.clone(),
            // An http handler isn't a terminal; a throwaway TermIo satisfies the
            // `wk:tty/control` impl without affecting anything.
            term_io: term_io.clone().unwrap_or_else(crate::terminal::TermIo::new),
            capture_src: crate::capture::new_src(),
            capture_seq: 0,
            // No clipboard. Only a NODE wired to a Clipboard node on the
            // canvas gets one, and this store is not a node — it is a build
            // step, an http request, an exec'd child or a bare surface probe.
            // Both permits stay false, so `get` returns none and `set` drops.
            clip_src: crate::clipboard::new_src(),
            clip_read: crate::clipboard::new_permit(),
            clip_write: crate::clipboard::new_permit(),
            clip_denied_logged: false,
            midi_in: midi_in.clone(),
            midi_router: midi.clone(),
            scene_reg: crate::scene::new_registry(),
            options: crate::options::new_options(Vec::new()),
            net: None,
            fs_serve: None,
            random_ctx: wasmtime_wasi::random::WasiRandomCtx::default(),
            http_ctx: wasmtime_wasi_http::WasiHttpCtx::new(),
            // A per-request serve store has no fabric stack, so outbound http is
            // denied — an incoming-handler can't proxy to arbitrary hosts.
            http_hooks: GatedHttpHooks { stack: None },
            gpu: gpu.clone(),
        };
        let engine = self.engine.clone();
        std::thread::spawn(move || {
            if let Err(e) = crate::http::serve(engine, pre, make_state, listener, kill) {
                eprintln!("http server error: {e:#}");
            }
        });
        Ok(())
    }

    /// Forward `127.0.0.1:port` into the fabric at `target`'s address (same port
    /// number) — publishing a `wasi:sockets` server node on a HostPort, the way
    /// [`Self::serve`] publishes a `wasi:http` node. Returns once the port is
    /// bound; runs until `kill` is set.
    pub fn forward(
        &self,
        target: wk_fabric::netstack::SharedStack,
        host_port: u16,
        guest_port: u16,
        kill: Arc<AtomicBool>,
    ) -> Result<()> {
        // The fabric crate reports plain anyhow errors; bridge into wasmtime's.
        wk_fabric::portfwd::forward(self.hub.clone(), target, host_port, guest_port, kill)
            .map_err(wasmtime::Error::from_anyhow)
    }

    /// Start an iroh uplink tunneling virtual network `net` (see
    /// [`wk_fabric::uplink`]), with n0's public relays/discovery enabled.
    pub fn uplink(
        &self,
        net: NodeId,
        secret: Option<[u8; 32]>,
    ) -> Result<wk_fabric::uplink::Uplink> {
        wk_fabric::uplink::Uplink::start(self.hub.clone(), net, secret, true)
            .map_err(wasmtime::Error::from_anyhow)
    }

    /// Start a host multicast bridge on virtual network `net` (see
    /// [`wk_fabric::hostmcast`]): the groups that network uses also travel on
    /// the host's real network.
    pub fn multicast_bridge(
        &self,
        net: NodeId,
        iface: Option<std::net::Ipv4Addr>,
        groups: &[wk_fabric::hostmcast::Group],
    ) -> Result<wk_fabric::hostmcast::HostMulticast> {
        wk_fabric::hostmcast::HostMulticast::start(self.hub.clone(), net, iface, groups)
            .map_err(wasmtime::Error::from_anyhow)
    }

    /// Start a Veilid uplink tunneling virtual network `net` (see
    /// [`wk_fabric::veilid`]). `node` namespaces its store; `identity` is the
    /// persisted DHT owner keypair (fresh if `None`).
    pub fn veilid_uplink(
        &self,
        net: NodeId,
        identity: Option<&str>,
        node: NodeId,
    ) -> Result<wk_fabric::veilid::VeilidUplink> {
        wk_fabric::veilid::VeilidUplink::start(self.hub.clone(), net, identity, node)
            .map_err(wasmtime::Error::from_anyhow)
    }

    /// Register a plugin as a `Node` under `id` and return immediately — the
    /// component is compiled on a background thread so other nodes aren't blocked
    /// (Cranelift on a multi-MB debug component takes hundreds of ms to seconds).
    /// Until it's ready the node is in a *loading* state; once compiled the node's
    /// `setup` is published and, for a non-networked non-http node, its guest
    /// starts. A **networked** node (imports wasi:sockets) stays idle so it can be
    /// wired onto a Network/Gateway before it runs; an **http** server node stays
    /// idle until served on a Port.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        &self,
        path: &Path,
        name: &str,
        id: NodeId,
        args: &[String],
        surfaces: SurfaceRegistry,
        nodes: NodeRegistry,
        initial_options: Vec<f32>,
        container: Option<crate::images::ContainerSetup>,
    ) -> Result<()> {
        let node = Arc::new(Node {
            id,
            name: name.to_string(),
            term_io: crate::terminal::TermIo::new(),
            fs: crate::vfs::new_fs(),
            midi_in: crate::midi::new_inbox(),
            // Seeded with any saved values; the guest reads them via `load` at
            // start and overwrites with its current values via `store`.
            options: crate::options::new_options(initial_options),
            finished: Arc::new(AtomicBool::new(false)),
            running: Arc::new(AtomicBool::new(false)),
            kill: Arc::new(AtomicBool::new(false)),
            setup: OnceLock::new(),
            env: container
                .as_ref()
                .map(|c| c.env.clone())
                .unwrap_or_default(),
            layers: container
                .as_ref()
                .map(|c| c.layers.clone())
                .unwrap_or_default(),
            capture_src: crate::capture::new_src(),
            // DENIED until the server's reconciler says otherwise — the
            // opposite default from exec_permit below, and deliberately so.
            // Exec grants a node nothing it does not already have (running a
            // program out of its own filesystem); the clipboard is a genuine
            // cross-sandbox channel, so the window between spawn and the
            // first tick must not be an open one.
            clip_src: crate::clipboard::new_src(),
            clip_read: crate::clipboard::new_permit(),
            clip_write: crate::clipboard::new_permit(),
            // Allowed until the server's reconciler says otherwise (it runs
            // before the guest does).
            exec_permit: crate::exec::new_permit(true),
            fs_serve: wk_vfs::ProviderConn::new(),
        });
        nodes.lock().unwrap().push(node.clone());

        let host = self.clone();
        let path = path.to_path_buf();
        let name = name.to_string();
        let args = args.to_vec();
        std::thread::Builder::new()
            .name(format!("wk-compile-{name}"))
            .spawn(move || {
                // Mount the container image's rootfs layers (Arc-shared,
                // copy-on-write) before the guest can run.
                if let Some(c) = &container {
                    if let Err(e) = crate::images::mount(&node.fs, c) {
                        eprintln!("failed to mount image for {name:?}: {e}");
                        // Publishing never happens, so mark the node finished
                        // rather than leaving it "compiling" forever.
                        node.finished.store(true, Ordering::Relaxed);
                        return;
                    }
                }
                if let Err(e) = host.load_and_setup(&node, &path, &name, &args, surfaces) {
                    eprintln!("failed to load plugin {name:?}: {e:#}");
                    node.finished.store(true, Ordering::Relaxed);
                }
            })
            .expect("spawn compile thread");
        Ok(())
    }

    /// Background: compile the component, work out how to run/wire it, publish the
    /// node's `setup`, then auto-start it unless it's networked or an http server.
    fn load_and_setup(
        &self,
        node: &SharedNode,
        path: &Path,
        name: &str,
        args: &[String],
        surfaces: SurfaceRegistry,
    ) -> Result<()> {
        let component = Component::from_file(&self.engine, path)?;
        // A `wasi:http` server (exports incoming-handler) doesn't run a `run`
        // loop — it's served on demand when wired to a HostPort.
        let is_http = component_is_proxy(&component, &self.engine);
        // A standard `wasi:cli/command` (any `fn main` recompiled to wasm) is run
        // through its `wasi:cli/run` export; a wk-world guest through its `run`.
        let is_command = component_is_command(&component, &self.engine);
        // A node that imports wasi:sockets gets a NIC on the fabric. By default
        // it's alone on its own virtual network (net id = node id) — isolated —
        // until the server wires it to a Network node.
        let imports_sockets = component_imports_sockets(&component, &self.engine);
        let imports_midi = component_imports_midi(&component, &self.engine);
        let imports_capture = component_imports_capture(&component, &self.engine);
        let imports_clipboard = component_imports_clipboard(&component, &self.engine);
        let imports_fs_provider = component_imports_fs_provider(&component, &self.engine);
        let net_stack = if !is_http && imports_sockets {
            // Seeded from the node id so a node keeps its address across
            // re-runs; alloc_ip skips octets already taken by other stacks.
            let ip = self.hub.alloc_ip((2 + (node.id.as_u128() % 250)) as u8);
            Some(self.hub.attach(node.id, ip, name))
        } else {
            None
        };
        let networked = net_stack.is_some();
        let setup = NodeSetup {
            net_stack,
            http_path: is_http.then(|| path.to_path_buf()),
            run: (!is_http).then(|| RunInfo {
                component,
                is_command,
                surfaces,
            }),
            midi: imports_midi,
            net: imports_sockets,
            capture: imports_capture,
            clipboard: imports_clipboard,
            fs_provider: imports_fs_provider,
        };
        // Publish; the server now sees a ready node.
        let _ = node.setup.set(setup);

        // If the node was deleted while it was compiling, `close_node` set its
        // kill flag but couldn't detach a fabric stack that didn't exist yet
        // (setup was unpublished). Honor the deletion now that setup is public:
        // detach the stack we just attached and never start the guest —
        // otherwise it would run unkillable, its id already gone from every
        // table. `detach` is idempotent, so a concurrent `close_node` racing us
        // here is harmless.
        if node.kill.load(Ordering::Relaxed) {
            if let Some(stack) = node.net_stack() {
                self.hub.detach(&stack);
            }
            node.finished.store(true, Ordering::Relaxed);
            return Ok(());
        }

        // Networked nodes wait to be wired + Run; http nodes wait to be served.
        // Everything else runs now (its component is already compiled).
        if !is_http && !networked {
            self.run_node(node, args)?;
        }
        Ok(())
    }

    /// (Re)start a registered node's guest on a fresh store, reusing its
    /// persistent state (filesystem, options, terminal, and — crucially — its
    /// fabric stack, so any network wiring already applied stays in effect).
    /// No-op if the node is already running or isn't runnable (an HTTP server).
    /// `args` are the launch args (argv after the program name).
    pub fn run_node(&self, node: &SharedNode, args: &[String]) -> Result<()> {
        // Still compiling, or an http server node — nothing to run.
        let Some(run) = node.setup.get().and_then(|s| s.run.as_ref()) else {
            return Ok(());
        };
        if node.running.swap(true, Ordering::Relaxed) {
            return Ok(()); // already running
        }
        node.finished.store(false, Ordering::Relaxed);
        node.kill.store(false, Ordering::Relaxed);
        // Re-open stdin in case a previous `stop` closed it (EOF).
        node.term_io.reopen();

        let linker = self.build_linker()?;
        // Reuse the already-compiled component (cheap Arc clone) — never recompile.
        let component = run.component.clone();

        // Rebuild the fabric socket context from the node's existing stack so
        // re-runs keep the same address and network membership.
        let net = node
            .net_stack()
            .map(|stack| crate::sockets::NetCtx::new(stack, self.hub.clone()));

        // argv[0] is the program name, then the configured args (e.g. a filename).
        let mut argv = vec![node.name.clone()];
        argv.extend(args.iter().cloned());
        // Initial $COLUMNS/$LINES from the terminal's current size (the client may
        // have already sized it to the node's window); apps that query the size
        // via ioctl/wk:tty get the live value and follow later resizes.
        let (cols, rows) = node.term_io.size();
        let mut ctx_builder = WasiCtxBuilder::new();
        ctx_builder
            .stdout(crate::terminal::stdout(&node.term_io))
            .stderr(crate::terminal::stdout(&node.term_io))
            .stdin(crate::terminal::stdin(&node.term_io))
            .args(&argv);
        // A container image's ENV first, then the terminal vars (so TERM etc.
        // reflect the actual terminal even if the image sets them).
        for (k, v) in &node.env {
            ctx_builder.env(k, v);
        }
        ctx_builder
            .env("TERM", "xterm-256color")
            .env("COLUMNS", cols.to_string())
            .env("LINES", rows.to_string())
            // A guest cannot see what it is running on, and GUI toolkits need
            // to: winit reports the macOS Command key as `meta`, and a toolkit
            // that follows Mac convention has to swap Ctrl/Meta for Cmd+C to
            // mean Copy. A browser gets this from `navigator`; wk's guests get
            // it from here (plugins/qt/qpa/qwkkeytranslator.cpp reads it).
            .env("WK_HOST_OS", std::env::consts::OS);
        // Outbound http follows the node's fabric stack's host access (gateway).
        let http_stack = net.as_ref().map(|n| n.stack.clone());
        let state = HostState {
            ctx: ctx_builder.build(),
            table: ResourceTable::new(),
            registry: run.surfaces.clone(),
            node_id: node.id,
            fs: node.fs.clone(),
            term_io: node.term_io.clone(),
            capture_src: node.capture_src.clone(),
            capture_seq: 0,
            // Shared with the node, so the server's `sync_clipboard` can point
            // this at a board and flip the permits while the guest is running.
            clip_src: node.clip_src.clone(),
            clip_read: node.clip_read.clone(),
            clip_write: node.clip_write.clone(),
            clip_denied_logged: false,
            exec: Some(crate::exec::ExecCtx {
                host: Arc::new(self.clone()),
                depth: 0,
                permit: node.exec_permit.clone(),
            }),
            midi_in: node.midi_in.clone(),
            midi_router: self.midi.clone(),
            scene_reg: self.scene.clone(),
            options: node.options.clone(),
            net,
            fs_serve: Some(crate::fsprov::FsServeCtx {
                conn: node.fs_serve.clone(),
                kill: node.kill.clone(),
            }),
            random_ctx: wasmtime_wasi::random::WasiRandomCtx::default(),
            http_ctx: wasmtime_wasi_http::WasiHttpCtx::new(),
            http_hooks: GatedHttpHooks { stack: http_stack },
            gpu: Arc::clone(&self.gpu),
        };
        let mut store = Store::new(&self.engine, state);
        // Trap the instance once it has been killed; otherwise keep running.
        store.set_epoch_deadline(1);
        let kill_cb = node.kill.clone();
        store.epoch_deadline_callback(move |_| {
            if kill_cb.load(Ordering::Relaxed) {
                Ok(UpdateDeadline::Interrupt)
            } else {
                Ok(UpdateDeadline::Continue(1))
            }
        });

        let is_command = run.is_command;
        let finished = node.finished.clone();
        let running = node.running.clone();
        let kill = node.kill.clone();
        // A provider node's conduit accepts consumer calls only while its
        // serve loop can answer them; otherwise they fail fast (EIO).
        let fs_conn = node.serves_fs().then(|| node.fs_serve.clone());
        if let Some(conn) = &fs_conn {
            conn.begin_serving();
        }
        std::thread::spawn(move || {
            // Drive the guest on a Tokio current-thread runtime (not pollster):
            // wasmtime-wasi's monotonic clock / timers need a Tokio reactor, so a
            // guest that sleeps would otherwise panic.
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .expect("tokio runtime");
            let result: Result<()> = rt.block_on(async move {
                if is_command {
                    // Either WASI generation, by the `wasi:cli/run` version the
                    // component exports. A clean `exit()` (incl. `main`
                    // returning) or a non-zero status is a normal end, not a
                    // host error.
                    run_command(&mut store, &component, &linker)
                        .await
                        .map(|_| ())
                } else {
                    let compositor =
                        Compositor::instantiate_async(&mut store, &component, &linker).await?;
                    compositor.call_run(&mut store).await
                }
            });
            // The serve loop is gone with the guest: fail in-flight consumer
            // calls and refuse new ones (mounts read EIO until a re-run).
            if let Some(conn) = &fs_conn {
                conn.end_serving();
            }
            finished.store(true, Ordering::Relaxed);
            running.store(false, Ordering::Relaxed);
            match result {
                Ok(()) => {}
                // A clean close (surface closed, or the kill switch tripped):
                // exit quietly.
                Err(_) if kill.load(Ordering::Relaxed) => {}
                Err(e) if e.downcast_ref::<SurfaceClosed>().is_some() => {}
                Err(e) => eprintln!("plugin client exited with error: {e:#}"),
            }
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// wk is a WASI 0.3 host: the standard `@0.3.0` interfaces link onto a
    /// `Linker<HostState>` (proving `HostState: WasiView` satisfies p3), and the
    /// 0.2 and 0.3 generations coexist in one linker without a name clash.
    #[test]
    fn host_links_wasi_0_3_alongside_0_2() {
        let mut config = Config::new();
        config.wasm_component_model(true);
        config.wasm_component_model_async(true);
        let engine = Engine::new(&config).expect("engine");
        let mut linker: Linker<HostState> = Linker::new(&engine);
        crate::vfs::add_wasi_except_fs(&mut linker).expect("wasi 0.2 (minus fs) links");
        // The actual 0.3 composition build_linker uses: wasmtime's
        // cli/clocks/random plus wk's own filesystem and sockets — NOT
        // wasmtime's p3::add_to_linker, whose fs/sockets wk replaces.
        wasmtime_wasi::p3::cli::add_to_linker(&mut linker).expect("wasi 0.3 cli links");
        wasmtime_wasi::p3::clocks::add_to_linker(&mut linker).expect("wasi 0.3 clocks links");
        wasmtime_wasi::p3::random::add_to_linker(&mut linker).expect("wasi 0.3 random links");
        crate::sockets_p3::add_to_linker(&mut linker).expect("wk sockets 0.3 links");
        crate::vfs::p3::add_to_linker(&mut linker).expect("wk vfs 0.3 links");
    }

    /// The full host linker — every wk interface (wasi-gfx, audio, midi, the 0.2
    /// http/vfs/random set) plus the WASI 0.3 set — composes without a name
    /// clash. Guards against a future interface overlapping an existing one.
    #[test]
    fn full_host_linker_builds() {
        let host = PluginHost::new().expect("host");
        host.build_linker().expect("full linker builds");
    }

    /// Input events queued on a [`VirtualSurface`] come back out through the
    /// wasi:surface host methods with their full payloads: a scroll event via
    /// `get-pointer-scroll` (whose subscribe also flips `wants_scroll`, the
    /// compositor's wheel-routing flag) and a pointer-down carrying its button.
    #[test]
    fn scroll_and_button_events_surface_through_the_host() {
        use wasi::surface::surface::HostSurface;

        let host = PluginHost::new().expect("host");
        let mut state = HostState {
            ctx: WasiCtxBuilder::new().build(),
            table: ResourceTable::new(),
            registry: Arc::new(Mutex::new(Vec::new())),
            node_id: NodeId::nil(),
            fs: crate::vfs::new_fs(),
            term_io: crate::terminal::TermIo::new(),
            capture_src: crate::capture::new_src(),
            capture_seq: 0,
            // No clipboard. Only a NODE wired to a Clipboard node on the
            // canvas gets one, and this store is not a node — it is a build
            // step, an http request, an exec'd child or a bare surface probe.
            // Both permits stay false, so `get` returns none and `set` drops.
            clip_src: crate::clipboard::new_src(),
            clip_read: crate::clipboard::new_permit(),
            clip_write: crate::clipboard::new_permit(),
            clip_denied_logged: false,
            exec: None,
            midi_in: crate::midi::new_inbox(),
            midi_router: host.midi.clone(),
            scene_reg: crate::scene::new_registry(),
            options: crate::options::new_options(Vec::new()),
            net: None,
            fs_serve: None,
            random_ctx: wasmtime_wasi::random::WasiRandomCtx::default(),
            http_ctx: wasmtime_wasi_http::WasiHttpCtx::new(),
            http_hooks: GatedHttpHooks { stack: None },
            gpu: Arc::clone(&host.gpu),
        };

        let res = HostSurface::new(
            &mut state,
            CreateDesc {
                width: None,
                height: None,
            },
        )
        .expect("surface");
        let shared = state.registry.lock().unwrap()[0].clone();

        // Subscribing to scroll marks the surface as a scroll consumer.
        assert!(!shared.lock().unwrap().wants_scroll);
        state
            .subscribe_pointer_scroll(Resource::new_own(res.rep()))
            .expect("subscribe scroll");
        assert!(shared.lock().unwrap().wants_scroll);

        // A queued scroll event comes out of get-pointer-scroll intact.
        {
            let mut s = shared.lock().unwrap();
            s.pointer_scroll.push_back(ScrollEvent {
                x: 12.0,
                y: 34.0,
                delta_x: 0.5,
                delta_y: -3.0,
            });
            s.pointer_down.push_back(PointerEvent {
                x: 12.0,
                y: 34.0,
                button: Some(PointerButton::Right),
            });
        }
        let ev = state
            .get_pointer_scroll(Resource::new_own(res.rep()))
            .expect("get scroll")
            .expect("a queued scroll event");
        assert_eq!((ev.x, ev.y), (12.0, 34.0));
        assert_eq!((ev.delta_x, ev.delta_y), (0.5, -3.0));
        assert!(state
            .get_pointer_scroll(Resource::new_own(res.rep()))
            .expect("get scroll")
            .is_none());

        // A queued pointer-down surfaces its button identity.
        let down = state
            .get_pointer_down(Resource::new_own(res.rep()))
            .expect("get down")
            .expect("a queued pointer-down");
        assert_eq!(down.button, Some(PointerButton::Right));
    }

    /// A guest-requested surface size is clamped and its RGBA8 byte length is
    /// computed without overflowing `u32` (65536² * 4 would wrap otherwise).
    #[test]
    fn surface_dims_clamp_without_overflow() {
        let (w, h, bytes) = surface_dims(u32::MAX, u32::MAX);
        assert!(w <= MAX_SURFACE_EDGE && h <= MAX_SURFACE_EDGE);
        assert_eq!(bytes, w as usize * h as usize * 4);
        // Zero clamps up to 1 — no zero-area (empty-buffer) surface.
        assert_eq!(surface_dims(0, 0), (1, 1, 4));
    }

    /// The gfx-compat shim end to end: a C `main()` guest (gfx-smoke) built
    /// against ../../plugins/gfx-compat opens a wasi-gfx surface, paints, and
    /// consumes @0.0.2 input events (a right-button pointer-down and a scroll).
    /// With no compositor client connected the test pumps frames itself: it
    /// plays the server's per-frame role — set `frame_ready` on the
    /// [`VirtualSurface`] and wake its parked pollables — and reads back the
    /// pixels the guest presented. Skipped when the artifact isn't built.
    #[test]
    fn gfx_smoke_c_guest_paints_and_consumes_events() {
        let wasm =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/gfx-smoke/gfx-smoke.wasm");
        if !wasm.exists() {
            eprintln!("skipping: build plugins/gfx-smoke first (./build.sh)");
            return;
        }

        let host = PluginHost::new().expect("host");
        let nodes: NodeRegistry = Arc::new(Mutex::new(Vec::new()));
        let surfaces: SurfaceRegistry = Arc::new(Mutex::new(Vec::new()));
        let id = NodeId::new();
        host.spawn(
            &wasm,
            "gfx-smoke",
            id,
            &[],
            surfaces.clone(),
            nodes.clone(),
            Vec::new(),
            None,
        )
        .expect("spawn");

        // The surface appears once the background compile finishes and the
        // guest's wkgfx_open runs.
        let surface = {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
            loop {
                if let Some(s) = surfaces.lock().unwrap().first().cloned() {
                    break s;
                }
                if let Some(n) = nodes.lock().unwrap().iter().find(|n| n.id == id) {
                    assert!(
                        !n.finished.load(Ordering::Relaxed),
                        "gfx-smoke exited before opening a surface"
                    );
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "gfx-smoke never opened a surface"
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        };

        // Headless frame pacing: signal one frame and wake the guest's parked
        // frame pollable, exactly what the server does per compositor frame.
        let pump_frame = || {
            let mut s = surface.lock().unwrap();
            s.frame_ready = true;
            s.wake();
        };

        // Pump until the guest paints something non-uniform (the gradient).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            pump_frame();
            std::thread::sleep(std::time::Duration::from_millis(15));
            let s = surface.lock().unwrap();
            let non_uniform = s.pixels.chunks_exact(4).any(|px| px != &s.pixels[0..4]);
            if non_uniform {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "gfx-smoke never painted a non-uniform frame"
            );
        }

        // Queue a right-button pointer-down (the @0.0.2 button field) and a
        // scroll event (the new @0.0.2 scroll queue), then pump until the
        // guest has drained both.
        let (px, py) = (30u32, 40u32);
        {
            let mut s = surface.lock().unwrap();
            s.pointer_down.push_back(PointerEvent {
                x: px as f64,
                y: py as f64,
                button: Some(PointerButton::Right),
            });
            s.pointer_scroll.push_back(ScrollEvent {
                x: px as f64,
                y: py as f64,
                delta_x: 0.0,
                delta_y: -1.0,
            });
            s.wake();
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            pump_frame();
            std::thread::sleep(std::time::Duration::from_millis(15));
            let s = surface.lock().unwrap();
            if s.pointer_down.is_empty() && s.pointer_scroll.is_empty() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "gfx-smoke never consumed the queued pointer-down + scroll"
            );
        }

        // The events had a visible effect: the guest draws its square at the
        // pointer, so after a couple more frames the clicked pixel is white
        // (the gradient is never white there: red = x/W caps well below 255).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            pump_frame();
            std::thread::sleep(std::time::Duration::from_millis(15));
            let s = surface.lock().unwrap();
            let i = ((py * s.width + px) * 4) as usize;
            if s.pixels.len() >= i + 4 && s.pixels[i..i + 3] == [255, 255, 255] {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the square never appeared at the queued pointer position"
            );
        }

        // Type a character. `key` and `text` are independent halves of a key
        // event and only the first has been exercised so far (the arrow keys
        // above): `text` is what a text field inserts, and it reached no guest
        // at all while the compositor hardcoded it to `None`. The guest echoes
        // the scalar it decoded into the top-left pixel, so this asserts the
        // whole path — VirtualSurface queue, wasi:surface record, wkgfx's
        // UTF-8 decode, C code — and not merely that the field is populated.
        {
            let mut s = surface.lock().unwrap();
            let ev = KeyEvent {
                key: Some(Key::KeyK),
                text: Some("k".into()),
                alt_key: false,
                ctrl_key: false,
                meta_key: false,
                shift_key: false,
                repeat: false,
            };
            s.key_down.push_back(ev.clone());
            s.key_up.push_back(ev);
            s.wake();
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            pump_frame();
            std::thread::sleep(std::time::Duration::from_millis(15));
            let s = surface.lock().unwrap();
            if s.pixels.len() >= 4 && s.pixels[0..3] == [b'k', 0, 0] {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the typed character never reached the guest (top-left pixel is {:?}, want [107, 0, 0])",
                &s.pixels[0..3.min(s.pixels.len())]
            );
        }

        // Close the surface: the guest traps on its next get-frame and exits.
        {
            let mut s = surface.lock().unwrap();
            s.closed = true;
            s.wake();
        }
        let node = nodes.lock().unwrap().iter().find(|n| n.id == id).cloned();
        if let Some(n) = node {
            n.kill.store(true, Ordering::Relaxed);
        }
    }

    /// A REAL Qt Widgets application (Qt 6.8.4 cross-built for wasm32-wasip2)
    /// paints onto a wk surface through the `wk` QPA plugin.
    ///
    /// This exercises considerably more than gfx-smoke: QApplication startup,
    /// static QPA plugin resolution, the widget layout engine, the raster
    /// paint engine, FreeType+HarfBuzz text out of a compiled-in Qt resource,
    /// the fbconvenience compositor blitting N top-levels into ONE surface,
    /// and an event dispatcher whose only blocking call is the wk frame
    /// pollable. Frames are pumped headless exactly like the gfx-smoke test.
    /// Skipped when the artifact isn't built.
    #[test]
    fn qt_widgets_app_paints_through_the_wk_qpa() {
        let wasm = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/qt/qt-smoke.wasm");
        if !wasm.exists() {
            eprintln!("skipping: build plugins/qt first (./build-smoke.sh)");
            return;
        }

        let host = PluginHost::new().expect("host");
        let nodes: NodeRegistry = Arc::new(Mutex::new(Vec::new()));
        let surfaces: SurfaceRegistry = Arc::new(Mutex::new(Vec::new()));
        let id = NodeId::new();
        host.spawn(
            &wasm,
            "qt-smoke",
            id,
            &[],
            surfaces.clone(),
            nodes.clone(),
            Vec::new(),
            // WK_SMOKE_SELFTEST makes the guest click its own button and print
            // the result, so the test can check widget behaviour and not only
            // pixels.
            Some(crate::images::ContainerSetup {
                layers: Vec::new(),
                env: vec![("WK_SMOKE_SELFTEST".into(), "1".into())],
            }),
        )
        .expect("spawn");

        let node = {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
            loop {
                if let Some(n) = nodes.lock().unwrap().iter().find(|n| n.id == id).cloned() {
                    break n;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "qt-smoke node never appeared"
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        };

        // Compiling a 21 MB component takes a while; wkgfx_open() only runs
        // after that, and after QApplication has built its font database.
        let surface = {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
            loop {
                if let Some(s) = surfaces.lock().unwrap().first().cloned() {
                    break s;
                }
                assert!(
                    !node.finished.load(Ordering::Relaxed),
                    "qt-smoke exited before opening a surface; node log:\n{}",
                    String::from_utf8_lossy(&node.term_io.log_read(0).0)
                );
                assert!(
                    std::time::Instant::now() < deadline,
                    "qt-smoke never opened a surface; node log:\n{}",
                    String::from_utf8_lossy(&node.term_io.log_read(0).0)
                );
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        };

        let pump_frame = || {
            let mut s = surface.lock().unwrap();
            s.frame_ready = true;
            s.wake();
        };

        // A Qt window is not a gradient: the bar is a frame that is neither
        // uniform nor uniformly black. Fusion's window background is a light
        // grey, so a painted frame has both dark (text, button border) and
        // light (background) pixels.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
        let (dark, light) = loop {
            pump_frame();
            std::thread::sleep(std::time::Duration::from_millis(15));
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
                if dark > 100 && light > 10_000 {
                    break (dark, light);
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "qt-smoke never painted a widget frame; node log:\n{}",
                String::from_utf8_lossy(&node.term_io.log_read(0).0)
            );
        };
        eprintln!("qt-smoke frame: {dark} dark px, {light} light px");

        // A pixel histogram proves "not blank"; it does not prove "a window".
        // WK_QT_SMOKE_DUMP=/tmp/f.ppm writes the composited surface out so a
        // human can look at it — which is the only way to catch a QPA plugin
        // that paints something plausible but wrong.
        if let Ok(path) = std::env::var("WK_QT_SMOKE_DUMP") {
            let s = surface.lock().unwrap();
            let mut ppm = format!("P6\n{} {}\n255\n", s.width, s.height).into_bytes();
            ppm.extend(s.pixels.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]));
            std::fs::write(&path, ppm).expect("write frame dump");
            eprintln!("qt-smoke frame dumped to {path}");
            eprintln!(
                "--- qt-smoke node log ---\n{}",
                String::from_utf8_lossy(&node.term_io.log_read(0).0)
            );
        }

        // The guest's own assertion: it clicked its QPushButton directly and
        // the QLabel followed. Pixels prove the paint path; this proves the
        // widget machinery underneath it. It also prints the button's rect in
        // surface coordinates, which is what the input check below aims at.
        let log_now = || String::from_utf8_lossy(&node.term_io.log_read(0).0).to_string();
        // The LAST BUTTON line, not the first: the guest republishes the rect
        // on a repeating timer and the early readings predate the forced
        // resize to the full surface.
        let button_rect = |log: &str| -> Option<(i64, i64, i64, i64)> {
            let line = log.lines().rfind(|l| l.starts_with("BUTTON "))?;
            let n: Vec<i64> = line[7..]
                .split_whitespace()
                .filter_map(|t| t.parse().ok())
                .collect();
            (n.len() == 4).then(|| (n[0], n[1], n[2], n[3]))
        };

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        loop {
            pump_frame();
            std::thread::sleep(std::time::Duration::from_millis(15));
            let log = log_now();
            assert!(
                !log.contains("SELFTEST FAIL"),
                "qt-smoke's self-test failed; node log:\n{log}"
            );
            if log.contains("SELFTEST PASS") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "qt-smoke never reported its self-test; node log:\n{log}"
            );
        }

        // Typing, which is what `text` on a key event is for. The guest has
        // focused its QLineEdit and echoes every change to it, so this drives
        // the whole chain — VirtualSurface queue, wkgfx's UTF-8 decode,
        // QWkKeyTranslator's layout branch, QWidgetLineControl — and asserts
        // on the string a user would see. It runs before the pointer click
        // below because clicking the QPushButton takes the focus away.
        let typed = |s: &mut VirtualSurface, key: Key, text: &str, ctrl: bool, meta: bool| {
            let ev = KeyEvent {
                key: Some(key),
                text: Some(text.into()),
                alt_key: false,
                ctrl_key: ctrl,
                meta_key: meta,
                shift_key: false,
                repeat: false,
            };
            s.key_down.push_back(ev.clone());
            s.key_up.push_back(ev);
            s.wake();
        };
        typed(&mut surface.lock().unwrap(), Key::KeyA, "a", false, false);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            pump_frame();
            std::thread::sleep(std::time::Duration::from_millis(15));
            if log_now().contains("EDIT 'a'") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "a typed 'a' never reached the QLineEdit; node log:\n{}",
                log_now()
            );
        }
        eprintln!("qt-smoke: a real key event typed into the QLineEdit");

        // ...and the other half of typing: a command chord must NOT type its
        // letter. Both are sent because which one is dangerous depends on the
        // host: whichever of ctrl/meta the QPA maps to Qt::MetaModifier slips
        // past QInputControl's exact-ControlModifier guard (QTBUG-35734), and
        // the swap that decides which is which follows WK_HOST_OS.
        typed(&mut surface.lock().unwrap(), Key::KeyS, "s", false, true);
        typed(&mut surface.lock().unwrap(), Key::KeyG, "g", true, false);
        let until = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while std::time::Instant::now() < until {
            pump_frame();
            std::thread::sleep(std::time::Duration::from_millis(15));
            let log = log_now();
            assert!(
                !log.contains("EDIT 'as'") && !log.contains("EDIT 'ag'"),
                "a command chord typed its letter into the QLineEdit; node log:\n{log}"
            );
        }

        // The host told the guest what it is running on, which is the only way
        // a sandboxed toolkit can know whether Cmd or Ctrl is the shortcut key.
        assert!(
            log_now().contains(&format!("host_os={}", std::env::consts::OS)),
            "qt-smoke did not see WK_HOST_OS; node log:\n{}",
            log_now()
        );

        // Now the real thing: a genuine wasi:surface pointer press and release
        // aimed at the button. Reaching the QPushButton means the whole input
        // path works — the host queue, wkgfx_poll_event, QWkInput, and
        // QGuiApplication's null-window hit-testing through
        // QFbScreen::topLevelAt. Re-aim from the newest published rect on each
        // attempt, since the layout settles a few frames in.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        let aimed_at = 'click: loop {
            let rect = loop {
                pump_frame();
                std::thread::sleep(std::time::Duration::from_millis(15));
                if let Some(r) = button_rect(&log_now()) {
                    break r;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "qt-smoke never published a button rect -- its repeating QTimer never fired, \
                     so the dispatcher is not servicing timers; node log:\n{}",
                    log_now()
                );
            };
            let (bx, by) = (rect.0 + rect.2 / 2, rect.1 + rect.3 / 2);
            {
                let mut s = surface.lock().unwrap();
                s.pointer_move.push_back(PointerEvent {
                    x: bx as f64,
                    y: by as f64,
                    button: None,
                });
                s.pointer_down.push_back(PointerEvent {
                    x: bx as f64,
                    y: by as f64,
                    button: Some(PointerButton::Left),
                });
                s.pointer_up.push_back(PointerEvent {
                    x: bx as f64,
                    y: by as f64,
                    button: Some(PointerButton::Left),
                });
                s.wake();
            }
            let attempt = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                pump_frame();
                std::thread::sleep(std::time::Duration::from_millis(15));
                if log_now().contains("clicked 2") {
                    break 'click (bx, by);
                }
                if std::time::Instant::now() > attempt {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "a real pointer click at ({bx}, {by}) never reached the QPushButton; \
                     node log:\n{}",
                    log_now()
                );
            }
        };
        eprintln!("qt-smoke: real pointer click at {aimed_at:?} reached the QPushButton");

        {
            let mut s = surface.lock().unwrap();
            s.closed = true;
            s.wake();
        }
        node.kill.store(true, Ordering::Relaxed);
    }

    /// A real Qt app COPIES to and PASTES from the HOST's system clipboard.
    ///
    /// This is `wk:clipboard` end to end, through the layer a user actually
    /// touches: `Cmd/Ctrl+A` then `Cmd/Ctrl+C` in a `QLineEdit`, and
    /// `QClipboard::text()` for the paste side. Nothing in `qt-smoke` calls
    /// the shim — it calls `QClipboard`, which only reaches `QWkClipboard`
    /// because `QWkIntegration::clipboard()` returns one, which is exactly the
    /// path any Qt app's Copy takes. What is asserted on is the HOST side of
    /// the bridge: the board's `outbox` is the string the local client's
    /// `pump_clipboard` would hand to `arboard::set_text`, so a match there is
    /// "this text reached the machine's clipboard" minus one function call.
    ///
    /// The gate is exercised as its own claim first: the node is spawned with
    /// both permits FALSE (the default a fresh node gets — a Clipboard wire is
    /// what turns them on), and the guest's opening `CLIP` line must show an
    /// empty clipboard even though a board is already sitting there full of
    /// text. That is the difference between "the bridge works" and "the bridge
    /// is a hole".
    ///
    /// Frames are pumped by hand exactly like the paint test; the guest
    /// re-narrates its clipboard on the same repeating timer that publishes
    /// the button rect, because there is no host-side change notification for
    /// a `QClipboard::dataChanged()` to hang on.
    #[test]
    fn qt_app_copies_and_pastes_through_the_host_clipboard() {
        let wasm = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/qt/qt-smoke.wasm");
        if !wasm.exists() {
            eprintln!("skipping: build plugins/qt first (./build-smoke.sh)");
            return;
        }

        let host = PluginHost::new().expect("host");
        let nodes: NodeRegistry = Arc::new(Mutex::new(Vec::new()));
        let surfaces: SurfaceRegistry = Arc::new(Mutex::new(Vec::new()));
        let id = NodeId::new();
        host.spawn(
            &wasm,
            "qt-smoke",
            id,
            &[],
            surfaces.clone(),
            nodes.clone(),
            Vec::new(),
            Some(crate::images::ContainerSetup {
                layers: Vec::new(),
                env: vec![("WK_SMOKE_SELFTEST".into(), "1".into())],
            }),
        )
        .expect("spawn");

        let node = {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
            loop {
                if let Some(n) = nodes.lock().unwrap().iter().find(|n| n.id == id).cloned() {
                    break n;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "qt-smoke node never appeared"
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        };

        // A Clipboard node's board, holding what "the host clipboard" contains.
        // Attached to the node the way `Server::sync_clipboard` attaches it,
        // but with BOTH PERMITS OFF — the state of a node that has a board in
        // reach and a token that says no.
        let board = crate::clipboard::new_board();
        const PASTED: &str = "wk pasted this into Qt";
        {
            let mut b = board.lock().unwrap();
            b.present = true;
            b.seq = 1;
            b.text = PASTED.to_string();
        }
        *node.clip_src.lock().unwrap() = Some(board.clone());

        let surface = {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
            loop {
                if let Some(s) = surfaces.lock().unwrap().first().cloned() {
                    break s;
                }
                assert!(
                    !node.finished.load(Ordering::Relaxed),
                    "qt-smoke exited before opening a surface; node log:\n{}",
                    String::from_utf8_lossy(&node.term_io.log_read(0).0)
                );
                assert!(
                    std::time::Instant::now() < deadline,
                    "qt-smoke never opened a surface; node log:\n{}",
                    String::from_utf8_lossy(&node.term_io.log_read(0).0)
                );
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        };

        let pump_frame = || {
            let mut s = surface.lock().unwrap();
            s.frame_ready = true;
            s.wake();
        };
        let log_now = || String::from_utf8_lossy(&node.term_io.log_read(0).0).to_string();
        // The NEWEST reading, since the guest republishes on a timer.
        let clip_line = |log: &str| -> Option<String> {
            log.lines()
                .rfind(|l| l.starts_with("CLIP "))
                .map(String::from)
        };
        let pump_until = |want: &dyn Fn(&str) -> bool, secs: u64, what: &str| -> String {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
            loop {
                pump_frame();
                std::thread::sleep(std::time::Duration::from_millis(15));
                let log = log_now();
                if want(&log) {
                    return log;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "{what}; node log:\n{log}"
                );
            }
        };

        // ---- 1. DENIED: a board in reach, a token that says no ------------
        // The guest is up and reading its clipboard, and it sees nothing.
        let log = pump_until(
            &|l: &str| clip_line(l).is_some(),
            180,
            "qt-smoke never narrated its clipboard",
        );
        let first = clip_line(&log).unwrap();
        assert_eq!(
            first, "CLIP owns=0 ''",
            "a node with NO clipboard grant read the host clipboard; the token gate is a hole. \
             Board holds {PASTED:?}. node log:\n{log}"
        );
        eprintln!("qt-smoke: clipboard denied without a grant ({first})");

        // ---- 2. PASTE: grant read, and Qt sees what the host holds ---------
        node.clip_read.store(true, Ordering::Relaxed);
        let want = format!("CLIP owns=0 '{PASTED}'");
        let log = pump_until(
            &|l: &str| clip_line(l).as_deref() == Some(want.as_str()),
            60,
            "QClipboard::text() never returned what the host clipboard held",
        );
        let _ = log;
        eprintln!("qt-smoke: pasted the host clipboard into Qt ({want})");

        // ---- 3. COPY: type into the QLineEdit, then select-all + copy ------
        // Which physical modifier means "shortcut" depends on the host, and
        // the guest learns it from WK_HOST_OS — so the test has to agree with
        // it or the chord lands on a different Qt shortcut entirely (Ctrl+A is
        // MoveToStartOfLine on macOS, not SelectAll, and it would clear the
        // selection the copy needs).
        let mac = std::env::consts::OS == "macos";
        let typed = |key: Key, text: &str, chord: bool| {
            let ev = KeyEvent {
                key: Some(key),
                text: Some(text.into()),
                alt_key: false,
                ctrl_key: chord && !mac,
                meta_key: chord && mac,
                shift_key: false,
                repeat: false,
            };
            let mut s = surface.lock().unwrap();
            s.key_down.push_back(ev.clone());
            s.key_up.push_back(ev);
            s.wake();
        };

        const COPIED: &str = "wk";
        typed(Key::KeyW, "w", false);
        typed(Key::KeyK, "k", false);
        let log = pump_until(
            &|l: &str| l.contains(&format!("EDIT '{COPIED}'")),
            60,
            "the text to be copied never reached the QLineEdit",
        );
        let _ = log;

        typed(Key::KeyA, "a", true); // select all
        typed(Key::KeyC, "c", true); // copy

        // ...and `write` is still DENIED, so this copy must go NOWHERE. What
        // it proves is the useful half of the read/write split: the node's own
        // in-process clipboard keeps working — Qt owns it and holds the text —
        // while nothing reaches the machine. This is the state a
        //
        //   check if operation($k,$t,$a), $k != "clipboard" || $a == "read"
        //
        // attenuation puts an app in, and it is worth pinning, because a
        // write-gate that silently leaked would look identical from inside the
        // guest (a denied `set` returns nothing, on purpose).
        let want = format!("CLIP owns=1 '{COPIED}'");
        let log = pump_until(
            &|l: &str| clip_line(l).as_deref() == Some(want.as_str()),
            60,
            "Cmd/Ctrl+C did not even reach Qt's own clipboard",
        );
        let _ = log;
        let until = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while std::time::Instant::now() < until {
            pump_frame();
            std::thread::sleep(std::time::Duration::from_millis(15));
            assert!(
                board.lock().unwrap().outbox.is_none(),
                "a node with clipboard/write DENIED wrote to the host clipboard;                  node log:\n{}",
                log_now()
            );
        }
        eprintln!("qt-smoke: copy stayed inside the node while write was denied ({want})");

        // Now grant `write` and copy again. The selection is untouched, so the
        // same chord is all it takes.
        node.clip_write.store(true, Ordering::Relaxed);
        typed(Key::KeyC, "c", true);

        // The assertion that matters: the string is in the board's OUTBOX,
        // which is precisely what the local client's `pump_clipboard` hands to
        // `arboard::Clipboard::set_text`.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            pump_frame();
            std::thread::sleep(std::time::Duration::from_millis(15));
            if board.lock().unwrap().outbox.is_some() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "Cmd/Ctrl+C in the QLineEdit never reached the host clipboard \
                 (the board's outbox is still empty); node log:\n{}",
                log_now()
            );
        }
        let out = board.lock().unwrap().outbox.take().unwrap();
        assert_eq!(
            out,
            COPIED,
            "the wrong text was copied to the host clipboard; node log:\n{}",
            log_now()
        );
        eprintln!("qt-smoke: Cmd/Ctrl+C in a QLineEdit put {out:?} on the host clipboard");

        // ---- 4. OWNERSHIP: apply the copy the way the client's pump does ---
        // Publishing our own write back must NOT read to the guest as "someone
        // else copied": Qt has to keep saying it owns the clipboard, which is
        // what keeps `m_userMimeData` (and with it every non-text format an
        // in-node copy carried) alive.
        {
            let mut b = board.lock().unwrap();
            b.seq += 1;
            b.text = out.clone();
        }
        let want = format!("CLIP owns=1 '{COPIED}'");
        let log = pump_until(
            &|l: &str| clip_line(l).as_deref() == Some(want.as_str()),
            60,
            "Qt did not keep ownership of a clipboard it had just written",
        );
        let _ = log;
        eprintln!("qt-smoke: Qt still owns the clipboard after its own copy ({want})");

        // ---- 5. A FOREIGN copy takes ownership away ------------------------
        const FOREIGN: &str = "copied somewhere else entirely";
        {
            let mut b = board.lock().unwrap();
            b.seq += 1;
            b.text = FOREIGN.to_string();
        }
        let want = format!("CLIP owns=0 '{FOREIGN}'");
        let log = pump_until(
            &|l: &str| clip_line(l).as_deref() == Some(want.as_str()),
            60,
            "Qt kept claiming a clipboard another application had taken",
        );
        let _ = log;
        eprintln!("qt-smoke: a foreign copy took the clipboard back ({want})");

        // ---- 6. REVOCATION is live ----------------------------------------
        // Attenuating a token flips `clip_read` on the next tick with the guest
        // still running; nothing is restarted and nothing is torn down.
        node.clip_read.store(false, Ordering::Relaxed);
        let log = pump_until(
            &|l: &str| clip_line(l).as_deref() == Some("CLIP owns=0 ''"),
            60,
            "revoking clipboard/read did not stop a running guest from reading it",
        );
        let _ = log;
        eprintln!("qt-smoke: revoking the grant blinded a running guest");

        {
            let mut s = surface.lock().unwrap();
            s.closed = true;
            s.wake();
        }
        node.kill.store(true, Ordering::Relaxed);
    }

    /// A Qt guest is woken by a SOCKET.
    ///
    /// `plugins/qt/qt-net.wasm` is a QGuiApplication that shows no window and
    /// registers no QTimer: it starts a non-blocking connect() to a peer on
    /// the fabric and then sits in `QGuiApplication::exec()` with nothing but
    /// a `QSocketNotifier`. This test deliberately **never pumps a frame**, so
    /// the only thing in the guest's world that can wake
    /// `QWkEventDispatcher`'s single blocking `ppoll` is the file descriptor.
    /// Before socket notifiers existed that block was `wkgfx_wait_frame()` and
    /// this test could only hang.
    ///
    /// Both halves of the notifier contract are asserted, in order:
    /// `SOCKET CONNECTED` can only be printed from a **Write** activation
    /// (wasi-libc registers a CONNECTING socket's own pollable and completes
    /// finish-connect in its poll_finish), and `SOCKET RECV` only from
    /// **Read** activations that ran until EOF — level-triggered, several
    /// passes, no polling timer anywhere.
    ///
    /// The peer is `plugins/netserve`, the same plain-BSD-sockets node the
    /// fabric tests use, addressed BY NODE NAME so the fabric's
    /// ip-name-lookup is in the path too.
    #[test]
    fn qt_socket_notifier_wakes_on_the_fabric() {
        let qt_wasm = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/qt/qt-net.wasm");
        let srv_wasm =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/netserve/netserve.wasm");
        if !qt_wasm.exists() || !srv_wasm.exists() {
            eprintln!("skipping: build plugins/qt (./build-net.sh) and plugins/netserve first");
            return;
        }

        let host = PluginHost::new().expect("host");
        let nodes: NodeRegistry = Arc::new(Mutex::new(Vec::new()));
        let surfaces: SurfaceRegistry = Arc::new(Mutex::new(Vec::new()));

        // Both nodes import wasi:sockets, so both stay idle after spawn until
        // they are wired and Run — which is exactly the handle this test
        // needs: the server must be listening before the client dials.
        let spawn = |path: &Path, name: &str| -> SharedNode {
            let id = NodeId::new();
            host.spawn(
                path,
                name,
                id,
                &[],
                surfaces.clone(),
                nodes.clone(),
                Vec::new(),
                None,
            )
            .expect("spawn");
            let node = nodes
                .lock()
                .unwrap()
                .iter()
                .find(|n| n.id == id)
                .cloned()
                .expect("node registered");
            // First-ever wasmtime compile of a 12 MB Qt component takes
            // minutes; cached runs break out in milliseconds.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(900);
            while !node.is_runnable() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "{name} never compiled"
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            node
        };

        let server = spawn(&srv_wasm, "netserve");
        let client = spawn(&qt_wasm, "qt-net");

        // One shared Network, like a netlink in a .wk workspace.
        let shared_net = NodeId::new();
        for n in [&server, &client] {
            n.net_stack().expect("fabric stack").lock().unwrap().net = shared_net;
        }

        host.run_node(&server, &["8080".to_string()])
            .expect("run netserve");
        {
            let stack = server.net_stack().unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
            loop {
                let listening = stack.lock().unwrap().sockets.iter().any(|(_, s)| {
                    matches!(
                        s,
                        smoltcp::socket::Socket::Tcp(t)
                            if t.state() == smoltcp::socket::tcp::State::Listen
                    )
                });
                if listening {
                    break;
                }
                assert!(
                    !server.finished.load(Ordering::Relaxed),
                    "netserve exited before listening"
                );
                assert!(
                    std::time::Instant::now() < deadline,
                    "netserve never started listening"
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }

        host.run_node(&client, &["netserve".to_string(), "8080".to_string()])
            .expect("run qt-net");

        let log_now = || String::from_utf8_lossy(&client.term_io.log_read(0).0).to_string();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
        let log = loop {
            let log = log_now();
            assert!(
                !log.contains("SOCKET FAIL"),
                "qt-net reported a socket failure; node log:\n{log}"
            );
            if log.contains("SOCKET RECV") {
                break log;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "qt-net never got a socket readiness callback. Its event loop has \
                 no window and no timer, so this is the socket notifier failing, \
                 not a slow paint; node log:\n{log}"
            );
            // NOTE: no frame is pumped here. That is the point of the test.
            std::thread::sleep(std::time::Duration::from_millis(50));
        };

        // Ordering matters: CONNECTING is printed before exec() is entered, so
        // everything after it was delivered by the dispatcher's poll.
        let at = |needle: &str| {
            log.find(needle)
                .unwrap_or_else(|| panic!("missing {needle}"))
        };
        assert!(at("SOCKET WAITING") < at("SOCKET CONNECTED"), "log:\n{log}");
        assert!(at("SOCKET CONNECTED") < at("SOCKET READ"), "log:\n{log}");
        // Level-triggered, and this is the evidence: the Read notifier is
        // never disabled between the two, so `SOCKET RECV` (printed only when
        // read() returns 0) is a SECOND activation of a notifier that was
        // already ready once and was left enabled.
        assert!(at("SOCKET READ") < at("SOCKET RECV"), "log:\n{log}");
        assert!(
            log.contains("hello from a wk node"),
            "qt-net did not receive netserve's banner; node log:\n{log}"
        );
        eprintln!(
            "qt-net: {}",
            log.lines()
                .filter(|l| l.starts_with("SOCKET"))
                .collect::<Vec<_>>()
                .join(" | ")
        );

        client.kill.store(true, Ordering::Relaxed);
        server.kill.store(true, Ordering::Relaxed);
    }

    /// QtNetwork itself — the module, not just the dispatcher — talks to
    /// another wk node.
    ///
    /// `qt_socket_notifier_wakes_on_the_fabric` above proves the lower half:
    /// a raw fd can wake `QWkEventDispatcher`. This proves the upper half, and
    /// it is a different claim, because `plugins/qt/build-qtbase.sh` builds
    /// QtNetwork for a genuine `WASI` CMake platform — where half of Qt's own
    /// network feature CONDITIONs are written `NOT WASM` and so autodetect
    /// back ON against a libc that cannot honour them.
    ///
    /// `plugins/qt/qt-qtnetwork.wasm` runs three stages against
    /// `plugins/netserve`, addressed BY NODE NAME, and each names its own
    /// layer if it fails:
    ///
    /// * `DNS OK` — `QHostInfo::lookupHost()`. With `FEATURE_thread=OFF`,
    ///   qhostinfo.cpp's QThreadPool path compiles out and the lookup runs
    ///   inline, but the result still arrives as a posted event, and the name
    ///   is answered by the fabric's own resolver through wasi:sockets
    ///   ip-name-lookup.
    /// * `TCP RECV` — `QTcpSocket` driven purely by
    ///   connected/readyRead/disconnected. No `waitForReadyRead()`, which
    ///   would block inside `qt_safe_poll()` and prove only that ppoll works;
    ///   going through the signals is what puts QAbstractSocket's own
    ///   notifiers in the dispatcher's poll set.
    /// * `HTTP STATUS 200` — `QNetworkAccessManager`. Upstream this stack
    ///   does not exist without threads (`qt_feature("http" CONDITION
    ///   QT_FEATURE_thread)`, and Qt 6.8's backend really does `new QThread` +
    ///   `QHttpThreadDelegate`). `patches/qtbase-0009` makes the delegate live
    ///   on the calling thread, which is *correct* rather than approximate
    ///   here: with `QT_CONFIG(thread)` off, qobject.cpp compiles the
    ///   BlockingQueuedConnection arm out entirely, so those emits become
    ///   direct calls. If that reasoning is wrong, this assert is where it
    ///   shows.
    ///
    /// There is no TLS stage and there cannot be one: `QT_FEATURE_ssl` is 0
    /// (no SecureTransport off-Apple, no Schannel, no OpenSSL cross-built for
    /// wasm32-wasip2). The guest prints `TLS ABSENT` and the test asserts on
    /// it, so that the day a TLS backend appears this test fails loudly rather
    /// than quietly continuing to claim less than the port can do.
    #[test]
    fn qt_network_speaks_to_a_wk_node() {
        let qt_wasm =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/qt/qt-qtnetwork.wasm");
        let srv_wasm =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/netserve/netserve.wasm");
        // The DNS peer. Authoritative for wk.test and nothing else, so the
        // records asserted below are ones this repo wrote -- QDnsLookup gets
        // tested without reaching the internet.
        let dns_wasm =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/dnsstub/dnsstub.wasm");
        if !qt_wasm.exists() || !srv_wasm.exists() || !dns_wasm.exists() {
            eprintln!(
                "skipping: build plugins/qt (./build-qtnetwork.sh), plugins/netserve \
                 and plugins/dnsstub first"
            );
            return;
        }

        let host = PluginHost::new().expect("host");
        let nodes: NodeRegistry = Arc::new(Mutex::new(Vec::new()));
        let surfaces: SurfaceRegistry = Arc::new(Mutex::new(Vec::new()));

        // Both nodes import wasi:sockets, so both stay idle after spawn until
        // they are wired and Run — the handle this test needs, because the
        // server must be listening before the client dials.
        let spawn = |path: &Path, name: &str| -> SharedNode {
            let id = NodeId::new();
            host.spawn(
                path,
                name,
                id,
                &[],
                surfaces.clone(),
                nodes.clone(),
                Vec::new(),
                None,
            )
            .expect("spawn");
            let node = nodes
                .lock()
                .unwrap()
                .iter()
                .find(|n| n.id == id)
                .cloned()
                .expect("node registered");
            // First-ever wasmtime compile of a 13 MB Qt component takes
            // minutes; cached runs break out in milliseconds.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(900);
            while !node.is_runnable() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "{name} never compiled"
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            node
        };

        let server = spawn(&srv_wasm, "netserve");
        let client = spawn(&qt_wasm, "qt-qtnetwork");

        // One shared Network, like a netlink in a .wk workspace.
        let dns = spawn(&dns_wasm, "dnsstub");
        let shared_net = NodeId::new();
        for n in [&server, &client, &dns] {
            n.net_stack().expect("fabric stack").lock().unwrap().net = shared_net;
        }

        host.run_node(&server, &["8080".to_string()])
            .expect("run netserve");
        {
            let stack = server.net_stack().unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
            loop {
                let listening = stack.lock().unwrap().sockets.iter().any(|(_, s)| {
                    matches!(
                        s,
                        smoltcp::socket::Socket::Tcp(t)
                            if t.state() == smoltcp::socket::tcp::State::Listen
                    )
                });
                if listening {
                    break;
                }
                assert!(
                    !server.finished.load(Ordering::Relaxed),
                    "netserve exited before listening"
                );
                assert!(
                    std::time::Instant::now() < deadline,
                    "netserve never started listening"
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }

        // Port 53: QDnsLookup gives no way to say otherwise -- setNameserver()
        // takes an address and the port is Qt's own default.
        host.run_node(&dns, &["53".to_string()])
            .expect("run dnsstub");

        host.run_node(
            &client,
            &[
                "netserve".to_string(),
                "8080".to_string(),
                "dnsstub".to_string(),
            ],
        )
        .expect("run qt-qtnetwork");

        let log_now = || String::from_utf8_lossy(&client.term_io.log_read(0).0).to_string();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
        let log = loop {
            let log = log_now();
            for stage in [
                "DNS FAIL",
                "TCP FAIL",
                "HTTP FAIL",
                "TLS FAIL",
                "DNSREC FAIL",
                "NET FAIL",
            ] {
                assert!(
                    !log.contains(stage),
                    "qt-qtnetwork reported {stage}; node log:\n{log}"
                );
            }
            if log.contains("DNSREC MX") {
                break log;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "qt-qtnetwork never reached its last stage; node log:\n{log}"
            );
            // No frame is pumped: nothing here paints.
            std::thread::sleep(std::time::Duration::from_millis(50));
        };

        let at = |needle: &str| {
            log.find(needle)
                .unwrap_or_else(|| panic!("missing {needle}; node log:\n{log}"))
        };
        // Ordering is the evidence that each stage really drove the next: the
        // TCP stage is started from inside the DNS callback and the HTTP stage
        // from inside disconnected(), so this sequence cannot be produced by
        // anything except the event loop delivering all three.
        assert!(at("DNS OK") < at("TCP CONNECTED"), "log:\n{log}");
        assert!(at("TCP CONNECTED") < at("TCP RECV"), "log:\n{log}");
        assert!(at("TCP RECV") < at("HTTP STATUS 200"), "log:\n{log}");
        assert!(
            log.matches("hello from a wk node").count() >= 2,
            "both QTcpSocket and QNetworkAccessManager should have read \
             netserve's banner; node log:\n{log}"
        );
        // TLS, as a NEGATIVE result and a security property rather than a
        // missing feature. `TLS ABSENT` is what the build decided
        // (QT_FEATURE_ssl 0); `TLS REJECTED` is what the guest OBSERVED when
        // it aimed an https:// URL at the plaintext peer. A silent downgrade
        // to cleartext would have printed `HTTP STATUS 200` twice and `TLS
        // FAIL` — which the loop above already refuses.
        assert!(
            log.contains("TLS ABSENT") && !log.contains("TLS BUILT"),
            "QT_FEATURE_ssl changed; update this test and PORTING.md rather \
             than deleting the assert; node log:\n{log}"
        );
        assert!(at("HTTP STATUS 200") < at("TLS REJECTED"), "log:\n{log}");
        // QDnsLookup: the record types getaddrinfo cannot express, and a path
        // that shares nothing with stage 1 -- QHostInfo goes through the
        // fabric's ip-name-lookup, whereas this builds a DNS query, sends it
        // over UDP to plugins/dnsstub and parses the answer. It reaches
        // libQt6Network at all only because QDnsLookup is no longer gated on
        // QT_FEATURE_thread, and it can only ANSWER because plugins/resolv-compat
        // supplies the res_n*/dn_expand that wasi-libc lacks.
        assert!(at("TLS REJECTED") < at("DNSREC MX"), "log:\n{log}");
        // Both fields, because they fail differently: a wrong `exchange` means
        // dn_expand mis-walked the name, a wrong `pref` means the MX RDATA was
        // read at the wrong offset. dnsstub serves exactly these.
        assert!(
            log.contains("DNSREC MX mail.wk.test pref=10"),
            "QDnsLookup did not decode dnsstub's MX record; node log:\n{log}"
        );
        eprintln!(
            "qt-qtnetwork: {}",
            log.lines()
                .filter(|l| {
                    ["NET ", "TLS ", "DNS ", "DNSREC ", "TCP ", "HTTP "]
                        .iter()
                        .any(|p| l.starts_with(p))
                })
                .collect::<Vec<_>>()
                .join(" | ")
        );

        client.kill.store(true, Ordering::Relaxed);
        server.kill.store(true, Ordering::Relaxed);
    }

    /// The real thing: UNMODIFIED doomgeneric (plugins/doom) boots Freedoom
    /// Phase 1 as a wk node. The engine decompresses the WAD, draws its title
    /// screen through gfx-compat onto a [`VirtualSurface`], and consumes key
    /// events — the test pumps frames headless exactly like the gfx-smoke
    /// test. Generous deadlines: first-ever wasmtime compile of the engine
    /// plus WAD lump loading can take a while. Skipped when the artifacts
    /// (doom.wasm + freedoom1.wad, both produced by ./build.sh) are missing.
    ///
    /// doom.wasm is built with FEATURE_SOUND (i_wksound.c over wk:webaudio),
    /// and the host constructs a real `AudioContext` — an output device via
    /// cpal — the moment the guest opens audio. That works even headless (and
    /// the Enter below would audibly play the menu switch sound), but tests
    /// must not open sound devices (see audio.rs), so boot with doom's own
    /// vanilla `-nosound` flag: the sound-compiled binary still links and
    /// boots, and no wk:webaudio call is ever made.
    #[test]
    fn doom_boots_freedoom_and_takes_keys() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/doom");
        let wasm = dir.join("doom.wasm");
        let wad = dir.join("freedoom1.wad");
        if !wasm.exists() || !wad.exists() {
            eprintln!("skipping: build plugins/doom first (./build.sh)");
            return;
        }

        let host = PluginHost::new().expect("host");
        let nodes: NodeRegistry = Arc::new(Mutex::new(Vec::new()));
        let surfaces: SurfaceRegistry = Arc::new(Mutex::new(Vec::new()));
        let id = NodeId::new();
        host.spawn(
            &wasm,
            "doom",
            id,
            &[
                "-iwad".to_string(),
                "/freedoom1.wad".to_string(),
                "-nosound".to_string(),
            ],
            surfaces.clone(),
            nodes.clone(),
            Vec::new(),
            None,
        )
        .expect("spawn");

        // The node registers synchronously; seed the IWAD into its filesystem
        // before the (much slower) background compile lets the guest run —
        // standing in for the container image's COPY freedoom1.wad.
        let node = nodes
            .lock()
            .unwrap()
            .iter()
            .find(|n| n.id == id)
            .cloned()
            .expect("node registered");
        node.fs
            .lock()
            .unwrap()
            .put_file_at("freedoom1.wad", std::fs::read(&wad).expect("read wad"));

        // Engine compile + boot, then the surface appears from DG_Init.
        let surface = {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
            loop {
                if let Some(s) = surfaces.lock().unwrap().first().cloned() {
                    break s;
                }
                assert!(
                    !node.finished.load(Ordering::Relaxed),
                    "doom exited before opening a surface"
                );
                assert!(
                    std::time::Instant::now() < deadline,
                    "doom never opened a surface"
                );
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        };
        let pump_frame = || {
            let mut s = surface.lock().unwrap();
            s.frame_ready = true;
            s.wake();
        };

        // Pump until the title screen lands: non-uniform pixels (WAD lumps
        // decompress on the way, so keep the deadline generous).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        loop {
            pump_frame();
            std::thread::sleep(std::time::Duration::from_millis(15));
            let s = surface.lock().unwrap();
            let non_uniform = s.pixels.chunks_exact(4).any(|px| px != &s.pixels[0..4]);
            if non_uniform {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "doom never painted its title screen"
            );
        }

        // Enter opens the main menu: the key round-trips through gfx-compat
        // into doom's event loop, and the frame keeps changing.
        let before: Vec<u8> = surface.lock().unwrap().pixels.clone();
        {
            let mut s = surface.lock().unwrap();
            let enter = |sur: &mut VirtualSurface, down: bool| {
                let ev = KeyEvent {
                    key: Some(Key::Enter),
                    // winit's text for Enter is "\r" and the compositor
                    // forwards it, so send what a real keystroke sends —
                    // doom's `ch` fallback ignores control characters, which
                    // is exactly the property worth exercising here.
                    text: Some("\r".into()),
                    alt_key: false,
                    ctrl_key: false,
                    meta_key: false,
                    shift_key: false,
                    repeat: false,
                };
                if down {
                    sur.key_down.push_back(ev);
                } else {
                    sur.key_up.push_back(ev);
                }
            };
            enter(&mut s, true);
            enter(&mut s, false);
            s.wake();
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        loop {
            pump_frame();
            std::thread::sleep(std::time::Duration::from_millis(15));
            let s = surface.lock().unwrap();
            if s.key_down.is_empty() && s.key_up.is_empty() && s.pixels != before {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "doom never consumed the Enter key / repainted"
            );
        }

        // Shut down: close the surface and trip the kill switch.
        {
            let mut s = surface.lock().unwrap();
            s.closed = true;
            s.wake();
        }
        node.kill.store(true, Ordering::Relaxed);
    }

    /// The real thing, Quake edition: UNMODIFIED quakegeneric (plugins/quake)
    /// boots the id 1.06 shareware episode as a wk node. The engine loads
    /// pak0.pak, sets its palette, and paints the console/title through
    /// gfx-compat onto a [`VirtualSurface`]; then its `startdemos` loop keeps
    /// the frame changing with no input at all — so the test pumps frames
    /// headless and asserts non-uniform, then still-changing, pixels.
    /// Generous deadlines: first-ever wasmtime compile of the engine (with
    /// setjmp/longjmp lowered to exnref EH) plus pak loading can take a
    /// while. Skipped when the artifacts (quake.wasm + pak0.pak, both
    /// produced by ./build.sh) are missing.
    ///
    /// No `-nosound` needed (unlike doom): quakegeneric compiles snd_null.c
    /// and exposes no audio hook, so the node is silent by design and no
    /// audio device is ever opened.
    #[test]
    fn quake_boots_shareware_and_animates() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/quake");
        let wasm = dir.join("quake.wasm");
        let pak = dir.join("pak0.pak");
        if !wasm.exists() || !pak.exists() {
            eprintln!("skipping: build plugins/quake first (./build.sh)");
            return;
        }

        let host = PluginHost::new().expect("host");
        let nodes: NodeRegistry = Arc::new(Mutex::new(Vec::new()));
        let surfaces: SurfaceRegistry = Arc::new(Mutex::new(Vec::new()));
        let id = NodeId::new();
        host.spawn(
            &wasm,
            "quake",
            id,
            &["-basedir".to_string(), "/".to_string()],
            surfaces.clone(),
            nodes.clone(),
            Vec::new(),
            None,
        )
        .expect("spawn");

        // The node registers synchronously; seed the pak into its filesystem
        // before the (much slower) background compile lets the guest run —
        // standing in for the container image's COPY pak0.pak to /id1/.
        let node = nodes
            .lock()
            .unwrap()
            .iter()
            .find(|n| n.id == id)
            .cloned()
            .expect("node registered");
        node.fs
            .lock()
            .unwrap()
            .put_file_at("id1/pak0.pak", std::fs::read(&pak).expect("read pak"));

        // Engine compile + Host_Init, then the surface appears from QG_Init.
        let surface = {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
            loop {
                if let Some(s) = surfaces.lock().unwrap().first().cloned() {
                    break s;
                }
                assert!(
                    !node.finished.load(Ordering::Relaxed),
                    "quake exited before opening a surface"
                );
                assert!(
                    std::time::Instant::now() < deadline,
                    "quake never opened a surface"
                );
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        };
        let pump_frame = || {
            let mut s = surface.lock().unwrap();
            s.frame_ready = true;
            s.wake();
        };

        // Pump until the console lands: non-uniform pixels through the
        // 8-bit-palette-to-RGBA conversion (pak lumps load on the way, so
        // keep the deadline generous).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        loop {
            pump_frame();
            std::thread::sleep(std::time::Duration::from_millis(15));
            let s = surface.lock().unwrap();
            let non_uniform = s.pixels.chunks_exact(4).any(|px| px != &s.pixels[0..4]);
            if non_uniform {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "quake never painted its console"
            );
        }

        // The engine animates on its own (console scroll-up, then the
        // shareware demo loop): keep pumping and the frame must change with
        // no input ever queued.
        let before: Vec<u8> = surface.lock().unwrap().pixels.clone();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        loop {
            pump_frame();
            std::thread::sleep(std::time::Duration::from_millis(15));
            let s = surface.lock().unwrap();
            if s.pixels != before {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "quake never animated past its first frame"
            );
        }

        // Shut down: close the surface and trip the kill switch.
        {
            let mut s = surface.lock().unwrap();
            s.closed = true;
            s.wake();
        }
        node.kill.store(true, Ordering::Relaxed);
    }

    /// Live-coding's whole promise: a host file bind-mounted into the shader
    /// node is re-read every frame, so editing it on disk swaps the running
    /// program. Both shaders here are *static* (a constant colour), so a
    /// change in the rendered pixels can only mean a recompile — an animated
    /// shader would change pixels frame to frame on its own and prove
    /// nothing. A mount that never took would render the colour-bar fallback,
    /// which is not uniform, so the first wait would time out rather than
    /// pass by accident.
    #[test]
    fn shader_hot_reloads_a_bind_mounted_file_edited_on_disk() {
        let wasm = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../plugins/shader/target/wasm32-wasip1/debug/shader.wasm");
        if !wasm.exists() {
            eprintln!("skipping: build plugins/shader first (cargo component build)");
            return;
        }
        // `u` stays referenced (times zero) so the uniform binding can't be
        // optimised out of the pipeline layout; the colour is still constant.
        let src = |rgb: &str| {
            format!("fn main_image(uv: vec2<f32>) -> vec3<f32> {{\n    return {rgb} + vec3<f32>(0.0) * u.time;\n}}\n")
        };
        let dir = std::env::temp_dir().join(format!("wk-shader-live-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("live.wgsl");
        std::fs::write(&path, src("vec3<f32>(1.0, 0.0, 0.0)")).expect("seed the shader file");

        let host = PluginHost::new().expect("host");
        let nodes: NodeRegistry = Arc::new(Mutex::new(Vec::new()));
        let surfaces: SurfaceRegistry = Arc::new(Mutex::new(Vec::new()));
        let id = NodeId::new();
        host.spawn(
            &wasm,
            "shader",
            id,
            &[],
            surfaces.clone(),
            nodes.clone(),
            Vec::new(),
            None,
        )
        .expect("spawn");

        // Bind the file in exactly as a BindMount wire does. The guest
        // rescans `/` for a `.wgsl` every frame, so mounting after start is
        // fine — this is the same "wired in later" path zipfs relies on.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        let node = loop {
            if let Some(n) = nodes.lock().unwrap().iter().find(|n| n.id == id).cloned() {
                break n;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "shader node never registered"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        crate::vfs::mount_host(&node.fs, "/live.wgsl", path.clone(), true);

        let surface = loop {
            if let Some(s) = surfaces.lock().unwrap().first().cloned() {
                break s;
            }
            assert!(
                !node.finished.load(Ordering::Relaxed),
                "shader exited before opening a surface"
            );
            assert!(
                std::time::Instant::now() < deadline,
                "shader never opened a surface"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        };

        // Headless frame pacing, as the compositor would drive it.
        let pump_frame = || {
            let mut s = surface.lock().unwrap();
            s.frame_ready = true;
            s.wake();
        };
        // The single colour the whole surface is painted, once it is painted
        // uniformly at all (`None` while it is blank or mid-paint).
        let uniform_pixel = || -> Option<[u8; 4]> {
            let s = surface.lock().unwrap();
            let px = s.pixels.chunks_exact(4).next()?;
            if px == [0, 0, 0, 0] || s.pixels.chunks_exact(4).any(|p| p != px) {
                return None;
            }
            Some([px[0], px[1], px[2], px[3]])
        };
        let wait_for = |want: &dyn Fn(Option<[u8; 4]>) -> bool, what: &str| -> Option<[u8; 4]> {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
            loop {
                pump_frame();
                std::thread::sleep(std::time::Duration::from_millis(15));
                let px = uniform_pixel();
                if want(px) {
                    return px;
                }
                assert!(
                    !node.finished.load(Ordering::Relaxed),
                    "shader exited while waiting for {what}"
                );
                assert!(
                    std::time::Instant::now() < deadline,
                    "timed out waiting for {what}"
                );
            }
        };

        let first =
            wait_for(&|px| px.is_some(), "the mounted shader to render").expect("a uniform frame");

        // The edit under test: same file, new contents, as a save from vim
        // (or any host editor) would leave it.
        std::fs::write(&path, src("vec3<f32>(0.0, 1.0, 0.0)")).expect("edit the shader file");
        let second = wait_for(
            &|px| matches!(px, Some(p) if p != first),
            "the edit to be picked up",
        )
        .expect("a uniform frame");
        assert_ne!(first, second, "the shader reloaded after the file changed");

        {
            let mut s = surface.lock().unwrap();
            s.closed = true;
            s.wake();
        }
        node.kill.store(true, Ordering::Relaxed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A gatewayed guest reaches a **real host UDP service** — the datagram
    /// counterpart of the TCP gateway, which until now silently went nowhere
    /// (UDP had no `HostConn` equivalent, so off-fabric sends fell into the
    /// fabric and vanished).
    ///
    /// The target must be a non-loopback address: `on_fabric` claims 127/8 for
    /// the guest's own loopback, so a localhost echo server would never take
    /// the host path and the test would pass without proving anything. The
    /// machine's own LAN address is discovered without sending a packet (a
    /// UDP `connect` only picks a route); with no route there is nothing to
    /// prove and the test skips.
    #[test]
    fn gatewayed_guest_reaches_a_host_udp_service() {
        let wasm = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/udpecho/udpecho.wasm");
        if !wasm.exists() {
            eprintln!("skipping: build plugins/udpecho first (./build.sh)");
            return;
        }
        let Some(local_ip) = std::net::UdpSocket::bind("0.0.0.0:0")
            .ok()
            .and_then(|s| s.connect("8.8.8.8:53").ok().map(|_| s))
            .and_then(|s| s.local_addr().ok())
            .map(|a| a.ip())
            .filter(|ip| !ip.is_loopback())
        else {
            eprintln!("skipping: no non-loopback local address to serve on");
            return;
        };

        // A host UDP echo server, reachable at the machine's own address.
        let server = std::net::UdpSocket::bind("0.0.0.0:0").expect("bind host echo");
        let host_port = server.local_addr().expect("local addr").port();
        let stop = Arc::new(AtomicBool::new(false));
        let stopper = stop.clone();
        std::thread::spawn(move || {
            server
                .set_read_timeout(Some(std::time::Duration::from_millis(50)))
                .expect("read timeout");
            let mut buf = [0u8; 2048];
            while !stopper.load(Ordering::Relaxed) {
                if let Ok((n, src)) = server.recv_from(&mut buf) {
                    let mut reply = b"echo:".to_vec();
                    reply.extend_from_slice(&buf[..n]);
                    let _ = server.send_to(&reply, src);
                }
            }
        });

        let host = PluginHost::new().expect("host");
        let nodes: NodeRegistry = Arc::new(Mutex::new(Vec::new()));
        let id = NodeId::new();
        host.spawn(
            &wasm,
            "udpecho",
            id,
            &[],
            Arc::new(Mutex::new(Vec::new())),
            nodes.clone(),
            Vec::new(),
            None,
        )
        .expect("spawn");

        let node = {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
            loop {
                if let Some(n) = nodes.lock().unwrap().iter().find(|n| n.id == id).cloned() {
                    if n.is_runnable() {
                        break n;
                    }
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "udpecho never compiled"
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        };

        // What wiring the node to a Gateway node grants.
        node.net_stack()
            .expect("udpecho has a fabric stack")
            .lock()
            .unwrap()
            .host_access = true;

        host.run_node(
            &node,
            &[
                "client".to_string(),
                local_ip.to_string(),
                host_port.to_string(),
                "hello-gateway".to_string(),
            ],
        )
        .expect("run udpecho");

        let want = "echo:hello-gateway";
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            let (bytes, _) = node.term_io.log_read(0);
            let out = String::from_utf8_lossy(&bytes).to_string();
            if out.contains(want) {
                break;
            }
            if node.finished.load(Ordering::Relaxed) {
                let (bytes, _) = node.term_io.log_read(0);
                let out = String::from_utf8_lossy(&bytes).to_string();
                assert!(out.contains(want), "udpecho exited without the echo: {out}");
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no echo from the host UDP service"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        stop.store(true, Ordering::Relaxed);
        node.kill.store(true, Ordering::Relaxed);
    }

    /// wk's FUSE, end to end with real wasm: the hellofs plugin (a `wk:fs`
    /// provider) is spawned as a node, its served tree is mounted into a
    /// consumer filesystem, and reads/writes cross the mount into the running
    /// guest. Skipped (with a note) when the plugin artifact isn't built.
    #[test]
    fn hellofs_node_serves_a_mounted_filesystem() {
        use wk_vfs::wasi::filesystem::preopens::Host as Preopens;
        use wk_vfs::wasi::filesystem::types::{
            DescriptorFlags, HostDescriptor, OpenFlags, PathFlags,
        };

        let wasm = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../plugins/hellofs/target/wasm32-wasip1/debug/hellofs.wasm");
        if !wasm.exists() {
            eprintln!("skipping: build plugins/hellofs first (cargo component build)");
            return;
        }

        let host = PluginHost::new().expect("host");
        let nodes: NodeRegistry = Arc::new(Mutex::new(Vec::new()));
        let id = NodeId::new();
        host.spawn(
            &wasm,
            "hellofs",
            id,
            &[],
            Arc::new(Mutex::new(Vec::new())),
            nodes.clone(),
            Vec::new(),
            None,
        )
        .expect("spawn");

        // Wait for the background compile to publish setup and the guest to
        // start serving.
        let node = {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
            loop {
                if let Some(n) = nodes.lock().unwrap().iter().find(|n| n.id == id).cloned() {
                    if n.serves_fs() && n.fs_serve.is_serving() {
                        break n;
                    }
                    assert!(
                        !n.finished.load(Ordering::Relaxed),
                        "hellofs exited before serving"
                    );
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "hellofs never started serving"
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        };

        // A consumer node's filesystem with the provider mounted at /hellofs.
        struct ConsumerStore {
            table: ResourceTable,
            fs: crate::vfs::SharedFs,
        }
        impl wasmtime_wasi_io::IoView for ConsumerStore {
            fn table(&mut self) -> &mut ResourceTable {
                &mut self.table
            }
        }
        impl wk_vfs::VfsView for ConsumerStore {
            fn fs(&mut self) -> crate::vfs::SharedFs {
                self.fs.clone()
            }
        }
        let fs = crate::vfs::new_fs();
        crate::vfs::mount_provider(&fs, "/hellofs", node.fs_serve.clone(), true);
        let mut store = wk_vfs::VfsImpl(ConsumerStore {
            table: ResourceTable::new(),
            fs: fs.clone(),
        });
        let root = Preopens::get_directories(&mut store)
            .expect("preopen")
            .remove(0)
            .0;
        let root_fd = || wasmtime::component::Resource::new_own(root.rep());

        // Read the served greeting through the mount.
        let fd = HostDescriptor::open_at(
            &mut store,
            root_fd(),
            PathFlags::SYMLINK_FOLLOW,
            "hellofs/hello.txt".into(),
            OpenFlags::empty(),
            DescriptorFlags::empty(),
        )
        .unwrap()
        .expect("opens the served file");
        let (bytes, _) = HostDescriptor::read(
            &mut store,
            wasmtime::component::Resource::new_own(fd.rep()),
            256,
            0,
        )
        .unwrap()
        .expect("reads the served file");
        assert_eq!(bytes, b"Hello from another node's filesystem!\n");

        // Write a new file into the provider and read it back: the guest's
        // memfs really holds it.
        let wfd = HostDescriptor::open_at(
            &mut store,
            root_fd(),
            PathFlags::SYMLINK_FOLLOW,
            "hellofs/from-consumer.txt".into(),
            OpenFlags::CREATE,
            DescriptorFlags::empty(),
        )
        .unwrap()
        .expect("creates through the mount");
        HostDescriptor::write(
            &mut store,
            wasmtime::component::Resource::new_own(wfd.rep()),
            b"round trip".to_vec(),
            0,
        )
        .unwrap()
        .expect("writes through the mount");
        let (bytes, _) = HostDescriptor::read(
            &mut store,
            wasmtime::component::Resource::new_own(wfd.rep()),
            64,
            0,
        )
        .unwrap()
        .expect("reads back");
        assert_eq!(bytes, b"round trip");

        // Kill the node: its serve loop sees `none`, the conduit detaches, and
        // the mount degrades to EIO instead of hanging.
        node.kill.store(true, Ordering::Relaxed);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while node.running.load(Ordering::Relaxed) {
            assert!(
                std::time::Instant::now() < deadline,
                "hellofs didn't stop after kill"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            HostDescriptor::open_at(
                &mut store,
                root_fd(),
                PathFlags::SYMLINK_FOLLOW,
                "hellofs/hello.txt".into(),
                OpenFlags::empty(),
                DescriptorFlags::empty(),
            )
            .unwrap()
            .is_err(),
            "a dead provider's mount fails instead of hanging"
        );
    }

    /// The libfuse-compat shim end to end: libfuse's UNMODIFIED upstream
    /// hello.c example, compiled against our fuse.h, serves its filesystem
    /// as a wk:fs provider node — `hello` reads "Hello World!\n", the root
    /// lists it, and writes are refused exactly as hello_open's -EACCES /
    /// missing write op dictate. Skipped when the artifact isn't built.
    #[test]
    fn hellofuse_unmodified_libfuse_example_serves() {
        use wk_vfs::wasi::filesystem::preopens::Host as Preopens;
        use wk_vfs::wasi::filesystem::types::{
            DescriptorFlags, ErrorCode, HostDescriptor, OpenFlags, PathFlags,
        };

        let wasm =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/hellofuse/hellofuse.wasm");
        if !wasm.exists() {
            eprintln!("skipping: build plugins/hellofuse first (./build.sh)");
            return;
        }

        let host = PluginHost::new().expect("host");
        let nodes: NodeRegistry = Arc::new(Mutex::new(Vec::new()));
        let id = NodeId::new();
        host.spawn(
            &wasm,
            "hellofuse",
            id,
            &[],
            Arc::new(Mutex::new(Vec::new())),
            nodes.clone(),
            Vec::new(),
            None,
        )
        .expect("spawn");
        let node = {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
            loop {
                if let Some(n) = nodes.lock().unwrap().iter().find(|n| n.id == id).cloned() {
                    if n.serves_fs() && n.fs_serve.is_serving() {
                        break n;
                    }
                    assert!(
                        !n.finished.load(Ordering::Relaxed),
                        "hellofuse exited before serving"
                    );
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "hellofuse never started serving"
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        };

        struct ConsumerStore {
            table: ResourceTable,
            fs: crate::vfs::SharedFs,
        }
        impl wasmtime_wasi_io::IoView for ConsumerStore {
            fn table(&mut self) -> &mut ResourceTable {
                &mut self.table
            }
        }
        impl wk_vfs::VfsView for ConsumerStore {
            fn fs(&mut self) -> crate::vfs::SharedFs {
                self.fs.clone()
            }
        }
        let fs = crate::vfs::new_fs();
        crate::vfs::mount_provider(&fs, "/mnt", node.fs_serve.clone(), true);
        let mut store = wk_vfs::VfsImpl(ConsumerStore {
            table: ResourceTable::new(),
            fs,
        });
        let root = Preopens::get_directories(&mut store)
            .expect("preopen")
            .remove(0)
            .0;
        let root_fd = || wasmtime::component::Resource::new_own(root.rep());

        // The canonical hello file, served by unmodified libfuse example code.
        let fd = HostDescriptor::open_at(
            &mut store,
            root_fd(),
            PathFlags::SYMLINK_FOLLOW,
            "mnt/hello".into(),
            OpenFlags::empty(),
            DescriptorFlags::empty(),
        )
        .unwrap()
        .expect("opens hello");
        let (bytes, eof) = HostDescriptor::read(
            &mut store,
            wasmtime::component::Resource::new_own(fd.rep()),
            64,
            0,
        )
        .unwrap()
        .expect("reads hello");
        assert_eq!(bytes, b"Hello World!\n");
        assert!(eof);

        // stat crosses to hello_getattr.
        let st = HostDescriptor::stat_at(
            &mut store,
            root_fd(),
            PathFlags::SYMLINK_FOLLOW,
            "mnt/hello".into(),
        )
        .unwrap()
        .expect("stats hello");
        assert_eq!(st.size, 13);

        // readdir crosses to hello_readdir (which fills ".", "..", "hello" —
        // the shim strips the dot entries).
        let dirfd = HostDescriptor::open_at(
            &mut store,
            root_fd(),
            PathFlags::SYMLINK_FOLLOW,
            "mnt".into(),
            OpenFlags::DIRECTORY,
            DescriptorFlags::empty(),
        )
        .unwrap()
        .expect("opens the mount root");
        let stream = HostDescriptor::read_directory(&mut store, dirfd)
            .unwrap()
            .expect("lists");
        let mut names = Vec::new();
        loop {
            use wk_vfs::wasi::filesystem::types::HostDirectoryEntryStream;
            match HostDirectoryEntryStream::read_directory_entry(
                &mut store,
                wasmtime::component::Resource::new_own(stream.rep()),
            )
            .unwrap()
            .unwrap()
            {
                Some(e) => names.push(e.name),
                None => break,
            }
        }
        assert_eq!(names, ["hello"]);

        // hello has no write op: mutations come back NotPermitted, and a
        // missing file is the daemon's own -ENOENT.
        assert_eq!(
            HostDescriptor::write(
                &mut store,
                wasmtime::component::Resource::new_own(fd.rep()),
                b"x".to_vec(),
                0,
            )
            .unwrap()
            .unwrap_err(),
            ErrorCode::NotPermitted
        );
        assert_eq!(
            HostDescriptor::open_at(
                &mut store,
                root_fd(),
                PathFlags::SYMLINK_FOLLOW,
                "mnt/nope".into(),
                OpenFlags::empty(),
                DescriptorFlags::empty(),
            )
            .unwrap()
            .unwrap_err(),
            ErrorCode::NoEntry
        );

        node.kill.store(true, Ordering::Relaxed);
    }

    /// libfuse's UNMODIFIED upstream passthrough.c as a provider node: it
    /// mirrors "the underlying filesystem", which in wk is the node's OWN
    /// vfs — so the node re-exports its filesystem to whoever wires it. The
    /// full chain both ways: a consumer's wasi:filesystem ops cross the
    /// provider mount into the shim, into xmp_* callbacks, into wasi-libc,
    /// into the passfs node's vfs — and its writes are visible right back
    /// in that vfs from the host side. Skipped when the artifact isn't built.
    #[test]
    fn passfs_reexports_its_own_vfs() {
        use wk_vfs::wasi::filesystem::preopens::Host as Preopens;
        use wk_vfs::wasi::filesystem::types::{
            DescriptorFlags, HostDescriptor, OpenFlags, PathFlags,
        };

        let wasm = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/passfs/passfs.wasm");
        if !wasm.exists() {
            eprintln!("skipping: build plugins/passfs first (./build.sh)");
            return;
        }

        let host = PluginHost::new().expect("host");
        let nodes: NodeRegistry = Arc::new(Mutex::new(Vec::new()));
        let id = NodeId::new();
        host.spawn(
            &wasm,
            "passfs",
            id,
            &[],
            Arc::new(Mutex::new(Vec::new())),
            nodes.clone(),
            Vec::new(),
            None,
        )
        .expect("spawn");
        let node = {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
            loop {
                if let Some(n) = nodes.lock().unwrap().iter().find(|n| n.id == id).cloned() {
                    if n.serves_fs() && n.fs_serve.is_serving() {
                        break n;
                    }
                    assert!(
                        !n.finished.load(Ordering::Relaxed),
                        "passfs exited before serving"
                    );
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "passfs never started serving"
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        };

        // Seed the provider's OWN filesystem — what passthrough.c mirrors.
        {
            let mut g = node.fs.lock().unwrap();
            g.ensure_dir_path("srv");
            g.put_file_at("srv/motd.txt", b"from the node's own vfs".to_vec());
        }

        struct ConsumerStore {
            table: ResourceTable,
            fs: crate::vfs::SharedFs,
        }
        impl wasmtime_wasi_io::IoView for ConsumerStore {
            fn table(&mut self) -> &mut ResourceTable {
                &mut self.table
            }
        }
        impl wk_vfs::VfsView for ConsumerStore {
            fn fs(&mut self) -> crate::vfs::SharedFs {
                self.fs.clone()
            }
        }
        let fs = crate::vfs::new_fs();
        crate::vfs::mount_provider(&fs, "/peer", node.fs_serve.clone(), true);
        let mut store = wk_vfs::VfsImpl(ConsumerStore {
            table: ResourceTable::new(),
            fs,
        });
        let root = Preopens::get_directories(&mut store)
            .expect("preopen")
            .remove(0)
            .0;
        let root_fd = || wasmtime::component::Resource::new_own(root.rep());

        // Read a seeded file through the whole chain.
        let fd = HostDescriptor::open_at(
            &mut store,
            root_fd(),
            PathFlags::SYMLINK_FOLLOW,
            "peer/srv/motd.txt".into(),
            OpenFlags::empty(),
            DescriptorFlags::empty(),
        )
        .unwrap()
        .expect("opens the mirrored file");
        let (bytes, _) = HostDescriptor::read(
            &mut store,
            wasmtime::component::Resource::new_own(fd.rep()),
            64,
            0,
        )
        .unwrap()
        .expect("reads through passthrough");
        assert_eq!(bytes, b"from the node's own vfs");

        // Directory kinds survive the d_type mapping: `srv` lists as a dir.
        let dirfd = HostDescriptor::open_at(
            &mut store,
            root_fd(),
            PathFlags::SYMLINK_FOLLOW,
            "peer".into(),
            OpenFlags::DIRECTORY,
            DescriptorFlags::empty(),
        )
        .unwrap()
        .expect("opens the mount root");
        let stream = HostDescriptor::read_directory(&mut store, dirfd)
            .unwrap()
            .expect("lists");
        let mut entries = Vec::new();
        loop {
            use wk_vfs::wasi::filesystem::types::HostDirectoryEntryStream;
            match HostDirectoryEntryStream::read_directory_entry(
                &mut store,
                wasmtime::component::Resource::new_own(stream.rep()),
            )
            .unwrap()
            .unwrap()
            {
                Some(e) => entries.push((e.name, e.type_)),
                None => break,
            }
        }
        use wk_vfs::wasi::filesystem::types::DescriptorType;
        assert!(entries
            .iter()
            .any(|(n, t)| n == "srv" && *t == DescriptorType::Directory));

        // Files inside a listing must type as files: passthrough fills
        // st_mode as `d_type << 12`, and on wasi a regular file's d_type (4)
        // lands exactly on S_IFDIR — the shim must not take its word for it
        // (the "shared-notes.txt shows as an empty directory" regression).
        let srvfd = HostDescriptor::open_at(
            &mut store,
            root_fd(),
            PathFlags::SYMLINK_FOLLOW,
            "peer/srv".into(),
            OpenFlags::DIRECTORY,
            DescriptorFlags::empty(),
        )
        .unwrap()
        .expect("opens srv");
        let stream = HostDescriptor::read_directory(&mut store, srvfd)
            .unwrap()
            .expect("lists srv");
        let mut srv_entries = Vec::new();
        loop {
            use wk_vfs::wasi::filesystem::types::HostDirectoryEntryStream;
            match HostDirectoryEntryStream::read_directory_entry(
                &mut store,
                wasmtime::component::Resource::new_own(stream.rep()),
            )
            .unwrap()
            .unwrap()
            {
                Some(e) => srv_entries.push((e.name, e.type_)),
                None => break,
            }
        }
        assert!(srv_entries
            .iter()
            .any(|(n, t)| n == "motd.txt" && *t == DescriptorType::RegularFile));

        // Write back through the mount: create + write land in the provider
        // node's own vfs, visible from the host side.
        let wfd = HostDescriptor::open_at(
            &mut store,
            root_fd(),
            PathFlags::SYMLINK_FOLLOW,
            "peer/srv/note.txt".into(),
            OpenFlags::CREATE,
            DescriptorFlags::WRITE,
        )
        .unwrap()
        .expect("creates through passthrough");
        HostDescriptor::write(
            &mut store,
            wasmtime::component::Resource::new_own(wfd.rep()),
            b"written across the wire".to_vec(),
            0,
        )
        .unwrap()
        .expect("writes through passthrough");
        assert_eq!(
            node.fs
                .lock()
                .unwrap()
                .read_file("/srv/note.txt", 64)
                .as_deref(),
            Some(&b"written across the wire"[..]),
            "the write landed in the provider's own vfs"
        );

        // mkdir + unlink round-trip the same way.
        HostDescriptor::create_directory_at(&mut store, root_fd(), "peer/made".into())
            .unwrap()
            .expect("mkdir through passthrough");
        assert!(node.fs.lock().unwrap().list_dir("/made").is_some());
        HostDescriptor::unlink_file_at(&mut store, root_fd(), "peer/srv/note.txt".into())
            .unwrap()
            .expect("unlinks through passthrough");
        assert_eq!(
            node.fs.lock().unwrap().read_file("/srv/note.txt", 8),
            None,
            "the unlink landed in the provider's own vfs"
        );

        node.kill.store(true, Ordering::Relaxed);
    }

    /// zipfs (miniz + the libfuse shim) serves a zip archive's tree: the
    /// archive is wired into the node's own vfs AFTER it starts (the lazy
    /// index), and a consumer browses and reads the members through the
    /// mount. Read-only: the daemon has no write ops.
    #[test]
    fn zipfs_serves_an_archive_wired_in_later() {
        use std::io::Write;
        use wk_vfs::wasi::filesystem::preopens::Host as Preopens;
        use wk_vfs::wasi::filesystem::types::{
            DescriptorFlags, DescriptorType, ErrorCode, HostDescriptor, OpenFlags, PathFlags,
        };

        let wasm = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/zipfs/zipfs.wasm");
        if !wasm.exists() {
            eprintln!("skipping: build plugins/zipfs first (./build.sh)");
            return;
        }

        let host = PluginHost::new().expect("host");
        let nodes: NodeRegistry = Arc::new(Mutex::new(Vec::new()));
        let id = NodeId::new();
        host.spawn(
            &wasm,
            "zipfs",
            id,
            &[],
            Arc::new(Mutex::new(Vec::new())),
            nodes.clone(),
            Vec::new(),
            None,
        )
        .expect("spawn");
        let node = {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
            loop {
                if let Some(n) = nodes.lock().unwrap().iter().find(|n| n.id == id).cloned() {
                    if n.serves_fs() && n.fs_serve.is_serving() {
                        break n;
                    }
                    assert!(
                        !n.finished.load(Ordering::Relaxed),
                        "zipfs exited before serving"
                    );
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "zipfs never started serving"
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        };

        // A real archive, wired in AFTER the daemon is already serving —
        // exactly how a BindMount lands on a running node.
        let archive = {
            let mut buf = std::io::Cursor::new(Vec::new());
            let mut w = zip::ZipWriter::new(&mut buf);
            let opts = zip::write::SimpleFileOptions::default();
            w.start_file("top.txt", opts).unwrap();
            w.write_all(b"at the top").unwrap();
            w.start_file("docs/readme.md", opts).unwrap();
            w.write_all(b"# from inside a zip\n").unwrap();
            w.finish().unwrap();
            buf.into_inner()
        };
        node.fs.lock().unwrap().put_file_at("archive.zip", archive);

        struct ConsumerStore {
            table: ResourceTable,
            fs: crate::vfs::SharedFs,
        }
        impl wasmtime_wasi_io::IoView for ConsumerStore {
            fn table(&mut self) -> &mut ResourceTable {
                &mut self.table
            }
        }
        impl wk_vfs::VfsView for ConsumerStore {
            fn fs(&mut self) -> crate::vfs::SharedFs {
                self.fs.clone()
            }
        }
        let fs = crate::vfs::new_fs();
        crate::vfs::mount_provider(&fs, "/z", node.fs_serve.clone(), true);
        let mut store = wk_vfs::VfsImpl(ConsumerStore {
            table: ResourceTable::new(),
            fs,
        });
        let root = Preopens::get_directories(&mut store)
            .expect("preopen")
            .remove(0)
            .0;
        let root_fd = || wasmtime::component::Resource::new_own(root.rep());

        // A nested member reads back through miniz's decompression.
        let fd = HostDescriptor::open_at(
            &mut store,
            root_fd(),
            PathFlags::SYMLINK_FOLLOW,
            "z/docs/readme.md".into(),
            OpenFlags::empty(),
            DescriptorFlags::empty(),
        )
        .unwrap()
        .expect("opens a member");
        let (bytes, _) = HostDescriptor::read(
            &mut store,
            wasmtime::component::Resource::new_own(fd.rep()),
            64,
            0,
        )
        .unwrap()
        .expect("reads a member");
        assert_eq!(bytes, b"# from inside a zip\n");

        // The listing shows the file and the synthesized directory, typed.
        let dirfd = HostDescriptor::open_at(
            &mut store,
            root_fd(),
            PathFlags::SYMLINK_FOLLOW,
            "z".into(),
            OpenFlags::DIRECTORY,
            DescriptorFlags::empty(),
        )
        .unwrap()
        .expect("opens the archive root");
        let stream = HostDescriptor::read_directory(&mut store, dirfd)
            .unwrap()
            .expect("lists");
        let mut entries = Vec::new();
        loop {
            use wk_vfs::wasi::filesystem::types::HostDirectoryEntryStream;
            match HostDirectoryEntryStream::read_directory_entry(
                &mut store,
                wasmtime::component::Resource::new_own(stream.rep()),
            )
            .unwrap()
            .unwrap()
            {
                Some(e) => entries.push((e.name, e.type_)),
                None => break,
            }
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(entries.len(), 2);
        assert!(entries[0].0 == "docs" && entries[0].1 == DescriptorType::Directory);
        assert!(entries[1].0 == "top.txt" && entries[1].1 == DescriptorType::RegularFile);

        // Read-only: the daemon has no write callbacks.
        assert_eq!(
            HostDescriptor::write(
                &mut store,
                wasmtime::component::Resource::new_own(fd.rep()),
                b"x".to_vec(),
                0,
            )
            .unwrap()
            .unwrap_err(),
            ErrorCode::NotPermitted
        );

        node.kill.store(true, Ordering::Relaxed);
    }

    /// httpfs: a network-backed filesystem whose network is the fabric. A
    /// host-side HTTP file server listens as the named fabric peer
    /// "filesrv" on the node's net; httpfs dials it BY NAME over its BSD
    /// sockets, and a consumer browses/reads the server's files through the
    /// provider mount — every filesystem op is an HTTP exchange riding the
    /// userspace netstack. Offset reads exercise Range requests.
    #[test]
    fn httpfs_mounts_a_fabric_http_server() {
        use std::io::{Read, Write};
        use wk_vfs::wasi::filesystem::preopens::Host as Preopens;
        use wk_vfs::wasi::filesystem::types::{
            DescriptorFlags, HostDescriptor, OpenFlags, PathFlags,
        };

        let wasm = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/httpfs/httpfs.wasm");
        if !wasm.exists() {
            eprintln!("skipping: build plugins/httpfs first (./build.sh)");
            return;
        }

        let host = PluginHost::new().expect("host");
        let nodes: NodeRegistry = Arc::new(Mutex::new(Vec::new()));
        let id = NodeId::new();
        host.spawn(
            &wasm,
            "httpfs",
            id,
            &[],
            Arc::new(Mutex::new(Vec::new())),
            nodes.clone(),
            Vec::new(),
            None,
        )
        .expect("spawn");

        // Networked nodes don't auto-run: wait for the compile, then start
        // the server peer, then run the guest against it.
        let node = {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
            loop {
                if let Some(n) = nodes.lock().unwrap().iter().find(|n| n.id == id).cloned() {
                    if n.serves_fs() && n.net_stack().is_some() {
                        break n;
                    }
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "httpfs never finished compiling"
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        };

        // The file server: a named fabric peer on the node's own net. The
        // listing convention is httpfs's documented one — text/plain, one
        // entry per line, directories with a trailing slash.
        let big: Vec<u8> = (0..70_000u32).map(|i| (i % 251) as u8).collect();
        let big_clone = big.clone();
        let kill_srv = Arc::new(AtomicBool::new(false));
        wk_fabric::listen::listen(
            host.hub(),
            node.net_stack().unwrap(),
            "filesrv",
            8080,
            kill_srv.clone(),
            Arc::new(move |mut s: std::os::unix::net::UnixStream| {
                let big = big_clone.clone();
                std::thread::spawn(move || {
                    let mut req = Vec::new();
                    let mut byte = [0u8; 1];
                    while !req.ends_with(b"\r\n\r\n") {
                        match s.read(&mut byte) {
                            Ok(1) => req.push(byte[0]),
                            _ => return,
                        }
                    }
                    let req = String::from_utf8_lossy(&req).to_string();
                    let mut lines = req.split("\r\n");
                    let mut first = lines.next().unwrap_or("").split(' ');
                    let method = first.next().unwrap_or("");
                    let path = first.next().unwrap_or("");
                    let range = req
                        .split("\r\n")
                        .find_map(|l| l.strip_prefix("Range: bytes="))
                        .and_then(|r| {
                            let (a, b) = r.split_once('-')?;
                            Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?))
                        });
                    let file: Option<&[u8]> = match path {
                        "/data/hello.txt" => Some(b"hello over the fabric"),
                        "/data/big.bin" => Some(&big),
                        _ => None,
                    };
                    let listing: Option<&str> = match path {
                        "/" => Some("data/\n"),
                        "/data/" => Some("hello.txt\nbig.bin\n"),
                        _ => None,
                    };
                    let reply = if let Some(bytes) = file {
                        if method == "HEAD" {
                            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", bytes.len())
                                .into_bytes()
                        } else if let Some((a, b)) = range {
                            let end = (b + 1).min(bytes.len());
                            let slice = &bytes[a.min(end)..end];
                            let mut r = format!(
                                "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\n\r\n",
                                slice.len()
                            )
                            .into_bytes();
                            r.extend_from_slice(slice);
                            r
                        } else {
                            let mut r = format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                                bytes.len()
                            )
                            .into_bytes();
                            r.extend_from_slice(bytes);
                            r
                        }
                    } else if let Some(text) = listing {
                        let mut r =
                            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", text.len())
                                .into_bytes();
                        if method != "HEAD" {
                            r.extend_from_slice(text.as_bytes());
                        }
                        r
                    } else {
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec()
                    };
                    let _ = s.write_all(&reply);
                });
            }),
        );

        host.run_node(&node, &["http://filesrv:8080".to_string()])
            .expect("run httpfs");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !node.fs_serve.is_serving() {
            assert!(
                std::time::Instant::now() < deadline,
                "httpfs never started serving"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        struct ConsumerStore {
            table: ResourceTable,
            fs: crate::vfs::SharedFs,
        }
        impl wasmtime_wasi_io::IoView for ConsumerStore {
            fn table(&mut self) -> &mut ResourceTable {
                &mut self.table
            }
        }
        impl wk_vfs::VfsView for ConsumerStore {
            fn fs(&mut self) -> crate::vfs::SharedFs {
                self.fs.clone()
            }
        }
        let fs = crate::vfs::new_fs();
        crate::vfs::mount_provider(&fs, "/web", node.fs_serve.clone(), true);
        let mut store = wk_vfs::VfsImpl(ConsumerStore {
            table: ResourceTable::new(),
            fs,
        });
        let root = Preopens::get_directories(&mut store)
            .expect("preopen")
            .remove(0)
            .0;
        let root_fd = || wasmtime::component::Resource::new_own(root.rep());

        // Read a file: mount → shim → HTTP GET over the fabric → server.
        let fd = HostDescriptor::open_at(
            &mut store,
            root_fd(),
            PathFlags::SYMLINK_FOLLOW,
            "web/data/hello.txt".into(),
            OpenFlags::empty(),
            DescriptorFlags::empty(),
        )
        .unwrap()
        .expect("opens over http");
        let (bytes, _) = HostDescriptor::read(
            &mut store,
            wasmtime::component::Resource::new_own(fd.rep()),
            64,
            0,
        )
        .unwrap()
        .expect("reads over http");
        assert_eq!(bytes, b"hello over the fabric");

        // An offset read of the big file exercises a Range request.
        let bfd = HostDescriptor::open_at(
            &mut store,
            root_fd(),
            PathFlags::SYMLINK_FOLLOW,
            "web/data/big.bin".into(),
            OpenFlags::empty(),
            DescriptorFlags::empty(),
        )
        .unwrap()
        .expect("opens big.bin");
        let (bytes, _) = HostDescriptor::read(
            &mut store,
            wasmtime::component::Resource::new_own(bfd.rep()),
            100,
            40_000,
        )
        .unwrap()
        .expect("range-reads big.bin");
        assert_eq!(bytes, big[40_000..40_100]);

        // The autoindex becomes a directory listing.
        let dirfd = HostDescriptor::open_at(
            &mut store,
            root_fd(),
            PathFlags::SYMLINK_FOLLOW,
            "web/data".into(),
            OpenFlags::DIRECTORY,
            DescriptorFlags::empty(),
        )
        .unwrap()
        .expect("opens the http dir");
        let stream = HostDescriptor::read_directory(&mut store, dirfd)
            .unwrap()
            .expect("lists over http");
        let mut names = Vec::new();
        loop {
            use wk_vfs::wasi::filesystem::types::HostDirectoryEntryStream;
            match HostDirectoryEntryStream::read_directory_entry(
                &mut store,
                wasmtime::component::Resource::new_own(stream.rep()),
            )
            .unwrap()
            .unwrap()
            {
                Some(e) => names.push(e.name),
                None => break,
            }
        }
        names.sort();
        assert_eq!(names, ["big.bin", "hello.txt"]);

        node.kill.store(true, Ordering::Relaxed);
        kill_srv.store(true, Ordering::Relaxed);
    }

    /// GNU bash with readline, live: tab completion happens guest-side (the
    /// UI just forwards \t), so feeding "ech<TAB>" must make readline echo
    /// the completed builtin back. Drives the real bash.wasm through a node's
    /// terminal ring. Skipped when the artifact isn't built (plugins/bash).
    #[test]
    fn bash_readline_completes_on_tab() {
        let wasm = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/bash/bash.wasm");
        if !wasm.exists() {
            eprintln!("skipping: build plugins/bash first (./build.sh)");
            return;
        }

        let host = PluginHost::new().expect("host");
        let nodes: NodeRegistry = Arc::new(Mutex::new(Vec::new()));
        let id = NodeId::new();
        host.spawn(
            &wasm,
            "bash",
            id,
            &[],
            Arc::new(Mutex::new(Vec::new())),
            nodes.clone(),
            Vec::new(),
            None,
        )
        .expect("spawn");
        // bash imports wasi:sockets (/dev/tcp), so it's a networked node and
        // waits to be Run rather than auto-starting. First-ever compile of a
        // 2 MB shell can take minutes; cached runs break out in milliseconds.
        let node = {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
            loop {
                if let Some(n) = nodes.lock().unwrap().iter().find(|n| n.id == id).cloned() {
                    if n.is_runnable() {
                        break n;
                    }
                }
                assert!(std::time::Instant::now() < deadline, "bash never compiled");
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        };
        // What the image's COPY etc/ /etc/ provides: GNU termcap's database,
        // without which readline's clear-screen degrades to a newline.
        let termcap = std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/bash/etc/termcap"),
        )
        .expect("plugins/bash/etc/termcap");
        node.fs.lock().unwrap().put_file_at("etc/termcap", termcap);
        host.run_node(&node, &[]).expect("run bash");
        {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            while !node.running.load(Ordering::Relaxed) {
                assert!(std::time::Instant::now() < deadline, "bash never started");
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }

        // Wait for readline's prompt, then complete a builtin.
        let wait_for = |needle: &str, from: u64| -> u64 {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            loop {
                let (bytes, upto) = node.term_io.log_read(from);
                if String::from_utf8_lossy(&bytes).contains(needle) {
                    return upto;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "never saw {needle:?} in bash output"
                );
                std::thread::sleep(std::time::Duration::from_millis(30));
            }
        };
        let after_prompt = wait_for("bash-5.2", 0);

        // "ech" + TAB: readline completes the builtin and echoes "echo".
        node.term_io.feed_in(b"ech\t");
        let after_complete = wait_for("echo", after_prompt);

        // Finish the line: the completed command actually runs, and its
        // output carries the pty-style CRLF — the write-time ONLCR that the
        // shim's OPOST transport enables (without it, a readline session
        // holds the terminal raw at the prompt and cooked output renders
        // under raw rules: the staircase-alignment regression).
        node.term_io.feed_in(b"readline-works\n");
        let upto = wait_for("readline-works", after_complete);
        let (bytes, _) = node.term_io.log_read(0);
        let _ = upto;
        assert!(
            String::from_utf8_lossy(&bytes).contains("readline-works\r\n"),
            "command output is CRLF-translated at write time"
        );

        // Ctrl+L: readline's clear-screen, which needs the termcap `cl`
        // capability — with /etc/termcap in place it emits a real clear
        // sequence instead of degrading to a newline.
        node.term_io.feed_in(b"\x0c");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let (bytes, _) = node.term_io.log_read(0);
            if bytes.windows(4).any(|w| w == b"\x1b[2J") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "Ctrl+L never emitted a clear-screen sequence"
            );
            std::thread::sleep(std::time::Duration::from_millis(30));
        }

        node.kill.store(true, Ordering::Relaxed);
    }

    /// Seed a netsurf node's filesystem with the browser's runtime resources
    /// — what the container image's `COPY res /usr/share/netsurf` provides —
    /// and spawn it. NetSurf imports wasi:sockets (its curl fetcher), so it is
    /// a networked node: it waits to be Run, which the caller does once any
    /// server peer is listening. Returns the registered node.
    fn spawn_netsurf(
        host: &PluginHost,
        id: NodeId,
        surfaces: &SurfaceRegistry,
        nodes: &NodeRegistry,
    ) -> Option<SharedNode> {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/netsurf");
        let wasm = dir.join("netsurf.wasm");
        let res = dir.join("res");
        if !wasm.exists() || !res.exists() {
            eprintln!("skipping: build plugins/netsurf first (./build.sh)");
            return None;
        }

        host.spawn(
            &wasm,
            "netsurf",
            id,
            &[],
            surfaces.clone(),
            nodes.clone(),
            Vec::new(),
            None,
        )
        .expect("spawn");

        // The node registers synchronously; seed the resources before the
        // (much slower) background compile can let the guest run.
        let node = nodes
            .lock()
            .unwrap()
            .iter()
            .find(|n| n.id == id)
            .cloned()
            .expect("node registered");
        for entry in std::fs::read_dir(&res).expect("read res/") {
            let entry = entry.expect("res entry");
            let name = entry.file_name().into_string().expect("utf8 name");
            node.fs.lock().unwrap().put_file_at(
                &format!("usr/share/netsurf/{name}"),
                std::fs::read(entry.path()).expect("read resource"),
            );
        }

        // First-ever wasmtime compile of a 6.5 MB browser takes minutes;
        // cached runs break out in milliseconds.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(600);
        loop {
            if node.is_runnable() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "netsurf never compiled"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        Some(node)
    }

    /// Pump headless compositor frames (the server's per-frame role) until
    /// `done` is satisfied by the surface's presented pixels.
    fn pump_until(surface: &SharedSurface, what: &str, done: impl Fn(&VirtualSurface) -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        loop {
            {
                let mut s = surface.lock().unwrap();
                s.frame_ready = true;
                s.wake();
            }
            std::thread::sleep(std::time::Duration::from_millis(15));
            if done(&surface.lock().unwrap()) {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "netsurf never {what}");
        }
    }

    /// The real thing, part one: NetSurf 3.11 (framebuffer frontend, libnsfb's
    /// new wk surface) boots as a wk node and paints its built-in
    /// `about:welcome` page — resources from the node's own filesystem, zero
    /// network. The test pumps frames headless exactly like the gfx-smoke and
    /// doom tests and asserts the surface stops being a uniform colour.
    #[test]
    fn netsurf_paints_its_welcome_page() {
        let host = PluginHost::new().expect("host");
        let nodes: NodeRegistry = Arc::new(Mutex::new(Vec::new()));
        let surfaces: SurfaceRegistry = Arc::new(Mutex::new(Vec::new()));
        let id = NodeId::new();
        let Some(node) = spawn_netsurf(&host, id, &surfaces, &nodes) else {
            return;
        };

        // No URL argument: the browser opens its NETSURF_HOMEPAGE
        // (about:welcome → resource:welcome.html from the seeded resources).
        host.run_node(&node, &[]).expect("run netsurf");

        // The surface appears once the guest's nsfb wk backend calls
        // wkgfx_open.
        let surface = {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
            loop {
                if let Some(s) = surfaces.lock().unwrap().first().cloned() {
                    break s;
                }
                assert!(
                    !node.finished.load(Ordering::Relaxed),
                    "netsurf exited before opening a surface"
                );
                assert!(
                    std::time::Instant::now() < deadline,
                    "netsurf never opened a surface"
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        };

        // The welcome page (toolbar, search box, logo) is anything but a
        // uniform field of pixels.
        pump_until(&surface, "painted a non-uniform frame", |s| {
            !s.pixels.is_empty() && s.pixels.chunks_exact(4).any(|px| px != &s.pixels[0..4])
        });

        node.kill.store(true, Ordering::Relaxed);
    }

    /// The real thing, part two: NetSurf fetches a page over the fabric. An
    /// HTTP server listens as the named fabric peer "websrv" on the node's own
    /// net (the httpfs test's pattern) serving a page with a solid bright-red
    /// body; netsurf is launched pointed at `http://websrv:8080/`, resolves
    /// the name over the fabric, drives its curl fetcher through the
    /// userspace netstack, renders the page — and the test asserts a run of
    /// pure-red pixels appears on the surface.
    #[test]
    fn netsurf_fetches_over_the_fabric() {
        use std::io::{Read, Write};

        let host = PluginHost::new().expect("host");
        let nodes: NodeRegistry = Arc::new(Mutex::new(Vec::new()));
        let surfaces: SurfaceRegistry = Arc::new(Mutex::new(Vec::new()));
        let id = NodeId::new();
        let Some(node) = spawn_netsurf(&host, id, &surfaces, &nodes) else {
            return;
        };

        // The page: a solid #ff0000 background nothing in netsurf's own
        // chrome uses, so its arrival on screen proves the fetch+render.
        let page = "<html><head><title>fabric</title></head>\
                    <body style=\"background-color: #ff0000\"></body></html>";
        let kill_srv = Arc::new(AtomicBool::new(false));
        wk_fabric::listen::listen(
            host.hub(),
            node.net_stack().expect("netsurf has a fabric stack"),
            "websrv",
            8080,
            kill_srv.clone(),
            Arc::new(move |mut s: std::os::unix::net::UnixStream| {
                let page = page.to_string();
                std::thread::spawn(move || {
                    let mut req = Vec::new();
                    let mut byte = [0u8; 1];
                    while !req.ends_with(b"\r\n\r\n") {
                        match s.read(&mut byte) {
                            Ok(1) => req.push(byte[0]),
                            _ => return,
                        }
                    }
                    let path = String::from_utf8_lossy(&req)
                        .split(' ')
                        .nth(1)
                        .unwrap_or("")
                        .to_string();
                    let reply = if path == "/" {
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\
                             Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                            page.len(),
                            page
                        )
                    } else {
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\
                         Connection: close\r\n\r\n"
                            .to_string()
                    };
                    let _ = s.write_all(reply.as_bytes());
                });
            }),
        );

        host.run_node(&node, &["http://websrv:8080/".to_string()])
            .expect("run netsurf");

        let surface = {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
            loop {
                if let Some(s) = surfaces.lock().unwrap().first().cloned() {
                    break s;
                }
                assert!(
                    !node.finished.load(Ordering::Relaxed),
                    "netsurf exited before opening a surface"
                );
                assert!(
                    std::time::Instant::now() < deadline,
                    "netsurf never opened a surface"
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        };

        // A whole row's worth of pure red only exists once the fetched page's
        // background has been rendered (the throbber/toolbar have no such
        // run).
        pump_until(&surface, "rendered the fetched red page", |s| {
            let red = s
                .pixels
                .chunks_exact(4)
                .filter(|px| px[0] == 0xff && px[1] == 0 && px[2] == 0)
                .count();
            red >= s.width as usize
        });

        node.kill.store(true, Ordering::Relaxed);
        kill_srv.store(true, Ordering::Relaxed);
    }

    /// The real thing, part three — and the regression test for the
    /// accept-family trap: `example/browser.wk`'s topology with a REAL guest
    /// server. A CPython node runs the example's `http.server`
    /// (`browser-www/server.py`, a v4 wildcard bind) on a shared network;
    /// NetSurf is launched at `http://python:8000/`. Fabric DNS resolves the
    /// name to both fabric addresses and curl may dial the v6 ULA — which
    /// must be REFUSED (family-scoped listeners), never delivered to the v4
    /// listener: wasi-libc's accept() aborts converting a v6 peer for a v4
    /// socket, trapping the interpreter mid-serve. The test asserts the
    /// python-served red page renders AND python outlives the visit.
    #[test]
    fn python_http_server_survives_netsurf_over_the_fabric() {
        let pydir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/python");
        let python_wasm = pydir.join("python.wasm");
        let stdlib = pydir.join("lib/python3.14");
        if !python_wasm.exists() || !stdlib.exists() {
            eprintln!("skipping: build plugins/python first (mise run build)");
            return;
        }

        let host = PluginHost::new().expect("host");
        let nodes: NodeRegistry = Arc::new(Mutex::new(Vec::new()));
        let surfaces: SurfaceRegistry = Arc::new(Mutex::new(Vec::new()));

        let ns_id = NodeId::new();
        let Some(netsurf) = spawn_netsurf(&host, ns_id, &surfaces, &nodes) else {
            return;
        };

        // The python node: python.wasm with its Dockerfile's environment,
        // the stdlib seeded into the node's filesystem, and /app holding the
        // example's real webserver plus a solid-red index page (the same
        // pixel proof as the fabric fetch test).
        let py_id = NodeId::new();
        host.spawn(
            &python_wasm,
            "python",
            py_id,
            &[],
            surfaces.clone(),
            nodes.clone(),
            Vec::new(),
            Some(crate::images::ContainerSetup {
                layers: Vec::new(),
                env: vec![
                    ("PYTHONHOME".into(), "/usr/local".into()),
                    ("PYTHONDONTWRITEBYTECODE".into(), "1".into()),
                    ("HOME".into(), "/root".into()),
                ],
            }),
        )
        .expect("spawn python");
        let python = nodes
            .lock()
            .unwrap()
            .iter()
            .find(|n| n.id == py_id)
            .cloned()
            .expect("python registered");
        {
            let mut fsg = python.fs.lock().unwrap();
            let libroot = pydir.join("lib");
            let mut dirs = vec![stdlib.clone()];
            while let Some(dir) = dirs.pop() {
                for entry in std::fs::read_dir(&dir).expect("read stdlib dir") {
                    let path = entry.expect("stdlib entry").path();
                    if path.is_dir() {
                        dirs.push(path);
                        continue;
                    }
                    let rel = path.strip_prefix(&libroot).expect("under lib/");
                    fsg.put_file_at(
                        &format!("usr/local/lib/{}", rel.display()),
                        std::fs::read(&path).expect("read stdlib file"),
                    );
                }
            }
            let server_py =
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../../example/browser-www/server.py");
            fsg.put_file_at(
                "app/server.py",
                std::fs::read(&server_py).expect("read example server.py"),
            );
            fsg.put_file_at(
                "app/index.html",
                b"<html><head><title>fabric</title></head>\
                  <body style=\"background-color: #ff0000\"></body></html>"
                    .to_vec(),
            );
        }
        {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(600);
            while !python.is_runnable() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "python never compiled"
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }

        // One shared Network, like browser.wk's netlinks.
        let shared_net = NodeId::new();
        for n in [&netsurf, &python] {
            n.net_stack().expect("fabric stack").lock().unwrap().net = shared_net;
        }

        // Start python first (the example's instruction), and wait until its
        // http.server actually listens before pointing the browser at it.
        host.run_node(&python, &["/app/server.py".to_string(), "8000".to_string()])
            .expect("run python");
        {
            let stack = python.net_stack().unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
            loop {
                let listening = {
                    let g = stack.lock().unwrap();
                    let any = g.sockets.iter().any(|(_, s)| {
                        matches!(
                            s,
                            smoltcp::socket::Socket::Tcp(t)
                                if t.state() == smoltcp::socket::tcp::State::Listen
                        )
                    });
                    any
                };
                if listening {
                    break;
                }
                assert!(
                    !python.finished.load(Ordering::Relaxed),
                    "python exited before listening"
                );
                assert!(
                    std::time::Instant::now() < deadline,
                    "python never started listening"
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }

        host.run_node(&netsurf, &["http://python:8000/".to_string()])
            .expect("run netsurf");
        let surface = {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
            loop {
                if let Some(s) = surfaces.lock().unwrap().first().cloned() {
                    break s;
                }
                assert!(
                    !netsurf.finished.load(Ordering::Relaxed),
                    "netsurf exited before opening a surface"
                );
                assert!(
                    std::time::Instant::now() < deadline,
                    "netsurf never opened a surface"
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        };

        // The page arrives only if python's accept loop survives the visit:
        // a trap mid-serve is caught immediately (finished flips), not as a
        // 120s render timeout.
        pump_until(&surface, "rendered python's red page", |s| {
            assert!(
                !python.finished.load(Ordering::Relaxed),
                "python trapped while netsurf connected — accept must never \
                 surface a peer of another family than the socket's own"
            );
            let red = s
                .pixels
                .chunks_exact(4)
                .filter(|px| px[0] == 0xff && px[1] == 0 && px[2] == 0)
                .count();
            red >= s.width as usize
        });
        assert!(
            !python.finished.load(Ordering::Relaxed),
            "python survived serving the browser"
        );

        netsurf.kill.store(true, Ordering::Relaxed);
        python.kill.store(true, Ordering::Relaxed);
    }

    /// Outbound wasi:http is denied unless the node's fabric stack has host
    /// access (i.e. it's wired to a Gateway) — the same gate as raw sockets.
    /// A stackless store (pure-http node / serve store) is always denied.
    #[test]
    fn outbound_http_gated_by_host_access() {
        // No stack → denied (a served http node can't proxy to the host).
        assert!(!GatedHttpHooks { stack: None }.host_allowed());

        let hub = wk_fabric::netstack::NetHub::new();
        let stack = hub.attach(
            NodeId::nil(),
            smoltcp::wire::Ipv4Address::new(10, 0, 0, 2),
            "n",
        );
        let hooks = GatedHttpHooks {
            stack: Some(stack.clone()),
        };
        // On its own isolated net (no Gateway) → denied.
        assert!(!hooks.host_allowed());
        // Wiring to a Gateway sets host_access → allowed.
        stack.lock().unwrap().host_access = true;
        assert!(hooks.host_allowed());
    }

    /// wk's MIDI story end to end: the fluidsynth node (the real FluidLite
    /// engine — FluidSynth's synthesis core — built by plugins/fluidsynth)
    /// loads a SoundFont from its vfs, and MIDI injected through the router
    /// (standing in for a piano or hardware MidiIn node wired on the canvas)
    /// reaches it through `wk:midi` and the midi-compat shim.
    ///
    /// Boots with `--dry-run`: the synth consumes MIDI and renders blocks but
    /// never calls wkaudio_open, because tests must not open real audio
    /// devices (the same reason the doom test passes -nosound). The node's
    /// terminal log is the observable: it prints the soundfont load and each
    /// note event. Skipped when the artifacts (fluidsynth.wasm +
    /// soundfont.sf2, both produced by ./build.sh) are missing.
    #[test]
    fn fluidsynth_sounds_injected_midi_from_a_soundfont() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/fluidsynth");
        let wasm = dir.join("fluidsynth.wasm");
        let sf2 = dir.join("soundfont.sf2");
        if !wasm.exists() || !sf2.exists() {
            eprintln!("skipping: build plugins/fluidsynth first (./build.sh)");
            return;
        }

        let host = PluginHost::new().expect("host");
        let nodes: NodeRegistry = Arc::new(Mutex::new(Vec::new()));
        let id = NodeId::new();
        host.spawn(
            &wasm,
            "fluidsynth",
            id,
            &["--dry-run".to_string(), "/soundfont.sf2".to_string()],
            Arc::new(Mutex::new(Vec::new())),
            nodes.clone(),
            Vec::new(),
            None,
        )
        .expect("spawn");

        // The node registers synchronously; seed the SoundFont into its
        // filesystem before the (much slower) background compile lets the
        // guest run — standing in for the image's COPY soundfont.sf2.
        let node = nodes
            .lock()
            .unwrap()
            .iter()
            .find(|n| n.id == id)
            .cloned()
            .expect("node registered");
        node.fs
            .lock()
            .unwrap()
            .put_file_at("soundfont.sf2", std::fs::read(&sf2).expect("read sf2"));

        // Wire a phantom keyboard onto the node exactly the way the server
        // wires a canvas "midi" connection: router entry into its inbox.
        // Messages queue there even before the guest opens its input port,
        // so injecting early can't race the boot.
        let kbd = NodeId::nil();
        host.midi()
            .lock()
            .unwrap()
            .connect(kbd, id, node.midi_in.clone());

        // The node's terminal (its stdout) is the observable.
        let log = || String::from_utf8_lossy(&node.term_io.log_read(0).0).into_owned();
        let wait_for = |what: &str, secs: u64| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
            loop {
                if log().contains(what) {
                    break;
                }
                assert!(
                    !node.finished.load(Ordering::Relaxed),
                    "fluidsynth exited before logging {what:?}; log:\n{}",
                    log()
                );
                assert!(
                    std::time::Instant::now() < deadline,
                    "fluidsynth never logged {what:?}; log:\n{}",
                    log()
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        };

        // Engine compile, then the guest parses the ~6 MB SF2 from its vfs.
        wait_for("soundfont loaded: /soundfont.sf2", 300);

        // Note-on and note-off round-trip: router -> inbox -> wk:midi receive
        // -> midi-compat shim -> fluid_synth_noteon/noteoff, each logged.
        let midi = host.midi();
        midi.lock()
            .unwrap()
            .send_from(kbd, &crate::midi::Event::now(vec![0x90, 60, 100]));
        wait_for("note-on ch=0 key=60 vel=100", 60);
        midi.lock()
            .unwrap()
            .send_from(kbd, &crate::midi::Event::now(vec![0x80, 60, 0]));
        wait_for("note-off ch=0 key=60", 60);

        // Still rendering — no trap on the way.
        assert!(
            !node.finished.load(Ordering::Relaxed),
            "fluidsynth trapped after the notes; log:\n{}",
            log()
        );
        node.kill.store(true, Ordering::Relaxed);
    }

    /// The sequencer's work leaves: it opens a MIDI file wired to the node,
    /// plays what is in it, and writes it back.
    ///
    /// A pattern that only exists inside the node is a sketch. This is the
    /// whole path — a real `.mid` in the node's filesystem, parsed into a song,
    /// scheduled onto the shared clock, and exported again — with a file
    /// written by something other than the sequencer going in at the front.
    #[test]
    fn sequencer_opens_plays_and_saves_a_midi_file() {
        let wasm = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../plugins/sequencer/target/wasm32-wasip1/debug/sequencer.wasm");
        if !wasm.exists() {
            eprintln!("skipping: build plugins/sequencer first (mise run build)");
            return;
        }

        // A file as another program would write it: 480 ticks per quarter, a
        // tempo the sequencer's defaults do not have, and two parts on their own
        // channels. Nothing here matches what the node starts with, so anything
        // it plays can only have come from the file.
        use wk_midifile::{Event, EventKind, MidiFile};
        let mut file = MidiFile::new(480);
        file.tracks.push(vec![
            Event::new(0, EventKind::tempo(400_000)), // 150 BPM
            Event::new(0, EventKind::end_of_track()),
        ]);
        file.tracks.push(vec![
            // A half note on channel 3, at a velocity worth recognising. Half a
            // bar rather than a whole one, so it releases and retriggers and
            // the loop can be timed.
            Event::new(0, EventKind::Midi(vec![0x92, 55, 97])),
            Event::new(960, EventKind::Midi(vec![0x82, 55, 0])),
            Event::new(0, EventKind::end_of_track()),
        ]);
        let bytes = file.write();

        let host = PluginHost::new().expect("host");
        let nodes: NodeRegistry = Arc::new(Mutex::new(Vec::new()));
        let surfaces: SurfaceRegistry = Arc::new(Mutex::new(Vec::new()));
        let id = NodeId::new();
        host.spawn(
            &wasm,
            "sequencer",
            id,
            &[],
            surfaces.clone(),
            nodes.clone(),
            Vec::new(),
            None,
        )
        .expect("spawn");

        // The node registers synchronously, so the file lands in its filesystem
        // before the guest runs and goes looking for one.
        let node = nodes
            .lock()
            .unwrap()
            .iter()
            .find(|n| n.id == id)
            .cloned()
            .expect("node registered");
        node.fs.lock().unwrap().put_file_at("riff.mid", bytes);

        let inbox = crate::midi::new_inbox();
        host.midi()
            .lock()
            .unwrap()
            .connect(id, NodeId::new(), inbox.clone());

        let surface = {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
            loop {
                if let Some(s) = surfaces.lock().unwrap().first().cloned() {
                    break s;
                }
                assert!(
                    !node.finished.load(Ordering::Relaxed),
                    "the sequencer exited before opening a surface"
                );
                assert!(
                    std::time::Instant::now() < deadline,
                    "the sequencer never opened a surface"
                );
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        };
        let pump_frame = || {
            let mut s = surface.lock().unwrap();
            s.frame_ready = true;
            s.wake();
        };
        for _ in 0..5 {
            pump_frame();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        let key = |code: Key, meta: bool| KeyEvent {
            key: Some(code),
            text: None,
            alt_key: false,
            ctrl_key: false,
            meta_key: meta,
            shift_key: false,
            repeat: false,
        };
        {
            let mut s = surface.lock().unwrap();
            s.key_down.push_back(key(Key::Space, false));
            s.key_up.push_back(key(Key::Space, false));
            s.wake();
        }

        // What it plays is the file's note, on the file's channel, at the
        // file's velocity — and its tempo, which the loop period proves.
        let mut events: Vec<crate::midi::Event> = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            pump_frame();
            std::thread::sleep(std::time::Duration::from_millis(10));
            events.extend(inbox.lock().unwrap().drain());
            let ons = events
                .iter()
                .filter(|e| e.data.first() == Some(&0x92))
                .count();
            if ons >= 2 {
                break;
            }
            assert!(
                !node.finished.load(Ordering::Relaxed),
                "the sequencer exited while playing the file"
            );
            assert!(
                std::time::Instant::now() < deadline,
                "the file's note never played; {} events seen; log:\n{}",
                events.len(),
                String::from_utf8_lossy(&node.term_io.log_read(0).0)
            );
        }
        let ons: Vec<&crate::midi::Event> = events
            .iter()
            .filter(|e| e.data.first() == Some(&0x92))
            .collect();
        assert_eq!(
            ons[0].data,
            vec![0x92, 55, 97],
            "the note, channel and velocity all come from the file"
        );
        // The pattern is sixteen sixteenth-note steps; at 150 BPM that is 1.6s.
        let cycle = ons[1].time as f64 - ons[0].time as f64;
        assert!(
            (cycle - 1_600_000.0).abs() < 2.0,
            "the file's tempo and length should loop every 1.6s, got {cycle}us"
        );

        // Cmd+S writes it back, and what comes out is still a MIDI file with
        // that note in it.
        {
            let mut s = surface.lock().unwrap();
            s.key_down.push_back(key(Key::KeyS, true));
            s.key_up.push_back(key(Key::KeyS, true));
            s.wake();
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let saved = loop {
            pump_frame();
            std::thread::sleep(std::time::Duration::from_millis(20));
            let written = node
                .fs
                .lock()
                .unwrap()
                .read_file("riff.mid", 1 << 20)
                .unwrap_or_default();
            // The node rewrites the file, so wait for bytes it authored: its
            // own files are 96 ticks per quarter, not the 480 that went in.
            if let Ok(parsed) = MidiFile::parse(&written) {
                if parsed.ppq == 96 {
                    break parsed;
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "Cmd+S never wrote the file back"
            );
        };
        let played: Vec<&Vec<u8>> = saved
            .tracks
            .iter()
            .flatten()
            .filter_map(|e| match &e.kind {
                EventKind::Midi(m) if m[0] == 0x92 => Some(m),
                _ => None,
            })
            .collect();
        assert_eq!(
            played,
            vec![&vec![0x92, 55, 97]],
            "the saved file still holds the note, on its channel, at its velocity"
        );
        assert!(
            (60_000_000.0 / saved.tempo() as f64 - 150.0).abs() < 0.5,
            "and its tempo"
        );

        // With a file wired, the node's own options stay empty: the file is the
        // document. Writing the song here as well would put a copy of the music
        // into every `.wk` that uses the node, and leave two sources of truth
        // free to drift apart.
        assert!(
            node.options.lock().unwrap().is_empty(),
            "the song is in the file, not duplicated into the workspace"
        );

        node.kill.store(true, Ordering::Relaxed);
    }

    /// The sequencer keeps the clock's time, not the frame rate's.
    ///
    /// This is the property the node was rebuilt for. It used to count
    /// compositor frames and assume sixty a second, so at 120 BPM a sixteenth
    /// note came out as seven or eight whole frames — a tempo that was not the
    /// tempo, jittering by up to a frame either way, and moving with the
    /// display. Now every event is stamped with the instant it belongs to, so
    /// the test can read the *stamps* and check the arithmetic exactly, with no
    /// dependence on how fast this machine happens to pump frames.
    #[test]
    fn sequencer_stamps_its_notes_on_the_clock_not_the_frame_rate() {
        let wasm = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../plugins/sequencer/target/wasm32-wasip1/debug/sequencer.wasm");
        if !wasm.exists() {
            eprintln!("skipping: build plugins/sequencer first (mise run build)");
            return;
        }

        // A saved pattern, in the node's own persisted layout: tag, version,
        // 150 BPM, 8 steps long, then one note — C4 at step 0, one step, at
        // velocity 96. Deliberately not the defaults, so the numbers below can
        // only come from the saved tempo and length.
        const BPM: f64 = 150.0;
        const STEPS: i64 = 8;
        let options = vec![-1.0, 1.0, BPM as f32, STEPS as f32, 0.0, 60.0, 1.0, 96.0];
        // Microseconds per sixteenth-note step, and per MIDI clock pulse (24
        // per quarter note, so six per sixteenth).
        let step_us = 60_000_000.0 / BPM / 4.0;
        let pulse_us = step_us / 6.0;

        let host = PluginHost::new().expect("host");
        let nodes: NodeRegistry = Arc::new(Mutex::new(Vec::new()));
        let surfaces: SurfaceRegistry = Arc::new(Mutex::new(Vec::new()));
        let id = NodeId::new();
        host.spawn(
            &wasm,
            "sequencer",
            id,
            &[],
            surfaces.clone(),
            nodes.clone(),
            options,
            None,
        )
        .expect("spawn");

        let node = nodes
            .lock()
            .unwrap()
            .iter()
            .find(|n| n.id == id)
            .cloned()
            .expect("node registered");

        // Wire the sequencer's output into an inbox we can read, exactly the
        // way the server wires a canvas "midi" connection to a synth.
        let inbox = crate::midi::new_inbox();
        host.midi()
            .lock()
            .unwrap()
            .connect(id, NodeId::new(), inbox.clone());

        // Wait for the surface, then drive frames by hand.
        let surface = {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
            loop {
                if let Some(s) = surfaces.lock().unwrap().first().cloned() {
                    break s;
                }
                assert!(
                    !node.finished.load(Ordering::Relaxed),
                    "the sequencer exited before opening a surface"
                );
                assert!(
                    std::time::Instant::now() < deadline,
                    "the sequencer never opened a surface"
                );
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        };
        let pump_frame = || {
            let mut s = surface.lock().unwrap();
            s.frame_ready = true;
            s.wake();
        };
        // A few frames so the guest reaches its loop before the keystroke.
        for _ in 0..5 {
            pump_frame();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        // Space starts the transport, the same key a player would press.
        {
            let mut s = surface.lock().unwrap();
            let space = KeyEvent {
                key: Some(Key::Space),
                text: Some(" ".into()),
                alt_key: false,
                ctrl_key: false,
                meta_key: false,
                shift_key: false,
                repeat: false,
            };
            s.key_down.push_back(space.clone());
            s.key_up.push_back(space);
            s.wake();
        }

        // Collect until two full cycles of the pattern have been queued. The
        // sequencer runs ahead of the clock, so this takes a bit less than two
        // pattern lengths of real time.
        let mut events: Vec<crate::midi::Event> = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            pump_frame();
            std::thread::sleep(std::time::Duration::from_millis(10));
            events.extend(inbox.lock().unwrap().drain());
            let note_ons = events
                .iter()
                .filter(|e| e.data.first() == Some(&0x90))
                .count();
            if note_ons >= 3 {
                break;
            }
            assert!(
                !node.finished.load(Ordering::Relaxed),
                "the sequencer exited while playing"
            );
            assert!(
                std::time::Instant::now() < deadline,
                "the sequencer never played its pattern; got {} events",
                events.len()
            );
        }

        // It announced the start, so anything slaved to it knows to run.
        assert!(
            events.iter().any(|e| e.data == vec![0xFA]),
            "no MIDI start message"
        );

        // The clock pulses are evenly spaced at exactly 24 per quarter note.
        // This is the arithmetic that used to be frame counting.
        let clocks: Vec<u64> = events
            .iter()
            .filter(|e| e.data == vec![0xF8])
            .map(|e| e.time)
            .collect();
        assert!(
            clocks.len() > 20,
            "expected a run of clock pulses, got {}",
            clocks.len()
        );
        for pair in clocks.windows(2) {
            let gap = pair[1] as f64 - pair[0] as f64;
            assert!(
                (gap - pulse_us).abs() < 2.0,
                "clock pulses {pulse_us:.1}us apart, got {gap:.1}us"
            );
        }

        // The note lands at velocity 96 — the sequencer plays what was written,
        // it does not flatten every note to one loudness.
        let note_ons: Vec<&crate::midi::Event> = events
            .iter()
            .filter(|e| e.data.first() == Some(&0x90))
            .collect();
        assert_eq!(
            note_ons[0].data,
            vec![0x90, 60, 96],
            "the saved velocity reaches the synth"
        );

        // And the loop comes round after exactly the pattern's length, at the
        // saved tempo: eight sixteenths at 150 BPM is 800ms to the microsecond.
        let cycle_us = STEPS as f64 * step_us;
        for pair in note_ons.windows(2) {
            let gap = pair[1].time as f64 - pair[0].time as f64;
            assert!(
                (gap - cycle_us).abs() < 2.0,
                "the pattern should come round every {cycle_us:.1}us, got {gap:.1}us"
            );
        }

        // The note is released one step later, not left hanging.
        let off = events
            .iter()
            .find(|e| e.data == vec![0x80, 60, 0])
            .expect("the note is released");
        let gap = off.time as f64 - note_ons[0].time as f64;
        assert!(
            (gap - step_us).abs() < 2.0,
            "a one-step note should last {step_us:.1}us, got {gap:.1}us"
        );

        node.kill.store(true, Ordering::Relaxed);
    }

    /// HTTPS lands on the fabric: a rustls listener joins the node's net as
    /// the named peer "tlssrv" (rcgen self-signed CA cert for that name), the
    /// guest is the real curl.wasm with its wolfSSL backend, and the CA is
    /// seeded at /etc/ssl/cacert.pem — the exact path plugins/curl baked in
    /// via --with-ca-bundle, so this exercises default trust, no --cacert
    /// flag. The body arriving in the terminal log proves the whole chain:
    /// fabric DNS, TCP over smoltcp, the wolfSSL<->rustls handshake, cert
    /// verification against the vfs bundle, HTTP over the encrypted stream.
    #[test]
    fn curl_fetches_https_over_the_fabric() {
        use std::io::{Read, Write};

        let wasm = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/curl/curl.wasm");
        if !wasm.exists() {
            eprintln!("skipping: build plugins/curl first (./build.sh)");
            return;
        }

        // A self-signed CA that doubles as the server cert for "tlssrv".
        // CA:TRUE matters: wolfSSL only takes basic-constraint CAs from a
        // bundle, so a plain self-signed leaf would be rejected as an anchor.
        let mut params =
            rcgen::CertificateParams::new(vec!["tlssrv".to_string()]).expect("cert params");
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let key = rcgen::KeyPair::generate().expect("keypair");
        let cert = params.self_signed(&key).expect("self-signed cert");

        // ring explicitly: two providers are linked into the test binary, so
        // rustls's process default would be ambiguous.
        let tls_config = {
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            Arc::new(
                rustls::ServerConfig::builder_with_provider(provider)
                    .with_safe_default_protocol_versions()
                    .expect("protocol versions")
                    .with_no_client_auth()
                    .with_single_cert(
                        vec![cert.der().clone()],
                        rustls::pki_types::PrivateKeyDer::Pkcs8(key.serialize_der().into()),
                    )
                    .expect("server config"),
            )
        };

        let host = PluginHost::new().expect("host");
        let nodes: NodeRegistry = Arc::new(Mutex::new(Vec::new()));
        let id = NodeId::new();
        host.spawn(
            &wasm,
            "curl",
            id,
            &[],
            Arc::new(Mutex::new(Vec::new())),
            nodes.clone(),
            Vec::new(),
            None,
        )
        .expect("spawn");
        // curl imports wasi:sockets, so it waits to be Run; the first-ever
        // compile of a 2.4 MB tool can take minutes, cached runs are instant.
        let node = {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
            loop {
                if let Some(n) = nodes.lock().unwrap().iter().find(|n| n.id == id).cloned() {
                    if n.is_runnable() {
                        break n;
                    }
                }
                assert!(std::time::Instant::now() < deadline, "curl never compiled");
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        };

        // What the images' COPY cacert.pem provides: trust on the baked path.
        node.fs
            .lock()
            .unwrap()
            .put_file_at("etc/ssl/cacert.pem", cert.pem().into_bytes());

        // One HTTPS response, served through rustls over the accepted fabric
        // connection (the netsurf websrv pattern, plus the TLS wrap).
        let body = "tls over the fabric works";
        let kill_srv = Arc::new(AtomicBool::new(false));
        wk_fabric::listen::listen(
            host.hub(),
            node.net_stack().expect("curl has a fabric stack"),
            "tlssrv",
            8443,
            kill_srv.clone(),
            Arc::new(move |mut s: std::os::unix::net::UnixStream| {
                let tls_config = tls_config.clone();
                std::thread::spawn(move || {
                    let mut conn = rustls::ServerConnection::new(tls_config).expect("tls conn");
                    let mut tls = rustls::Stream::new(&mut conn, &mut s);
                    let mut req = Vec::new();
                    let mut byte = [0u8; 1];
                    while !req.ends_with(b"\r\n\r\n") {
                        match tls.read(&mut byte) {
                            Ok(1) => req.push(byte[0]),
                            _ => return,
                        }
                    }
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = tls.write_all(reply.as_bytes());
                    let _ = tls.flush();
                    tls.conn.send_close_notify();
                    let _ = tls.flush();
                });
            }),
        );

        host.run_node(
            &node,
            &["-sS".to_string(), "https://tlssrv:8443/".to_string()],
        )
        .expect("run curl");

        // The body in the terminal log is the proof; -sS prints only the
        // payload on success and a bare error line on failure.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        loop {
            let (bytes, _) = node.term_io.log_read(0);
            let out = String::from_utf8_lossy(&bytes).to_string();
            if out.contains(body) {
                break;
            }
            if node.finished.load(Ordering::Relaxed) {
                // Re-read once: the last write can land right at exit.
                let (bytes, _) = node.term_io.log_read(0);
                let out = String::from_utf8_lossy(&bytes).to_string();
                assert!(
                    out.contains(body),
                    "curl exited without the body; output:\n{out}"
                );
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "curl never printed the fetched body; output:\n{out}"
            );
            std::thread::sleep(std::time::Duration::from_millis(30));
        }

        node.kill.store(true, Ordering::Relaxed);
        kill_srv.store(true, Ordering::Relaxed);
    }

    /// A real PDF reader: UNMODIFIED MuPDF (plugins/mupdf — fitz plus its
    /// vendored thirdparty tree, cross-compiled by mupdf's own Makefile)
    /// renders the checked-in single-page test PDF (test/red-box.pdf: a big
    /// red rectangle plus a line of base-14 text) onto a [`VirtualSurface`]
    /// through the shared gfx-compat shim. The test seeds the PDF into the
    /// node's vfs — standing in for the bindmount wire; the argv-less viewer
    /// scans / for the first *.pdf — pumps frames headless exactly like the
    /// doom test, asserts a long run of red page pixels, then injects a `-`
    /// zoom-out key and asserts the frame changes (the PDF has one page, so
    /// page-turn would be a no-op; zoom is the observable control). Skipped
    /// when the artifact (mupdf-view.wasm, from ./build.sh) is missing.
    #[test]
    fn mupdf_renders_red_box_pdf_and_zooms() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/mupdf");
        let wasm = dir.join("mupdf-view.wasm");
        let pdf = dir.join("test/red-box.pdf");
        if !wasm.exists() {
            eprintln!("skipping: build plugins/mupdf first (./build.sh)");
            return;
        }

        let host = PluginHost::new().expect("host");
        let nodes: NodeRegistry = Arc::new(Mutex::new(Vec::new()));
        let surfaces: SurfaceRegistry = Arc::new(Mutex::new(Vec::new()));
        let id = NodeId::new();
        host.spawn(
            &wasm,
            "mupdf",
            id,
            &[],
            surfaces.clone(),
            nodes.clone(),
            Vec::new(),
            None,
        )
        .expect("spawn");

        // The node registers synchronously; seed the document before the
        // (much slower) background compile lets the guest run — a wired PDF
        // lands at /<name>, which is exactly where the viewer scans.
        let node = nodes
            .lock()
            .unwrap()
            .iter()
            .find(|n| n.id == id)
            .cloned()
            .expect("node registered");
        node.fs
            .lock()
            .unwrap()
            .put_file_at("red-box.pdf", std::fs::read(&pdf).expect("read pdf"));

        // Engine compile + fitz boot, then the surface appears.
        let surface = {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
            loop {
                if let Some(s) = surfaces.lock().unwrap().first().cloned() {
                    break s;
                }
                assert!(
                    !node.finished.load(Ordering::Relaxed),
                    "mupdf exited before opening a surface"
                );
                assert!(
                    std::time::Instant::now() < deadline,
                    "mupdf never opened a surface"
                );
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        };
        let pump_frame = || {
            let mut s = surface.lock().unwrap();
            s.frame_ready = true;
            s.wake();
        };

        // The PDF fills most of its page with pure-red (1 0 0 rg) — at
        // fit-width that is a run of hundreds of red pixels mid-frame, and
        // nothing else in the composition (white mat, dark text, error
        // screen) comes close. Require a healthy run to rule out artifacts.
        let red_run = |pixels: &[u8]| {
            let mut best = 0usize;
            let mut run = 0usize;
            for px in pixels.chunks_exact(4) {
                if px[0] > 200 && px[1] < 60 && px[2] < 60 {
                    run += 1;
                    best = best.max(run);
                } else {
                    run = 0;
                }
            }
            best
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        loop {
            pump_frame();
            std::thread::sleep(std::time::Duration::from_millis(15));
            let s = surface.lock().unwrap();
            if red_run(&s.pixels) >= 100 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "mupdf never painted the red page"
            );
        }

        // Zoom out: `-` round-trips through gfx-compat into the viewer,
        // which re-renders the page smaller (white mat appears at the
        // sides), so the frame must change once the key queues drain.
        let before: Vec<u8> = surface.lock().unwrap().pixels.clone();
        {
            let mut s = surface.lock().unwrap();
            let minus = KeyEvent {
                key: Some(Key::Minus),
                // What the compositor sends: winit resolves the character and
                // the host forwards it, so a harness event that omits it is
                // testing a shape no real keystroke has.
                text: Some("-".into()),
                alt_key: false,
                ctrl_key: false,
                meta_key: false,
                shift_key: false,
                repeat: false,
            };
            s.key_down.push_back(minus.clone());
            s.key_up.push_back(minus);
            s.wake();
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        loop {
            pump_frame();
            std::thread::sleep(std::time::Duration::from_millis(15));
            let s = surface.lock().unwrap();
            if s.key_down.is_empty() && s.key_up.is_empty() && s.pixels != before {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "mupdf never consumed the zoom key / repainted"
            );
        }
        // Still a red page after the zoom — the document survived, smaller.
        assert!(
            red_run(&surface.lock().unwrap().pixels) >= 50,
            "the page vanished after zooming out"
        );

        // Shut down: close the surface and trip the kill switch.
        {
            let mut s = surface.lock().unwrap();
            s.closed = true;
            s.wake();
        }
        node.kill.store(true, Ordering::Relaxed);
    }

    /// doc-tools: the real pandoc (3.5, the upstream GHC-wasm build) and the
    /// real pdfTeX (1.40.29, cross-compiled from TeX Live source) layered on
    /// the bash image, exercised end-to-end through wk's own machinery. The
    /// test builds both images into an isolated store — the doctools
    /// Dockerfile's RUN steps dump latex.fmt with the *wasm* pdftex during
    /// the build — then mounts the image like a node and runs bash, which
    /// PATH-searches /bin and execs each tool via wk:exec. Finishes with the
    /// pipeline the image exists for: markdown -> LaTeX -> a real PDF in the
    /// node's vfs. Skipped when the artifacts aren't built
    /// (plugins/doctools: ./build.sh, which needs plugins/bash built first).
    /// SLOW by nature: pandoc is a 50 MB GHC-wasm module, and this test has
    /// consistently spent ~11 minutes single-core on it per run (the compile
    /// cache that makes the CLI's reruns quick has not been kicking in under
    /// the test harness — measured, not assumed). nextest reports it slow
    /// and moves on; it has no timeout.
    #[test]
    #[ignore = "~11 min: pandoc's 50MB GHC-wasm module compiles cold each run; run with --run-ignored"]
    fn doctools_image_execs_pandoc_and_pdflatex() {
        let doctools = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/doctools");
        let bash = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/bash");
        for needed in [
            bash.join("bash.wasm"),
            bash.join("bin"),
            doctools.join("pandoc.wasm"),
            doctools.join("pdftex.wasm"),
            doctools.join("texmf/texmf-dist/tex/latex/base/latex.ltx"),
        ] {
            if !needed.exists() {
                eprintln!("skipping: build plugins/doctools first (./build.sh)");
                return;
            }
        }

        // An isolated image store (thread-local), so the build neither reads
        // nor pollutes the user's.
        let store = std::env::temp_dir().join("wk-image-store-doctools-exec");
        let _ = std::fs::remove_dir_all(&store);
        std::fs::create_dir_all(&store).unwrap();
        crate::oci::set_test_cache_root(&store);

        let host = PluginHost::new().expect("host");
        let bash_id =
            crate::images::build_with_runner(&bash.join("Dockerfile"), Some(&host), false)
                .expect("bash base image builds");
        crate::images::set_tag("bash", &bash_id).unwrap();
        let id = crate::images::build_with_runner(&doctools.join("Dockerfile"), Some(&host), false)
            .expect("doctools image builds (pdftex -ini dumps latex.fmt in a RUN)");

        // A node running this image: rootfs layers mounted lazily, the
        // image's env, and the shell exec'ing tools out of its own /bin.
        let m = crate::images::load_image(&id).expect("manifest stored");
        let fs = crate::vfs::new_fs();
        crate::images::mount(&fs, &m.container_setup()).expect("mount image");
        let sh = fs
            .lock()
            .unwrap()
            .read_file("/bin/bash.wasm", usize::MAX)
            .expect("the base image's shell is in the rootfs");

        let run = |cmd: &str| {
            let argv = vec!["bash".to_string(), "-c".to_string(), cmd.to_string()];
            host.run_program(&sh, &argv, &m.env, &fs, Vec::new(), 0)
                .unwrap_or_else(|e| panic!("bash -c {cmd:?}: {e}"))
        };

        let out = run("pandoc --version");
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        assert_eq!(
            out.exit_code,
            0,
            "pandoc --version: {stdout}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            stdout.contains("pandoc 3.5"),
            "unexpected pandoc banner: {stdout}"
        );

        let out = run("pdftex --version");
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        assert_eq!(
            out.exit_code,
            0,
            "pdftex --version: {stdout}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            stdout.contains("pdfTeX 3.14"),
            "unexpected pdftex banner: {stdout}"
        );

        // The pipeline: pandoc's standalone LaTeX through pdflatex (argv[0]
        // picks the format), typeset against the image's minimal texmf.
        fs.lock().unwrap().put_file_at(
            "work/notes.md",
            b"# Hello\n\nA *small* doc with math: $x^2$.\n".to_vec(),
        );
        let out = run("cd /work && pandoc notes.md -s -o notes.tex \
             && pdflatex -interaction=nonstopmode notes.tex");
        assert_eq!(
            out.exit_code,
            0,
            "pipeline: {}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let pdf = fs
            .lock()
            .unwrap()
            .read_file("/work/notes.pdf", usize::MAX)
            .expect("pdflatex produced notes.pdf");
        assert!(
            pdf.starts_with(b"%PDF-"),
            "notes.pdf does not look like a PDF"
        );
    }

    /// The biscuit inspector (plugins/biscuit) end to end: a node reading its
    /// OWN capability token. The test mints a REAL credential the way wk does
    /// at startup — wk-token-service's node base token, whose authority block
    /// carries the wiring rule — publishes it hex-encoded at the path the
    /// server's `write_token_file` uses (`/run/wk/token` in the node's vfs),
    /// and pumps frames headless like the gfx-smoke test until the guest
    /// renders it. Then the live half: overwrite the file with a holder-side
    /// attenuated token (an appended `check if` block, exactly what `wk token
    /// attenuate` produces) and require the picture to change within the
    /// guest's ~1s re-read interval — the new block landing on the canvas.
    /// Skipped when the artifact isn't built.
    #[test]
    fn biscuit_inspector_renders_own_token_and_tracks_attenuation() {
        let wasm = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../plugins/biscuit/target/wasm32-wasip1/debug/biscuit.wasm");
        if !wasm.exists() {
            eprintln!("skipping: build plugins/biscuit first (mise run build)");
            return;
        }

        // The real minting authority: the same base token every wk app node
        // holds (authority rule: use what you're wired to, in every mode).
        let svc = wk_token_service::TokenService::new();
        let base = svc.mint_node_base().expect("mint the node base token");

        let host = PluginHost::new().expect("host");
        let nodes: NodeRegistry = Arc::new(Mutex::new(Vec::new()));
        let surfaces: SurfaceRegistry = Arc::new(Mutex::new(Vec::new()));
        let id = NodeId::new();
        host.spawn(
            &wasm,
            "biscuit",
            id,
            &[],
            surfaces.clone(),
            nodes.clone(),
            Vec::new(),
            None,
        )
        .expect("spawn");

        // The node registers synchronously; publish the token before the
        // (much slower) background compile lets the guest run — playing the
        // server's `write_token_file` role, same path, same hex encoding.
        let node = nodes
            .lock()
            .unwrap()
            .iter()
            .find(|n| n.id == id)
            .cloned()
            .expect("node registered");
        node.fs.lock().unwrap().put_file_at(
            "run/wk/token",
            crate::workspace::bytes_hex(&base).into_bytes(),
        );

        let surface = {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
            loop {
                if let Some(s) = surfaces.lock().unwrap().first().cloned() {
                    break s;
                }
                assert!(
                    !node.finished.load(Ordering::Relaxed),
                    "biscuit exited before opening a surface"
                );
                assert!(
                    std::time::Instant::now() < deadline,
                    "biscuit never opened a surface"
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        };
        let pump_frame = || {
            let mut s = surface.lock().unwrap();
            s.frame_ready = true;
            s.wake();
        };

        // The decoded datalog on the dark background: a non-uniform frame.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            pump_frame();
            std::thread::sleep(std::time::Duration::from_millis(15));
            let s = surface.lock().unwrap();
            let non_uniform = s.pixels.chunks_exact(4).any(|px| px != &s.pixels[0..4]);
            if non_uniform {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "biscuit never painted its token view"
            );
        }

        // The view is static between token changes — snapshot it, then
        // refresh the published file with an attenuated token: an appended
        // check block, holder-side (no signing key), `wk token attenuate`'s
        // exact move.
        let before: Vec<u8> = surface.lock().unwrap().pixels.clone();
        let attenuated = biscuit_auth::UnverifiedBiscuit::from(&base)
            .expect("reparse the minted token")
            .append(
                biscuit_auth::builder::BlockBuilder::new()
                    .code(r#"check if operation($k, $t, $a), $a != "write";"#)
                    .expect("attenuation block"),
            )
            .expect("append")
            .to_vec()
            .expect("serialize");
        node.fs.lock().unwrap().put_file_at(
            "run/wk/token",
            crate::workspace::bytes_hex(&attenuated).into_bytes(),
        );

        // The guest re-reads the file about once a second of pumped frames;
        // the new block (and its update flash) must repaint the canvas.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            pump_frame();
            std::thread::sleep(std::time::Duration::from_millis(15));
            let s = surface.lock().unwrap();
            if s.pixels != before {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "biscuit never re-rendered after the token was attenuated"
            );
        }

        // Shut down: close the surface and trip the kill switch.
        {
            let mut s = surface.lock().unwrap();
            s.closed = true;
            s.wake();
        }
        node.kill.store(true, Ordering::Relaxed);
    }
    #[test]
    fn world_node_publishes_a_glb_as_scenery_and_reloads_it() {
        // wk has no built-in world: the surrounding plaza is a node reading a
        // .glb out of its own filesystem and handing it to wk:scene as
        // scenery. This is that whole path, minus the renderer.
        let wasm = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../plugins/world/target/wasm32-wasip1/debug/world.wasm");
        if !wasm.exists() {
            eprintln!("skipping: build plugins/world first (cargo component build)");
            return;
        }
        let glb = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../example/home.glb"
        ))
        .expect("the home world ships with the repo");

        let host = PluginHost::new().expect("host");
        let nodes: NodeRegistry = Arc::new(Mutex::new(Vec::new()));
        let id = NodeId::new();
        host.spawn(
            &wasm,
            "world",
            id,
            &[], // no args: it picks up the first glTF wired into it
            Arc::new(Mutex::new(Vec::new())),
            nodes.clone(),
            Vec::new(),
            None,
        )
        .expect("spawn");

        // The node registers synchronously; seed the world into it before the
        // background compile lets the guest look, standing in for the bind
        // mount example/home.wk wires up.
        let node = nodes
            .lock()
            .unwrap()
            .iter()
            .find(|n| n.id == id)
            .cloned()
            .expect("node registered");
        node.fs.lock().unwrap().put_file_at("home.glb", glb.clone());

        // Wait for the entity, then check what the view would draw: this
        // node's geometry, byte for byte, flagged as scenery so it is never
        // ray-picked and stands in for the fallback ground plane.
        let published = |want: &[u8]| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
            loop {
                // Copy what we need out from under the lock: an assert that
                // fires while holding it would poison the entity for the
                // guest thread and bury this failure under another.
                let hit = host.scene_entities().into_iter().find_map(|e| {
                    let e = e.lock().unwrap();
                    (e.node_id == id && e.glb.as_slice() == want)
                        .then_some((e.scenery, e.glb_hash, e.pos))
                });
                if let Some((scenery, hash, pos)) = hit {
                    assert!(scenery, "a world is scenery, not a clickable object");
                    assert_eq!(hash, crate::scene::glb_hash(want), "cache key");
                    assert_eq!(pos, [0.0; 3], "the world sits at its node's pose");
                    return;
                }
                assert!(
                    !node.finished.load(Ordering::Relaxed),
                    "the world node exited without publishing its scenery"
                );
                assert!(
                    std::time::Instant::now() < deadline,
                    "no scenery entity appeared"
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        };
        published(&glb);

        // Editing the file swaps the plaza under your feet — no restart. (The
        // node never parses the blob; only the view does, so any changed bytes
        // exercise the reload.)
        let mut edited = glb.clone();
        edited.extend_from_slice(b"\0edited");
        node.fs
            .lock()
            .unwrap()
            .put_file_at("home.glb", edited.clone());
        published(&edited);
        assert_eq!(
            host.scene_entities()
                .iter()
                .filter(|e| e.lock().unwrap().node_id == id)
                .count(),
            1,
            "the old world is dropped, not stacked"
        );

        node.kill.store(true, Ordering::Relaxed);
    }
    /// LibreOffice Impress, drawing its own window through vcl/wk.
    ///
    /// This is the whole port's observable: an office suite that has never had
    /// a windowing system compiled into it, presenting a composited frame to a
    /// wk surface. vcl/wk is a SvpSalInstance subclass -- cairo, freetype and
    /// fontconfig do the drawing exactly as they do headless -- plus a
    /// compositor that flattens VCL's many SalFrames (a menu, a tooltip and a
    /// dialog are each their own frame) into the one RGBA8 buffer a node has.
    ///
    /// instdir is bind-mounted rather than layered into an image because the
    /// install tree is ~1 GB of build output and the port hardcodes /instdir --
    /// nothing at run time can discover where it is, since wasm has no dladdr,
    /// wasi-libc's realpath is a stub, and a guest gets only a basename in
    /// argv[0]. Skipped when the artifact isn't built.
    #[test]
    fn libreoffice_impress_paints_its_window_through_vcl_wk() {
        let plugin = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/libreoffice");
        let wasm = plugin.join("build/instdir/program/soffice.bin");
        if !wasm.exists() {
            eprintln!("skipping: build plugins/libreoffice first (./build-lo.sh)");
            return;
        }
        // The install tree comes from image://libreoffice, exactly as it does
        // for a node on the canvas -- NOT from a bind mount of build/instdir.
        // Bind-mounting it here is what let a node type ship that threw
        //   Cannot open uno ini file:///instdir/program/unorc
        // the moment anyone added one from the Cmd+K palette: the test supplied
        // a filesystem the shipped node had no way to get.
        let Some(image_id) = crate::images::resolve_ref("libreoffice") else {
            eprintln!("skipping: build the image (plugins/libreoffice/build-image.sh)");
            return;
        };
        let image = crate::images::load_image(&image_id)
            .expect("the libreoffice image")
            .container_setup();

        // A writable place for the document. std::env::temp_dir rather than a
        // tempfile crate, matching the shader test: the directory outlives the
        // guest by design, because a node log is worth reading after a failure.
        let base = std::env::temp_dir().join(format!("wk-lo-{}", std::process::id()));
        let work = base.join("work");
        std::fs::create_dir_all(&work).expect("workdir");
        std::fs::copy(plugin.join("work/mini.fodp"), work.join("mini.fodp"))
            .expect("the test document; run plugins/libreoffice/run-lo.sh once to create work/");

        let host = PluginHost::new().expect("host");
        let nodes: NodeRegistry = Arc::new(Mutex::new(Vec::new()));
        let surfaces: SurfaceRegistry = Arc::new(Mutex::new(Vec::new()));
        let id = NodeId::new();
        host.spawn(
            &wasm,
            "libreoffice",
            id,
            &[],
            surfaces.clone(),
            nodes.clone(),
            Vec::new(),
            Some(crate::images::ContainerSetup {
                layers: image.layers.clone(),
                env: {
                    // The image's own env (HOME, TMPDIR), plus what only a test
                    // wants. Nothing is invented: a test that supplies an
                    // environment the shipped node does not get is testing a
                    // configuration nobody runs, which is how the first version
                    // of this test passed while `wk run ./example/impress.wk`
                    // threw.
                    let mut env = image.env.clone();
                    // The build is configured with --enable-sal-log, and a
                    // silent LibreOffice is indistinguishable from one that
                    // never started. These warnings are the progress report.
                    env.push((
                        "SAL_LOG".into(),
                        std::env::var("WK_LO_SAL_LOG").unwrap_or_else(|_| "+WARN".into()),
                    ));
                    // The shim's own trace knobs (WK_LO_TRACE_THROW,
                    // WK_LO_TRAP_THROW, WK_LO_TRACE_ALLOC and vcl/wk's frame
                    // dumps) are forwarded rather than named, exactly as
                    // plugins/libreoffice/run-lo.sh forwards them: a knob added
                    // to the guest should not need a change here to be usable.
                    for (k, v) in std::env::vars() {
                        if k.starts_with("WK_LO_") && k != "WK_LO_SAL_LOG" && k != "WK_LO_DUMP" {
                            env.push((k, v));
                        }
                    }
                    env
                },
            }),
        )
        .expect("spawn");

        // soffice.bin links curl, so it imports wasi:sockets, so wk classes it
        // as a networked node -- and networked nodes wait to be Run rather than
        // auto-starting. Without run_node below, the node sits compiled and
        // idle forever and the only symptom is an empty log.
        //
        // Compiling 190 MB of wasm is minutes on a cold cache, so the wait for
        // is_runnable() is generous.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(900);
        let node = loop {
            if let Some(n) = nodes.lock().unwrap().iter().find(|n| n.id == id).cloned() {
                if n.is_runnable() {
                    break n;
                }
                assert!(
                    !n.finished.load(Ordering::Relaxed),
                    "libreoffice node failed to compile"
                );
            }
            assert!(
                std::time::Instant::now() < deadline,
                "libreoffice node never compiled"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        };

        // The one wire example/impress.wk draws: the documents. /instdir came
        // from the image layers, and there is no /tmp mount because a node's own
        // filesystem root is writable. The mount goes between compile and run
        // rather than racing the guest, which the shader test's single file can
        // afford to do and a whole install tree cannot.
        crate::vfs::mount_host(&node.fs, "/work", work.clone(), true);

        // The arguments belong to run_node, not to spawn: a networked node's
        // spawn arguments are the ones it would have auto-started with, and it
        // does not auto-start. Passing them here is what a Run from the UI does.
        host.run_node(
            &node,
            // The image's ENTRYPOINT already carries -env:UserInstallation, so
            // this is what example/impress.wk's node passes and nothing more.
            &["/work/mini.fodp".into()],
        )
        .expect("run libreoffice");

        // Cranelift on a 190 MB component, then UNO bootstrap, then the
        // document. Generous, and the assert reports the node's own log.
        let started = std::time::Instant::now();
        let mut last_report = 0u64;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(900);
        let surface = loop {
            if let Some(s) = surfaces.lock().unwrap().first().cloned() {
                break s;
            }
            assert!(
                !node.finished.load(Ordering::Relaxed),
                "soffice exited before opening a surface; node log:\n{}",
                String::from_utf8_lossy(&node.term_io.log_read(0).0)
            );
            assert!(
                std::time::Instant::now() < deadline,
                "soffice never opened a surface; node log:\n{}",
                String::from_utf8_lossy(&node.term_io.log_read(0).0)
            );
            std::thread::sleep(std::time::Duration::from_millis(100));
            if started.elapsed().as_secs() / 30 != last_report {
                last_report = started.elapsed().as_secs() / 30;
                eprintln!(
                    "  [{}s] still waiting for a surface; {} bytes of node log",
                    started.elapsed().as_secs(),
                    node.term_io.log_read(0).0.len()
                );
            }
        };

        let pump_frame = || {
            let mut s = surface.lock().unwrap();
            s.frame_ready = true;
            s.wake();
        };

        // The UI resizes the surface to the node's on-canvas size on the very
        // first frame it drives (client-local-ui/src/compositor.rs's
        // drive_surfaces), and example/impress.wk gives the node a size that is
        // not 800x600. A test that never resizes is testing a configuration
        // nobody runs -- which is exactly how this one passed while `wk run
        // ./example/impress.wk` threw.
        let resize_to = |w: u32, h: u32| {
            let mut s = surface.lock().unwrap();
            s.width = w;
            s.height = h;
            s.pixels = vec![0; (w * h * 4) as usize];
            s.resize = Some(ResizeEvent {
                width: w,
                height: h,
            });
            s.frame_ready = true;
            s.wake();
        };

        // The node in example/impress.wk is 1000x720, and the host is
        // resize-authoritative.
        resize_to(1000, 720);

        // What an Impress window is, as a histogram: mostly VCL's light grey
        // face colour, a substantial white area (the slide and the sidebar),
        // and thousands of dark pixels (menu text, toolbar glyphs, borders).
        // A blank fill passes none of those three; the compositor's own
        // mid-grey background passes none of them either, which is why it is
        // mid-grey and not black.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(600);
        let (dark, light, white) = loop {
            pump_frame();
            std::thread::sleep(std::time::Duration::from_millis(20));
            {
                let s = surface.lock().unwrap();
                let mut dark = 0usize;
                let mut light = 0usize;
                let mut white = 0usize;
                for px in s.pixels.chunks_exact(4) {
                    let lum = px[0] as u32 + px[1] as u32 + px[2] as u32;
                    if lum < 200 {
                        dark += 1;
                    } else if lum > 750 {
                        white += 1;
                        light += 1;
                    } else if lum > 600 {
                        light += 1;
                    }
                }
                if dark > 2_000 && light > 100_000 && white > 20_000 {
                    break (dark, light, white);
                }
            }
            assert!(
                !node.finished.load(Ordering::Relaxed),
                "soffice exited before painting; node log:\n{}",
                String::from_utf8_lossy(&node.term_io.log_read(0).0)
            );
            assert!(
                std::time::Instant::now() < deadline,
                "soffice never painted a window; node log:\n{}",
                String::from_utf8_lossy(&node.term_io.log_read(0).0)
            );
        };
        eprintln!("impress frame: {dark} dark, {light} light, {white} white px");

        // Now input. The layout has to settle first: for the first seconds
        // after the document opens, panels appear and toolbars reflow, and a
        // pixel change during that proves nothing about input at all -- the
        // click that "worked" in the first draft of this test landed on empty
        // canvas and was credited with the Slides panel arriving.
        let snapshot = |s: &std::sync::MutexGuard<'_, VirtualSurface>| -> Vec<u8> {
            s.pixels
                .chunks_exact(4)
                .flat_map(|p| [p[0], p[1], p[2]])
                .collect::<Vec<u8>>()
        };
        let differing = |a: &[u8], b: &[u8]| -> usize {
            a.chunks_exact(3)
                .zip(b.chunks_exact(3))
                .filter(|(x, y)| x != y)
                .count()
        };

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
        let mut settled = snapshot(&surface.lock().unwrap());
        let mut stable = 0;
        loop {
            pump_frame();
            std::thread::sleep(std::time::Duration::from_millis(30));
            let now = snapshot(&surface.lock().unwrap());
            stable = if differing(&settled, &now) < 50 {
                stable + 1
            } else {
                0
            };
            settled = now;
            if stable >= 10 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the window never stopped changing on its own, so no input test can be \
                 attributed; node log:\n{}",
                String::from_utf8_lossy(&node.term_io.log_read(0).0)
            );
        }
        eprintln!("impress: layout settled");

        // The window must FILL the surface. wkcompositor.cxx paints its
        // background 0x303030 -- a mid-grey chosen so that anything showing
        // through looks like the bug it is -- and a settled window that leaves
        // any of it visible means a frame did not follow the host's resize.
        //
        // This is the assertion that was missing when the resize filter asked
        // GetParent() == nullptr: LibreOffice's document window is parented to
        // the hidden default frame, so it never grew, and the node presented an
        // 800x600 window inside a 1000x720 surface with a band down two sides.
        // Nothing failed. It just looked wrong, and the histogram below counted
        // the band as "dark pixels" and passed.
        {
            let s = surface.lock().unwrap();
            let bg = s
                .pixels
                .chunks_exact(4)
                .filter(|p| p[0] == 0x30 && p[1] == 0x30 && p[2] == 0x30)
                .count();
            assert!(
                bg < 1000,
                "{bg} pixels of compositor background are still showing at {}x{}: \
                 a frame did not follow the resize",
                s.width,
                s.height
            );
        }

        // Tab, which in Impress's editing view selects the next object on the
        // slide and draws handles around it. A key rather than a click because
        // it needs no coordinates and so cannot be aimed at the wrong thing;
        // and it exercises the half of the input path a click does not -- the
        // key translation, the modifier mapping, and delivery to the frame the
        // Router last decided was focused.
        {
            let mut s = surface.lock().unwrap();
            let ev = KeyEvent {
                key: Some(Key::Tab),
                text: None,
                alt_key: false,
                ctrl_key: false,
                meta_key: false,
                shift_key: false,
                repeat: false,
            };
            s.key_down.push_back(ev.clone());
            s.key_up.push_back(ev);
            s.wake();
        }

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        let changed = loop {
            pump_frame();
            std::thread::sleep(std::time::Duration::from_millis(20));
            let n = differing(&settled, &snapshot(&surface.lock().unwrap()));
            if n > 300 {
                break n;
            }
            assert!(
                !node.finished.load(Ordering::Relaxed),
                "soffice exited during the key test; node log:\n{}",
                String::from_utf8_lossy(&node.term_io.log_read(0).0)
            );
            assert!(
                std::time::Instant::now() < deadline,
                "Tab changed nothing on a settled window -- key input never reached sd; \
                 node log:\n{}",
                String::from_utf8_lossy(&node.term_io.log_read(0).0)
            );
        };
        eprintln!("impress: Tab selected an object, {changed} px changed");

        // Then stop pumping, and check it is still alive. This is the one thing
        // the earlier version of this test never did, and it is what shipped a
        // node that died a few seconds after anyone opened it with no document:
        // SvpSalInstance::ImplYield's idle path is an untimed
        // condition_variable::wait ("wait until something happens"), and in a
        // component with one thread nothing can ever signal it. A window with a
        // document in it always has work and never reaches that wait; the Start
        // Center reaches it within seconds.
        //
        // No pump_frame() in this loop on purpose: the guest must find its own
        // way to wake, which is what WkSalInstance::waitForSomething is for.
        let idle_until = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while std::time::Instant::now() < idle_until {
            std::thread::sleep(std::time::Duration::from_millis(200));
            assert!(
                !node.finished.load(Ordering::Relaxed),
                "soffice died while idle -- something reached svp's untimed condvar wait; \
                 node log:\n{}",
                String::from_utf8_lossy(&node.term_io.log_read(0).0)
            );
        }
        eprintln!("impress: survived 20s idle with nobody pumping frames");

        // A histogram proves "not blank", never "a window". WK_LO_DUMP=/tmp/f.ppm
        // writes the composited surface out so a human can look at it, which is
        // the only way to catch a backend that paints something plausible and
        // wrong.
        if let Ok(path) = std::env::var("WK_LO_DUMP") {
            let s = surface.lock().unwrap();
            let mut ppm = format!("P6\n{} {}\n255\n", s.width, s.height).into_bytes();
            ppm.extend(s.pixels.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]));
            std::fs::write(&path, ppm).expect("write frame dump");
            eprintln!("impress frame dumped to {path}");
            // With the dump comes the log, because the interesting half of
            // "does it look right" is vcl/wk's frame census -- which frames
            // exist, where, and which are visible -- and a passing test prints
            // nothing otherwise.
            eprintln!(
                "--- libreoffice node log ---\n{}",
                String::from_utf8_lossy(&node.term_io.log_read(0).0)
            );
        }

        node.kill.store(true, Ordering::Relaxed);
    }
}
