// Live shader — edit me (in the wired-up vim, or any editor: this is a real
// file on disk). The shader viewer hot-reloads on every save.
//   main_image(uv) returns an RGB colour; uv is 0..1 across the surface.
//   u.time  seconds since start      u.res  surface size in pixels
//   cc(n)   MIDI CC/note n, 0..1 (the piano is wired in — try cc(1u), cc(60u))
fn main_image(uv: vec2<f32>) -> vec3<f32> {
    let p = uv * 2.0 - 1.0;
    let d = length(p);
    let a = atan2(p.y, p.x);
    // Mod wheel (CC 1) brightens; middle-C (note 60) ripples the rings.
    let rings = 0.5 + 0.5 * sin(d * 12.0 - u.time * 2.0 + cc(60u) * 6.28);
    let glow = 0.6 + 0.4 * cc(1u);
    let hue = 0.5 + 0.5 * cos(a + u.time + vec3<f32>(0.0, 2.09, 4.19));
    return hue * rings * glow;
}
