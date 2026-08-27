//! The `wk` CLI as a *client* of a running server — wk's `docker`/`docker
//! compose` command surface. These subcommands don't start a server; they
//! connect to one already started by `wk run` (windowed or headless) over its
//! per-workspace Unix socket and drive it live.

use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::path::Path;

use wk_api::ipc::socket_path;
use wk_protocol::ipc::{read_msg, write_msg, ClientMsg, ServerMsg, Snapshot};
use wk_protocol::{Command, NodeKind, NodePatch, Resource, ResourceRef, ViewMode};

/// Connect to the running server for `workspace`, or a helpful error if none.
pub(crate) fn connect(workspace: &Path) -> Result<UnixStream, String> {
    let sock = socket_path(workspace);
    UnixStream::connect(&sock).map_err(|_| {
        format!(
            "no running wk server for {} — start one with `wk run{}`",
            workspace.display(),
            if workspace == Path::new(wk_server::workspace::DEFAULT_WORKSPACE) {
                String::new()
            } else {
                format!(" {}", workspace.display())
            }
        )
    })
}

/// Fetch a fresh snapshot from the connected server.
pub(crate) fn get_snapshot(stream: &mut UnixStream) -> Result<Snapshot, String> {
    write_msg(stream, &ClientMsg::GetSnapshot).map_err(|e| e.to_string())?;
    let mut r = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
    match read_msg::<_, ServerMsg>(&mut r).map_err(|e| e.to_string())? {
        Some(ServerMsg::Snapshot(s)) => Ok(s),
        Some(ServerMsg::Error(e)) => Err(e),
        other => Err(format!("unexpected reply: {other:?}")),
    }
}

/// A short, docker-style node id (first 12 chars of the 26-char ULID).
fn short(id: wk_protocol::NodeId) -> String {
    id.to_string().chars().take(12).collect()
}

/// Send one command and wait for the server's ack.
fn send_command(stream: &mut UnixStream, cmd: Command) -> Result<(), String> {
    write_msg(stream, &ClientMsg::Command(cmd)).map_err(|e| e.to_string())?;
    let mut r = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
    match read_msg::<_, ServerMsg>(&mut r).map_err(|e| e.to_string())? {
        Some(ServerMsg::Ok) => Ok(()),
        Some(ServerMsg::Error(e)) => Err(e),
        other => Err(format!("unexpected reply: {other:?}")),
    }
}

/// Resolve a user-supplied node reference to exactly one node. A reference
/// matches by node name, or by any substring of its id (so a short id prefix
/// *or* the distinguishing suffix both work). Ambiguous or absent → error.
pub(crate) fn resolve<'a>(
    snap: &'a Snapshot,
    query: &str,
) -> Result<&'a wk_protocol::ipc::NodeInfo, String> {
    let q = query.to_lowercase();
    let matches: Vec<_> = snap
        .nodes
        .iter()
        .filter(|n| {
            n.name.eq_ignore_ascii_case(query) || n.id.to_string().to_lowercase().contains(&q)
        })
        .collect();
    match matches.as_slice() {
        [one] => Ok(one),
        [] => Err(format!("no node matches {query:?} (see `wk ps`)")),
        many => Err(format!(
            "{query:?} is ambiguous — matches {} nodes: {}",
            many.len(),
            many.iter()
                .map(|n| short(n.id))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// `wk node add <name> [args...]`: launch a dependency as a new node.
pub fn add(workspace: &Path, name: &str, args: &[String]) -> Result<(), String> {
    let mut stream = connect(workspace)?;
    let snap = get_snapshot(&mut stream)?;
    let dep = snap
        .available
        .iter()
        .position(|d| d == name)
        .ok_or_else(|| {
            format!(
                "no dependency named {name:?}; available: {}",
                snap.available.join(", ")
            )
        })?;
    let ws = *snap.workspaces.first().ok_or("the workspace has no tabs")?;
    // Cascade new nodes so they don't stack exactly on top of each other.
    let n = snap.nodes.len() as f32;
    let pos = [60.0 + 24.0 * (n % 8.0), 60.0 + 24.0 * (n % 8.0)];
    send_command(
        &mut stream,
        Command::Create(Resource::Node {
            kind: NodeKind::App { dep },
            pos,
            ws,
        }),
    )?;
    // Set launch args, if given (a second command; empty falls back to default).
    if !args.is_empty() {
        // The new node's id isn't in our stale snapshot; re-fetch and target the
        // newest node of this dependency without args yet.
        let snap = get_snapshot(&mut stream)?;
        if let Some(node) = snap
            .nodes
            .iter()
            .rfind(|nd| nd.name == name && nd.args.is_empty())
        {
            send_command(
                &mut stream,
                Command::Update {
                    id: node.id,
                    patch: NodePatch {
                        args: Some(args.join(" ")),
                        ..Default::default()
                    },
                },
            )?;
        }
    }
    println!("added {name}");
    Ok(())
}

/// Fetch a node's effective capability token: (node id, token bytes).
fn node_token(workspace: &Path, node: &str) -> Result<(wk_protocol::NodeId, Vec<u8>), String> {
    let mut stream = connect(workspace)?;
    let snap = get_snapshot(&mut stream)?;
    let n = resolve(&snap, node)?;
    let hex = n.token.as_deref().ok_or_else(|| {
        format!(
            "node {} has no capability token ({})",
            short(n.id),
            if n.kind == "app" {
                "the server was started without a token service"
            } else {
                "only app nodes bear tokens"
            }
        )
    })?;
    let bytes = wk_server::workspace::hex_bytes(hex)
        .filter(|b| !b.is_empty())
        .ok_or("malformed token hex in the server snapshot")?;
    Ok((n.id, bytes))
}

/// `wk token show <ref>`: print a node's capability token — each Datalog block
/// (the authority policy, then any attenuations) and its hex form (the
/// currency `token set` accepts).
pub fn token_show(workspace: &Path, node: &str) -> Result<(), String> {
    let (id, bytes) = node_token(workspace, node)?;
    let tok =
        biscuit_auth::UnverifiedBiscuit::from(&bytes).map_err(|e| format!("parse token: {e}"))?;
    println!("node {}", short(id));
    for i in 0..tok.block_count() {
        let source = tok
            .print_block_source(i)
            .map_err(|e| format!("block {i}: {e}"))?;
        let label = if i == 0 { "authority" } else { "attenuation" };
        println!("block {i} ({label}):");
        for line in source.lines().filter(|l| !l.trim().is_empty()) {
            println!("  {}", line.trim());
        }
    }
    println!("hex: {}", wk_server::workspace::bytes_hex(&bytes));
    Ok(())
}

/// `wk token attenuate <ref> <block>`: append an attenuation block (Datalog
/// checks) to the node's current token and swap it in. Attenuation is offline —
/// it needs no signing key, only the token — and can only *narrow* what the
/// token grants. E.g. `'check if operation($k, $t), $k != "net"'` keeps a node
/// off every network while leaving its other wires working.
pub fn token_attenuate(workspace: &Path, node: &str, block: &str) -> Result<(), String> {
    let (id, bytes) = node_token(workspace, node)?;
    let appended = biscuit_auth::UnverifiedBiscuit::from(&bytes)
        .map_err(|e| format!("parse token: {e}"))?
        .append(
            biscuit_auth::builder::BlockBuilder::new()
                .code(block)
                .map_err(|e| format!("attenuation datalog: {e}"))?,
        )
        .map_err(|e| format!("append block: {e}"))?;
    let token = appended
        .to_vec()
        .map_err(|e| format!("serialize token: {e}"))?;
    let mut stream = connect(workspace)?;
    send_command(&mut stream, Command::SetToken { id, token })?;
    println!(
        "attenuated {} (token now has {} blocks)",
        short(id),
        appended.block_count()
    );
    Ok(())
}

/// `wk token set <ref> <hex>`: replace a node's token wholesale — e.g. one
/// minted with different authority logic, or a token saved from `token show`.
/// The server refuses tokens not signed by this workspace's token service.
pub fn token_set(workspace: &Path, node: &str, hex: &str) -> Result<(), String> {
    let token = wk_server::workspace::hex_bytes(hex.trim())
        .filter(|b| !b.is_empty())
        .ok_or("the token must be non-empty hex (as printed by `wk token show`)")?;
    let mut stream = connect(workspace)?;
    let snap = get_snapshot(&mut stream)?;
    let id = resolve(&snap, node)?.id;
    send_command(&mut stream, Command::SetToken { id, token })?;
    println!("set token on {}", short(id));
    Ok(())
}

/// `wk token reset <ref>`: return a node to the workspace's default token
/// ("a node may use what it is wired to").
pub fn token_reset(workspace: &Path, node: &str) -> Result<(), String> {
    let mut stream = connect(workspace)?;
    let snap = get_snapshot(&mut stream)?;
    let id = resolve(&snap, node)?.id;
    send_command(
        &mut stream,
        Command::SetToken {
            id,
            token: Vec::new(),
        },
    )?;
    println!("reset {} to the default token", short(id));
    Ok(())
}

/// `wk logs [-f] <ref>`: print a node's output log (non-destructive; doesn't
/// steal the live stream the way `attach` does). `follow` streams new output
/// until the node exits or Ctrl-C.
pub fn logs(workspace: &Path, node: &str, follow: bool) -> Result<(), String> {
    use std::io::Write;
    let mut stream = connect(workspace)?;
    let snap = get_snapshot(&mut stream)?;
    let id = resolve(&snap, node)?.id;
    write_msg(&mut stream, &ClientMsg::Logs { node: id, follow }).map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
    let mut stdout = std::io::stdout();
    loop {
        match read_msg::<_, ServerMsg>(&mut reader).map_err(|e| e.to_string())? {
            Some(ServerMsg::LogChunk(bytes)) => {
                let _ = stdout.write_all(&bytes);
                let _ = stdout.flush();
            }
            Some(ServerMsg::LogEnd) | None => break,
            Some(ServerMsg::Error(e)) => return Err(e),
            Some(_) => {}
        }
    }
    Ok(())
}

/// One connection of an inspected node: the wire kind and the peer it joins.
#[derive(serde::Serialize)]
struct Connection {
    kind: String,
    peer: String,
    peer_name: String,
}

/// A node's full detail, as `wk inspect` prints it (pretty JSON, docker-style).
#[derive(serde::Serialize)]
struct NodeReport<'a> {
    id: String,
    kind: &'a str,
    name: &'a str,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a str>,
    running: bool,
    runnable: bool,
    terminal: bool,
    attached: bool,
    args: &'a [String],
    /// An uplink's own dialable ticket — paste it into the remote side.
    #[serde(skip_serializing_if = "Option::is_none")]
    ticket: Option<&'a str>,
    /// An uplink's live peer count.
    #[serde(skip_serializing_if = "Option::is_none")]
    peers: Option<usize>,
    /// The node's address on its virtual network — what a peer dials when a
    /// name won't do (names don't resolve across an uplink).
    #[serde(skip_serializing_if = "Option::is_none")]
    ip: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ip6: Option<&'a str>,
    pos: [f32; 2],
    size: [f32; 2],
    workspace: String,
    connections: Vec<Connection>,
}

/// The first `n` characters of `s`, for long opaque strings (uplink tickets)
/// that would otherwise wrap a table. Char-wise, so it can't split a UTF-8
/// sequence.
fn ellipsis(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Status label for an inspected node, matching `wk ps`'s vocabulary.
fn status_of(n: &wk_protocol::ipc::NodeInfo) -> &'static str {
    if n.error.is_some() {
        "error"
    } else if n.compiling {
        "compiling"
    } else if n.attached {
        "attached"
    } else if !n.runnable {
        "-"
    } else if n.running {
        "running"
    } else {
        "idle"
    }
}

/// Build the detail report for one node, resolving its wires to the peer nodes.
fn node_report<'a>(snap: &'a Snapshot, node: &'a wk_protocol::ipc::NodeInfo) -> NodeReport<'a> {
    let name_of = |id| {
        snap.nodes
            .iter()
            .find(|n| n.id == id)
            .map(|n| n.name.clone())
            .unwrap_or_default()
    };
    let connections = snap
        .wires
        .iter()
        .filter(|w| w.a == node.id || w.b == node.id)
        .map(|w| {
            let peer = if w.a == node.id { w.b } else { w.a };
            Connection {
                kind: w.kind.clone(),
                peer: short(peer),
                peer_name: name_of(peer),
            }
        })
        .collect();
    NodeReport {
        id: node.id.to_string(),
        kind: &node.kind,
        name: &node.name,
        status: status_of(node),
        error: node.error.as_deref(),
        running: node.running,
        runnable: node.runnable,
        terminal: node.terminal,
        attached: node.attached,
        args: &node.args,
        ticket: node.ticket.as_deref(),
        peers: node.peers,
        ip: node.ip.as_deref(),
        ip6: node.ip6.as_deref(),
        pos: node.pos,
        size: node.size,
        workspace: short(node.ws),
        connections,
    }
}

/// `wk inspect <ref>`: print a node's or image's full detail as pretty JSON
/// (like `docker inspect`). An image tag or id/prefix in the local store wins;
/// anything else is resolved as a node against the running server.
pub fn inspect(workspace: &Path, target: &str) -> Result<(), String> {
    // Images are a local store (no server needed), so check them first — by tag
    // or by id/prefix.
    if let Some(id) = wk_server::images::resolve_ref(target) {
        if let Some(manifest) = wk_server::images::load_image(&id) {
            #[derive(serde::Serialize)]
            struct ImageReport<'a> {
                id: &'a str,
                tags: Vec<String>,
                #[serde(flatten)]
                manifest: &'a wk_server::images::ImageManifest,
            }
            let report = ImageReport {
                id: &id,
                tags: wk_server::images::tags_for(&id),
                manifest: &manifest,
            };
            println!(
                "{}",
                serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
            );
            return Ok(());
        }
    }
    // Otherwise it's a live node: resolve it against the running server.
    let mut stream = connect(workspace)?;
    let snap = get_snapshot(&mut stream)?;
    let node = resolve(&snap, target)?;
    let report = node_report(&snap, node);
    println!(
        "{}",
        serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
    );
    Ok(())
}

/// `wk node rm <ref>`: delete a node.
pub fn rm(workspace: &Path, node: &str) -> Result<(), String> {
    let mut stream = connect(workspace)?;
    let snap = get_snapshot(&mut stream)?;
    let target = resolve(&snap, node)?;
    let id = target.id;
    let label = short(id);
    send_command(&mut stream, Command::Delete(ResourceRef::Node(id)))?;
    println!("removed {label}");
    Ok(())
}

/// `wk node start <ref>`: (re)run an idle/exited node's guest.
pub fn start(workspace: &Path, node: &str) -> Result<(), String> {
    let mut stream = connect(workspace)?;
    let snap = get_snapshot(&mut stream)?;
    let id = resolve(&snap, node)?.id;
    send_command(&mut stream, Command::Run(id))?;
    println!("started {}", short(id));
    Ok(())
}

/// `wk stop <ref>`: stop a running node's guest (it stays placed, restartable).
pub fn stop(workspace: &Path, node: &str) -> Result<(), String> {
    let mut stream = connect(workspace)?;
    let snap = get_snapshot(&mut stream)?;
    let id = resolve(&snap, node)?.id;
    send_command(&mut stream, Command::Stop(id))?;
    println!("stopped {}", short(id));
    Ok(())
}

/// `wk view <2d|3d|toggle>`: switch every attached client between the flat
/// canvas and the 3D world — the palette's "3D View", from a shell. A headless
/// server takes the request too; the next client to attach inherits it.
pub fn view(workspace: &Path, mode: &str) -> Result<(), String> {
    let mode = ViewMode::parse(mode)
        .ok_or_else(|| format!("unknown view {mode:?} (expected 2d, 3d, or toggle)"))?;
    let mut stream = connect(workspace)?;
    send_command(&mut stream, Command::SetView(mode))?;
    println!(
        "view: {}",
        match mode {
            ViewMode::Flat => "2d",
            ViewMode::World => "3d",
            ViewMode::Toggle => "toggled",
        }
    );
    Ok(())
}

/// `wk restart <ref>`: stop the node, wait for it to exit, then start it again.
pub fn restart(workspace: &Path, node: &str) -> Result<(), String> {
    let mut stream = connect(workspace)?;
    let snap = get_snapshot(&mut stream)?;
    let id = resolve(&snap, node)?.id;
    send_command(&mut stream, Command::Stop(id))?;
    // The guest exits asynchronously; wait until it's idle before re-running
    // (Run is a no-op while it's still marked running).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
        let snap = get_snapshot(&mut stream)?;
        if snap
            .nodes
            .iter()
            .find(|n| n.id == id)
            .is_none_or(|n| !n.running)
        {
            break;
        }
    }
    send_command(&mut stream, Command::Run(id))?;
    println!("restarted {}", short(id));
    Ok(())
}

/// `wk down`: stop every running app node in the workspace (leaves them placed;
/// bring them back with `wk up`).
pub fn down(workspace: &Path) -> Result<(), String> {
    let mut stream = connect(workspace)?;
    let snap = get_snapshot(&mut stream)?;
    let mut n = 0;
    for node in snap.nodes.iter().filter(|n| n.running) {
        send_command(&mut stream, Command::Stop(node.id))?;
        n += 1;
    }
    println!("stopped {n} node(s)");
    Ok(())
}

/// `wk up`: start every idle/exited runnable app node in the workspace.
pub fn up(workspace: &Path) -> Result<(), String> {
    let mut stream = connect(workspace)?;
    let snap = get_snapshot(&mut stream)?;
    let mut n = 0;
    for node in snap.nodes.iter().filter(|n| n.runnable && !n.running) {
        send_command(&mut stream, Command::Run(node.id))?;
        n += 1;
    }
    println!("started {n} node(s)");
    Ok(())
}

/// `wk node set <ref> [--args "..."] [--host-path P]`: reconfigure a node's
/// launch args and/or (for a BindMount) the host file/folder it exposes.
#[allow(clippy::too_many_arguments)]
pub fn set_node(
    workspace: &Path,
    node: &str,
    args: Option<&str>,
    host_path: Option<&str>,
    persist: Option<bool>,
    port: Option<u16>,
) -> Result<(), String> {
    if args.is_none() && host_path.is_none() && persist.is_none() && port.is_none() {
        return Err("nothing to set — pass --args, --host-path, --persist, and/or --port".into());
    }
    let mut stream = connect(workspace)?;
    let snap = get_snapshot(&mut stream)?;
    let id = resolve(&snap, node)?.id;
    send_command(
        &mut stream,
        Command::Update {
            id,
            patch: NodePatch {
                args: args.map(str::to_string),
                host_path: host_path.map(str::to_string),
                persist,
                port_set: port,
                ..Default::default()
            },
        },
    )?;
    println!("updated {}", short(id));
    Ok(())
}

/// A short human label for a creatable node kind (for CLI output).
fn kind_label(kind: NodeKind) -> &'static str {
    match kind {
        NodeKind::App { .. } => "app",
        NodeKind::Volume => "volume",
        NodeKind::BindMount => "bindmount",
        NodeKind::Port => "hostport",
        NodeKind::Network => "network",
        NodeKind::Gateway => "gateway",
        NodeKind::Iroh => "iroh",
        NodeKind::Veilid => "veilid",
        NodeKind::Note => "note",
        NodeKind::Capture => "capture",
        NodeKind::Clipboard => "clipboard",
        NodeKind::Api => "api",
        NodeKind::MidiIn => "midiin",
        NodeKind::HostService => "hostservice",
    }
}

/// `wk create <kind> [value]`: create a non-app node headlessly. `value` seeds
/// the kind's key config — a bind's host path, a host port number, or a note's
/// text — mirroring how `wk node add` sets an app's args.
pub fn create(
    workspace: &Path,
    kind: NodeKind,
    value: Option<&str>,
    persist: bool,
) -> Result<(), String> {
    let mut stream = connect(workspace)?;
    let snap = get_snapshot(&mut stream)?;
    let ws = *snap.workspaces.first().ok_or("the workspace has no tabs")?;
    let before: std::collections::HashSet<_> = snap.nodes.iter().map(|n| n.id).collect();
    // Cascade so successive nodes don't stack exactly on top of each other.
    let n = snap.nodes.len() as f32;
    let pos = [60.0 + 24.0 * (n % 8.0), 60.0 + 24.0 * (n % 8.0)];
    send_command(
        &mut stream,
        Command::Create(Resource::Node { kind, pos, ws }),
    )?;

    // The create is applied asynchronously on the server's tick, so poll until
    // the new node (the id absent from the prior snapshot) appears before
    // seeding its config.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let id = loop {
        let snap = get_snapshot(&mut stream)?;
        if let Some(id) = snap
            .nodes
            .iter()
            .map(|n| n.id)
            .find(|id| !before.contains(id))
        {
            break id;
        }
        if std::time::Instant::now() >= deadline {
            println!("created a {} node", kind_label(kind));
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let patch = match kind {
        NodeKind::BindMount => NodePatch {
            host_path: value.map(str::to_string),
            ..Default::default()
        },
        NodeKind::Port => NodePatch {
            port_set: value
                .map(|v| v.parse::<u16>().map_err(|_| format!("invalid port {v:?}")))
                .transpose()?,
            ..Default::default()
        },
        NodeKind::Note => NodePatch {
            text: value.map(str::to_string),
            ..Default::default()
        },
        NodeKind::MidiIn => NodePatch {
            // A specific device name; None keeps the default the node opened.
            midi_device: value.map(str::to_string),
            ..Default::default()
        },
        NodeKind::Volume if persist => NodePatch {
            persist: Some(true),
            ..Default::default()
        },
        // `<name>=<addr:port>` sets both; a bare `addr:port` just the target.
        NodeKind::HostService => match value.map(|v| match v.split_once('=') {
            Some((name, target)) => (Some(name.to_string()), target.to_string()),
            None => (None, v.to_string()),
        }) {
            Some((name, target)) => NodePatch {
                service_name: name,
                service_target: Some(target),
                ..Default::default()
            },
            None => NodePatch::default(),
        },
        _ => NodePatch::default(),
    };
    // Only send a follow-up if there's actually something to configure.
    if !is_empty_patch(&patch) {
        send_command(&mut stream, Command::Update { id, patch })?;
    }
    println!("created {} ({})", short(id), kind_label(kind));
    Ok(())
}

/// Whether a patch would change nothing (all fields `None`/absent).
fn is_empty_patch(p: &NodePatch) -> bool {
    p.args.is_none()
        && p.host_path.is_none()
        && p.persist.is_none()
        && p.port_set.is_none()
        && p.port_delta.is_none()
        && p.text.is_none()
        && p.pos.is_none()
        && p.size.is_none()
        && p.service_name.is_none()
        && p.service_target.is_none()
}

/// `wk mount <volume> <app> [path]`: set where a volume bind mounts inside an
/// app (e.g. `/data/notes.txt`). Omitting the path resets it to the default
/// (the volume's name at the filesystem root).
pub fn mount(workspace: &Path, volume: &str, app: &str, path: &str) -> Result<(), String> {
    let mut stream = connect(workspace)?;
    let snap = get_snapshot(&mut stream)?;
    let vol = resolve(&snap, volume)?.id;
    let app_id = resolve(&snap, app)?.id;
    if !snap
        .wires
        .iter()
        .any(|w| w.kind == "bind" && w.a == vol && w.b == app_id)
    {
        return Err(format!(
            "{volume} is not bound into {app} — wire them first with `wk wire`"
        ));
    }
    send_command(
        &mut stream,
        Command::SetMount {
            volume: vol,
            app: app_id,
            path: path.to_string(),
        },
    )?;
    let where_ = if path.trim().is_empty() {
        "(default)"
    } else {
        path
    };
    println!("mounted {} into {} at {where_}", short(vol), short(app_id));
    Ok(())
}

/// `wk port <served> <hostport> <container>`: set the guest (container) port a
/// serve wire forwards to — the container side of a Docker `host:container` map
/// (the host side is the HostPort's own port). `0` resets to forward verbatim.
pub fn port(workspace: &Path, served: &str, hostport: &str, container: u16) -> Result<(), String> {
    let mut stream = connect(workspace)?;
    let snap = get_snapshot(&mut stream)?;
    let served_id = resolve(&snap, served)?.id;
    let hp_id = resolve(&snap, hostport)?.id;
    if !snap
        .wires
        .iter()
        .any(|w| w.kind == "serve" && w.a == served_id && w.b == hp_id)
    {
        return Err(format!(
            "{served} is not served on {hostport} — wire them first with `wk wire`"
        ));
    }
    send_command(
        &mut stream,
        Command::SetServePort {
            served: served_id,
            hostport: hp_id,
            container,
        },
    )?;
    if container == 0 {
        println!(
            "reset {} → {} to forward verbatim",
            short(served_id),
            short(hp_id)
        );
    } else {
        println!(
            "mapped {} → {} to container port {container}",
            short(served_id),
            short(hp_id)
        );
    }
    Ok(())
}

/// `wk wire <a> <b>`: connect two nodes (the server infers the wire kind).
pub fn wire(workspace: &Path, a: &str, b: &str) -> Result<(), String> {
    let mut stream = connect(workspace)?;
    let snap = get_snapshot(&mut stream)?;
    let (ida, idb) = (resolve(&snap, a)?.id, resolve(&snap, b)?.id);
    send_command(
        &mut stream,
        Command::Create(Resource::Wire { a: ida, b: idb }),
    )?;
    println!("wired {} <-> {}", short(ida), short(idb));
    Ok(())
}

/// `wk unwire <a> <b>`: remove the wire joining two nodes (either direction).
pub fn unwire(workspace: &Path, a: &str, b: &str) -> Result<(), String> {
    use wk_protocol::Wire;
    let mut stream = connect(workspace)?;
    let snap = get_snapshot(&mut stream)?;
    let (ida, idb) = (resolve(&snap, a)?.id, resolve(&snap, b)?.id);
    let w = snap
        .wires
        .iter()
        .find(|w| (w.a == ida && w.b == idb) || (w.a == idb && w.b == ida))
        .ok_or_else(|| format!("no wire between {a} and {b}"))?;
    let wire = match w.kind.as_str() {
        "bind" => Wire::Bind(w.a, w.b),
        "midi" => Wire::Midi(w.a, w.b),
        "serve" => Wire::Serve(w.a, w.b),
        "net" => Wire::Net(w.a, w.b),
        "capture" => Wire::Capture(w.a, w.b),
        "clipboard" => Wire::Clipboard(w.a, w.b),
        "api" => Wire::Api(w.a, w.b),
        other => return Err(format!("unknown wire kind {other:?}")),
    };
    send_command(&mut stream, Command::Delete(ResourceRef::Wire(wire)))?;
    println!("unwired {} <-> {}", short(ida), short(idb));
    Ok(())
}

/// `wk ps`: list the running workspace's nodes.
pub fn ps(workspace: &Path) -> Result<(), String> {
    let mut stream = connect(workspace)?;
    let snap = get_snapshot(&mut stream)?;
    if snap.nodes.is_empty() {
        println!("(no nodes; add one with `wk add <name>`)");
        return Ok(());
    }
    // Tab-separated columns, docker-ps-like.
    println!(
        "{:<12}  {:<11}  {:<16}  {:<9}  ARGS",
        "ID", "KIND", "NAME", "STATUS"
    );
    for n in &snap.nodes {
        let status = if n.error.is_some() {
            "error".to_string()
        } else if n.compiling {
            "compiling".to_string()
        } else if n.attached {
            "attached".to_string()
        } else if let Some(peers) = n.peers {
            // An uplink is never "runnable", so its connection state is the
            // only status worth showing (same vocabulary as the UI widget).
            match peers {
                0 if n.args.is_empty() => "no peer".to_string(),
                0 => "dialing".to_string(),
                p => format!("{p} peer(s)"),
            }
        } else if !n.runnable {
            "-".to_string()
        } else if n.running {
            "running".to_string()
        } else {
            "idle".to_string()
        };
        let name = if n.name.is_empty() { "-" } else { &n.name };
        // The trailing column carries the error message when there is one (a
        // HostPort has no args of its own), else the launch args. An uplink's
        // args are a ~200-char ticket, which would wrap the whole table — it
        // is elided here and printed in full by `wk inspect`.
        let detail = match (&n.error, n.peers.is_some()) {
            (Some(e), _) => e.clone(),
            (None, true) => match n.args.join(" ") {
                t if t.is_empty() => String::new(),
                t => format!("dials {}…", ellipsis(&t, 16)),
            },
            (None, false) => n.args.join(" "),
        };
        println!(
            "{:<12}  {:<11}  {:<16}  {:<9}  {}",
            short(n.id),
            n.kind,
            name,
            status,
            detail
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wk_protocol::ipc::NodeInfo;
    use wk_protocol::NodeId;

    fn node(id: u128, name: &str) -> NodeInfo {
        NodeInfo {
            id: NodeId::from_u128(id),
            kind: "app".into(),
            name: name.into(),
            ws: NodeId::from_u128(1),
            pos: [0.0, 0.0],
            size: [0.0, 0.0],
            args: vec![],
            running: false,
            runnable: true,
            terminal: false,
            attached: false,
            compiling: false,
            error: None,
            token: None,
            ticket: None,
            peers: None,
            ip: None,
            ip6: None,
        }
    }

    fn snap(nodes: Vec<NodeInfo>) -> Snapshot {
        Snapshot {
            workspaces: vec![NodeId::from_u128(1)],
            nodes,
            wires: vec![],
            available: vec![],
        }
    }

    #[test]
    fn resolve_matches_by_name_and_id_substring() {
        let piano = node(0xA1, "piano");
        let synth = node(0xB2, "synth");
        let s = snap(vec![piano.clone(), synth.clone()]);

        // By name.
        assert_eq!(resolve(&s, "piano").unwrap().id, piano.id);
        assert_eq!(
            resolve(&s, "SYNTH").unwrap().id,
            synth.id,
            "case-insensitive"
        );
        // By any substring of the id — a suffix works even when prefixes collide.
        let tail: String = piano.id.to_string().chars().rev().take(4).collect();
        let tail: String = tail.chars().rev().collect();
        assert_eq!(resolve(&s, &tail).unwrap().id, piano.id);
    }

    /// An uplink's ticket reaches `wk inspect`'s JSON in full — it is the one
    /// string the remote side cannot connect without, and scraping it out of
    /// the server's stderr used to be the only way to get it. Non-uplink
    /// nodes omit the field entirely rather than carrying a null.
    #[test]
    fn inspect_reports_an_uplink_ticket_and_peers() {
        let mut up = node(0xC3, "");
        up.kind = "iroh".into();
        up.runnable = false;
        up.ticket = Some("endpointabc123".into());
        up.peers = Some(2);
        let s = snap(vec![up.clone(), node(0xD4, "vim")]);

        let json = serde_json::to_string(&node_report(&s, &s.nodes[0])).unwrap();
        assert!(
            json.contains("endpointabc123"),
            "full ticket in output: {json}"
        );
        assert!(json.contains("\"peers\":2"), "{json}");

        // A plain app node carries neither field.
        let plain = serde_json::to_string(&node_report(&s, &s.nodes[1])).unwrap();
        assert!(!plain.contains("ticket"), "{plain}");
        assert!(!plain.contains("peers"), "{plain}");
    }

    /// A node's fabric address reaches `wk inspect`. It is the only way to
    /// address a node across an uplink — names are per-hub and don't resolve
    /// over a trunk — and deriving it from the node id by hand was previously
    /// the only option. A node with no fabric stack omits the fields rather
    /// than reporting a null address.
    #[test]
    fn inspect_reports_a_nodes_fabric_address() {
        let mut netserve = node(0xE5, "netserve");
        netserve.ip = Some("10.0.0.129".into());
        netserve.ip6 = Some("fd00::81".into());
        let s = snap(vec![netserve.clone(), node(0xF6, "note")]);

        let json = serde_json::to_string(&node_report(&s, &s.nodes[0])).unwrap();
        assert!(json.contains("10.0.0.129"), "{json}");
        assert!(json.contains("fd00::81"), "{json}");

        let plain = serde_json::to_string(&node_report(&s, &s.nodes[1])).unwrap();
        assert!(!plain.contains("\"ip\""), "{plain}");
        assert!(!plain.contains("\"ip6\""), "{plain}");
    }

    #[test]
    fn inspect_report_resolves_a_nodes_connections() {
        use wk_protocol::ipc::WireInfo;
        let mut vim = node(0xA1, "vim");
        vim.terminal = true;
        vim.running = true;
        let notes = node(0xB2, "notes.txt");
        let s = Snapshot {
            workspaces: vec![NodeId::from_u128(1)],
            nodes: vec![vim.clone(), notes.clone()],
            wires: vec![WireInfo {
                kind: "bind".into(),
                a: notes.id,
                b: vim.id,
            }],
            available: vec![],
        };
        let report = node_report(&s, &s.nodes[0]);
        assert_eq!(report.name, "vim");
        assert_eq!(report.status, "running");
        assert_eq!(report.connections.len(), 1);
        assert_eq!(report.connections[0].kind, "bind");
        assert_eq!(report.connections[0].peer_name, "notes.txt");
        // Serializes to JSON (what the command prints).
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("\"connections\""));
        assert!(json.contains(&vim.id.to_string()), "full id in output");
    }

    #[test]
    fn resolve_reports_ambiguous_and_absent() {
        // Two nodes share the zero-heavy prefix but differ by name.
        let s = snap(vec![node(0x100, "a"), node(0x200, "b")]);
        // A prefix common to both ids is ambiguous.
        let err = resolve(&s, "0000000000").unwrap_err();
        assert!(err.contains("ambiguous"), "{err}");
        assert!(resolve(&s, "nope").unwrap_err().contains("no node"));
    }
}
