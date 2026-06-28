//! The wk project model: a `wk.kdl` manifest in the working directory that
//! lists the plugin components the project loads, with optional per-plugin
//! config.
//!
//! ```kdl
//! name "my-workspace"
//! plugin "plugins/paint/.../paint.wasm" {
//!     title "Paint"
//!     size 320 240
//! }
//! plugin "plugins/triangle/.../triangle.wasm"
//! ```

use kdl::{KdlDocument, KdlEntry, KdlNode};
use std::path::{Path, PathBuf};

/// Manifest file name, looked up in the current directory.
pub const MANIFEST: &str = "wk.kdl";

/// One plugin entry in the project, with optional display config.
#[derive(Debug, Clone)]
pub struct PluginSpec {
    pub path: PathBuf,
    /// Display name in the launcher and window title; defaults to the file stem.
    pub title: Option<String>,
    /// Default window size on the canvas.
    pub size: Option<(u32, u32)>,
}

impl PluginSpec {
    pub fn from_path(path: PathBuf) -> Self {
        Self {
            path,
            title: None,
            size: None,
        }
    }

    /// The launcher/window label: the configured title, else the file stem.
    pub fn label(&self) -> String {
        self.title.clone().unwrap_or_else(|| {
            self.path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "plugin".to_string())
        })
    }

    /// Whether `arg` names this plugin (its path, file stem, or title).
    fn matches(&self, arg: &str) -> bool {
        self.path.to_string_lossy() == arg
            || self.path.file_stem().is_some_and(|s| s == arg)
            || self.label() == arg
    }
}

#[derive(Debug)]
pub struct Project {
    pub name: String,
    pub plugins: Vec<PluginSpec>,
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
            .filter_map(|n| {
                let path = n.get(0)?.as_string()?;
                let mut spec = PluginSpec::from_path(PathBuf::from(path));
                if let Some(children) = n.children() {
                    spec.title = children
                        .get("title")
                        .and_then(|t| t.get(0))
                        .and_then(|v| v.as_string())
                        .map(String::from);
                    if let Some(size) = children.get("size") {
                        if let (Some(w), Some(h)) = (
                            size.get(0).and_then(|v| v.as_integer()),
                            size.get(1).and_then(|v| v.as_integer()),
                        ) {
                            spec.size = Some((w as u32, h as u32));
                        }
                    }
                }
                Some(spec)
            })
            .collect();

        Ok(Project { name, plugins })
    }

    /// Write the manifest back to the current directory.
    pub fn save(&self) -> Result<(), String> {
        let mut doc = KdlDocument::new();

        let mut name_node = KdlNode::new("name");
        name_node.push(KdlEntry::new(self.name.clone()));
        doc.nodes_mut().push(name_node);

        for spec in &self.plugins {
            let mut node = KdlNode::new("plugin");
            node.push(KdlEntry::new(spec.path.to_string_lossy().to_string()));
            if spec.title.is_some() || spec.size.is_some() {
                let mut children = KdlDocument::new();
                if let Some(title) = &spec.title {
                    let mut t = KdlNode::new("title");
                    t.push(KdlEntry::new(title.clone()));
                    children.nodes_mut().push(t);
                }
                if let Some((w, h)) = spec.size {
                    let mut s = KdlNode::new("size");
                    s.push(KdlEntry::new(w as i128));
                    s.push(KdlEntry::new(h as i128));
                    children.nodes_mut().push(s);
                }
                node.set_children(children);
            }
            doc.nodes_mut().push(node);
        }

        doc.autoformat();
        std::fs::write(MANIFEST, doc.to_string())
            .map_err(|e| format!("failed to write {MANIFEST}: {e}"))
    }

    /// Add a plugin path to the project (idempotent), persisting the manifest.
    /// Returns `true` if the plugin was newly added.
    pub fn add_plugin(&mut self, plugin: PathBuf) -> Result<bool, String> {
        if self.plugins.iter().any(|s| s.path == plugin) {
            return Ok(false);
        }
        self.plugins.push(PluginSpec::from_path(plugin));
        self.save()?;
        Ok(true)
    }

    /// Remove every plugin matching `arg` (path, file stem, or title),
    /// persisting the manifest. Returns how many were removed.
    pub fn remove_plugin(&mut self, arg: &str) -> Result<usize, String> {
        let before = self.plugins.len();
        self.plugins.retain(|s| !s.matches(arg));
        let removed = before - self.plugins.len();
        if removed > 0 {
            self.save()?;
        }
        Ok(removed)
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

/// Print the project's plugins.
pub fn list() -> Result<(), String> {
    let project = Project::load()?;
    println!("{}", project.name);
    if project.plugins.is_empty() {
        println!("  (no plugins; add one with `wk add <path>`)");
    }
    for (i, spec) in project.plugins.iter().enumerate() {
        let mut line = format!("  [{i}] {}  {}", spec.label(), spec.path.display());
        if let Some((w, h)) = spec.size {
            line += &format!("  {w}x{h}");
        }
        println!("{line}");
    }
    Ok(())
}

/// Remove a plugin from the project manifest by path, file stem, or title.
pub fn remove(arg: String) -> Result<(), String> {
    let mut project = Project::load()?;
    match project.remove_plugin(&arg)? {
        0 => println!("no plugin matching {arg:?}"),
        n => println!("removed {n} plugin(s) matching {arg:?}"),
    }
    Ok(())
}
