//! Host runtime for CLAP plugins compiled to `wk:clap` components (see
//! `plugins/clap`). wk drives such a component the way a CLAP host
//! drives a native plugin: instantiate → create → init → activate →
//! start-processing → `process()` per block. CLAP components run on the *main*
//! plugin engine (one engine for every node); the async calls are `block_on`'d so
//! the audio pump keeps a plain sync API.

use wasmtime::component::{Component, Linker, ResourceAny};
use wasmtime::Result;
use wasmtime::{Engine, Store};

wasmtime::component::bindgen!({
    path: "wit-clap",
    world: "plugin",
    // Exports are async: CLAP components run on the main engine, which has
    // async_support on (so every wasm call must be async). The audio pump
    // `block_on`s them — `process()` never awaits host I/O, so it completes in
    // one poll.
    exports: { default: async },
    // Reuse the main engine's wasi-gfx host types: a GUI plugin's surface is
    // composited by the same host impl every other node uses (the main linker
    // provides these; this bindgen only needs the types to line up).
    with: {
        "wasi:io/poll.pollable": wasmtime_wasi::p2::DynPollable,
        "wasi:surface/surface.surface": crate::plugin::SurfaceState,
        "wasi:graphics-context/graphics-context.context": crate::plugin::ContextState,
        "wasi:graphics-context/graphics-context.abstract-buffer": crate::plugin::AbstractBufferState,
        "wasi:frame-buffer/frame-buffer.device": crate::plugin::DeviceState,
        "wasi:frame-buffer/frame-buffer.buffer": crate::plugin::BufferState,
    },
});

pub use exports::wk::clap::plugins::ProcessResult;
pub use wk::clap::types::{
    AudioPortInfo, Descriptor, Event, LogSeverity, Midi, Note, NotePortInfo, ParamInfo,
    ProcessStatus, Supported, Transport,
};

/// One audio port's samples for a block: channels × frames.
pub type AudioBuffer = Vec<Vec<f32>>;

// CLAP components run on the *main* plugin engine — the one that already links
// wasi-gfx / wk:midi / wk:options / sockets — so a node's capabilities come from
// the interfaces it imports, with no separate "CLAP node" runtime. The store
// data is the shared `HostState`; the wk:clap `host` import is implemented on it
// below and added to the main linker.
struct HasClapHostMain;
impl wasmtime::component::HasData for HasClapHostMain {
    type Data<'a> = &'a mut crate::plugin::HostState;
}

/// Add the wk:clap `host` import to the main plugin linker.
pub fn add_host_to_linker(linker: &mut Linker<crate::plugin::HostState>) -> Result<()> {
    wk::clap::host::add_to_linker::<_, HasClapHostMain>(linker, |s| s)
}

impl wk::clap::host::Host for crate::plugin::HostState {
    fn log(&mut self, severity: LogSeverity, message: String) {
        let _ = severity;
        eprintln!("[clap] {message}");
    }
    fn request_restart(&mut self) {}
    fn request_process(&mut self) {}
    fn request_callback(&mut self) {}
    fn params_rescan(&mut self, _flags: u32) {}
    fn params_clear(&mut self, _param_id: u32, _flags: u32) {}
    fn state_mark_dirty(&mut self) {}
    fn latency_changed(&mut self) {}
}

use crate::plugin::HostState;

/// Instantiate a CLAP component on the main engine, create its first plugin, and
/// bring it up to the point of processing (create → init → activate → start).
/// The main engine is async, so this drives the lifecycle with `block_on`; the
/// resulting `ClapInstance` exposes a plain sync API.
pub fn instantiate(
    engine: &Engine,
    linker: &Linker<HostState>,
    state: HostState,
    component: &Component,
    sample_rate: f64,
    max_frames: u32,
) -> Result<ClapInstance> {
    let mut store = Store::new(engine, state);
    // A CLAP node is stopped by removing it from the mix, not by the frame-kill
    // epoch — so its calls just keep going when the epoch ticks (and this avoids
    // the deadline overflow a huge fixed deadline would hit on a live server).
    store.set_epoch_deadline(1);
    store.epoch_deadline_callback(|_| Ok(wasmtime::UpdateDeadline::Continue(1)));
    let (world, handle, descriptor, out_channels) = pollster::block_on(async {
        let world = Plugin::instantiate_async(&mut store, component, linker).await?;
        let plugins = world.wk_clap_plugins();
        if plugins.call_count(&mut store).await? == 0 {
            return Err(wasmtime::Error::msg("component exposes no CLAP plugins"));
        }
        let desc = plugins
            .call_get(&mut store, 0)
            .await?
            .ok_or_else(|| wasmtime::Error::msg("plugin 0 has no descriptor"))?;
        let handle = plugins
            .call_create(&mut store, &desc.id)
            .await?
            .ok_or_else(|| wasmtime::Error::msg(format!("create returned none for {}", desc.id)))?;
        let p = plugins.plugin();
        if !p.call_init(&mut store, handle).await? {
            return Err(wasmtime::Error::msg("plugin init failed"));
        }
        if !p
            .call_activate(&mut store, handle, sample_rate, 1, max_frames)
            .await?
        {
            return Err(wasmtime::Error::msg("plugin activate failed"));
        }
        if !p.call_start_processing(&mut store, handle).await? {
            return Err(wasmtime::Error::msg("plugin start_processing failed"));
        }
        let n_out = p.call_audio_port_count(&mut store, handle, false).await?;
        let mut out_channels = Vec::new();
        for i in 0..n_out {
            let ch = p
                .call_audio_port_info_at(&mut store, handle, i, false)
                .await?
                .map(|info| info.channel_count)
                .unwrap_or(0);
            out_channels.push(ch);
        }
        Ok::<_, wasmtime::Error>((world, handle, desc, out_channels))
    })?;

    Ok(ClapInstance {
        store,
        world,
        handle,
        descriptor,
        out_channels,
        steady: 0,
    })
}

/// A live CLAP plugin instance driven by the host, on the main engine.
pub struct ClapInstance {
    store: Store<HostState>,
    world: Plugin,
    handle: ResourceAny,
    pub descriptor: Descriptor,
    /// Channel count of each output audio port.
    pub out_channels: Vec<u32>,
    steady: i64,
}

impl ClapInstance {
    pub fn features(&mut self) -> Result<Supported> {
        let p = self.world.wk_clap_plugins();
        pollster::block_on(p.plugin().call_features(&mut self.store, self.handle))
    }

    pub fn note_input_count(&mut self) -> Result<u32> {
        let p = self.world.wk_clap_plugins();
        pollster::block_on(
            p.plugin()
                .call_note_port_count(&mut self.store, self.handle, true),
        )
    }

    pub fn param_count(&mut self) -> Result<u32> {
        let p = self.world.wk_clap_plugins();
        pollster::block_on(p.plugin().call_param_count(&mut self.store, self.handle))
    }

    pub fn param_info(&mut self, index: u32) -> Result<Option<ParamInfo>> {
        let p = self.world.wk_clap_plugins();
        pollster::block_on(
            p.plugin()
                .call_param_info_at(&mut self.store, self.handle, index),
        )
    }

    pub fn audio_output_info(&mut self, index: u32) -> Result<Option<AudioPortInfo>> {
        let p = self.world.wk_clap_plugins();
        pollster::block_on(p.plugin().call_audio_port_info_at(
            &mut self.store,
            self.handle,
            index,
            false,
        ))
    }

    /// Run one process block. Returns the plugin's status, output audio (one
    /// buffer per output port), and any events it emitted.
    pub fn process(
        &mut self,
        frames: u32,
        events: &[Event],
        transport: Option<Transport>,
        audio_in: &[AudioBuffer],
    ) -> Result<ProcessResult> {
        let steady = self.steady;
        self.steady += frames as i64;
        let p = self.world.wk_clap_plugins();
        pollster::block_on(p.plugin().call_process(
            &mut self.store,
            self.handle,
            steady,
            frames,
            transport,
            events,
            audio_in,
        ))
    }
}

/// Convert a raw MIDI message (as it arrives on a node's inbox) into a CLAP
/// input event — a passthrough `CLAP_EVENT_MIDI`, which CLAP instruments handle
/// alongside native note events. Short messages are zero-padded to three bytes.
pub fn midi_to_event(msg: &[u8]) -> Option<Event> {
    if msg.is_empty() {
        return None;
    }
    Some(Event::Midi(Midi {
        time: 0,
        port_index: 0,
        data: (
            msg[0],
            msg.get(1).copied().unwrap_or(0),
            msg.get(2).copied().unwrap_or(0),
        ),
    }))
}

/// Convert a CLAP event the plugin emitted into a raw MIDI message, so a CLAP
/// MIDI-effect (e.g. an arpeggiator) can drive downstream wk nodes. Note events
/// become MIDI note-on/off; raw MIDI passes through; other events are dropped.
pub fn event_to_midi(e: &Event) -> Option<Vec<u8>> {
    let chan = |c: i16| (c.max(0) as u8) & 0x0f;
    let key = |k: i16| k.clamp(0, 127) as u8;
    match e {
        Event::NoteOn(n) => Some(vec![
            0x90 | chan(n.channel),
            key(n.key),
            ((n.velocity * 127.0).round() as i64).clamp(1, 127) as u8,
        ]),
        Event::NoteOff(n) => Some(vec![0x80 | chan(n.channel), key(n.key), 0]),
        Event::Midi(m) => Some(vec![m.data.0, m.data.1, m.data.2]),
        _ => None,
    }
}

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use wk_protocol::NodeId;

/// Max block size a node is activated for (cpal callbacks are far smaller).
const MAX_FRAMES: u32 = 8192;

/// The shared CLAP audio engine: one output stream for the whole process, into
/// which every live CLAP node is mixed. Each audio callback runs the nodes in
/// topological order over the audio edges — draining each node's MIDI inbox,
/// `process()`-ing a block, threading a source's output into its destinations'
/// input, forwarding emitted events downstream, and summing the sinks (nodes with
/// no downstream audio wire) to the speakers.
///
/// Held by the `PluginHost` and shared with the audio thread. `add`/`remove` are
/// device-independent, so a node's start/stop/restart works even with no output
/// device (it just makes no sound).
#[derive(Clone)]
pub struct ClapAudio {
    inner: Arc<Mutex<Mixer>>,
}

struct Mixer {
    router: crate::midi::Router,
    sample_rate: f64,
    channels: usize,
    /// The output stream thread has been started (device opened or attempted).
    stream_started: bool,
    nodes: HashMap<NodeId, EngineNode>,
    /// Audio connections: source output -> destination input.
    edges: Vec<(NodeId, NodeId)>,
}

struct EngineNode {
    inst: ClapInstance,
    inbox: crate::midi::SharedInbox,
    /// Last block's output, cached so a downstream node can read it as input.
    last: Vec<AudioBuffer>,
}

impl ClapAudio {
    pub fn new(router: crate::midi::Router) -> Result<Self> {
        let mixer = Mixer {
            router,
            sample_rate: 48_000.0,
            channels: 2,
            stream_started: false,
            nodes: HashMap::new(),
            edges: Vec::new(),
        };
        Ok(ClapAudio {
            inner: Arc::new(Mutex::new(mixer)),
        })
    }

    /// Open the output stream if needed and report the device sample rate, so the
    /// caller can instantiate a node's plugin at the right rate before `add`.
    pub fn prepare(&self) -> f64 {
        self.ensure_stream();
        self.inner.lock().unwrap().sample_rate
    }

    /// The block size a node should be activated for.
    pub fn max_frames(&self) -> u32 {
        MAX_FRAMES
    }

    /// Insert a live, already-instantiated CLAP node into the mix.
    pub fn add(&self, id: NodeId, inst: ClapInstance, inbox: crate::midi::SharedInbox) {
        if let Ok(mut e) = self.inner.lock() {
            e.nodes.insert(
                id,
                EngineNode {
                    inst,
                    inbox,
                    last: Vec::new(),
                },
            );
        }
    }

    pub fn remove(&self, id: NodeId) {
        if let Ok(mut e) = self.inner.lock() {
            e.nodes.remove(&id);
        }
    }

    /// Set the audio connections (source output -> destination input).
    pub fn set_edges(&self, edges: Vec<(NodeId, NodeId)>) {
        if let Ok(mut e) = self.inner.lock() {
            e.edges = edges;
        }
    }

    /// Open the default output device and start the mixing stream (once). The
    /// stream lives on its own thread (cpal streams are !Send). Best-effort: with
    /// no device we keep default sample settings and never render.
    fn ensure_stream(&self) {
        if self.inner.lock().unwrap().stream_started {
            return;
        }
        use cpal::traits::{DeviceTrait, HostTrait};
        let picked = cpal::default_host()
            .default_output_device()
            .and_then(|d| d.default_output_config().ok().map(|c| (d, c)));

        let mut e = self.inner.lock().unwrap();
        e.stream_started = true;
        let Some((device, supported)) = picked else {
            eprintln!("wk clap: no audio output device; CLAP nodes will be silent");
            return;
        };
        if supported.sample_format() != cpal::SampleFormat::F32 {
            eprintln!("wk clap: output is not f32; CLAP nodes will be silent");
            return;
        }
        let cfg: cpal::StreamConfig = supported.config();
        e.sample_rate = cfg.sample_rate as f64;
        e.channels = cfg.channels.max(1) as usize;
        drop(e);

        let inner = self.inner.clone();
        std::thread::spawn(move || {
            use cpal::traits::{DeviceTrait, StreamTrait};
            let stream = device.build_output_stream(
                cfg,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| match inner.lock() {
                    Ok(mut e) => e.render(data),
                    Err(_) => data.fill(0.0),
                },
                |err| eprintln!("wk clap: audio stream error: {err}"),
                None,
            );
            match stream {
                Ok(s) => match s.play() {
                    Ok(()) => std::thread::park(), // keep the stream alive for the process life
                    Err(err) => eprintln!("wk clap: can't start audio: {err}"),
                },
                Err(err) => eprintln!("wk clap: can't open audio stream: {err}"),
            }
        });
    }
}

#[cfg(test)]
impl ClapAudio {
    /// Build an engine that never opens an audio device (for tests): nodes are
    /// added and `render` can be driven by hand at 48 kHz / stereo.
    fn new_headless(router: crate::midi::Router) -> Result<Self> {
        let a = Self::new(router)?;
        a.inner.lock().unwrap().stream_started = true;
        Ok(a)
    }

    /// Render one block synchronously (test-only; the device thread does this).
    fn render_block(&self, frames: usize, channels: usize) -> Vec<f32> {
        let mut e = self.inner.lock().unwrap();
        e.channels = channels;
        let mut data = vec![0.0f32; frames * channels];
        e.render(&mut data);
        data
    }
}

impl Mixer {
    /// Render one block: run every node in dependency order and mix the sinks.
    fn render(&mut self, data: &mut [f32]) {
        let channels = self.channels.max(1);
        let frames = data.len() / channels;
        data.fill(0.0);
        if self.nodes.is_empty() || frames == 0 {
            return;
        }
        // Nodes that feed another node aren't sinks; only sinks reach the speakers.
        let has_downstream: HashSet<NodeId> = self.edges.iter().map(|&(s, _)| s).collect();

        for id in self.topo_order() {
            // Sum any upstream nodes' cached output as this node's audio input.
            let inputs: Vec<NodeId> = self
                .edges
                .iter()
                .filter(|&&(_, d)| d == id)
                .map(|&(s, _)| s)
                .collect();
            let audio_in: Vec<AudioBuffer> = if inputs.is_empty() {
                Vec::new()
            } else {
                let mut buf: AudioBuffer = vec![vec![0.0; frames]; channels];
                for src in &inputs {
                    if let Some(port) = self.nodes.get(src).and_then(|n| n.last.first()) {
                        for (c, dst) in buf.iter_mut().enumerate() {
                            if let Some(ch) = port.get(c.min(port.len().saturating_sub(1))) {
                                for (f, v) in dst.iter_mut().enumerate().take(ch.len().min(frames))
                                {
                                    *v += ch[f];
                                }
                            }
                        }
                    }
                }
                vec![buf]
            };

            // Drain this node's MIDI inbox into events.
            let mut events = Vec::new();
            if let Some(n) = self.nodes.get(&id) {
                if let Ok(mut q) = n.inbox.lock() {
                    while let Some(m) = q.pop() {
                        if let Some(ev) = midi_to_event(&m) {
                            events.push(ev);
                        }
                    }
                }
            }

            let out = self
                .nodes
                .get_mut(&id)
                .map(|n| n.inst.process(frames as u32, &events, None, &audio_in));
            let Some(Ok(res)) = out else { continue };

            // Forward emitted events downstream (a MIDI effect / arp).
            if !res.out_events.is_empty() {
                if let Ok(router) = self.router.lock() {
                    for ev in &res.out_events {
                        if let Some(bytes) = event_to_midi(ev) {
                            router.send_from(id, &bytes);
                        }
                    }
                }
            }
            // Sinks reach the speakers.
            if !has_downstream.contains(&id) {
                if let Some(port) = res.audio_out.first() {
                    for f in 0..frames {
                        for c in 0..channels {
                            data[f * channels + c] += port
                                .get(c.min(port.len().saturating_sub(1)))
                                .and_then(|ch| ch.get(f).copied())
                                .unwrap_or(0.0);
                        }
                    }
                }
            }
            if let Some(n) = self.nodes.get_mut(&id) {
                n.last = res.audio_out;
            }
        }
    }

    /// Topological order over the audio edges (Kahn); any nodes left in a cycle
    /// are appended so they still render.
    fn topo_order(&self) -> Vec<NodeId> {
        let ids: Vec<NodeId> = self.nodes.keys().copied().collect();
        let mut indeg: HashMap<NodeId, usize> = ids.iter().map(|&id| (id, 0)).collect();
        for &(s, d) in &self.edges {
            if self.nodes.contains_key(&s) && self.nodes.contains_key(&d) {
                *indeg.entry(d).or_insert(0) += 1;
            }
        }
        let mut queue: Vec<NodeId> = ids.iter().copied().filter(|id| indeg[id] == 0).collect();
        let mut order = Vec::new();
        let mut i = 0;
        while i < queue.len() {
            let id = queue[i];
            i += 1;
            order.push(id);
            for &(s, d) in &self.edges {
                if s == id && self.nodes.contains_key(&d) {
                    if let Some(x) = indeg.get_mut(&d) {
                        *x = x.saturating_sub(1);
                        if *x == 0 {
                            queue.push(d);
                        }
                    }
                }
            }
        }
        for id in ids {
            if !order.contains(&id) {
                order.push(id);
            }
        }
        order
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a live CLAP instance on the main engine (as the host does).
    fn instance(bytes: &[u8], sr: f64, max: u32) -> ClapInstance {
        let host = crate::plugin::PluginHost::new().unwrap();
        host.clap_instance(host.bare_clap_state(), bytes, sr, max)
            .unwrap()
    }

    fn note_on(key: i16, time: u32) -> Event {
        Event::NoteOn(Note {
            time,
            flag_set: wk::clap::types::EventFlags::empty(),
            note_id: -1,
            port_index: 0,
            channel: 0,
            key,
            velocity: 1.0,
        })
    }

    fn set_param(id: u32, value: f64) -> Event {
        Event::ParamValue(wk::clap::types::ParamValue {
            time: 0,
            flag_set: wk::clap::types::EventFlags::empty(),
            param_id: id,
            note_id: -1,
            port_index: -1,
            channel: -1,
            key: -1,
            value,
        })
    }

    fn peak(chan: &[f32]) -> f32 {
        chan.iter().fold(0.0, |m, &x| m.max(x.abs()))
    }

    #[test]
    fn note_events_convert_to_raw_midi() {
        let on = Event::NoteOn(Note {
            time: 0,
            flag_set: wk::clap::types::EventFlags::empty(),
            note_id: -1,
            port_index: 0,
            channel: 0,
            key: 60,
            velocity: 1.0,
        });
        assert_eq!(event_to_midi(&on), Some(vec![0x90, 60, 127]));
    }

    /// The octaver is a note-in/note-out CLAP effect: every note it receives is
    /// echoed plus a copy an octave up, via CLAP output events. Proves the
    /// out-events path the host forwards downstream.
    #[test]
    fn octaver_doubles_notes_an_octave_up() {
        let bytes = include_bytes!("../testdata/octaver.wasm");
        let mut inst = instance(bytes, 48_000.0, 128);

        let ev = midi_to_event(&[0x90, 60, 100]).unwrap();
        let res = inst.process(16, &[ev], None, &[]).unwrap();
        let notes: Vec<u8> = res
            .out_events
            .iter()
            .filter_map(event_to_midi)
            .map(|m| m[1])
            .collect();
        assert!(notes.contains(&60), "echoes the original note");
        assert!(notes.contains(&72), "adds an octave up");
    }

    /// A node with a downstream audio edge is routed *into* its destination, not
    /// to the speakers. Two synths A and B, edge A→B: a note played on A produces
    /// audio, but because A feeds B (a synth that ignores audio input and has no
    /// note of its own), the speakers stay silent — proving the graph routes A's
    /// output away from the mix. Without the edge, A reaches the speakers.
    #[test]
    fn audio_edge_routes_a_node_away_from_the_speakers() {
        let synth = include_bytes!("../testdata/polysynth.wasm");
        let note = |inbox: &crate::midi::SharedInbox| {
            inbox.lock().unwrap().push(vec![0x90, 69, 100]);
        };

        let host = crate::plugin::PluginHost::new().unwrap();
        let mk = || {
            host.clap_instance(host.bare_clap_state(), synth, 48_000.0, 512)
                .unwrap()
        };

        // A alone: the note reaches the speakers.
        let solo = ClapAudio::new_headless(crate::midi::new_router()).unwrap();
        let a = crate::midi::new_inbox();
        solo.add(NodeId::new(), mk(), a.clone());
        note(&a);
        assert!(peak(&solo.render_block(256, 2)) > 0.0);

        // A → B: the note on A is routed into B (silent) — speakers stay quiet.
        let chain = ClapAudio::new_headless(crate::midi::new_router()).unwrap();
        let (ia, ib) = (crate::midi::new_inbox(), crate::midi::new_inbox());
        let (a_id, b_id) = (NodeId::new(), NodeId::new());
        chain.add(a_id, mk(), ia.clone());
        chain.add(b_id, mk(), ib.clone());
        chain.set_edges(vec![(a_id, b_id)]);
        note(&ia);
        assert_eq!(peak(&chain.render_block(256, 2)), 0.0);
    }

    #[test]
    fn midi_passthrough_becomes_a_clap_midi_event() {
        match midi_to_event(&[0x90, 60, 100]) {
            Some(Event::Midi(m)) => assert_eq!(m.data, (0x90, 60, 100)),
            _ => panic!("expected a midi event"),
        }
        // Short messages zero-pad; empty yields nothing.
        match midi_to_event(&[0xB0, 7]) {
            Some(Event::Midi(m)) => assert_eq!(m.data, (0xB0, 7, 0)),
            _ => panic!("expected a midi event"),
        }
        assert!(midi_to_event(&[]).is_none());
    }

    /// The delay is a CLAP *audio effect*: it transforms its audio input. Feed an
    /// impulse with a short delay time and full wet mix — the output should be
    /// silent at first (dry removed) then carry a time-shifted copy of the
    /// impulse, proving it processes audio input rather than passing it through.
    #[test]
    fn delay_produces_a_time_shifted_tap() {
        let bytes = include_bytes!("../testdata/delay.wasm");
        let mut inst = instance(bytes, 48_000.0, 512);
        assert_eq!(inst.param_count().unwrap(), 3);
        assert_eq!(inst.param_info(0).unwrap().unwrap().name, "Time");

        let mut l = vec![0.0f32; 256];
        let mut r = vec![0.0f32; 256];
        l[0] = 1.0;
        r[0] = 1.0;
        let audio_in = vec![vec![l, r]];
        // time = 2 ms (~96 samples @ 48k), mix = 1.0 (pure wet).
        let events = [set_param(0, 2.0), set_param(2, 1.0)];
        let out = inst.process(256, &events, None, &audio_in).unwrap();
        let ch = &out.audio_out[0][0];
        assert!(ch[0].abs() < 0.01, "dry removed at full wet mix");
        assert!(peak(&ch[10..]) > 0.5, "a delayed tap appears later");
    }

    /// The subtractive synth (saw → resonant filter → envelope, 5 params) sounds
    /// on a note-on and stays finite — a richer instrument through wk:clap.
    #[test]
    fn subsynth_sounds_and_stays_finite() {
        let bytes = include_bytes!("../testdata/subsynth.wasm");
        let mut inst = instance(bytes, 48_000.0, 512);
        assert_eq!(inst.param_count().unwrap(), 5);
        assert_eq!(inst.param_info(0).unwrap().unwrap().name, "Cutoff");

        let out = inst.process(512, &[note_on(57, 0)], None, &[]).unwrap();
        let ch = &out.audio_out[0][0];
        assert!(ch.iter().all(|s| s.is_finite()), "no NaN/inf");
        assert!(peak(ch) > 0.01, "should sound");
    }

    /// A note-on delivered as a raw MIDI message (the wire format) drives the
    /// polysynth — proving the inbox→event→process path the audio node uses.
    #[test]
    fn polysynth_sounds_from_raw_midi() {
        let bytes = include_bytes!("../testdata/polysynth.wasm");
        let mut inst = instance(bytes, 48_000.0, 512);
        let ev = midi_to_event(&[0x90, 69, 100]).unwrap();
        let out = inst.process(512, &[ev], None, &[]).unwrap();
        assert!(peak(&out.audio_out[0][0]) > 0.05);
    }

    /// The vendored `plugin-template.c` is an L/R-swap stereo effect with one
    /// note input and the audio-ports/note-ports/state extensions. Drive it
    /// through the whole lifecycle and confirm it swaps channels — proving the
    /// wk:clap host path end to end.
    #[test]
    fn clap_template_swaps_channels_and_reports_ports() {
        let bytes = include_bytes!("../testdata/clap-template.wasm");
        let mut inst = instance(bytes, 48_000.0, 128);

        // Extensions the plugin implements.
        let feats = inst.features().unwrap();
        assert!(feats.contains(Supported::AUDIO_PORTS));
        assert!(feats.contains(Supported::NOTE_PORTS));
        assert!(feats.contains(Supported::STATE));
        assert_eq!(inst.note_input_count().unwrap(), 1);
        assert_eq!(inst.out_channels, vec![2]);

        // One stereo input port, four frames; the effect swaps L and R.
        let audio_in = vec![vec![vec![1.0, 2.0, 3.0, 4.0], vec![5.0, 6.0, 7.0, 8.0]]];
        let res = inst.process(4, &[note_on(60, 0)], None, &audio_in).unwrap();
        assert!(matches!(res.status, ProcessStatus::Continue));
        assert_eq!(res.audio_out.len(), 1);
        assert_eq!(res.audio_out[0][0], vec![5.0, 6.0, 7.0, 8.0]);
        assert_eq!(res.audio_out[0][1], vec![1.0, 2.0, 3.0, 4.0]);
    }

    /// The gain effect exposes one automatable parameter and scales its input by
    /// it — proving the params extension and PARAM_VALUE automation in process().
    #[test]
    fn gain_applies_its_parameter() {
        let bytes = include_bytes!("../testdata/gain.wasm");
        let mut inst = instance(bytes, 48_000.0, 128);

        assert!(inst.features().unwrap().contains(Supported::PARAMS));
        assert_eq!(inst.param_count().unwrap(), 1);
        assert_eq!(inst.param_info(0).unwrap().unwrap().name, "Gain");

        let ones = vec![vec![vec![1.0f32; 4], vec![1.0f32; 4]]];
        // Default gain is 1.0 → passthrough.
        let res = inst.process(4, &[], None, &ones).unwrap();
        assert_eq!(res.audio_out[0][0], vec![1.0; 4]);
        // Automate to 0.5 → halved.
        let res = inst.process(4, &[set_param(0, 0.5)], None, &ones).unwrap();
        assert_eq!(res.audio_out[0][0], vec![0.5; 4]);
    }

    /// The polysynth is silent until a note-on, then produces audio on its stereo
    /// output — proving the instrument path (note-ports in, audio-ports out).
    #[test]
    fn polysynth_sounds_on_note_on() {
        let bytes = include_bytes!("../testdata/polysynth.wasm");
        let mut inst = instance(bytes, 48_000.0, 512);

        let feats = inst.features().unwrap();
        assert!(feats.contains(Supported::NOTE_PORTS));
        assert!(feats.contains(Supported::AUDIO_PORTS));
        assert_eq!(inst.note_input_count().unwrap(), 1);
        assert_eq!(inst.out_channels, vec![2]);

        // No notes yet → silence.
        let quiet = inst.process(256, &[], None, &[]).unwrap();
        assert_eq!(peak(&quiet.audio_out[0][0]), 0.0);

        // Note-on A4 → audible output on both channels.
        let loud = inst.process(512, &[note_on(69, 0)], None, &[]).unwrap();
        assert!(
            peak(&loud.audio_out[0][0]) > 0.05,
            "left channel should sound"
        );
        assert!(
            peak(&loud.audio_out[0][1]) > 0.05,
            "right channel should sound"
        );
    }
}
