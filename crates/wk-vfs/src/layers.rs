//! Immutable filesystem layers: the content of an OCI image layer (or a local
//! layer source) as a reusable, `Arc`-shared value that can be applied into any
//! node's [`crate::Fs`].
//!
//! A [`Layer`] is an ordered list of entries — directories, files, and OCI
//! whiteouts (`.wh.<name>` deletes, `.wh..wh..opq` clears a directory). File
//! bytes are `Arc`s: applying the same layer to five nodes stores the bytes
//! once, and the vfs copy-on-writes per node on first write (see
//! the crate's `RoFile`). A process-wide cache keyed by source digest makes
//! repeat applications (and repeat loads) cheap.

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use crate::SharedFs;

/// One entry of a layer, with a `/`-free normalized path ("a/b/c").
#[derive(Debug)]
pub enum LayerEntry {
    /// Ensure this directory (and its parents) exist.
    Dir(String),
    /// Place a file at this path (replacing any earlier entry).
    File(String, Arc<Vec<u8>>),
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
fn classify(path: &str, file: Option<Arc<Vec<u8>>>) -> LayerEntry {
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

/// Load a layer from a tarball (gzip-compressed or plain, auto-detected).
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
                entries.push(classify(&path, Some(Arc::new(data))));
            }
            // Links and specials aren't representable in the vfs; skip them.
            // (Whiteouts arrive as empty regular files, handled above.)
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
                entries.push(LayerEntry::File(path, Arc::new(data)));
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
