//! The Cmd/Ctrl+K command palette: the actions it can run and one filtered row.

use super::*;

/// An action runnable from the Cmd/Ctrl+K command palette.
#[derive(Clone, Copy)]
pub(super) enum PaletteCmd {
    /// Launch the dependency at this index in `available`.
    Launch(usize),
    /// Centre the camera on this node.
    GoTo(NodeId),
    AddVolume,
    AddBindMount,
    AddPort,
    AddNetwork,
    AddGateway,
    AddIroh,
    AddVeilid,
    AddNote,
    AddCapture,
    AddMidiIn,
    NewWorkspace,
    CloseWorkspace,
    /// Jump the camera to this zoom factor.
    Zoom(f32),
    /// Enter the 3D workspace view (Esc returns to the canvas).
    View3d,
    Quit,
}

/// One command-palette row: a label, an optional dim description drawn after
/// it, and the command the row runs.
pub(super) struct PaletteRow {
    pub(super) label: String,
    pub(super) desc: Option<String>,
    pub(super) cmd: PaletteCmd,
}

impl PaletteRow {
    pub(super) fn new(label: impl Into<String>, desc: Option<String>, cmd: PaletteCmd) -> Self {
        PaletteRow {
            label: label.into(),
            desc,
            cmd,
        }
    }
}

/// Most filtered command-palette rows shown at once.
pub(super) const PALETTE_MAX: usize = 9;
