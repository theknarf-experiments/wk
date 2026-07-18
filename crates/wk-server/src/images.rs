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

/// Every stored image: (id, manifest), sorted by id.
pub fn list_images() -> Vec<(String, ImageManifest)> {
    let dir = store_dir().join("images");
    let mut out: Vec<(String, ImageManifest)> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let id = name.strip_suffix(".json")?.to_string();
            Some((id.clone(), load_image(&id)?))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Remove a stored image: its manifest and extracted entrypoint. Layer tars
/// stay (they are content-addressed and may be shared by other images).
/// Returns whether anything was removed.
pub fn remove_image(id: &str) -> bool {
    let removed = std::fs::remove_file(image_path(id)).is_ok();
    let _ = std::fs::remove_file(entrypoint_path(id));
    removed
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

/// Turns pulled wasm into loadable wasm (componentizes a core module; see
/// `crate::oci::ensure_component`). A seam so image ingestion is testable.
pub type AdaptFn = dyn Fn(&[u8]) -> Result<Vec<u8>, String>;

/// Ingest a pulled OCI container image into the store: tar layers (gunzipped
/// if compressed) become content-addressed layers, the image config's
/// Entrypoint/Cmd/Env/WorkingDir/Labels become the manifest, and the
/// entrypoint wasm is extracted from the rootfs — run through `adapt` (which
/// componentizes a core module; see `crate::oci`) — and written to the
/// artifact cache path so `Source::Oci` loads it unchanged. The image is
/// stored under the sanitized reference, so `FROM <reference>` finds it.
pub fn store_pulled_image(
    reference: &str,
    layers: &[(String, Vec<u8>)],
    config_json: &[u8],
    adapt: &AdaptFn,
) -> Result<String, String> {
    let mut manifest = ImageManifest {
        layers: Vec::new(),
        entrypoint: Vec::new(),
        cmd: Vec::new(),
        env: Vec::new(),
        workdir: None,
        labels: BTreeMap::new(),
    };
    for (media, bytes) in layers {
        if !media.contains("tar") {
            return Err(format!("{reference}: unsupported layer media type {media}"));
        }
        // Store decompressed, so every consumer reads plain tars.
        let plain: Vec<u8> = if bytes.starts_with(&[0x1f, 0x8b]) {
            use std::io::Read;
            let mut out = Vec::new();
            flate2::read::GzDecoder::new(bytes.as_slice())
                .read_to_end(&mut out)
                .map_err(|e| format!("{reference}: gunzip layer: {e}"))?;
            out
        } else {
            bytes.clone()
        };
        manifest.layers.push(put_layer(&plain)?);
    }

    // The OCI/Docker image config: {"config": {"Entrypoint": [...], ...}}.
    let config: serde_json::Value = serde_json::from_slice(config_json)
        .map_err(|e| format!("{reference}: parse image config: {e}"))?;
    let cfg = config.get("config").cloned().unwrap_or_default();
    let strings = |v: Option<&serde_json::Value>| -> Vec<String> {
        v.and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    manifest.entrypoint = strings(cfg.get("Entrypoint"));
    manifest.cmd = strings(cfg.get("Cmd"));
    manifest.env = strings(cfg.get("Env"))
        .into_iter()
        .filter_map(|kv| {
            kv.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
        })
        .collect();
    manifest.workdir = cfg
        .get("WorkingDir")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    if let Some(labels) = cfg.get("Labels").and_then(|v| v.as_object()) {
        for (k, v) in labels {
            if let Some(v) = v.as_str() {
                manifest.labels.insert(k.clone(), v.to_string());
            }
        }
    }

    // Extract the entrypoint wasm from the rootfs; componentize if needed.
    let exe = manifest
        .entrypoint
        .first()
        .or(manifest.cmd.first())
        .cloned()
        .ok_or_else(|| format!("{reference}: image config has no Entrypoint or Cmd"))?;
    let rootfs = crate::vfs::new_fs();
    for digest in &manifest.layers {
        let bytes =
            std::fs::read(layer_path(digest)).map_err(|e| format!("read layer {digest}: {e}"))?;
        crate::layers::apply(&rootfs, &crate::layers::from_tar_bytes(&bytes)?, "");
    }
    let wasm = rootfs
        .lock()
        .unwrap()
        .read_file(&exe, usize::MAX)
        .ok_or_else(|| format!("{reference}: entrypoint {exe:?} not found in the image"))?;
    let wasm = adapt(&wasm)?;

    let id = crate::oci::sanitize(reference);
    save_image(&id, &manifest)?;
    write_creating_dirs(&crate::oci::cache_path(reference), &wasm)?;
    Ok(id)
}

/// Path of the alias file mapping a Dockerfile source to its built image id.
fn alias_path(dockerfile: &Path) -> PathBuf {
    let key = crate::oci::sanitize(&dockerfile.to_string_lossy());
    store_dir().join("images").join(format!("{key}.alias"))
}

/// Build the Dockerfile and record a source→image alias, so later lookups
/// (without rebuilding) resolve the entrypoint and manifest. A Dockerfile with
/// RUN steps gets a real wasm runner (a scratch plugin host).
pub fn build_and_alias(dockerfile: &Path) -> Result<String, String> {
    let needs_runner = std::fs::read_to_string(dockerfile)
        .ok()
        .and_then(|src| dockerfile::parse(&src).ok())
        .is_some_and(|df| df.instructions.iter().any(|i| matches!(i, Instr::Run(_))));
    let id = if needs_runner {
        let host = crate::plugin::PluginHost::new().map_err(|e| format!("build runner: {e:#}"))?;
        build_with_runner(dockerfile, Some(&host))?
    } else {
        build(dockerfile)?
    };
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
/// layer contents + config, so an unchanged build is a cache hit. RUN
/// instructions error without a runner; use [`build_with_runner`].
pub fn build(dockerfile_path: &Path) -> Result<String, String> {
    build_with_runner(dockerfile_path, None)
}

/// Executes a build-time `RUN` step: run the wasm CLI `wasm` with `argv` and
/// `env` against the build rootfs `fs` (its writes become the RUN's layer).
/// The plugin host implements this; tests inject mocks.
pub trait BuildRunner {
    fn run(
        &self,
        wasm: &[u8],
        argv: &[String],
        env: &[(String, String)],
        fs: &crate::vfs::SharedFs,
    ) -> Result<(), String>;
}

/// Capture everything written to `fs` since `before` as a layer tarball:
/// privately written files (created or copied-up), new directories, and
/// deletions (as OCI whiteouts, topmost path only). `None` if nothing changed.
fn diff_layer(
    before: &BTreeMap<String, crate::vfs::PathKind>,
    fs: &crate::vfs::SharedFs,
) -> Result<Option<Vec<u8>>, String> {
    use crate::vfs::PathKind;
    let g = fs.lock().unwrap();
    let after = g.snapshot();
    let mut b = tar::Builder::new(Vec::new());
    let mut changed = false;
    for path in before.keys() {
        if !after.contains_key(path) {
            // Skip children of an already-whited-out directory.
            let covered = path
                .rsplit_once('/')
                .is_some_and(|(dir, _)| before.contains_key(dir) && !after.contains_key(dir));
            if covered {
                continue;
            }
            let (dir, name) = path.rsplit_once('/').unwrap_or(("", path.as_str()));
            let wh = if dir.is_empty() {
                format!(".wh.{name}")
            } else {
                format!("{dir}/.wh.{name}")
            };
            tar_file(&mut b, &wh, b"")?;
            changed = true;
        }
    }
    for (path, kind) in &after {
        match kind {
            PathKind::Dir if !before.contains_key(path) => {
                tar_dir_entry(&mut b, path)?;
                changed = true;
            }
            // A private file is a write since the last snapshot: RUN layers are
            // re-applied as layer files, so anything private here is new.
            PathKind::PrivateFile => {
                let data = g
                    .read_file(path, usize::MAX)
                    .ok_or_else(|| format!("diff: {path} vanished"))?;
                tar_file(&mut b, path, &data)?;
                changed = true;
            }
            _ => {}
        }
    }
    if !changed {
        return Ok(None);
    }
    b.into_inner()
        .map(Some)
        .map_err(|e| format!("finish diff layer: {e}"))
}

/// [`build`], with a runner for `RUN` instructions. The rootfs is materialized
/// live as layers apply, so each RUN sees the filesystem built so far and its
/// writes are captured (via [`diff_layer`]) as the next layer.
pub fn build_with_runner(
    dockerfile_path: &Path,
    runner: Option<&dyn BuildRunner>,
) -> Result<String, String> {
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
    // The rootfs built so far, kept live for RUN steps and the final
    // entrypoint extraction.
    let rootfs = crate::vfs::new_fs();
    let apply_tar = |manifest: &mut ImageManifest, tar: &[u8]| -> Result<(), String> {
        let digest = put_layer(tar)?;
        crate::layers::apply(&rootfs, &crate::layers::from_tar_bytes(tar)?, "");
        manifest.layers.push(digest);
        Ok(())
    };
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
                    for digest in &base.layers {
                        let bytes = std::fs::read(layer_path(digest))
                            .map_err(|e| format!("read layer {digest}: {e}"))?;
                        crate::layers::apply(&rootfs, &crate::layers::from_tar_bytes(&bytes)?, "");
                    }
                    manifest.layers.extend(base.layers);
                    manifest.env.extend(base.env);
                    manifest.entrypoint = base.entrypoint;
                    manifest.cmd = base.cmd;
                }
            }
            Instr::Copy { srcs, dest } => {
                let tar = copy_layer(&context, &workdir, srcs, dest)?;
                apply_tar(&mut manifest, &tar)?;
            }
            Instr::Run(argv) => {
                let Some(runner) = runner else {
                    return Err(
                        "RUN needs a build runner (wasm execution); build through wk".into(),
                    );
                };
                let exe = argv
                    .first()
                    .ok_or("RUN needs a command (a wasm path in the rootfs)")?;
                let wasm = rootfs
                    .lock()
                    .unwrap()
                    .read_file(exe, usize::MAX)
                    .ok_or_else(|| format!("RUN {exe}: not found in the rootfs built so far"))?;
                if !wasm.starts_with(b"\0asm") {
                    return Err(format!("RUN {exe}: not a wasm file"));
                }
                let before = rootfs.lock().unwrap().snapshot();
                runner.run(&wasm, argv, &manifest.env, &rootfs)?;
                if let Some(tar) = diff_layer(&before, &rootfs)? {
                    // Store AND re-apply: the outputs become layer files, so the
                    // next RUN's diff starts clean.
                    apply_tar(&mut manifest, &tar)?;
                }
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

    /// A mock RUN runner: records its invocation and mutates the rootfs the
    /// way a generator component would (writes a file, deletes a seed).
    struct MockRunner {
        called: std::sync::atomic::AtomicUsize,
    }
    impl BuildRunner for MockRunner {
        fn run(
            &self,
            wasm: &[u8],
            argv: &[String],
            env: &[(String, String)],
            fs: &crate::vfs::SharedFs,
        ) -> Result<(), String> {
            self.called
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            assert!(wasm.starts_with(b"\0asm"), "handed the target's bytes");
            assert_eq!(argv, ["/gen.wasm", "--make"]);
            assert!(
                env.contains(&("X".to_string(), "1".to_string())),
                "sees prior ENV"
            );
            let mut g = fs.lock().unwrap();
            g.put_file_at("data/out.txt", b"made".to_vec());
            g.remove_path("data/seed.txt");
            Ok(())
        }
    }

    #[test]
    fn run_layer_captures_writes_and_deletes() {
        isolated_store("run");
        let ctx = vim_like_context("run");
        std::fs::write(ctx.join("seed.txt"), b"seed").unwrap();
        std::fs::write(
            ctx.join("Dockerfile"),
            "FROM scratch\n\
             COPY app.wasm /gen.wasm\n\
             COPY seed.txt /data/seed.txt\n\
             ENV X=1\n\
             RUN /gen.wasm --make\n\
             ENTRYPOINT [\"/gen.wasm\"]\n",
        )
        .unwrap();
        let runner = MockRunner {
            called: std::sync::atomic::AtomicUsize::new(0),
        };
        let id = build_with_runner(&ctx.join("Dockerfile"), Some(&runner)).expect("builds");
        assert_eq!(runner.called.load(std::sync::atomic::Ordering::SeqCst), 1);

        let m = load_image(&id).expect("stored");
        assert_eq!(m.layers.len(), 3, "two COPYs + one RUN layer");

        // Replay all layers into a fresh fs: the RUN's write is there, the
        // deleted seed is gone (captured as a whiteout).
        let fs = crate::vfs::new_fs();
        for l in &m.layers {
            let bytes = std::fs::read(layer_path(l)).unwrap();
            crate::layers::apply(&fs, &crate::layers::from_tar_bytes(&bytes).unwrap(), "");
        }
        let g = fs.lock().unwrap();
        assert_eq!(
            g.read_file("/data/out.txt", 64).as_deref(),
            Some(&b"made"[..])
        );
        assert!(
            g.read_file("/data/seed.txt", 64).is_none(),
            "whiteout captured"
        );
        assert!(g.read_file("/gen.wasm", 64).is_some());
    }

    #[test]
    fn run_without_a_runner_is_an_error() {
        isolated_store("norunner");
        let ctx = vim_like_context("norunner");
        std::fs::write(
            ctx.join("Dockerfile"),
            "FROM scratch\nCOPY app.wasm /g.wasm\nRUN /g.wasm\nENTRYPOINT [\"/g.wasm\"]\n",
        )
        .unwrap();
        let err = build(&ctx.join("Dockerfile")).unwrap_err();
        assert!(err.contains("RUN"), "err was: {err}");
    }

    #[test]
    fn run_target_must_exist_and_be_wasm() {
        isolated_store("runtarget");
        let ctx = vim_like_context("runtarget");
        let runner = MockRunner {
            called: std::sync::atomic::AtomicUsize::new(0),
        };
        std::fs::write(
            ctx.join("Dockerfile"),
            "FROM scratch\nCOPY app.wasm /g.wasm\nRUN /missing.wasm\nENTRYPOINT [\"/g.wasm\"]\n",
        )
        .unwrap();
        let err = build_with_runner(&ctx.join("Dockerfile"), Some(&runner)).unwrap_err();
        assert!(err.contains("missing.wasm"), "err was: {err}");

        std::fs::write(ctx.join("notwasm.txt"), b"plain text").unwrap();
        std::fs::write(
            ctx.join("Dockerfile"),
            "FROM scratch\nCOPY notwasm.txt /t\nRUN /t\nENTRYPOINT [\"/t\"]\n",
        )
        .unwrap();
        let err = build_with_runner(&ctx.join("Dockerfile"), Some(&runner)).unwrap_err();
        assert!(err.contains("wasm"), "err was: {err}");
    }

    #[test]
    fn list_and_remove_images() {
        isolated_store("listrm");
        let ctx = vim_like_context("listrm");
        let id = build_and_alias(&ctx.join("Dockerfile")).expect("builds");

        let listed = list_images();
        assert!(listed
            .iter()
            .any(|(i, m)| i == &id && m.entrypoint == vec!["/app.wasm"]));

        assert!(remove_image(&id), "removes a stored image");
        assert!(load_image(&id).is_none(), "manifest gone");
        assert!(!entrypoint_path(&id).is_file(), "extracted wasm gone");
        assert!(!remove_image(&id), "second remove reports missing");
        assert!(list_images().iter().all(|(i, _)| i != &id));
    }

    /// Synthetic pulled image: a plain tar layer holding the component, a
    /// gzipped tar layer holding data, and a Docker-style config JSON.
    #[test]
    fn pulled_image_is_stored_with_config_and_entrypoint() {
        isolated_store("pull");
        // A minimal *component* header (\0asm + component-model version), so no
        // adaptation is needed.
        let component = b"\0asm\x0d\x00\x01\x00-pretend-component".to_vec();
        let mut b1 = tar::Builder::new(Vec::new());
        {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Regular);
            h.set_size(component.len() as u64);
            h.set_mode(0o755);
            b1.append_data(&mut h, "app.wasm", component.as_slice())
                .unwrap();
        }
        let l1 = b1.into_inner().unwrap();
        let mut b2 = tar::Builder::new(Vec::new());
        {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Regular);
            h.set_size(4);
            h.set_mode(0o644);
            b2.append_data(&mut h, "etc/motd", &b"data"[..]).unwrap();
        }
        let l2_plain = b2.into_inner().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut gz, &l2_plain).unwrap();
        let l2 = gz.finish().unwrap();

        let config = br#"{
            "architecture": "wasm32", "os": "wasi",
            "config": {
                "Entrypoint": ["/app.wasm"],
                "Cmd": ["--serve"],
                "Env": ["A=1", "B=two words"],
                "WorkingDir": "/srv",
                "Labels": {"maintainer": "wk"}
            }
        }"#;

        let layers = vec![
            ("application/vnd.oci.image.layer.v1.tar".to_string(), l1),
            (
                "application/vnd.oci.image.layer.v1.tar+gzip".to_string(),
                l2,
            ),
        ];
        let reference = "ghcr.io/org/thing:1";
        let id =
            store_pulled_image(reference, &layers, config, &|b| Ok(b.to_vec())).expect("stores");

        let m = load_image(&id).expect("manifest under the sanitized reference");
        assert_eq!(m.layers.len(), 2);
        assert_eq!(m.entrypoint, vec!["/app.wasm"]);
        assert_eq!(m.cmd, vec!["--serve"]);
        assert_eq!(
            m.env,
            vec![
                ("A".to_string(), "1".to_string()),
                ("B".to_string(), "two words".to_string())
            ]
        );
        assert_eq!(m.workdir.as_deref(), Some("/srv"));

        // The gzipped layer is stored decompressed (plain tar).
        let stored = std::fs::read(layer_path(&m.layers[1])).unwrap();
        assert!(!stored.starts_with(&[0x1f, 0x8b]), "stored as plain tar");

        // The entrypoint component landed at the artifact cache path, so
        // Source::Oci's local_path works unchanged.
        assert_eq!(
            std::fs::read(crate::oci::cache_path(reference)).unwrap(),
            component
        );
    }

    #[test]
    fn pulled_core_module_goes_through_the_adapter() {
        isolated_store("pulladapt");
        let module = b"\0asm\x01\x00\x00\x00-core-module".to_vec();
        let mut b1 = tar::Builder::new(Vec::new());
        {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Regular);
            h.set_size(module.len() as u64);
            h.set_mode(0o755);
            b1.append_data(&mut h, "m.wasm", module.as_slice()).unwrap();
        }
        let layers = vec![(
            "application/vnd.docker.image.rootfs.diff.tar.gzip".to_string(),
            b1.into_inner().unwrap(),
        )];
        let config = br#"{"config": {"Entrypoint": ["/m.wasm"]}}"#;
        let reference = "docker.io/org/mod:1";
        store_pulled_image(reference, &layers, config, &|_| Ok(b"ADAPTED".to_vec()))
            .expect("stores");
        assert_eq!(
            std::fs::read(crate::oci::cache_path(reference)).unwrap(),
            b"ADAPTED"
        );
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
