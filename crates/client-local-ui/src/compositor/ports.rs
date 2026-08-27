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

/// One typed connection point on a node.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Port {
    pub(super) kind: PortKind,
    pub(super) dir: PortDir,
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
