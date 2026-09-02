//! Host side of wk's audio: a minimal subset of the Web Audio API, implemented
//! on top of `web-audio-api-rs`. Each plugin gets its own `AudioContext` (one
//! output stream on the device) and builds an oscillator/gain graph the same
//! way a web page would. This is the audio analogue of the wasi:webgpu host.

use std::collections::VecDeque;
use std::sync::Arc;

use wasmtime::component::{HasData, Linker, Resource};
use wasmtime::Result;
use wasmtime_wasi_io::IoView;
use web_audio_api::context::{AudioContext, AudioContextOptions, BaseAudioContext};
use web_audio_api::node::{
    AudioBufferSourceNode, AudioNode, AudioScheduledSourceNode, BiquadFilterNode, BiquadFilterType,
    GainNode, OscillatorNode, OscillatorType as WaType,
};
use web_audio_api::AudioBuffer;

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
        "wk:webaudio/audio.pcm-queue": PcmQueue,
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

/// A push-style PCM sink (the SDL_QueueAudio analogue). Chunks written by the
/// guest become one-shot `AudioBufferSourceNode`s scheduled back to back on
/// the context clock, all feeding a persistent unit-gain hub node — so
/// `connect`/`connect-destination` wire the hub once and every future chunk
/// flows through it.
pub struct PcmQueue {
    /// Persistent output hub; chunk sources connect here.
    out: GainNode,
    ctx: Arc<AudioContext>,
    /// The queue's own sample rate; buffers at this rate are resampled by the
    /// context automatically if it runs at a different rate.
    sample_rate: f32,
    channels: u32,
    /// Context-clock second where the next written chunk starts.
    next_time: f64,
    /// Live one-shot sources with their scheduled end times, oldest first.
    /// Kept alive until played out — dropping a source early may stop it.
    live: VecDeque<(AudioBufferSourceNode, f64)>,
}

/// Safety lead applied when the queue has drained: a fresh chunk starts this
/// many seconds in the future so it never lands in the past and glitches.
const PCM_LEAD_SECONDS: f64 = 0.03;

/// Context-clock start time for the next chunk: gapless after the previous
/// chunk, or `now` plus a small safety lead if the queue has drained.
fn pcm_chunk_start(next_time: f64, current_time: f64) -> f64 {
    next_time.max(current_time + PCM_LEAD_SECONDS)
}

/// Context-clock end time of a chunk of `frames` frames starting at `start`.
fn pcm_chunk_end(start: f64, frames: usize, sample_rate: f32) -> f64 {
    start + frames as f64 / sample_rate as f64
}

/// Seconds of audio queued but not yet played.
fn pcm_buffered(next_time: f64, current_time: f64) -> f64 {
    (next_time - current_time).max(0.0)
}

/// Split interleaved samples into per-channel planes, truncating any trailing
/// partial frame.
fn deinterleave(samples: &[f32], channels: usize) -> Vec<Vec<f32>> {
    let frames = samples.len() / channels;
    let mut planes = vec![Vec::with_capacity(frames); channels];
    for frame in samples.chunks_exact(channels) {
        for (plane, &sample) in planes.iter_mut().zip(frame) {
            plane.push(sample);
        }
    }
    planes
}

/// Drop entries that finished playing before `now`. Entries are in schedule
/// order, so ends are non-decreasing and we only need to pop from the front;
/// a source is never dropped before its end time has passed.
fn reap_finished<T>(live: &mut VecDeque<(T, f64)>, now: f64) {
    while live.front().is_some_and(|(_, end)| *end < now) {
        live.pop_front();
    }
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

    fn create_pcm_queue(
        &mut self,
        this: Resource<AudioCtx>,
        sample_rate: f32,
        channels: u32,
    ) -> Result<Resource<PcmQueue>> {
        if !(1..=2).contains(&channels) {
            return Err(wasmtime::Error::msg(format!(
                "pcm-queue supports 1 or 2 channels, got {channels}"
            )));
        }
        if !(sample_rate.is_finite() && sample_rate > 0.0) {
            return Err(wasmtime::Error::msg(format!(
                "pcm-queue sample rate must be positive, got {sample_rate}"
            )));
        }
        let ctx = self.table().get(&this)?.ctx.clone();
        let out = ctx.create_gain();
        out.gain().set_value(1.0);
        Ok(self.table().push(PcmQueue {
            out,
            ctx,
            sample_rate,
            channels,
            next_time: 0.0,
            live: VecDeque::new(),
        })?)
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

    fn ramp_at(
        &mut self,
        this: Resource<Gain>,
        start_value: f32,
        end_value: f32,
        seconds: f32,
        when: f64,
    ) -> Result<()> {
        let g = self.table().get(&this)?;
        // An instant already past is the caller running late; start at once
        // rather than schedule into the past, which the param would ignore.
        let at = when.max(g.ctx.current_time());
        let param = g.node.gain();
        // Cancel only from `at` onward, so an envelope segment already running
        // keeps its shape right up to the moment this one takes over.
        param.cancel_scheduled_values(at);
        param.set_value_at_time(start_value, at);
        param.linear_ramp_to_value_at_time(end_value, at + seconds as f64);
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

impl wk::webaudio::audio::HostPcmQueue for HostState {
    fn buffered(&mut self, this: Resource<PcmQueue>) -> Result<f64> {
        let q = self.table().get(&this)?;
        Ok(pcm_buffered(q.next_time, q.ctx.current_time()))
    }

    fn write(&mut self, this: Resource<PcmQueue>, samples: Vec<f32>) -> Result<()> {
        let q = self.table().get_mut(&this)?;
        let channels = q.channels as usize;
        let frames = samples.len() / channels;
        if frames == 0 {
            return Ok(());
        }
        let now = q.ctx.current_time();
        reap_finished(&mut q.live, now);

        let buffer = AudioBuffer::from(deinterleave(&samples, channels), q.sample_rate);
        let mut src = q.ctx.create_buffer_source();
        src.set_buffer(buffer);
        src.connect(&q.out);
        let start = pcm_chunk_start(q.next_time, now);
        src.start_at(start);
        let end = pcm_chunk_end(start, frames, q.sample_rate);
        q.next_time = end;
        q.live.push_back((src, end));
        Ok(())
    }

    fn connect(&mut self, this: Resource<PcmQueue>, dst: Resource<Gain>) -> Result<()> {
        let table = self.table();
        let gain = table.get(&dst)?;
        let queue = table.get(&this)?;
        queue.out.connect(&gain.node);
        Ok(())
    }

    fn connect_destination(&mut self, this: Resource<PcmQueue>) -> Result<()> {
        let queue = self.table().get(&this)?;
        queue.out.connect(&queue.ctx.destination());
        Ok(())
    }

    fn drop(&mut self, this: Resource<PcmQueue>) -> Result<()> {
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

// The pcm-queue scheduling math is tested here without a `PcmQueue`: building
// one needs a real `AudioContext`, which opens an output device via cpal — not
// something a unit test suite should do.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drained_queue_starts_with_safety_lead() {
        // next_time is in the past (queue drained): schedule slightly ahead of
        // the clock so the chunk doesn't start in the past and glitch.
        let start = pcm_chunk_start(1.0, 5.0);
        assert_eq!(start, 5.0 + PCM_LEAD_SECONDS);
    }

    #[test]
    fn busy_queue_appends_gaplessly() {
        // next_time is ahead of the clock: the chunk butts up against the
        // previous one exactly.
        let start = pcm_chunk_start(5.5, 5.0);
        assert_eq!(start, 5.5);
    }

    #[test]
    fn chunk_end_advances_by_frames_over_rate() {
        // 4800 frames at 48 kHz = 100 ms.
        let end = pcm_chunk_end(2.0, 4800, 48_000.0);
        assert!((end - 2.1).abs() < 1e-9);
    }

    #[test]
    fn consecutive_writes_schedule_back_to_back() {
        let rate = 44_100.0;
        let now = 0.5;
        let mut next_time = 0.0;

        let first = pcm_chunk_start(next_time, now);
        next_time = pcm_chunk_end(first, 4410, rate);
        let second = pcm_chunk_start(next_time, now);

        assert_eq!(first, now + PCM_LEAD_SECONDS);
        assert_eq!(second, first + 0.1); // gapless: starts where the first ends
        assert!((pcm_buffered(pcm_chunk_end(second, 4410, rate), now) - 0.23).abs() < 1e-9);
    }

    #[test]
    fn buffered_clamps_to_zero_when_drained() {
        assert_eq!(pcm_buffered(1.0, 3.0), 0.0);
        assert!((pcm_buffered(3.25, 3.0) - 0.25).abs() < 1e-12);
    }

    #[test]
    fn deinterleave_splits_stereo() {
        let planes = deinterleave(&[1.0, -1.0, 2.0, -2.0, 3.0, -3.0], 2);
        assert_eq!(planes, vec![vec![1.0, 2.0, 3.0], vec![-1.0, -2.0, -3.0]]);
    }

    #[test]
    fn deinterleave_truncates_partial_frame() {
        // 5 samples over 2 channels: the trailing odd sample is dropped.
        let planes = deinterleave(&[1.0, -1.0, 2.0, -2.0, 9.0], 2);
        assert_eq!(planes, vec![vec![1.0, 2.0], vec![-1.0, -2.0]]);
    }

    #[test]
    fn deinterleave_mono_is_passthrough() {
        let planes = deinterleave(&[0.25, 0.5, 0.75], 1);
        assert_eq!(planes, vec![vec![0.25, 0.5, 0.75]]);
    }

    #[test]
    fn reap_drops_only_finished_chunks() {
        let mut live: VecDeque<((), f64)> = VecDeque::from([((), 1.0), ((), 2.0), ((), 3.0)]);
        reap_finished(&mut live, 2.5);
        // Ends at 1.0 and 2.0 have passed; 3.0 is still playing and must live.
        assert_eq!(live, VecDeque::from([((), 3.0)]));

        // A source exactly at its end time is not reaped (never drop early).
        reap_finished(&mut live, 3.0);
        assert_eq!(live.len(), 1);

        reap_finished(&mut live, 3.1);
        assert!(live.is_empty());
    }
}
