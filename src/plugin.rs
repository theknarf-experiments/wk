//! Host side of the wk plugin system.
//!
//! wk acts as a compositor: it loads WASM component "clients" and hands each one
//! a virtual surface that is composited inside an imgui window. For this
//! milestone the host drives the client by calling its `render` export once per
//! frame; later the client will own its own loop.

use std::path::Path;
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Result, Store};
use wasmtime_wasi::{ResourceTable, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

wasmtime::component::bindgen!({
    path: "wit/plugin.wit",
    world: "plugin",
    exports: { default: async },
});

/// Per-store host state: the WASI context plus the resource table backing
/// guest-visible handles.
struct HostState {
    ctx: WasiCtx,
    table: ResourceTable,
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

/// Owns the wasmtime engine and a linker preloaded with the host imports every
/// plugin can use. Reused to instantiate one or more plugin components.
pub struct PluginHost {
    engine: Engine,
    linker: Linker<HostState>,
}

impl PluginHost {
    pub fn new() -> Result<Self> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        let engine = Engine::new(&config)?;

        let mut linker: Linker<HostState> = Linker::new(&engine);
        wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;

        Ok(Self { engine, linker })
    }

    /// Load and instantiate a plugin component from a `.wasm` file.
    pub fn instantiate(&self, path: &Path) -> Result<PluginInstance> {
        let component = Component::from_file(&self.engine, path)?;
        let state = HostState {
            ctx: WasiCtxBuilder::new().inherit_stdio().build(),
            table: ResourceTable::new(),
        };
        let mut store = Store::new(&self.engine, state);
        let bindings = pollster::block_on(Plugin::instantiate_async(
            &mut store,
            &component,
            &self.linker,
        ))?;
        Ok(PluginInstance { store, bindings })
    }
}

/// A single instantiated plugin component.
pub struct PluginInstance {
    store: Store<HostState>,
    bindings: Plugin,
}

impl PluginInstance {
    /// Ask the plugin to paint one `width` x `height` frame; returns RGBA8 pixels.
    pub fn render(&mut self, width: u32, height: u32, time_ms: u64) -> Result<Vec<u8>> {
        pollster::block_on(
            self.bindings
                .call_render(&mut self.store, width, height, time_ms),
        )
    }
}
