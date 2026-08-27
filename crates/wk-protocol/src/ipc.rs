//! The **network wire** between a running wk server and an attached CLI/TUI
//! client — the "docker daemon socket" of wk.
//!
//! A server started by `wk run` (windowed or headless) listens on a per-
//! workspace Unix socket; a separate `wk` process connects and drives it live.
//! Two message streams cross the socket, framed as newline-delimited JSON
//! (debuggable, and each message is one line):
//!
//! - [`ClientMsg`]: what a client asks — read a [`Snapshot`], apply a
//!   [`Command`] (the same vocabulary the UI uses), or attach to a node's
//!   terminal and stream its I/O.
//! - [`ServerMsg`]: the replies — a snapshot, an ack/error, or terminal bytes
//!   while attached.
//!
//! [`Snapshot`] is a plain-data projection of the server's live view: unlike the
//! server's internal `View` (which holds shared runtime handles), it is
//! serializable, so a remote client can list and target nodes without any
//! server types.

use std::io::{self, BufRead, Write};

use serde::{Deserialize, Serialize};

use crate::{Command, NodeId};

/// One node as seen over the wire — enough for a CLI to list and target it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NodeInfo {
    pub id: NodeId,
    /// `app` | `volume` | `bindmount` | `hostport` | `network` | `gateway`
    /// | `iroh` | `veilid` | `note` | `capture` | `api`.
    pub kind: String,
    /// The dependency/app name, file name, or note preview — else empty.
    pub name: String,
    /// Which workspace (tab) the node belongs to.
    pub ws: NodeId,
    pub pos: [f32; 2],
    pub size: [f32; 2],
    /// Launch args (app nodes), argv after the program name.
    pub args: Vec<String>,
    /// An app node with a live guest.
    pub running: bool,
    /// An app node whose component is still compiling (not yet runnable). A run
    /// requested now is deferred and applied automatically once it's ready.
    #[serde(default)]
    pub compiling: bool,
    /// An app node that can be (re)started.
    pub runnable: bool,
    /// A terminal (wasi:cli) node a client may `attach` to.
    pub terminal: bool,
    /// A terminal node currently attached by a CLI client.
    pub attached: bool,
    /// A problem the server wants surfaced for this node — e.g. a HostPort whose
    /// localhost port is already in use. `None` when healthy.
    #[serde(default)]
    pub error: Option<String>,
    /// An app node's *effective* capability token (custom or the workspace
    /// default), hex-encoded — what `wk token show/attenuate` operate on.
    /// `None` for non-app nodes (or a server without node auth configured).
    #[serde(default)]
    pub token: Option<String>,
    /// An uplink node's own dialable ticket — the string the *remote* side
    /// pastes to reach this fabric. `None` for every other kind. (The ticket
    /// it dials, if any, is its `args`.)
    #[serde(default)]
    pub ticket: Option<String>,
    /// An uplink node's live peer-connection count. `None` for other kinds.
    #[serde(default)]
    pub peers: Option<usize>,
    /// The node's address on its virtual network, e.g. `10.0.0.7` — what a
    /// peer dials when a name won't do (names are per-hub, so they do not
    /// resolve across an uplink). `None` until the node has a fabric stack:
    /// only a compiled component importing `wasi:sockets` gets one.
    #[serde(default)]
    pub ip: Option<String>,
    /// The same node's fabric IPv6 address (`fd00::7`), derived from `ip`'s
    /// host octet so the two stay in lock-step.
    #[serde(default)]
    pub ip6: Option<String>,
}

/// One wire between two nodes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WireInfo {
    /// `bind` | `midi` | `serve` | `net` | `capture` | `api`.
    pub kind: String,
    pub a: NodeId,
    pub b: NodeId,
}

/// A serializable projection of the server's state for a remote client.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    /// The workspaces (tabs), in order; the first is the default target.
    pub workspaces: Vec<NodeId>,
    /// What each named workspace is called, keyed by its id. A map rather than a
    /// list parallel to `workspaces`, so nothing here can drift out of step with
    /// the tab order; unnamed workspaces simply have no entry.
    #[serde(default)]
    pub workspace_names: std::collections::HashMap<NodeId, String>,
    pub nodes: Vec<NodeInfo>,
    pub wires: Vec<WireInfo>,
    /// The launchable dependency names, for `wk add <name>`.
    pub available: Vec<String>,
}

/// A message from a client to the server.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ClientMsg {
    /// Request the current [`Snapshot`].
    GetSnapshot,
    /// Apply a mutation — the same [`Command`] the UI issues.
    Command(Command),
    /// Attach to a node's terminal; the server replies [`ServerMsg::Attached`]
    /// then streams [`ServerMsg::Term`] until [`ClientMsg::Detach`].
    Attach { node: NodeId },
    /// Terminal input bytes (while attached).
    Input(Vec<u8>),
    /// The client terminal was resized (while attached).
    Resize { cols: u16, rows: u16 },
    /// Stop attaching (the node keeps running).
    Detach,
    /// Read a node's output log (a non-destructive scrollback). `follow` keeps
    /// the connection open, streaming new output as it arrives.
    Logs { node: NodeId, follow: bool },
}

/// A message from the server to a client.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ServerMsg {
    Snapshot(Snapshot),
    /// A command was accepted.
    Ok,
    /// A request failed (bad target, unauthorized, not a terminal, ...).
    Error(String),
    /// Attach succeeded; the node's current terminal size.
    Attached {
        cols: u16,
        rows: u16,
    },
    /// Terminal output bytes from the attached node.
    Term(Vec<u8>),
    /// The attach ended (the node exited, or the client detached).
    Detached,
    /// A chunk of a node's output log (in response to [`ClientMsg::Logs`]).
    LogChunk(Vec<u8>),
    /// End of the log stream (a non-following `Logs` request is complete).
    LogEnd,
}

/// Write one message as a single JSON line. The newline frames it, so the peer
/// reads with [`read_msg`].
pub fn write_msg<W: Write, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    let mut line = serde_json::to_vec(msg)?;
    line.push(b'\n');
    w.write_all(&line)?;
    w.flush()
}

/// Read one newline-framed JSON message, or `None` at end of stream.
pub fn read_msg<R: BufRead, T: for<'de> Deserialize<'de>>(r: &mut R) -> io::Result<Option<T>> {
    let mut line = String::new();
    if r.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let msg = serde_json::from_str(line.trim_end())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(Some(msg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NodeKind, NodePatch, Resource, ResourceRef, Wire};

    fn id(n: u128) -> NodeId {
        NodeId::from_u128(n)
    }

    /// Every command shape survives a JSON round-trip (the CLI serializes these).
    #[test]
    fn commands_round_trip() {
        let cmds = vec![
            Command::Create(Resource::Node {
                kind: NodeKind::App { dep: 3 },
                pos: [12.5, -4.0],
                ws: id(1),
            }),
            Command::Create(Resource::Wire { a: id(2), b: id(3) }),
            Command::Update {
                id: id(4),
                patch: NodePatch {
                    args: Some("-u NONE".into()),
                    text: Some("hi".into()),
                    ..Default::default()
                },
            },
            Command::Delete(ResourceRef::Wire(Wire::Midi(id(5), id(6)))),
            Command::Run(id(7)),
            Command::Undo,
        ];
        for c in cmds {
            let line = serde_json::to_string(&c).unwrap();
            let back: Command = serde_json::from_str(&line).unwrap();
            assert_eq!(format!("{c:?}"), format!("{back:?}"));
        }
    }

    /// A snapshot round-trips, and node ids survive as their 26-char text.
    #[test]
    fn snapshot_round_trips() {
        let snap = Snapshot {
            workspaces: vec![id(10)],
            workspace_names: std::collections::HashMap::from([(id(10), "voice".to_string())]),
            nodes: vec![NodeInfo {
                id: id(11),
                kind: "app".into(),
                name: "vim".into(),
                ws: id(10),
                pos: [1.0, 2.0],
                size: [400.0, 300.0],
                args: vec!["notes.txt".into()],
                running: true,
                compiling: false,
                runnable: false,
                terminal: true,
                attached: false,
                error: None,
                token: None,
                ticket: None,
                peers: None,
                ip: None,
                ip6: None,
            }],
            wires: vec![WireInfo {
                kind: "file".into(),
                a: id(12),
                b: id(11),
            }],
            available: vec!["vim".into(), "triangle".into()],
        };
        let line = serde_json::to_string(&snap).unwrap();
        assert!(line.contains(&id(11).to_string()), "ids as ULID text");
        let back: Snapshot = serde_json::from_str(&line).unwrap();
        assert_eq!(back.nodes.len(), 1);
        assert_eq!(back.nodes[0].name, "vim");
        assert_eq!(back.available, vec!["vim", "triangle"]);
        assert_eq!(
            back.workspace_names.get(&id(10)).map(String::as_str),
            Some("voice")
        );

        // Additive field: a snapshot from a peer that predates workspace names
        // still deserializes, with no names rather than an error.
        let older = line.replace(
            &format!(r#""workspace_names":{{"{}":"voice"}},"#, id(10)),
            "",
        );
        assert!(!older.contains("workspace_names"), "removed from: {older}");
        let back: Snapshot = serde_json::from_str(&older).unwrap();
        assert!(back.workspace_names.is_empty());
    }

    /// The framing is length-independent: multiple messages on one stream read
    /// back one at a time, in order.
    #[test]
    fn framing_reads_one_message_per_line() {
        let mut buf: Vec<u8> = Vec::new();
        write_msg(&mut buf, &ClientMsg::GetSnapshot).unwrap();
        write_msg(&mut buf, &ClientMsg::Attach { node: id(9) }).unwrap();
        write_msg(&mut buf, &ClientMsg::Detach).unwrap();

        let mut r = std::io::BufReader::new(&buf[..]);
        let msgs: Vec<ClientMsg> = std::iter::from_fn(|| read_msg(&mut r).unwrap()).collect();
        assert_eq!(msgs.len(), 3);
        assert!(matches!(msgs[0], ClientMsg::GetSnapshot));
        assert!(matches!(msgs[1], ClientMsg::Attach { .. }));
        assert!(matches!(msgs[2], ClientMsg::Detach));
    }
}
