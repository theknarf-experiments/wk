//! Screen-space geometry: rect maths, node chrome button layout, hit-testing,
//! and the curved connection arrows. All pure functions shared by the draw and
//! the input hit-tests so the two never disagree.

use super::*;

pub(super) fn contains(r: [f32; 4], p: [f32; 2]) -> bool {
    p[0] >= r[0] && p[0] < r[2] && p[1] >= r[1] && p[1] < r[3]
}

pub(super) fn intersect(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    [
        a[0].max(b[0]),
        a[1].max(b[1]),
        a[2].min(b[2]),
        a[3].min(b[3]),
    ]
}

pub(super) fn win_rect(cam: Camera, pos: [f32; 2], size: [f32; 2]) -> [f32; 4] {
    let s = cam.to_screen(pos);
    [
        s[0],
        s[1],
        s[0] + size[0] * cam.zoom,
        s[1] + size[1] * cam.zoom,
    ]
}

pub(super) fn title_bar(r: [f32; 4], z: f32) -> [f32; 4] {
    [r[0], r[1], r[2], r[1] + TITLE_H * z]
}
/// The close box at the right of a workspace tab rect.
pub(super) fn tab_close_btn(r: [f32; 4]) -> [f32; 4] {
    let s = (TAB_H - 12.0).max(8.0);
    let x1 = r[2] - 5.0;
    let y0 = (TAB_H - s) * 0.5;
    [x1 - s, y0, x1, y0 + s]
}
pub(super) fn close_btn(r: [f32; 4], z: f32) -> [f32; 4] {
    let s = (TITLE_H - 8.0) * z;
    let x1 = r[2] - 4.0 * z;
    let y0 = r[1] + 4.0 * z;
    [x1 - s, y0, x1, y0 + s]
}
/// The detach button, just left of the close button. Pops the node out into its
/// own OS window (and, when already detached, reattaches it). Shown on app nodes.
pub(super) fn detach_btn(r: [f32; 4], z: f32) -> [f32; 4] {
    let cb = close_btn(r, z);
    let w = cb[2] - cb[0];
    let gap = 4.0 * z;
    [cb[0] - w - gap, cb[1], cb[0] - gap, cb[3]]
}
/// The Files button, just left of the detach button. Opens the node's virtual
/// filesystem inspector. Shown on app nodes (which have a per-node fs).
pub(super) fn files_btn(r: [f32; 4], z: f32) -> [f32; 4] {
    let db = detach_btn(r, z);
    let w = db[2] - db[0];
    let gap = 4.0 * z;
    [db[0] - w - gap, db[1], db[0] - gap, db[3]]
}
/// The Logs button, just left of the Files button. Opens the node's output-log
/// panel (its captured stdout/stderr scrollback). Shown on app nodes.
pub(super) fn logs_btn(r: [f32; 4], z: f32) -> [f32; 4] {
    let fb = files_btn(r, z);
    let w = fb[2] - fb[0];
    let gap = 4.0 * z;
    [fb[0] - w - gap, fb[1], fb[0] - gap, fb[3]]
}
/// The Run/▶ button, just left of the Logs button. Shown only on an idle or
/// exited node so it can be (re)started after wiring.
pub(super) fn run_btn(r: [f32; 4], z: f32) -> [f32; 4] {
    let lb = logs_btn(r, z);
    let w = lb[2] - lb[0];
    let gap = 4.0 * z;
    [lb[0] - w - gap, lb[1], lb[0] - gap, lb[3]]
}
/// The editable launch-args bar along the bottom of an idle node's body (a
/// one-line input strip, so it doesn't paint over the node's output above).
pub(super) fn args_bar(r: [f32; 4], z: f32) -> [f32; 4] {
    let ca = content_rect(r, z);
    let h = (TITLE_H * z).min((ca[3] - ca[1]).max(0.0));
    [ca[0], ca[3] - h, ca[2], ca[3]]
}
pub(super) fn resize_grip(r: [f32; 4], z: f32) -> [f32; 4] {
    let g = 16.0 * z;
    [r[2] - g, r[3] - g, r[2], r[3]]
}
/// The "−" and "+" port-step buttons on a HostPort node (bottom-right).
pub(super) fn port_step_btns(r: [f32; 4], z: f32) -> ([f32; 4], [f32; 4]) {
    let s = 14.0 * z;
    let gap = 3.0 * z;
    let y0 = r[3] - s - 4.0 * z;
    let px = r[2] - 4.0 * z;
    let plus = [px - s, y0, px, y0 + s];
    let minus = [px - 2.0 * s - gap, y0, px - s - gap, y0 + s];
    (minus, plus)
}
pub(super) fn content_rect(r: [f32; 4], z: f32) -> [f32; 4] {
    [
        r[0] + BORDER * z,
        r[1] + TITLE_H * z,
        r[2] - BORDER * z,
        r[3] - BORDER * z,
    ]
}

pub(super) fn near(a: [f32; 2], b: [f32; 2], radius: f32) -> bool {
    let (dx, dy) = (a[0] - b[0], a[1] - b[1]);
    dx * dx + dy * dy <= radius * radius
}

pub(super) fn dist_to_segment(p: [f32; 2], a: [f32; 2], b: [f32; 2]) -> f32 {
    let (abx, aby) = (b[0] - a[0], b[1] - a[1]);
    let (apx, apy) = (p[0] - a[0], p[1] - a[1]);
    let len2 = abx * abx + aby * aby;
    let t = if len2 > 0.0 {
        ((apx * abx + apy * aby) / len2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let (cx, cy) = (a[0] + abx * t, a[1] + aby * t);
    let (dx, dy) = (p[0] - cx, p[1] - cy);
    (dx * dx + dy * dy).sqrt()
}

/// How close (screen px) a click must be to a wire to select it.
pub(super) const WIRE_PICK: f32 = 6.0;

/// The curved arrow (perfect-arrows) for a connection from output port `a` to
/// input port `b`. Shared by drawing and hit-testing so they agree.
pub(super) fn connection_arrow(a: [f32; 2], b: [f32; 2], zf: f32) -> crate::arrows::Arrow {
    let opts = crate::arrows::ArrowOptions {
        // End the curve a touch before the input port so the arrowhead sits there.
        pad_end: (6.0 * zf).max(4.0),
        ..Default::default()
    };
    crate::arrows::get_arrow(a[0], a[1], b[0], b[1], &opts)
}

/// Draw a connection as a curved arrow with a head at the target end, so a wire
/// looks smooth and shows its direction (source output -> target input).
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_connection(
    quads: &mut Vec<Quad>,
    white: TextureId,
    a: [f32; 2],
    b: [f32; 2],
    sel: bool,
    color: [f32; 4],
    zf: f32,
    clip: [f32; 4],
) {
    let th = if sel {
        (3.5 * zf).max(2.5)
    } else {
        (2.0 * zf).max(1.5)
    };
    let arrow = connection_arrow(a, b, zf);
    // The curved shaft, tessellated into short segments.
    let pts = crate::arrows::polyline(&arrow, 24);
    for s in pts.windows(2) {
        quads.push(Quad::line(white, s[0], s[1], th, color, clip));
    }
    // Arrowhead at the end, pointing along the arrival angle.
    let size = (7.0 * zf).max(5.0);
    let end = [arrow.end.0, arrow.end.1];
    let ang = arrow.end_angle;
    let spread = 0.5;
    for wing in [
        ang + std::f32::consts::PI - spread,
        ang + std::f32::consts::PI + spread,
    ] {
        let p = [end[0] + wing.cos() * size, end[1] + wing.sin() * size];
        quads.push(Quad::line(white, end, p, th.max(1.5), color, clip));
    }
}
