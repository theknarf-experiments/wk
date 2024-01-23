mod imguisdlhelper;
mod imguirenderer;
mod example1;

use clap::Parser;
use clap::Subcommand;
use crate::example1::example1;

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
    Init { },

    /// Adds dependency
    Add { name: Option<String> },

    Example1 { },
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
        Some(Commands::Example1 {}) => {
            example1()
        }
        None => {
            println!("Default subcommand");
            Ok(())
        }
    }
}
