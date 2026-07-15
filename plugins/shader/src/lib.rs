#[allow(warnings)]
mod bindings;

use bindings::wasi::frame_buffer::frame_buffer::{Buffer as FbBuffer, Device as FbDevice};
use bindings::wasi::graphics_context::graphics_context::Context as GraphicsContext;
use bindings::wasi::surface::surface::{CreateDesc, Surface};
use bindings::wasi::webgpu::webgpu::{
    get_gpu, Gpu, GpuBindGroup, GpuBindGroupDescriptor, GpuBindGroupEntry, GpuBindingResource,
    GpuBufferBinding, GpuBufferDescriptor, GpuColor, GpuColorTargetState, GpuDevice, GpuExtent3D,
    GpuErrorFilter, GpuFragmentState, GpuLayoutMode, GpuLoadOp, GpuPrimitiveState,
    GpuPrimitiveTopology, GpuRenderPassColorAttachment, GpuRenderPassDescriptor, GpuRenderPipeline,
    GpuRenderPipelineDescriptor, GpuShaderModuleDescriptor, GpuStoreOp, GpuTexelCopyBufferInfo,
    GpuTexelCopyTextureInfo, GpuTextureDescriptor, GpuTextureFormat, GpuVertexState,
};
use bindings::wk::midi::midi::Input as MidiInput;
use bindings::Guest;

// WebGPU bit flags (the WIT models these as plain u32).
const TEX_COPY_SRC: u32 = 0x01;
const TEX_RENDER_ATTACHMENT: u32 = 0x10;
const BUF_MAP_READ: u32 = 0x0001;
const BUF_COPY_DST: u32 = 0x0008;
const BUF_UNIFORM: u32 = 0x0040;
const MAP_MODE_READ: u32 = 0x0001;

/// Uniform block size: time + frame + vec2 res (16 bytes) + 128 MIDI floats
/// (32 * vec4 = 512 bytes). Must match the `Uni` struct in `PRELUDE`.
const UNIFORM_SIZE: u64 = 16 + 512;

/// Prepended to every user shader: the uniforms, the `cc()` MIDI helper, and a
/// fullscreen-triangle vertex shader. The user writes `main_image`.
const PRELUDE: &str = r#"
struct Uni {
    time: f32,
    frame: f32,
    res: vec2<f32>,
    midi: array<vec4<f32>, 32>,
};
@group(0) @binding(0) var<uniform> u: Uni;

// MIDI value n (a CC controller number or a note number), in 0..1.
fn cc(n: u32) -> f32 { return u.midi[n / 4u][n % 4u]; }

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> @builtin(position) vec4<f32> {
    var p = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -3.0),
        vec2<f32>(-1.0, 1.0),
        vec2<f32>(3.0, 1.0),
    );
    return vec4<f32>(p[vi], 0.0, 1.0);
}
"#;

/// Appended after the user shader: calls their `main_image(uv)` per pixel. `uv`
/// runs 0..1 across the surface (y down).
const EPILOGUE: &str = r#"
@fragment
fn fs_main(@builtin(position) frag: vec4<f32>) -> @location(0) vec4<f32> {
    let uv = frag.xy / u.res;
    return vec4<f32>(main_image(uv), 1.0);
}
"#;

/// The starter shader: rendered when nothing is connected, and written into a
/// connected-but-empty file so there's example code to edit. Documents the
/// live-coding contract (write `main_image`; use `u.time`, `u.res`, `cc(n)`).
const DEFAULT_USER: &str = r#"// Live shader — edit me; the viewer hot-reloads on every save.
//   main_image(uv) returns an RGB colour; uv is 0..1 across the surface.
//   u.time  seconds since start      u.res  surface size in pixels
//   cc(n)   MIDI CC/note n, 0..1 (wire a MIDI source into this node)
fn main_image(uv: vec2<f32>) -> vec3<f32> {
    let p = uv * 2.0 - 1.0;
    let d = length(p);
    let a = atan2(p.y, p.x);
    let rings = 0.5 + 0.5 * sin(d * 12.0 - u.time * 2.0 + cc(1u) * 6.28);
    let hue = 0.5 + 0.5 * cos(a + u.time + vec3<f32>(0.0, 2.09, 4.19));
    return hue * rings;
}
"#;

/// The first regular file mounted into us (a `.wgsl` file wins), or `None` when
/// nothing is connected yet.
fn shader_path() -> Option<String> {
    let mut first = None;
    for entry in std::fs::read_dir("/").ok()?.flatten() {
        if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".wgsl") {
                return Some(format!("/{name}"));
            }
            first.get_or_insert(format!("/{name}"));
        }
    }
    first
}

/// Assemble the full WGSL and build a render pipeline, or return the compile
/// error message. Wraps creation in a validation error scope: the host captures
/// a bad shader as an error (it never traps), so we can detect it, discard the
/// broken pipeline, and keep the last good one — live-coding survives typos.
fn build_pipeline(device: &GpuDevice, user_src: &str) -> Result<GpuRenderPipeline, String> {
    let code = format!("{PRELUDE}\n{user_src}\n{EPILOGUE}");
    device.push_error_scope(GpuErrorFilter::Validation);
    let module = device.create_shader_module(&GpuShaderModuleDescriptor {
        code,
        compilation_hints: None,
        label: None,
    });
    let pipeline = device.create_render_pipeline(GpuRenderPipelineDescriptor {
        vertex: GpuVertexState {
            buffers: None,
            module: &module,
            entry_point: Some("vs_main".to_string()),
            constants: None,
        },
        primitive: Some(GpuPrimitiveState {
            topology: Some(GpuPrimitiveTopology::TriangleList),
            strip_index_format: None,
            front_face: None,
            cull_mode: None,
            unclipped_depth: None,
        }),
        depth_stencil: None,
        multisample: None,
        fragment: Some(GpuFragmentState {
            targets: vec![Some(GpuColorTargetState {
                format: GpuTextureFormat::Rgba8unorm,
                blend: None,
                write_mask: None,
            })],
            module: &module,
            entry_point: Some("fs_main".to_string()),
            constants: None,
        }),
        layout: GpuLayoutMode::Auto,
        label: None,
    });
    match device.pop_error_scope() {
        Ok(Some(err)) => Err(err.message()),
        _ => Ok(pipeline),
    }
}

/// A pipeline plus the bind group wiring its uniform buffer (recreated together,
/// since `Auto` layout gives each pipeline its own bind-group layout).
struct Program {
    pipeline: GpuRenderPipeline,
    bind_group: GpuBindGroup,
}

impl Program {
    fn new(device: &GpuDevice, uniforms: &bindings::wasi::webgpu::webgpu::GpuBuffer, user_src: &str) -> Result<Self, String> {
        let pipeline = build_pipeline(device, user_src)?;
        let layout = pipeline.get_bind_group_layout(0);
        let bind_group = device.create_bind_group(&GpuBindGroupDescriptor {
            layout: &layout,
            entries: vec![GpuBindGroupEntry {
                binding: 0,
                resource: GpuBindingResource::GpuBufferBinding(GpuBufferBinding {
                    buffer: uniforms,
                    offset: None,
                    size: None,
                }),
            }],
            label: None,
        });
        Ok(Program {
            pipeline,
            bind_group,
        })
    }
}

/// Pack the uniform block: time, frame, resolution, then 128 MIDI floats.
fn uniform_bytes(time: f32, frame: f32, res: (u32, u32), midi: &[f32; 128]) -> Vec<u8> {
    let mut b = Vec::with_capacity(UNIFORM_SIZE as usize);
    b.extend_from_slice(&time.to_le_bytes());
    b.extend_from_slice(&frame.to_le_bytes());
    b.extend_from_slice(&(res.0 as f32).to_le_bytes());
    b.extend_from_slice(&(res.1 as f32).to_le_bytes());
    for v in midi.iter() {
        b.extend_from_slice(&v.to_le_bytes());
    }
    b
}

/// Fold a raw MIDI message into `midi[]`: CC and note velocities land at their
/// controller/note number (0..1); note-off clears the slot.
fn apply_midi(midi: &mut [f32; 128], msg: &[u8]) {
    if msg.len() < 3 {
        return;
    }
    let (status, n, val) = (msg[0] & 0xF0, msg[1] as usize & 0x7f, msg[2] as f32 / 127.0);
    match status {
        0xB0 => midi[n] = val,                              // control change
        0x90 => midi[n] = val,                              // note-on (vel 0 = off)
        0x80 => midi[n] = 0.0,                              // note-off
        _ => {}
    }
}

struct Component;

impl Guest for Component {
    fn run() {
        let surface = Surface::new(CreateDesc {
            width: Some(512),
            height: Some(512),
        });
        let ctx = GraphicsContext::new();
        surface.connect_graphics_context(&ctx);
        let fb_device = FbDevice::new();
        fb_device.connect_graphics_context(&ctx);
        let frame = surface.subscribe_frame();

        let gpu: Gpu = get_gpu();
        let adapter = gpu.request_adapter(None).expect("no webgpu adapter");
        let device = adapter.request_device(None).expect("no webgpu device");
        let queue = device.queue();

        let midi_in = MidiInput::new();
        let mut midi = [0.0f32; 128];

        // One persistent uniform buffer, rewritten every frame.
        let uniforms = device.create_buffer(&GpuBufferDescriptor {
            size: UNIFORM_SIZE,
            usage: BUF_UNIFORM | BUF_COPY_DST,
            mapped_at_creation: None,
            label: None,
        });

        // Compile the initial program (connected file, else the default). If a
        // file is connected but empty, seed it with the starter shader so it's
        // ready to edit in a wired-up editor.
        let mut current_src;
        let mut program = {
            let path = shader_path();
            let existing = path
                .as_ref()
                .and_then(|p| std::fs::read_to_string(p).ok())
                .unwrap_or_default();
            let src = if existing.trim().is_empty() {
                if let Some(p) = &path {
                    let _ = std::fs::write(p, DEFAULT_USER);
                }
                DEFAULT_USER.to_string()
            } else {
                existing
            };
            current_src = src.clone();
            Program::new(&device, &uniforms, &src)
                .or_else(|e| {
                    println!("[shader] initial compile failed: {e}\n[shader] using the default");
                    Program::new(&device, &uniforms, DEFAULT_USER)
                })
                .expect("default shader compiles")
        };

        let start = std::time::Instant::now();
        let mut fnum: u32 = 0;
        loop {
            frame.block();
            let _ = surface.get_frame();

            // Drain MIDI into the uniform state.
            while let Some(msg) = midi_in.receive() {
                apply_midi(&mut midi, &msg);
            }

            // Hot-reload: recompile when the file's content changed. A failed
            // compile keeps the previous good program.
            if let Some(src) = shader_path().and_then(|p| std::fs::read_to_string(p).ok()) {
                if !src.trim().is_empty() && src != current_src {
                    match Program::new(&device, &uniforms, &src) {
                        Ok(p) => {
                            program = p;
                            current_src = src;
                            println!("[shader] reloaded");
                        }
                        Err(e) => println!("[shader] compile error (keeping last good):\n{e}"),
                    }
                }
            }

            let w = surface.width().max(1);
            let h = surface.height().max(1);
            let unpadded_bpr = w * 4;
            let padded_bpr = unpadded_bpr.div_ceil(256) * 256;

            let time = start.elapsed().as_secs_f32();
            let bytes = uniform_bytes(time, fnum as f32, (w, h), &midi);
            queue
                .write_buffer_with_copy(&uniforms, 0, &bytes, None, None)
                .expect("write uniforms");

            let texture = device.create_texture(&GpuTextureDescriptor {
                size: GpuExtent3D {
                    width: w,
                    height: Some(h),
                    depth_or_array_layers: Some(1),
                },
                mip_level_count: None,
                sample_count: None,
                dimension: None,
                format: GpuTextureFormat::Rgba8unorm,
                usage: TEX_RENDER_ATTACHMENT | TEX_COPY_SRC,
                view_formats: None,
                label: None,
            });
            let view = texture.create_view(None);

            let readback = device.create_buffer(&GpuBufferDescriptor {
                size: (padded_bpr * h) as u64,
                usage: BUF_MAP_READ | BUF_COPY_DST,
                mapped_at_creation: None,
                label: None,
            });

            let encoder = device.create_command_encoder(None);
            {
                let pass = encoder.begin_render_pass(&GpuRenderPassDescriptor {
                    color_attachments: vec![Some(GpuRenderPassColorAttachment {
                        view: &view,
                        depth_slice: None,
                        resolve_target: None,
                        clear_value: Some(GpuColor {
                            r: 0.0,
                            g: 0.0,
                            b: 0.0,
                            a: 1.0,
                        }),
                        load_op: GpuLoadOp::Clear,
                        store_op: GpuStoreOp::Store,
                    })],
                    depth_stencil_attachment: None,
                    occlusion_query_set: None,
                    timestamp_writes: None,
                    max_draw_count: None,
                    label: None,
                });
                pass.set_pipeline(&program.pipeline);
                let _ = pass.set_bind_group(0, Some(&program.bind_group), None, None, None);
                pass.draw(3, None, None, None);
                pass.end();
            }
            encoder.copy_texture_to_buffer(
                &GpuTexelCopyTextureInfo {
                    texture: &texture,
                    mip_level: None,
                    origin: None,
                    aspect: None,
                },
                &GpuTexelCopyBufferInfo {
                    buffer: &readback,
                    offset: None,
                    bytes_per_row: Some(padded_bpr),
                    rows_per_image: Some(h),
                },
                GpuExtent3D {
                    width: w,
                    height: Some(h),
                    depth_or_array_layers: Some(1),
                },
            );
            let cmd = encoder.finish(None);
            queue.submit(&[&cmd]);

            readback
                .map_async(MAP_MODE_READ, None, None)
                .expect("map_async");
            let padded = readback
                .get_mapped_range_get_with_copy(None, None)
                .expect("get_mapped_range");
            let _ = readback.unmap();

            let mut pixels = vec![0u8; (w * h * 4) as usize];
            for row in 0..h as usize {
                let src = row * padded_bpr as usize;
                let dst = row * unpadded_bpr as usize;
                pixels[dst..dst + unpadded_bpr as usize]
                    .copy_from_slice(&padded[src..src + unpadded_bpr as usize]);
            }

            let fb = FbBuffer::from_graphics_buffer(ctx.get_current_buffer());
            fb.set(&pixels);
            ctx.present();

            fnum = fnum.wrapping_add(1);
        }
    }
}

bindings::export!(Component with_types_in bindings);
