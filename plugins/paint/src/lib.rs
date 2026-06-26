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
        // Pointer state, updated from the input the compositor routes in.
        let mut px: i32 = -100;
        let mut py: i32 = -100;
        loop {
            frame.block();
            let _ = surface.get_frame();

            // Drain pointer input the host delivered for this frame.
            while let Some(ev) = surface.get_pointer_move() {
                px = ev.x as i32;
                py = ev.y as i32;
            }
            let mut clicked = false;
            while surface.get_pointer_down().is_some() {
                clicked = true;
            }
            while surface.get_pointer_up().is_some() {}

            let w = surface.width().max(1);
            let h = surface.height().max(1);

            let buffer = Buffer::from_graphics_buffer(ctx.get_current_buffer());
            let mut pixels = vec![0u8; (w * h * 4) as usize];
            for y in 0..h {
                for x in 0..w {
                    let i = ((y * w + x) * 4) as usize;
                    let mut r = ((x * 255) / w) as u8;
                    let mut g = ((y * 255) / h) as u8;
                    let mut b = ((x + y + t) % 256) as u8;
                    // White cursor marker that follows the pointer.
                    if (x as i32 - px).abs() < 5 && (y as i32 - py).abs() < 5 {
                        r = 255;
                        g = 255;
                        b = 255;
                    }
                    // Flash brighter on the frame a click arrives.
                    if clicked {
                        r = r.saturating_add(90);
                        g = g.saturating_add(90);
                        b = b.saturating_add(90);
                    }
                    pixels[i] = r;
                    pixels[i + 1] = g;
                    pixels[i + 2] = b;
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
