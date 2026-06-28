//! Workspace session: the runtime layout of a wk workspace — the canvas camera
//! and which app instances were open and where — persisted to `wk.session.kdl`
//! in the working directory so a `wk run` restores where you left off.
//!
//! This is local runtime state (not project definition), kept separate from
//! `wk.kdl` and git-ignored.

use kdl::{KdlDocument, KdlEntry, KdlNode, KdlValue};
use std::path::PathBuf;

/// Session file name, alongside `wk.kdl` in the working directory.
pub const SESSION: &str = "wk.session.kdl";

/// One open window: which plugin it runs and its rect in canvas space.
pub struct SessionNode {
    pub path: PathBuf,
    pub pos: [f32; 2],
    pub size: [f32; 2],
}

pub struct Session {
    /// Canvas camera: pan x, pan y, zoom.
    pub camera: (f32, f32, f32),
    pub nodes: Vec<SessionNode>,
}

/// Read a KDL value as a number, accepting either a float or an integer.
fn num(v: &KdlValue) -> Option<f32> {
    v.as_float()
        .map(|f| f as f32)
        .or_else(|| v.as_integer().map(|i| i as f32))
}

impl Session {
    /// Load the session for the current directory, if any exists and parses.
    pub fn load() -> Option<Session> {
        Self::from_kdl(&std::fs::read_to_string(SESSION).ok()?)
    }

    /// Write the session for the current directory.
    pub fn save(&self) -> Result<(), String> {
        std::fs::write(SESSION, self.to_kdl())
            .map_err(|e| format!("failed to write {SESSION}: {e}"))
    }

    /// Parse a session from KDL text.
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

        let nodes = doc
            .nodes()
            .iter()
            .filter(|n| n.name().value() == "node")
            .filter_map(|n| {
                let path = PathBuf::from(n.get(0)?.as_string()?);
                let ch = n.children()?;
                let pos = ch.get("pos")?;
                let size = ch.get("size")?;
                Some(SessionNode {
                    path,
                    pos: [pos.get(0).and_then(num)?, pos.get(1).and_then(num)?],
                    size: [size.get(0).and_then(num)?, size.get(1).and_then(num)?],
                })
            })
            .collect();

        Some(Session { camera, nodes })
    }

    /// Render the session to KDL text.
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

        for w in &self.nodes {
            let mut node = KdlNode::new("node");
            node.push(KdlEntry::new(w.path.to_string_lossy().to_string()));
            let mut ch = KdlDocument::new();
            ch.nodes_mut().push(node2("pos", w.pos[0], w.pos[1]));
            ch.nodes_mut().push(node2("size", w.size[0], w.size[1]));
            node.set_children(ch);
            doc.nodes_mut().push(node);
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
            nodes: vec![
                SessionNode {
                    path: PathBuf::from("plugins/paint/paint.wasm"),
                    pos: [40.0, 56.0],
                    size: [320.0, 240.0],
                },
                SessionNode {
                    path: PathBuf::from("plugins/triangle/triangle.wasm"),
                    pos: [200.0, 120.0],
                    size: [256.0, 256.0],
                },
            ],
        };

        let back = Session::from_kdl(&s.to_kdl()).expect("parses");
        assert_eq!(back.camera, s.camera);
        assert_eq!(back.nodes.len(), 2);
        for (a, b) in back.nodes.iter().zip(&s.nodes) {
            assert_eq!(a.path, b.path);
            assert_eq!(a.pos, b.pos);
            assert_eq!(a.size, b.size);
        }
    }
}
