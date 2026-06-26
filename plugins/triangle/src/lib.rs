#[allow(warnings)]
mod bindings;

use bindings::wasi::frame_buffer::frame_buffer::{Buffer as FbBuffer, Device as FbDevice};
use bindings::wasi::graphics_context::graphics_context::Context as GraphicsContext;
use bindings::wasi::surface::surface::{CreateDesc, Surface};
use bindings::wasi::webgpu::webgpu::{
    get_gpu, GpuBufferDescriptor, GpuColor, GpuColorTargetState, GpuExtent3D, GpuFragmentState,
    GpuLayoutMode, GpuLoadOp, GpuPrimitiveState, GpuPrimitiveTopology,
    GpuRenderPassColorAttachment, GpuRenderPassDescriptor, GpuRenderPipelineDescriptor,
    GpuShaderModuleDescriptor, GpuStoreOp, GpuTexelCopyBufferInfo, GpuTexelCopyTextureInfo,
    GpuTextureDescriptor, GpuTextureFormat, GpuVertexState,
};
use bindings::Guest;

const W: u32 = 256;
const H: u32 = 256;

// WebGPU bit flags (the WIT models these as plain u32).
const TEX_COPY_SRC: u32 = 0x01;
const TEX_RENDER_ATTACHMENT: u32 = 0x10;
const BUF_MAP_READ: u32 = 0x0001;
const BUF_COPY_DST: u32 = 0x0008;
const MAP_MODE_READ: u32 = 0x0001;

/// A self-contained triangle: positions come from the vertex index, so no vertex
/// buffer is needed.
const SHADER: &str = r#"
@vertex
fn vs_main(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    var p = array<vec2<f32>, 3>(
        vec2<f32>(0.0, 0.6),
        vec2<f32>(-0.6, -0.6),
        vec2<f32>(0.6, -0.6),
    );
    return vec4<f32>(p[i], 0.0, 1.0);
}

@fragment
fn fs_main() -> @location(0) vec4<f32> {
    return vec4<f32>(1.0, 0.8, 0.1, 1.0);
}
"#;

struct Component;

impl Guest for Component {
    /// A GPU client: clears an offscreen texture on the GPU each frame, reads the
    /// pixels back, and delivers them through wasi:frame-buffer for the host to
    /// composite. (No winit canvas/swapchain — that path is a later step.)
    fn run() {
        // wasi-gfx surface + frame-buffer for delivery.
        let surface = Surface::new(CreateDesc {
            width: Some(W),
            height: Some(H),
        });
        let ctx = GraphicsContext::new();
        surface.connect_graphics_context(&ctx);
        let fb_device = FbDevice::new();
        fb_device.connect_graphics_context(&ctx);
        let frame = surface.subscribe_frame();

        // wasi:webgpu device.
        let gpu = get_gpu();
        let adapter = gpu.request_adapter(None).expect("no webgpu adapter");
        let device = adapter.request_device(None).expect("no webgpu device");
        let queue = device.queue();

        // Build the render pipeline once.
        let shader = device.create_shader_module(&GpuShaderModuleDescriptor {
            code: SHADER.to_string(),
            compilation_hints: None,
            label: None,
        });
        let pipeline = device.create_render_pipeline(GpuRenderPipelineDescriptor {
            vertex: GpuVertexState {
                buffers: None,
                module: &shader,
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
                module: &shader,
                entry_point: Some("fs_main".to_string()),
                constants: None,
            }),
            layout: GpuLayoutMode::Auto,
            label: None,
        });

        // Readback rows must be aligned to 256 bytes.
        let unpadded_bpr = W * 4;
        let padded_bpr = unpadded_bpr.div_ceil(256) * 256;
        let buf_size = (padded_bpr * H) as u64;

        let mut t: u32 = 0;
        loop {
            frame.block();
            let _ = surface.get_frame();

            let texture = device.create_texture(&GpuTextureDescriptor {
                size: GpuExtent3D {
                    width: W,
                    height: Some(H),
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

            let buffer = device.create_buffer(&GpuBufferDescriptor {
                size: buf_size,
                usage: BUF_MAP_READ | BUF_COPY_DST,
                mapped_at_creation: None,
                label: None,
            });

            let encoder = device.create_command_encoder(None);
            {
                // Animated dark background so the triangle stands out.
                let b = 0.1 + 0.1 * (((t % 256) as f64) / 255.0);
                let pass = encoder.begin_render_pass(&GpuRenderPassDescriptor {
                    color_attachments: vec![Some(GpuRenderPassColorAttachment {
                        view: &view,
                        depth_slice: None,
                        resolve_target: None,
                        clear_value: Some(GpuColor {
                            r: 0.05,
                            g: 0.05,
                            b,
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
                pass.set_pipeline(&pipeline);
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
                    buffer: &buffer,
                    offset: None,
                    bytes_per_row: Some(padded_bpr),
                    rows_per_image: Some(H),
                },
                GpuExtent3D {
                    width: W,
                    height: Some(H),
                    depth_or_array_layers: Some(1),
                },
            );
            let cmd = encoder.finish(None);
            queue.submit(&[&cmd]);

            // Read the rendered pixels back to the CPU.
            buffer
                .map_async(MAP_MODE_READ, None, None)
                .expect("map_async");
            let padded = buffer
                .get_mapped_range_get_with_copy(None, None)
                .expect("get_mapped_range");
            let _ = buffer.unmap();

            // Strip row padding to a tight RGBA buffer.
            let mut pixels = vec![0u8; (W * H * 4) as usize];
            for row in 0..H as usize {
                let src = row * padded_bpr as usize;
                let dst = row * unpadded_bpr as usize;
                pixels[dst..dst + unpadded_bpr as usize]
                    .copy_from_slice(&padded[src..src + unpadded_bpr as usize]);
            }

            // Deliver to the host for compositing.
            let fb = FbBuffer::from_graphics_buffer(ctx.get_current_buffer());
            fb.set(&pixels);
            ctx.present();

            t = t.wrapping_add(2);
        }
    }
}

bindings::export!(Component with_types_in bindings);
