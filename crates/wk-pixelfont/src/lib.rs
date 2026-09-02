//! Bitmap fonts for plugins that paint their own pixels.
//!
//! A wk plugin drawing into a frame buffer has no text renderer, so each one
//! that wanted a label grew a little `match` of hand-drawn glyphs — containing
//! exactly the letters its labels happened to spell at the time. That works
//! until someone adds a label. The sequencer's `TRK` came out as `RK`; the
//! synth's `CHAN` knob came out as `C AN`, because nothing it said had ever
//! needed an H before. Both were silent: no crash, no warning, just a word with
//! a hole in it.
//!
//! So the fonts live here, complete, with a test that every letter and digit
//! actually has a shape. Adding a label can no longer break it.
//!
//! Two sizes, because the plugins that need one need different amounts of room:
//! [`SMALL`] is 3x5, for a knob label under a knob; [`REGULAR`] is 5x7 and
//! readable, for anything with space for it.

/// A bitmap font: fixed-size glyphs, one bit per pixel, high bit leftmost.
pub struct Font {
    /// Pixels across, at most 8.
    pub width: i32,
    /// Pixels down.
    pub height: i32,
    /// `(character, rows)`, one row per `height`.
    glyphs: &'static [(char, &'static [u8])],
}

impl Font {
    /// The rows of `c`, or `None` if the font has no shape for it.
    fn rows(&self, c: char) -> Option<&'static [u8]> {
        let upper = c.to_ascii_uppercase();
        self.glyphs
            .iter()
            .find(|&&(ch, _)| ch == upper)
            .map(|&(_, rows)| rows)
    }

    /// Does this font have a shape for `c`? Space is deliberately *not* a
    /// shape: it draws as a blank of the same width, which is what it is.
    pub fn has(&self, c: char) -> bool {
        self.rows(c).is_some()
    }

    /// The horizontal step from one glyph to the next, gap included.
    fn advance(&self, scale: i32) -> i32 {
        (self.width + 1) * scale
    }

    /// How wide `s` renders at `scale`, without the trailing gap.
    pub fn measure(&self, s: &str, scale: i32) -> i32 {
        let n = s.chars().count() as i32;
        (n * self.advance(scale) - scale).max(0)
    }

    /// Draw `s` into an RGBA buffer with its top-left corner at `(x, y)`, each
    /// font pixel a `scale`-sized block. Anything off the buffer is clipped;
    /// anything the font has no shape for leaves a blank of the same width, so
    /// text stays aligned.
    // A blitter's arguments are the destination, the position, the string and
    // the ink. Bundling them into a struct would only move the same list one
    // call further out.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &self,
        buf: &mut [u8],
        w: u32,
        h: u32,
        x: i32,
        y: i32,
        s: &str,
        scale: i32,
        color: [u8; 3],
    ) {
        let mut pen = x;
        for c in s.chars() {
            if let Some(rows) = self.rows(c) {
                for (row, bits) in rows.iter().enumerate() {
                    for col in 0..self.width {
                        if bits & (1 << (self.width - 1 - col)) == 0 {
                            continue;
                        }
                        for dy in 0..scale {
                            for dx in 0..scale {
                                let px = pen + col * scale + dx;
                                let py = y + row as i32 * scale + dy;
                                if px < 0 || py < 0 || px >= w as i32 || py >= h as i32 {
                                    continue;
                                }
                                let i = ((py as u32 * w + px as u32) * 4) as usize;
                                if i + 3 < buf.len() {
                                    buf[i] = color[0];
                                    buf[i + 1] = color[1];
                                    buf[i + 2] = color[2];
                                    buf[i + 3] = 255;
                                }
                            }
                        }
                    }
                }
            }
            pen += self.advance(scale);
        }
    }
}

/// The 5x7 font: readable, for anything with room for it. The bitmaps are laid
/// out so the shape is visible in the source, which is the only way to
/// proofread a font by eye.
#[rustfmt::skip]
pub const REGULAR: Font = Font {
    width: 5,
    height: 7,
    glyphs: &[
        ('0', &[0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110]),
        ('1', &[0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110]),
        ('2', &[0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111]),
        ('3', &[0b11111, 0b00010, 0b00100, 0b00010, 0b00001, 0b10001, 0b01110]),
        ('4', &[0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010]),
        ('5', &[0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110]),
        ('6', &[0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110]),
        ('7', &[0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000]),
        ('8', &[0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110]),
        ('9', &[0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100]),
        ('A', &[0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001]),
        ('B', &[0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110]),
        ('C', &[0b01110, 0b10001, 0b10000, 0b10000, 0b10000, 0b10001, 0b01110]),
        ('D', &[0b11100, 0b10010, 0b10001, 0b10001, 0b10001, 0b10010, 0b11100]),
        ('E', &[0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111]),
        ('F', &[0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000]),
        ('G', &[0b01110, 0b10001, 0b10000, 0b10111, 0b10001, 0b10001, 0b01111]),
        ('H', &[0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001]),
        ('I', &[0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b11111]),
        ('J', &[0b00111, 0b00010, 0b00010, 0b00010, 0b00010, 0b10010, 0b01100]),
        ('K', &[0b10001, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010, 0b10001]),
        ('L', &[0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111]),
        ('M', &[0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001]),
        ('N', &[0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001, 0b10001]),
        ('O', &[0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110]),
        ('P', &[0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000]),
        ('Q', &[0b01110, 0b10001, 0b10001, 0b10001, 0b10101, 0b10010, 0b01101]),
        ('R', &[0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001]),
        ('S', &[0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110]),
        ('T', &[0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100]),
        ('U', &[0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110]),
        ('V', &[0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100]),
        ('W', &[0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b11011, 0b10001]),
        ('X', &[0b10001, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b10001]),
        ('Y', &[0b10001, 0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0b00100]),
        ('Z', &[0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b11111]),
        ('#', &[0b01010, 0b01010, 0b11111, 0b01010, 0b11111, 0b01010, 0b01010]),
        ('-', &[0b00000, 0b00000, 0b00000, 0b11111, 0b00000, 0b00000, 0b00000]),
        ('+', &[0b00000, 0b00100, 0b00100, 0b11111, 0b00100, 0b00100, 0b00000]),
        ('.', &[0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b01100, 0b01100]),
        (':', &[0b00000, 0b01100, 0b01100, 0b00000, 0b01100, 0b01100, 0b00000]),
        ('*', &[0b00000, 0b10101, 0b01110, 0b11111, 0b01110, 0b10101, 0b00000]),
        ('/', &[0b00001, 0b00010, 0b00010, 0b00100, 0b01000, 0b01000, 0b10000]),
    ],
};

/// The 3x5 font: for a label that has to fit under a knob. Three pixels is not
/// much to draw a letter in, so some of these are more suggestion than shape —
/// but a suggestion in the right place beats a gap.
#[rustfmt::skip]
pub const SMALL: Font = Font {
    width: 3,
    height: 5,
    glyphs: &[
        ('0', &[0b111, 0b101, 0b101, 0b101, 0b111]),
        ('1', &[0b010, 0b110, 0b010, 0b010, 0b111]),
        ('2', &[0b111, 0b001, 0b111, 0b100, 0b111]),
        ('3', &[0b111, 0b001, 0b111, 0b001, 0b111]),
        ('4', &[0b101, 0b101, 0b111, 0b001, 0b001]),
        ('5', &[0b111, 0b100, 0b111, 0b001, 0b111]),
        ('6', &[0b111, 0b100, 0b111, 0b101, 0b111]),
        ('7', &[0b111, 0b001, 0b010, 0b010, 0b010]),
        ('8', &[0b111, 0b101, 0b111, 0b101, 0b111]),
        ('9', &[0b111, 0b101, 0b111, 0b001, 0b111]),
        ('A', &[0b010, 0b101, 0b111, 0b101, 0b101]),
        ('B', &[0b110, 0b101, 0b110, 0b101, 0b110]),
        ('C', &[0b111, 0b100, 0b100, 0b100, 0b111]),
        ('D', &[0b110, 0b101, 0b101, 0b101, 0b110]),
        ('E', &[0b111, 0b100, 0b111, 0b100, 0b111]),
        ('F', &[0b111, 0b100, 0b111, 0b100, 0b100]),
        ('G', &[0b111, 0b100, 0b101, 0b101, 0b111]),
        ('H', &[0b101, 0b101, 0b111, 0b101, 0b101]),
        ('I', &[0b111, 0b010, 0b010, 0b010, 0b111]),
        ('J', &[0b001, 0b001, 0b001, 0b101, 0b111]),
        ('K', &[0b101, 0b101, 0b110, 0b101, 0b101]),
        ('L', &[0b100, 0b100, 0b100, 0b100, 0b111]),
        ('M', &[0b101, 0b111, 0b111, 0b101, 0b101]),
        ('N', &[0b101, 0b111, 0b111, 0b111, 0b101]),
        ('O', &[0b111, 0b101, 0b101, 0b101, 0b111]),
        ('P', &[0b110, 0b101, 0b110, 0b100, 0b100]),
        ('Q', &[0b111, 0b101, 0b101, 0b111, 0b011]),
        ('R', &[0b110, 0b101, 0b110, 0b101, 0b101]),
        ('S', &[0b111, 0b100, 0b111, 0b001, 0b111]),
        ('T', &[0b111, 0b010, 0b010, 0b010, 0b010]),
        ('U', &[0b101, 0b101, 0b101, 0b101, 0b111]),
        ('V', &[0b101, 0b101, 0b101, 0b101, 0b010]),
        ('W', &[0b101, 0b101, 0b111, 0b111, 0b101]),
        ('X', &[0b101, 0b101, 0b010, 0b101, 0b101]),
        ('Y', &[0b101, 0b101, 0b010, 0b010, 0b010]),
        ('Z', &[0b111, 0b001, 0b010, 0b100, 0b111]),
        ('#', &[0b101, 0b111, 0b101, 0b111, 0b101]),
        ('-', &[0b000, 0b000, 0b111, 0b000, 0b000]),
        ('+', &[0b000, 0b010, 0b111, 0b010, 0b000]),
        ('.', &[0b000, 0b000, 0b000, 0b000, 0b010]),
        (':', &[0b000, 0b010, 0b000, 0b010, 0b000]),
        ('/', &[0b001, 0b001, 0b010, 0b100, 0b100]),
    ],
};

#[cfg(test)]
mod tests {
    use super::*;

    /// The bug this crate exists to stop, in both sizes: a font that is missing
    /// the letter someone's next label happens to need, failing silently.
    #[test]
    fn every_letter_and_digit_has_a_shape() {
        for (name, font) in [("REGULAR", &REGULAR), ("SMALL", &SMALL)] {
            for c in ('A'..='Z').chain('0'..='9') {
                assert!(font.has(c), "{name} has no glyph for {c:?}");
            }
        }
    }

    /// A glyph that is present must also be *drawn* — an all-zero bitmap is a
    /// missing letter that passes the check above.
    #[test]
    fn no_letter_is_secretly_blank() {
        for (name, font) in [("REGULAR", &REGULAR), ("SMALL", &SMALL)] {
            for c in ('A'..='Z').chain('0'..='9') {
                let mut buf = vec![0u8; (font.width as usize + 2) * font.height as usize * 4];
                font.draw(
                    &mut buf,
                    font.width as u32 + 2,
                    font.height as u32,
                    0,
                    0,
                    &c.to_string(),
                    1,
                    [255, 255, 255],
                );
                assert!(
                    buf.iter().any(|&b| b != 0),
                    "{name}'s {c:?} draws nothing at all"
                );
            }
        }
    }

    /// Case does not matter: the fonts are uppercase, and a caller passing a
    /// filename should get letters rather than gaps.
    #[test]
    fn lowercase_draws_as_uppercase() {
        assert!(REGULAR.has('a') && SMALL.has('a'));
    }

    /// Every label the plugins actually draw is fully covered. Adding one and
    /// forgetting a glyph is the whole failure mode; this is the list to extend
    /// when a plugin gains a word.
    #[test]
    fn the_labels_the_plugins_draw_are_covered() {
        // synth and arp knobs (SMALL), then the sequencer's chrome (REGULAR).
        let small = [
            "VOL", "WAVE", "TUNE", "CUT", "RES", "ATK", "REL", "CHAN", "RATE", "GATE", "MODE",
            "OCT",
        ];
        for label in small {
            for c in label.chars() {
                assert!(SMALL.has(c), "SMALL cannot draw {c:?} of {label:?}");
            }
        }
        let regular = [
            "BPM",
            "LEN",
            "CH",
            "SONG",
            "TRK",
            "PAT",
            "SNG",
            "VEL",
            "SHIFT-CLICK A PATTERN",
        ];
        for label in regular {
            for c in label.chars().filter(|c| *c != ' ') {
                assert!(REGULAR.has(c), "REGULAR cannot draw {c:?} of {label:?}");
            }
        }
    }

    #[test]
    fn width_counts_the_gaps_between_glyphs_but_not_after_the_last() {
        // Three 5px glyphs with 1px gaps: 5 + 1 + 5 + 1 + 5.
        assert_eq!(REGULAR.measure("ABC", 1), 17);
        assert_eq!(REGULAR.measure("ABC", 2), 34);
        assert_eq!(REGULAR.measure("", 1), 0);
        // Three 3px glyphs: 3 + 1 + 3 + 1 + 3.
        assert_eq!(SMALL.measure("ABC", 1), 11);
    }

    /// A space takes room without drawing anything, so columns line up.
    #[test]
    fn a_space_advances_but_marks_nothing() {
        let (w, h) = (40u32, 8u32);
        let mut buf = vec![0u8; (w * h * 4) as usize];
        REGULAR.draw(&mut buf, w, h, 0, 0, " ", 1, [255, 255, 255]);
        assert!(buf.iter().all(|&b| b == 0), "a space drew ink");
        assert_eq!(REGULAR.measure("A B", 1), REGULAR.measure("AAB", 1));
    }

    /// Drawing off the edge clips instead of panicking or wrapping.
    #[test]
    fn drawing_outside_the_buffer_is_clipped() {
        let (w, h) = (8u32, 8u32);
        let mut buf = vec![0u8; (w * h * 4) as usize];
        for (x, y) in [(-99, 0), (0, -99), (99, 0), (0, 99), (6, 6)] {
            REGULAR.draw(&mut buf, w, h, x, y, "WK", 3, [255, 255, 255]);
        }
    }
}
