//! Immutable filesystem layers: the content of an OCI image layer (or a local
//! layer source) as a reusable, `Arc`-shared value that can be applied into any
//! node's [`crate::Fs`].
//!
//! A [`Layer`] is an ordered list of entries — directories, files, and OCI
//! whiteouts (`.wh.<name>` deletes, `.wh..wh..opq` clears a directory). File
//! bytes are `Arc`-shared [`LayerBytes`]: applying the same layer to five
//! nodes stores the bytes once, and the vfs copy-on-writes per node on first
//! write (see the crate's `RoFile`). A layer indexed from an on-disk tar
//! ([`from_tar_file`]) is *lazy* — each file's bytes stay on disk until the
//! first read materializes them — so mounting a large image costs directory
//! entries, not its full size in RAM. A process-wide cache keyed by source
//! digest makes repeat applications (and repeat loads) cheap, and means N
//! nodes share the one (possibly not-yet-materialized) copy per file.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use crate::SharedFs;

/// Where a lazy file's bytes live: a member slice of an *uncompressed* tar in
/// the on-disk layer store (the store writes layers decompressed, which is
/// what makes per-file random access possible — seeking into gzip without an
/// index would cost O(layer) per file).
#[derive(Debug)]
struct TarSlice {
    /// The layer tar on disk, shared by every file of the layer.
    tar: Arc<PathBuf>,
    /// Byte offset of this member's data within the tar.
    offset: u64,
}

impl TarSlice {
    fn read(&self, len: usize) -> std::io::Result<Vec<u8>> {
        use std::io::{Seek, SeekFrom};
        let mut f = std::fs::File::open(self.tar.as_path())?;
        f.seek(SeekFrom::Start(self.offset))?;
        let mut buf = vec![0u8; len];
        f.read_exact(&mut buf)?;
        Ok(buf)
    }
}

/// A layer file's bytes: either already in RAM (an eager layer, e.g. one
/// parsed from an in-memory tar) or a recipe to read them from the on-disk
/// layer store, materialized once on first access and then `Arc`-shared
/// exactly like an eager file. The length comes from the tar header, so
/// stat/list never touch the disk or allocate the content.
#[derive(Debug)]
pub struct LayerBytes {
    /// Byte length, from the tar header (or the eager buffer).
    len: usize,
    /// The materialized bytes: filled at construction for an eager file, on
    /// the first [`bytes`](Self::bytes) call for a lazy one, then shared.
    cell: OnceLock<Arc<Vec<u8>>>,
    /// The on-disk recipe; `None` for an eager file.
    src: Option<TarSlice>,
}

impl LayerBytes {
    /// Bytes already in RAM (in-memory tars, local dir layers, tests).
    pub fn eager(bytes: Arc<Vec<u8>>) -> Self {
        let cell = OnceLock::new();
        let len = bytes.len();
        let _ = cell.set(bytes);
        LayerBytes {
            len,
            cell,
            src: None,
        }
    }

    /// Bytes to be read from `tar` at `offset` on first access.
    fn lazy(tar: Arc<PathBuf>, offset: u64, len: usize) -> Self {
        LayerBytes {
            len,
            cell: OnceLock::new(),
            src: Some(TarSlice { tar, offset }),
        }
    }

    /// The file's byte length — known from the tar header, so asking never
    /// materializes the content.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether the bytes are in RAM (a probe for tests and telemetry; every
    /// real consumer just calls [`bytes`](Self::bytes)).
    pub fn is_materialized(&self) -> bool {
        self.cell.get().is_some()
    }

    /// The bytes, read from the on-disk tar on first call and shared
    /// thereafter. A missing or torn blob degrades to an empty read (with a
    /// logged warning), never a panic: the affected file reads as EOF, the
    /// rest of the layer is untouched.
    pub fn bytes(&self) -> Arc<Vec<u8>> {
        self.cell
            .get_or_init(|| {
                // No recipe means eager, and eager pre-fills the cell — this
                // arm only guards against an impossible state.
                let Some(src) = &self.src else {
                    return Arc::new(Vec::new());
                };
                match src.read(self.len) {
                    Ok(data) => Arc::new(data),
                    Err(e) => {
                        eprintln!("wk-vfs: layer file unreadable ({}): {e}", src.tar.display());
                        Arc::new(Vec::new())
                    }
                }
            })
            .clone()
    }
}

/// One entry of a layer, with a `/`-free normalized path ("a/b/c").
#[derive(Debug)]
pub enum LayerEntry {
    /// Ensure this directory (and its parents) exist.
    Dir(String),
    /// Place a file at this path (replacing any earlier entry).
    File(String, Arc<LayerBytes>),
    /// A symbolic link at this path, pointing at the stored target. Real
    /// images lean on these heavily — busybox and coreutils ship one binary
    /// plus a farm of links, and `/lib64 -> /lib` style aliases are
    /// everywhere — so a layer that dropped them produced an image missing
    /// most of its commands.
    Symlink(String, String),
    /// OCI whiteout: remove the entry at this path from lower layers.
    Whiteout(String),
    /// OCI opaque marker: clear the directory at this path (lower layers'
    /// contents disappear; this layer's own entries still apply).
    Opaque(String),
}

/// An immutable, shareable filesystem layer.
#[derive(Debug)]
pub struct Layer {
    pub entries: Vec<LayerEntry>,
}

/// Normalize a tar/dir member path: strip leading `/` and `./`, drop empties.
fn normalize(path: &str) -> String {
    path.split('/')
        .filter(|c| !c.is_empty() && *c != ".")
        .collect::<Vec<_>>()
        .join("/")
}

/// Classify a normalized path into an add/whiteout/opaque entry per the OCI
/// layer spec: a basename of `.wh..wh..opq` marks its directory opaque; a
/// `.wh.<name>` basename whites out `<name>` in the same directory.
fn classify(path: &str, file: Option<Arc<LayerBytes>>) -> LayerEntry {
    let (dir, base) = match path.rsplit_once('/') {
        Some((d, b)) => (d, b),
        None => ("", path),
    };
    if base == ".wh..wh..opq" {
        return LayerEntry::Opaque(dir.to_string());
    }
    if let Some(target) = base.strip_prefix(".wh.") {
        let full = if dir.is_empty() {
            target.to_string()
        } else {
            format!("{dir}/{target}")
        };
        return LayerEntry::Whiteout(full);
    }
    match file {
        Some(bytes) => LayerEntry::File(path.to_string(), bytes),
        None => LayerEntry::Dir(path.to_string()),
    }
}

/// Load a layer *eagerly* from a tarball in memory (gzip-compressed or plain,
/// auto-detected): every file's bytes land in RAM up front. The lazy path for
/// stored layers is [`from_tar_file`].
pub fn from_tar_bytes(bytes: &[u8]) -> Result<Layer, String> {
    let plain: Vec<u8> = if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(bytes)
            .read_to_end(&mut out)
            .map_err(|e| format!("gunzip layer: {e}"))?;
        out
    } else {
        bytes.to_vec()
    };
    let mut archive = tar::Archive::new(plain.as_slice());
    let mut entries = Vec::new();
    for entry in archive.entries().map_err(|e| format!("read tar: {e}"))? {
        let mut entry = entry.map_err(|e| format!("read tar entry: {e}"))?;
        let path = normalize(
            &entry
                .path()
                .map_err(|e| format!("entry path: {e}"))?
                .to_string_lossy(),
        );
        if path.is_empty() {
            continue;
        }
        match entry.header().entry_type() {
            tar::EntryType::Directory => entries.push(classify(&path, None)),
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                let mut data = Vec::with_capacity(entry.size() as usize);
                entry
                    .read_to_end(&mut data)
                    .map_err(|e| format!("read {path}: {e}"))?;
                entries.push(classify(
                    &path,
                    Some(Arc::new(LayerBytes::eager(Arc::new(data)))),
                ));
            }
            // Symlinks (and hard links, which we materialise as symlinks —
            // the vfs has no second name for one inode, and the target path
            // is what matters for resolution).
            tar::EntryType::Symlink | tar::EntryType::Link => {
                if let Ok(Some(target)) = entry.link_name() {
                    entries.push(LayerEntry::Symlink(
                        path,
                        target.to_string_lossy().into_owned(),
                    ));
                }
            }
            // Devices, fifos and sockets have no meaning in a wasm sandbox.
            _ => {}
        }
    }
    Ok(Layer { entries })
}

/// Index a layer *lazily* from an uncompressed tarball on disk: each regular
/// member becomes a [`LayerBytes`] recipe recording its data offset and
/// header length, and no file content is read — applying the layer creates
/// directory entries whose bytes load from `path` on first access. This is
/// what makes a big pulled image cost only what a node actually reads.
///
/// The index pass itself seeks header-to-header, so it is cheap even for a
/// multi-GB tar. A gzipped file (which the layer store never writes, but a
/// pre-existing store might hold) falls back to the eager [`from_tar_bytes`]:
/// correct, just paying the old full-memory price.
pub fn from_tar_file(path: &Path) -> Result<Layer, String> {
    use std::io::{Seek, SeekFrom};
    let mut file =
        std::fs::File::open(path).map_err(|e| format!("open layer {}: {e}", path.display()))?;
    let mut magic = [0u8; 2];
    let n = file
        .read(&mut magic)
        .map_err(|e| format!("read layer {}: {e}", path.display()))?;
    if n == 2 && magic == [0x1f, 0x8b] {
        let bytes =
            std::fs::read(path).map_err(|e| format!("read layer {}: {e}", path.display()))?;
        return from_tar_bytes(&bytes);
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|e| format!("seek layer {}: {e}", path.display()))?;

    let tar_path = Arc::new(path.to_path_buf());
    let mut archive = tar::Archive::new(file);
    let mut entries = Vec::new();
    for entry in archive
        .entries_with_seek()
        .map_err(|e| format!("read tar: {e}"))?
    {
        let entry = entry.map_err(|e| format!("read tar entry: {e}"))?;
        let path = normalize(
            &entry
                .path()
                .map_err(|e| format!("entry path: {e}"))?
                .to_string_lossy(),
        );
        if path.is_empty() {
            continue;
        }
        match entry.header().entry_type() {
            tar::EntryType::Directory => entries.push(classify(&path, None)),
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                // The recipe, not the bytes: offset of the member's data in
                // the tar plus the header's size. Whiteout names still go
                // through `classify`, which ignores the (never-read) bytes.
                let lazy = LayerBytes::lazy(
                    tar_path.clone(),
                    entry.raw_file_position(),
                    entry.size() as usize,
                );
                entries.push(classify(&path, Some(Arc::new(lazy))));
            }
            // Symlinks/hard links: same treatment as the eager loader.
            tar::EntryType::Symlink | tar::EntryType::Link => {
                if let Ok(Some(target)) = entry.link_name() {
                    entries.push(LayerEntry::Symlink(
                        path,
                        target.to_string_lossy().into_owned(),
                    ));
                }
            }
            // Devices, fifos and sockets have no meaning in a wasm sandbox.
            _ => {}
        }
    }
    Ok(Layer { entries })
}

/// Load a layer from a directory tree on the host disk (each file's bytes are
/// read once and shared). Entries are sorted, so the layer is deterministic.
pub fn from_dir(dir: &Path) -> Result<Layer, String> {
    fn walk(dir: &Path, rel: &str, entries: &mut Vec<LayerEntry>) -> Result<(), String> {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .map_err(|e| format!("read {}: {e}", dir.display()))?
            .filter_map(|e| e.ok())
            .collect();
        names.sort_by_key(|e| e.file_name());
        for e in names {
            let name = e.file_name().to_string_lossy().to_string();
            let path = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            // Don't follow symlinks out of the layer root.
            let meta = e
                .path()
                .symlink_metadata()
                .map_err(|e2| format!("stat {path}: {e2}"))?;
            if meta.is_dir() {
                entries.push(LayerEntry::Dir(path.clone()));
                walk(&e.path(), &path, entries)?;
            } else if meta.is_file() {
                let data = std::fs::read(e.path()).map_err(|e2| format!("read {path}: {e2}"))?;
                entries.push(LayerEntry::File(
                    path,
                    Arc::new(LayerBytes::eager(Arc::new(data))),
                ));
            }
        }
        Ok(())
    }
    let mut entries = Vec::new();
    walk(dir, "", &mut entries)?;
    Ok(Layer { entries })
}

/// Apply `layer` into `fs` under `prefix` (`""` or `"/"` = the root). Whiteouts
/// and opaque markers apply first, then directories, then files, so a layer
/// that clears a directory and refills it lands deterministically.
pub fn apply(fs: &SharedFs, layer: &Layer, prefix: &str) {
    let prefix = normalize(prefix);
    let join = |p: &str| {
        if prefix.is_empty() {
            p.to_string()
        } else if p.is_empty() {
            prefix.clone()
        } else {
            format!("{prefix}/{p}")
        }
    };
    let mut g = fs.lock().unwrap();
    g.ensure_dir_path(&prefix);
    for e in &layer.entries {
        match e {
            LayerEntry::Whiteout(p) => g.remove_path(&join(p)),
            LayerEntry::Opaque(p) => g.clear_dir_at(&join(p)),
            _ => {}
        }
    }
    for e in &layer.entries {
        if let LayerEntry::Dir(p) = e {
            g.ensure_dir_path(&join(p));
        }
    }
    for e in &layer.entries {
        if let LayerEntry::File(p, bytes) = e {
            g.put_ro_file_at(&join(p), bytes.clone());
        }
    }
    // Links last: their targets may be files this same layer just placed.
    for e in &layer.entries {
        if let LayerEntry::Symlink(p, target) = e {
            g.put_symlink_at(&join(p), target.clone());
        }
    }
}

/// Load-through cache: the layer for `key` (a digest or source path), loading
/// it with `load` on first use. Every caller shares one `Arc<Layer>`.
pub fn cached(
    key: &str,
    load: impl FnOnce() -> Result<Layer, String>,
) -> Result<Arc<Layer>, String> {
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<Layer>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(hit) = cache.lock().unwrap().get(key) {
        return Ok(hit.clone());
    }
    let layer = Arc::new(load()?);
    cache
        .lock()
        .unwrap()
        .entry(key.to_string())
        .or_insert_with(|| layer.clone());
    Ok(layer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::new_fs;

    /// Build an in-memory tar with the given (path, contents) files; a trailing
    /// `/` in the path makes a directory.
    fn tar_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (path, data) in entries {
            let mut h = tar::Header::new_gnu();
            if path.ends_with('/') {
                h.set_entry_type(tar::EntryType::Directory);
                h.set_size(0);
                h.set_path(path).unwrap();
            } else {
                h.set_entry_type(tar::EntryType::Regular);
                h.set_size(data.len() as u64);
                h.set_path(path).unwrap();
            }
            h.set_cksum();
            b.append(&h, *data).unwrap();
        }
        b.into_inner().unwrap()
    }

    #[test]
    fn tar_layer_applies_files_and_dirs() {
        let tar = tar_bytes(&[
            ("etc/", b""),
            ("etc/motd", b"welcome"),
            ("hello.txt", b"hi"),
        ]);
        let layer = from_tar_bytes(&tar).expect("parses");
        let fs = new_fs();
        apply(&fs, &layer, "");
        let g = fs.lock().unwrap();
        assert_eq!(g.read_file("/hello.txt", 64).as_deref(), Some(&b"hi"[..]));
        assert_eq!(
            g.read_file("/etc/motd", 64).as_deref(),
            Some(&b"welcome"[..])
        );
        assert!(g.list_dir("/etc").is_some());
    }

    #[test]
    fn gzipped_tar_is_detected() {
        use flate2::write::GzEncoder;
        use std::io::Write;
        let tar = tar_bytes(&[("f", b"zipped")]);
        let mut enc = GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(&tar).unwrap();
        let gz = enc.finish().unwrap();

        let layer = from_tar_bytes(&gz).expect("parses gzip");
        let fs = new_fs();
        apply(&fs, &layer, "");
        assert_eq!(
            fs.lock().unwrap().read_file("/f", 64).as_deref(),
            Some(&b"zipped"[..])
        );
    }

    #[test]
    fn dir_layer_mounts_under_a_prefix() {
        let root = std::env::temp_dir().join("wk-layer-dir-test");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("doc")).unwrap();
        std::fs::write(root.join("doc/help.txt"), b"*help*").unwrap();
        std::fs::write(root.join("vimrc"), b"set nocp").unwrap();

        let layer = from_dir(&root).expect("loads");
        let fs = new_fs();
        apply(&fs, &layer, "/usr/share/vim/runtime");
        let g = fs.lock().unwrap();
        assert_eq!(
            g.read_file("/usr/share/vim/runtime/doc/help.txt", 64)
                .as_deref(),
            Some(&b"*help*"[..])
        );
        assert_eq!(
            g.read_file("/usr/share/vim/runtime/vimrc", 64).as_deref(),
            Some(&b"set nocp"[..])
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn whiteout_removes_and_opaque_clears() {
        let base = from_tar_bytes(&tar_bytes(&[
            ("a/", b""),
            ("a/f1", b"one"),
            ("a/f2", b"two"),
            ("b/", b""),
            ("b/old", b"old"),
        ]))
        .unwrap();
        // Upper layer: delete a/f1, wipe b entirely and refill it.
        let upper = from_tar_bytes(&tar_bytes(&[
            ("a/.wh.f1", b""),
            ("b/.wh..wh..opq", b""),
            ("b/new", b"new"),
        ]))
        .unwrap();

        let fs = new_fs();
        apply(&fs, &base, "");
        apply(&fs, &upper, "");
        let g = fs.lock().unwrap();
        assert!(g.read_file("/a/f1", 8).is_none(), "whiteout removed f1");
        assert_eq!(g.read_file("/a/f2", 8).as_deref(), Some(&b"two"[..]));
        assert!(g.read_file("/b/old", 8).is_none(), "opaque cleared b");
        assert_eq!(g.read_file("/b/new", 8).as_deref(), Some(&b"new"[..]));
    }

    #[test]
    fn applying_to_two_nodes_shares_the_bytes() {
        let layer = from_tar_bytes(&tar_bytes(&[("big", b"shared-bytes")])).unwrap();
        let arc = match &layer.entries[..] {
            [LayerEntry::File(_, a)] => a.clone(),
            other => panic!("expected one file entry, got {other:?}"),
        };
        let before = Arc::strong_count(&arc);
        let a = new_fs();
        let b = new_fs();
        apply(&a, &layer, "");
        apply(&b, &layer, "");
        // Both filesystems hold the same allocation, not copies.
        assert_eq!(Arc::strong_count(&arc), before + 2);
        assert_eq!(
            a.lock().unwrap().read_file("/big", 64),
            b.lock().unwrap().read_file("/big", 64)
        );
    }

    /// Write a tar built by [`tar_bytes`] to a fresh temp file and return its
    /// path (the shape of a stored layer: an uncompressed tar on disk).
    fn tar_on_disk(name: &str, entries: &[(&str, &[u8])]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("wk-layer-lazy-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("layer.tar");
        std::fs::write(&path, tar_bytes(entries)).unwrap();
        path
    }

    /// Every `File` entry's shared bytes, by path.
    fn file_bytes(layer: &Layer) -> HashMap<String, Arc<LayerBytes>> {
        layer
            .entries
            .iter()
            .filter_map(|e| match e {
                LayerEntry::File(p, b) => Some((p.clone(), b.clone())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn indexing_and_applying_reads_no_file_contents() {
        let path = tar_on_disk(
            "index",
            &[
                ("etc/", b""),
                ("etc/motd", b"welcome"),
                ("hello.txt", b"hi"),
            ],
        );
        let layer = from_tar_file(&path).expect("indexes");
        let files = file_bytes(&layer);
        assert_eq!(files.len(), 2);
        assert!(
            files.values().all(|b| !b.is_materialized()),
            "indexing must not read contents"
        );

        // Applying creates the tree, and stat/list see the header sizes —
        // still without touching the tar's data.
        let fs = new_fs();
        apply(&fs, &layer, "");
        let g = fs.lock().unwrap();
        let root = g.list_dir("/").unwrap();
        let hello = root.iter().find(|e| e.name == "hello.txt").unwrap();
        assert_eq!(hello.size, 2);
        let etc = g.list_dir("/etc").unwrap();
        assert_eq!(etc[0].name, "motd");
        assert_eq!(etc[0].size, 7);
        assert!(
            files.values().all(|b| !b.is_materialized()),
            "apply/stat/list must not materialize"
        );
    }

    #[test]
    fn first_read_materializes_and_later_reads_share_the_arc() {
        let path = tar_on_disk("firstread", &[("a.txt", b"lazy-a"), ("b.txt", b"lazy-b")]);
        let layer = from_tar_file(&path).unwrap();
        let files = file_bytes(&layer);
        let fs = new_fs();
        apply(&fs, &layer, "");

        assert_eq!(
            fs.lock().unwrap().read_file("/a.txt", 64).as_deref(),
            Some(&b"lazy-a"[..]),
            "first read loads the on-disk bytes"
        );
        let a = &files["a.txt"];
        assert!(a.is_materialized());
        assert!(
            Arc::ptr_eq(&a.bytes(), &a.bytes()),
            "repeat access shares one allocation"
        );
        assert!(
            !files["b.txt"].is_materialized(),
            "an unread sibling stays on disk"
        );
    }

    #[test]
    fn lazy_layers_keep_whiteouts_opaque_and_symlinks() {
        let base = tar_on_disk(
            "wh-base",
            &[
                ("a/", b""),
                ("a/f1", b"one"),
                ("a/f2", b"two"),
                ("b/", b""),
                ("b/old", b"old"),
            ],
        );
        // Upper layer: delete a/f1, wipe b and refill it, and add a symlink.
        let upper_path = {
            let dir = std::env::temp_dir().join("wk-layer-lazy-wh-upper");
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let mut b = tar::Builder::new(Vec::new());
            for name in ["a/.wh.f1", "b/.wh..wh..opq"] {
                let mut h = tar::Header::new_gnu();
                h.set_entry_type(tar::EntryType::Regular);
                h.set_size(0);
                h.set_cksum();
                b.append_data(&mut h, name, &b""[..]).unwrap();
            }
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Regular);
            h.set_size(3);
            h.set_cksum();
            b.append_data(&mut h, "b/new", &b"new"[..]).unwrap();
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Symlink);
            h.set_size(0);
            b.append_link(&mut h, "a/link", "f2").unwrap();
            let path = dir.join("layer.tar");
            std::fs::write(&path, b.into_inner().unwrap()).unwrap();
            path
        };

        let fs = new_fs();
        apply(&fs, &from_tar_file(&base).unwrap(), "");
        apply(&fs, &from_tar_file(&upper_path).unwrap(), "");
        let g = fs.lock().unwrap();
        assert!(g.read_file("/a/f1", 8).is_none(), "whiteout removed f1");
        assert_eq!(g.read_file("/a/f2", 8).as_deref(), Some(&b"two"[..]));
        assert!(g.read_file("/b/old", 8).is_none(), "opaque cleared b");
        assert_eq!(g.read_file("/b/new", 8).as_deref(), Some(&b"new"[..]));
        assert_eq!(g.read_symlink("/a/link"), Some("f2".to_string()));
        // A read through the link reaches the (lazily loaded) target bytes.
        assert_eq!(g.read_file("/a/link", 8).as_deref(), Some(&b"two"[..]));
    }

    #[test]
    fn a_deleted_layer_tar_degrades_to_an_empty_read() {
        let path = tar_on_disk("deleted", &[("gone.txt", b"you won't see me")]);
        let layer = from_tar_file(&path).unwrap();
        let fs = new_fs();
        apply(&fs, &layer, "");
        std::fs::remove_file(&path).unwrap();

        let g = fs.lock().unwrap();
        // Stat still answers from the header; the read degrades to empty
        // (with a logged warning) instead of panicking the host.
        assert_eq!(g.list_dir("/").unwrap()[0].size, 16);
        assert_eq!(g.read_file("/gone.txt", 64).as_deref(), Some(&b""[..]));
    }

    #[test]
    fn two_nodes_share_one_lazy_materialization() {
        let path = tar_on_disk("shared", &[("big", b"shared-lazy-bytes")]);
        let layer = from_tar_file(&path).unwrap();
        let arc = file_bytes(&layer).remove("big").unwrap();
        let a = new_fs();
        let b = new_fs();
        apply(&a, &layer, "");
        apply(&b, &layer, "");
        assert!(!arc.is_materialized(), "mounting twice reads nothing");

        let ra = a.lock().unwrap().read_file("/big", 64).unwrap();
        let rb = b.lock().unwrap().read_file("/big", 64).unwrap();
        assert_eq!(ra, b"shared-lazy-bytes");
        assert_eq!(ra, rb);
        // Both nodes read through the same LayerBytes, so the content was
        // loaded once and both hold the one allocation.
        assert!(Arc::ptr_eq(&arc.bytes(), &arc.bytes()));
    }

    #[test]
    fn cached_returns_one_shared_layer() {
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let load = || {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            from_tar_bytes(&tar_bytes(&[("x", b"1")]))
        };
        let a = cached("wk-test-layer-cache-key", load).unwrap();
        let b = cached("wk-test-layer-cache-key", || unreachable!("cached")).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
