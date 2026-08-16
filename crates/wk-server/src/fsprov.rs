//! Host side of `wk:fs/provider` — a node serving a filesystem to other nodes
//! (wk's FUSE; see `wit-fs/world.wit`). The consumer half lives in `wk-vfs`:
//! a provider mount in another node's tree forwards descriptor operations
//! into this node's [`ProviderConn`], and the serving guest pulls them here
//! via the blocking `next-request` / `reply` loop, `/dev/fuse`-style.
//!
//! `next-request` blocks the provider guest's own thread inside a host call,
//! where the epoch interrupt cannot reach — so the wait polls the node's kill
//! flag and returns `none` on shutdown, which tells the guest to leave its
//! serve loop (and the epoch trap finishes the job on the next wasm step).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use wasmtime::component::{HasData, Linker};
use wasmtime::Result;

use crate::plugin::HostState;
use wk_vfs::{FsDirent, FsEntryKind, FsError, FsOp, FsOpened, FsReplyData, FsStat, ProviderConn};

wasmtime::component::bindgen!({
    path: "wit-fs",
    world: "fs-provider-host",
    imports: { default: trappable },
    require_store_data_send: true,
});

use wk::fs::provider as wit;

/// What a store needs to serve `wk:fs`: the node's conduit and its kill flag
/// (so a blocked `next-request` notices shutdown). `None` for contexts that
/// don't serve — exec children, build `RUN` steps, per-request http stores.
pub struct FsServeCtx {
    pub conn: Arc<ProviderConn>,
    pub kill: Arc<AtomicBool>,
}

pub fn add_to_linker(l: &mut Linker<HostState>) -> Result<()> {
    wit::add_to_linker::<_, HasFs>(l, |s| s)?;
    Ok(())
}

struct HasFs;
impl HasData for HasFs {
    type Data<'a> = &'a mut HostState;
}

/// How long one `next-request` wait lasts before re-checking the kill flag.
const POLL: Duration = Duration::from_millis(50);

impl wit::Host for HostState {
    fn next_request(&mut self) -> Result<Option<wit::Request>> {
        let Some(serve) = &self.fs_serve else {
            // Not a serving context: tell the guest's loop to stop.
            return Ok(None);
        };
        loop {
            if serve.kill.load(Ordering::Relaxed) {
                return Ok(None);
            }
            if let Some((id, op)) = serve.conn.next_request(POLL) {
                return Ok(Some(wit::Request {
                    id,
                    op: op_to_wit(op),
                }));
            }
        }
    }

    fn reply(
        &mut self,
        id: u64,
        result: std::result::Result<wit::ReplyData, wit::Error>,
    ) -> Result<()> {
        let Some(serve) = &self.fs_serve else {
            return Ok(());
        };
        serve.conn.reply(
            id,
            match result {
                Ok(data) => Ok(reply_from_wit(data)),
                Err(e) => Err(error_from_wit(e)),
            },
        );
        Ok(())
    }
}

fn kind_from_wit(k: wit::EntryKind) -> FsEntryKind {
    match k {
        wit::EntryKind::File => FsEntryKind::File,
        wit::EntryKind::Dir => FsEntryKind::Dir,
    }
}

fn op_to_wit(op: FsOp) -> wit::Op {
    match op {
        FsOp::Getattr { path } => wit::Op::Getattr(path),
        FsOp::Readdir { path } => wit::Op::Readdir(path),
        FsOp::Open {
            path,
            create,
            truncate,
            exclusive,
        } => wit::Op::Open(wit::OpenArgs {
            path,
            create,
            truncate,
            exclusive,
        }),
        FsOp::Read {
            handle,
            offset,
            len,
        } => wit::Op::Read(wit::ReadArgs {
            handle,
            offset,
            len,
        }),
        FsOp::Write {
            handle,
            offset,
            data,
        } => wit::Op::Write(wit::WriteArgs {
            handle,
            offset,
            data,
        }),
        FsOp::Release { handle } => wit::Op::Release(handle),
        FsOp::SetSize { handle, size } => wit::Op::SetSize(wit::SetSizeArgs { handle, size }),
        FsOp::Mkdir { path } => wit::Op::Mkdir(path),
        FsOp::Unlink { path } => wit::Op::Unlink(path),
        FsOp::Rmdir { path } => wit::Op::Rmdir(path),
        FsOp::Rename { from, to } => wit::Op::Rename(wit::RenameArgs {
            src: from,
            dest: to,
        }),
    }
}

fn reply_from_wit(data: wit::ReplyData) -> FsReplyData {
    match data {
        wit::ReplyData::Done => FsReplyData::Done,
        wit::ReplyData::Attr(s) => FsReplyData::Attr(FsStat {
            kind: kind_from_wit(s.kind),
            size: s.size,
        }),
        wit::ReplyData::Entries(list) => FsReplyData::Entries(
            list.into_iter()
                .map(|d| FsDirent {
                    name: d.name,
                    kind: kind_from_wit(d.kind),
                })
                .collect(),
        ),
        wit::ReplyData::Opened(o) => FsReplyData::Opened(FsOpened {
            handle: o.handle,
            kind: kind_from_wit(o.kind),
            size: o.size,
        }),
        wit::ReplyData::Data(r) => FsReplyData::Data {
            bytes: r.bytes,
            eof: r.eof,
        },
        wit::ReplyData::Written(n) => FsReplyData::Written(n),
    }
}

fn error_from_wit(e: wit::Error) -> FsError {
    match e {
        wit::Error::NoEntry => FsError::NoEntry,
        wit::Error::NotDir => FsError::NotDir,
        wit::Error::IsDir => FsError::IsDir,
        wit::Error::Exist => FsError::Exist,
        wit::Error::NotPermitted => FsError::NotPermitted,
        wit::Error::Io => FsError::Io,
        wit::Error::TooLarge => FsError::TooLarge,
        wit::Error::Unsupported => FsError::Unsupported,
    }
}
