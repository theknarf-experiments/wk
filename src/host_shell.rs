//! Host graphics: a winit window + wgpu device/surface + the 2D quad renderer
//! and text fonts. Created lazily from winit's `ActiveEventLoop` (winit only
//! lets you make a window once the event loop is running).

use std::sync::Arc;

use winit::dpi::LogicalSize;
use winit::event_loop::ActiveEventLoop;
use winit::window::Window;

use crate::render2d::Renderer;
use crate::text::Fonts;

/// Base font size in (logical) pixels for host UI text.
pub const FONT_PX: f32 = 15.0;

pub struct Gfx {
    pub window: Arc<Window>,
    // `surface` borrows the window via the Arc and so is fine to keep as 'static.
    pub surface: wgpu::Surface<'static>,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub surface_desc: wgpu::SurfaceConfiguration,
    pub renderer: Renderer,
    pub fonts: Fonts,
}

impl Gfx {
    pub fn new(event_loop: &ActiveEventLoop) -> Result<Self, String> {
        let attrs = Window::default_attributes()
            .with_title("wk compositor")
            .with_inner_size(LogicalSize::new(800.0, 600.0));
        let window = Arc::new(event_loop.create_window(attrs).map_err(|e| e.to_string())?);

        // We render in logical pixels: configure the surface at logical size and
        // convert input from physical, so UI metrics stay resolution-independent.
        let scale = window.scale_factor();
        let phys = window.inner_size();
        let lw = ((phys.width as f64 / scale).round() as u32).max(1);
        let lh = ((phys.height as f64 / scale).round() as u32).max(1);

        let mut instance_desc = wgpu::InstanceDescriptor::new_without_display_handle();
        instance_desc.backends = wgpu::Backends::PRIMARY;
        let instance = wgpu::Instance::new(instance_desc);

        let surface = instance
            .create_surface(window.clone())
            .map_err(|e| e.to_string())?;

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
            width: lw,
            height: lh,
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
            view_formats: vec![wgpu::TextureFormat::Bgra8Unorm],
        };
        surface.configure(&device, &surface_desc);

        let renderer = Renderer::new(&device, &queue, surface_desc.format);
        let fonts = Fonts::new(FONT_PX)?;

        Ok(Gfx {
            window,
            surface,
            device,
            queue,
            surface_desc,
            renderer,
            fonts,
        })
    }

    /// Reconfigure the surface to the window's current logical size.
    pub fn resize(&mut self) {
        let scale = self.window.scale_factor();
        let phys = self.window.inner_size();
        self.surface_desc.width = ((phys.width as f64 / scale).round() as u32).max(1);
        self.surface_desc.height = ((phys.height as f64 / scale).round() as u32).max(1);
        self.surface.configure(&self.device, &self.surface_desc);
    }
}
