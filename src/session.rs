//! Workspace session: the runtime layout of a wk workspace — the canvas camera,
//! which nodes (app instances and file nodes) were open and where, and the
//! connections wiring them — persisted to `wk.session.kdl` so a `wk run`
//! restores where you left off. Local runtime state, git-ignored.
//!
//! ```kdl
//! camera { pan 0 0; zoom 1 }
//! node "file_demo" 1 { pos 19 88; size 360 260 }
//! file "chan" 2 { pos 400 120; size 130 44 }
//! connection 2 1
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
}

pub struct Session {
    /// Canvas camera: pan x, pan y, zoom.
    pub camera: (f32, f32, f32),
    pub nodes: Vec<SessionNode>,
    pub files: Vec<SessionNode>,
    /// Connections as (file id, app node id).
    pub connections: Vec<(u64, u64)>,
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
    Some(SessionNode {
        name,
        id,
        pos: [pos.get(0).and_then(num)?, pos.get(1).and_then(num)?],
        size: [size.get(0).and_then(num)?, size.get(1).and_then(num)?],
    })
}

fn placed_kdl(kind: &str, n: &SessionNode) -> KdlNode {
    let mut node = KdlNode::new(kind);
    node.push(KdlEntry::new(n.name.clone()));
    node.push(KdlEntry::new(n.id as i128));
    let mut ch = KdlDocument::new();
    ch.nodes_mut().push(node2("pos", n.pos[0], n.pos[1]));
    ch.nodes_mut().push(node2("size", n.size[0], n.size[1]));
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

        let mut nodes = Vec::new();
        let mut files = Vec::new();
        let mut connections = Vec::new();
        for n in doc.nodes() {
            match n.name().value() {
                "node" => nodes.extend(parse_placed(n)),
                "file" => files.extend(parse_placed(n)),
                "connection" => {
                    if let (Some(f), Some(t)) = (n.get(0).and_then(uint), n.get(1).and_then(uint)) {
                        connections.push((f, t));
                    }
                }
                _ => {}
            }
        }

        Some(Session {
            camera,
            nodes,
            files,
            connections,
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
        for f in &self.files {
            doc.nodes_mut().push(placed_kdl("file", f));
        }
        for &(file, node) in &self.connections {
            let mut c = KdlNode::new("connection");
            c.push(KdlEntry::new(file as i128));
            c.push(KdlEntry::new(node as i128));
            doc.nodes_mut().push(c);
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
            }],
            files: vec![SessionNode {
                name: "chan".into(),
                id: 2,
                pos: [200.0, 120.0],
                size: [130.0, 44.0],
            }],
            connections: vec![(2, 1)],
        };

        let back = Session::from_kdl(&s.to_kdl()).expect("parses");
        assert_eq!(back.camera, s.camera);
        assert_eq!(back.nodes.len(), 1);
        assert_eq!(back.nodes[0].name, "file_demo");
        assert_eq!(back.nodes[0].id, 1);
        assert_eq!(back.files.len(), 1);
        assert_eq!(back.files[0].name, "chan");
        assert_eq!(back.connections, vec![(2, 1)]);
    }
}
