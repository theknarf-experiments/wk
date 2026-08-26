use client_local_ui::WindowClient;
use wk_protocol::{Client, NodeKind};
use wk_server::runtime::ServerRuntime;
use wk_server::workspace;
use wk_token_service::TokenService;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use clap::CommandFactory;
use clap::Parser;
use clap::Subcommand;
use std::path::{Path, PathBuf};

mod attach;
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

    /// Re-pull OCI dependencies from their registries (like `docker pull`):
    /// refreshes moved tags; unchanged content is a cheap no-op
    Pull {
        /// A dependency name or an `oci://<ref>`; omit to pull every
        /// `oci://` dependency of the workspace
        target: Option<String>,
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

    /// Create a non-app node headlessly: a volume, bind mount, host port,
    /// network, gateway, uplink, capture, api, or note (apps are `wk node add`)
    Create {
        /// What kind of node to create
        kind: CreateKind,
        /// Kind-specific value: a bind's host path, a port number, or a note's
        /// text (ignored for the others)
        value: Option<String>,
        /// For a volume: turn on persistence
        #[arg(long)]
        persist: bool,
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

    /// Set where a volume bind mounts inside an app (like docker `-v`)
    Mount {
        /// Volume node (name, or any part of its id)
        volume: String,
        /// App node the volume is bound into
        app: String,
        /// In-app mount path, e.g. /data/notes.txt (omit to reset to default)
        path: Option<String>,
    },

    /// Map a serve wire's guest port (the container side of `host:container`)
    Port {
        /// Served node (name, or any part of its id)
        served: String,
        /// HostPort node it's served on
        hostport: String,
        /// Guest/container port to forward to (0 = forward verbatim)
        container: u16,
    },

    /// Attach to a running terminal node's I/O (like `docker attach`)
    Attach {
        /// Node reference: its name, or any part of its id
        node: String,
    },

    /// Show a node's output log (like `docker logs`)
    Logs {
        /// Node reference: its name, or any part of its id
        node: String,
        /// Keep streaming new output until the node exits or Ctrl-C
        #[arg(long)]
        follow: bool,
    },

    /// Show a node's or image's full detail as JSON (like `docker inspect`)
    Inspect {
        /// A node reference (name / id part) or an image id in the local store
        target: String,
    },

    /// Stop a running node's guest (it stays placed; restart with `wk up`/start)
    Stop { node: String },

    /// Restart a node (stop, then start)
    Restart { node: String },

    /// Stop every running node in the workspace
    Down,

    /// Start every idle runnable node in the workspace
    Up,

    /// Manage wk's local OCI image store
    Images {
        #[command(subcommand)]
        cmd: ImagesCmd,
    },

    /// Hardware MIDI
    Midi {
        #[command(subcommand)]
        cmd: MidiCmd,
    },

    /// Inspect and swap node capability tokens (the Biscuit whose Datalog
    /// decides what a node's wires grant it)
    Token {
        #[command(subcommand)]
        cmd: TokenCmd,
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
    /// Reconfigure a node: launch args, a BindMount's host path, a Volume's
    /// persistence, or a HostPort's localhost port
    Set {
        node: String,
        /// The full argument string (quote it)
        #[arg(long)]
        args: Option<String>,
        /// For a BindMount: the host file or folder to expose
        #[arg(long)]
        host_path: Option<String>,
        /// For a Volume: persist its bytes across restarts (true/false)
        #[arg(long)]
        persist: Option<bool>,
        /// For a HostPort: set its localhost port
        #[arg(long)]
        port: Option<u16>,
    },
}

/// A node kind creatable headlessly with `wk create` (everything but an app,
/// which is `wk node add <dependency>`).
#[derive(Clone, Copy, clap::ValueEnum)]
enum CreateKind {
    Volume,
    Bind,
    Port,
    Network,
    Gateway,
    Iroh,
    Veilid,
    Capture,
    /// The HOST's system clipboard as a node: wire an app to it to let the app
    /// copy and paste. Read and write are separate token actions — see
    /// `wk token attenuate` in the Readme.
    Clipboard,
    /// The wk API as a node: wire an app to it to let the app drive wk
    Api,
    /// A hardware MIDI input node (value = device name; omit for the default)
    Midi,
    Note,
    /// A host TCP service published into a Network (value = `<name>=<addr:port>`,
    /// e.g. `subduction=127.0.0.1:8080`; a bare `addr:port` keeps the default name)
    Hostservice,
}

impl CreateKind {
    fn node_kind(self) -> NodeKind {
        match self {
            CreateKind::Volume => NodeKind::Volume,
            CreateKind::Bind => NodeKind::BindMount,
            CreateKind::Port => NodeKind::Port,
            CreateKind::Network => NodeKind::Network,
            CreateKind::Gateway => NodeKind::Gateway,
            CreateKind::Iroh => NodeKind::Iroh,
            CreateKind::Veilid => NodeKind::Veilid,
            CreateKind::Capture => NodeKind::Capture,
            CreateKind::Clipboard => NodeKind::Clipboard,
            CreateKind::Api => NodeKind::Api,
            CreateKind::Midi => NodeKind::MidiIn,
            CreateKind::Note => NodeKind::Note,
            CreateKind::Hostservice => NodeKind::HostService,
        }
    }
}

#[derive(Subcommand)]
enum MidiCmd {
    /// List connected hardware MIDI input devices (their port names)
    Devices,
}

#[derive(Subcommand)]
enum TokenCmd {
    /// Print a node's token: its Datalog blocks (authority policy, then any
    /// attenuations) and the hex form `set` accepts
    Show {
        /// Node reference: its name, or any part of its id
        node: String,
    },
    /// Append an attenuation block (Datalog checks) to a node's token — an
    /// offline, narrow-only permission change. Example:
    /// wk token attenuate vim 'check if operation($k, $t), $k != "net"'
    Attenuate {
        node: String,
        /// The block's Datalog (quote it)
        block: String,
    },
    /// Replace a node's token wholesale (hex from `show`, or an external mint
    /// signed by this workspace's token service)
    Set { node: String, hex: String },
    /// Reset a node to the workspace default: a node may use what it is wired to
    Reset { node: String },
    /// Mint a token with the given Datalog authority, signed by this
    /// workspace's key — for crafting test/scoped tokens. Prints hex for
    /// `wk token set`. Example: wk token mint 'right("document", "read");
    /// can_use($k,$t,$a) <- wired($k,$t), operation($k,$t,$a);'
    Mint {
        /// The authority block's Datalog (quote it)
        datalog: String,
    },
}

#[derive(Subcommand)]
enum ImagesCmd {
    /// List stored images (tags, id, entrypoint, layers)
    List,
    /// Remove a stored image by tag or id (layer tars stay; they're shared)
    Rm {
        /// Image reference: a tag (name:tag) or an id / id-prefix
        image: String,
    },
    /// Build a Dockerfile into the image store (wasm RUN steps execute)
    Build {
        /// Path to the Dockerfile (context = its directory)
        dockerfile: PathBuf,
        /// Name the built image (e.g. myapp:1.0); a bare name implies :latest
        #[arg(long)]
        tag: Option<String>,
        /// Allow build-time network so ADD <url> can fetch. Off by default
        /// (builds are otherwise hermetic).
        #[arg(long)]
        network: bool,
    },
    /// Name a stored image so it can be referenced as image://<tag>
    Tag {
        /// Existing image: a tag or an id / id-prefix
        image: String,
        /// New tag (e.g. myapp:1.0); a bare name implies :latest
        tag: String,
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
                let tags = images::tags_for(&id);
                let names = if tags.is_empty() {
                    "<none>".to_string()
                } else {
                    tags.join(", ")
                };
                println!(
                    "  {names}\n    {id}  entrypoint={}  layers={}",
                    m.entrypoint.join(" "),
                    m.layers.len()
                );
            }
            Ok(())
        }
        ImagesCmd::Rm { image } => {
            let id = images::resolve_ref(image).ok_or_else(|| format!("no image {image:?}"))?;
            images::remove_image(&id);
            println!("removed {id}");
            Ok(())
        }
        ImagesCmd::Build {
            dockerfile,
            tag,
            network,
        } => {
            let id = images::build_and_alias(dockerfile, *network)?;
            if let Some(tag) = tag {
                images::set_tag(tag, &id)?;
                println!(
                    "built {} -> {} ({})",
                    dockerfile.display(),
                    id,
                    images::normalize_tag(tag)
                );
            } else {
                println!("built {} -> {id}", dockerfile.display());
            }
            Ok(())
        }
        ImagesCmd::Tag { image, tag } => {
            let id = images::resolve_ref(image).ok_or_else(|| format!("no image {image:?}"))?;
            images::set_tag(tag, &id)?;
            println!("tagged {} as {}", id, images::normalize_tag(tag));
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
        Some(Commands::Pull { target }) => workspace::pull(target.clone(), file),
        Some(Commands::Publish { plugin, reference }) => {
            workspace::publish(plugin.clone(), reference.clone(), file)
        }
        Some(Commands::List) => workspace::list(file),
        Some(Commands::Ps) => cli::ps(file),
        Some(Commands::Node { cmd }) => match cmd {
            NodeCmd::Add { name, args } => cli::add(file, name, args),
            NodeCmd::Rm { node } => cli::rm(file, node),
            NodeCmd::Start { node } => cli::start(file, node),
            NodeCmd::Set {
                node,
                args,
                host_path,
                persist,
                port,
            } => cli::set_node(
                file,
                node,
                args.as_deref(),
                host_path.as_deref(),
                *persist,
                *port,
            ),
        },
        Some(Commands::Create {
            kind,
            value,
            persist,
        }) => cli::create(file, kind.node_kind(), value.as_deref(), *persist),
        Some(Commands::Wire { a, b }) => cli::wire(file, a, b),
        Some(Commands::Unwire { a, b }) => cli::unwire(file, a, b),
        Some(Commands::Mount { volume, app, path }) => {
            cli::mount(file, volume, app, path.as_deref().unwrap_or(""))
        }
        Some(Commands::Port {
            served,
            hostport,
            container,
        }) => cli::port(file, served, hostport, *container),
        Some(Commands::Attach { node }) => attach::attach(file, node),
        Some(Commands::Logs { node, follow }) => cli::logs(file, node, *follow),
        Some(Commands::Inspect { target }) => cli::inspect(file, target),
        Some(Commands::Stop { node }) => cli::stop(file, node),
        Some(Commands::Restart { node }) => cli::restart(file, node),
        Some(Commands::Down) => cli::down(file),
        Some(Commands::Up) => cli::up(file),
        Some(Commands::Images { cmd }) => images_cmd(cmd),
        Some(Commands::Midi { cmd }) => {
            let MidiCmd::Devices = cmd;
            let devices = wk_server::midihw::input_devices();
            if devices.is_empty() {
                println!("(no MIDI input devices connected)");
            } else {
                for d in devices {
                    println!("{d}");
                }
            }
            Ok(())
        }
        Some(Commands::Token { cmd }) => match cmd {
            TokenCmd::Show { node } => cli::token_show(file, node),
            TokenCmd::Attenuate { node, block } => cli::token_attenuate(file, node, block),
            TokenCmd::Set { node, hex } => cli::token_set(file, node, hex),
            TokenCmd::Reset { node } => cli::token_reset(file, node),
            TokenCmd::Mint { datalog } => {
                // Signs with the workspace's persisted key — no server needed.
                let svc = TokenService::load_or_create(&key_path(file));
                let token = svc.mint_code(datalog)?;
                println!("{}", wk_server::workspace::bytes_hex(&token));
                Ok(())
            }
        },
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

/// Where a workspace's token-service root key persists: beside the `.wk` file
/// (e.g. `workspace.wk` → `workspace.wk.key`), like the volume sidecar dir.
fn key_path(file: &Path) -> PathBuf {
    let mut s = file.to_path_buf().into_os_string();
    s.push(".key");
    PathBuf::from(s)
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
    // The root key persists beside the workspace (`<file>.key`) so node
    // capability tokens — minted per node, swappable via `wk token` — survive
    // restarts.
    let tokens = TokenService::load_or_create(&key_path(file));
    let node_base = tokens.mint_node_base()?;
    let runtime = ServerRuntime::spawn(&doc, file.to_path_buf(), tokens.public_key(), node_base)?;
    // Put the wk API on the fabric for nodes wired to an Api node: each
    // accepted connection is served by the same loop as the CLI socket, but
    // bearing the *node's own* capability token — so what a node may do over
    // the API is exactly what its token's Datalog grants.
    let api_base = runtime.handle();
    runtime.install_api_conn_server(std::sync::Arc::new(move |stream, token| {
        let handle = api_base.clone().with_token(token);
        std::thread::spawn(move || {
            let Ok(read_half) = stream.try_clone() else {
                return;
            };
            let reader = std::io::BufReader::new(read_half);
            let writer = std::sync::Arc::new(std::sync::Mutex::new(stream));
            let _ = wk_api::serve_client(handle, reader, writer);
        });
    }));
    // Start the CLI socket (wk's "docker daemon") so a separate `wk` process can
    // attach and drive this server live — for both windowed and headless runs.
    let _ipc = match tokens.mint_admin().and_then(|tok| {
        wk_api::ipc::IpcServer::start(runtime.handle().with_token(tok), file)
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
        // Ctrl-C interrupts the client loop instead of killing the process:
        // the loop exits, the shutdown below runs, and the workspace
        // persists. This is what makes the palette's "Go headless" stoppable
        // (its windowless loop has no other input), and it fixes windowed
        // Ctrl-C, which used to die without saving.
        let interrupt = Arc::new(AtomicBool::new(false));
        #[cfg(unix)]
        {
            static SIGINT_FLAG: OnceLock<Arc<AtomicBool>> = OnceLock::new();
            extern "C" fn on_sigint(_: libc::c_int) {
                if let Some(f) = SIGINT_FLAG.get() {
                    f.store(true, Ordering::Relaxed);
                }
            }
            let _ = SIGINT_FLAG.set(interrupt.clone());
            let handler = on_sigint as *const () as libc::sighandler_t;
            unsafe { libc::signal(libc::SIGINT, handler) };
        }
        let result = Box::new(WindowClient { interrupt }).run(conn);
        // Window closed (or errored): stop the server, which persists the state.
        runtime.shutdown();
        result
    }
}
