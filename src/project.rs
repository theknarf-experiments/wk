//! The wk project model: a `wk.kdl` manifest in the working directory that
//! lists the plugin components the project loads.
//!
//! ```kdl
//! name "my-workspace"
//! plugin "plugins/paint/.../paint.wasm"
//! plugin "plugins/triangle/.../triangle.wasm"
//! ```

use kdl::{KdlDocument, KdlEntry, KdlNode};
use std::path::{Path, PathBuf};

/// Manifest file name, looked up in the current directory.
pub const MANIFEST: &str = "wk.kdl";

#[derive(Debug)]
pub struct Project {
    pub name: String,
    pub plugins: Vec<PathBuf>,
}

impl Project {
    /// Load the project manifest from the current directory.
    pub fn load() -> Result<Self, String> {
        let text = std::fs::read_to_string(MANIFEST)
            .map_err(|e| format!("no {MANIFEST} in this directory ({e}); run `wk init` first"))?;
        let doc: KdlDocument = text
            .parse()
            .map_err(|e| format!("failed to parse {MANIFEST}: {e}"))?;

        let name = doc
            .get("name")
            .and_then(|n| n.get(0))
            .and_then(|v| v.as_string())
            .unwrap_or("wk-project")
            .to_string();

        let plugins = doc
            .nodes()
            .iter()
            .filter(|n| n.name().value() == "plugin")
            .filter_map(|n| n.get(0).and_then(|v| v.as_string()))
            .map(PathBuf::from)
            .collect();

        Ok(Project { name, plugins })
    }

    /// Write the manifest back to the current directory.
    pub fn save(&self) -> Result<(), String> {
        let mut doc = KdlDocument::new();

        let mut name_node = KdlNode::new("name");
        name_node.push(KdlEntry::new(self.name.clone()));
        doc.nodes_mut().push(name_node);

        for plugin in &self.plugins {
            let mut node = KdlNode::new("plugin");
            node.push(KdlEntry::new(plugin.to_string_lossy().to_string()));
            doc.nodes_mut().push(node);
        }

        doc.autoformat();
        std::fs::write(MANIFEST, doc.to_string())
            .map_err(|e| format!("failed to write {MANIFEST}: {e}"))
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

/// Create a new `wk.kdl` in the current directory. Errors if one exists.
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
