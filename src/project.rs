//! The wk project model: a `wk.kdl` manifest in the working directory. It names
//! the project and lists its dependencies — plugins referenced by a short name
//! (npm-style), each resolved to a source. Today a source is a local `.wasm`
//! path; in future it may be a version fetched from a package manager.
//!
//! ```kdl
//! name "my-workspace"
//! dependencies {
//!     triangle "plugins/triangle/.../triangle.wasm"
//!     paint    "plugins/paint/.../paint.wasm"
//! }
//! ```

use kdl::{KdlDocument, KdlEntry, KdlNode};
use std::path::{Path, PathBuf};

/// Manifest file name, looked up in the current directory.
pub const MANIFEST: &str = "wk.kdl";

/// One project dependency: a short name resolving to a plugin source.
#[derive(Debug, Clone)]
pub struct Dependency {
    pub name: String,
    /// For now always a local `.wasm` path; later, possibly a package version.
    pub source: PathBuf,
}

impl Dependency {
    pub fn from_path(source: PathBuf) -> Self {
        let name = source
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "plugin".to_string());
        Dependency { name, source }
    }
}

#[derive(Debug)]
pub struct Project {
    pub name: String,
    pub dependencies: Vec<Dependency>,
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

        let dependencies = doc
            .get("dependencies")
            .and_then(|n| n.children())
            .map(|ch| {
                ch.nodes()
                    .iter()
                    .filter_map(|n| {
                        // Tolerate an npm-style trailing colon on the name.
                        let name = n.name().value().trim_end_matches(':').to_string();
                        let source = n.get(0).and_then(|v| v.as_string())?;
                        Some(Dependency {
                            name,
                            source: PathBuf::from(source),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(Project { name, dependencies })
    }

    /// Write the manifest back to the current directory.
    pub fn save(&self) -> Result<(), String> {
        let mut doc = KdlDocument::new();

        let mut name_node = KdlNode::new("name");
        name_node.push(KdlEntry::new(self.name.clone()));
        doc.nodes_mut().push(name_node);

        let mut deps = KdlNode::new("dependencies");
        let mut children = KdlDocument::new();
        for dep in &self.dependencies {
            let mut node = KdlNode::new(dep.name.clone());
            node.push(KdlEntry::new(dep.source.to_string_lossy().to_string()));
            children.nodes_mut().push(node);
        }
        deps.set_children(children);
        doc.nodes_mut().push(deps);

        doc.autoformat();
        std::fs::write(MANIFEST, doc.to_string())
            .map_err(|e| format!("failed to write {MANIFEST}: {e}"))
    }

    /// Add a dependency (idempotent by name), persisting the manifest. Returns
    /// `true` if newly added.
    pub fn add_dependency(&mut self, dep: Dependency) -> Result<bool, String> {
        if self.dependencies.iter().any(|d| d.name == dep.name) {
            return Ok(false);
        }
        self.dependencies.push(dep);
        self.save()?;
        Ok(true)
    }

    /// Remove a dependency by name, persisting the manifest. Returns how many
    /// were removed.
    pub fn remove_dependency(&mut self, name: &str) -> Result<usize, String> {
        let before = self.dependencies.len();
        self.dependencies.retain(|d| d.name != name);
        let removed = before - self.dependencies.len();
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
        dependencies: Vec::new(),
    };
    project.save()?;
    println!("created {MANIFEST}");
    Ok(())
}

/// Add a plugin to the project as a dependency (named after its file stem).
pub fn add(plugin: PathBuf) -> Result<(), String> {
    let mut project = Project::load()?;
    let dep = Dependency::from_path(plugin);
    let name = dep.name.clone();
    if project.add_dependency(dep)? {
        println!("added dependency: {name}");
    } else {
        println!("dependency already in project: {name}");
    }
    Ok(())
}

/// Print the project's dependencies.
pub fn list() -> Result<(), String> {
    let project = Project::load()?;
    println!("{}", project.name);
    if project.dependencies.is_empty() {
        println!("  (no dependencies; add one with `wk add <path>`)");
    }
    for dep in &project.dependencies {
        println!("  {}  {}", dep.name, dep.source.display());
    }
    Ok(())
}

/// Remove a dependency from the project by name.
pub fn remove(name: String) -> Result<(), String> {
    let mut project = Project::load()?;
    match project.remove_dependency(&name)? {
        0 => println!("no dependency named {name:?}"),
        n => println!("removed {n} dependency named {name:?}"),
    }
    Ok(())
}
