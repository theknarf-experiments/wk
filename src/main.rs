mod audio;
mod compositor;
mod host_shell;
mod midi;
mod plugin;
mod project;
mod render2d;
mod session;
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

    /// Add a plugin component to the project as a named dependency
    Add {
        /// Path to the plugin `.wasm` component (named after its file stem)
        plugin: PathBuf,
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
        Some(Commands::Add { plugin }) => project::add(plugin.clone()),
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
    compositor::run(&deps, project_mode)
}
