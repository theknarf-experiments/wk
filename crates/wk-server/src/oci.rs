//! Pulling wasm plugins from OCI registries as **Wasm OCI Artifacts** (the
//! CNCF/Bytecode-Alliance format: a config of `application/vnd.wasm.config.v0+json`
//! and a single `application/wasm` layer holding the component). This is wk's
//! package-manager path — a dependency in the workspace file can be `oci://<ref>` instead
//! of a local path, e.g. `oci://ghcr.io/org/name:1.0`.
//!
//! Pulled artifacts are cached by reference under `~/.cache/wk/oci/`, so `wk run`
//! only hits the network the first time.

use std::path::PathBuf;

use oci_client::client::{ClientConfig, ClientProtocol, Config, ImageLayer};
use oci_client::secrets::RegistryAuth;
use oci_client::{Client, Reference};

/// The Wasm OCI Artifact layer media type.
const WASM_LAYER: &str = "application/wasm";
/// Container-image rootfs layer media types (OCI + Docker).
const TAR_LAYERS: [&str; 3] = [
    "application/vnd.oci.image.layer.v1.tar",
    "application/vnd.oci.image.layer.v1.tar+gzip",
    "application/vnd.docker.image.rootfs.diff.tar.gzip",
];
/// The Wasm OCI Artifact config media type, and a minimal config body.
const WASM_CONFIG: &str = "application/vnd.wasm.config.v0+json";
const WASM_CONFIG_BODY: &str = r#"{"architecture":"wasm","os":"wasi"}"#;

/// A client for `image`'s registry. A `localhost` registry is served over plain
/// HTTP (the common local-testing setup, e.g. `registry:2` in compose.yml).
fn client_for(image: &Reference) -> Client {
    let registry = image.registry().to_string();
    let mut config = ClientConfig::default();
    if registry.starts_with("localhost") || registry.starts_with("127.0.0.1") {
        config.protocol = ClientProtocol::HttpsExcept(vec![registry]);
    }
    // Multi-platform images: pick the wasi/wasm entry (falling back to the
    // first) instead of the host platform — wk runs wasm, not host binaries.
    config.platform_resolver = Some(Box::new(|entries| {
        entries
            .iter()
            .find(|e| {
                e.platform.as_ref().is_some_and(|p| {
                    p.os.to_string() == "wasi" || p.architecture.to_string().starts_with("wasm")
                })
            })
            .or_else(|| entries.first())
            .map(|e| e.digest.clone())
    }));
    Client::new(config)
}

pub(crate) fn cache_dir() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|| PathBuf::from(".wk-cache"))
        .join("wk")
        .join("oci")
}

/// Make a reference safe to use as a filename.
pub(crate) fn sanitize(reference: &str) -> String {
    reference
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Where a pulled artifact for `reference` is cached.
pub fn cache_path(reference: &str) -> PathBuf {
    cache_dir().join(format!("{}.wasm", sanitize(reference)))
}

/// A reasonable dependency name for an OCI reference: the last path segment of
/// the repository (e.g. `ghcr.io/org/foo:1.0` -> `foo`).
pub fn name_for(reference: &str) -> String {
    reference
        .parse::<Reference>()
        .ok()
        .and_then(|r| r.repository().rsplit('/').next().map(|s| s.to_string()))
        .unwrap_or_else(|| "plugin".to_string())
}

/// Pull the wasm component bytes for `reference` from its OCI registry
/// (anonymously). Blocking — runs a small Tokio runtime internally.
pub fn pull(reference: &str) -> Result<Vec<u8>, String> {
    let image: Reference = reference
        .parse()
        .map_err(|e| format!("invalid OCI reference {reference:?}: {e}"))?;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("tokio runtime: {e}"))?;

    rt.block_on(async {
        let client = client_for(&image);
        let data = client
            .pull(&image, &RegistryAuth::Anonymous, vec![WASM_LAYER])
            .await
            .map_err(|e| format!("failed to pull {reference}: {e}"))?;
        let layer = data
            .layers
            .into_iter()
            .next()
            .ok_or_else(|| format!("{reference}: artifact has no {WASM_LAYER} layer"))?;
        Ok(layer.data.to_vec())
    })
}

/// Whether `bytes` is a *core* wasm module (as opposed to a component):
/// `\0asm` magic followed by the core version field `1`.
pub(crate) fn is_core_module(bytes: &[u8]) -> bool {
    bytes.len() >= 8 && &bytes[0..4] == b"\0asm" && bytes[4..8] == [1, 0, 0, 0]
}

/// The WASIp1→component command adapter, downloaded once from the wasmtime
/// release matching our runtime and cached in the store.
fn cached_adapter() -> Result<Vec<u8>, String> {
    const WASMTIME_VER: &str = "46.0.1";
    let path = cache_dir().join(format!(
        "wasi_snapshot_preview1.command-{WASMTIME_VER}.wasm"
    ));
    if let Ok(bytes) = std::fs::read(&path) {
        return Ok(bytes);
    }
    let url = format!(
        "https://github.com/bytecodealliance/wasmtime/releases/download/v{WASMTIME_VER}/wasi_snapshot_preview1.command.wasm"
    );
    println!("fetching WASI command adapter {WASMTIME_VER} ...");
    let out = std::process::Command::new("curl")
        .args(["-fsSL", &url])
        .output()
        .map_err(|e| format!("curl for the WASI adapter: {e}"))?;
    if !out.status.success() || out.stdout.is_empty() {
        return Err(format!(
            "failed to fetch the WASI adapter from {url}; place it at {}",
            path.display()
        ));
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&path, &out.stdout).map_err(|e| e.to_string())?;
    Ok(out.stdout)
}

/// Make pulled wasm loadable: a component passes through; a core WASIp1 module
/// (what most registry wasm images ship) is componentized with the command
/// adapter, the same transformation `wasm-tools component new` performs.
pub(crate) fn ensure_component(bytes: &[u8]) -> Result<Vec<u8>, String> {
    if !is_core_module(bytes) {
        return Ok(bytes.to_vec());
    }
    let adapter = cached_adapter()?;
    wit_component::ComponentEncoder::default()
        .module(bytes)
        .map_err(|e| format!("read core module: {e:#}"))?
        .adapter("wasi_snapshot_preview1", &adapter)
        .map_err(|e| format!("apply WASI adapter: {e:#}"))?
        .encode()
        .map_err(|e| format!("componentize pulled module: {e:#}"))
}

/// Pull `reference` into the local cache/store: a Wasm OCI Artifact writes its
/// component to the cache path; a container image stores its layers + config
/// via `crate::images` (entrypoint extracted and componentized as needed).
pub fn pull_into_cache(reference: &str) -> Result<(), String> {
    let image: Reference = reference
        .parse()
        .map_err(|e| format!("invalid OCI reference {reference:?}: {e}"))?;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("tokio runtime: {e}"))?;

    let data = rt.block_on(async {
        let client = client_for(&image);
        let mut accepted = vec![WASM_LAYER];
        accepted.extend(TAR_LAYERS);
        client
            .pull(&image, &RegistryAuth::Anonymous, accepted)
            .await
            .map_err(|e| format!("failed to pull {reference}: {e}"))
    })?;

    let is_artifact = data.layers.len() == 1
        && data
            .layers
            .first()
            .is_some_and(|l| l.media_type == WASM_LAYER);
    if is_artifact {
        let wasm = ensure_component(&data.layers[0].data)?;
        let path = cache_path(reference);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        return std::fs::write(&path, wasm).map_err(|e| e.to_string());
    }
    // A container image: tar layers + config.
    let layers: Vec<(String, Vec<u8>)> = data
        .layers
        .into_iter()
        .map(|l| (l.media_type, l.data.to_vec()))
        .collect();
    crate::images::store_pulled_image(reference, &layers, &data.config.data, &ensure_component)?;
    Ok(())
}

/// Push `wasm` to `reference` as a Wasm OCI Artifact (anonymously). Blocking.
pub fn push(reference: &str, wasm: &[u8]) -> Result<(), String> {
    let image: Reference = reference
        .parse()
        .map_err(|e| format!("invalid OCI reference {reference:?}: {e}"))?;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("tokio runtime: {e}"))?;

    rt.block_on(async {
        let client = client_for(&image);
        let layer = ImageLayer::new(wasm.to_vec(), WASM_LAYER.to_string(), None);
        let config = Config::new(
            WASM_CONFIG_BODY.as_bytes().to_vec(),
            WASM_CONFIG.to_string(),
            None,
        );
        // `manifest: None` lets oci-client build the OCI manifest from the
        // config + layer (digests and sizes filled in).
        client
            .push(
                &image,
                std::slice::from_ref(&layer),
                config,
                &RegistryAuth::Anonymous,
                None,
            )
            .await
            .map_err(|e| format!("failed to push {reference}: {e}"))?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_from_reference() {
        assert_eq!(name_for("ghcr.io/org/foo:1.0"), "foo");
        assert_eq!(name_for("ghcr.io/a/b/c:latest"), "c");
        assert_eq!(name_for("docker.io/library/hello"), "hello");
    }

    #[test]
    fn cache_path_is_stable_and_under_cache() {
        let a = cache_path("ghcr.io/org/foo:1.0");
        let b = cache_path("ghcr.io/org/foo:1.0");
        assert_eq!(a, b);
        assert!(a.to_string_lossy().ends_with(".wasm"));
        // No path separators from the reference leak into the filename.
        assert!(!a.file_name().unwrap().to_string_lossy().contains('/'));
    }
}
