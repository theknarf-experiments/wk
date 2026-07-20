#[allow(warnings)]
mod bindings;

use bindings::wasi::frame_buffer::frame_buffer::{Buffer, Device};
use bindings::wasi::graphics_context::graphics_context::Context;
use bindings::wasi::surface::surface::{CreateDesc, Surface};
use bindings::wk::capture::frames;
use bindings::Guest;

struct Component;

impl Guest for Component {
    /// A screen-capture viewer: polls wk:capture for frames (granted by wiring
    /// this node to a Screen Capture node) and blits the latest one into its
    /// surface, scaled nearest-neighbour. Without a grant it shows a dim
    /// "no signal" checkerboard — wire it up and the canvas appears within
    /// the canvas.
    fn run() {
        let surface = Surface::new(CreateDesc {
            width: Some(360),
            height: Some(240),
        });
        let ctx = Context::new();
        surface.connect_graphics_context(&ctx);
        let device = Device::new();
        device.connect_graphics_context(&ctx);
        let frame = surface.subscribe_frame();

        // The latest captured frame, kept so resizes re-blit without a new one.
        let mut cap: Option<frames::Frame> = None;
        let mut t: u32 = 0;
        loop {
            frame.block();
            let _ = surface.get_frame();

            if let Some(f) = frames::next_frame() {
                cap = Some(f);
            }

            let w = surface.width().max(1) as usize;
            let h = surface.height().max(1) as usize;
            let mut pixels = vec![0u8; w * h * 4];

            match &cap {
                Some(f) if f.width > 0 && f.height > 0 => {
                    // Nearest-neighbour scale into the surface (letterboxed).
                    let (fw, fh) = (f.width as usize, f.height as usize);
                    let scale = (fw as f32 / w as f32).max(fh as f32 / h as f32);
                    let (dw, dh) = (
                        ((fw as f32 / scale) as usize).clamp(1, w),
                        ((fh as f32 / scale) as usize).clamp(1, h),
                    );
                    let (ox, oy) = ((w - dw) / 2, (h - dh) / 2);
                    for y in 0..dh {
                        let sy = (y as f32 * scale) as usize;
                        for x in 0..dw {
                            let sx = (x as f32 * scale) as usize;
                            let s = (sy.min(fh - 1) * fw + sx.min(fw - 1)) * 4;
                            let d = ((y + oy) * w + (x + ox)) * 4;
                            pixels[d..d + 4].copy_from_slice(&f.data[s..s + 4]);
                        }
                    }
                }
                _ => {
                    // "No signal": a slow-pulsing checkerboard.
                    let pulse = 24 + ((t / 2) % 32) as u8;
                    for y in 0..h {
                        for x in 0..w {
                            let on = ((x / 16) + (y / 16)) % 2 == 0;
                            let v = if on { pulse } else { 12 };
                            let i = (y * w + x) * 4;
                            pixels[i..i + 4].copy_from_slice(&[v, v, v + 4, 255]);
                        }
                    }
                }
            }

            let buf = Buffer::from_graphics_buffer(ctx.get_current_buffer());
            buf.set(&pixels);
            ctx.present();
            t = t.wrapping_add(1);
        }
    }
}

bindings::export!(Component with_types_in bindings);
