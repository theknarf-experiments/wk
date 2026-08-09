//! Hardware MIDI input on the host: a `MidiIn` node opens a physical MIDI input
//! port (a USB keyboard, etc.) and feeds its messages into the same MIDI
//! [`Router`](crate::midi::Router) a guest's `output` uses — so a hardware
//! device wires to app nodes on the canvas exactly like the piano plugin does.
//!
//! Bound to CoreMIDI on macOS; other platforms stub out (the cross-platform
//! `midir` crate's ALSA version clashes with cpal's at resolve time).

use wk_protocol::NodeId;

use crate::midi::Router;

#[cfg(target_os = "macos")]
mod imp {
    use super::*;

    /// The names of the available MIDI input sources (a USB keyboard's name).
    pub fn input_devices() -> Vec<String> {
        coremidi::Sources
            .into_iter()
            .filter_map(|s| s.display_name())
            .collect()
    }

    /// An open hardware MIDI input, feeding the router as its node. Holds the
    /// CoreMIDI client + port alive — dropping it closes the connection.
    pub struct MidiDevice {
        _client: coremidi::Client,
        _port: coremidi::InputPort,
        /// The resolved source name (what the node persists so it reconnects).
        pub name: String,
    }

    /// Open a MIDI input source and route its packets into `router` tagged as
    /// `node`. When `want` is empty the first source is used; otherwise the
    /// first whose name contains `want` (case-insensitive substring).
    pub fn open(node: NodeId, want: &str, router: Router) -> Result<MidiDevice, String> {
        let wanted = want.trim().to_lowercase();
        let source = coremidi::Sources
            .into_iter()
            .find(|s| {
                wanted.is_empty()
                    || s.display_name()
                        .map(|n| n.to_lowercase().contains(&wanted))
                        .unwrap_or(false)
            })
            .ok_or_else(|| {
                if wanted.is_empty() {
                    "no MIDI input devices are connected".to_string()
                } else {
                    format!("no MIDI input device matching {want:?}")
                }
            })?;
        let name = source.display_name().unwrap_or_default();

        let client = coremidi::Client::new("wk").map_err(|e| format!("MIDI client: {e}"))?;
        let port = client
            .input_port("wk-midi-in", move |packets: &coremidi::PacketList| {
                let routes = router.lock().unwrap();
                for packet in packets.iter() {
                    let data = packet.data();
                    if !data.is_empty() {
                        routes.send_from(node, &data.to_vec());
                    }
                }
            })
            .map_err(|e| format!("MIDI input port: {e}"))?;
        port.connect_source(&source)
            .map_err(|e| format!("connect MIDI source {name:?}: {e}"))?;

        Ok(MidiDevice {
            _client: client,
            _port: port,
            name,
        })
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::*;

    pub fn input_devices() -> Vec<String> {
        Vec::new()
    }

    pub struct MidiDevice {
        pub name: String,
    }

    pub fn open(_node: NodeId, _want: &str, _router: Router) -> Result<MidiDevice, String> {
        Err("hardware MIDI is only available on macOS in this build".to_string())
    }
}

pub use imp::{input_devices, open, MidiDevice};
