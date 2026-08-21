//! The pushwork `vfs` doc shape, decoded: a *directory* doc whose non-`@`
//! keys map relative paths to per-file doc urls, and file docs whose
//! `content` is text (a Text object or plain string) or bytes. Exactly what
//! flow-page writes and `pushwork clone` reads — automergefs serves the same
//! repo those tools sync.

use std::collections::BTreeSet;

use automerge::transaction::Transactable;
use automerge::{Automerge, ObjType, ReadDoc, ScalarValue, Value};
use sedimentree_core::{
    blob::{Blob, BlobMeta},
    fragment::Fragment,
    id::SedimentreeId,
    loose_commit::{id::CommitId, LooseCommit},
    sedimentree::Sedimentree,
};

/// `automerge:<bs58check>` (or raw 64-hex) → the zero-padded 32-byte
/// sedimentree id subduction files the doc under.
pub fn parse_doc_id(input: &str) -> Result<SedimentreeId, String> {
    let stripped = input.strip_prefix("automerge:").unwrap_or(input);
    let stripped = stripped.split('#').next().unwrap_or(stripped);
    if stripped.len() == 64 && stripped.chars().all(|c| c.is_ascii_hexdigit()) {
        let mut bytes = [0u8; 32];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = u8::from_str_radix(&stripped[i * 2..i * 2 + 2], 16)
                .map_err(|e| format!("bad hex doc id: {e}"))?;
        }
        return Ok(SedimentreeId::new(bytes));
    }
    let decoded = bs58::decode(stripped)
        .with_check(None)
        .into_vec()
        .map_err(|e| format!("bad doc id {input:?}: {e}"))?;
    if decoded.len() > 32 {
        return Err(format!("doc id too long: {} bytes", decoded.len()));
    }
    let mut padded = [0u8; 32];
    padded[..decoded.len()].copy_from_slice(&decoded);
    Ok(SedimentreeId::new(padded))
}

/// Load a doc from every blob subduction holds for it. Order-insensitive:
/// automerge buffers changes whose dependencies haven't arrived yet.
pub fn load_doc(blobs: impl IntoIterator<Item = Vec<u8>>) -> Result<Automerge, String> {
    let mut doc = Automerge::new();
    for b in blobs {
        doc.load_incremental(&b)
            .map_err(|e| format!("load chunk: {e}"))?;
    }
    Ok(doc)
}

/// A string-ish value at `key`, whichever way a JS writer stored it: a Text
/// object (the automerge-JS default for plain strings), an immutable string
/// scalar, or a str scalar.
fn string_at(doc: &Automerge, key: &str) -> Option<String> {
    match doc.get(automerge::ROOT, key) {
        Ok(Some((Value::Object(ObjType::Text), id))) => doc.text(&id).ok(),
        Ok(Some((Value::Scalar(s), _))) => match s.as_ref() {
            ScalarValue::Str(t) => Some(t.to_string()),
            _ => None,
        },
        _ => None,
    }
}

/// The directory doc's leaves: relative path → the file doc's url. `@`-keys
/// are metadata (`@patchwork`, the canvas ref), not files.
pub fn dir_leaves(dir: &Automerge) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for key in dir.keys(automerge::ROOT) {
        if key.starts_with('@') {
            continue;
        }
        if let Some(url) = string_at(dir, &key) {
            if url.starts_with("automerge:") {
                out.push((key, url));
            }
        }
    }
    out
}

/// A file doc's `content`: bytes for byte content, utf8 for text (whether a
/// live Text object or an immutable string scalar).
pub fn file_content(doc: &Automerge) -> Vec<u8> {
    match doc.get(automerge::ROOT, "content") {
        Ok(Some((Value::Object(ObjType::Text), id))) => {
            doc.text(&id).map(String::into_bytes).unwrap_or_default()
        }
        Ok(Some((Value::Scalar(s), _))) => match s.as_ref() {
            ScalarValue::Bytes(b) => b.clone(),
            ScalarValue::Str(t) => t.as_bytes().to_vec(),
            other => other.to_string().into_bytes(),
        },
        _ => Vec::new(),
    }
}

// ── The write path: local edits become automerge changes ──

/// Replace a file doc's `content`: utf8 becomes (or updates) a Text object —
/// `update_text` diffs, so concurrent editors merge rather than clobber —
/// anything else is a bytes scalar.
pub fn set_content(doc: &mut Automerge, bytes: &[u8]) -> Result<(), String> {
    let text = std::str::from_utf8(bytes).ok().map(str::to_string);
    doc.transact::<_, _, automerge::AutomergeError>(|tx| {
        match (&text, tx.get(automerge::ROOT, "content")?) {
            (Some(s), Some((Value::Object(ObjType::Text), id))) => tx.update_text(&id, s)?,
            (Some(s), _) => {
                let t = tx.put_object(automerge::ROOT, "content", ObjType::Text)?;
                tx.splice_text(&t, 0, 0, s)?;
            }
            (None, _) => tx.put(automerge::ROOT, "content", bytes.to_vec())?,
        }
        Ok(())
    })
    .map(|_| ())
    .map_err(|e| format!("set_content: {}", e.error))
}

/// A brand-new pushwork file doc for `rel` (mirrors flow-page's
/// `makeFileEntry`: `@patchwork` meta + name/extension/mimeType + content).
pub fn make_file_doc(rel: &str, content: &[u8]) -> Result<Automerge, String> {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    let extension = match name.rfind('.') {
        Some(dot) if dot > 0 => &name[dot + 1..],
        _ => "",
    };
    let mime = match extension {
        "html" => "text/html",
        "css" => "text/css",
        "js" | "jsx" | "ts" | "tsx" => "text/javascript",
        "json" => "application/json",
        "md" => "text/markdown",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        _ => "text/plain",
    };
    let mut doc = Automerge::new();
    doc.transact::<_, _, automerge::AutomergeError>(|tx| {
        let meta = tx.put_object(automerge::ROOT, "@patchwork", ObjType::Map)?;
        tx.put(&meta, "type", "file")?;
        tx.put(automerge::ROOT, "name", name)?;
        tx.put(automerge::ROOT, "extension", extension)?;
        tx.put(automerge::ROOT, "mimeType", mime)?;
        Ok(())
    })
    .map_err(|e| format!("make_file_doc: {}", e.error))?;
    set_content(&mut doc, content)?;
    Ok(doc)
}

/// Point `path` at `url` in the directory doc (or, with `None`, drop it).
pub fn set_dir_entry(dir: &mut Automerge, path: &str, url: Option<&str>) -> Result<(), String> {
    dir.transact::<_, _, automerge::AutomergeError>(|tx| {
        match url {
            Some(u) => tx.put(automerge::ROOT, path, u)?,
            None => tx.delete(automerge::ROOT, path)?,
        }
        Ok(())
    })
    .map(|_| ())
    .map_err(|e| format!("set_dir_entry: {}", e.error))
}

/// The change the last transaction produced, as the loose commit subduction
/// pushes: (head, parents, blob).
pub fn last_change(doc: &mut Automerge) -> Option<(CommitId, BTreeSet<CommitId>, Blob)> {
    let change = doc.get_last_local_change()?;
    let head = CommitId::new(change.hash().0);
    let parents: BTreeSet<CommitId> = change.deps().iter().map(|d| CommitId::new(d.0)).collect();
    let blob = Blob::new(change.raw_bytes().to_vec());
    Some((head, parents, blob))
}

/// A fresh automerge url (16 random bytes, bs58check) and its sedimentree id.
pub fn new_doc_url() -> (String, SedimentreeId) {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).expect("getrandom");
    let url = format!(
        "automerge:{}",
        bs58::encode(&bytes).with_check().into_string()
    );
    let mut padded = [0u8; 32];
    padded[..16].copy_from_slice(&bytes);
    (url, SedimentreeId::new(padded))
}

/// Decompose a whole doc into the sedimentree + blobs `add_sedimentree`
/// uploads — the same mapping the ingest CLI uses: level-1+ fragments become
/// [`Fragment`]s, level-0 become [`LooseCommit`]s.
pub fn ingest_doc(doc: &Automerge, id: SedimentreeId) -> (Sedimentree, Vec<Blob>) {
    let cached = doc.fragments(1..);
    let loose = doc.fragments(0..=0);
    let cached_bytes = doc.bundle_fragments(cached.iter().cloned());
    let loose_bytes = doc.bundle_fragments(loose.iter().cloned());

    let mut fragments = Vec::with_capacity(cached.len());
    let mut blobs = Vec::with_capacity(cached.len() + loose.len());
    for (f, raw) in cached.iter().zip(cached_bytes) {
        let blob = Blob::new(raw);
        let boundary: BTreeSet<CommitId> = f.boundary.iter().map(|h| CommitId::new(h.0)).collect();
        let checkpoints: Vec<CommitId> = f.checkpoints.iter().map(|h| CommitId::new(h.0)).collect();
        let meta = BlobMeta::new(&blob);
        fragments.push(Fragment::new(
            id,
            CommitId::new(f.head.0),
            boundary,
            &checkpoints,
            meta,
        ));
        blobs.push(blob);
    }
    let mut loose_commits = Vec::with_capacity(loose.len());
    for (f, raw) in loose.iter().zip(loose_bytes) {
        let head = CommitId::new(f.head.0);
        let parents: BTreeSet<CommitId> = f.boundary.iter().map(|p| CommitId::new(p.0)).collect();
        let blob = Blob::new(raw);
        let meta = BlobMeta::new(&blob);
        loose_commits.push(LooseCommit::new(id, head, parents, meta));
        blobs.push(blob);
    }
    (Sedimentree::new(fragments, loose_commits), blobs)
}
