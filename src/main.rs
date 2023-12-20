extern crate sdl2;
extern crate wgpu;
extern crate imgui;
extern crate pollster;
extern crate wk;

use std::time::Instant;

use sdl2::event::Event;
use sdl2::keyboard::Keycode;

use imgui::*;
use sdl2::keyboard::Scancode;
use sdl2::mouse::{Cursor, MouseState, SystemCursor};
use sdl2::video::Window;

use pollster::block_on;
use wk::{Renderer, RendererConfig};

pub struct ImguiSdl2 {
    mouse_press: [bool; 5],
    ignore_mouse: bool,
    ignore_keyboard: bool,
    cursor: Option<MouseCursor>,
    sdl_cursor: Option<Cursor>,
}

struct Sdl2ClipboardBackend(sdl2::clipboard::ClipboardUtil);

impl imgui::ClipboardBackend for Sdl2ClipboardBackend {
    fn get(&mut self) -> Option<String> {
        if !self.0.has_clipboard_text() {
            return None;
        }

        self.0.clipboard_text().ok().map(String::from)
    }

    fn set(&mut self, value: &str) {
        let _ = self.0.set_clipboard_text(value);
    }
}

impl ImguiSdl2 {
    pub fn new(imgui: &mut Context, window: &Window) -> Self {
        let clipboard_util = window.subsystem().clipboard();
        imgui.set_clipboard_backend(Sdl2ClipboardBackend(clipboard_util));

        imgui.io_mut().key_map[Key::Tab as usize] = Scancode::Tab as u32;
        imgui.io_mut().key_map[Key::LeftArrow as usize] = Scancode::Left as u32;
        imgui.io_mut().key_map[Key::RightArrow as usize] = Scancode::Right as u32;
        imgui.io_mut().key_map[Key::UpArrow as usize] = Scancode::Up as u32;
        imgui.io_mut().key_map[Key::DownArrow as usize] = Scancode::Down as u32;
        imgui.io_mut().key_map[Key::PageUp as usize] = Scancode::PageUp as u32;
        imgui.io_mut().key_map[Key::PageDown as usize] = Scancode::PageDown as u32;
        imgui.io_mut().key_map[Key::Home as usize] = Scancode::Home as u32;
        imgui.io_mut().key_map[Key::End as usize] = Scancode::End as u32;
        imgui.io_mut().key_map[Key::Delete as usize] = Scancode::Delete as u32;
        imgui.io_mut().key_map[Key::Backspace as usize] = Scancode::Backspace as u32;
        imgui.io_mut().key_map[Key::Enter as usize] = Scancode::Return as u32;
        imgui.io_mut().key_map[Key::Escape as usize] = Scancode::Escape as u32;
        imgui.io_mut().key_map[Key::Space as usize] = Scancode::Space as u32;
        imgui.io_mut().key_map[Key::A as usize] = Scancode::A as u32;
        imgui.io_mut().key_map[Key::C as usize] = Scancode::C as u32;
        imgui.io_mut().key_map[Key::V as usize] = Scancode::V as u32;
        imgui.io_mut().key_map[Key::X as usize] = Scancode::X as u32;
        imgui.io_mut().key_map[Key::Y as usize] = Scancode::Y as u32;
        imgui.io_mut().key_map[Key::Z as usize] = Scancode::Z as u32;

        Self {
            mouse_press: [false; 5],
            ignore_keyboard: false,
            ignore_mouse: false,
            cursor: None,
            sdl_cursor: None,
        }
    }

    pub fn ignore_event(&self, event: &Event) -> bool {
        match *event {
            Event::KeyDown { .. }
            | Event::KeyUp { .. }
            | Event::TextEditing { .. }
            | Event::TextInput { .. } => self.ignore_keyboard,
            Event::MouseMotion { .. }
            | Event::MouseButtonDown { .. }
            | Event::MouseButtonUp { .. }
            | Event::MouseWheel { .. }
            | Event::FingerDown { .. }
            | Event::FingerUp { .. }
            | Event::FingerMotion { .. }
            | Event::DollarGesture { .. }
            | Event::DollarRecord { .. }
            | Event::MultiGesture { .. } => self.ignore_mouse,
            _ => false,
        }
    }

    pub fn handle_event(&mut self, imgui: &mut Context, event: &Event) {
        use sdl2::keyboard;
        use sdl2::mouse::MouseButton;

        fn set_mod(imgui: &mut Context, keymod: keyboard::Mod) {
            let ctrl = keymod.intersects(keyboard::Mod::RCTRLMOD | keyboard::Mod::LCTRLMOD);
            let alt = keymod.intersects(keyboard::Mod::RALTMOD | keyboard::Mod::LALTMOD);
            let shift = keymod.intersects(keyboard::Mod::RSHIFTMOD | keyboard::Mod::LSHIFTMOD);
            let super_ = keymod.intersects(keyboard::Mod::RGUIMOD | keyboard::Mod::LGUIMOD);

            imgui.io_mut().key_ctrl = ctrl;
            imgui.io_mut().key_alt = alt;
            imgui.io_mut().key_shift = shift;
            imgui.io_mut().key_super = super_;
        }

        match *event {
            Event::MouseWheel { y, .. } => {
                imgui.io_mut().mouse_wheel = y as f32;
            }
            Event::MouseButtonDown { mouse_btn, .. } => {
                if mouse_btn != MouseButton::Unknown {
                    let index = match mouse_btn {
                        MouseButton::Left => 0,
                        MouseButton::Right => 1,
                        MouseButton::Middle => 2,
                        MouseButton::X1 => 3,
                        MouseButton::X2 => 4,
                        MouseButton::Unknown => unreachable!(),
                    };
                    self.mouse_press[index] = true;
                }
            }
            Event::TextInput { ref text, .. } => {
                for chr in text.chars() {
                    imgui.io_mut().add_input_character(chr);
                }
            }
            Event::KeyDown {
                scancode, keymod, ..
            } => {
                set_mod(imgui, keymod);
                if let Some(scancode) = scancode {
                    imgui.io_mut().keys_down[scancode as usize] = true;
                }
            }
            Event::KeyUp {
                scancode, keymod, ..
            } => {
                set_mod(imgui, keymod);
                if let Some(scancode) = scancode {
                    imgui.io_mut().keys_down[scancode as usize] = false;
                }
            }
            _ => {}
        }
    }

    pub fn prepare_frame(&mut self, io: &mut imgui::Io, window: &Window, mouse_state: &MouseState) {
        let mouse_util = window.subsystem().sdl().mouse();

        let (win_w, win_h) = window.size();
        let (draw_w, draw_h) = window.drawable_size();

        let dpi_scaling = 1.0 / 2.0; // TODO: get dpi dynamically

        io.display_size = [win_w as f32, win_h as f32];
        io.display_framebuffer_scale = [
            (draw_w as f32) / (win_w as f32) * dpi_scaling,
            (draw_h as f32) / (win_h as f32) * dpi_scaling,
        ];

        // Merging the mousedown events we received into the current state prevents us from missing
        // clicks that happen faster than a frame
        io.mouse_down = [
            self.mouse_press[0] || mouse_state.left(),
            self.mouse_press[1] || mouse_state.right(),
            self.mouse_press[2] || mouse_state.middle(),
            self.mouse_press[3] || mouse_state.x1(),
            self.mouse_press[4] || mouse_state.x2(),
        ];
        self.mouse_press = [false; 5];

        let any_mouse_down = io.mouse_down.iter().any(|&b| b);
        mouse_util.capture(any_mouse_down);

        io.mouse_pos = [mouse_state.x() as f32, mouse_state.y() as f32];

        self.ignore_keyboard = io.want_capture_keyboard;
        self.ignore_mouse = io.want_capture_mouse;
    }

    pub fn prepare_render(&mut self, ui: &imgui::Ui, window: &Window) {
        let io = ui.io();
        if !io
            .config_flags
            .contains(ConfigFlags::NO_MOUSE_CURSOR_CHANGE)
        {
            let mouse_util = window.subsystem().sdl().mouse();

            match ui.mouse_cursor() {
                Some(mouse_cursor) if !io.mouse_draw_cursor => {
                    mouse_util.show_cursor(true);

                    let sdl_cursor = match mouse_cursor {
                        MouseCursor::Arrow => SystemCursor::Arrow,
                        MouseCursor::TextInput => SystemCursor::IBeam,
                        MouseCursor::ResizeAll => SystemCursor::SizeAll,
                        MouseCursor::ResizeNS => SystemCursor::SizeNS,
                        MouseCursor::ResizeEW => SystemCursor::SizeWE,
                        MouseCursor::ResizeNESW => SystemCursor::SizeNESW,
                        MouseCursor::ResizeNWSE => SystemCursor::SizeNWSE,
                        MouseCursor::Hand => SystemCursor::Hand,
                        MouseCursor::NotAllowed => SystemCursor::No,
                    };

                    if self.cursor != Some(mouse_cursor) {
                        let sdl_cursor = Cursor::from_system(sdl_cursor).unwrap();
                        sdl_cursor.set();
                        self.cursor = Some(mouse_cursor);
                        self.sdl_cursor = Some(sdl_cursor);
                    }
                }
                _ => {
                    self.cursor = None;
                    self.sdl_cursor = None;
                    mouse_util.show_cursor(false);
                }
            }
        }
    }
}

fn main() -> Result<(), String> {
    env_logger::init();

    let sdl_context = sdl2::init()?;
    let video = sdl_context.video()?;

    let window = video
        .window("Raw Window Handle Example", 800, 600)
        .position_centered()
        .resizable()
        .metal_view()
        .allow_highdpi()
        .build()
        .map_err(|e| e.to_string())?;

    let (width, height) = window.size();

    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::PRIMARY,
        ..Default::default()
    });

    let surface = unsafe { instance.create_surface(&window) }.unwrap();

    let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: Some(&surface),
        force_fallback_adapter: false,
    }))
    .unwrap();

    let (device, queue) =
        block_on(adapter.request_device(&wgpu::DeviceDescriptor::default(), None))
        .unwrap();

    // Set up swap chain
    let surface_desc = wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        format: wgpu::TextureFormat::Bgra8UnormSrgb,
        width: width,
        height: height,
        present_mode: wgpu::PresentMode::Fifo,
        alpha_mode: wgpu::CompositeAlphaMode::Auto,
        view_formats: vec![wgpu::TextureFormat::Bgra8Unorm],
    };

    surface.configure(&device, &surface_desc);

    // Set up dear imgui
    let mut imgui = imgui::Context::create();
    imgui.set_ini_filename(None);

    let hidpi_factor = 1.0;
    let font_size = (13.0 * hidpi_factor) as f32;
    imgui.io_mut().font_global_scale = (1.0 / hidpi_factor) as f32;

    imgui.fonts().add_font(&[FontSource::DefaultFontData {
        config: Some(imgui::FontConfig {
            oversample_h: 1,
            pixel_snap_h: true,
            size_pixels: font_size,
            ..Default::default()
        }),
    }]);

    //
    // Set up dear imgui wgpu renderer
    //
    let clear_color = wgpu::Color {
        r: 0.1,
        g: 0.2,
        b: 0.3,
        a: 1.0,
    };

    let renderer_config = RendererConfig {
        texture_format: surface_desc.format,
        ..Default::default()
    };

    let mut renderer = Renderer::new(&mut imgui, &device, &queue, renderer_config);

    let mut last_frame = Instant::now();

    //let mut last_cursor = None;

    let mut imgui_sdl2 = ImguiSdl2::new(&mut imgui, &window);
    let mut event_pump = sdl_context.event_pump().unwrap();

    // Game loop / rendering loop
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
            Ok(frame) => frame,
            Err(e) => {
                eprintln!("dropped frame: {e:?}");
                continue;
            }
        };

        let ui = imgui.frame();
        // Render example Imgui UI:
        {
            let window = ui.window("Hello world");
            window
                .size([300.0, 100.0], Condition::FirstUseEver)
                .build(|| {
                    ui.text("Hello world!");
                    ui.text("This...is...imgui-rs on WGPU!");
                    ui.separator();
                    let mouse_pos = ui.io().mouse_pos;
                    ui.text(format!(
                            "Mouse Position: ({:.1},{:.1})",
                            mouse_pos[0], mouse_pos[1]
                    ));
                });

            let window = ui.window("Hello too");
            window
                .size([400.0, 200.0], Condition::FirstUseEver)
                .position([400.0, 200.0], Condition::FirstUseEver)
                .build(|| {
                    ui.text(format!("Frametime: {delta_s:?}"));
                });


            ui.show_demo_window(&mut true);
        }

        imgui_sdl2.prepare_render(&ui, &window);

        let mut encoder = device.create_command_encoder(
            &wgpu::CommandEncoderDescriptor {
                label: Some("command_encoder"),
            }
        );

        {
            let view = frame
                .texture
                .create_view(&wgpu::TextureViewDescriptor::default());

            let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear_color),
                        store: true,
                    },
                })],
                depth_stencil_attachment: None,
            });

            renderer
                .render(imgui.render(), &queue, &device, &mut rpass)
                .expect("Rendering failed");
        }

        queue.submit([encoder.finish()]);
        frame.present();

        ::std::thread::sleep(::std::time::Duration::new(0, 1_000_000_000u32 / 60));
    }
}
