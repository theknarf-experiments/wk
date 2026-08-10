//! A cache of rasterised strings as GPU textures, tinted at draw time.

use super::*;

/// Caches rasterized strings as textures (white glyphs, tinted at draw time).
#[derive(Default)]
pub(super) struct TextCache {
    map: HashMap<String, (TextureId, f32, f32)>,
}

impl TextCache {
    /// The cached texture (and its pixel size) for a string, rasterising on
    /// miss — for callers that draw the text themselves (the 3D view).
    pub(super) fn get(
        &mut self,
        r: &mut Renderer,
        fonts: &Fonts,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        s: &str,
    ) -> Option<(TextureId, f32, f32)> {
        match self.map.get(s) {
            Some(e) => Some(*e),
            None => {
                let g = fonts.rasterize(s)?;
                if self.map.len() >= 1024 {
                    for (_, (tex, _, _)) in self.map.drain() {
                        r.remove_texture(tex);
                    }
                }
                let tex = r.create_texture(device, queue, g.width, g.height, &g.rgba);
                let e = (tex, g.width as f32, g.height as f32);
                self.map.insert(s.to_string(), e);
                Some(e)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn draw(
        &mut self,
        quads: &mut Vec<Quad>,
        r: &mut Renderer,
        fonts: &Fonts,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        s: &str,
        x: f32,
        y: f32,
        scale: f32,
        color: [f32; 4],
        clip: [f32; 4],
    ) {
        let Some((tex, w, h)) = self.get(r, fonts, device, queue, s) else {
            return;
        };
        quads.push(Quad::tex(
            [x, y, x + w * scale, y + h * scale],
            [0.0, 0.0, 1.0, 1.0],
            color,
            tex,
            clip,
        ));
    }
}
