mod arrows;
mod audio;
mod client;
mod compositor;
mod host_shell;
mod http;
mod midi;
mod netstack;
mod oci;
mod options;
mod plugin;
mod protocol;
mod render2d;
mod server;
mod sockets;
mod terminal;
mod text;
mod vfs;
mod workspace;

use clap::CommandFactory;
use clap::Parser;
use clap::Subcommand;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
#[command(propagate_version = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
    /// Workspace file to operate on (several `.wk` workspaces can share a
    /// directory). Defaults to `workspace.wk`.
    #[arg(
        short,
        long,
        global = true,
        default_value = workspace::DEFAULT_WORKSPACE
    )]
    file: PathBuf,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize a new wk workspace (creates workspace.wk)
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

    /// Open a workspace (default workspace.wk)
    Run {
        /// Run without a window: load and run the workspace, keep the guests
        /// alive, and exit on Ctrl-C. No rendering or OS input.
        #[arg(long)]
        headless: bool,
    },
}

fn main() -> Result<(), String> {
    env_logger::init();

    let cli = Cli::parse();

    let file = &cli.file;
    match &cli.command {
        Some(Commands::Init) => workspace::init(file),
        Some(Commands::Add { target }) => workspace::add(target.clone(), file),
        Some(Commands::Publish { plugin, reference }) => {
            workspace::publish(plugin.clone(), reference.clone(), file)
        }
        Some(Commands::List) => workspace::list(file),
        Some(Commands::Remove { plugin }) => workspace::remove(plugin.clone(), file),
        // `wk run [-f name.wk] [--headless]` opens the workspace.
        Some(Commands::Run { headless }) => run(file, *headless),
        // Bare `wk` shows help.
        None => {
            Cli::command().print_help().map_err(|e| e.to_string())?;
            Ok(())
        }
    }
}

/// Open the given `.wk` workspace with a window, or headless.
fn run(file: &Path, headless: bool) -> Result<(), String> {
    let ws = workspace::Workspace::load(file)?;
    // Pull any OCI-artifact dependencies into the local cache before launching.
    for dep in &ws.dependencies {
        if let Err(e) = dep.ensure() {
            eprintln!("warning: dependency {:?} unavailable: {e}", dep.name);
        }
    }
    // Build the authoritative half, then hand it to whichever client drives it.
    let server = server::Server::new(&ws, file.to_path_buf())?;
    let client: Box<dyn client::Client> = if headless {
        Box::new(client::HeadlessClient)
    } else {
        Box::new(compositor::WindowClient)
    };
    client.run(server)
}
