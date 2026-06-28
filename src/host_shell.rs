//! Shared host setup: an SDL3 window + wgpu device/surface + the 2D quad
//! renderer and text fonts. The plugin compositor builds its run loop on top of
//! a `HostShell`.

use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use sdl3::video::Window;
use sdl3::{EventPump, Sdl};

use crate::render2d::Renderer;
use crate::text::Fonts;

/// Base font size in pixels for host UI text.
pub const FONT_PX: f32 = 15.0;

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
    pub renderer: Renderer,
    pub fonts: Fonts,
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

        let renderer = Renderer::new(&device, &queue, surface_desc.format);
        let fonts = Fonts::new(FONT_PX)?;
        let event_pump = sdl.event_pump().map_err(|e| e.to_string())?;

        Ok(Self {
            surface,
            window,
            sdl,
            event_pump,
            device,
            queue,
            surface_desc,
            renderer,
            fonts,
        })
    }
}
