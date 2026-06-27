//! Per-instance in-memory filesystem: wk implements `wasi:filesystem` itself
//! (instead of wasmtime-wasi's cap-std one) so each plugin instance sees its own
//! sandboxed, in-RAM filesystem. Nothing touches the host disk.

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context as TaskContext, Poll, Waker};

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

enum Node {
    File(Vec<u8>),
    Dir(BTreeMap<String, u64>),
    /// A mount point: descending into it crosses over to another filesystem's
    /// root. Used to graft the shared workspace into each instance at `/shared`.
    Mount(SharedFs),
    /// A socket file: two openers become the two endpoints of a duplex byte
    /// stream and can read/write to talk to each other (Unix AF_UNIX-like).
    Socket(Socket),
}

const ROOT: u64 = 0;

/// One instance's in-memory filesystem.
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

// ---- sockets ----

/// A bidirectional in-memory socket, shared by its (up to) two endpoints. The
/// `Condvar` lets a blocking `read` syscall park its thread until the peer
/// writes or closes; the wakers serve the async `read-via-stream` path.
type Socket = Arc<(Mutex<SocketChannel>, Condvar)>;

#[derive(Default)]
struct SocketChannel {
    /// Bytes pending for endpoint `i`, written by its peer `1 - i`.
    queues: [VecDeque<u8>; 2],
    /// Wakers parked by an async reader on endpoint `i`.
    read_wakers: [Vec<Waker>; 2],
    /// Endpoint `i` has closed (its descriptor was dropped).
    closed: [bool; 2],
    /// How many endpoints have bound so far (first opener is 0, rest are 1).
    connected: u8,
}

fn new_socket() -> Socket {
    Arc::new((Mutex::new(SocketChannel::default()), Condvar::new()))
}

/// Bind the next endpoint: the first opener is endpoint 0, every later opener
/// shares endpoint 1 (the two-app case is the supported one).
fn socket_connect(sock: &Socket) -> usize {
    let (lock, _) = &**sock;
    let mut c = lock.lock().unwrap();
    let ep = if c.connected == 0 { 0 } else { 1 };
    c.connected = c.connected.saturating_add(1);
    ep
}

/// Queue `data` for endpoint `to`, waking whoever is reading it.
fn socket_write(sock: &Socket, to: usize, data: &[u8]) {
    let (lock, cvar) = &**sock;
    let mut c = lock.lock().unwrap();
    c.queues[to].extend(data.iter().copied());
    for w in c.read_wakers[to].drain(..) {
        w.wake();
    }
    drop(c);
    cvar.notify_all();
}

/// Block the calling thread until endpoint `ep` has bytes to read or its peer
/// closes, then return up to `len` bytes (and whether EOF was reached). This is
/// the blocking-read syscall behaviour for a socket file.
fn socket_read_blocking(sock: &Socket, ep: usize, len: usize) -> (Vec<u8>, bool) {
    let (lock, cvar) = &**sock;
    let mut c = lock.lock().unwrap();
    loop {
        let q = &mut c.queues[ep];
        if !q.is_empty() {
            let n = len.min(q.len());
            return (q.drain(..n).collect(), false);
        }
        if c.closed[1 - ep] {
            return (Vec::new(), true);
        }
        c = cvar.wait(c).unwrap();
    }
}

/// Mark endpoint `ep` closed and wake the peer so it observes EOF.
fn socket_close(sock: &Socket, ep: usize) {
    let (lock, cvar) = &**sock;
    let mut c = lock.lock().unwrap();
    c.closed[ep] = true;
    for w in c.read_wakers[1 - ep].drain(..) {
        w.wake();
    }
    drop(c);
    cvar.notify_all();
}

/// Create the host-global shared workspace filesystem, seeded with a socket
/// file at `/sock` that two instances can use to talk to each other.
pub fn new_shared_workspace() -> SharedFs {
    let mut fs = Fs::default();
    fs.add_child(ROOT, "sock", Node::Socket(new_socket()));
    Arc::new(Mutex::new(fs))
}

/// Create a fresh per-instance filesystem with the shared workspace grafted in
/// at `/shared`.
pub fn new_instance_fs(shared: &SharedFs) -> SharedFs {
    let mut fs = Fs::default();
    fs.add_child(ROOT, "shared", Node::Mount(shared.clone()));
    Arc::new(Mutex::new(fs))
}

/// Split a path into normal components (ignoring empty and `.`).
fn components(path: &str) -> Vec<&str> {
    path.split('/')
        .filter(|c| !c.is_empty() && *c != ".")
        .collect()
}

/// Resolve an existing node from `start` following `path`, crossing `Mount`
/// points into their target filesystem. Returns the filesystem that actually
/// holds the node together with the node id.
fn resolve(fs: &SharedFs, start: u64, path: &str) -> Option<(SharedFs, u64)> {
    let mut cur_fs = fs.clone();
    let mut cur = start;
    for comp in components(path) {
        // Look up the child and, if it is a mount, the filesystem to cross into.
        let (cross, next) = {
            let g = cur_fs.lock().unwrap();
            match g.nodes.get(&cur)? {
                Node::Dir(children) => {
                    let child = *children.get(comp)?;
                    match g.nodes.get(&child)? {
                        Node::Mount(shared) => (Some(shared.clone()), ROOT),
                        _ => (None, child),
                    }
                }
                _ => return None,
            }
        };
        if let Some(shared) = cross {
            cur_fs = shared;
        }
        cur = next;
    }
    Some((cur_fs, cur))
}

/// Resolve the parent directory of the last component of `path`, returning the
/// filesystem that holds it, the parent node id, and the final name.
fn resolve_parent(fs: &SharedFs, start: u64, path: &str) -> Option<(SharedFs, u64, String)> {
    let comps = components(path);
    let (name, dirs) = comps.split_last()?;
    let (pfs, parent) = resolve(fs, start, &dirs.join("/"))?;
    let is_dir = matches!(pfs.lock().unwrap().nodes.get(&parent), Some(Node::Dir(_)));
    is_dir.then(|| (pfs, parent, (*name).to_string()))
}

fn node_type(fs: &Fs, id: u64) -> DescriptorType {
    match fs.nodes.get(&id) {
        Some(Node::Dir(_)) | Some(Node::Mount(_)) => DescriptorType::Directory,
        _ => DescriptorType::RegularFile,
    }
}

// ---- resources ----

/// A descriptor handle: an open file or directory in some instance's `Fs`.
pub struct Descriptor {
    fs: SharedFs,
    node: u64,
    /// For an open socket: which endpoint (0 or 1) this handle owns.
    endpoint: Option<usize>,
}

/// A snapshot directory listing.
pub struct DirEntryStream {
    entries: Vec<DirectoryEntry>,
    pos: usize,
}

/// An output stream that writes into an in-memory file at a moving offset.
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
                let start = self.offset as usize;
                let end = start + bytes.len();
                if data.len() < end {
                    data.resize(end, 0);
                }
                data[start..end].copy_from_slice(&bytes);
                self.offset = end as u64;
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

/// Reading end of a socket: drains the bytes queued for this endpoint.
struct SocketInputStream {
    sock: Socket,
    endpoint: usize,
}

impl InputStream for SocketInputStream {
    fn read(&mut self, size: usize) -> std::result::Result<Bytes, StreamError> {
        let (lock, _) = &*self.sock;
        let mut c = lock.lock().unwrap();
        let q = &mut c.queues[self.endpoint];
        let n = size.min(q.len());
        if n == 0 {
            // Nothing buffered: EOF once the peer has closed, else just empty.
            if c.closed[1 - self.endpoint] {
                return Err(StreamError::Closed);
            }
            return Ok(Bytes::new());
        }
        Ok(Bytes::from(q.drain(..n).collect::<Vec<u8>>()))
    }
}

#[async_trait]
impl Pollable for SocketInputStream {
    async fn ready(&mut self) {
        SocketReadable {
            sock: self.sock.clone(),
            endpoint: self.endpoint,
        }
        .await
    }
}

/// Future that resolves once this endpoint has bytes to read or the peer closed.
struct SocketReadable {
    sock: Socket,
    endpoint: usize,
}

impl Future for SocketReadable {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<()> {
        let (lock, _) = &*self.sock;
        let mut c = lock.lock().unwrap();
        if !c.queues[self.endpoint].is_empty() || c.closed[1 - self.endpoint] {
            Poll::Ready(())
        } else {
            c.read_wakers[self.endpoint].push(cx.waker().clone());
            Poll::Pending
        }
    }
}

/// Writing end of a socket: queues bytes for the peer endpoint.
struct SocketOutputStream {
    sock: Socket,
    /// This handle's own endpoint; writes go to the peer `1 - endpoint`.
    endpoint: usize,
}

#[async_trait]
impl Pollable for SocketOutputStream {
    async fn ready(&mut self) {}
}

impl OutputStream for SocketOutputStream {
    fn write(&mut self, bytes: Bytes) -> std::result::Result<(), StreamError> {
        let peer = 1 - self.endpoint;
        {
            let (lock, _) = &*self.sock;
            if lock.lock().unwrap().closed[peer] {
                return Err(StreamError::Closed);
            }
        }
        socket_write(&self.sock, peer, &bytes);
        Ok(())
    }
    fn flush(&mut self) -> std::result::Result<(), StreamError> {
        Ok(())
    }
    fn check_write(&mut self) -> std::result::Result<usize, StreamError> {
        Ok(1024 * 1024)
    }
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

impl HostState {
    /// Clone the `fs` Arc for the descriptor `fd` (all this instance's
    /// descriptors share the one filesystem).
    fn fd_fs(&mut self, fd: &Resource<Descriptor>) -> Result<(SharedFs, u64)> {
        let d = self.table().get(fd)?;
        Ok((d.fs.clone(), d.node))
    }

    /// The socket endpoint owned by `fd`, defaulting to 0 for non-socket fds.
    fn fd_endpoint(&mut self, fd: &Resource<Descriptor>) -> Result<usize> {
        Ok(self.table().get(fd)?.endpoint.unwrap_or(0))
    }
}

impl wasi::filesystem::preopens::Host for HostState {
    fn get_directories(&mut self) -> Result<Vec<(Resource<Descriptor>, String)>> {
        let fs = self.fs.clone();
        let root = self.table().push(Descriptor {
            fs,
            node: ROOT,
            endpoint: None,
        })?;
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
        // A socket reads live bytes from its peer; a file reads its contents.
        enum Source {
            File(Bytes),
            Socket(Socket),
        }
        let source = {
            let g = fs.lock().unwrap();
            match g.nodes.get(&node) {
                Some(Node::File(data)) => {
                    let start = (offset as usize).min(data.len());
                    Source::File(Bytes::copy_from_slice(&data[start..]))
                }
                Some(Node::Socket(sock)) => Source::Socket(sock.clone()),
                Some(Node::Dir(_)) | Some(Node::Mount(_)) => return err(ErrorCode::IsDirectory),
                None => return err(ErrorCode::NoEntry),
            }
        };
        let stream: DynInputStream = match source {
            Source::File(bytes) => Box::new(wasmtime_wasi::p2::pipe::MemoryInputPipe::new(bytes)),
            Source::Socket(sock) => Box::new(SocketInputStream {
                sock,
                endpoint: self.fd_endpoint(&fd)?,
            }),
        };
        Ok(Ok(self.table().push(stream)?))
    }

    fn write_via_stream(
        &mut self,
        fd: Resource<Descriptor>,
        offset: Filesize,
    ) -> Result<std::result::Result<Resource<DynOutputStream>, ErrorCode>> {
        let (fs, node) = self.fd_fs(&fd)?;
        let sock = {
            let g = fs.lock().unwrap();
            match g.nodes.get(&node) {
                Some(Node::File(_)) => None,
                Some(Node::Socket(sock)) => Some(sock.clone()),
                _ => return err(ErrorCode::IsDirectory),
            }
        };
        let stream: DynOutputStream = match sock {
            Some(sock) => Box::new(SocketOutputStream {
                sock,
                endpoint: self.fd_endpoint(&fd)?,
            }),
            None => Box::new(VfsOutputStream { fs, node, offset }),
        };
        Ok(Ok(self.table().push(stream)?))
    }

    fn append_via_stream(
        &mut self,
        fd: Resource<Descriptor>,
    ) -> Result<std::result::Result<Resource<DynOutputStream>, ErrorCode>> {
        let (fs, node) = self.fd_fs(&fd)?;
        enum Sink {
            File(u64),
            Socket(Socket),
        }
        let sink = match fs.lock().unwrap().nodes.get(&node) {
            Some(Node::File(data)) => Sink::File(data.len() as u64),
            Some(Node::Socket(sock)) => Sink::Socket(sock.clone()),
            Some(Node::Dir(_)) | Some(Node::Mount(_)) => return err(ErrorCode::IsDirectory),
            None => return err(ErrorCode::NoEntry),
        };
        let stream: DynOutputStream = match sink {
            Sink::File(offset) => Box::new(VfsOutputStream { fs, node, offset }),
            Sink::Socket(sock) => Box::new(SocketOutputStream {
                sock,
                endpoint: self.fd_endpoint(&fd)?,
            }),
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
        let endpoint = self.fd_endpoint(&fd)?;
        // For a socket, block this client's thread until the peer writes or
        // closes — a blocking read syscall on a Unix socket.
        let sock = {
            let g = fs.lock().unwrap();
            match g.nodes.get(&node) {
                Some(Node::File(data)) => {
                    let start = (offset as usize).min(data.len());
                    let end = (start + len as usize).min(data.len());
                    let eof = end >= data.len();
                    return Ok(Ok((data[start..end].to_vec(), eof)));
                }
                Some(Node::Socket(sock)) => sock.clone(),
                Some(Node::Dir(_)) | Some(Node::Mount(_)) => return err(ErrorCode::IsDirectory),
                None => return err(ErrorCode::NoEntry),
            }
        };
        Ok(Ok(socket_read_blocking(&sock, endpoint, len as usize)))
    }

    fn write(
        &mut self,
        fd: Resource<Descriptor>,
        buf: Vec<u8>,
        offset: Filesize,
    ) -> Result<std::result::Result<Filesize, ErrorCode>> {
        let (fs, node) = self.fd_fs(&fd)?;
        let endpoint = self.fd_endpoint(&fd)?;
        let sock = {
            let mut g = fs.lock().unwrap();
            match g.nodes.get_mut(&node) {
                Some(Node::File(data)) => {
                    let start = offset as usize;
                    let end = start + buf.len();
                    if data.len() < end {
                        data.resize(end, 0);
                    }
                    data[start..end].copy_from_slice(&buf);
                    return Ok(Ok(buf.len() as u64));
                }
                Some(Node::Socket(sock)) => sock.clone(),
                Some(Node::Dir(_)) | Some(Node::Mount(_)) => return err(ErrorCode::IsDirectory),
                None => return err(ErrorCode::NoEntry),
            }
        };
        socket_write(&sock, 1 - endpoint, &buf);
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
        let Some((pfs, parent, name)) = resolve_parent(&fs, node, &path) else {
            return err(ErrorCode::NoEntry);
        };
        let mut g = pfs.lock().unwrap();
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
        let Some((tfs, id)) = resolve(&fs, node, &path) else {
            return err(ErrorCode::NoEntry);
        };
        let g = tfs.lock().unwrap();
        match stat_node(&g, id) {
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
        let mut g = fs.lock().unwrap();
        match g.nodes.get_mut(&node) {
            Some(Node::File(data)) => {
                data.resize(size as usize, 0);
                Ok(Ok(()))
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
        // Resolve (crossing mounts) to the filesystem and node being opened,
        // creating a file in the target filesystem if it does not yet exist.
        let (tfs, node) = match resolve(&fs, start, &path) {
            Some((tfs, id)) => {
                if oflags.contains(OpenFlags::EXCLUSIVE) {
                    return err(ErrorCode::Exist);
                }
                let mut g = tfs.lock().unwrap();
                if oflags.contains(OpenFlags::TRUNCATE) {
                    if let Some(Node::File(data)) = g.nodes.get_mut(&id) {
                        data.clear();
                    }
                }
                if oflags.contains(OpenFlags::DIRECTORY)
                    && !matches!(g.nodes.get(&id), Some(Node::Dir(_)))
                {
                    return err(ErrorCode::NotDirectory);
                }
                drop(g);
                (tfs, id)
            }
            None => {
                if !oflags.contains(OpenFlags::CREATE) {
                    return err(ErrorCode::NoEntry);
                }
                let Some((pfs, parent, name)) = resolve_parent(&fs, start, &path) else {
                    return err(ErrorCode::NoEntry);
                };
                let mut g = pfs.lock().unwrap();
                let id = g.alloc(Node::File(Vec::new()));
                if let Some(Node::Dir(children)) = g.nodes.get_mut(&parent) {
                    children.insert(name, id);
                }
                drop(g);
                (pfs, id)
            }
        };
        // Opening a socket binds this handle to one of its two endpoints.
        let endpoint = match tfs.lock().unwrap().nodes.get(&node) {
            Some(Node::Socket(sock)) => Some(socket_connect(sock)),
            _ => None,
        };
        Ok(Ok(self.table().push(Descriptor {
            fs: tfs,
            node,
            endpoint,
        })?))
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
        let (new_fs, new_start) = self.fd_fs(&new_fd)?;
        let Some((old_pfs, old_parent, old_name)) = resolve_parent(&fs, old_start, &old_path)
        else {
            return err(ErrorCode::NoEntry);
        };
        let Some((new_pfs, new_parent, new_name)) = resolve_parent(&new_fs, new_start, &new_path)
        else {
            return err(ErrorCode::NoEntry);
        };
        // A node id only means something within its own filesystem, so renames
        // across a mount boundary are not supported.
        if !Arc::ptr_eq(&old_pfs, &new_pfs) {
            return err(ErrorCode::CrossDevice);
        }
        let mut g = old_pfs.lock().unwrap();
        let id = match g.nodes.get(&old_parent) {
            Some(Node::Dir(c)) => match c.get(&old_name) {
                Some(id) => *id,
                None => return err(ErrorCode::NoEntry),
            },
            _ => return err(ErrorCode::NotDirectory),
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
        match resolve(&fs, node, &path) {
            Some((_tfs, id)) => Ok(Ok(MetadataHashValue {
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
        // Closing a socket handle disconnects its endpoint so the peer sees EOF.
        let (fs, node, endpoint) = {
            let d = self.table().get(&fd)?;
            (d.fs.clone(), d.node, d.endpoint)
        };
        if let Some(ep) = endpoint {
            let sock = match fs.lock().unwrap().nodes.get(&node) {
                Some(Node::Socket(sock)) => Some(sock.clone()),
                _ => None,
            };
            if let Some(sock) = sock {
                socket_close(&sock, ep);
            }
        }
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
        let Some((pfs, parent, name)) = resolve_parent(&fs, start, path) else {
            return err(ErrorCode::NoEntry);
        };
        let mut g = pfs.lock().unwrap();
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
            (false, Some(Node::File(_))) => {}
            (false, _) => return err(ErrorCode::IsDirectory),
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
        Node::Dir(_) | Node::Mount(_) => (DescriptorType::Directory, 0),
        Node::Socket(_) => (DescriptorType::RegularFile, 0),
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

    /// The shared workspace is grafted into an instance at `/shared`, so a path
    /// crossing the mount resolves into the same shared filesystem and node.
    #[test]
    fn shared_mount_resolves_socket() {
        let shared = new_shared_workspace();
        let inst = new_instance_fs(&shared);

        let (tfs, node) = resolve(&inst, ROOT, "/shared/sock").expect("socket resolves");
        assert!(Arc::ptr_eq(&tfs, &shared), "resolves into the shared fs");
        assert!(matches!(
            tfs.lock().unwrap().nodes.get(&node),
            Some(Node::Socket(_))
        ));

        // Two separate instances reach the very same socket node.
        let other = new_instance_fs(&shared);
        let (ofs, onode) = resolve(&other, ROOT, "/shared/sock").unwrap();
        assert!(Arc::ptr_eq(&ofs, &tfs) && onode == node);
    }

    /// Two endpoints exchange bytes both ways, and a reader sees EOF once its
    /// peer closes.
    #[test]
    fn socket_duplex_and_eof() {
        let sock = new_socket();
        let a = socket_connect(&sock);
        let b = socket_connect(&sock);
        assert_eq!((a, b), (0, 1));

        // A writes to its peer (B); B reads it.
        socket_write(&sock, 1 - a, b"ping");
        let mut rb = SocketInputStream {
            sock: sock.clone(),
            endpoint: b,
        };
        assert_eq!(&rb.read(64).unwrap()[..], b"ping");

        // B writes to its peer (A); A reads it.
        socket_write(&sock, 1 - b, b"pong");
        let mut ra = SocketInputStream {
            sock: sock.clone(),
            endpoint: a,
        };
        assert_eq!(&ra.read(64).unwrap()[..], b"pong");

        // Nothing buffered and peer still open: a non-blocking read is empty.
        assert!(ra.read(64).unwrap().is_empty());

        // Once A closes, B's read reports EOF (a closed stream).
        socket_close(&sock, a);
        assert!(matches!(rb.read(64), Err(StreamError::Closed)));
    }

    /// A blocking read parks until the peer (on another thread) writes.
    #[test]
    fn socket_blocking_read_unblocks_on_peer_write() {
        let sock = new_socket();
        let a = socket_connect(&sock);
        let b = socket_connect(&sock);

        let writer = sock.clone();
        // Endpoint `b` delivering to reader `a` writes into queue `a`.
        let handle = std::thread::spawn(move || {
            // Give the reader time to actually block on the condvar first.
            std::thread::sleep(std::time::Duration::from_millis(50));
            socket_write(&writer, a, b"delivered");
        });

        // This blocks until the writer thread delivers.
        let (data, eof) = socket_read_blocking(&sock, a, 64);
        handle.join().unwrap();
        assert_eq!(&data, b"delivered");
        assert!(!eof);
        let _ = b;
    }
}
