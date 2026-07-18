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
pub use wasi::surface::surface::{Key, KeyEvent, PointerEvent, ResizeEvent};
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
    pub fn is_runnable(&self) -> bool {
        self.setup.get().is_some_and(|s| s.run.is_some())
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
    pub(crate) midi_in: crate::midi::SharedInbox,
    pub(crate) midi_router: crate::midi::Router,
    pub(crate) options: crate::options::SharedOptions,
    /// This node's network context (smoltcp stack on the fabric) — `Some` only
    /// for nodes that import wasi:sockets. Backs wk's own wasi:sockets impl.
    pub(crate) net: Option<crate::sockets::NetCtx>,
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

/// Whether a component imports `wasi:sockets` — i.e. it does networking and so
/// needs a NIC on the fabric.
fn component_imports_sockets(component: &Component, engine: &Engine) -> bool {
    component
        .component_type()
        .imports(engine)
        .any(|(name, _)| name.starts_with("wasi:sockets/"))
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
            midi_in: crate::midi::new_inbox(),
            midi_router: self.midi.clone(),
            options: crate::options::new_options(Vec::new()),
            net: None,
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
            let command = wasmtime_wasi::p2::bindings::Command::instantiate_async(
                &mut store, &component, &linker,
            )
            .await
            .map_err(|e| format!("instantiate RUN step: {e:#}"))?;
            match command.wasi_cli_run().call_run(&mut store).await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(())) => Err("RUN step exited with failure".to_string()),
                Err(e) => match e.downcast_ref::<wasmtime_wasi::I32Exit>() {
                    Some(wasmtime_wasi::I32Exit(0)) => Ok(()),
                    Some(wasmtime_wasi::I32Exit(code)) => {
                        Err(format!("RUN step exited with status {code}"))
                    }
                    None => Err(format!("RUN step trapped: {e:#}")),
                },
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
    hub: Arc<wk_fabric::netstack::NetHub>,
}

impl PluginHost {
    pub fn new() -> Result<Self> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        // The WebAssembly exception-handling proposal (new `exnref` model), so
        // guests that use setjmp/longjmp run: wasi-sdk lowers setjmp to wasm EH,
        // and with LTO + `-mllvm -wasm-use-legacy-eh=false` it emits the exnref
        // form cranelift supports. This unlocks interpreters (Lua) and the whole
        // error-recovery class of recompiled C/C++.
        config.wasm_exceptions(true);
        // Lets the server stop a runaway node: increment_epoch() each frame
        // trips the per-store deadline callback, which traps on `kill`.
        config.epoch_interruption(true);
        // Persist compiled machine code to an on-disk cache so a plugin is only
        // Cranelift-compiled once ever — subsequent launches load the cached
        // artifact (a debug sqlite drops from ~3s to milliseconds). Best-effort:
        // if the cache can't be set up, we just compile every launch as before.
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
            hub: wk_fabric::netstack::NetHub::new(),
        })
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
        // WASI 0.3 (`@0.3.0`) interfaces — cli, clocks, filesystem, random,
        // sockets — built on the Component Model's native async (no `wasi:io`).
        // Added alongside the 0.2 set above (different version namespaces, no
        // clash) so a guest compiled against either WASI generation runs. p3 in
        // wasmtime-wasi is still experimental; it reuses our existing `WasiCtx`
        // (`HostState: WasiView`), so it's purely additive.
        //
        // FOLLOW-UP: 0.3 guests get wasmtime's real (sandboxed, no-preopen)
        // filesystem here rather than our in-memory vfs — so they effectively see
        // no files and can't reach VirtualFile/HostMappedFile nodes. Backing 0.3
        // with the vfs means a from-scratch host impl of `wasi:filesystem@0.3.0`'s
        // ~26 async (`stream`/`future`, component-model-async) methods over
        // `crate::vfs::Fs` — comparable in size to the 0.2 vfs. Deferred until a
        // wasip3 toolchain exists to build a 0.3 guest to verify it against (the
        // newest stable Clang target is wasm32-wasip2). Until then this is
        // unverifiable, so we keep the host-backed (empty) 0.3 fs as a stub.
        wasmtime_wasi::p3::add_to_linker(&mut linker)?;
        // Only the wasi:http interfaces (outgoing-handler + types); the rest of
        // the wasi world is already linked above.
        wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker)?;
        // WASI 0.3 http (`@0.3.0` client + types), alongside the 0.2 http above.
        wasmtime_wasi_http::p3::add_to_linker(&mut linker)?;
        crate::vfs::add_to_linker(&mut linker)?;
        crate::audio::add_to_linker(&mut linker)?;
        crate::midi::add_to_linker(&mut linker)?;
        crate::options::add_to_linker(&mut linker)?;
        crate::tty::add_to_linker(&mut linker)?;
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
            midi_in: midi_in.clone(),
            midi_router: midi.clone(),
            options: crate::options::new_options(Vec::new()),
            net: None,
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
        port: u16,
        kill: Arc<AtomicBool>,
    ) -> Result<()> {
        // The fabric crate reports plain anyhow errors; bridge into wasmtime's.
        wk_fabric::portfwd::forward(self.hub.clone(), target, port, kill)
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
                        return;
                    }
                }
                if let Err(e) = host.load_and_setup(&node, &path, &name, &args, surfaces) {
                    eprintln!("failed to load plugin {name:?}: {e:#}");
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
        let net_stack = if !is_http && component_imports_sockets(&component, &self.engine) {
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
            .env("LINES", rows.to_string());
        // Outbound http follows the node's fabric stack's host access (gateway).
        let http_stack = net.as_ref().map(|n| n.stack.clone());
        let state = HostState {
            ctx: ctx_builder.build(),
            table: ResourceTable::new(),
            registry: run.surfaces.clone(),
            node_id: node.id,
            fs: node.fs.clone(),
            term_io: node.term_io.clone(),
            midi_in: node.midi_in.clone(),
            midi_router: self.midi.clone(),
            options: node.options.clone(),
            net,
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
                    let command = wasmtime_wasi::p2::bindings::Command::instantiate_async(
                        &mut store, &component, &linker,
                    )
                    .await?;
                    // A clean `exit()` (incl. `main` returning) surfaces as an
                    // `I32Exit` trap; that's a normal end, not a host error. The
                    // run result's inner Err is just a non-zero exit code.
                    match command.wasi_cli_run().call_run(&mut store).await {
                        Ok(_) => Ok(()),
                        Err(e) if e.downcast_ref::<wasmtime_wasi::I32Exit>().is_some() => Ok(()),
                        Err(e) => Err(e),
                    }
                } else {
                    let compositor =
                        Compositor::instantiate_async(&mut store, &component, &linker).await?;
                    compositor.call_run(&mut store).await
                }
            });
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
        let engine = Engine::new(&config).expect("engine");
        let mut linker: Linker<HostState> = Linker::new(&engine);
        crate::vfs::add_wasi_except_fs(&mut linker).expect("wasi 0.2 (minus fs) links");
        wasmtime_wasi::p3::add_to_linker(&mut linker).expect("wasi 0.3 links");
    }

    /// The full host linker — every wk interface (wasi-gfx, audio, midi, the 0.2
    /// http/vfs/random set) plus the WASI 0.3 set — composes without a name
    /// clash. Guards against a future interface overlapping an existing one.
    #[test]
    fn full_host_linker_builds() {
        let host = PluginHost::new().expect("host");
        host.build_linker().expect("full linker builds");
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
}
