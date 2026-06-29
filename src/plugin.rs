//! Host side of the wk plugin system: the compositor implements the standard
//! wasi-gfx interfaces (`wasi:surface`, `wasi:graphics-context`,
//! `wasi:frame-buffer`) over a *virtual surface* and drives a self-driving
//! client's `run` loop.
//!
//! Each client runs on its own thread with its own wasmtime `Store`; the
//! compositor (main thread) shares per-surface state via `SurfaceRegistry`. The
//! client blocks on its surface frame event; the host signals one frame per
//! compositor frame and reads back the pixels the client paints.

use std::collections::VecDeque;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
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

/// Shared state of one virtual surface, touched by both the client thread (via
/// the host interface impls) and the compositor thread.
pub struct VirtualSurface {
    /// Stable unique id, used by the compositor to track this surface.
    pub id: u64,
    /// The instance that created this surface (its window belongs to it).
    pub node_id: u64,
    pub width: u32,
    pub height: u32,
    /// Latest painted RGBA8 pixels (`width * height * 4`).
    pub pixels: Vec<u8>,
    /// Set by the compositor once per frame; consumed by the frame pollable.
    pub frame_ready: bool,
    /// Set by the compositor to close this instance: the client traps on its
    /// next `get_frame` and its thread exits.
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

/// Error a closed surface returns to unwind and end its client cleanly. The
/// driver recognises it and exits the client thread without logging an error.
#[derive(Debug)]
struct SurfaceClosed;

impl std::fmt::Display for SurfaceClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "surface closed")
    }
}

impl std::error::Error for SurfaceClosed {}

impl VirtualSurface {
    fn new(node_id: u64, width: u32, height: u32) -> Self {
        Self {
            id: NEXT_SURFACE_ID.fetch_add(1, Ordering::Relaxed),
            node_id,
            width,
            height,
            pixels: vec![0; (width * height * 4) as usize],
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

    /// Wake every pollable parked on this surface so they re-check readiness.
    pub fn wake(&mut self) {
        for w in self.wakers.drain(..) {
            w.wake();
        }
    }
}

pub type SharedSurface = Arc<Mutex<VirtualSurface>>;
/// All virtual surfaces created by clients, shared with the compositor thread.
pub type SurfaceRegistry = Arc<Mutex<Vec<SharedSurface>>>;

/// A launched plugin instance. Every instance gets a window in the compositor —
/// its surface if it created one, otherwise a console showing this captured
/// output — so nothing ever runs invisibly or un-quittably.
pub struct Node {
    /// Stable id, assigned by the compositor and persisted in the session so
    /// connections can refer to this node across restarts.
    pub id: u64,
    /// The dependency name this node was launched from.
    pub name: String,
    /// The node's terminal stdio: its stdout feeds the compositor's VT parser,
    /// its stdin is fed from the keyboard. Non-graphical nodes render as a
    /// terminal window.
    pub term_io: crate::terminal::SharedTermIo,
    /// This node's in-memory filesystem, so the compositor can mount connected
    /// file nodes into it.
    pub fs: crate::vfs::SharedFs,
    /// This node's MIDI input queue, so the compositor can wire a MIDI source's
    /// output to it.
    pub midi_in: crate::midi::SharedInbox,
    /// This node's option values (e.g. knob settings) reported by the guest, so
    /// the compositor can persist them to the session and seed them on restore.
    pub options: crate::options::SharedOptions,
    /// If this node is a `wasi:http` server (exports `incoming-handler`), the
    /// component path to serve when it's wired to a HostPort. Such nodes don't
    /// run a `run` loop; they're served on demand.
    pub http_path: Option<std::path::PathBuf>,
    /// Set by the guest thread when its `run` returns (it exited on its own).
    pub finished: Arc<AtomicBool>,
    /// Kill switch: set by the compositor to stop a still-running node.
    pub kill: Arc<AtomicBool>,
}

pub type SharedNode = Arc<Node>;
/// All launched app nodes, shared with the compositor thread.
pub type NodeRegistry = Arc<Mutex<Vec<SharedNode>>>;

// ---- resource representations stored in the wasmtime ResourceTable ----

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

// ---- pollables ----

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
            // A closed surface wakes the frame poll so the client proceeds to
            // `get_frame`, which then traps and ends the client thread.
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

// ---- per-store host state ----

pub struct HostState {
    ctx: WasiCtx,
    table: ResourceTable,
    registry: SurfaceRegistry,
    /// The instance this store belongs to; tags the surfaces it creates and the
    /// MIDI it sends.
    pub(crate) node_id: u64,
    /// This node's private in-memory filesystem.
    pub(crate) fs: crate::vfs::SharedFs,
    /// This node's MIDI input queue (drained by its `input` ports).
    pub(crate) midi_in: crate::midi::SharedInbox,
    /// Shared MIDI router; this node's `output` ports send through it.
    pub(crate) midi_router: crate::midi::Router,
    /// This node's option values, shared with its `Node` so the compositor can
    /// read (to save) and seed (to restore) them.
    pub(crate) options: crate::options::SharedOptions,
    /// This store's RNG, backing the standard `wasi:random` interface (needed by
    /// e.g. a guest's `HashMap`).
    random_ctx: wasmtime_wasi::random::WasiRandomCtx,
    /// This store's `wasi:http` context (outbound requests, and serving when a
    /// node exports `wasi:http/incoming-handler`).
    http_ctx: wasmtime_wasi_http::WasiHttpCtx,
    /// Shared wgpu-core instance backing the wasi:webgpu host.
    gpu: Arc<wgpu_core::global::Global>,
}

impl wasmtime_wasi_http::p2::WasiHttpView for HostState {
    fn http(&mut self) -> wasmtime_wasi_http::p2::WasiHttpCtxView<'_> {
        wasmtime_wasi_http::p2::WasiHttpCtxView {
            ctx: &mut self.http_ctx,
            table: &mut self.table,
            hooks: Default::default(),
        }
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

/// Create the shared wgpu-core instance used by the wasi:webgpu host.
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

// ---- interface-level Host markers (no free functions) ----

impl wasi::surface::surface::Host for HostState {}
impl wasi::graphics_context::graphics_context::Host for HostState {}
impl wasi::frame_buffer::frame_buffer::Host for HostState {}

// ---- wasi:surface/surface.surface ----

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
        if let Some(w) = width {
            s.width = w;
        }
        if let Some(h) = height {
            s.height = h;
        }
        s.pixels = vec![0; (s.width * s.height * 4) as usize];
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
            // Compositor closed this surface: trap to unwind and end the client.
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
        self.table.delete(rep)?;
        Ok(())
    }
}

// ---- wasi:graphics-context/graphics-context.{context, abstract-buffer} ----

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
        // frame-buffer; the compositor reads the latest buffer each frame.
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

// ---- wasi:frame-buffer/frame-buffer.{device, buffer} ----

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

// ---- the driver ----

/// Whether a component is a standard `wasi:cli/command` (exports `wasi:cli/run`)
/// rather than a wk-world guest (which exports a bare `run`).
fn component_is_command(component: &Component, engine: &Engine) -> bool {
    component
        .component_type()
        .exports(engine)
        .any(|(name, _)| name == "wasi:cli/run" || name.starts_with("wasi:cli/run@"))
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

/// Owns the wasmtime engine and spawns plugin clients on their own threads.
pub struct PluginHost {
    engine: Engine,
    gpu: Arc<wgpu_core::global::Global>,
    midi: crate::midi::Router,
}

impl PluginHost {
    pub fn new() -> Result<Self> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        // Lets the compositor stop a runaway node: increment_epoch() each frame
        // trips the per-store deadline callback, which traps on `kill`.
        config.epoch_interruption(true);
        Ok(Self {
            engine: Engine::new(&config)?,
            gpu: new_gpu_instance(),
            midi: crate::midi::new_router(),
        })
    }

    /// The shared MIDI router, so the compositor can wire MIDI connections.
    pub fn midi(&self) -> crate::midi::Router {
        self.midi.clone()
    }

    /// Advance the epoch so every running node re-checks its kill switch.
    pub fn tick_epoch(&self) {
        self.engine.increment_epoch();
    }

    /// Build a linker with every interface wk provides to a guest.
    fn build_linker(&self) -> Result<Linker<HostState>> {
        let mut linker: Linker<HostState> = Linker::new(&self.engine);
        // Provide every wasmtime-wasi interface except its filesystem, then our
        // own in-memory filesystem in its place.
        crate::vfs::add_wasi_except_fs(&mut linker)?;
        add_random(&mut linker)?;
        // WASI 0.3 (`@0.3.0`) interfaces — cli, clocks, filesystem, random,
        // sockets — built on the Component Model's native async (no `wasi:io`).
        // Added alongside the 0.2 set above (different version namespaces, no
        // clash) so a guest compiled against either WASI generation runs. p3 in
        // wasmtime-wasi is still experimental; it reuses our existing `WasiCtx`
        // (`HostState: WasiView`), so it's purely additive. Note: 0.3 guests get
        // wasmtime's real (sandboxed, no-preopen) filesystem here rather than our
        // in-memory vfs, which is 0.2-only — fine until a 0.3 guest needs files.
        wasmtime_wasi::p3::add_to_linker(&mut linker)?;
        // Only the wasi:http interfaces (outgoing-handler + types); the rest of
        // the wasi world is already linked above.
        wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker)?;
        crate::vfs::add_to_linker(&mut linker)?;
        crate::audio::add_to_linker(&mut linker)?;
        crate::midi::add_to_linker(&mut linker)?;
        crate::options::add_to_linker(&mut linker)?;
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
    /// throwaway CLI case). Blocks until `kill` is set.
    pub fn serve(
        &self,
        path: &Path,
        port: u16,
        term_io: Option<crate::terminal::SharedTermIo>,
        kill: Arc<AtomicBool>,
    ) -> Result<()> {
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
            node_id: 0,
            fs: fs.clone(),
            midi_in: midi_in.clone(),
            midi_router: midi.clone(),
            options: crate::options::new_options(Vec::new()),
            random_ctx: wasmtime_wasi::random::WasiRandomCtx::default(),
            http_ctx: wasmtime_wasi_http::WasiHttpCtx::new(),
            gpu: gpu.clone(),
        };
        let engine = self.engine.clone();
        std::thread::spawn(move || {
            if let Err(e) = crate::http::serve(engine, pre, make_state, port, kill) {
                eprintln!("http server error: {e:#}");
            }
        });
        Ok(())
    }

    /// Load a client component and run its `run` export on a dedicated thread,
    /// registering it as a `Node` under the compositor-assigned `id`. Surfaces
    /// it creates appear in `surfaces` (tagged with the node id); its
    /// stdout/stderr are captured for the node's console window.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        &self,
        path: &Path,
        name: &str,
        id: u64,
        args: &[String],
        surfaces: SurfaceRegistry,
        nodes: NodeRegistry,
        initial_options: Vec<f32>,
    ) -> Result<()> {
        let linker = self.build_linker()?;

        let finished = Arc::new(AtomicBool::new(false));
        let kill = Arc::new(AtomicBool::new(false));
        let fs = crate::vfs::new_fs();
        let midi_in = crate::midi::new_inbox();
        // Seeded with any saved values; the guest reads them via `load` at start
        // and overwrites with its current values via `store`.
        let options = crate::options::new_options(initial_options);
        let term_io = crate::terminal::TermIo::new();

        let component = Component::from_file(&self.engine, path)?;
        // A `wasi:http` server (exports incoming-handler) doesn't run a `run`
        // loop — it's served on demand when wired to a HostPort.
        let is_http = component_is_proxy(&component, &self.engine);
        // A standard `wasi:cli/command` (any `fn main` recompiled to wasm) is run
        // through its `wasi:cli/run` export; a wk-world guest through its `run`.
        let is_command = component_is_command(&component, &self.engine);

        nodes.lock().unwrap().push(Arc::new(Node {
            id,
            name: name.to_string(),
            term_io: term_io.clone(),
            fs: fs.clone(),
            midi_in: midi_in.clone(),
            options: options.clone(),
            finished: finished.clone(),
            kill: kill.clone(),
            http_path: is_http.then(|| path.to_path_buf()),
        }));

        if is_http {
            // Registered as a node; nothing runs until it's exposed on a HostPort.
            return Ok(());
        }
        // argv[0] is the program name, then the configured args (e.g. a filename).
        let mut argv = vec![name.to_string()];
        argv.extend(args.iter().cloned());
        let state = HostState {
            ctx: WasiCtxBuilder::new()
                .stdout(crate::terminal::stdout(&term_io))
                .stderr(crate::terminal::stdout(&term_io))
                .stdin(crate::terminal::stdin(&term_io))
                .args(&argv)
                .env("TERM", "xterm-256color")
                .env("COLUMNS", crate::terminal::COLS.to_string())
                .env("LINES", crate::terminal::ROWS.to_string())
                .build(),
            table: ResourceTable::new(),
            registry: surfaces,
            node_id: id,
            fs,
            midi_in,
            midi_router: self.midi.clone(),
            options,
            random_ctx: wasmtime_wasi::random::WasiRandomCtx::default(),
            http_ctx: wasmtime_wasi_http::WasiHttpCtx::new(),
            gpu: Arc::clone(&self.gpu),
        };
        let mut store = Store::new(&self.engine, state);
        // Trap the instance once it has been killed; otherwise keep running.
        store.set_epoch_deadline(1);
        let kill_cb = kill.clone();
        store.epoch_deadline_callback(move |_| {
            if kill_cb.load(Ordering::Relaxed) {
                Ok(UpdateDeadline::Interrupt)
            } else {
                Ok(UpdateDeadline::Continue(1))
            }
        });

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
}
