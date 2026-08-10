struct Uniforms {
    u_ViewProj: mat4x4<f32>,
};

struct VertexInput {
    @location(0) a_Pos: vec3<f32>,
    @location(1) a_UV: vec2<f32>,
    @location(2) a_Color: vec4<f32>,
};

struct VertexOutput {
    @location(0) v_UV: vec2<f32>,
    @location(1) v_Color: vec4<f32>,
    @builtin(position) v_Position: vec4<f32>,
};

@group(0) @binding(0)
var<uniform> uniforms: Uniforms;

@vertex
fn vs_main(in: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    out.v_UV = in.a_UV;
    out.v_Color = in.a_Color;
    out.v_Position = uniforms.u_ViewProj * vec4<f32>(in.a_Pos, 1.0);
    return out;
}

struct FragmentOutput {
    @location(0) o_Target: vec4<f32>,
};

@group(1) @binding(0)
var u_Texture: texture_2d<f32>;
@group(1) @binding(1)
var u_Sampler: sampler;

// Vertex colors are given in sRGB (matching render2d's palette constants) and
// converted here, since the surface format is sRGB.
fn srgb_to_linear(srgb: vec4<f32>) -> vec4<f32> {
    let color_srgb = srgb.rgb;
    let selector = ceil(color_srgb - 0.04045); // 0 if under value, 1 if over
    let under = color_srgb / 12.92;
    let over = pow((color_srgb + 0.055) / 1.055, vec3<f32>(2.4));
    let result = mix(under, over, selector);
    return vec4<f32>(result, srgb.a);
}

@fragment
fn fs_main(in: VertexOutput) -> FragmentOutput {
    let color = srgb_to_linear(in.v_Color);
    return FragmentOutput(color * textureSample(u_Texture, u_Sampler, in.v_UV));
}
