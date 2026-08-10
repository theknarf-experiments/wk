//! The 3D-view fly camera: WASD + Q/E movement, right-drag mouse look.
//!
//! Matrices are column-major `[[f32; 4]; 4]` (each inner array is one column),
//! matching what WGSL's `mat4x4<f32>` expects in the uniform buffer.

use winit::keyboard::KeyCode;

/// Vertical field of view, radians.
pub(super) const FOV_Y: f32 = 60.0 * std::f32::consts::PI / 180.0;
const NEAR: f32 = 0.05;
const FAR: f32 = 200.0;
/// Base fly speed in world units (≈ metres) per second; Shift quadruples it.
const SPEED: f32 = 2.0;
const SPRINT: f32 = 4.0;
/// Mouse-look sensitivity, radians per logical pixel dragged.
const LOOK: f32 = 0.005;
/// Forward distance per scroll-wheel line.
const SCROLL_FLY: f32 = 0.35;

#[derive(Clone, Copy)]
pub(super) struct Camera3d {
    pub(super) pos: [f32; 3],
    /// Radians around +y; 0 looks down −z, positive turns right.
    pub(super) yaw: f32,
    /// Radians; positive looks up. Clamped shy of straight up/down.
    pub(super) pitch: f32,
}

impl Camera3d {
    pub(super) fn new() -> Self {
        Camera3d {
            pos: [0.0; 3],
            yaw: 0.0,
            pitch: 0.0,
        }
    }

    pub(super) fn forward(&self) -> [f32; 3] {
        let (cy, sy) = (self.yaw.cos(), self.yaw.sin());
        let (cp, sp) = (self.pitch.cos(), self.pitch.sin());
        [sy * cp, sp, -cy * cp]
    }

    pub(super) fn right(&self) -> [f32; 3] {
        [self.yaw.cos(), 0.0, self.yaw.sin()]
    }

    pub(super) fn look(&mut self, drag: [f32; 2]) {
        self.yaw += drag[0] * LOOK;
        self.pitch = (self.pitch - drag[1] * LOOK).clamp(-1.5, 1.5);
    }

    /// Advance the camera one frame: held movement keys, plus `fly` units of
    /// scroll-wheel travel along the view direction. In walk mode (the
    /// VRChat-style default) movement stays on the ground plane at eye height;
    /// fly mode moves along the full gaze with Q/E for down/up.
    pub(super) fn advance(
        &mut self,
        keys: &std::collections::HashSet<KeyCode>,
        sprint: bool,
        fly: f32,
        walk: bool,
        dt: f32,
    ) {
        let speed = if sprint { SPRINT * SPEED } else { SPEED } * dt;
        let f = if walk {
            // Gaze projected onto the ground, so looking down doesn't stall.
            let (cy, sy) = (self.yaw.cos(), self.yaw.sin());
            [sy, 0.0, -cy]
        } else {
            self.forward()
        };
        let r = self.right();
        let mut mv = [0.0f32; 3];
        let key = |c| keys.contains(&c) as i32 as f32;
        let ahead = key(KeyCode::KeyW) - key(KeyCode::KeyS);
        let strafe = key(KeyCode::KeyD) - key(KeyCode::KeyA);
        for i in 0..3 {
            mv[i] += f[i] * ahead * speed + r[i] * strafe * speed;
        }
        if !walk {
            let rise = key(KeyCode::KeyE) - key(KeyCode::KeyQ);
            mv[1] += rise * speed;
        }
        for i in 0..3 {
            self.pos[i] += mv[i] + f[i] * fly * SCROLL_FLY;
        }
        if walk {
            self.pos[1] = 0.0; // eye height above the plaza floor
        }
    }

    /// The combined view-projection matrix (column-major) for this pose.
    pub(super) fn view_proj(&self, aspect: f32) -> [[f32; 4]; 4] {
        let f = self.forward();
        let r = self.right();
        // Orthonormal up = right × forward.
        let u = [
            r[1] * f[2] - r[2] * f[1],
            r[2] * f[0] - r[0] * f[2],
            r[0] * f[1] - r[1] * f[0],
        ];
        let p = self.pos;
        let dot = |a: [f32; 3], b: [f32; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
        let view = [
            [r[0], u[0], -f[0], 0.0],
            [r[1], u[1], -f[1], 0.0],
            [r[2], u[2], -f[2], 0.0],
            [-dot(r, p), -dot(u, p), dot(f, p), 1.0],
        ];
        // Perspective with wgpu's 0..1 clip-space depth.
        let fl = 1.0 / (FOV_Y * 0.5).tan();
        let proj = [
            [fl / aspect, 0.0, 0.0, 0.0],
            [0.0, fl, 0.0, 0.0],
            [0.0, 0.0, FAR / (NEAR - FAR), -1.0],
            [0.0, 0.0, NEAR * FAR / (NEAR - FAR), 0.0],
        ];
        mat_mul(proj, view)
    }

    /// A world-space ray through the given logical pixel of a `fb`-sized
    /// viewport: returns (origin, direction).
    pub(super) fn pixel_ray(&self, px: [f32; 2], fb: [f32; 2]) -> ([f32; 3], [f32; 3]) {
        let aspect = fb[0] / fb[1].max(1.0);
        let tan = (FOV_Y * 0.5).tan();
        let ndc_x = (2.0 * px[0] / fb[0] - 1.0) * tan * aspect;
        let ndc_y = (1.0 - 2.0 * px[1] / fb[1]) * tan;
        let f = self.forward();
        let r = self.right();
        let u = [
            r[1] * f[2] - r[2] * f[1],
            r[2] * f[0] - r[0] * f[2],
            r[0] * f[1] - r[1] * f[0],
        ];
        let mut d = [0.0f32; 3];
        for i in 0..3 {
            d[i] = f[i] + r[i] * ndc_x + u[i] * ndc_y;
        }
        let len = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
        (self.pos, [d[0] / len, d[1] / len, d[2] / len])
    }
}

/// How many canvas pixels make one world unit (≈ a metre): a default 360×260
/// node becomes a card roughly 0.9 m wide.
pub(super) const PX_PER_M: f32 = 420.0;
/// Radius of the cylinder the workspace is wrapped onto.
pub(super) const CYL_R: f32 = 3.0;

pub(super) fn dot3(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

pub(super) fn sub3(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

/// A translation matrix (column-major).
pub(super) fn mat_translate(p: [f32; 3]) -> [[f32; 4]; 4] {
    let mut m = mat_ident();
    m[3][0] = p[0];
    m[3][1] = p[1];
    m[3][2] = p[2];
    m
}

/// A rotation around +y (column-major).
pub(super) fn mat_rot_y(yaw: f32) -> [[f32; 4]; 4] {
    let (s, c) = yaw.sin_cos();
    let mut m = mat_ident();
    m[0][0] = c;
    m[0][2] = -s;
    m[2][0] = s;
    m[2][2] = c;
    m
}

/// A uniform scale matrix.
pub(super) fn mat_scale(k: f32) -> [[f32; 4]; 4] {
    let mut m = mat_ident();
    m[0][0] = k;
    m[1][1] = k;
    m[2][2] = k;
    m
}

pub(super) fn mat_ident() -> [[f32; 4]; 4] {
    let mut m = [[0.0; 4]; 4];
    for (i, row) in m.iter_mut().enumerate() {
        row[i] = 1.0;
    }
    m
}

/// Transform a point by a column-major matrix (w = 1).
pub(super) fn transform_point3(m: [[f32; 4]; 4], p: [f32; 3]) -> [f32; 3] {
    [
        m[0][0] * p[0] + m[1][0] * p[1] + m[2][0] * p[2] + m[3][0],
        m[0][1] * p[0] + m[1][1] * p[1] + m[2][1] * p[2] + m[3][1],
        m[0][2] * p[0] + m[1][2] * p[1] + m[2][2] * p[2] + m[3][2],
    ]
}

/// The nearest positive hit of a ray with a sphere, if any.
pub(super) fn ray_sphere(o: [f32; 3], d: [f32; 3], c: [f32; 3], r: f32) -> Option<f32> {
    let oc = sub3(o, c);
    let b = 2.0 * dot3(oc, d);
    let cc = dot3(oc, oc) - r * r;
    let disc = b * b - 4.0 * cc;
    if disc < 0.0 {
        return None;
    }
    let s = disc.sqrt();
    let t1 = (-b - s) / 2.0;
    let t2 = (-b + s) / 2.0;
    let t = if t1 > 0.0 { t1 } else { t2 };
    (t > 0.0).then_some(t)
}

/// Column-major 4×4 multiply: `a * b`.
pub(super) fn mat_mul(a: [[f32; 4]; 4], b: [[f32; 4]; 4]) -> [[f32; 4]; 4] {
    let mut out = [[0.0f32; 4]; 4];
    for (col, out_col) in out.iter_mut().enumerate() {
        for (row, out_cell) in out_col.iter_mut().enumerate() {
            *out_cell = (0..4).map(|k| a[k][row] * b[col][k]).sum();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_camera_projects_forward_point_to_center() {
        let cam = Camera3d::new();
        let vp = cam.view_proj(1.0);
        // A point straight ahead (−z) should land at clip x=y=0, w>0.
        let p = [0.0f32, 0.0, -5.0];
        let mut clip = [0.0f32; 4];
        for (row, c) in clip.iter_mut().enumerate() {
            *c = vp[0][row] * p[0] + vp[1][row] * p[1] + vp[2][row] * p[2] + vp[3][row];
        }
        assert!(clip[0].abs() < 1e-5 && clip[1].abs() < 1e-5);
        assert!(clip[3] > 0.0);
        // Depth must be inside 0..1 clip space after the perspective divide.
        let z = clip[2] / clip[3];
        assert!((0.0..=1.0).contains(&z), "z={z}");
    }

    #[test]
    fn walk_mode_stays_grounded_fly_mode_climbs() {
        use std::collections::HashSet;
        let mut keys = HashSet::new();
        keys.insert(KeyCode::KeyW);
        // Looking sharply down, walking must still move forward on the plane
        // and stay at eye height.
        let mut cam = Camera3d::new();
        cam.pitch = -1.2;
        cam.advance(&keys, false, 0.0, true, 1.0);
        assert_eq!(cam.pos[1], 0.0);
        assert!(cam.pos[2] < -1.0, "moved ahead: {:?}", cam.pos);
        // The same input flying descends along the gaze.
        let mut cam = Camera3d::new();
        cam.pitch = -1.2;
        cam.advance(&keys, false, 0.0, false, 1.0);
        assert!(cam.pos[1] < -0.5, "flew down: {:?}", cam.pos);
    }

    #[test]
    fn trs_compose_and_ray_sphere_agree() {
        // T(1,2,3) * RotY(pi/2) * S(2) applied to +x: x-axis rotates to -z,
        // scaled to length 2, then translated.
        let m = mat_mul(
            mat_mul(
                mat_translate([1.0, 2.0, 3.0]),
                mat_rot_y(std::f32::consts::FRAC_PI_2),
            ),
            mat_scale(2.0),
        );
        let p = transform_point3(m, [1.0, 0.0, 0.0]);
        assert!((p[0] - 1.0).abs() < 1e-4, "{p:?}");
        assert!((p[1] - 2.0).abs() < 1e-4);
        assert!((p[2] - 1.0).abs() < 1e-4, "{p:?}");
        // A ray straight at a unit sphere 5 ahead hits at t=4.
        let t = ray_sphere([0.0; 3], [0.0, 0.0, -1.0], [0.0, 0.0, -5.0], 1.0).unwrap();
        assert!((t - 4.0).abs() < 1e-4);
        // Missing ray misses.
        assert!(ray_sphere([0.0; 3], [0.0, 1.0, 0.0], [0.0, 0.0, -5.0], 1.0).is_none());
    }

    #[test]
    fn pixel_ray_center_matches_forward() {
        let mut cam = Camera3d::new();
        cam.yaw = 0.7;
        cam.pitch = -0.3;
        let (_, d) = cam.pixel_ray([400.0, 300.0], [800.0, 600.0]);
        let f = cam.forward();
        for i in 0..3 {
            assert!((d[i] - f[i]).abs() < 1e-4);
        }
    }
}
