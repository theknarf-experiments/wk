//! Workspace session: the runtime layout of a wk workspace — the canvas camera,
//! which nodes (app instances and file nodes) were open and where, and the
//! connections wiring them — persisted to `wk.session.kdl` so a `wk run`
//! restores where you left off. Local runtime state, git-ignored.
//!
//! ```kdl
//! camera { pan 0 0; zoom 1 }
//! node "synth" 1 { pos 19 88; size 360 260; options 0.3 2.0 0.0 1800.0 3.0 0.02 0.35 }
//! virtualfile "chan" 2 { pos 400 120; size 130 44 }
//! hostfile "notes.txt" 6 { pos 400 200; size 130 44 }
//! connection 2 1
//! midi 3 4
//! hostport 5 { port 8080; pos 600 100; size 130 44 }
//! serve 1 5
//! network 7 { pos 700 100; size 130 44 }
//! gateway 8 { pos 700 200; size 130 44 }
//! netlink 1 7
//! ```

use kdl::{KdlDocument, KdlEntry, KdlNode, KdlValue};

/// Session file name, alongside `wk.kdl` in the working directory.
pub const SESSION: &str = "wk.session.kdl";

/// A placed node: an app instance (`node`) or a file node (`file`).
pub struct SessionNode {
    /// Dependency name (for app nodes) or file name (for file nodes).
    pub name: String,
    pub id: u64,
    pub pos: [f32; 2],
    pub size: [f32; 2],
    /// App-node option values (e.g. knob settings), persisted positionally.
    /// Empty for file nodes (and app nodes that report none).
    pub options: Vec<f32>,
    /// App-node launch args (e.g. a client's target host/port), editable in the
    /// GUI. Empty for file nodes and nodes left at their dependency default.
    pub args: Vec<String>,
}

/// A HostPort node: a localhost port plus its canvas placement.
pub struct SessionPort {
    pub id: u64,
    pub port: u16,
    pub pos: [f32; 2],
    pub size: [f32; 2],
}

/// A Network (or Gateway) node and its canvas placement.
pub struct SessionNet {
    pub id: u64,
    pub gateway: bool,
    pub pos: [f32; 2],
    pub size: [f32; 2],
}

pub struct Session {
    /// Canvas camera: pan x, pan y, zoom.
    pub camera: (f32, f32, f32),
    pub nodes: Vec<SessionNode>,
    /// In-memory VirtualFile nodes; `name` holds the mount name.
    pub virtual_files: Vec<SessionNode>,
    /// HostMappedFile nodes; `name` holds the host file path.
    pub host_files: Vec<SessionNode>,
    /// HostPort nodes (localhost port + canvas placement).
    pub host_ports: Vec<SessionPort>,
    /// File connections as (file id, app node id).
    pub connections: Vec<(u64, u64)>,
    /// MIDI connections as (source node id, destination node id).
    pub midi: Vec<(u64, u64)>,
    /// Serve wiring as (wasi:http node id, HostPort id).
    pub serves: Vec<(u64, u64)>,
    /// Network/Gateway nodes.
    pub nets: Vec<SessionNet>,
    /// Network membership wiring as (app node id, Network/Gateway node id).
    pub net_links: Vec<(u64, u64)>,
}

fn num(v: &KdlValue) -> Option<f32> {
    v.as_float()
        .map(|f| f as f32)
        .or_else(|| v.as_integer().map(|i| i as f32))
}

fn uint(v: &KdlValue) -> Option<u64> {
    v.as_integer().map(|i| i as u64)
}

/// Parse a `node`/`file` entry: `<kind> "<name>" <id> { pos x y; size w h }`.
fn parse_placed(n: &KdlNode) -> Option<SessionNode> {
    let name = n.get(0)?.as_string()?.to_string();
    let id = uint(n.get(1)?)?;
    let ch = n.children()?;
    let pos = ch.get("pos")?;
    let size = ch.get("size")?;
    let options = ch
        .get("options")
        .map(|o| o.entries().iter().filter_map(|e| num(e.value())).collect())
        .unwrap_or_default();
    let args = ch
        .get("args")
        .map(|a| {
            a.entries()
                .iter()
                .filter_map(|e| e.value().as_string().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    Some(SessionNode {
        name,
        id,
        pos: [pos.get(0).and_then(num)?, pos.get(1).and_then(num)?],
        size: [size.get(0).and_then(num)?, size.get(1).and_then(num)?],
        options,
        args,
    })
}

/// Parse a `hostport <id> { port <p>; pos x y; size w h }` entry.
fn parse_hostport(n: &KdlNode) -> Option<SessionPort> {
    let id = uint(n.get(0)?)?;
    let ch = n.children()?;
    let port = ch.get("port").and_then(|p| p.get(0)).and_then(uint)? as u16;
    let pos = ch.get("pos")?;
    let size = ch.get("size")?;
    Some(SessionPort {
        id,
        port,
        pos: [pos.get(0).and_then(num)?, pos.get(1).and_then(num)?],
        size: [size.get(0).and_then(num)?, size.get(1).and_then(num)?],
    })
}

fn hostport_kdl(p: &SessionPort) -> KdlNode {
    let mut node = KdlNode::new("hostport");
    node.push(KdlEntry::new(p.id as i128));
    let mut ch = KdlDocument::new();
    let mut port = KdlNode::new("port");
    port.push(KdlEntry::new(p.port as i128));
    ch.nodes_mut().push(port);
    ch.nodes_mut().push(node2("pos", p.pos[0], p.pos[1]));
    ch.nodes_mut().push(node2("size", p.size[0], p.size[1]));
    node.set_children(ch);
    node
}

/// Parse a `network`/`gateway <id> { pos x y; size w h }` entry.
fn parse_net(n: &KdlNode, gateway: bool) -> Option<SessionNet> {
    let id = uint(n.get(0)?)?;
    let ch = n.children()?;
    let pos = ch.get("pos")?;
    let size = ch.get("size")?;
    Some(SessionNet {
        id,
        gateway,
        pos: [pos.get(0).and_then(num)?, pos.get(1).and_then(num)?],
        size: [size.get(0).and_then(num)?, size.get(1).and_then(num)?],
    })
}

fn net_kdl(n: &SessionNet) -> KdlNode {
    let mut node = KdlNode::new(if n.gateway { "gateway" } else { "network" });
    node.push(KdlEntry::new(n.id as i128));
    let mut ch = KdlDocument::new();
    ch.nodes_mut().push(node2("pos", n.pos[0], n.pos[1]));
    ch.nodes_mut().push(node2("size", n.size[0], n.size[1]));
    node.set_children(ch);
    node
}

fn placed_kdl(kind: &str, n: &SessionNode) -> KdlNode {
    let mut node = KdlNode::new(kind);
    node.push(KdlEntry::new(n.name.clone()));
    node.push(KdlEntry::new(n.id as i128));
    let mut ch = KdlDocument::new();
    ch.nodes_mut().push(node2("pos", n.pos[0], n.pos[1]));
    ch.nodes_mut().push(node2("size", n.size[0], n.size[1]));
    if !n.options.is_empty() {
        let mut opts = KdlNode::new("options");
        for &v in &n.options {
            opts.push(KdlEntry::new(v as f64));
        }
        ch.nodes_mut().push(opts);
    }
    if !n.args.is_empty() {
        let mut args = KdlNode::new("args");
        for a in &n.args {
            args.push(KdlEntry::new(a.clone()));
        }
        ch.nodes_mut().push(args);
    }
    node.set_children(ch);
    node
}

impl Session {
    pub fn load() -> Option<Session> {
        Self::from_kdl(&std::fs::read_to_string(SESSION).ok()?)
    }

    pub fn save(&self) -> Result<(), String> {
        std::fs::write(SESSION, self.to_kdl())
            .map_err(|e| format!("failed to write {SESSION}: {e}"))
    }

    fn from_kdl(text: &str) -> Option<Session> {
        let doc: KdlDocument = text.parse().ok()?;

        let mut camera = (0.0, 0.0, 1.0);
        if let Some(cam) = doc.get("camera").and_then(|n| n.children()) {
            if let Some(pan) = cam.get("pan") {
                if let (Some(x), Some(y)) = (pan.get(0).and_then(num), pan.get(1).and_then(num)) {
                    camera.0 = x;
                    camera.1 = y;
                }
            }
            if let Some(z) = cam.get("zoom").and_then(|n| n.get(0)).and_then(num) {
                camera.2 = z;
            }
        }

        let pair = |n: &KdlNode| match (n.get(0).and_then(uint), n.get(1).and_then(uint)) {
            (Some(a), Some(b)) => Some((a, b)),
            _ => None,
        };

        let mut nodes = Vec::new();
        let mut virtual_files = Vec::new();
        let mut host_files = Vec::new();
        let mut host_ports = Vec::new();
        let mut connections = Vec::new();
        let mut midi = Vec::new();
        let mut serves = Vec::new();
        let mut nets = Vec::new();
        let mut net_links = Vec::new();
        for n in doc.nodes() {
            match n.name().value() {
                "node" => nodes.extend(parse_placed(n)),
                "virtualfile" => virtual_files.extend(parse_placed(n)),
                "hostfile" => host_files.extend(parse_placed(n)),
                "hostport" => host_ports.extend(parse_hostport(n)),
                "connection" => connections.extend(pair(n)),
                "midi" => midi.extend(pair(n)),
                "serve" => serves.extend(pair(n)),
                "network" => nets.extend(parse_net(n, false)),
                "gateway" => nets.extend(parse_net(n, true)),
                "netlink" => net_links.extend(pair(n)),
                _ => {}
            }
        }

        Some(Session {
            camera,
            nodes,
            virtual_files,
            host_files,
            host_ports,
            connections,
            midi,
            serves,
            nets,
            net_links,
        })
    }

    fn to_kdl(&self) -> String {
        let mut doc = KdlDocument::new();

        let mut cam = KdlNode::new("camera");
        let mut cam_ch = KdlDocument::new();
        cam_ch
            .nodes_mut()
            .push(node2("pan", self.camera.0, self.camera.1));
        let mut zoom = KdlNode::new("zoom");
        zoom.push(KdlEntry::new(self.camera.2 as f64));
        cam_ch.nodes_mut().push(zoom);
        cam.set_children(cam_ch);
        doc.nodes_mut().push(cam);

        for n in &self.nodes {
            doc.nodes_mut().push(placed_kdl("node", n));
        }
        for f in &self.virtual_files {
            doc.nodes_mut().push(placed_kdl("virtualfile", f));
        }
        for f in &self.host_files {
            doc.nodes_mut().push(placed_kdl("hostfile", f));
        }
        for hp in &self.host_ports {
            doc.nodes_mut().push(hostport_kdl(hp));
        }
        for &(file, node) in &self.connections {
            doc.nodes_mut().push(pair_kdl("connection", file, node));
        }
        for &(src, dst) in &self.midi {
            doc.nodes_mut().push(pair_kdl("midi", src, dst));
        }
        for &(http, hostport) in &self.serves {
            doc.nodes_mut().push(pair_kdl("serve", http, hostport));
        }
        for n in &self.nets {
            doc.nodes_mut().push(net_kdl(n));
        }
        for &(app, net) in &self.net_links {
            doc.nodes_mut().push(pair_kdl("netlink", app, net));
        }

        doc.autoformat();
        doc.to_string()
    }
}

/// A KDL node `name a b` with two float args.
fn node2(name: &str, a: f32, b: f32) -> KdlNode {
    let mut n = KdlNode::new(name);
    n.push(KdlEntry::new(a as f64));
    n.push(KdlEntry::new(b as f64));
    n
}

/// A KDL node `name a b` with two integer-id args.
fn pair_kdl(name: &str, a: u64, b: u64) -> KdlNode {
    let mut n = KdlNode::new(name);
    n.push(KdlEntry::new(a as i128));
    n.push(KdlEntry::new(b as i128));
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_kdl_round_trips() {
        let s = Session {
            camera: (12.5, -40.0, 1.5),
            nodes: vec![SessionNode {
                name: "file_demo".into(),
                id: 1,
                pos: [40.0, 56.0],
                size: [360.0, 260.0],
                options: vec![8.0, 0.6, 0.0, 1.0],
                args: vec!["netserve".into(), "80".into()],
            }],
            virtual_files: vec![SessionNode {
                name: "chan".into(),
                id: 2,
                pos: [200.0, 120.0],
                size: [130.0, 44.0],
                options: Vec::new(),
                args: Vec::new(),
            }],
            host_files: vec![SessionNode {
                name: "notes.txt".into(),
                id: 6,
                pos: [200.0, 200.0],
                size: [130.0, 44.0],
                options: Vec::new(),
                args: Vec::new(),
            }],
            host_ports: vec![SessionPort {
                id: 5,
                port: 8080,
                pos: [600.0, 100.0],
                size: [130.0, 44.0],
            }],
            connections: vec![(2, 1)],
            midi: vec![(3, 4)],
            serves: vec![(1, 5)],
            nets: vec![
                SessionNet {
                    id: 7,
                    gateway: false,
                    pos: [700.0, 100.0],
                    size: [130.0, 44.0],
                },
                SessionNet {
                    id: 8,
                    gateway: true,
                    pos: [700.0, 200.0],
                    size: [130.0, 44.0],
                },
            ],
            net_links: vec![(1, 7)],
        };

        let back = Session::from_kdl(&s.to_kdl()).expect("parses");
        assert_eq!(back.camera, s.camera);
        assert_eq!(back.nodes.len(), 1);
        assert_eq!(back.nodes[0].name, "file_demo");
        assert_eq!(back.nodes[0].id, 1);
        assert_eq!(back.nodes[0].options, vec![8.0, 0.6, 0.0, 1.0]);
        assert_eq!(
            back.nodes[0].args,
            vec!["netserve".to_string(), "80".into()]
        );
        assert!(back.virtual_files[0].options.is_empty());
        assert!(back.virtual_files[0].args.is_empty());
        assert_eq!(back.virtual_files.len(), 1);
        assert_eq!(back.virtual_files[0].name, "chan");
        assert_eq!(back.host_files.len(), 1);
        assert_eq!(back.host_files[0].name, "notes.txt");
        assert_eq!(back.host_files[0].id, 6);
        assert_eq!(back.host_ports.len(), 1);
        assert_eq!(back.host_ports[0].port, 8080);
        assert_eq!(back.host_ports[0].id, 5);
        assert_eq!(back.connections, vec![(2, 1)]);
        assert_eq!(back.midi, vec![(3, 4)]);
        assert_eq!(back.serves, vec![(1, 5)]);
        assert_eq!(back.nets.len(), 2);
        assert_eq!(back.nets[0].id, 7);
        assert!(!back.nets[0].gateway);
        assert_eq!(back.nets[1].id, 8);
        assert!(back.nets[1].gateway);
        assert_eq!(back.net_links, vec![(1, 7)]);
    }
}
