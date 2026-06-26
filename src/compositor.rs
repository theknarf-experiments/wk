//! The wk compositor: spawns a self-driving wasi-gfx client and composites the
//! virtual surface(s) it paints into imgui windows, routing pointer input back
//! to the client. wk is "the OS + compositor"; the client thinks it owns its
//! window.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use imgui::{Condition, MouseButton};
use sdl3::event::Event;
use sdl3::keyboard::{Keycode, Mod, Scancode};

use crate::host_shell::HostShell;
use crate::imguirenderer::{Renderer, Texture, TextureConfig};
use crate::plugin::{
    Key, KeyEvent, PluginHost, PointerEvent, ResizeEvent, SharedSurface, SurfaceRegistry,
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
/// pixels into and draw with `imgui::Image`.
struct SurfaceView {
    id: imgui::TextureId,
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

/// Return the `TextureId` for surface `i` at size `w`x`h`, (re)creating the
/// backing wgpu texture when it is missing or the size changed.
fn surface_texture(
    views: &mut Vec<SurfaceView>,
    i: usize,
    w: u32,
    h: u32,
    device: &wgpu::Device,
    renderer: &mut Renderer,
) -> imgui::TextureId {
    if let Some(v) = views.get(i) {
        if v.width == w && v.height == h {
            return v.id;
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
    let id = renderer.textures.insert(tex);
    let view = SurfaceView {
        id,
        width: w,
        height: h,
    };
    if let Some(old) = views.get(i) {
        renderer.textures.remove(old.id);
        views[i] = view;
    } else {
        views.push(view);
    }
    id
}

pub fn run(plugins: &[PathBuf]) -> Result<(), String> {
    let host = PluginHost::new().map_err(|e| format!("{e:#}"))?;
    let registry: SurfaceRegistry = Arc::new(Mutex::new(Vec::new()));
    for plugin in plugins {
        host.spawn(plugin, registry.clone())
            .map_err(|e| format!("{e:#}"))?;
    }

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

    let mut views: Vec<SurfaceView> = Vec::new();
    // Input over each surface's window, collected last frame.
    let mut inputs: Vec<SurfaceInput> = Vec::new();

    'running: loop {
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
        for (i, shared) in surfaces.iter().enumerate() {
            let (w, h, pixels) = {
                let s = shared.lock().unwrap();
                let ready = s.pixels.len() == (s.width * s.height * 4) as usize;
                (s.width, s.height, ready.then(|| s.pixels.clone()))
            };
            if w == 0 || h == 0 {
                continue;
            }
            let id = surface_texture(&mut views, i, w, h, &device, &mut renderer);
            if let Some(pixels) = pixels {
                if let Some(tex) = renderer.textures.get(id) {
                    tex.write(&queue, &pixels, w, h);
                }
            }

            // Deliver last frame's input, then signal the next frame.
            let mut s = shared.lock().unwrap();
            if let Some(input) = inputs.get(i) {
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

        let ui = imgui.frame();
        {
            inputs.clear();
            for (i, _shared) in surfaces.iter().enumerate() {
                let mut input = SurfaceInput::default();
                if let Some(view) = views.get(i) {
                    let (id, w, h) = (view.id, view.width as f32, view.height as f32);
                    ui.window(format!("plugin {i}"))
                        .size([w + 16.0, h + 36.0], Condition::FirstUseEver)
                        .build(|| {
                            input.focused = ui.is_window_focused();

                            // Resize the client to fill the window's content area.
                            let avail = ui.content_region_avail();
                            input.resize = Some((clamp_size(avail[0]), clamp_size(avail[1])));

                            let origin = ui.cursor_screen_pos();
                            imgui::Image::new(id, [w, h]).build(ui);
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
                }
                inputs.push(input);
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

        std::thread::sleep(FRAME);
    }
}
