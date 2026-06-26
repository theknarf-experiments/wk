#[allow(warnings)]
mod bindings;

use bindings::Guest;

struct Component;

impl Guest for Component {
    /// Paint an animated gradient so we can see the compositor driving the
    /// plugin frame by frame.
    fn render(width: u32, height: u32, time_ms: u64) -> Vec<u8> {
        let t = (time_ms / 8) as u32;
        let mut buf = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                let r = ((x * 255) / width.max(1)) as u8;
                let g = ((y * 255) / height.max(1)) as u8;
                let b = ((x + y + t) % 256) as u8;
                buf.extend_from_slice(&[r, g, b, 255]);
            }
        }
        buf
    }
}

bindings::export!(Component with_types_in bindings);
