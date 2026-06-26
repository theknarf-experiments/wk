//! The wk compositor: spawns a self-driving wasi-gfx client and composites the
//! virtual surface(s) it paints into imgui windows, routing pointer input back
//! to the client. wk is "the OS + compositor"; the client thinks it owns its
//! window.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use imgui::Condition;
use sdl3::event::Event;
use sdl3::keyboard::Keycode;

use crate::host_shell::HostShell;
use crate::imguirenderer::{Texture, TextureConfig};
use crate::plugin::{PluginHost, PointerEvent, SharedSurface, SurfaceRegistry};

/// Per-surface compositor-side state: the wgpu texture we upload the client's
/// pixels into and draw with `imgui::Image`.
struct SurfaceView {
    id: imgui::TextureId,
    width: u32,
    height: u32,
}

pub fn run(plugin_path: &Path) -> Result<(), String> {
    let host = PluginHost::new().map_err(|e| format!("{e:#}"))?;
    let registry: SurfaceRegistry = Arc::new(Mutex::new(Vec::new()));
    host.spawn(plugin_path, registry.clone())
        .map_err(|e| format!("{e:#}"))?;

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
    // Pointer position over each surface's image, from the previous frame.
    let mut hovers: Vec<Option<(f64, f64)>> = Vec::new();

    'running: loop {
        for event in event_pump.poll_iter() {
            imgui_sdl2.handle_event(&mut imgui, &event);
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
        let delta = now - last_frame;
        let delta_s = delta.as_secs() as f32 + delta.subsec_nanos() as f32 / 1_000_000_000.0;
        last_frame = now;
        imgui.io_mut().delta_time = delta_s;

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
            // (Re)create the texture if missing or resized.
            let needs_new = match views.get(i) {
                Some(v) => v.width != w || v.height != h,
                None => true,
            };
            if needs_new {
                let tex = Texture::new(
                    &device,
                    &renderer,
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
                if let Some(old) = views.get(i) {
                    renderer.textures.remove(old.id);
                }
                let view = SurfaceView {
                    id,
                    width: w,
                    height: h,
                };
                if i < views.len() {
                    views[i] = view;
                } else {
                    views.push(view);
                }
            }
            if let Some(pixels) = pixels {
                if let Some(tex) = renderer.textures.get(views[i].id) {
                    tex.write(&queue, &pixels, w, h);
                }
            }

            // Deliver last frame's pointer input, then signal the next frame.
            let mut s = shared.lock().unwrap();
            if let Some(Some((x, y))) = hovers.get(i).copied() {
                s.pointer_move.push_back(PointerEvent { x, y });
            }
            s.frame_ready = true;
            s.wake();
        }

        let frame = match surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame)
            | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
            _ => {
                std::thread::sleep(std::time::Duration::new(0, 1_000_000_000u32 / 60));
                continue;
            }
        };

        let ui = imgui.frame();
        {
            hovers.clear();
            for (i, _shared) in surfaces.iter().enumerate() {
                let Some(view) = views.get(i) else {
                    hovers.push(None);
                    continue;
                };
                let (id, w, h) = (view.id, view.width as f32, view.height as f32);
                let mut hover_local: Option<(f64, f64)> = None;
                ui.window(format!("plugin {i}"))
                    .size([w + 16.0, h + 36.0], Condition::FirstUseEver)
                    .build(|| {
                        let origin = ui.cursor_screen_pos();
                        imgui::Image::new(id, [w, h]).build(ui);
                        if ui.is_item_hovered() {
                            let mouse = ui.io().mouse_pos;
                            hover_local = Some((
                                (mouse[0] - origin[0]) as f64,
                                (mouse[1] - origin[1]) as f64,
                            ));
                        }
                    });
                hovers.push(hover_local);
            }
        }

        imgui_sdl2.prepare_render(&ui);

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

        std::thread::sleep(std::time::Duration::new(0, 1_000_000_000u32 / 60));
    }
}
