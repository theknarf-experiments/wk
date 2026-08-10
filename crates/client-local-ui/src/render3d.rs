//! A depth-buffered 3D quad renderer over wgpu: the 3D workspace view draws
//! node panels (and, later, real meshes) as textured quads in world space. It
//! samples the same textures `render2d` owns, so live node surfaces need no
//! second upload path.

use std::mem::size_of;

use bytemuck::{Pod, Zeroable};
use wgpu::util::{BufferInitDescriptor, DeviceExt};
use wgpu::*;

use crate::render2d::{Renderer, TextureId};

/// One textured quad in world space: four corners (tl, tr, br, bl), a uv rect
/// (x0,y0,x1,y1), an RGBA tint, and the texture (from `render2d`'s registry).
#[derive(Clone, Copy)]
pub struct Quad3 {
    pub corners: [[f32; 3]; 4],
    pub uv: [f32; 4],
    pub color: [f32; 4],
    pub tex: TextureId,
}

impl Quad3 {
    /// A world-space rect spanned from `center` by half-extent vectors `half_r`
    /// (toward the right edge) and `half_u` (toward the top edge).
    pub fn spanned(
        center: [f32; 3],
        half_r: [f32; 3],
        half_u: [f32; 3],
        uv: [f32; 4],
        color: [f32; 4],
        tex: TextureId,
    ) -> Self {
        let c = center;
        let add = |sr: f32, su: f32| {
            [
                c[0] + sr * half_r[0] + su * half_u[0],
                c[1] + sr * half_r[1] + su * half_u[1],
                c[2] + sr * half_r[2] + su * half_u[2],
            ]
        };
        Quad3 {
            corners: [
                add(-1.0, 1.0),
                add(1.0, 1.0),
                add(1.0, -1.0),
                add(-1.0, -1.0),
            ],
            uv,
            color,
            tex,
        }
    }

    /// A solid ribbon from `a` to `b`, `thickness` wide, facing `eye` — used
    /// for connection wires in the 3D view.
    pub fn ribbon(
        white: TextureId,
        a: [f32; 3],
        b: [f32; 3],
        eye: [f32; 3],
        thickness: f32,
        color: [f32; 4],
    ) -> Self {
        let dir = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
        let mid = [
            (a[0] + b[0]) * 0.5 - eye[0],
            (a[1] + b[1]) * 0.5 - eye[1],
            (a[2] + b[2]) * 0.5 - eye[2],
        ];
        // Perpendicular to both the segment and the view direction, so the
        // ribbon presents its face to the camera.
        let side = cross(dir, mid);
        let len = (side[0] * side[0] + side[1] * side[1] + side[2] * side[2])
            .sqrt()
            .max(1e-6);
        let h = thickness * 0.5 / len;
        let s = [side[0] * h, side[1] * h, side[2] * h];
        Quad3 {
            corners: [
                [a[0] + s[0], a[1] + s[1], a[2] + s[2]],
                [b[0] + s[0], b[1] + s[1], b[2] + s[2]],
                [b[0] - s[0], b[1] - s[1], b[2] - s[2]],
                [a[0] - s[0], a[1] - s[1], a[2] - s[2]],
            ],
            uv: [0.0, 0.0, 1.0, 1.0],
            color,
            tex: white,
        }
    }

    /// The quad's centroid, for depth sorting.
    pub fn center(&self) -> [f32; 3] {
        let mut c = [0.0f32; 3];
        for p in &self.corners {
            c[0] += p[0] * 0.25;
            c[1] += p[1] * 0.25;
            c[2] += p[2] * 0.25;
        }
        c
    }
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Vertex3 {
    pos: [f32; 3],
    uv: [f32; 2],
    color: [u8; 4],
}

const DEPTH_FORMAT: TextureFormat = TextureFormat::Depth32Float;

pub struct Renderer3d {
    pipeline: RenderPipeline,
    uniform_buffer: Buffer,
    uniform_bind_group: BindGroup,
    vertex_buffer: Option<Buffer>,
    vertex_capacity: usize,
    /// Depth buffer matching the current framebuffer, recreated on resize.
    depth: Option<(TextureView, u32, u32)>,
}

impl Renderer3d {
    /// `texture_layout` must be `render2d`'s texture bind-group layout, so this
    /// pipeline can bind the 2D renderer's textures directly.
    pub fn new(device: &Device, format: TextureFormat, texture_layout: &BindGroupLayout) -> Self {
        let shader = device.create_shader_module(include_wgsl!("render3d.wgsl"));

        let uniform_buffer = device.create_buffer(&BufferDescriptor {
            label: Some("render3d uniforms"),
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

        let pipeline_layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[Some(&uniform_layout), Some(texture_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&RenderPipelineDescriptor {
            label: Some("render3d pipeline"),
            layout: Some(&pipeline_layout),
            vertex: VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[VertexBufferLayout {
                    array_stride: size_of::<Vertex3>() as BufferAddress,
                    step_mode: VertexStepMode::Vertex,
                    attributes: &vertex_attr_array![0 => Float32x3, 1 => Float32x2, 2 => Unorm8x4],
                }],
            },
            primitive: PrimitiveState {
                topology: PrimitiveTopology::TriangleList,
                front_face: FrontFace::Cw,
                // Panels are visible from both sides (walk behind a node and
                // you see its mirror), so no culling.
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: Some(DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: Some(true),
                depth_compare: Some(CompareFunction::Less),
                stencil: StencilState::default(),
                bias: DepthBiasState::default(),
            }),
            multisample: MultisampleState::default(),
            fragment: Some(FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
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

        Self {
            pipeline,
            uniform_buffer,
            uniform_bind_group,
            vertex_buffer: None,
            vertex_capacity: 0,
            depth: None,
        }
    }

    /// The depth view for a `w`×`h` framebuffer, recreated when the size
    /// changes. Returned by value (wgpu views are cheap handles) so the caller
    /// can hold it across a later `&mut self` draw call.
    pub fn depth_view(&mut self, device: &Device, w: u32, h: u32) -> TextureView {
        let stale = !matches!(&self.depth, Some((_, dw, dh)) if *dw == w && *dh == h);
        if stale {
            let tex = device.create_texture(&TextureDescriptor {
                label: Some("render3d depth"),
                size: Extent3d {
                    width: w.max(1),
                    height: h.max(1),
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: TextureDimension::D2,
                format: DEPTH_FORMAT,
                usage: TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            });
            self.depth = Some((tex.create_view(&TextureViewDescriptor::default()), w, h));
        }
        self.depth.as_ref().unwrap().0.clone()
    }

    /// Draw `quads` onto the current (depth-attached) render pass, resolving
    /// textures from the 2D renderer's registry. `view_proj` is column-major.
    pub fn draw<'r>(
        &'r mut self,
        device: &Device,
        queue: &Queue,
        rpass: &mut RenderPass<'r>,
        r2d: &'r Renderer,
        view_proj: [[f32; 4]; 4],
        quads: &[Quad3],
    ) {
        if quads.is_empty() {
            return;
        }
        queue.write_buffer(&self.uniform_buffer, 0, bytemuck::bytes_of(&view_proj));

        let mut verts: Vec<Vertex3> = Vec::with_capacity(quads.len() * 6);
        for q in quads {
            let [u0, v0, u1, v1] = q.uv;
            let c = [
                (q.color[0] * 255.0) as u8,
                (q.color[1] * 255.0) as u8,
                (q.color[2] * 255.0) as u8,
                (q.color[3] * 255.0) as u8,
            ];
            let v = |pos: [f32; 3], uv: [f32; 2]| Vertex3 { pos, uv, color: c };
            let tl = v(q.corners[0], [u0, v0]);
            let tr = v(q.corners[1], [u1, v0]);
            let br = v(q.corners[2], [u1, v1]);
            let bl = v(q.corners[3], [u0, v1]);
            verts.extend_from_slice(&[tl, tr, br, tl, br, bl]);
        }

        let bytes: &[u8] = bytemuck::cast_slice(&verts);
        let need_new = match &self.vertex_buffer {
            Some(_) => self.vertex_capacity < bytes.len(),
            None => true,
        };
        if need_new {
            self.vertex_buffer = Some(device.create_buffer_init(&BufferInitDescriptor {
                label: Some("render3d vertices"),
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
            let Some(bg) = r2d.bind_group(q.tex) else {
                continue;
            };
            rpass.set_bind_group(1, bg, &[]);
            let base = (i * 6) as u32;
            rpass.draw(base..base + 6, 0..1);
        }
    }
}
