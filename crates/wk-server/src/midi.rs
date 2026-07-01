//! Host side of wk's MIDI transport: plugins send/receive raw MIDI messages
//! through `output`/`input` ports, and the server wires a source node's
//! output to the inputs of the nodes it is connected to (a "midi" connection on
//! the canvas). A keyboard plugin can thus drive a separate synth plugin — the
//! same split as real MIDI gear joined by a cable.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use wk_protocol::NodeId;

use wasmtime::component::{HasData, Linker, Resource};
use wasmtime::Result;
use wasmtime_wasi_io::IoView;

use crate::plugin::HostState;

wasmtime::component::bindgen!({
    path: "wit-midi",
    world: "midi-host",
    imports: { default: trappable },
    require_store_data_send: true,
    with: {
        "wk:midi/midi.input": MidiInput,
        "wk:midi/midi.output": MidiOutput,
    },
});

/// One MIDI message: raw status + data bytes, as in the MIDI 1.0 spec.
pub type Message = Vec<u8>;

/// A node's MIDI input queue: connected sources push, the guest drains.
#[derive(Default)]
pub struct Inbox {
    queue: VecDeque<Message>,
}

impl Inbox {
    fn push(&mut self, msg: Message) {
        // Bound the backlog so a node that never reads can't grow it forever.
        if self.queue.len() < 1024 {
            self.queue.push_back(msg);
        }
    }
}

pub type SharedInbox = Arc<Mutex<Inbox>>;

pub fn new_inbox() -> SharedInbox {
    Arc::new(Mutex::new(Inbox::default()))
}

/// Routes MIDI from each source node to the inboxes of the nodes it is wired to.
/// Owned by `PluginHost`; the server edits it as connections are made and
/// broken, and guest `output.send` calls read it.
#[derive(Default)]
pub struct Routes {
    /// Source node id -> connected (destination id, destination inbox).
    links: HashMap<NodeId, Vec<(NodeId, SharedInbox)>>,
}

impl Routes {
    pub fn connect(&mut self, src: NodeId, dst: NodeId, inbox: SharedInbox) {
        let v = self.links.entry(src).or_default();
        if !v.iter().any(|(id, _)| *id == dst) {
            v.push((dst, inbox));
        }
    }

    pub fn disconnect(&mut self, src: NodeId, dst: NodeId) {
        if let Some(v) = self.links.get_mut(&src) {
            v.retain(|(id, _)| *id != dst);
        }
    }

    /// Drop a node entirely, as a source and as any destination.
    pub fn remove_node(&mut self, id: NodeId) {
        self.links.remove(&id);
        for v in self.links.values_mut() {
            v.retain(|(d, _)| *d != id);
        }
    }

    fn send(&self, src: NodeId, msg: &Message) {
        if let Some(v) = self.links.get(&src) {
            for (_, inbox) in v {
                inbox.lock().unwrap().push(msg.clone());
            }
        }
    }
}

pub type Router = Arc<Mutex<Routes>>;

pub fn new_router() -> Router {
    Arc::new(Mutex::new(Routes::default()))
}

/// Resource reps. `input` drains this node's inbox; `output` sends via the
/// router tagged with this node's id.
pub struct MidiInput {
    inbox: SharedInbox,
}
pub struct MidiOutput;

pub fn add_to_linker(l: &mut Linker<HostState>) -> Result<()> {
    wk::midi::midi::add_to_linker::<_, HasMidi>(l, |s| s)?;
    Ok(())
}

struct HasMidi;
impl HasData for HasMidi {
    type Data<'a> = &'a mut HostState;
}

impl wk::midi::midi::Host for HostState {}

impl wk::midi::midi::HostInput for HostState {
    fn new(&mut self) -> Result<Resource<MidiInput>> {
        let inbox = self.midi_in.clone();
        Ok(self.table().push(MidiInput { inbox })?)
    }

    fn receive(&mut self, this: Resource<MidiInput>) -> Result<Option<Vec<u8>>> {
        let input = self.table().get(&this)?;
        let msg = input.inbox.lock().unwrap().queue.pop_front();
        Ok(msg)
    }

    fn drop(&mut self, this: Resource<MidiInput>) -> Result<()> {
        self.table().delete(this)?;
        Ok(())
    }
}

impl wk::midi::midi::HostOutput for HostState {
    fn new(&mut self) -> Result<Resource<MidiOutput>> {
        Ok(self.table().push(MidiOutput)?)
    }

    fn send(&mut self, _this: Resource<MidiOutput>, data: Vec<u8>) -> Result<()> {
        self.midi_router.lock().unwrap().send(self.node_id, &data);
        Ok(())
    }

    fn drop(&mut self, this: Resource<MidiOutput>) -> Result<()> {
        self.table().delete(this)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn len(inbox: &SharedInbox) -> usize {
        inbox.lock().unwrap().queue.len()
    }

    #[test]
    fn routes_only_to_connected_destinations() {
        let mut routes = Routes::default();
        let to_synth = new_inbox();
        let unrelated = new_inbox();
        let (kbd, synth) = (NodeId::nil(), NodeId::new());

        // Wire keyboard -> synth; leave the unrelated node unconnected.
        routes.connect(kbd, synth, to_synth.clone());
        routes.send(kbd, &vec![0x90, 60, 100]);
        assert_eq!(len(&to_synth), 1, "connected destination receives");
        assert_eq!(len(&unrelated), 0, "unconnected node receives nothing");

        // Idempotent connect doesn't duplicate delivery.
        routes.connect(kbd, synth, to_synth.clone());
        routes.send(kbd, &vec![0x80, 60, 0]);
        assert_eq!(len(&to_synth), 2);

        // Disconnecting stops delivery.
        routes.disconnect(kbd, synth);
        routes.send(kbd, &vec![0x90, 62, 100]);
        assert_eq!(len(&to_synth), 2);

        // Removing the source node also stops delivery.
        routes.connect(kbd, synth, to_synth.clone());
        routes.remove_node(kbd);
        routes.send(kbd, &vec![0x90, 64, 100]);
        assert_eq!(len(&to_synth), 2);
    }
}
