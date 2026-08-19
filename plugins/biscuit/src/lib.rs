#[allow(warnings)]
mod bindings;

use bindings::Guest;
use bindings::wasi::frame_buffer::frame_buffer::{Buffer, Device};
use bindings::wasi::graphics_context::graphics_context::Context;
use bindings::wasi::surface::surface::{CreateDesc, Key, Surface};

use biscuit_auth::UnverifiedBiscuit;
use font8x8::legacy::BASIC_LEGACY;

/// Where the host publishes this node's own effective capability token: a
/// hex-encoded Biscuit, refreshed by the server on every token change (see
/// `write_token_file`). Absent when node auth is not configured.
const TOKEN_PATH: &str = "/run/wk/token";
/// Re-read the token file every this many frames (~1s at compositor pace),
/// so attenuations made with `wk token attenuate <node> ...` appear live.
const POLL_FRAMES: u32 = 60;
/// How long the "updated" accent border flashes after the token changes.
const FLASH_FRAMES: u32 = 45;

const BG: [u8; 3] = [0x12, 0x14, 0x1a];
const ACCENT: [u8; 3] = [0x7f, 0xd4, 0xff];

/// Visual class of a rendered line — each gets its own tint. `check if`
/// lines get the warning color: attenuations are the story here.
#[derive(Clone, Copy, PartialEq)]
enum Class {
    Title,
    Header,
    Meta,
    Fact,
    Rule,
    Check,
    Absent,
}

impl Class {
    fn color(self) -> [u8; 3] {
        match self {
            Class::Title => [0xe8, 0xec, 0xf4],
            Class::Header => ACCENT,
            Class::Meta => [0x6c, 0x74, 0x84],
            Class::Fact => [0x9e, 0xce, 0x6e],
            Class::Rule => [0xb0, 0xa2, 0xf8],
            Class::Check => [0xff, 0xb4, 0x54],
            Class::Absent => [0xf0, 0x64, 0x64],
        }
    }
}

/// One logical line of the document: text plus its tint class.
type Line = (String, Class);

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if s.is_empty() || !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// Tint a line of datalog block source: checks (attenuations) get the warning
/// color, rules and facts their own.
fn classify(line: &str) -> Class {
    let t = line.trim_start();
    if t.starts_with("check if") || t.starts_with("check all") || t.starts_with("reject if") {
        Class::Check
    } else if t.contains("<-") {
        Class::Rule
    } else {
        Class::Fact
    }
}

/// The screen shown while /run/wk/token does not exist (node auth off).
fn absent_doc() -> Vec<Line> {
    vec![
        ("wk biscuit inspector".into(), Class::Title),
        (String::new(), Class::Meta),
        (
            "no token published (auth not configured)".into(),
            Class::Absent,
        ),
        (String::new(), Class::Meta),
        (
            "the host publishes this node's capability token".into(),
            Class::Meta,
        ),
        (
            "at /run/wk/token when node auth is on; polling...".into(),
            Class::Meta,
        ),
    ]
}

/// Decode + pretty-print the published token. `UnverifiedBiscuit` is the
/// right tool: this node holds no root public key — it decodes and prints
/// the datalog without verifying signatures (verification is the server's
/// job; we are only introspecting the credential we act under).
fn token_doc(hex: &str) -> Vec<Line> {
    let Some(bytes) = hex_decode(hex) else {
        return vec![
            ("wk biscuit inspector".into(), Class::Title),
            (String::new(), Class::Meta),
            ("token file is not valid hex".into(), Class::Absent),
        ];
    };
    let token = match UnverifiedBiscuit::from(&bytes) {
        Ok(t) => t,
        Err(e) => {
            return vec![
                ("wk biscuit inspector".into(), Class::Title),
                (String::new(), Class::Meta),
                ("token does not decode as a biscuit:".into(), Class::Absent),
                (format!("{e}"), Class::Meta),
            ];
        }
    };
    let blocks = token.block_count();
    let mut out = vec![(
        format!(
            "biscuit token - {blocks} block{}",
            if blocks == 1 { "" } else { "s" }
        ),
        Class::Title,
    )];
    let revs: Vec<String> = token
        .revocation_identifiers()
        .iter()
        .map(|r| r.iter().take(4).map(|b| format!("{b:02x}")).collect())
        .collect();
    out.push((format!("rev {}", revs.join(" ")), Class::Meta));
    for i in 0..blocks {
        out.push((String::new(), Class::Meta));
        let title = if i == 0 {
            "authority (block 0)".to_string()
        } else {
            format!("block {i} (attenuation)")
        };
        out.push((title, Class::Header));
        match token.print_block_source(i) {
            Ok(src) => {
                for l in src.lines() {
                    out.push((l.to_string(), classify(l)));
                }
            }
            Err(e) => out.push((format!("<block source unavailable: {e}>"), Class::Meta)),
        }
    }
    out
}

fn build_doc(raw: Option<&str>) -> Vec<Line> {
    match raw {
        None => absent_doc(),
        Some(hex) => token_doc(hex),
    }
}

/// Word-wrap the logical lines to `cols` characters (the live surface width
/// in glyphs); words longer than a line are hard-broken on char boundaries.
fn wrap(lines: &[Line], cols: usize) -> Vec<Line> {
    let cols = cols.max(8);
    let mut out = Vec::new();
    for (text, class) in lines {
        if text.chars().count() <= cols {
            out.push((text.clone(), *class));
            continue;
        }
        let mut cur = String::new();
        let mut cur_len = 0usize;
        for word in text.split(' ') {
            let word_len = word.chars().count();
            let sep = usize::from(cur_len > 0);
            if cur_len + sep + word_len <= cols {
                if sep == 1 {
                    cur.push(' ');
                }
                cur.push_str(word);
                cur_len += sep + word_len;
                continue;
            }
            if cur_len > 0 {
                out.push((std::mem::take(&mut cur), *class));
            }
            let mut rest = word;
            while rest.chars().count() > cols {
                let cut = rest
                    .char_indices()
                    .nth(cols)
                    .map(|(i, _)| i)
                    .unwrap_or(rest.len());
                let (head, tail) = rest.split_at(cut);
                out.push((head.to_string(), *class));
                rest = tail;
            }
            cur.push_str(rest);
            cur_len = rest.chars().count();
        }
        if cur_len > 0 {
            out.push((cur, *class));
        }
    }
    out
}

/// Software text: the public-domain font8x8 glyphs, scaled by an integer
/// factor, clipped to the surface. Codepoints outside ASCII render as '?'.
fn draw_text(
    px: &mut [u8],
    w: u32,
    h: u32,
    x: i32,
    y: i32,
    scale: u32,
    text: &str,
    color: [u8; 3],
) {
    let mut cx = x;
    for ch in text.chars() {
        let glyph = BASIC_LEGACY
            .get(ch as usize)
            .unwrap_or(&BASIC_LEGACY[b'?' as usize]);
        for (row, bits) in glyph.iter().enumerate() {
            for col in 0..8u32 {
                if bits & (1 << col) == 0 {
                    continue;
                }
                for dy in 0..scale {
                    for dx in 0..scale {
                        let sx = cx + (col * scale + dx) as i32;
                        let sy = y + (row as u32 * scale + dy) as i32;
                        if sx >= 0 && sy >= 0 && (sx as u32) < w && (sy as u32) < h {
                            let i = ((sy as u32 * w + sx as u32) * 4) as usize;
                            px[i..i + 3].copy_from_slice(&color);
                            px[i + 3] = 255;
                        }
                    }
                }
            }
        }
        cx += (8 * scale) as i32;
    }
}

/// The "updated" flash: a `thick`-pixel accent border around the surface.
fn draw_border(px: &mut [u8], w: u32, h: u32, thick: u32, color: [u8; 3]) {
    for y in 0..h {
        for x in 0..w {
            if x < thick || y < thick || x >= w - thick.min(w) || y >= h - thick.min(h) {
                let i = ((y * w + x) * 4) as usize;
                px[i..i + 3].copy_from_slice(&color);
                px[i + 3] = 255;
            }
        }
    }
}

struct Component;

impl Guest for Component {
    /// The node introspecting its own credentials: read /run/wk/token (the
    /// host re-publishes it on every token change), decode the Biscuit, and
    /// render its datalog blocks — polling ~1 Hz so a live
    /// `wk token attenuate <this node> 'check if ...'` shows up on the canvas
    /// a second later, flagged with an accent-border flash.
    fn run() {
        let surface = Surface::new(CreateDesc {
            width: Some(480),
            height: Some(360),
        });
        let ctx = Context::new();
        surface.connect_graphics_context(&ctx);
        let device = Device::new();
        device.connect_graphics_context(&ctx);

        let frame = surface.subscribe_frame();
        // Keep this pollable alive: subscribing flips the host's wants_scroll
        // flag, routing the wheel to this node in both the 2D and 3D views.
        let _scroll = surface.subscribe_pointer_scroll();

        let mut raw: Option<String> = std::fs::read_to_string(TOKEN_PATH).ok();
        let doc_of = |raw: &Option<String>| build_doc(raw.as_deref());
        let mut doc = doc_of(&raw);
        let mut wrapped: Vec<Line> = Vec::new();
        let mut last_cols = 0usize;
        let mut rewrap = true;

        let mut scroll: f32 = 0.0;
        let mut frames: u32 = 0;
        let mut flash: u32 = 0;
        let mut pixels: Vec<u8> = Vec::new();

        loop {
            frame.block();
            let _ = surface.get_frame();
            frames = frames.wrapping_add(1);

            // ~1 Hz: pick up the host's refresh of the token file. This is
            // the live part — attenuate this node and watch the block land.
            if frames % POLL_FRAMES == 0 {
                let now = std::fs::read_to_string(TOKEN_PATH).ok();
                if now != raw {
                    raw = now;
                    doc = doc_of(&raw);
                    rewrap = true;
                    flash = FLASH_FRAMES;
                }
            }

            // The host is resize-authoritative: take the live size each frame.
            let w = surface.width().max(1);
            let h = surface.height().max(1);
            let scale: u32 = if w >= 640 && h >= 480 { 2 } else { 1 };
            let cell = 8 * scale;
            let margin = cell;
            let cols = (w.saturating_sub(2 * margin) / cell).max(8) as usize;
            let rows = (h.saturating_sub(2 * margin) / cell).max(1) as usize;
            if rewrap || cols != last_cols {
                wrapped = wrap(&doc, cols);
                last_cols = cols;
                rewrap = false;
            }

            // Input: wheel + keys scroll the view; nothing else is needed.
            while let Some(ev) = surface.get_pointer_scroll() {
                scroll -= ev.delta_y as f32 * 3.0;
            }
            while surface.get_pointer_move().is_some() {}
            while surface.get_pointer_down().is_some() {}
            while surface.get_pointer_up().is_some() {}
            while let Some(ev) = surface.get_key_down() {
                match ev.key {
                    Some(Key::ArrowUp) => scroll -= 1.0,
                    Some(Key::ArrowDown) => scroll += 1.0,
                    Some(Key::PageUp) => scroll -= rows as f32,
                    Some(Key::PageDown) => scroll += rows as f32,
                    Some(Key::Home) => scroll = 0.0,
                    Some(Key::End) => scroll = f32::INFINITY,
                    _ => {}
                }
            }
            while surface.get_key_up().is_some() {}
            let max_scroll = wrapped.len().saturating_sub(rows) as f32;
            scroll = scroll.clamp(0.0, max_scroll);
            let first = scroll as usize;

            // Software render: dark background, one wrapped line per row.
            pixels.clear();
            pixels.resize((w as usize) * (h as usize) * 4, 0);
            for px in pixels.chunks_exact_mut(4) {
                px[..3].copy_from_slice(&BG);
                px[3] = 255;
            }
            for (row, (text, class)) in wrapped.iter().skip(first).take(rows).enumerate() {
                draw_text(
                    &mut pixels,
                    w,
                    h,
                    margin as i32,
                    (margin + row as u32 * cell) as i32,
                    scale,
                    text,
                    class.color(),
                );
            }
            if flash > 0 {
                flash -= 1;
                draw_border(&mut pixels, w, h, 3, ACCENT);
            }

            let buffer = Buffer::from_graphics_buffer(ctx.get_current_buffer());
            buffer.set(&pixels);
            ctx.present();
        }
    }
}

bindings::export!(Component with_types_in bindings);
