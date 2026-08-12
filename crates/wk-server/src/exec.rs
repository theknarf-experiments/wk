//! Host side of `wk:exec`: running a program from inside a node.
//!
//! WASI has no `fork`/`exec`, which is why a shell in a sandbox can only be a
//! script engine (see `plugins/bash`). wk sits a level above that — the host
//! already instantiates components, which is what a node *is* and what a
//! Dockerfile `RUN` step does — so "run a program" becomes a capability the
//! host offers rather than a syscall WASI has to grow.
//!
//! The shape is `exec` without the `fork`: [`run`](wk::exec::process::Host::run)
//! reads a component out of the *calling node's own* filesystem, runs it to
//! completion sharing that filesystem, and hands back its exit status and
//! captured output. A caller composes pipelines itself by feeding one
//! program's stdout into the next one's stdin.
//!
//! Two safety properties, both enforced here:
//!
//! * **No authority gain.** The child gets the caller's filesystem and nothing
//!   else — no surfaces, no MIDI, no capture, and (for now) no network. It can
//!   therefore reach nothing the caller could not already reach.
//! * **Revocable.** Each node carries an [`ExecPermit`] the server refreshes
//!   from its capability token every tick (kind `exec`), so attenuating a
//!   token stops further `run`s within a tick — the same live-revocation the
//!   other wired capabilities have.
//!
//! Depth is bounded ([`MAX_DEPTH`]): a program run this way may itself run
//! programs, but not forever.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use wasmtime::component::{HasData, Linker};
use wasmtime::Result;

use crate::plugin::HostState;

wasmtime::component::bindgen!({
    path: "wit-exec",
    world: "exec-host",
    imports: { default: trappable },
    require_store_data_send: true,
});

pub use wk::exec::process::Output;

/// How deeply `run` may nest. A program started by `run` may start others (a
/// shell script calling a script), but the chain is bounded so a guest can't
/// spend the host's stack — the wasm equivalent of a fork bomb.
pub const MAX_DEPTH: u32 = 8;

/// Largest stdout/stderr a child may hand back (per stream). A child that
/// writes more is truncated rather than allowed to exhaust host memory.
pub const MAX_OUTPUT: usize = 64 * 1024 * 1024;

/// A node's live permission to run programs, refreshed from its capability
/// token by the server's reconciler. Shared with the node so revocation takes
/// effect on the next call, not the next restart.
pub type ExecPermit = Arc<AtomicBool>;

pub fn new_permit(allowed: bool) -> ExecPermit {
    Arc::new(AtomicBool::new(allowed))
}

/// What a store needs to serve `wk:exec`: the caller's filesystem, its permit,
/// how deep it already is, and a handle able to instantiate components.
#[derive(Clone)]
pub struct ExecCtx {
    /// The host that runs children (engine + linker); cheap handles only.
    pub host: Arc<crate::plugin::PluginHost>,
    /// Nesting depth of the *caller* (0 for a node's own guest).
    pub depth: u32,
    /// Live token decision for this node's `exec` capability.
    pub permit: ExecPermit,
}

/// Register `wk:exec` on a linker. A store without an [`ExecCtx`] still links
/// (the import resolves) but every `run` reports that exec is unavailable —
/// that is what a build step or a test harness gets.
pub fn add_to_linker(linker: &mut Linker<HostState>) -> Result<()> {
    wk::exec::process::add_to_linker::<_, ExecData>(linker, |s| s)
}

struct ExecData;
impl HasData for ExecData {
    type Data<'a> = &'a mut HostState;
}

impl wk::exec::process::Host for HostState {
    fn run(
        &mut self,
        path: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
        stdin: Vec<u8>,
    ) -> Result<std::result::Result<Output, String>> {
        let Some(ctx) = self.exec_ctx().cloned() else {
            return Ok(Err("wk:exec is not available in this context".into()));
        };
        if !ctx.permit.load(Ordering::Relaxed) {
            return Ok(Err(
                "this node's capability token does not grant exec".into()
            ));
        }
        if ctx.depth >= MAX_DEPTH {
            return Ok(Err(format!(
                "too many nested wk:exec calls (limit {MAX_DEPTH})"
            )));
        }
        // Read the program out of the caller's own filesystem, so a base image
        // that ships /bin/<tool>.wasm can run it.
        let fs = self.fs();
        let wasm = match fs.lock().unwrap().read_file(&path, usize::MAX) {
            Some(bytes) if !bytes.is_empty() => bytes,
            Some(_) => return Ok(Err(format!("{path}: empty file"))),
            None => return Ok(Err(format!("{path}: no such file"))),
        };
        if !wasm.starts_with(b"\0asm") {
            return Ok(Err(format!("{path}: not a wasm program")));
        }
        // argv[0] is the program itself, as any exec would pass it.
        let mut argv = Vec::with_capacity(args.len() + 1);
        argv.push(path.clone());
        argv.extend(args);
        Ok(ctx
            .host
            .run_program(&wasm, &argv, &env, &fs, stdin, ctx.depth + 1))
    }
}
