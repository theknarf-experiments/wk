//! Shared host setup: an SDL3 window + wgpu device/surface + dear imgui and the
//! custom imgui renderer. The plugin compositor builds its run loop on top of a
//! `HostShell`.

use std::time::Instant;

use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use sdl3::video::Window;
use sdl3::{EventPump, Sdl};

use crate::imguirenderer::{Renderer, RendererConfig};
use crate::imguisdlhelper::ImguiSdl2;

/// Everything needed to drive a frame loop. Callers typically destructure this
/// into locals and run their own loop (see `compositor`).
pub struct HostShell {
    // `surface` is declared before `window` so it is dropped first: the surface
    // is created from the window's raw handle and must not outlive it.
    pub surface: wgpu::Surface<'static>,
    pub window: Window,
    pub sdl: Sdl,
    pub event_pump: EventPump,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub surface_desc: wgpu::SurfaceConfiguration,
    pub imgui: imgui::Context,
    pub renderer: Renderer,
    pub imgui_sdl2: ImguiSdl2,
    pub last_frame: Instant,
}

impl HostShell {
    pub fn new(title: &str) -> Result<Self, String> {
        let sdl = sdl3::init().map_err(|e| e.to_string())?;
        let video = sdl.video().map_err(|e| e.to_string())?;

        let window = video
            .window(title, 800, 600)
            .position_centered()
            .resizable()
            .metal_view()
            .high_pixel_density()
            .build()
            .map_err(|e| e.to_string())?;

        let (width, height) = window.size();

        let mut instance_desc = wgpu::InstanceDescriptor::new_without_display_handle();
        instance_desc.backends = wgpu::Backends::PRIMARY;
        let instance = wgpu::Instance::new(instance_desc);

        let surface = unsafe {
            let target = wgpu::SurfaceTargetUnsafe::RawHandle {
                raw_display_handle: Some(
                    window.display_handle().map_err(|e| e.to_string())?.as_raw(),
                ),
                raw_window_handle: window.window_handle().map_err(|e| e.to_string())?.as_raw(),
            };
            instance
                .create_surface_unsafe(target)
                .map_err(|e| e.to_string())?
        };

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .map_err(|e| e.to_string())?;

        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
                .map_err(|e| e.to_string())?;

        let surface_desc = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: wgpu::TextureFormat::Bgra8UnormSrgb,
            width,
            height,
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
            view_formats: vec![wgpu::TextureFormat::Bgra8Unorm],
        };
        surface.configure(&device, &surface_desc);

        let mut imgui = imgui::Context::create();
        imgui.set_ini_filename(None);

        let hidpi_factor = 1.0;
        let font_size = (13.0 * hidpi_factor) as f32;
        imgui.io_mut().font_global_scale = (1.0 / hidpi_factor) as f32;
        imgui
            .fonts()
            .add_font(&[imgui::FontSource::DefaultFontData {
                config: Some(imgui::FontConfig {
                    oversample_h: 1,
                    pixel_snap_h: true,
                    size_pixels: font_size,
                    ..Default::default()
                }),
            }]);

        let renderer_config = RendererConfig {
            texture_format: surface_desc.format,
            ..Default::default()
        };
        let renderer = Renderer::new(&mut imgui, &device, &queue, renderer_config);

        let imgui_sdl2 = ImguiSdl2::new(&mut imgui, &window, &sdl);
        let event_pump = sdl.event_pump().map_err(|e| e.to_string())?;

        Ok(Self {
            surface,
            window,
            sdl,
            event_pump,
            device,
            queue,
            surface_desc,
            imgui,
            renderer,
            imgui_sdl2,
            last_frame: Instant::now(),
        })
    }
}
