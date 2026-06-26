mod compositor;
mod example1;
mod host_shell;
mod imguirenderer;
mod imguisdlhelper;
mod plugin;

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
    /// Initialize new project
    Init {},

    /// Adds dependency
    Add {
        name: Option<String>,
    },

    Example1 {},

    /// Run one or more WASM plugin components, each composited in its own window
    Run {
        /// Paths to the plugin `.wasm` components
        #[arg(required = true)]
        plugins: Vec<PathBuf>,
    },
}

fn main() -> Result<(), String> {
    env_logger::init();

    let cli = Cli::parse();

    match &cli.command {
        Some(Commands::Init {}) => {
            println!("'init' was used");
            Ok(())
        }
        Some(Commands::Add { name }) => {
            println!("'add' was used, name is: {:?}", name);
            Ok(())
        }
        Some(Commands::Example1 {}) => example1(),
        Some(Commands::Run { plugins }) => compositor::run(plugins),
        None => {
            println!("Default subcommand");
            Ok(())
        }
    }
}
