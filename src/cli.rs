//! The `wk` CLI as a *client* of a running server — wk's `docker`/`docker
//! compose` command surface. These subcommands don't start a server; they
//! connect to one already started by `wk run` (windowed or headless) over its
//! per-workspace Unix socket and drive it live.

use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::path::Path;

use wk_protocol::ipc::{read_msg, write_msg, ClientMsg, ServerMsg, Snapshot};
use wk_protocol::{Command, NodeKind, NodePatch, Resource, ResourceRef};
use wk_server::ipc_server::socket_path;

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

/// `wk node set <ref> --args "..."`: set a node's launch args.
pub fn set_args(workspace: &Path, node: &str, args: &str) -> Result<(), String> {
    let mut stream = connect(workspace)?;
    let snap = get_snapshot(&mut stream)?;
    let id = resolve(&snap, node)?.id;
    send_command(
        &mut stream,
        Command::Update {
            id,
            patch: NodePatch {
                args: Some(args.to_string()),
                ..Default::default()
            },
        },
    )?;
    println!("set args on {}", short(id));
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
        "file" => Wire::File(w.a, w.b),
        "midi" => Wire::Midi(w.a, w.b),
        "serve" => Wire::Serve(w.a, w.b),
        "net" => Wire::Net(w.a, w.b),
        "capture" => Wire::Capture(w.a, w.b),
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
        let status = if n.attached {
            "attached".to_string()
        } else if !n.runnable {
            "-".to_string()
        } else if n.running {
            "running".to_string()
        } else {
            "idle".to_string()
        };
        let name = if n.name.is_empty() { "-" } else { &n.name };
        println!(
            "{:<12}  {:<11}  {:<16}  {:<9}  {}",
            short(n.id),
            n.kind,
            name,
            status,
            n.args.join(" ")
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
