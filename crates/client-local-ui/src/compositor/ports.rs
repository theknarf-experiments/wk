//! Typed connection ports: the per-kind dots a node exposes, and the pure
//! geometry placing them along a node's edges. The wire colours they reference
//! live in the compositor's palette (via `super`).

use super::*;

/// Connection port radius, in canvas pixels.
pub(super) const PORT_R: f32 = 6.0;
/// A port lights up when the cursor is over it (hover / valid drop target).
pub(super) const PORT_HOT: [f32; 4] = [0.55, 0.80, 1.0, 1.0];

/// The dot / wire colour for a connection kind, so the canvas shows *what* a
/// connection carries rather than a single generic in/out dot.
///
/// [`PortKind`] itself lives in `wk-protocol`: a boundary port declares one in
/// the `.wk` file, so the server names the same kinds and the two must agree.
/// A free function rather than a method because the type is not ours.
pub(super) fn port_color(kind: PortKind) -> [f32; 4] {
    match kind {
        PortKind::Bind => WIRE_COL,
        PortKind::Midi => MIDI_WIRE_COL,
        PortKind::Serve => HOSTPORT_WIRE,
        PortKind::Net => NET_WIRE_COL,
        PortKind::Capture => CAPTURE_BORDER,
        PortKind::Clipboard => CLIPBOARD_BORDER,
        PortKind::Api => API_BORDER,
    }
}

/// What a boundary port's direction is called on the canvas — the word an
/// author writes in the `.wk` file, so the node reads as what it is.
pub(super) fn port_label(dir: PortDir) -> &'static str {
    match dir {
        PortDir::In => "inport",
        PortDir::Out => "outport",
    }
}

/// What an instance's widget says under its name: how much of a workspace it
/// is standing in for, and how it can be reached. A group's own ports are the
/// definition's, so the count of them is the honest measure of its edge.
pub(super) fn group_status(g: &wk_server::server::GroupInfo) -> String {
    let node = |n: usize| if n == 1 { "node" } else { "nodes" };
    match (g.nodes, g.ports.len()) {
        (0, _) => "empty definition".to_string(),
        (n, 0) => format!("{n} {}", node(n)),
        (n, p) => format!("{n} {} · {p} ports", node(n)),
    }
}

/// One typed connection point on a node.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Port {
    pub(super) kind: PortKind,
    pub(super) dir: PortDir,
    /// Which of the node's ports this is, in `node_ports` order — the port's
    /// identity.
    ///
    /// Kind and direction used to be identity enough, because no node had two
    /// ports alike. An *instance* does: it wears the definition's boundary
    /// ports, and a definition may perfectly well declare two `midi` in-ports
    /// with different names. Without this the hit-test could not say which of
    /// them a drag landed on, and `View::groups[id].ports` — which is in the
    /// definition's file order, the same order [`super::Compositor::node_ports`]
    /// builds — is what turns the slot back into the port's name.
    pub(super) slot: usize,
}

/// A port of a kind and direction, before it knows where on the node it sits.
/// [`super::Compositor::node_ports`] stamps the slots on in one pass, so no
/// caller has to count.
pub(super) fn port(kind: PortKind, dir: PortDir) -> Port {
    Port { kind, dir, slot: 0 }
}

/// The y-centres for `n` ports stacked down a node edge (screen rect `r`),
/// evenly spaced with margins so a single port sits at the middle and none
/// overflow the node.
pub(super) fn port_slots_y(r: [f32; 4], n: usize) -> Vec<f32> {
    let h = r[3] - r[1];
    (0..n)
        .map(|i| r[1] + h * (i as f32 + 1.0) / (n as f32 + 1.0))
        .collect()
}

/// Anchor points for a node's typed ports: inputs down the left edge, outputs
/// down the right edge, in `ports` order. Pure geometry — shared by the draw,
/// the hit-test, and the wire-endpoint lookup so they never diverge.
pub(super) fn port_anchors(r: [f32; 4], ports: &[Port]) -> Vec<[f32; 2]> {
    let ins: Vec<usize> = (0..ports.len())
        .filter(|&i| ports[i].dir == PortDir::In)
        .collect();
    let outs: Vec<usize> = (0..ports.len())
        .filter(|&i| ports[i].dir == PortDir::Out)
        .collect();
    let in_y = port_slots_y(r, ins.len());
    let out_y = port_slots_y(r, outs.len());
    let mut anchors = vec![[0.0, 0.0]; ports.len()];
    for (slot, &pi) in ins.iter().enumerate() {
        anchors[pi] = [r[0], in_y[slot]];
    }
    for (slot, &pi) in outs.iter().enumerate() {
        anchors[pi] = [r[2], out_y[slot]];
    }
    anchors
}
