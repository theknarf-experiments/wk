//! The wk project model: a `wk.toml` manifest in the working directory that
//! lists the plugin components the project loads.
//!
//! ```toml
//! name = "my-workspace"
//! plugins = ["plugins/paint/.../paint.wasm"]
//! ```

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Manifest file name, looked up in the current directory.
pub const MANIFEST: &str = "wk.toml";

#[derive(Debug, Serialize, Deserialize)]
pub struct Project {
    pub name: String,
    #[serde(default)]
    pub plugins: Vec<PathBuf>,
}

impl Project {
    /// Load the project manifest from the current directory.
    pub fn load() -> Result<Self, String> {
        let text = std::fs::read_to_string(MANIFEST)
            .map_err(|e| format!("no {MANIFEST} in this directory ({e}); run `wk init` first"))?;
        toml::from_str(&text).map_err(|e| format!("failed to parse {MANIFEST}: {e}"))
    }

    /// Write the manifest back to the current directory.
    pub fn save(&self) -> Result<(), String> {
        let text = toml::to_string_pretty(self)
            .map_err(|e| format!("failed to serialize project: {e}"))?;
        std::fs::write(MANIFEST, text).map_err(|e| format!("failed to write {MANIFEST}: {e}"))
    }

    /// Add a plugin path to the project (idempotent), persisting the manifest.
    /// Returns `true` if the plugin was newly added.
    pub fn add_plugin(&mut self, plugin: PathBuf) -> Result<bool, String> {
        if self.plugins.contains(&plugin) {
            return Ok(false);
        }
        self.plugins.push(plugin);
        self.save()?;
        Ok(true)
    }
}

/// Create a new `wk.toml` in the current directory. Errors if one exists.
pub fn init(name: Option<String>) -> Result<(), String> {
    if Path::new(MANIFEST).exists() {
        return Err(format!("{MANIFEST} already exists"));
    }
    let name = name.unwrap_or_else(|| {
        std::env::current_dir()
            .ok()
            .and_then(|d| d.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "wk-project".to_string())
    });
    let project = Project {
        name,
        plugins: Vec::new(),
    };
    project.save()?;
    println!("created {MANIFEST}");
    Ok(())
}

/// Add a plugin to the project manifest.
pub fn add(plugin: PathBuf) -> Result<(), String> {
    let mut project = Project::load()?;
    if project.add_plugin(plugin.clone())? {
        println!("added plugin: {}", plugin.display());
    } else {
        println!("plugin already in project: {}", plugin.display());
    }
    Ok(())
}
