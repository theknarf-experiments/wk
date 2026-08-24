//! Booting wk on iOS.
//!
//! Two entry points onto the same checks: [`boot`] for Rust callers (the smoke
//! binary), and [`wk_ios_boot`] for an Xcode app shell to call across the C
//! ABI. What they verify, in order, is what a phone can refuse:
//!
//! 1. the wasm engine constructs — on iOS that means Pulley, since no page can
//!    be made executable, and the bounded memory reservations;
//! 2. the userspace network fabric attaches a node and resolves it;
//! 3. a real guest compiles and runs, which is the whole question — compiling
//!    is where a JIT would have been needed.

use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wk_protocol::NodeId;
use wk_server::plugin::{NodeRegistry, PluginHost, SurfaceRegistry};

/// What happened, as lines to print or show. `ok` is false if any check failed.
pub struct Report {
    pub ok: bool,
    pub lines: Vec<String>,
}

impl Report {
    fn pass(&mut self, what: impl Into<String>) {
        self.lines.push(format!("ok    {}", what.into()));
    }
    fn fail(&mut self, what: impl Into<String>) {
        self.ok = false;
        self.lines.push(format!("FAIL  {}", what.into()));
    }
}

/// Run the checks. `plugin` is a `.wasm` to compile and run; without one the
/// guest check is skipped and the rest still reports.
pub fn boot(plugin: Option<&Path>) -> Report {
    let mut r = Report {
        ok: true,
        lines: Vec::new(),
    };
    r.lines.push(format!(
        "wk on {} ({}), guest backend {}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        if cfg!(feature = "pulley") {
            "pulley (interpreted)"
        } else {
            "native codegen"
        }
    ));

    let host = match PluginHost::new() {
        Ok(h) => {
            r.pass("wasm engine constructed");
            h
        }
        Err(e) => {
            r.fail(format!("wasm engine: {e}"));
            return r;
        }
    };

    // The fabric is pure userspace, so it should not care what it runs on —
    // but it is the other half of wk, and cheap to prove. Its own hub rather
    // than the host's, which is deliberately not public.
    let hub = wk_fabric::netstack::NetHub::new();
    let net = NodeId::new();
    let _stack = hub.attach(net, hub.alloc_ip(2), "ios");
    match hub.resolve(net, "ios") {
        Some(ip) => r.pass(format!("fabric attached and resolved (ios = {ip})")),
        None => r.fail("fabric attached but the name did not resolve"),
    }

    let Some(plugin) = plugin else {
        r.lines
            .push("skip  no guest given, so nothing was compiled or run".into());
        return r;
    };
    if !plugin.exists() {
        r.fail(format!("no such guest: {}", plugin.display()));
        return r;
    }

    let nodes: NodeRegistry = Arc::new(Mutex::new(Vec::new()));
    let surfaces: SurfaceRegistry = Arc::new(Mutex::new(Vec::new()));
    let id = NodeId::new();
    let name = plugin
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "guest".into());
    if let Err(e) = host.spawn(
        plugin,
        &name,
        id,
        &[],
        surfaces,
        nodes.clone(),
        Vec::new(),
        None,
    ) {
        r.fail(format!("spawn {name}: {e}"));
        return r;
    }

    // Compilation happens on a background thread; on the interpreter it is
    // also the slowest step, so wait generously.
    let started = Instant::now();
    let deadline = started + Duration::from_secs(180);
    let node = loop {
        let found = nodes.lock().unwrap().iter().find(|n| n.id == id).cloned();
        match found {
            Some(n) if n.is_runnable() => break Some(n),
            Some(n) if n.finished.load(Ordering::Relaxed) => break Some(n),
            _ if Instant::now() >= deadline => break None,
            _ => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    let Some(node) = node else {
        r.fail(format!("{name} never compiled (180s)"));
        return r;
    };
    if !node.is_runnable() {
        r.fail(format!("{name} exited during load"));
        return r;
    }
    r.pass(format!("guest compiled: {name} in {:?}", started.elapsed()));

    if let Err(e) = host.run_node(&node, &[]) {
        r.fail(format!("run {name}: {e}"));
        return r;
    }
    // A guest that is still running, or that ran and exited cleanly, both mean
    // the interpreter executed its code.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if node.running.load(Ordering::Relaxed) || node.finished.load(Ordering::Relaxed) {
            r.pass(format!("guest executed: {name}"));
            break;
        }
        if Instant::now() >= deadline {
            r.fail(format!("{name} never started"));
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    node.kill.store(true, Ordering::Relaxed);
    r
}

/// The C entry point an app shell calls. Prints the report (visible in the
/// device/simulator log) and returns 0 when every check passed.
///
/// # Safety
/// `plugin` must be a NUL-terminated C string or null.
#[no_mangle]
pub unsafe extern "C" fn wk_ios_boot(plugin: *const std::os::raw::c_char) -> i32 {
    let path = if plugin.is_null() {
        None
    } else {
        std::ffi::CStr::from_ptr(plugin)
            .to_str()
            .ok()
            .map(Path::new)
    };
    let report = boot(path);
    for line in &report.lines {
        println!("[wk] {line}");
    }
    i32::from(!report.ok)
}
