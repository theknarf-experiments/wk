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

use wasmtime::component::{HasData, Linker, Resource};
use wasmtime::Result;
use wasmtime_wasi_io::IoView;

use crate::execpipe::Pipe;
use crate::plugin::{HostState, Sink, Stdin};

wasmtime::component::bindgen!({
    path: "wit-exec",
    world: "exec-host",
    imports: { default: trappable },
    require_store_data_send: true,
    // The two resources are the host types themselves, held in the store's
    // table: a pipe is the shared buffer, a child is the running thread.
    with: {
        "wasi:io/error": wasmtime_wasi_io::bindings::wasi::io::error,
        "wasi:io/poll": wasmtime_wasi_io::bindings::wasi::io::poll,
        "wasi:io/streams": wasmtime_wasi_io::bindings::wasi::io::streams,
        "wk:exec/process.pipe": crate::execpipe::Pipe,
        "wk:exec/process.child": ChildHandle,
    },
});

pub use wk::exec::process::Output;
use wk::exec::process::{StdinFrom, StdoutTo};

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
/// that is what a request-scoped `wasi:http` handler or a test harness gets.
pub fn add_to_linker(linker: &mut Linker<HostState>) -> Result<()> {
    wk::exec::process::add_to_linker::<_, ExecData>(linker, |s| s)
}

struct ExecData;
impl HasData for ExecData {
    type Data<'a> = &'a mut HostState;
}

impl wk::exec::process::Host for HostState {
    fn spawn(
        &mut self,
        path: String,
        argv: Vec<String>,
        env: Vec<(String, String)>,
        stdin: StdinFrom,
        stdout: StdoutTo,
        stderr: StdoutTo,
    ) -> Result<std::result::Result<Resource<ChildHandle>, String>> {
        let (ctx, wasm) = match program_bytes(self, &path) {
            Ok(v) => v,
            Err(e) => return Ok(Err(e)),
        };
        let argv = if argv.is_empty() { vec![path] } else { argv };

        let stdin = match stdin {
            StdinFrom::Empty => Stdin::Empty,
            StdinFrom::Bytes(b) => Stdin::Bytes(b),
            StdinFrom::PipeEnd(p) => Stdin::Pipe(self.pipe_of(&p)?.reader()),
        };
        let mut sink = |s: StdoutTo| -> Result<Sink> {
            Ok(match s {
                StdoutTo::Capture => Sink::Capture,
                StdoutTo::PipeEnd(p) => Sink::Pipe(self.table().get(&p)?.clone().writer()),
            })
        };
        let (stdout, stderr) = (sink(stdout)?, sink(stderr)?);

        let fs = self.fs();
        match ctx.host.spawn_program(
            &wasm,
            &argv,
            &env,
            &fs,
            stdin,
            stdout,
            stderr,
            ctx.depth + 1,
        ) {
            Ok(child) => Ok(Ok(self.table().push(ChildHandle(Some(child)))?)),
            Err(e) => Ok(Err(e)),
        }
    }

    fn run(
        &mut self,
        path: String,
        argv: Vec<String>,
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
        // argv arrives whole, `execve`-style: the caller owns argv[0], which is
        // what multicall binaries dispatch on. An empty argv would leave the
        // child without a program name, so fall back to the path.
        let argv = if argv.is_empty() {
            vec![path.clone()]
        } else {
            argv
        };
        Ok(ctx
            .host
            .run_program(&wasm, &argv, &env, &fs, stdin, ctx.depth + 1))
    }
}

/// Everything `spawn` needs that `run` also needs: the permit, the depth
/// budget, and the program's bytes out of the caller's own filesystem.
fn program_bytes(state: &HostState, path: &str) -> std::result::Result<(ExecCtx, Vec<u8>), String> {
    let Some(ctx) = state.exec_ctx().cloned() else {
        return Err("wk:exec is not available in this context".into());
    };
    if !ctx.permit.load(Ordering::Relaxed) {
        return Err("this node's capability token does not grant exec".into());
    }
    if ctx.depth >= MAX_DEPTH {
        return Err(format!("too many nested wk:exec calls (limit {MAX_DEPTH})"));
    }
    let fs = state.fs();
    let wasm = match fs.lock().unwrap().read_file(path, usize::MAX) {
        Some(bytes) if !bytes.is_empty() => bytes,
        Some(_) => return Err(format!("{path}: empty file")),
        None => return Err(format!("{path}: no such file")),
    };
    if !wasm.starts_with(b"\0asm") {
        return Err(format!("{path}: not a wasm program"));
    }
    Ok((ctx, wasm))
}

impl wk::exec::process::HostPipe for HostState {
    fn new(&mut self) -> Result<Resource<Pipe>> {
        Ok(self.table().push(Pipe::new())?)
    }

    fn read_end(
        &mut self,
        rep: Resource<Pipe>,
    ) -> Result<Resource<wasmtime_wasi_io::streams::DynInputStream>> {
        // The stream owns a counted end, so dropping the stream is what tells
        // the pipe this reader has gone — the same bookkeeping a child's end
        // gets, and what a guest closing its fd ends up doing.
        let end = self.table().get(&rep)?.reader();
        let stream: wasmtime_wasi_io::streams::DynInputStream = Box::new(end.stream());
        Ok(self.table().push(stream)?)
    }

    fn write_end(
        &mut self,
        rep: Resource<Pipe>,
    ) -> Result<Resource<wasmtime_wasi_io::streams::DynOutputStream>> {
        let end = self.table().get(&rep)?.writer();
        let stream: wasmtime_wasi_io::streams::DynOutputStream = Box::new(end.stream());
        Ok(self.table().push(stream)?)
    }
    fn drop(&mut self, rep: Resource<Pipe>) -> Result<()> {
        self.table().delete(rep)?;
        Ok(())
    }
}

/// A child in the store's table.
///
/// `wait` is a method, so it is handed a *borrow* — but collecting a child
/// consumes it (joining a thread takes the handle). The entry therefore holds
/// an option the first `wait` empties, which is also what makes waiting twice
/// an error the guest can see rather than a panic. Reaping a process once is
/// the same rule POSIX has.
pub struct ChildHandle(Option<crate::plugin::Child>);

impl wk::exec::process::HostChild for HostState {
    fn wait(&mut self, rep: Resource<ChildHandle>) -> Result<std::result::Result<Output, String>> {
        let Some(child) = self.table().get_mut(&rep)?.0.take() else {
            return Ok(Err("this child has already been waited for".into()));
        };
        Ok(child.wait())
    }
    fn drop(&mut self, rep: Resource<ChildHandle>) -> Result<()> {
        // Dropping without waiting detaches: the thread runs on and any
        // captured output is discarded, like a shell losing interest in a job.
        self.table().delete(rep)?;
        Ok(())
    }
}

impl HostState {
    /// Mint a counted end of a borrowed pipe for a child to own. The guest's
    /// own handle stays valid; end-of-file is when *every* end has gone.
    fn pipe_of(&mut self, r: &Resource<Pipe>) -> Result<Pipe> {
        Ok(self.table().get(r)?.clone())
    }
}
