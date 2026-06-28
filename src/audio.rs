//! Host side of wk's audio: a minimal subset of the Web Audio API, implemented
//! on top of `web-audio-api-rs`. Each plugin gets its own `AudioContext` (one
//! output stream on the device) and builds an oscillator/gain graph the same
//! way a web page would. This is the audio analogue of the wasi:webgpu host.

use std::sync::Arc;

use wasmtime::component::{HasData, Linker, Resource};
use wasmtime::Result;
use wasmtime_wasi_io::IoView;
use web_audio_api::context::{AudioContext, AudioContextOptions, BaseAudioContext};
use web_audio_api::node::{
    AudioNode, AudioScheduledSourceNode, BiquadFilterNode, BiquadFilterType, GainNode,
    OscillatorNode, OscillatorType as WaType,
};

wasmtime::component::bindgen!({
    path: "wit-audio",
    world: "audio-host",
    imports: { default: trappable },
    require_store_data_send: true,
    with: {
        "wk:webaudio/audio.context": AudioCtx,
        "wk:webaudio/audio.oscillator": Osc,
        "wk:webaudio/audio.gain": Gain,
        "wk:webaudio/audio.biquad-filter": Filter,
    },
});

use crate::plugin::HostState;
use wk::webaudio::audio::{FilterType, OscillatorType};

/// Resource representations stored in the wasmtime `ResourceTable`. Nodes keep
/// an `Arc` to their context so they can reach its destination (speakers).
pub struct AudioCtx {
    ctx: Arc<AudioContext>,
}
pub struct Osc {
    node: OscillatorNode,
    ctx: Arc<AudioContext>,
}
pub struct Gain {
    node: GainNode,
    ctx: Arc<AudioContext>,
}
pub struct Filter {
    node: BiquadFilterNode,
    ctx: Arc<AudioContext>,
}

/// Add wk's Web Audio interface to the linker.
pub fn add_to_linker(l: &mut Linker<HostState>) -> Result<()> {
    wk::webaudio::audio::add_to_linker::<_, HasAudio>(l, |s| s)?;
    Ok(())
}

struct HasAudio;
impl HasData for HasAudio {
    type Data<'a> = &'a mut HostState;
}

impl wk::webaudio::audio::Host for HostState {}

impl wk::webaudio::audio::HostContext for HostState {
    fn new(&mut self) -> Result<Resource<AudioCtx>> {
        let ctx = Arc::new(AudioContext::new(AudioContextOptions::default()));
        Ok(self.table().push(AudioCtx { ctx })?)
    }

    fn sample_rate(&mut self, this: Resource<AudioCtx>) -> Result<f32> {
        Ok(self.table().get(&this)?.ctx.sample_rate())
    }

    fn current_time(&mut self, this: Resource<AudioCtx>) -> Result<f64> {
        Ok(self.table().get(&this)?.ctx.current_time())
    }

    fn create_oscillator(&mut self, this: Resource<AudioCtx>) -> Result<Resource<Osc>> {
        let ctx = self.table().get(&this)?.ctx.clone();
        let node = ctx.create_oscillator();
        Ok(self.table().push(Osc { node, ctx })?)
    }

    fn create_gain(&mut self, this: Resource<AudioCtx>) -> Result<Resource<Gain>> {
        let ctx = self.table().get(&this)?.ctx.clone();
        let node = ctx.create_gain();
        Ok(self.table().push(Gain { node, ctx })?)
    }

    fn create_biquad_filter(&mut self, this: Resource<AudioCtx>) -> Result<Resource<Filter>> {
        let ctx = self.table().get(&this)?.ctx.clone();
        let node = ctx.create_biquad_filter();
        Ok(self.table().push(Filter { node, ctx })?)
    }

    fn drop(&mut self, this: Resource<AudioCtx>) -> Result<()> {
        self.table().delete(this)?;
        Ok(())
    }
}

impl wk::webaudio::audio::HostOscillator for HostState {
    fn set_type(&mut self, this: Resource<Osc>, ty: OscillatorType) -> Result<()> {
        let kind = match ty {
            OscillatorType::Sine => WaType::Sine,
            OscillatorType::Square => WaType::Square,
            OscillatorType::Sawtooth => WaType::Sawtooth,
            OscillatorType::Triangle => WaType::Triangle,
        };
        self.table().get_mut(&this)?.node.set_type(kind);
        Ok(())
    }

    fn set_frequency(&mut self, this: Resource<Osc>, hz: f32) -> Result<()> {
        self.table().get(&this)?.node.frequency().set_value(hz);
        Ok(())
    }

    fn set_detune(&mut self, this: Resource<Osc>, cents: f32) -> Result<()> {
        self.table().get(&this)?.node.detune().set_value(cents);
        Ok(())
    }

    fn connect(&mut self, this: Resource<Osc>, dst: Resource<Gain>) -> Result<()> {
        let table = self.table();
        let gain = table.get(&dst)?;
        let osc = table.get(&this)?;
        osc.node.connect(&gain.node);
        Ok(())
    }

    fn connect_filter(&mut self, this: Resource<Osc>, dst: Resource<Filter>) -> Result<()> {
        let table = self.table();
        let filter = table.get(&dst)?;
        let osc = table.get(&this)?;
        osc.node.connect(&filter.node);
        Ok(())
    }

    fn connect_destination(&mut self, this: Resource<Osc>) -> Result<()> {
        let osc = self.table().get(&this)?;
        osc.node.connect(&osc.ctx.destination());
        Ok(())
    }

    fn start(&mut self, this: Resource<Osc>, when: f64) -> Result<()> {
        self.table().get_mut(&this)?.node.start_at(when);
        Ok(())
    }

    fn stop(&mut self, this: Resource<Osc>, when: f64) -> Result<()> {
        self.table().get_mut(&this)?.node.stop_at(when);
        Ok(())
    }

    fn drop(&mut self, this: Resource<Osc>) -> Result<()> {
        self.table().delete(this)?;
        Ok(())
    }
}

impl wk::webaudio::audio::HostGain for HostState {
    fn set_gain(&mut self, this: Resource<Gain>, value: f32) -> Result<()> {
        self.table().get(&this)?.node.gain().set_value(value);
        Ok(())
    }

    fn ramp_to(&mut self, this: Resource<Gain>, value: f32, seconds: f32) -> Result<()> {
        let g = self.table().get(&this)?;
        let now = g.ctx.current_time();
        let param = g.node.gain();
        // Anchor at the current value, then ramp — a click-free envelope segment.
        param.cancel_scheduled_values(now);
        param.set_value_at_time(param.value(), now);
        param.linear_ramp_to_value_at_time(value, now + seconds as f64);
        Ok(())
    }

    fn connect(&mut self, this: Resource<Gain>, dst: Resource<Gain>) -> Result<()> {
        let table = self.table();
        let target = table.get(&dst)?;
        let gain = table.get(&this)?;
        gain.node.connect(&target.node);
        Ok(())
    }

    fn connect_destination(&mut self, this: Resource<Gain>) -> Result<()> {
        let gain = self.table().get(&this)?;
        gain.node.connect(&gain.ctx.destination());
        Ok(())
    }

    fn drop(&mut self, this: Resource<Gain>) -> Result<()> {
        self.table().delete(this)?;
        Ok(())
    }
}

impl wk::webaudio::audio::HostBiquadFilter for HostState {
    fn set_type(&mut self, this: Resource<Filter>, ty: FilterType) -> Result<()> {
        let kind = match ty {
            FilterType::Lowpass => BiquadFilterType::Lowpass,
            FilterType::Highpass => BiquadFilterType::Highpass,
            FilterType::Bandpass => BiquadFilterType::Bandpass,
        };
        self.table().get_mut(&this)?.node.set_type(kind);
        Ok(())
    }

    fn set_frequency(&mut self, this: Resource<Filter>, hz: f32) -> Result<()> {
        self.table().get(&this)?.node.frequency().set_value(hz);
        Ok(())
    }

    fn set_q(&mut self, this: Resource<Filter>, value: f32) -> Result<()> {
        self.table().get(&this)?.node.q().set_value(value);
        Ok(())
    }

    fn connect(&mut self, this: Resource<Filter>, dst: Resource<Gain>) -> Result<()> {
        let table = self.table();
        let gain = table.get(&dst)?;
        let filter = table.get(&this)?;
        filter.node.connect(&gain.node);
        Ok(())
    }

    fn connect_destination(&mut self, this: Resource<Filter>) -> Result<()> {
        let filter = self.table().get(&this)?;
        filter.node.connect(&filter.ctx.destination());
        Ok(())
    }

    fn drop(&mut self, this: Resource<Filter>) -> Result<()> {
        self.table().delete(this)?;
        Ok(())
    }
}
