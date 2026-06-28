#[allow(warnings)]
mod bindings;

use bindings::wk::webaudio::audio::{Context, OscillatorType};
use bindings::Guest;

struct Component;

impl Guest for Component {
    fn run() {
        // Build the classic Web Audio graph: oscillator -> gain -> speakers.
        let ctx = Context::new();
        let osc = ctx.create_oscillator();
        let gain = ctx.create_gain();

        gain.set_gain(0.2);
        gain.connect_destination();

        osc.set_type(OscillatorType::Sine);
        osc.set_frequency(440.0);
        osc.connect(&gain);
        osc.start(0.0);

        println!("[beep] playing 440 Hz for ~1s (sample rate {})", ctx.sample_rate());
        std::thread::sleep(std::time::Duration::from_millis(1000));
        osc.stop(0.0);
        // Hold the graph alive briefly so the stop is rendered.
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
}

bindings::export!(Component with_types_in bindings);
