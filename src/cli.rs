//! The `wk` CLI as a *client* of a running server — wk's `docker`/`docker
//! compose` command surface. These subcommands don't start a server; they
//! connect to one already started by `wk run` (windowed or headless) over its
//! per-workspace Unix socket and drive it live.

use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::path::Path;

use wk_protocol::ipc::{read_msg, write_msg, ClientMsg, ServerMsg, Snapshot};
use wk_server::ipc_server::socket_path;

/// Connect to the running server for `workspace`, or a helpful error if none.
fn connect(workspace: &Path) -> Result<UnixStream, String> {
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
fn get_snapshot(stream: &mut UnixStream) -> Result<Snapshot, String> {
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
        let status = if !n.runnable {
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
