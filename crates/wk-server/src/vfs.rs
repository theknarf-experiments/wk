//! Per-node in-memory filesystem: wk implements `wasi:filesystem` itself
//! (instead of wasmtime-wasi's cap-std one) so each app node sees its own
//! sandboxed, in-RAM filesystem. Nothing touches the host disk and nodes are
//! isolated from each other (Docker-like). The only shared state is a "file
//! node" on the canvas explicitly *connected* to an app: it appears as a shared
//! file in that app's filesystem (see `mount_file`), so wiring one file node to
//! two apps lets them talk through it.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use wasmtime::component::{HasData, Linker, Resource, ResourceTable};
use wasmtime::Result;
use wasmtime_wasi::WasiView;
use wasmtime_wasi_io::async_trait;
use wasmtime_wasi_io::bytes::Bytes;
use wasmtime_wasi_io::poll::Pollable;
use wasmtime_wasi_io::streams::{DynInputStream, DynOutputStream, OutputStream, StreamError};
use wasmtime_wasi_io::IoView;

wasmtime::component::bindgen!({
    path: "wit-fs",
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

use crate::plugin::HostState;
use wasi::filesystem::types::{
    Advice, DescriptorFlags, DescriptorStat, DescriptorType, DirectoryEntry, ErrorCode, Filesize,
    MetadataHashValue, NewTimestamp, OpenFlags, PathFlags,
};

/// The bytes of a canvas "file node", shared by every app it is connected to.
pub type SharedFile = Arc<Mutex<Vec<u8>>>;

/// How a path exists in an `Fs`, for build-time diffs (see [`Fs::snapshot`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PathKind {
    Dir,
    /// An immutable layer file (`RoFile`) — untouched since its layer applied.
    LayerFile,
    /// A privately written file: created or copied-up since the last layer.
    PrivateFile,
    /// A canvas file mount (shared/host) — not part of any image.
    Mounted,
}

enum Node {
    File(Vec<u8>),
    Dir(BTreeMap<String, u64>),
    /// A file whose bytes live in a canvas VirtualFile node connected to this
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
    /// see [`Fs::copy_up`]).
    RoFile(Arc<Vec<u8>>),
}

const ROOT: u64 = 0;

/// One app node's in-memory filesystem.
pub struct Fs {
    nodes: BTreeMap<u64, Node>,
    next: u64,
}

impl Default for Fs {
    fn default() -> Self {
        let mut nodes = BTreeMap::new();
        nodes.insert(ROOT, Node::Dir(BTreeMap::new()));
        Fs { nodes, next: 1 }
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

    /// Insert `node` as a child named `name` under `parent` (a directory).
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
            let private = bytes.as_ref().clone();
            self.nodes.insert(id, Node::File(private));
        }
    }

    // ---- path-level helpers for applying filesystem layers (crate::layers) ----

    /// Resolve the directory at `path`, creating missing components. `None` if a
    /// component already exists as a file, or the fs is at capacity.
    pub(crate) fn ensure_dir_path(&mut self, path: &str) -> Option<u64> {
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
    pub(crate) fn put_ro_file_at(&mut self, path: &str, bytes: Arc<Vec<u8>>) {
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
    pub(crate) fn remove_path(&mut self, path: &str) {
        let comps = components(path);
        let Some((name, dirs)) = comps.split_last() else {
            return;
        };
        if let Some(parent) = resolve(self, ROOT, &dirs.join("/")) {
            self.remove_path_in(parent, name);
        }
    }

    /// Remove every child of the directory at `path` (an OCI opaque marker).
    pub(crate) fn clear_dir_at(&mut self, path: &str) {
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
    /// replacing any existing entry. Guest writes create private files through
    /// the wasi host traits; this host-side twin lets tests (the mock RUN
    /// runner, diff fixtures) mutate a rootfs the same way.
    #[cfg(test)]
    pub(crate) fn put_file_at(&mut self, path: &str, bytes: Vec<u8>) {
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
    pub(crate) fn snapshot(&self) -> BTreeMap<String, PathKind> {
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
                    None => continue,
                };
                out.insert(path.clone(), kind);
                if kind == PathKind::Dir {
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
            .map(|(name, &cid)| DirEntry {
                name: name.clone(),
                is_dir: matches!(self.nodes.get(&cid), Some(Node::Dir(_))),
                size: self.file_len(cid),
            })
            .collect();
        out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
        Some(out)
    }

    /// Read up to `cap` bytes of the file at `path` for preview, or `None` if it
    /// isn't a file. Read-only; a host-mapped file is read from disk.
    pub fn read_file(&self, path: &str, cap: usize) -> Option<Vec<u8>> {
        let id = resolve(self, ROOT, path)?;
        match self.nodes.get(&id)? {
            Node::File(d) => Some(d.iter().take(cap).copied().collect()),
            Node::RoFile(d) => Some(d.iter().take(cap).copied().collect()),
            Node::Shared(sh) => Some(sh.lock().unwrap().iter().take(cap).copied().collect()),
            Node::Host(p) => {
                use std::io::Read;
                let mut f = std::fs::File::open(p).ok()?;
                let mut buf = vec![0u8; cap];
                let n = f.read(&mut buf).ok()?;
                buf.truncate(n);
                Some(buf)
            }
            Node::Dir(_) => None,
        }
    }
}

/// Connect a canvas file node into `fs` as the shared file `/name`.
pub fn mount_file(fs: &SharedFs, name: &str, data: SharedFile) {
    unmount_file(fs, name);
    fs.lock().unwrap().add_child(ROOT, name, Node::Shared(data));
}

/// Connect a canvas HostMappedFile node into `fs` as `/name`, backed by the
/// real host file at `path`. Reads and writes go straight to disk.
pub fn mount_host_file(fs: &SharedFs, name: &str, path: std::path::PathBuf) {
    unmount_file(fs, name);
    fs.lock().unwrap().add_child(ROOT, name, Node::Host(path));
}

/// Disconnect the shared file `/name` from `fs` (leaves the file node's bytes
/// intact for any other app still connected).
pub fn unmount_file(fs: &SharedFs, name: &str) {
    let mut g = fs.lock().unwrap();
    let removed = match g.nodes.get_mut(&ROOT) {
        Some(Node::Dir(children)) => children.remove(name),
        _ => None,
    };
    if let Some(id) = removed {
        g.nodes.remove(&id);
    }
}

/// Split a path into normal components (ignoring empty and `.`).
fn components(path: &str) -> Vec<&str> {
    path.split('/')
        .filter(|c| !c.is_empty() && *c != ".")
        .collect()
}

/// Resolve an existing node from `start` following `path`.
fn resolve(fs: &Fs, start: u64, path: &str) -> Option<u64> {
    let mut cur = start;
    for comp in components(path) {
        match fs.nodes.get(&cur)? {
            Node::Dir(children) => cur = *children.get(comp)?,
            _ => return None,
        }
    }
    Some(cur)
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
        Some(Node::Dir(_)) => DescriptorType::Directory,
        _ => DescriptorType::RegularFile,
    }
}

/// A descriptor handle: an open file or directory in some app node's `Fs`.
pub struct Descriptor {
    fs: SharedFs,
    node: u64,
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

/// Size of the host file in bytes (0 if it doesn't exist yet).
fn host_size(path: &std::path::Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
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

/// Add our in-memory `wasi:filesystem` to the linker.
pub fn add_to_linker(l: &mut Linker<HostState>) -> Result<()> {
    wasi::filesystem::types::add_to_linker::<_, HasFs>(l, |s| s)?;
    wasi::filesystem::preopens::add_to_linker::<_, HasFs>(l, |s| s)?;
    Ok(())
}

struct HasFs;
impl HasData for HasFs {
    type Data<'a> = &'a mut HostState;
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
    Missing,
}

impl HostState {
    /// Clone the `fs` Arc for the descriptor `fd` (all this node's descriptors
    /// share the one filesystem).
    fn fd_fs(&mut self, fd: &Resource<Descriptor>) -> Result<(SharedFs, u64)> {
        let d = self.table().get(fd)?;
        Ok((d.fs.clone(), d.node))
    }

    fn kind(fs: &SharedFs, node: u64) -> Kind {
        match fs.lock().unwrap().nodes.get(&node) {
            Some(Node::File(_)) => Kind::File,
            Some(Node::RoFile(d)) => Kind::Ro(d.clone()),
            Some(Node::Shared(sh)) => Kind::Shared(sh.clone()),
            Some(Node::Host(p)) => Kind::Host(p.clone()),
            Some(Node::Dir(_)) => Kind::Dir,
            None => Kind::Missing,
        }
    }
}

impl wasi::filesystem::preopens::Host for HostState {
    fn get_directories(&mut self) -> Result<Vec<(Resource<Descriptor>, String)>> {
        let fs = self.fs.clone();
        let root = self.table().push(Descriptor { fs, node: ROOT })?;
        Ok(vec![(root, "/".to_string())])
    }
}

impl wasi::filesystem::types::Host for HostState {
    fn filesystem_error_code(
        &mut self,
        _err: Resource<wasmtime::Error>,
    ) -> Result<Option<ErrorCode>> {
        Ok(None)
    }
}

impl wasi::filesystem::types::HostDescriptor for HostState {
    fn read_via_stream(
        &mut self,
        fd: Resource<Descriptor>,
        offset: Filesize,
    ) -> Result<std::result::Result<Resource<DynInputStream>, ErrorCode>> {
        let (fs, node) = self.fd_fs(&fd)?;
        let bytes = match Self::kind(&fs, node) {
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
            Kind::Dir => return err(ErrorCode::IsDirectory),
            Kind::Missing => return err(ErrorCode::NoEntry),
        };
        let stream: DynInputStream = Box::new(wasmtime_wasi::p2::pipe::MemoryInputPipe::new(bytes));
        Ok(Ok(self.table().push(stream)?))
    }

    fn write_via_stream(
        &mut self,
        fd: Resource<Descriptor>,
        offset: Filesize,
    ) -> Result<std::result::Result<Resource<DynOutputStream>, ErrorCode>> {
        let (fs, node) = self.fd_fs(&fd)?;
        let stream: DynOutputStream = match Self::kind(&fs, node) {
            // A layer file copy-ups on the stream's first write.
            Kind::File | Kind::Ro(_) => Box::new(VfsOutputStream { fs, node, offset }),
            Kind::Shared(data) => Box::new(SharedOutputStream { data, offset }),
            Kind::Host(path) => Box::new(HostOutputStream { path, offset }),
            _ => return err(ErrorCode::IsDirectory),
        };
        Ok(Ok(self.table().push(stream)?))
    }

    fn append_via_stream(
        &mut self,
        fd: Resource<Descriptor>,
    ) -> Result<std::result::Result<Resource<DynOutputStream>, ErrorCode>> {
        let (fs, node) = self.fd_fs(&fd)?;
        let stream: DynOutputStream = match Self::kind(&fs, node) {
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
        let (fs, node) = self.fd_fs(&fd)?;
        match Self::kind(&fs, node) {
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
        let (fs, node) = self.fd_fs(&fd)?;
        match Self::kind(&fs, node) {
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
        let (fs, node) = self.fd_fs(&fd)?;
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
        let (fs, node) = self.fd_fs(&fd)?;
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
        let (fs, node) = self.fd_fs(&fd)?;
        let g = fs.lock().unwrap();
        match stat_node(&g, node) {
            Some(s) => Ok(Ok(s)),
            None => err(ErrorCode::NoEntry),
        }
    }

    fn stat_at(
        &mut self,
        fd: Resource<Descriptor>,
        _path_flags: PathFlags,
        path: String,
    ) -> Result<std::result::Result<DescriptorStat, ErrorCode>> {
        let (fs, node) = self.fd_fs(&fd)?;
        let g = fs.lock().unwrap();
        match resolve(&g, node, &path).and_then(|id| stat_node(&g, id)) {
            Some(s) => Ok(Ok(s)),
            None => err(ErrorCode::NoEntry),
        }
    }

    fn get_type(
        &mut self,
        fd: Resource<Descriptor>,
    ) -> Result<std::result::Result<DescriptorType, ErrorCode>> {
        let (fs, node) = self.fd_fs(&fd)?;
        let g = fs.lock().unwrap();
        Ok(Ok(node_type(&g, node)))
    }

    fn get_flags(
        &mut self,
        _fd: Resource<Descriptor>,
    ) -> Result<std::result::Result<DescriptorFlags, ErrorCode>> {
        Ok(Ok(DescriptorFlags::READ | DescriptorFlags::WRITE))
    }

    fn set_size(
        &mut self,
        fd: Resource<Descriptor>,
        size: Filesize,
    ) -> Result<std::result::Result<(), ErrorCode>> {
        let (fs, node) = self.fd_fs(&fd)?;
        let size = match usize::try_from(size) {
            Ok(s) if s <= MAX_FILE_SIZE => s,
            _ => return err(ErrorCode::FileTooLarge),
        };
        match Self::kind(&fs, node) {
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
            _ => err(ErrorCode::IsDirectory),
        }
    }

    fn open_at(
        &mut self,
        fd: Resource<Descriptor>,
        _path_flags: PathFlags,
        path: String,
        oflags: OpenFlags,
        _flags: DescriptorFlags,
    ) -> Result<std::result::Result<Resource<Descriptor>, ErrorCode>> {
        let (fs, start) = self.fd_fs(&fd)?;
        let node = {
            let mut g = fs.lock().unwrap();
            match resolve(&g, start, &path) {
                Some(id) => {
                    if oflags.contains(OpenFlags::EXCLUSIVE) {
                        return err(ErrorCode::Exist);
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
        Ok(Ok(self.table().push(Descriptor { fs, node })?))
    }

    fn remove_directory_at(
        &mut self,
        fd: Resource<Descriptor>,
        path: String,
    ) -> Result<std::result::Result<(), ErrorCode>> {
        self.unlink(fd, &path, true)
    }

    fn unlink_file_at(
        &mut self,
        fd: Resource<Descriptor>,
        path: String,
    ) -> Result<std::result::Result<(), ErrorCode>> {
        self.unlink(fd, &path, false)
    }

    fn rename_at(
        &mut self,
        fd: Resource<Descriptor>,
        old_path: String,
        new_fd: Resource<Descriptor>,
        new_path: String,
    ) -> Result<std::result::Result<(), ErrorCode>> {
        let (fs, old_start) = self.fd_fs(&fd)?;
        let new_start = self.table().get(&new_fd)?.node;
        // Rename only within one filesystem (node ids are per-fs).
        if !Arc::ptr_eq(&fs, &self.table().get(&new_fd)?.fs) {
            return err(ErrorCode::CrossDevice);
        }
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
        let (afs, an) = self.fd_fs(&a)?;
        let (bfs, bn) = self.fd_fs(&b)?;
        Ok(Arc::ptr_eq(&afs, &bfs) && an == bn)
    }

    fn metadata_hash(
        &mut self,
        fd: Resource<Descriptor>,
    ) -> Result<std::result::Result<MetadataHashValue, ErrorCode>> {
        let (_fs, node) = self.fd_fs(&fd)?;
        Ok(Ok(MetadataHashValue {
            lower: node,
            upper: 0,
        }))
    }

    fn metadata_hash_at(
        &mut self,
        fd: Resource<Descriptor>,
        _path_flags: PathFlags,
        path: String,
    ) -> Result<std::result::Result<MetadataHashValue, ErrorCode>> {
        let (fs, node) = self.fd_fs(&fd)?;
        let g = fs.lock().unwrap();
        match resolve(&g, node, &path) {
            Some(id) => Ok(Ok(MetadataHashValue {
                lower: id,
                upper: 0,
            })),
            None => err(ErrorCode::NoEntry),
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
    fn link_at(
        &mut self,
        _fd: Resource<Descriptor>,
        _old_path_flags: PathFlags,
        _old_path: String,
        _new_descriptor: Resource<Descriptor>,
        _new_path: String,
    ) -> Result<std::result::Result<(), ErrorCode>> {
        err(ErrorCode::Unsupported)
    }
    fn symlink_at(
        &mut self,
        _fd: Resource<Descriptor>,
        _src_path: String,
        _dest_path: String,
    ) -> Result<std::result::Result<(), ErrorCode>> {
        err(ErrorCode::Unsupported)
    }
    fn readlink_at(
        &mut self,
        _fd: Resource<Descriptor>,
        _path: String,
    ) -> Result<std::result::Result<String, ErrorCode>> {
        err(ErrorCode::Invalid)
    }

    fn drop(&mut self, fd: Resource<Descriptor>) -> Result<()> {
        self.table().delete(fd)?;
        Ok(())
    }
}

impl HostState {
    /// Remove a file (`dir=false`) or empty directory (`dir=true`) at `path`.
    fn unlink(
        &mut self,
        fd: Resource<Descriptor>,
        path: &str,
        dir: bool,
    ) -> Result<std::result::Result<(), ErrorCode>> {
        let (fs, start) = self.fd_fs(&fd)?;
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
        match (dir, g.nodes.get(&id)) {
            (true, Some(Node::Dir(c))) if !c.is_empty() => return err(ErrorCode::NotEmpty),
            (true, Some(Node::Dir(_))) => {}
            (true, _) => return err(ErrorCode::NotDirectory),
            (false, Some(Node::Dir(_))) | (false, None) => return err(ErrorCode::IsDirectory),
            (false, _) => {}
        }
        g.nodes.remove(&id);
        if let Some(Node::Dir(c)) = g.nodes.get_mut(&parent) {
            c.remove(&name);
        }
        Ok(Ok(()))
    }
}

fn stat_node(fs: &Fs, id: u64) -> Option<DescriptorStat> {
    let (ty, size) = match fs.nodes.get(&id)? {
        Node::File(data) => (DescriptorType::RegularFile, data.len() as u64),
        Node::RoFile(data) => (DescriptorType::RegularFile, data.len() as u64),
        Node::Dir(_) => (DescriptorType::Directory, 0),
        Node::Shared(sh) => (DescriptorType::RegularFile, sh.lock().unwrap().len() as u64),
        Node::Host(p) => (DescriptorType::RegularFile, host_size(p)),
    };
    Some(DescriptorStat {
        type_: ty,
        link_count: 1,
        size,
        data_access_timestamp: None,
        data_modification_timestamp: None,
        status_change_timestamp: None,
    })
}

impl wasi::filesystem::types::HostDirectoryEntryStream for HostState {
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

        // Root: directory first, then file, each with its size.
        let root = g.list_dir("").expect("root is a dir");
        assert_eq!(
            root,
            vec![
                DirEntry {
                    name: "sub".into(),
                    is_dir: true,
                    size: 0
                },
                DirEntry {
                    name: "readme".into(),
                    is_dir: false,
                    size: 11
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
    fn connected_file_is_shared_then_unmounted() {
        let a = new_fs();
        let b = new_fs();
        let data: SharedFile = Arc::new(Mutex::new(Vec::new()));

        // Wiring the same file node into both apps gives both a shared file.
        mount_file(&a, "chan", data.clone());
        mount_file(&b, "chan", data.clone());
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
    fn ro_file_reads_and_stats_like_a_file() {
        // A layer-backed read-only file is indistinguishable from a private one
        // on the read paths: size, preview, listing.
        let fs = new_fs();
        let bytes: Arc<Vec<u8>> = Arc::new(b"from a layer".to_vec());
        fs.lock()
            .unwrap()
            .add_child(ROOT, "ro", Node::RoFile(bytes.clone()));
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
        a.lock()
            .unwrap()
            .add_child(ROOT, "f", Node::RoFile(bytes.clone()));
        b.lock()
            .unwrap()
            .add_child(ROOT, "f", Node::RoFile(bytes.clone()));

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
            g.put_ro_file_at("layered/ro.txt", Arc::new(b"layer".to_vec()));
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
        mount_host_file(&fs, "h", path.clone());
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
