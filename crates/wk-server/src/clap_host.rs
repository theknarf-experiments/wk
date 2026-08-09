//! Host runtime for CLAP plugins compiled to `wk:clap` components (see
//! `plugins/clap-template`). wk drives such a component the way a CLAP host
//! drives a native plugin: instantiate → create → init → activate →
//! start-processing → `process()` per block. This module owns a dedicated,
//! **synchronous** wasmtime engine (the audio pump calls `process` on a normal
//! thread, so blocking sync calls are simplest) and provides the `wk:clap`
//! `host` imports the plugin may call.

use wasmtime::component::{Component, Linker, ResourceAny, ResourceTable};
use wasmtime::Result;
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};
use wasmtime_wasi_io::IoView;

wasmtime::component::bindgen!({
    path: "wit-clap",
    world: "plugin",
});

pub use exports::wk::clap::plugins::ProcessResult;
pub use wk::clap::types::{
    AudioPortInfo, Descriptor, Event, LogSeverity, Midi, Note, NotePortInfo, ParamInfo,
    ProcessStatus, Supported, Transport,
};

/// One audio port's samples for a block: channels × frames.
pub type AudioBuffer = Vec<Vec<f32>>;

/// Store data for a CLAP instance: wasi context + the `host` callbacks the
/// plugin can invoke.
pub struct ClapHost {
    table: ResourceTable,
    wasi: WasiCtx,
    /// Log lines the plugin emitted (severity, message), newest last.
    pub logs: Vec<(LogSeverity, String)>,
    /// The plugin asked to be reactivated (e.g. latency changed).
    pub restart_requested: bool,
    /// The plugin marked its state dirty (should be saved).
    pub state_dirty: bool,
}

impl ClapHost {
    fn new() -> Self {
        ClapHost {
            table: ResourceTable::new(),
            wasi: WasiCtxBuilder::new().build(),
            logs: Vec::new(),
            restart_requested: false,
            state_dirty: false,
        }
    }
}

impl IoView for ClapHost {
    fn table(&mut self) -> &mut ResourceTable {
        &mut self.table
    }
}
impl WasiView for ClapHost {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl wk::clap::host::Host for ClapHost {
    fn log(&mut self, severity: LogSeverity, message: String) {
        self.logs.push((severity, message));
    }
    fn request_restart(&mut self) {
        self.restart_requested = true;
    }
    fn request_process(&mut self) {}
    fn request_callback(&mut self) {}
    fn params_rescan(&mut self, _flags: u32) {}
    fn params_clear(&mut self, _param_id: u32, _flags: u32) {}
    fn state_mark_dirty(&mut self) {
        self.state_dirty = true;
    }
    fn latency_changed(&mut self) {}
}

struct HasClapHost;
impl wasmtime::component::HasData for HasClapHost {
    type Data<'a> = &'a mut ClapHost;
}

/// A compiled-and-linked CLAP engine, reused across instances.
pub struct ClapEngine {
    engine: Engine,
    linker: Linker<ClapHost>,
}

impl ClapEngine {
    pub fn new() -> Result<Self> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        let engine = Engine::new(&config)?;
        let mut linker = Linker::<ClapHost>::new(&engine);
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)?;
        wk::clap::host::add_to_linker::<_, HasClapHost>(&mut linker, |s| s)?;
        Ok(ClapEngine { engine, linker })
    }

    pub fn compile(&self, bytes: &[u8]) -> Result<Component> {
        Component::new(&self.engine, bytes)
    }

    /// Instantiate a component, create its first plugin, and bring it up to the
    /// point of processing (`init` → `activate` → `start-processing`).
    pub fn instantiate(
        &self,
        component: &Component,
        sample_rate: f64,
        max_frames: u32,
    ) -> Result<ClapInstance> {
        let mut store = Store::new(&self.engine, ClapHost::new());
        let world = Plugin::instantiate(&mut store, component, &self.linker)?;
        let plugins = world.wk_clap_plugins();

        let count = plugins.call_count(&mut store)?;
        if count == 0 {
            return Err(wasmtime::Error::msg("component exposes no CLAP plugins"));
        }
        let desc = plugins
            .call_get(&mut store, 0)?
            .ok_or_else(|| wasmtime::Error::msg("plugin 0 has no descriptor"))?;
        let handle = plugins
            .call_create(&mut store, &desc.id)?
            .ok_or_else(|| wasmtime::Error::msg(format!("create returned none for {}", desc.id)))?;

        let p = plugins.plugin();
        if !p.call_init(&mut store, handle)? {
            return Err(wasmtime::Error::msg("plugin init failed"));
        }
        if !p.call_activate(&mut store, handle, sample_rate, 1, max_frames)? {
            return Err(wasmtime::Error::msg("plugin activate failed"));
        }
        if !p.call_start_processing(&mut store, handle)? {
            return Err(wasmtime::Error::msg("plugin start_processing failed"));
        }

        // Cache output-port channel counts (needed to size the audio the pump
        // routes on).
        let n_out = p.call_audio_port_count(&mut store, handle, false)?;
        let mut out_channels = Vec::new();
        for i in 0..n_out {
            let ch = p
                .call_audio_port_info_at(&mut store, handle, i, false)?
                .map(|info| info.channel_count)
                .unwrap_or(0);
            out_channels.push(ch);
        }

        Ok(ClapInstance {
            store,
            world,
            handle,
            descriptor: desc,
            out_channels,
            steady: 0,
        })
    }
}

/// A live CLAP plugin instance driven by the host.
pub struct ClapInstance {
    store: Store<ClapHost>,
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
        p.plugin().call_features(&mut self.store, self.handle)
    }

    pub fn note_input_count(&mut self) -> Result<u32> {
        let p = self.world.wk_clap_plugins();
        p.plugin()
            .call_note_port_count(&mut self.store, self.handle, true)
    }

    pub fn param_count(&mut self) -> Result<u32> {
        let p = self.world.wk_clap_plugins();
        p.plugin().call_param_count(&mut self.store, self.handle)
    }

    pub fn param_info(&mut self, index: u32) -> Result<Option<ParamInfo>> {
        let p = self.world.wk_clap_plugins();
        p.plugin()
            .call_param_info_at(&mut self.store, self.handle, index)
    }

    pub fn audio_output_info(&mut self, index: u32) -> Result<Option<AudioPortInfo>> {
        let p = self.world.wk_clap_plugins();
        p.plugin()
            .call_audio_port_info_at(&mut self.store, self.handle, index, false)
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
        p.plugin().call_process(
            &mut self.store,
            self.handle,
            steady,
            frames,
            transport,
            events,
            audio_in,
        )
    }

    /// Drain and return the log lines the plugin emitted so far.
    pub fn take_logs(&mut self) -> Vec<(LogSeverity, String)> {
        std::mem::take(&mut self.store.data_mut().logs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// The vendored `plugin-template.c` is an L/R-swap stereo effect with one
    /// note input and the audio-ports/note-ports/state extensions. Drive it
    /// through the whole lifecycle and confirm it swaps channels — proving the
    /// wk:clap host path end to end.
    #[test]
    fn clap_template_swaps_channels_and_reports_ports() {
        let bytes = include_bytes!("../testdata/clap-template.wasm");
        let engine = ClapEngine::new().unwrap();
        let component = engine.compile(bytes).unwrap();
        let mut inst = engine.instantiate(&component, 48_000.0, 128).unwrap();

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
        let engine = ClapEngine::new().unwrap();
        let component = engine.compile(bytes).unwrap();
        let mut inst = engine.instantiate(&component, 48_000.0, 128).unwrap();

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
        let engine = ClapEngine::new().unwrap();
        let component = engine.compile(bytes).unwrap();
        let mut inst = engine.instantiate(&component, 48_000.0, 512).unwrap();

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
