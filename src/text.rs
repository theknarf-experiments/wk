//! Text rendering via `ab_glyph` (pure Rust). Strings are rasterized once as
//! white glyphs (coverage in the alpha channel) into RGBA pixel buffers; the 2D
//! renderer uploads them as textures and tints them at draw time via the quad
//! color.

use ab_glyph::{Font, FontVec, GlyphId, PxScale, ScaleFont};

/// A rasterized string: tightly-packed RGBA8 pixels and its dimensions.
pub struct Glyphs {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

pub struct Fonts {
    font: FontVec,
    scale: PxScale,
    ascent: f32,
    line_height: u32,
}

impl Fonts {
    pub fn new(point_size: f32) -> Result<Self, String> {
        let path = font_path().ok_or("no usable system font found")?;
        let data = std::fs::read(&path).map_err(|e| format!("read font {path}: {e}"))?;
        let font = FontVec::try_from_vec(data).map_err(|e| format!("parse font {path}: {e}"))?;
        let scale = PxScale::from(point_size);
        let sf = font.as_scaled(scale);
        let ascent = sf.ascent();
        let line_height = (sf.ascent() - sf.descent() + sf.line_gap()).ceil().max(1.0) as u32;
        Ok(Fonts {
            font,
            scale,
            ascent,
            line_height,
        })
    }

    /// Pixel height of one line of text.
    pub fn line_height(&self) -> u32 {
        self.line_height
    }

    /// Pixel width the string would occupy.
    pub fn measure(&self, s: &str) -> u32 {
        let sf = self.font.as_scaled(self.scale);
        let mut w = 0.0f32;
        let mut prev: Option<GlyphId> = None;
        for c in s.chars() {
            let id = sf.glyph_id(c);
            if let Some(p) = prev {
                w += sf.kern(p, id);
            }
            w += sf.h_advance(id);
            prev = Some(id);
        }
        w.ceil().max(1.0) as u32
    }

    /// Rasterize `s` as white glyphs. Returns `None` for an empty string.
    pub fn rasterize(&self, s: &str) -> Option<Glyphs> {
        if s.is_empty() {
            return None;
        }
        let sf = self.font.as_scaled(self.scale);
        let width = self.measure(s);
        let height = self.line_height;
        let mut rgba = vec![0u8; width as usize * height as usize * 4];

        let mut caret = 0.0f32;
        let mut prev: Option<GlyphId> = None;
        for c in s.chars() {
            let id = sf.glyph_id(c);
            if let Some(p) = prev {
                caret += sf.kern(p, id);
            }
            let mut glyph = sf.scaled_glyph(c);
            glyph.position = ab_glyph::point(caret, self.ascent);
            if let Some(outline) = sf.outline_glyph(glyph) {
                let bounds = outline.px_bounds();
                outline.draw(|gx, gy, cov| {
                    let px = bounds.min.x.round() as i32 + gx as i32;
                    let py = bounds.min.y.round() as i32 + gy as i32;
                    if px >= 0 && py >= 0 && (px as u32) < width && (py as u32) < height {
                        let i = ((py as u32 * width + px as u32) * 4) as usize;
                        rgba[i] = 255;
                        rgba[i + 1] = 255;
                        rgba[i + 2] = 255;
                        rgba[i + 3] = rgba[i + 3].max((cov * 255.0) as u8);
                    }
                });
            }
            caret += sf.h_advance(id);
            prev = Some(id);
        }
        Some(Glyphs {
            rgba,
            width,
            height,
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
