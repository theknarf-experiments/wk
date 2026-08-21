//! The wk:fs op handler over an in-memory tree — the shape hellofs serves,
//! factored so the sync client can swap the storage underneath: reads come
//! from the synced snapshot, writes become Automerge changes.

use crate::bindings::wk::fs::provider::{
    Dirent, EntryKind, Error, Op, Opened, ReadResult, ReplyData, Stat,
};
use std::collections::{HashMap, HashSet};

/// Full paths ("a/b.txt", no leading slash) to bytes, plus explicitly created
/// directories (they also exist implicitly as path prefixes).
pub struct MemTree {
    pub files: HashMap<String, Vec<u8>>,
    pub dirs: HashSet<String>,
    handles: HashMap<u64, String>,
    next_handle: u64,
    /// Paths written or removed locally since the last sync flush — the set
    /// the sync client turns into Automerge changes.
    pub dirty: HashSet<String>,
    /// Renames since the last flush, in order (order matters: a→b then b→c
    /// must replay as two steps). Kept apart from `dirty` so a rename stays a
    /// rename — a directory-doc re-key that the file doc's history survives —
    /// instead of decaying into delete + create.
    pub renames: Vec<(String, String)>,
}

const MAX_FILE: usize = 64 * 1024 * 1024;

impl MemTree {
    pub fn empty() -> Self {
        MemTree {
            files: HashMap::new(),
            dirs: HashSet::new(),
            handles: HashMap::new(),
            next_handle: 1,
            dirty: HashSet::new(),
            renames: Vec::new(),
        }
    }

    /// Move one file's local state from `src` to `dest`: bytes, dirty
    /// membership, open handles (a descriptor held across a rename keeps
    /// addressing the same file), and the rename log the flush replays.
    fn move_file(&mut self, src: String, dest: String) {
        if let Some(bytes) = self.files.remove(&src) {
            self.files.insert(dest.clone(), bytes);
        }
        if self.dirty.remove(&src) {
            self.dirty.insert(dest.clone());
        }
        for path in self.handles.values_mut() {
            if *path == src {
                *path = dest.clone();
            }
        }
        self.renames.push((src, dest));
    }

    fn is_dir(&self, path: &str) -> bool {
        path.is_empty()
            || self.dirs.contains(path)
            || self
                .files
                .keys()
                .chain(self.dirs.iter())
                .any(|p| p.strip_prefix(path).is_some_and(|r| r.starts_with('/')))
    }

    pub fn handle(&mut self, op: Op) -> Result<ReplyData, Error> {
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
                let exists = self.files.contains_key(&a.path) || self.is_dir(&a.path);
                if exists && a.exclusive {
                    return Err(Error::Exist);
                }
                if !exists && !a.create {
                    return Err(Error::NoEntry);
                }
                if !exists || (a.truncate && self.files.contains_key(&a.path)) {
                    self.files.insert(a.path.clone(), Vec::new());
                    self.dirty.insert(a.path.clone());
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
                let Some(b) = self.handles.get(&a.handle).and_then(|p| self.files.get(p)) else {
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
                let b = self.files.entry(path.clone()).or_default();
                let start = a.offset as usize;
                let end = start.saturating_add(a.data.len());
                if end > MAX_FILE {
                    return Err(Error::TooLarge);
                }
                if b.len() < end {
                    b.resize(end, 0);
                }
                b[start..end].copy_from_slice(&a.data);
                self.dirty.insert(path);
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
                if a.size as usize > MAX_FILE {
                    return Err(Error::TooLarge);
                }
                self.files
                    .entry(path.clone())
                    .or_default()
                    .resize(a.size as usize, 0);
                self.dirty.insert(path);
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
                Some(_) => {
                    self.dirty.insert(path);
                    Ok(ReplyData::Done)
                }
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
            Op::Rename(a) => {
                if self.files.contains_key(&a.src) {
                    self.move_file(a.src, a.dest);
                    Ok(ReplyData::Done)
                } else if self.is_dir(&a.src) {
                    // A directory rename is a prefix re-key of every file
                    // under it (file docs carry only their own basename, so
                    // none of them changes — just the directory-doc keys).
                    let prefix = format!("{}/", a.src);
                    let moved: Vec<String> = self
                        .files
                        .keys()
                        .filter(|p| p.starts_with(&prefix))
                        .cloned()
                        .collect();
                    for old in moved {
                        let new = format!("{}/{}", a.dest, &old[prefix.len()..]);
                        self.move_file(old, new);
                    }
                    let dirs: Vec<String> = self
                        .dirs
                        .iter()
                        .filter(|d| **d == a.src || d.starts_with(&prefix))
                        .cloned()
                        .collect();
                    for old in dirs {
                        self.dirs.remove(&old);
                        let new = if old == a.src {
                            a.dest.clone()
                        } else {
                            format!("{}/{}", a.dest, &old[prefix.len()..])
                        };
                        self.dirs.insert(new);
                    }
                    Ok(ReplyData::Done)
                } else {
                    Err(Error::NoEntry)
                }
            }
        }
    }
}
