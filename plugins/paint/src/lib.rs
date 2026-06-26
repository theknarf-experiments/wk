#[allow(warnings)]
mod bindings;

use bindings::wasi::frame_buffer::frame_buffer::{Buffer, Device};
use bindings::wasi::graphics_context::graphics_context::Context;
use bindings::wasi::surface::surface::{CreateDesc, Surface};
use bindings::Guest;

struct Component;

impl Guest for Component {
    /// A self-driving client: it opens a surface, then owns its own frame loop,
    /// blocking on the surface's frame event and painting an animated gradient.
    /// This is the "app that thinks it owns its window" model — the host
    /// (compositor) drives the frame events and composites what we present.
    fn run() {
        let surface = Surface::new(CreateDesc {
            width: Some(256),
            height: Some(256),
        });
        let ctx = Context::new();
        surface.connect_graphics_context(&ctx);
        let device = Device::new();
        device.connect_graphics_context(&ctx);

        let frame = surface.subscribe_frame();
        let mut t: u32 = 0;
        loop {
            frame.block();
            let _ = surface.get_frame();

            let w = surface.width().max(1);
            let h = surface.height().max(1);

            let buffer = Buffer::from_graphics_buffer(ctx.get_current_buffer());
            let mut pixels = vec![0u8; (w * h * 4) as usize];
            for y in 0..h {
                for x in 0..w {
                    let i = ((y * w + x) * 4) as usize;
                    pixels[i] = ((x * 255) / w) as u8;
                    pixels[i + 1] = ((y * 255) / h) as u8;
                    pixels[i + 2] = ((x + y + t) % 256) as u8;
                    pixels[i + 3] = 255;
                }
            }
            buffer.set(&pixels);
            ctx.present();

            t = t.wrapping_add(2);
        }
    }
}

bindings::export!(Component with_types_in bindings);
