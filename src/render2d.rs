//! A small 2D quad renderer over wgpu: the host draws everything (plugin
//! surfaces, window chrome, text) as textured, colored, alpha-blended quads.
//! Replaces the dear-imgui renderer; the shader (pos/uv/color) is the same.

use std::collections::HashMap;
use std::mem::size_of;

use bytemuck::{Pod, Zeroable};
use wgpu::util::{BufferInitDescriptor, DeviceExt};
use wgpu::*;

/// Handle to a texture owned by the renderer.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct TextureId(pub u32);

/// One quad to draw: four corners (tl, tr, br, bl), a uv rect (x0,y0,x1,y1), an
/// RGBA color the texture is multiplied by, the texture, and a scissor clip.
#[derive(Clone, Copy)]
pub struct Quad {
    corners: [[f32; 2]; 4],
    uv: [f32; 4],
    color: [f32; 4],
    tex: TextureId,
    clip: [f32; 4],
}

fn rect_corners(r: [f32; 4]) -> [[f32; 2]; 4] {
    [[r[0], r[1]], [r[2], r[1]], [r[2], r[3]], [r[0], r[3]]]
}

impl Quad {
    /// A textured rect.
    pub fn tex(
        rect: [f32; 4],
        uv: [f32; 4],
        color: [f32; 4],
        tex: TextureId,
        clip: [f32; 4],
    ) -> Self {
        Quad {
            corners: rect_corners(rect),
            uv,
            color,
            tex,
            clip,
        }
    }

    /// A solid-colored rect (uses the renderer's white texture).
    pub fn solid(white: TextureId, rect: [f32; 4], color: [f32; 4], clip: [f32; 4]) -> Self {
        Quad::tex(rect, [0.0, 0.0, 1.0, 1.0], color, white, clip)
    }

    /// A solid line segment of the given thickness, in screen pixels.
    pub fn line(
        white: TextureId,
        a: [f32; 2],
        b: [f32; 2],
        thickness: f32,
        color: [f32; 4],
        clip: [f32; 4],
    ) -> Self {
        let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
        let len = (dx * dx + dy * dy).sqrt().max(0.001);
        // Perpendicular offset, half-thickness on each side.
        let (nx, ny) = (-dy / len * thickness * 0.5, dx / len * thickness * 0.5);
        Quad {
            corners: [
                [a[0] + nx, a[1] + ny],
                [b[0] + nx, b[1] + ny],
                [b[0] - nx, b[1] - ny],
                [a[0] - nx, a[1] - ny],
            ],
            uv: [0.0, 0.0, 1.0, 1.0],
            color,
            tex: white,
            clip,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Vertex {
    pos: [f32; 2],
    uv: [f32; 2],
    color: [u8; 4],
}

struct Texture {
    texture: wgpu::Texture,
    bind_group: BindGroup,
}

pub struct Renderer {
    pipeline: RenderPipeline,
    uniform_buffer: Buffer,
    uniform_bind_group: BindGroup,
    texture_layout: BindGroupLayout,
    textures: HashMap<u32, Texture>,
    next_id: u32,
    vertex_buffer: Option<Buffer>,
    vertex_capacity: usize,
    /// 1x1 white texture for solid fills.
    pub white: TextureId,
}

impl Renderer {
    pub fn new(device: &Device, queue: &Queue, format: TextureFormat) -> Self {
        let shader = device.create_shader_module(include_wgsl!("render2d.wgsl"));

        let uniform_buffer = device.create_buffer(&BufferDescriptor {
            label: Some("render2d uniforms"),
            size: 64,
            usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let uniform_layout = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: None,
            entries: &[BindGroupLayoutEntry {
                binding: 0,
                visibility: ShaderStages::VERTEX,
                ty: BindingType::Buffer {
                    ty: BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let uniform_bind_group = device.create_bind_group(&BindGroupDescriptor {
            label: None,
            layout: &uniform_layout,
            entries: &[BindGroupEntry {
                binding: 0,
                resource: uniform_buffer.as_entire_binding(),
            }],
        });

        let texture_layout = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("render2d texture layout"),
            entries: &[
                BindGroupLayoutEntry {
                    binding: 0,
                    visibility: ShaderStages::FRAGMENT,
                    ty: BindingType::Texture {
                        multisampled: false,
                        sample_type: TextureSampleType::Float { filterable: true },
                        view_dimension: TextureViewDimension::D2,
                    },
                    count: None,
                },
                BindGroupLayoutEntry {
                    binding: 1,
                    visibility: ShaderStages::FRAGMENT,
                    ty: BindingType::Sampler(SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[Some(&uniform_layout), Some(&texture_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&RenderPipelineDescriptor {
            label: Some("render2d pipeline"),
            layout: Some(&pipeline_layout),
            vertex: VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[VertexBufferLayout {
                    array_stride: size_of::<Vertex>() as BufferAddress,
                    step_mode: VertexStepMode::Vertex,
                    attributes: &vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Unorm8x4],
                }],
            },
            primitive: PrimitiveState {
                topology: PrimitiveTopology::TriangleList,
                front_face: FrontFace::Cw,
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: MultisampleState::default(),
            fragment: Some(FragmentState {
                module: &shader,
                entry_point: Some("fs_main_linear"),
                compilation_options: Default::default(),
                targets: &[Some(ColorTargetState {
                    format,
                    blend: Some(BlendState::ALPHA_BLENDING),
                    write_mask: ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });

        let mut renderer = Self {
            pipeline,
            uniform_buffer,
            uniform_bind_group,
            texture_layout,
            textures: HashMap::new(),
            next_id: 0,
            vertex_buffer: None,
            vertex_capacity: 0,
            white: TextureId(0),
        };
        renderer.white = renderer.create_texture(device, queue, 1, 1, &[255, 255, 255, 255]);
        renderer
    }

    /// Create an RGBA8 texture from tightly-packed pixels.
    pub fn create_texture(
        &mut self,
        device: &Device,
        queue: &Queue,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> TextureId {
        let texture = device.create_texture(&TextureDescriptor {
            label: Some("render2d texture"),
            size: Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: TextureDimension::D2,
            format: TextureFormat::Rgba8Unorm,
            usage: TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST,
            view_formats: &[TextureFormat::Rgba8Unorm],
        });
        queue.write_texture(
            TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: Origin3d::ZERO,
                aspect: TextureAspect::All,
            },
            rgba,
            TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width * 4),
                rows_per_image: Some(height),
            },
            Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        let view = texture.create_view(&TextureViewDescriptor::default());
        let sampler = device.create_sampler(&SamplerDescriptor {
            mag_filter: FilterMode::Linear,
            min_filter: FilterMode::Linear,
            ..Default::default()
        });
        let bind_group = device.create_bind_group(&BindGroupDescriptor {
            label: None,
            layout: &self.texture_layout,
            entries: &[
                BindGroupEntry {
                    binding: 0,
                    resource: BindingResource::TextureView(&view),
                },
                BindGroupEntry {
                    binding: 1,
                    resource: BindingResource::Sampler(&sampler),
                },
            ],
        });
        let id = self.next_id;
        self.next_id += 1;
        self.textures.insert(
            id,
            Texture {
                texture,
                bind_group,
            },
        );
        TextureId(id)
    }

    /// Overwrite an existing texture's pixels (same dimensions).
    pub fn update_texture(
        &self,
        queue: &Queue,
        id: TextureId,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) {
        if let Some(t) = self.textures.get(&id.0) {
            queue.write_texture(
                TexelCopyTextureInfo {
                    texture: &t.texture,
                    mip_level: 0,
                    origin: Origin3d::ZERO,
                    aspect: TextureAspect::All,
                },
                rgba,
                TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(width * 4),
                    rows_per_image: Some(height),
                },
                Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
            );
        }
    }

    pub fn remove_texture(&mut self, id: TextureId) {
        self.textures.remove(&id.0);
    }

    /// Draw `quads` in order onto the current render pass. `fb` is the
    /// framebuffer size in the same pixel space as the quad coordinates.
    pub fn draw<'r>(
        &'r mut self,
        device: &Device,
        queue: &Queue,
        rpass: &mut RenderPass<'r>,
        fb: [f32; 2],
        quads: &[Quad],
    ) {
        if quads.is_empty() {
            return;
        }
        // Orthographic projection mapping [0,fb] -> clip space.
        let matrix: [[f32; 4]; 4] = [
            [2.0 / fb[0], 0.0, 0.0, 0.0],
            [0.0, -2.0 / fb[1], 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [-1.0, 1.0, 0.0, 1.0],
        ];
        queue.write_buffer(&self.uniform_buffer, 0, bytemuck::bytes_of(&matrix));

        let mut verts: Vec<Vertex> = Vec::with_capacity(quads.len() * 6);
        for q in quads {
            let [u0, v0, u1, v1] = q.uv;
            let c = [
                (q.color[0] * 255.0) as u8,
                (q.color[1] * 255.0) as u8,
                (q.color[2] * 255.0) as u8,
                (q.color[3] * 255.0) as u8,
            ];
            let tl = Vertex {
                pos: q.corners[0],
                uv: [u0, v0],
                color: c,
            };
            let tr = Vertex {
                pos: q.corners[1],
                uv: [u1, v0],
                color: c,
            };
            let br = Vertex {
                pos: q.corners[2],
                uv: [u1, v1],
                color: c,
            };
            let bl = Vertex {
                pos: q.corners[3],
                uv: [u0, v1],
                color: c,
            };
            verts.extend_from_slice(&[tl, tr, br, tl, br, bl]);
        }

        let bytes: &[u8] = bytemuck::cast_slice(&verts);
        let need_new = match &self.vertex_buffer {
            Some(_) => self.vertex_capacity < bytes.len(),
            None => true,
        };
        if need_new {
            self.vertex_buffer = Some(device.create_buffer_init(&BufferInitDescriptor {
                label: Some("render2d vertices"),
                contents: bytes,
                usage: BufferUsages::VERTEX | BufferUsages::COPY_DST,
            }));
            self.vertex_capacity = bytes.len();
        } else if let Some(buf) = &self.vertex_buffer {
            queue.write_buffer(buf, 0, bytes);
        }
        let Some(vbuf) = &self.vertex_buffer else {
            return;
        };

        rpass.set_pipeline(&self.pipeline);
        rpass.set_bind_group(0, &self.uniform_bind_group, &[]);
        rpass.set_vertex_buffer(0, vbuf.slice(..));

        for (i, q) in quads.iter().enumerate() {
            let Some(tex) = self.textures.get(&q.tex.0) else {
                continue;
            };
            // Clip rect -> integer scissor, clamped to the framebuffer.
            let x0 = q.clip[0].max(0.0).floor();
            let y0 = q.clip[1].max(0.0).floor();
            let x1 = q.clip[2].min(fb[0]).ceil();
            let y1 = q.clip[3].min(fb[1]).ceil();
            if x1 <= x0 || y1 <= y0 {
                continue;
            }
            rpass.set_scissor_rect(x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32);
            rpass.set_bind_group(1, &tex.bind_group, &[]);
            let base = (i * 6) as u32;
            rpass.draw(base..base + 6, 0..1);
        }
    }
}
