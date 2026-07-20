#[allow(warnings)]
mod bindings;

use bindings::wasi::frame_buffer::frame_buffer::{Buffer, Device};
use bindings::wasi::graphics_context::graphics_context::Context;
use bindings::wasi::surface::surface::{CreateDesc, Surface};
use bindings::wk::capture::frames;
use bindings::Guest;

use std::fs;
use std::io::Write;

use jpeg_encoder::{ColorType, Encoder};

/// The output file: the first regular file in `/` — a wired-in file node
/// (a HostFile writes to disk) — else a private path in the plugin's own vfs.
fn output_path() -> String {
    if let Ok(rd) = fs::read_dir("/") {
        for entry in rd.flatten() {
            if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                return format!("/{}", entry.file_name().to_string_lossy());
            }
        }
    }
    "/recording.mjpeg".to_string()
}

/// JPEG-encode one RGBA frame into an MJPEG stream frame (a standalone JPEG).
fn encode_jpeg(rgba: &[u8], w: u32, h: u32) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let enc = Encoder::new(&mut out, 80);
    enc.encode(rgba, w as u16, h as u16, ColorType::Rgba).ok()?;
    Some(out)
}

/// A tiny 3x5 bitmap font for the status readout, enough for the glyphs used.
fn glyph(c: char) -> [u8; 5] {
    match c {
        '0' => [0b111, 0b101, 0b101, 0b101, 0b111],
        '1' => [0b010, 0b110, 0b010, 0b010, 0b111],
        '2' => [0b111, 0b001, 0b111, 0b100, 0b111],
        '3' => [0b111, 0b001, 0b111, 0b001, 0b111],
        '4' => [0b101, 0b101, 0b111, 0b001, 0b001],
        '5' => [0b111, 0b100, 0b111, 0b001, 0b111],
        '6' => [0b111, 0b100, 0b111, 0b101, 0b111],
        '7' => [0b111, 0b001, 0b010, 0b010, 0b010],
        '8' => [0b111, 0b101, 0b111, 0b101, 0b111],
        '9' => [0b111, 0b101, 0b111, 0b001, 0b111],
        'R' => [0b110, 0b101, 0b110, 0b101, 0b101],
        'E' => [0b111, 0b100, 0b110, 0b100, 0b111],
        'C' => [0b111, 0b100, 0b100, 0b100, 0b111],
        'F' => [0b111, 0b100, 0b110, 0b100, 0b100],
        'K' => [0b101, 0b101, 0b110, 0b101, 0b101],
        'B' => [0b110, 0b101, 0b110, 0b101, 0b110],
        'W' => [0b101, 0b101, 0b101, 0b111, 0b101],
        'A' => [0b111, 0b101, 0b111, 0b101, 0b101],
        'I' => [0b111, 0b010, 0b010, 0b010, 0b111],
        'T' => [0b111, 0b010, 0b010, 0b010, 0b010],
        'N' => [0b101, 0b111, 0b111, 0b111, 0b101],
        'O' => [0b111, 0b101, 0b101, 0b101, 0b111],
        'U' => [0b101, 0b101, 0b101, 0b101, 0b111],
        'P' => [0b110, 0b101, 0b110, 0b100, 0b100],
        'S' => [0b111, 0b100, 0b111, 0b001, 0b111],
        'L' => [0b100, 0b100, 0b100, 0b100, 0b111],
        '-' => [0b000, 0b000, 0b111, 0b000, 0b000],
        '.' => [0b000, 0b000, 0b000, 0b000, 0b010],
        _ => [0; 5], // space and unknowns
    }
}

/// Blit `text` at (x, y) into an RGBA buffer, scaled `s`x, in colour `col`.
fn draw_text(px: &mut [u8], w: usize, h: usize, x: usize, y: usize, s: usize, text: &str, col: [u8; 3]) {
    let mut cx = x;
    for ch in text.chars() {
        let g = glyph(ch.to_ascii_uppercase());
        for (row, bits) in g.iter().enumerate() {
            for bit in 0..3 {
                if bits & (1 << (2 - bit)) != 0 {
                    for dy in 0..s {
                        for dx in 0..s {
                            let ox = cx + bit * s + dx;
                            let oy = y + row * s + dy;
                            if ox < w && oy < h {
                                let i = (oy * w + ox) * 4;
                                px[i..i + 3].copy_from_slice(&col);
                                px[i + 3] = 255;
                            }
                        }
                    }
                }
            }
        }
        cx += 4 * s;
    }
}

struct Component;

impl Guest for Component {
    fn run() {
        let surface = Surface::new(CreateDesc {
            width: Some(300),
            height: Some(120),
        });
        let ctx = Context::new();
        surface.connect_graphics_context(&ctx);
        let device = Device::new();
        device.connect_graphics_context(&ctx);
        let frame = surface.subscribe_frame();

        let mut out: Option<fs::File> = None;
        let mut out_name = String::new();
        let mut frames_written: u64 = 0;
        let mut bytes_written: u64 = 0;
        let mut blink: u32 = 0;

        loop {
            frame.block();
            let _ = surface.get_frame();

            // Record every captured frame the grant delivers.
            if let Some(f) = frames::next_frame() {
                if f.width > 0 && f.height > 0 {
                    // Open (append) the output lazily, re-checking the wired file.
                    if out.is_none() {
                        out_name = output_path();
                        out = fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(&out_name)
                            .ok();
                    }
                    if let (Some(file), Some(jpeg)) =
                        (out.as_mut(), encode_jpeg(&f.data, f.width, f.height))
                    {
                        if file.write_all(&jpeg).is_ok() {
                            frames_written += 1;
                            bytes_written += jpeg.len() as u64;
                        }
                    }
                }
            }

            // Status surface.
            let w = surface.width().max(1) as usize;
            let h = surface.height().max(1) as usize;
            let mut px = vec![0u8; w * h * 4];
            for p in px.chunks_exact_mut(4) {
                p.copy_from_slice(&[16, 12, 14, 255]);
            }

            let recording = out.is_some();
            blink = blink.wrapping_add(1);
            if recording {
                // Blinking red dot + "REC".
                if (blink / 15) % 2 == 0 {
                    for dy in 0..12 {
                        for dx in 0..12 {
                            let i = ((16 + dy) * w + 16 + dx) * 4;
                            px[i..i + 4].copy_from_slice(&[230, 60, 70, 255]);
                        }
                    }
                }
                draw_text(&mut px, w, h, 40, 18, 3, "REC", [235, 120, 130]);
                let kb = bytes_written / 1024;
                draw_text(&mut px, w, h, 16, 56, 2, &format!("{frames_written} FRAMES"), [200, 200, 205]);
                draw_text(&mut px, w, h, 16, 82, 2, &format!("{kb} KB {out_name}"), [150, 155, 165]);
            } else {
                draw_text(&mut px, w, h, 16, 30, 2, "WAITING - WIRE A", [170, 175, 185]);
                draw_text(&mut px, w, h, 16, 56, 2, "CAPTURE NODE IN", [170, 175, 185]);
            }

            let buf = Buffer::from_graphics_buffer(ctx.get_current_buffer());
            buf.set(&px);
            ctx.present();
        }
    }
}

bindings::export!(Component with_types_in bindings);
