//! Per-node in-memory filesystem: wk implements `wasi:filesystem` itself
//! (instead of wasmtime-wasi's cap-std one) so each app node sees its own
//! sandboxed, in-RAM filesystem. Nothing touches the host disk and nodes are
//! isolated from each other (Docker-like). The only shared state is a "file
//! node" on the canvas explicitly *connected* to an app: it appears as a shared
//! file in that app's filesystem (see `mount_file`), so wiring one file node to
//! two apps lets them talk through it.
//!
//! On top of the private tree sit **immutable layers** ([`layers`]): OCI image
//! content applied as `Arc`-shared read-only files with file-granularity
//! copy-on-write (overlayfs semantics), so N nodes running one image store its
//! bytes once. The host embeds the filesystem by implementing [`VfsView`] and
//! calling [`add_to_linker`].

pub mod layers;
pub mod p3;
pub mod provider;

pub use provider::{
    FsDirent, FsEntryKind, FsError, FsOp, FsOpened, FsReplyData, FsStat, ProviderConn,
};

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};

use wasmtime::component::{HasData, Linker, Resource, ResourceTable};
use wasmtime::Result;
use wasmtime_wasi::WasiView;
use wasmtime_wasi_io::async_trait;
use wasmtime_wasi_io::bytes::Bytes;
use wasmtime_wasi_io::poll::Pollable;
use wasmtime_wasi_io::streams::{
    DynInputStream, DynOutputStream, InputStream, OutputStream, StreamError,
};
use wasmtime_wasi_io::IoView;

wasmtime::component::bindgen!({
    path: "wit",
    world: "fs-host",
    imports: { default: trappable },
    require_store_data_send: true,
    with: {
        // Our files' read/write streams ARE wasmtime-wasi's io streams, so the
        // guest's wasi:io/streams (provided by wasmtime-wasi) can read them.
        "wasi:io/error": wasmtime_wasi_io::bindings::wasi::io::error,
        "wasi:io/poll": wasmtime_wasi_io::bindings::wasi::io::poll,
        "wasi:io/streams": wasmtime_wasi_io::bindings::wasi::io::streams,
        "wasi:filesystem/types.descriptor": Descriptor,
        "wasi:filesystem/types.directory-entry-stream": DirEntryStream,
    },
});

/// What the embedder's store must provide for this crate's `wasi:filesystem`
/// implementation: the resource table (via [`IoView`]) and the filesystem the
/// guest should see.
pub trait VfsView: IoView {
    /// The filesystem this store's guest sees (an `Arc` handle).
    fn fs(&mut self) -> SharedFs;
}

impl<T: VfsView + ?Sized> VfsView for &mut T {
    fn fs(&mut self) -> SharedFs {
        T::fs(self)
    }
}

/// Adapter carrying the generated `wasi:filesystem` trait impls for any
/// [`VfsView`] store — a local wrapper, so the impls can't collide with the
/// generated blanket `&mut T` ones (the same shape as wasmtime-wasi's
/// `WasiImpl`).
#[repr(transparent)]
pub struct VfsImpl<T>(pub T);

impl<T: VfsView> IoView for VfsImpl<T> {
    fn table(&mut self) -> &mut ResourceTable {
        self.0.table()
    }
}
impl<T: VfsView> VfsView for VfsImpl<T> {
    fn fs(&mut self) -> SharedFs {
        self.0.fs()
    }
}

use wasi::clocks::wall_clock::Datetime;
use wasi::filesystem::types::{
    Advice, DescriptorFlags, DescriptorStat, DescriptorType, DirectoryEntry, ErrorCode, Filesize,
    MetadataHashValue, NewTimestamp, OpenFlags, PathFlags,
};

/// The bytes of a canvas "file node", shared by every app it is connected to.
pub type SharedFile = Arc<Mutex<Vec<u8>>>;

/// How a path exists in an `Fs`: provenance for build-time diffs (see
/// [`Fs::snapshot`]) and for the UI's file inspector (layer vs written vs
/// mounted badges).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathKind {
    Dir,
    /// An immutable layer file (`RoFile`) — untouched since its layer applied.
    LayerFile,
    /// A privately written file: created or copied-up since the last layer.
    PrivateFile,
    /// A canvas file mount (shared/host) — not part of any image.
    Mounted,
    /// A symbolic link, and what it points at.
    ///
    /// The target is part of the kind because it is the only thing that says
    /// whether a link changed: unlike a file, which a layer turns into a
    /// `RoFile` so that a private one is known to be a fresh write, a link is
    /// a `Symlink` whether it came from a layer or was just created.
    Symlink(String),
}

enum Node {
    File(Vec<u8>),
    Dir(BTreeMap<String, u64>),
    /// A file whose bytes live in a canvas Volume node connected to this
    /// app (in-memory, shared between connected apps).
    Shared(SharedFile),
    /// A file backed by a real file on the host disk (a canvas HostMappedFile
    /// node connected to this app): reads and writes hit the actual path, so
    /// they persist and are visible to the host.
    Host(std::path::PathBuf),
    /// An immutable file from a filesystem layer (an OCI image layer or a local
    /// layer source). The bytes are `Arc`-shared with every other node running
    /// the same layer and never mutated: a write first replaces this with a
    /// private [`Node::File`] copy (file-granularity copy-up, like overlayfs —
    /// see [`Fs::copy_up`]). A layer indexed from the on-disk store holds a
    /// lazy [`layers::LayerBytes`]: stat and listing use the header length,
    /// and the content loads from disk on the first read (then stays, shared).
    RoFile(Arc<layers::LayerBytes>),
    /// A symbolic link: the stored string is the target path, resolved when a
    /// path walk crosses it (or returned verbatim by `readlink`). Real links
    /// matter beyond POSIX fidelity — a multicall binary like busybox or GNU
    /// coreutils provides its hundred command names *as symlinks* to one
    /// executable, and plenty of OCI images are built the same way.
    Symlink(String),
    /// The null device (`/dev/null`): every write is discarded, every read is
    /// end-of-file. Provisioned into each container so the ubiquitous
    /// `2>/dev/null` / `>/dev/null` / `</dev/null` work without a growing file.
    Null,
    /// The zero device (`/dev/zero`): writes discarded, reads return endless
    /// `\0` bytes (`head -c N /dev/zero`, `dd if=/dev/zero`).
    Zero,
    /// The random devices (`/dev/urandom`, `/dev/random`): writes discarded,
    /// reads return endless OS-random bytes (`head -c 32 /dev/urandom`).
    Random,
    /// A provider mount: the root of a subtree served live by another node's
    /// program (wk's FUSE). A path walk stops here and every operation on the
    /// residual path is forwarded over the [`ProviderConn`] to that node's
    /// serve loop — lookup, readdir, create and all; nothing under this point
    /// exists in this `Fs`.
    Provider(Arc<ProviderConn>),
}

const ROOT: u64 = 0;

/// One app node's in-memory filesystem.
pub struct Fs {
    nodes: BTreeMap<u64, Node>,
    next: u64,
    /// Node ids mounted read-only: every write path (streams, direct writes,
    /// truncate, resize, unlink, rename) refuses them with `NotPermitted` —
    /// how a token that grants `read` but not `write` on a file is enforced.
    /// Ids are never reused, so stale entries after an unmount are inert.
    readonly: HashSet<u64>,
    /// Number of open descriptors per node id. A node whose last directory
    /// entry is unlinked while a descriptor is still open must survive (POSIX
    /// unlinked-but-open files): `tac`/`sort` mkstemp a scratch file, unlink it
    /// immediately, then keep writing and seeking the open fd. Freed only when
    /// both the link count (dir entries) and this count reach zero.
    open_fds: HashMap<u64, u32>,
}

impl Default for Fs {
    fn default() -> Self {
        let mut nodes = BTreeMap::new();
        nodes.insert(ROOT, Node::Dir(BTreeMap::new()));
        Fs {
            nodes,
            next: 1,
            readonly: HashSet::new(),
            open_fds: HashMap::new(),
        }
    }
}

/// Largest number of nodes (files + directories) one app's in-memory fs may
/// hold. Bounds host memory: a guest can otherwise `open`/`mkdir` in a loop and
/// allocate unbounded entries (each a file up to [`MAX_FILE_SIZE`]).
const MAX_FS_NODES: usize = 100_000;

impl Fs {
    /// Whether the fs is at its node cap, so a create must be refused.
    fn at_capacity(&self) -> bool {
        self.nodes.len() >= MAX_FS_NODES
    }

    fn alloc(&mut self, node: Node) -> u64 {
        let id = self.next;
        self.next += 1;
        self.nodes.insert(id, node);
        id
    }

    /// Register an opened descriptor against `id` (keeps the node alive even if
    /// its last directory entry is later unlinked).
    fn open_ref(&mut self, id: u64) {
        *self.open_fds.entry(id).or_insert(0) += 1;
    }

    /// Drop an opened descriptor. When the last one closes and no directory
    /// entry names the node any more, its content is finally freed — the moment
    /// a POSIX unlinked-but-open file actually disappears.
    fn close_ref(&mut self, id: u64) {
        if let Some(count) = self.open_fds.get_mut(&id) {
            *count -= 1;
            if *count == 0 {
                self.open_fds.remove(&id);
                if id != ROOT && !node_is_referenced(self, id) {
                    self.drop_subtree(id);
                }
            }
        }
    }

    fn open_count(&self, id: u64) -> u32 {
        self.open_fds.get(&id).copied().unwrap_or(0)
    }

    /// Insert `node` as a child named `name` under `parent` (a directory).
    /// Insert `node` as a child of directory `parent`. Only tests build trees
    /// this directly now — mounts go through the path-based [`mount_file`].
    #[cfg(test)]
    fn add_child(&mut self, parent: u64, name: &str, node: Node) {
        let id = self.alloc(node);
        if let Some(Node::Dir(children)) = self.nodes.get_mut(&parent) {
            children.insert(name.to_string(), id);
        }
    }

    /// If `id` is a read-only layer file, replace it in place with a private
    /// mutable copy of its bytes (file-granularity copy-up, like overlayfs).
    /// The shared layer bytes are untouched; every write path calls this first.
    fn copy_up(&mut self, id: u64) {
        if let Some(Node::RoFile(bytes)) = self.nodes.get(&id) {
            // `bytes()` materializes a lazy layer file first — the private
            // copy needs the real content, and later readers of the layer
            // share the materialization anyway.
            let private = bytes.bytes().as_ref().clone();
            self.nodes.insert(id, Node::File(private));
        }
    }

    // ---- path-level helpers for applying filesystem layers (crate::layers) ----

    /// Resolve the directory at `path`, creating missing components. `None` if a
    /// component already exists as a file, or the fs is at capacity.
    pub fn ensure_dir_path(&mut self, path: &str) -> Option<u64> {
        let mut cur = ROOT;
        for comp in components(path) {
            let existing = match self.nodes.get(&cur) {
                Some(Node::Dir(children)) => children.get(comp).copied(),
                _ => return None,
            };
            cur = match existing {
                Some(id) => match self.nodes.get(&id) {
                    Some(Node::Dir(_)) => id,
                    _ => return None,
                },
                None => {
                    if self.at_capacity() {
                        return None;
                    }
                    let id = self.alloc(Node::Dir(BTreeMap::new()));
                    if let Some(Node::Dir(children)) = self.nodes.get_mut(&cur) {
                        children.insert(comp.to_string(), id);
                    }
                    id
                }
            };
        }
        Some(cur)
    }

    /// Place a shared read-only layer file at `path`, creating parent
    /// directories and replacing any existing entry (a later layer wins).
    pub fn put_ro_file_at(&mut self, path: &str, bytes: Arc<layers::LayerBytes>) {
        let comps = components(path);
        let Some((name, dirs)) = comps.split_last() else {
            return;
        };
        let Some(parent) = self.ensure_dir_path(&dirs.join("/")) else {
            return;
        };
        if self.at_capacity() {
            return;
        }
        self.remove_path_in(parent, name);
        let id = self.alloc(Node::RoFile(bytes));
        if let Some(Node::Dir(children)) = self.nodes.get_mut(&parent) {
            children.insert((*name).to_string(), id);
        }
    }

    /// Remove the entry at `path` (recursively for a directory). A missing path
    /// is a no-op — an OCI whiteout may target something no layer provided.
    pub fn remove_path(&mut self, path: &str) {
        let comps = components(path);
        let Some((name, dirs)) = comps.split_last() else {
            return;
        };
        if let Some(parent) = resolve(self, ROOT, &dirs.join("/")) {
            self.remove_path_in(parent, name);
        }
    }

    /// Remove every child of the directory at `path` (an OCI opaque marker).
    pub fn clear_dir_at(&mut self, path: &str) {
        if let Some(id) = resolve(self, ROOT, path) {
            let children: Vec<u64> = match self.nodes.get_mut(&id) {
                Some(Node::Dir(c)) => {
                    let ids = c.values().copied().collect();
                    c.clear();
                    ids
                }
                _ => return,
            };
            for child in children {
                self.drop_subtree(child);
            }
        }
    }

    /// Place a *private* (mutable) file at `path`, creating parents and
    /// replacing any existing entry — the host-side twin of a guest write
    /// (used by build tooling and tests to seed a rootfs).
    pub fn put_file_at(&mut self, path: &str, bytes: Vec<u8>) {
        let comps = components(path);
        let Some((name, dirs)) = comps.split_last() else {
            return;
        };
        let Some(parent) = self.ensure_dir_path(&dirs.join("/")) else {
            return;
        };
        if self.at_capacity() {
            return;
        }
        self.remove_path_in(parent, name);
        let id = self.alloc(Node::File(bytes));
        if let Some(Node::Dir(children)) = self.nodes.get_mut(&parent) {
            children.insert((*name).to_string(), id);
        }
    }

    /// Every path in the filesystem (no leading `/`; the root itself omitted)
    /// classified for build-time diffs: layer files vs privately written files
    /// vs directories vs canvas mounts. See `crate::images`'s RUN capture.
    /// The target of `path` if it is a symbolic link, without following it.
    ///
    /// [`snapshot`](Self::snapshot) reports a link as a file, because to a
    /// build layer it is content either way; a caller that has to *carry* the
    /// link — copying between build stages, say — needs to tell them apart, or
    /// `/bin/ls -> coreutils.wasm` becomes another whole copy of coreutils.
    pub fn read_symlink(&self, path: &str) -> Option<String> {
        let id = resolve_at(self, ROOT, path.trim_start_matches('/'), false)?;
        match self.nodes.get(&id) {
            Some(Node::Symlink(target)) => Some(target.clone()),
            _ => None,
        }
    }

    pub fn snapshot(&self) -> BTreeMap<String, PathKind> {
        let mut out = BTreeMap::new();
        fn walk(fs: &Fs, dir: u64, prefix: &str, out: &mut BTreeMap<String, PathKind>) {
            let Some(Node::Dir(children)) = fs.nodes.get(&dir) else {
                return;
            };
            for (name, &id) in children {
                let path = if prefix.is_empty() {
                    name.clone()
                } else {
                    format!("{prefix}/{name}")
                };
                let kind = match fs.nodes.get(&id) {
                    Some(Node::Dir(_)) => PathKind::Dir,
                    Some(Node::RoFile(_)) => PathKind::LayerFile,
                    Some(Node::File(_)) => PathKind::PrivateFile,
                    Some(Node::Shared(_) | Node::Host(_)) => PathKind::Mounted,
                    Some(Node::Symlink(target)) => PathKind::Symlink(target.clone()),
                    // Device nodes and provider mounts are provisioned at
                    // runtime, not build content — keep them out of layer diffs.
                    Some(Node::Null | Node::Zero | Node::Random | Node::Provider(_)) => continue,
                    None => continue,
                };
                let is_dir = kind == PathKind::Dir;
                out.insert(path.clone(), kind);
                if is_dir {
                    walk(fs, id, &path, out);
                }
            }
        }
        walk(self, ROOT, "", &mut out);
        out
    }

    /// Detach child `name` of `parent` and drop its whole subtree.
    fn remove_path_in(&mut self, parent: u64, name: &str) {
        let removed = match self.nodes.get_mut(&parent) {
            Some(Node::Dir(children)) => children.remove(name),
            _ => None,
        };
        if let Some(id) = removed {
            self.drop_subtree(id);
        }
    }

    /// Drop `id` and, if it is a directory, everything under it.
    fn drop_subtree(&mut self, id: u64) {
        if let Some(Node::Dir(children)) = self.nodes.remove(&id) {
            for (_, child) in children {
                self.drop_subtree(child);
            }
        }
    }
}

pub type SharedFs = Arc<Mutex<Fs>>;

/// A fresh, empty filesystem for a new app node.
pub fn new_fs() -> SharedFs {
    Arc::new(Mutex::new(Fs::default()))
}

/// One entry in a directory listing, for read-only UI inspection.
#[derive(Clone, Debug, PartialEq)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    /// File byte length (0 for a directory). For a host-mapped file this is the
    /// on-disk length; for a shared file, the connected node's current bytes.
    pub size: usize,
    /// Where the entry comes from: an image layer, a private write, or a
    /// canvas mount — shown as a badge in the file inspector.
    pub origin: PathKind,
}

impl Fs {
    /// Byte length of the file node `id` (0 for a directory or missing node).
    fn file_len(&self, id: u64) -> usize {
        match self.nodes.get(&id) {
            Some(Node::File(d)) => d.len(),
            Some(Node::RoFile(d)) => d.len(),
            Some(Node::Shared(sh)) => sh.lock().unwrap().len(),
            Some(Node::Host(p)) => std::fs::metadata(p).map(|m| m.len() as usize).unwrap_or(0),
            _ => 0,
        }
    }

    /// List the entries directly under directory `path` (root = `""` or `"/"`),
    /// directories first then files, each group sorted by name. `None` if the
    /// path doesn't resolve to a directory. Read-only; for UI inspection.
    pub fn list_dir(&self, path: &str) -> Option<Vec<DirEntry>> {
        let id = resolve(self, ROOT, path)?;
        let Node::Dir(children) = self.nodes.get(&id)? else {
            return None;
        };
        let mut out: Vec<DirEntry> = children
            .iter()
            .map(|(name, &cid)| {
                let origin = match self.nodes.get(&cid) {
                    Some(Node::Dir(_)) => PathKind::Dir,
                    Some(Node::RoFile(_)) => PathKind::LayerFile,
                    Some(Node::Shared(_) | Node::Host(_) | Node::Provider(_)) => PathKind::Mounted,
                    _ => PathKind::PrivateFile,
                };
                let is_dir = origin == PathKind::Dir
                    || matches!(self.nodes.get(&cid), Some(Node::Provider(_)));
                DirEntry {
                    name: name.clone(),
                    is_dir,
                    size: self.file_len(cid),
                    origin,
                }
            })
            .collect();
        out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
        Some(out)
    }

    /// Place a symlink at `path` pointing at `target`, creating parents and
    /// replacing any existing entry (how a layer materialises its links).
    pub fn put_symlink_at(&mut self, path: &str, target: String) {
        let comps = components(path);
        let Some((name, dirs)) = comps.split_last() else {
            return;
        };
        let Some(parent) = self.ensure_dir_path(&dirs.join("/")) else {
            return;
        };
        if self.at_capacity() {
            return;
        }
        self.remove_path_in(parent, name);
        let id = self.alloc(Node::Symlink(target));
        if let Some(Node::Dir(children)) = self.nodes.get_mut(&parent) {
            children.insert((*name).to_string(), id);
        }
    }

    /// Read up to `cap` bytes of the file at `path` for preview, or `None` if it
    /// isn't a file. Read-only; a host-mapped file is read from disk.
    pub fn read_file(&self, path: &str, cap: usize) -> Option<Vec<u8>> {
        let id = resolve(self, ROOT, path)?;
        match self.nodes.get(&id)? {
            Node::File(d) => Some(d.iter().take(cap).copied().collect()),
            // A preview is a read: a lazy layer file materializes here.
            Node::RoFile(d) => Some(d.bytes().iter().take(cap).copied().collect()),
            Node::Shared(sh) => Some(sh.lock().unwrap().iter().take(cap).copied().collect()),
            // resolve() follows links, so reaching one here means it dangles.
            Node::Symlink(_) => None,
            Node::Host(p) => {
                use std::io::Read;
                let mut f = std::fs::File::open(p).ok()?;
                let mut buf = vec![0u8; cap];
                let n = f.read(&mut buf).ok()?;
                buf.truncate(n);
                Some(buf)
            }
            Node::Dir(_) => None,
            Node::Null | Node::Zero | Node::Random => Some(Vec::new()),
            // Previewing inside a provider would block the UI on a guest;
            // the file inspector shows the mount point, not its contents.
            Node::Provider(_) => None,
        }
    }
}

/// Bind-mount a Volume's shared bytes into `fs` at `at` (an absolute-ish path
/// like `/data/notes.txt`; a bare name mounts at the root). Missing parent
/// directories are created; any existing entry at that path is replaced.
/// `writable = false` mounts read-only: reads see the live shared bytes but
/// every mutation (write, truncate, resize, unlink, rename) is refused.
pub fn mount_file(fs: &SharedFs, at: &str, data: SharedFile, writable: bool) {
    mount_node_at(fs, at, Node::Shared(data), !writable);
}

/// Bind-mount a real host file into `fs` at `at`, backed by the disk file at
/// `path`. Reads (and, if `writable`, writes) go straight to disk. Path
/// semantics as [`mount_file`].
pub fn mount_host_file(fs: &SharedFs, at: &str, path: std::path::PathBuf, writable: bool) {
    mount_node_at(fs, at, Node::Host(path), !writable);
}

/// Bind a real host path into `fs` at `at`: a file mounts as one host-backed
/// file; a directory is mirrored — each file inside it mounts as a host-backed
/// file at the matching sub-path, so existing files stay live (reads and writes
/// hit disk). The tree is a snapshot taken at mount time: files the host adds
/// afterwards don't appear until the next mount, and files the guest creates
/// inside a bound directory are private (in-memory), not written back to disk.
/// Symlinks are not followed (so a directory cycle can't loop the walk).
pub fn mount_host(fs: &SharedFs, at: &str, path: std::path::PathBuf, writable: bool) {
    if path.is_dir() {
        mount_host_dir(fs, at, &path, writable);
    } else {
        mount_host_file(fs, at, path, writable);
    }
}

/// Mirror the host directory `dir` into `fs` under `at` (see [`mount_host`]).
fn mount_host_dir(fs: &SharedFs, at: &str, dir: &std::path::Path, writable: bool) {
    fs.lock().unwrap().ensure_dir_path(at);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let child_at = format!("{at}/{}", entry.file_name().to_string_lossy());
        if is_dir {
            mount_host_dir(fs, &child_at, &entry.path(), writable);
        } else {
            // Regular files (and, deliberately, symlinks treated as their target
            // file) mount live; a broken link just reads empty.
            mount_host_file(fs, &child_at, entry.path(), writable);
        }
    }
}

/// Place `node` at `at`, creating parent dirs and replacing any existing entry.
fn mount_node_at(fs: &SharedFs, at: &str, node: Node, readonly: bool) {
    let mut g = fs.lock().unwrap();
    let comps = components(at);
    let Some((name, dirs)) = comps.split_last() else {
        return; // empty path: nothing to mount
    };
    let Some(parent) = g.ensure_dir_path(&dirs.join("/")) else {
        return; // a path component collided with a file
    };
    if g.at_capacity() {
        return;
    }
    g.remove_path_in(parent, name);
    let id = g.alloc(node);
    if readonly {
        g.readonly.insert(id);
    }
    if let Some(Node::Dir(children)) = g.nodes.get_mut(&parent) {
        children.insert((*name).to_string(), id);
    }
}

/// Mount another node's served filesystem into `fs` at `at` (wk's FUSE): the
/// subtree under `at` is answered live by that node's `wk:fs` serve loop via
/// `conn`. Path semantics as [`mount_file`]. `writable = false` refuses every
/// mutation host-side before it reaches the provider.
pub fn mount_provider(fs: &SharedFs, at: &str, conn: Arc<ProviderConn>, writable: bool) {
    mount_node_at(fs, at, Node::Provider(conn), !writable);
}

/// Provision the standard device files a container expects: `/dev/null`,
/// `/dev/zero`, `/dev/urandom`, `/dev/random`. Idempotent per name — an entry
/// already at that path (e.g. from an image layer) is left alone. Call once
/// after an image's layers are mounted so the ubiquitous `2>/dev/null`,
/// `head -c N /dev/urandom`, `dd if=/dev/zero`, … work.
pub fn ensure_standard_devices(fs: &SharedFs) {
    let mut g = fs.lock().unwrap();
    let Some(dev) = g.ensure_dir_path("dev") else {
        return;
    };
    for (name, kind) in [
        ("null", Node::Null),
        ("zero", Node::Zero),
        ("urandom", Node::Random),
        ("random", Node::Random),
    ] {
        let present = matches!(g.nodes.get(&dev), Some(Node::Dir(c)) if c.contains_key(name));
        if present || g.at_capacity() {
            continue;
        }
        let id = g.alloc(kind);
        if let Some(Node::Dir(children)) = g.nodes.get_mut(&dev) {
            children.insert(name.to_string(), id);
        }
    }
}

/// Whether the node behind `id` was mounted read-only.
fn is_readonly(fs: &SharedFs, id: u64) -> bool {
    fs.lock().unwrap().readonly.contains(&id)
}

/// Disconnect a bind mounted at `at` from `fs` (leaves the volume's bytes intact
/// for any other app still connected). Parent directories created for the mount
/// are left in place — harmless empty dirs.
pub fn unmount_file(fs: &SharedFs, at: &str) {
    fs.lock().unwrap().remove_path(at);
}

/// Whether `path` lands on or inside a provider mount — i.e. listing or
/// reading it means asking the serving node. Cheap (a local walk, no
/// provider call): the check a UI uses to route a browse through its
/// background fetcher instead of the render thread.
pub fn path_crosses_provider(fs: &SharedFs, path: &str) -> bool {
    let g = fs.lock().unwrap();
    matches!(resolve_place(&g, ROOT, path, true), Resolved::Remote { .. })
}

/// How many of a forwarded listing's files get a per-entry `getattr` for
/// their size (each is one more provider round trip; a huge listing
/// shouldn't fan out). Entries past the cap show size 0.
const FORWARDED_SIZES: usize = 64;

/// [`Fs::list_dir`] that crosses provider mounts: a path reaching a provider
/// is answered by the serving node — one forwarded readdir, plus a getattr
/// per file (capped) for sizes. BLOCKS up to the provider-call timeout, so
/// this is for background threads (the file inspector fetches through it
/// off-thread and caches), never a render loop.
pub fn list_dir_forwarded(fs: &SharedFs, path: &str) -> Option<Vec<DirEntry>> {
    let target = {
        let g = fs.lock().unwrap();
        match resolve_place(&g, ROOT, path, true) {
            Resolved::Remote { conn, path, .. } => Some((conn, path)),
            Resolved::Local(_) => None,
            Resolved::Missing => return None,
        }
    };
    let Some((conn, rpath)) = target else {
        return fs.lock().unwrap().list_dir(path);
    };
    let entries = match conn
        .call(FsOp::Readdir {
            path: rpath.clone(),
        })
        .ok()?
    {
        FsReplyData::Entries(list) => list,
        _ => return None,
    };
    let mut out: Vec<DirEntry> = entries
        .into_iter()
        .map(|d| DirEntry {
            is_dir: d.kind == FsEntryKind::Dir,
            size: 0,
            // Directories render plain; files keep the mount badge — they
            // ARE served content, and the badge says so.
            origin: if d.kind == FsEntryKind::Dir {
                PathKind::Dir
            } else {
                PathKind::Mounted
            },
            name: d.name,
        })
        .collect();
    for e in out.iter_mut().filter(|e| !e.is_dir).take(FORWARDED_SIZES) {
        let p = if rpath.is_empty() {
            e.name.clone()
        } else {
            format!("{rpath}/{}", e.name)
        };
        if let Ok(FsReplyData::Attr(st)) = conn.call(FsOp::Getattr { path: p }) {
            e.size = st.size as usize;
        }
    }
    out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
    Some(out)
}

/// [`Fs::read_file`] (bounded preview) that crosses provider mounts: open,
/// read up to `cap`, release. Blocks like [`list_dir_forwarded`] — for
/// background threads only.
pub fn read_file_forwarded(fs: &SharedFs, path: &str, cap: usize) -> Option<Vec<u8>> {
    let target = {
        let g = fs.lock().unwrap();
        match resolve_place(&g, ROOT, path, true) {
            Resolved::Remote { conn, path, .. } => Some((conn, path)),
            Resolved::Local(_) => None,
            Resolved::Missing => return None,
        }
    };
    let Some((conn, rpath)) = target else {
        return fs.lock().unwrap().read_file(path, cap);
    };
    let opened = match conn
        .call(FsOp::Open {
            path: rpath,
            create: false,
            truncate: false,
            exclusive: false,
        })
        .ok()?
    {
        FsReplyData::Opened(o) => o,
        _ => return None,
    };
    if opened.kind == FsEntryKind::Dir {
        conn.cast(FsOp::Release {
            handle: opened.handle,
        });
        return None;
    }
    let mut out = Vec::new();
    while out.len() < cap {
        let want = (cap - out.len()).min(FILE_READ_CHUNK) as u32;
        match conn.call(FsOp::Read {
            handle: opened.handle,
            offset: out.len() as u64,
            len: want,
        }) {
            Ok(FsReplyData::Data { bytes, eof }) => {
                let done = bytes.is_empty() || eof;
                out.extend_from_slice(&bytes);
                if done {
                    break;
                }
            }
            _ => break,
        }
    }
    conn.cast(FsOp::Release {
        handle: opened.handle,
    });
    out.truncate(cap);
    Some(out)
}

/// Split a path into normal components (ignoring empty and `.`).
fn components(path: &str) -> Vec<&str> {
    path.split('/')
        .filter(|c| !c.is_empty() && *c != ".")
        .collect()
}

/// Resolve an existing node from `start` following `path`.
/// How many symlinks one path walk may cross before giving up — POSIX's
/// ELOOP guard, so a link cycle (`a -> b`, `b -> a`) terminates.
const MAX_SYMLINK_HOPS: usize = 32;

/// Resolve `path` from `start`, following symlinks (POSIX path resolution).
/// Intermediate components are always followed; the final component is
/// followed only if `follow_final` — that is the difference between `stat`
/// and `lstat`, and between opening a link's target and the link itself.
fn resolve_at(fs: &Fs, start: u64, path: &str, follow_final: bool) -> Option<u64> {
    let mut hops = 0usize;
    // A worklist of components still to walk, so an expanded link can push its
    // own components back on.
    let mut todo: Vec<String> = components(path)
        .into_iter()
        .rev()
        .map(str::to_string)
        .collect();
    let mut cur = if path.starts_with('/') { ROOT } else { start };
    while let Some(comp) = todo.pop() {
        let next = match fs.nodes.get(&cur)? {
            Node::Dir(children) => *children.get(&comp)?,
            // A non-directory in the middle of a path is an error.
            _ => return None,
        };
        let is_final = todo.is_empty();
        match fs.nodes.get(&next) {
            Some(Node::Symlink(target)) if !is_final || follow_final => {
                hops += 1;
                if hops > MAX_SYMLINK_HOPS {
                    return None; // ELOOP
                }
                if target.starts_with('/') {
                    cur = ROOT;
                }
                // The link's own components run before whatever is left.
                for c in components(target).into_iter().rev() {
                    todo.push(c.to_string());
                }
            }
            Some(_) => cur = next,
            None => return None,
        }
    }
    Some(cur)
}

/// Resolve an existing node, following symlinks all the way (the common case).
fn resolve(fs: &Fs, start: u64, path: &str) -> Option<u64> {
    resolve_at(fs, start, path, true)
}

/// Resolve the parent directory of the last component of `path`.
fn resolve_parent(fs: &Fs, start: u64, path: &str) -> Option<(u64, String)> {
    let comps = components(path);
    let (name, dirs) = comps.split_last()?;
    let parent = resolve(fs, start, &dirs.join("/"))?;
    matches!(fs.nodes.get(&parent), Some(Node::Dir(_))).then(|| (parent, (*name).to_string()))
}

fn node_type(fs: &Fs, id: u64) -> DescriptorType {
    match fs.nodes.get(&id) {
        Some(Node::Dir(_) | Node::Provider(_)) => DescriptorType::Directory,
        Some(Node::Symlink(_)) => DescriptorType::SymbolicLink,
        Some(Node::Null | Node::Zero | Node::Random) => DescriptorType::CharacterDevice,
        _ => DescriptorType::RegularFile,
    }
}

/// An open entry behind a provider mount: everything needed to forward
/// descriptor operations to the serving node.
#[derive(Clone)]
struct RemoteDesc {
    conn: Arc<ProviderConn>,
    /// The serve-loop incarnation this descriptor's `handle` belongs to; a
    /// provider restart bumps it and stale handles are refused (EIO), never
    /// replayed against the new incarnation.
    generation: u64,
    /// Path relative to the provider's root (`""` = the root itself).
    path: String,
    /// The provider's handle for an *opened* entry (present after `open_at`;
    /// absent for path-only descriptors like the walk's synthesized dirs).
    handle: Option<u64>,
    kind: FsEntryKind,
    /// Mounted read-only: refuse mutations host-side, before the provider.
    readonly: bool,
}

impl RemoteDesc {
    /// Whether `handle` is still valid — i.e. the same serve loop that minted
    /// it is the one that would receive it.
    fn live(&self) -> bool {
        self.conn.generation() == self.generation
    }
}

/// Where a descriptor points: a node in this `Fs`, or an entry served by a
/// provider node across a mount boundary.
#[derive(Clone)]
enum DescPlace {
    Local(u64),
    Remote(RemoteDesc),
}

/// A descriptor handle: an open file or directory in some app node's `Fs`, or
/// an open entry forwarded to a provider mount.
pub struct Descriptor {
    fs: SharedFs,
    place: DescPlace,
}

impl Descriptor {
    /// Open a descriptor onto `node`, counting the reference so an unlink while
    /// this is open does not free the node out from under it.
    fn open(fs: SharedFs, node: u64) -> Self {
        fs.lock().unwrap().open_ref(node);
        Descriptor {
            fs,
            place: DescPlace::Local(node),
        }
    }

    fn remote(fs: SharedFs, r: RemoteDesc) -> Self {
        Descriptor {
            fs,
            place: DescPlace::Remote(r),
        }
    }
}

impl Drop for Descriptor {
    fn drop(&mut self) {
        match &self.place {
            DescPlace::Local(node) => {
                if let Ok(mut g) = self.fs.lock() {
                    g.close_ref(*node);
                }
            }
            // Let the provider free its handle; fire-and-forget, because a
            // Drop must not block on a slow guest.
            DescPlace::Remote(r) => {
                if let Some(handle) = r.handle {
                    if r.live() {
                        r.conn.cast(FsOp::Release { handle });
                    }
                }
            }
        }
    }
}

/// Map a provider/conduit error onto the `wasi:filesystem` error the consumer
/// guest sees. Conduit failures (dead provider, timeout) are EIO — the FUSE
/// convention for a daemon that went away.
fn provider_err(e: FsError) -> ErrorCode {
    match e {
        FsError::NoEntry => ErrorCode::NoEntry,
        FsError::NotDir => ErrorCode::NotDirectory,
        FsError::IsDir => ErrorCode::IsDirectory,
        FsError::Exist => ErrorCode::Exist,
        FsError::NotPermitted => ErrorCode::NotPermitted,
        FsError::TooLarge => ErrorCode::FileTooLarge,
        FsError::Unsupported => ErrorCode::Unsupported,
        FsError::Io | FsError::Dead | FsError::Timeout => ErrorCode::Io,
    }
}

/// Join a path onto a remote base path (both provider-relative). A leading `/`
/// cannot re-anchor at the consumer's root — the fd is the anchor in wasi, so
/// the provider root is as far up as a remote path can reach.
fn remote_join(base: &str, path: &str) -> String {
    let mut comps = components(base);
    comps.extend(components(path));
    comps.join("/")
}

/// Where a path walk from a local directory ends up.
enum Resolved {
    Local(u64),
    /// The walk crossed a provider mount: the remaining path is the provider's
    /// to answer (it may or may not exist there — that, too, is its answer).
    Remote {
        conn: Arc<ProviderConn>,
        path: String,
        readonly: bool,
    },
    /// The walk ended nowhere, locally (create may follow).
    Missing,
}

/// Resolve `path` from `start` like [`resolve_at`], but stop at a provider
/// mount boundary and hand the residual path to the caller for forwarding.
/// The boundary check happens on every component, so `mnt/p/a/b` forwards
/// `a/b` no matter how deep the mount sits.
fn resolve_place(fs: &Fs, start: u64, path: &str, follow_final: bool) -> Resolved {
    let mut hops = 0usize;
    let mut todo: Vec<String> = components(path)
        .into_iter()
        .rev()
        .map(str::to_string)
        .collect();
    let mut cur = if path.starts_with('/') { ROOT } else { start };
    // `start` may itself be a provider mount point reached by an earlier open.
    if let Some(Node::Provider(conn)) = fs.nodes.get(&cur) {
        let mut rest: Vec<String> = todo;
        rest.reverse();
        return Resolved::Remote {
            conn: conn.clone(),
            path: rest.join("/"),
            readonly: fs.readonly.contains(&cur),
        };
    }
    while let Some(comp) = todo.pop() {
        let next = match fs.nodes.get(&cur) {
            Some(Node::Dir(children)) => match children.get(&comp) {
                Some(id) => *id,
                None => return Resolved::Missing,
            },
            _ => return Resolved::Missing,
        };
        let is_final = todo.is_empty();
        match fs.nodes.get(&next) {
            Some(Node::Provider(conn)) => {
                // Everything still on the worklist belongs to the provider.
                let mut rest: Vec<String> = std::mem::take(&mut todo);
                rest.reverse();
                return Resolved::Remote {
                    conn: conn.clone(),
                    path: rest.join("/"),
                    readonly: fs.readonly.contains(&next),
                };
            }
            Some(Node::Symlink(target)) if !is_final || follow_final => {
                hops += 1;
                if hops > MAX_SYMLINK_HOPS {
                    return Resolved::Missing; // ELOOP
                }
                if target.starts_with('/') {
                    cur = ROOT;
                }
                for c in components(target).into_iter().rev() {
                    todo.push(c.to_string());
                }
            }
            Some(_) => cur = next,
            None => return Resolved::Missing,
        }
    }
    Resolved::Local(cur)
}

/// An input stream over an opened provider file: each read is one forwarded
/// `read` op at a moving offset, capped like local file reads.
struct ProviderInputStream {
    remote: RemoteDesc,
    handle: u64,
    offset: u64,
}

#[async_trait]
impl Pollable for ProviderInputStream {
    async fn ready(&mut self) {}
}

impl InputStream for ProviderInputStream {
    fn read(&mut self, size: usize) -> std::result::Result<Bytes, StreamError> {
        if !self.remote.live() {
            return Err(StreamError::Closed);
        }
        let len = size.min(FILE_READ_CHUNK) as u32;
        match self.remote.conn.call(FsOp::Read {
            handle: self.handle,
            offset: self.offset,
            len,
        }) {
            Ok(FsReplyData::Data { bytes, eof }) => {
                self.offset += bytes.len() as u64;
                if bytes.is_empty() && eof {
                    return Err(StreamError::Closed);
                }
                Ok(Bytes::from(bytes))
            }
            _ => Err(StreamError::Closed),
        }
    }
}

/// An output stream over an opened provider file: forwarded `write` ops at a
/// moving offset.
struct ProviderOutputStream {
    remote: RemoteDesc,
    handle: u64,
    offset: u64,
}

#[async_trait]
impl Pollable for ProviderOutputStream {
    async fn ready(&mut self) {}
}

impl OutputStream for ProviderOutputStream {
    fn write(&mut self, bytes: Bytes) -> std::result::Result<(), StreamError> {
        if !self.remote.live() {
            return Err(StreamError::Closed);
        }
        match self.remote.conn.call(FsOp::Write {
            handle: self.handle,
            offset: self.offset,
            data: bytes.to_vec(),
        }) {
            Ok(FsReplyData::Written(n)) => {
                self.offset += n;
                Ok(())
            }
            _ => Err(StreamError::Closed),
        }
    }
    fn flush(&mut self) -> std::result::Result<(), StreamError> {
        Ok(())
    }
    fn check_write(&mut self) -> std::result::Result<usize, StreamError> {
        Ok(1024 * 1024)
    }
}

/// A snapshot directory listing.
pub struct DirEntryStream {
    entries: Vec<DirectoryEntry>,
    pos: usize,
}

/// An output stream that writes into a private in-memory file at a moving
/// offset.
struct VfsOutputStream {
    fs: SharedFs,
    node: u64,
    offset: u64,
}

#[async_trait]
impl Pollable for VfsOutputStream {
    async fn ready(&mut self) {}
}

impl OutputStream for VfsOutputStream {
    fn write(&mut self, bytes: Bytes) -> std::result::Result<(), StreamError> {
        let mut fs = self.fs.lock().unwrap();
        fs.copy_up(self.node);
        match fs.nodes.get_mut(&self.node) {
            Some(Node::File(data)) => {
                write_at(data, self.offset, &bytes).map_err(|_| StreamError::Closed)?;
                self.offset += bytes.len() as u64;
                Ok(())
            }
            _ => Err(StreamError::Closed),
        }
    }
    fn flush(&mut self) -> std::result::Result<(), StreamError> {
        Ok(())
    }
    fn check_write(&mut self) -> std::result::Result<usize, StreamError> {
        Ok(1024 * 1024)
    }
}

/// The null device's output stream: every write is accepted and discarded.
struct NullOutputStream;

#[async_trait]
impl Pollable for NullOutputStream {
    async fn ready(&mut self) {}
}

impl OutputStream for NullOutputStream {
    fn write(&mut self, _bytes: Bytes) -> std::result::Result<(), StreamError> {
        Ok(())
    }
    fn flush(&mut self) -> std::result::Result<(), StreamError> {
        Ok(())
    }
    fn check_write(&mut self) -> std::result::Result<usize, StreamError> {
        Ok(usize::MAX)
    }
}

/// Cap on the bytes one device read produces, so a caller asking for a huge
/// count (or `usize::MAX`) can't force an unbounded allocation. The reader loops
/// for more, exactly as it would against a real endless device.
const DEVICE_READ_CHUNK: usize = 1 << 20; // 1 MiB

/// Cap on the bytes one regular-file read returns to the guest. A `read` that
/// hands back a large `list<u8>` makes wasmtime lower it into guest memory in
/// one shot (via the guest's `cabi_realloc`); past a threshold that trips the
/// component model's "cannot leave component instance" reentrancy trap, which
/// crashed loading any module over a few tens of KB (e.g. a bundled React app).
/// Returning a short read instead — every reader loops for the rest — keeps each
/// host→guest transfer small. 32 KiB is within the range already exercised on
/// every load (the resolver's 16 KiB probe read, then the remainder).
const FILE_READ_CHUNK: usize = 32 * 1024;

/// OS-random bytes for `/dev/urandom`; falls back to zeros rather than error
/// (a device read must not fail) in the extraordinarily unlikely getrandom miss.
fn random_bytes(n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    let _ = getrandom::fill(&mut buf);
    buf
}

/// `/dev/zero`'s input stream: an endless run of `\0`.
struct ZeroInputStream;

#[async_trait]
impl Pollable for ZeroInputStream {
    async fn ready(&mut self) {}
}

impl InputStream for ZeroInputStream {
    fn read(&mut self, size: usize) -> std::result::Result<Bytes, StreamError> {
        Ok(Bytes::from(vec![0u8; size.min(DEVICE_READ_CHUNK)]))
    }
}

/// `/dev/urandom` / `/dev/random`'s input stream: an endless run of OS-random.
struct RandomInputStream;

#[async_trait]
impl Pollable for RandomInputStream {
    async fn ready(&mut self) {}
}

impl InputStream for RandomInputStream {
    fn read(&mut self, size: usize) -> std::result::Result<Bytes, StreamError> {
        Ok(Bytes::from(random_bytes(size.min(DEVICE_READ_CHUNK))))
    }
}

/// A regular file's bytes served as an input stream, but at most FILE_READ_CHUNK
/// per read. `wasmtime_wasi`'s `MemoryInputPipe` hands back its whole remaining
/// buffer in one read, which for a large file is exactly the oversized
/// host→guest transfer that trips the component reentrancy trap (see
/// FILE_READ_CHUNK). The guest loops for the rest, so short reads are transparent.
struct BoundedFileStream {
    bytes: Bytes,
    pos: usize,
}

#[async_trait]
impl Pollable for BoundedFileStream {
    async fn ready(&mut self) {}
}

impl InputStream for BoundedFileStream {
    fn read(&mut self, size: usize) -> std::result::Result<Bytes, StreamError> {
        if self.pos >= self.bytes.len() {
            return Err(StreamError::Closed);
        }
        let n = size.min(FILE_READ_CHUNK).min(self.bytes.len() - self.pos);
        let chunk = self.bytes.slice(self.pos..self.pos + n);
        self.pos += n;
        Ok(chunk)
    }
}

/// An output stream that writes into a connected file node's shared bytes.
struct SharedOutputStream {
    data: SharedFile,
    offset: u64,
}

#[async_trait]
impl Pollable for SharedOutputStream {
    async fn ready(&mut self) {}
}

impl OutputStream for SharedOutputStream {
    fn write(&mut self, bytes: Bytes) -> std::result::Result<(), StreamError> {
        write_at(&mut self.data.lock().unwrap(), self.offset, &bytes)
            .map_err(|_| StreamError::Closed)?;
        self.offset += bytes.len() as u64;
        Ok(())
    }
    fn flush(&mut self) -> std::result::Result<(), StreamError> {
        Ok(())
    }
    fn check_write(&mut self) -> std::result::Result<usize, StreamError> {
        Ok(1024 * 1024)
    }
}

/// An output stream that writes into a host-backed file at a moving offset.
struct HostOutputStream {
    path: std::path::PathBuf,
    offset: u64,
}

#[async_trait]
impl Pollable for HostOutputStream {
    async fn ready(&mut self) {}
}

impl OutputStream for HostOutputStream {
    fn write(&mut self, bytes: Bytes) -> std::result::Result<(), StreamError> {
        host_write_at(&self.path, self.offset, &bytes).map_err(|_| StreamError::Closed)?;
        self.offset += bytes.len() as u64;
        Ok(())
    }
    fn flush(&mut self) -> std::result::Result<(), StreamError> {
        Ok(())
    }
    fn check_write(&mut self) -> std::result::Result<usize, StreamError> {
        Ok(1024 * 1024)
    }
}

/// Read the whole host file (a missing file reads as empty).
fn host_read(path: &std::path::Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_default()
}

/// The remaining bytes of a readable node from `offset`, as one snapshot —
/// the 0.2 and 0.3 stream reads share this copy-on-read semantics.
fn snapshot_from(g: &Fs, node: u64, offset: u64) -> Option<Bytes> {
    let slice = |data: &[u8]| {
        let start = (offset as usize).min(data.len());
        Bytes::copy_from_slice(&data[start..])
    };
    match g.nodes.get(&node)? {
        Node::File(d) => Some(slice(d)),
        Node::RoFile(d) => Some(slice(&d.bytes())),
        Node::Shared(sh) => Some(slice(&sh.lock().unwrap())),
        Node::Host(p) => Some(slice(&host_read(p))),
        _ => None,
    }
}

/// Size of the host file in bytes (0 if it doesn't exist yet).
fn host_size(path: &std::path::Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// A host-backed file's modification time, as wasi's `datetime`. In-memory
/// nodes have none — nothing tracks when they changed — but a bind mount is a
/// real file, and guests that watch one for edits (a world node reloading its
/// `.glb`, a live-coded shader) need the timestamp to notice a change that
/// leaves the size the same.
fn host_mtime(path: &std::path::Path) -> Option<Datetime> {
    let d = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    Some(Datetime {
        seconds: d.as_secs(),
        nanoseconds: d.subsec_nanos(),
    })
}

/// Write `buf` into the host file at `offset`, creating it if needed.
fn host_write_at(path: &std::path::Path, offset: u64, buf: &[u8]) -> std::io::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    f.seek(SeekFrom::Start(offset))?;
    f.write_all(buf)?;
    Ok(())
}

/// Upper bound on the size of a single in-memory (or shared) file. Guests fully
/// control the write offset and `set-size`, so without a cap a single call like
/// `write(offset = 2^48)` would ask `Vec::resize` for a multi-terabyte
/// allocation and abort the whole server process.
const MAX_FILE_SIZE: usize = 256 * 1024 * 1024;

/// Copy `bytes` into `data` at `offset`, growing it if needed. Returns `Err` if
/// the write would push the file past [`MAX_FILE_SIZE`] (or overflow `usize`),
/// in which case `data` is left unchanged.
fn write_at(data: &mut Vec<u8>, offset: u64, bytes: &[u8]) -> std::result::Result<(), ()> {
    let start = usize::try_from(offset).map_err(|_| ())?;
    let end = start.checked_add(bytes.len()).ok_or(())?;
    if end > MAX_FILE_SIZE {
        return Err(());
    }
    if data.len() < end {
        data.resize(end, 0);
    }
    data[start..end].copy_from_slice(bytes);
    Ok(())
}

/// Read up to `len` bytes of `data` from `offset`, returning (bytes, eof).
fn read_at(data: &[u8], offset: u64, len: u64) -> (Vec<u8>, bool) {
    let start = usize::try_from(offset)
        .unwrap_or(usize::MAX)
        .min(data.len());
    let len = usize::try_from(len).unwrap_or(usize::MAX);
    let end = start.saturating_add(len).min(data.len());
    (data[start..end].to_vec(), end >= data.len())
}

/// Add every wasmtime-wasi interface our guests use *except* its (cap-std)
/// filesystem, so we can provide our own in-memory filesystem instead.
pub fn add_wasi_except_fs<T: WasiView + 'static>(l: &mut Linker<T>) -> Result<()> {
    use wasmtime_wasi::cli::{WasiCli, WasiCliView};
    use wasmtime_wasi::clocks::{WasiClocks, WasiClocksView};
    use wasmtime_wasi::p2::bindings::{cli, clocks};

    struct HasIo;
    impl HasData for HasIo {
        type Data<'a> = &'a mut ResourceTable;
    }

    wasmtime_wasi_io::bindings::wasi::io::error::add_to_linker::<T, HasIo>(l, |t| t.ctx().table)?;
    wasmtime_wasi_io::bindings::wasi::io::poll::add_to_linker::<T, HasIo>(l, |t| t.ctx().table)?;
    wasmtime_wasi_io::bindings::wasi::io::streams::add_to_linker::<T, HasIo>(l, |t| t.ctx().table)?;

    clocks::wall_clock::add_to_linker::<T, WasiClocks>(l, T::clocks)?;
    clocks::monotonic_clock::add_to_linker::<T, WasiClocks>(l, T::clocks)?;

    cli::exit::add_to_linker::<T, WasiCli>(l, T::cli)?;
    cli::environment::add_to_linker::<T, WasiCli>(l, T::cli)?;
    cli::stdin::add_to_linker::<T, WasiCli>(l, T::cli)?;
    cli::stdout::add_to_linker::<T, WasiCli>(l, T::cli)?;
    cli::stderr::add_to_linker::<T, WasiCli>(l, T::cli)?;
    cli::terminal_input::add_to_linker::<T, WasiCli>(l, T::cli)?;
    cli::terminal_output::add_to_linker::<T, WasiCli>(l, T::cli)?;
    cli::terminal_stdin::add_to_linker::<T, WasiCli>(l, T::cli)?;
    cli::terminal_stdout::add_to_linker::<T, WasiCli>(l, T::cli)?;
    cli::terminal_stderr::add_to_linker::<T, WasiCli>(l, T::cli)?;
    // Note: wasi:sockets is NOT added here — wk provides its own implementation
    // over the userspace network fabric (see crate::sockets).
    Ok(())
}

/// Add our in-memory `wasi:filesystem` to the linker, for any store type that
/// implements [`VfsView`].
pub fn add_to_linker<T: VfsView + 'static>(l: &mut Linker<T>) -> Result<()> {
    wasi::filesystem::types::add_to_linker::<_, HasFs<T>>(l, |s| VfsImpl(s))?;
    wasi::filesystem::preopens::add_to_linker::<_, HasFs<T>>(l, |s| VfsImpl(s))?;
    Ok(())
}

struct HasFs<T>(std::marker::PhantomData<T>);
impl<T: VfsView + 'static> HasData for HasFs<T> {
    type Data<'a> = VfsImpl<&'a mut T>;
}

/// `Ok(Err(code))` shorthand.
fn err<T>(code: ErrorCode) -> Result<std::result::Result<T, ErrorCode>> {
    Ok(Err(code))
}

/// What `node` is, for read/write/stream dispatch — cloning the shared handle so
/// callers can act without holding the filesystem lock.
enum Kind {
    File,
    /// An immutable layer file (reads serve the shared bytes; writes copy up).
    Ro(Arc<Vec<u8>>),
    Shared(SharedFile),
    Host(std::path::PathBuf),
    Dir,
    /// The null device — writes discarded, reads at end-of-file.
    Null,
    /// The zero device — writes discarded, reads return `\0` forever.
    Zero,
    /// A random device — writes discarded, reads return OS-random bytes.
    Random,
    Missing,
}

/// Clone the `fs` Arc and place for the descriptor `fd` (all this node's
/// descriptors share the one filesystem).
fn fd_place<T: VfsView>(view: &mut T, fd: &Resource<Descriptor>) -> Result<(SharedFs, DescPlace)> {
    let d = view.table().get(fd)?;
    Ok((d.fs.clone(), d.place.clone()))
}

/// A stable-enough identity for a remote path (`metadata_hash`): FNV-1a over
/// the provider-relative path, with `upper = 1` so it can't collide with local
/// node ids (which use `upper = 0`).
fn remote_path_hash(path: &str) -> MetadataHashValue {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for b in path.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    MetadataHashValue { lower: h, upper: 1 }
}

/// Forward an `open` across a provider mount and mint the remote descriptor.
fn remote_open<T: VfsView>(
    view: &mut T,
    fs: SharedFs,
    conn: Arc<ProviderConn>,
    readonly: bool,
    path: String,
    oflags: OpenFlags,
) -> Result<std::result::Result<Resource<Descriptor>, ErrorCode>> {
    let create = oflags.contains(OpenFlags::CREATE);
    let truncate = oflags.contains(OpenFlags::TRUNCATE);
    let exclusive = oflags.contains(OpenFlags::EXCLUSIVE);
    if readonly && (create || truncate) {
        return err(ErrorCode::NotPermitted);
    }
    let generation = conn.generation();
    match conn.call(FsOp::Open {
        path: path.clone(),
        create,
        truncate,
        exclusive,
    }) {
        Ok(FsReplyData::Opened(o)) => {
            if oflags.contains(OpenFlags::DIRECTORY) && o.kind != FsEntryKind::Dir {
                conn.cast(FsOp::Release { handle: o.handle });
                return err(ErrorCode::NotDirectory);
            }
            let desc = Descriptor::remote(
                fs,
                RemoteDesc {
                    conn,
                    generation,
                    path,
                    handle: Some(o.handle),
                    kind: o.kind,
                    readonly,
                },
            );
            Ok(Ok(view.table().push(desc)?))
        }
        Ok(_) => err(ErrorCode::Io),
        Err(e) => err(provider_err(e)),
    }
}

/// Forward `getattr` for a remote path and shape it as a `DescriptorStat`.
fn remote_stat(conn: &ProviderConn, path: &str) -> std::result::Result<DescriptorStat, ErrorCode> {
    match conn.call(FsOp::Getattr {
        path: path.to_string(),
    }) {
        Ok(FsReplyData::Attr(st)) => Ok(DescriptorStat {
            type_: match st.kind {
                FsEntryKind::Dir => DescriptorType::Directory,
                FsEntryKind::File => DescriptorType::RegularFile,
            },
            link_count: 1,
            size: st.size,
            data_access_timestamp: None,
            data_modification_timestamp: None,
            status_change_timestamp: None,
        }),
        Ok(_) => Err(ErrorCode::Io),
        Err(e) => Err(provider_err(e)),
    }
}

/// What `node` is, cloning shared handles so callers can act without the lock.
fn node_kind(fs: &SharedFs, node: u64) -> Kind {
    {
        match fs.lock().unwrap().nodes.get(&node) {
            Some(Node::File(_)) => Kind::File,
            // Every `node_kind` caller is a data path (read/write/stream
            // dispatch), so materializing a lazy layer file here is exactly
            // "first access"; the stat paths go through `stat_node`/`file_len`,
            // which use the header length and never call this.
            Some(Node::RoFile(d)) => Kind::Ro(d.bytes()),
            Some(Node::Shared(sh)) => Kind::Shared(sh.clone()),
            Some(Node::Host(p)) => Kind::Host(p.clone()),
            // Descriptors never land on a provider mount point (resolution
            // turns it into a remote descriptor), but a stray local id still
            // presents as the directory it stats as.
            Some(Node::Dir(_) | Node::Provider(_)) => Kind::Dir,
            Some(Node::Null) => Kind::Null,
            Some(Node::Zero) => Kind::Zero,
            Some(Node::Random) => Kind::Random,
            // Nothing reads or writes through an open link handle; readlink
            // and lstat work on the path, not the descriptor.
            Some(Node::Symlink(_)) | None => Kind::Missing,
        }
    }
}

impl<T: VfsView> wasi::filesystem::preopens::Host for VfsImpl<T> {
    fn get_directories(&mut self) -> Result<Vec<(Resource<Descriptor>, String)>> {
        let fs = self.fs();
        let root = self.table().push(Descriptor::open(fs, ROOT))?;
        Ok(vec![(root, "/".to_string())])
    }
}

impl<T: VfsView> wasi::filesystem::types::Host for VfsImpl<T> {
    fn filesystem_error_code(
        &mut self,
        _err: Resource<wasmtime::Error>,
    ) -> Result<Option<ErrorCode>> {
        Ok(None)
    }
}

impl<T: VfsView> wasi::filesystem::types::HostDescriptor for VfsImpl<T> {
    fn read_via_stream(
        &mut self,
        fd: Resource<Descriptor>,
        offset: Filesize,
    ) -> Result<std::result::Result<Resource<DynInputStream>, ErrorCode>> {
        let (fs, place) = fd_place(self, &fd)?;
        let node = match place {
            DescPlace::Local(n) => n,
            DescPlace::Remote(r) => {
                let Some(handle) = r.handle else {
                    return err(ErrorCode::IsDirectory);
                };
                if !r.live() {
                    return err(ErrorCode::Io);
                }
                let stream: DynInputStream = Box::new(ProviderInputStream {
                    remote: r.clone(),
                    handle,
                    offset,
                });
                return Ok(Ok(self.table().push(stream)?));
            }
        };
        // The endless devices produce their own on-demand streams; everything
        // else serves a fixed byte range through a MemoryInputPipe.
        let bytes = match node_kind(&fs, node) {
            Kind::Zero => {
                let stream: DynInputStream = Box::new(ZeroInputStream);
                return Ok(Ok(self.table().push(stream)?));
            }
            Kind::Random => {
                let stream: DynInputStream = Box::new(RandomInputStream);
                return Ok(Ok(self.table().push(stream)?));
            }
            Kind::File => {
                let g = fs.lock().unwrap();
                let Some(Node::File(data)) = g.nodes.get(&node) else {
                    return err(ErrorCode::NoEntry);
                };
                let start = (offset as usize).min(data.len());
                Bytes::copy_from_slice(&data[start..])
            }
            Kind::Ro(d) => {
                let start = (offset as usize).min(d.len());
                Bytes::copy_from_slice(&d[start..])
            }
            Kind::Shared(sh) => {
                let d = sh.lock().unwrap();
                let start = (offset as usize).min(d.len());
                Bytes::copy_from_slice(&d[start..])
            }
            Kind::Host(p) => {
                let d = host_read(&p);
                let start = (offset as usize).min(d.len());
                Bytes::copy_from_slice(&d[start..])
            }
            Kind::Null => Bytes::new(),
            Kind::Dir => return err(ErrorCode::IsDirectory),
            Kind::Missing => return err(ErrorCode::NoEntry),
        };
        let stream: DynInputStream = Box::new(BoundedFileStream { bytes, pos: 0 });
        Ok(Ok(self.table().push(stream)?))
    }

    fn write_via_stream(
        &mut self,
        fd: Resource<Descriptor>,
        offset: Filesize,
    ) -> Result<std::result::Result<Resource<DynOutputStream>, ErrorCode>> {
        let (fs, place) = fd_place(self, &fd)?;
        let node = match place {
            DescPlace::Local(n) => n,
            DescPlace::Remote(r) => {
                if r.readonly {
                    return err(ErrorCode::NotPermitted);
                }
                let Some(handle) = r.handle else {
                    return err(ErrorCode::IsDirectory);
                };
                if !r.live() {
                    return err(ErrorCode::Io);
                }
                let stream: DynOutputStream = Box::new(ProviderOutputStream {
                    remote: r.clone(),
                    handle,
                    offset,
                });
                return Ok(Ok(self.table().push(stream)?));
            }
        };
        if is_readonly(&fs, node) {
            return err(ErrorCode::NotPermitted);
        }
        let stream: DynOutputStream = match node_kind(&fs, node) {
            // A layer file copy-ups on the stream's first write.
            Kind::File | Kind::Ro(_) => Box::new(VfsOutputStream { fs, node, offset }),
            Kind::Shared(data) => Box::new(SharedOutputStream { data, offset }),
            Kind::Host(path) => Box::new(HostOutputStream { path, offset }),
            Kind::Null | Kind::Zero | Kind::Random => Box::new(NullOutputStream),
            _ => return err(ErrorCode::IsDirectory),
        };
        Ok(Ok(self.table().push(stream)?))
    }

    fn append_via_stream(
        &mut self,
        fd: Resource<Descriptor>,
    ) -> Result<std::result::Result<Resource<DynOutputStream>, ErrorCode>> {
        let (fs, place) = fd_place(self, &fd)?;
        let node = match place {
            DescPlace::Local(n) => n,
            DescPlace::Remote(r) => {
                if r.readonly {
                    return err(ErrorCode::NotPermitted);
                }
                let Some(handle) = r.handle else {
                    return err(ErrorCode::IsDirectory);
                };
                if !r.live() {
                    return err(ErrorCode::Io);
                }
                // Append starts at the provider's current size.
                let offset = match remote_stat(&r.conn, &r.path) {
                    Ok(st) => st.size,
                    Err(code) => return err(code),
                };
                let stream: DynOutputStream = Box::new(ProviderOutputStream {
                    remote: r.clone(),
                    handle,
                    offset,
                });
                return Ok(Ok(self.table().push(stream)?));
            }
        };
        if is_readonly(&fs, node) {
            return err(ErrorCode::NotPermitted);
        }
        let stream: DynOutputStream = match node_kind(&fs, node) {
            Kind::File | Kind::Ro(_) => {
                // Append needs the private copy's length; copy up now.
                let offset = {
                    let mut g = fs.lock().unwrap();
                    g.copy_up(node);
                    g.nodes.get(&node).map_or(0, |n| match n {
                        Node::File(d) => d.len() as u64,
                        _ => 0,
                    })
                };
                Box::new(VfsOutputStream { fs, node, offset })
            }
            Kind::Shared(data) => {
                let offset = data.lock().unwrap().len() as u64;
                Box::new(SharedOutputStream { data, offset })
            }
            Kind::Host(path) => {
                let offset = host_size(&path);
                Box::new(HostOutputStream { path, offset })
            }
            Kind::Null | Kind::Zero | Kind::Random => Box::new(NullOutputStream),
            Kind::Dir => return err(ErrorCode::IsDirectory),
            Kind::Missing => return err(ErrorCode::NoEntry),
        };
        Ok(Ok(self.table().push(stream)?))
    }

    fn read(
        &mut self,
        fd: Resource<Descriptor>,
        len: Filesize,
        offset: Filesize,
    ) -> Result<std::result::Result<(Vec<u8>, bool), ErrorCode>> {
        let (fs, place) = fd_place(self, &fd)?;
        // Bound one read's result (see FILE_READ_CHUNK): a large single transfer
        // to the guest trips the component "cannot leave component instance"
        // trap. Short reads are valid — the caller loops for the rest.
        let len = len.min(FILE_READ_CHUNK as u64);
        let node = match place {
            DescPlace::Local(n) => n,
            DescPlace::Remote(r) => {
                let Some(handle) = r.handle else {
                    return err(ErrorCode::IsDirectory);
                };
                if !r.live() {
                    return err(ErrorCode::Io);
                }
                return match r.conn.call(FsOp::Read {
                    handle,
                    offset,
                    len: len as u32,
                }) {
                    Ok(FsReplyData::Data { bytes, eof }) => Ok(Ok((bytes, eof))),
                    Ok(_) => err(ErrorCode::Io),
                    Err(e) => err(provider_err(e)),
                };
            }
        };
        match node_kind(&fs, node) {
            Kind::File => {
                let g = fs.lock().unwrap();
                let Some(Node::File(data)) = g.nodes.get(&node) else {
                    return err(ErrorCode::NoEntry);
                };
                Ok(Ok(read_at(data, offset, len)))
            }
            Kind::Ro(d) => Ok(Ok(read_at(&d, offset, len))),
            Kind::Shared(sh) => Ok(Ok(read_at(&sh.lock().unwrap(), offset, len))),
            Kind::Host(p) => Ok(Ok(read_at(&host_read(&p), offset, len))),
            Kind::Null => Ok(Ok((Vec::new(), true))),
            // Endless devices: `len` bytes, never end-of-file.
            Kind::Zero => Ok(Ok((
                vec![0u8; (len as usize).min(DEVICE_READ_CHUNK)],
                false,
            ))),
            Kind::Random => Ok(Ok((
                random_bytes((len as usize).min(DEVICE_READ_CHUNK)),
                false,
            ))),
            Kind::Dir => err(ErrorCode::IsDirectory),
            Kind::Missing => err(ErrorCode::NoEntry),
        }
    }

    fn write(
        &mut self,
        fd: Resource<Descriptor>,
        buf: Vec<u8>,
        offset: Filesize,
    ) -> Result<std::result::Result<Filesize, ErrorCode>> {
        let (fs, place) = fd_place(self, &fd)?;
        let node = match place {
            DescPlace::Local(n) => n,
            DescPlace::Remote(r) => {
                if r.readonly {
                    return err(ErrorCode::NotPermitted);
                }
                let Some(handle) = r.handle else {
                    return err(ErrorCode::IsDirectory);
                };
                if !r.live() {
                    return err(ErrorCode::Io);
                }
                return match r.conn.call(FsOp::Write {
                    handle,
                    offset,
                    data: buf,
                }) {
                    Ok(FsReplyData::Written(n)) => Ok(Ok(n)),
                    Ok(_) => err(ErrorCode::Io),
                    Err(e) => err(provider_err(e)),
                };
            }
        };
        if is_readonly(&fs, node) {
            return err(ErrorCode::NotPermitted);
        }
        match node_kind(&fs, node) {
            Kind::File | Kind::Ro(_) => {
                let mut g = fs.lock().unwrap();
                g.copy_up(node);
                let Some(Node::File(data)) = g.nodes.get_mut(&node) else {
                    return err(ErrorCode::NoEntry);
                };
                if write_at(data, offset, &buf).is_err() {
                    return err(ErrorCode::FileTooLarge);
                }
            }
            Kind::Shared(sh) => {
                if write_at(&mut sh.lock().unwrap(), offset, &buf).is_err() {
                    return err(ErrorCode::FileTooLarge);
                }
            }
            Kind::Null | Kind::Zero | Kind::Random => {} // discard every byte
            Kind::Host(p) => {
                if host_write_at(&p, offset, &buf).is_err() {
                    return err(ErrorCode::Io);
                }
            }
            Kind::Dir => return err(ErrorCode::IsDirectory),
            Kind::Missing => return err(ErrorCode::NoEntry),
        }
        Ok(Ok(buf.len() as u64))
    }

    fn read_directory(
        &mut self,
        fd: Resource<Descriptor>,
    ) -> Result<std::result::Result<Resource<DirEntryStream>, ErrorCode>> {
        let (fs, place) = fd_place(self, &fd)?;
        let node = match place {
            DescPlace::Remote(r) => {
                if r.kind != FsEntryKind::Dir {
                    return err(ErrorCode::NotDirectory);
                }
                let entries = match r.conn.call(FsOp::Readdir {
                    path: r.path.clone(),
                }) {
                    Ok(FsReplyData::Entries(list)) => list
                        .into_iter()
                        .map(|d| DirectoryEntry {
                            type_: match d.kind {
                                FsEntryKind::Dir => DescriptorType::Directory,
                                FsEntryKind::File => DescriptorType::RegularFile,
                            },
                            name: d.name,
                        })
                        .collect(),
                    Ok(_) => return err(ErrorCode::Io),
                    Err(e) => return err(provider_err(e)),
                };
                return Ok(Ok(self.table().push(DirEntryStream { entries, pos: 0 })?));
            }
            DescPlace::Local(n) => n,
        };
        let entries = {
            let g = fs.lock().unwrap();
            match g.nodes.get(&node) {
                Some(Node::Dir(children)) => children
                    .iter()
                    .map(|(name, id)| DirectoryEntry {
                        type_: node_type(&g, *id),
                        name: name.clone(),
                    })
                    .collect(),
                Some(_) => return err(ErrorCode::NotDirectory),
                None => return err(ErrorCode::NoEntry),
            }
        };
        Ok(Ok(self.table().push(DirEntryStream { entries, pos: 0 })?))
    }

    fn create_directory_at(
        &mut self,
        fd: Resource<Descriptor>,
        path: String,
    ) -> Result<std::result::Result<(), ErrorCode>> {
        let (fs, place) = fd_place(self, &fd)?;
        // A walk that crosses a provider mount forwards the mkdir; a remote
        // start descriptor forwards with its base path joined on.
        let target = match &place {
            DescPlace::Local(start) => {
                let g = fs.lock().unwrap();
                resolve_place(&g, *start, &path, true)
            }
            DescPlace::Remote(r) => Resolved::Remote {
                conn: r.conn.clone(),
                path: remote_join(&r.path, &path),
                readonly: r.readonly,
            },
        };
        match target {
            Resolved::Remote {
                conn,
                path,
                readonly,
            } => {
                if readonly {
                    return err(ErrorCode::NotPermitted);
                }
                return match conn.call(FsOp::Mkdir { path }) {
                    Ok(_) => Ok(Ok(())),
                    Err(e) => err(provider_err(e)),
                };
            }
            Resolved::Local(_) => return err(ErrorCode::Exist),
            Resolved::Missing => {}
        }
        let DescPlace::Local(node) = place else {
            return err(ErrorCode::NoEntry);
        };
        let mut g = fs.lock().unwrap();
        let Some((parent, name)) = resolve_parent(&g, node, &path) else {
            return err(ErrorCode::NoEntry);
        };
        if let Some(Node::Dir(children)) = g.nodes.get(&parent) {
            if children.contains_key(&name) {
                return err(ErrorCode::Exist);
            }
        }
        if g.at_capacity() {
            return err(ErrorCode::InsufficientSpace);
        }
        let id = g.alloc(Node::Dir(BTreeMap::new()));
        if let Some(Node::Dir(children)) = g.nodes.get_mut(&parent) {
            children.insert(name, id);
        }
        Ok(Ok(()))
    }

    fn stat(
        &mut self,
        fd: Resource<Descriptor>,
    ) -> Result<std::result::Result<DescriptorStat, ErrorCode>> {
        let (fs, place) = fd_place(self, &fd)?;
        let node = match place {
            DescPlace::Local(n) => n,
            DescPlace::Remote(r) => {
                return match remote_stat(&r.conn, &r.path) {
                    Ok(s) => Ok(Ok(s)),
                    Err(code) => err(code),
                }
            }
        };
        let g = fs.lock().unwrap();
        match stat_node(&g, node) {
            Some(s) => Ok(Ok(s)),
            None => err(ErrorCode::NoEntry),
        }
    }

    fn stat_at(
        &mut self,
        fd: Resource<Descriptor>,
        path_flags: PathFlags,
        path: String,
    ) -> Result<std::result::Result<DescriptorStat, ErrorCode>> {
        let (fs, place) = fd_place(self, &fd)?;
        // Without SYMLINK_FOLLOW this is `lstat`: report the link itself.
        let follow = path_flags.contains(PathFlags::SYMLINK_FOLLOW);
        let target = match &place {
            DescPlace::Local(start) => {
                let g = fs.lock().unwrap();
                match resolve_place(&g, *start, &path, follow) {
                    Resolved::Local(id) => {
                        return match stat_node(&g, id) {
                            Some(s) => Ok(Ok(s)),
                            None => err(ErrorCode::NoEntry),
                        }
                    }
                    other => other,
                }
            }
            DescPlace::Remote(r) => Resolved::Remote {
                conn: r.conn.clone(),
                path: remote_join(&r.path, &path),
                readonly: r.readonly,
            },
        };
        match target {
            Resolved::Remote { conn, path, .. } => match remote_stat(&conn, &path) {
                Ok(s) => Ok(Ok(s)),
                Err(code) => err(code),
            },
            _ => err(ErrorCode::NoEntry),
        }
    }

    fn get_type(
        &mut self,
        fd: Resource<Descriptor>,
    ) -> Result<std::result::Result<DescriptorType, ErrorCode>> {
        let (fs, place) = fd_place(self, &fd)?;
        let node = match place {
            DescPlace::Local(n) => n,
            DescPlace::Remote(r) => {
                return Ok(Ok(match r.kind {
                    FsEntryKind::Dir => DescriptorType::Directory,
                    FsEntryKind::File => DescriptorType::RegularFile,
                }))
            }
        };
        let g = fs.lock().unwrap();
        Ok(Ok(node_type(&g, node)))
    }

    fn get_flags(
        &mut self,
        fd: Resource<Descriptor>,
    ) -> Result<std::result::Result<DescriptorFlags, ErrorCode>> {
        let (fs, place) = fd_place(self, &fd)?;
        let node = match place {
            DescPlace::Local(n) => n,
            DescPlace::Remote(r) => {
                return Ok(Ok(if r.readonly {
                    DescriptorFlags::READ
                } else {
                    DescriptorFlags::READ | DescriptorFlags::WRITE
                }))
            }
        };
        if is_readonly(&fs, node) {
            return Ok(Ok(DescriptorFlags::READ));
        }
        Ok(Ok(DescriptorFlags::READ | DescriptorFlags::WRITE))
    }

    fn set_size(
        &mut self,
        fd: Resource<Descriptor>,
        size: Filesize,
    ) -> Result<std::result::Result<(), ErrorCode>> {
        let (fs, place) = fd_place(self, &fd)?;
        let node = match place {
            DescPlace::Local(n) => n,
            DescPlace::Remote(r) => {
                if r.readonly {
                    return err(ErrorCode::NotPermitted);
                }
                let Some(handle) = r.handle else {
                    return err(ErrorCode::IsDirectory);
                };
                if !r.live() {
                    return err(ErrorCode::Io);
                }
                return match r.conn.call(FsOp::SetSize { handle, size }) {
                    Ok(_) => Ok(Ok(())),
                    Err(e) => err(provider_err(e)),
                };
            }
        };
        if is_readonly(&fs, node) {
            return err(ErrorCode::NotPermitted);
        }
        let size = match usize::try_from(size) {
            Ok(s) if s <= MAX_FILE_SIZE => s,
            _ => return err(ErrorCode::FileTooLarge),
        };
        match node_kind(&fs, node) {
            Kind::File | Kind::Ro(_) => {
                let mut g = fs.lock().unwrap();
                g.copy_up(node);
                if let Some(Node::File(data)) = g.nodes.get_mut(&node) {
                    data.resize(size, 0);
                }
                Ok(Ok(()))
            }
            Kind::Shared(sh) => {
                sh.lock().unwrap().resize(size, 0);
                Ok(Ok(()))
            }
            Kind::Host(p) => {
                match std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(&p)
                    .and_then(|f| f.set_len(size as u64))
                {
                    Ok(()) => Ok(Ok(())),
                    Err(_) => err(ErrorCode::Io),
                }
            }
            Kind::Null | Kind::Zero | Kind::Random => Ok(Ok(())), // nothing to size
            _ => err(ErrorCode::IsDirectory),
        }
    }

    fn open_at(
        &mut self,
        fd: Resource<Descriptor>,
        path_flags: PathFlags,
        path: String,
        oflags: OpenFlags,
        _flags: DescriptorFlags,
    ) -> Result<std::result::Result<Resource<Descriptor>, ErrorCode>> {
        let (fs, place) = fd_place(self, &fd)?;
        // Opening follows the final link unless the caller says otherwise;
        // wasi-libc asks for SYMLINK_FOLLOW on an ordinary open().
        let follow = path_flags.contains(PathFlags::SYMLINK_FOLLOW);
        let start = match &place {
            DescPlace::Local(start) => *start,
            DescPlace::Remote(r) => {
                let joined = remote_join(&r.path, &path);
                return remote_open(self, fs, r.conn.clone(), r.readonly, joined, oflags);
            }
        };
        // A walk that crosses a provider mount forwards the whole open —
        // including creates: whether the residual path exists is the
        // provider's answer, not ours.
        let crossed = {
            let g = fs.lock().unwrap();
            match resolve_place(&g, start, &path, follow) {
                Resolved::Remote {
                    conn,
                    path,
                    readonly,
                } => Some((conn, path, readonly)),
                _ => None,
            }
        };
        if let Some((conn, rpath, readonly)) = crossed {
            return remote_open(self, fs, conn, readonly, rpath, oflags);
        }
        let node = {
            let mut g = fs.lock().unwrap();
            match resolve_at(&g, start, &path, follow) {
                Some(id) => {
                    if oflags.contains(OpenFlags::EXCLUSIVE) {
                        return err(ErrorCode::Exist);
                    }
                    if oflags.contains(OpenFlags::TRUNCATE) && g.readonly.contains(&id) {
                        return err(ErrorCode::NotPermitted);
                    }
                    if oflags.contains(OpenFlags::TRUNCATE) {
                        match g.nodes.get_mut(&id) {
                            Some(Node::File(data)) => data.clear(),
                            Some(Node::RoFile(_)) => {
                                g.nodes.insert(id, Node::File(Vec::new()));
                            }
                            Some(Node::Shared(sh)) => sh.lock().unwrap().clear(),
                            // Truncate (or create) the backing host file to empty.
                            Some(Node::Host(p)) => {
                                let _ = std::fs::File::create(p.as_path());
                            }
                            _ => {}
                        }
                    }
                    if oflags.contains(OpenFlags::DIRECTORY)
                        && !matches!(g.nodes.get(&id), Some(Node::Dir(_)))
                    {
                        return err(ErrorCode::NotDirectory);
                    }
                    id
                }
                None => {
                    if !oflags.contains(OpenFlags::CREATE) {
                        return err(ErrorCode::NoEntry);
                    }
                    let Some((parent, name)) = resolve_parent(&g, start, &path) else {
                        return err(ErrorCode::NoEntry);
                    };
                    if g.at_capacity() {
                        return err(ErrorCode::InsufficientSpace);
                    }
                    let id = g.alloc(Node::File(Vec::new()));
                    if let Some(Node::Dir(children)) = g.nodes.get_mut(&parent) {
                        children.insert(name, id);
                    }
                    id
                }
            }
        };
        Ok(Ok(self.table().push(Descriptor::open(fs, node))?))
    }

    fn remove_directory_at(
        &mut self,
        fd: Resource<Descriptor>,
        path: String,
    ) -> Result<std::result::Result<(), ErrorCode>> {
        unlink(self, fd, &path, true)
    }

    fn unlink_file_at(
        &mut self,
        fd: Resource<Descriptor>,
        path: String,
    ) -> Result<std::result::Result<(), ErrorCode>> {
        unlink(self, fd, &path, false)
    }

    fn rename_at(
        &mut self,
        fd: Resource<Descriptor>,
        old_path: String,
        new_fd: Resource<Descriptor>,
        new_path: String,
    ) -> Result<std::result::Result<(), ErrorCode>> {
        let (fs, old_place) = fd_place(self, &fd)?;
        let (new_fs_arc, new_place) = fd_place(self, &new_fd)?;
        // Rename only within one filesystem (node ids are per-fs).
        if !Arc::ptr_eq(&fs, &new_fs_arc) {
            return err(ErrorCode::CrossDevice);
        }
        // Work out where each side lands; a provider crossing on either side
        // means the rename happens remotely (same provider) or not at all.
        let side = |place: &DescPlace, path: &str, g: &Fs| -> Resolved {
            match place {
                DescPlace::Local(start) => match resolve_place(g, *start, path, false) {
                    // For rename we only care whether the walk crossed; a
                    // local hit or miss both mean "local side".
                    Resolved::Remote {
                        conn,
                        path,
                        readonly,
                    } => Resolved::Remote {
                        conn,
                        path,
                        readonly,
                    },
                    other => other,
                },
                DescPlace::Remote(r) => Resolved::Remote {
                    conn: r.conn.clone(),
                    path: remote_join(&r.path, path),
                    readonly: r.readonly,
                },
            }
        };
        let (old_side, new_side) = {
            let g = fs.lock().unwrap();
            (
                side(&old_place, &old_path, &g),
                side(&new_place, &new_path, &g),
            )
        };
        match (&old_side, &new_side) {
            (
                Resolved::Remote {
                    conn: a,
                    path: from,
                    readonly,
                },
                Resolved::Remote {
                    conn: b, path: to, ..
                },
            ) => {
                if !Arc::ptr_eq(a, b) {
                    return err(ErrorCode::CrossDevice);
                }
                if *readonly {
                    return err(ErrorCode::NotPermitted);
                }
                return match a.call(FsOp::Rename {
                    from: from.clone(),
                    to: to.clone(),
                }) {
                    Ok(_) => Ok(Ok(())),
                    Err(e) => err(provider_err(e)),
                };
            }
            (Resolved::Remote { .. }, _) | (_, Resolved::Remote { .. }) => {
                return err(ErrorCode::CrossDevice);
            }
            _ => {}
        }
        let (DescPlace::Local(old_start), DescPlace::Local(new_start)) = (old_place, new_place)
        else {
            return err(ErrorCode::CrossDevice);
        };
        let mut g = fs.lock().unwrap();
        let Some((old_parent, old_name)) = resolve_parent(&g, old_start, &old_path) else {
            return err(ErrorCode::NoEntry);
        };
        let id = match g.nodes.get(&old_parent) {
            Some(Node::Dir(c)) => match c.get(&old_name) {
                Some(id) => *id,
                None => return err(ErrorCode::NoEntry),
            },
            _ => return err(ErrorCode::NotDirectory),
        };
        // Moving a read-only mount is as off-limits as removing it.
        if g.readonly.contains(&id) {
            return err(ErrorCode::NotPermitted);
        }
        let Some((new_parent, new_name)) = resolve_parent(&g, new_start, &new_path) else {
            return err(ErrorCode::NoEntry);
        };
        if let Some(Node::Dir(c)) = g.nodes.get_mut(&old_parent) {
            c.remove(&old_name);
        }
        if let Some(Node::Dir(c)) = g.nodes.get_mut(&new_parent) {
            c.insert(new_name, id);
        }
        Ok(Ok(()))
    }

    fn is_same_object(&mut self, a: Resource<Descriptor>, b: Resource<Descriptor>) -> Result<bool> {
        let (afs, ap) = fd_place(self, &a)?;
        let (bfs, bp) = fd_place(self, &b)?;
        Ok(match (&ap, &bp) {
            (DescPlace::Local(an), DescPlace::Local(bn)) => Arc::ptr_eq(&afs, &bfs) && an == bn,
            (DescPlace::Remote(ra), DescPlace::Remote(rb)) => {
                Arc::ptr_eq(&ra.conn, &rb.conn) && ra.path == rb.path
            }
            _ => false,
        })
    }

    fn metadata_hash(
        &mut self,
        fd: Resource<Descriptor>,
    ) -> Result<std::result::Result<MetadataHashValue, ErrorCode>> {
        let (_fs, place) = fd_place(self, &fd)?;
        Ok(Ok(match place {
            DescPlace::Local(node) => MetadataHashValue {
                lower: node,
                upper: 0,
            },
            DescPlace::Remote(r) => remote_path_hash(&r.path),
        }))
    }

    fn metadata_hash_at(
        &mut self,
        fd: Resource<Descriptor>,
        _path_flags: PathFlags,
        path: String,
    ) -> Result<std::result::Result<MetadataHashValue, ErrorCode>> {
        let (fs, place) = fd_place(self, &fd)?;
        let start = match &place {
            DescPlace::Local(start) => *start,
            DescPlace::Remote(r) => {
                return Ok(Ok(remote_path_hash(&remote_join(&r.path, &path))));
            }
        };
        let g = fs.lock().unwrap();
        match resolve_place(&g, start, &path, true) {
            Resolved::Local(id) => Ok(Ok(MetadataHashValue {
                lower: id,
                upper: 0,
            })),
            Resolved::Remote { path, .. } => Ok(Ok(remote_path_hash(&path))),
            Resolved::Missing => err(ErrorCode::NoEntry),
        }
    }

    // ---- not meaningful for an in-memory FS: accept or report unsupported ----

    fn advise(
        &mut self,
        _fd: Resource<Descriptor>,
        _offset: Filesize,
        _len: Filesize,
        _advice: Advice,
    ) -> Result<std::result::Result<(), ErrorCode>> {
        Ok(Ok(()))
    }
    fn sync_data(
        &mut self,
        _fd: Resource<Descriptor>,
    ) -> Result<std::result::Result<(), ErrorCode>> {
        Ok(Ok(()))
    }
    fn sync(&mut self, _fd: Resource<Descriptor>) -> Result<std::result::Result<(), ErrorCode>> {
        Ok(Ok(()))
    }
    fn set_times(
        &mut self,
        _fd: Resource<Descriptor>,
        _atim: NewTimestamp,
        _mtim: NewTimestamp,
    ) -> Result<std::result::Result<(), ErrorCode>> {
        Ok(Ok(()))
    }
    fn set_times_at(
        &mut self,
        _fd: Resource<Descriptor>,
        _path_flags: PathFlags,
        _path: String,
        _atim: NewTimestamp,
        _mtim: NewTimestamp,
    ) -> Result<std::result::Result<(), ErrorCode>> {
        Ok(Ok(()))
    }
    /// Create a hard link: a second directory entry pointing at the *same*
    /// node id as an existing file. No node is allocated (that is what makes it
    /// a hard link, as opposed to `symlink_at`). The node is freed only once
    /// its last directory entry is removed (see `unlink`).
    fn link_at(
        &mut self,
        fd: Resource<Descriptor>,
        old_path_flags: PathFlags,
        old_path: String,
        new_descriptor: Resource<Descriptor>,
        new_path: String,
    ) -> Result<std::result::Result<(), ErrorCode>> {
        let (fs, old_place) = fd_place(self, &fd)?;
        let (new_fs_arc, new_place) = fd_place(self, &new_descriptor)?;
        // Hard links can't cross filesystems (node ids are per-fs), and a
        // provider serves plain trees — no hard links across the boundary.
        if !Arc::ptr_eq(&fs, &new_fs_arc) {
            return err(ErrorCode::CrossDevice);
        }
        let (DescPlace::Local(old_start), DescPlace::Local(new_start)) = (old_place, new_place)
        else {
            return err(ErrorCode::Unsupported);
        };
        let mut g = fs.lock().unwrap();
        // POSIX link(2) does not follow a trailing symlink unless
        // AT_SYMLINK_FOLLOW (SYMLINK_FOLLOW) is set.
        let follow = old_path_flags.contains(PathFlags::SYMLINK_FOLLOW);
        let Some(src_id) = resolve_at(&g, old_start, &old_path, follow) else {
            return err(ErrorCode::NoEntry);
        };
        // Directories cannot be hard-linked; a read-only (layer) node cannot
        // gain a writable-namespace alias.
        match g.nodes.get(&src_id) {
            Some(Node::Dir(_)) => return err(ErrorCode::NotPermitted),
            None => return err(ErrorCode::NoEntry),
            Some(_) => {}
        }
        if g.readonly.contains(&src_id) {
            return err(ErrorCode::NotPermitted);
        }
        let Some((new_parent, new_name)) = resolve_parent(&g, new_start, &new_path) else {
            return err(ErrorCode::NoEntry);
        };
        match g.nodes.get(&new_parent) {
            Some(Node::Dir(children)) => {
                if children.contains_key(&new_name) {
                    return err(ErrorCode::Exist);
                }
            }
            _ => return err(ErrorCode::NotDirectory),
        }
        if let Some(Node::Dir(children)) = g.nodes.get_mut(&new_parent) {
            children.insert(new_name, src_id);
        }
        Ok(Ok(()))
    }
    /// Create a symlink at `dest_path` pointing at `src_path`. The target is
    /// stored verbatim (it may dangle, exactly as POSIX allows) and resolved
    /// on each traversal.
    fn symlink_at(
        &mut self,
        fd: Resource<Descriptor>,
        src_path: String,
        dest_path: String,
    ) -> Result<std::result::Result<(), ErrorCode>> {
        let (fs, place) = fd_place(self, &fd)?;
        // Providers serve plain trees: no symlink creation across the boundary.
        let DescPlace::Local(start) = place else {
            return err(ErrorCode::Unsupported);
        };
        let mut g = fs.lock().unwrap();
        if matches!(
            resolve_place(&g, start, &dest_path, false),
            Resolved::Remote { .. }
        ) {
            return err(ErrorCode::Unsupported);
        }
        let Some((parent, name)) = resolve_parent(&g, start, &dest_path) else {
            return err(ErrorCode::NoEntry);
        };
        if let Some(Node::Dir(children)) = g.nodes.get(&parent) {
            if children.contains_key(&name) {
                return err(ErrorCode::Exist);
            }
        } else {
            return err(ErrorCode::NotDirectory);
        }
        if g.at_capacity() {
            return err(ErrorCode::InsufficientSpace);
        }
        let id = g.alloc(Node::Symlink(src_path));
        if let Some(Node::Dir(children)) = g.nodes.get_mut(&parent) {
            children.insert(name, id);
        }
        Ok(Ok(()))
    }

    /// Read a symlink's target. The final component is *not* followed — that
    /// is the whole point — so this reports the link's own contents.
    fn readlink_at(
        &mut self,
        fd: Resource<Descriptor>,
        path: String,
    ) -> Result<std::result::Result<String, ErrorCode>> {
        let (fs, place) = fd_place(self, &fd)?;
        // Providers serve plain trees: nothing behind a mount is a symlink.
        let DescPlace::Local(start) = place else {
            return err(ErrorCode::Invalid);
        };
        let g = fs.lock().unwrap();
        let Some(id) = resolve_at(&g, start, &path, false) else {
            return err(ErrorCode::NoEntry);
        };
        match g.nodes.get(&id) {
            Some(Node::Symlink(target)) => Ok(Ok(target.clone())),
            Some(_) => err(ErrorCode::Invalid),
            None => err(ErrorCode::NoEntry),
        }
    }

    fn drop(&mut self, fd: Resource<Descriptor>) -> Result<()> {
        self.table().delete(fd)?;
        Ok(())
    }
}

/// Remove a file (`dir=false`) or empty directory (`dir=true`) at `path`.
fn unlink<T: VfsView>(
    view: &mut T,
    fd: Resource<Descriptor>,
    path: &str,
    dir: bool,
) -> Result<std::result::Result<(), ErrorCode>> {
    let (fs, place) = fd_place(view, &fd)?;
    // A removal behind a provider mount is the provider's to perform.
    let target = match &place {
        DescPlace::Local(start) => {
            let g = fs.lock().unwrap();
            match resolve_place(&g, *start, path, false) {
                Resolved::Remote {
                    conn,
                    path,
                    readonly,
                } => Some((conn, path, readonly)),
                _ => None,
            }
        }
        DescPlace::Remote(r) => Some((r.conn.clone(), remote_join(&r.path, path), r.readonly)),
    };
    if let Some((conn, rpath, readonly)) = target {
        if readonly {
            return err(ErrorCode::NotPermitted);
        }
        let op = if dir {
            FsOp::Rmdir { path: rpath }
        } else {
            FsOp::Unlink { path: rpath }
        };
        return match conn.call(op) {
            Ok(_) => Ok(Ok(())),
            Err(e) => err(provider_err(e)),
        };
    }
    let DescPlace::Local(start) = place else {
        return err(ErrorCode::NoEntry);
    };
    let mut g = fs.lock().unwrap();
    let Some((parent, name)) = resolve_parent(&g, start, path) else {
        return err(ErrorCode::NoEntry);
    };
    let id = match g.nodes.get(&parent) {
        Some(Node::Dir(c)) => match c.get(&name) {
            Some(id) => *id,
            None => return err(ErrorCode::NoEntry),
        },
        _ => return err(ErrorCode::NotDirectory),
    };
    // A read-only mount can't be removed from inside the guest (its binding is
    // the host's decision, like EROFS).
    if g.readonly.contains(&id) {
        return err(ErrorCode::NotPermitted);
    }
    match (dir, g.nodes.get(&id)) {
        (true, Some(Node::Dir(c))) if !c.is_empty() => return err(ErrorCode::NotEmpty),
        (true, Some(Node::Dir(_))) => {}
        (true, _) => return err(ErrorCode::NotDirectory),
        (false, Some(Node::Dir(_))) | (false, None) => return err(ErrorCode::IsDirectory),
        (false, _) => {}
    }
    // Remove the directory entry first, then free the node only if no other
    // entry still references it — hard links (see `link_at`) share a node id,
    // so the backing content must survive until the last name is gone.
    if let Some(Node::Dir(c)) = g.nodes.get_mut(&parent) {
        c.remove(&name);
    }
    // Free the content only once no name AND no open descriptor remains. A file
    // still held open (POSIX unlinked-but-open — `tac`/`sort` rely on it) lives
    // on until its last descriptor drops (see `Fs::close_ref`).
    if !node_is_referenced(&g, id) && g.open_count(id) == 0 {
        g.nodes.remove(&id);
    }
    Ok(Ok(()))
}

/// True if any directory entry still maps to `id` (i.e. the node has remaining
/// hard links). Linear scan over all nodes; only the cold unlink path calls it.
fn node_is_referenced(fs: &Fs, id: u64) -> bool {
    fs.nodes.values().any(|n| match n {
        Node::Dir(children) => children.values().any(|&v| v == id),
        _ => false,
    })
}

fn stat_node(fs: &Fs, id: u64) -> Option<DescriptorStat> {
    let (ty, size, mtime) = match fs.nodes.get(&id)? {
        Node::File(data) => (DescriptorType::RegularFile, data.len() as u64, None),
        Node::RoFile(data) => (DescriptorType::RegularFile, data.len() as u64, None),
        Node::Dir(_) => (DescriptorType::Directory, 0, None),
        Node::Shared(sh) => (
            DescriptorType::RegularFile,
            sh.lock().unwrap().len() as u64,
            None,
        ),
        Node::Host(p) => (DescriptorType::RegularFile, host_size(p), host_mtime(p)),
        Node::Symlink(target) => (DescriptorType::SymbolicLink, target.len() as u64, None),
        Node::Null | Node::Zero | Node::Random => (DescriptorType::CharacterDevice, 0, None),
        // The mount point itself stats as a directory; anything *inside* is
        // resolved remotely and never reaches this local-id path.
        Node::Provider(_) => (DescriptorType::Directory, 0, None),
    };
    Some(DescriptorStat {
        type_: ty,
        link_count: 1,
        size,
        data_access_timestamp: None,
        data_modification_timestamp: mtime,
        status_change_timestamp: mtime,
    })
}

impl<T: VfsView> wasi::filesystem::types::HostDirectoryEntryStream for VfsImpl<T> {
    fn read_directory_entry(
        &mut self,
        stream: Resource<DirEntryStream>,
    ) -> Result<std::result::Result<Option<DirectoryEntry>, ErrorCode>> {
        let s = self.table().get_mut(&stream)?;
        let entry = s.entries.get(s.pos).cloned();
        if entry.is_some() {
            s.pos += 1;
        }
        Ok(Ok(entry))
    }
    fn drop(&mut self, stream: Resource<DirEntryStream>) -> Result<()> {
        self.table().delete(stream)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Eager layer bytes for tests that plant `RoFile`s directly.
    fn ro_bytes(data: &[u8]) -> Arc<layers::LayerBytes> {
        Arc::new(layers::LayerBytes::eager(Arc::new(data.to_vec())))
    }

    /// A minimal store so tests can drive the generated `wasi:filesystem`
    /// methods (the same surface a guest hits) without a live wasm instance.
    struct TestStore {
        table: ResourceTable,
        fs: SharedFs,
    }
    impl wasmtime_wasi_io::IoView for TestStore {
        fn table(&mut self) -> &mut ResourceTable {
            &mut self.table
        }
    }
    impl VfsView for TestStore {
        fn fs(&mut self) -> SharedFs {
            self.fs.clone()
        }
    }

    /// A minimal in-memory provider: a map of path -> bytes served over a
    /// `ProviderConn` on its own thread, the way a wk:fs guest node would.
    /// Returns the join handle and a stop flag.
    fn spawn_memfs_provider(
        conn: Arc<ProviderConn>,
        seed: &[(&str, &[u8])],
    ) -> (
        std::thread::JoinHandle<()>,
        Arc<std::sync::atomic::AtomicBool>,
    ) {
        use std::sync::atomic::{AtomicBool, Ordering};
        let stop = Arc::new(AtomicBool::new(false));
        let files: HashMap<String, Vec<u8>> = seed
            .iter()
            .map(|(p, b)| (p.to_string(), b.to_vec()))
            .collect();
        let stop2 = stop.clone();
        conn.begin_serving();
        let t = std::thread::spawn(move || {
            let mut files = files;
            let mut dirs: HashSet<String> = HashSet::new();
            let mut handles: HashMap<u64, String> = HashMap::new();
            let mut next_handle = 1u64;
            let is_dir = |files: &HashMap<String, Vec<u8>>, dirs: &HashSet<String>, p: &str| {
                p.is_empty()
                    || dirs.contains(p)
                    || files.keys().any(|f| f.starts_with(&format!("{p}/")))
            };
            while !stop2.load(Ordering::Relaxed) {
                let Some((id, op)) = conn.next_request(std::time::Duration::from_millis(20)) else {
                    continue;
                };
                let reply = match op {
                    FsOp::Getattr { path } => {
                        if is_dir(&files, &dirs, &path) {
                            Ok(FsReplyData::Attr(FsStat {
                                kind: FsEntryKind::Dir,
                                size: 0,
                            }))
                        } else if let Some(b) = files.get(&path) {
                            Ok(FsReplyData::Attr(FsStat {
                                kind: FsEntryKind::File,
                                size: b.len() as u64,
                            }))
                        } else {
                            Err(FsError::NoEntry)
                        }
                    }
                    FsOp::Readdir { path } => {
                        if !is_dir(&files, &dirs, &path) {
                            Err(FsError::NotDir)
                        } else {
                            let prefix = if path.is_empty() {
                                String::new()
                            } else {
                                format!("{path}/")
                            };
                            let mut seen = HashSet::new();
                            let mut entries = Vec::new();
                            for f in files.keys().chain(dirs.iter()) {
                                let Some(rest) = f.strip_prefix(&prefix) else {
                                    continue;
                                };
                                if rest.is_empty() {
                                    continue;
                                }
                                let first = rest.split('/').next().unwrap();
                                if seen.insert(first.to_string()) {
                                    let full = format!("{prefix}{first}");
                                    entries.push(FsDirent {
                                        name: first.to_string(),
                                        kind: if is_dir(&files, &dirs, &full)
                                            && !files.contains_key(&full)
                                        {
                                            FsEntryKind::Dir
                                        } else {
                                            FsEntryKind::File
                                        },
                                    });
                                }
                            }
                            Ok(FsReplyData::Entries(entries))
                        }
                    }
                    FsOp::Open {
                        path,
                        create,
                        truncate,
                        exclusive,
                    } => {
                        let exists = files.contains_key(&path) || is_dir(&files, &dirs, &path);
                        if exists && exclusive {
                            Err(FsError::Exist)
                        } else if !exists && !create {
                            Err(FsError::NoEntry)
                        } else {
                            if !exists || (truncate && files.contains_key(&path)) {
                                files.insert(path.clone(), Vec::new());
                            }
                            let kind = if files.contains_key(&path) {
                                FsEntryKind::File
                            } else {
                                FsEntryKind::Dir
                            };
                            let size = files.get(&path).map_or(0, |b| b.len() as u64);
                            let handle = next_handle;
                            next_handle += 1;
                            handles.insert(handle, path);
                            Ok(FsReplyData::Opened(FsOpened { handle, kind, size }))
                        }
                    }
                    FsOp::Read {
                        handle,
                        offset,
                        len,
                    } => match handles.get(&handle).and_then(|p| files.get(p)) {
                        Some(b) => {
                            let (bytes, eof) = read_at(b, offset, len as u64);
                            Ok(FsReplyData::Data { bytes, eof })
                        }
                        None => Err(FsError::NoEntry),
                    },
                    FsOp::Write {
                        handle,
                        offset,
                        data,
                    } => match handles.get(&handle).cloned() {
                        Some(p) => {
                            let b = files.entry(p).or_default();
                            let n = data.len() as u64;
                            write_at(b, offset, &data).unwrap();
                            Ok(FsReplyData::Written(n))
                        }
                        None => Err(FsError::NoEntry),
                    },
                    FsOp::Release { handle } => {
                        handles.remove(&handle);
                        Ok(FsReplyData::Done)
                    }
                    FsOp::SetSize { handle, size } => match handles.get(&handle).cloned() {
                        Some(p) => {
                            files.entry(p).or_default().resize(size as usize, 0);
                            Ok(FsReplyData::Done)
                        }
                        None => Err(FsError::NoEntry),
                    },
                    FsOp::Mkdir { path } => {
                        dirs.insert(path);
                        Ok(FsReplyData::Done)
                    }
                    FsOp::Unlink { path } => match files.remove(&path) {
                        Some(_) => Ok(FsReplyData::Done),
                        None => Err(FsError::NoEntry),
                    },
                    FsOp::Rmdir { path } => {
                        if dirs.remove(&path) {
                            Ok(FsReplyData::Done)
                        } else {
                            Err(FsError::NoEntry)
                        }
                    }
                    FsOp::Rename { from, to } => match files.remove(&from) {
                        Some(b) => {
                            files.insert(to, b);
                            Ok(FsReplyData::Done)
                        }
                        None => Err(FsError::NoEntry),
                    },
                };
                conn.reply(id, reply);
            }
        });
        (t, stop)
    }

    /// The FUSE loop end to end: a provider node's tree mounted at /mnt/p,
    /// driven through the same `wasi:filesystem` surface a guest hits —
    /// lookup, readdir, read, write-back, mkdir, unlink, and death semantics.
    #[test]
    fn provider_mount_serves_a_live_subtree() {
        use std::sync::atomic::Ordering;
        use wasi::filesystem::types::HostDescriptor;

        let conn = ProviderConn::new();
        let (t, stop) = spawn_memfs_provider(
            conn.clone(),
            &[
                ("hello.txt", b"served by another node"),
                ("sub/inner.txt", b"nested"),
            ],
        );

        let fs = new_fs();
        fs.lock()
            .unwrap()
            .put_file_at("local.txt", b"local".to_vec());
        mount_provider(&fs, "/mnt/p", conn.clone(), true);

        let mut store = VfsImpl(TestStore {
            table: ResourceTable::new(),
            fs: fs.clone(),
        });
        let root = store
            .0
            .table
            .push(Descriptor::open(fs.clone(), ROOT))
            .unwrap();
        let root_fd = || Resource::<Descriptor>::new_own(root.rep());

        // stat_at through the mount reaches the provider.
        let st = HostDescriptor::stat_at(
            &mut store,
            root_fd(),
            PathFlags::SYMLINK_FOLLOW,
            "mnt/p/hello.txt".into(),
        )
        .unwrap()
        .expect("stat crosses the mount");
        assert_eq!(st.type_, DescriptorType::RegularFile);
        assert_eq!(st.size, 22);

        // Open + read a provider file.
        let fd = HostDescriptor::open_at(
            &mut store,
            root_fd(),
            PathFlags::SYMLINK_FOLLOW,
            "mnt/p/hello.txt".into(),
            OpenFlags::empty(),
            DescriptorFlags::empty(),
        )
        .unwrap()
        .expect("opens across the mount");
        let (bytes, eof) = HostDescriptor::read(&mut store, Resource::new_own(fd.rep()), 64, 0)
            .unwrap()
            .expect("reads");
        assert_eq!(bytes, b"served by another node");
        assert!(eof);

        // Write back through the same descriptor; the provider sees it.
        HostDescriptor::write(
            &mut store,
            Resource::new_own(fd.rep()),
            b"SERVED".to_vec(),
            0,
        )
        .unwrap()
        .expect("writes");
        let (bytes, _) = HostDescriptor::read(&mut store, Resource::new_own(fd.rep()), 6, 0)
            .unwrap()
            .expect("re-reads");
        assert_eq!(bytes, b"SERVED");

        // readdir of the mount root lists provider entries; a nested dir walks.
        let dirfd = HostDescriptor::open_at(
            &mut store,
            root_fd(),
            PathFlags::SYMLINK_FOLLOW,
            "mnt/p".into(),
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
            use wasi::filesystem::types::HostDirectoryEntryStream;
            match HostDirectoryEntryStream::read_directory_entry(
                &mut store,
                Resource::new_own(stream.rep()),
            )
            .unwrap()
            .unwrap()
            {
                Some(e) => names.push(e.name),
                None => break,
            }
        }
        names.sort();
        assert_eq!(names, ["hello.txt", "sub"]);
        assert_eq!(
            HostDescriptor::stat_at(
                &mut store,
                root_fd(),
                PathFlags::SYMLINK_FOLLOW,
                "mnt/p/sub/inner.txt".into(),
            )
            .unwrap()
            .expect("nested stat")
            .size,
            6
        );

        // Create, mkdir, unlink — all forwarded.
        HostDescriptor::open_at(
            &mut store,
            root_fd(),
            PathFlags::SYMLINK_FOLLOW,
            "mnt/p/new.txt".into(),
            OpenFlags::CREATE,
            DescriptorFlags::empty(),
        )
        .unwrap()
        .expect("creates remotely");
        HostDescriptor::create_directory_at(&mut store, root_fd(), "mnt/p/made".into())
            .unwrap()
            .expect("mkdir remotely");
        HostDescriptor::unlink_file_at(&mut store, root_fd(), "mnt/p/new.txt".into())
            .unwrap()
            .expect("unlinks remotely");

        // Local files are untouched by all of this.
        assert_eq!(
            fs.lock().unwrap().read_file("/local.txt", 64).as_deref(),
            Some(&b"local"[..])
        );

        // Provider death: in-flight and future ops fail as EIO, fast.
        stop.store(true, Ordering::Relaxed);
        t.join().unwrap();
        conn.end_serving();
        assert_eq!(
            HostDescriptor::stat_at(
                &mut store,
                root_fd(),
                PathFlags::SYMLINK_FOLLOW,
                "mnt/p/hello.txt".into(),
            )
            .unwrap()
            .unwrap_err(),
            ErrorCode::Io
        );
        // A handle from the dead incarnation is refused, not replayed.
        assert_eq!(
            HostDescriptor::read(&mut store, fd, 4, 0)
                .unwrap()
                .unwrap_err(),
            ErrorCode::Io
        );
    }

    /// A read-only provider mount refuses every mutation host-side.
    #[test]
    fn read_only_provider_mount_refuses_mutations() {
        use std::sync::atomic::Ordering;
        use wasi::filesystem::types::HostDescriptor;

        let conn = ProviderConn::new();
        let (t, stop) = spawn_memfs_provider(conn.clone(), &[("f.txt", b"ro")]);
        let fs = new_fs();
        mount_provider(&fs, "/mnt/ro", conn.clone(), false);

        let mut store = VfsImpl(TestStore {
            table: ResourceTable::new(),
            fs: fs.clone(),
        });
        let root = store
            .0
            .table
            .push(Descriptor::open(fs.clone(), ROOT))
            .unwrap();
        let root_fd = || Resource::<Descriptor>::new_own(root.rep());

        let fd = HostDescriptor::open_at(
            &mut store,
            root_fd(),
            PathFlags::SYMLINK_FOLLOW,
            "mnt/ro/f.txt".into(),
            OpenFlags::empty(),
            DescriptorFlags::empty(),
        )
        .unwrap()
        .expect("read-only open works");
        let (bytes, _) = HostDescriptor::read(&mut store, Resource::new_own(fd.rep()), 8, 0)
            .unwrap()
            .expect("reads");
        assert_eq!(bytes, b"ro");
        assert_eq!(
            HostDescriptor::write(&mut store, Resource::new_own(fd.rep()), b"x".to_vec(), 0)
                .unwrap()
                .unwrap_err(),
            ErrorCode::NotPermitted
        );
        assert_eq!(
            HostDescriptor::open_at(
                &mut store,
                root_fd(),
                PathFlags::SYMLINK_FOLLOW,
                "mnt/ro/new.txt".into(),
                OpenFlags::CREATE,
                DescriptorFlags::empty(),
            )
            .unwrap()
            .unwrap_err(),
            ErrorCode::NotPermitted
        );
        assert_eq!(
            HostDescriptor::unlink_file_at(&mut store, root_fd(), "mnt/ro/f.txt".into())
                .unwrap()
                .unwrap_err(),
            ErrorCode::NotPermitted
        );

        stop.store(true, Ordering::Relaxed);
        t.join().unwrap();
    }

    /// The UI's background browse in one test: crossing detection routes a
    /// path, forwarded listings and previews reach through the mount, and
    /// local paths still answer locally.
    #[test]
    fn forwarded_listing_and_preview_cross_the_mount() {
        use std::sync::atomic::Ordering;
        let conn = ProviderConn::new();
        let (t, stop) = spawn_memfs_provider(
            conn.clone(),
            &[("hello.txt", b"served"), ("sub/inner.txt", b"nested")],
        );
        let fs = new_fs();
        fs.lock()
            .unwrap()
            .put_file_at("local.txt", b"local".to_vec());
        mount_provider(&fs, "/mnt/p", conn.clone(), true);

        assert!(!path_crosses_provider(&fs, ""));
        assert!(!path_crosses_provider(&fs, "local.txt"));
        assert!(path_crosses_provider(&fs, "/mnt/p"));
        assert!(path_crosses_provider(&fs, "/mnt/p/sub/inner.txt"));

        // The mount root lists remotely: dirs first, files sized via getattr.
        let list = list_dir_forwarded(&fs, "/mnt/p").expect("lists the mount");
        let shape: Vec<(&str, bool, usize)> = list
            .iter()
            .map(|e| (e.name.as_str(), e.is_dir, e.size))
            .collect();
        assert_eq!(shape, [("sub", true, 0), ("hello.txt", false, 6)]);
        let nested = list_dir_forwarded(&fs, "/mnt/p/sub").expect("lists nested");
        assert_eq!(nested.len(), 1);
        assert_eq!(nested[0].name, "inner.txt");

        // A local path goes straight to the local listing.
        assert!(list_dir_forwarded(&fs, "")
            .expect("local root lists")
            .iter()
            .any(|e| e.name == "local.txt"));

        // Previews: through the mount (with the cap honored) and local.
        assert_eq!(
            read_file_forwarded(&fs, "/mnt/p/hello.txt", 64).as_deref(),
            Some(&b"served"[..])
        );
        assert_eq!(
            read_file_forwarded(&fs, "/mnt/p/hello.txt", 3).as_deref(),
            Some(&b"ser"[..])
        );
        assert_eq!(
            read_file_forwarded(&fs, "local.txt", 64).as_deref(),
            Some(&b"local"[..])
        );

        stop.store(true, Ordering::Relaxed);
        t.join().unwrap();
    }

    #[test]
    fn a_file_stream_hands_back_bounded_chunks() {
        // A large module once crashed loading because `read_via_stream` returned
        // the whole file in one oversized transfer. `BoundedFileStream` caps each
        // read; the guest loops for the rest.
        let size = 100 * 1024;
        let mut s = BoundedFileStream {
            bytes: Bytes::from(vec![7u8; size]),
            pos: 0,
        };
        let mut total = 0usize;
        let mut reads = 0usize;
        loop {
            match s.read(1 << 30) {
                Ok(b) => {
                    assert!(
                        b.len() <= FILE_READ_CHUNK,
                        "one read exceeded the chunk cap"
                    );
                    assert!(!b.is_empty(), "a non-EOF read returned nothing");
                    total += b.len();
                    reads += 1;
                }
                Err(StreamError::Closed) => break,
                Err(e) => panic!("unexpected stream error: {e:?}"),
            }
        }
        assert_eq!(total, size, "every byte is delivered across the chunks");
        assert!(
            reads >= size / FILE_READ_CHUNK,
            "large file read in >1 chunk"
        );
    }

    #[test]
    fn read_only_mount_reads_but_refuses_every_mutation() {
        use wasi::filesystem::types::HostDescriptor;
        let fs = new_fs();
        let data: SharedFile = Arc::new(Mutex::new(b"shared".to_vec()));
        mount_file(&fs, "ro.txt", data.clone(), false);

        let mut store = VfsImpl(TestStore {
            table: ResourceTable::new(),
            fs: fs.clone(),
        });
        let root = store
            .0
            .table
            .push(Descriptor::open(fs.clone(), ROOT))
            .unwrap();
        let open = |st: &mut VfsImpl<TestStore>, root: &Resource<Descriptor>, of: OpenFlags| {
            HostDescriptor::open_at(
                st,
                Resource::new_own(root.rep()),
                PathFlags::empty(),
                "ro.txt".to_string(),
                of,
                DescriptorFlags::empty(),
            )
            .unwrap()
        };

        // Plain open + read works and the flags report read-only.
        let fd = open(&mut store, &root, OpenFlags::empty()).expect("opens");
        let (bytes, _) = HostDescriptor::read(&mut store, Resource::new_own(fd.rep()), 64, 0)
            .unwrap()
            .expect("reads");
        assert_eq!(bytes, b"shared");
        assert_eq!(
            HostDescriptor::get_flags(&mut store, Resource::new_own(fd.rep()))
                .unwrap()
                .expect("flags"),
            DescriptorFlags::READ
        );

        // Every mutation path refuses: direct write, write/append streams,
        // resize, truncate-open, unlink, rename.
        assert_eq!(
            HostDescriptor::write(&mut store, Resource::new_own(fd.rep()), b"x".to_vec(), 0)
                .unwrap()
                .unwrap_err(),
            ErrorCode::NotPermitted
        );
        assert!(matches!(
            HostDescriptor::write_via_stream(&mut store, Resource::new_own(fd.rep()), 0).unwrap(),
            Err(ErrorCode::NotPermitted)
        ));
        assert!(matches!(
            HostDescriptor::append_via_stream(&mut store, Resource::new_own(fd.rep())).unwrap(),
            Err(ErrorCode::NotPermitted)
        ));
        assert_eq!(
            HostDescriptor::set_size(&mut store, Resource::new_own(fd.rep()), 0)
                .unwrap()
                .unwrap_err(),
            ErrorCode::NotPermitted
        );
        assert!(matches!(
            open(&mut store, &root, OpenFlags::TRUNCATE),
            Err(ErrorCode::NotPermitted)
        ));
        assert_eq!(
            HostDescriptor::unlink_file_at(
                &mut store,
                Resource::new_own(root.rep()),
                "ro.txt".to_string()
            )
            .unwrap()
            .unwrap_err(),
            ErrorCode::NotPermitted
        );
        assert_eq!(
            HostDescriptor::rename_at(
                &mut store,
                Resource::new_own(root.rep()),
                "ro.txt".to_string(),
                Resource::new_own(root.rep()),
                "moved.txt".to_string()
            )
            .unwrap()
            .unwrap_err(),
            ErrorCode::NotPermitted
        );

        // The shared bytes never changed, and a writable mount of the same
        // volume elsewhere still works (read-only is per-mount, not per-volume).
        assert_eq!(&*data.lock().unwrap(), b"shared");
        let rw = new_fs();
        mount_file(&rw, "rw.txt", data.clone(), true);
        let mut store2 = VfsImpl(TestStore {
            table: ResourceTable::new(),
            fs: rw.clone(),
        });
        let root2 = store2
            .0
            .table
            .push(Descriptor::open(rw.clone(), ROOT))
            .unwrap();
        let fd2 = HostDescriptor::open_at(
            &mut store2,
            root2,
            PathFlags::empty(),
            "rw.txt".to_string(),
            OpenFlags::empty(),
            DescriptorFlags::empty(),
        )
        .unwrap()
        .expect("opens rw");
        HostDescriptor::write(&mut store2, fd2, b"W".to_vec(), 0)
            .unwrap()
            .expect("writes through the rw mount");
        assert_eq!(data.lock().unwrap()[0], b'W');
    }

    /// Symlinks resolve on traversal, report themselves to lstat/readlink,
    /// survive being pointed at directories, and can't loop forever. This is
    /// how a multicall binary provides its command names: one executable,
    /// many links.
    #[test]
    fn symlinks_resolve_and_bound_loops() {
        use wasi::filesystem::types::HostDescriptor;
        let fs = new_fs();
        {
            let mut g = fs.lock().unwrap();
            g.ensure_dir_path("bin");
            g.put_file_at("bin/coreutils.wasm", b"\0asm-the-real-binary".to_vec());
            // One binary, many names — exactly a busybox/coreutils install.
            g.put_symlink_at("bin/ls", "coreutils.wasm".into());
            g.put_symlink_at("bin/cat", "/bin/coreutils.wasm".into());
            // A directory alias, and a cycle.
            g.put_symlink_at("usr", "/bin".into());
            g.put_symlink_at("loop-a", "loop-b".into());
            g.put_symlink_at("loop-b", "loop-a".into());
        }

        // Relative and absolute links both reach the binary...
        assert_eq!(
            fs.lock().unwrap().read_file("/bin/ls", 64).as_deref(),
            Some(&b"\0asm-the-real-binary"[..])
        );
        assert_eq!(
            fs.lock().unwrap().read_file("/bin/cat", 64).as_deref(),
            Some(&b"\0asm-the-real-binary"[..])
        );
        // ...including through a symlinked directory component.
        assert_eq!(
            fs.lock().unwrap().read_file("/usr/ls", 64).as_deref(),
            Some(&b"\0asm-the-real-binary"[..])
        );
        // A cycle terminates instead of hanging.
        assert!(fs.lock().unwrap().read_file("/loop-a", 64).is_none());

        let mut store = VfsImpl(TestStore {
            table: ResourceTable::new(),
            fs: fs.clone(),
        });
        let root = store
            .0
            .table
            .push(Descriptor::open(fs.clone(), ROOT))
            .unwrap();

        // readlink reports the link itself, never its target's contents.
        assert_eq!(
            HostDescriptor::readlink_at(&mut store, Resource::new_own(root.rep()), "bin/ls".into())
                .unwrap()
                .unwrap(),
            "coreutils.wasm"
        );
        // lstat sees a link; stat (follow) sees the file behind it.
        let lst = HostDescriptor::stat_at(
            &mut store,
            Resource::new_own(root.rep()),
            PathFlags::empty(),
            "bin/ls".into(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(lst.type_, DescriptorType::SymbolicLink);
        let st = HostDescriptor::stat_at(
            &mut store,
            Resource::new_own(root.rep()),
            PathFlags::SYMLINK_FOLLOW,
            "bin/ls".into(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(st.type_, DescriptorType::RegularFile);

        // A guest can create links too, and reading through one works.
        HostDescriptor::symlink_at(
            &mut store,
            Resource::new_own(root.rep()),
            "/bin/coreutils.wasm".into(),
            "bin/wc".into(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            fs.lock().unwrap().read_file("/bin/wc", 64).as_deref(),
            Some(&b"\0asm-the-real-binary"[..])
        );
        // readlink on something that isn't a link is an error, not a guess.
        assert!(HostDescriptor::readlink_at(
            &mut store,
            Resource::new_own(root.rep()),
            "bin/coreutils.wasm".into()
        )
        .unwrap()
        .is_err());
    }

    #[test]
    fn apps_are_isolated() {
        // Two fresh filesystems share nothing.
        let a = new_fs();
        let b = new_fs();
        a.lock()
            .unwrap()
            .add_child(ROOT, "secret", Node::File(b"x".to_vec()));
        assert!(resolve(&a.lock().unwrap(), ROOT, "/secret").is_some());
        assert!(resolve(&b.lock().unwrap(), ROOT, "/secret").is_none());
    }

    #[test]
    fn unlinked_but_open_file_survives_until_last_close() {
        // POSIX unlinked-but-open semantics: `tac`/`sort` mkstemp a scratch
        // file, unlink it while the fd is open, then keep writing/seeking it.
        let fs = new_fs();
        let mut g = fs.lock().unwrap();
        g.add_child(ROOT, "scratch", Node::File(b"data".to_vec()));
        let id = resolve(&g, ROOT, "/scratch").unwrap();

        // Two open descriptors, then drop the only directory entry (unlink).
        g.open_ref(id);
        g.open_ref(id);
        if let Some(Node::Dir(children)) = g.nodes.get_mut(&ROOT) {
            children.remove("scratch");
        }
        assert!(!node_is_referenced(&g, id));

        // First close: another descriptor is still open, so content is intact.
        g.close_ref(id);
        assert_eq!(g.open_count(id), 1);
        assert!(matches!(g.nodes.get(&id), Some(Node::File(d)) if d == b"data"));

        // Last close: the orphan is finally freed.
        g.close_ref(id);
        assert_eq!(g.open_count(id), 0);
        assert!(!g.nodes.contains_key(&id));
    }

    #[test]
    fn provisions_a_null_device() {
        let fs = new_fs();
        ensure_standard_devices(&fs);
        let g = fs.lock().unwrap();
        let id = resolve(&g, ROOT, "/dev/null").expect("/dev/null exists");
        assert!(matches!(g.nodes.get(&id), Some(Node::Null)));
        assert_eq!(node_type(&g, id), DescriptorType::CharacterDevice);

        // The full standard device set, each a character device.
        for (path, want_zero) in [
            ("/dev/zero", true),
            ("/dev/urandom", false),
            ("/dev/random", false),
        ] {
            let id = resolve(&g, ROOT, path).unwrap_or_else(|| panic!("{path} exists"));
            assert_eq!(node_type(&g, id), DescriptorType::CharacterDevice);
            assert_eq!(
                matches!(g.nodes.get(&id), Some(Node::Zero)),
                want_zero,
                "{path}"
            );
        }
    }

    #[test]
    fn ensure_standard_devices_is_idempotent_and_keeps_existing() {
        let fs = new_fs();
        // An image already shipped its own /dev/null as a regular file.
        {
            let mut g = fs.lock().unwrap();
            let dev = g.ensure_dir_path("dev").unwrap();
            let existing = g.alloc(Node::File(b"real".to_vec()));
            if let Some(Node::Dir(c)) = g.nodes.get_mut(&dev) {
                c.insert("null".to_string(), existing);
            }
        }
        ensure_standard_devices(&fs);
        let g = fs.lock().unwrap();
        let id = resolve(&g, ROOT, "/dev/null").unwrap();
        // Left the layer's file alone rather than replacing it with a device.
        assert!(matches!(g.nodes.get(&id), Some(Node::File(_))));
    }

    #[test]
    fn closing_a_still_linked_file_keeps_it() {
        // A descriptor closing must not free a node that still has a name.
        let fs = new_fs();
        let mut g = fs.lock().unwrap();
        g.add_child(ROOT, "keep", Node::File(b"x".to_vec()));
        let id = resolve(&g, ROOT, "/keep").unwrap();
        g.open_ref(id);
        g.close_ref(id);
        assert!(g.nodes.contains_key(&id));
    }

    #[test]
    fn inspection_lists_dirs_first_and_reads_files() {
        let fs = new_fs();
        {
            let mut g = fs.lock().unwrap();
            g.add_child(ROOT, "readme", Node::File(b"hello world".to_vec()));
            g.add_child(ROOT, "sub", Node::Dir(BTreeMap::new()));
            let sub = resolve(&g, ROOT, "/sub").unwrap();
            g.add_child(sub, "nested.txt", Node::File(b"deep".to_vec()));
        }
        let g = fs.lock().unwrap();

        // Root: directory first, then file, each with its size and origin (a
        // plain File is a private write; the UI badges these).
        let root = g.list_dir("").expect("root is a dir");
        assert_eq!(
            root,
            vec![
                DirEntry {
                    name: "sub".into(),
                    is_dir: true,
                    size: 0,
                    origin: PathKind::Dir,
                },
                DirEntry {
                    name: "readme".into(),
                    is_dir: false,
                    size: 11,
                    origin: PathKind::PrivateFile,
                },
            ]
        );
        // Descend and read.
        assert_eq!(g.list_dir("/sub").unwrap().len(), 1);
        assert_eq!(
            g.read_file("/readme", 1024).as_deref(),
            Some(&b"hello world"[..])
        );
        assert_eq!(
            g.read_file("/sub/nested.txt", 1024).as_deref(),
            Some(&b"deep"[..])
        );
        // A directory isn't readable as a file; a file isn't listable as a dir.
        assert!(g.read_file("/sub", 16).is_none());
        assert!(g.list_dir("/readme").is_none());
        // Preview is capped.
        assert_eq!(g.read_file("/readme", 4).as_deref(), Some(&b"hell"[..]));
    }

    #[test]
    fn listing_reports_each_entrys_origin() {
        let fs = new_fs();
        let shared: SharedFile = Arc::new(Mutex::new(b"chan".to_vec()));
        {
            let mut g = fs.lock().unwrap();
            g.put_ro_file_at("from-layer.txt", ro_bytes(b"ro"));
            g.add_child(ROOT, "written.txt", Node::File(b"w".to_vec()));
            g.add_child(ROOT, "chan", Node::Shared(shared.clone()));
        }
        let g = fs.lock().unwrap();
        let by_name = |n: &str| {
            g.list_dir("")
                .unwrap()
                .into_iter()
                .find(|e| e.name == n)
                .unwrap()
        };
        assert_eq!(by_name("from-layer.txt").origin, PathKind::LayerFile);
        assert_eq!(by_name("written.txt").origin, PathKind::PrivateFile);
        assert_eq!(by_name("chan").origin, PathKind::Mounted);
    }

    #[test]
    fn connected_file_is_shared_then_unmounted() {
        let a = new_fs();
        let b = new_fs();
        let data: SharedFile = Arc::new(Mutex::new(Vec::new()));

        // Wiring the same file node into both apps gives both a shared file.
        mount_file(&a, "chan", data.clone(), true);
        mount_file(&b, "chan", data.clone(), true);
        let na = resolve(&a.lock().unwrap(), ROOT, "/chan").expect("a sees it");
        let nb = resolve(&b.lock().unwrap(), ROOT, "/chan").expect("b sees it");

        // One app writes the shared bytes; the other sees them.
        data.lock().unwrap().extend_from_slice(b"hello");
        assert_eq!(stat_node(&a.lock().unwrap(), na).unwrap().size, 5);
        assert_eq!(stat_node(&b.lock().unwrap(), nb).unwrap().size, 5);

        // Disconnecting one app leaves the other connected.
        unmount_file(&a, "chan");
        assert!(resolve(&a.lock().unwrap(), ROOT, "/chan").is_none());
        assert!(resolve(&b.lock().unwrap(), ROOT, "/chan").is_some());
    }

    #[test]
    fn mounts_at_a_nested_path_creating_parents() {
        let fs = new_fs();
        let data: SharedFile = Arc::new(Mutex::new(b"hi".to_vec()));
        // A volume can bind at a chosen path deep in the tree; parents appear.
        mount_file(&fs, "/data/inputs/notes.txt", data.clone(), true);
        let id = resolve(&fs.lock().unwrap(), ROOT, "/data/inputs/notes.txt")
            .expect("mounted at the nested path");
        assert_eq!(stat_node(&fs.lock().unwrap(), id).unwrap().size, 2);
        // Same bytes are shared: an external write is visible through the mount.
        data.lock().unwrap().extend_from_slice(b" there");
        assert_eq!(stat_node(&fs.lock().unwrap(), id).unwrap().size, 8);
        // Unmounting removes the leaf (the parent dirs are harmless leftovers).
        unmount_file(&fs, "/data/inputs/notes.txt");
        assert!(resolve(&fs.lock().unwrap(), ROOT, "/data/inputs/notes.txt").is_none());
        assert!(resolve(&fs.lock().unwrap(), ROOT, "/data/inputs").is_some());
    }

    #[test]
    fn ro_file_reads_and_stats_like_a_file() {
        // A layer-backed read-only file is indistinguishable from a private one
        // on the read paths: size, preview, listing.
        let fs = new_fs();
        let bytes: Arc<Vec<u8>> = Arc::new(b"from a layer".to_vec());
        fs.lock().unwrap().add_child(
            ROOT,
            "ro",
            Node::RoFile(Arc::new(layers::LayerBytes::eager(bytes.clone()))),
        );
        let g = fs.lock().unwrap();
        let node = resolve(&g, ROOT, "/ro").expect("resolves");
        assert_eq!(stat_node(&g, node).unwrap().size, 12);
        assert_eq!(
            g.read_file("/ro", 1024).as_deref(),
            Some(&b"from a layer"[..])
        );
        assert_eq!(g.list_dir("").unwrap()[0].size, 12);
    }

    #[test]
    fn copy_up_detaches_from_the_shared_layer() {
        // Two filesystems share one layer file (same Arc). Writing in one
        // copies up to a private file; the other still sees the layer bytes,
        // and the layer itself is never mutated.
        let bytes: Arc<Vec<u8>> = Arc::new(b"immutable".to_vec());
        let a = new_fs();
        let b = new_fs();
        a.lock().unwrap().add_child(
            ROOT,
            "f",
            Node::RoFile(Arc::new(layers::LayerBytes::eager(bytes.clone()))),
        );
        b.lock().unwrap().add_child(
            ROOT,
            "f",
            Node::RoFile(Arc::new(layers::LayerBytes::eager(bytes.clone()))),
        );

        {
            let mut g = a.lock().unwrap();
            let id = resolve(&g, ROOT, "/f").unwrap();
            g.copy_up(id);
            match g.nodes.get_mut(&id) {
                Some(Node::File(data)) => {
                    write_at(data, 0, b"MUTATED!!").unwrap();
                }
                other => panic!(
                    "copy_up should yield a private File, got {:?}",
                    other.is_some()
                ),
            }
        }
        // A sees its private mutation; B still reads the untouched layer bytes.
        assert_eq!(
            a.lock().unwrap().read_file("/f", 64).as_deref(),
            Some(&b"MUTATED!!"[..])
        );
        assert_eq!(
            b.lock().unwrap().read_file("/f", 64).as_deref(),
            Some(&b"immutable"[..])
        );
        assert_eq!(&*bytes, b"immutable");
    }

    #[test]
    fn copy_up_materializes_a_lazy_layer_file() {
        // A lazy (disk-indexed) layer applied to two nodes: writing in one
        // must materialize a *private* copy there, while the other node and
        // the shared layer bytes stay exactly the on-disk content.
        let dir = std::env::temp_dir().join("wk-vfs-lazy-copyup");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let tar_path = dir.join("layer.tar");
        let mut b = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Regular);
        h.set_size(9);
        h.set_cksum();
        b.append_data(&mut h, "f", &b"immutable"[..]).unwrap();
        std::fs::write(&tar_path, b.into_inner().unwrap()).unwrap();

        let layer = layers::from_tar_file(&tar_path).expect("indexes");
        let a = new_fs();
        let bfs = new_fs();
        layers::apply(&a, &layer, "");
        layers::apply(&bfs, &layer, "");

        {
            let mut g = a.lock().unwrap();
            let id = resolve(&g, ROOT, "/f").unwrap();
            g.copy_up(id);
            match g.nodes.get_mut(&id) {
                Some(Node::File(data)) => write_at(data, 0, b"MUTATED!!").unwrap(),
                _ => panic!("copy_up should yield a private File"),
            }
        }
        assert_eq!(
            a.lock().unwrap().read_file("/f", 64).as_deref(),
            Some(&b"MUTATED!!"[..])
        );
        assert_eq!(
            bfs.lock().unwrap().read_file("/f", 64).as_deref(),
            Some(&b"immutable"[..]),
            "the other node still reads the layer's on-disk bytes"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn copy_up_is_a_no_op_for_private_and_dir_nodes() {
        let fs = new_fs();
        {
            let mut g = fs.lock().unwrap();
            g.add_child(ROOT, "f", Node::File(b"mine".to_vec()));
            g.add_child(ROOT, "d", Node::Dir(BTreeMap::new()));
            let f = resolve(&g, ROOT, "/f").unwrap();
            let d = resolve(&g, ROOT, "/d").unwrap();
            g.copy_up(f);
            g.copy_up(d);
            assert!(matches!(g.nodes.get(&f), Some(Node::File(_))));
            assert!(matches!(g.nodes.get(&d), Some(Node::Dir(_))));
        }
        assert_eq!(
            fs.lock().unwrap().read_file("/f", 64).as_deref(),
            Some(&b"mine"[..])
        );
    }

    #[test]
    fn snapshot_classifies_paths_for_build_diffs() {
        let fs = new_fs();
        {
            let mut g = fs.lock().unwrap();
            g.put_ro_file_at("layered/ro.txt", ro_bytes(b"layer"));
            g.put_file_at("written/out.txt", b"private".to_vec());
            g.ensure_dir_path("empty");
        }
        let g = fs.lock().unwrap();
        let snap = g.snapshot();
        assert_eq!(snap.get("layered/ro.txt"), Some(&PathKind::LayerFile));
        assert_eq!(snap.get("written/out.txt"), Some(&PathKind::PrivateFile));
        assert_eq!(snap.get("empty"), Some(&PathKind::Dir));
        assert_eq!(snap.get("layered"), Some(&PathKind::Dir));
        assert!(!snap.contains_key(""), "root itself is not listed");
    }

    #[test]
    fn host_mapped_file_reads_and_writes_disk() {
        let path = std::env::temp_dir().join("wk_host_mapped_test.txt");
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, b"on disk").unwrap();

        let fs = new_fs();
        mount_host_file(&fs, "h", path.clone(), true);
        let node = resolve(&fs.lock().unwrap(), ROOT, "/h").expect("mounted");

        // The mounted node reports the real file's size, and a write through it
        // lands on disk.
        assert_eq!(stat_node(&fs.lock().unwrap(), node).unwrap().size, 7);
        host_write_at(&path, 0, b"changed!").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"changed!");
        assert_eq!(host_read(&path), b"changed!");

        // Unmounting leaves the disk file untouched.
        unmount_file(&fs, "h");
        assert!(resolve(&fs.lock().unwrap(), ROOT, "/h").is_none());
        assert_eq!(std::fs::read(&path).unwrap(), b"changed!");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn host_mapped_file_reports_its_mtime() {
        // A guest watching a bind mount for edits (the world node reloading
        // its .glb) sees a real modification time; in-memory nodes have none
        // to report, and say so rather than inventing one.
        let path = std::env::temp_dir().join("wk_host_mtime_test.txt");
        std::fs::write(&path, b"v1").unwrap();
        let fs = new_fs();
        mount_host_file(&fs, "h", path.clone(), true);
        fs.lock().unwrap().put_file_at("mem", b"v1".to_vec());

        let stat = |name: &str| {
            let g = fs.lock().unwrap();
            stat_node(&g, resolve(&g, ROOT, name).expect("mounted")).unwrap()
        };
        let before = stat("/h").data_modification_timestamp.expect("host mtime");
        assert!(stat("/mem").data_modification_timestamp.is_none());

        // A same-size edit moves the timestamp — the case a size check misses.
        filetime_bump(&path);
        std::fs::write(&path, b"v2").unwrap();
        let after = stat("/h").data_modification_timestamp.expect("host mtime");
        assert!(
            (after.seconds, after.nanoseconds) > (before.seconds, before.nanoseconds),
            "mtime advanced ({before:?} -> {after:?})"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// Push a file's mtime a second into the past, so a rewrite in the same
    /// tick still registers as newer on filesystems with coarse timestamps.
    fn filetime_bump(path: &std::path::Path) {
        let old = std::fs::metadata(path).unwrap().modified().unwrap()
            - std::time::Duration::from_secs(2);
        let f = std::fs::File::options().write(true).open(path).unwrap();
        f.set_modified(old).unwrap();
    }

    #[test]
    fn mount_host_mirrors_a_directory_tree() {
        // A host dir with a top file and a nested subdir/file.
        let root = std::env::temp_dir().join("wk_host_dir_test");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("top.txt"), b"top").unwrap();
        std::fs::write(root.join("sub/deep.txt"), b"deep!").unwrap();

        let fs = new_fs();
        mount_host(&fs, "/vol", root.clone(), true);

        // The tree is mirrored: files appear at their sub-paths, backed by disk.
        let top = resolve(&fs.lock().unwrap(), ROOT, "/vol/top.txt").expect("top mounted");
        let deep = resolve(&fs.lock().unwrap(), ROOT, "/vol/sub/deep.txt").expect("nested mounted");
        assert_eq!(stat_node(&fs.lock().unwrap(), top).unwrap().size, 3);
        assert_eq!(stat_node(&fs.lock().unwrap(), deep).unwrap().size, 5);
        // Reads are live: an external write to the disk file is seen through it.
        host_write_at(&root.join("sub/deep.txt"), 0, b"DEEP.").unwrap();
        assert_eq!(
            fs.lock()
                .unwrap()
                .read_file("/vol/sub/deep.txt", 64)
                .as_deref(),
            Some(&b"DEEP."[..])
        );

        // Unmounting the mount root removes the whole subtree.
        unmount_file(&fs, "/vol");
        assert!(resolve(&fs.lock().unwrap(), ROOT, "/vol").is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- property-based: the guest-controlled offset/len arithmetic ----
    //
    // `read_at`/`write_at` take a fully guest-controlled `u64` offset and length.
    // These properties pin the invariants that a guest cannot panic the host or
    // force an unbounded allocation, across the whole `u64` range.

    use proptest::prelude::*;

    proptest! {
        /// `read_at` is total: for *any* offset/len it never panics and returns
        /// exactly the contiguous run of `data` starting at `offset` (clamped to
        /// the file), with a correct EOF flag. Guards the former `start + len`
        /// overflow that panicked on e.g. `offset = 1, len = u64::MAX`.
        #[test]
        fn read_at_is_total(
            data in prop::collection::vec(any::<u8>(), 0..512),
            offset in any::<u64>(),
            len in any::<u64>(),
        ) {
            let (bytes, eof) = read_at(&data, offset, len);

            // Independent oracle: a contiguous slice from the clamped start.
            let expected: &[u8] = if offset < data.len() as u64 {
                let start = offset as usize;
                let take = usize::try_from(len).unwrap_or(usize::MAX).min(data.len() - start);
                &data[start..start + take]
            } else {
                &[]
            };
            prop_assert_eq!(&bytes[..], expected);
            prop_assert!(bytes.len() as u64 <= len);
            prop_assert_eq!(eof, offset as usize + expected.len() >= data.len()
                || offset >= data.len() as u64);
        }

        /// A write that fits under the cap is readable back byte-for-byte and never
        /// grows the file past [`MAX_FILE_SIZE`].
        #[test]
        fn write_at_within_cap_round_trips(
            mut data in prop::collection::vec(any::<u8>(), 0..256),
            offset in 0u64..8192,
            payload in prop::collection::vec(any::<u8>(), 0..256),
        ) {
            write_at(&mut data, offset, &payload).expect("small write is under the cap");
            prop_assert!(data.len() <= MAX_FILE_SIZE);
            let (read, _) = read_at(&data, offset, payload.len() as u64);
            prop_assert_eq!(read, payload);
        }

        /// A write whose end exceeds the cap (or overflows `usize`) is rejected and
        /// leaves the file untouched — no giant `Vec::resize` allocation. Directly
        /// guards the `write(offset = 2^48)` process-abort DoS.
        #[test]
        fn write_at_rejects_oversized_offset(
            mut data in prop::collection::vec(any::<u8>(), 0..64),
            offset in (MAX_FILE_SIZE as u64)..=u64::MAX,
            payload in prop::collection::vec(any::<u8>(), 1..16),
        ) {
            let before = data.clone();
            prop_assert!(write_at(&mut data, offset, &payload).is_err());
            prop_assert_eq!(data, before);
        }
    }
}
