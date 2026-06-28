//! The wk compositor: spawns self-driving wasi-gfx clients and composites the
//! surfaces they paint into draggable windows on an infinite canvas, routing
//! input back to the focused client. wk is "the OS + compositor"; the client
//! thinks it owns its window. The whole UI (windows, menu, text) is drawn by
//! hand as 2D quads via `render2d` — no immediate-mode GUI library.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sdl3::event::Event;
use sdl3::keyboard::{Keycode, Mod, Scancode};

use crate::host_shell::HostShell;
use crate::plugin::{
    InstanceRegistry, Key, KeyEvent, PluginHost, PointerEvent, ResizeEvent, SharedInstance,
    SharedSurface, SurfaceRegistry,
};
use crate::project::PluginSpec;
use crate::render2d::{Quad, Renderer, TextureId};
use crate::text::Fonts;

/// Target frame time (~60 fps).
const FRAME: Duration = Duration::from_nanos(1_000_000_000 / 60);
/// Canvas pixels panned per unit of scroll wheel.
const SCROLL_PAN_SPEED: f32 = 30.0;
/// Fraction of the remaining pan distance covered each frame.
const PAN_SMOOTH: f32 = 0.3;
/// Zoom multiplier per unit of zoom-scroll.
const ZOOM_STEP: f32 = 1.1;

/// Window title-bar height and border thickness, in canvas pixels.
const TITLE_H: f32 = 22.0;
const BORDER: f32 = 1.0;
/// Top menu bar height, in screen pixels (not zoomed).
const MENU_H: f32 = 26.0;
const PAD: f32 = 6.0;

const CLEAR: wgpu::Color = wgpu::Color {
    r: 0.05,
    g: 0.05,
    b: 0.08,
    a: 1.0,
};
const MENU_BG: [f32; 4] = [0.13, 0.13, 0.16, 1.0];
const MENU_HOVER: [f32; 4] = [0.26, 0.28, 0.34, 1.0];
const TITLE: [f32; 4] = [0.18, 0.19, 0.24, 1.0];
const TITLE_FOCUS: [f32; 4] = [0.24, 0.34, 0.52, 1.0];
const BODY: [f32; 4] = [0.10, 0.10, 0.13, 1.0];
const BORDER_COL: [f32; 4] = [0.32, 0.33, 0.38, 1.0];
const TEXT: [f32; 4] = [0.90, 0.90, 0.93, 1.0];
const CLOSE_HOT: [f32; 4] = [0.80, 0.30, 0.30, 1.0];

/// Map an SDL physical key to the wasi-gfx W3C `key` code.
fn map_key(sc: Scancode) -> Option<Key> {
    use Scancode as S;
    Some(match sc {
        S::A => Key::KeyA,
        S::B => Key::KeyB,
        S::C => Key::KeyC,
        S::D => Key::KeyD,
        S::E => Key::KeyE,
        S::F => Key::KeyF,
        S::G => Key::KeyG,
        S::H => Key::KeyH,
        S::I => Key::KeyI,
        S::J => Key::KeyJ,
        S::K => Key::KeyK,
        S::L => Key::KeyL,
        S::M => Key::KeyM,
        S::N => Key::KeyN,
        S::O => Key::KeyO,
        S::P => Key::KeyP,
        S::Q => Key::KeyQ,
        S::R => Key::KeyR,
        S::S => Key::KeyS,
        S::T => Key::KeyT,
        S::U => Key::KeyU,
        S::V => Key::KeyV,
        S::W => Key::KeyW,
        S::X => Key::KeyX,
        S::Y => Key::KeyY,
        S::Z => Key::KeyZ,
        S::_1 => Key::Digit1,
        S::_2 => Key::Digit2,
        S::_3 => Key::Digit3,
        S::_4 => Key::Digit4,
        S::_5 => Key::Digit5,
        S::_6 => Key::Digit6,
        S::_7 => Key::Digit7,
        S::_8 => Key::Digit8,
        S::_9 => Key::Digit9,
        S::_0 => Key::Digit0,
        S::Up => Key::ArrowUp,
        S::Down => Key::ArrowDown,
        S::Left => Key::ArrowLeft,
        S::Right => Key::ArrowRight,
        S::Space => Key::Space,
        S::Return => Key::Enter,
        S::Tab => Key::Tab,
        S::Escape => Key::Escape,
        S::Backspace => Key::Backspace,
        S::LShift => Key::ShiftLeft,
        S::RShift => Key::ShiftRight,
        S::LCtrl => Key::ControlLeft,
        S::RCtrl => Key::ControlRight,
        S::LAlt => Key::AltLeft,
        S::RAlt => Key::AltRight,
        S::LGui => Key::MetaLeft,
        S::RGui => Key::MetaRight,
        _ => return None,
    })
}

fn key_event(sc: Scancode, keymod: Mod) -> KeyEvent {
    KeyEvent {
        key: map_key(sc),
        text: None,
        alt_key: keymod.intersects(Mod::LALTMOD | Mod::RALTMOD),
        ctrl_key: keymod.intersects(Mod::LCTRLMOD | Mod::RCTRLMOD),
        meta_key: keymod.intersects(Mod::LGUIMOD | Mod::RGUIMOD),
        shift_key: keymod.intersects(Mod::LSHIFTMOD | Mod::RSHIFTMOD),
    }
}

/// The infinite-canvas camera: windows live in canvas space and map to screen
/// space by panning (scroll) and zooming (Cmd/Ctrl + scroll).
#[derive(Clone, Copy)]
struct Camera {
    pan: [f32; 2],
    zoom: f32,
}

impl Camera {
    fn to_screen(self, p: [f32; 2]) -> [f32; 2] {
        [
            self.pan[0] + p[0] * self.zoom,
            self.pan[1] + p[1] * self.zoom,
        ]
    }
    fn to_canvas(self, p: [f32; 2]) -> [f32; 2] {
        [
            (p[0] - self.pan[0]) / self.zoom,
            (p[1] - self.pan[1]) / self.zoom,
        ]
    }
    fn zoom_at(&mut self, factor: f32, focus: [f32; 2]) {
        let anchor = self.to_canvas(focus);
        self.zoom = (self.zoom * factor).clamp(0.2, 8.0);
        self.pan = [
            focus[0] - anchor[0] * self.zoom,
            focus[1] - anchor[1] * self.zoom,
        ];
    }
}

fn ease(current: f32, target: f32) -> f32 {
    let d = target - current;
    if d.abs() < 0.5 {
        target
    } else {
        current + d * PAN_SMOOTH
    }
}

fn contains(r: [f32; 4], p: [f32; 2]) -> bool {
    p[0] >= r[0] && p[0] < r[2] && p[1] >= r[1] && p[1] < r[3]
}

fn intersect(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    [
        a[0].max(b[0]),
        a[1].max(b[1]),
        a[2].min(b[2]),
        a[3].min(b[3]),
    ]
}

/// A window's full screen rect from its canvas pos/size under `cam`.
fn win_rect(cam: Camera, pos: [f32; 2], size: [f32; 2]) -> [f32; 4] {
    let s = cam.to_screen(pos);
    [
        s[0],
        s[1],
        s[0] + size[0] * cam.zoom,
        s[1] + size[1] * cam.zoom,
    ]
}

fn title_bar(r: [f32; 4], z: f32) -> [f32; 4] {
    [r[0], r[1], r[2], r[1] + TITLE_H * z]
}
fn close_btn(r: [f32; 4], z: f32) -> [f32; 4] {
    let s = (TITLE_H - 8.0) * z;
    let x1 = r[2] - 4.0 * z;
    let y0 = r[1] + 4.0 * z;
    [x1 - s, y0, x1, y0 + s]
}
fn resize_grip(r: [f32; 4], z: f32) -> [f32; 4] {
    let g = 16.0 * z;
    [r[2] - g, r[3] - g, r[2], r[3]]
}
fn content_rect(r: [f32; 4], z: f32) -> [f32; 4] {
    [
        r[0] + BORDER * z,
        r[1] + TITLE_H * z,
        r[2] - BORDER * z,
        r[3] - BORDER * z,
    ]
}

/// Caches rasterized strings as textures (white glyphs, tinted at draw time).
#[derive(Default)]
struct TextCache {
    map: HashMap<String, (TextureId, f32, f32)>,
}

impl TextCache {
    #[allow(clippy::too_many_arguments)]
    fn draw(
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
        let entry = match self.map.get(s) {
            Some(e) => *e,
            None => {
                let Some(g) = fonts.rasterize(s) else {
                    return;
                };
                if self.map.len() >= 1024 {
                    for (_, (tex, _, _)) in self.map.drain() {
                        r.remove_texture(tex);
                    }
                }
                let tex = r.create_texture(device, queue, g.width, g.height, &g.rgba);
                let e = (tex, g.width as f32, g.height as f32);
                self.map.insert(s.to_string(), e);
                e
            }
        };
        let (tex, w, h) = entry;
        quads.push(Quad {
            dst: [x, y, x + w * scale, y + h * scale],
            uv: [0.0, 0.0, 1.0, 1.0],
            color,
            tex,
            clip,
        });
    }
}

enum DragMode {
    Move,
    Resize,
}
struct Drag {
    id: u64,
    mode: DragMode,
    grab: [f32; 2],
}

pub fn run(plugins: &[PluginSpec], persist_session: bool) -> Result<(), String> {
    let host = PluginHost::new().map_err(|e| format!("{e:#}"))?;
    let registry: SurfaceRegistry = Arc::new(Mutex::new(Vec::new()));
    let instance_reg: InstanceRegistry = Arc::new(Mutex::new(Vec::new()));
    let available: Vec<PluginSpec> = plugins.to_vec();

    let HostShell {
        surface,
        // Kept alive: the wgpu surface borrows the window's handle.
        window: _window,
        sdl: _sdl,
        mut event_pump,
        device,
        queue,
        mut surface_desc,
        mut renderer,
        fonts,
    } = HostShell::new("wk compositor")?;

    // Per-surface texture (id -> (texture, w, h)).
    let mut views: HashMap<u64, (TextureId, u32, u32)> = HashMap::new();
    let mut text_cache = TextCache::default();

    let mut cam = Camera {
        pan: [0.0, 0.0],
        zoom: 1.0,
    };
    let mut pan_target = [0.0f32, 0.0];

    // Window state by instance id; `z` is back-to-front draw/hit order.
    let mut win_pos: HashMap<u64, [f32; 2]> = HashMap::new();
    let mut win_size: HashMap<u64, [f32; 2]> = HashMap::new();
    let mut z: Vec<u64> = Vec::new();
    let mut pending_layout: HashMap<u64, ([f32; 2], [f32; 2])> = HashMap::new();

    let mut kbd_focus: Option<u64> = None;
    let mut drag: Option<Drag> = None;
    let mut menu_open = false;
    let mut prev_lmb = false;

    // Restore the saved workspace.
    if persist_session {
        if let Some(saved) = crate::session::Session::load() {
            cam.pan = [saved.camera.0, saved.camera.1];
            cam.zoom = saved.camera.2;
            pan_target = cam.pan;
            for w in &saved.windows {
                let spec = available
                    .iter()
                    .find(|s| s.path == w.path)
                    .cloned()
                    .unwrap_or_else(|| PluginSpec::from_path(w.path.clone()));
                match host.spawn(
                    &spec.path,
                    &spec.label(),
                    spec.size,
                    registry.clone(),
                    instance_reg.clone(),
                ) {
                    Ok(id) => {
                        pending_layout.insert(id, (w.pos, w.size));
                    }
                    Err(e) => eprintln!("failed to restore {}: {e:#}", spec.label()),
                }
            }
        }
    }

    let exit: Result<(), String> = 'running: loop {
        host.tick_epoch();

        let mut key_events: Vec<(KeyEvent, bool)> = Vec::new();
        let mut pan_delta = [0.0f32, 0.0];
        let mut zoom_step = 0.0f32;
        let mut zoom_focus = [0.0f32, 0.0];

        let zoom_mod = {
            let ks = event_pump.keyboard_state();
            ks.is_scancode_pressed(Scancode::LGui)
                || ks.is_scancode_pressed(Scancode::RGui)
                || ks.is_scancode_pressed(Scancode::LCtrl)
                || ks.is_scancode_pressed(Scancode::RCtrl)
        };

        for event in event_pump.poll_iter() {
            match event {
                Event::Quit { .. }
                | Event::KeyDown {
                    keycode: Some(Keycode::Escape),
                    ..
                } => break 'running Ok(()),
                Event::Window {
                    win_event: sdl3::event::WindowEvent::Resized(w, h),
                    ..
                } => {
                    surface_desc.width = (w as u32).max(1);
                    surface_desc.height = (h as u32).max(1);
                    surface.configure(&device, &surface_desc);
                }
                Event::MouseWheel {
                    x,
                    y,
                    mouse_x,
                    mouse_y,
                    ..
                } => {
                    if zoom_mod {
                        zoom_step += y;
                        zoom_focus = [mouse_x, mouse_y];
                    } else {
                        pan_delta[0] -= x * SCROLL_PAN_SPEED;
                        pan_delta[1] += y * SCROLL_PAN_SPEED;
                    }
                }
                Event::KeyDown {
                    scancode: Some(sc),
                    keymod,
                    ..
                } => key_events.push((key_event(sc, keymod), true)),
                Event::KeyUp {
                    scancode: Some(sc),
                    keymod,
                    ..
                } => key_events.push((key_event(sc, keymod), false)),
                _ => {}
            }
        }

        // Apply pan/zoom to the camera (zoom immediate, pan eased).
        if zoom_step != 0.0 {
            cam.zoom_at(ZOOM_STEP.powf(zoom_step), zoom_focus);
            pan_target = cam.pan;
        }
        pan_target[0] += pan_delta[0];
        pan_target[1] += pan_delta[1];
        cam.pan = [
            ease(cam.pan[0], pan_target[0]),
            ease(cam.pan[1], pan_target[1]),
        ];

        let mouse = event_pump.mouse_state();
        let mp = [mouse.x(), mouse.y()];
        let lmb = mouse.left();
        let down_edge = lmb && !prev_lmb;
        let up_edge = !lmb && prev_lmb;
        let zf = cam.zoom;
        let fb = [surface_desc.width as f32, surface_desc.height as f32];

        // ---- sync windows with the instance registry ----
        let instances: Vec<SharedInstance> = instance_reg.lock().unwrap().clone();
        let inst_by_id: HashMap<u64, SharedInstance> =
            instances.iter().map(|i| (i.id, i.clone())).collect();
        for inst in &instances {
            if let std::collections::hash_map::Entry::Vacant(slot) = win_pos.entry(inst.id) {
                let (pos, size) = pending_layout.remove(&inst.id).unwrap_or_else(|| {
                    let step = (z.len() % 8) as f32 * 28.0;
                    let size = inst
                        .default_size
                        .map(|(w, h)| [w as f32, h as f32])
                        .unwrap_or([360.0, 260.0]);
                    ([40.0 + step, 56.0 + step], size)
                });
                slot.insert(pos);
                win_size.insert(inst.id, size);
                z.push(inst.id);
            }
        }
        z.retain(|id| inst_by_id.contains_key(id));

        // Surfaces snapshot + instance -> surface map (for input routing).
        let surfaces: Vec<SharedSurface> = registry.lock().unwrap().clone();
        let inst_surface: HashMap<u64, SharedSurface> = surfaces
            .iter()
            .map(|s| (s.lock().unwrap().instance_id, s.clone()))
            .collect();

        // ---- interaction ----
        let mut to_close: Vec<u64> = Vec::new();
        let menu_w = MENU_H + fonts.measure("Apps") as f32 + PAD;
        let apps_rect = [0.0, 0.0, menu_w, MENU_H];
        let item_w = available
            .iter()
            .map(|s| fonts.measure(&s.label()) as f32)
            .fold(120.0, f32::max)
            + 2.0 * PAD;

        // Continue an in-progress drag.
        if let Some(d) = &drag {
            if lmb {
                let mc = cam.to_canvas(mp);
                match d.mode {
                    DragMode::Move => {
                        win_pos.insert(d.id, [mc[0] - d.grab[0], mc[1] - d.grab[1]]);
                    }
                    DragMode::Resize => {
                        let p = win_pos[&d.id];
                        win_size.insert(
                            d.id,
                            [
                                (mc[0] - p[0]).max(100.0),
                                (mc[1] - p[1]).max(TITLE_H + 40.0),
                            ],
                        );
                    }
                }
            } else {
                drag = None;
            }
        }

        // A fresh press: menus first, then windows (front-to-back).
        let mut consumed = false;
        if down_edge && drag.is_none() {
            if contains(apps_rect, mp) {
                menu_open = !menu_open;
                consumed = true;
            } else if menu_open {
                for (i, spec) in available.iter().enumerate() {
                    let r = [
                        0.0,
                        MENU_H + i as f32 * MENU_H,
                        item_w,
                        MENU_H + (i + 1) as f32 * MENU_H,
                    ];
                    if contains(r, mp) {
                        if let Err(e) = host.spawn(
                            &spec.path,
                            &spec.label(),
                            spec.size,
                            registry.clone(),
                            instance_reg.clone(),
                        ) {
                            eprintln!("failed to launch {}: {e:#}", spec.label());
                        }
                        menu_open = false;
                        consumed = true;
                        break;
                    }
                }
                if !consumed {
                    menu_open = false;
                }
            }
            if !consumed {
                // Topmost window under the cursor.
                if let Some(&id) = z
                    .iter()
                    .rev()
                    .find(|&&id| contains(win_rect(cam, win_pos[&id], win_size[&id]), mp))
                {
                    z.retain(|&x| x != id);
                    z.push(id);
                    let r = win_rect(cam, win_pos[&id], win_size[&id]);
                    if contains(close_btn(r, zf), mp) {
                        to_close.push(id);
                    } else if contains(resize_grip(r, zf), mp) {
                        drag = Some(Drag {
                            id,
                            mode: DragMode::Resize,
                            grab: [0.0, 0.0],
                        });
                    } else if contains(title_bar(r, zf), mp) {
                        let mc = cam.to_canvas(mp);
                        let p = win_pos[&id];
                        drag = Some(Drag {
                            id,
                            mode: DragMode::Move,
                            grab: [mc[0] - p[0], mc[1] - p[1]],
                        });
                    } else {
                        kbd_focus = Some(id);
                    }
                    consumed = true;
                }
            }
            if !consumed {
                menu_open = false;
            }
        }

        // Route pointer to the surface under the cursor (when not dragging/menu).
        if drag.is_none() && mp[1] >= MENU_H {
            if let Some(&id) = z
                .iter()
                .rev()
                .find(|&&id| contains(win_rect(cam, win_pos[&id], win_size[&id]), mp))
            {
                let r = win_rect(cam, win_pos[&id], win_size[&id]);
                let ca = content_rect(r, zf);
                if contains(ca, mp) {
                    if let Some(surf) = inst_surface.get(&id) {
                        let local = PointerEvent {
                            x: ((mp[0] - ca[0]) / zf) as f64,
                            y: ((mp[1] - ca[1]) / zf) as f64,
                        };
                        let mut s = surf.lock().unwrap();
                        s.pointer_move.push_back(local);
                        if down_edge {
                            s.pointer_down.push_back(local);
                        }
                        if up_edge {
                            s.pointer_up.push_back(local);
                        }
                    }
                }
            }
        }

        // Keyboard to the focused window's surface.
        if let Some(fid) = kbd_focus {
            if let Some(surf) = inst_surface.get(&fid) {
                let mut s = surf.lock().unwrap();
                for (ev, down) in &key_events {
                    if *down {
                        s.key_down.push_back(ev.clone());
                    } else {
                        s.key_up.push_back(ev.clone());
                    }
                }
            }
        }

        // ---- drive surfaces: sync size, upload pixels, signal a frame ----
        for shared in &surfaces {
            let (sid, w, h, pixels) = {
                let mut s = shared.lock().unwrap();
                if let Some(size) = win_size.get(&s.instance_id) {
                    let cw = (size[0] - 2.0 * BORDER).max(16.0) as u32;
                    let ch = (size[1] - TITLE_H - BORDER).max(16.0) as u32;
                    if cw != s.width || ch != s.height {
                        s.width = cw;
                        s.height = ch;
                        s.pixels = vec![0; (cw * ch * 4) as usize];
                        s.resize = Some(ResizeEvent {
                            width: cw,
                            height: ch,
                        });
                    }
                }
                let ready = s.pixels.len() == (s.width * s.height * 4) as usize;
                let px = ready.then(|| s.pixels.clone());
                let out = (s.id, s.width, s.height, px);
                s.frame_ready = true;
                s.wake();
                out
            };
            if w == 0 || h == 0 {
                continue;
            }
            let stale = views.get(&sid).map(|&(_, vw, vh)| vw != w || vh != h);
            match stale {
                None | Some(true) => {
                    if let Some((old, _, _)) = views.remove(&sid) {
                        renderer.remove_texture(old);
                    }
                    let init = pixels.unwrap_or_else(|| vec![0; (w * h * 4) as usize]);
                    let tex = renderer.create_texture(&device, &queue, w, h, &init);
                    views.insert(sid, (tex, w, h));
                }
                Some(false) => {
                    if let Some(px) = &pixels {
                        renderer.update_texture(&queue, views[&sid].0, w, h, px);
                    }
                }
            }
        }

        // ---- build the frame's quads ----
        let white = renderer.white;
        let full = [0.0, 0.0, fb[0], fb[1]];
        let mut quads: Vec<Quad> = Vec::new();

        for &id in &z {
            let pos = win_pos[&id];
            let size = win_size[&id];
            let r = win_rect(cam, pos, size);
            if r[2] < 0.0 || r[0] > fb[0] || r[3] < 0.0 || r[1] > fb[1] {
                continue;
            }
            let clip = intersect(r, full);
            let focused = kbd_focus == Some(id);
            // Border, body, title bar.
            quads.push(Quad::solid(white, r, BORDER_COL, clip));
            let body = [
                r[0] + BORDER * zf,
                r[1] + BORDER * zf,
                r[2] - BORDER * zf,
                r[3] - BORDER * zf,
            ];
            quads.push(Quad::solid(white, body, BODY, clip));
            let tb = title_bar(r, zf);
            quads.push(Quad::solid(
                white,
                tb,
                if focused { TITLE_FOCUS } else { TITLE },
                clip,
            ));

            // Title text (vertically centred in the bar).
            if let Some(inst) = inst_by_id.get(&id) {
                let label = if inst.finished.load(Ordering::Relaxed) {
                    format!("{} (exited)", inst.name)
                } else {
                    inst.name.clone()
                };
                let ty = tb[1] + (TITLE_H * zf - fonts.line_height() as f32 * zf) * 0.5;
                text_cache.draw(
                    &mut quads,
                    &mut renderer,
                    &fonts,
                    &device,
                    &queue,
                    &label,
                    tb[0] + PAD * zf,
                    ty,
                    zf,
                    TEXT,
                    intersect(tb, full),
                );
            }

            // Close button.
            let cb = close_btn(r, zf);
            if contains(cb, mp) {
                quads.push(Quad::solid(white, cb, CLOSE_HOT, clip));
            }
            text_cache.draw(
                &mut quads,
                &mut renderer,
                &fonts,
                &device,
                &queue,
                "x",
                cb[0] + (cb[2] - cb[0]) * 0.28,
                cb[1] + (cb[3] - cb[1]) * 0.05,
                zf * 0.8,
                TEXT,
                clip,
            );

            // Content: surface texture or console text.
            let ca = content_rect(r, zf);
            let ca_clip = intersect(ca, full);
            let sid = inst_surface.get(&id).map(|s| s.lock().unwrap().id);
            if let Some(sid) = sid {
                if let Some(&(tex, _, _)) = views.get(&sid) {
                    quads.push(Quad {
                        dst: ca,
                        uv: [0.0, 0.0, 1.0, 1.0],
                        color: [1.0, 1.0, 1.0, 1.0],
                        tex,
                        clip: ca_clip,
                    });
                }
            } else if let Some(inst) = inst_by_id.get(&id) {
                let bytes = inst.console.contents();
                let txt = String::from_utf8_lossy(&bytes);
                let line_h = fonts.line_height() as f32;
                let rows = ((size[1] - TITLE_H - BORDER) / line_h).max(1.0) as usize;
                let lines: Vec<&str> = txt.lines().collect();
                let start = lines.len().saturating_sub(rows);
                for (i, line) in lines[start..].iter().enumerate() {
                    let ly = ca[1] + i as f32 * line_h * zf;
                    text_cache.draw(
                        &mut quads,
                        &mut renderer,
                        &fonts,
                        &device,
                        &queue,
                        line,
                        ca[0] + 3.0 * zf,
                        ly,
                        zf,
                        TEXT,
                        ca_clip,
                    );
                }
            }
        }

        // Menu bar (always on top), then the dropdown, then the zoom label.
        quads.push(Quad::solid(white, [0.0, 0.0, fb[0], MENU_H], MENU_BG, full));
        if menu_open || contains(apps_rect, mp) {
            quads.push(Quad::solid(white, apps_rect, MENU_HOVER, full));
        }
        text_cache.draw(
            &mut quads,
            &mut renderer,
            &fonts,
            &device,
            &queue,
            "Apps",
            PAD,
            (MENU_H - fonts.line_height() as f32) * 0.5,
            1.0,
            TEXT,
            full,
        );
        if menu_open {
            for (i, spec) in available.iter().enumerate() {
                let r = [
                    0.0,
                    MENU_H + i as f32 * MENU_H,
                    item_w,
                    MENU_H + (i + 1) as f32 * MENU_H,
                ];
                quads.push(Quad::solid(
                    white,
                    r,
                    if contains(r, mp) { MENU_HOVER } else { MENU_BG },
                    full,
                ));
                text_cache.draw(
                    &mut quads,
                    &mut renderer,
                    &fonts,
                    &device,
                    &queue,
                    &spec.label(),
                    PAD,
                    r[1] + (MENU_H - fonts.line_height() as f32) * 0.5,
                    1.0,
                    TEXT,
                    full,
                );
            }
        }
        text_cache.draw(
            &mut quads,
            &mut renderer,
            &fonts,
            &device,
            &queue,
            &format!("{:.0}%", cam.zoom * 100.0),
            8.0,
            fb[1] - fonts.line_height() as f32 - 6.0,
            1.0,
            [1.0, 1.0, 1.0, 0.6],
            full,
        );

        // ---- render ----
        let frame = match surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f)
            | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            _ => {
                std::thread::sleep(FRAME);
                prev_lmb = lmb;
                continue;
            }
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("frame"),
        });
        {
            let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(CLEAR),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            renderer.draw(&device, &queue, &mut rpass, fb, &quads);
        }
        queue.submit([encoder.finish()]);
        frame.present();

        // ---- quit closed instances (after present) ----
        for id in &to_close {
            if let Some(inst) = inst_by_id.get(id) {
                inst.kill.store(true, Ordering::Relaxed);
            }
            registry.lock().unwrap().retain(|s| {
                let mut g = s.lock().unwrap();
                if g.instance_id != *id {
                    return true;
                }
                g.closed = true;
                g.wake();
                if let Some((tex, _, _)) = views.remove(&g.id) {
                    renderer.remove_texture(tex);
                }
                false
            });
            instance_reg.lock().unwrap().retain(|x| x.id != *id);
            win_pos.remove(id);
            win_size.remove(id);
            z.retain(|x| x != id);
            if kbd_focus == Some(*id) {
                kbd_focus = None;
            }
        }

        prev_lmb = lmb;
        std::thread::sleep(FRAME);
    };

    // Persist the workspace: camera + each open window's canvas rect.
    if persist_session {
        let windows = instance_reg
            .lock()
            .unwrap()
            .iter()
            .filter_map(|inst| {
                Some(crate::session::SessionWindow {
                    path: inst.path.clone(),
                    pos: *win_pos.get(&inst.id)?,
                    size: *win_size.get(&inst.id)?,
                })
            })
            .collect();
        let saved = crate::session::Session {
            camera: (cam.pan[0], cam.pan[1], cam.zoom),
            windows,
        };
        if let Err(e) = saved.save() {
            eprintln!("failed to save session: {e}");
        }
    }

    exit
}
