//! Text rendering via SDL3_ttf. Strings are rasterized once as white glyphs
//! (coverage in the alpha channel) into RGBA pixel buffers; the 2D renderer
//! uploads them as textures and tints them at draw time via the quad color.

use sdl3::pixels::{Color, PixelFormat};
use sdl3::ttf::{Font, Sdl3TtfContext};

/// A rasterized string: tightly-packed RGBA8 pixels and its dimensions.
pub struct Glyphs {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

pub struct Fonts {
    // Kept alive: the font borrows the initialized ttf context.
    _ctx: Sdl3TtfContext,
    font: Font<'static>,
    line_height: u32,
}

impl Fonts {
    pub fn new(point_size: f32) -> Result<Self, String> {
        let ctx = sdl3::ttf::init().map_err(|e| format!("ttf init: {e}"))?;
        let path = font_path().ok_or("no usable system font found")?;
        let font = ctx
            .load_font(&path, point_size)
            .map_err(|e| format!("load font {path}: {e}"))?;
        let line_height = font.height().max(1) as u32;
        Ok(Fonts {
            _ctx: ctx,
            font,
            line_height,
        })
    }

    /// Pixel height of one line of text.
    pub fn line_height(&self) -> u32 {
        self.line_height
    }

    /// Pixel width the string would occupy.
    pub fn measure(&self, s: &str) -> u32 {
        self.font.size_of(s).map(|(w, _)| w).unwrap_or(0)
    }

    /// Rasterize `s` as white glyphs. Returns `None` for an empty string.
    pub fn rasterize(&self, s: &str) -> Option<Glyphs> {
        if s.is_empty() {
            return None;
        }
        let surface = self.font.render(s).blended(Color::WHITE).ok()?;
        // ABGR8888 is, in little-endian memory, R,G,B,A bytes == wgpu Rgba8Unorm.
        let surface = surface.convert_format(PixelFormat::ABGR8888).ok()?;
        let (w, h) = (surface.width(), surface.height());
        let pitch = surface.pitch() as usize;
        let row = (w * 4) as usize;
        let mut rgba = vec![0u8; row * h as usize];
        surface.with_lock(|bytes| {
            for y in 0..h as usize {
                rgba[y * row..y * row + row].copy_from_slice(&bytes[y * pitch..y * pitch + row]);
            }
        });
        Some(Glyphs {
            rgba,
            width: w,
            height: h,
        })
    }
}

/// The first existing candidate system font (monospace preferred).
fn font_path() -> Option<String> {
    const CANDIDATES: &[&str] = &[
        "/System/Library/Fonts/SFNSMono.ttf",
        "/System/Library/Fonts/Monaco.ttf",
        "/System/Library/Fonts/Supplemental/Andale Mono.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
        "/usr/share/fonts/truetype/liberation/LiberationMono-Regular.ttf",
        "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
    ];
    CANDIDATES
        .iter()
        .find(|p| std::path::Path::new(p).exists())
        .map(|s| s.to_string())
}
