//! End-to-end: a client connects to a running server's Unix socket, reads a
//! snapshot, issues a command, and sees the effect — the CLI's core loop.

use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use wk_protocol::ipc::{read_msg, write_msg, ClientMsg, ServerMsg};
use wk_protocol::{Command, NodeKind, Resource};
use wk_server::ipc_server::{socket_path, IpcServer};
use wk_server::runtime::ServerRuntime;
use wk_server::workspace::Document;
use wk_token_service::TokenService;

fn snapshot(stream: &mut UnixStream) -> wk_protocol::ipc::Snapshot {
    write_msg(stream, &ClientMsg::GetSnapshot).unwrap();
    let mut r = BufReader::new(stream.try_clone().unwrap());
    match read_msg::<_, ServerMsg>(&mut r).unwrap().unwrap() {
        ServerMsg::Snapshot(s) => s,
        other => panic!("expected snapshot, got {other:?}"),
    }
}

#[test]
fn cli_reads_snapshot_and_applies_a_command() {
    // A minimal one-workspace document written to a temp file.
    let dir = std::env::temp_dir().join(format!("wk-ipc-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("workspace.wk");
    std::fs::write(
        &path,
        "// vim: set filetype=kdl :\nworkspace \"01KXKGZ000000000000000WS00\" {\n}\n",
    )
    .unwrap();

    let doc = Document::load_resolved(&path).unwrap();
    let tokens = TokenService::new();
    let runtime = ServerRuntime::spawn(&doc, path.clone(), tokens.public_key()).unwrap();
    let handle = runtime.handle().with_token(tokens.mint_admin().unwrap());
    let ipc = IpcServer::start(handle, &path).unwrap();

    // Connect the way the CLI does: compute the socket path from the workspace.
    let sock = socket_path(&path);
    let mut stream = UnixStream::connect(&sock).expect("connect to server socket");

    // The fresh workspace has no nodes; note its id for a create target.
    let snap = snapshot(&mut stream);
    assert_eq!(snap.nodes.len(), 0, "empty workspace");
    let ws = snap.workspaces[0];

    // Add a note node, exactly as the UI's palette would.
    write_msg(
        &mut stream,
        &ClientMsg::Command(Command::Create(Resource::Node {
            kind: NodeKind::Note,
            pos: [10.0, 20.0],
            ws,
        })),
    )
    .unwrap();
    let mut r = BufReader::new(stream.try_clone().unwrap());
    assert!(matches!(
        read_msg::<_, ServerMsg>(&mut r).unwrap().unwrap(),
        ServerMsg::Ok
    ));

    // The server applies commands on its tick; poll the snapshot until it lands.
    let deadline = Instant::now() + Duration::from_secs(3);
    let note = loop {
        let snap = snapshot(&mut stream);
        if let Some(n) = snap.nodes.iter().find(|n| n.kind == "note") {
            break n.clone();
        }
        assert!(Instant::now() < deadline, "note never appeared");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(note.pos, [10.0, 20.0]);
    assert_eq!(note.ws, ws);

    ipc.shutdown();
    runtime.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}
