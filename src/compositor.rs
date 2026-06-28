//! The wk compositor: spawns a self-driving wasi-gfx client and composites the
//! virtual surface(s) it paints into imgui windows, routing pointer input back
//! to the client. wk is "the OS + compositor"; the client thinks it owns its
//! window.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use imgui::{Condition, MouseButton};
use sdl3::event::Event;
use sdl3::keyboard::{Keycode, Mod, Scancode};

use crate::host_shell::HostShell;
use crate::imguirenderer::{Renderer, Texture, TextureConfig};
use crate::plugin::{
    InstanceRegistry, Key, KeyEvent, PluginHost, PointerEvent, ResizeEvent, SharedInstance,
    SharedSurface, SurfaceRegistry,
};

/// Target frame time (~60 fps).
const FRAME: Duration = Duration::from_nanos(1_000_000_000 / 60);

/// Canvas pixels panned per unit of scroll wheel.
const SCROLL_PAN_SPEED: f32 = 30.0;
/// Fraction of the remaining pan distance covered each frame (0..1); lower is
/// smoother but laggier.
const PAN_SMOOTH: f32 = 0.3;
/// Zoom multiplier per unit of zoom-scroll.
const ZOOM_STEP: f32 = 1.1;

/// Map an SDL physical key to the wasi-gfx W3C `key` code. Returns `None` for
/// keys we don't translate (the client still gets the event, just `key: none`).
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

/// Build a wasi-gfx key event from an SDL scancode + modifier state.
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

/// Per-surface compositor-side state: the wgpu texture we upload the client's
/// pixels into and draw with `imgui::Image`. Keyed by surface id.
struct SurfaceView {
    texture: imgui::TextureId,
    width: u32,
    height: u32,
}

/// Input collected over a surface's window during one frame, delivered to the
/// client on the next: pointer events and an optional resize request.
#[derive(Default)]
struct SurfaceInput {
    moved: Option<(f64, f64)>,
    down: Vec<(f64, f64)>,
    up: Vec<(f64, f64)>,
    resize: Option<(u32, u32)>,
    /// Whether this surface's window held keyboard focus last frame.
    focused: bool,
}

/// Clamp a requested surface size to something sane.
fn clamp_size(v: f32) -> u32 {
    (v.max(16.0) as u32).min(4096)
}

/// Move `current` a fraction `PAN_SMOOTH` of the way toward `target`, snapping
/// once within half a pixel so panning glides smoothly and then settles.
fn ease(current: f32, target: f32) -> f32 {
    let diff = target - current;
    if diff.abs() < 0.5 {
        target
    } else {
        current + diff * PAN_SMOOTH
    }
}

/// Push window chrome (padding, spacing, border, rounding) scaled by `zoom`, so
/// an app window's frame zooms uniformly with its contents rather than staying a
/// fixed pixel size. Combined with `set_window_font_scale` (title/text) and the
/// window's zoomed outer size, this gives a true visual zoom. The returned
/// tokens pop the styles when dropped, so keep them alive across the window.
fn push_zoom_chrome<'ui>(
    ui: &'ui imgui::Ui,
    base: &imgui::Style,
    zoom: f32,
) -> [imgui::StyleStackToken<'ui>; 5] {
    use imgui::StyleVar::*;
    let s2 = |v: [f32; 2]| [v[0] * zoom, v[1] * zoom];
    [
        ui.push_style_var(WindowPadding(s2(base.window_padding))),
        ui.push_style_var(FramePadding(s2(base.frame_padding))),
        ui.push_style_var(ItemSpacing(s2(base.item_spacing))),
        ui.push_style_var(WindowBorderSize(base.window_border_size * zoom)),
        ui.push_style_var(WindowRounding(base.window_rounding * zoom)),
    ]
}

/// Derive a display name for a plugin from its file stem.
fn plugin_name(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "plugin".to_string())
}

/// The infinite-canvas camera: app windows live in canvas space and are mapped
/// to screen space by panning (scroll) and zooming (Cmd/Ctrl + scroll). The top
/// menu bar is drawn in screen space and so stays fixed.
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
    /// Zoom by `factor` while keeping the canvas point under `focus` (screen
    /// pixels) fixed, so zooming homes in on the cursor.
    fn zoom_at(&mut self, factor: f32, focus: [f32; 2]) {
        let anchor = self.to_canvas(focus);
        self.zoom = (self.zoom * factor).clamp(0.2, 8.0);
        self.pan = [
            focus[0] - anchor[0] * self.zoom,
            focus[1] - anchor[1] * self.zoom,
        ];
    }
}

/// A window's last on-screen position and size, read back from imgui each frame
/// so the camera can move it when it pans/zooms.
#[derive(Clone, Copy)]
struct WinScreen {
    pos: [f32; 2],
    size: [f32; 2],
}

/// How to place a window this frame. imgui owns the position/size natively (so
/// the user can drag and resize), and we only *override* on frames where the
/// camera moved, transforming the window's last screen rect by the camera delta.
struct Placement {
    default_pos: [f32; 2],
    default_size: [f32; 2],
    force_pos: Option<[f32; 2]>,
    force_size: Option<[f32; 2]>,
}

fn placement(
    key: &str,
    default_size: [f32; 2],
    spawn_idx: usize,
    cam: &Camera,
    prev_cam: &Camera,
    last: &HashMap<String, WinScreen>,
) -> Placement {
    let step = (spawn_idx % 8) as f32 * 28.0;
    let default_pos = [40.0 + step, 56.0 + step];

    let mut force_pos = None;
    let mut force_size = None;
    let cam_moved = cam.pan != prev_cam.pan || cam.zoom != prev_cam.zoom;
    if cam_moved {
        if let Some(ls) = last.get(key) {
            // Keep the window pinned to its canvas spot as the camera moves.
            force_pos = Some(cam.to_screen(prev_cam.to_canvas(ls.pos)));
            if cam.zoom != prev_cam.zoom {
                let r = cam.zoom / prev_cam.zoom;
                force_size = Some([ls.size[0] * r, ls.size[1] * r]);
            }
        }
    }
    Placement {
        default_pos,
        default_size,
        force_pos,
        force_size,
    }
}

/// Draw the console window for a non-graphical instance: its captured
/// stdout/stderr in a scrolling region, placed on the canvas. Returns `false`
/// if its close box was clicked this frame.
fn console_window(
    ui: &imgui::Ui,
    inst: &SharedInstance,
    base_style: &imgui::Style,
    last_win: &mut HashMap<String, WinScreen>,
    cam: &Camera,
    prev_cam: &Camera,
    spawn_idx: usize,
) -> bool {
    let finished = inst.finished.load(Ordering::Relaxed);
    let title = if finished {
        format!("{} (exited)##inst{}", inst.name, inst.id)
    } else {
        format!("{}##inst{}", inst.name, inst.id)
    };
    let bytes = inst.console.contents();
    let text = String::from_utf8_lossy(&bytes);

    let key = format!("console:{}", inst.id);
    let p = placement(&key, [460.0, 280.0], spawn_idx, cam, prev_cam, last_win);

    let mut open = true;
    let mut cur = WinScreen {
        pos: p.default_pos,
        size: p.default_size,
    };
    let _chrome = push_zoom_chrome(ui, base_style, cam.zoom);
    let mut win = ui
        .window(title)
        .opened(&mut open)
        .position(p.default_pos, Condition::FirstUseEver)
        .size(p.default_size, Condition::FirstUseEver);
    if let Some(fp) = p.force_pos {
        win = win.position(fp, Condition::Always);
    }
    if let Some(fs) = p.force_size {
        win = win.size(fs, Condition::Always);
    }
    win.build(|| {
        ui.set_window_font_scale(cam.zoom);
        ui.child_window("##console")
            .horizontal_scrollbar(true)
            .build(|| {
                let at_bottom = ui.scroll_y() >= ui.scroll_max_y();
                ui.text_wrapped(text.as_ref());
                // Keep following the tail unless the user scrolled up.
                if at_bottom {
                    ui.set_scroll_here_y_with_ratio(1.0);
                }
            });
        cur = WinScreen {
            pos: ui.window_pos(),
            size: ui.window_size(),
        };
    });
    last_win.insert(key, cur);
    open
}

/// Return the `TextureId` for surface `sid` at size `w`x`h`, (re)creating the
/// backing wgpu texture when it is missing or the size changed.
fn surface_texture(
    views: &mut HashMap<u64, SurfaceView>,
    sid: u64,
    w: u32,
    h: u32,
    device: &wgpu::Device,
    renderer: &mut Renderer,
) -> imgui::TextureId {
    if let Some(v) = views.get(&sid) {
        if v.width == w && v.height == h {
            return v.texture;
        }
    }
    let tex = Texture::new(
        device,
        renderer,
        TextureConfig {
            label: Some("plugin-surface"),
            format: Some(wgpu::TextureFormat::Rgba8Unorm),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            ..Default::default()
        },
    );
    let texture = renderer.textures.insert(tex);
    if let Some(old) = views.insert(
        sid,
        SurfaceView {
            texture,
            width: w,
            height: h,
        },
    ) {
        renderer.textures.remove(old.texture);
    }
    texture
}

pub fn run(plugins: &[PathBuf]) -> Result<(), String> {
    let host = PluginHost::new().map_err(|e| format!("{e:#}"))?;
    let registry: SurfaceRegistry = Arc::new(Mutex::new(Vec::new()));
    let instance_reg: InstanceRegistry = Arc::new(Mutex::new(Vec::new()));

    // The project's plugins become launchable "apps" (named by file stem). The
    // workspace starts empty; instances are launched from the top bar.
    let available: Vec<(String, PathBuf)> = plugins
        .iter()
        .map(|p| (plugin_name(p), p.clone()))
        .collect();

    let HostShell {
        surface,
        window,
        sdl: _sdl,
        mut event_pump,
        device,
        queue,
        mut surface_desc,
        mut imgui,
        mut renderer,
        mut imgui_sdl2,
        mut last_frame,
    } = HostShell::new("wk compositor")?;

    // Per-surface compositor state, keyed by surface id so it survives removal.
    let mut views: HashMap<u64, SurfaceView> = HashMap::new();
    // Input over each surface's window, collected last frame.
    let mut inputs: HashMap<u64, SurfaceInput> = HashMap::new();
    // Infinite-canvas camera and each window's canvas-space rect. `pan_target`
    // is where scrolling wants the camera; `cam.pan` eases toward it each frame
    // so panning glides instead of jumping per scroll event.
    let mut cam = Camera {
        pan: [0.0, 0.0],
        zoom: 1.0,
    };
    // The camera as of last frame; windows are re-pinned only when it moves.
    let mut prev_cam = cam;
    let mut pan_target = [0.0f32, 0.0];
    // Each window's last on-screen rect, read back from imgui every frame.
    let mut last_win: HashMap<String, WinScreen> = HashMap::new();
    // The surface that keyboard goes to. Tracked ourselves (not imgui focus) so
    // that re-focusing the menu bar to keep it on top doesn't steal it.
    let mut kbd_focus: Option<u64> = None;

    'running: loop {
        // Advance the epoch so any killed instance traps and ends this frame.
        host.tick_epoch();

        // Key events captured this frame, delivered to the focused client below.
        let mut key_events: Vec<(KeyEvent, bool)> = Vec::new();
        // Canvas pan/zoom requested by scroll / Cmd-Ctrl+scroll this frame.
        let mut pan_delta = [0.0f32, 0.0];
        let mut zoom_step = 0.0f32;
        let mut zoom_focus = [0.0f32, 0.0];

        // Whether a zoom modifier (Cmd or Ctrl) is held: turns scroll into zoom.
        let zoom_mod = {
            let ks = event_pump.keyboard_state();
            ks.is_scancode_pressed(Scancode::LGui)
                || ks.is_scancode_pressed(Scancode::RGui)
                || ks.is_scancode_pressed(Scancode::LCtrl)
                || ks.is_scancode_pressed(Scancode::RCtrl)
        };

        for event in event_pump.poll_iter() {
            imgui_sdl2.handle_event(&mut imgui, &event);

            // Scroll pans the canvas; with the zoom modifier it zooms instead.
            // (SDL3 doesn't surface native trackpad pinch, so Cmd/Ctrl+scroll is
            // the zoom gesture.)
            if let Event::MouseWheel {
                x,
                y,
                mouse_x,
                mouse_y,
                ..
            } = event
            {
                if zoom_mod {
                    zoom_step += y;
                    zoom_focus = [mouse_x, mouse_y];
                } else {
                    pan_delta[0] -= x * SCROLL_PAN_SPEED;
                    pan_delta[1] += y * SCROLL_PAN_SPEED;
                }
            }

            // Capture keyboard regardless of imgui's own handling.
            match &event {
                Event::KeyDown {
                    scancode: Some(sc),
                    keymod,
                    ..
                } => key_events.push((key_event(*sc, *keymod), true)),
                Event::KeyUp {
                    scancode: Some(sc),
                    keymod,
                    ..
                } => key_events.push((key_event(*sc, *keymod), false)),
                _ => {}
            }

            if imgui_sdl2.ignore_event(&event) {
                continue;
            }
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
                    surface_desc.width = w as u32;
                    surface_desc.height = h as u32;
                    surface.configure(&device, &surface_desc);
                }
                _ => {}
            }
        }

        // Apply this frame's pan/zoom to the canvas camera. Zoom is immediate
        // (and re-anchors the pan target); panning eases toward its target.
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

        imgui_sdl2.prepare_frame(imgui.io_mut(), &window, &event_pump.mouse_state());
        // Scroll drives the canvas, not imgui's own window scrolling.
        imgui.io_mut().mouse_wheel = 0.0;
        imgui.io_mut().mouse_wheel_h = 0.0;

        let now = Instant::now();
        imgui.io_mut().delta_time = (now - last_frame).as_secs_f32();
        last_frame = now;

        // Snapshot the client surfaces (cheap Arc clones) without holding the
        // registry lock during compositing.
        let surfaces: Vec<SharedSurface> = registry.lock().unwrap().clone();

        // Upload each surface's latest pixels into its texture, then signal the
        // client to paint its next frame. This drives the client every
        // iteration, independent of whether the host window is presented.
        for shared in &surfaces {
            let (sid, w, h, pixels) = {
                let s = shared.lock().unwrap();
                let ready = s.pixels.len() == (s.width * s.height * 4) as usize;
                (s.id, s.width, s.height, ready.then(|| s.pixels.clone()))
            };
            if w == 0 || h == 0 {
                continue;
            }
            let tex = surface_texture(&mut views, sid, w, h, &device, &mut renderer);
            if let Some(pixels) = pixels {
                if let Some(t) = renderer.textures.get(tex) {
                    t.write(&queue, &pixels, w, h);
                }
            }

            // Deliver last frame's input, then signal the next frame.
            let mut s = shared.lock().unwrap();
            if let Some(input) = inputs.get(&sid) {
                if let Some((x, y)) = input.moved {
                    s.pointer_move.push_back(PointerEvent { x, y });
                }
                for &(x, y) in &input.down {
                    s.pointer_down.push_back(PointerEvent { x, y });
                }
                for &(x, y) in &input.up {
                    s.pointer_up.push_back(PointerEvent { x, y });
                }
                if let Some((rw, rh)) = input.resize {
                    if rw != s.width || rh != s.height {
                        s.width = rw;
                        s.height = rh;
                        s.pixels = vec![0; (rw * rh * 4) as usize];
                        s.resize = Some(ResizeEvent {
                            width: rw,
                            height: rh,
                        });
                    }
                }
                // Keyboard goes to the most recently focused client only.
                if kbd_focus == Some(sid) {
                    for (ev, down) in &key_events {
                        if *down {
                            s.key_down.push_back(ev.clone());
                        } else {
                            s.key_up.push_back(ev.clone());
                        }
                    }
                }
            }
            s.frame_ready = true;
            s.wake();
        }

        let frame = match surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame)
            | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
            _ => {
                std::thread::sleep(FRAME);
                continue;
            }
        };

        // Apps the user clicked to launch, and instances whose window was closed.
        let mut to_launch: Vec<(String, PathBuf)> = Vec::new();
        let mut to_close: Vec<SharedInstance> = Vec::new();
        // Snapshot of launched instances (cheap Arc clones).
        let instances: Vec<SharedInstance> = instance_reg.lock().unwrap().clone();

        let ui = imgui.frame();
        {
            // Base style, scaled per app window by the zoom level.
            let base_style = ui.clone_style();

            // Top bar: launch an instance of any project plugin (apps can have
            // multiple instances).
            ui.main_menu_bar(|| {
                ui.menu("Apps", || {
                    for (idx, (name, path)) in available.iter().enumerate() {
                        if ui.menu_item(format!("{name}##app{idx}")) {
                            to_launch.push((name.clone(), path.clone()));
                        }
                    }
                });
            });

            inputs.clear();
            // Every instance gets a window on the canvas: its surface(s) if it
            // created any, otherwise a console showing its captured
            // stdout/stderr. Each window is positioned and sized through the
            // camera (so it pans/zooms), and any drag/resize is captured back
            // into canvas space. Closing any window quits the whole instance.
            for (idx, inst) in instances.iter().enumerate() {
                let inst_surfaces: Vec<&SharedSurface> = surfaces
                    .iter()
                    .filter(|s| s.lock().unwrap().instance_id == inst.id)
                    .collect();

                if inst_surfaces.is_empty() {
                    if !console_window(ui, inst, &base_style, &mut last_win, &cam, &prev_cam, idx) {
                        to_close.push(inst.clone());
                    }
                    continue;
                }

                for shared in inst_surfaces {
                    let (sid, title) = {
                        let s = shared.lock().unwrap();
                        (s.id, s.title.clone())
                    };
                    let Some(view) = views.get(&sid) else {
                        continue;
                    };
                    let tex = view.texture;
                    let key = format!("surf:{sid}");
                    let default = [view.width as f32 + 16.0, view.height as f32 + 36.0];
                    let p = placement(&key, default, idx, &cam, &prev_cam, &last_win);

                    let mut input = SurfaceInput::default();
                    let mut open = true;
                    let mut cur = WinScreen {
                        pos: p.default_pos,
                        size: p.default_size,
                    };
                    let _chrome = push_zoom_chrome(ui, &base_style, cam.zoom);
                    let mut win = ui
                        .window(format!("{title}##{sid}"))
                        .opened(&mut open)
                        .position(p.default_pos, Condition::FirstUseEver)
                        .size(p.default_size, Condition::FirstUseEver);
                    if let Some(fp) = p.force_pos {
                        win = win.position(fp, Condition::Always);
                    }
                    if let Some(fs) = p.force_size {
                        win = win.size(fs, Condition::Always);
                    }
                    let zoom = cam.zoom;
                    win.build(|| {
                        ui.set_window_font_scale(zoom);
                        input.focused = ui.is_window_focused();

                        // Zoom is purely visual: the client keeps rendering at
                        // its canvas-space resolution (zoom-independent) and we
                        // display that texture scaled to fill the zoomed window.
                        let avail = ui.content_region_avail();
                        input.resize =
                            Some((clamp_size(avail[0] / zoom), clamp_size(avail[1] / zoom)));

                        let origin = ui.cursor_screen_pos();
                        imgui::Image::new(tex, avail).build(ui);
                        if ui.is_item_hovered() {
                            let mouse = ui.io().mouse_pos;
                            // Screen offset -> client pixel (texture is scaled by zoom).
                            let local = (
                                (mouse[0] - origin[0]) as f64 / zoom as f64,
                                (mouse[1] - origin[1]) as f64 / zoom as f64,
                            );
                            input.moved = Some(local);
                            if ui.is_mouse_clicked(MouseButton::Left) {
                                input.down.push(local);
                            }
                            if ui.is_mouse_released(MouseButton::Left) {
                                input.up.push(local);
                            }
                        }
                        cur = WinScreen {
                            pos: ui.window_pos(),
                            size: ui.window_size(),
                        };
                    });
                    last_win.insert(key, cur);
                    if input.focused {
                        kbd_focus = Some(sid);
                    }
                    if !open {
                        to_close.push(inst.clone());
                    }
                    inputs.insert(sid, input);
                }
            }

            // Keep the Apps menu bar on top of the (raisable) app windows: when
            // no mouse button is held we re-focus it to the front, which also
            // brings it to the top of the draw order. We skip this while the
            // mouse is down (so clicking/dragging a window isn't disturbed) and
            // while a popup/menu is open (else re-focusing would close it).
            // SAFETY: querying/focusing existing windows by imgui name; the main
            // menu bar always uses this name.
            let any_popup = imgui::sys::ImGuiPopupFlags_AnyPopup as imgui::sys::ImGuiPopupFlags;
            let popup_open = unsafe { imgui::sys::igIsPopupOpen(std::ptr::null(), any_popup) };
            if !ui.is_any_mouse_down() && !popup_open {
                unsafe { imgui::sys::igSetWindowFocus_Str(c"##MainMenuBar".as_ptr()) };
            }

            // Zoom level, lower-left corner.
            let h = ui.io().display_size[1];
            ui.get_foreground_draw_list().add_text(
                [10.0, h - 22.0],
                [1.0, 1.0, 1.0, 0.6],
                format!("{:.0}%", cam.zoom * 100.0),
            );
        }

        // Launch any apps clicked in the top bar (a fresh instance each click).
        for (name, path) in &to_launch {
            if let Err(e) = host.spawn(path, name, registry.clone(), instance_reg.clone()) {
                eprintln!("failed to launch {name}: {e:#}");
            }
        }

        imgui_sdl2.prepare_render(ui);

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("command_encoder"),
        });
        {
            let texture_view = frame
                .texture
                .create_view(&wgpu::TextureViewDescriptor::default());
            let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &texture_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.05,
                            g: 0.05,
                            b: 0.08,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            renderer
                .render(imgui.render(), &queue, &device, &mut rpass)
                .expect("Rendering failed");
        }
        queue.submit([encoder.finish()]);
        frame.present();

        // Quit closed instances AFTER rendering: this frame's draw data still
        // references their textures, so freeing them earlier panics the
        // renderer. Trip the kill switch (epoch-traps a busy guest and unwinds a
        // blocked one), close any surfaces (trapping a frame-blocked guest),
        // free their textures, and drop the instance.
        for inst in &to_close {
            inst.kill.store(true, Ordering::Relaxed);
            last_win.remove(&format!("console:{}", inst.id));
            registry.lock().unwrap().retain(|s| {
                let mut g = s.lock().unwrap();
                if g.instance_id != inst.id {
                    return true;
                }
                g.closed = true;
                g.wake();
                if let Some(v) = views.remove(&g.id) {
                    renderer.textures.remove(v.texture);
                }
                inputs.remove(&g.id);
                last_win.remove(&format!("surf:{}", g.id));
                false
            });
            instance_reg
                .lock()
                .unwrap()
                .retain(|x| !Arc::ptr_eq(x, inst));
        }

        // Remember this frame's camera so next frame can detect movement.
        prev_cam = cam;

        std::thread::sleep(FRAME);
    }
}
