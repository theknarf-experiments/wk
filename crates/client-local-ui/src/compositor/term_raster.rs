//! CPU rasterizer for terminal grids: cells → one RGBA image. The 3D view
//! draws a terminal node as a single texture on a panel, so the grid is
//! rendered to pixels here rather than as per-cell quads like the 2D canvas.

use std::collections::HashMap;

use crate::text::{Fonts, Glyphs};
use wk_server::terminal::CellView;

/// The terminal background, as bytes (matches the 2D view's `TERM_BG`).
const BG: [u8; 4] = [16, 16, 22, 255];
/// Cursor overlay colour and opacity (matches the 2D block cursor).
const CURSOR: ([u8; 3], f32) = ([217, 217, 230], 0.45);

/// Rasterizes cell grids, caching one white glyph bitmap per character (tinted
/// per cell at blit time).
#[derive(Default)]
pub(super) struct TermRaster {
    glyphs: HashMap<char, Option<Glyphs>>,
}

impl TermRaster {
    /// Render `cells` (and the cursor) into a tightly-packed RGBA image sized
    /// `cols`×`rows` cells at the font's natural cell metrics.
    pub(super) fn rasterize(
        &mut self,
        fonts: &Fonts,
        cells: &[CellView],
        cursor: Option<(usize, usize)>,
        grid: (u16, u16),
    ) -> (u32, u32, Vec<u8>) {
        let bw = fonts.measure("M").max(1);
        let bh = fonts.line_height().max(1);
        let (cols, rows) = (grid.0 as u32, grid.1 as u32);
        let (w, h) = (cols * bw, rows * bh);
        let mut px = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..w * h {
            px.extend_from_slice(&BG);
        }

        let fill = |x0: u32, y0: u32, c: [u8; 3], a: f32, px: &mut Vec<u8>| {
            for y in y0..(y0 + bh).min(h) {
                for x in x0..(x0 + bw).min(w) {
                    let i = ((y * w + x) * 4) as usize;
                    for k in 0..3 {
                        px[i + k] = (c[k] as f32 * a + px[i + k] as f32 * (1.0 - a)) as u8;
                    }
                }
            }
        };

        for cell in cells {
            let (cx, cy) = (cell.col as u32 * bw, cell.row as u32 * bh);
            if let Some(bg) = cell.bg {
                fill(cx, cy, bg, 1.0, &mut px);
            }
            if cell.ch == ' ' {
                continue;
            }
            let glyph = self
                .glyphs
                .entry(cell.ch)
                .or_insert_with(|| {
                    let mut buf = [0u8; 4];
                    fonts.rasterize(cell.ch.encode_utf8(&mut buf))
                })
                .as_ref();
            let Some(g) = glyph else {
                continue;
            };
            // Blit the white glyph tinted by the cell's fg, alpha-blended,
            // clipped to the image (a glyph can slightly overhang its cell).
            for gy in 0..g.height.min(bh) {
                let y = cy + gy;
                if y >= h {
                    break;
                }
                for gx in 0..g.width {
                    let x = cx + gx;
                    if x >= w {
                        break;
                    }
                    let a = g.rgba[((gy * g.width + gx) * 4 + 3) as usize] as f32 / 255.0;
                    if a <= 0.0 {
                        continue;
                    }
                    let i = ((y * w + x) * 4) as usize;
                    for k in 0..3 {
                        px[i + k] = (cell.fg[k] as f32 * a + px[i + k] as f32 * (1.0 - a)) as u8;
                    }
                }
            }
        }

        if let Some((ccol, crow)) = cursor {
            fill(
                ccol as u32 * bw,
                crow as u32 * bh,
                CURSOR.0,
                CURSOR.1,
                &mut px,
            );
        }

        (w, h, px)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fonts() -> Fonts {
        Fonts::new(15.0).expect("system font")
    }

    #[test]
    fn grid_has_cell_metrics_and_bg() {
        let f = fonts();
        let (w, h, px) = TermRaster::default().rasterize(&f, &[], None, (80, 24));
        assert_eq!(w, f.measure("M").max(1) * 80);
        assert_eq!(h, f.line_height().max(1) * 24);
        // Empty grid: every pixel is the terminal background.
        assert_eq!(&px[0..4], &BG);
        assert_eq!(px.len(), (w * h * 4) as usize);
    }

    #[test]
    fn glyph_and_bg_cells_change_pixels() {
        let f = fonts();
        let cells = [
            CellView {
                col: 0,
                row: 0,
                ch: 'M',
                fg: [255, 0, 0],
                bg: None,
            },
            CellView {
                col: 1,
                row: 0,
                ch: ' ',
                fg: [255, 255, 255],
                bg: Some([0, 0, 255]),
            },
        ];
        let (w, _, px) = TermRaster::default().rasterize(&f, &cells, None, (4, 2));
        let bw = f.measure("M").max(1);
        // Cell (0,0): some pixel picked up the red glyph.
        let cell0_has_red = (0..f.line_height()).any(|y| {
            (0..bw).any(|x| {
                let i = ((y * w + x) * 4) as usize;
                px[i] > BG[0] + 30
            })
        });
        assert!(cell0_has_red, "expected red glyph coverage in cell 0");
        // Cell (1,0): its background is blue.
        let i = ((bw + 1) * 4) as usize;
        assert_eq!(px[i + 2], 255, "expected blue bg in cell 1");
    }
}
