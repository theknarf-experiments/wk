//! hellofs — a wk node that *is* a filesystem (the first `wk:fs` provider).
//!
//! `run()` is the serve loop, the same inverted shape as a FUSE daemon reading
//! `/dev/fuse`: block on `next-request`, answer with `reply`, forever. The
//! served tree is a small in-memory map seeded with a greeting, plus one
//! synthetic file (`counter`) whose content is generated fresh on every open —
//! proof that what a consumer reads is *behavior*, not stored bytes. The rest
//! behaves like a memfs: creates, writes, mkdir, unlink, rename all work and
//! persist for as long as this node runs.

#[allow(warnings)]
mod bindings;

use std::collections::{HashMap, HashSet};

use bindings::wk::fs::provider::{
    self, Dirent, EntryKind, Error, Op, Opened, ReadResult, ReplyData, Stat,
};
use bindings::Guest;

struct Component;

/// The served tree: `files` maps full paths ("a/b.txt", no leading slash) to
/// bytes; `dirs` holds explicitly created directories (directories also exist
/// implicitly as file-path prefixes, like object stores).
struct HelloFs {
    files: HashMap<String, Vec<u8>>,
    dirs: HashSet<String>,
    /// Open handles → path.
    handles: HashMap<u64, String>,
    next_handle: u64,
    /// How many times `counter` has been opened (its content is synthesized
    /// from this on each open).
    opens: u64,
}

impl HelloFs {
    fn new() -> Self {
        let mut files = HashMap::new();
        files.insert(
            "hello.txt".to_string(),
            b"Hello from another node's filesystem!\n".to_vec(),
        );
        files.insert(
            "README.md".to_string(),
            b"# hellofs\n\nEverything under this mount is served live by the \
              hellofs node's code.\nWrites stick around for as long as the node \
              runs. `counter` regenerates on every open.\n"
                .to_vec(),
        );
        HelloFs {
            files,
            dirs: HashSet::new(),
            handles: HashMap::new(),
            next_handle: 1,
            opens: 0,
        }
    }

    fn is_dir(&self, path: &str) -> bool {
        path.is_empty()
            || self.dirs.contains(path)
            || self
                .files
                .keys()
                .any(|f| f.strip_prefix(path).is_some_and(|r| r.starts_with('/')))
            || self
                .dirs
                .iter()
                .any(|d| d.strip_prefix(path).is_some_and(|r| r.starts_with('/')))
    }

    fn handle_op(&mut self, op: Op) -> Result<ReplyData, Error> {
        match op {
            Op::Getattr(path) => {
                if self.is_dir(&path) {
                    Ok(ReplyData::Attr(Stat {
                        kind: EntryKind::Dir,
                        size: 0,
                    }))
                } else if let Some(b) = self.files.get(&path) {
                    Ok(ReplyData::Attr(Stat {
                        kind: EntryKind::File,
                        size: b.len() as u64,
                    }))
                } else {
                    Err(Error::NoEntry)
                }
            }
            Op::Readdir(path) => {
                if !self.is_dir(&path) {
                    return Err(Error::NotDir);
                }
                let prefix = if path.is_empty() {
                    String::new()
                } else {
                    format!("{path}/")
                };
                let mut seen = HashSet::new();
                let mut entries = Vec::new();
                for p in self.files.keys().chain(self.dirs.iter()) {
                    let Some(rest) = p.strip_prefix(&prefix) else {
                        continue;
                    };
                    if rest.is_empty() {
                        continue;
                    }
                    let first = rest.split('/').next().unwrap().to_string();
                    if seen.insert(first.clone()) {
                        let full = format!("{prefix}{first}");
                        entries.push(Dirent {
                            kind: if self.files.contains_key(&full) {
                                EntryKind::File
                            } else {
                                EntryKind::Dir
                            },
                            name: first,
                        });
                    }
                }
                Ok(ReplyData::Entries(entries))
            }
            Op::Open(a) => {
                // The synthetic file: regenerated from live state on each open.
                if a.path == "counter" {
                    self.opens += 1;
                    let body = format!("opened {} times\n", self.opens).into_bytes();
                    self.files.insert(a.path.clone(), body);
                }
                let exists = self.files.contains_key(&a.path) || self.is_dir(&a.path);
                if exists && a.exclusive {
                    return Err(Error::Exist);
                }
                if !exists && !a.create {
                    return Err(Error::NoEntry);
                }
                if !exists || (a.truncate && self.files.contains_key(&a.path)) {
                    self.files.insert(a.path.clone(), Vec::new());
                }
                let (kind, size) = match self.files.get(&a.path) {
                    Some(b) => (EntryKind::File, b.len() as u64),
                    None => (EntryKind::Dir, 0),
                };
                let handle = self.next_handle;
                self.next_handle += 1;
                self.handles.insert(handle, a.path);
                Ok(ReplyData::Opened(Opened { handle, kind, size }))
            }
            Op::Read(a) => {
                let Some(b) = self.handles.get(&a.handle).and_then(|p| self.files.get(p))
                else {
                    return Err(Error::NoEntry);
                };
                let start = (a.offset as usize).min(b.len());
                let end = start.saturating_add(a.len as usize).min(b.len());
                Ok(ReplyData::Data(ReadResult {
                    bytes: b[start..end].to_vec(),
                    eof: end >= b.len(),
                }))
            }
            Op::Write(a) => {
                let Some(path) = self.handles.get(&a.handle).cloned() else {
                    return Err(Error::NoEntry);
                };
                let b = self.files.entry(path).or_default();
                let start = a.offset as usize;
                let end = start.saturating_add(a.data.len());
                if end > 64 * 1024 * 1024 {
                    return Err(Error::TooLarge);
                }
                if b.len() < end {
                    b.resize(end, 0);
                }
                b[start..end].copy_from_slice(&a.data);
                Ok(ReplyData::Written(a.data.len() as u64))
            }
            Op::Release(handle) => {
                self.handles.remove(&handle);
                Ok(ReplyData::Done)
            }
            Op::SetSize(a) => {
                let Some(path) = self.handles.get(&a.handle).cloned() else {
                    return Err(Error::NoEntry);
                };
                if a.size > 64 * 1024 * 1024 {
                    return Err(Error::TooLarge);
                }
                self.files.entry(path).or_default().resize(a.size as usize, 0);
                Ok(ReplyData::Done)
            }
            Op::Mkdir(path) => {
                if self.files.contains_key(&path) || self.is_dir(&path) {
                    return Err(Error::Exist);
                }
                self.dirs.insert(path);
                Ok(ReplyData::Done)
            }
            Op::Unlink(path) => match self.files.remove(&path) {
                Some(_) => Ok(ReplyData::Done),
                None => Err(if self.is_dir(&path) {
                    Error::IsDir
                } else {
                    Error::NoEntry
                }),
            },
            Op::Rmdir(path) => {
                if self.files.contains_key(&path) {
                    return Err(Error::NotDir);
                }
                if !self.is_dir(&path) {
                    return Err(Error::NoEntry);
                }
                // Only an explicitly created, empty directory can go.
                let has_children = self
                    .files
                    .keys()
                    .chain(self.dirs.iter())
                    .any(|p| p.strip_prefix(&path).is_some_and(|r| r.starts_with('/')));
                if has_children {
                    return Err(Error::NotPermitted);
                }
                self.dirs.remove(&path);
                Ok(ReplyData::Done)
            }
            Op::Rename(a) => match self.files.remove(&a.src) {
                Some(b) => {
                    self.files.insert(a.dest, b);
                    Ok(ReplyData::Done)
                }
                None => Err(Error::NoEntry),
            },
        }
    }
}

impl Guest for Component {
    fn run() {
        println!("[hellofs] serving; wire me into a node and ls the mount");
        let mut fs = HelloFs::new();
        // The serve loop: block for a request, answer it. `none` means the
        // host is shutting this node down.
        while let Some(req) = provider::next_request() {
            let outcome = fs.handle_op(req.op);
            provider::reply(req.id, outcome.as_ref());
        }
        println!("[hellofs] shutting down");
    }
}

bindings::export!(Component with_types_in bindings);
