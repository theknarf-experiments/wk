//! The wk compositor: spawns self-driving wasi-gfx clients and composites the
//! surfaces they paint into draggable windows on an infinite canvas, routing
//! input back to the focused client. wk is "the OS + compositor"; the client
//! thinks it owns its window. The whole UI (windows, menu, text) is drawn by
//! hand as 2D quads via `render2d`; windowing/input is winit.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use winit::platform::pump_events::{EventLoopExtPumpEvents, PumpStatus};
use winit::window::WindowId;

use crate::host_shell::Gfx;
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

/// Map a winit physical key to the wasi-gfx W3C `key` code.
fn map_key(code: KeyCode) -> Option<Key> {
    use KeyCode as C;
    Some(match code {
        C::KeyA => Key::KeyA,
        C::KeyB => Key::KeyB,
        C::KeyC => Key::KeyC,
        C::KeyD => Key::KeyD,
        C::KeyE => Key::KeyE,
        C::KeyF => Key::KeyF,
        C::KeyG => Key::KeyG,
        C::KeyH => Key::KeyH,
        C::KeyI => Key::KeyI,
        C::KeyJ => Key::KeyJ,
        C::KeyK => Key::KeyK,
        C::KeyL => Key::KeyL,
        C::KeyM => Key::KeyM,
        C::KeyN => Key::KeyN,
        C::KeyO => Key::KeyO,
        C::KeyP => Key::KeyP,
        C::KeyQ => Key::KeyQ,
        C::KeyR => Key::KeyR,
        C::KeyS => Key::KeyS,
        C::KeyT => Key::KeyT,
        C::KeyU => Key::KeyU,
        C::KeyV => Key::KeyV,
        C::KeyW => Key::KeyW,
        C::KeyX => Key::KeyX,
        C::KeyY => Key::KeyY,
        C::KeyZ => Key::KeyZ,
        C::Digit0 => Key::Digit0,
        C::Digit1 => Key::Digit1,
        C::Digit2 => Key::Digit2,
        C::Digit3 => Key::Digit3,
        C::Digit4 => Key::Digit4,
        C::Digit5 => Key::Digit5,
        C::Digit6 => Key::Digit6,
        C::Digit7 => Key::Digit7,
        C::Digit8 => Key::Digit8,
        C::Digit9 => Key::Digit9,
        C::ArrowUp => Key::ArrowUp,
        C::ArrowDown => Key::ArrowDown,
        C::ArrowLeft => Key::ArrowLeft,
        C::ArrowRight => Key::ArrowRight,
        C::Space => Key::Space,
        C::Enter => Key::Enter,
        C::Tab => Key::Tab,
        C::Escape => Key::Escape,
        C::Backspace => Key::Backspace,
        C::ShiftLeft => Key::ShiftLeft,
        C::ShiftRight => Key::ShiftRight,
        C::ControlLeft => Key::ControlLeft,
        C::ControlRight => Key::ControlRight,
        C::AltLeft => Key::AltLeft,
        C::AltRight => Key::AltRight,
        C::SuperLeft => Key::MetaLeft,
        C::SuperRight => Key::MetaRight,
        _ => return None,
    })
}

fn key_event(code: KeyCode, mods: ModifiersState) -> KeyEvent {
    KeyEvent {
        key: map_key(code),
        text: None,
        alt_key: mods.alt_key(),
        ctrl_key: mods.control_key(),
        meta_key: mods.super_key(),
        shift_key: mods.shift_key(),
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
        let (tex, w, h) = match self.map.get(s) {
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

/// The compositor application: owns all state. winit drives it via
/// `ApplicationHandler`; the per-frame work happens in `frame`.
struct App {
    gfx: Option<Gfx>,
    persist_session: bool,
    host: PluginHost,
    registry: SurfaceRegistry,
    instance_reg: InstanceRegistry,
    available: Vec<PluginSpec>,

    views: HashMap<u64, (TextureId, u32, u32)>,
    text_cache: TextCache,

    cam: Camera,
    pan_target: [f32; 2],
    win_pos: HashMap<u64, [f32; 2]>,
    win_size: HashMap<u64, [f32; 2]>,
    z: Vec<u64>,
    kbd_focus: Option<u64>,
    drag: Option<Drag>,
    menu_open: bool,
    pending_layout: HashMap<u64, ([f32; 2], [f32; 2])>,

    // Input state, fed by winit events between frames.
    mouse: [f32; 2],
    lmb: bool,
    prev_lmb: bool,
    mods: ModifiersState,
    pan_delta: [f32; 2],
    /// Accumulated zoom multiplier this frame (1.0 = none); fed by Cmd/Ctrl +
    /// scroll and by trackpad pinch.
    zoom_factor: f32,
    zoom_focus: [f32; 2],
    key_events: Vec<(KeyEvent, bool)>,
    should_exit: bool,
}

impl App {
    fn new(plugins: &[PluginSpec], persist_session: bool) -> Result<Self, String> {
        let host = PluginHost::new().map_err(|e| format!("{e:#}"))?;
        let registry: SurfaceRegistry = Arc::new(Mutex::new(Vec::new()));
        let instance_reg: InstanceRegistry = Arc::new(Mutex::new(Vec::new()));
        let mut app = App {
            gfx: None,
            persist_session,
            host,
            registry,
            instance_reg,
            available: plugins.to_vec(),
            views: HashMap::new(),
            text_cache: TextCache::default(),
            cam: Camera {
                pan: [0.0, 0.0],
                zoom: 1.0,
            },
            pan_target: [0.0, 0.0],
            win_pos: HashMap::new(),
            win_size: HashMap::new(),
            z: Vec::new(),
            kbd_focus: None,
            drag: None,
            menu_open: false,
            pending_layout: HashMap::new(),
            mouse: [0.0, 0.0],
            lmb: false,
            prev_lmb: false,
            mods: ModifiersState::empty(),
            pan_delta: [0.0, 0.0],
            zoom_factor: 1.0,
            zoom_focus: [0.0, 0.0],
            key_events: Vec::new(),
            should_exit: false,
        };
        app.restore_session();
        Ok(app)
    }

    fn restore_session(&mut self) {
        if !self.persist_session {
            return;
        }
        let Some(saved) = crate::session::Session::load() else {
            return;
        };
        self.cam.pan = [saved.camera.0, saved.camera.1];
        self.cam.zoom = saved.camera.2;
        self.pan_target = self.cam.pan;
        for w in &saved.windows {
            let spec = self
                .available
                .iter()
                .find(|s| s.path == w.path)
                .cloned()
                .unwrap_or_else(|| PluginSpec::from_path(w.path.clone()));
            match self.host.spawn(
                &spec.path,
                &spec.label(),
                spec.size,
                self.registry.clone(),
                self.instance_reg.clone(),
            ) {
                Ok(id) => {
                    self.pending_layout.insert(id, (w.pos, w.size));
                }
                Err(e) => eprintln!("failed to restore {}: {e:#}", spec.label()),
            }
        }
    }

    fn launch(&mut self, spec: &PluginSpec) {
        if let Err(e) = self.host.spawn(
            &spec.path,
            &spec.label(),
            spec.size,
            self.registry.clone(),
            self.instance_reg.clone(),
        ) {
            eprintln!("failed to launch {}: {e:#}", spec.label());
        }
    }

    /// One compositor frame: update from input, drive surfaces, render.
    fn frame(&mut self) {
        let Some(mut gfx) = self.gfx.take() else {
            return;
        };
        self.host.tick_epoch();

        // Apply pan/zoom (zoom immediate, pan eased).
        if (self.zoom_factor - 1.0).abs() > f32::EPSILON {
            self.cam.zoom_at(self.zoom_factor, self.zoom_focus);
            self.pan_target = self.cam.pan;
        }
        self.pan_target[0] += self.pan_delta[0];
        self.pan_target[1] += self.pan_delta[1];
        self.cam.pan = [
            ease(self.cam.pan[0], self.pan_target[0]),
            ease(self.cam.pan[1], self.pan_target[1]),
        ];
        self.pan_delta = [0.0, 0.0];
        self.zoom_factor = 1.0;

        let mp = self.mouse;
        let lmb = self.lmb;
        let down_edge = lmb && !self.prev_lmb;
        let up_edge = !lmb && self.prev_lmb;
        let zf = self.cam.zoom;
        let fb = [
            gfx.surface_desc.width as f32,
            gfx.surface_desc.height as f32,
        ];

        // ---- sync windows with the instance registry ----
        let instances: Vec<SharedInstance> = self.instance_reg.lock().unwrap().clone();
        let inst_by_id: HashMap<u64, SharedInstance> =
            instances.iter().map(|i| (i.id, i.clone())).collect();
        for inst in &instances {
            if let std::collections::hash_map::Entry::Vacant(slot) = self.win_pos.entry(inst.id) {
                let (pos, size) = self.pending_layout.remove(&inst.id).unwrap_or_else(|| {
                    let step = (self.z.len() % 8) as f32 * 28.0;
                    let size = inst
                        .default_size
                        .map(|(w, h)| [w as f32, h as f32])
                        .unwrap_or([360.0, 260.0]);
                    ([40.0 + step, 56.0 + step], size)
                });
                slot.insert(pos);
                self.win_size.insert(inst.id, size);
                self.z.push(inst.id);
            }
        }
        self.z.retain(|id| inst_by_id.contains_key(id));

        let surfaces: Vec<SharedSurface> = self.registry.lock().unwrap().clone();
        let inst_surface: HashMap<u64, SharedSurface> = surfaces
            .iter()
            .map(|s| (s.lock().unwrap().instance_id, s.clone()))
            .collect();

        // ---- interaction ----
        let mut to_close: Vec<u64> = Vec::new();
        let menu_w = MENU_H + gfx.fonts.measure("Apps") as f32 + PAD;
        let apps_rect = [0.0, 0.0, menu_w, MENU_H];
        let item_w = self
            .available
            .iter()
            .map(|s| gfx.fonts.measure(&s.label()) as f32)
            .fold(120.0, f32::max)
            + 2.0 * PAD;

        if let Some(d) = &self.drag {
            if lmb {
                let mc = self.cam.to_canvas(mp);
                match d.mode {
                    DragMode::Move => {
                        self.win_pos
                            .insert(d.id, [mc[0] - d.grab[0], mc[1] - d.grab[1]]);
                    }
                    DragMode::Resize => {
                        let p = self.win_pos[&d.id];
                        self.win_size.insert(
                            d.id,
                            [
                                (mc[0] - p[0]).max(100.0),
                                (mc[1] - p[1]).max(TITLE_H + 40.0),
                            ],
                        );
                    }
                }
            } else {
                self.drag = None;
            }
        }

        if down_edge && self.drag.is_none() {
            let mut consumed = false;
            if contains(apps_rect, mp) {
                self.menu_open = !self.menu_open;
                consumed = true;
            } else if self.menu_open {
                for (i, spec) in self.available.iter().enumerate() {
                    let r = [
                        0.0,
                        MENU_H + i as f32 * MENU_H,
                        item_w,
                        MENU_H + (i + 1) as f32 * MENU_H,
                    ];
                    if contains(r, mp) {
                        let spec = spec.clone();
                        self.launch(&spec);
                        self.menu_open = false;
                        consumed = true;
                        break;
                    }
                }
                if !consumed {
                    self.menu_open = false;
                }
            }
            if !consumed {
                if let Some(&id) = self.z.iter().rev().find(|&&id| {
                    contains(
                        win_rect(self.cam, self.win_pos[&id], self.win_size[&id]),
                        mp,
                    )
                }) {
                    self.z.retain(|&x| x != id);
                    self.z.push(id);
                    // Clicking anywhere on a window activates it (header included).
                    self.kbd_focus = Some(id);
                    let r = win_rect(self.cam, self.win_pos[&id], self.win_size[&id]);
                    if contains(close_btn(r, zf), mp) {
                        to_close.push(id);
                    } else if contains(resize_grip(r, zf), mp) {
                        self.drag = Some(Drag {
                            id,
                            mode: DragMode::Resize,
                            grab: [0.0, 0.0],
                        });
                    } else if contains(title_bar(r, zf), mp) {
                        let mc = self.cam.to_canvas(mp);
                        let p = self.win_pos[&id];
                        self.drag = Some(Drag {
                            id,
                            mode: DragMode::Move,
                            grab: [mc[0] - p[0], mc[1] - p[1]],
                        });
                    }
                    consumed = true;
                }
            }
            if !consumed {
                // Clicked empty canvas: dismiss the menu and unfocus the app.
                self.menu_open = false;
                self.kbd_focus = None;
            }
        }

        // Route pointer to the surface under the cursor.
        if self.drag.is_none() && mp[1] >= MENU_H {
            if let Some(&id) = self.z.iter().rev().find(|&&id| {
                contains(
                    win_rect(self.cam, self.win_pos[&id], self.win_size[&id]),
                    mp,
                )
            }) {
                let r = win_rect(self.cam, self.win_pos[&id], self.win_size[&id]);
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
        if let Some(fid) = self.kbd_focus {
            if let Some(surf) = inst_surface.get(&fid) {
                let mut s = surf.lock().unwrap();
                for (ev, down) in &self.key_events {
                    if *down {
                        s.key_down.push_back(ev.clone());
                    } else {
                        s.key_up.push_back(ev.clone());
                    }
                }
            }
        }
        self.key_events.clear();

        // ---- drive surfaces ----
        for shared in &surfaces {
            let (sid, w, h, pixels) = {
                let mut s = shared.lock().unwrap();
                if let Some(size) = self.win_size.get(&s.instance_id) {
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
            let stale = self.views.get(&sid).map(|&(_, vw, vh)| vw != w || vh != h);
            match stale {
                None | Some(true) => {
                    if let Some((old, _, _)) = self.views.remove(&sid) {
                        gfx.renderer.remove_texture(old);
                    }
                    let init = pixels.unwrap_or_else(|| vec![0; (w * h * 4) as usize]);
                    let tex = gfx
                        .renderer
                        .create_texture(&gfx.device, &gfx.queue, w, h, &init);
                    self.views.insert(sid, (tex, w, h));
                }
                Some(false) => {
                    if let Some(px) = &pixels {
                        gfx.renderer
                            .update_texture(&gfx.queue, self.views[&sid].0, w, h, px);
                    }
                }
            }
        }

        // ---- build quads ----
        let white = gfx.renderer.white;
        let full = [0.0, 0.0, fb[0], fb[1]];
        let mut quads: Vec<Quad> = Vec::new();

        for &id in &self.z {
            let pos = self.win_pos[&id];
            let size = self.win_size[&id];
            let r = win_rect(self.cam, pos, size);
            if r[2] < 0.0 || r[0] > fb[0] || r[3] < 0.0 || r[1] > fb[1] {
                continue;
            }
            let clip = intersect(r, full);
            let focused = self.kbd_focus == Some(id);
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

            if let Some(inst) = inst_by_id.get(&id) {
                let label = if inst.finished.load(Ordering::Relaxed) {
                    format!("{} (exited)", inst.name)
                } else {
                    inst.name.clone()
                };
                let ty = tb[1] + (TITLE_H * zf - gfx.fonts.line_height() as f32 * zf) * 0.5;
                self.text_cache.draw(
                    &mut quads,
                    &mut gfx.renderer,
                    &gfx.fonts,
                    &gfx.device,
                    &gfx.queue,
                    &label,
                    tb[0] + PAD * zf,
                    ty,
                    zf,
                    TEXT,
                    intersect(tb, full),
                );
            }

            let cb = close_btn(r, zf);
            if contains(cb, mp) {
                quads.push(Quad::solid(white, cb, CLOSE_HOT, clip));
            }
            self.text_cache.draw(
                &mut quads,
                &mut gfx.renderer,
                &gfx.fonts,
                &gfx.device,
                &gfx.queue,
                "x",
                cb[0] + (cb[2] - cb[0]) * 0.28,
                cb[1] + (cb[3] - cb[1]) * 0.05,
                zf * 0.8,
                TEXT,
                clip,
            );

            let ca = content_rect(r, zf);
            let ca_clip = intersect(ca, full);
            let sid = inst_surface.get(&id).map(|s| s.lock().unwrap().id);
            if let Some(sid) = sid {
                if let Some(&(tex, _, _)) = self.views.get(&sid) {
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
                let line_h = gfx.fonts.line_height() as f32;
                let rows = ((size[1] - TITLE_H - BORDER) / line_h).max(1.0) as usize;
                let lines: Vec<&str> = txt.lines().collect();
                let start = lines.len().saturating_sub(rows);
                for (i, line) in lines[start..].iter().enumerate() {
                    let ly = ca[1] + i as f32 * line_h * zf;
                    self.text_cache.draw(
                        &mut quads,
                        &mut gfx.renderer,
                        &gfx.fonts,
                        &gfx.device,
                        &gfx.queue,
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

        quads.push(Quad::solid(white, [0.0, 0.0, fb[0], MENU_H], MENU_BG, full));
        if self.menu_open || contains(apps_rect, mp) {
            quads.push(Quad::solid(white, apps_rect, MENU_HOVER, full));
        }
        self.text_cache.draw(
            &mut quads,
            &mut gfx.renderer,
            &gfx.fonts,
            &gfx.device,
            &gfx.queue,
            "Apps",
            PAD,
            (MENU_H - gfx.fonts.line_height() as f32) * 0.5,
            1.0,
            TEXT,
            full,
        );
        if self.menu_open {
            for (i, spec) in self.available.iter().enumerate() {
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
                self.text_cache.draw(
                    &mut quads,
                    &mut gfx.renderer,
                    &gfx.fonts,
                    &gfx.device,
                    &gfx.queue,
                    &spec.label(),
                    PAD,
                    r[1] + (MENU_H - gfx.fonts.line_height() as f32) * 0.5,
                    1.0,
                    TEXT,
                    full,
                );
            }
        }
        self.text_cache.draw(
            &mut quads,
            &mut gfx.renderer,
            &gfx.fonts,
            &gfx.device,
            &gfx.queue,
            &format!("{:.0}%", self.cam.zoom * 100.0),
            8.0,
            fb[1] - gfx.fonts.line_height() as f32 - 6.0,
            1.0,
            [1.0, 1.0, 1.0, 0.6],
            full,
        );

        // ---- render ----
        let frame = match gfx.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f)
            | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            _ => {
                self.prev_lmb = lmb;
                self.gfx = Some(gfx);
                return;
            }
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = gfx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
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
            gfx.renderer
                .draw(&gfx.device, &gfx.queue, &mut rpass, fb, &quads);
        }
        gfx.queue.submit([encoder.finish()]);
        frame.present();

        // ---- quit closed instances ----
        for id in &to_close {
            if let Some(inst) = inst_by_id.get(id) {
                inst.kill.store(true, Ordering::Relaxed);
            }
            self.registry.lock().unwrap().retain(|s| {
                let mut g = s.lock().unwrap();
                if g.instance_id != *id {
                    return true;
                }
                g.closed = true;
                g.wake();
                if let Some((tex, _, _)) = self.views.remove(&g.id) {
                    gfx.renderer.remove_texture(tex);
                }
                false
            });
            self.instance_reg.lock().unwrap().retain(|x| x.id != *id);
            self.win_pos.remove(id);
            self.win_size.remove(id);
            self.z.retain(|x| x != id);
            if self.kbd_focus == Some(*id) {
                self.kbd_focus = None;
            }
        }

        self.prev_lmb = lmb;
        self.gfx = Some(gfx);
    }

    fn save_session(&self) {
        if !self.persist_session {
            return;
        }
        let windows = self
            .instance_reg
            .lock()
            .unwrap()
            .iter()
            .filter_map(|inst| {
                Some(crate::session::SessionWindow {
                    path: inst.path.clone(),
                    pos: *self.win_pos.get(&inst.id)?,
                    size: *self.win_size.get(&inst.id)?,
                })
            })
            .collect();
        let saved = crate::session::Session {
            camera: (self.cam.pan[0], self.cam.pan[1], self.cam.zoom),
            windows,
        };
        if let Err(e) = saved.save() {
            eprintln!("failed to save session: {e}");
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.gfx.is_none() {
            match Gfx::new(event_loop) {
                Ok(gfx) => self.gfx = Some(gfx),
                Err(e) => {
                    eprintln!("failed to create window: {e}");
                    self.should_exit = true;
                }
            }
        }
    }

    fn window_event(&mut self, _el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let scale = self
            .gfx
            .as_ref()
            .map(|g| g.window.scale_factor())
            .unwrap_or(1.0);
        match event {
            WindowEvent::CloseRequested => self.should_exit = true,
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                if let Some(gfx) = &mut self.gfx {
                    gfx.resize();
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.mouse = [(position.x / scale) as f32, (position.y / scale) as f32];
            }
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } => self.lmb = state == ElementState::Pressed,
            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x, y),
                    MouseScrollDelta::PixelDelta(p) => (p.x as f32 / 50.0, p.y as f32 / 50.0),
                };
                if self.mods.control_key() || self.mods.super_key() {
                    self.zoom_factor *= ZOOM_STEP.powf(dy);
                    self.zoom_focus = self.mouse;
                } else {
                    self.pan_delta[0] += dx * SCROLL_PAN_SPEED;
                    self.pan_delta[1] += dy * SCROLL_PAN_SPEED;
                }
            }
            // Native trackpad pinch (macOS): delta is the incremental
            // magnification; zoom around the cursor.
            WindowEvent::PinchGesture { delta, .. } if delta.is_finite() => {
                self.zoom_factor *= (1.0 + delta as f32).clamp(0.1, 10.0);
                self.zoom_focus = self.mouse;
            }
            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),
            WindowEvent::KeyboardInput { event, .. } => {
                if let PhysicalKey::Code(code) = event.physical_key {
                    if code == KeyCode::Escape && event.state == ElementState::Pressed {
                        self.should_exit = true;
                    }
                    self.key_events.push((
                        key_event(code, self.mods),
                        event.state == ElementState::Pressed,
                    ));
                }
            }
            _ => {}
        }
    }
}

pub fn run(plugins: &[PluginSpec], persist_session: bool) -> Result<(), String> {
    let mut event_loop = EventLoop::builder().build().map_err(|e| e.to_string())?;
    let mut app = App::new(plugins, persist_session)?;
    loop {
        let status = event_loop.pump_app_events(Some(Duration::ZERO), &mut app);
        if let PumpStatus::Exit(_) = status {
            break;
        }
        if app.should_exit {
            break;
        }
        if app.gfx.is_some() {
            app.frame();
        }
        std::thread::sleep(FRAME);
    }
    app.save_session();
    Ok(())
}
