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

// ---- the in-memory tree ----

/// The bytes of a canvas "file node", shared by every app it is connected to.
pub type SharedFile = Arc<Mutex<Vec<u8>>>;

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

impl Fs {
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
}

pub type SharedFs = Arc<Mutex<Fs>>;

/// A fresh, empty filesystem for a new app node.
pub fn new_fs() -> SharedFs {
    Arc::new(Mutex::new(Fs::default()))
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

// ---- resources ----

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
        match fs.nodes.get_mut(&self.node) {
            Some(Node::File(data)) => {
                write_at(data, self.offset, &bytes);
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
        write_at(&mut self.data.lock().unwrap(), self.offset, &bytes);
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

/// Copy `bytes` into `data` at `offset`, growing it if needed.
fn write_at(data: &mut Vec<u8>, offset: u64, bytes: &[u8]) {
    let start = offset as usize;
    let end = start + bytes.len();
    if data.len() < end {
        data.resize(end, 0);
    }
    data[start..end].copy_from_slice(bytes);
}

/// Read up to `len` bytes of `data` from `offset`, returning (bytes, eof).
fn read_at(data: &[u8], offset: u64, len: u64) -> (Vec<u8>, bool) {
    let start = (offset as usize).min(data.len());
    let end = (start + len as usize).min(data.len());
    (data[start..end].to_vec(), end >= data.len())
}

// ---- linker wiring ----

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

// ---- host impls ----

/// `Ok(Err(code))` shorthand.
fn err<T>(code: ErrorCode) -> Result<std::result::Result<T, ErrorCode>> {
    Ok(Err(code))
}

/// What `node` is, for read/write/stream dispatch — cloning the shared handle so
/// callers can act without holding the filesystem lock.
enum Kind {
    File,
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
            Kind::File => Box::new(VfsOutputStream { fs, node, offset }),
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
            Kind::File => {
                let offset = fs.lock().unwrap().nodes.get(&node).map_or(0, |n| match n {
                    Node::File(d) => d.len() as u64,
                    _ => 0,
                });
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
            Kind::File => {
                let mut g = fs.lock().unwrap();
                let Some(Node::File(data)) = g.nodes.get_mut(&node) else {
                    return err(ErrorCode::NoEntry);
                };
                write_at(data, offset, &buf);
            }
            Kind::Shared(sh) => write_at(&mut sh.lock().unwrap(), offset, &buf),
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
        match Self::kind(&fs, node) {
            Kind::File => {
                if let Some(Node::File(data)) = fs.lock().unwrap().nodes.get_mut(&node) {
                    data.resize(size as usize, 0);
                }
                Ok(Ok(()))
            }
            Kind::Shared(sh) => {
                sh.lock().unwrap().resize(size as usize, 0);
                Ok(Ok(()))
            }
            Kind::Host(p) => {
                match std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(&p)
                    .and_then(|f| f.set_len(size))
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
}
