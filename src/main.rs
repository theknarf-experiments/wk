mod compositor;
mod host_shell;
mod imguirenderer;
mod imguisdlhelper;
mod plugin;
mod project;

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

    /// Add a plugin component to the project
    Add {
        /// Path to the plugin `.wasm` component
        plugin: PathBuf,
    },

    /// Run the project's plugins, or explicit `.wasm` paths if given
    Run {
        /// Plugin `.wasm` paths; if omitted, the project's plugins are used
        plugins: Vec<PathBuf>,
    },
}

fn main() -> Result<(), String> {
    env_logger::init();

    let cli = Cli::parse();

    match &cli.command {
        Some(Commands::Init { name }) => project::init(name.clone()),
        Some(Commands::Add { plugin }) => project::add(plugin.clone()),
        // `wk run [paths...]` runs the project (or the given ad-hoc paths).
        Some(Commands::Run { plugins }) => run(plugins),
        // Bare `wk` shows help.
        None => {
            Cli::command().print_help().map_err(|e| e.to_string())?;
            Ok(())
        }
    }
}

/// Run the given plugin paths, or the project's plugins if none are given.
fn run(plugins: &[PathBuf]) -> Result<(), String> {
    let plugins = if plugins.is_empty() {
        project::Project::load()?.plugins
    } else {
        plugins.to_vec()
    };
    compositor::run(&plugins)
}
