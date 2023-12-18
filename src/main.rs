extern crate sdl2;
extern crate imgui;
extern crate wk;
extern crate gl;
extern crate imgui_opengl_renderer;
extern crate pollster;
extern crate wgpu;
extern crate imgui_wgpu;

use imgui_wgpu::{Renderer, RendererConfig};

use std::time::Instant;

use std::borrow::Cow;
use wgpu::SurfaceError;

use sdl2::event::{Event, WindowEvent};
use sdl2::keyboard::Keycode;

fn main() -> Result<(), String> {
    // Show logs from wgpu
    env_logger::init();

    let sdl_context = sdl2::init()?;
    let video = sdl_context.video()?;

    let window = video
        .window("Raw Window Handle Example", 800, 600)
        .position_centered()
        .resizable()
        .metal_view()
        .build()
        .map_err(|e| e.to_string())?;
    let (width, height) = window.size();

    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::PRIMARY,
        dx12_shader_compiler: Default::default(),
    });
    let surface = unsafe {
        match instance.create_surface(&window) {
            Ok(s) => s,
            Err(e) => return Err(e.to_string()),
        }
    };
    let adapter_opt = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        force_fallback_adapter: false,
        compatible_surface: Some(&surface),
    }));
    let adapter = match adapter_opt {
        Some(a) => a,
        None => return Err(String::from("No adapter found")),
    };

    let (device, queue) = match pollster::block_on(adapter.request_device(
        &wgpu::DeviceDescriptor {
            limits: wgpu::Limits::default(),
            label: Some("device"),
            features: wgpu::Features::empty(),
        },
        None,
    )) {
        Ok(a) => a,
        Err(e) => return Err(e.to_string()),
    };

    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("shader"),
        source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(include_str!("shader.wgsl"))),
    });

    let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        entries: &[],
        label: Some("bind_group_layout"),
    });
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        layout: &bind_group_layout,
        entries: &[],
        label: Some("bind_group"),
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        bind_group_layouts: &[&bind_group_layout],
        label: None,
        push_constant_ranges: &[],
    });

    let render_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            buffers: &[],
            module: &shader,
            entry_point: "vs_main",
        },
        fragment: Some(wgpu::FragmentState {
            targets: &[Some(wgpu::ColorTargetState {
                format: wgpu::TextureFormat::Bgra8UnormSrgb,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
            module: &shader,
            entry_point: "fs_main",
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            strip_index_format: None,
            front_face: wgpu::FrontFace::Ccw,
            cull_mode: Some(wgpu::Face::Front),
            unclipped_depth: false,
            polygon_mode: wgpu::PolygonMode::Fill,
            conservative: false,
        },
        depth_stencil: None,
        label: None,
        multisample: wgpu::MultisampleState {
            count: 1,
            mask: !0,
            alpha_to_coverage_enabled: false,
        },
        multiview: None,
    });

    let surface_caps = surface.get_capabilities(&adapter);

    let surface_format = surface_caps
        .formats
        .iter()
        .copied()
        .find(|f| f.is_srgb())
        .unwrap_or(surface_caps.formats[0]);

    let mut config = wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        format: surface_format,
        width,
        height,
        present_mode: wgpu::PresentMode::Fifo,
        alpha_mode: wgpu::CompositeAlphaMode::Auto,
        view_formats: Vec::default(),
    };
    surface.configure(&device, &config);

    let mut event_pump = sdl_context.event_pump()?;
    'running: loop {
        for event in event_pump.poll_iter() {
            match event {
                Event::Window {
                    window_id,
                    win_event: WindowEvent::SizeChanged(width, height),
                    ..
                } if window_id == window.id() => {
                    config.width = width as u32;
                    config.height = height as u32;
                    surface.configure(&device, &config);
                }
                Event::Quit { .. }
                | Event::KeyDown {
                    keycode: Some(Keycode::Escape),
                    ..
                } => {
                    break 'running;
                }
                e => {
                    dbg!(e);
                }
            }
        }

        let frame = match surface.get_current_texture() {
            Ok(frame) => frame,
            Err(err) => {
                let reason = match err {
                    SurfaceError::Timeout => "Timeout",
                    SurfaceError::Outdated => "Outdated",
                    SurfaceError::Lost => "Lost",
                    SurfaceError::OutOfMemory => "OutOfMemory",
                };
                panic!("Failed to get current surface texture! Reason: {}", reason)
            }
        };

        let output = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("command_encoder"),
        });

        {
            let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &output,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::GREEN),
                        store: true,
                    },
                })],
                depth_stencil_attachment: None,
                label: None,
            });
            rpass.set_pipeline(&render_pipeline);
            rpass.set_bind_group(0, &bind_group, &[]);
            rpass.draw(0..3, 0..1);
        }
        queue.submit([encoder.finish()]);
        frame.present();
    }

    Ok(())
/*
  {
    let gl_attr = video.gl_attr();
    gl_attr.set_context_profile(sdl2::video::GLProfile::Core);
    gl_attr.set_context_version(3, 0);
  }

  let window = video.window("rust-imgui-sdl2 demo", 1000, 1000)
    .position_centered()
    .resizable()
    .opengl()
    .allow_highdpi()
    .build()
    .unwrap();

  let _gl_context = window.gl_create_context().expect("Couldn't create GL context");
  gl::load_with(|s| video.gl_get_proc_address(s) as _);

  let mut imgui = imgui::Context::create();
  imgui.set_ini_filename(None);


  let mut imgui_sdl2 = wk::ImguiSdl2::new(&mut imgui, &window);

  let renderer = imgui_opengl_renderer::Renderer::new(&mut imgui, |s| video.gl_get_proc_address(s) as _);

  let mut event_pump = sdl_context.event_pump().unwrap();

  let mut last_frame = Instant::now();


  'running: loop {
    use sdl2::event::Event;
    use sdl2::keyboard::Keycode;

    for event in event_pump.poll_iter() {
      imgui_sdl2.handle_event(&mut imgui, &event);
      if imgui_sdl2.ignore_event(&event) { continue; }

      match event {
        Event::Quit {..} | Event::KeyDown { keycode: Some(Keycode::Escape), .. } => {
          break 'running Ok(())
        },
        _ => {}
      }
    }


    imgui_sdl2.prepare_frame(imgui.io_mut(), &window, &event_pump.mouse_state());

    let now = Instant::now();
    let delta = now - last_frame;
    let delta_s = delta.as_secs() as f32 + delta.subsec_nanos() as f32 / 1_000_000_000.0;
    last_frame = now;
    imgui.io_mut().delta_time = delta_s;

    let ui = imgui.frame();
    ui.show_demo_window(&mut true);

    unsafe {
      gl::ClearColor(0.2, 0.2, 0.2, 1.0);
      gl::Clear(gl::COLOR_BUFFER_BIT);
    }

    imgui_sdl2.prepare_render(&ui, &window);
    renderer.render(&mut imgui);

    window.gl_swap_window();

    ::std::thread::sleep(::std::time::Duration::new(0, 1_000_000_000u32 / 60));
  }
	// */
}
