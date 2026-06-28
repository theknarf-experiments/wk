//! The wk project model: a `wk.kdl` manifest in the working directory. It names
//! the project and lists its dependencies — plugins referenced by a short name
//! (npm-style), each resolved to a source: a local `.wasm` path, or an `oci://`
//! reference pulled from a registry as a Wasm OCI Artifact (see [`crate::oci`]).
//!
//! ```kdl
//! name "my-workspace"
//! dependencies {
//!     triangle "plugins/triangle/.../triangle.wasm"
//!     foo      "oci://ghcr.io/org/foo:1.0"
//! }
//! ```

use kdl::{KdlDocument, KdlEntry, KdlNode};
use std::path::{Path, PathBuf};

/// Manifest file name, looked up in the current directory.
pub const MANIFEST: &str = "wk.kdl";

/// Where a dependency's wasm comes from.
#[derive(Debug, Clone)]
pub enum Source {
    /// A local `.wasm` file.
    Path(PathBuf),
    /// An OCI registry reference (e.g. `ghcr.io/org/name:1.0`), pulled + cached.
    Oci(String),
}

impl Source {
    /// Parse the string form stored in wk.kdl (an `oci://` prefix means OCI).
    pub fn parse(s: &str) -> Self {
        match s.strip_prefix("oci://") {
            Some(reference) => Source::Oci(reference.to_string()),
            None => Source::Path(PathBuf::from(s)),
        }
    }

    /// The string written back to wk.kdl.
    pub fn to_kdl(&self) -> String {
        match self {
            Source::Path(p) => p.to_string_lossy().into_owned(),
            Source::Oci(reference) => format!("oci://{reference}"),
        }
    }

    /// The local path to load the wasm from. For OCI this is the cache location
    /// (which [`Source::ensure`] populates); it may not exist until then.
    pub fn local_path(&self) -> PathBuf {
        match self {
            Source::Path(p) => p.clone(),
            Source::Oci(reference) => crate::oci::cache_path(reference),
        }
    }

    /// Make the wasm available locally: pull + cache an OCI artifact if it isn't
    /// already cached. A no-op for local paths.
    pub fn ensure(&self) -> Result<(), String> {
        if let Source::Oci(reference) = self {
            let path = crate::oci::cache_path(reference);
            if !path.exists() {
                println!("pulling {reference} ...");
                let bytes = crate::oci::pull(reference)?;
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }
}

/// One project dependency: a short name resolving to a plugin source.
#[derive(Debug, Clone)]
pub struct Dependency {
    pub name: String,
    pub source: Source,
    /// Command-line arguments passed to the plugin (after argv[0] = name), e.g.
    /// a filename for an editor. Set in wk.kdl as `name "path" { args "..." }`.
    pub args: Vec<String>,
}

impl Dependency {
    pub fn from_path(source: PathBuf) -> Self {
        let name = source
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "plugin".to_string());
        Dependency {
            name,
            source: Source::Path(source),
            args: Vec::new(),
        }
    }

    /// The local path to load this dependency's wasm from.
    pub fn local_path(&self) -> PathBuf {
        self.source.local_path()
    }

    /// Pull + cache the dependency if it's an OCI artifact not yet local.
    pub fn ensure(&self) -> Result<(), String> {
        self.source.ensure()
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
                        let args = n
                            .children()
                            .and_then(|ch| ch.get("args"))
                            .map(|a| {
                                a.entries()
                                    .iter()
                                    .filter_map(|e| e.value().as_string().map(str::to_string))
                                    .collect()
                            })
                            .unwrap_or_default();
                        Some(Dependency {
                            name,
                            source: Source::parse(source),
                            args,
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
            node.push(KdlEntry::new(dep.source.to_kdl()));
            if !dep.args.is_empty() {
                let mut sub = KdlDocument::new();
                let mut args_node = KdlNode::new("args");
                for a in &dep.args {
                    args_node.push(KdlEntry::new(a.clone()));
                }
                sub.nodes_mut().push(args_node);
                node.set_children(sub);
            }
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

/// Add a plugin to the project as a dependency. `target` is a local `.wasm`
/// path or an `oci://<ref>` registry reference; the name is its file stem or the
/// OCI repository's last segment. An OCI artifact is pulled now to validate it.
pub fn add(target: String) -> Result<(), String> {
    let mut project = Project::load()?;
    let source = Source::parse(&target);
    let name = match &source {
        Source::Path(p) => p
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "plugin".to_string()),
        Source::Oci(reference) => crate::oci::name_for(reference),
    };
    source.ensure()?;
    let dep = Dependency {
        name: name.clone(),
        source,
        args: Vec::new(),
    };
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
        println!("  {}  {}", dep.name, dep.source.to_kdl());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_parse_and_roundtrip() {
        match Source::parse("oci://ghcr.io/org/foo:1.0") {
            Source::Oci(r) => assert_eq!(r, "ghcr.io/org/foo:1.0"),
            other => panic!("expected oci, got {other:?}"),
        }
        assert!(matches!(Source::parse("plugins/x.wasm"), Source::Path(_)));
        assert_eq!(
            Source::Oci("ghcr.io/o/f:1".into()).to_kdl(),
            "oci://ghcr.io/o/f:1"
        );
        assert_eq!(Source::Path("a/b.wasm".into()).to_kdl(), "a/b.wasm");
    }
}
