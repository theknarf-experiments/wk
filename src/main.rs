use client_local_ui::WindowClient;
use wk_protocol::Client;
use wk_server::runtime::ServerRuntime;
use wk_server::workspace;
use wk_token_service::TokenService;

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
        Some(Commands::Run { headless }) => run(file, *headless),
        None => {
            Cli::command().print_help().map_err(|e| e.to_string())?;
            Ok(())
        }
    }
}

/// Open the given `.wk` workspace. The server runs independently on its own
/// thread; a windowed run attaches the local UI client, a headless run attaches
/// none and just keeps the server alive until Ctrl-C.
fn run(file: &Path, headless: bool) -> Result<(), String> {
    // Resolve `import`s into one merged document to run (the CLI edit commands
    // use the raw single-file `load` instead).
    let doc = workspace::Document::load_resolved(file)?;
    // Pull any OCI-artifact dependencies into the local cache before launching.
    for dep in &doc.dependencies {
        if let Err(e) = dep.ensure() {
            eprintln!("warning: dependency {:?} unavailable: {e}", dep.name);
        }
    }
    // Three-way auth split, wired up locally:
    //  1. the token service owns the signing keys and mints tokens;
    //  2. the server gets a copy of the public key and only verifies;
    //  3. the client is handed a minted token and bears it with every action.
    let tokens = TokenService::new();
    let runtime = ServerRuntime::spawn(&doc, file.to_path_buf(), tokens.public_key())?;
    if headless {
        // No client attached; run the server until Ctrl-C, then save + stop.
        runtime.block_until_ctrl_c();
        Ok(())
    } else {
        // Mint a full-authority token for the trusted local client and attach it
        // to the connection, then run the client on this (main) thread — winit
        // needs it.
        let token = tokens.mint_admin()?;
        let conn = runtime.handle().with_token(token);
        let result = Box::new(WindowClient).run(conn);
        // Window closed (or errored): stop the server, which persists the state.
        runtime.shutdown();
        result
    }
}
