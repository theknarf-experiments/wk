mod compositor;
mod example1;
mod host_shell;
mod imguirenderer;
mod imguisdlhelper;
mod plugin;
mod project;

use crate::example1::example1;
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
    /// Initialize a new wk project (creates wk.toml)
    Init {
        /// Project name (defaults to the directory name)
        name: Option<String>,
    },

    /// Add a plugin component to the project
    Add {
        /// Path to the plugin `.wasm` component
        plugin: PathBuf,
    },

    Example1 {},

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
        Some(Commands::Example1 {}) => example1(),
        Some(Commands::Run { plugins }) => {
            let plugins = if plugins.is_empty() {
                project::Project::load()?.plugins
            } else {
                plugins.clone()
            };
            compositor::run(&plugins)
        }
        None => {
            println!("Default subcommand");
            Ok(())
        }
    }
}
