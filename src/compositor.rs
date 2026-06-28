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

/// Derive a display name for a plugin from its file stem.
fn plugin_name(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "plugin".to_string())
}

/// Draw the console window for a non-graphical instance: its captured
/// stdout/stderr in a scrolling region. Returns `false` if its close box was
/// clicked this frame.
fn console_window(ui: &imgui::Ui, inst: &SharedInstance) -> bool {
    let finished = inst.finished.load(Ordering::Relaxed);
    let title = if finished {
        format!("{} (exited)##inst{}", inst.name, inst.id)
    } else {
        format!("{}##inst{}", inst.name, inst.id)
    };
    let bytes = inst.console.contents();
    let text = String::from_utf8_lossy(&bytes);

    let mut open = true;
    ui.window(title)
        .opened(&mut open)
        .size([460.0, 280.0], Condition::FirstUseEver)
        .build(|| {
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
        });
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

    'running: loop {
        // Advance the epoch so any killed instance traps and ends this frame.
        host.tick_epoch();

        // Key events captured this frame, delivered to the focused client below.
        let mut key_events: Vec<(KeyEvent, bool)> = Vec::new();

        for event in event_pump.poll_iter() {
            imgui_sdl2.handle_event(&mut imgui, &event);

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

        imgui_sdl2.prepare_frame(imgui.io_mut(), &window, &event_pump.mouse_state());

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
                // Keyboard goes to the focused client only.
                if input.focused {
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
            // Every instance gets a window: its surface(s) if it created any,
            // otherwise a console showing its captured stdout/stderr. Closing
            // any of an instance's windows quits the whole instance.
            for inst in &instances {
                let inst_surfaces: Vec<&SharedSurface> = surfaces
                    .iter()
                    .filter(|s| s.lock().unwrap().instance_id == inst.id)
                    .collect();

                if inst_surfaces.is_empty() {
                    if !console_window(ui, inst) {
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
                    let (tex, w, h) = (view.texture, view.width as f32, view.height as f32);
                    let mut input = SurfaceInput::default();
                    let mut open = true;
                    ui.window(format!("{title}##{sid}"))
                        .opened(&mut open)
                        .size([w + 16.0, h + 36.0], Condition::FirstUseEver)
                        .build(|| {
                            input.focused = ui.is_window_focused();

                            // Resize the client to fill the window's content area.
                            let avail = ui.content_region_avail();
                            input.resize = Some((clamp_size(avail[0]), clamp_size(avail[1])));

                            let origin = ui.cursor_screen_pos();
                            imgui::Image::new(tex, [w, h]).build(ui);
                            if ui.is_item_hovered() {
                                let mouse = ui.io().mouse_pos;
                                let local =
                                    ((mouse[0] - origin[0]) as f64, (mouse[1] - origin[1]) as f64);
                                input.moved = Some(local);
                                if ui.is_mouse_clicked(MouseButton::Left) {
                                    input.down.push(local);
                                }
                                if ui.is_mouse_released(MouseButton::Left) {
                                    input.up.push(local);
                                }
                            }
                        });
                    if !open {
                        to_close.push(inst.clone());
                    }
                    inputs.insert(sid, input);
                }
            }
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
                false
            });
            instance_reg
                .lock()
                .unwrap()
                .retain(|x| !Arc::ptr_eq(x, inst));
        }

        std::thread::sleep(FRAME);
    }
}
