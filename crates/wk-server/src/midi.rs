//! Host side of wk's MIDI transport: plugins send/receive MIDI messages through
//! `output`/`input` ports, and the server wires a source node's output to the
//! inputs of the nodes it is connected to (a "midi" connection on the canvas).
//! A keyboard plugin can thus drive a separate synth plugin — the same split as
//! real MIDI gear joined by a cable.
//!
//! Two things make this more than a queue of byte strings:
//!
//! * Every message carries an instant on a clock shared by all nodes, so a
//!   sequencer can say when a note belongs instead of leaving it to land
//!   wherever the receiving node next happens to wake up. See [`now`].
//! * The router remembers which notes each link has turned on, so unplugging a
//!   cable or deleting a node releases them instead of leaving the synth
//!   holding a chord forever.
//! * A wire can carry one MIDI channel instead of all sixteen. A multi-track
//!   sequencer sends every part down every wire, so without this the only way
//!   to say "this synth plays the bass" is a setting buried inside the synth,
//!   where the canvas cannot show it. See [`Routes::set_channel`].

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;
use wk_protocol::NodeId;

use wasmtime::component::{HasData, Linker, Resource};
use wasmtime::Result;
use wasmtime_wasi_io::IoView;

use crate::plugin::HostState;

pub mod stream;

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

/// An instant on wk's shared MIDI clock, in microseconds.
pub type Instant64 = u64;

/// The origin of the shared clock. Fixed on first read and never reset, so
/// every node in the workspace measures from the same zero.
static ORIGIN: OnceLock<Instant> = OnceLock::new();

/// Read wk's shared monotonic MIDI clock, in microseconds.
///
/// This is the one clock MIDI instants are expressed on. It is deliberately not
/// any node's audio clock: nodes come and go and each audio context starts its
/// own timeline, whereas a MIDI instant has to mean the same moment in the
/// sequencer that scheduled it and the synth that plays it.
pub fn now() -> Instant64 {
    ORIGIN.get_or_init(Instant::now).elapsed().as_micros() as u64
}

/// A MIDI message and when it belongs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    pub data: Message,
    /// Microseconds on the shared clock; `0` means "as soon as possible".
    pub time: Instant64,
}

impl Event {
    /// A message to take effect immediately.
    pub fn now(data: Message) -> Self {
        Event { data, time: 0 }
    }

    /// Is this a note-off — either a note-off status or the note-on-with-zero-
    /// velocity spelling of one? Those must not be dropped under load, or the
    /// note they release stays stuck on.
    fn is_note_off(&self) -> bool {
        match self.data.as_slice() {
            [status, _, velocity] => {
                status & 0xF0 == 0x80 || (status & 0xF0 == 0x90 && *velocity == 0)
            }
            _ => false,
        }
    }
}

/// How many messages a node may fall behind before the router starts shedding.
const BACKLOG: usize = 1024;

/// A node's MIDI input queue: connected sources push, the guest drains.
#[derive(Default)]
pub struct Inbox {
    queue: VecDeque<Event>,
}

impl Inbox {
    fn push(&mut self, event: Event) {
        // Bound the backlog so a node that never reads can't grow it forever.
        // When shedding, drop something that is not a note-off: a lost note-on
        // is a missing note, but a lost note-off is a note that sounds forever.
        if self.queue.len() >= BACKLOG {
            let victim = self.queue.iter().position(|e| !e.is_note_off());
            match victim {
                Some(i) => {
                    self.queue.remove(i);
                }
                None => {
                    self.queue.pop_front();
                }
            }
        }
        self.queue.push_back(event);
    }

    /// Take everything queued, leaving the inbox empty. Used by a hardware
    /// output port, which drains on its own thread rather than through a
    /// guest's `receive`.
    pub fn drain(&mut self) -> Vec<Event> {
        self.queue.drain(..).collect()
    }
}

pub type SharedInbox = Arc<Mutex<Inbox>>;

pub fn new_inbox() -> SharedInbox {
    Arc::new(Mutex::new(Inbox::default()))
}

/// One wire from a source node to a destination node.
struct Link {
    dst: NodeId,
    inbox: SharedInbox,
    /// The one MIDI channel this wire carries, 1 to 16. `None` — the default —
    /// carries all of them.
    channel: Option<u8>,
    /// The notes this wire has turned on and not yet turned off, as
    /// `(channel, note)`. Kept so the wire can release them if it is cut.
    held: HashSet<(u8, u8)>,
}

impl Link {
    /// Does this wire carry `msg`?
    ///
    /// System messages — clock, start, stop, song position, system-exclusive —
    /// always pass, whatever the wire is set to. They address the device rather
    /// than a channel, and a part filtered down to one channel still has to
    /// keep time with the rest of the song.
    fn carries(&self, msg: &Message) -> bool {
        let Some(want) = self.channel else {
            return true;
        };
        match msg.first() {
            Some(&status) if status < 0xF0 => (status & 0x0F) + 1 == want,
            _ => true,
        }
    }

    /// Track what `msg` does to the set of sounding notes, so a later
    /// disconnect knows what to release.
    fn observe(&mut self, msg: &Message) {
        let [status, a, b] = msg[..] else { return };
        let channel = status & 0x0F;
        match status & 0xF0 {
            0x90 if b > 0 => {
                self.held.insert((channel, a));
            }
            0x80 | 0x90 => {
                self.held.remove(&(channel, a));
            }
            // All notes off / all sound off / reset all controllers all release
            // the channel's notes at the receiver, so stop tracking them.
            0xB0 if matches!(a, 120 | 121 | 123) => {
                self.held.retain(|&(c, _)| c != channel);
            }
            _ => {}
        }
    }

    /// Release every note this wire is holding, then ask the destination to
    /// clear anything it is still sounding on those channels.
    fn release(&mut self) {
        let mut inbox = self.inbox.lock().unwrap();
        let channels: HashSet<u8> = self.held.iter().map(|&(c, _)| c).collect();
        for (channel, note) in self.held.drain() {
            inbox.push(Event::now(vec![0x80 | channel, note, 0]));
        }
        // Belt and braces for a destination that tracks its own notes: the
        // explicit note-offs above cover a synth that only understands notes,
        // and "all notes off" covers one that has notes we never saw start.
        for channel in channels {
            inbox.push(Event::now(vec![0xB0 | channel, 123, 0]));
        }
    }
}

/// Routes MIDI from each source node to the inboxes of the nodes it is wired to.
/// Owned by `PluginHost`; the server edits it as connections are made and
/// broken, and guest `output.send` calls read it.
#[derive(Default)]
pub struct Routes {
    /// Source node id -> the wires leaving it.
    links: HashMap<NodeId, Vec<Link>>,
}

impl Routes {
    pub fn connect(&mut self, src: NodeId, dst: NodeId, inbox: SharedInbox) {
        let v = self.links.entry(src).or_default();
        if !v.iter().any(|l| l.dst == dst) {
            v.push(Link {
                dst,
                inbox,
                channel: None,
                held: HashSet::new(),
            });
        }
    }

    /// Set which MIDI channel a wire carries: `Some(1..=16)` for one part,
    /// `None` for all of them.
    ///
    /// Narrowing a wire releases anything it is holding on the channels it no
    /// longer carries, so changing it mid-chord does not leave a note sounding
    /// with nothing left to turn it off.
    pub fn set_channel(&mut self, src: NodeId, dst: NodeId, channel: Option<u8>) {
        let channel = channel.filter(|c| (1..=16).contains(c));
        let Some(v) = self.links.get_mut(&src) else {
            return;
        };
        for link in v.iter_mut().filter(|l| l.dst == dst) {
            if link.channel == channel {
                continue;
            }
            link.channel = channel;
            let orphaned: Vec<(u8, u8)> = link
                .held
                .iter()
                .copied()
                .filter(|&(c, note)| !link.carries(&vec![0x90 | c, note, 1]))
                .collect();
            let mut inbox = link.inbox.lock().unwrap();
            for (c, note) in orphaned {
                link.held.remove(&(c, note));
                inbox.push(Event::now(vec![0x80 | c, note, 0]));
            }
        }
    }

    /// Cut a wire, releasing any notes it left sounding.
    pub fn disconnect(&mut self, src: NodeId, dst: NodeId) {
        if let Some(v) = self.links.get_mut(&src) {
            for link in v.iter_mut().filter(|l| l.dst == dst) {
                link.release();
            }
            v.retain(|l| l.dst != dst);
        }
    }

    /// Drop a node entirely, as a source and as any destination. Notes the node
    /// was sounding elsewhere are released; notes played *into* a node that is
    /// going away need no cleanup, since the node and its voices go with it.
    pub fn remove_node(&mut self, id: NodeId) {
        if let Some(mut v) = self.links.remove(&id) {
            for link in v.iter_mut() {
                link.release();
            }
        }
        for v in self.links.values_mut() {
            v.retain(|l| l.dst != id);
        }
    }

    fn send(&mut self, src: NodeId, event: &Event) {
        let Some(v) = self.links.get_mut(&src) else {
            return;
        };
        for link in v.iter_mut() {
            if !link.carries(&event.data) {
                continue;
            }
            link.observe(&event.data);
            link.inbox.lock().unwrap().push(event.clone());
        }
    }

    /// Inject a message from a non-guest source (a hardware MIDI device node),
    /// routed to that node's connected destinations exactly like a guest's
    /// `output.send`.
    pub fn send_from(&mut self, src: NodeId, event: &Event) {
        self.send(src, event);
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

impl wk::midi::midi::Host for HostState {
    fn now(&mut self) -> Result<u64> {
        Ok(now())
    }
}

impl wk::midi::midi::HostInput for HostState {
    fn new(&mut self) -> Result<Resource<MidiInput>> {
        let inbox = self.midi_in.clone();
        Ok(self.table().push(MidiInput { inbox })?)
    }

    fn receive(&mut self, this: Resource<MidiInput>) -> Result<Option<Vec<u8>>> {
        let input = self.table().get(&this)?;
        let msg = input.inbox.lock().unwrap().queue.pop_front();
        Ok(msg.map(|e| e.data))
    }

    fn receive_event(
        &mut self,
        this: Resource<MidiInput>,
    ) -> Result<Option<wk::midi::midi::Event>> {
        let input = self.table().get(&this)?;
        let event = input.inbox.lock().unwrap().queue.pop_front();
        Ok(event.map(|e| wk::midi::midi::Event {
            data: e.data,
            time: e.time,
        }))
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
        self.midi_router
            .lock()
            .unwrap()
            .send(self.node_id, &Event::now(data));
        Ok(())
    }

    fn send_at(&mut self, _this: Resource<MidiOutput>, data: Vec<u8>, time: u64) -> Result<()> {
        self.midi_router
            .lock()
            .unwrap()
            .send(self.node_id, &Event { data, time });
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

    fn drain(inbox: &SharedInbox) -> Vec<Message> {
        let mut q = inbox.lock().unwrap();
        q.queue.drain(..).map(|e| e.data).collect()
    }

    fn len(inbox: &SharedInbox) -> usize {
        inbox.lock().unwrap().queue.len()
    }

    fn note_on(note: u8) -> Event {
        Event::now(vec![0x90, note, 100])
    }

    #[test]
    fn routes_only_to_connected_destinations() {
        let mut routes = Routes::default();
        let to_synth = new_inbox();
        let unrelated = new_inbox();
        let (kbd, synth) = (NodeId::nil(), NodeId::new());

        // Wire keyboard -> synth; leave the unrelated node unconnected.
        routes.connect(kbd, synth, to_synth.clone());
        routes.send(kbd, &note_on(60));
        assert_eq!(len(&to_synth), 1, "connected destination receives");
        assert_eq!(len(&unrelated), 0, "unconnected node receives nothing");

        // Idempotent connect doesn't duplicate delivery.
        routes.connect(kbd, synth, to_synth.clone());
        routes.send(kbd, &Event::now(vec![0x80, 60, 0]));
        assert_eq!(len(&to_synth), 2);

        // Disconnecting stops delivery.
        routes.disconnect(kbd, synth);
        drain(&to_synth);
        routes.send(kbd, &note_on(62));
        assert_eq!(len(&to_synth), 0);

        // Removing the source node also stops delivery.
        routes.connect(kbd, synth, to_synth.clone());
        drain(&to_synth);
        routes.remove_node(kbd);
        routes.send(kbd, &note_on(64));
        assert_eq!(len(&to_synth), 0);
    }

    #[test]
    fn instants_travel_with_the_message() {
        let mut routes = Routes::default();
        let inbox = new_inbox();
        let (src, dst) = (NodeId::nil(), NodeId::new());
        routes.connect(src, dst, inbox.clone());

        routes.send(
            src,
            &Event {
                data: vec![0x90, 60, 100],
                time: 12_345,
            },
        );
        let got = inbox.lock().unwrap().queue.pop_front().unwrap();
        assert_eq!(
            got.time, 12_345,
            "the destination sees when the note belongs"
        );
    }

    #[test]
    fn cutting_a_wire_releases_the_notes_it_was_holding() {
        let mut routes = Routes::default();
        let inbox = new_inbox();
        let (kbd, synth) = (NodeId::nil(), NodeId::new());
        routes.connect(kbd, synth, inbox.clone());

        // Two notes down, one released, then the cable is pulled.
        routes.send(kbd, &note_on(60));
        routes.send(kbd, &note_on(64));
        routes.send(kbd, &Event::now(vec![0x80, 60, 0]));
        drain(&inbox);
        routes.disconnect(kbd, synth);

        let after = drain(&inbox);
        assert!(
            after.contains(&vec![0x80, 64, 0]),
            "the note still down is released, got {after:?}"
        );
        assert!(
            !after.contains(&vec![0x80, 60, 0]),
            "the note already released is not released twice, got {after:?}"
        );
        assert!(
            after.contains(&vec![0xB0, 123, 0]),
            "and the channel is told to clear anything else, got {after:?}"
        );
    }

    #[test]
    fn deleting_a_source_node_releases_its_notes() {
        let mut routes = Routes::default();
        let inbox = new_inbox();
        let (kbd, synth) = (NodeId::nil(), NodeId::new());
        routes.connect(kbd, synth, inbox.clone());
        routes.send(kbd, &note_on(60));
        drain(&inbox);

        routes.remove_node(kbd);
        assert!(
            drain(&inbox).contains(&vec![0x80, 60, 0]),
            "deleting the keyboard must not leave the synth droning"
        );
    }

    #[test]
    fn an_all_notes_off_stops_the_router_tracking_that_channel() {
        let mut routes = Routes::default();
        let inbox = new_inbox();
        let (kbd, synth) = (NodeId::nil(), NodeId::new());
        routes.connect(kbd, synth, inbox.clone());
        routes.send(kbd, &note_on(60));
        routes.send(kbd, &Event::now(vec![0xB0, 123, 0]));
        drain(&inbox);

        routes.disconnect(kbd, synth);
        assert!(
            drain(&inbox).is_empty(),
            "nothing is sounding, so nothing needs releasing"
        );
    }

    #[test]
    fn a_wire_set_to_one_channel_carries_only_that_part() {
        // The reason this exists: a multi-track sequencer sends every part down
        // every wire, so without it the only way to say "this synth plays the
        // bass" is a setting buried inside the synth.
        let mut routes = Routes::default();
        let inbox = new_inbox();
        let (seq, synth) = (NodeId::nil(), NodeId::new());
        routes.connect(seq, synth, inbox.clone());
        routes.set_channel(seq, synth, Some(3));

        routes.send(seq, &Event::now(vec![0x90, 60, 100])); // channel 1
        routes.send(seq, &Event::now(vec![0x92, 64, 100])); // channel 3
        routes.send(seq, &Event::now(vec![0x95, 67, 100])); // channel 6
        assert_eq!(
            drain(&inbox),
            vec![vec![0x92, 64, 100]],
            "only the part this wire carries arrives"
        );
    }

    #[test]
    fn the_clock_goes_down_a_narrowed_wire_too() {
        // System messages address the device, not a channel: a part filtered to
        // one channel still has to keep time with the rest of the song.
        let mut routes = Routes::default();
        let inbox = new_inbox();
        let (seq, synth) = (NodeId::nil(), NodeId::new());
        routes.connect(seq, synth, inbox.clone());
        routes.set_channel(seq, synth, Some(2));

        for msg in [vec![0xFA], vec![0xF8], vec![0xFC]] {
            routes.send(seq, &Event::now(msg));
        }
        assert_eq!(
            drain(&inbox),
            vec![vec![0xFA], vec![0xF8], vec![0xFC]],
            "start, clock and stop all pass"
        );
    }

    #[test]
    fn narrowing_a_wire_releases_the_parts_it_stops_carrying() {
        // Changing the channel mid-chord must not leave a note sounding with
        // nothing left that can turn it off.
        let mut routes = Routes::default();
        let inbox = new_inbox();
        let (seq, synth) = (NodeId::nil(), NodeId::new());
        routes.connect(seq, synth, inbox.clone());
        routes.send(seq, &Event::now(vec![0x90, 60, 100])); // channel 1
        routes.send(seq, &Event::now(vec![0x92, 64, 100])); // channel 3
        drain(&inbox);

        routes.set_channel(seq, synth, Some(3));
        let after = drain(&inbox);
        assert_eq!(
            after,
            vec![vec![0x80, 60, 0]],
            "the note on the channel it no longer carries is released, got {after:?}"
        );

        // And the surviving note is still tracked, so cutting the wire releases it.
        routes.disconnect(seq, synth);
        assert!(drain(&inbox).contains(&vec![0x82, 64, 0]));
    }

    #[test]
    fn a_wire_carries_everything_until_it_is_told_otherwise() {
        let mut routes = Routes::default();
        let inbox = new_inbox();
        let (seq, synth) = (NodeId::nil(), NodeId::new());
        routes.connect(seq, synth, inbox.clone());
        routes.set_channel(seq, synth, Some(4));
        routes.set_channel(seq, synth, None);
        for channel in 0..16u8 {
            routes.send(seq, &Event::now(vec![0x90 | channel, 60, 100]));
        }
        assert_eq!(drain(&inbox).len(), 16, "all sixteen parts go down it");
    }

    #[test]
    fn a_channel_outside_the_midi_range_is_refused_rather_than_wrapped() {
        let mut routes = Routes::default();
        let inbox = new_inbox();
        let (seq, synth) = (NodeId::nil(), NodeId::new());
        routes.connect(seq, synth, inbox.clone());
        routes.set_channel(seq, synth, Some(17));
        routes.send(seq, &Event::now(vec![0x90, 60, 100]));
        assert_eq!(
            drain(&inbox).len(),
            1,
            "a nonsense channel leaves the wire carrying everything"
        );
    }

    #[test]
    fn a_flooded_inbox_sheds_notes_but_keeps_note_offs() {
        let mut inbox = Inbox::default();
        // One note-off, then far more note-ons than the backlog holds.
        inbox.push(Event::now(vec![0x80, 60, 0]));
        for i in 0..BACKLOG * 2 {
            inbox.push(Event::now(vec![0x90, (i % 128) as u8, 100]));
        }
        assert_eq!(inbox.queue.len(), BACKLOG, "the backlog stays bounded");
        assert!(
            inbox.queue.iter().any(|e| e.data == vec![0x80, 60, 0]),
            "the note-off survived the flood, so nothing is left stuck on"
        );
    }
}
