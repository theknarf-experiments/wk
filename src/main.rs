mod audio;
mod compositor;
mod host_shell;
mod http;
mod midi;
mod netstack;
mod oci;
mod options;
mod plugin;
mod project;
mod render2d;
mod session;
mod sockets;
mod terminal;
mod text;
mod vfs;

use clap::CommandFactory;
use clap::Parser;
use clap::Subcommand;
use std::path::PathBuf;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
#[command(propagate_version = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize a new wk project (creates wk.kdl)
    Init {
        /// Project name (defaults to the directory name)
        name: Option<String>,
    },

    /// Add a plugin to the project as a named dependency
    Add {
        /// A local `.wasm` path, or an `oci://<ref>` registry artifact
        target: String,
    },

    /// Publish a plugin to an OCI registry as a Wasm OCI Artifact
    Publish {
        /// Dependency name or local `.wasm` path
        plugin: String,
        /// Target OCI reference, e.g. localhost:5000/triangle:1.0
        reference: String,
    },

    /// List the project's dependencies
    List,

    /// Remove a dependency from the project (by name)
    Remove {
        /// Dependency name
        plugin: String,
    },

    /// Run the project's dependencies, or explicit `.wasm` paths if given
    Run {
        /// Plugin `.wasm` paths; if omitted, the project's dependencies are used
        plugins: Vec<PathBuf>,
    },
}

fn main() -> Result<(), String> {
    env_logger::init();

    let cli = Cli::parse();

    match &cli.command {
        Some(Commands::Init { name }) => project::init(name.clone()),
        Some(Commands::Add { target }) => project::add(target.clone()),
        Some(Commands::Publish { plugin, reference }) => {
            project::publish(plugin.clone(), reference.clone())
        }
        Some(Commands::List) => project::list(),
        Some(Commands::Remove { plugin }) => project::remove(plugin.clone()),
        // `wk run [paths...]` runs the project (or the given ad-hoc paths).
        Some(Commands::Run { plugins }) => run(plugins),
        // Bare `wk` shows help.
        None => {
            Cli::command().print_help().map_err(|e| e.to_string())?;
            Ok(())
        }
    }
}

/// Run the project's dependencies, or the given ad-hoc `.wasm` paths.
fn run(plugins: &[PathBuf]) -> Result<(), String> {
    // Project mode (no explicit paths) persists the workspace session.
    let project_mode = plugins.is_empty();
    let deps = if project_mode {
        project::Project::load()?.dependencies
    } else {
        plugins
            .iter()
            .cloned()
            .map(project::Dependency::from_path)
            .collect()
    };
    // Pull any OCI-artifact dependencies into the local cache before launching.
    for dep in &deps {
        if let Err(e) = dep.ensure() {
            eprintln!("warning: dependency {:?} unavailable: {e}", dep.name);
        }
    }
    compositor::run(&deps, project_mode)
}
