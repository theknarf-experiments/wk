//! The world, as a node: the place you stand in is a wasm guest like any
//! other. It reads a glTF/GLB out of its own vfs — whatever you wire in, be
//! it a bind mount, a volume, a container image, or another node's `wk:fs`
//! tree — and publishes it through `wk:scene` as scenery, geometry you walk
//! through rather than click. Editing the file swaps the plaza under your
//! feet without a restart.

#[allow(warnings)]
mod bindings;

use bindings::Guest;
use bindings::wk::scene::scene::Entity;

/// How often to look at the source file for changes.
const POLL: std::time::Duration = std::time::Duration::from_millis(500);

struct Component;

impl Guest for Component {
    fn run() {
        let Some(path) = source_path() else {
            println!(
                "world: nothing to show. Wire a .glb (or .gltf) into this node — \
                 a bind mount, a volume, another node's filesystem — or pass its \
                 path as an argument."
            );
            // Stay alive: a mount can arrive after we start, and the node
            // going away would take the (empty) world with it.
            park_until(|| source_path().is_some());
            return run_with(source_path().expect("just checked"));
        };
        run_with(path)
    }
}

/// Publish `path`'s geometry as scenery and keep it in sync with the file.
fn run_with(path: String) {
    let mut bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            println!("world: cannot read {path}: {e}");
            return;
        }
    };
    println!("world: {path} — {} KiB of scenery.", bytes.len() / 1024);
    let mut ent = scenery(&bytes);
    let mut stamp = stamp_of(&path);

    loop {
        std::thread::sleep(POLL);
        // Cheap check first: only re-read when size or mtime moved. A bind
        // mount is a real file and reports both; an in-memory volume reports
        // no mtime at all, so it falls through to the byte compare below.
        let now = stamp_of(&path);
        if now.is_some() && now == stamp {
            continue;
        }
        stamp = now;
        let Ok(fresh) = std::fs::read(&path) else {
            continue; // mid-write, or the mount went away — try again later
        };
        if fresh == bytes || fresh.is_empty() {
            continue;
        }
        println!(
            "world: {path} changed — {} KiB, reloading.",
            fresh.len() / 1024
        );
        // Order matters: drop the old entity before publishing the new one,
        // so the view never holds two copies of a whole plaza at once.
        drop(ent);
        bytes = fresh;
        ent = scenery(&bytes);
    }
}

/// Hand a GLB to `wk:scene` as scenery at this node's pose.
fn scenery(glb: &[u8]) -> Entity {
    let ent = Entity::scenery(glb);
    ent.set_position(0.0, 0.0, 0.0);
    ent
}

/// (size, mtime-secs) for `path`, when the filesystem reports them.
fn stamp_of(path: &str) -> Option<(u64, u64)> {
    let m = std::fs::metadata(path).ok()?;
    let secs = m
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())?;
    Some((m.len(), secs))
}

/// The world file: the first launch argument if given, else the first glTF
/// mounted at the filesystem root — the same "whatever you wired in" rule the
/// shader node uses for its `.wgsl`.
fn source_path() -> Option<String> {
    if let Some(arg) = std::env::args().nth(1) {
        return Some(arg);
    }
    let mut any = None;
    for entry in std::fs::read_dir("/").ok()?.flatten() {
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.ends_with(".glb") || name.ends_with(".gltf") {
            return Some(format!("/{name}"));
        }
        any.get_or_insert(format!("/{name}"));
    }
    any
}

/// Block until `ready`, checking once per poll interval.
fn park_until(ready: impl Fn() -> bool) {
    while !ready() {
        std::thread::sleep(POLL);
    }
}

bindings::export!(Component with_types_in bindings);
