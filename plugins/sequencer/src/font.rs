//! A 5x7 bitmap font, just wide enough for the labels a sequencer needs.
//!
//! The plugin paints raw pixels into a frame buffer and has no text renderer, so
//! without something like this there is no tempo readout, no note names and no
//! bar numbers — and a piano roll you cannot read the tempo of is a toy. Only
//! the glyphs actually used are here: digits, the dozen letters those labels
//! spell, and a sharp sign.

/// One glyph: seven rows of five pixels, most significant bit leftmost.
type Glyph = [u8; 7];

const BLANK: Glyph = [0; 7];

/// The glyph for `c`, or a blank for anything not in the font.
fn glyph(c: char) -> Glyph {
    match c.to_ascii_uppercase() {
        '0' => [
            0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110,
        ],
        '1' => [
            0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        '2' => [
            0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111,
        ],
        '3' => [
            0b11111, 0b00010, 0b00100, 0b00010, 0b00001, 0b10001, 0b01110,
        ],
        '4' => [
            0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010,
        ],
        '5' => [
            0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110,
        ],
        '6' => [
            0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110,
        ],
        '7' => [
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000,
        ],
        '8' => [
            0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110,
        ],
        '9' => [
            0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100,
        ],
        'A' => [
            0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        'B' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110,
        ],
        'C' => [
            0b01110, 0b10001, 0b10000, 0b10000, 0b10000, 0b10001, 0b01110,
        ],
        'D' => [
            0b11100, 0b10010, 0b10001, 0b10001, 0b10001, 0b10010, 0b11100,
        ],
        'E' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111,
        ],
        'F' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000,
        ],
        'G' => [
            0b01110, 0b10001, 0b10000, 0b10111, 0b10001, 0b10001, 0b01111,
        ],
        'L' => [
            0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111,
        ],
        'M' => [
            0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001,
        ],
        'N' => [
            0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001, 0b10001,
        ],
        'P' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000,
        ],
        'V' => [
            0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100,
        ],
        '#' => [
            0b01010, 0b01010, 0b11111, 0b01010, 0b11111, 0b01010, 0b01010,
        ],
        '-' => [
            0b00000, 0b00000, 0b00000, 0b11111, 0b00000, 0b00000, 0b00000,
        ],
        _ => BLANK,
    }
}

/// Glyph cell width including the gap that follows it, at `scale`.
fn advance(scale: i32) -> i32 {
    6 * scale
}

/// How wide `s` renders at `scale`, without the trailing gap.
pub fn text_width(s: &str, scale: i32) -> i32 {
    let n = s.chars().count() as i32;
    (n * advance(scale) - scale).max(0)
}

/// Draw `s` with its top-left corner at `(x, y)`.
pub fn text(buf: &mut [u8], w: u32, h: u32, x: i32, y: i32, s: &str, scale: i32, color: [u8; 3]) {
    let mut pen = x;
    for c in s.chars() {
        let g = glyph(c);
        for (row, bits) in g.iter().enumerate() {
            for col in 0..5 {
                if bits & (1 << (4 - col)) == 0 {
                    continue;
                }
                // Each font pixel is a `scale`-sized block.
                for dy in 0..scale {
                    for dx in 0..scale {
                        let px = pen + col * scale + dx;
                        let py = y + row as i32 * scale + dy;
                        if px < 0 || py < 0 || px >= w as i32 || py >= h as i32 {
                            continue;
                        }
                        let i = ((py as u32 * w + px as u32) * 4) as usize;
                        buf[i] = color[0];
                        buf[i + 1] = color[1];
                        buf[i + 2] = color[2];
                        buf[i + 3] = 255;
                    }
                }
            }
        }
        pen += advance(scale);
    }
}
