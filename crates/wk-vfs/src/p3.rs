//! `wasi:filesystem@0.3` over the same vfs as 0.2 — closing the gap where
//! wasip3 guests saw wasmtime's empty host filesystem instead of their node's
//! layers, mounts, devices, and provider mounts.
//!
//! Shape: 0.3 replaces `wasi:io` streams with component-model-native
//! `stream<u8>`/`future<result<_, error-code>>` pairs and makes every path
//! operation an async function on an [`Accessor`]. The path operations here
//! are thin adapters over the crate's 0.2 trait impls (same [`Descriptor`]
//! resource type, same table, same resolution and provider forwarding), with
//! type/error mapping between the two generated namespaces. The stream halves
//! are implemented directly over the vfs primitives — a producer/consumer
//! captures the backing handle (`SharedFs` + node id, shared bytes, host
//! path, device kind, or a provider mount's remote descriptor) at call time,
//! exactly how wasmtime-wasi's own p3 filesystem captures `File` handles.

use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::sync::oneshot;
use wasmtime::component::{
    Access, Accessor, Destination, FutureReader, Linker, Resource, Source, StreamConsumer,
    StreamProducer, StreamReader, StreamResult, VecBuffer,
};
use wasmtime::StoreContextMut;
use wasmtime_wasi_io::bytes::Bytes;
use wasmtime_wasi_io::IoView;

use crate::wasi::filesystem::types::HostDescriptor as P2Descriptor;
use crate::wasi::filesystem::types::{
    DescriptorFlags as P2DescriptorFlags, DescriptorType as P2DescriptorType,
    ErrorCode as P2ErrorCode, OpenFlags as P2OpenFlags, PathFlags as P2PathFlags,
};
use crate::{
    node_kind, DescPlace, Descriptor, FsOp, FsReplyData, HasFs, Kind, RemoteDesc, SharedFile,
    SharedFs, VfsImpl, VfsView, DEVICE_READ_CHUNK, FILE_READ_CHUNK,
};

wasmtime::component::bindgen!({
    path: "wit-p3",
    world: "fs-host-p3",
    imports: {
        "wasi:filesystem/types.[method]descriptor.read-via-stream": store | trappable,
        "wasi:filesystem/types.[method]descriptor.write-via-stream": store | trappable,
        "wasi:filesystem/types.[method]descriptor.append-via-stream": store | trappable,
        "wasi:filesystem/types.[method]descriptor.read-directory": store | trappable,
        default: trappable,
    },
    with: {
        "wasi:filesystem/types.descriptor": crate::Descriptor,
    },
    trappable_error_type: {
        "wasi:filesystem/types.error-code" => crate::p3::FilesystemError,
    },
    require_store_data_send: true,
});

pub use wasi::filesystem::types;
use wasi::filesystem::types::{
    DescriptorFlags, DescriptorStat, DescriptorType, DirectoryEntry, ErrorCode, Filesize,
    MetadataHashValue, NewTimestamp, OpenFlags, PathFlags,
};

pub type FilesystemError = wasmtime_wasi::TrappableError<ErrorCode>;
pub type FilesystemResult<T> = Result<T, FilesystemError>;

/// Add wk's `wasi:filesystem@0.3` to the linker, alongside (not instead of)
/// the 0.2 vfs — a guest compiled against either generation sees the same
/// filesystem.
pub fn add_to_linker<T: VfsView + Send + 'static>(l: &mut Linker<T>) -> wasmtime::Result<()> {
    wasi::filesystem::types::add_to_linker::<_, HasFs<T>>(l, |s| VfsImpl(s))?;
    wasi::filesystem::preopens::add_to_linker::<_, HasFs<T>>(l, |s| VfsImpl(s))?;
    Ok(())
}

/// Map a 0.2 error code onto its 0.3 twin. The 0.3 set dropped `would-block`
/// (component-model async made it meaningless); nothing in the vfs emits it,
/// but map it to `io` rather than trap if it ever appears.
fn code3(c: P2ErrorCode) -> ErrorCode {
    match c {
        P2ErrorCode::Access => ErrorCode::Access,
        P2ErrorCode::WouldBlock => ErrorCode::Io,
        P2ErrorCode::Already => ErrorCode::Already,
        P2ErrorCode::BadDescriptor => ErrorCode::BadDescriptor,
        P2ErrorCode::Busy => ErrorCode::Busy,
        P2ErrorCode::Deadlock => ErrorCode::Deadlock,
        P2ErrorCode::Quota => ErrorCode::Quota,
        P2ErrorCode::Exist => ErrorCode::Exist,
        P2ErrorCode::FileTooLarge => ErrorCode::FileTooLarge,
        P2ErrorCode::IllegalByteSequence => ErrorCode::IllegalByteSequence,
        P2ErrorCode::InProgress => ErrorCode::InProgress,
        P2ErrorCode::Interrupted => ErrorCode::Interrupted,
        P2ErrorCode::Invalid => ErrorCode::Invalid,
        P2ErrorCode::Io => ErrorCode::Io,
        P2ErrorCode::IsDirectory => ErrorCode::IsDirectory,
        P2ErrorCode::Loop => ErrorCode::Loop,
        P2ErrorCode::TooManyLinks => ErrorCode::TooManyLinks,
        P2ErrorCode::MessageSize => ErrorCode::MessageSize,
        P2ErrorCode::NameTooLong => ErrorCode::NameTooLong,
        P2ErrorCode::NoDevice => ErrorCode::NoDevice,
        P2ErrorCode::NoEntry => ErrorCode::NoEntry,
        P2ErrorCode::NoLock => ErrorCode::NoLock,
        P2ErrorCode::InsufficientMemory => ErrorCode::InsufficientMemory,
        P2ErrorCode::InsufficientSpace => ErrorCode::InsufficientSpace,
        P2ErrorCode::NotDirectory => ErrorCode::NotDirectory,
        P2ErrorCode::NotEmpty => ErrorCode::NotEmpty,
        P2ErrorCode::NotRecoverable => ErrorCode::NotRecoverable,
        P2ErrorCode::Unsupported => ErrorCode::Unsupported,
        P2ErrorCode::NoTty => ErrorCode::NoTty,
        P2ErrorCode::NoSuchDevice => ErrorCode::NoSuchDevice,
        P2ErrorCode::Overflow => ErrorCode::Overflow,
        P2ErrorCode::NotPermitted => ErrorCode::NotPermitted,
        P2ErrorCode::Pipe => ErrorCode::Pipe,
        P2ErrorCode::ReadOnly => ErrorCode::ReadOnly,
        P2ErrorCode::InvalidSeek => ErrorCode::InvalidSeek,
        P2ErrorCode::TextFileBusy => ErrorCode::TextFileBusy,
        P2ErrorCode::CrossDevice => ErrorCode::CrossDevice,
    }
}

fn type3(t: P2DescriptorType) -> DescriptorType {
    match t {
        P2DescriptorType::Unknown => DescriptorType::Other(None),
        P2DescriptorType::BlockDevice => DescriptorType::BlockDevice,
        P2DescriptorType::CharacterDevice => DescriptorType::CharacterDevice,
        P2DescriptorType::Directory => DescriptorType::Directory,
        P2DescriptorType::Fifo => DescriptorType::Fifo,
        P2DescriptorType::SymbolicLink => DescriptorType::SymbolicLink,
        P2DescriptorType::RegularFile => DescriptorType::RegularFile,
        P2DescriptorType::Socket => DescriptorType::Socket,
    }
}

fn path_flags2(f: PathFlags) -> P2PathFlags {
    let mut out = P2PathFlags::empty();
    if f.contains(PathFlags::SYMLINK_FOLLOW) {
        out |= P2PathFlags::SYMLINK_FOLLOW;
    }
    out
}

fn open_flags2(f: OpenFlags) -> P2OpenFlags {
    let mut out = P2OpenFlags::empty();
    if f.contains(OpenFlags::CREATE) {
        out |= P2OpenFlags::CREATE;
    }
    if f.contains(OpenFlags::DIRECTORY) {
        out |= P2OpenFlags::DIRECTORY;
    }
    if f.contains(OpenFlags::EXCLUSIVE) {
        out |= P2OpenFlags::EXCLUSIVE;
    }
    if f.contains(OpenFlags::TRUNCATE) {
        out |= P2OpenFlags::TRUNCATE;
    }
    out
}

fn descriptor_flags2(f: DescriptorFlags) -> P2DescriptorFlags {
    let mut out = P2DescriptorFlags::empty();
    if f.contains(DescriptorFlags::READ) {
        out |= P2DescriptorFlags::READ;
    }
    if f.contains(DescriptorFlags::WRITE) {
        out |= P2DescriptorFlags::WRITE;
    }
    out
}

fn descriptor_flags3(f: P2DescriptorFlags) -> DescriptorFlags {
    let mut out = DescriptorFlags::empty();
    if f.contains(P2DescriptorFlags::READ) {
        out |= DescriptorFlags::READ;
    }
    if f.contains(P2DescriptorFlags::WRITE) {
        out |= DescriptorFlags::WRITE;
    }
    out
}

/// Adapt a 0.2 `Result<Result<T, code>>` into a 0.3 `FilesystemResult<U>`.
fn adapt<T, U>(
    r: wasmtime::Result<Result<T, P2ErrorCode>>,
    f: impl FnOnce(T) -> U,
) -> FilesystemResult<U> {
    match r.map_err(FilesystemError::trap)? {
        Ok(v) => Ok(f(v)),
        Err(c) => Err(code3(c).into()),
    }
}

// ---- stream halves: read producer, write consumer, directory list producer ----

/// Host-chosen chunk size when the guest's read buffer size is unknown.
const DEFAULT_BUFFER_CAPACITY: usize = 8192;

/// Where a 0.3 read stream's bytes come from, captured at call time.
enum ReadSrc {
    /// A snapshot of a local/shared/host file's remaining bytes (same
    /// snapshot semantics as the 0.2 `read-via-stream`).
    Bytes(Bytes),
    Zero,
    Random,
    /// An open file behind a provider mount: each chunk is one forwarded
    /// `read` (the call blocks this node's thread, exactly like 0.2 reads).
    Remote {
        remote: RemoteDesc,
        handle: u64,
        offset: u64,
    },
}

struct VfsReadProducer {
    src: ReadSrc,
    pos: usize,
    result: Option<oneshot::Sender<Result<(), ErrorCode>>>,
}

impl VfsReadProducer {
    fn close(&mut self, res: Result<(), ErrorCode>) {
        if let Some(tx) = self.result.take() {
            let _ = tx.send(res);
        }
    }
}

impl Drop for VfsReadProducer {
    fn drop(&mut self) {
        self.close(Ok(()));
    }
}

impl<D> StreamProducer<D> for VfsReadProducer {
    type Item = u8;
    type Buffer = VecBuffer<u8>;

    fn poll_produce<'a>(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        store: StoreContextMut<'a, D>,
        dst: Destination<'a, Self::Item, Self::Buffer>,
        _finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        let mut dst = dst.as_direct(store, DEFAULT_BUFFER_CAPACITY);
        let buf = dst.remaining();
        if buf.is_empty() {
            return Poll::Ready(Ok(StreamResult::Completed));
        }
        let me = &mut *self;
        match &mut me.src {
            ReadSrc::Bytes(data) => {
                if me.pos >= data.len() {
                    me.close(Ok(()));
                    return Poll::Ready(Ok(StreamResult::Dropped));
                }
                let n = buf.len().min(data.len() - me.pos).min(FILE_READ_CHUNK);
                buf[..n].copy_from_slice(&data[me.pos..me.pos + n]);
                me.pos += n;
                dst.mark_written(n);
                Poll::Ready(Ok(StreamResult::Completed))
            }
            ReadSrc::Zero => {
                let n = buf.len().min(DEVICE_READ_CHUNK);
                buf[..n].fill(0);
                dst.mark_written(n);
                Poll::Ready(Ok(StreamResult::Completed))
            }
            ReadSrc::Random => {
                let n = buf.len().min(DEVICE_READ_CHUNK);
                let _ = getrandom::fill(&mut buf[..n]);
                dst.mark_written(n);
                Poll::Ready(Ok(StreamResult::Completed))
            }
            ReadSrc::Remote {
                remote,
                handle,
                offset,
            } => {
                if !remote.live() {
                    me.close(Err(ErrorCode::Io));
                    return Poll::Ready(Ok(StreamResult::Dropped));
                }
                let len = buf.len().min(FILE_READ_CHUNK) as u32;
                match remote.conn.call(FsOp::Read {
                    handle: *handle,
                    offset: *offset,
                    len,
                }) {
                    Ok(FsReplyData::Data { bytes, eof }) => {
                        if bytes.is_empty() && eof {
                            me.close(Ok(()));
                            return Poll::Ready(Ok(StreamResult::Dropped));
                        }
                        let n = bytes.len().min(buf.len());
                        buf[..n].copy_from_slice(&bytes[..n]);
                        *offset += n as u64;
                        dst.mark_written(n);
                        Poll::Ready(Ok(StreamResult::Completed))
                    }
                    _ => {
                        me.close(Err(ErrorCode::Io));
                        Poll::Ready(Ok(StreamResult::Dropped))
                    }
                }
            }
        }
    }
}

/// Where a 0.3 write stream's bytes land, captured at call time.
enum WriteDst {
    /// A private in-memory file (a layer file copy-ups on first write).
    Local {
        fs: SharedFs,
        node: u64,
    },
    Shared(SharedFile),
    Host(std::path::PathBuf),
    /// The device files: every byte accepted and discarded.
    Null,
    /// An open file behind a provider mount.
    Remote {
        remote: RemoteDesc,
        handle: u64,
    },
}

struct VfsWriteConsumer {
    dst: WriteDst,
    offset: u64,
    result: Option<oneshot::Sender<Result<(), ErrorCode>>>,
}

impl VfsWriteConsumer {
    fn close(&mut self, res: Result<(), ErrorCode>) {
        if let Some(tx) = self.result.take() {
            let _ = tx.send(res);
        }
    }

    /// Write `bytes` at the moving offset; `Err` is the code for the guest.
    fn write(&mut self, bytes: &[u8]) -> Result<(), ErrorCode> {
        match &self.dst {
            WriteDst::Local { fs, node } => {
                let mut g = fs.lock().unwrap();
                g.copy_up(*node);
                match g.nodes.get_mut(node) {
                    Some(crate::Node::File(data)) => crate::write_at(data, self.offset, bytes)
                        .map_err(|_| ErrorCode::FileTooLarge)?,
                    _ => return Err(ErrorCode::NoEntry),
                }
            }
            WriteDst::Shared(sh) => crate::write_at(&mut sh.lock().unwrap(), self.offset, bytes)
                .map_err(|_| ErrorCode::FileTooLarge)?,
            WriteDst::Host(p) => {
                crate::host_write_at(p, self.offset, bytes).map_err(|_| ErrorCode::Io)?
            }
            WriteDst::Null => {}
            WriteDst::Remote { remote, handle } => {
                if !remote.live() {
                    return Err(ErrorCode::Io);
                }
                match remote.conn.call(FsOp::Write {
                    handle: *handle,
                    offset: self.offset,
                    data: bytes.to_vec(),
                }) {
                    Ok(FsReplyData::Written(_)) => {}
                    _ => return Err(ErrorCode::Io),
                }
            }
        }
        self.offset += bytes.len() as u64;
        Ok(())
    }
}

impl Drop for VfsWriteConsumer {
    fn drop(&mut self) {
        self.close(Ok(()));
    }
}

impl<D> StreamConsumer<D> for VfsWriteConsumer {
    type Item = u8;

    fn poll_consume(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        store: StoreContextMut<D>,
        src: Source<Self::Item>,
        _finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        let mut src = src.as_direct(store);
        let bytes = src.remaining().to_vec();
        match self.write(&bytes) {
            Ok(()) => {
                src.mark_read(bytes.len());
                Poll::Ready(Ok(StreamResult::Completed))
            }
            Err(code) => {
                self.close(Err(code));
                Poll::Ready(Ok(StreamResult::Dropped))
            }
        }
    }
}

/// A snapshot directory listing served as a `stream<directory-entry>`.
struct ListProducer {
    items: std::vec::IntoIter<DirectoryEntry>,
    result: Option<oneshot::Sender<Result<(), ErrorCode>>>,
}

impl ListProducer {
    fn close(&mut self, res: Result<(), ErrorCode>) {
        if let Some(tx) = self.result.take() {
            let _ = tx.send(res);
        }
    }
}

impl Drop for ListProducer {
    fn drop(&mut self) {
        self.close(Ok(()));
    }
}

impl<D> StreamProducer<D> for ListProducer {
    type Item = DirectoryEntry;
    type Buffer = VecBuffer<DirectoryEntry>;

    fn poll_produce<'a>(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        mut store: StoreContextMut<'a, D>,
        mut dst: Destination<'a, Self::Item, Self::Buffer>,
        _finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        let count = dst.remaining(&mut store).unwrap_or(32).max(1);
        let buf: Vec<DirectoryEntry> = self.items.by_ref().take(count).collect();
        if buf.is_empty() {
            self.close(Ok(()));
            return Poll::Ready(Ok(StreamResult::Dropped));
        }
        dst.set_buffer(buf.into());
        Poll::Ready(Ok(StreamResult::Completed))
    }
}

// ---- call-time inspection: what backs a descriptor's stream ----

/// What backs a read stream for `fd` at `offset`, or the error to report.
fn read_source<T: VfsView>(
    view: &mut VfsImpl<&mut T>,
    fd: &Resource<Descriptor>,
    offset: u64,
) -> wasmtime::Result<Result<ReadSrc, ErrorCode>> {
    let d = view.table().get(fd)?;
    let (fs, place) = (d.fs.clone(), d.place.clone());
    match place {
        DescPlace::Remote(r) => {
            let Some(handle) = r.handle else {
                return Ok(Err(ErrorCode::IsDirectory));
            };
            if !r.live() {
                return Ok(Err(ErrorCode::Io));
            }
            Ok(Ok(ReadSrc::Remote {
                remote: r,
                handle,
                offset,
            }))
        }
        DescPlace::Local(node) => Ok(match node_kind(&fs, node) {
            Kind::File | Kind::Ro(_) | Kind::Shared(_) | Kind::Host(_) => {
                // Reuse the 0.2 snapshot logic verbatim: the whole remaining
                // content from `offset`, copy-on-read.
                let g = fs.lock().unwrap();
                match crate::snapshot_from(&g, node, offset) {
                    Some(bytes) => Ok(ReadSrc::Bytes(bytes)),
                    None => Err(ErrorCode::NoEntry),
                }
            }
            Kind::Null => Ok(ReadSrc::Bytes(Bytes::new())),
            Kind::Zero => Ok(ReadSrc::Zero),
            Kind::Random => Ok(ReadSrc::Random),
            Kind::Dir => Err(ErrorCode::IsDirectory),
            Kind::Missing => Err(ErrorCode::NoEntry),
        }),
    }
}

/// What backs a write stream for `fd`, or the error to report. `append`
/// resolves the starting offset to the current end of the file.
fn write_target<T: VfsView>(
    view: &mut VfsImpl<&mut T>,
    fd: &Resource<Descriptor>,
    append: bool,
    offset: u64,
) -> wasmtime::Result<Result<(WriteDst, u64), ErrorCode>> {
    let d = view.table().get(fd)?;
    let (fs, place) = (d.fs.clone(), d.place.clone());
    match place {
        DescPlace::Remote(r) => {
            if r.readonly {
                return Ok(Err(ErrorCode::NotPermitted));
            }
            let Some(handle) = r.handle else {
                return Ok(Err(ErrorCode::IsDirectory));
            };
            if !r.live() {
                return Ok(Err(ErrorCode::Io));
            }
            let offset = if append {
                match crate::remote_stat(&r.conn, &r.path) {
                    Ok(st) => st.size,
                    Err(_) => return Ok(Err(ErrorCode::Io)),
                }
            } else {
                offset
            };
            Ok(Ok((WriteDst::Remote { remote: r, handle }, offset)))
        }
        DescPlace::Local(node) => {
            if crate::is_readonly(&fs, node) {
                return Ok(Err(ErrorCode::NotPermitted));
            }
            let (dst, len) = match node_kind(&fs, node) {
                Kind::File | Kind::Ro(_) => {
                    let len = {
                        let mut g = fs.lock().unwrap();
                        if append {
                            g.copy_up(node);
                        }
                        match g.nodes.get(&node) {
                            Some(crate::Node::File(d)) => d.len() as u64,
                            Some(crate::Node::RoFile(d)) => d.len() as u64,
                            _ => 0,
                        }
                    };
                    (
                        WriteDst::Local {
                            fs: fs.clone(),
                            node,
                        },
                        len,
                    )
                }
                Kind::Shared(sh) => {
                    let len = sh.lock().unwrap().len() as u64;
                    (WriteDst::Shared(sh), len)
                }
                Kind::Host(p) => {
                    let len = crate::host_size(&p);
                    (WriteDst::Host(p), len)
                }
                Kind::Null | Kind::Zero | Kind::Random => (WriteDst::Null, 0),
                Kind::Dir => return Ok(Err(ErrorCode::IsDirectory)),
                Kind::Missing => return Ok(Err(ErrorCode::NoEntry)),
            };
            Ok(Ok((dst, if append { len } else { offset })))
        }
    }
}

/// The full directory listing for `fd`, or the error to report.
fn list_entries<T: VfsView>(
    view: &mut VfsImpl<&mut T>,
    fd: &Resource<Descriptor>,
) -> wasmtime::Result<Result<Vec<DirectoryEntry>, ErrorCode>> {
    let d = view.table().get(fd)?;
    let (fs, place) = (d.fs.clone(), d.place.clone());
    match place {
        DescPlace::Remote(r) => {
            if r.kind != crate::FsEntryKind::Dir {
                return Ok(Err(ErrorCode::NotDirectory));
            }
            match r.conn.call(FsOp::Readdir {
                path: r.path.clone(),
            }) {
                Ok(FsReplyData::Entries(list)) => Ok(Ok(list
                    .into_iter()
                    .map(|e| DirectoryEntry {
                        type_: match e.kind {
                            crate::FsEntryKind::Dir => DescriptorType::Directory,
                            crate::FsEntryKind::File => DescriptorType::RegularFile,
                        },
                        name: e.name,
                    })
                    .collect())),
                Ok(_) => Ok(Err(ErrorCode::Io)),
                Err(e) => Ok(Err(code3(crate::provider_err(e)))),
            }
        }
        DescPlace::Local(node) => {
            let g = fs.lock().unwrap();
            match g.nodes.get(&node) {
                Some(crate::Node::Dir(children)) => Ok(Ok(children
                    .iter()
                    .map(|(name, id)| DirectoryEntry {
                        type_: type3(crate::node_type(&g, *id)),
                        name: name.clone(),
                    })
                    .collect())),
                Some(_) => Ok(Err(ErrorCode::NotDirectory)),
                None => Ok(Err(ErrorCode::NoEntry)),
            }
        }
    }
}

// ---- the generated host traits ----

impl<T: VfsView + Send> wasi::filesystem::types::Host for VfsImpl<&mut T> {
    fn convert_error_code(&mut self, err: FilesystemError) -> wasmtime::Result<ErrorCode> {
        err.downcast()
    }
}

impl<T: VfsView + Send> wasi::filesystem::types::HostDescriptor for VfsImpl<&mut T> {
    fn drop(&mut self, fd: Resource<Descriptor>) -> wasmtime::Result<()> {
        self.table().delete(fd)?;
        Ok(())
    }
}

impl<T: VfsView + Send> wasi::filesystem::preopens::Host for VfsImpl<&mut T> {
    fn get_directories(&mut self) -> wasmtime::Result<Vec<(Resource<Descriptor>, String)>> {
        let fs = self.fs();
        let root = self.table().push(Descriptor::open(fs, crate::ROOT))?;
        Ok(vec![(root, "/".to_string())])
    }
}

impl<T: VfsView + Send + 'static> wasi::filesystem::types::HostDescriptorWithStore<T> for HasFs<T> {
    fn read_via_stream(
        mut store: Access<'_, T, Self>,
        fd: Resource<Descriptor>,
        offset: Filesize,
    ) -> wasmtime::Result<(StreamReader<u8>, FutureReader<Result<(), ErrorCode>>)> {
        let (tx, rx) = oneshot::channel();
        let src = {
            let mut view = store.get();
            read_source(&mut view, &fd, offset)?
        };
        let stream = match src {
            Ok(src) => StreamReader::new(
                &mut store,
                VfsReadProducer {
                    src,
                    pos: 0,
                    result: Some(tx),
                },
            )?,
            Err(code) => {
                let _ = tx.send(Err(code));
                StreamReader::new(&mut store, std::iter::empty())?
            }
        };
        Ok((stream, FutureReader::new(&mut store, rx)?))
    }

    fn write_via_stream(
        mut store: Access<'_, T, Self>,
        fd: Resource<Descriptor>,
        mut data: StreamReader<u8>,
        offset: Filesize,
    ) -> wasmtime::Result<FutureReader<Result<(), ErrorCode>>> {
        let (tx, rx) = oneshot::channel();
        let target = {
            let mut view = store.get();
            write_target(&mut view, &fd, false, offset)?
        };
        match target {
            Ok((dst, offset)) => data.pipe(
                &mut store,
                VfsWriteConsumer {
                    dst,
                    offset,
                    result: Some(tx),
                },
            )?,
            Err(code) => {
                data.close(&mut store)?;
                let _ = tx.send(Err(code));
            }
        }
        FutureReader::new(&mut store, rx)
    }

    fn append_via_stream(
        mut store: Access<'_, T, Self>,
        fd: Resource<Descriptor>,
        mut data: StreamReader<u8>,
    ) -> wasmtime::Result<FutureReader<Result<(), ErrorCode>>> {
        let (tx, rx) = oneshot::channel();
        let target = {
            let mut view = store.get();
            write_target(&mut view, &fd, true, 0)?
        };
        match target {
            Ok((dst, offset)) => data.pipe(
                &mut store,
                VfsWriteConsumer {
                    dst,
                    offset,
                    result: Some(tx),
                },
            )?,
            Err(code) => {
                data.close(&mut store)?;
                let _ = tx.send(Err(code));
            }
        }
        FutureReader::new(&mut store, rx)
    }

    fn read_directory(
        mut store: Access<'_, T, Self>,
        fd: Resource<Descriptor>,
    ) -> wasmtime::Result<(
        StreamReader<DirectoryEntry>,
        FutureReader<Result<(), ErrorCode>>,
    )> {
        let (tx, rx) = oneshot::channel();
        let entries = {
            let mut view = store.get();
            list_entries(&mut view, &fd)?
        };
        let stream = match entries {
            Ok(list) => StreamReader::new(
                &mut store,
                ListProducer {
                    items: list.into_iter(),
                    result: Some(tx),
                },
            )?,
            Err(code) => {
                let _ = tx.send(Err(code));
                StreamReader::new(&mut store, std::iter::empty())?
            }
        };
        Ok((stream, FutureReader::new(&mut store, rx)?))
    }

    async fn advise(
        _store: &Accessor<T, Self>,
        _fd: Resource<Descriptor>,
        _offset: Filesize,
        _length: Filesize,
        _advice: types::Advice,
    ) -> FilesystemResult<()> {
        Ok(())
    }

    async fn sync_data(
        _store: &Accessor<T, Self>,
        _fd: Resource<Descriptor>,
    ) -> FilesystemResult<()> {
        Ok(())
    }

    async fn get_flags(
        store: &Accessor<T, Self>,
        fd: Resource<Descriptor>,
    ) -> FilesystemResult<DescriptorFlags> {
        store.with(|mut a| {
            let mut view = a.get();
            adapt(P2Descriptor::get_flags(&mut view, fd), descriptor_flags3)
        })
    }

    async fn get_type(
        store: &Accessor<T, Self>,
        fd: Resource<Descriptor>,
    ) -> FilesystemResult<DescriptorType> {
        store.with(|mut a| {
            let mut view = a.get();
            adapt(P2Descriptor::get_type(&mut view, fd), type3)
        })
    }

    async fn set_size(
        store: &Accessor<T, Self>,
        fd: Resource<Descriptor>,
        size: Filesize,
    ) -> FilesystemResult<()> {
        store.with(|mut a| {
            let mut view = a.get();
            adapt(P2Descriptor::set_size(&mut view, fd, size), |v| v)
        })
    }

    async fn set_times(
        _store: &Accessor<T, Self>,
        _fd: Resource<Descriptor>,
        _atim: NewTimestamp,
        _mtim: NewTimestamp,
    ) -> FilesystemResult<()> {
        // The vfs stores no timestamps (same as the 0.2 impl).
        Ok(())
    }

    async fn sync(_store: &Accessor<T, Self>, _fd: Resource<Descriptor>) -> FilesystemResult<()> {
        Ok(())
    }

    async fn create_directory_at(
        store: &Accessor<T, Self>,
        fd: Resource<Descriptor>,
        path: String,
    ) -> FilesystemResult<()> {
        store.with(|mut a| {
            let mut view = a.get();
            adapt(
                P2Descriptor::create_directory_at(&mut view, fd, path),
                |v| v,
            )
        })
    }

    async fn stat(
        store: &Accessor<T, Self>,
        fd: Resource<Descriptor>,
    ) -> FilesystemResult<DescriptorStat> {
        store.with(|mut a| {
            let mut view = a.get();
            adapt(P2Descriptor::stat(&mut view, fd), stat3)
        })
    }

    async fn stat_at(
        store: &Accessor<T, Self>,
        fd: Resource<Descriptor>,
        path_flags: PathFlags,
        path: String,
    ) -> FilesystemResult<DescriptorStat> {
        store.with(|mut a| {
            let mut view = a.get();
            adapt(
                P2Descriptor::stat_at(&mut view, fd, path_flags2(path_flags), path),
                stat3,
            )
        })
    }

    async fn set_times_at(
        _store: &Accessor<T, Self>,
        _fd: Resource<Descriptor>,
        _path_flags: PathFlags,
        _path: String,
        _atim: NewTimestamp,
        _mtim: NewTimestamp,
    ) -> FilesystemResult<()> {
        Ok(())
    }

    async fn link_at(
        store: &Accessor<T, Self>,
        fd: Resource<Descriptor>,
        old_path_flags: PathFlags,
        old_path: String,
        new_fd: Resource<Descriptor>,
        new_path: String,
    ) -> FilesystemResult<()> {
        store.with(|mut a| {
            let mut view = a.get();
            adapt(
                P2Descriptor::link_at(
                    &mut view,
                    fd,
                    path_flags2(old_path_flags),
                    old_path,
                    new_fd,
                    new_path,
                ),
                |v| v,
            )
        })
    }

    async fn open_at(
        store: &Accessor<T, Self>,
        fd: Resource<Descriptor>,
        path_flags: PathFlags,
        path: String,
        open_flags: OpenFlags,
        flags: DescriptorFlags,
    ) -> FilesystemResult<Resource<Descriptor>> {
        store.with(|mut a| {
            let mut view = a.get();
            adapt(
                P2Descriptor::open_at(
                    &mut view,
                    fd,
                    path_flags2(path_flags),
                    path,
                    open_flags2(open_flags),
                    descriptor_flags2(flags),
                ),
                |v| v,
            )
        })
    }

    async fn readlink_at(
        store: &Accessor<T, Self>,
        fd: Resource<Descriptor>,
        path: String,
    ) -> FilesystemResult<String> {
        store.with(|mut a| {
            let mut view = a.get();
            adapt(P2Descriptor::readlink_at(&mut view, fd, path), |v| v)
        })
    }

    async fn remove_directory_at(
        store: &Accessor<T, Self>,
        fd: Resource<Descriptor>,
        path: String,
    ) -> FilesystemResult<()> {
        store.with(|mut a| {
            let mut view = a.get();
            adapt(
                P2Descriptor::remove_directory_at(&mut view, fd, path),
                |v| v,
            )
        })
    }

    async fn rename_at(
        store: &Accessor<T, Self>,
        fd: Resource<Descriptor>,
        old_path: String,
        new_fd: Resource<Descriptor>,
        new_path: String,
    ) -> FilesystemResult<()> {
        store.with(|mut a| {
            let mut view = a.get();
            adapt(
                P2Descriptor::rename_at(&mut view, fd, old_path, new_fd, new_path),
                |v| v,
            )
        })
    }

    async fn symlink_at(
        store: &Accessor<T, Self>,
        fd: Resource<Descriptor>,
        old_path: String,
        new_path: String,
    ) -> FilesystemResult<()> {
        store.with(|mut a| {
            let mut view = a.get();
            adapt(
                P2Descriptor::symlink_at(&mut view, fd, old_path, new_path),
                |v| v,
            )
        })
    }

    async fn unlink_file_at(
        store: &Accessor<T, Self>,
        fd: Resource<Descriptor>,
        path: String,
    ) -> FilesystemResult<()> {
        store.with(|mut a| {
            let mut view = a.get();
            adapt(P2Descriptor::unlink_file_at(&mut view, fd, path), |v| v)
        })
    }

    async fn is_same_object(
        store: &Accessor<T, Self>,
        fd: Resource<Descriptor>,
        other: Resource<Descriptor>,
    ) -> wasmtime::Result<bool> {
        store.with(|mut a| {
            let mut view = a.get();
            P2Descriptor::is_same_object(&mut view, fd, other)
        })
    }

    async fn metadata_hash(
        store: &Accessor<T, Self>,
        fd: Resource<Descriptor>,
    ) -> FilesystemResult<MetadataHashValue> {
        store.with(|mut a| {
            let mut view = a.get();
            adapt(P2Descriptor::metadata_hash(&mut view, fd), hash3)
        })
    }

    async fn metadata_hash_at(
        store: &Accessor<T, Self>,
        fd: Resource<Descriptor>,
        path_flags: PathFlags,
        path: String,
    ) -> FilesystemResult<MetadataHashValue> {
        store.with(|mut a| {
            let mut view = a.get();
            adapt(
                P2Descriptor::metadata_hash_at(&mut view, fd, path_flags2(path_flags), path),
                hash3,
            )
        })
    }
}

fn stat3(s: crate::wasi::filesystem::types::DescriptorStat) -> DescriptorStat {
    DescriptorStat {
        type_: type3(s.type_),
        link_count: s.link_count,
        size: s.size,
        // The vfs stores no timestamps.
        data_access_timestamp: None,
        data_modification_timestamp: None,
        status_change_timestamp: None,
    }
}

fn hash3(h: crate::wasi::filesystem::types::MetadataHashValue) -> MetadataHashValue {
    MetadataHashValue {
        lower: h.lower,
        upper: h.upper,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{mount_file, new_fs};
    use std::mem;
    use wasmtime::component::{Lift, ResourceTable};
    use wasmtime::{Config, Engine, Store};

    struct TestStore {
        table: ResourceTable,
        fs: SharedFs,
    }
    impl IoView for TestStore {
        fn table(&mut self) -> &mut ResourceTable {
            &mut self.table
        }
    }
    impl VfsView for TestStore {
        fn fs(&mut self) -> SharedFs {
            self.fs.clone()
        }
    }

    /// Collects a `stream<u8>` host-side; sends the bytes on drop (the pipe
    /// machinery drops the consumer when the producer's stream ends).
    struct Collect {
        data: Vec<u8>,
        tx: Option<oneshot::Sender<Vec<u8>>>,
    }
    impl<D> StreamConsumer<D> for Collect {
        type Item = u8;
        fn poll_consume(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            store: StoreContextMut<D>,
            src: Source<Self::Item>,
            _finish: bool,
        ) -> Poll<wasmtime::Result<StreamResult>> {
            let me = &mut *self;
            let mut src = src.as_direct(store);
            me.data.extend_from_slice(src.remaining());
            let n = src.remaining().len();
            src.mark_read(n);
            Poll::Ready(Ok(StreamResult::Completed))
        }
    }
    impl Drop for Collect {
        fn drop(&mut self) {
            if let Some(tx) = self.tx.take() {
                let _ = tx.send(mem::take(&mut self.data));
            }
        }
    }

    /// Collects a `stream<T>` of lifted values (directory entries).
    struct CollectItems<T: 'static> {
        items: Vec<T>,
        tx: Option<oneshot::Sender<Vec<T>>>,
    }
    impl<D, T: Lift + Send + Sync + Unpin + 'static> StreamConsumer<D> for CollectItems<T> {
        type Item = T;
        fn poll_consume(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            mut store: StoreContextMut<D>,
            mut src: Source<Self::Item>,
            _finish: bool,
        ) -> Poll<wasmtime::Result<StreamResult>> {
            let me = self.get_mut();
            // `Vec`'s stream read fills only spare `capacity`; make some.
            me.items.reserve(64);
            src.read(&mut store, &mut me.items)?;
            Poll::Ready(Ok(StreamResult::Completed))
        }
    }
    impl<T: 'static> Drop for CollectItems<T> {
        fn drop(&mut self) {
            if let Some(tx) = self.tx.take() {
                let _ = tx.send(mem::take(&mut self.items));
            }
        }
    }

    /// Resolves a `future<T>` host-side into a oneshot.
    struct FutureRx<T: Send + Sync + 'static> {
        tx: Option<oneshot::Sender<T>>,
    }
    impl<D, T: Lift + Send + Sync + 'static> wasmtime::component::FutureConsumer<D> for FutureRx<T> {
        type Item = T;
        fn poll_consume(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            mut store: StoreContextMut<D>,
            mut source: Source<'_, Self::Item>,
            _finish: bool,
        ) -> Poll<wasmtime::Result<()>> {
            let mut buf: Option<T> = None;
            source.read(&mut store, &mut buf)?;
            if let (Some(v), Some(tx)) = (buf, self.tx.take()) {
                let _ = tx.send(v);
            }
            Poll::Ready(Ok(()))
        }
    }

    fn engine() -> Engine {
        let mut config = Config::new();
        config.wasm_component_model(true);
        config.wasm_component_model_async(true);
        Engine::new(&config).expect("engine")
    }

    type Facc = Accessor<TestStore, HasFs<TestStore>>;

    fn res(fd: &Resource<Descriptor>) -> Resource<Descriptor> {
        Resource::new_own(fd.rep())
    }

    /// Unwrap a p3 filesystem result in a test, showing the code on failure.
    fn must<T>(r: FilesystemResult<T>) -> T {
        match r {
            Ok(v) => v,
            Err(e) => panic!("filesystem error: {:?}", e.downcast()),
        }
    }

    async fn await_result(
        facc: &Facc,
        fut: FutureReader<Result<(), ErrorCode>>,
    ) -> Result<(), ErrorCode> {
        let (tx, rx) = oneshot::channel();
        facc.with(|mut a| fut.pipe(&mut a, FutureRx { tx: Some(tx) }))
            .expect("pipe result future");
        rx.await.expect("result future resolves")
    }

    /// The whole 0.3 surface over one vfs: preopens, async path ops, stream
    /// reads/writes, directory streams — the same files a 0.2 guest sees.
    #[test]
    fn p3_filesystem_serves_the_vfs() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        rt.block_on(async {
            let fs = new_fs();
            {
                let mut g = fs.lock().unwrap();
                g.ensure_dir_path("sub");
                g.put_file_at("hello.txt", b"hi from the vfs".to_vec());
                g.put_file_at("sub/inner.txt", b"nested".to_vec());
            }
            let engine = engine();
            let mut store = Store::new(
                &engine,
                TestStore {
                    table: ResourceTable::new(),
                    fs: fs.clone(),
                },
            );
            store
                .run_concurrent(async |acc| {
                    use wasi::filesystem::types::HostDescriptorWithStore as S;
                    let facc: Facc = acc.with_getter(|s| VfsImpl(s));

                    // The preopen is the vfs root.
                    let root = facc
                        .with(|mut a| {
                            wasi::filesystem::preopens::Host::get_directories(&mut a.get())
                        })?
                        .remove(0)
                        .0;

                    // Async path ops: stat crosses the same resolution as 0.2.
                    let st = must(
                        S::stat_at(
                            &facc,
                            res(&root),
                            PathFlags::SYMLINK_FOLLOW,
                            "hello.txt".into(),
                        )
                        .await,
                    );
                    assert!(matches!(st.type_, DescriptorType::RegularFile));
                    assert_eq!(st.size, 15);

                    // Open + read via the component-model stream.
                    let fd = must(
                        S::open_at(
                            &facc,
                            res(&root),
                            PathFlags::SYMLINK_FOLLOW,
                            "hello.txt".into(),
                            OpenFlags::empty(),
                            DescriptorFlags::READ,
                        )
                        .await,
                    );
                    let (stream, done) = facc.with(|a| S::read_via_stream(a, res(&fd), 0))?;
                    let (tx, rx) = oneshot::channel();
                    facc.with(|mut a| {
                        stream.pipe(
                            &mut a,
                            Collect {
                                data: Vec::new(),
                                tx: Some(tx),
                            },
                        )
                    })?;
                    assert_eq!(rx.await.unwrap(), b"hi from the vfs");
                    assert!(matches!(await_result(&facc, done).await, Ok(())));

                    // Create + write via stream, then read it back.
                    let wfd = must(
                        S::open_at(
                            &facc,
                            res(&root),
                            PathFlags::SYMLINK_FOLLOW,
                            "written.txt".into(),
                            OpenFlags::CREATE,
                            DescriptorFlags::WRITE,
                        )
                        .await,
                    );
                    let data = b"streamed into the vfs".to_vec();
                    let input =
                        facc.with(|mut a| StreamReader::new(&mut a, data.into_boxed_slice()))?;
                    let done = facc.with(|a| S::write_via_stream(a, res(&wfd), input, 0))?;
                    assert!(matches!(await_result(&facc, done).await, Ok(())));
                    assert_eq!(
                        fs.lock().unwrap().read_file("/written.txt", 64).as_deref(),
                        Some(&b"streamed into the vfs"[..])
                    );

                    // Directory listing as a stream of entries.
                    let (entries, done) = facc.with(|a| S::read_directory(a, res(&root)))?;
                    let (tx, rx) = oneshot::channel();
                    facc.with(|mut a| {
                        entries.pipe(
                            &mut a,
                            CollectItems {
                                items: Vec::new(),
                                tx: Some(tx),
                            },
                        )
                    })?;
                    let mut names: Vec<String> =
                        rx.await.unwrap().into_iter().map(|e| e.name).collect();
                    names.sort();
                    assert_eq!(names, ["hello.txt", "sub", "written.txt"]);
                    assert!(matches!(await_result(&facc, done).await, Ok(())));

                    // Error paths: reading a directory's bytes fails via the
                    // result future, not a hang or a trap.
                    let dirfd = must(
                        S::open_at(
                            &facc,
                            res(&root),
                            PathFlags::SYMLINK_FOLLOW,
                            "sub".into(),
                            OpenFlags::DIRECTORY,
                            DescriptorFlags::READ,
                        )
                        .await,
                    );
                    let (mut stream, done) =
                        facc.with(|a| S::read_via_stream(a, res(&dirfd), 0))?;
                    facc.with(|mut a| stream.close(&mut a))?;
                    assert!(matches!(
                        await_result(&facc, done).await,
                        Err(ErrorCode::IsDirectory)
                    ));

                    wasmtime::error::Ok(())
                })
                .await
                .expect("run_concurrent")
                .expect("test body");
        });
    }

    /// A read-only mount refuses 0.3 stream writes through the result future.
    #[test]
    fn p3_readonly_mount_refuses_writes() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        rt.block_on(async {
            let fs = new_fs();
            let data: crate::SharedFile =
                std::sync::Arc::new(std::sync::Mutex::new(b"shared".to_vec()));
            mount_file(&fs, "ro.txt", data.clone(), false);
            let engine = engine();
            let mut store = Store::new(
                &engine,
                TestStore {
                    table: ResourceTable::new(),
                    fs: fs.clone(),
                },
            );
            store
                .run_concurrent(async |acc| {
                    use wasi::filesystem::types::HostDescriptorWithStore as S;
                    let facc: Facc = acc.with_getter(|s| VfsImpl(s));
                    let root = facc
                        .with(|mut a| {
                            wasi::filesystem::preopens::Host::get_directories(&mut a.get())
                        })?
                        .remove(0)
                        .0;
                    let fd = must(
                        S::open_at(
                            &facc,
                            res(&root),
                            PathFlags::SYMLINK_FOLLOW,
                            "ro.txt".into(),
                            OpenFlags::empty(),
                            DescriptorFlags::READ,
                        )
                        .await,
                    );
                    let input = facc.with(|mut a| {
                        StreamReader::new(&mut a, b"x".to_vec().into_boxed_slice())
                    })?;
                    let done = facc.with(|a| S::write_via_stream(a, res(&fd), input, 0))?;
                    assert!(matches!(
                        await_result(&facc, done).await,
                        Err(ErrorCode::NotPermitted)
                    ));
                    // The shared bytes never changed.
                    assert_eq!(&*data.lock().unwrap(), b"shared");
                    wasmtime::error::Ok(())
                })
                .await
                .expect("run_concurrent")
                .expect("test body");
        });
    }
}
