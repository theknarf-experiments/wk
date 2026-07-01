mod arrows;
mod audio;
mod compositor;
mod host_shell;
mod http;
mod midi;
mod netstack;
mod oci;
mod options;
mod plugin;
mod render2d;
mod sockets;
mod terminal;
mod text;
mod vfs;
mod workspace;

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
    /// Initialize a new wk workspace (creates wk.kdl)
    Init,

    /// Add a plugin to the workspace as a named dependency
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
        Some(Commands::Init) => workspace::init(),
        Some(Commands::Add { target }) => workspace::add(target.clone()),
        Some(Commands::Publish { plugin, reference }) => {
            workspace::publish(plugin.clone(), reference.clone())
        }
        Some(Commands::List) => workspace::list(),
        Some(Commands::Remove { plugin }) => workspace::remove(plugin.clone()),
        // `wk run [paths...]` opens the workspace (or the given ad-hoc paths).
        Some(Commands::Run { plugins }) => run(plugins),
        // Bare `wk` shows help.
        None => {
            Cli::command().print_help().map_err(|e| e.to_string())?;
            Ok(())
        }
    }
}

/// Open the workspace (or an ad-hoc set of `.wasm` paths).
fn run(plugins: &[PathBuf]) -> Result<(), String> {
    // Workspace mode (no explicit paths) persists the canvas; ad-hoc mode doesn't.
    let workspace_mode = plugins.is_empty();
    let ws = if workspace_mode {
        workspace::Workspace::load()?
    } else {
        workspace::Workspace::from_paths(plugins)
    };
    // Pull any OCI-artifact dependencies into the local cache before launching.
    for dep in &ws.dependencies {
        if let Err(e) = dep.ensure() {
            eprintln!("warning: dependency {:?} unavailable: {e}", dep.name);
        }
    }
    compositor::run(ws, workspace_mode)
}
