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
    AddRouter,
    AddIroh,
    AddVeilid,
    AddNote,
    AddCapture,
    AddClipboard,
    AddApi,
    AddMidiIn,
    AddMidiOut,
    AddHostService,
    AddMulticast,
    NewWorkspace,
    CloseWorkspace,
    /// Jump the camera to this zoom factor.
    Zoom(f32),
    /// Enter the 3D workspace view (Esc returns to the canvas).
    View3d,
    /// Toggle the 3D camera between grounded walking and free flight.
    ToggleFly,
    /// Show or hide this node's flat 2D panel in the 3D world, leaving a
    /// `wk:scene` node as its 3D object alone.
    TogglePanel3d(NodeId),
    Quit,
    /// Close the UI but keep the server (and every node) running.
    Headless,
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

/// How well a row matches the palette query — `None` filters it out, and a
/// lower score sorts first.
///
/// The label is what someone is typing *at*; the description is context that
/// happens to be searchable. So a description hit never outranks a label hit:
/// "3d" offers **3D View** before "Add totem — a spinning 3D crystal" and
/// "Add world — the surrounding 3D world". Within the label, an earlier,
/// word-starting match wins, which is the difference between typing a command
/// and typing something that merely appears inside one.
pub(super) fn palette_rank(label: &str, desc: Option<&str>, query: &str) -> Option<u8> {
    if query.is_empty() {
        return Some(0); // no query: every row, in the order the palette built them
    }
    let q = query.to_lowercase();
    let hit = |hay: &str| -> Option<u8> {
        let h = hay.to_lowercase();
        let at = h.find(&q)?;
        Some(match h[..at].chars().next_back() {
            None => 0,                            // the row starts with what you typed
            Some(c) if !c.is_alphanumeric() => 1, // a word inside it does
            Some(_) => 2,                         // it's in there somewhere
        })
    };
    hit(label).or_else(|| desc.and_then(hit).map(|_| 3))
}

/// Most filtered command-palette rows shown at once.
pub(super) const PALETTE_MAX: usize = 9;

#[cfg(test)]
mod tests {
    use super::palette_rank;

    /// Typing "3d" must offer the view switch first — not the two plugins
    /// whose *descriptions* happen to mention 3D.
    #[test]
    fn a_label_hit_outranks_a_description_hit() {
        let view = palette_rank("3D View", Some("walk the workspace — WASD/QE move"), "3d");
        let totem = palette_rank("Add totem", Some("a spinning 3D crystal (wk:scene)"), "3d");
        let world = palette_rank(
            "Add world",
            Some("the surrounding 3D world: wire a .glb in"),
            "3d",
        );
        assert!(
            view < totem,
            "the command beats a description mentioning it"
        );
        assert!(view < world);
        assert_eq!(
            totem, world,
            "both are description-only hits, order untouched"
        );
    }

    /// Inside the label: a prefix beats a word start beats mid-word.
    #[test]
    fn an_earlier_word_starting_match_wins() {
        let prefix = palette_rank("Port 8080", None, "port");
        let word = palette_rank("Add Port", None, "port");
        let inside = palette_rank("Export", None, "port");
        assert!(prefix < word && word < inside);
    }

    #[test]
    fn a_row_matching_neither_is_filtered_out() {
        assert_eq!(palette_rank("Quit", Some("close wk"), "3d"), None);
    }

    #[test]
    fn an_empty_query_keeps_every_row_at_one_rank() {
        // Nothing typed: no reordering, so the palette's own order stands.
        assert_eq!(palette_rank("Quit", None, ""), Some(0));
        assert_eq!(palette_rank("3D View", None, ""), Some(0));
    }

    /// Case is irrelevant on both sides.
    #[test]
    fn matching_ignores_case() {
        assert_eq!(palette_rank("3D View", None, "3D"), Some(0));
        assert_eq!(palette_rank("3D View", None, "view"), Some(1));
    }
}
