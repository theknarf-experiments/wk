//! A Rust port of steveruizok's `perfect-arrows` `getArrow`: given two points,
//! compute a nice curved (quadratic-bezier) arrow between them, plus the angle
//! the arrow arrives at its end (for drawing an arrowhead).
//!
//! Ported from <https://github.com/steveruizok/perfect-arrows> (`src/lib`),
//! MIT-licensed. Kept close to the original so its behaviour matches.

use std::f32::consts::PI;

fn get_angle(x0: f32, y0: f32, x1: f32, y1: f32) -> f32 {
    (y1 - y0).atan2(x1 - x0)
}
fn get_distance(x0: f32, y0: f32, x1: f32, y1: f32) -> f32 {
    (y1 - y0).hypot(x1 - x0)
}
/// How close the two points are to a 45° angle (0, 1 and ∞ are the "straight"
/// cases the original snaps to when `straights` is on).
fn get_angliness(x0: f32, y0: f32, x1: f32, y1: f32) -> f32 {
    ((x1 - x0) / (y1 - y0)).abs()
}
fn project_point(x0: f32, y0: f32, a: f32, d: f32) -> (f32, f32) {
    (a.cos() * d + x0, a.sin() * d + y0)
}
fn point_between(x0: f32, y0: f32, x1: f32, y1: f32, d: f32) -> (f32, f32) {
    (x0 + (x1 - x0) * d, y0 + (y1 - y0) * d)
}
fn get_sector(a: f32, s: f32) -> f32 {
    (s * (0.5 + ((a / (PI * 2.0)) % s))).floor()
}
fn rotate_point(x: f32, y: f32, cx: f32, cy: f32, angle: f32) -> (f32, f32) {
    let (s, c) = (angle.sin(), angle.cos());
    let (px, py) = (x - cx, y - cy);
    (px * c - py * s + cx, px * s + py * c + cy)
}
fn modulate(value: f32, from: (f32, f32), to: (f32, f32), clamp: bool) -> f32 {
    let (fl, fh) = from;
    let (tl, th) = to;
    let result = tl + ((value - fl) / (fh - fl)) * (th - tl);
    if clamp {
        if tl < th {
            result.clamp(tl, th)
        } else {
            result.clamp(th, tl)
        }
    } else {
        result
    }
}

/// A curved arrow: a quadratic bezier `start → control → end`, plus the angle
/// (radians) at which it arrives at `end` (for the arrowhead).
#[derive(Clone, Copy)]
pub struct Arrow {
    pub start: (f32, f32),
    pub control: (f32, f32),
    pub end: (f32, f32),
    pub end_angle: f32,
}

/// Tuning knobs, matching `perfect-arrows`' options.
pub struct ArrowOptions {
    pub bow: f32,
    pub stretch: f32,
    pub stretch_min: f32,
    pub stretch_max: f32,
    pub pad_start: f32,
    pub pad_end: f32,
    pub flip: bool,
    pub straights: bool,
}

impl Default for ArrowOptions {
    fn default() -> Self {
        ArrowOptions {
            bow: 0.0,
            stretch: 0.5,
            stretch_min: 0.0,
            stretch_max: 420.0,
            pad_start: 0.0,
            pad_end: 0.0,
            flip: false,
            straights: true,
        }
    }
}

/// The curved arrow between two points. Direct port of `getArrow`.
pub fn get_arrow(x0: f32, y0: f32, x1: f32, y1: f32, o: &ArrowOptions) -> Arrow {
    let angle = get_angle(x0, y0, x1, y1);
    let dist = get_distance(x0, y0, x1, y1);
    let angliness = get_angliness(x0, y0, x1, y1);

    // Straight arrow: too short, no bow/stretch, or (near) a 45°/axis angle.
    let straight_angle =
        o.straights && (angliness == 0.0 || angliness == 1.0 || angliness.is_infinite());
    if dist < (o.pad_start + o.pad_end) * 2.0
        || (o.bow == 0.0 && o.stretch == 0.0)
        || straight_angle
    {
        let ps = (dist - o.pad_start).min(o.pad_start).max(0.0);
        let pe = (dist - ps).min(o.pad_end).max(0.0);
        let (px0, py0) = project_point(x0, y0, angle, ps);
        let (px1, py1) = project_point(x1, y1, angle + PI, pe);
        let (mx, my) = point_between(px0, py0, px1, py1, 0.5);
        return Arrow {
            start: (px0, py0),
            control: (mx, my),
            end: (px1, py1),
            end_angle: angle,
        };
    }

    // Arc: clockwise or counter-clockwise, bowing out by an amount that eases off
    // with distance.
    let rot = (if (get_sector(angle, 8.0) as i32) % 2 == 0 {
        1.0
    } else {
        -1.0
    }) * (if o.flip { -1.0 } else { 1.0 });
    let arc = o.bow + modulate(dist, (o.stretch_min, o.stretch_max), (1.0, 0.0), true) * o.stretch;

    // Control point from the raw endpoints.
    let (mx, my) = point_between(x0, y0, x1, y1, 0.5);
    let (cx, cy) = point_between(x0, y0, x1, y1, 0.5 - arc);
    let (cx, cy) = rotate_point(cx, cy, mx, my, (PI / 2.0) * rot);

    // Padded endpoints (aim them at the control point).
    let a0 = get_angle(x0, y0, cx, cy);
    let (px0, py0) = project_point(x0, y0, a0, o.pad_start);
    let a1 = get_angle(x1, y1, cx, cy);
    let (px1, py1) = project_point(x1, y1, a1, o.pad_end);

    let end_angle = get_angle(cx, cy, x1, y1);

    // Control point for the padded endpoints, then average the two.
    let (mx1, my1) = point_between(px0, py0, px1, py1, 0.5);
    let (cx1, cy1) = point_between(px0, py0, px1, py1, 0.5 - arc);
    let (cx1, cy1) = rotate_point(cx1, cy1, mx1, my1, (PI / 2.0) * rot);
    let (cx2, cy2) = point_between(cx, cy, cx1, cy1, 0.5);

    Arrow {
        start: (px0, py0),
        control: (cx2, cy2),
        end: (px1, py1),
        end_angle,
    }
}

/// Sample the arrow's quadratic bezier at `steps + 1` points.
pub fn polyline(arrow: &Arrow, steps: usize) -> Vec<[f32; 2]> {
    let (p0, c, p1) = (arrow.start, arrow.control, arrow.end);
    (0..=steps)
        .map(|i| {
            let t = i as f32 / steps as f32;
            let u = 1.0 - t;
            [
                u * u * p0.0 + 2.0 * u * t * c.0 + t * t * p1.0,
                u * u * p0.1 + 2.0 * u * t * c.1 + t * t * p1.1,
            ]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An axis-aligned run is the `straights` case: a flat line, control on it.
    #[test]
    fn straight_horizontal_is_flat() {
        let a = get_arrow(0.0, 0.0, 100.0, 0.0, &ArrowOptions::default());
        assert!(a.start.0.abs() < 1e-3 && a.start.1.abs() < 1e-3);
        assert!((a.end.0 - 100.0).abs() < 1e-3 && a.end.1.abs() < 1e-3);
        assert!(a.end_angle.abs() < 1e-3);
        assert!(a.control.1.abs() < 1e-3, "control stays on the line");
    }

    /// A non-45° diagonal arcs: its control bows away from the chord midpoint.
    #[test]
    fn diagonal_bows_off_the_chord() {
        let a = get_arrow(0.0, 0.0, 100.0, 50.0, &ArrowOptions::default());
        let (mx, my) = (50.0, 25.0);
        let off = ((a.control.0 - mx).powi(2) + (a.control.1 - my).powi(2)).sqrt();
        assert!(
            off > 5.0,
            "control should bow away from the chord (off={off})"
        );
    }
}
