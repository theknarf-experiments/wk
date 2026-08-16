//! The conduit between a consumer node's `wasi:filesystem` and a *provider*
//! node serving a filesystem (wk's FUSE: a guest program answering open/read/
//! readdir for a subtree mounted into other nodes).
//!
//! Control is inverted, exactly like `/dev/fuse`: the provider guest's own
//! `run()` loop pulls requests with a blocking host import and answers them,
//! because a running component instance cannot be re-entered from outside to
//! call an export. The host side of `wasi:filesystem` (this crate) is the
//! kernel half: it turns a descriptor operation that crossed a provider mount
//! into an [`FsOp`], blocks the *consumer's* thread on [`ProviderConn::call`],
//! and maps the reply back. Requests ride a plain in-process queue + condvar —
//! never the network fabric — so a round-trip costs a wake, not a hub tick.
//!
//! Failure is EIO, never a hang: a call fails fast when no provider loop is
//! attached ([`ProviderConn::end_serving`] ran, or the node never served) and
//! times out if a live provider stops answering. Each serve loop bumps a
//! generation; open handles from a previous provider incarnation are refused,
//! so a provider restart invalidates descriptors instead of corrupting them.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// What a provider entry is. Providers serve plain trees: no symlinks or
/// devices across the boundary (a provider wanting `ls -> file` semantics
/// resolves them itself, like an NFS server would).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsEntryKind {
    File,
    Dir,
}

/// A provider's answer to `getattr`.
#[derive(Clone, Debug)]
pub struct FsStat {
    pub kind: FsEntryKind,
    pub size: u64,
}

/// One entry of a provider directory listing.
#[derive(Clone, Debug)]
pub struct FsDirent {
    pub name: String,
    pub kind: FsEntryKind,
}

/// A provider's answer to `open`: its chosen handle for the entry plus enough
/// metadata for the host to type the descriptor without a second round-trip.
#[derive(Clone, Debug)]
pub struct FsOpened {
    pub handle: u64,
    pub kind: FsEntryKind,
    pub size: u64,
}

/// One filesystem operation forwarded to a provider. Paths are relative to the
/// provider's root (`""` is the root itself), with no leading slash.
#[derive(Clone, Debug)]
pub enum FsOp {
    Getattr {
        path: String,
    },
    Readdir {
        path: String,
    },
    Open {
        path: String,
        create: bool,
        truncate: bool,
        exclusive: bool,
    },
    Read {
        handle: u64,
        offset: u64,
        len: u32,
    },
    Write {
        handle: u64,
        offset: u64,
        data: Vec<u8>,
    },
    Release {
        handle: u64,
    },
    SetSize {
        handle: u64,
        size: u64,
    },
    Mkdir {
        path: String,
    },
    Unlink {
        path: String,
    },
    Rmdir {
        path: String,
    },
    Rename {
        from: String,
        to: String,
    },
}

/// A provider's successful reply (which variant is legal depends on the op).
#[derive(Clone, Debug)]
pub enum FsReplyData {
    /// Ops with nothing to return (write-side metadata ops, release).
    Done,
    Attr(FsStat),
    Entries(Vec<FsDirent>),
    Opened(FsOpened),
    /// `read`'s bytes; `eof` true when the read reached the end of the file.
    Data {
        bytes: Vec<u8>,
        eof: bool,
    },
    /// `write`'s byte count.
    Written(u64),
}

/// Provider-reported errors plus the conduit's own failure modes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsError {
    NoEntry,
    NotDir,
    IsDir,
    Exist,
    NotPermitted,
    Io,
    TooLarge,
    Unsupported,
    /// No provider loop is attached (node not running / restarted / never
    /// served). Fails fast, so an unwired provider reads as EIO, not a hang.
    Dead,
    /// A live provider didn't answer within [`CALL_TIMEOUT`].
    Timeout,
}

/// How long a consumer waits for a *live* provider's answer before giving up.
/// Long enough for a slow backend (a provider may itself be a network
/// filesystem), short enough that a wedged or self-mounted provider resolves
/// to EIO instead of hanging a guest forever.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// Cap on queued-but-unclaimed requests, so a stuck provider can't accumulate
/// unbounded ops from many consumers.
const MAX_QUEUE: usize = 1024;

/// The reply slot one blocked consumer call waits on.
type ReplySlot = Arc<(Mutex<Option<Result<FsReplyData, FsError>>>, Condvar)>;

struct Inner {
    /// Requests issued but not yet claimed by the provider loop.
    queue: VecDeque<(u64, FsOp)>,
    /// Claimed (or queued) requests still owed a reply, by id. Fire-and-forget
    /// ops ([`ProviderConn::cast`]) never appear here.
    outstanding: HashMap<u64, ReplySlot>,
    next_id: u64,
    /// Whether a provider serve loop is currently attached.
    serving: bool,
    /// Bumped every time a serve loop detaches: handles minted by an earlier
    /// incarnation are refused by generation mismatch.
    generation: u64,
}

/// One provider node's request conduit. Created once per app node and shared:
/// the serving guest pulls from it, every consumer whose vfs has this node
/// mounted pushes into it.
pub struct ProviderConn {
    inner: Mutex<Inner>,
    /// Wakes the provider's blocking `next_request` wait.
    req_cv: Condvar,
}

impl Default for ProviderConn {
    fn default() -> Self {
        ProviderConn {
            inner: Mutex::new(Inner {
                queue: VecDeque::new(),
                outstanding: HashMap::new(),
                next_id: 1,
                serving: false,
                generation: 0,
            }),
            req_cv: Condvar::new(),
        }
    }
}

impl ProviderConn {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Issue `op` and block until the provider answers, it dies, or the
    /// deadline passes. Called on the *consumer* guest's thread from inside a
    /// `wasi:filesystem` host function — the caller must not hold its `Fs`
    /// lock, or a slow provider would stall the host's reconciler too.
    pub fn call(&self, op: FsOp) -> Result<FsReplyData, FsError> {
        let slot: ReplySlot = Arc::new((Mutex::new(None), Condvar::new()));
        let id = {
            let mut g = self.inner.lock().unwrap();
            if !g.serving {
                return Err(FsError::Dead);
            }
            if g.queue.len() >= MAX_QUEUE {
                return Err(FsError::Io);
            }
            let id = g.next_id;
            g.next_id += 1;
            g.queue.push_back((id, op));
            g.outstanding.insert(id, slot.clone());
            id
        };
        self.req_cv.notify_all();

        let (lock, cv) = &*slot;
        let deadline = Instant::now() + CALL_TIMEOUT;
        let mut got = lock.lock().unwrap();
        while got.is_none() {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            got = cv.wait_timeout(got, deadline - now).unwrap().0;
        }
        match got.take() {
            Some(result) => result,
            None => {
                // Timed out: withdraw the request so a later answer is dropped
                // instead of filling a slot nobody waits on.
                let mut g = self.inner.lock().unwrap();
                g.outstanding.remove(&id);
                g.queue.retain(|(qid, _)| *qid != id);
                Err(FsError::Timeout)
            }
        }
    }

    /// Issue `op` without waiting for an answer — used for `release` from a
    /// descriptor `Drop`, where blocking would stall table teardown.
    pub fn cast(&self, op: FsOp) {
        let mut g = self.inner.lock().unwrap();
        if !g.serving || g.queue.len() >= MAX_QUEUE {
            return;
        }
        let id = g.next_id;
        g.next_id += 1;
        g.queue.push_back((id, op));
        drop(g);
        self.req_cv.notify_all();
    }

    /// Provider side: block up to `wait` for the next request. `None` after the
    /// wait means "nothing yet" — the serve loop checks its shutdown flag and
    /// calls again, so a kill reaches a blocked provider within one `wait`.
    pub fn next_request(&self, wait: Duration) -> Option<(u64, FsOp)> {
        let mut g = self.inner.lock().unwrap();
        if let Some(req) = g.queue.pop_front() {
            return Some(req);
        }
        g = self.req_cv.wait_timeout(g, wait).unwrap().0;
        g.queue.pop_front()
    }

    /// Provider side: answer request `id`. Unknown ids (a timed-out caller, a
    /// `cast`) are silently dropped.
    pub fn reply(&self, id: u64, result: Result<FsReplyData, FsError>) {
        let slot = self.inner.lock().unwrap().outstanding.remove(&id);
        if let Some(slot) = slot {
            let (lock, cv) = &*slot;
            *lock.lock().unwrap() = Some(result);
            cv.notify_all();
        }
    }

    /// A provider serve loop is attaching (its node just started running).
    pub fn begin_serving(&self) {
        self.inner.lock().unwrap().serving = true;
    }

    /// The serve loop detached (node exited or was killed): fail every call in
    /// flight and refuse new ones until a loop attaches again. Bumps the
    /// generation so handles from this incarnation die with it.
    pub fn end_serving(&self) {
        let mut g = self.inner.lock().unwrap();
        g.serving = false;
        g.generation += 1;
        g.queue.clear();
        for (_, slot) in g.outstanding.drain() {
            let (lock, cv) = &*slot;
            *lock.lock().unwrap() = Some(Err(FsError::Dead));
            cv.notify_all();
        }
    }

    /// The current serve-loop incarnation (see [`Self::end_serving`]).
    pub fn generation(&self) -> u64 {
        self.inner.lock().unwrap().generation
    }

    pub fn is_serving(&self) -> bool {
        self.inner.lock().unwrap().serving
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_round_trips_through_a_serve_loop() {
        let conn = ProviderConn::new();
        conn.begin_serving();
        let server = conn.clone();
        let t = std::thread::spawn(move || {
            let (id, op) = server
                .next_request(Duration::from_secs(5))
                .expect("a request");
            match op {
                FsOp::Getattr { path } => assert_eq!(path, "hello.txt"),
                other => panic!("unexpected op {other:?}"),
            }
            server.reply(
                id,
                Ok(FsReplyData::Attr(FsStat {
                    kind: FsEntryKind::File,
                    size: 5,
                })),
            );
        });
        let reply = conn.call(FsOp::Getattr {
            path: "hello.txt".into(),
        });
        t.join().unwrap();
        match reply {
            Ok(FsReplyData::Attr(st)) => assert_eq!(st.size, 5),
            other => panic!("unexpected reply {other:?}"),
        }
    }

    #[test]
    fn a_dead_provider_fails_fast_and_a_detach_fails_in_flight_calls() {
        let conn = ProviderConn::new();
        // Never served: immediate EIO-shaped failure, no timeout wait.
        assert_eq!(
            conn.call(FsOp::Getattr { path: "x".into() }).unwrap_err(),
            FsError::Dead
        );

        conn.begin_serving();
        let waiter = conn.clone();
        let t = std::thread::spawn(move || {
            waiter.call(FsOp::Getattr { path: "y".into() }).unwrap_err()
        });
        // Let the call enqueue, then detach the loop out from under it.
        while conn.inner.lock().unwrap().outstanding.is_empty() {
            std::thread::yield_now();
        }
        conn.end_serving();
        assert_eq!(t.join().unwrap(), FsError::Dead);
        assert_eq!(conn.generation(), 1);
    }
}
