struct FrameUniforms {
    u_ViewProj: mat4x4<f32>,
    // xyz = direction toward the light, w = ambient strength.
    u_Light: vec4<f32>,
};

struct ModelUniforms {
    u_Model: mat4x4<f32>,
    // Material base colour factor.
    u_Color: vec4<f32>,
};

struct VertexInput {
    @location(0) a_Pos: vec3<f32>,
    @location(1) a_Normal: vec3<f32>,
    @location(2) a_UV: vec2<f32>,
    @location(3) a_Color: vec4<f32>,
};

struct VertexOutput {
    @location(0) v_UV: vec2<f32>,
    @location(1) v_Color: vec4<f32>,
    @location(2) v_Normal: vec3<f32>,
    @builtin(position) v_Position: vec4<f32>,
};

@group(0) @binding(0)
var<uniform> frame: FrameUniforms;
@group(2) @binding(0)
var<uniform> model: ModelUniforms;

@vertex
fn vs_main(in: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    out.v_UV = in.a_UV;
    out.v_Color = in.a_Color * model.u_Color;
    // Rigid/uniform transforms only: rotate the normal by the model's 3x3.
    let n = (model.u_Model * vec4<f32>(in.a_Normal, 0.0)).xyz;
    out.v_Normal = n;
    out.v_Position = frame.u_ViewProj * model.u_Model * vec4<f32>(in.a_Pos, 1.0);
    return out;
}

struct FragmentOutput {
    @location(0) o_Target: vec4<f32>,
};

@group(1) @binding(0)
var u_Texture: texture_2d<f32>;
@group(1) @binding(1)
var u_Sampler: sampler;

// Base-colour textures are sRGB-encoded but stored in a non-sRGB texture (the
// shared render2d registry), so decode here. Factors/vertex colours are
// already linear per the glTF spec.
fn srgb_to_linear(srgb: vec4<f32>) -> vec4<f32> {
    let c = srgb.rgb;
    let selector = ceil(c - 0.04045);
    let under = c / 12.92;
    let over = pow((c + 0.055) / 1.055, vec3<f32>(2.4));
    return vec4<f32>(mix(under, over, selector), srgb.a);
}

@fragment
fn fs_main(in: VertexOutput) -> FragmentOutput {
    let n = normalize(in.v_Normal);
    let l = normalize(frame.u_Light.xyz);
    let ambient = frame.u_Light.w;
    // Lambert with an ambient floor so faces away from the light stay readable.
    let diffuse = max(dot(n, l), 0.0) * (1.0 - ambient);
    let lit = ambient + diffuse;
    let tex = srgb_to_linear(textureSample(u_Texture, u_Sampler, in.v_UV));
    let base = in.v_Color * tex;
    return FragmentOutput(vec4<f32>(base.rgb * lit, base.a));
}
