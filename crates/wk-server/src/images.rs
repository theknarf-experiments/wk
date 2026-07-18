//! wk's local OCI image store, and building images from Dockerfiles.
//!
//! An image is ordered filesystem layers plus a runtime config (entrypoint,
//! default args, env, workdir) — exactly OCI's model, with the entrypoint
//! required to be a wasm component inside the rootfs. The store lives under the
//! wk cache: content-addressed layer tarballs (`layers/sha256-<hex>.tar`)
//! shared by every image that references them, and per-image manifests
//! (`images/<id>.json`). [`build`] turns a Dockerfile + its context directory
//! into a stored image: each `COPY` becomes one layer, config instructions
//! accumulate, and the entrypoint component is extracted from the finished
//! rootfs so the plugin host can load it like any other wasm.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::Digest;

use wk_dockerfile::{self as dockerfile, Instr};

/// A stored image's manifest: what to mount and how to run it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ImageManifest {
    /// Content digests of the rootfs layers, in application order.
    pub layers: Vec<String>,
    /// The container entrypoint; `entrypoint[0]` is the wasm component's path
    /// inside the rootfs.
    pub entrypoint: Vec<String>,
    /// Default arguments appended after the entrypoint's own.
    #[serde(default)]
    pub cmd: Vec<String>,
    /// Environment for the guest.
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// Working directory, if set.
    #[serde(default)]
    pub workdir: Option<String>,
    /// Image labels (informational).
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

impl ImageManifest {
    /// The default guest argv after the program name: the entrypoint's own
    /// arguments, then CMD — Docker's argv composition, minus entrypoint[0]
    /// (the wasm itself).
    pub fn default_args(&self) -> Vec<String> {
        let mut out: Vec<String> = self.entrypoint.iter().skip(1).cloned().collect();
        out.extend(self.cmd.iter().cloned());
        out
    }

    /// What containerizes a node running this image: the rootfs layers to
    /// mount and the guest environment.
    pub fn container_setup(&self) -> ContainerSetup {
        ContainerSetup {
            layers: self.layers.clone(),
            env: self.env.clone(),
        }
    }
}

/// The per-node slice of an image: rootfs layers + guest env. Handed to the
/// plugin host at spawn so the node's filesystem is populated (Arc-shared,
/// copy-on-write) before the guest starts.
#[derive(Clone, Debug, Default)]
pub struct ContainerSetup {
    pub layers: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// Mount `setup`'s image layers into a node's filesystem, in order. Layer
/// content loads through the shared cache, so N nodes running one image store
/// its bytes once.
pub fn mount(fs: &crate::vfs::SharedFs, setup: &ContainerSetup) -> Result<(), String> {
    for digest in &setup.layers {
        let layer = crate::layers::cached(digest, || {
            let bytes = std::fs::read(layer_path(digest))
                .map_err(|e| format!("read layer {digest}: {e}"))?;
            crate::layers::from_tar_bytes(&bytes)
        })?;
        crate::layers::apply(fs, &layer, "");
    }
    Ok(())
}

/// Root of the image store (shares the wk cache dir with pulled artifacts).
fn store_dir() -> PathBuf {
    crate::oci::cache_dir()
}

/// Path of the stored layer tar for `digest` (`sha256-<hex>`).
pub fn layer_path(digest: &str) -> PathBuf {
    store_dir().join("layers").join(format!("{digest}.tar"))
}

/// Path of the stored manifest for image `id`.
fn image_path(id: &str) -> PathBuf {
    store_dir().join("images").join(format!("{id}.json"))
}

/// Path of the extracted entrypoint component for image `id`.
pub fn entrypoint_path(id: &str) -> PathBuf {
    store_dir().join("images").join(format!("{id}.wasm"))
}

fn sha256_hex(bytes: &[u8]) -> String {
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn write_creating_dirs(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    std::fs::write(path, bytes).map_err(|e| format!("write {}: {e}", path.display()))
}

/// Store a layer tarball by content digest (a no-op if already stored).
/// Returns the digest key.
pub fn put_layer(tar: &[u8]) -> Result<String, String> {
    let digest = format!("sha256-{}", sha256_hex(tar));
    let path = layer_path(&digest);
    if !path.exists() {
        write_creating_dirs(&path, tar)?;
    }
    Ok(digest)
}

/// Persist an image manifest under `id`.
pub fn save_image(id: &str, manifest: &ImageManifest) -> Result<(), String> {
    let json = serde_json::to_vec_pretty(manifest).map_err(|e| format!("encode manifest: {e}"))?;
    write_creating_dirs(&image_path(id), &json)
}

/// Load the manifest for image `id`, if stored.
pub fn load_image(id: &str) -> Option<ImageManifest> {
    let bytes = std::fs::read(image_path(id)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Normalize an in-image destination path against the current workdir: an
/// absolute dest is used as-is; a relative one joins the workdir. The result is
/// a normalized, leading-`/`-free layer path.
fn dest_path(workdir: &str, dest: &str) -> String {
    let joined = if dest.starts_with('/') {
        dest.to_string()
    } else {
        format!("{}/{}", workdir.trim_end_matches('/'), dest)
    };
    joined
        .split('/')
        .filter(|c| !c.is_empty() && *c != ".")
        .collect::<Vec<_>>()
        .join("/")
}

/// Resolve a COPY source against the build context, refusing escapes — first
/// lexically (`..`/absolute paths), then via canonicalization (symlinks).
fn context_path(context: &Path, src: &str) -> Result<PathBuf, String> {
    let rel = Path::new(src);
    let escapes = rel.is_absolute()
        || rel
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir));
    if escapes {
        return Err(format!("COPY {src}: escapes the build context"));
    }
    let canon = context
        .join(src)
        .canonicalize()
        .map_err(|e| format!("COPY {src}: {e}"))?;
    let ctx = context
        .canonicalize()
        .map_err(|e| format!("build context: {e}"))?;
    if !canon.starts_with(&ctx) {
        return Err(format!("COPY {src}: escapes the build context"));
    }
    Ok(canon)
}

/// Append one on-disk file into the layer tar at `path` (mtime 0, so layer
/// digests are deterministic). `append_data` handles long names.
fn tar_file(b: &mut tar::Builder<Vec<u8>>, path: &str, data: &[u8]) -> Result<(), String> {
    let mut h = tar::Header::new_gnu();
    h.set_entry_type(tar::EntryType::Regular);
    h.set_size(data.len() as u64);
    h.set_mode(0o644);
    b.append_data(&mut h, path, data)
        .map_err(|e| format!("tar {path}: {e}"))
}

fn tar_dir_entry(b: &mut tar::Builder<Vec<u8>>, path: &str) -> Result<(), String> {
    let mut h = tar::Header::new_gnu();
    h.set_entry_type(tar::EntryType::Directory);
    h.set_size(0);
    h.set_mode(0o755);
    b.append_data(&mut h, format!("{path}/"), std::io::empty())
        .map_err(|e| format!("tar {path}/: {e}"))
}

/// Recursively append the *contents* of the directory `src` under `dest`
/// (Docker's `COPY dir /dest` rule), sorted for determinism.
fn tar_dir_contents(b: &mut tar::Builder<Vec<u8>>, src: &Path, dest: &str) -> Result<(), String> {
    let mut entries: Vec<_> = std::fs::read_dir(src)
        .map_err(|e| format!("read {}: {e}", src.display()))?
        .filter_map(|e| e.ok())
        .collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().to_string();
        let sub = if dest.is_empty() {
            name.clone()
        } else {
            format!("{dest}/{name}")
        };
        let meta = e
            .path()
            .symlink_metadata()
            .map_err(|e2| format!("stat {}: {e2}", e.path().display()))?;
        if meta.is_dir() {
            tar_dir_entry(b, &sub)?;
            tar_dir_contents(b, &e.path(), &sub)?;
        } else if meta.is_file() {
            let data = std::fs::read(e.path())
                .map_err(|e2| format!("read {}: {e2}", e.path().display()))?;
            tar_file(b, &sub, &data)?;
        }
        // Symlinks/specials are skipped: not representable in the node vfs.
    }
    Ok(())
}

/// Build one COPY instruction into a layer tarball.
fn copy_layer(
    context: &Path,
    workdir: &str,
    srcs: &[String],
    dest: &str,
) -> Result<Vec<u8>, String> {
    let mut b = tar::Builder::new(Vec::new());
    let dest_base = dest_path(workdir, dest);
    let multi = srcs.len() > 1;
    for src in srcs {
        let from = context_path(context, src)?;
        if from.is_dir() {
            // Directory: contents land under the destination.
            tar_dir_contents(&mut b, &from, &dest_base)?;
        } else {
            let data = std::fs::read(&from).map_err(|e| format!("read {src}: {e}"))?;
            // A trailing '/' (or multiple sources) makes dest a directory.
            let target = if multi || dest.ends_with('/') {
                let base = from
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| src.clone());
                if dest_base.is_empty() {
                    base
                } else {
                    format!("{dest_base}/{base}")
                }
            } else {
                dest_base.clone()
            };
            tar_file(&mut b, &target, &data)?;
        }
    }
    b.into_inner().map_err(|e| format!("finish layer: {e}"))
}

/// Path of the alias file mapping a Dockerfile source to its built image id.
fn alias_path(dockerfile: &Path) -> PathBuf {
    let key = crate::oci::sanitize(&dockerfile.to_string_lossy());
    store_dir().join("images").join(format!("{key}.alias"))
}

/// Build the Dockerfile and record a source→image alias, so later lookups
/// (without rebuilding) resolve the entrypoint and manifest.
pub fn build_and_alias(dockerfile: &Path) -> Result<String, String> {
    let id = build(dockerfile)?;
    write_creating_dirs(&alias_path(dockerfile), id.as_bytes())?;
    Ok(id)
}

/// The built image behind a Dockerfile source, if it has been built.
pub fn aliased_image(dockerfile: &Path) -> Option<(String, ImageManifest)> {
    let id = std::fs::read_to_string(alias_path(dockerfile)).ok()?;
    let manifest = load_image(id.trim())?;
    Some((id.trim().to_string(), manifest))
}

/// Build the image described by `dockerfile_path` (context = its directory)
/// into the store. Deterministic: the returned image id is a digest of the
/// layer contents + config, so an unchanged build is a cache hit.
pub fn build(dockerfile_path: &Path) -> Result<String, String> {
    let source = std::fs::read_to_string(dockerfile_path)
        .map_err(|e| format!("read {}: {e}", dockerfile_path.display()))?;
    let context = dockerfile_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .to_path_buf();
    let df = dockerfile::parse(&source)?;

    let mut manifest = ImageManifest {
        layers: Vec::new(),
        entrypoint: Vec::new(),
        cmd: Vec::new(),
        env: Vec::new(),
        workdir: None,
        labels: BTreeMap::new(),
    };
    let mut workdir = "/".to_string();
    for instr in &df.instructions {
        match instr {
            Instr::From { image } => {
                if image != "scratch" {
                    // A non-scratch base must already be in the local store
                    // (built earlier, or pulled). Layers stack under ours.
                    let base = load_image(&crate::oci::sanitize(image)).ok_or_else(|| {
                        format!(
                            "base image {image:?} is not in the local store \
                             (FROM scratch, or pull/build it first)"
                        )
                    })?;
                    manifest.layers.extend(base.layers);
                    manifest.env.extend(base.env);
                    manifest.entrypoint = base.entrypoint;
                    manifest.cmd = base.cmd;
                }
            }
            Instr::Copy { srcs, dest } => {
                let tar = copy_layer(&context, &workdir, srcs, dest)?;
                manifest.layers.push(put_layer(&tar)?);
            }
            Instr::Env(pairs) => manifest.env.extend(pairs.iter().cloned()),
            Instr::Entrypoint(argv) => manifest.entrypoint = argv.clone(),
            Instr::Cmd(argv) => manifest.cmd = argv.clone(),
            Instr::Workdir(dir) => {
                workdir = dir.clone();
                manifest.workdir = Some(dir.clone());
            }
            Instr::Label(pairs) => manifest.labels.extend(pairs.iter().cloned()),
            Instr::Ignored { keyword } => {
                eprintln!(
                    "wk build: ignoring {} (not applicable to a wasm container)",
                    keyword
                );
            }
        }
    }

    // The executable: entrypoint[0], or cmd[0] when no entrypoint was given.
    let exe = manifest
        .entrypoint
        .first()
        .or(manifest.cmd.first())
        .cloned()
        .ok_or("the Dockerfile sets no ENTRYPOINT (or CMD)")?;

    // Materialize the rootfs and extract the entrypoint component from it.
    let rootfs = crate::vfs::new_fs();
    for digest in &manifest.layers {
        let bytes =
            std::fs::read(layer_path(digest)).map_err(|e| format!("read layer {digest}: {e}"))?;
        let layer = crate::layers::from_tar_bytes(&bytes)?;
        crate::layers::apply(&rootfs, &layer, "");
    }
    let wasm = rootfs
        .lock()
        .unwrap()
        .read_file(&exe, usize::MAX)
        .ok_or_else(|| format!("entrypoint {exe:?} not found in the image rootfs"))?;

    // Content-addressed image id over the manifest (layers + config).
    let manifest_json =
        serde_json::to_vec(&manifest).map_err(|e| format!("encode manifest: {e}"))?;
    let id = format!("sha256-{}", sha256_hex(&manifest_json));
    save_image(&id, &manifest)?;
    write_creating_dirs(&entrypoint_path(&id), &wasm)?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Point the store at a fresh temp dir (nextest = one process per test, so
    /// setting the env var is safe) and return its root.
    fn isolated_store(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wk-image-store-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("XDG_CACHE_HOME", &dir);
        dir
    }

    #[test]
    fn layers_are_content_addressed_and_deduped() {
        isolated_store("layers");
        let a = put_layer(b"same bytes").unwrap();
        let b = put_layer(b"same bytes").unwrap();
        let c = put_layer(b"other bytes").unwrap();
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(a.starts_with("sha256-"));
        assert!(layer_path(&a).is_file());
        assert!(layer_path(&c).is_file());
    }

    #[test]
    fn image_manifest_round_trips() {
        isolated_store("manifest");
        let m = ImageManifest {
            layers: vec!["sha256-abc".into()],
            entrypoint: vec!["/app.wasm".into()],
            cmd: vec!["--flag".into()],
            env: vec![("K".into(), "V".into())],
            workdir: Some("/w".into()),
            labels: BTreeMap::new(),
        };
        save_image("test-image", &m).unwrap();
        assert_eq!(load_image("test-image"), Some(m));
        assert_eq!(load_image("missing"), None);
    }

    /// A build context with a wasm "component", a data dir, and a Dockerfile.
    fn vim_like_context(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wk-build-ctx-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("runtime/doc")).unwrap();
        std::fs::write(dir.join("app.wasm"), b"\0asm-pretend-component").unwrap();
        std::fs::write(dir.join("runtime/doc/help.txt"), b"*help*").unwrap();
        std::fs::write(dir.join("runtime/vimrc"), b"set nocp").unwrap();
        std::fs::write(
            dir.join("Dockerfile"),
            "FROM scratch\n\
             COPY app.wasm /app.wasm\n\
             COPY runtime /usr/share/app/runtime\n\
             ENV APPRUNTIME=/usr/share/app/runtime\n\
             ENTRYPOINT [\"/app.wasm\"]\n\
             CMD [\"--default\"]\n",
        )
        .unwrap();
        dir
    }

    #[test]
    fn builds_a_scratch_image_with_copy_layers() {
        isolated_store("build");
        let ctx = vim_like_context("build");
        let id = build(&ctx.join("Dockerfile")).expect("builds");

        let m = load_image(&id).expect("manifest stored");
        assert_eq!(m.layers.len(), 2, "one layer per COPY");
        for l in &m.layers {
            assert!(layer_path(l).is_file(), "layer {l} stored");
        }
        assert_eq!(m.entrypoint, vec!["/app.wasm"]);
        assert_eq!(m.cmd, vec!["--default"]);
        assert_eq!(
            m.env,
            vec![("APPRUNTIME".into(), "/usr/share/app/runtime".into())]
        );

        // The entrypoint component was extracted from the rootfs.
        assert_eq!(
            std::fs::read(entrypoint_path(&id)).unwrap(),
            b"\0asm-pretend-component"
        );

        // COPY of a directory copies its *contents* under the destination:
        // the layer holds usr/share/app/runtime/vimrc (not .../runtime/runtime).
        let tar = std::fs::read(layer_path(&m.layers[1])).unwrap();
        let layer = crate::layers::from_tar_bytes(&tar).unwrap();
        let fs = crate::vfs::new_fs();
        crate::layers::apply(&fs, &layer, "");
        let g = fs.lock().unwrap();
        assert_eq!(
            g.read_file("/usr/share/app/runtime/vimrc", 64).as_deref(),
            Some(&b"set nocp"[..])
        );
        assert_eq!(
            g.read_file("/usr/share/app/runtime/doc/help.txt", 64)
                .as_deref(),
            Some(&b"*help*"[..])
        );
    }

    #[test]
    fn build_and_alias_resolves_the_entrypoint() {
        isolated_store("alias");
        let ctx = vim_like_context("alias");
        let df = ctx.join("Dockerfile");
        let id = build_and_alias(&df).expect("builds");
        let (aid, manifest) = aliased_image(&df).expect("alias resolves");
        assert_eq!(aid, id);
        assert_eq!(manifest.entrypoint, vec!["/app.wasm"]);
        assert!(entrypoint_path(&id).is_file());
        // Default args = entrypoint[1..] ++ cmd.
        assert_eq!(manifest.default_args(), vec!["--default".to_string()]);
    }

    #[test]
    fn rebuild_is_deterministic_and_cached() {
        isolated_store("determinism");
        let ctx = vim_like_context("determinism");
        let a = build(&ctx.join("Dockerfile")).expect("first build");
        let b = build(&ctx.join("Dockerfile")).expect("second build");
        assert_eq!(a, b, "unchanged context builds the same image id");
    }

    #[test]
    fn copy_outside_the_context_is_rejected() {
        isolated_store("escape");
        let ctx = vim_like_context("escape");
        std::fs::write(
            ctx.join("Dockerfile"),
            "FROM scratch\nCOPY ../secrets /s\nENTRYPOINT [\"/s\"]\n",
        )
        .unwrap();
        let err = build(&ctx.join("Dockerfile")).unwrap_err();
        assert!(err.contains("context"), "err was: {err}");
    }

    #[test]
    fn missing_entrypoint_wasm_is_an_error() {
        isolated_store("noentry");
        let ctx = vim_like_context("noentry");
        std::fs::write(
            ctx.join("Dockerfile"),
            "FROM scratch\nCOPY app.wasm /app.wasm\nENTRYPOINT [\"/nope.wasm\"]\n",
        )
        .unwrap();
        let err = build(&ctx.join("Dockerfile")).unwrap_err();
        assert!(err.contains("entrypoint"), "err was: {err}");
    }

    #[test]
    fn from_unknown_base_is_an_error_for_now() {
        isolated_store("base");
        let ctx = vim_like_context("base");
        std::fs::write(
            ctx.join("Dockerfile"),
            "FROM ghcr.io/nowhere/base:1\nCOPY app.wasm /a\nENTRYPOINT [\"/a\"]\n",
        )
        .unwrap();
        let err = build(&ctx.join("Dockerfile")).unwrap_err();
        assert!(err.contains("base image"), "err was: {err}");
    }
}
