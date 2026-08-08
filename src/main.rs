use client_local_ui::WindowClient;
use wk_protocol::Client;
use wk_server::runtime::ServerRuntime;
use wk_server::workspace;
use wk_token_service::TokenService;

use clap::CommandFactory;
use clap::Parser;
use clap::Subcommand;
use std::path::{Path, PathBuf};

mod cli;

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

    /// List the nodes of a running workspace (connects to `wk run`)
    Ps,

    /// Manage nodes of a running workspace live (connects to `wk run`)
    Node {
        #[command(subcommand)]
        cmd: NodeCmd,
    },

    /// Connect two nodes in a running workspace (kind inferred)
    Wire {
        /// First node (name, or any part of its id)
        a: String,
        /// Second node
        b: String,
    },

    /// Remove the wire between two nodes in a running workspace
    Unwire { a: String, b: String },

    /// Manage wk's local OCI image store
    Images {
        #[command(subcommand)]
        cmd: ImagesCmd,
    },

    /// Open a workspace (default workspace.wk)
    Run {
        /// Workspace file to open. Overrides the global `--file`; defaults to
        /// `workspace.wk`. So `wk run example/live-coding.wk` just works.
        file: Option<PathBuf>,
        /// Run without a window: load and run the workspace, keep the guests
        /// alive, and exit on Ctrl-C. No rendering or OS input.
        #[arg(long)]
        headless: bool,
    },
}

#[derive(Subcommand)]
enum NodeCmd {
    /// Launch a dependency as a new node
    Add {
        /// Dependency name (see the workspace's `dependencies`)
        name: String,
        /// Launch args passed to the node
        args: Vec<String>,
    },
    /// Delete a node
    Rm {
        /// Node reference: its name, or any part of its id
        node: String,
    },
    /// (Re)start an idle/exited node's guest
    Start { node: String },
    /// Set a node's launch args
    Set {
        node: String,
        /// The full argument string (quote it)
        #[arg(long)]
        args: String,
    },
}

#[derive(Subcommand)]
enum ImagesCmd {
    /// List stored images (id, entrypoint, layers)
    List,
    /// Remove a stored image by id (layer tars stay; they're shared)
    Rm {
        /// Image id (sha256-<hex>)
        id: String,
    },
    /// Build a Dockerfile into the image store (wasm RUN steps execute)
    Build {
        /// Path to the Dockerfile (context = its directory)
        dockerfile: PathBuf,
    },
}

fn images_cmd(cmd: &ImagesCmd) -> Result<(), String> {
    use wk_server::images;
    match cmd {
        ImagesCmd::List => {
            let all = images::list_images();
            if all.is_empty() {
                println!("(no images; build one with `wk images build <Dockerfile>`)");
            }
            for (id, m) in all {
                println!(
                    "  {id}  entrypoint={}  layers={}",
                    m.entrypoint.join(" "),
                    m.layers.len()
                );
            }
            Ok(())
        }
        ImagesCmd::Rm { id } => {
            if images::remove_image(id) {
                println!("removed {id}");
                Ok(())
            } else {
                Err(format!("no image {id}"))
            }
        }
        ImagesCmd::Build { dockerfile } => {
            let id = images::build_and_alias(dockerfile)?;
            println!("built {} -> {id}", dockerfile.display());
            Ok(())
        }
    }
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
        Some(Commands::Ps) => cli::ps(file),
        Some(Commands::Node { cmd }) => match cmd {
            NodeCmd::Add { name, args } => cli::add(file, name, args),
            NodeCmd::Rm { node } => cli::rm(file, node),
            NodeCmd::Start { node } => cli::start(file, node),
            NodeCmd::Set { node, args } => cli::set_args(file, node, args),
        },
        Some(Commands::Wire { a, b }) => cli::wire(file, a, b),
        Some(Commands::Unwire { a, b }) => cli::unwire(file, a, b),
        Some(Commands::Images { cmd }) => images_cmd(cmd),
        Some(Commands::Remove { plugin }) => workspace::remove(plugin.clone(), file),
        Some(Commands::Run {
            file: run_file,
            headless,
        }) => run(run_file.as_deref().unwrap_or(file), *headless),
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
    // Start the CLI socket (wk's "docker daemon") so a separate `wk` process can
    // attach and drive this server live — for both windowed and headless runs.
    let _ipc = match tokens.mint_admin().and_then(|tok| {
        wk_server::ipc_server::IpcServer::start(runtime.handle().with_token(tok), file)
            .map_err(|e| e.to_string())
    }) {
        Ok(s) => {
            eprintln!("wk: CLI socket at {}", s.path().display());
            Some(s)
        }
        Err(e) => {
            eprintln!("wk: CLI socket unavailable: {e}");
            None
        }
    };
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
