//! Host side of per-node options: a node reports its option values (e.g. a
//! synth's knob settings) via `store`, and reads back saved values via `load`.
//! The compositor persists them to the workspace session, so relaunching a node
//! restores its knobs. The host treats the values as an opaque flat list of
//! floats; the node interprets them positionally.

use std::sync::{Arc, Mutex};

use wasmtime::component::{HasData, Linker};
use wasmtime::Result;

use crate::plugin::HostState;

wasmtime::component::bindgen!({
    path: "wit-options",
    world: "options-host",
    imports: { default: trappable },
    require_store_data_send: true,
});

/// A node's option values (knob settings), shared with the compositor so it can
/// seed saved values on launch and read the current values to persist.
pub type SharedOptions = Arc<Mutex<Vec<f32>>>;

/// A fresh options slot seeded with `initial` (the saved values, or empty).
pub fn new_options(initial: Vec<f32>) -> SharedOptions {
    Arc::new(Mutex::new(initial))
}

pub fn add_to_linker(l: &mut Linker<HostState>) -> Result<()> {
    wk::options::options::add_to_linker::<_, HasOptions>(l, |s| s)?;
    Ok(())
}

struct HasOptions;
impl HasData for HasOptions {
    type Data<'a> = &'a mut HostState;
}

impl wk::options::options::Host for HostState {
    fn load(&mut self) -> Result<Vec<f32>> {
        Ok(self.options.lock().unwrap().clone())
    }

    fn store(&mut self, values: Vec<f32>) -> Result<()> {
        *self.options.lock().unwrap() = values;
        Ok(())
    }
}
