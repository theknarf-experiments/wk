//! The wk compositor: loads a plugin component and composites the surface it
//! paints into an imgui window. For this milestone the compositor drives the
//! plugin (calls `render` each frame); input routing and self-driving clients
//! come next.

use std::path::Path;
use std::time::Instant;

use imgui::Condition;
use sdl3::event::Event;
use sdl3::keyboard::Keycode;

use crate::host_shell::HostShell;
use crate::imguirenderer::{Texture, TextureConfig};
use crate::plugin::PluginHost;

/// Size of the virtual surface handed to the plugin, in pixels.
const SURFACE_W: u32 = 256;
const SURFACE_H: u32 = 256;

pub fn run(plugin_path: &Path) -> Result<(), String> {
    let host = PluginHost::new().map_err(|e| format!("{e:#}"))?;
    let mut plugin = host
        .instantiate(plugin_path)
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

    let start = Instant::now();

    // The plugin's virtual surface: a wgpu texture registered with the imgui
    // renderer so we can draw it inside a window with `imgui::Image`.
    let surface_tex = Texture::new(
        &device,
        &renderer,
        TextureConfig {
            label: Some("plugin-surface"),
            format: Some(wgpu::TextureFormat::Rgba8Unorm),
            size: wgpu::Extent3d {
                width: SURFACE_W,
                height: SURFACE_H,
                depth_or_array_layers: 1,
            },
            ..Default::default()
        },
    );
    let surface_id = renderer.textures.insert(surface_tex);

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

        let frame = match surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame)
            | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
            other => {
                eprintln!("dropped frame: {other:?}");
                continue;
            }
        };

        // Drive the plugin one frame and upload its pixels into the surface texture.
        let time_ms = start.elapsed().as_millis() as u64;
        let pixels = plugin
            .render(SURFACE_W, SURFACE_H, time_ms)
            .map_err(|e| format!("{e:#}"))?;
        if let Some(tex) = renderer.textures.get(surface_id) {
            tex.write(&queue, &pixels, SURFACE_W, SURFACE_H);
        }

        let ui = imgui.frame();
        {
            ui.window("plugin")
                .size(
                    [SURFACE_W as f32 + 16.0, SURFACE_H as f32 + 36.0],
                    Condition::FirstUseEver,
                )
                .build(|| {
                    imgui::Image::new(surface_id, [SURFACE_W as f32, SURFACE_H as f32]).build(ui);
                });
        }
        imgui_sdl2.prepare_render(&ui);

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("command_encoder"),
        });
        {
            let view = frame
                .texture
                .create_view(&wgpu::TextureViewDescriptor::default());
            let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
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
