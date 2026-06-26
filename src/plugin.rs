//! Host side of the wk plugin system.
//!
//! wk acts as a compositor: it loads WASM component "clients" and (later) hands
//! each one a virtual surface that is composited inside an imgui window. This
//! module currently establishes the wasmtime <-> component boundary; the
//! compositor surface interfaces are layered on in subsequent steps.

use std::path::Path;
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Result, Store};
use wasmtime_wasi::{ResourceTable, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

wasmtime::component::bindgen!({
    path: "wit/plugin.wit",
    world: "plugin",
    exports: { default: async },
});

/// Per-store host state. Holds the WASI context plus the resource table that
/// backs guest-visible handles.
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

/// Build a wasmtime engine configured for async component execution.
fn engine() -> Result<Engine> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    Engine::new(&config)
}

/// Load a plugin component and call its `hello-world` export. This is a smoke
/// test of the host/guest boundary that later steps replace with the compositor
/// run loop.
pub fn run(path: &Path) -> Result<()> {
    let engine = engine()?;

    let mut linker: Linker<HostState> = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;

    let component = Component::from_file(&engine, path)?;

    let state = HostState {
        ctx: WasiCtxBuilder::new().inherit_stdio().build(),
        table: ResourceTable::new(),
    };
    let mut store = Store::new(&engine, state);

    pollster::block_on(async {
        let plugin = Plugin::instantiate_async(&mut store, &component, &linker).await?;
        let message = plugin.call_hello_world(&mut store).await?;
        println!("plugin says: {message}");
        Ok(())
    })
}
